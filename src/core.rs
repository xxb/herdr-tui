//! Multiplexed client connection to `herdr-core` via `herdr-core connect` / `hc connect`.

use crate::wire;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug)]
pub enum CoreMessage {
    /// A JSON-RPC event from the `events.subscribe` stream or notification.
    Event(Value),
    /// Raw VT output bytes for an attached terminal channel.
    TerminalData { channel: u32, data: Vec<u8> },
    /// An attached terminal channel was closed by the core.
    TerminalClosed {
        channel: u32,
        #[allow(dead_code)]
        reason: Value,
    },
    /// The connection to `herdr-core` was lost.
    Disconnected,
}

pub struct CoreClient {
    _child: Child,
    writer: Arc<Mutex<ChildStdin>>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, (String, Sender<Value>)>>>,
    events_channel: Arc<Mutex<Option<u32>>>,
}

fn find_core_binary() -> PathBuf {
    if let Some(env_bin) = std::env::var_os("HERDR_CORE_BIN") {
        return PathBuf::from(env_bin);
    }
    for candidate in ["herdr-core", "hc"] {
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                let full = dir.join(candidate);
                if full.is_file() {
                    return full;
                }
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let local = PathBuf::from(home).join(".local/bin/herdr-core");
        if local.is_file() {
            return local;
        }
    }
    PathBuf::from("herdr-core")
}

impl CoreClient {
    pub fn connect(session: Option<&str>) -> Result<(Self, Receiver<CoreMessage>)> {
        let bin = find_core_binary();
        let mut cmd = Command::new(&bin);
        cmd.arg("connect");
        if let Some(s) = session {
            cmd.arg("--session").arg(s);
            cmd.env("HC_SESSION", s);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn `{}` connect", bin.display()))?;

        let mut stdin = child.stdin.take().context("missing child stdin")?;
        let stdout = child.stdout.take().context("missing child stdout")?;
        let mut reader = BufReader::new(stdout);

        // Upgrade connection to framed mode
        stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"hello\",\"params\":{\"framed\":true}}\n")
            .context("failed to write hello")?;
        stdin.flush()?;

        let mut hello_line = String::new();
        reader
            .read_line(&mut hello_line)
            .context("failed to read hello reply")?;
        if hello_line.trim().is_empty() {
            bail!("herdr-core closed connection during hello handshake");
        }

        let writer = Arc::new(Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<u64, (String, Sender<Value>)>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let events_channel: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

        let (msg_tx, msg_rx) = mpsc::sync_channel(512);

        {
            let writer_clone = writer.clone();
            let pending_clone = pending.clone();
            let events_ch_clone = events_channel.clone();

            std::thread::spawn(move || {
                let mut event_buf: Vec<u8> = Vec::new();
                while let Ok(Some((ch, kind, payload))) = wire::read_frame(&mut reader) {
                    if ch == 0 && kind == wire::DATA {
                        if let Ok(val) = serde_json::from_slice::<Value>(&payload) {
                            if let Some(id) = val.get("id").and_then(Value::as_u64) {
                                if let Some((method, tx)) = pending_clone
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .remove(&id)
                                {
                                    // Register the event stream channel immediately on the reader thread
                                    // before reading subsequent frames so early event frames are never
                                    // misclassified as TerminalData.
                                    if method == "events.subscribe" {
                                        if let Some(sub_ch) = val
                                            .get("result")
                                            .and_then(|r| r.get("channel"))
                                            .and_then(Value::as_u64)
                                        {
                                            *events_ch_clone
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner()) =
                                                Some(sub_ch as u32);
                                        }
                                    }
                                    let _ = tx.send(val);
                                }
                            } else if val.get("method").is_some() {
                                let _ = msg_tx.send(CoreMessage::Event(val));
                            }
                        }
                        continue;
                    }

                    let is_events_ch = *events_ch_clone.lock().unwrap_or_else(|e| e.into_inner())
                        == Some(ch);

                    if is_events_ch {
                        if kind == wire::DATA {
                            // Acknowledge flow-control credit immediately for low-volume JSON event lines
                            let credit = wire::encode(ch, wire::CREDIT, &(payload.len() as u32).to_le_bytes());
                            let _ = writer_clone
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .write_all(&credit);

                            event_buf.extend_from_slice(&payload);
                            while let Some(pos) = event_buf.iter().position(|&b| b == b'\n') {
                                let line: Vec<u8> = event_buf.drain(..=pos).collect();
                                if let Ok(val) = serde_json::from_slice::<Value>(&line) {
                                    let _ = msg_tx.send(CoreMessage::Event(val));
                                }
                            }
                        }
                        continue;
                    }

                    match kind {
                        wire::DATA => {
                            let len = payload.len() as u32;
                            if msg_tx
                                .send(CoreMessage::TerminalData {
                                    channel: ch,
                                    data: payload,
                                })
                                .is_ok()
                                && len > 0
                            {
                                // Acknowledge flow-control credit immediately after queuing into
                                // the bounded channel so `herdr-core` never stalls at 32 KiB
                                // boundaries waiting for the UI thread's poll timer.
                                let credit = wire::encode(ch, wire::CREDIT, &len.to_le_bytes());
                                let mut w =
                                    writer_clone.lock().unwrap_or_else(|e| e.into_inner());
                                let _ = w.write_all(&credit);
                                let _ = w.flush();
                            }
                        }
                        wire::CLOSE => {
                            let reason = serde_json::from_slice(&payload).unwrap_or(Value::Null);
                            let _ = msg_tx.send(CoreMessage::TerminalClosed {
                                channel: ch,
                                reason,
                            });
                        }
                        _ => {}
                    }
                }
                let _ = msg_tx.send(CoreMessage::Disconnected);
            });
        }

        let client = Self {
            _child: child,
            writer,
            next_id: AtomicU64::new(1),
            pending,
            events_channel,
        };

        Ok((client, msg_rx))
    }

