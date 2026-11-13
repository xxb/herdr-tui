mod core;
mod keys;
mod model;
mod ui;
mod usage;
#[cfg(test)]
mod version;
mod wire;

pub(crate) const BUILD_VERSION: &str = env!("HCT_BUILD_VERSION");

use crate::core::{CoreClient, CoreMessage};
use crate::keys::{
    encode_key_event, is_prefix_key, match_direct_action, match_prefix_action, prefix_bytes,
    Action, FileConfig,
};
use crate::model::{EventDisposition, World};
use crate::ui::{
    collect_fuzzy_items, compute_terminal_rect, find_hit_target, is_on_dpad_disc,
    render_ui_with_pads_and_quota, resolve_dpad_disc_rect, resolve_fuzzy_selection,
    resolve_mod_cross_rect, HitRegion, HitTarget, Modal, PromptKind, TerminalSelection,
};
use crate::usage::UsageTracker;
use anyhow::Result;
use clap::Parser;
use crossterm::{
    cursor::{MoveTo, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute, queue,
    style::{
        Attribute as CrosstermAttribute, Color as CrosstermColor, SetAttribute, SetBackgroundColor,
        SetForegroundColor, SetUnderlineColor,
    },
    terminal::{
        disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, Clear, ClearType,
        EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{self, BufWriter, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

const SCROLLBACK_LINES: usize = 2000;
const ATTACH_HISTORY_LINES: usize = SCROLLBACK_LINES;
const MAX_EVENTS_PER_TICK: usize = 512;
const CREDIT_FLUSH_THRESHOLD_BYTES: usize = 16 * 1024;
const SIBLING_RESIZE_DEBOUNCE: Duration = Duration::from_millis(80);
const ATTACH_RETRY_DELAY: Duration = Duration::from_millis(250);
const CLIPBOARD_READ_TIMEOUT: Duration = Duration::from_millis(200);
const HORIZ_WHEEL_QUIET_GAP: Duration = Duration::from_millis(160);
const VERT_WHEEL_BLEED_GUARD: Duration = Duration::from_millis(180);

enum AppEvent {
    Core(CoreMessage),
    Input(Event),
    QuotaUpdated,
}

fn should_forward_input_event(ev: &Event) -> bool {
    match ev {
        Event::Mouse(mev) => !matches!(mev.kind, MouseEventKind::Moved),
        Event::Key(kev) => kev.kind == KeyEventKind::Press,
        Event::FocusGained | Event::FocusLost => false,
        Event::Resize(_, _) | Event::Paste(_) => true,
    }
}

fn write_terminal_enter_sequence<W: Write>(w: &mut W) -> io::Result<()> {
    execute!(
        w,
        SetForegroundColor(CrosstermColor::Reset),
        SetBackgroundColor(CrosstermColor::Reset),
        SetUnderlineColor(CrosstermColor::Reset),
        SetAttribute(CrosstermAttribute::Reset),
        EnterAlternateScreen,
        Clear(ClearType::All),
        MoveTo(0, 0),
        EnableMouseCapture,
        EnableBracketedPaste,
    )
}

fn write_terminal_leave_sequence<W: Write>(w: &mut W) -> io::Result<()> {
    execute!(
        w,
        EndSynchronizedUpdate,
        DisableBracketedPaste,
        DisableMouseCapture,
        SetForegroundColor(CrosstermColor::Reset),
        SetBackgroundColor(CrosstermColor::Reset),
        SetUnderlineColor(CrosstermColor::Reset),
        SetAttribute(CrosstermAttribute::Reset),
        Clear(ClearType::All),
        MoveTo(0, 0),
        LeaveAlternateScreen,
        SetForegroundColor(CrosstermColor::Reset),
        SetBackgroundColor(CrosstermColor::Reset),
        SetUnderlineColor(CrosstermColor::Reset),
        SetAttribute(CrosstermAttribute::Reset),
        Show,
    )
}

fn drain_pending_input_events() {
    for _ in 0..256 {
        match event::poll(Duration::ZERO) {
            Ok(true) => {
                if event::read().is_err() {
                    break;
                }
            }
            _ => break,
        }
    }
}

struct TerminalCleanupGuard;

impl Drop for TerminalCleanupGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = write_terminal_leave_sequence(&mut stdout);
        drain_pending_input_events();
        let _ = disable_raw_mode();
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "hct",
    version = BUILD_VERSION,
    about = "herdr-tui (hct) — Multi-tab TUI client for herdr-core (hc) with bidirectional Space/Tab sync"
)]
struct Cli {
    /// Target herdr-core session name (same as HC_SESSION)
    #[arg(short, long)]
    session: Option<String>,

    /// Start with the left sidebar expanded
    #[arg(long)]
    sidebar: bool,

    /// Start with the left sidebar collapsed
    #[arg(long, conflicts_with = "sidebar")]
    no_sidebar: bool,

    /// Show the dual floating touch `+` cross pads (`Esc/Ctl/^B/Alt/Tab` and `↑/←/⎘/→/↓`)
    #[arg(long)]
    touch: bool,

    /// Hide the dual floating touch `+` cross pads
    #[arg(long, conflicts_with = "touch")]
    no_touch: bool,

    /// Allow running `hct` nested inside an active `hc`, `herdr`, `tmux`, `zellij`, or GNU `screen` session
    #[arg(long)]
    allow_nested: bool,

    /// Refuse running `hct` nested inside another terminal multiplexer (overrides `ui.allow_nested = true`)
    #[arg(long, conflicts_with = "allow_nested")]
    no_allow_nested: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NestedMultiplexer {
    program: &'static str,
    env_var: &'static str,
    value: String,
}

fn resolve_allow_nested(cli_allow: bool, cli_no_allow: bool, cfg_allow: Option<bool>) -> bool {
    if cli_allow {
        true
    } else if cli_no_allow {
        false
    } else {
        cfg_allow.unwrap_or(false)
    }
}

fn detect_nested_multiplexer<F>(mut get_env: F) -> Option<NestedMultiplexer>
where
    F: FnMut(&str) -> Option<String>,
{
    const CHECKS: &[(&str, &str, bool)] = &[
        ("hc (herdr-core)", "HC_TERMINAL_ID", false),
        ("hc (herdr-core)", "HC_TERMINAL_SOCKET", false),
        ("herdr", "HERDR_PANE_ID", false),
        ("herdr", "HERDR_SOCKET_PATH", false),
        ("herdr", "HERDR_ENV", true),
        ("tmux", "TMUX", false),
        ("zellij", "ZELLIJ", false),
        ("zellij", "ZELLIJ_SESSION_NAME", false),
        ("GNU screen", "STY", false),
    ];

    for &(program, env_var, is_bool_flag) in CHECKS {
        if let Some(raw) = get_env(env_var) {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            if is_bool_flag && (trimmed == "0" || trimmed.eq_ignore_ascii_case("false")) {
                continue;
            }
            return Some(NestedMultiplexer {
                program,
                env_var,
                value: trimmed.to_string(),
            });
        }
    }
    None
}

fn format_nested_session_error(nested: &NestedMultiplexer) -> String {
    let detail = if !nested.value.contains('/')
        && !nested.value.contains('\\')
        && nested.value.chars().count() <= 32
    {
        format!("{}={}", nested.env_var, nested.value)
    } else {
        format!("${}", nested.env_var)
    };
    format!(
        "Sessions should not be nested: already inside an active {} session (detected {}).\n\
         Detach or open a plain terminal first, or pass `hct --allow-nested` (or set `ui.allow_nested = true` in config.toml) to force.",
        nested.program, detail
    )
}

struct AttachedTerminal {
    channel: u32,
    parser: vt100::Parser,
    cols: u16,
    rows: u16,
    utf8_tail: Vec<u8>,
    prev_width1_symbol: bool,
    vt_seq: u64,
}

fn non_zero_u16(v: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(v).unwrap_or(std::num::NonZeroU16::MIN)
}

impl AttachedTerminal {
    fn new(channel: u32, cols: u16, rows: u16) -> Self {
        Self {
            channel,
            parser: vt100::Parser::new(non_zero_u16(rows), non_zero_u16(cols), SCROLLBACK_LINES),
            cols,
            rows,
            utf8_tail: Vec::new(),
            prev_width1_symbol: false,
            vt_seq: 0,
        }
    }

    fn process_vt_bytes(&mut self, payload: &[u8]) {
        let clean = sanitize_vt_stream(payload, &mut self.utf8_tail, &mut self.prev_width1_symbol);
        if clean.is_empty() {
            return;
        }
        const DECSTR: &[u8] = b"\x1b[!p";
        let mut rest: &[u8] = &clean;
        while let Some(idx) = rest.windows(DECSTR.len()).position(|w| w == DECSTR) {
            if idx > 0 {
                self.parser.process(&rest[..idx]);
            }
            apply_decstr(&mut self.parser);
            rest = &rest[idx + DECSTR.len()..];
        }
        if !rest.is_empty() {
            self.parser.process(rest);
        }
        self.vt_seq = self.vt_seq.wrapping_add(1);
    }
}

/// Emulate DECSTR (`CSI ! p` — Soft Terminal Reset), which `herdr-core` emits on attach replay
/// and `vt100` otherwise ignores as `unhandled_csi`. Resets SGR, cursor/keypad/origin modes,
/// scroll margins (`DECSTBM`), cursor visibility, and saved-cursor state (`DECSC`) while
/// preserving the active physical cursor position (unlike `CSI r` / `CSI ?6l` alone, which home).
fn apply_decstr(parser: &mut vt100::Parser) {
    let (row, col) = parser.screen().cursor_position();
    parser.process(b"\x1b[0m\x1b[?1l\x1b[?6l\x1b[r\x1b[?25h\x1b>\x1b7");
    let restore = format!("\x1b[{};{}H", row.saturating_add(1), col.saturating_add(1));
    parser.process(restore.as_bytes());
}

fn last_utf8_char(buf: &[u8]) -> Option<char> {
    if buf.is_empty() {
        return None;
    }
    let mut start = buf.len() - 1;
    while start > 0 && start + 4 > buf.len() && (buf[start] & 0b1100_0000) == 0b1000_0000 {
        start -= 1;
    }
    std::str::from_utf8(&buf[start..]).ok()?.chars().next_back()
}

fn trailing_incomplete_vt_or_utf8_len(buf: &[u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    if buf[buf.len() - 1] == 0x1b {
        return 1;
    }
    let max_csi_back = buf.len().min(128);
    for back in 2..=max_csi_back {
        let idx = buf.len() - back;
        if buf[idx] == 0x1b && buf[idx + 1] == b'[' {
            if buf[idx + 2..].iter().all(|&b| (0x20..=0x3F).contains(&b)) {
                return back;
            }
            break;
        }
    }
    let max_back = buf.len().min(3);
    for back in 1..=max_back {
        let idx = buf.len() - back;
        let b = buf[idx];
        if (b & 0b1100_0000) != 0b1000_0000 {
            if b >= 0xC2
                && std::str::from_utf8(&buf[idx..])
                    .err()
                    .is_some_and(|e| e.error_len().is_none())
            {
                return back;
            }
            return 0;
        }
    }
    0
}

fn next_utf8_char(slice: &[u8]) -> Option<(char, usize)> {
    let max_len = slice.len().min(4);
    match std::str::from_utf8(&slice[..max_len]) {
        Ok(s) => {
            let ch = s.chars().next()?;
            Some((ch, ch.len_utf8()))
        }
        Err(e) if e.valid_up_to() > 0 => {
            let s = std::str::from_utf8(&slice[..e.valid_up_to()]).ok()?;
            let ch = s.chars().next()?;
            Some((ch, ch.len_utf8()))
        }
        Err(_) => None,
    }
}

fn is_width1_non_ascii(ch: char) -> bool {
    !ch.is_ascii() && UnicodeWidthChar::width(ch) == Some(1)
}

fn sanitize_vt_stream<'a>(
    payload: &'a [u8],
    utf8_tail: &mut Vec<u8>,
    prev_width1_symbol: &mut bool,
) -> Cow<'a, [u8]> {
    let combined: Cow<'a, [u8]> = if utf8_tail.is_empty() {
        Cow::Borrowed(payload)
    } else {
        let mut buf = Vec::with_capacity(utf8_tail.len() + payload.len());
        buf.extend_from_slice(utf8_tail);
        buf.extend_from_slice(payload);
        utf8_tail.clear();
        Cow::Owned(buf)
    };

    let trailing = trailing_incomplete_vt_or_utf8_len(&combined);
    let valid_len = combined.len() - trailing;
    if trailing > 0 {
        utf8_tail.extend_from_slice(&combined[valid_len..]);
    }
    let slice = &combined[..valid_len];
    if slice.is_empty() {
        return Cow::Borrowed(&[]);
    }

    let has_esc = slice.contains(&0x1b);
    let has_fe0f = slice.windows(3).any(|w| w == b"\xef\xb8\x8f");

    if !has_esc && !has_fe0f {
        *prev_width1_symbol = last_utf8_char(slice).is_some_and(is_width1_non_ascii);
        return match combined {
            Cow::Borrowed(b) => Cow::Borrowed(&b[..valid_len]),
            Cow::Owned(mut v) => {
                v.truncate(valid_len);
                Cow::Owned(v)
            }
        };
    }

    let mut out = Vec::with_capacity(slice.len());
    let mut i = 0;
    while i < slice.len() {
        if has_esc && slice[i] == 0x1b && i + 1 < slice.len() && slice[i + 1] == b'[' {
            let param_start = i + 2;
            let mut p = param_start;
            while p < slice.len() && (0x30..=0x3F).contains(&slice[p]) {
                p += 1;
            }
            let param_end = p;
            while p < slice.len() && (0x20..=0x2F).contains(&slice[p]) {
                p += 1;
            }
            let inter_end = p;
            if p < slice.len() && (0x40..=0x7E).contains(&slice[p]) {
                let params = &slice[param_start..param_end];
                let intermediates = &slice[param_end..inter_end];
                let final_byte = slice[p];
                *prev_width1_symbol = false;
                if intermediates.is_empty() {
                    if params == b"?9001" && (final_byte == b'h' || final_byte == b'l') {
                        i = p + 1;
                        continue;
                    }
                    if params.iter().all(|&b| b.is_ascii_digit() || b == b';') {
                        match final_byte {
                            // CHT (`CSI n I` — Cursor Forward Tabulation) -> `n` HT (`\t`) stops
                            b'I' => {
                                let count = std::str::from_utf8(params)
                                    .ok()
                                    .and_then(|s| s.split(';').next())
                                    .and_then(|s| {
                                        if s.is_empty() {
                                            Some(1usize)
                                        } else {
                                            s.parse::<usize>().ok()
                                        }
                                    })
                                    .unwrap_or(1)
                                    .clamp(1, 256);
                                out.extend(std::iter::repeat_n(b'\t', count));
                                i = p + 1;
                                continue;
                            }
                            // ECMA-48 HVP (`CSI row ; col f`), used heavily by `btop` (`Mv::to`),
                            // is identical to CUP (`CSI row ; col H`), which `vt100` supports.
                            b'f' => {
                                out.extend_from_slice(b"\x1b[");
                                out.extend_from_slice(params);
                                out.push(b'H');
                                i = p + 1;
                                continue;
                            }
                            // HPA (`CSI col \``) -> CHA (`CSI col G`)
                            b'`' => {
                                out.extend_from_slice(b"\x1b[");
                                out.extend_from_slice(params);
                                out.push(b'G');
                                i = p + 1;
                                continue;
                            }
                            // HPR (`CSI n a`) -> CUF (`CSI n C`)
                            b'a' => {
                                out.extend_from_slice(b"\x1b[");
                                out.extend_from_slice(params);
                                out.push(b'C');
                                i = p + 1;
                                continue;
                            }
                            // VPR (`CSI n e`) -> CUD (`CSI n B`)
                            b'e' => {
                                out.extend_from_slice(b"\x1b[");
                                out.extend_from_slice(params);
                                out.push(b'B');
                                i = p + 1;
                                continue;
                            }
                            // CNL (`CSI n E`) -> CUD (`CSI n B`) + CR (`\r`)
                            b'E' => {
                                out.extend_from_slice(b"\x1b[");
                                out.extend_from_slice(params);
                                out.extend_from_slice(b"B\r");
                                i = p + 1;
                                continue;
                            }
                            // CPL (`CSI n F`) -> CUU (`CSI n A`) + CR (`\r`)
                            b'F' => {
                                out.extend_from_slice(b"\x1b[");
                                out.extend_from_slice(params);
                                out.extend_from_slice(b"A\r");
                                i = p + 1;
                                continue;
                            }
                            // SCOSC (`CSI s`) -> DECSC (`ESC 7`)
                            b's' if params.is_empty() => {
                                out.extend_from_slice(b"\x1b7");
                                i = p + 1;
                                continue;
                            }
                            // SCORC (`CSI u`) -> DECRC (`ESC 8`)
                            b'u' if params.is_empty() => {
                                out.extend_from_slice(b"\x1b8");
                                i = p + 1;
                                continue;
                            }
                            _ => {}
                        }
                    }
                }
                out.extend_from_slice(&slice[i..=p]);
                i = p + 1;
                continue;
            }
        }
        if has_fe0f && i + 3 <= slice.len() && &slice[i..i + 3] == b"\xef\xb8\x8f" {
            // U+FE0F (Variation Selector-16) requests 2-column emoji presentation for preceding
            // 1-column symbols (such as U+2699 '⚙' or U+26A0 '⚠'). Replacing U+FE0F with an ASCII
            // space after a width-1 non-ASCII symbol guarantees identical 2-column grid alignment
            // across both Grapheme-Cluster terminals (Ghostty, Kitty) and legacy SSH clients.
            if *prev_width1_symbol {
                out.push(b' ');
                *prev_width1_symbol = false;
            }
            i += 3;
            continue;
        }
        if slice[i].is_ascii() {
            out.push(slice[i]);
            *prev_width1_symbol = false;
            i += 1;
        } else if let Some((ch, ch_len)) = next_utf8_char(&slice[i..]) {
            out.extend_from_slice(&slice[i..i + ch_len]);
            *prev_width1_symbol = is_width1_non_ascii(ch);
            i += ch_len;
        } else {
            out.push(slice[i]);
            *prev_width1_symbol = false;
            i += 1;
        }
    }
    Cow::Owned(out)
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn format_osc52_sequence(b64: &str, in_tmux: bool, in_screen: bool) -> String {
    if in_tmux {
        format!("\x1bPtmux;\x1b\x1b]52;c;{b64}\x07\x1b\\\x1b]52;c;{b64}\x07")
    } else if in_screen {
        // GNU screen caps individual DCS passthrough blocks (historically 256 bytes);
        // chunking into <=76-byte DCS frames forwards arbitrarily long OSC 52 payloads intact.
        let osc = format!("\x1b]52;c;{b64}\x07");
        let mut out = String::with_capacity(osc.len() + (osc.len() / 76 + 1) * 4);
        for chunk in osc.as_bytes().chunks(76) {
            out.push_str("\x1bP");
            out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
            out.push_str("\x1b\\");
        }
        out
    } else {
        format!("\x1b]52;c;{b64}\x07\x1b]52;;{b64}\x07")
    }
}

fn copy_to_clipboard(text: &str) {
    if text.is_empty() {
        return;
    }
    // 1. Emit OSC 52 clipboard escape sequence directly to the outer SSH / terminal stdout
    // so remote SSH clients (iPad Termius, Blink, ShellFish, WezTerm, Kitty, iTerm2, Ghostty)
    // automatically update the client device's native clipboard.
    let b64 = base64_encode(text.as_bytes());
    let in_tmux = std::env::var_os("TMUX").is_some_and(|v| !v.is_empty());
    let in_screen = std::env::var_os("STY").is_some_and(|v| !v.is_empty());
    let seq = format_osc52_sequence(&b64, in_tmux, in_screen);
    let mut stdout = io::stdout();
    let _ = stdout.write_all(seq.as_bytes());
    let _ = stdout.flush();

    // 2. Also attempt local clipboard helper in a detached background thread when a local display server is active
    let owned = text.to_string();
    std::thread::spawn(move || {
        let local_cmds: &[(&str, &[&str])] = &[
            ("pbcopy", &[]),
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
        ];
        for (prog, args) in local_cmds {
            if *prog == "wl-copy" && std::env::var_os("WAYLAND_DISPLAY").is_none() {
                continue;
            }
            if *prog == "xclip" && std::env::var_os("DISPLAY").is_none() {
                continue;
            }
            if let Ok(mut child) = Command::new(prog)
                .args(*args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(owned.as_bytes());
                }
                let _ = child.wait();
                break;
            }
        }
    });
}

fn run_clipboard_read_cmd(prog: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let res = io::Read::read_to_end(&mut stdout, &mut buf).map(|_| buf);
        let _ = tx.send(res);
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let rem = deadline.saturating_duration_since(Instant::now());
                let buf = rx
                    .recv_timeout(rem.max(Duration::from_millis(10)))
                    .ok()?
                    .ok()?;
                let s = String::from_utf8_lossy(&buf).into_owned();
                return (!s.is_empty()).then_some(s);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn read_clipboard_text(fallback: &str) -> String {
    if std::env::var_os("SSH_CONNECTION").is_none() && std::env::var_os("SSH_TTY").is_none() {
        let local_cmds: &[(&str, &[&str])] = &[
            ("pbpaste", &[]),
            ("wl-paste", &["--no-newline"]),
            ("xclip", &["-selection", "clipboard", "-o"]),
        ];
        for (prog, args) in local_cmds {
            if *prog == "wl-paste" && std::env::var_os("WAYLAND_DISPLAY").is_none() {
                continue;
            }
            if *prog == "xclip" && std::env::var_os("DISPLAY").is_none() {
                continue;
            }
            if let Some(s) = run_clipboard_read_cmd(prog, args, CLIPBOARD_READ_TIMEOUT) {
                return s;
            }
        }
    }
    fallback.to_string()
}

fn paste_text_into_app(
    text: &str,
    core: &CoreClient,
    world: &World,
    attached: Option<&AttachedTerminal>,
    scrollback_offset: &mut usize,
    modal: &mut Modal,
) {
    if text.is_empty() {
        return;
    }
    if matches!(modal, Modal::None) {
        *scrollback_offset = 0;
        if let Some(att) = attached {
            if att.parser.screen().bracketed_paste() {
                let mut payload = b"\x1b[200~".to_vec();
                payload.extend_from_slice(text.as_bytes());
                payload.extend_from_slice(b"\x1b[201~");
                core.send_input(att.channel, &payload);
            } else {
                core.send_input(att.channel, text.as_bytes());
            }
        }
    } else if let Modal::Prompt { input, .. } = modal {
        input.push_str(text);
    } else if let Modal::Fuzzy {
        query,
        selected,
        selected_tab_id,
    } = modal
    {
        query.push_str(text);
        *selected = 0;
        let items = collect_fuzzy_items(world, query);
        *selected_tab_id = items.first().map(|it| it.tab_id.clone());
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let file_cfg = FileConfig::load();
    let allow_nested = resolve_allow_nested(
        cli.allow_nested,
        cli.no_allow_nested,
        file_cfg.ui.allow_nested,
    );
    if !allow_nested {
        if let Some(nested) = detect_nested_multiplexer(|k| {
            std::env::var_os(k).map(|v| v.to_string_lossy().into_owned())
        }) {
            anyhow::bail!("{}", format_nested_session_error(&nested));
        }
    }
    let prefix_spec = file_cfg
        .keys
        .prefix
        .clone()
        .unwrap_or_else(|| "ctrl+b".to_string());
    let sidebar_width = file_cfg.ui.sidebar_width.unwrap_or(24).clamp(16, 48);
    let mobile_threshold = file_cfg.ui.mobile_width_threshold.unwrap_or(80);
    let touch_mode = if cli.touch {
        true
    } else if cli.no_touch {
        false
    } else {
        file_cfg.ui.touch_mode.unwrap_or(false)
    };

    let (app_tx, app_rx) = mpsc::channel::<AppEvent>();
    let core =
        CoreClient::connect_with_sender(cli.session.as_deref(), app_tx.clone(), AppEvent::Core)?;
    let snapshot = core.subscribe_events()?;

    let mut world = World::default();
    world.load_initial(&core, &snapshot)?;

    // Setup terminal with an immediate RAII guard so any setup or runtime error
    // restores raw mode, alternate screen, mouse capture, SGR state, and cursor visibility.
    enable_raw_mode()?;
    let _cleanup_guard = TerminalCleanupGuard;
    let mut stdout = BufWriter::with_capacity(64 * 1024, io::stdout());
    write_terminal_enter_sequence(&mut stdout)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let term_size = terminal.size()?;
    let initial_area = Rect::new(0, 0, term_size.width, term_size.height);
    let mut sidebar_open = if cli.sidebar {
        true
    } else if cli.no_sidebar {
        false
    } else if let Some(collapsed) = file_cfg.ui.sidebar_start_collapsed {
        !collapsed
    } else {
        initial_area.width >= mobile_threshold
    };

    let quota_tx = app_tx.clone();
    let usage_tracker = UsageTracker::new_with_waker(file_cfg.quota.clone(), move || {
        let _ = quota_tx.send(AppEvent::QuotaUpdated);
    });

    let input_running = Arc::new(AtomicBool::new(true));
    let input_flag = Arc::clone(&input_running);
    let input_tx = app_tx;
    let input_thread = std::thread::spawn(move || {
        while input_flag.load(Ordering::Relaxed) {
            match event::poll(Duration::from_millis(25)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if should_forward_input_event(&ev)
                            && input_tx.send(AppEvent::Input(ev)).is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });

    let res = run_app(
        &mut terminal,
        &core,
        &app_rx,
        &mut world,
        &usage_tracker,
        &prefix_spec,
        &mut sidebar_open,
        sidebar_width,
        mobile_threshold,
        touch_mode,
    );

    // Disable mouse capture, bracketed paste, and alternate screen BEFORE disabling raw mode
    // so late mouse/terminal packets never echo onto the outer shell prompt.
    input_running.store(false, Ordering::SeqCst);
    let leave_ok = write_terminal_leave_sequence(terminal.backend_mut()).is_ok();
    drop(app_rx);
    let _ = input_thread.join();
    drain_pending_input_events();
    let raw_ok = disable_raw_mode().is_ok();
    if leave_ok && raw_ok {
        std::mem::forget(_cleanup_guard);
    }

    res
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SyncOutcome {
    newly_attached_active: bool,
    active_attach_failed: bool,
    has_unwarmed_siblings: bool,
    has_unresized_siblings: bool,
}

fn active_needs_sync(
    want_addr: Option<&str>,
    active_addr: Option<&str>,
    attached_has_active: bool,
    pending_attach_retry: Option<&(String, Instant)>,
) -> bool {
    let cur_attached = active_addr.filter(|_| attached_has_active);
    if want_addr == cur_attached {
        return false;
    }
    if let Some((retry_addr, _)) = pending_attach_retry {
        if want_addr == Some(retry_addr.as_str()) {
            return false;
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn sync_attached_terminals(
    core: &CoreClient,
    world: &mut World,
    attached_map: &mut BTreeMap<String, AttachedTerminal>,
    active_addr: &mut Option<String>,
    term_rect: Rect,
    scrollback_offset: &mut usize,
    warm_siblings: bool,
    resize_siblings: bool,
) -> SyncOutcome {
    let mut outcome = SyncOutcome::default();
    let cols = term_rect.width.max(1);
    let rows = term_rect.height.max(1);

    // Prune any terminal channels that have exited or been removed from the world.
    attached_map.retain(|addr, att| {
        if world.terminals.contains_key(addr) {
            true
        } else {
            core.detach_channel(att.channel);
            false
        }
    });

    let want_addr = world.active_terminal_address();
    if want_addr != *active_addr {
        if let Some(prev) = active_addr.as_ref().and_then(|a| attached_map.get(a)) {
            core.set_focus(prev.channel, false);
        }
        *scrollback_offset = 0;
    }

    let mut resolved_active: Option<String> = None;
    if let Some(ref addr) = want_addr {
        if let Some(cur) = attached_map.get_mut(addr) {
            if cur.cols != cols || cur.rows != rows {
                cur.cols = cols;
                cur.rows = rows;
                cur.parser.set_size(non_zero_u16(rows), non_zero_u16(cols));
                core.resize_channel(cur.channel, cols, rows);
            }
            if active_addr.as_ref() != Some(addr) {
                core.set_focus(cur.channel, true);
                world.mark_active_seen(core);
            }
            resolved_active = Some(addr.clone());
        } else if let Ok(ch) = core.attach_terminal(addr, cols, rows, ATTACH_HISTORY_LINES) {
            core.set_focus(ch, true);
            attached_map.insert(addr.clone(), AttachedTerminal::new(ch, cols, rows));
            world.mark_active_seen(core);
            outcome.newly_attached_active = true;
            resolved_active = Some(addr.clone());
        } else {
            outcome.active_attach_failed = true;
        }
    }
    *active_addr = resolved_active;

    // Progressively pre-attach sibling terminals in the active space (at most 1 per pass)
    // and debounce background sibling SIGWINCH resizes so active interaction stays 0ms-responsive.
    if warm_siblings {
        let mut sibling_addrs = Vec::new();
        if let Some(space) = world.active_space() {
            for tab in &space.tabs {
                for addr in world.tab_terminals(tab) {
                    if Some(addr.as_str()) != want_addr.as_deref() && !sibling_addrs.contains(&addr)
                    {
                        sibling_addrs.push(addr);
                    }
                }
            }
        }
        let mut warmed_one = false;
        for sib_addr in sibling_addrs {
            if let Some(sib) = attached_map.get_mut(&sib_addr) {
                if sib.cols != cols || sib.rows != rows {
                    if resize_siblings {
                        sib.cols = cols;
                        sib.rows = rows;
                        sib.parser.set_size(non_zero_u16(rows), non_zero_u16(cols));
                        core.resize_channel(sib.channel, cols, rows);
                    } else {
                        outcome.has_unresized_siblings = true;
                    }
                }
            } else if world.terminals.contains_key(&sib_addr) {
                if !warmed_one {
                    if let Ok(ch) =
                        core.attach_terminal(&sib_addr, cols, rows, ATTACH_HISTORY_LINES)
                    {
                        core.set_focus(ch, false);
                        attached_map.insert(sib_addr, AttachedTerminal::new(ch, cols, rows));
                        warmed_one = true;
                    }
                } else {
                    outcome.has_unwarmed_siblings = true;
                }
            }
        }
        if warmed_one {
            if let Some(act) = active_addr.as_ref().and_then(|a| attached_map.get(a)) {
                core.set_focus(act.channel, true);
            }
        }
    }

    outcome
}

fn encode_wheel_for_terminal(
    screen: &vt100::Screen,
    scroll_up: bool,
    rel_col_1based: u16,
    rel_row_1based: u16,
) -> Option<Vec<u8>> {
    if !screen.alternate_screen() {
        return None;
    }
    if !matches!(screen.mouse_protocol_mode(), vt100::MouseProtocolMode::None) {
        let btn = if scroll_up { 64u8 } else { 65u8 };
        let col = rel_col_1based.max(1);
        let row = rel_row_1based.max(1);
        if matches!(
            screen.mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Sgr
        ) {
            Some(format!("\x1b[<{btn};{col};{row}M").into_bytes())
        } else {
            let cx = (col.min(223) as u8).saturating_add(32);
            let cy = (row.min(223) as u8).saturating_add(32);
            Some(vec![0x1b, b'[', b'M', 32 + btn, cx, cy])
        }
    } else {
        let seq: &[u8] = match (scroll_up, screen.application_cursor()) {
            (true, true) => b"\x1bOA\x1bOA\x1bOA",
            (true, false) => b"\x1b[A\x1b[A\x1b[A",
            (false, true) => b"\x1bOB\x1bOB\x1bOB",
            (false, false) => b"\x1b[B\x1b[B\x1b[B",
        };
        Some(seq.to_vec())
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_action(
    action: Action,
    core: &CoreClient,
    world: &mut World,
    attached: Option<&AttachedTerminal>,
    prefix_spec: &str,
    _sidebar_open: &mut bool,
    scrollback_offset: &mut usize,
    modal: &mut Modal,
) -> Result<bool> {
    match action {
        Action::Detach => return Ok(true),
        Action::SendPrefix => {
            if let Some(att) = attached {
                let bytes = prefix_bytes(prefix_spec);
                core.send_input(att.channel, &bytes);
            }
        }
        Action::NextTab => {
            world.next_tab();
            world.mark_active_seen(core);
        }
        Action::PrevTab => {
            world.prev_tab();
            world.mark_active_seen(core);
        }
        Action::SelectTab(idx) => {
            world.select_tab_index(idx);
            world.mark_active_seen(core);
        }
        Action::NewTab => {
            let _ = world.create_new_tab(core);
        }
        Action::CloseTab => {
            if let (Some(space), Some(tab)) = (world.active_space(), world.active_tab()) {
                let tab_id = tab.id.clone();
                let tab_title = world.tab_display_name(space, world.active_tab_index());
                *modal = Modal::ConfirmCloseTab { tab_id, tab_title };
            }
        }
        Action::RenameTab => {
            if let Some(tab) = world.active_tab() {
                *modal = Modal::Prompt {
                    kind: PromptKind::RenameTab {
                        tab_id: tab.id.clone(),
                    },
                    input: tab.name.clone(),
                };
            }
        }
        Action::NextSpace => {
            world.next_space();
            world.mark_active_seen(core);
        }
        Action::PrevSpace => {
            world.prev_space();
            world.mark_active_seen(core);
        }
        Action::NewSpace => {
            *modal = Modal::Prompt {
                kind: PromptKind::NewSpace,
                input: String::new(),
            };
        }
        Action::RenameSpace => {
            if let Some(space) = world.active_space() {
                *modal = Modal::Prompt {
                    kind: PromptKind::RenameSpace {
                        space_id: space.id.clone(),
                    },
                    input: space.name.clone(),
                };
            }
        }
        Action::ToggleSidebar => {
            *modal = if matches!(modal, Modal::SpaceMenu) {
                Modal::None
            } else {
                Modal::SpaceMenu
            };
        }
        Action::FuzzyFinder => {
            let items = collect_fuzzy_items(world, "");
            let active_sid = world.active_space().map(|s| s.id.clone());
            let active_tid = world.active_tab().map(|t| t.id.clone());
            let selected = match (active_sid.as_deref(), active_tid.as_deref()) {
                (Some(sid), Some(tid)) => items
                    .iter()
                    .position(|it| it.space_id == sid && it.tab_id == tid)
                    .unwrap_or(0),
                _ => 0,
            };
            let selected_tab_id = items.get(selected).map(|it| it.tab_id.clone());
            *modal = Modal::Fuzzy {
                query: String::new(),
                selected,
                selected_tab_id,
            };
        }
        Action::JumpAlertAgent => {
            if world.jump_next_alert_agent() {
                world.mark_active_seen(core);
            }
        }
        Action::CyclePaneInTab => {
            world.cycle_pane_in_tab();
            world.mark_active_seen(core);
        }
        Action::EnterScrollback => {
            *scrollback_offset = (*scrollback_offset).max(1);
        }
        Action::ToggleHelp => {
            *modal = if matches!(modal, Modal::Help) {
                Modal::None
            } else {
                Modal::Help
            };
        }
    }
    Ok(false)
}

#[derive(Debug, Clone)]
struct FloatingDPadDrag {
    is_mod: bool,
    start_mouse: (u16, u16),
    start_pos: (u16, u16),
    moved: bool,
    tap_target: Option<HitTarget>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HorizontalSwipeDir {
    Prev,
    Next,
}

#[derive(Clone, Debug, Default)]
struct HorizontalWheelGestureState {
    last_viewport_vertical_wheel_at: Option<Instant>,
    last_top_bar_vertical_wheel_at: Option<Instant>,
    last_horiz_dir: Option<HorizontalSwipeDir>,
    last_horiz_event_at: Option<Instant>,
}

impl HorizontalWheelGestureState {
    fn record_viewport_vertical_wheel(&mut self, now: Instant) {
        self.last_viewport_vertical_wheel_at = Some(now);
    }

    fn record_top_bar_vertical_wheel(&mut self, now: Instant) {
        self.last_top_bar_vertical_wheel_at = Some(now);
    }

    fn reset_burst(&mut self) {
        self.last_horiz_dir = None;
        self.last_horiz_event_at = None;
    }

    fn accept_horizontal_wheel(
        &mut self,
        dir: HorizontalSwipeDir,
        on_top_bar_or_space_menu: bool,
        now: Instant,
    ) -> bool {
        let bleed_ref = if on_top_bar_or_space_menu {
            self.last_top_bar_vertical_wheel_at
        } else {
            self.last_viewport_vertical_wheel_at
        };
        if bleed_ref.is_some_and(|t| now.saturating_duration_since(t) < VERT_WHEEL_BLEED_GUARD) {
            return false;
        }
        if self.last_horiz_dir == Some(dir)
            && self
                .last_horiz_event_at
                .is_some_and(|t| now.saturating_duration_since(t) < HORIZ_WHEEL_QUIET_GAP)
        {
            self.last_horiz_event_at = Some(now);
            return false;
        }
        self.last_horiz_dir = Some(dir);
        self.last_horiz_event_at = Some(now);
        true
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_horizontal_wheel_event(
    dir: HorizontalSwipeDir,
    mev_row: u16,
    term_rect: Rect,
    now: Instant,
    horiz_wheel: &mut HorizontalWheelGestureState,
    core: &CoreClient,
    world: &mut World,
    modal: &Modal,
    dpad_drag_active: bool,
    prefix_active: &mut bool,
    ctrl_active: &mut bool,
    alt_active: &mut bool,
    selection: &mut Option<TerminalSelection>,
    selection_screen: &mut Option<vt100::Screen>,
    selection_vt_seq: &mut Option<u64>,
) -> bool {
    if matches!(modal, Modal::SpaceMenu) {
        if !horiz_wheel.accept_horizontal_wheel(dir, true, now) {
            return false;
        }
        let prev_sid = world.active_space().map(|s| s.id.clone());
        match dir {
            HorizontalSwipeDir::Prev => world.prev_space(),
            HorizontalSwipeDir::Next => world.next_space(),
        }
        let space_changed = world.active_space().map(|s| s.id.as_str()) != prev_sid.as_deref();
        if space_changed {
            world.mark_active_seen(core);
        }
        return space_changed;
    }
    if !matches!(modal, Modal::None)
        || dpad_drag_active
        || selection.as_ref().is_some_and(|s| s.active)
    {
        return false;
    }
    let on_top_bar = mev_row < term_rect.y;
    if !horiz_wheel.accept_horizontal_wheel(dir, on_top_bar, now) {
        return false;
    }
    let had_sticky = *prefix_active || *ctrl_active || *alt_active;
    *prefix_active = false;
    *ctrl_active = false;
    *alt_active = false;
    let had_vis_sel = selection
        .as_ref()
        .is_some_and(TerminalSelection::is_non_empty);
    *selection = None;
    *selection_screen = None;
    *selection_vt_seq = None;
    let prev_ti = world.active_tab_index();
    match dir {
        HorizontalSwipeDir::Prev => world.prev_tab(),
        HorizontalSwipeDir::Next => world.next_tab(),
    }
    let tab_changed = world.active_tab_index() != prev_ti;
    if tab_changed {
        world.mark_active_seen(core);
    }
    tab_changed || had_sticky || had_vis_sel
}

fn record_credit(pending: &mut Vec<(u32, u32)>, channel: u32, bytes: u32) {
    if bytes == 0 {
        return;
    }
    if let Some(entry) = pending.iter_mut().find(|(ch, _)| *ch == channel) {
        entry.1 = entry.1.saturating_add(bytes);
    } else {
        pending.push((channel, bytes));
    }
}

fn flush_credits(core: &CoreClient, pending: &mut Vec<(u32, u32)>, accum_bytes: &mut usize) {
    if !pending.is_empty() {
        core.ack_credits(pending);
        pending.clear();
        *accum_bytes = 0;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyOutcome {
    ForwardedToPty,
    UiChanged,
    Quit,
}

#[allow(clippy::too_many_arguments)]
fn run_app(
    terminal: &mut Terminal<CrosstermBackend<BufWriter<io::Stdout>>>,
    core: &CoreClient,
    app_rx: &mpsc::Receiver<AppEvent>,
    world: &mut World,
    usage_tracker: &UsageTracker,
    prefix_spec: &str,
    sidebar_open: &mut bool,
    sidebar_width: u16,
    mobile_threshold: u16,
    touch_mode: bool,
) -> Result<()> {
    let mut attached_map: BTreeMap<String, AttachedTerminal> = BTreeMap::new();
    let mut active_addr: Option<String> = None;
    let res = run_app_loop(
        terminal,
        core,
        app_rx,
        world,
        usage_tracker,
        prefix_spec,
        sidebar_open,
        sidebar_width,
        mobile_threshold,
        touch_mode,
        &mut attached_map,
        &mut active_addr,
    );
    if let Some(prev_att) = active_addr.as_deref().and_then(|a| attached_map.get(a)) {
        core.set_focus(prev_att.channel, false);
    }
    for att in attached_map.values() {
        core.detach_channel(att.channel);
    }
    res
}

#[allow(clippy::too_many_arguments)]
fn run_app_loop(
    terminal: &mut Terminal<CrosstermBackend<BufWriter<io::Stdout>>>,
    core: &CoreClient,
    app_rx: &mpsc::Receiver<AppEvent>,
    world: &mut World,
    usage_tracker: &UsageTracker,
    prefix_spec: &str,
    sidebar_open: &mut bool,
    sidebar_width: u16,
    mobile_threshold: u16,
    touch_mode: bool,
    attached_map: &mut BTreeMap<String, AttachedTerminal>,
    active_addr: &mut Option<String>,
) -> Result<()> {
    let mut first_frame_drawn = false;
    let mut force_clear = true;
    let mut prefix_active = false;
    let mut ctrl_active = false;
    let mut alt_active = false;
    let mut scrollback_offset: usize = 0;
    let mut modal = Modal::None;
    let mut last_hits: Vec<HitRegion> = Vec::new();
    let mut mod_pos: Option<(u16, u16)> = None;
    let mut last_mod_rect: Option<Rect> = None;
    let mut dpad_pos: Option<(u16, u16)> = None;
    let mut last_dpad_rect: Option<Rect> = None;
    let mut dpad_drag: Option<FloatingDPadDrag> = None;
    let mut horiz_wheel = HorizontalWheelGestureState::default();
    let mut selection: Option<TerminalSelection> = None;
    let mut selection_screen: Option<vt100::Screen> = None;
    let mut selection_vt_seq: Option<u64> = None;
    let mut clipboard_buffer = String::new();
    let mut copied_until: Option<Instant> = None;
    let mut pending_sibling_resize_at: Option<Instant> = None;
    let mut pending_attach_retry: Option<(String, Instant)> = None;
    let mut last_usage_rev: u64 = 0;
    let mut has_focused_agent = false;
    let mut dirty = true;
    let mut need_sync_terminals = true;
    let mut more_events = false;
    let mut resynced_this_frame = false;
    let mut last_synced_doc_version: u64 = 0;
    let init_size = terminal.size()?;
    let mut cur_size = (init_size.width, init_size.height);
    let mut prev_term_rect = Rect::default();
    let mut pending_credits: Vec<(u32, u32)> = Vec::with_capacity(8);
    let mut pending_credit_bytes: usize = 0;

    loop {
        if world.should_exit_when_empty() {
            return Ok(());
        }
        let now = Instant::now();
        if copied_until.is_some_and(|t| now >= t) {
            copied_until = None;
            dirty = true;
        }
        if pending_sibling_resize_at.is_some_and(|t| now >= t) {
            pending_sibling_resize_at = None;
            need_sync_terminals = true;
        }
        if pending_attach_retry.as_ref().is_some_and(|(addr, t)| {
            now >= *t || world.active_terminal_address_ref() != Some(addr.as_str())
        }) {
            pending_attach_retry = None;
            need_sync_terminals = true;
        }

        let area_size = cur_size;
        let area = Rect::new(0, 0, area_size.0, area_size.1);
        let term_rect = compute_terminal_rect(area, *sidebar_open, sidebar_width, touch_mode);
        if term_rect != prev_term_rect {
            prev_term_rect = term_rect;
            need_sync_terminals = true;
            dirty = true;
        }

        let attached_has_active = active_addr
            .as_ref()
            .is_some_and(|a| attached_map.contains_key(a));
        let mut newly_attached_active = false;
        if need_sync_terminals
            || world.doc_version != last_synced_doc_version
            || active_needs_sync(
                world.active_terminal_address_ref(),
                active_addr.as_deref(),
                attached_has_active,
                pending_attach_retry.as_ref(),
            )
        {
            let prev_addr = active_addr.clone();
            let allow_sibling_resize = pending_sibling_resize_at.is_none();
            let sync_out = sync_attached_terminals(
                core,
                world,
                attached_map,
                active_addr,
                term_rect,
                &mut scrollback_offset,
                first_frame_drawn,
                allow_sibling_resize,
            );
            last_synced_doc_version = world.doc_version;
            newly_attached_active = sync_out.newly_attached_active;
            need_sync_terminals = sync_out.has_unwarmed_siblings;
            if sync_out.active_attach_failed {
                pending_attach_retry = world
                    .active_terminal_address()
                    .map(|want| (want, Instant::now() + ATTACH_RETRY_DELAY));
            } else {
                pending_attach_retry = None;
            }
            if sync_out.has_unresized_siblings && pending_sibling_resize_at.is_none() {
                pending_sibling_resize_at = Some(Instant::now() + SIBLING_RESIZE_DEBOUNCE);
            }
            if *active_addr != prev_addr {
                selection = None;
                selection_screen = None;
                selection_vt_seq = None;
                dirty = true;
            }
        }

        let mut active_channel = active_addr
            .as_ref()
            .and_then(|a| attached_map.get(a))
            .map(|att| att.channel);

        // When completely idle, block on the unified `AppEvent` channel with zero polling.
        let mut first_event: Option<AppEvent> = None;
        if !dirty && !more_events && !need_sync_terminals && !newly_attached_active {
            let now = Instant::now();
            let mut wait_timeout: Option<Duration> = if has_focused_agent {
                Some(Duration::from_secs(30))
            } else {
                None
            };
            let retry_deadline = pending_attach_retry.as_ref().map(|(_, t)| *t);
            for deadline in [copied_until, pending_sibling_resize_at, retry_deadline]
                .into_iter()
                .flatten()
            {
                let rem = deadline.saturating_duration_since(now);
                wait_timeout = Some(match wait_timeout {
                    Some(cur) => cur.min(rem),
                    None => rem,
                });
            }
            first_event = match wait_timeout {
                Some(dur) => match app_rx.recv_timeout(dur) {
                    Ok(ev) => Some(ev),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let after = Instant::now();
                        if copied_until.is_some_and(|t| after >= t) {
                            copied_until = None;
                            dirty = true;
                        }
                        if pending_sibling_resize_at.is_some_and(|t| after >= t) {
                            pending_sibling_resize_at = None;
                            need_sync_terminals = true;
                            continue;
                        }
                        if pending_attach_retry
                            .as_ref()
                            .is_some_and(|(_, t)| after >= *t)
                        {
                            pending_attach_retry = None;
                            need_sync_terminals = true;
                            continue;
                        }
                        None
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                },
                None => match app_rx.recv() {
                    Ok(ev) => Some(ev),
                    Err(_) => return Ok(()),
                },
            };
        }

        // Drain queued `AppEvent`s BEFORE rendering so initial attach history replays,
        // rapid PTY output bursts, core events, and mouse drag streams coalesce into a single draw.
        let drain_start = Instant::now();
        let mut drained = 0usize;
        more_events = false;
        let drain_budget = if newly_attached_active {
            Duration::from_millis(80)
        } else {
            Duration::from_millis(12)
        };
        let mut pending_doc_sync = false;
        let mut pending_lifecycle_sync = false;
        let mut pending_term_refresh = false;
        let mut check_active_seen = false;

        loop {
            let next_ev = if let Some(ev) = first_event.take() {
                Some(ev)
            } else if drained == 0 && newly_attached_active {
                flush_credits(core, &mut pending_credits, &mut pending_credit_bytes);
                app_rx.recv_timeout(Duration::from_millis(30)).ok()
            } else if let Ok(ev) = app_rx.try_recv() {
                Some(ev)
            } else if newly_attached_active && drain_start.elapsed() < drain_budget {
                flush_credits(core, &mut pending_credits, &mut pending_credit_bytes);
                app_rx.recv_timeout(Duration::from_millis(4)).ok()
            } else {
                None
            };
            let Some(app_ev) = next_ev else {
                break;
            };
            drained += 1;

            match app_ev {
                AppEvent::Core(msg) => match msg {
                    CoreMessage::TerminalData { channel, data } => {
                        record_credit(&mut pending_credits, channel, data.len() as u32);
                        pending_credit_bytes += data.len();
                        if pending_credit_bytes >= CREDIT_FLUSH_THRESHOLD_BYTES {
                            flush_credits(core, &mut pending_credits, &mut pending_credit_bytes);
                        }
                        let is_active = active_channel == Some(channel);
                        if is_active {
                            dirty = true;
                        }
                        if let Some(att) = attached_map.values_mut().find(|a| a.channel == channel)
                        {
                            att.process_vt_bytes(&data);
                            if is_active && selection.as_ref().is_some_and(|s| !s.active) {
                                selection = None;
                                selection_screen = None;
                                selection_vt_seq = None;
                            }
                        }
                    }
                    CoreMessage::TerminalClosed { channel, .. } => {
                        dirty = true;
                        need_sync_terminals = true;
                        attached_map.retain(|_, a| a.channel != channel);
                        if active_channel == Some(channel) {
                            *active_addr = None;
                            active_channel = None;
                        }
                        pending_lifecycle_sync = true;
                    }
                    CoreMessage::Event(ev) => match world.apply_core_event(&ev) {
                        EventDisposition::Unchanged => {}
                        EventDisposition::AppliedInPlace => {
                            dirty = true;
                            check_active_seen = true;
                        }
                        EventDisposition::NeedsTerminalRefresh => {
                            pending_term_refresh = true;
                        }
                        EventDisposition::NeedsLifecycleSync => {
                            pending_lifecycle_sync = true;
                        }
                        EventDisposition::NeedsDocSync => {
                            pending_doc_sync = true;
                        }
                    },
                    CoreMessage::Disconnected => {
                        flush_credits(core, &mut pending_credits, &mut pending_credit_bytes);
                        return Ok(());
                    }
                },
                AppEvent::QuotaUpdated => {
                    dirty = true;
                }
                AppEvent::Input(ev) => {
                    let active_att = active_addr.as_ref().and_then(|a| attached_map.get(a));
                    match ev {
                        Event::Resize(w, h) => {
                            if w < mobile_threshold {
                                *sidebar_open = false;
                            }
                            cur_size = (w, h);
                            pending_sibling_resize_at =
                                Some(Instant::now() + SIBLING_RESIZE_DEBOUNCE);
                            need_sync_terminals = true;
                            force_clear = true;
                            dirty = true;
                        }
                        Event::Paste(text) => {
                            if !text.is_empty() {
                                clipboard_buffer = text.clone();
                            }
                            let was_modal = !matches!(modal, Modal::None);
                            let prev_scroll = scrollback_offset;
                            paste_text_into_app(
                                &text,
                                core,
                                world,
                                active_att,
                                &mut scrollback_offset,
                                &mut modal,
                            );
                            if was_modal || scrollback_offset != prev_scroll {
                                dirty = true;
                            }
                        }
                        Event::Mouse(mev) => match mev.kind {
                            MouseEventKind::Down(MouseButton::Left) => {
                                horiz_wheel.reset_burst();
                                let mx = mev.column;
                                let my = mev.row;
                                let had_vis_sel = selection
                                    .as_ref()
                                    .is_some_and(TerminalSelection::is_non_empty);
                                if is_on_dpad_disc(last_dpad_rect, mx, my) {
                                    selection = None;
                                    selection_screen = None;
                                    selection_vt_seq = None;
                                    if had_vis_sel {
                                        dirty = true;
                                    }
                                    if let Some(disc) = last_dpad_rect {
                                        dpad_drag = Some(FloatingDPadDrag {
                                            is_mod: false,
                                            start_mouse: (mx, my),
                                            start_pos: (disc.x, disc.y),
                                            moved: false,
                                            tap_target: find_hit_target(&last_hits, mx, my),
                                        });
                                    }
                                } else if is_on_dpad_disc(last_mod_rect, mx, my) {
                                    selection = None;
                                    selection_screen = None;
                                    selection_vt_seq = None;
                                    if had_vis_sel {
                                        dirty = true;
                                    }
                                    if let Some(disc) = last_mod_rect {
                                        dpad_drag = Some(FloatingDPadDrag {
                                            is_mod: true,
                                            start_mouse: (mx, my),
                                            start_pos: (disc.x, disc.y),
                                            moved: false,
                                            tap_target: find_hit_target(&last_hits, mx, my),
                                        });
                                    }
                                } else if let Some(target) = find_hit_target(&last_hits, mx, my) {
                                    dpad_drag = None;
                                    selection = None;
                                    selection_screen = None;
                                    selection_vt_seq = None;
                                    if had_vis_sel {
                                        dirty = true;
                                    }
                                    match target {
                                        HitTarget::ModalNoop => {}
                                        HitTarget::CancelModal => {
                                            modal = Modal::None;
                                            prefix_active = false;
                                            ctrl_active = false;
                                            alt_active = false;
                                            dirty = true;
                                        }
                                        HitTarget::ConfirmCloseYes { tab_id } => {
                                            modal = Modal::None;
                                            prefix_active = false;
                                            ctrl_active = false;
                                            alt_active = false;
                                            let _ = world.close_tab_by_id(core, &tab_id);
                                            need_sync_terminals = true;
                                            dirty = true;
                                        }
                                        HitTarget::SubmitPrompt => {
                                            prefix_active = false;
                                            ctrl_active = false;
                                            alt_active = false;
                                            if let Modal::Prompt { kind, input } =
                                                std::mem::replace(&mut modal, Modal::None)
                                            {
                                                match kind {
                                                    PromptKind::RenameTab { tab_id } => {
                                                        let _ = world.rename_tab_by_id(
                                                            core, &tab_id, &input,
                                                        );
                                                    }
                                                    PromptKind::RenameSpace { space_id } => {
                                                        let _ = world.rename_space_by_id(
                                                            core, &space_id, &input,
                                                        );
                                                    }
                                                    PromptKind::NewSpace => {
                                                        let _ =
                                                            world.create_new_space(core, &input);
                                                        need_sync_terminals = true;
                                                    }
                                                }
                                            }
                                            dirty = true;
                                        }
                                        HitTarget::TriggerAction(act) => {
                                            modal = Modal::None;
                                            prefix_active = false;
                                            ctrl_active = false;
                                            alt_active = false;
                                            if execute_action(
                                                act,
                                                core,
                                                world,
                                                active_att,
                                                prefix_spec,
                                                sidebar_open,
                                                &mut scrollback_offset,
                                                &mut modal,
                                            )? {
                                                flush_credits(
                                                    core,
                                                    &mut pending_credits,
                                                    &mut pending_credit_bytes,
                                                );
                                                return Ok(());
                                            }
                                            dirty = true;
                                        }
                                        HitTarget::TogglePrefix => {
                                            modal = Modal::None;
                                            ctrl_active = false;
                                            alt_active = false;
                                            if prefix_active {
                                                prefix_active = false;
                                                if let Some(att) = active_att {
                                                    let bytes = prefix_bytes(prefix_spec);
                                                    core.send_input(att.channel, &bytes);
                                                }
                                            } else {
                                                prefix_active = true;
                                            }
                                            dirty = true;
                                        }
                                        HitTarget::ToggleCtrl => {
                                            modal = Modal::None;
                                            prefix_active = false;
                                            ctrl_active = !ctrl_active;
                                            dirty = true;
                                        }
                                        HitTarget::ToggleAlt => {
                                            modal = Modal::None;
                                            prefix_active = false;
                                            alt_active = !alt_active;
                                            dirty = true;
                                        }
                                        HitTarget::PasteClipboard => {
                                            let had_sticky =
                                                prefix_active || ctrl_active || alt_active;
                                            prefix_active = false;
                                            ctrl_active = false;
                                            alt_active = false;
                                            let was_modal = !matches!(modal, Modal::None);
                                            let prev_scroll = scrollback_offset;
                                            let text = read_clipboard_text(&clipboard_buffer);
                                            if !text.is_empty() {
                                                paste_text_into_app(
                                                    &text,
                                                    core,
                                                    world,
                                                    active_att,
                                                    &mut scrollback_offset,
                                                    &mut modal,
                                                );
                                            }
                                            if had_sticky
                                                || was_modal
                                                || scrollback_offset != prev_scroll
                                            {
                                                dirty = true;
                                            }
                                        }
                                        HitTarget::ExitScrollback => {
                                            modal = Modal::None;
                                            prefix_active = false;
                                            ctrl_active = false;
                                            alt_active = false;
                                            scrollback_offset = 0;
                                            dirty = true;
                                        }
                                        HitTarget::ToggleSidebar | HitTarget::ToggleSpaceMenu => {
                                            prefix_active = false;
                                            ctrl_active = false;
                                            alt_active = false;
                                            modal = if matches!(modal, Modal::SpaceMenu) {
                                                Modal::None
                                            } else {
                                                Modal::SpaceMenu
                                            };
                                            dirty = true;
                                        }
                                        HitTarget::ToggleSpaceCollapse(space_id) => {
                                            modal = Modal::None;
                                            prefix_active = false;
                                            ctrl_active = false;
                                            alt_active = false;
                                            world.toggle_space_collapsed(&space_id);
                                            dirty = true;
                                        }
                                        HitTarget::SelectSpace(space_id) => {
                                            modal = Modal::None;
                                            prefix_active = false;
                                            ctrl_active = false;
                                            alt_active = false;
                                            if world.select_space_by_id(&space_id) {
                                                world.mark_active_seen(core);
                                            }
                                            dirty = true;
                                        }
                                        HitTarget::SelectTab { space_id, tab_id } => {
                                            modal = Modal::None;
                                            prefix_active = false;
                                            ctrl_active = false;
                                            alt_active = false;
                                            if world.select_tab_by_id(&space_id, &tab_id) {
                                                world.mark_active_seen(core);
                                            }
                                            dirty = true;
                                        }
                                        HitTarget::NewTab => {
                                            modal = Modal::None;
                                            prefix_active = false;
                                            ctrl_active = false;
                                            alt_active = false;
                                            let _ = world.create_new_tab(core);
                                            need_sync_terminals = true;
                                            dirty = true;
                                        }
                                        HitTarget::NewSpacePrompt => {
                                            prefix_active = false;
                                            ctrl_active = false;
                                            alt_active = false;
                                            modal = Modal::Prompt {
                                                kind: PromptKind::NewSpace,
                                                input: String::new(),
                                            };
                                            dirty = true;
                                        }
                                        HitTarget::ToggleHelp => {
                                            prefix_active = false;
                                            ctrl_active = false;
                                            alt_active = false;
                                            modal = if matches!(modal, Modal::Help) {
                                                Modal::None
                                            } else {
                                                Modal::Help
                                            };
                                            dirty = true;
                                        }
                                        HitTarget::SendKey(kev) => match handle_key_event(
                                            kev,
                                            core,
                                            world,
                                            active_att,
                                            prefix_spec,
                                            sidebar_open,
                                            &mut prefix_active,
                                            &mut ctrl_active,
                                            &mut alt_active,
                                            &mut scrollback_offset,
                                            &mut modal,
                                            term_rect,
                                        )? {
                                            KeyOutcome::Quit => {
                                                flush_credits(
                                                    core,
                                                    &mut pending_credits,
                                                    &mut pending_credit_bytes,
                                                );
                                                return Ok(());
                                            }
                                            KeyOutcome::UiChanged => dirty = true,
                                            KeyOutcome::ForwardedToPty => {}
                                        },
                                    }
                                } else if !matches!(modal, Modal::None) {
                                    dpad_drag = None;
                                    modal = Modal::None;
                                    prefix_active = false;
                                    ctrl_active = false;
                                    alt_active = false;
                                    dirty = true;
                                } else if mx >= term_rect.x
                                    && mx < term_rect.x.saturating_add(term_rect.width)
                                    && my >= term_rect.y
                                    && my < term_rect.y.saturating_add(term_rect.height)
                                {
                                    dpad_drag = None;
                                    let rel_col = mx.saturating_sub(term_rect.x);
                                    let rel_row = my.saturating_sub(term_rect.y);
                                    selection = Some(TerminalSelection::new(rel_row, rel_col));
                                    selection_screen = None;
                                    selection_vt_seq = None;
                                    if had_vis_sel {
                                        dirty = true;
                                    }
                                } else {
                                    dpad_drag = None;
                                    selection = None;
                                    selection_screen = None;
                                    selection_vt_seq = None;
                                    if had_vis_sel {
                                        dirty = true;
                                    }
                                }
                            }
                            MouseEventKind::Drag(MouseButton::Left) => {
                                if let Some(ref mut drag) = dpad_drag {
                                    let dx = mev.column as i32 - drag.start_mouse.0 as i32;
                                    let dy = mev.row as i32 - drag.start_mouse.1 as i32;
                                    if drag.moved || dx.abs() >= 2 || dy.abs() >= 1 {
                                        let was_moved = drag.moved;
                                        drag.moved = true;
                                        let nx = (drag.start_pos.0 as i32 + dx).max(0) as u16;
                                        let ny = (drag.start_pos.1 as i32 + dy).max(0) as u16;
                                        if drag.is_mod {
                                            if let Some(clamped) =
                                                resolve_mod_cross_rect(term_rect, Some((nx, ny)))
                                            {
                                                let next_pos = Some((clamped.x, clamped.y));
                                                if !was_moved || mod_pos != next_pos {
                                                    mod_pos = next_pos;
                                                    dirty = true;
                                                }
                                            }
                                        } else if let Some(clamped) =
                                            resolve_dpad_disc_rect(term_rect, Some((nx, ny)))
                                        {
                                            let next_pos = Some((clamped.x, clamped.y));
                                            if !was_moved || dpad_pos != next_pos {
                                                dpad_pos = next_pos;
                                                dirty = true;
                                            }
                                        }
                                    }
                                } else if let Some(ref mut sel) = selection {
                                    if sel.active && term_rect.width > 0 && term_rect.height > 0 {
                                        let rel_col = mev
                                            .column
                                            .saturating_sub(term_rect.x)
                                            .min(term_rect.width.saturating_sub(1));
                                        let rel_row = mev
                                            .row
                                            .saturating_sub(term_rect.y)
                                            .min(term_rect.height.saturating_sub(1));
                                        let new_head = (rel_row, rel_col);
                                        if sel.head != new_head {
                                            sel.head = new_head;
                                            if sel.is_non_empty() && selection_screen.is_none() {
                                                selection_screen =
                                                    active_att.map(|a| a.parser.screen().clone());
                                                selection_vt_seq = active_att.map(|a| a.vt_seq);
                                            }
                                            dirty = true;
                                        }
                                    }
                                }
                            }
                            MouseEventKind::Up(MouseButton::Left) => {
                                if let Some(drag) = dpad_drag.take() {
                                    if drag.moved {
                                        dirty = true;
                                    } else if matches!(modal, Modal::None) {
                                        match drag.tap_target {
                                            Some(HitTarget::TogglePrefix) => {
                                                modal = Modal::None;
                                                ctrl_active = false;
                                                alt_active = false;
                                                if prefix_active {
                                                    prefix_active = false;
                                                    if let Some(att) = active_att {
                                                        let bytes = prefix_bytes(prefix_spec);
                                                        core.send_input(att.channel, &bytes);
                                                    }
                                                } else {
                                                    prefix_active = true;
                                                }
                                                dirty = true;
                                            }
                                            Some(HitTarget::ToggleCtrl) => {
                                                modal = Modal::None;
                                                prefix_active = false;
                                                ctrl_active = !ctrl_active;
                                                dirty = true;
                                            }
                                            Some(HitTarget::ToggleAlt) => {
                                                modal = Modal::None;
                                                prefix_active = false;
                                                alt_active = !alt_active;
                                                dirty = true;
                                            }
                                            Some(HitTarget::PasteClipboard) => {
                                                let had_sticky =
                                                    prefix_active || ctrl_active || alt_active;
                                                prefix_active = false;
                                                ctrl_active = false;
                                                alt_active = false;
                                                let was_modal = !matches!(modal, Modal::None);
                                                let prev_scroll = scrollback_offset;
                                                let text = read_clipboard_text(&clipboard_buffer);
                                                if !text.is_empty() {
                                                    paste_text_into_app(
                                                        &text,
                                                        core,
                                                        world,
                                                        active_att,
                                                        &mut scrollback_offset,
                                                        &mut modal,
                                                    );
                                                }
                                                if had_sticky
                                                    || was_modal
                                                    || scrollback_offset != prev_scroll
                                                {
                                                    dirty = true;
                                                }
                                            }
                                            Some(HitTarget::SendKey(kev)) => {
                                                match handle_key_event(
                                                    kev,
                                                    core,
                                                    world,
                                                    active_att,
                                                    prefix_spec,
                                                    sidebar_open,
                                                    &mut prefix_active,
                                                    &mut ctrl_active,
                                                    &mut alt_active,
                                                    &mut scrollback_offset,
                                                    &mut modal,
                                                    term_rect,
                                                )? {
                                                    KeyOutcome::Quit => {
                                                        flush_credits(
                                                            core,
                                                            &mut pending_credits,
                                                            &mut pending_credit_bytes,
                                                        );
                                                        return Ok(());
                                                    }
                                                    KeyOutcome::UiChanged => dirty = true,
                                                    KeyOutcome::ForwardedToPty => {}
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                } else if let Some(ref mut sel) = selection {
                                    if sel.active {
                                        if term_rect.width > 0 && term_rect.height > 0 {
                                            let rel_col = mev
                                                .column
                                                .saturating_sub(term_rect.x)
                                                .min(term_rect.width.saturating_sub(1));
                                            let rel_row = mev
                                                .row
                                                .saturating_sub(term_rect.y)
                                                .min(term_rect.height.saturating_sub(1));
                                            sel.head = (rel_row, rel_col);
                                        }
                                        sel.active = false;
                                        let snap = selection_screen.take();
                                        let snap_seq = selection_vt_seq.take();
                                        if sel.is_non_empty() {
                                            let scr = snap
                                                .as_ref()
                                                .or_else(|| active_att.map(|a| a.parser.screen()));
                                            if let Some(screen) = scr {
                                                let text = sel.extract_text(screen);
                                                if !text.is_empty() {
                                                    clipboard_buffer = text.clone();
                                                    copy_to_clipboard(&text);
                                                    copied_until = Some(
                                                        Instant::now() + Duration::from_secs(2),
                                                    );
                                                }
                                            }
                                            if snap.is_some()
                                                && snap_seq != active_att.map(|a| a.vt_seq)
                                            {
                                                selection = None;
                                            }
                                            dirty = true;
                                        } else {
                                            selection = None;
                                        }
                                    }
                                }
                            }
                            MouseEventKind::ScrollUp => {
                                let wheel_now = Instant::now();
                                if matches!(modal, Modal::SpaceMenu) {
                                    horiz_wheel.record_top_bar_vertical_wheel(wheel_now);
                                    world.prev_space();
                                    world.mark_active_seen(core);
                                    dirty = true;
                                } else if let Modal::Fuzzy {
                                    ref query,
                                    ref mut selected,
                                    ref mut selected_tab_id,
                                } = modal
                                {
                                    horiz_wheel.record_viewport_vertical_wheel(wheel_now);
                                    let items = collect_fuzzy_items(world, query);
                                    let cur_sel = resolve_fuzzy_selection(
                                        &items,
                                        *selected,
                                        selected_tab_id.as_deref(),
                                    )
                                    .unwrap_or(0);
                                    *selected = cur_sel.saturating_sub(1);
                                    *selected_tab_id =
                                        items.get(*selected).map(|it| it.tab_id.clone());
                                    dirty = true;
                                } else if matches!(modal, Modal::None)
                                    && mev.row < term_rect.y
                                    && dpad_drag.is_none()
                                    && !selection.as_ref().is_some_and(|s| s.active)
                                {
                                    horiz_wheel.record_top_bar_vertical_wheel(wheel_now);
                                    let had_sticky = prefix_active || ctrl_active || alt_active;
                                    prefix_active = false;
                                    ctrl_active = false;
                                    alt_active = false;
                                    let had_vis_sel = selection
                                        .as_ref()
                                        .is_some_and(TerminalSelection::is_non_empty);
                                    selection = None;
                                    selection_screen = None;
                                    selection_vt_seq = None;
                                    let prev_ti = world.active_tab_index();
                                    world.prev_tab();
                                    let tab_changed = world.active_tab_index() != prev_ti;
                                    if tab_changed {
                                        world.mark_active_seen(core);
                                    }
                                    if tab_changed || had_sticky || had_vis_sel {
                                        dirty = true;
                                    }
                                } else if matches!(modal, Modal::None)
                                    && mev.row >= term_rect.y
                                    && mev.row < term_rect.y.saturating_add(term_rect.height)
                                    && !is_on_dpad_disc(last_dpad_rect, mev.column, mev.row)
                                    && !is_on_dpad_disc(last_mod_rect, mev.column, mev.row)
                                {
                                    horiz_wheel.record_viewport_vertical_wheel(wheel_now);
                                    if selection
                                        .as_ref()
                                        .is_some_and(TerminalSelection::is_non_empty)
                                    {
                                        dirty = true;
                                    }
                                    selection = None;
                                    selection_screen = None;
                                    selection_vt_seq = None;
                                    let rel_col =
                                        mev.column.saturating_sub(term_rect.x).saturating_add(1);
                                    let rel_row =
                                        mev.row.saturating_sub(term_rect.y).saturating_add(1);
                                    if let Some((ch, bytes)) = active_att.and_then(|att| {
                                        encode_wheel_for_terminal(
                                            att.parser.screen(),
                                            true,
                                            rel_col,
                                            rel_row,
                                        )
                                        .map(|b| (att.channel, b))
                                    }) {
                                        core.send_input(ch, &bytes);
                                    } else {
                                        let prev_scroll = scrollback_offset;
                                        scrollback_offset = scrollback_offset.saturating_add(3);
                                        if scrollback_offset != prev_scroll {
                                            dirty = true;
                                        }
                                    }
                                }
                            }
                            MouseEventKind::ScrollDown => {
                                let wheel_now = Instant::now();
                                if matches!(modal, Modal::SpaceMenu) {
                                    horiz_wheel.record_top_bar_vertical_wheel(wheel_now);
                                    world.next_space();
                                    world.mark_active_seen(core);
                                    dirty = true;
                                } else if let Modal::Fuzzy {
                                    ref query,
                                    ref mut selected,
                                    ref mut selected_tab_id,
                                } = modal
                                {
                                    horiz_wheel.record_viewport_vertical_wheel(wheel_now);
                                    let items = collect_fuzzy_items(world, query);
                                    if !items.is_empty() {
                                        let cur_sel = resolve_fuzzy_selection(
                                            &items,
                                            *selected,
                                            selected_tab_id.as_deref(),
                                        )
                                        .unwrap_or(0);
                                        *selected = (cur_sel + 1).min(items.len() - 1);
                                        *selected_tab_id =
                                            items.get(*selected).map(|it| it.tab_id.clone());
                                        dirty = true;
                                    }
                                } else if matches!(modal, Modal::None)
                                    && mev.row < term_rect.y
                                    && dpad_drag.is_none()
                                    && !selection.as_ref().is_some_and(|s| s.active)
                                {
                                    horiz_wheel.record_top_bar_vertical_wheel(wheel_now);
                                    let had_sticky = prefix_active || ctrl_active || alt_active;
                                    prefix_active = false;
                                    ctrl_active = false;
                                    alt_active = false;
                                    let had_vis_sel = selection
                                        .as_ref()
                                        .is_some_and(TerminalSelection::is_non_empty);
                                    selection = None;
                                    selection_screen = None;
                                    selection_vt_seq = None;
                                    let prev_ti = world.active_tab_index();
                                    world.next_tab();
                                    let tab_changed = world.active_tab_index() != prev_ti;
                                    if tab_changed {
                                        world.mark_active_seen(core);
                                    }
                                    if tab_changed || had_sticky || had_vis_sel {
                                        dirty = true;
                                    }
                                } else if matches!(modal, Modal::None)
                                    && mev.row >= term_rect.y
                                    && mev.row < term_rect.y.saturating_add(term_rect.height)
                                    && !is_on_dpad_disc(last_dpad_rect, mev.column, mev.row)
                                    && !is_on_dpad_disc(last_mod_rect, mev.column, mev.row)
                                {
                                    horiz_wheel.record_viewport_vertical_wheel(wheel_now);
                                    if selection
                                        .as_ref()
                                        .is_some_and(TerminalSelection::is_non_empty)
                                    {
                                        dirty = true;
                                    }
                                    selection = None;
                                    selection_screen = None;
                                    selection_vt_seq = None;
                                    let rel_col =
                                        mev.column.saturating_sub(term_rect.x).saturating_add(1);
                                    let rel_row =
                                        mev.row.saturating_sub(term_rect.y).saturating_add(1);
                                    if let Some((ch, bytes)) = active_att.and_then(|att| {
                                        encode_wheel_for_terminal(
                                            att.parser.screen(),
                                            false,
                                            rel_col,
                                            rel_row,
                                        )
                                        .map(|b| (att.channel, b))
                                    }) {
                                        core.send_input(ch, &bytes);
                                    } else {
                                        let prev_scroll = scrollback_offset;
                                        scrollback_offset = scrollback_offset.saturating_sub(3);
                                        if scrollback_offset != prev_scroll {
                                            dirty = true;
                                        }
                                    }
                                }
                            }
                            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
                                let dir = if matches!(mev.kind, MouseEventKind::ScrollLeft) {
                                    HorizontalSwipeDir::Prev
                                } else {
                                    HorizontalSwipeDir::Next
                                };
                                if handle_horizontal_wheel_event(
                                    dir,
                                    mev.row,
                                    term_rect,
                                    Instant::now(),
                                    &mut horiz_wheel,
                                    core,
                                    world,
                                    &modal,
                                    dpad_drag.is_some(),
                                    &mut prefix_active,
                                    &mut ctrl_active,
                                    &mut alt_active,
                                    &mut selection,
                                    &mut selection_screen,
                                    &mut selection_vt_seq,
                                ) {
                                    dirty = true;
                                }
                            }
                            _ => {}
                        },
                        Event::Key(kev) if kev.kind == KeyEventKind::Press => {
                            horiz_wheel.reset_burst();
                            let had_drag_or_sel = dpad_drag.as_ref().is_some_and(|d| d.moved)
                                || selection
                                    .as_ref()
                                    .is_some_and(TerminalSelection::is_non_empty);
                            dpad_drag = None;
                            selection = None;
                            selection_screen = None;
                            selection_vt_seq = None;
                            if had_drag_or_sel {
                                dirty = true;
                            }
                            match handle_key_event(
                                kev,
                                core,
                                world,
                                active_att,
                                prefix_spec,
                                sidebar_open,
                                &mut prefix_active,
                                &mut ctrl_active,
                                &mut alt_active,
                                &mut scrollback_offset,
                                &mut modal,
                                term_rect,
                            )? {
                                KeyOutcome::Quit => {
                                    flush_credits(
                                        core,
                                        &mut pending_credits,
                                        &mut pending_credit_bytes,
                                    );
                                    return Ok(());
                                }
                                KeyOutcome::UiChanged => {
                                    dirty = true;
                                }
                                KeyOutcome::ForwardedToPty => {}
                            }
                        }
                        _ => {}
                    }

                    // Break batch early if the active terminal switched so `sync_attached_terminals`
                    // updates `active_addr` before any subsequent input events in the queue.
                    let has_active_att = active_addr
                        .as_ref()
                        .is_some_and(|a| attached_map.contains_key(a));
                    if active_needs_sync(
                        world.active_terminal_address_ref(),
                        active_addr.as_deref(),
                        has_active_att,
                        pending_attach_retry.as_ref(),
                    ) {
                        pending_attach_retry = None;
                        need_sync_terminals = true;
                        more_events = true;
                        break;
                    }
                }
            }

            if drained >= MAX_EVENTS_PER_TICK || drain_start.elapsed() >= drain_budget {
                more_events = true;
                break;
            }
        }

        flush_credits(core, &mut pending_credits, &mut pending_credit_bytes);

        // Execute at most ONE coalesced RPC sync for the entire drained event batch.
        if pending_doc_sync {
            if world.fetch_doc(core).is_ok() && world.refresh_terminals(core).is_ok() {
                let _ = world.reconcile_and_sync(core);
            }
            world.mark_active_seen(core);
            need_sync_terminals = true;
            dirty = true;
        } else if pending_lifecycle_sync {
            if world.refresh_terminals(core).is_ok() {
                let _ = world.reconcile_and_sync(core);
            }
            world.mark_active_seen(core);
            need_sync_terminals = true;
            dirty = true;
        } else if pending_term_refresh {
            let _ = world.refresh_terminals(core);
            world.mark_active_seen(core);
            dirty = true;
        } else if check_active_seen {
            world.mark_active_seen(core);
        }

        if world.should_exit_when_empty() {
            return Ok(());
        }

        // If the active terminal or viewport dimensions changed while draining events,
        // resync immediately before drawing so the tab header and terminal screen never mismatch.
        let has_active_att = active_addr
            .as_ref()
            .is_some_and(|a| attached_map.contains_key(a));
        if !resynced_this_frame
            && (cur_size != area_size
                || active_needs_sync(
                    world.active_terminal_address_ref(),
                    active_addr.as_deref(),
                    has_active_att,
                    pending_attach_retry.as_ref(),
                ))
        {
            pending_attach_retry = None;
            resynced_this_frame = true;
            need_sync_terminals = true;
            continue;
        }
        resynced_this_frame = false;

        if let Modal::ConfirmCloseTab { ref tab_id, .. } = modal {
            if !world.has_tab(tab_id) {
                modal = Modal::None;
                dirty = true;
            }
        } else if let Modal::Fuzzy {
            ref mut selected_tab_id,
            ..
        } = modal
        {
            if selected_tab_id
                .as_deref()
                .is_some_and(|tid| !world.has_tab(tid))
            {
                *selected_tab_id = None;
                dirty = true;
            }
        }

        if let Some(att) = active_addr.as_ref().and_then(|a| attached_map.get_mut(a)) {
            att.parser.screen_mut().set_scrollback(scrollback_offset);
            scrollback_offset = att.parser.screen().scrollback();
        }

        let focused_quota = usage_tracker.focused_quota(world);
        has_focused_agent = focused_quota.is_some();
        let cur_usage_rev = usage_tracker.revision();
        if cur_usage_rev != last_usage_rev {
            last_usage_rev = cur_usage_rev;
            dirty = true;
        }

        if dirty {
            let active_att = active_addr.as_ref().and_then(|a| attached_map.get(a));
            let scr_ref = if selection.as_ref().is_some_and(|s| s.active) {
                selection_screen
                    .as_ref()
                    .or_else(|| active_att.map(|a| a.parser.screen()))
            } else {
                active_att.map(|a| a.parser.screen())
            };
            let copied_flash = copied_until.is_some();
            let mod_dragging = dpad_drag.as_ref().is_some_and(|d| d.is_mod && d.moved);
            let dpad_dragging = dpad_drag.as_ref().is_some_and(|d| !d.is_mod && d.moved);
            queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
            if force_clear {
                terminal.clear()?;
                force_clear = false;
            }
            terminal.draw(|f| {
                let geom = render_ui_with_pads_and_quota(
                    f,
                    world,
                    scr_ref,
                    *sidebar_open,
                    sidebar_width,
                    touch_mode,
                    prefix_active,
                    ctrl_active,
                    alt_active,
                    scrollback_offset,
                    selection.as_ref(),
                    copied_flash,
                    &modal,
                    mod_pos,
                    mod_dragging,
                    dpad_pos,
                    dpad_dragging,
                    focused_quota.as_ref(),
                );
                last_hits = geom.hits;
                last_mod_rect = geom.mod_rect;
                last_dpad_rect = geom.dpad_rect;
            })?;
            execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
            dirty = false;
            if !first_frame_drawn {
                first_frame_drawn = true;
                need_sync_terminals = true;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_key_event(
    mut kev: KeyEvent,
    core: &CoreClient,
    world: &mut World,
    attached: Option<&AttachedTerminal>,
    prefix_spec: &str,
    sidebar_open: &mut bool,
    prefix_active: &mut bool,
    ctrl_active: &mut bool,
    alt_active: &mut bool,
    scrollback_offset: &mut usize,
    modal: &mut Modal,
    term_rect: Rect,
) -> Result<KeyOutcome> {
    let mut ui_changed = false;
    if *ctrl_active {
        kev.modifiers.insert(KeyModifiers::CONTROL);
        *ctrl_active = false;
        ui_changed = true;
    }
    if *alt_active {
        kev.modifiers.insert(KeyModifiers::ALT);
        *alt_active = false;
        ui_changed = true;
    }

    // 1. Handle text-input and confirmation modals first so typed characters are never intercepted
    match modal {
        Modal::ConfirmCloseTab { tab_id, .. } => {
            *prefix_active = false;
            let target_id = tab_id.clone();
            match kev.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                    *modal = Modal::None;
                    let _ = world.close_tab_by_id(core, &target_id);
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    *modal = Modal::None;
                }
                _ => {}
            }
            return Ok(KeyOutcome::UiChanged);
        }
        Modal::Prompt { kind, input } => {
            *prefix_active = false;
            let k = kind.clone();
            match kev.code {
                KeyCode::Esc => {
                    *modal = Modal::None;
                }
                KeyCode::Enter => {
                    let text = input.clone();
                    *modal = Modal::None;
                    match k {
                        PromptKind::RenameTab { tab_id } => {
                            let _ = world.rename_tab_by_id(core, &tab_id, &text);
                        }
                        PromptKind::RenameSpace { space_id } => {
                            let _ = world.rename_space_by_id(core, &space_id, &text);
                        }
                        PromptKind::NewSpace => {
                            let _ = world.create_new_space(core, &text);
                        }
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c)
                    if !kev
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    input.push(c);
                }
                _ => {}
            }
            return Ok(KeyOutcome::UiChanged);
        }
        Modal::Fuzzy {
            query,
            selected,
            selected_tab_id,
        } => {
            *prefix_active = false;
            let items = collect_fuzzy_items(world, query);
            let cur_sel =
                resolve_fuzzy_selection(&items, *selected, selected_tab_id.as_deref()).unwrap_or(0);
            match kev.code {
                KeyCode::Esc => {
                    *modal = Modal::None;
                }
                KeyCode::Up => {
                    *selected = cur_sel.saturating_sub(1);
                    *selected_tab_id = items.get(*selected).map(|it| it.tab_id.clone());
                }
                KeyCode::Down => {
                    if !items.is_empty() {
                        *selected = (cur_sel + 1).min(items.len() - 1);
                        *selected_tab_id = items.get(*selected).map(|it| it.tab_id.clone());
                    }
                }
                KeyCode::Char('p') if kev.modifiers.contains(KeyModifiers::CONTROL) => {
                    *selected = cur_sel.saturating_sub(1);
                    *selected_tab_id = items.get(*selected).map(|it| it.tab_id.clone());
                }
                KeyCode::Char('n') if kev.modifiers.contains(KeyModifiers::CONTROL) => {
                    if !items.is_empty() {
                        *selected = (cur_sel + 1).min(items.len() - 1);
                        *selected_tab_id = items.get(*selected).map(|it| it.tab_id.clone());
                    }
                }
                KeyCode::Enter => {
                    if let Some(ref tid) = *selected_tab_id {
                        if let Some(chosen) = items.iter().find(|it| &it.tab_id == tid) {
                            if world.select_tab_by_id(&chosen.space_id, &chosen.tab_id) {
                                world.mark_active_seen(core);
                            }
                        }
                    }
                    *modal = Modal::None;
                }
                KeyCode::Backspace => {
                    query.pop();
                    *selected = 0;
                    let next_items = collect_fuzzy_items(world, query);
                    *selected_tab_id = next_items.first().map(|it| it.tab_id.clone());
                }
                KeyCode::Char(c)
                    if !kev
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    query.push(c);
                    *selected = 0;
                    let next_items = collect_fuzzy_items(world, query);
                    *selected_tab_id = next_items.first().map(|it| it.tab_id.clone());
                }
                _ => {}
            }
            return Ok(KeyOutcome::UiChanged);
        }
        Modal::SpaceMenu | Modal::Help | Modal::None => {}
    }

    // 2. Prefix mode active (`Ctrl+b` pressed previously or `[^B]` tapped)
    // Checked before `SpaceMenu`/`Help` so `^b b` and `^b ?` toggle those popovers closed cleanly.
    if *prefix_active {
        *prefix_active = false;
        if let Some(action) = match_prefix_action(&kev, prefix_spec) {
            if matches!(modal, Modal::SpaceMenu | Modal::Help) {
                if action == Action::SendPrefix {
                    *modal = Modal::None;
                    return Ok(KeyOutcome::UiChanged);
                }
                if !matches!(action, Action::ToggleSidebar | Action::ToggleHelp) {
                    *modal = Modal::None;
                }
            }
            if execute_action(
                action,
                core,
                world,
                attached,
                prefix_spec,
                sidebar_open,
                scrollback_offset,
                modal,
            )? {
                return Ok(KeyOutcome::Quit);
            }
        } else if matches!(modal, Modal::SpaceMenu | Modal::Help) && kev.code == KeyCode::Esc {
            *modal = Modal::None;
        }
        return Ok(KeyOutcome::UiChanged);
    }

    // 3. Check if prefix key was pressed (`Ctrl+b`)
    if is_prefix_key(&kev, prefix_spec) {
        *prefix_active = true;
        return Ok(KeyOutcome::UiChanged);
    }

    // 4. Handle non-prefix keys inside `SpaceMenu` and `Help` popovers
    match modal {
        Modal::SpaceMenu => {
            if match_direct_action(&kev) == Some(Action::Detach) {
                *modal = Modal::None;
                return Ok(KeyOutcome::Quit);
            }
            let no_ctrl_alt = !kev
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
            match kev.code {
                KeyCode::Esc | KeyCode::Enter => {
                    *modal = Modal::None;
                }
                KeyCode::Char('q' | 'b') if no_ctrl_alt => {
                    *modal = Modal::None;
                }
                KeyCode::Char('c') if kev.modifiers.contains(KeyModifiers::CONTROL) => {
                    *modal = Modal::None;
                }
                KeyCode::Up => {
                    world.prev_space();
                    world.mark_active_seen(core);
                }
                KeyCode::Char('k') if no_ctrl_alt => {
                    world.prev_space();
                    world.mark_active_seen(core);
                }
                KeyCode::Down => {
                    world.next_space();
                    world.mark_active_seen(core);
                }
                KeyCode::Char('j') if no_ctrl_alt => {
                    world.next_space();
                    world.mark_active_seen(core);
                }
                KeyCode::Char('c' | 'C' | '+') if no_ctrl_alt => {
                    *modal = Modal::Prompt {
                        kind: PromptKind::NewSpace,
                        input: String::new(),
                    };
                }
                _ => {}
            }
            return Ok(KeyOutcome::UiChanged);
        }
        Modal::Help => {
            if match_direct_action(&kev) == Some(Action::Detach) {
                *modal = Modal::None;
                return Ok(KeyOutcome::Quit);
            }
            *modal = Modal::None;
            return Ok(KeyOutcome::UiChanged);
        }
        _ => {}
    }

    // 5. Direct global shortcuts (`Alt+Left/Right/Up/Down/1..9`, `Ctrl+\`)
    if let Some(action) = match_direct_action(&kev) {
        if execute_action(
            action,
            core,
            world,
            attached,
            prefix_spec,
            sidebar_open,
            scrollback_offset,
            modal,
        )? {
            return Ok(KeyOutcome::Quit);
        }
        return Ok(KeyOutcome::UiChanged);
    }

    // 5. Scrollback mode navigation (or Shift+PageUp to enter scrollback from main screen)
    if *scrollback_offset == 0
        && kev.code == KeyCode::PageUp
        && kev.modifiers.contains(KeyModifiers::SHIFT)
        && attached.is_none_or(|a| !a.parser.screen().alternate_screen())
    {
        *scrollback_offset = (term_rect.height as usize / 2).max(1);
        return Ok(KeyOutcome::UiChanged);
    }
    if *scrollback_offset > 0 {
        match kev.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                *scrollback_offset = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                *scrollback_offset = scrollback_offset.saturating_add(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *scrollback_offset = scrollback_offset.saturating_sub(1);
            }
            KeyCode::PageUp | KeyCode::Char('u') => {
                *scrollback_offset =
                    scrollback_offset.saturating_add((term_rect.height as usize / 2).max(1));
            }
            KeyCode::PageDown | KeyCode::Char('d') => {
                *scrollback_offset =
                    scrollback_offset.saturating_sub((term_rect.height as usize / 2).max(1));
            }
            KeyCode::Char('g') => {
                *scrollback_offset = SCROLLBACK_LINES;
            }
            KeyCode::Char('G') => {
                *scrollback_offset = 0;
            }
            _ => {}
        }
        return Ok(KeyOutcome::UiChanged);
    }

    // 6. Forward regular keyboard input to the active terminal
    if let Some(att) = attached {
        let app_cursor = att.parser.screen().application_cursor();
        let bytes = encode_key_event(&kev, app_cursor);
        if !bytes.is_empty() {
            core.send_input(att.channel, &bytes);
        }
    }
    Ok(if ui_changed {
        KeyOutcome::UiChanged
    } else {
        KeyOutcome::ForwardedToPty
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encodes_osc52_payloads_accurately() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"hello world"), "aGVsbG8gd29ybGQ=");
    }

    #[test]
    fn sanitize_vt_stream_aligns_vs16_emoji_across_arbitrary_frame_boundaries() {
        let frame = "\x1b[H╭──────╮\r\n│ ⚙️ a │\r\n╰──────╯\x1b[J".as_bytes();

        // 1. Single-frame delivery
        let mut att_whole = AttachedTerminal::new(1, 20, 4);
        att_whole.process_vt_bytes(frame);
        let screen_whole = att_whole.parser.screen();
        assert_eq!(screen_whole.cell(0, 7).unwrap().contents(), "╮");
        assert_eq!(screen_whole.cell(1, 7).unwrap().contents(), "│");
        assert_eq!(screen_whole.cell(2, 7).unwrap().contents(), "╯");

        // 2. Split specifically between '⚙' (\xe2\x9a\x99) and U+FE0F (\xef\xb8\x8f),
        // and also test 1-byte-at-a-time streaming across partial UTF-8 sequences.
        let mut att_bytewise = AttachedTerminal::new(2, 20, 4);
        for b in frame {
            att_bytewise.process_vt_bytes(std::slice::from_ref(b));
        }
        let screen_bytewise = att_bytewise.parser.screen();
        assert_eq!(screen_bytewise.cell(0, 7).unwrap().contents(), "╮");
        assert_eq!(screen_bytewise.cell(1, 7).unwrap().contents(), "│");
        assert_eq!(screen_bytewise.cell(2, 7).unwrap().contents(), "╯");
    }

    #[test]
    fn wheel_scrolls_main_screen_scrollback_and_forwards_in_alternate_screen() {
        assert_eq!(ATTACH_HISTORY_LINES, SCROLLBACK_LINES);

        let mut att = AttachedTerminal::new(1, 20, 4);
        for i in 1..=10 {
            att.process_vt_bytes(format!("line {i}\r\n").as_bytes());
        }
        // Main screen returns None so hct adjusts scrollback_offset naturally
        assert_eq!(
            encode_wheel_for_terminal(att.parser.screen(), true, 5, 2),
            None
        );
        att.parser.screen_mut().set_scrollback(3);
        assert_eq!(att.parser.screen().scrollback(), 3);

        // Alternate screen without mouse capture sends Up/Down arrow keys
        att.process_vt_bytes(b"\x1b[?1049h");
        assert_eq!(
            encode_wheel_for_terminal(att.parser.screen(), true, 5, 2).as_deref(),
            Some(&b"\x1b[A\x1b[A\x1b[A"[..])
        );
        assert_eq!(
            encode_wheel_for_terminal(att.parser.screen(), false, 5, 2).as_deref(),
            Some(&b"\x1b[B\x1b[B\x1b[B"[..])
        );

        // Alternate screen with SGR mouse protocol sends SGR wheel events
        att.process_vt_bytes(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            encode_wheel_for_terminal(att.parser.screen(), true, 10, 3).as_deref(),
            Some(&b"\x1b[<64;10;3M"[..])
        );
        assert_eq!(
            encode_wheel_for_terminal(att.parser.screen(), false, 10, 3).as_deref(),
            Some(&b"\x1b[<65;10;3M"[..])
        );
    }

    #[test]
    fn sanitize_vt_stream_translates_btop_hvp_and_ecma48_cursor_sequences_across_frames() {
        // btop uses `\x1b[<row>;<col>f` (HVP) instead of `\x1b[<row>;<col>H` (CUP)
        let stream = "\x1b[2J\x1b[1;1f╭──────╮\x1b[2;1f│      │\x1b[3;1f╰──────╯\x1b[2;3fCPU\x1b[s\x1b[3;8fX\x1b[u!".as_bytes();

        let mut att_whole = AttachedTerminal::new(1, 20, 5);
        att_whole.process_vt_bytes(stream);
        let scr = att_whole.parser.screen();
        assert_eq!(scr.contents_between(0, 0, 0, 8), "╭──────╮");
        assert_eq!(scr.contents_between(1, 0, 1, 8), "│ CPU! │");
        assert_eq!(scr.contents_between(2, 0, 2, 8), "╰──────X");

        // Also verify 1-byte-at-a-time streaming across split CSI sequences
        let mut att_split = AttachedTerminal::new(2, 20, 5);
        for b in stream {
            att_split.process_vt_bytes(std::slice::from_ref(b));
        }
        let scr_split = att_split.parser.screen();
        assert_eq!(scr_split.contents_between(0, 0, 0, 8), "╭──────╮");
        assert_eq!(scr_split.contents_between(1, 0, 1, 8), "│ CPU! │");
        assert_eq!(scr_split.contents_between(2, 0, 2, 8), "╰──────X");
    }

    #[test]
    fn input_filter_drops_mouse_moved_and_key_release_events() {
        use crossterm::event::MouseEvent;

        let moved = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 4,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!should_forward_input_event(&moved));

        let drag = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 4,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        assert!(should_forward_input_event(&drag));

        let release = Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
        assert!(!should_forward_input_event(&release));

        let press = Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        assert!(should_forward_input_event(&press));
        assert!(!should_forward_input_event(&Event::FocusGained));
        assert!(should_forward_input_event(&Event::Resize(120, 40)));
    }

    #[test]
    fn vt_seq_and_credit_coalescing_track_mutations_accurately() {
        let mut att = AttachedTerminal::new(7, 20, 4);
        assert_eq!(att.vt_seq, 0);
        // Incomplete UTF-8 prefix produces no VT write yet
        att.process_vt_bytes(&[0xe2, 0x9a]);
        assert_eq!(att.vt_seq, 0);
        att.process_vt_bytes(&[0x99]);
        assert_eq!(att.vt_seq, 1);

        let mut credits = Vec::new();
        record_credit(&mut credits, 3, 1024);
        record_credit(&mut credits, 5, 512);
        record_credit(&mut credits, 3, 2048);
        assert_eq!(credits, vec![(3, 3072), (5, 512)]);
    }

    #[test]
    fn clipboard_helper_captures_stdout_and_times_out_hung_process() {
        let ok = run_clipboard_read_cmd(
            "sh",
            &["-c", "printf 'copied-text'"],
            Duration::from_millis(200),
        );
        assert_eq!(ok.as_deref(), Some("copied-text"));

        let start = Instant::now();
        let timed_out = run_clipboard_read_cmd("sh", &["-c", "sleep 5"], Duration::from_millis(40));
        assert!(timed_out.is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn active_needs_sync_tracks_missing_attachments_and_respects_retry_backoff() {
        let retry_t1 = ("t1".to_string(), Instant::now() + ATTACH_RETRY_DELAY);

        // Healthy attached active terminal needs no sync
        assert!(!active_needs_sync(Some("t1"), Some("t1"), true, None));
        // Missing channel for active address triggers sync when not in backoff
        assert!(active_needs_sync(Some("t1"), None, false, None));
        assert!(active_needs_sync(Some("t1"), Some("t1"), false, None));
        // Active backoff suppresses immediate tight-loop retries for the same terminal
        assert!(!active_needs_sync(Some("t1"), None, false, Some(&retry_t1)));
        // Switching to a different terminal bypasses the previous terminal's retry backoff immediately
        assert!(active_needs_sync(Some("t2"), None, false, Some(&retry_t1)));
    }

    #[test]
    fn detect_nested_multiplexer_catches_hc_herdr_tmux_zellij_and_screen() {
        let env_from = |pairs: &[(&str, &str)]| {
            let map: BTreeMap<String, String> = pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            detect_nested_multiplexer(|k| map.get(k).cloned())
        };

        assert_eq!(env_from(&[]), None);
        assert_eq!(
            env_from(&[("TMUX", "   "), ("HERDR_ENV", "0"), ("STY", "")]),
            None
        );
        assert_eq!(env_from(&[("HERDR_ENV", "false")]), None);

        assert_eq!(
            env_from(&[("HC_TERMINAL_ID", "t18")]),
            Some(NestedMultiplexer {
                program: "hc (herdr-core)",
                env_var: "HC_TERMINAL_ID",
                value: "t18".to_string(),
            })
        );
        assert_eq!(
            env_from(&[("HC_TERMINAL_SOCKET", "/tmp/hc/default.sock")]),
            Some(NestedMultiplexer {
                program: "hc (herdr-core)",
                env_var: "HC_TERMINAL_SOCKET",
                value: "/tmp/hc/default.sock".to_string(),
            })
        );
        assert_eq!(
            env_from(&[("HERDR_PANE_ID", "w1:p1")]),
            Some(NestedMultiplexer {
                program: "herdr",
                env_var: "HERDR_PANE_ID",
                value: "w1:p1".to_string(),
            })
        );
        assert_eq!(
            env_from(&[("HERDR_ENV", "1")]),
            Some(NestedMultiplexer {
                program: "herdr",
                env_var: "HERDR_ENV",
                value: "1".to_string(),
            })
        );
        assert_eq!(
            env_from(&[("TMUX", "/tmp/tmux-1000/default,42,0")]),
            Some(NestedMultiplexer {
                program: "tmux",
                env_var: "TMUX",
                value: "/tmp/tmux-1000/default,42,0".to_string(),
            })
        );
        // Zellij sets `ZELLIJ=0` inside active panes; must NOT be ignored as falsey
        assert_eq!(
            env_from(&[("ZELLIJ", "0")]),
            Some(NestedMultiplexer {
                program: "zellij",
                env_var: "ZELLIJ",
                value: "0".to_string(),
            })
        );
        assert_eq!(
            env_from(&[("ZELLIJ_SESSION_NAME", "work")]),
            Some(NestedMultiplexer {
                program: "zellij",
                env_var: "ZELLIJ_SESSION_NAME",
                value: "work".to_string(),
            })
        );
        assert_eq!(
            env_from(&[("STY", "1234.pts-0.box")]),
            Some(NestedMultiplexer {
                program: "GNU screen",
                env_var: "STY",
                value: "1234.pts-0.box".to_string(),
            })
        );
    }

    #[test]
    fn nested_error_message_and_cli_override_behave_consistently() {
        let short_msg = format_nested_session_error(&NestedMultiplexer {
            program: "herdr",
            env_var: "HERDR_PANE_ID",
            value: "w1:p1".to_string(),
        });
        assert!(short_msg.contains("HERDR_PANE_ID=w1:p1"));
        assert!(short_msg.contains("--allow-nested"));

        let path_msg = format_nested_session_error(&NestedMultiplexer {
            program: "tmux",
            env_var: "TMUX",
            value: "/tmp/tmux-1000/default,42,0".to_string(),
        });
        assert!(path_msg.contains("detected $TMUX"));
        assert!(!path_msg.contains("/tmp/tmux-1000"));

        assert!(!resolve_allow_nested(false, false, None));
        assert!(resolve_allow_nested(false, false, Some(true)));
        assert!(!resolve_allow_nested(false, true, Some(true)));
        assert!(resolve_allow_nested(true, false, Some(false)));

        let parsed = Cli::try_parse_from(["hct", "--allow-nested"]).unwrap();
        assert!(parsed.allow_nested);
        assert!(!parsed.no_allow_nested);

        assert!(Cli::try_parse_from(["hct", "--allow-nested", "--no-allow-nested"]).is_err());
    }

    #[test]
    fn format_osc52_sequence_supports_plain_tmux_and_chunked_screen_dcs() {
        let plain = format_osc52_sequence("YWJj", false, false);
        assert_eq!(plain, "\x1b]52;c;YWJj\x07\x1b]52;;YWJj\x07");

        let tmux = format_osc52_sequence("YWJj", true, true);
        assert_eq!(
            tmux,
            "\x1bPtmux;\x1b\x1b]52;c;YWJj\x07\x1b\\\x1b]52;c;YWJj\x07"
        );

        let long_b64 = "A".repeat(120);
        let screen = format_osc52_sequence(&long_b64, false, true);
        assert!(screen.starts_with("\x1bP\x1b]52;c;"));
        assert!(screen.ends_with("\x07\x1b\\"));
        // Verify stripping DCS wrappers reconstructs the exact OSC 52 payload
        let unwrapped = screen.replace("\x1bP", "").replace("\x1b\\", "");
        assert_eq!(unwrapped, format!("\x1b]52;c;{long_b64}\x07"));
    }

    #[test]
    fn cli_version_flag_embeds_build_version() {
        use clap::CommandFactory;
        let rendered = Cli::command().render_version();
        assert_eq!(rendered, format!("hct {BUILD_VERSION}\n"));
        assert!(BUILD_VERSION.starts_with(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn terminal_enter_and_leave_sequences_reset_sgr_and_clear_alt_screen() {
        let mut enter_buf = Vec::new();
        write_terminal_enter_sequence(&mut enter_buf).unwrap();
        let enter_str = String::from_utf8_lossy(&enter_buf);
        let reset_idx = enter_str.find("\x1b[0m").expect("SGR reset on enter");
        let alt_enter_idx = enter_str
            .find("\x1b[?1049h")
            .expect("EnterAlternateScreen on enter");
        let clear_enter_idx = enter_str
            .find("\x1b[2J")
            .expect("Clear(ClearType::All) on enter");
        assert!(reset_idx < alt_enter_idx);
        assert!(alt_enter_idx < clear_enter_idx);

        let mut leave_buf = Vec::new();
        write_terminal_leave_sequence(&mut leave_buf).unwrap();
        let leave_str = String::from_utf8_lossy(&leave_buf);
        let disable_mouse_idx = leave_str
            .find("\x1b[?1000l")
            .expect("DisableMouseCapture on leave");
        let clear_leave_idx = leave_str
            .find("\x1b[2J")
            .expect("Clear(ClearType::All) before LeaveAlternateScreen");
        let alt_leave_idx = leave_str
            .find("\x1b[?1049l")
            .expect("LeaveAlternateScreen on leave");
        let show_cursor_idx = leave_str
            .rfind("\x1b[?25h")
            .expect("Show cursor after LeaveAlternateScreen");
        assert!(disable_mouse_idx < alt_leave_idx);
        assert!(clear_leave_idx < alt_leave_idx);
        assert!(alt_leave_idx < show_cursor_idx);
    }

    #[test]
    fn sanitize_vt_stream_handles_decstr_cht_and_long_split_truecolor_csi() {
        // 1. DECSTR (`\x1b[!p`, emitted by herdr-core attach replay) resets DECSTBM scroll region,
        //    origin/cursor modes, and SGR attributes while preserving the active cursor position!
        let mut att_pos = AttachedTerminal::new(1, 30, 6);
        let decstr_mid_screen = b"\x1b[2;4r\x1b[?1h\x1b[1;31m\x1b[5;10H\x1b[!pX";
        for b in decstr_mid_screen {
            att_pos.process_vt_bytes(std::slice::from_ref(b));
        }
        let scr_pos = att_pos.parser.screen();
        assert_eq!(scr_pos.cursor_position(), (4, 10));
        let cell_x = scr_pos.cell(4, 9).unwrap();
        assert_eq!(cell_x.contents(), "X");
        assert!(!cell_x.bold());
        assert!(!scr_pos.application_cursor());
        // Saved cursor state (`ESC 8`) was also reset to home (0, 0) by DECSTR
        att_pos.process_vt_bytes(b"\x1b8Z");
        assert_eq!(att_pos.parser.screen().cell(0, 0).unwrap().contents(), "Z");

        // Also verify full-screen scrolling works normally after DECSTR clears a narrow DECSTBM
        let mut att = AttachedTerminal::new(2, 30, 6);
        att.process_vt_bytes(b"\x1b[2;3r\x1b[1;31m");
        att.process_vt_bytes(b"\x18\x1b[!p\x1b[1;1HA\x1b[2IB\r\n1\r\n2\r\n3\r\n4\r\n5");
        let scr = att.parser.screen();
        let bottom_cell = scr.cell(5, 0).unwrap();
        assert_eq!(bottom_cell.contents(), "5");
        assert!(!bottom_cell.bold());

        // Check CHT (`\x1b[2I`) expanded to two tab stops (col 0 -> col 16)
        let mut att_tab = AttachedTerminal::new(3, 30, 3);
        att_tab.process_vt_bytes(b"X\x1b[2IY");
        assert_eq!(att_tab.parser.screen().cell(0, 0).unwrap().contents(), "X");
        assert_eq!(att_tab.parser.screen().cell(0, 16).unwrap().contents(), "Y");

        // 2. >32-byte TrueColor SGR CSI split 1 byte at a time never leaks raw CSI text
        let long_sgr = b"\x1b[0;1;3;4;38;2;137;180;250;48;2;30;30;46mOK\x1b[0m";
        assert!(long_sgr.len() > 40);
        let mut att_split = AttachedTerminal::new(4, 20, 3);
        for b in long_sgr {
            att_split.process_vt_bytes(std::slice::from_ref(b));
        }
        assert_eq!(att_split.parser.screen().contents_between(0, 0, 0, 2), "OK");
    }

    #[test]
    fn prefix_b_and_prefix_help_toggle_popovers_open_and_closed() {
        let core = CoreClient::dummy_for_tests();
        let mut world = World::default();
        let prefix = "Ctrl+b";
        let mut sidebar_open = false;
        let mut prefix_active = false;
        let mut ctrl_active = false;
        let mut alt_active = false;
        let mut scrollback_offset = 0;
        let mut modal = Modal::None;
        let rect = Rect::new(0, 1, 80, 22);

        let ctrl_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        let plain_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE);
        let qmark = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);

        // First `^b b` opens `Modal::SpaceMenu`
        assert_eq!(
            handle_key_event(
                ctrl_b,
                &core,
                &mut world,
                None,
                prefix,
                &mut sidebar_open,
                &mut prefix_active,
                &mut ctrl_active,
                &mut alt_active,
                &mut scrollback_offset,
                &mut modal,
                rect,
            )
            .unwrap(),
            KeyOutcome::UiChanged
        );
        assert!(prefix_active);
        assert_eq!(modal, Modal::None);

        assert_eq!(
            handle_key_event(
                plain_b,
                &core,
                &mut world,
                None,
                prefix,
                &mut sidebar_open,
                &mut prefix_active,
                &mut ctrl_active,
                &mut alt_active,
                &mut scrollback_offset,
                &mut modal,
                rect,
            )
            .unwrap(),
            KeyOutcome::UiChanged
        );
        assert!(!prefix_active);
        assert_eq!(modal, Modal::SpaceMenu);

        // Second `^b b` hides `Modal::SpaceMenu`
        handle_key_event(
            ctrl_b,
            &core,
            &mut world,
            None,
            prefix,
            &mut sidebar_open,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut scrollback_offset,
            &mut modal,
            rect,
        )
        .unwrap();
        assert!(prefix_active);
        assert_eq!(modal, Modal::SpaceMenu);

        handle_key_event(
            plain_b,
            &core,
            &mut world,
            None,
            prefix,
            &mut sidebar_open,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut scrollback_offset,
            &mut modal,
            rect,
        )
        .unwrap();
        assert!(!prefix_active);
        assert_eq!(modal, Modal::None);

        // `^b ?` opens `Modal::Help`, and a second `^b ?` closes it cleanly
        handle_key_event(
            ctrl_b,
            &core,
            &mut world,
            None,
            prefix,
            &mut sidebar_open,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut scrollback_offset,
            &mut modal,
            rect,
        )
        .unwrap();
        handle_key_event(
            qmark,
            &core,
            &mut world,
            None,
            prefix,
            &mut sidebar_open,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut scrollback_offset,
            &mut modal,
            rect,
        )
        .unwrap();
        assert_eq!(modal, Modal::Help);

        handle_key_event(
            ctrl_b,
            &core,
            &mut world,
            None,
            prefix,
            &mut sidebar_open,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut scrollback_offset,
            &mut modal,
            rect,
        )
        .unwrap();
        handle_key_event(
            qmark,
            &core,
            &mut world,
            None,
            prefix,
            &mut sidebar_open,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut scrollback_offset,
            &mut modal,
            rect,
        )
        .unwrap();
        assert_eq!(modal, Modal::None);
    }

    #[test]
    fn horizontal_wheel_switches_tabs_and_spaces_with_momentum_and_vertical_bleed_guards() {
        use crate::model::{make_single_terminal_tab, SharedSpace};
        use serde_json::Map;

        let core = CoreClient::dummy_for_tests();
        let mut world = World::default();
        world.doc.spaces.push(SharedSpace {
            id: "sp1".to_string(),
            name: "alpha".to_string(),
            folder: "/work/alpha".to_string(),
            tabs: vec![
                make_single_terminal_tab("tb1", "one", "t1"),
                make_single_terminal_tab("tb2", "two", "t2"),
                make_single_terminal_tab("tb3", "three", "t3"),
            ],
            rest: Map::new(),
        });
        world.doc.spaces.push(SharedSpace {
            id: "sp2".to_string(),
            name: "beta".to_string(),
            folder: "/work/beta".to_string(),
            tabs: vec![make_single_terminal_tab("tb4", "four", "t4")],
            rest: Map::new(),
        });
        world.clamp_selection();
        assert_eq!(world.active_tab_index(), 0);

        let term_rect = Rect::new(0, 1, 80, 22);
        let mut horiz_wheel = HorizontalWheelGestureState::default();
        let mut modal = Modal::None;
        let mut prefix_active = true;
        let mut ctrl_active = true;
        let mut alt_active = false;
        let mut done_sel = TerminalSelection::new(2, 4);
        done_sel.head = (2, 10);
        done_sel.active = false;
        let mut selection = Some(done_sel);
        let mut selection_screen = None;
        let mut selection_vt_seq = None;

        let t0 = Instant::now();

        // 1. ScrollRight (natural swipe left) inside viewport switches Tab 0 -> Tab 1 immediately
        //    and clears sticky modifiers + completed selection highlight.
        assert!(handle_horizontal_wheel_event(
            HorizontalSwipeDir::Next,
            10,
            term_rect,
            t0,
            &mut horiz_wheel,
            &core,
            &mut world,
            &modal,
            false,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut selection,
            &mut selection_screen,
            &mut selection_vt_seq,
        ));
        assert_eq!(world.active_tab_index(), 1);
        assert!(!prefix_active && !ctrl_active);
        assert!(selection.is_none());

        // 2. Momentum inertia tail (packets arriving every 16ms..80ms in same direction) is coalesced
        //    so a single trackpad flick or fast thumb swipe never overshoots multiple tabs!
        for ms in [16u64, 40, 90, 150, 230] {
            assert!(!handle_horizontal_wheel_event(
                HorizontalSwipeDir::Next,
                10,
                term_rect,
                t0 + Duration::from_millis(ms),
                &mut horiz_wheel,
                &core,
                &mut world,
                &modal,
                false,
                &mut prefix_active,
                &mut ctrl_active,
                &mut alt_active,
                &mut selection,
                &mut selection_screen,
                &mut selection_vt_seq,
            ));
        }
        assert_eq!(world.active_tab_index(), 1);

        // 3. Reversing direction (ScrollLeft) bypasses same-direction quiet gap with zero latency -> Tab 1 -> Tab 0!
        assert!(handle_horizontal_wheel_event(
            HorizontalSwipeDir::Prev,
            10,
            term_rect,
            t0 + Duration::from_millis(250),
            &mut horiz_wheel,
            &core,
            &mut world,
            &modal,
            false,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut selection,
            &mut selection_screen,
            &mut selection_vt_seq,
        ));
        assert_eq!(world.active_tab_index(), 0);

        // 4. After HORIZ_WHEEL_QUIET_GAP elapses, a second deliberate swipe in the same direction steps cleanly -> Tab 2 (wrap)
        let t_next = t0 + Duration::from_millis(250) + HORIZ_WHEEL_QUIET_GAP;
        assert!(handle_horizontal_wheel_event(
            HorizontalSwipeDir::Prev,
            10,
            term_rect,
            t_next,
            &mut horiz_wheel,
            &core,
            &mut world,
            &modal,
            false,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut selection,
            &mut selection_screen,
            &mut selection_vt_seq,
        ));
        assert_eq!(world.active_tab_index(), 2);

        // 5. Diagonal trackpad bleed during viewport vertical scroll is suppressed within VERT_WHEEL_BLEED_GUARD,
        //    while explicit top-bar (row 0) horizontal scroll remains unaffected by viewport vertical scroll!
        let t_vert = t_next + Duration::from_secs(1);
        horiz_wheel.record_viewport_vertical_wheel(t_vert);
        assert!(!handle_horizontal_wheel_event(
            HorizontalSwipeDir::Next,
            10,
            term_rect,
            t_vert + Duration::from_millis(40),
            &mut horiz_wheel,
            &core,
            &mut world,
            &modal,
            false,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut selection,
            &mut selection_screen,
            &mut selection_vt_seq,
        ));
        assert_eq!(world.active_tab_index(), 2);

        // Top bar (row 0 < term_rect.y) switches tabs unless top bar itself just scrolled vertically
        assert!(handle_horizontal_wheel_event(
            HorizontalSwipeDir::Next,
            0,
            term_rect,
            t_vert + Duration::from_millis(50),
            &mut horiz_wheel,
            &core,
            &mut world,
            &modal,
            false,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut selection,
            &mut selection_screen,
            &mut selection_vt_seq,
        ));
        assert_eq!(world.active_tab_index(), 0);

        horiz_wheel.record_top_bar_vertical_wheel(t_vert + Duration::from_millis(300));
        assert!(!handle_horizontal_wheel_event(
            HorizontalSwipeDir::Next,
            0,
            term_rect,
            t_vert + Duration::from_millis(320),
            &mut horiz_wheel,
            &core,
            &mut world,
            &modal,
            false,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut selection,
            &mut selection_screen,
            &mut selection_vt_seq,
        ));
        assert_eq!(world.active_tab_index(), 0);

        // 6. Active text selection drag, floating D-pad drag, or blocking modals (Help) block tab switches
        let t_guards = t_vert + Duration::from_secs(2);
        let mut active_sel = Some(TerminalSelection::new(3, 3));
        assert!(!handle_horizontal_wheel_event(
            HorizontalSwipeDir::Next,
            10,
            term_rect,
            t_guards,
            &mut horiz_wheel,
            &core,
            &mut world,
            &modal,
            false,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut active_sel,
            &mut selection_screen,
            &mut selection_vt_seq,
        ));
        assert!(!handle_horizontal_wheel_event(
            HorizontalSwipeDir::Next,
            10,
            term_rect,
            t_guards,
            &mut horiz_wheel,
            &core,
            &mut world,
            &modal,
            true,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut selection,
            &mut selection_screen,
            &mut selection_vt_seq,
        ));
        modal = Modal::Help;
        assert!(!handle_horizontal_wheel_event(
            HorizontalSwipeDir::Next,
            10,
            term_rect,
            t_guards,
            &mut horiz_wheel,
            &core,
            &mut world,
            &modal,
            false,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut selection,
            &mut selection_screen,
            &mut selection_vt_seq,
        ));
        assert_eq!(world.active_tab_index(), 0);

        // 7. In Modal::SpaceMenu, horizontal wheel switches Spaces cleanly
        modal = Modal::SpaceMenu;
        assert_eq!(world.active_space().map(|s| s.id.as_str()), Some("sp1"));
        assert!(handle_horizontal_wheel_event(
            HorizontalSwipeDir::Next,
            10,
            term_rect,
            t_guards + Duration::from_secs(1),
            &mut horiz_wheel,
            &core,
            &mut world,
            &modal,
            false,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut selection,
            &mut selection_screen,
            &mut selection_vt_seq,
        ));
        assert_eq!(world.active_space().map(|s| s.id.as_str()), Some("sp2"));
    }
}
