//! Focused-agent quota & usage tracker for the 1-row read-only bottom status bar.
//! Collects 5h / 7d / daily quota usage strictly from local agent logs, databases, and files
//! in a background worker thread so TUI rendering never blocks and no OAuth credentials,
//! system keyrings, binaries, or external network endpoints are accessed.

use crate::keys::QuotaFileConfig;
use crate::model::{TerminalInfo, World};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CACHE_TTL: Duration = Duration::from_secs(10);
const EMPTY_RETRY_TTL: Duration = Duration::from_secs(5);
const QUOTA_LOCAL_HELPER_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_HELPER_STDOUT_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentKind {
    Codex,
    Claude,
    OpenCode,
    Antigravity,
    Freebuff,
}

impl AgentKind {
    pub fn display_name(self) -> &'static str {
        match self {
            AgentKind::Codex => "Codex",
            AgentKind::Claude => "Claude",
            AgentKind::OpenCode => "OpenCode",
            AgentKind::Antigravity => "Antigravity",
            AgentKind::Freebuff => "Freebuff",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct QuotaMetric {
    pub window_label: String,
    /// Remaining percentage (`0.0..=100.0`).
    pub remaining_percent: f64,
    /// Optional usage-only display text (e.g. `"21.1M"` or `"559步"`).
    /// When present, the bottom bar renders this value directly without a progress bar or `%`.
    pub value_text: Option<String>,
    /// Optional compact suffix such as reset countdown (`"2h14m"`) or count (`"28/200 · 142k"`).
    pub detail: Option<String>,
}

impl QuotaMetric {
    pub fn usage(
        window_label: impl Into<String>,
        value_text: impl Into<String>,
        detail: Option<String>,
    ) -> Self {
        Self {
            window_label: window_label.into(),
            remaining_percent: 0.0,
            value_text: Some(value_text.into()),
            detail,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentQuota {
    pub agent: AgentKind,
    pub metrics: Vec<QuotaMetric>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct AgySessionHint {
    pub conversation_id: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
}

type CacheKey = (AgentKind, Option<String>);

#[derive(Clone, Debug)]
struct QuotaRequest {
    cache_key: CacheKey,
    kind: AgentKind,
    agy_hint: Option<AgySessionHint>,
}

#[derive(Clone, Debug, PartialEq)]
enum QuotaFetchOutcome {
    /// Definitive quota snapshot (possibly empty when credentials/files are absent or a session
    /// such as Freebuff has expired and must clear any previously cached metrics).
    Authoritative(Vec<QuotaMetric>),
    /// Local SQLite helper timed out or transiently failed; retain last-known-good cached metrics
    /// if present and retry shortly.
    TransientError,
}

struct CacheEntry {
    fetched_at: Instant,
    ttl: Duration,
    quota: AgentQuota,
}

#[derive(Default)]
struct TrackerState {
    cache: HashMap<CacheKey, CacheEntry>,
    in_flight: HashSet<CacheKey>,
}

pub struct UsageTracker {
    state: Arc<Mutex<TrackerState>>,
    revision: Arc<AtomicU64>,
    req_tx: SyncSender<QuotaRequest>,
}

impl UsageTracker {
    #[allow(dead_code)]
    pub fn new(cfg: QuotaFileConfig) -> Self {
        Self::new_with_waker(cfg, || {})
    }

    pub fn new_with_waker<F>(cfg: QuotaFileConfig, waker: F) -> Self
    where
        F: Fn() + Send + 'static,
    {
        Self::new_with_collector(
            move |kind, agy_hint, claude_cache, agy_cache| {
                collect_quota_for_agent(kind, agy_hint, &cfg, claude_cache, agy_cache)
            },
            waker,
        )
    }

    fn new_with_collector<C, F>(mut collector: C, waker: F) -> Self
    where
        C: FnMut(
                AgentKind,
                Option<&AgySessionHint>,
                &mut ClaudeFileCache,
                &mut AgyFileCache,
            ) -> QuotaFetchOutcome
            + Send
            + 'static,
        F: Fn() + Send + 'static,
    {
        let state = Arc::new(Mutex::new(TrackerState::default()));
        let revision = Arc::new(AtomicU64::new(0));
        let (req_tx, req_rx) = mpsc::sync_channel::<QuotaRequest>(16);
        let state_bg = Arc::clone(&state);
        let rev_bg = Arc::clone(&revision);

        std::thread::spawn(move || {
            let mut claude_cache = ClaudeFileCache::default();
            let mut agy_cache = AgyFileCache::default();
            while let Ok(req) = req_rx.recv() {
                let should_collect = {
                    let guard = state_bg.lock().unwrap();
                    guard
                        .cache
                        .get(&req.cache_key)
                        .is_none_or(|entry| entry.fetched_at.elapsed() >= entry.ttl)
                };
                if !should_collect {
                    let mut guard = state_bg.lock().unwrap();
                    guard.in_flight.remove(&req.cache_key);
                    continue;
                }
                let outcome = collector(
                    req.kind,
                    req.agy_hint.as_ref(),
                    &mut claude_cache,
                    &mut agy_cache,
                );
                let normal_ttl = CACHE_TTL;
                {
                    let mut guard = state_bg.lock().unwrap();
                    guard.in_flight.remove(&req.cache_key);
                    let now = Instant::now();
                    let next_entry = match outcome {
                        QuotaFetchOutcome::Authoritative(metrics) => {
                            let ttl = if metrics.is_empty() {
                                EMPTY_RETRY_TTL
                            } else {
                                normal_ttl
                            };
                            CacheEntry {
                                fetched_at: now,
                                ttl,
                                quota: AgentQuota {
                                    agent: req.kind,
                                    metrics,
                                },
                            }
                        }
                        QuotaFetchOutcome::TransientError => {
                            if let Some(prev) = guard
                                .cache
                                .get(&req.cache_key)
                                .filter(|e| !e.quota.metrics.is_empty())
                            {
                                CacheEntry {
                                    fetched_at: now,
                                    ttl: EMPTY_RETRY_TTL,
                                    quota: prev.quota.clone(),
                                }
                            } else {
                                CacheEntry {
                                    fetched_at: now,
                                    ttl: EMPTY_RETRY_TTL,
                                    quota: AgentQuota {
                                        agent: req.kind,
                                        metrics: Vec::new(),
                                    },
                                }
                            }
                        }
                    };
                    if guard.cache.len() > 64 {
                        guard.cache.retain(|_, entry| {
                            entry.fetched_at.elapsed() < Duration::from_secs(600)
                        });
                    }
                    guard.cache.insert(req.cache_key, next_entry);
                }
                rev_bg.fetch_add(1, Ordering::Release);
                waker();
            }
        });

        Self {
            state,
            revision,
            req_tx,
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub fn focused_quota(&self, world: &World) -> Option<AgentQuota> {
        let addr = world.active_terminal_address_ref()?;
        let info = world.terminals.get(addr)?;
        let kind = detect_agent_from_terminal(info)?;
        let (cache_key, agy_hint) = if kind == AgentKind::Antigravity {
            (
                (kind, Some(info.address.clone())),
                Some(extract_agy_session_hint(info)),
            )
        } else {
            ((kind, None), None)
        };

        let mut enqueue = false;
        let cached = {
            let mut guard = self.state.lock().unwrap();
            let (entry_opt, expired) = match guard.cache.get(&cache_key) {
                Some(entry) => (
                    Some(entry.quota.clone()),
                    entry.fetched_at.elapsed() >= entry.ttl,
                ),
                None => (None, true),
            };
            if expired && !guard.in_flight.contains(&cache_key) {
                guard.in_flight.insert(cache_key.clone());
                enqueue = true;
            }
            entry_opt
        };

        if enqueue {
            let req = QuotaRequest {
                cache_key: cache_key.clone(),
                kind,
                agy_hint,
            };
            if self.req_tx.try_send(req).is_err() {
                let mut guard = self.state.lock().unwrap();
                guard.in_flight.remove(&cache_key);
            }
        }

        Some(cached.unwrap_or_else(|| AgentQuota {
            agent: kind,
            metrics: Vec::new(),
        }))
    }
}

pub fn extract_conversation_id_from_argv(argv: &[String]) -> Option<String> {
    let mut iter = argv.iter();
    while let Some(arg) = iter.next() {
        let trimmed = arg.trim();
        if let Some(val) = trimmed.strip_prefix("--conversation=") {
            let id = val.trim();
            if !id.is_empty() {
                return Some(id.to_string());
            }
        } else if trimmed == "--conversation" {
            if let Some(next) = iter.next() {
                let id = next.trim();
                if !id.is_empty() {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

fn extract_agy_session_hint(info: &TerminalInfo) -> AgySessionHint {
    let conversation_id = extract_conversation_id_from_argv(&info.fg_argv);
    let cwd = info
        .fg_cwd
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let c = info.cwd.trim();
            (!c.is_empty()).then_some(c)
        })
        .map(String::from);
    let title = {
        let t = info.title.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    AgySessionHint {
        conversation_id,
        cwd,
        title,
    }
}

#[cfg(test)]
pub fn detect_focused_agent(world: &World) -> Option<AgentKind> {
    let addr = world.active_terminal_address_ref()?;
    let info = world.terminals.get(addr)?;
    detect_agent_from_terminal(info)
}

pub fn detect_agent_from_terminal(info: &TerminalInfo) -> Option<AgentKind> {
    for candidate in [
        info.status_app.as_deref(),
        info.fg_name.as_deref(),
        info.name.as_deref(),
        Some(info.title.as_str()),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(kind) = detect_agent_from_str(candidate) {
            return Some(kind);
        }
    }
    None
}

pub fn detect_agent_from_str(s: &str) -> Option<AgentKind> {
    let lower = s.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    if lower.contains("codex") {
        Some(AgentKind::Codex)
    } else if lower.contains("claude") {
        Some(AgentKind::Claude)
    } else if lower.contains("opencode") {
        Some(AgentKind::OpenCode)
    } else if lower.contains("antigravity")
        || lower == "agy"
        || lower.starts_with("agy ")
        || lower.contains("/agy")
        || lower.contains("gemini")
    {
        Some(AgentKind::Antigravity)
    } else if lower.contains("freebuff") {
        Some(AgentKind::Freebuff)
    } else {
        None
    }
}

pub fn render_progress_bar(percent: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let clamped = percent.clamp(0.0, 100.0);
    let total_eighths = ((clamped / 100.0) * (width as f64) * 8.0).round() as usize;
    let full_cells = (total_eighths / 8).min(width);
    let rem_eighth = total_eighths % 8;
    const PARTIALS: [char; 8] = ['░', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];

    let mut out = String::with_capacity(width * 3);
    for i in 0..width {
        if i < full_cells {
            out.push('█');
        } else if i == full_cells && rem_eighth > 0 {
            out.push(PARTIALS[rem_eighth]);
        } else {
            out.push('░');
        }
    }
    out
}

/// Dither-free solid-block progress bar (`█` / `─`) for 300 PPI monochrome E-Ink panels.
pub fn render_eink_progress_bar(percent: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let clamped = percent.clamp(0.0, 100.0);
    let filled = (((clamped / 100.0) * (width as f64)).round() as usize).min(width);
    let mut out = String::with_capacity(width * 3);
    for i in 0..width {
        out.push(if i < filled { '█' } else { '─' });
    }
    out
}

pub fn format_duration_short(secs: u64) -> String {
    if secs == 0 {
        return "0m".to_string();
    }
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        if hours > 0 {
            format!("{days}d{hours}h")
        } else {
            format!("{days}d")
        }
    } else if hours > 0 {
        if mins > 0 {
            format!("{hours}h{mins:02}m")
        } else {
            format!("{hours}h")
        }
    } else {
        format!("{}m", mins.max(1))
    }
}

pub fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000_000 {
        format!("{:.1}B", tokens as f64 / 1_000_000_000.0)
    } else if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", (tokens + 500) / 1_000)
    } else {
        format!("{tokens}")
    }
}

pub fn format_steps_compact(steps: u64) -> String {
    if steps >= 10_000 {
        format!("{:.1}k步", steps as f64 / 1_000.0)
    } else {
        format!("{steps}步")
    }
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn parse_utc_iso8601_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date_part, rest) = s.split_once('T').or_else(|| s.split_once(' '))?;
    let (time_part, offset_secs): (&str, i64) =
        if let Some(t) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
            (t, 0)
        } else {
            let idx = rest.rfind(['+', '-'])?;
            let (t, off_str) = rest.split_at(idx);
            let sign: i64 = if off_str.starts_with('-') { -1 } else { 1 };
            let off_body = &off_str[1..];
            let (oh, om) = if let Some((h, m)) = off_body.split_once(':') {
                (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?)
            } else if off_body.len() == 4 {
                (
                    off_body[..2].parse::<i64>().ok()?,
                    off_body[2..].parse::<i64>().ok()?,
                )
            } else {
                return None;
            };
            (t, sign * (oh * 3_600 + om * 60))
        };

    let mut d_it = date_part.split('-');
    let year: i64 = d_it.next()?.parse().ok()?;
    let month: i64 = d_it.next()?.parse().ok()?;
    let day: i64 = d_it.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut t_it = time_part.split(':');
    let hour: i64 = t_it.next()?.parse().ok()?;
    let min: i64 = t_it.next()?.parse().ok()?;
    let sec_str = t_it.next()?;
    let sec_int_str = sec_str.split('.').next().unwrap_or(sec_str);
    let sec: i64 = sec_int_str.parse().ok()?;

    // Howard Hinnant's days_from_civil algorithm
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let utc_secs = days * 86_400 + hour * 3_600 + min * 60 + sec - offset_secs;
    u64::try_from(utc_secs).ok()
}

fn kill_helper_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        const SIGKILL: i32 = 9;
        if let Ok(pgid) = i32::try_from(child.id()) {
            if pgid > 0 {
                // SAFETY: `child` was spawned with `process_group(0)` so `pgid` is its isolated
                // process group ID (`> 0`). Sending `SIGKILL` to `-pgid` terminates both the
                // helper and any descendants holding open the stdout pipe.
                unsafe {
                    let _ = kill(-pgid, SIGKILL);
                }
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn run_command_stdout_with_timeout<I, S>(prog: &str, args: I, timeout: Duration) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let res = stdout
            .take(MAX_HELPER_STDOUT_BYTES)
            .read_to_end(&mut buf)
            .map(|_| buf);
        let _ = tx.send(res);
    });

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    kill_helper_process_group(&mut child);
                    return None;
                }
                let rem = deadline
                    .saturating_duration_since(Instant::now())
                    .clamp(Duration::from_millis(20), Duration::from_millis(250));
                let Ok(read_res) = rx.recv_timeout(rem) else {
                    kill_helper_process_group(&mut child);
                    return None;
                };
                let buf = read_res.ok()?;
                return Some(String::from_utf8_lossy(&buf).into_owned());
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_helper_process_group(&mut child);
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                kill_helper_process_group(&mut child);
                return None;
            }
        }
    }
}

fn local_utc_offset_secs() -> i64 {
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| {
        let Some(s) = run_command_stdout_with_timeout("date", ["+%z"], Duration::from_secs(2))
        else {
            return 0;
        };
        let trimmed = s.trim();
        if trimmed.len() >= 5 {
            let sign: i64 = if trimmed.starts_with('-') { -1 } else { 1 };
            let hh = trimmed[1..3].parse::<i64>().unwrap_or(0);
            let mm = trimmed[3..5].parse::<i64>().unwrap_or(0);
            sign * (hh * 3_600 + mm * 60)
        } else {
            0
        }
    })
}

pub fn today_midnight_unix_secs(now_secs: u64, utc_offset_secs: i64) -> u64 {
    let local_secs = (now_secs as i64).saturating_add(utc_offset_secs);
    let local_midnight = local_secs - local_secs.rem_euclid(86_400);
    local_midnight.saturating_sub(utc_offset_secs).max(0) as u64
}

fn collect_quota_for_agent(
    kind: AgentKind,
    agy_hint: Option<&AgySessionHint>,
    cfg: &QuotaFileConfig,
    claude_cache: &mut ClaudeFileCache,
    agy_cache: &mut AgyFileCache,
) -> QuotaFetchOutcome {
    let now_secs = now_unix_secs();
    match kind {
        AgentKind::Codex => QuotaFetchOutcome::Authoritative(collect_codex_metrics(now_secs)),
        AgentKind::Claude => {
            QuotaFetchOutcome::Authoritative(collect_claude_metrics(now_secs, cfg, claude_cache))
        }
        AgentKind::OpenCode => collect_opencode_metrics(now_secs, cfg),
        AgentKind::Antigravity => QuotaFetchOutcome::Authoritative(collect_antigravity_metrics(
            now_secs, agy_hint, agy_cache,
        )),
        AgentKind::Freebuff => QuotaFetchOutcome::Authoritative(collect_freebuff_metrics(now_secs)),
    }
}

fn collect_recursive_files(
    dir: &Path,
    ext: &str,
    min_mtime: Option<SystemTime>,
    out: &mut Vec<(SystemTime, PathBuf)>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            collect_recursive_files(&path, ext, min_mtime, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some(ext) {
            let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
            if min_mtime.is_none_or(|min_t| mtime >= min_t) {
                out.push((mtime, path));
            }
        }
    }
}

fn read_tail_string(path: &Path, max_bytes: u64) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len > max_bytes {
        let _ = f.seek(SeekFrom::Start(len - max_bytes));
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

pub fn parse_codex_rate_limits_snapshot_from_jsonl(
    text: &str,
    now_secs: u64,
) -> Option<(Option<u64>, Vec<QuotaMetric>)> {
    let mut best: Option<(Option<u64>, usize, Vec<QuotaMetric>)> = None;
    for (idx, line) in text.lines().enumerate() {
        if !line.contains("\"rate_limits\"") {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(rl) = val
            .get("payload")
            .and_then(|p| p.get("rate_limits"))
            .or_else(|| val.get("rate_limits"))
        else {
            continue;
        };
        if rl.is_null() {
            continue;
        }
        let event_ts = val
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_utc_iso8601_secs)
            .or_else(|| val.get("timestamp").and_then(Value::as_u64));
        let mut metrics = Vec::new();
        for (key, default_lbl) in [("primary", "5h"), ("secondary", "7d")] {
            let Some(win) = rl.get(key).filter(|v| v.is_object()) else {
                continue;
            };
            let Some(raw_used_pct) = win
                .get("used_percent")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite())
            else {
                continue;
            };
            let win_mins = win
                .get("window_minutes")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let lbl = match win_mins {
                300 => "5h".to_string(),
                10080 => "7d".to_string(),
                1440 => "1d".to_string(),
                m if m > 0 && m % 60 == 0 => format!("{}h", m / 60),
                _ => default_lbl.to_string(),
            };
            let raw_rem_pct = (100.0 - raw_used_pct).clamp(0.0, 100.0);
            let resets_at = win.get("resets_at").and_then(Value::as_u64).unwrap_or(0);
            let (rem_pct, detail) = if resets_at > 0 && now_secs >= resets_at {
                (100.0, None)
            } else if resets_at > now_secs {
                (
                    raw_rem_pct,
                    Some(format_duration_short(resets_at - now_secs)),
                )
            } else {
                (raw_rem_pct, None)
            };
            metrics.push(QuotaMetric {
                window_label: lbl,
                remaining_percent: rem_pct,
                value_text: None,
                detail,
            });
        }
        if !metrics.is_empty() {
            let replace = match &best {
                None => true,
                Some((prev_ts, prev_idx, _)) => {
                    (event_ts.unwrap_or(0), idx) >= (prev_ts.unwrap_or(0), *prev_idx)
                }
            };
            if replace {
                best = Some((event_ts, idx, metrics));
            }
        }
    }
    best.map(|(ts, _, m)| (ts, m))
}

#[cfg(test)]
pub fn parse_codex_rate_limits_from_jsonl(text: &str, now_secs: u64) -> Option<Vec<QuotaMetric>> {
    parse_codex_rate_limits_snapshot_from_jsonl(text, now_secs).map(|(_, m)| m)
}

pub fn collect_codex_metrics_from_sessions_dir(
    sessions_dir: &Path,
    now_secs: u64,
) -> Vec<QuotaMetric> {
    let mut files = Vec::new();
    let min_mtime = UNIX_EPOCH + Duration::from_secs(now_secs.saturating_sub(7 * 86_400));
    collect_recursive_files(sessions_dir, "jsonl", Some(min_mtime), &mut files);
    files.sort_by_key(|a| std::cmp::Reverse(a.0));

    let mut best: Option<((u64, SystemTime), Vec<QuotaMetric>)> = None;
    for (mtime, path) in files.into_iter().take(12) {
        if let Some(tail) = read_tail_string(&path, 256 * 1024) {
            if let Some((event_ts, metrics)) =
                parse_codex_rate_limits_snapshot_from_jsonl(&tail, now_secs)
            {
                let rank = (event_ts.unwrap_or(0), mtime);
                if best.as_ref().is_none_or(|(prev_rank, _)| rank > *prev_rank) {
                    best = Some((rank, metrics));
                }
            }
        }
    }
    best.map(|(_, m)| m).unwrap_or_default()
}

fn codex_home_dir() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".codex")))
}

