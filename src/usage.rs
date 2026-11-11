//! Focused-agent quota & usage tracker for the 1-row read-only bottom status bar.
//! Collects 5h / 7d / daily quota usage in a background worker thread so TUI rendering never blocks.

use crate::keys::QuotaFileConfig;
use crate::model::{TerminalInfo, World};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CACHE_TTL: Duration = Duration::from_secs(10);
const AGY_CACHE_TTL: Duration = Duration::from_secs(30);

fn cache_ttl_for_agent(kind: AgentKind) -> Duration {
    match kind {
        AgentKind::Codex | AgentKind::Antigravity => AGY_CACHE_TTL,
        _ => CACHE_TTL,
    }
}

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
    /// Optional compact suffix such as reset countdown (`"2h14m"`) or count (`"28/200 · 142k"`).
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentQuota {
    pub agent: AgentKind,
    pub metrics: Vec<QuotaMetric>,
}

pub struct UsageTracker {
    cache: Arc<Mutex<HashMap<AgentKind, (Instant, AgentQuota)>>>,
    revision: Arc<AtomicU64>,
    req_tx: SyncSender<AgentKind>,
}

impl UsageTracker {
    pub fn new(cfg: QuotaFileConfig) -> Self {
        let cache: Arc<Mutex<HashMap<AgentKind, (Instant, AgentQuota)>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let revision = Arc::new(AtomicU64::new(0));
        let (req_tx, req_rx) = mpsc::sync_channel::<AgentKind>(8);
        let cache_bg = Arc::clone(&cache);
        let rev_bg = Arc::clone(&revision);

        std::thread::spawn(move || {
            while let Ok(kind) = req_rx.recv() {
                let ttl = cache_ttl_for_agent(kind);
                let should_collect = {
                    let guard = cache_bg.lock().unwrap();
                    guard
                        .get(&kind)
                        .is_none_or(|(ts, _)| ts.elapsed() >= ttl)
                };
                if !should_collect {
                    continue;
                }
                let quota = collect_quota_for_agent(kind, &cfg);
                {
                    let mut guard = cache_bg.lock().unwrap();
                    guard.insert(kind, (Instant::now(), quota));
                }
                rev_bg.fetch_add(1, Ordering::Release);
            }
        });

        Self {
            cache,
            revision,
            req_tx,
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub fn focused_quota(&self, world: &World) -> Option<AgentQuota> {
        let kind = detect_focused_agent(world)?;
        let ttl = cache_ttl_for_agent(kind);
        let mut need_refresh = false;
        let cached = {
            let guard = self.cache.lock().unwrap();
            if let Some((ts, q)) = guard.get(&kind) {
                if ts.elapsed() >= ttl {
                    need_refresh = true;
                }
                Some(q.clone())
            } else {
                need_refresh = true;
                None
            }
        };

        if need_refresh {
            let _ = self.req_tx.try_send(kind);
        }

        Some(cached.unwrap_or_else(|| AgentQuota {
            agent: kind,
            metrics: Vec::new(),
        }))
    }
}

pub fn detect_focused_agent(world: &World) -> Option<AgentKind> {
    let addr = world.active_terminal_address()?;
    let info = world.terminals.get(&addr)?;
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
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", (tokens + 500) / 1_000)
    } else {
        format!("{tokens}")
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
        } else if let Some(idx) = rest.rfind(['+', '-']) {
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
        } else {
            return None;
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

fn local_utc_offset_secs() -> i64 {
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| {
        let Ok(out) = Command::new("date")
            .arg("+%z")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        else {
            return 0;
        };
        if !out.status.success() {
            return 0;
        }
        let s = String::from_utf8_lossy(&out.stdout);
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

fn collect_quota_for_agent(kind: AgentKind, cfg: &QuotaFileConfig) -> AgentQuota {
    let now_secs = now_unix_secs();
    let metrics = match kind {
        AgentKind::Codex => collect_codex_metrics(now_secs),
        AgentKind::Claude => collect_claude_metrics(now_secs, cfg),
        AgentKind::OpenCode => collect_opencode_metrics(now_secs, cfg),
        AgentKind::Antigravity => collect_antigravity_metrics(now_secs, cfg),
        AgentKind::Freebuff => collect_freebuff_metrics(now_secs),
    };
    AgentQuota {
        agent: kind,
        metrics,
    }
}

fn collect_recursive_files(dir: &Path, ext: &str, min_mtime: Option<SystemTime>, out: &mut Vec<(SystemTime, PathBuf)>) {
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
            if let Some(win) = rl.get(key) {
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
                let raw_used_pct = win
                    .get("used_percent")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
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
                    detail,
                });
            }
        }
        if !metrics.is_empty() {
            let replace = match &best {
                None => true,
                Some((prev_ts, prev_idx, _)) => (event_ts.unwrap_or(0), idx) >= (prev_ts.unwrap_or(0), *prev_idx),
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

pub fn parse_codex_wham_usage_json(text: &str, now_secs: u64) -> Option<Vec<QuotaMetric>> {
    let val: Value = serde_json::from_str(text.trim()).ok()?;
    let rl = val.get("rate_limit")?;
    if rl.is_null() {
        return None;
    }
    let mut metrics = Vec::new();
    for (key, default_lbl) in [("primary_window", "5h"), ("secondary_window", "7d")] {
        let Some(win) = rl.get(key) else {
            continue;
        };
        if win.is_null() {
            continue;
        }
        let win_secs = win
            .get("limit_window_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let lbl = match win_secs {
            18_000 => "5h".to_string(),
            604_800 => "7d".to_string(),
            86_400 => "1d".to_string(),
            s if s > 0 && s % 3_600 == 0 => format!("{}h", s / 3_600),
            _ => default_lbl.to_string(),
        };
        let raw_used_pct = win
            .get("used_percent")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let raw_rem_pct = (100.0 - raw_used_pct).clamp(0.0, 100.0);
        let reset_at = win.get("reset_at").and_then(Value::as_u64).unwrap_or(0);
        let reset_after = win
            .get("reset_after_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let (rem_pct, detail) = if reset_at > 0 && now_secs >= reset_at {
            (100.0, None)
        } else if reset_at > now_secs {
            (
                raw_rem_pct,
                Some(format_duration_short(reset_at - now_secs)),
            )
        } else if reset_after > 0 {
            (raw_rem_pct, Some(format_duration_short(reset_after)))
        } else {
            (raw_rem_pct, None)
        };
        metrics.push(QuotaMetric {
            window_label: lbl,
            remaining_percent: rem_pct,
            detail,
        });
    }
    if metrics.is_empty() {
        None
    } else {
        Some(metrics)
    }
}

fn fetch_codex_wham_usage(home: &Path, now_secs: u64) -> Option<Vec<QuotaMetric>> {
    let auth_path = home.join(".codex/auth.json");
    if !auth_path.is_file() {
        return None;
    }
    let script = r#"
import json, sys, urllib.request
auth_path = sys.argv[1]
with open(auth_path, "r", encoding="utf-8", errors="ignore") as f:
    auth = json.load(f)
tokens = auth.get("tokens") or {}
access_token = tokens.get("access_token")
if not access_token:
    sys.exit(1)
headers = {
    "Authorization": f"Bearer {access_token}",
    "User-Agent": "codex-cli",
}
account_id = tokens.get("account_id")
if account_id:
    headers["ChatGPT-Account-Id"] = account_id
req = urllib.request.Request("https://chatgpt.com/backend-api/wham/usage", headers=headers)
out = urllib.request.urlopen(req, timeout=4).read().decode("utf-8", errors="ignore")
print(out)
"#;
    let output = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(&auth_path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_codex_wham_usage_json(&text, now_secs)
}

pub fn collect_codex_metrics_from_sessions_dir(
    sessions_dir: &Path,
    now_secs: u64,
) -> Vec<QuotaMetric> {
    let mut files = Vec::new();
    let min_mtime = UNIX_EPOCH + Duration::from_secs(now_secs.saturating_sub(7 * 86_400));
    collect_recursive_files(sessions_dir, "jsonl", Some(min_mtime), &mut files);
    files.sort_by(|a, b| b.0.cmp(&a.0));

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

fn collect_codex_metrics(now_secs: u64) -> Vec<QuotaMetric> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    if let Some(wham_metrics) = fetch_codex_wham_usage(&home, now_secs) {
        return wham_metrics;
    }
    let sessions_dir = home.join(".codex/sessions");
    collect_codex_metrics_from_sessions_dir(&sessions_dir, now_secs)
}

fn collect_opencode_metrics(_now_secs: u64, cfg: &QuotaFileConfig) -> Vec<QuotaMetric> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let db_path = home.join(".local/share/opencode/opencode.db");
    if !db_path.is_file() {
        return Vec::new();
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

    let Ok(output) = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(&db_path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(val) = serde_json::from_slice::<Value>(&output.stdout) else {
        return Vec::new();
    };
    let day_reqs = val.get("day_reqs").and_then(Value::as_u64).unwrap_or(0);
    let day_toks = val.get("day_toks").and_then(Value::as_u64).unwrap_or(0);
    let five_reqs = val.get("five_reqs").and_then(Value::as_u64).unwrap_or(0);
    let five_toks = val.get("five_toks").and_then(Value::as_u64).unwrap_or(0);

    let day_used_pct = ((day_reqs as f64 / daily_limit as f64) * 100.0).clamp(0.0, 100.0);
    let five_used_pct = ((five_reqs as f64 / limit_5h as f64) * 100.0).clamp(0.0, 100.0);

    vec![
        QuotaMetric {
            window_label: "今日".to_string(),
            remaining_percent: (100.0 - day_used_pct).clamp(0.0, 100.0),
            detail: Some(format!("{day_reqs}/{daily_limit} · {}", format_tokens(day_toks))),
        },
        QuotaMetric {
            window_label: "5h".to_string(),
            remaining_percent: (100.0 - five_used_pct).clamp(0.0, 100.0),
            detail: Some(format!("{five_reqs}/{limit_5h} · {}", format_tokens(five_toks))),
        },
    ]
}

pub fn collect_claude_metrics_from_dir(
    projects_dir: &Path,
    now_secs: u64,
    cfg: &QuotaFileConfig,
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

    // Map response identity (`message.id` / `requestId`) -> (earliest_ts, cumulative_tokens)
    let mut responses: HashMap<String, (u64, u64)> = HashMap::new();
    for (file_idx, (mtime, path)) in files.into_iter().enumerate() {
        let mtime_secs = mtime
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(now_secs);
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let content = String::from_utf8_lossy(&bytes);
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
                format!("anon:{file_idx}:{line_idx}")
            };

            responses
                .entry(resp_key)
                .and_modify(|(prev_ts, prev_toks)| {
                    *prev_ts = (*prev_ts).min(ts);
                    *prev_toks = (*prev_toks).max(t);
                })
                .or_insert((ts, t));
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

    let used_5h = ((tok_5h as f64 / limit_5h_toks as f64)
        .max(msg_5h as f64 / limit_5h_msgs as f64)
        * 100.0)
        .clamp(0.0, 100.0);
    let used_7d = ((tok_7d as f64 / limit_7d_toks as f64)
        .max(msg_7d as f64 / limit_7d_msgs as f64)
        * 100.0)
        .clamp(0.0, 100.0);

    let detail_5h = if reset_5h > 0 {
        format!("{} · {}", format_tokens(tok_5h), format_duration_short(reset_5h))
    } else {
        format_tokens(tok_5h)
    };

    vec![
        QuotaMetric {
            window_label: "5h".to_string(),
            remaining_percent: (100.0 - used_5h).clamp(0.0, 100.0),
            detail: Some(detail_5h),
        },
        QuotaMetric {
            window_label: "7d".to_string(),
            remaining_percent: (100.0 - used_7d).clamp(0.0, 100.0),
            detail: Some(format_tokens(tok_7d)),
        },
    ]
}

fn collect_claude_metrics(now_secs: u64, cfg: &QuotaFileConfig) -> Vec<QuotaMetric> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let projects_dir = home.join(".claude/projects");
    collect_claude_metrics_from_dir(&projects_dir, now_secs, cfg)
}

pub fn parse_antigravity_quota_summary(text: &str, now_secs: u64) -> Option<Vec<QuotaMetric>> {
    let val: Value = serde_json::from_str(text.trim()).ok()?;
    let groups = val.get("groups")?.as_array()?;
    if groups.is_empty() {
        return None;
    }

    // Prefer the "Gemini Models" group (`displayName` or `bucketId` contains "gemini"),
    // falling back to the first group returned by `retrieveUserQuotaSummary`.
    let chosen_group = groups
        .iter()
        .find(|g| {
            let dname = g
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if dname.contains("gemini") {
                return true;
            }
            g.get("buckets")
                .and_then(Value::as_array)
                .is_some_and(|bs| {
                    bs.iter().any(|b| {
                        b.get("bucketId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_ascii_lowercase()
                            .contains("gemini")
                    })
                })
        })
        .unwrap_or(&groups[0]);

    let buckets = chosen_group.get("buckets")?.as_array()?;
    let mut metric_5h: Option<QuotaMetric> = None;
    let mut metric_7d: Option<QuotaMetric> = None;
    let mut others: Vec<QuotaMetric> = Vec::new();

    for b in buckets {
        let win_str = b
            .get("window")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let bid_str = b
            .get("bucketId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let is_5h = win_str == "5h" || bid_str.ends_with("-5h");
        let is_7d = win_str == "weekly" || win_str == "7d" || bid_str.ends_with("-weekly");
        let lbl = if is_5h {
            "5h".to_string()
        } else if is_7d {
            "7d".to_string()
        } else if !win_str.is_empty() {
            win_str
        } else {
            continue;
        };

        let raw_rem = (b
            .get("remainingFraction")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            * 100.0)
            .clamp(0.0, 100.0);
        let resets_at = b
            .get("resetTime")
            .and_then(Value::as_str)
            .and_then(parse_utc_iso8601_secs)
            .unwrap_or(0);
        let (rem_pct, detail) = if resets_at > 0 && now_secs >= resets_at {
            (100.0, None)
        } else if resets_at > now_secs {
            (raw_rem, Some(format_duration_short(resets_at - now_secs)))
        } else {
            (raw_rem, None)
        };
        let metric = QuotaMetric {
            window_label: lbl,
            remaining_percent: rem_pct,
            detail,
        };
        if is_5h && metric_5h.is_none() {
            metric_5h = Some(metric);
        } else if is_7d && metric_7d.is_none() {
            metric_7d = Some(metric);
        } else {
            others.push(metric);
        }
    }

    let mut out = Vec::new();
    if let Some(m) = metric_5h {
        out.push(m);
    }
    if let Some(m) = metric_7d {
        out.push(m);
    }
    out.extend(others);

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn fetch_antigravity_api_quota(now_secs: u64) -> Option<Vec<QuotaMetric>> {
    let script = r#"
import ctypes, datetime, json, os, re, shutil, sys, time, urllib.parse, urllib.request

def load_agy_token():
    lib = None
    for cand in ("libsecret-1.so.0", "/usr/lib/libsecret-1.so.0", "/usr/lib/x86_64-linux-gnu/libsecret-1.so.0"):
        try:
            lib = ctypes.CDLL(cand)
            break
        except Exception:
            pass
    if lib is None and os.path.isdir("/nix/store"):
        with os.scandir("/nix/store") as it:
            for e in it:
                if "libsecret-" in e.name and not e.name.endswith(".drv"):
                    p = os.path.join(e.path, "lib/libsecret-1.so.0")
                    if os.path.exists(p):
                        try:
                            lib = ctypes.CDLL(p)
                            break
                        except Exception:
                            pass
    if lib is not None:
        try:
            class SecretSchemaAttribute(ctypes.Structure):
                _fields_ = [("name", ctypes.c_char_p), ("type", ctypes.c_int)]
            class SecretSchema(ctypes.Structure):
                _fields_ = [("name", ctypes.c_char_p), ("flags", ctypes.c_int), ("attributes", SecretSchemaAttribute * 32)]
            attrs = (SecretSchemaAttribute * 32)()
            attrs[0] = SecretSchemaAttribute(b"service", 0)
            attrs[1] = SecretSchemaAttribute(b"username", 0)
            attrs[2] = SecretSchemaAttribute(None, 0)
            schema = SecretSchema(b"org.freedesktop.Secret.Generic", 2, attrs)
            lib.secret_password_lookup_sync.restype = ctypes.c_char_p
            raw = lib.secret_password_lookup_sync(
                ctypes.byref(schema), None, None, b"service", b"gemini", b"username", b"antigravity", None
            )
            if raw:
                return json.loads(raw.decode("utf-8", errors="ignore"))
        except Exception:
            pass
    home = os.environ.get("HOME", "")
    tok_file = os.path.join(home, ".gemini/antigravity-cli/antigravity-oauth-token")
    if os.path.isfile(tok_file):
        with open(tok_file, "r", encoding="utf-8", errors="ignore") as f:
            return json.load(f)
    return None

def refresh_oauth_token(refresh_token):
    agy_bin = shutil.which("agy") or "/run/current-system/sw/bin/agy"
    with open(agy_bin, "rb") as f:
        blob = f.read()
    cid_m = re.search(rb"[0-9]{10,}-[a-z0-9]+\.apps\.googleusercontent\.com", blob)
    csec_m = re.search(rb"GOCSPX-[A-Za-z0-9_-]{24,36}", blob)
    if not cid_m or not csec_m:
        return None
    data = urllib.parse.urlencode({
        "client_id": cid_m.group(0).decode("ascii"),
        "client_secret": csec_m.group(0).decode("ascii"),
        "refresh_token": refresh_token,
        "grant_type": "refresh_token",
    }).encode()
    req = urllib.request.Request("https://oauth2.googleapis.com/token", data=data)
    return json.loads(urllib.request.urlopen(req, timeout=4).read().decode())["access_token"]

def query_quota(access_token, auth_method):
    headers = {
        "Authorization": f"Bearer {access_token}",
        "Content-Type": "application/json",
        "User-Agent": f"antigravity/cli/1.5.3 (aidev_client; os_type=linux; arch=amd64; auth_method={auth_method})",
        "X-Goog-Api-Client": "gl-go/1.24.0",
    }
    req = urllib.request.Request(
        "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
        data=json.dumps({"project": "aicode-consumers"}).encode(),
        headers=headers,
    )
    return urllib.request.urlopen(req, timeout=4).read().decode("utf-8", errors="ignore")

obj = load_agy_token()
if not obj:
    sys.exit(1)
auth_method = (obj.get("auth_method") or "consumer").strip() or "consumer"
tok_info = obj.get("token") or {}
access_token = tok_info.get("access_token")
refresh_token = tok_info.get("refresh_token")
expiry_str = tok_info.get("expiry") or ""
need_refresh = True
if access_token and expiry_str:
    try:
        exp_ts = datetime.datetime.fromisoformat(expiry_str.replace("Z", "+00:00")).timestamp()
        if exp_ts > time.time() + 60:
            need_refresh = False
    except Exception:
        pass

if need_refresh and refresh_token:
    access_token = refresh_oauth_token(refresh_token)

if not access_token:
    sys.exit(1)

try:
    out = query_quota(access_token, auth_method)
except Exception:
    if not need_refresh and refresh_token:
        access_token = refresh_oauth_token(refresh_token)
        out = query_quota(access_token, auth_method)
    else:
        raise

print(out)
"#;

    let output = Command::new("python3")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_antigravity_quota_summary(&text, now_secs)
}

pub fn count_antigravity_transcript_steps_in_str(
    text: &str,
    cutoff_5h: u64,
    midnight_secs: u64,
) -> (u64, u64) {
    let mut steps_5h = 0u64;
    let mut steps_day = 0u64;
    for line in text.lines() {
        if !line.contains("\"PLANNER_RESPONSE\"") {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("type").and_then(Value::as_str) != Some("PLANNER_RESPONSE") {
            continue;
        }
        let Some(ts) = val
            .get("created_at")
            .or_else(|| val.get("timestamp"))
            .and_then(|v| {
                v.as_str()
                    .and_then(parse_utc_iso8601_secs)
                    .or_else(|| v.as_u64())
            })
        else {
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

fn collect_antigravity_metrics(now_secs: u64, cfg: &QuotaFileConfig) -> Vec<QuotaMetric> {
    if let Some(api_metrics) = fetch_antigravity_api_quota(now_secs) {
        return api_metrics;
    }

    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let brain_dir = home.join(".gemini/antigravity-cli/brain");
    let limit_5h = cfg.agy_5h_steps.unwrap_or(300).max(1);
    let limit_day = cfg.agy_daily_steps.unwrap_or(1000).max(1);

    let cutoff_5h = now_secs.saturating_sub(5 * 3_600);
    let midnight_secs = today_midnight_unix_secs(now_secs, local_utc_offset_secs());
    let min_cutoff_secs = cutoff_5h.min(midnight_secs);
    let min_mtime = UNIX_EPOCH.checked_add(Duration::from_secs(min_cutoff_secs));

    let mut steps_5h = 0u64;
    let mut steps_day = 0u64;
    if let Ok(entries) = fs::read_dir(&brain_dir) {
        for entry in entries.flatten() {
            let log_path = entry
                .path()
                .join(".system_generated/logs/transcript.jsonl");
            let Ok(meta) = fs::metadata(&log_path) else {
                continue;
            };
            let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
            if min_mtime.is_some_and(|m| mtime < m) {
                continue;
            }
            let Some(tail) = read_tail_string(&log_path, 512 * 1024) else {
                continue;
            };
            let (s5, sd) =
                count_antigravity_transcript_steps_in_str(&tail, cutoff_5h, midnight_secs);
            steps_5h += s5;
            steps_day += sd;
        }
    }

    let used_5h = ((steps_5h as f64 / limit_5h as f64) * 100.0).clamp(0.0, 100.0);
    let used_day = ((steps_day as f64 / limit_day as f64) * 100.0).clamp(0.0, 100.0);

    vec![
        QuotaMetric {
            window_label: "5h".to_string(),
            remaining_percent: (100.0 - used_5h).clamp(0.0, 100.0),
            detail: Some(format!("{steps_5h}/{limit_5h} steps")),
        },
        QuotaMetric {
            window_label: "今日".to_string(),
            remaining_percent: (100.0 - used_day).clamp(0.0, 100.0),
            detail: Some(format!("{steps_day}/{limit_day} steps")),
        },
    ]
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

        // Also verify live `/backend-api/wham/usage` JSON parsing
        let wham_sample = r#"{"rate_limit":{"allowed":true,"limit_reached":false,"primary_window":{"used_percent":17,"limit_window_seconds":18000,"reset_after_seconds":10860,"reset_at":11860},"secondary_window":{"used_percent":84,"limit_window_seconds":604800,"reset_after_seconds":446400,"reset_at":447400}}}"#;
        let wham_metrics = parse_codex_wham_usage_json(wham_sample, 1000).unwrap();
        assert_eq!(wham_metrics.len(), 2);
        assert_eq!(wham_metrics[0].window_label, "5h");
        assert_eq!(wham_metrics[0].remaining_percent, 83.0);
        assert_eq!(wham_metrics[0].detail.as_deref(), Some("3h01m"));
        assert_eq!(wham_metrics[1].window_label, "7d");
        assert_eq!(wham_metrics[1].remaining_percent, 16.0);
        assert_eq!(wham_metrics[1].detail.as_deref(), Some("5d4h"));
    }

    #[test]
    fn parses_antigravity_quota_summary_gemini_models_remaining_percentages() {
        let sample = r#"{
          "groups": [
            {
              "displayName": "Gemini Models",
              "description": "Models within this group: Gemini Flash, Gemini Pro",
              "buckets": [
                {
                  "bucketId": "gemini-weekly",
                  "displayName": "Weekly Limit Remaining",
                  "window": "weekly",
                  "resetTime": "2026-11-14T00:28:34Z",
                  "remainingFraction": 0.4396
                },
                {
                  "bucketId": "gemini-5h",
                  "displayName": "Five Hour Limit Remaining",
                  "window": "5h",
                  "resetTime": "2026-11-11T10:41:18Z",
                  "remainingFraction": 0.9545
                }
              ]
            },
            {
              "displayName": "Claude and GPT models",
              "buckets": [
                {
                  "bucketId": "3p-weekly",
                  "window": "weekly",
                  "resetTime": "2026-11-12T07:31:10Z",
                  "remainingFraction": 0.0084
                }
              ]
            }
          ]
        }"#;

        let now_secs = parse_utc_iso8601_secs("2026-11-11T06:50:00Z").unwrap();
        let metrics = parse_antigravity_quota_summary(sample, now_secs).unwrap();
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].window_label, "5h");
        assert!((metrics[0].remaining_percent - 95.45).abs() < 1e-4);
        assert_eq!(metrics[0].detail.as_deref(), Some("3h51m"));
        assert_eq!(metrics[1].window_label, "7d");
        assert!((metrics[1].remaining_percent - 43.96).abs() < 1e-4);
        assert_eq!(metrics[1].detail.as_deref(), Some("2d17h"));
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
        let tmp_dir = std::env::temp_dir().join(format!(
            "hct_test_codex_sessions_{}",
            std::process::id()
        ));
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
            format!("{line_older}\n{{\"timestamp\":\"2026-11-11T10:05:00Z\",\"type\":\"other\"}}\n"),
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
        let tmp_dir = std::env::temp_dir().join(format!(
            "hct_test_claude_projects_{}",
            std::process::id()
        ));
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
}