    pub fn send_frame(&self, channel: u32, kind: u8, payload: &[u8]) -> Result<()> {
        let frame = wire::encode(channel, kind, payload);
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        w.write_all(&frame)?;
        w.flush()?;
        Ok(())
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_timeout(method, params, Duration::from_secs(10))
    }

    pub fn call_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, (method.to_string(), tx));

        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(e) = self.send_frame(0, wire::DATA, req.to_string().as_bytes()) {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(e);
        }

        let reply = rx.recv_timeout(timeout).map_err(|_| {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            anyhow!("RPC `{method}` timed out")
        })?;

        if let Some(err) = reply.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown RPC error");
            let kind = err
                .get("data")
                .and_then(|d| d.get("kind"))
                .and_then(Value::as_str)
                .unwrap_or("error");
            bail!("{kind}: {msg}");
        }

        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Subscribe to core events (`events.subscribe`). Returns the initial snapshot.
    pub fn subscribe_events(&self) -> Result<Value> {
        let result = self.call("events.subscribe", json!({}))?;
        if let Some(ch) = result.get("channel").and_then(Value::as_u64) {
            *self.events_channel.lock().unwrap_or_else(|e| e.into_inner()) = Some(ch as u32);
        }
        Ok(result)
    }

    /// Attach to a terminal and return its stream channel ID.
    pub fn attach_terminal(&self, terminal: &str, cols: u16, rows: u16, history: usize) -> Result<u32> {
        let params = json!({
            "terminal": terminal,
            "observe": false,
            "history": history,
            "cols": cols,
            "rows": rows,
        });
        let result = self.call("terminal.attach", params)?;
        let ch = result
            .get("channel")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("terminal.attach returned no channel"))?;
        Ok(ch as u32)
    }

    /// Close an attached stream channel without killing the underlying terminal.
    pub fn detach_channel(&self, channel: u32) {
        let _ = self.send_frame(channel, wire::CLOSE, &[]);
    }

    /// Send input bytes to an attached terminal channel.
    pub fn send_input(&self, channel: u32, bytes: &[u8]) {
        let _ = self.send_frame(channel, wire::DATA, bytes);
    }

    /// Resize an attached terminal channel.
    pub fn resize_channel(&self, channel: u32, cols: u16, rows: u16) {
        let mut payload = [0u8; 4];
        payload[0..2].copy_from_slice(&cols.to_le_bytes());
        payload[2..4].copy_from_slice(&rows.to_le_bytes());
        let _ = self.send_frame(channel, wire::RESIZE, &payload);
    }
}