fn collect_codex_metrics(now_secs: u64) -> Vec<QuotaMetric> {
    let Some(codex_home) = codex_home_dir() else {
        return Vec::new();
    };
    let sessions_dir = codex_home.join("sessions");
    collect_codex_metrics_from_sessions_dir(&sessions_dir, now_secs)
}

fn collect_opencode_metrics(_now_secs: u64, cfg: &QuotaFileConfig) -> QuotaFetchOutcome {
    let Some(home) = home_dir() else {
        return QuotaFetchOutcome::Authoritative(Vec::new());
    };
    let db_path = home.join(".local/share/opencode/opencode.db");
    if !db_path.is_file() {
        return QuotaFetchOutcome::Authoritative(Vec::new());
    }
    let daily_limit = cfg.opencode_daily_requests.unwrap_or(200).max(1);
    let limit_5h = cfg.opencode_5h_requests.unwrap_or(80).max(1);

    let script = r#"
import sqlite3, json, sys, time, datetime
db_path = sys.argv[1]
now_ms = int(time.time() * 1000)
now_dt = datetime.datetime.now()
midnight_dt = now_dt.replace(hour=0, minute=0, second=0, microsecond=0)
midnight_ms = int(midnight_dt.timestamp() * 1000)
five_h_ms = now_ms - 5 * 3600 * 1000
conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True, timeout=1.0)
rows = conn.execute(
    "SELECT time_created, data FROM session_message WHERE type='assistant' AND time_created >= ?",
    (min(midnight_ms, five_h_ms),)
).fetchall()
day_reqs = 0
day_toks = 0
five_reqs = 0
five_toks = 0
for ts, raw in rows:
    try:
        d = json.loads(raw)
    except Exception:
        continue
    toks_obj = d.get("tokens") or {}
    t = int(toks_obj.get("input") or 0) + int(toks_obj.get("output") or 0) + int(toks_obj.get("reasoning") or 0)
    if ts >= midnight_ms:
        day_reqs += 1
        day_toks += t
    if ts >= five_h_ms:
        five_reqs += 1
        five_toks += t
