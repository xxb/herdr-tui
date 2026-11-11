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
    /// Used percentage (`0.0..=100.0`).
    pub used_percent: f64,
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
                let should_collect = {
                    let guard = cache_bg.lock().unwrap();
                    guard
                        .get(&kind)
                        .is_none_or(|(ts, _)| ts.elapsed() >= CACHE_TTL)
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
        let mut need_refresh = false;
        let cached = {
            let guard = self.cache.lock().unwrap();
            if let Some((ts, q)) = guard.get(&kind) {
                if ts.elapsed() >= CACHE_TTL {
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

pub fn render_progress_bar(used_percent: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let clamped = used_percent.clamp(0.0, 100.0);
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
                let raw_pct = win
                    .get("used_percent")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                let resets_at = win.get("resets_at").and_then(Value::as_u64).unwrap_or(0);
                let (used_pct, detail) = if resets_at > 0 && now_secs >= resets_at {
                    (0.0, None)
                } else if resets_at > now_secs {
                    (
                        raw_pct.clamp(0.0, 100.0),
                        Some(format_duration_short(resets_at - now_secs)),
                    )
                } else {
                    (raw_pct.clamp(0.0, 100.0), None)
                };
                metrics.push(QuotaMetric {
                    window_label: lbl,
                    used_percent: used_pct,
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

fn collect_codex_metrics(now_secs: u64) -> Vec<QuotaMetric> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
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

    let day_pct = ((day_reqs as f64 / daily_limit as f64) * 100.0).clamp(0.0, 100.0);
    let five_pct = ((five_reqs as f64 / limit_5h as f64) * 100.0).clamp(0.0, 100.0);

    vec![
        QuotaMetric {
            window_label: "今日".to_string(),
            used_percent: day_pct,
            detail: Some(format!("{day_reqs}/{daily_limit} · {}", format_tokens(day_toks))),
        },
        QuotaMetric {
            window_label: "5h".to_string(),
            used_percent: five_pct,
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

    let pct_5h = ((tok_5h as f64 / limit_5h_toks as f64)
        .max(msg_5h as f64 / limit_5h_msgs as f64)
        * 100.0)
        .clamp(0.0, 100.0);
    let pct_7d = ((tok_7d as f64 / limit_7d_toks as f64)
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
            used_percent: pct_5h,
            detail: Some(detail_5h),
        },
        QuotaMetric {
            window_label: "7d".to_string(),
            used_percent: pct_7d,
            detail: Some(format_tokens(tok_7d)),
        },
    ]
}

fn collect_antigravity_metrics(now_secs: u64, cfg: &QuotaFileConfig) -> Vec<QuotaMetric> {
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

    let pct_5h = ((steps_5h as f64 / limit_5h as f64) * 100.0).clamp(0.0, 100.0);
    let pct_day = ((steps_day as f64 / limit_day as f64) * 100.0).clamp(0.0, 100.0);

    vec![
        QuotaMetric {
            window_label: "5h".to_string(),
            used_percent: pct_5h,
            detail: Some(format!("{steps_5h}/{limit_5h} steps")),
        },
        QuotaMetric {
            window_label: "今日".to_string(),
            used_percent: pct_day,
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
    let pct = ((today_briefs as f64 / 100.0) * 100.0).clamp(0.0, 100.0);
    vec![QuotaMetric {
        window_label: "今日".to_string(),
        used_percent: pct,
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
    fn parses_codex_rate_limits_and_resets_expired_windows() {
        let sample = r#"{"timestamp":"2026-11-11T04:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":49.0,"window_minutes":300,"resets_at":2000},"secondary":{"used_percent":12.0,"window_minutes":10080,"resets_at":500000}}}}"#;

        // Before `resets_at`: both 5h (49%) and 7d (12%) are active
        let metrics = parse_codex_rate_limits_from_jsonl(sample, 1000).unwrap();
        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].window_label, "5h");
        assert_eq!(metrics[0].used_percent, 49.0);
        assert_eq!(metrics[0].detail.as_deref(), Some("16m"));
        assert_eq!(metrics[1].window_label, "7d");
        assert_eq!(metrics[1].used_percent, 12.0);

        // After primary `resets_at` (2500 >= 2000): 5h window resets to 0%
        let expired = parse_codex_rate_limits_from_jsonl(sample, 2500).unwrap();
        assert_eq!(expired[0].used_percent, 0.0);
        assert_eq!(expired[0].detail, None);
        assert_eq!(expired[1].used_percent, 12.0);
    }
}
