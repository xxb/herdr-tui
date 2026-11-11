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
use std::sync::{Arc, Mutex};
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
    if let Some(addr) = world.active_terminal_address() {
        if let Some(info) = world.terminals.get(&addr) {
            if let Some(kind) = detect_agent_from_terminal(info) {
                return Some(kind);
            }
        }
    }
    if let Some(tab) = world.active_tab() {
        for addr in world.tab_terminals(tab) {
            if let Some(info) = world.terminals.get(&addr) {
                if let Some(kind) = detect_agent_from_terminal(info) {
                    return Some(kind);
                }
            }
        }
        if let Some(kind) = detect_agent_from_str(&tab.name) {
            return Some(kind);
        }
    }
    None
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
    let s = s.trim().strip_suffix('Z')?;
    let (date_part, time_part) = s.split_once('T')?;
    let mut d_it = date_part.split('-');
    let year: i64 = d_it.next()?.parse().ok()?;
    let month: i64 = d_it.next()?.parse().ok()?;
    let day: i64 = d_it.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut t_it = time_part.split(':');
    let hour: u64 = t_it.next()?.parse().ok()?;
    let min: u64 = t_it.next()?.parse().ok()?;
    let sec_str = t_it.next()?;
    let sec_int_str = sec_str.split('.').next().unwrap_or(sec_str);
    let sec: u64 = sec_int_str.parse().ok()?;

    // Howard Hinnant's days_from_civil algorithm
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    if days < 0 {
        return None;
    }
    Some((days as u64) * 86_400 + hour * 3_600 + min * 60 + sec)
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

pub fn parse_codex_rate_limits_from_jsonl(text: &str, now_secs: u64) -> Option<Vec<QuotaMetric>> {
    for line in text.lines().rev() {
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
            return Some(metrics);
        }
    }
    None
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

fn collect_codex_metrics(now_secs: u64) -> Vec<QuotaMetric> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    if let Some(wham_metrics) = fetch_codex_wham_usage(&home, now_secs) {
        return wham_metrics;
    }
    let sessions_dir = home.join(".codex/sessions");
    let mut files = Vec::new();
    let min_mtime = SystemTime::now().checked_sub(Duration::from_secs(7 * 86_400));
    collect_recursive_files(&sessions_dir, "jsonl", min_mtime, &mut files);
    files.sort_by(|a, b| b.0.cmp(&a.0));

    for (_, path) in files.into_iter().take(6) {
        if let Some(tail) = read_tail_string(&path, 256 * 1024) {
            if let Some(metrics) = parse_codex_rate_limits_from_jsonl(&tail, now_secs) {
                return metrics;
            }
        }
    }
    Vec::new()
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

fn collect_claude_metrics(_now_secs: u64, cfg: &QuotaFileConfig) -> Vec<QuotaMetric> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let projects_dir = home.join(".claude/projects");
    if !projects_dir.is_dir() {
        return Vec::new();
    }
    let limit_5h_toks = cfg.claude_5h_tokens.unwrap_or(2_500_000).max(1);
    let limit_5h_msgs = cfg.claude_5h_messages.unwrap_or(45).max(1);
    let limit_7d_toks = cfg.claude_7d_tokens.unwrap_or(25_000_000).max(1);
    let limit_7d_msgs = cfg.claude_7d_messages.unwrap_or(450).max(1);

    let script = r#"
import glob, os, json, sys, time, datetime
projects_dir = sys.argv[1]
now = time.time()
cut_7d = now - 7 * 86400
cut_5h = now - 5 * 3600
tok_5h = 0
msg_5h = 0
tok_7d = 0
msg_7d = 0
oldest_5h = None
for root, _, files in os.walk(projects_dir):
    for fn in files:
        if not fn.endswith(".jsonl"):
            continue
        p = os.path.join(root, fn)
        try:
            st = os.stat(p)
        except Exception:
            continue
        if st.st_mtime < cut_7d:
            continue
        try:
            with open(p, "r", errors="ignore") as f:
                for line in f:
                    if '"usage"' not in line:
                        continue
                    obj = json.loads(line)
                    msg = obj.get("message") or {}
                    u = msg.get("usage") or obj.get("usage")
                    if not isinstance(u, dict):
                        continue
                    ts_str = obj.get("timestamp")
                    ts = st.st_mtime
                    if isinstance(ts_str, str) and ts_str:
                        try:
                            ts = datetime.datetime.fromisoformat(ts_str.replace("Z", "+00:00")).timestamp()
                        except Exception:
                            pass
                    if ts < cut_7d:
                        continue
                    t = int(u.get("input_tokens") or 0) + int(u.get("output_tokens") or 0) + int(u.get("cache_creation_input_tokens") or 0)
                    tok_7d += t
                    msg_7d += 1
                    if ts >= cut_5h:
                        tok_5h += t
                        msg_5h += 1
                        if oldest_5h is None or ts < oldest_5h:
                            oldest_5h = ts
        except Exception:
            continue
reset_5h = int((oldest_5h + 5 * 3600) - now) if oldest_5h is not None else 0
print(json.dumps({"tok_5h": tok_5h, "msg_5h": msg_5h, "tok_7d": tok_7d, "msg_7d": msg_7d, "reset_5h": max(0, reset_5h)}))
"#;

    let Ok(output) = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(&projects_dir)
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
    let tok_5h = val.get("tok_5h").and_then(Value::as_u64).unwrap_or(0);
    let msg_5h = val.get("msg_5h").and_then(Value::as_u64).unwrap_or(0);
    let tok_7d = val.get("tok_7d").and_then(Value::as_u64).unwrap_or(0);
    let msg_7d = val.get("msg_7d").and_then(Value::as_u64).unwrap_or(0);
    let reset_5h = val.get("reset_5h").and_then(Value::as_u64).unwrap_or(0);

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

    let mut steps_5h = 0u64;
    let mut steps_day = 0u64;
    let min_mtime = SystemTime::now().checked_sub(Duration::from_secs(86_400));
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
            };
            let Some(tail) = read_tail_string(&log_path, 256 * 1024) else {
                continue;
            };
            let mtime_secs = mtime
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(now_secs);
            for line in tail.lines() {
                if !line.contains("\"PLANNER_RESPONSE\"") {
                    continue;
                }
                steps_day += 1;
                if now_secs.saturating_sub(mtime_secs) <= 5 * 3_600 {
                    steps_5h += 1;
                }
            }
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

fn collect_freebuff_metrics(_now_secs: u64) -> Vec<QuotaMetric> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let cache_dir = home.join(".cache/herdr-freebuff");
    let min_mtime = SystemTime::now().checked_sub(Duration::from_secs(86_400));
    let mut today_briefs = 0u64;
    if let Ok(entries) = fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
                if min_mtime.is_none_or(|m| mtime >= m) {
                    today_briefs += 1;
                }
            }
        }
    }
    let used_pct = ((today_briefs as f64 / 100.0) * 100.0).clamp(0.0, 100.0);
    vec![QuotaMetric {
        window_label: "今日".to_string(),
        remaining_percent: (100.0 - used_pct).clamp(0.0, 100.0),
        detail: Some(format!("{today_briefs} briefs")),
    }]
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
        assert_eq!(wham_metrics[1].detail.as_deref(), Some("5d04h"));
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
}