print(json.dumps({"day_reqs": day_reqs, "day_toks": day_toks, "five_reqs": five_reqs, "five_toks": five_toks}))
"#;

    let Some(stdout) = run_command_stdout_with_timeout(
        "python3",
        [OsStr::new("-c"), OsStr::new(script), db_path.as_os_str()],
        QUOTA_LOCAL_HELPER_TIMEOUT,
    ) else {
        return QuotaFetchOutcome::TransientError;
    };
    let Ok(val) = serde_json::from_str::<Value>(&stdout) else {
        return QuotaFetchOutcome::TransientError;
    };
    let day_reqs = val.get("day_reqs").and_then(Value::as_u64).unwrap_or(0);
    let day_toks = val.get("day_toks").and_then(Value::as_u64).unwrap_or(0);
    let five_reqs = val.get("five_reqs").and_then(Value::as_u64).unwrap_or(0);
    let five_toks = val.get("five_toks").and_then(Value::as_u64).unwrap_or(0);

    let day_used_pct = ((day_reqs as f64 / daily_limit as f64) * 100.0).clamp(0.0, 100.0);
    let five_used_pct = ((five_reqs as f64 / limit_5h as f64) * 100.0).clamp(0.0, 100.0);

    QuotaFetchOutcome::Authoritative(vec![
        QuotaMetric {
            window_label: "今日".to_string(),
            remaining_percent: (100.0 - day_used_pct).clamp(0.0, 100.0),
            value_text: None,
            detail: Some(format!(
                "{day_reqs}/{daily_limit} · {}",
                format_tokens(day_toks)
            )),
        },
        QuotaMetric {
            window_label: "5h".to_string(),
            remaining_percent: (100.0 - five_used_pct).clamp(0.0, 100.0),
            value_text: None,
            detail: Some(format!(
                "{five_reqs}/{limit_5h} · {}",
                format_tokens(five_toks)
            )),
        },
    ])
}

type ClaudeFileEntry = (SystemTime, u64, Vec<(String, u64, u64)>);

#[derive(Default)]
pub struct ClaudeFileCache {
    entries: HashMap<PathBuf, ClaudeFileEntry>,
}

fn parse_claude_file_records(
    path: &Path,
    mtime_secs: u64,
    cut_7d: u64,
) -> Option<Vec<(String, u64, u64)>> {
    let bytes = fs::read(path).ok()?;
    let content = String::from_utf8_lossy(&bytes);
    let path_tag = path.to_string_lossy();
    let mut file_responses: HashMap<String, (u64, u64)> = HashMap::new();

    for (line_idx, line) in content.lines().enumerate() {
        if !line.contains("\"usage\"") {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let msg = obj.get("message");
        let Some(u) = msg
            .and_then(|m| m.get("usage"))
            .or_else(|| obj.get("usage"))
        else {
            continue;
        };
        if !u.is_object() {
            continue;
        }
        let ts = obj
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_utc_iso8601_secs)
            .or_else(|| obj.get("timestamp").and_then(Value::as_u64))
            .unwrap_or(mtime_secs);
        if ts < cut_7d {
            continue;
        }
        let t = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0)
            + u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0)
            + u.get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);

        let msg_id = msg
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let req_id = obj
            .get("requestId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let resp_key = if !msg_id.is_empty() || !req_id.is_empty() {
            format!("resp:{msg_id}:{req_id}")
        } else if let Some(uuid) = obj
            .get("uuid")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            format!("uuid:{uuid}")
        } else {
            format!("anon:{path_tag}:{line_idx}")
        };

        file_responses
            .entry(resp_key)
            .and_modify(|(prev_ts, prev_toks)| {
                *prev_ts = (*prev_ts).min(ts);
                *prev_toks = (*prev_toks).max(t);
            })
            .or_insert((ts, t));
    }

    Some(
        file_responses
            .into_iter()
            .map(|(k, (ts, t))| (k, ts, t))
            .collect(),
    )
}

#[allow(dead_code)]
pub fn collect_claude_metrics_from_dir(
    projects_dir: &Path,
    now_secs: u64,
    cfg: &QuotaFileConfig,
) -> Vec<QuotaMetric> {
    let mut cache = ClaudeFileCache::default();
    collect_claude_metrics_from_dir_cached(projects_dir, now_secs, cfg, &mut cache)
}

