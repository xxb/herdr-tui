//! Multiplexed client connection to `herdr-core` via `herdr-core connect` / `hc connect`.

use crate::wire;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    /// Asynchronous completion of a `terminal.read` JSON-RPC request.
    TerminalRead {
        request_id: u64,
        terminal: String,
        result: std::result::Result<TerminalReadResult, String>,
    },
    /// The connection to `herdr-core` was lost.
    Disconnected,
}

type PendingRpcMap = HashMap<u64, (String, Sender<Value>)>;
type PendingAsyncReadMap = HashMap<u64, (String, Instant)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalReadResult {
    pub text: String,
    pub complete: bool,
    pub how: String,
    pub why: Option<String>,
}

impl TerminalReadResult {
    pub fn from_value(v: &Value) -> Result<Self> {
        let text = v
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("terminal.read result missing `text`"))?
            .to_string();
        let complete = v.get("complete").and_then(Value::as_bool).unwrap_or(true);
        let how = v
            .get("how")
            .and_then(Value::as_str)
            .unwrap_or("screen")
            .to_string();
        let why = v
            .get("why")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from);
        Ok(Self {
            text,
            complete,
            how,
            why,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LastPtyInput {
    pub seq: u64,
    pub channel: u32,
    pub at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachedChannelInfo {
    pub channel: u32,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
}

fn parse_attach_channel_info(result: &Value) -> Result<AttachedChannelInfo> {
    let ch = result
        .get("channel")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| anyhow!("terminal.attach returned no valid channel"))?;
    let term = result.get("terminal");
    let cols = term
        .and_then(|t| t.get("cols"))
        .and_then(Value::as_u64)
        .and_then(|v| u16::try_from(v).ok())
        .filter(|&v| v > 0);
    let rows = term
        .and_then(|t| t.get("rows"))
        .and_then(Value::as_u64)
        .and_then(|v| u16::try_from(v).ok())
        .filter(|&v| v > 0);
    Ok(AttachedChannelInfo {
        channel: ch,
        cols,
        rows,
    })
}

pub struct CoreClient {
    _child: Child,
    writer: Arc<Mutex<BufWriter<ChildStdin>>>,
    next_id: AtomicU64,
    pending: Arc<Mutex<PendingRpcMap>>,
    pending_async_reads: Arc<Mutex<PendingAsyncReadMap>>,
    events_channel: Arc<Mutex<Option<u32>>>,
    last_pty_input: Mutex<Option<LastPtyInput>>,
    #[cfg(test)]
    skip_rpc_for_tests: bool,
}

fn format_rpc_error(err: &Value) -> String {
    let msg = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown RPC error");
    let kind = err
        .get("data")
        .and_then(|d| d.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("error");
    format!("{kind}: {msg}")
}

fn handle_rpc_reply_value<T, F>(
    val: Value,
    pending: &Mutex<PendingRpcMap>,
    pending_async_reads: &Mutex<PendingAsyncReadMap>,
    events_channel: &Mutex<Option<u32>>,
    msg_tx: &Sender<T>,
    wrap: &F,
) where
    F: Fn(CoreMessage) -> T,
{
    if let Some(id) = val.get("id").and_then(Value::as_u64) {
        if let Some((method, tx)) = pending
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
                    *events_channel.lock().unwrap_or_else(|e| e.into_inner()) = Some(sub_ch as u32);
                }
            }
            let _ = tx.send(val);
        } else if let Some((terminal, _)) = pending_async_reads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        {
            let result = if let Some(err) = val.get("error") {
                Err(format_rpc_error(err))
            } else {
                TerminalReadResult::from_value(val.get("result").unwrap_or(&Value::Null))
                    .map_err(|e| e.to_string())
            };
            let _ = msg_tx.send(wrap(CoreMessage::TerminalRead {
                request_id: id,
                terminal,
                result,
            }));
        }
    } else if val.get("method").is_some() {
        let _ = msg_tx.send(wrap(CoreMessage::Event(val)));
    }
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
    #[allow(dead_code)]
    pub fn connect(session: Option<&str>) -> Result<(Self, Receiver<CoreMessage>)> {
        let (msg_tx, msg_rx) = mpsc::channel();
        let client = Self::connect_with_sender(session, msg_tx, |msg| msg)?;
        Ok((client, msg_rx))
    }

    pub fn connect_with_sender<T, F>(
        session: Option<&str>,
        msg_tx: Sender<T>,
        wrap: F,
    ) -> Result<Self>
    where
        T: Send + 'static,
        F: Fn(CoreMessage) -> T + Send + 'static,
    {
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

        let writer = Arc::new(Mutex::new(BufWriter::with_capacity(16 * 1024, stdin)));
        let pending: Arc<Mutex<PendingRpcMap>> = Arc::new(Mutex::new(HashMap::new()));
        let pending_async_reads: Arc<Mutex<PendingAsyncReadMap>> =
            Arc::new(Mutex::new(HashMap::new()));
        let events_channel: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

        {
            let writer_clone = writer.clone();
            let pending_clone = pending.clone();
            let pending_async_clone = pending_async_reads.clone();
            let events_ch_clone = events_channel.clone();

            std::thread::spawn(move || {
                let mut event_buf: Vec<u8> = Vec::new();
                while let Ok(Some((ch, kind, payload))) = wire::read_frame(&mut reader) {
                    if ch == 0 && kind == wire::DATA {
                        if let Ok(val) = serde_json::from_slice::<Value>(&payload) {
                            handle_rpc_reply_value(
                                val,
                                &pending_clone,
                                &pending_async_clone,
                                &events_ch_clone,
                                &msg_tx,
                                &wrap,
                            );
                        }
                        continue;
                    }

                    let is_events_ch =
                        *events_ch_clone.lock().unwrap_or_else(|e| e.into_inner()) == Some(ch);

                    if is_events_ch {
                        if kind == wire::DATA {
                            // Acknowledge flow-control credit immediately for low-volume JSON event lines
                            let hdr = wire::encode_header(ch, wire::CREDIT, 4);
                            let mut w = writer_clone.lock().unwrap_or_else(|e| e.into_inner());
                            let _ = w.write_all(&hdr);
                            let _ = w.write_all(&(payload.len() as u32).to_le_bytes());
                            let _ = w.flush();
                            drop(w);

                            event_buf.extend_from_slice(&payload);
                            while let Some(pos) = event_buf.iter().position(|&b| b == b'\n') {
                                let line: Vec<u8> = event_buf.drain(..=pos).collect();
                                if let Ok(val) = serde_json::from_slice::<Value>(&line) {
                                    let _ = msg_tx.send(wrap(CoreMessage::Event(val)));
                                }
                            }
                        }
                        continue;
                    }

                    match kind {
                        wire::DATA => {
                            let _ = msg_tx.send(wrap(CoreMessage::TerminalData {
                                channel: ch,
                                data: payload,
                            }));
                        }
                        wire::CLOSE => {
                            let reason = serde_json::from_slice(&payload).unwrap_or(Value::Null);
                            let _ = msg_tx.send(wrap(CoreMessage::TerminalClosed {
                                channel: ch,
                                reason,
                            }));
                        }
                        _ => {}
                    }
                }
                let _ = msg_tx.send(wrap(CoreMessage::Disconnected));
            });
        }

        Ok(Self {
            _child: child,
            writer,
            next_id: AtomicU64::new(1),
            pending,
            pending_async_reads,
            events_channel,
            last_pty_input: Mutex::new(None),
            #[cfg(test)]
            skip_rpc_for_tests: false,
        })
    }

    pub fn send_frame(&self, channel: u32, kind: u8, payload: &[u8]) -> Result<()> {
        let hdr = wire::encode_header(channel, kind, payload.len());
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        w.write_all(&hdr)?;
        w.write_all(payload)?;
        w.flush()?;
        Ok(())
    }

    /// Fire-and-forget JSON-RPC call on channel 0 without blocking the caller for a reply.
    /// Allocates a normal request `id` so `herdr-core` processes standard RPC methods,
    /// while the reader thread silently drops the unawaited response.
    pub fn call_async(&self, method: &str, params: Value) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Ok(bytes) = serde_json::to_vec(&req) {
            let _ = self.send_frame(0, wire::DATA, &bytes);
        }
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
        let req_bytes = serde_json::to_vec(&req)?;
        if let Err(e) = self.send_frame(0, wire::DATA, &req_bytes) {
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
            bail!("{}", format_rpc_error(err));
        }

        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Subscribe to core events (`events.subscribe`). Returns the initial snapshot.
    pub fn subscribe_events(&self) -> Result<Value> {
        let result = self.call("events.subscribe", json!({}))?;
        if let Some(ch) = result.get("channel").and_then(Value::as_u64) {
            *self
                .events_channel
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(ch as u32);
        }
        Ok(result)
    }

    /// Attach to a terminal and return its stream channel ID and live PTY size.
    pub fn attach_terminal(
        &self,
        terminal: &str,
        cols: u16,
        rows: u16,
        history: usize,
        resumed: bool,
    ) -> Result<AttachedChannelInfo> {
        let params = json!({
            "terminal": terminal,
            "observe": false,
            "history": history,
            "cols": cols,
            "rows": rows,
            "resumed": resumed,
        });
        let result = self.call("terminal.attach", params)?;
        parse_attach_channel_info(&result)
    }

    /// Close an attached stream channel without killing the underlying terminal.
    pub fn detach_channel(&self, channel: u32) {
        let _ = self.send_frame(channel, wire::CLOSE, &[]);
    }

    /// Notify `herdr-core` whether this attached terminal channel currently has viewer focus.
    pub fn set_focus(&self, channel: u32, focused: bool) {
        let byte = u8::from(focused);
        let _ = self.send_frame(channel, wire::FOCUS, &[byte]);
    }

    /// Return flow-control credit for a terminal channel after the UI thread drains data.
    #[allow(dead_code)]
    pub fn ack_credit(&self, channel: u32, bytes: u32) {
        if bytes > 0 {
            let _ = self.send_frame(channel, wire::CREDIT, &bytes.to_le_bytes());
        }
    }

    /// Return coalesced flow-control credits for multiple channels in a single lock & flush.
    pub fn ack_credits(&self, credits: &[(u32, u32)]) {
        if !credits.iter().any(|&(_, bytes)| bytes > 0) {
            return;
        }
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        for &(channel, bytes) in credits {
            if bytes > 0 {
                let hdr = wire::encode_header(channel, wire::CREDIT, 4);
                let _ = w.write_all(&hdr);
                let _ = w.write_all(&bytes.to_le_bytes());
            }
        }
        let _ = w.flush();
    }

    /// Send input bytes to an attached terminal channel.
    pub fn send_input(&self, channel: u32, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if self.send_frame(channel, wire::DATA, bytes).is_ok() {
            let mut guard = self
                .last_pty_input
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let next_seq = guard.map_or(1, |prev| prev.seq.wrapping_add(1));
            *guard = Some(LastPtyInput {
                seq: next_seq,
                channel,
                at: Instant::now(),
            });
        }
    }

    /// Snapshot of the most recent user-forwarded PTY input chunk.
    pub fn last_pty_input(&self) -> Option<LastPtyInput> {
        *self
            .last_pty_input
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Send an asynchronous `terminal.read` JSON-RPC request without blocking the UI thread.
    /// Returns the assigned request ID when dispatched, so the reader thread delivers
    /// `CoreMessage::TerminalRead` once `herdr-core` replies.
    pub fn request_terminal_read(&self, terminal: &str, lines: usize) -> Option<u64> {
        #[cfg(test)]
        if self.skip_rpc_for_tests {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "terminal.read",
            "params": { "terminal": terminal, "lines": lines },
        });
        let bytes = serde_json::to_vec(&req).ok()?;
        let now = Instant::now();
        {
            let mut map = self
                .pending_async_reads
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            map.retain(|_, (_, at)| now.saturating_duration_since(*at) < Duration::from_secs(10));
            if map.len() >= 64 {
                if let Some(&oldest_id) = map.keys().min() {
                    map.remove(&oldest_id);
                }
            }
            map.insert(id, (terminal.to_string(), now));
        }
        if self.send_frame(0, wire::DATA, &bytes).is_err() {
            self.pending_async_reads
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return None;
        }
        Some(id)
    }

    /// Drop an expired or superseded `terminal.read` tracking entry so a silent core cannot leak map entries.
    pub fn cancel_terminal_read(&self, request_id: u64) {
        self.pending_async_reads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&request_id);
    }

    /// Resize an attached terminal channel.
    pub fn resize_channel(&self, channel: u32, cols: u16, rows: u16) {
        let mut payload = [0u8; 4];
        payload[0..2].copy_from_slice(&cols.to_le_bytes());
        payload[2..4].copy_from_slice(&rows.to_le_bytes());
        let _ = self.send_frame(channel, wire::RESIZE, &payload);
    }

    #[cfg(test)]
    pub(crate) fn set_skip_rpc_for_tests(&mut self, skip: bool) {
        self.skip_rpc_for_tests = skip;
    }

    #[cfg(test)]
    pub(crate) fn dummy_for_tests() -> TestCoreClient {
        let mut child = Command::new("sh")
            .args(["-c", "cat >/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn dummy sink");
        let stdin = child.stdin.take().expect("dummy stdin");
        TestCoreClient(Self {
            _child: child,
            writer: Arc::new(Mutex::new(BufWriter::new(stdin))),
            next_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            pending_async_reads: Arc::new(Mutex::new(HashMap::new())),
            events_channel: Arc::new(Mutex::new(None)),
            last_pty_input: Mutex::new(None),
            skip_rpc_for_tests: true,
        })
    }
}