pub fn collect_claude_metrics_from_dir_cached(
    projects_dir: &Path,
    now_secs: u64,
    cfg: &QuotaFileConfig,
    cache: &mut ClaudeFileCache,
) -> Vec<QuotaMetric> {
    if !projects_dir.is_dir() {
        return Vec::new();
    }
    let limit_5h_toks = cfg.claude_5h_tokens.unwrap_or(2_500_000).max(1);
    let limit_5h_msgs = cfg.claude_5h_messages.unwrap_or(45).max(1);
    let limit_7d_toks = cfg.claude_7d_tokens.unwrap_or(25_000_000).max(1);
    let limit_7d_msgs = cfg.claude_7d_messages.unwrap_or(450).max(1);

    let cut_7d = now_secs.saturating_sub(7 * 86_400);
    let cut_5h = now_secs.saturating_sub(5 * 3_600);
    let min_mtime = UNIX_EPOCH + Duration::from_secs(cut_7d);

    let mut files = Vec::new();
    collect_recursive_files(projects_dir, "jsonl", Some(min_mtime), &mut files);

    if cache.entries.len() > 512 {
        cache.entries.retain(|_, (mt, _, _)| *mt >= min_mtime);
    }

    // Map response identity (`message.id` / `requestId`) -> (earliest_ts, cumulative_tokens)
    let mut responses: HashMap<String, (u64, u64)> = HashMap::new();
    for (mtime, path) in files {
        let mtime_secs = mtime
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(now_secs);
        let Ok(file_len) = fs::metadata(&path).map(|m| m.len()) else {
            continue;
        };
        let records = match cache.entries.get(&path) {
            Some((cached_mtime, cached_len, recs))
                if *cached_mtime == mtime && *cached_len == file_len =>
            {
                recs.as_slice()
            }
            _ => {
                let Some(parsed) = parse_claude_file_records(&path, mtime_secs, cut_7d) else {
                    continue;
                };
                cache
                    .entries
                    .insert(path.clone(), (mtime, file_len, parsed));
                cache.entries.get(&path).unwrap().2.as_slice()
            }
        };

        for (resp_key, ts, t) in records {
            if *ts < cut_7d {
                continue;
            }
            responses
                .entry(resp_key.clone())
                .and_modify(|(prev_ts, prev_toks)| {
                    *prev_ts = (*prev_ts).min(*ts);
                    *prev_toks = (*prev_toks).max(*t);
                })
                .or_insert((*ts, *t));
        }
    }

    let mut tok_7d = 0u64;
    let mut msg_7d = 0u64;
    let mut tok_5h = 0u64;
    let mut msg_5h = 0u64;
    let mut oldest_5h: Option<u64> = None;

    for &(ts, t) in responses.values() {
        if ts < cut_7d {
            continue;
        }
        tok_7d += t;
        msg_7d += 1;
        if ts >= cut_5h {
            tok_5h += t;
            msg_5h += 1;
            oldest_5h = Some(oldest_5h.map_or(ts, |old| old.min(ts)));
        }
    }

    let reset_5h = oldest_5h
        .map(|oldest| (oldest + 5 * 3_600).saturating_sub(now_secs))
        .unwrap_or(0);

    let used_5h =
        ((tok_5h as f64 / limit_5h_toks as f64).max(msg_5h as f64 / limit_5h_msgs as f64) * 100.0)
            .clamp(0.0, 100.0);
    let used_7d =
        ((tok_7d as f64 / limit_7d_toks as f64).max(msg_7d as f64 / limit_7d_msgs as f64) * 100.0)
            .clamp(0.0, 100.0);

    let detail_5h = if reset_5h > 0 {
        format!(
            "{} · {}",
            format_tokens(tok_5h),
            format_duration_short(reset_5h)
        )
    } else {
        format_tokens(tok_5h)
    };

    vec![
        QuotaMetric {
            window_label: "5h".to_string(),
            remaining_percent: (100.0 - used_5h).clamp(0.0, 100.0),
            value_text: None,
            detail: Some(detail_5h),
        },
        QuotaMetric {
            window_label: "7d".to_string(),
            remaining_percent: (100.0 - used_7d).clamp(0.0, 100.0),
            value_text: None,
            detail: Some(format_tokens(tok_7d)),
        },
    ]
}

fn collect_claude_metrics(
    now_secs: u64,
    cfg: &QuotaFileConfig,
    cache: &mut ClaudeFileCache,
) -> Vec<QuotaMetric> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let projects_dir = home.join(".claude/projects");
    collect_claude_metrics_from_dir_cached(&projects_dir, now_secs, cfg, cache)
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum AgyTimestamp<'a> {
    Str(#[serde(borrow)] std::borrow::Cow<'a, str>),
    Num(u64),
}

#[derive(serde::Deserialize)]
struct AgyTranscriptLine<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    created_at: Option<AgyTimestamp<'a>>,
    #[serde(borrow)]
    timestamp: Option<AgyTimestamp<'a>>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

fn extract_agy_timestamp(ts: Option<AgyTimestamp<'_>>) -> Option<u64> {
    match ts? {
        AgyTimestamp::Str(s) => parse_utc_iso8601_secs(&s),
        AgyTimestamp::Num(n) => Some(n),
    }
}

fn parse_antigravity_transcript_step(line: &str) -> Option<(u64, u64)> {
    if !line.contains("\"PLANNER_RESPONSE\"") {
        return None;
    }
    let parsed = serde_json::from_str::<AgyTranscriptLine<'_>>(line).ok()?;
    if parsed.kind.as_deref() != Some("PLANNER_RESPONSE") {
        return None;
    }
    let ts = extract_agy_timestamp(parsed.created_at)
        .or_else(|| extract_agy_timestamp(parsed.timestamp))?;
    let tokens = parsed
        .input_tokens
        .unwrap_or(0)
        .saturating_add(parsed.output_tokens.unwrap_or(0));
    Some((ts, tokens))
}

#[cfg(test)]
pub fn count_antigravity_transcript_steps_in_str(
    text: &str,
    cutoff_5h: u64,
    midnight_secs: u64,
) -> (u64, u64) {
    let mut steps_5h = 0u64;
    let mut steps_day = 0u64;
    for line in text.lines() {
        let Some((ts, _)) = parse_antigravity_transcript_step(line) else {
            continue;
        };
        if ts >= midnight_secs {
            steps_day += 1;
        }
        if ts >= cutoff_5h {
            steps_5h += 1;
        }
    }
    (steps_5h, steps_day)
}

type AgyFileEntry = (SystemTime, u64, Vec<(u64, u64)>);

#[derive(Default)]
pub struct AgyFileCache {
    entries: HashMap<PathBuf, AgyFileEntry>,
}

fn parse_antigravity_transcript_file_records(path: &Path) -> Option<Vec<(u64, u64)>> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    let mut records = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if let Some(rec) = parse_antigravity_transcript_step(trimmed) {
                    records.push(rec);
                }
            }
            Err(_) => return None,
        }
    }
    Some(records)
}

pub fn resolve_antigravity_session_ids(agy_root: &Path, hint: &AgySessionHint) -> HashSet<String> {
    let fallback_ids = || {
        let mut set = HashSet::new();
        if let Some(id) = hint
            .conversation_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            set.insert(id.to_string());
        }
        set
    };

    let db_path = agy_root.join("conversation_summaries.db");
    if !db_path.is_file() {
        return fallback_ids();
    }

    let script = r#"
import json, os, sqlite3, sys, urllib.parse

db_path, explicit_id, cwd_arg, title_arg = sys.argv[1:5]
conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True, timeout=1.0)
rows = conn.execute(
    "SELECT conversation_id, title, workspace_uris, parent_conversation_id, not_fully_idle, last_modified_time "
    "FROM conversation_summaries"
).fetchall()
parents = {}
try:
    for cid, pid in conn.execute("SELECT conversation_id, parent_id FROM conversation_parents").fetchall():
        if cid and pid:
            parents[cid] = pid
except sqlite3.OperationalError:
    pass

children = {}
top_rows = []
for cid, row_title, wuri, pid, active, lmt in rows:
    if not cid:
        continue
    eff_pid = (pid or "").strip() or parents.get(cid, "")
    if eff_pid:
        children.setdefault(eff_pid, []).append(cid)
    else:
        top_rows.append((cid, row_title or "", wuri or "", int(bool(active)), str(lmt or "")))

target_id = explicit_id.strip() or None
if target_id is None:
    norm_cwd = os.path.realpath(cwd_arg.strip()) if cwd_arg.strip() else ""
    title_lower = title_arg.strip().lower()
    best = None
    for cid, row_title, wuri, active, lmt in top_rows:
        paths = []
        try:
            uris = json.loads(wuri) if wuri else []
        except Exception:
            uris = []
        if isinstance(uris, list):
            for u in uris:
                if isinstance(u, str) and u.startswith("file://"):
                    p = urllib.parse.unquote(urllib.parse.urlparse(u).path)
                    if p:
                        paths.append(os.path.realpath(p))
        cwd_exact = bool(norm_cwd) and norm_cwd in paths
        cwd_prefix = False
        if bool(norm_cwd) and not cwd_exact:
            for wp in paths:
                pref = wp.rstrip(os.sep)
                if pref and norm_cwd.startswith(pref + os.sep):
                    cwd_prefix = True
                    break
        rt = row_title.strip().lower()
        title_match = len(rt) >= 3 and bool(title_lower) and (rt in title_lower)
        if cwd_exact and title_match:
            tier = 4
        elif cwd_exact:
            tier = 3
        elif title_match:
            tier = 2
        elif cwd_prefix:
            tier = 1
        else:
            continue
        score = (tier, active, lmt, cid)
        if best is None or score > best[0]:
            best = (score, cid)
    if best is not None:
        target_id = best[1]

out = []
if target_id:
    seen = {target_id}
    queue = [target_id]
    while queue:
        cur = queue.pop()
        out.append(cur)
        for ch in children.get(cur, []):
            if ch not in seen:
                seen.add(ch)
                queue.append(ch)
print(json.dumps(out))
"#;

    let explicit_arg = hint.conversation_id.as_deref().unwrap_or("");
    let cwd_arg = hint.cwd.as_deref().unwrap_or("");
    let title_arg = hint.title.as_deref().unwrap_or("");
    let Some(stdout) = run_command_stdout_with_timeout(
        "python3",
        [
            OsStr::new("-c"),
            OsStr::new(script),
            db_path.as_os_str(),
            OsStr::new(explicit_arg),
            OsStr::new(cwd_arg),
            OsStr::new(title_arg),
        ],
        QUOTA_LOCAL_HELPER_TIMEOUT,
    ) else {
        return fallback_ids();
    };
    let Ok(ids) = serde_json::from_str::<Vec<String>>(stdout.trim()) else {
        return fallback_ids();
    };
    ids.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn antigravity_window_metric(label: &str, steps: u64, tokens: u64) -> QuotaMetric {
    if tokens > 0 {
        QuotaMetric::usage(
            label,
            format_tokens(tokens),
            (steps > 0).then(|| format_steps_compact(steps)),
        )
    } else if steps > 0 {
        QuotaMetric::usage(label, format_steps_compact(steps), None)
    } else {
        QuotaMetric::usage(label, "0", None)
    }
}

pub fn collect_antigravity_metrics_from_brain_dir_cached(
    brain_dir: &Path,
    now_secs: u64,
    utc_offset_secs: i64,
    session_ids: &HashSet<String>,
    cache: &mut AgyFileCache,
) -> Vec<QuotaMetric> {
    if !brain_dir.is_dir() {
        return Vec::new();
    }

    let cut_5h = now_secs.saturating_sub(5 * 3_600);
    let midnight_secs = today_midnight_unix_secs(now_secs, utc_offset_secs);
    let cut_7d = now_secs.saturating_sub(7 * 86_400);
    let cut_30d = now_secs.saturating_sub(30 * 86_400);
    let min_cutoff_secs = cut_30d.min(cut_7d).min(midnight_secs).min(cut_5h);
    let min_mtime = UNIX_EPOCH.checked_add(Duration::from_secs(min_cutoff_secs));

    if cache.entries.len() > 1024 {
        cache
            .entries
            .retain(|_, (mt, _, _)| min_mtime.is_none_or(|m| *mt >= m));
    }

    let mut sess_steps = 0u64;
    let mut sess_toks = 0u64;
    let mut steps_5h = 0u64;
    let mut toks_5h = 0u64;
    let mut steps_day = 0u64;
    let mut toks_day = 0u64;
    let mut steps_7d = 0u64;
    let mut toks_7d = 0u64;
    let mut steps_30d = 0u64;
    let mut toks_30d = 0u64;

    if let Ok(entries) = fs::read_dir(brain_dir) {
        for entry in entries.flatten() {
            let conv_id = entry.file_name();
            let conv_id_str = conv_id.to_string_lossy();
            let is_session = session_ids.contains(conv_id_str.as_ref());
            let log_path = entry.path().join(".system_generated/logs/transcript.jsonl");
            let Ok(meta) = fs::metadata(&log_path) else {
                continue;
            };
            let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
            let file_len = meta.len();
            if !is_session && min_mtime.is_some_and(|m| mtime < m) {
                continue;
            }
            let records = match cache.entries.get(&log_path) {
                Some((cached_mtime, cached_len, recs))
                    if *cached_mtime == mtime && *cached_len == file_len =>
                {
                    recs.as_slice()
                }
                _ => {
                    let Some(parsed) = parse_antigravity_transcript_file_records(&log_path) else {
                        continue;
                    };
                    cache
                        .entries
                        .insert(log_path.clone(), (mtime, file_len, parsed));
                    cache.entries.get(&log_path).unwrap().2.as_slice()
                }
            };

            for &(ts, toks) in records {
                if is_session {
                    sess_steps += 1;
                    sess_toks = sess_toks.saturating_add(toks);
                }
                if ts >= cut_30d {
                    steps_30d += 1;
                    toks_30d = toks_30d.saturating_add(toks);
                    if ts >= cut_7d {
                        steps_7d += 1;
                        toks_7d = toks_7d.saturating_add(toks);
                    }
                    if ts >= midnight_secs {
                        steps_day += 1;
                        toks_day = toks_day.saturating_add(toks);
                    }
                    if ts >= cut_5h {
                        steps_5h += 1;
                        toks_5h = toks_5h.saturating_add(toks);
                    }
                }
            }
        }
    }

    let mut out = Vec::with_capacity(5);
    if !session_ids.is_empty() {
        out.push(antigravity_window_metric("会话", sess_steps, sess_toks));
    }
    out.push(antigravity_window_metric("5h", steps_5h, toks_5h));
    out.push(antigravity_window_metric("今日", steps_day, toks_day));
    out.push(antigravity_window_metric("7d", steps_7d, toks_7d));
    out.push(antigravity_window_metric("30d", steps_30d, toks_30d));
    out
}

fn collect_antigravity_metrics(
    now_secs: u64,
    hint: Option<&AgySessionHint>,
    cache: &mut AgyFileCache,
) -> Vec<QuotaMetric> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let agy_root = home.join(".gemini/antigravity-cli");
    let brain_dir = agy_root.join("brain");
    if !brain_dir.is_dir() {
        return Vec::new();
    }
    let session_ids = hint
        .map(|h| resolve_antigravity_session_ids(&agy_root, h))
        .unwrap_or_default();
    collect_antigravity_metrics_from_brain_dir_cached(
        &brain_dir,
        now_secs,
        local_utc_offset_secs(),
        &session_ids,
        cache,
    )
}

pub fn parse_freebuff_session_quota_json(text: &str, now_secs: u64) -> Option<Vec<QuotaMetric>> {
    let val: Value = serde_json::from_str(text.trim()).ok()?;
    if let Some(exp) = val
        .get("expires_at")
        .or_else(|| val.get("resets_at"))
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(parse_utc_iso8601_secs))
        })
    {
        if exp > 0 && now_secs >= exp {
            return None;
        }
    }
    let rem_pct = if let Some(rem) = val.get("remaining_percent").and_then(Value::as_f64) {
        rem.clamp(0.0, 100.0)
    } else if let Some(used_pct) = val.get("used_percent").and_then(Value::as_f64) {
        (100.0 - used_pct).clamp(0.0, 100.0)
    } else {
        let used = val
            .get("used")
            .or_else(|| val.get("requests_used"))
            .and_then(Value::as_u64)?;
        let limit = val
            .get("limit")
            .or_else(|| val.get("requests_limit"))
            .and_then(Value::as_u64)
            .filter(|&l| l > 0)?;
        let used_pct = ((used as f64 / limit as f64) * 100.0).clamp(0.0, 100.0);
        (100.0 - used_pct).clamp(0.0, 100.0)
    };
    let label = val
        .get("window_label")
        .or_else(|| val.get("window"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("会话")
        .to_string();
    let detail = val
        .get("detail")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            let used = val
                .get("used")
                .or_else(|| val.get("requests_used"))
                .and_then(Value::as_u64)?;
            let limit = val
                .get("limit")
                .or_else(|| val.get("requests_limit"))
                .and_then(Value::as_u64)?;
            Some(format!("{used}/{limit}"))
        });
    Some(vec![QuotaMetric {
        window_label: label,
        remaining_percent: rem_pct,
        value_text: None,
        detail,
    }])
}