#[cfg(test)]
pub(crate) struct TestCoreClient(pub(crate) CoreClient);

#[cfg(test)]
impl std::ops::Deref for TestCoreClient {
    type Target = CoreClient;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TestCoreClient {
    fn drop(&mut self) {
        let _ = self.0._child.kill();
        let _ = self.0._child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_terminal_read_result_complete_and_partial_payloads() {
        let complete = json!({
            "text": "> hi\nhello",
            "complete": true,
            "how": "stitched",
            "why": null
        });
        assert_eq!(
            TerminalReadResult::from_value(&complete).unwrap(),
            TerminalReadResult {
                text: "> hi\nhello".to_string(),
                complete: true,
                how: "stitched".to_string(),
                why: None,
            }
        );

        let partial = json!({
            "text": "working output",
            "complete": false,
            "how": "screen",
            "why": "working"
        });
        assert_eq!(
            TerminalReadResult::from_value(&partial).unwrap(),
            TerminalReadResult {
                text: "working output".to_string(),
                complete: false,
                how: "screen".to_string(),
                why: Some("working".to_string()),
            }
        );

        let invalid = json!({ "complete": true, "how": "screen" });
        assert!(TerminalReadResult::from_value(&invalid).is_err());
    }

    #[test]
    fn async_terminal_read_dispatches_non_blocking_request_and_handles_ok_and_error_replies() {
        let mut core = CoreClient::dummy_for_tests();
        assert_eq!(core.request_terminal_read("t1", 200), None);

        // Enable real frame sending against dummy stdin sink to verify non-blocking dispatch + reply routing.
        core.0.skip_rpc_for_tests = false;
        let id_ok = core
            .request_terminal_read("t1", 450)
            .expect("non-blocking request_terminal_read returns id immediately");
        let id_err = core
            .request_terminal_read("t2", 200)
            .expect("second request returns unique ascending id");
        assert!(id_err > id_ok);

        let (tx, rx) = mpsc::channel();
        let events_ch = Mutex::new(None);
        handle_rpc_reply_value(
            json!({
                "jsonrpc": "2.0",
                "id": id_ok,
                "result": {
                    "text": "> hi\nanswer",
                    "complete": false,
                    "how": "screen",
                    "why": "working"
                }
            }),
            &core.pending,
            &core.pending_async_reads,
            &events_ch,
            &tx,
            &|m| m,
        );
        handle_rpc_reply_value(
            json!({
                "jsonrpc": "2.0",
                "id": id_err,
                "error": {
                    "message": "terminal vanished",
                    "data": { "kind": "not_found" }
                }
            }),
            &core.pending,
            &core.pending_async_reads,
            &events_ch,
            &tx,
            &|m| m,
        );
        // Unknown ID does not emit any message
        handle_rpc_reply_value(
            json!({ "jsonrpc": "2.0", "id": 99999, "result": {} }),
            &core.pending,
            &core.pending_async_reads,
            &events_ch,
            &tx,
            &|m| m,
        );

        match rx.try_recv().unwrap() {
            CoreMessage::TerminalRead {
                request_id,
                terminal,
                result: Ok(res),
            } => {
                assert_eq!(request_id, id_ok);
                assert_eq!(terminal, "t1");
                assert_eq!(res.text, "> hi\nanswer");
                assert!(!res.complete);
                assert_eq!(res.how, "screen");
                assert_eq!(res.why.as_deref(), Some("working"));
            }
            other => panic!("unexpected msg: {other:?}"),
        }
        match rx.try_recv().unwrap() {
            CoreMessage::TerminalRead {
                request_id,
                terminal,
                result: Err(err),
            } => {
                assert_eq!(request_id, id_err);
                assert_eq!(terminal, "t2");
                assert_eq!(err, "not_found: terminal vanished");
            }
            other => panic!("unexpected msg: {other:?}"),
        }
        assert!(rx.try_recv().is_err());
        assert!(core.pending_async_reads.lock().unwrap().is_empty());

        // cancel_terminal_read removes entry cleanly and aged >10s entries prune on subsequent request
        let id_cancel = core.request_terminal_read("t1", 100).unwrap();
        assert_eq!(core.pending_async_reads.lock().unwrap().len(), 1);
        core.cancel_terminal_read(id_cancel);
        assert!(core.pending_async_reads.lock().unwrap().is_empty());

        core.pending_async_reads.lock().unwrap().insert(
            777,
            (
                "stale".to_string(),
                Instant::now() - Duration::from_secs(11),
            ),
        );
        let id_fresh = core.request_terminal_read("t1", 100).unwrap();
        let map = core.pending_async_reads.lock().unwrap();
        assert!(!map.contains_key(&777));
        assert!(map.contains_key(&id_fresh));
    }

    #[test]
    fn parses_attach_channel_info_with_and_without_terminal_size() {
        let full = json!({
            "channel": 9,
            "terminal": { "address": "t1", "cols": 80, "rows": 24 }
        });
        assert_eq!(
            parse_attach_channel_info(&full).unwrap(),
            AttachedChannelInfo {
                channel: 9,
                cols: Some(80),
                rows: Some(24),
            }
        );

        let zero_dim = json!({
            "channel": 3,
            "terminal": { "cols": 0, "rows": 40 }
        });
        assert_eq!(
            parse_attach_channel_info(&zero_dim).unwrap(),
            AttachedChannelInfo {
                channel: 3,
                cols: None,
                rows: Some(40),
            }
        );

        let missing_channel = json!({ "terminal": { "cols": 80, "rows": 24 } });
        assert!(parse_attach_channel_info(&missing_channel).is_err());
    }
}