fn collect_freebuff_metrics(now_secs: u64) -> Vec<QuotaMetric> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    for rel in [
        ".cache/herdr-freebuff/session_quota.json",
        ".config/freebuff/session_quota.json",
    ] {
        let path = home.join(rel);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(metrics) = parse_freebuff_session_quota_json(&text, now_secs) {
            return metrics;
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_focused_agent_from_terminal_fields_and_ignores_plain_shell() {
        let mut info = TerminalInfo {
            address: "t1".to_string(),
            title: "zsh".to_string(),
            fg_name: Some("zsh".to_string()),
            ..Default::default()
        };
        assert_eq!(detect_agent_from_terminal(&info), None);

        info.fg_name = Some("codex".to_string());
        assert_eq!(detect_agent_from_terminal(&info), Some(AgentKind::Codex));

        info.fg_name = None;
        info.status_app = Some("antigravity".to_string());
        assert_eq!(
            detect_agent_from_terminal(&info),
            Some(AgentKind::Antigravity)
        );

        info.status_app = None;
        info.title = "opencode ~/project".to_string();
        assert_eq!(detect_agent_from_terminal(&info), Some(AgentKind::OpenCode));

        info.title = "claude --resume".to_string();
        assert_eq!(detect_agent_from_terminal(&info), Some(AgentKind::Claude));
    }

    #[test]
    fn renders_8_step_unicode_progress_bar_accurately() {
        assert_eq!(render_progress_bar(0.0, 8), "░░░░░░░░");
        assert_eq!(render_progress_bar(50.0, 8), "████░░░░");
        assert_eq!(render_progress_bar(100.0, 8), "████████");
        let bar_49 = render_progress_bar(49.0, 8);
        assert_eq!(bar_49.chars().count(), 8);
        assert!(bar_49.starts_with("███▉"));
    }

    #[test]
    fn parses_codex_rate_limits_as_remaining_percentage_and_resets_expired_windows() {
        let sample = r#"{"timestamp":"2026-11-11T04:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":17.0,"window_minutes":300,"resets_at":2000},"secondary":{"used_percent":84.0,"window_minutes":10080,"resets_at":500000}}}}"#;

        // Before `resets_at`: 5h has 83% remaining (100 - 17), 7d has 16% remaining (100 - 84)
        let metrics = parse_codex_rate_limits_from_jsonl(sample, 1000).unwrap();
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].window_label, "5h");
        assert_eq!(metrics[0].remaining_percent, 83.0);
        assert_eq!(metrics[0].detail.as_deref(), Some("16m"));
        assert_eq!(metrics[1].window_label, "7d");
        assert_eq!(metrics[1].remaining_percent, 16.0);

        // After primary `resets_at` (2500 >= 2000): 5h window restores to 100% remaining
        let expired = parse_codex_rate_limits_from_jsonl(sample, 2500).unwrap();
        assert_eq!(expired[0].remaining_percent, 100.0);
        assert_eq!(expired[0].detail, None);
        assert_eq!(expired[1].remaining_percent, 16.0);

        // Null or incomplete window objects are skipped instead of emitting fake 100% bars
        let partial = r#"{"timestamp":"2026-11-11T04:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":25.0,"window_minutes":300,"resets_at":2000},"secondary":null}}}"#;
        let partial_metrics = parse_codex_rate_limits_from_jsonl(partial, 1000).unwrap();
        assert_eq!(partial_metrics.len(), 1);
        assert_eq!(partial_metrics[0].window_label, "5h");
        assert_eq!(partial_metrics[0].remaining_percent, 75.0);

        let missing_used = r#"{"timestamp":"2026-11-11T04:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"window_minutes":300},"secondary":null}}}"#;
        assert!(parse_codex_rate_limits_from_jsonl(missing_used, 1000).is_none());
    }

    #[test]
    fn collects_antigravity_metrics_locally_from_brain_dir_and_handles_missing_dir() {
        use std::io::Write;

        assert_eq!(format_tokens(1_500_000_000), "1.5B");
        assert_eq!(format_steps_compact(559), "559步");
        assert_eq!(format_steps_compact(12_300), "12.3k步");

        let tmp_dir =
            std::env::temp_dir().join(format!("hct_test_agy_brain_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);

        let now_secs = now_unix_secs().max(40 * 86_400);
        let utc_offset_secs = 12 * 3_600 - (now_secs % 86_400) as i64;
        let mut cache = AgyFileCache::default();
        let mut session_ids = HashSet::new();
        session_ids.insert("conv_main".to_string());
        session_ids.insert("conv_sub".to_string());

        // Missing brain directory returns empty metrics rather than bogus zero counters
        assert!(collect_antigravity_metrics_from_brain_dir_cached(
            &tmp_dir,
            now_secs,
            utc_offset_secs,
            &session_ids,
            &mut cache,
        )
        .is_empty());

        let write_conv = |id: &str| {
            let log_dir = tmp_dir.join(id).join(".system_generated/logs");
            fs::create_dir_all(&log_dir).unwrap();
            log_dir.join("transcript.jsonl")
        };

        let main_log = write_conv("conv_main");
        let mut f = File::create(&main_log).unwrap();
        // Event A: within 5h/today/7d/30d/session, placed before >600 KiB filler to verify full-file streaming
        writeln!(
            f,
            "{{\"type\":\"PLANNER_RESPONSE\",\"created_at\":{},\"input_tokens\":1000,\"output_tokens\":500}}",
            now_secs - 3_600
        )
        .unwrap();
        let filler_line = format!(
            "{{\"type\":\"USER_INPUT\",\"created_at\":{},\"content\":\"{}\"}}\n",
            now_secs - 3_500,
            "x".repeat(2048)
        );
        for _ in 0..320 {
            f.write_all(filler_line.as_bytes()).unwrap();
        }
        // Event B: >30d old (counted in current session only, excluded from 30d/7d/today/5h)
        writeln!(
            f,
            "{{\"type\":\"PLANNER_RESPONSE\",\"created_at\":{},\"input_tokens\":10000,\"output_tokens\":0}}",
            now_secs - 35 * 86_400
        )
        .unwrap();
        // Event C: earlier today (midnight is now_secs - 12h), outside 5h
        writeln!(
            f,
            "{{\"type\":\"PLANNER_RESPONSE\",\"created_at\":{},\"input_tokens\":2000,\"output_tokens\":500}}",
            now_secs - 8 * 3_600
        )
        .unwrap();
        // Invalid UTF-8 and malformed JSON lines should be skipped cleanly
        f.write_all(b"{\"type\":\"PLANNER_RESPONSE\",\"bad\":\xff}\n")
            .unwrap();
        f.write_all(b"{\"type\":\"PLANNER_RESPONSE\",\"created_at\":\n")
            .unwrap();
        // Event D: recent within 5h; `cache_read_tokens` must not inflate uncached+output count
        writeln!(
            f,
            "{{\"type\":\"PLANNER_RESPONSE\",\"timestamp\":{},\"input_tokens\":500,\"output_tokens\":500,\"cache_read_tokens\":99999}}",
            now_secs - 600
        )
        .unwrap();
        drop(f);

        // Subagent belonging to current session, 3 days ago (7d & 30d & session)
        let sub_log = write_conv("conv_sub");
        fs::write(
            &sub_log,
            format!(
                "{{\"type\":\"PLANNER_RESPONSE\",\"created_at\":{},\"input_tokens\":4000,\"output_tokens\":1000}}\n",
                now_secs - 3 * 86_400
            ),
        )
        .unwrap();

        // Unrelated conversation with 15d and 2h events
        let other_log = write_conv("conv_other");
        fs::write(
            &other_log,
            format!(
                "{{\"type\":\"PLANNER_RESPONSE\",\"created_at\":{},\"input_tokens\":20000,\"output_tokens\":0}}\n{{\"type\":\"PLANNER_RESPONSE\",\"created_at\":{},\"input_tokens\":3000,\"output_tokens\":0}}\n",
                now_secs - 15 * 86_400,
                now_secs - 2 * 3_600
            ),
        )
        .unwrap();

        let metrics = collect_antigravity_metrics_from_brain_dir_cached(
            &tmp_dir,
            now_secs,
            utc_offset_secs,
            &session_ids,
            &mut cache,
        );
        assert_eq!(metrics.len(), 5);
        assert_eq!(metrics[0].window_label, "会话");
        assert_eq!(metrics[0].value_text.as_deref(), Some("20k"));
        assert_eq!(metrics[0].detail.as_deref(), Some("5步"));
        assert_eq!(metrics[1].window_label, "5h");
        assert_eq!(metrics[1].value_text.as_deref(), Some("6k"));
        assert_eq!(metrics[1].detail.as_deref(), Some("3步"));
        assert_eq!(metrics[2].window_label, "今日");
        assert_eq!(metrics[2].value_text.as_deref(), Some("8k"));
        assert_eq!(metrics[2].detail.as_deref(), Some("4步"));
        assert_eq!(metrics[3].window_label, "7d");
        assert_eq!(metrics[3].value_text.as_deref(), Some("13k"));
        assert_eq!(metrics[3].detail.as_deref(), Some("5步"));
        assert_eq!(metrics[4].window_label, "30d");
        assert_eq!(metrics[4].value_text.as_deref(), Some("33k"));
        assert_eq!(metrics[4].detail.as_deref(), Some("6步"));

        // Appending invalidates the cached `conv_main` entry and updates counts immediately
        let mut f = fs::OpenOptions::new().append(true).open(&main_log).unwrap();
        writeln!(
            f,
            "{{\"type\":\"PLANNER_RESPONSE\",\"created_at\":{},\"input_tokens\":1000000,\"output_tokens\":0}}",
            now_secs - 100
        )
        .unwrap();
        drop(f);

        let updated = collect_antigravity_metrics_from_brain_dir_cached(
            &tmp_dir,
            now_secs,
            utc_offset_secs,
            &session_ids,
            &mut cache,
        );
        assert_eq!(updated[0].value_text.as_deref(), Some("1.0M"));
        assert_eq!(updated[0].detail.as_deref(), Some("6步"));
        assert_eq!(updated[1].value_text.as_deref(), Some("1.0M"));
        assert_eq!(updated[1].detail.as_deref(), Some("4步"));

        // When no current session matches, omit `会话` and return the 4 global time windows
        let no_sess = collect_antigravity_metrics_from_brain_dir_cached(
            &tmp_dir,
            now_secs,
            utc_offset_secs,
            &HashSet::new(),
            &mut cache,
        );
        let _ = fs::remove_dir_all(&tmp_dir);

        assert_eq!(no_sess.len(), 4);
        assert_eq!(no_sess[0].window_label, "5h");
        assert_eq!(no_sess[1].window_label, "今日");
        assert_eq!(no_sess[2].window_label, "7d");
        assert_eq!(no_sess[3].window_label, "30d");
    }

    #[test]
    fn resolves_antigravity_session_ids_from_argv_and_sqlite_summaries() {
        assert_eq!(
            extract_conversation_id_from_argv(&[
                "agy".to_string(),
                "--conversation".to_string(),
                "conv-1".to_string()
            ]),
            Some("conv-1".to_string())
        );
        assert_eq!(
            extract_conversation_id_from_argv(&[
                "agy".to_string(),
                "--conversation=conv-2".to_string()
            ]),
            Some("conv-2".to_string())
        );
        assert_eq!(
            extract_conversation_id_from_argv(&["agy".to_string(), "--continue".to_string()]),
            None
        );

        let tmp_dir = std::env::temp_dir().join(format!("hct_test_agy_db_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);
        let proj_a = tmp_dir.join("proj_a");
        let proj_a_sub = proj_a.join("nested");
        let proj_b = tmp_dir.join("proj_b");
        let unrelated = tmp_dir.join("unrelated");
        fs::create_dir_all(&proj_a_sub).unwrap();
        fs::create_dir_all(&proj_b).unwrap();
        fs::create_dir_all(&unrelated).unwrap();

        let db_path = tmp_dir.join("conversation_summaries.db");
        let py_setup = r#"
import json, sqlite3, sys
db_path, proj_a, proj_b = sys.argv[1:4]
conn = sqlite3.connect(db_path)
conn.execute(
    "CREATE TABLE conversation_summaries ("
    "conversation_id TEXT PRIMARY KEY, title TEXT, workspace_uris TEXT, "
    "parent_conversation_id TEXT, not_fully_idle INTEGER, last_modified_time TEXT)"
)
conn.execute(
    "CREATE TABLE conversation_parents (conversation_id TEXT PRIMARY KEY, parent_id TEXT)"
)
uri_a = json.dumps(["file://" + proj_a])
uri_b = json.dumps(["file://" + proj_b])
rows = [
    ("conv_active_a", "Refactor Usage Bar", uri_a, "", 1, "2026-11-26T10:00:00Z"),
    ("conv_idle_a", "Old Session", uri_a, "", 0, "2026-11-26T12:00:00Z"),
    ("conv_sub_col", "", uri_a, "conv_active_a", 0, "2026-11-26T10:01:00Z"),
    ("conv_sub_tbl", "", uri_a, "", 0, "2026-11-26T10:02:00Z"),
    ("conv_grandchild", "", uri_a, "conv_sub_tbl", 0, "2026-11-26T10:03:00Z"),
    ("conv_other", "Other Work", uri_b, "", 1, "2026-11-26T13:00:00Z"),
]
conn.executemany("INSERT INTO conversation_summaries VALUES (?,?,?,?,?,?)", rows)
conn.execute("INSERT INTO conversation_parents VALUES (?,?)", ("conv_sub_tbl", "conv_active_a"))
conn.commit()
"#;
        let setup_out = run_command_stdout_with_timeout(
            "python3",
            [
                OsStr::new("-c"),
                OsStr::new(py_setup),
                db_path.as_os_str(),
                proj_a.as_os_str(),
                proj_b.as_os_str(),
            ],
            Duration::from_secs(3),
        );
        assert!(setup_out.is_some());

        // 1. cwd + title tiebreaker prefers exact matching title over active conversation in same dir
        let title_hint = AgySessionHint {
            conversation_id: None,
            cwd: Some(proj_a.to_string_lossy().into_owned()),
            title: Some("✳ Old Session · agy".to_string()),
        };
        let ids_title = resolve_antigravity_session_ids(&tmp_dir, &title_hint);
        assert_eq!(ids_title, HashSet::from(["conv_idle_a".to_string()]));

        // 2. cwd alone prefers active (`not_fully_idle = 1`) and gathers full subagent tree
        let cwd_hint = AgySessionHint {
            conversation_id: None,
            cwd: Some(proj_a.to_string_lossy().into_owned()),
            title: Some("agy".to_string()),
        };
        let ids_cwd = resolve_antigravity_session_ids(&tmp_dir, &cwd_hint);
        assert_eq!(
            ids_cwd,
            HashSet::from([
                "conv_active_a".to_string(),
                "conv_sub_col".to_string(),
                "conv_sub_tbl".to_string(),
                "conv_grandchild".to_string(),
            ])
        );

        // 3. Nested subdir matches via cwd_prefix fallback
        let sub_hint = AgySessionHint {
            conversation_id: None,
            cwd: Some(proj_a_sub.to_string_lossy().into_owned()),
            title: None,
        };
        let ids_sub = resolve_antigravity_session_ids(&tmp_dir, &sub_hint);
        assert!(ids_sub.contains("conv_active_a"));

        // 4. Unrelated workspace returns empty set
        let none_hint = AgySessionHint {
            conversation_id: None,
            cwd: Some(unrelated.to_string_lossy().into_owned()),
            title: Some("zsh".to_string()),
        };
        assert!(resolve_antigravity_session_ids(&tmp_dir, &none_hint).is_empty());

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn detect_focused_agent_stops_at_selected_pane_without_falling_back_to_tab_or_sibling_pane() {
        use crate::model::{SharedSpace, SharedTab};

        let mut world = World::default();
        world.terminals.insert(
            "t_codex".to_string(),
            TerminalInfo {
                address: "t_codex".to_string(),
                title: "codex".to_string(),
                fg_name: Some("codex".to_string()),
                ..Default::default()
            },
        );
        world.terminals.insert(
            "t_shell".to_string(),
            TerminalInfo {
                address: "t_shell".to_string(),
                title: "zsh".to_string(),
                fg_name: Some("zsh".to_string()),
                ..Default::default()
            },
        );
        world.doc.spaces.push(SharedSpace {
            id: "s1".to_string(),
            name: "Main".to_string(),
            folder: String::new(),
            tabs: vec![SharedTab {
                id: "tab1".to_string(),
                name: "codex".to_string(),
                root: serde_json::json!({
                    "type": "row",
                    "children": [
                        { "type": "block", "content": { "terminal": "t_codex" } },
                        { "type": "block", "content": { "terminal": "t_shell" } }
                    ]
                }),
                rest: Default::default(),
            }],
            rest: Default::default(),
        });
        world.active_space_idx = 0;
        world
            .active_tab_by_space
            .insert("s1".to_string(), "tab1".to_string());
        world
            .active_term_by_tab
            .insert("tab1".to_string(), "t_shell".to_string());

        assert_eq!(detect_focused_agent(&world), None);

        world
            .active_term_by_tab
            .insert("tab1".to_string(), "t_codex".to_string());
        assert_eq!(detect_focused_agent(&world), Some(AgentKind::Codex));
    }

    #[test]
    fn filters_antigravity_transcript_steps_by_individual_event_timestamps() {
        let transcript = concat!(
            r#"{"type":"PLANNER_RESPONSE","created_at":"2026-11-09T10:00:00Z"}"#,
            "\n",
            r#"{"type":"PLANNER_RESPONSE","created_at":"2026-11-11T01:00:00Z"}"#,
            "\n",
            r#"{"type":"PLANNER_RESPONSE","created_at":"2026-11-11T08:00:00Z"}"#,
            "\n",
        );
        let now_secs = parse_utc_iso8601_secs("2026-11-11T10:00:00Z").unwrap();
        let cutoff_5h = now_secs - 5 * 3_600; // 05:00:00Z
        let midnight_secs = today_midnight_unix_secs(now_secs, 0); // 00:00:00Z

        let (steps_5h, steps_day) =
            count_antigravity_transcript_steps_in_str(transcript, cutoff_5h, midnight_secs);
        assert_eq!(steps_5h, 1);
        assert_eq!(steps_day, 2);
    }

    #[test]
    fn selects_newest_codex_rate_limit_snapshot_across_session_files() {
        let tmp_dir =
            std::env::temp_dir().join(format!("hct_test_codex_sessions_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let older_snapshot_newer_file = tmp_dir.join("session_a.jsonl");
        let newer_snapshot_older_file = tmp_dir.join("session_b.jsonl");

        let line_newer = r#"{"timestamp":"2026-11-11T10:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":40.0,"window_minutes":300,"resets_at":2000000000}}}}"#;
        let line_older = r#"{"timestamp":"2026-11-11T08:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":10.0,"window_minutes":300,"resets_at":2000000000}}}}"#;

        fs::write(&newer_snapshot_older_file, format!("{line_newer}\n")).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        fs::write(
            &older_snapshot_newer_file,
            format!(
                "{line_older}\n{{\"timestamp\":\"2026-11-11T10:05:00Z\",\"type\":\"other\"}}\n"
            ),
        )
        .unwrap();

        let now_secs = parse_utc_iso8601_secs("2026-11-11T10:06:00Z").unwrap();
        let metrics = collect_codex_metrics_from_sessions_dir(&tmp_dir, now_secs);
        let _ = fs::remove_dir_all(&tmp_dir);

        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].remaining_percent, 60.0);
    }

    #[test]
    fn claude_collector_deduplicates_multi_block_responses_and_skips_malformed_lines() {
        let tmp_dir =
            std::env::temp_dir().join(format!("hct_test_claude_projects_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let now_secs = parse_utc_iso8601_secs("2026-11-11T10:00:00Z").unwrap();
        let transcript = concat!(
            r#"{"timestamp":"2026-11-11T09:00:00Z","requestId":"req_1","message":{"id":"msg_1","usage":{"input_tokens":100,"output_tokens":50}}}"#,
            "\n",
            r#"{"timestamp":"2026-11-11T09:00:01Z","requestId":"req_1","message":{"id":"msg_1","usage":{"input_tokens":100,"output_tokens":50}}}"#,
            "\n",
            r#"{"timestamp":"2026-11-11T09:00:02Z","requestId":"req_1","message":{"id":"msg_1","usage":{"input_tokens":100,"output_tokens":50}}}"#,
            "\n",
            r#"{"timestamp":"2026-11-11T09:05:00Z","message":{"usage":{"input_tokens":999"#,
            "\n",
            r#"{"timestamp":"2026-11-11T09:10:00Z","requestId":"req_2","message":{"id":"msg_2","usage":{"input_tokens":120,"output_tokens":30}}}"#,
            "\n",
        );
        fs::write(tmp_dir.join("conv.jsonl"), transcript).unwrap();

        let cfg = QuotaFileConfig {
            claude_5h_tokens: Some(1_000),
            claude_5h_messages: Some(10),
            claude_7d_tokens: Some(10_000),
            claude_7d_messages: Some(100),
            ..Default::default()
        };
        let metrics = collect_claude_metrics_from_dir(&tmp_dir, now_secs, &cfg);
        let _ = fs::remove_dir_all(&tmp_dir);

        assert_eq!(metrics.len(), 2);
        // 2 unique responses (300 tokens total, 2 messages):
        // 5h used = max(300/1000, 2/10) = 30% -> 70% remaining
        assert!((metrics[0].remaining_percent - 70.0).abs() < 1e-4);
        assert!(metrics[0].detail.as_deref().unwrap().starts_with("300 · "));
        // 7d used = max(300/10000, 2/100) = 3% -> 97% remaining
        assert!((metrics[1].remaining_percent - 97.0).abs() < 1e-4);
        assert_eq!(metrics[1].detail.as_deref(), Some("300"));
    }

    #[test]
    fn parses_freebuff_session_quota_and_rejects_expired_or_missing() {
        let sample = r#"{"window_label":"会话","used":12,"limit":60,"expires_at":2000}"#;
        let metrics = parse_freebuff_session_quota_json(sample, 1000).unwrap();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].window_label, "会话");
        assert!((metrics[0].remaining_percent - 80.0).abs() < 1e-4);
        assert_eq!(metrics[0].detail.as_deref(), Some("12/60"));

        assert!(parse_freebuff_session_quota_json(sample, 2500).is_none());
        assert!(parse_freebuff_session_quota_json("{}", 1000).is_none());
    }

    #[test]
    fn claude_file_cache_reuses_unchanged_files_and_invalidates_on_append() {
        let tmp_dir =
            std::env::temp_dir().join(format!("hct_test_claude_cache_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let now_secs = parse_utc_iso8601_secs("2026-11-11T10:00:00Z").unwrap();
        let conv_path = tmp_dir.join("conv.jsonl");
        fs::write(
            &conv_path,
            "{\"timestamp\":\"2026-11-11T09:00:00Z\",\"requestId\":\"r1\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":100,\"output_tokens\":100}}}\n",
        )
        .unwrap();

        let cfg = QuotaFileConfig {
            claude_5h_tokens: Some(1_000),
            claude_5h_messages: Some(100),
            claude_7d_tokens: Some(10_000),
            claude_7d_messages: Some(1000),
            ..Default::default()
        };
        let mut cache = ClaudeFileCache::default();
        let m1 = collect_claude_metrics_from_dir_cached(&tmp_dir, now_secs, &cfg, &mut cache);
        assert!((m1[0].remaining_percent - 80.0).abs() < 1e-4);
        assert_eq!(cache.entries.len(), 1);

        // Appending a second turn changes file length and invalidates the cached entry immediately
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&conv_path)
            .unwrap();
        use std::io::Write;
        writeln!(
            f,
            "{{\"timestamp\":\"2026-11-11T09:10:00Z\",\"requestId\":\"r2\",\"message\":{{\"id\":\"m2\",\"usage\":{{\"input_tokens\":200,\"output_tokens\":100}}}}}}"
        )
        .unwrap();
        drop(f);

        let m2 = collect_claude_metrics_from_dir_cached(&tmp_dir, now_secs, &cfg, &mut cache);
        let _ = fs::remove_dir_all(&tmp_dir);
        assert!((m2[0].remaining_percent - 50.0).abs() < 1e-4);
    }

    #[test]
    fn run_command_stdout_with_timeout_captures_output_and_kills_hung_process_group() {
        let out = run_command_stdout_with_timeout(
            "sh",
            ["-c", "printf 'quota-ok'"],
            Duration::from_secs(2),
        );
        assert_eq!(out.as_deref(), Some("quota-ok"));

        let failed =
            run_command_stdout_with_timeout("sh", ["-c", "exit 3"], Duration::from_secs(2));
        assert!(failed.is_none());

        let pid_file =
            std::env::temp_dir().join(format!("hct_test_pgkill_{}.pid", std::process::id()));
        let _ = fs::remove_file(&pid_file);

        let t0 = Instant::now();
        let hung = run_command_stdout_with_timeout(
            "sh",
            [
                OsStr::new("-c"),
                OsStr::new("sleep 30 & echo $! > \"$1\"; wait"),
                OsStr::new("_"),
                pid_file.as_os_str(),
            ],
            Duration::from_millis(150),
        );
        assert!(hung.is_none());
        assert!(t0.elapsed() < Duration::from_secs(3));

        #[cfg(unix)]
        if let Ok(raw_pid) = fs::read_to_string(&pid_file) {
            let _ = fs::remove_file(&pid_file);
            if let Ok(desc_pid) = raw_pid.trim().parse::<i32>() {
                extern "C" {
                    fn kill(pid: i32, sig: i32) -> i32;
                }
                let is_terminated = |pid: i32| -> bool {
                    if unsafe { kill(pid, 0) } != 0 {
                        return true;
                    }
                    // In containers where PID 1 does not reap orphaned grandchildren, a killed
                    // descendant remains in zombie (`Z`) / dead (`X`) state even though `kill(pid, 0)`
                    // still returns 0.
                    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
                        if let Some((_, after_comm)) = stat.rfind(')').map(|i| stat.split_at(i + 1))
                        {
                            if let Some(state_ch) = after_comm.trim_start().chars().next() {
                                return matches!(state_ch, 'Z' | 'X' | 'x');
                            }
                        }
                    }
                    false
                };
                let wait_deadline = Instant::now() + Duration::from_millis(500);
                let mut alive = true;
                while Instant::now() < wait_deadline {
                    if is_terminated(desc_pid) {
                        alive = false;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(
                    !alive,
                    "descendant pid {desc_pid} survived process-group timeout kill"
                );
            }
        }
        let _ = fs::remove_file(&pid_file);
    }

    #[test]
    fn usage_tracker_deduplicates_inflight_retains_transient_errors_and_clears_expired() {
        use crate::model::{SharedSpace, SharedTab};
        use std::sync::atomic::AtomicUsize;

        fn world_for_agent_with_argv(addr: &str, name: &str, fg_argv: Vec<String>) -> World {
            let mut world = World::default();
            world.terminals.insert(
                addr.to_string(),
                TerminalInfo {
                    address: addr.to_string(),
                    title: name.to_string(),
                    fg_name: Some(name.to_string()),
                    fg_argv,
                    ..Default::default()
                },
            );
            world.doc.spaces.push(SharedSpace {
                id: "s1".to_string(),
                name: "Main".to_string(),
                folder: String::new(),
                tabs: vec![SharedTab {
                    id: "tab1".to_string(),
                    name: name.to_string(),
                    root: serde_json::json!({
                        "type": "block",
                        "content": { "terminal": addr }
                    }),
                    rest: Default::default(),
                }],
                rest: Default::default(),
            });
            world.active_space_idx = 0;
            world
                .active_tab_by_space
                .insert("s1".to_string(), "tab1".to_string());
            world
                .active_term_by_tab
                .insert("tab1".to_string(), addr.to_string());
            world
        }

        let world_agy = world_for_agent_with_argv(
            "t_agy",
            "antigravity",
            vec!["agy".into(), "--conversation=conv-a".into()],
        );
        let world_agy_b = world_for_agent_with_argv(
            "t_agy_2",
            "antigravity",
            vec!["agy".into(), "--conversation=conv-b".into()],
        );
        let world_codex = world_for_agent_with_argv("t_codex", "codex", Vec::new());
        let world_freebuff = world_for_agent_with_argv("t_fb", "freebuff", Vec::new());

        let (agy_started_tx, agy_started_rx) = mpsc::channel();
        let (unblock_agy_tx, unblock_agy_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let codex_calls = Arc::new(AtomicUsize::new(0));
        let codex_calls_bg = Arc::clone(&codex_calls);
        let fb_calls = Arc::new(AtomicUsize::new(0));
        let fb_calls_bg = Arc::clone(&fb_calls);

        let tracker = UsageTracker::new_with_collector(
            move |kind, hint, _, _| match kind {
                AgentKind::Antigravity => {
                    let conv = hint
                        .and_then(|h| h.conversation_id.as_deref())
                        .unwrap_or("none");
                    if conv == "conv-a" {
                        let _ = agy_started_tx.send(());
                        let _ = unblock_agy_rx.recv_timeout(Duration::from_secs(3));
                        QuotaFetchOutcome::Authoritative(vec![QuotaMetric::usage(
                            "会话", "10M", None,
                        )])
                    } else {
                        QuotaFetchOutcome::Authoritative(vec![QuotaMetric::usage(
                            "会话", "25M", None,
                        )])
                    }
                }
                AgentKind::Codex => {
                    let n = codex_calls_bg.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        QuotaFetchOutcome::Authoritative(vec![QuotaMetric {
                            window_label: "5h".to_string(),
                            remaining_percent: 83.0,
                            value_text: None,
                            detail: None,
                        }])
                    } else {
                        QuotaFetchOutcome::TransientError
                    }
                }
                AgentKind::Freebuff => {
                    let n = fb_calls_bg.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        QuotaFetchOutcome::Authoritative(vec![QuotaMetric {
                            window_label: "会话".to_string(),
                            remaining_percent: 75.0,
                            value_text: None,
                            detail: None,
                        }])
                    } else {
                        // Session expired -> authoritative empty result must clear cached metrics
                        QuotaFetchOutcome::Authoritative(Vec::new())
                    }
                }
                _ => QuotaFetchOutcome::Authoritative(Vec::new()),
            },
            move || {
                let _ = done_tx.send(());
            },
        );

        // Simulate 32 rapid render frames while Antigravity is still in flight;
        // in-flight deduplication must keep channel space free for Codex.
        for _ in 0..32 {
            let q = tracker.focused_quota(&world_agy).unwrap();
            assert_eq!(q.agent, AgentKind::Antigravity);
        }
        agy_started_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        // Switch focus to Codex while Antigravity is blocked; Codex must still enqueue cleanly.
        let initial_codex = tracker.focused_quota(&world_codex).unwrap();
        assert!(initial_codex.metrics.is_empty());

        unblock_agy_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let populated_agy = tracker.focused_quota(&world_agy).unwrap();
        assert_eq!(populated_agy.metrics.len(), 1);
        assert_eq!(populated_agy.metrics[0].value_text.as_deref(), Some("10M"));

        // Second Antigravity pane caches independently by terminal address
        let _ = tracker.focused_quota(&world_agy_b);
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            tracker.focused_quota(&world_agy_b).unwrap().metrics[0]
                .value_text
                .as_deref(),
            Some("25M")
        );
        assert_eq!(
            tracker.focused_quota(&world_agy).unwrap().metrics[0]
                .value_text
                .as_deref(),
            Some("10M")
        );

        let populated_codex = tracker.focused_quota(&world_codex).unwrap();
        assert_eq!(populated_codex.metrics.len(), 1);
        assert_eq!(populated_codex.metrics[0].remaining_percent, 83.0);

        // TransientError on Codex retains 83%
        {
            let mut guard = tracker.state.lock().unwrap();
            if let Some(entry) = guard.cache.get_mut(&(AgentKind::Codex, None)) {
                entry.ttl = Duration::ZERO;
            }
        }
        let _ = tracker.focused_quota(&world_codex);
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(codex_calls.load(Ordering::SeqCst), 2);
        let retained_codex = tracker.focused_quota(&world_codex).unwrap();
        assert_eq!(retained_codex.metrics.len(), 1);
        assert_eq!(retained_codex.metrics[0].remaining_percent, 83.0);

        // Authoritative empty on Freebuff (expired session) clears cached metrics immediately
        let _ = tracker.focused_quota(&world_freebuff);
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            tracker.focused_quota(&world_freebuff).unwrap().metrics[0].remaining_percent,
            75.0
        );
        {
            let mut guard = tracker.state.lock().unwrap();
            if let Some(entry) = guard.cache.get_mut(&(AgentKind::Freebuff, None)) {
                entry.ttl = Duration::ZERO;
            }
        }
        let _ = tracker.focused_quota(&world_freebuff);
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(fb_calls.load(Ordering::SeqCst), 2);
        assert!(tracker
            .focused_quota(&world_freebuff)
            .unwrap()
            .metrics
            .is_empty());
    }

    #[test]
    fn eink_progress_bar_avoids_dithered_blocks() {
        assert_eq!(render_eink_progress_bar(83.0, 0), "");
        assert_eq!(render_eink_progress_bar(0.0, 8), "────────");
        assert_eq!(render_eink_progress_bar(50.0, 8), "████────");
        assert_eq!(render_eink_progress_bar(83.0, 8), "███████─");
        assert_eq!(render_eink_progress_bar(100.0, 8), "████████");
        assert_eq!(render_eink_progress_bar(140.0, 4), "████");
        assert_eq!(render_eink_progress_bar(-10.0, 4), "────");
    }
}
