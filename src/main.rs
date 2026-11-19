mod core;
mod keys;
mod model;
mod reader;
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
use crate::model::{AgentState, EventDisposition, World};
#[cfg(test)]
use crate::ui::capture_eink_screen_snapshot;
use crate::ui::{
    capture_eink_screen_snapshot_with_color_and_fence, collect_fuzzy_items, collect_overview_items,
    compute_terminal_rect, find_hit_target, is_active_tab_agent, is_in_rect, is_on_dpad_disc,
    overview_selection_after_query_change, overview_selection_for_index, reader_body_metrics,
    reader_initial_scroll, render_ui_full_with_eink_color_and_fence, resolve_dpad_disc_rect,
    resolve_fuzzy_selection, resolve_mod_cross_rect, resolve_overview_selection,
    scan_viewport_initial_code_fence, EinkScreenSnapshot, HitRegion, HitTarget, Modal,
    OverviewItem, OverviewSelection, PromptKind, TerminalSelection,
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
const HORIZ_WHEEL_QUIET_GAP: Duration = Duration::from_millis(180);
const VERT_SCROLL_LOCK_WINDOW: Duration = Duration::from_millis(350);
const HORIZ_SWIPE_ACCUM_WINDOW: Duration = Duration::from_millis(250);
const VIEWPORT_HORIZ_SWIPE_THRESHOLD: u8 = 3;
const TOP_BAR_HORIZ_SWIPE_THRESHOLD: u8 = 1;
const SPACE_MENU_HORIZ_SWIPE_THRESHOLD: u8 = 1;
const DEFAULT_EINK_INTERVAL_MS: u64 = 250;
const MAX_EINK_INTERVAL_MS: u64 = 2000;

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

fn write_terminal_leave_sequence_with_mux<W: Write>(
    w: &mut W,
    in_tmux: bool,
    in_screen: bool,
) -> io::Result<()> {
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
    )?;
    let clear_osc = format_herdr_session_clear_osc(in_tmux, in_screen);
    w.write_all(clear_osc.as_bytes())?;
    w.flush()
}

#[cfg(test)]
fn write_terminal_leave_sequence<W: Write>(w: &mut W) -> io::Result<()> {
    write_terminal_leave_sequence_with_mux(w, false, false)
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

struct TerminalCleanupGuard {
    in_tmux: bool,
    in_screen: bool,
}

impl Drop for TerminalCleanupGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = write_terminal_leave_sequence_with_mux(&mut stdout, self.in_tmux, self.in_screen);
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

    /// Optimize UI and frame pacing for 300 PPI monochrome E-Ink displays
    #[arg(short = 'e', long)]
    eink: bool,

    /// Disable E-Ink mode (overrides ui.eink = true or environment detection)
    #[arg(long, conflicts_with = "eink")]
    no_eink: bool,

    /// Enable Kaleido color E-Ink accent overprint mode (automatically activates --eink)
    #[arg(long, conflicts_with_all = ["no_eink", "no_eink_color"])]
    eink_color: bool,

    /// Disable Kaleido color E-Ink accent overprint mode (overrides ui.eink_color = true or environment detection)
    #[arg(long, conflicts_with = "eink_color")]
    no_eink_color: bool,

    /// Passive PTY output coalescing interval in milliseconds when --eink is enabled (default: 250, max: 2000)
    #[arg(long)]
    eink_interval_ms: Option<u64>,
}

fn resolve_eink_color_mode<F>(
    cli_eink_color: bool,
    cli_no_eink_color: bool,
    cfg_eink_color: Option<bool>,
    mut get_env: F,
) -> bool
where
    F: FnMut(&str) -> Option<String>,
{
    if cli_eink_color {
        return true;
    }
    if cli_no_eink_color {
        return false;
    }
    if let Some(v) = cfg_eink_color {
        return v;
    }
    if let Some(raw) = get_env("HCT_EINK_COLOR") {
        let trimmed = raw.trim().to_ascii_lowercase();
        match trimmed.as_str() {
            "1" | "true" | "yes" | "on" => return true,
            "0" | "false" | "no" | "off" => return false,
            _ => {}
        }
    }
    for var in ["LC_TERMINAL_PROFILE", "HCT_THEME", "TERM"] {
        if let Some(raw) = get_env(var) {
            let lower = raw.trim().to_ascii_lowercase();
            if lower.contains("kaleido")
                || lower.contains("eink-color")
                || lower.contains("eink_color")
                || lower.contains("e-ink-color")
            {
                return true;
            }
        }
    }
    false
}

fn resolve_eink_and_color_modes<F>(
    cli_eink: bool,
    cli_no_eink: bool,
    cli_eink_color: bool,
    cli_no_eink_color: bool,
    cfg_eink: Option<bool>,
    cfg_eink_color: Option<bool>,
    mut get_env: F,
) -> (bool, bool)
where
    F: FnMut(&str) -> Option<String>,
{
    let eink_color = resolve_eink_color_mode(
        cli_eink_color,
        cli_no_eink_color,
        cfg_eink_color,
        &mut get_env,
    );
    if cli_no_eink {
        return (false, eink_color);
    }
    if cli_eink || cli_eink_color {
        return (true, eink_color);
    }
    if let Some(v) = cfg_eink {
        return (v, eink_color);
    }
    if cfg_eink_color == Some(true) {
        return (true, eink_color);
    }
    if let Some(raw) = get_env("HCT_EINK") {
        let trimmed = raw.trim().to_ascii_lowercase();
        match trimmed.as_str() {
            "1" | "true" | "yes" | "on" => return (true, eink_color),
            "0" | "false" | "no" | "off" => return (false, eink_color),
            _ => {}
        }
    }
    for var in ["LC_TERMINAL_PROFILE", "HCT_THEME", "TERM"] {
        if let Some(raw) = get_env(var) {
            let lower = raw.trim().to_ascii_lowercase();
            if lower.contains("eink")
                || lower.contains("e-ink")
                || lower.contains("monochrome")
                || lower.contains("kaleido")
            {
                return (true, eink_color);
            }
        }
    }
    (eink_color, eink_color)
}

#[cfg(test)]
fn resolve_eink_mode<F>(
    cli_eink: bool,
    cli_no_eink: bool,
    cfg_eink: Option<bool>,
    get_env: F,
) -> bool
where
    F: FnMut(&str) -> Option<String>,
{
    resolve_eink_and_color_modes(cli_eink, cli_no_eink, false, false, cfg_eink, None, get_env).0
}

fn resolve_eink_interval(cli_ms: Option<u64>, cfg_ms: Option<u64>) -> Duration {
    Duration::from_millis(
        cli_ms
            .or(cfg_ms)
            .unwrap_or(DEFAULT_EINK_INTERVAL_MS)
            .min(MAX_EINK_INTERVAL_MS),
    )
}

fn main_scrollback_wheel_step(term_height: u16, eink_mode: bool) -> usize {
    if eink_mode {
        (term_height as usize / 2).max(3)
    } else {
        3
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NestedMultiplexer {
    program: &'static str,
    env_var: &'static str,
    value: String,
}

fn read_env_lossy(key: &str) -> Option<String> {
    std::env::var_os(key).map(|v| v.to_string_lossy().into_owned())
}

fn detect_multiplexer_env_with<F: Fn(&str) -> Option<String>>(env_get: F) -> (bool, bool) {
    let in_tmux = env_get("TMUX").is_some_and(|v| !v.trim().is_empty());
    let in_screen = env_get("STY").is_some_and(|v| !v.trim().is_empty());
    (in_tmux, in_screen)
}

fn detect_multiplexer_env() -> (bool, bool) {
    detect_multiplexer_env_with(read_env_lossy)
}

/// Resolve the effective `herdr-core` session name (`--session` overrides `$HC_SESSION`,
/// trimming surrounding whitespace and turning empty/whitespace values into `None`).
/// `main()` passes the resolved value back through `connect_session_arg` to
/// `CoreClient::connect_with_sender` so `herdr-core` and `herdr_session` always match.
fn resolve_herdr_session<F: Fn(&str) -> Option<String>>(
    cli_session: Option<&str>,
    env_get: F,
) -> Option<String> {
    if let Some(raw) = cli_session {
        let trimmed = raw.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_string());
    }
    env_get("HC_SESSION").and_then(|raw| {
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn connect_session_arg<'a>(
    cli_session: Option<&str>,
    resolved_session: Option<&'a str>,
) -> Option<&'a str> {
    if resolved_session.is_some() {
        resolved_session
    } else if cli_session.is_some() {
        Some("")
    } else {
        None
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlStringKind {
    Osc,
    StOnly,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct GraphemeState {
    cluster_cols: u8,
    prev_emoji_symbol: bool,
    ascii_keycap_base: bool,
    after_zwj: bool,
    suppress_zero_width: bool,
}

impl GraphemeState {
    fn from_char(ch: char) -> Self {
        if ch.is_ascii() {
            if is_ascii_keycap_base(ch as u8) {
                Self {
                    cluster_cols: 1,
                    ascii_keycap_base: true,
                    ..Self::default()
                }
            } else {
                Self::default()
            }
        } else {
            match UnicodeWidthChar::width(ch) {
                Some(2) => Self {
                    cluster_cols: 2,
                    prev_emoji_symbol: true,
                    ..Self::default()
                },
                Some(1) => Self {
                    cluster_cols: 1,
                    prev_emoji_symbol: is_emoji_symbol_width1(ch),
                    ..Self::default()
                },
                _ => Self::default(),
            }
        }
    }
}

struct AttachedTerminal {
    channel: u32,
    parser: vt100::Parser,
    cols: u16,
    rows: u16,
    utf8_tail: Vec<u8>,
    sanitize_ctrl_str: Option<ControlStringKind>,
    feed_ctrl_str: Option<ControlStringKind>,
    grapheme: GraphemeState,
    autowrap: bool,
    saved_autowrap: bool,
    alt_saved_autowrap: bool,
    dropped_wide_at_margin: bool,
    last_printed_char: Option<char>,
    scroll_top: u16,
    main_scroll_top: u16,
    vt_seq: u64,
    cached_viewport_fence: Option<(u64, usize, u16, u16, bool)>,
}

const VIEWPORT_FENCE_LOOKBACK: usize = 200;

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
            sanitize_ctrl_str: None,
            feed_ctrl_str: None,
            grapheme: GraphemeState::default(),
            autowrap: true,
            saved_autowrap: true,
            alt_saved_autowrap: true,
            dropped_wide_at_margin: false,
            last_printed_char: None,
            scroll_top: 0,
            main_scroll_top: 0,
            vt_seq: 0,
            cached_viewport_fence: None,
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.main_scroll_top = 0;
        self.dropped_wide_at_margin = false;
        self.cached_viewport_fence = None;
        self.parser.set_size(non_zero_u16(rows), non_zero_u16(cols));
    }

    fn viewport_initial_code_fence(&mut self, scrollback_offset: usize) -> bool {
        let key = (self.vt_seq, scrollback_offset, self.cols, self.rows);
        if let Some((k_seq, k_sb, k_cols, k_rows, cached)) = self.cached_viewport_fence {
            if (k_seq, k_sb, k_cols, k_rows) == key {
                return cached;
            }
        }
        let val =
            scan_viewport_initial_code_fence(self.parser.screen_mut(), VIEWPORT_FENCE_LOOKBACK);
        self.cached_viewport_fence = Some((key.0, key.1, key.2, key.3, val));
        val
    }

    fn process_vt_bytes(&mut self, payload: &[u8]) {
        let clean = sanitize_vt_stream(
            payload,
            &mut self.utf8_tail,
            &mut self.sanitize_ctrl_str,
            &mut self.grapheme,
        );
        if clean.is_empty() {
            return;
        }
        self.feed_sanitized_bytes(&clean);
        self.vt_seq = self.vt_seq.wrapping_add(1);
    }

    fn clamp_pending_wrap(&mut self) {
        let cols = self.parser.screen().size().1.get().max(1);
        let (_, col) = self.parser.screen().cursor_position();
        if col >= cols {
            let seq = format!("\x1b[{cols}G");
            self.parser.process(seq.as_bytes());
        }
    }

    fn feed_char_no_autowrap(&mut self, ch: char, utf8_bytes: &[u8]) {
        let Some(cw) = UnicodeWidthChar::width(ch).or_else(|| ((ch as u32) >= 256).then_some(1))
        else {
            return;
        };
        let w = (cw as u16).min(2);
        let cols = self.parser.screen().size().1.get().max(1);
        let (_, col) = self.parser.screen().cursor_position();

        if w == 0 {
            // Keep `vt100` at `pos.col == cols` lazily after printing in the rightmost column so
            // zero-width combining marks attach natively to the last printed base cell (`cols - 1`,
            // or `cols - 2` for wide glyphs). Suppress marks only after an overflowing wide glyph
            // at the right margin could not be drawn.
            if !self.dropped_wide_at_margin && col > 0 {
                self.parser.process(utf8_bytes);
            }
            return;
        }

        self.last_printed_char = Some(ch);
        if w == 1 {
            if col >= cols {
                self.clamp_pending_wrap();
            }
            self.parser.process(utf8_bytes);
            self.dropped_wide_at_margin = false;
        } else if cols >= 2 {
            if col.saturating_add(2) <= cols {
                self.parser.process(utf8_bytes);
                self.dropped_wide_at_margin = false;
            } else {
                // Wide character cannot fit in the single remaining column (`cols - 1`) while
                // DECAWM (`?7l`) forbids auto-wrapping onto the next row.
                self.clamp_pending_wrap();
                self.dropped_wide_at_margin = true;
            }
        } else {
            self.dropped_wide_at_margin = true;
        }
    }

    fn feed_sanitized_bytes(&mut self, clean: &[u8]) {
        let mut plain_start = 0usize;
        let mut i = 0usize;

        while i < clean.len() {
            if let Some(kind) = self.feed_ctrl_str {
                let b = clean[i];
                if (kind == ControlStringKind::Osc && b == 0x07) || b == 0x18 || b == 0x1a {
                    self.feed_ctrl_str = None;
                    i += 1;
                    continue;
                }
                if b == 0x1b {
                    self.feed_ctrl_str = None;
                    if i + 1 < clean.len() && clean[i + 1] == b'\\' {
                        i += 2;
                        continue;
                    }
                    // Unclosed control string interrupted by a new ESC introducer; fall through.
                } else {
                    i += 1;
                    continue;
                }
            }

            let b = clean[i];
            if b == 0x1b {
                if i + 1 >= clean.len() {
                    break;
                }
                let next = clean[i + 1];
                if next == b']' {
                    self.feed_ctrl_str = Some(ControlStringKind::Osc);
                    i += 2;
                    continue;
                }
                if matches!(next, b'P' | b'X' | b'^' | b'_') {
                    self.feed_ctrl_str = Some(ControlStringKind::StOnly);
                    i += 2;
                    continue;
                }
                if next == b'[' {
                    let param_start = i + 2;
                    let mut p = param_start;
                    while p < clean.len() && (0x30..=0x3F).contains(&clean[p]) {
                        p += 1;
                    }
                    let param_end = p;
                    while p < clean.len() && (0x20..=0x2F).contains(&clean[p]) {
                        p += 1;
                    }
                    let inter_end = p;
                    if p < clean.len() && (0x40..=0x7E).contains(&clean[p]) {
                        let params = &clean[param_start..param_end];
                        let intermediates = &clean[param_end..inter_end];
                        let final_byte = clean[p];

                        // DECSTR (`CSI ! p` — Soft Terminal Reset)
                        if params.is_empty() && intermediates == b"!" && final_byte == b'p' {
                            if plain_start < i {
                                self.parser.process(&clean[plain_start..i]);
                            }
                            apply_decstr(&mut self.parser);
                            self.autowrap = true;
                            self.saved_autowrap = true;
                            self.dropped_wide_at_margin = false;
                            self.scroll_top = 0;
                            i = p + 1;
                            plain_start = i;
                            continue;
                        }

                        if intermediates.is_empty() {
                            if params.starts_with(b"?")
                                && (final_byte == b'h' || final_byte == b'l')
                            {
                                let set = final_byte == b'h';
                                let touches_state = params[1..].split(|&c| c == b';').any(|seg| {
                                    matches!(seg, b"6" | b"7" | b"47" | b"1047" | b"1048" | b"1049")
                                });
                                if touches_state {
                                    if plain_start < i {
                                        self.parser.process(&clean[plain_start..i]);
                                    }
                                    let was_alt = self.parser.screen().alternate_screen();
                                    for seg in params[1..].split(|&c| c == b';') {
                                        match seg {
                                            b"7" => {
                                                self.clamp_pending_wrap();
                                                self.autowrap = set;
                                                self.dropped_wide_at_margin = false;
                                            }
                                            b"6" => {
                                                self.dropped_wide_at_margin = false;
                                            }
                                            b"1048" => {
                                                if set {
                                                    if !self.autowrap {
                                                        self.clamp_pending_wrap();
                                                    }
                                                    self.saved_autowrap = self.autowrap;
                                                } else {
                                                    self.autowrap = self.saved_autowrap;
                                                }
                                                self.dropped_wide_at_margin = false;
                                            }
                                            b"47" | b"1047" => {
                                                if set && !was_alt {
                                                    self.main_scroll_top = self.scroll_top;
                                                    self.scroll_top = 0;
                                                } else if !set && was_alt {
                                                    self.scroll_top = self.main_scroll_top;
                                                }
                                                self.dropped_wide_at_margin = false;
                                            }
                                            b"1049" => {
                                                if set && !was_alt {
                                                    if !self.autowrap {
                                                        self.clamp_pending_wrap();
                                                    }
                                                    self.alt_saved_autowrap = self.autowrap;
                                                    self.main_scroll_top = self.scroll_top;
                                                    self.scroll_top = 0;
                                                } else if !set && was_alt {
                                                    self.autowrap = self.alt_saved_autowrap;
                                                    self.scroll_top = self.main_scroll_top;
                                                }
                                                self.dropped_wide_at_margin = false;
                                            }
                                            _ => {}
                                        }
                                    }
                                    self.parser.process(&clean[i..=p]);
                                    i = p + 1;
                                    plain_start = i;
                                    continue;
                                }
                            } else if !self.autowrap
                                && params.starts_with(b"?")
                                && matches!(final_byte, b'J' | b'K')
                            {
                                if plain_start < i {
                                    self.parser.process(&clean[plain_start..i]);
                                }
                                self.dropped_wide_at_margin = false;
                                self.clamp_pending_wrap();
                                self.parser.process(&clean[i..=p]);
                                i = p + 1;
                                plain_start = i;
                                continue;
                            } else if params.iter().all(|&c| c.is_ascii_digit() || c == b';') {
                                match final_byte {
                                    // CBT (`CSI Ps Z` — Cursor Backward Tabulation)
                                    b'Z' => {
                                        if plain_start < i {
                                            self.parser.process(&clean[plain_start..i]);
                                        }
                                        self.dropped_wide_at_margin = false;
                                        let count =
                                            parse_csi_param_default(params, 0, 1).clamp(1, 512);
                                        let cols = self.parser.screen().size().1.get().max(1);
                                        let (_, col) = self.parser.screen().cursor_position();
                                        let mut target = col.min(cols - 1);
                                        for _ in 0..count {
                                            target =
                                                if target == 0 { 0 } else { (target - 1) & !7 };
                                        }
                                        let seq = format!("\x1b[{}G", target + 1);
                                        self.parser.process(seq.as_bytes());
                                        i = p + 1;
                                        plain_start = i;
                                        continue;
                                    }
                                    // CUB (`CSI Ps D` — Cursor Backward): clamp deferred-wrap first
                                    b'D' => {
                                        if plain_start < i {
                                            self.parser.process(&clean[plain_start..i]);
                                        }
                                        self.dropped_wide_at_margin = false;
                                        let cols = self.parser.screen().size().1.get().max(1);
                                        let (_, col) = self.parser.screen().cursor_position();
                                        if col >= cols {
                                            let count = parse_csi_param_default(params, 0, 1)
                                                .min(cols as usize)
                                                as u16;
                                            let target = (cols - 1).saturating_sub(count);
                                            let seq = format!("\x1b[{}G", target + 1);
                                            self.parser.process(seq.as_bytes());
                                        } else {
                                            self.parser.process(&clean[i..=p]);
                                        }
                                        i = p + 1;
                                        plain_start = i;
                                        continue;
                                    }
                                    // Vertical cursor motions clear pending_wrap in xterm/Ghostty
                                    b'A' | b'B' | b'd' => {
                                        if plain_start < i {
                                            self.parser.process(&clean[plain_start..i]);
                                        }
                                        self.dropped_wide_at_margin = false;
                                        self.clamp_pending_wrap();
                                        self.parser.process(&clean[i..=p]);
                                        i = p + 1;
                                        plain_start = i;
                                        continue;
                                    }
                                    // REP (`CSI Ps b` — Repeat Preceding Graphic Character)
                                    b'b' if !self.autowrap => {
                                        if plain_start < i {
                                            self.parser.process(&clean[plain_start..i]);
                                        }
                                        let count =
                                            parse_csi_param_default(params, 0, 1).clamp(1, 4096);
                                        if let Some(ch) = self.last_printed_char {
                                            let mut utf8 = [0u8; 4];
                                            let encoded = ch.encode_utf8(&mut utf8);
                                            for _ in 0..count {
                                                self.feed_char_no_autowrap(ch, encoded.as_bytes());
                                            }
                                        }
                                        i = p + 1;
                                        plain_start = i;
                                        continue;
                                    }
                                    // DECSTBM (`CSI top ; bot r` — Set Top and Bottom Margins)
                                    b'r' => {
                                        if plain_start < i {
                                            self.parser.process(&clean[plain_start..i]);
                                        }
                                        self.dropped_wide_at_margin = false;
                                        let rows = self.parser.screen().size().0.get().max(1);
                                        let top = (parse_csi_param_default(params, 0, 1) as u16)
                                            .max(1)
                                            - 1;
                                        let bot =
                                            (parse_csi_param_default(params, 1, rows as usize)
                                                as u16)
                                                .max(1)
                                                .saturating_sub(1)
                                                .min(rows - 1);
                                        self.scroll_top = if top < bot { top } else { 0 };
                                        self.parser.process(&clean[i..=p]);
                                        i = p + 1;
                                        plain_start = i;
                                        continue;
                                    }
                                    b'C' | b'G' | b'H' | b'J' | b'K' | b'L' | b'M' | b'P'
                                    | b'S' | b'T' | b'X' | b'@' => {
                                        if !self.autowrap
                                            && matches!(
                                                final_byte,
                                                b'J' | b'K' | b'X' | b'@' | b'P'
                                            )
                                        {
                                            if plain_start < i {
                                                self.parser.process(&clean[plain_start..i]);
                                            }
                                            self.dropped_wide_at_margin = false;
                                            self.clamp_pending_wrap();
                                            self.parser.process(&clean[i..=p]);
                                            i = p + 1;
                                            plain_start = i;
                                            continue;
                                        }
                                        self.dropped_wide_at_margin = false;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        i = p + 1;
                        continue;
                    }
                    // Malformed/aborted CSI sequence without valid final byte
                    i = p;
                    continue;
                }

                // Standard 2-byte or intermediate ESC sequence (`ESC <0x20..=0x2F>* <0x30..=0x7E>`)
                let mut e = i + 1;
                while e < clean.len() && (0x20..=0x2F).contains(&clean[e]) {
                    e += 1;
                }
                if e < clean.len() && (0x30..=0x7E).contains(&clean[e]) {
                    if e == i + 1 {
                        match clean[e] {
                            b'7' => {
                                if plain_start < i {
                                    self.parser.process(&clean[plain_start..i]);
                                }
                                if !self.autowrap {
                                    self.clamp_pending_wrap();
                                }
                                self.saved_autowrap = self.autowrap;
                                self.dropped_wide_at_margin = false;
                                self.parser.process(b"\x1b7");
                                i = e + 1;
                                plain_start = i;
                                continue;
                            }
                            b'8' => {
                                if plain_start < i {
                                    self.parser.process(&clean[plain_start..i]);
                                }
                                self.autowrap = self.saved_autowrap;
                                self.dropped_wide_at_margin = false;
                                self.parser.process(b"\x1b8");
                                i = e + 1;
                                plain_start = i;
                                continue;
                            }
                            b'c' => {
                                if plain_start < i {
                                    self.parser.process(&clean[plain_start..i]);
                                }
                                self.autowrap = true;
                                self.saved_autowrap = true;
                                self.alt_saved_autowrap = true;
                                self.dropped_wide_at_margin = false;
                                self.last_printed_char = None;
                                self.scroll_top = 0;
                                self.main_scroll_top = 0;
                                self.parser.process(b"\x1bc");
                                i = e + 1;
                                plain_start = i;
                                continue;
                            }
                            b'M' => {
                                if plain_start < i {
                                    self.parser.process(&clean[plain_start..i]);
                                }
                                self.dropped_wide_at_margin = false;
                                self.clamp_pending_wrap();
                                let (row, _) = self.parser.screen().cursor_position();
                                // Guard against vt100-0.18.1 `row_dec_scroll` scrolling
                                // `[scroll_top..=scroll_bottom]` when cursor is at row 0 above `scroll_top`.
                                if !(row == 0 && self.scroll_top > 0) {
                                    self.parser.process(b"\x1bM");
                                }
                                i = e + 1;
                                plain_start = i;
                                continue;
                            }
                            _ => {}
                        }
                    }
                    i = e + 1;
                    continue;
                }
                i += 1;
                continue;
            }

            // C0 control characters
            if b < 0x20 || b == 0x7f {
                match b {
                    0x08 => {
                        if plain_start < i {
                            self.parser.process(&clean[plain_start..i]);
                        }
                        self.dropped_wide_at_margin = false;
                        let cols = self.parser.screen().size().1.get().max(1);
                        let (_, col) = self.parser.screen().cursor_position();
                        if col >= cols {
                            let target = (cols - 1).saturating_sub(1);
                            let seq = format!("\x1b[{}G", target + 1);
                            self.parser.process(seq.as_bytes());
                        } else {
                            self.parser.process(b"\x08");
                        }
                        i += 1;
                        plain_start = i;
                        continue;
                    }
                    0x0a..=0x0c => {
                        self.dropped_wide_at_margin = false;
                        if i > plain_start && clean[i - 1] == b'\r' {
                            i += 1;
                            continue;
                        }
                        if plain_start < i {
                            self.parser.process(&clean[plain_start..i]);
                        }
                        self.clamp_pending_wrap();
                        self.parser.process(&clean[i..=i]);
                        i += 1;
                        plain_start = i;
                        continue;
                    }
                    0x09 | 0x0d => {
                        self.dropped_wide_at_margin = false;
                    }
                    _ => {}
                }
                i += 1;
                continue;
            }

            // Printable ASCII or UTF-8 character
            if !self.autowrap {
                if plain_start < i {
                    self.parser.process(&clean[plain_start..i]);
                }
                if b.is_ascii() {
                    self.feed_char_no_autowrap(b as char, &clean[i..=i]);
                    i += 1;
                } else if let Some((ch, ch_len)) = next_utf8_char(&clean[i..]) {
                    self.feed_char_no_autowrap(ch, &clean[i..i + ch_len]);
                    i += ch_len;
                } else {
                    i += 1;
                }
                plain_start = i;
                continue;
            }

            if b.is_ascii() {
                self.last_printed_char = Some(b as char);
                self.dropped_wide_at_margin = false;
                i += 1;
            } else if let Some((ch, ch_len)) = next_utf8_char(&clean[i..]) {
                if UnicodeWidthChar::width(ch).unwrap_or(1) > 0 {
                    self.last_printed_char = Some(ch);
                    self.dropped_wide_at_margin = false;
                }
                i += ch_len;
            } else {
                i += 1;
            }
        }

        if plain_start < clean.len() {
            self.parser.process(&clean[plain_start..]);
        }
    }
}

fn parse_csi_param_default(params: &[u8], index: usize, default: usize) -> usize {
    std::str::from_utf8(params)
        .ok()
        .and_then(|s| s.split(';').nth(index))
        .and_then(|s| {
            if s.is_empty() {
                Some(default)
            } else {
                s.parse::<usize>()
                    .ok()
                    .map(|v| if v == 0 { default } else { v })
            }
        })
        .unwrap_or(default)
}

/// Emulate DECSTR (`CSI ! p` — Soft Terminal Reset), which `herdr-core` emits on attach replay
/// and `vt100` otherwise ignores as `unhandled_csi`. Resets SGR, cursor/keypad/origin modes,
/// scroll margins (`DECSTBM`), cursor visibility, and saved-cursor state (`DECSC`) while
/// preserving the active physical cursor position (unlike `CSI r` / `CSI ?6l` alone, which home).
fn apply_decstr(parser: &mut vt100::Parser) {
    let (_, scr_cols) = parser.screen().size();
    let (row, col) = parser.screen().cursor_position();
    let clamped_col = col.min(scr_cols.get().saturating_sub(1));
    parser.process(b"\x1b[0m\x1b[?1l\x1b[?6l\x1b[r\x1b[?25h\x1b>\x1b7");
    let restore = format!(
        "\x1b[{};{}H",
        row.saturating_add(1),
        clamped_col.saturating_add(1)
    );
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
    let search_start = buf.len().saturating_sub(512);
    if let Some(rel) = buf[search_start..].iter().rposition(|&b| b == 0x1b) {
        let esc_idx = search_start + rel;
        let tail = &buf[esc_idx + 1..];
        if tail.is_empty() {
            return 1;
        }
        if tail[0] == b'[' {
            let mut p = 1;
            while p < tail.len() && (0x30..=0x3F).contains(&tail[p]) {
                p += 1;
            }
            while p < tail.len() && (0x20..=0x2F).contains(&tail[p]) {
                p += 1;
            }
            if p == tail.len() {
                return buf.len() - esc_idx;
            }
        } else if tail.len() <= 16 && tail.iter().all(|&b| (0x20..=0x2F).contains(&b)) {
            return buf.len() - esc_idx;
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

fn is_ascii_keycap_base(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'#' | b'*')
}

fn is_emoji_symbol_width1(ch: char) -> bool {
    !ch.is_ascii()
        && ch >= '\u{2000}'
        && !matches!(ch, '\u{1F1E6}'..='\u{1F1FF}')
        && UnicodeWidthChar::width(ch) == Some(1)
}

fn is_skin_tone_modifier(ch: char) -> bool {
    matches!(ch, '\u{1F3FB}'..='\u{1F3FF}')
}

fn has_special_grapheme_bytes(slice: &[u8]) -> bool {
    slice.windows(3).any(|w| {
        w == b"\xef\xb8\x8f" // U+FE0F (VS16)
            || w == b"\xe2\x80\x8d" // U+200D (ZWJ)
            || w == b"\xf0\x9f\x8f" // U+1F3FB..=U+1F3FF lead bytes
    })
}

fn sanitize_vt_stream<'a>(
    payload: &'a [u8],
    utf8_tail: &mut Vec<u8>,
    ctrl_str: &mut Option<ControlStringKind>,
    grapheme: &mut GraphemeState,
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
    if ctrl_str.is_none()
        && !grapheme.after_zwj
        && !grapheme.suppress_zero_width
        && !has_esc
        && !has_special_grapheme_bytes(slice)
    {
        let last_ch = last_utf8_char(slice);
        if last_ch.and_then(UnicodeWidthChar::width) != Some(0) {
            *grapheme = last_ch.map_or_else(GraphemeState::default, GraphemeState::from_char);
            return match combined {
                Cow::Borrowed(b) => Cow::Borrowed(&b[..valid_len]),
                Cow::Owned(mut v) => {
                    v.truncate(valid_len);
                    Cow::Owned(v)
                }
            };
        }
    }

    let mut out = Vec::with_capacity(slice.len());
    let mut i = 0;
    while i < slice.len() {
        if let Some(kind) = *ctrl_str {
            let b = slice[i];
            if (kind == ControlStringKind::Osc && b == 0x07) || b == 0x18 || b == 0x1a {
                out.push(b);
                *ctrl_str = None;
                i += 1;
                continue;
            }
            if b == 0x1b {
                *ctrl_str = None;
                if i + 1 < slice.len() && slice[i + 1] == b'\\' {
                    out.extend_from_slice(b"\x1b\\");
                    i += 2;
                    continue;
                }
            } else {
                out.push(b);
                i += 1;
                continue;
            }
        }

        if has_esc && slice[i] == 0x1b && i + 1 < slice.len() {
            *grapheme = GraphemeState::default();
            let next = slice[i + 1];
            if next == b']' {
                *ctrl_str = Some(ControlStringKind::Osc);
                out.extend_from_slice(b"\x1b]");
                i += 2;
                continue;
            }
            if matches!(next, b'P' | b'X' | b'^' | b'_') {
                *ctrl_str = Some(ControlStringKind::StOnly);
                out.extend_from_slice(&slice[i..i + 2]);
                i += 2;
                continue;
            }
            if next == b'[' {
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
                    if intermediates.is_empty() {
                        if params == b"?9001" && (final_byte == b'h' || final_byte == b'l') {
                            i = p + 1;
                            continue;
                        }
                        if params.iter().all(|&b| b.is_ascii_digit() || b == b';') {
                            match final_byte {
                                // CHT (`CSI n I` — Cursor Forward Tabulation) -> `n` HT (`\t`) stops
                                b'I' => {
                                    let count = parse_csi_param_default(params, 0, 1).clamp(1, 256);
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
                out.extend_from_slice(&slice[i..p]);
                i = p;
                continue;
            } else {
                let mut e = i + 1;
                while e < slice.len() && (0x20..=0x2F).contains(&slice[e]) {
                    e += 1;
                }
                if e < slice.len() && (0x30..=0x7E).contains(&slice[e]) {
                    if e == i + 1 {
                        match slice[e] {
                            // IND (`ESC D` — Index) -> LF (`\n`)
                            b'D' => {
                                out.push(b'\n');
                                i = e + 1;
                                continue;
                            }
                            // NEL (`ESC E` — Next Line) -> CR + LF (`\r\n`)
                            b'E' => {
                                out.extend_from_slice(b"\r\n");
                                i = e + 1;
                                continue;
                            }
                            _ => {}
                        }
                    }
                    out.extend_from_slice(&slice[i..=e]);
                    i = e + 1;
                    continue;
                }
            }
        }

        if slice[i].is_ascii() {
            let b = slice[i];
            if b < 0x20 || b == 0x7f {
                out.push(b);
                *grapheme = GraphemeState::default();
            } else if grapheme.after_zwj {
                grapheme.after_zwj = false;
                grapheme.suppress_zero_width = true;
            } else {
                out.push(b);
                *grapheme = GraphemeState::from_char(b as char);
            }
            i += 1;
        } else if let Some((ch, ch_len)) = next_utf8_char(&slice[i..]) {
            if ch == '\u{FE0F}' {
                // U+FE0F (Variation Selector-16) requests 2-column emoji presentation for
                // preceding 1-column symbols (such as U+2699 '⚙', U+26A0 '⚠', or keycap bases
                // `[0-9#*]`). Emitting an ASCII space widens 1-column bases to 2 columns
                // identically across `vt100`, Grapheme-Cluster terminals, and SSH clients.
                if grapheme.cluster_cols == 1 {
                    out.push(b' ');
                    grapheme.cluster_cols = 2;
                    grapheme.prev_emoji_symbol = true;
                    grapheme.ascii_keycap_base = false;
                    grapheme.suppress_zero_width = true;
                } else if grapheme.cluster_cols >= 2 {
                    grapheme.suppress_zero_width = true;
                }
            } else if ch == '\u{FE0E}' {
                if !grapheme.suppress_zero_width {
                    out.extend_from_slice(&slice[i..i + ch_len]);
                }
                *grapheme = GraphemeState::default();
            } else if ch == '\u{200D}'
                && (grapheme.cluster_cols >= 2
                    || (grapheme.cluster_cols == 1 && grapheme.prev_emoji_symbol))
            {
                if grapheme.cluster_cols == 1 {
                    out.push(b' ');
                    grapheme.cluster_cols = 2;
                }
                grapheme.after_zwj = true;
                grapheme.suppress_zero_width = true;
            } else if grapheme.after_zwj {
                grapheme.after_zwj = false;
                grapheme.suppress_zero_width = true;
            } else if is_skin_tone_modifier(ch)
                && (grapheme.cluster_cols >= 2
                    || (grapheme.cluster_cols == 1 && grapheme.prev_emoji_symbol))
            {
                if grapheme.cluster_cols == 1 {
                    out.push(b' ');
                    grapheme.cluster_cols = 2;
                }
                grapheme.suppress_zero_width = true;
            } else if UnicodeWidthChar::width(ch) == Some(0) {
                if !grapheme.suppress_zero_width {
                    out.extend_from_slice(&slice[i..i + ch_len]);
                }
            } else {
                out.extend_from_slice(&slice[i..i + ch_len]);
                *grapheme = GraphemeState::from_char(ch);
            }
            i += ch_len;
        } else {
            out.push(slice[i]);
            *grapheme = GraphemeState::default();
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

fn chunk_str_max_bytes(s: &str, max_bytes: usize) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < s.len() {
        let mut end = (start + max_bytes).min(s.len());
        while end > start && !s.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = s[start..]
                .char_indices()
                .nth(1)
                .map_or(s.len(), |(idx, _)| start + idx);
        }
        chunks.push(&s[start..end]);
        start = end;
    }
    chunks
}

fn format_single_osc_sequence(osc_body: &str, in_tmux: bool, in_screen: bool) -> String {
    const DCS_CHUNK_BYTES: usize = 76;
    let osc = format!("\x1b]{osc_body}\x07");
    if in_tmux {
        // Chunk tmux DCS passthrough into <=76-byte (pre-escape) frames and escape every ESC
        // inside each chunk so large OSC 52 payloads never overflow tmux passthrough buffers.
        let chunks = chunk_str_max_bytes(&osc, DCS_CHUNK_BYTES);
        let mut out = String::with_capacity(osc.len() + chunks.len() * 12);
        for chunk in chunks {
            out.push_str("\x1bPtmux;");
            for ch in chunk.chars() {
                if ch == '\x1b' {
                    out.push_str("\x1b\x1b");
                } else {
                    out.push(ch);
                }
            }
            out.push_str("\x1b\\");
        }
        out
    } else if in_screen {
        // GNU screen caps individual DCS passthrough blocks (historically 256 bytes);
        // chunking into <=76-byte DCS frames forwards arbitrarily long OSC payloads intact.
        let chunks = chunk_str_max_bytes(&osc, DCS_CHUNK_BYTES);
        let mut out = String::with_capacity(osc.len() + chunks.len() * 4);
        for chunk in chunks {
            out.push_str("\x1bP");
            out.push_str(chunk);
            out.push_str("\x1b\\");
        }
        out
    } else {
        osc
    }
}

fn format_osc52_sequence(b64: &str, in_tmux: bool, in_screen: bool) -> String {
    if in_tmux {
        let osc = format!("\x1b]52;c;{b64}\x07");
        let dcs = format_single_osc_sequence(&format!("52;c;{b64}"), true, in_screen);
        if osc.len() <= 76 {
            format!("{dcs}{osc}")
        } else {
            dcs
        }
    } else if in_screen {
        format_single_osc_sequence(&format!("52;c;{b64}"), false, true)
    } else {
        format!("\x1b]52;c;{b64}\x07\x1b]52;;{b64}\x07")
    }
}

const OSC_USER_VAR_TERMINAL: &str = "herdr_terminal";
const OSC_USER_VAR_SESSION: &str = "herdr_session";

fn format_osc1337_user_var(key: &str, value: &str, in_tmux: bool, in_screen: bool) -> String {
    let b64 = base64_encode(value.as_bytes());
    format_single_osc_sequence(&format!("1337;SetUserVar={key}={b64}"), in_tmux, in_screen)
}

fn format_herdr_session_clear_osc(in_tmux: bool, in_screen: bool) -> String {
    let mut out = format_osc1337_user_var(OSC_USER_VAR_TERMINAL, "", in_tmux, in_screen);
    out.push_str(&format_osc1337_user_var(
        OSC_USER_VAR_SESSION,
        "",
        in_tmux,
        in_screen,
    ));
    out
}

#[derive(Debug)]
struct OuterSessionReporter {
    session: Option<String>,
    in_tmux: bool,
    in_screen: bool,
    announced_session: bool,
    last_terminal: Option<Option<String>>,
}

impl OuterSessionReporter {
    fn new(session: Option<String>, in_tmux: bool, in_screen: bool) -> Self {
        Self {
            session,
            in_tmux,
            in_screen,
            announced_session: false,
            last_terminal: None,
        }
    }

    fn sync_sequence(&mut self, current_terminal: Option<&str>) -> Option<String> {
        let cur_norm = current_terminal.map(str::trim).filter(|s| !s.is_empty());
        let mut out = String::new();
        if !self.announced_session {
            out.push_str(&format_osc1337_user_var(
                OSC_USER_VAR_SESSION,
                self.session.as_deref().unwrap_or(""),
                self.in_tmux,
                self.in_screen,
            ));
            self.announced_session = true;
        }
        if self.last_terminal.as_ref().map(|o| o.as_deref()) != Some(cur_norm) {
            out.push_str(&format_osc1337_user_var(
                OSC_USER_VAR_TERMINAL,
                cur_norm.unwrap_or(""),
                self.in_tmux,
                self.in_screen,
            ));
            self.last_terminal = Some(cur_norm.map(String::from));
        }
        (!out.is_empty()).then_some(out)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn clear_sequence(&mut self) -> String {
        self.announced_session = false;
        self.last_terminal = None;
        format_herdr_session_clear_osc(self.in_tmux, self.in_screen)
    }
}

fn reader_page_step(term_rect: Rect) -> usize {
    let (body_rect, _, _) = reader_body_metrics(term_rect);
    usize::from(body_rect.height).saturating_sub(2).max(1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReaderScrollMove {
    Delta(isize),
    Top,
    Bottom,
    Latest,
}

fn move_reader_scroll(modal: &mut Modal, term_rect: Rect, mv: ReaderScrollMove) -> bool {
    let Modal::Reader {
        ref doc,
        ref mut scroll,
        ..
    } = *modal
    else {
        return false;
    };
    let (latest, max_scroll) = reader_initial_scroll(doc, term_rect);
    let next = match mv {
        ReaderScrollMove::Delta(d) if d < 0 => scroll.saturating_sub(d.unsigned_abs()),
        ReaderScrollMove::Delta(d) => scroll.saturating_add(d as usize).min(max_scroll),
        ReaderScrollMove::Top => 0,
        ReaderScrollMove::Bottom => max_scroll,
        ReaderScrollMove::Latest => latest,
    };
    let changed = next != *scroll;
    *scroll = next;
    changed
}

fn toggle_reader_clean_mode(modal: &mut Modal, term_rect: Rect) -> bool {
    let Modal::Reader {
        ref mut doc,
        ref mut scroll,
        ref raw_text,
        ref source_note,
    } = *modal
    else {
        return false;
    };
    if !doc.is_agent {
        return false;
    }
    let next_clean = !doc.clean_mode;
    *doc = crate::reader::build_reader_doc_from_text(
        raw_text,
        &doc.title,
        true,
        next_clean,
        source_note.clone(),
    );
    (*scroll, _) = reader_initial_scroll(doc, term_rect);
    true
}

fn handle_reader_mouse_hit(target: HitTarget, modal: &mut Modal, term_rect: Rect) -> (bool, bool) {
    let step = reader_page_step(term_rect) as isize;
    let moved = match target {
        HitTarget::ReaderPageUp => Some(move_reader_scroll(
            modal,
            term_rect,
            ReaderScrollMove::Delta(-step),
        )),
        HitTarget::ReaderPageDown => Some(move_reader_scroll(
            modal,
            term_rect,
            ReaderScrollMove::Delta(step),
        )),
        HitTarget::ReaderJumpLatest => Some(move_reader_scroll(
            modal,
            term_rect,
            ReaderScrollMove::Latest,
        )),
        HitTarget::ReaderToggleClean => {
            return (true, toggle_reader_clean_mode(modal, term_rect));
        }
        _ => None,
    };
    if let Some(changed) = moved {
        (true, changed)
    } else {
        (false, false)
    }
}

fn handle_reader_wheel(up: bool, modal: &mut Modal, term_rect: Rect, eink_mode: bool) -> bool {
    let magnitude = if eink_mode {
        reader_page_step(term_rect) as isize
    } else {
        3
    };
    let delta = if up { -magnitude } else { magnitude };
    move_reader_scroll(modal, term_rect, ReaderScrollMove::Delta(delta))
}

fn handle_reader_drag(
    reader_drag: &mut Option<u16>,
    row: u16,
    modal: &mut Modal,
    term_rect: Rect,
    eink_mode: bool,
) -> bool {
    let Some(last_row) = *reader_drag else {
        return false;
    };
    if !matches!(modal, Modal::Reader { .. }) {
        *reader_drag = None;
        return false;
    }
    let dy = row as i32 - last_row as i32;
    let threshold = if eink_mode { 2 } else { 1 };
    if dy.abs() < threshold {
        return false;
    }
    // On E-Ink, treat one vertical drag gesture as a single page flip to avoid multi-page skips.
    if eink_mode {
        *reader_drag = None;
    } else {
        *reader_drag = Some(row);
    }
    let delta = if eink_mode {
        let step = reader_page_step(term_rect) as isize;
        if dy < 0 {
            step
        } else {
            -step
        }
    } else {
        (-dy) as isize
    };
    move_reader_scroll(modal, term_rect, ReaderScrollMove::Delta(delta))
}

fn clamp_reader_scroll_on_resize(modal: &mut Modal, term_rect: Rect) {
    if let Modal::Reader {
        ref doc,
        ref mut scroll,
        ..
    } = *modal
    {
        let (_, max_scroll) = reader_initial_scroll(doc, term_rect);
        *scroll = (*scroll).min(max_scroll);
    }
}

type ReaderScope = (Option<String>, Option<String>, usize);

fn current_reader_scope(world: &World) -> ReaderScope {
    (
        world.active_terminal_address(),
        world.active_space().map(|s| s.id.clone()),
        world.active_tab_index(),
    )
}

const READER_RPC_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingReaderRpc {
    request_id: u64,
    terminal: String,
    user_scrolled: bool,
    deadline: Instant,
}

fn set_reader_user_scrolled(pending_rpc: &mut Option<PendingReaderRpc>, scrolled: bool) {
    if let Some(ref mut rpc) = *pending_rpc {
        rpc.user_scrolled = scrolled;
    }
}

fn set_pending_reader_rpc(
    core: &CoreClient,
    pending_rpc: &mut Option<PendingReaderRpc>,
    next: Option<PendingReaderRpc>,
) {
    if let Some(old) = pending_rpc.take() {
        core.cancel_terminal_read(old.request_id);
    }
    *pending_rpc = next;
}

fn clear_reader_syncing_badge(modal: &mut Modal) -> bool {
    if let Modal::Reader {
        ref mut doc,
        ref mut source_note,
        ..
    } = *modal
    {
        if source_note.as_deref() == Some("syncing…") {
            *source_note = None;
            doc.source_note = None;
            return true;
        }
    }
    false
}

fn expire_pending_reader_rpc(
    now: Instant,
    core: &CoreClient,
    world: &World,
    modal: &mut Modal,
    pending_rpc: &mut Option<PendingReaderRpc>,
    dirty: &mut bool,
) {
    if pending_rpc.as_ref().is_none_or(|rpc| now < rpc.deadline) {
        return;
    }
    let expired = pending_rpc.take().expect("checked above");
    core.cancel_terminal_read(expired.request_id);
    if world.active_terminal_address().as_deref() != Some(expired.terminal.as_str()) {
        return;
    }
    if clear_reader_syncing_badge(modal) {
        *dirty = true;
    }
}

fn sync_reader_modal_scope(
    core: &CoreClient,
    modal: &mut Modal,
    world: &World,
    reader_open_scope: &mut Option<ReaderScope>,
    reader_drag: &mut Option<u16>,
    pending_reader_rpc: &mut Option<PendingReaderRpc>,
) -> bool {
    if !matches!(modal, Modal::Reader { .. }) {
        *reader_open_scope = None;
        *reader_drag = None;
        set_pending_reader_rpc(core, pending_reader_rpc, None);
        return false;
    }
    let cur = current_reader_scope(world);
    let missing_term = cur
        .0
        .as_ref()
        .is_some_and(|addr| !world.terminals.contains_key(addr));
    match *reader_open_scope {
        None if !missing_term => {
            *reader_open_scope = Some(cur);
            false
        }
        Some(ref opened) if opened == &cur && !missing_term => false,
        _ => {
            *modal = Modal::None;
            *reader_open_scope = None;
            *reader_drag = None;
            set_pending_reader_rpc(core, pending_reader_rpc, None);
            true
        }
    }
}

fn format_read_source_note(res: &crate::core::TerminalReadResult) -> Option<String> {
    let why = res.why.as_deref().map(str::trim).filter(|w| !w.is_empty());
    let prefix = if res.how == "screen" {
        Some("screen")
    } else if !res.complete {
        Some("partial")
    } else {
        None
    };
    match (prefix, why) {
        (Some(p), Some(w)) => Some(format!("{p} · {w}")),
        (Some(p), None) => Some(p.to_string()),
        (None, Some(w)) => Some(w.to_string()),
        (None, None) => None,
    }
}

fn reader_line_budget(is_agent: bool) -> usize {
    if is_agent {
        450
    } else {
        crate::reader::SHELL_MAX_CLEAN_LINES
    }
}

fn reader_tab_meta(world: &World, addr: Option<&str>) -> (String, bool) {
    let is_agent = addr
        .and_then(|a| world.terminals.get(a))
        .is_some_and(crate::model::TerminalInfo::is_agent);
    let title = world
        .active_space()
        .map(|s| world.tab_display_name(s, world.active_tab_index()))
        .unwrap_or_else(|| addr.unwrap_or("reader").to_string());
    (title, is_agent)
}

fn build_reader_modal_state(
    core: &CoreClient,
    world: &World,
    attached: Option<&AttachedTerminal>,
    term_rect: Rect,
    clean_mode: bool,
    pending_reader_rpc: Option<&mut Option<PendingReaderRpc>>,
) -> Modal {
    let active_addr = world.active_terminal_address();
    let (tab_title, is_agent) = reader_tab_meta(world, active_addr.as_deref());
    let max_rows = if is_agent {
        crate::reader::AGENT_MAX_SCAN_ROWS
    } else {
        crate::reader::SHELL_MAX_SCAN_ROWS
    };
    let raw_text = attached
        .map(|att| {
            let mut scr = att.parser.screen().clone();
            crate::reader::extract_plain_text_from_screen(&mut scr, max_rows)
        })
        .unwrap_or_default();

    let dispatched_id = active_addr
        .as_deref()
        .and_then(|addr| core.request_terminal_read(addr, reader_line_budget(is_agent)));
    if let Some(slot) = pending_reader_rpc {
        let next = dispatched_id
            .zip(active_addr)
            .map(|(request_id, terminal)| PendingReaderRpc {
                request_id,
                terminal,
                user_scrolled: false,
                deadline: Instant::now() + READER_RPC_TIMEOUT,
            });
        set_pending_reader_rpc(core, slot, next);
    }
    let source_note = dispatched_id.map(|_| "syncing…".to_string());
    let doc = crate::reader::build_reader_doc_from_text(
        &raw_text,
        &tab_title,
        is_agent,
        clean_mode,
        source_note.clone(),
    );
    let (scroll, _) = reader_initial_scroll(&doc, term_rect);
    Modal::Reader {
        doc,
        scroll,
        raw_text,
        source_note,
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_terminal_read_message(
    request_id: u64,
    terminal: &str,
    result: std::result::Result<crate::core::TerminalReadResult, String>,
    world: &World,
    modal: &mut Modal,
    term_rect: Rect,
    pending_reader_rpc: &mut Option<PendingReaderRpc>,
    dirty: &mut bool,
) {
    let matches_pending = pending_reader_rpc
        .as_ref()
        .is_some_and(|p| p.request_id == request_id && p.terminal == terminal);
    if !matches_pending {
        return;
    }
    let pending = pending_reader_rpc.take().expect("checked above");
    if world.active_terminal_address().as_deref() != Some(terminal) {
        return;
    }

    let Modal::Reader {
        ref mut doc,
        ref mut scroll,
        ref mut raw_text,
        ref mut source_note,
    } = *modal
    else {
        return;
    };
    match result {
        Ok(res) => {
            let clean_mode = doc.clean_mode;
            let (tab_title, is_agent) = reader_tab_meta(world, Some(terminal));
            let note = format_read_source_note(&res);
            *doc = crate::reader::build_reader_doc_from_text(
                &res.text,
                &tab_title,
                is_agent,
                clean_mode,
                note.clone(),
            );
            *raw_text = res.text;
            *source_note = note;
            let (initial, max_scroll) = reader_initial_scroll(doc, term_rect);
            if pending.user_scrolled {
                *scroll = (*scroll).min(max_scroll);
            } else {
                *scroll = initial;
            }
            *dirty = true;
        }
        Err(_) => {
            if clear_reader_syncing_badge(modal) {
                *dirty = true;
            }
        }
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
    let (in_tmux, in_screen) = detect_multiplexer_env();
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
    } else if let Modal::TabOverview {
        query,
        selected,
        selected_target,
    } = modal
    {
        let sanitized: String = text.chars().filter(|&c| c != '\r' && c != '\n').collect();
        if !sanitized.is_empty() {
            query.push_str(&sanitized);
            let items = collect_overview_items(world, query);
            (*selected, *selected_target) = overview_selection_after_query_change(&items, query);
        }
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
        if let Some(nested) = detect_nested_multiplexer(read_env_lossy) {
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
    let (eink_mode, eink_color) = resolve_eink_and_color_modes(
        cli.eink,
        cli.no_eink,
        cli.eink_color,
        cli.no_eink_color,
        file_cfg.ui.eink,
        file_cfg.ui.eink_color,
        read_env_lossy,
    );
    let eink_interval = resolve_eink_interval(cli.eink_interval_ms, file_cfg.ui.eink_interval_ms);
    let (in_tmux, in_screen) = detect_multiplexer_env();
    let herdr_session = resolve_herdr_session(cli.session.as_deref(), read_env_lossy);
    let connect_session = connect_session_arg(cli.session.as_deref(), herdr_session.as_deref());

    let (app_tx, app_rx) = mpsc::channel::<AppEvent>();
    let core = CoreClient::connect_with_sender(connect_session, app_tx.clone(), AppEvent::Core)?;
    let snapshot = core.subscribe_events()?;

    let mut world = World::default();
    world.load_initial(&core, &snapshot)?;

    // Setup terminal with an immediate RAII guard so any setup or runtime error
    // restores raw mode, alternate screen, mouse capture, SGR state, and cursor visibility.
    enable_raw_mode()?;
    let _cleanup_guard = TerminalCleanupGuard { in_tmux, in_screen };
    let mut session_reporter = OuterSessionReporter::new(herdr_session, in_tmux, in_screen);
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
        eink_mode,
        eink_color,
        eink_interval,
        &mut session_reporter,
    );

    // Disable mouse capture, bracketed paste, and alternate screen BEFORE disabling raw mode
    // so late mouse/terminal packets never echo onto the outer shell prompt.
    input_running.store(false, Ordering::SeqCst);
    let leave_ok =
        write_terminal_leave_sequence_with_mux(terminal.backend_mut(), in_tmux, in_screen).is_ok();
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
    mark_seen: bool,
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
                cur.resize(cols, rows);
                core.resize_channel(cur.channel, cols, rows);
            }
            if active_addr.as_ref() != Some(addr) {
                core.set_focus(cur.channel, true);
                if mark_seen {
                    world.mark_active_seen(core);
                }
            }
            resolved_active = Some(addr.clone());
        } else if let Ok(ch) = core.attach_terminal(addr, cols, rows, ATTACH_HISTORY_LINES) {
            core.set_focus(ch, true);
            attached_map.insert(addr.clone(), AttachedTerminal::new(ch, cols, rows));
            if mark_seen {
                world.mark_active_seen(core);
            }
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
                        sib.resize(cols, rows);
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
fn execute_action_full(
    action: Action,
    core: &CoreClient,
    world: &mut World,
    attached: Option<&AttachedTerminal>,
    prefix_spec: &str,
    _sidebar_open: &mut bool,
    scrollback_offset: &mut usize,
    modal: &mut Modal,
    term_rect: Rect,
    reader_rect: Rect,
    eink_mode: &mut bool,
    eink_color: &mut bool,
    force_clear: &mut bool,
    pending_reader_rpc: Option<&mut Option<PendingReaderRpc>>,
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
            step_linear_tab_or_overview(HorizontalSwipeDir::Next, core, world, modal);
        }
        Action::PrevTab => {
            step_linear_tab_or_overview(HorizontalSwipeDir::Prev, core, world, modal);
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
        Action::ToggleEink => {
            *eink_mode = !*eink_mode;
            *force_clear = true;
        }
        Action::ToggleEinkColor => {
            if !*eink_mode {
                *eink_mode = true;
                *eink_color = true;
            } else {
                *eink_color = !*eink_color;
            }
            *force_clear = true;
        }
        Action::RedrawScreen => {
            *force_clear = true;
        }
        Action::OpenBookReader => {
            if matches!(modal, Modal::Reader { .. }) {
                *modal = Modal::None;
                if let Some(slot) = pending_reader_rpc {
                    set_pending_reader_rpc(core, slot, None);
                }
            } else {
                *modal = build_reader_modal_state(
                    core,
                    world,
                    attached,
                    reader_rect,
                    true,
                    pending_reader_rpc,
                );
            }
        }
        Action::PageUp => {
            if matches!(modal, Modal::Reader { .. }) {
                let step = reader_page_step(reader_rect) as isize;
                if move_reader_scroll(modal, reader_rect, ReaderScrollMove::Delta(-step)) {
                    if let Some(slot) = pending_reader_rpc {
                        set_reader_user_scrolled(slot, true);
                    }
                }
            } else if attached.is_some_and(|a| a.parser.screen().alternate_screen())
                && *scrollback_offset == 0
            {
                if let Some(att) = attached {
                    core.send_input(att.channel, b"\x1b[5~");
                }
            } else {
                let step = (term_rect.height as usize).saturating_sub(2).max(1);
                *scrollback_offset = scrollback_offset.saturating_add(step);
            }
        }
        Action::PageDown => {
            if matches!(modal, Modal::Reader { .. }) {
                let step = reader_page_step(reader_rect) as isize;
                if move_reader_scroll(modal, reader_rect, ReaderScrollMove::Delta(step)) {
                    if let Some(slot) = pending_reader_rpc {
                        set_reader_user_scrolled(slot, true);
                    }
                }
            } else if attached.is_some_and(|a| a.parser.screen().alternate_screen())
                && *scrollback_offset == 0
            {
                if let Some(att) = attached {
                    core.send_input(att.channel, b"\x1b[6~");
                }
            } else {
                let step = (term_rect.height as usize).saturating_sub(2).max(1);
                *scrollback_offset = scrollback_offset.saturating_sub(step);
            }
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

const PAGE_TAP_SLOP_CELLS: u16 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingPageTurnTap {
    dir: HorizontalSwipeDir,
    start_col: u16,
    start_row: u16,
}

fn match_eink_page_touch_zone(
    left_zone: Option<Rect>,
    right_zone: Option<Rect>,
    col: u16,
    row: u16,
) -> Option<HorizontalSwipeDir> {
    if is_in_rect(left_zone, col, row) {
        Some(HorizontalSwipeDir::Prev)
    } else if is_in_rect(right_zone, col, row) {
        Some(HorizontalSwipeDir::Next)
    } else {
        None
    }
}

fn page_tap_within_slop(tap: &PendingPageTurnTap, col: u16, row: u16) -> bool {
    col.abs_diff(tap.start_col) <= PAGE_TAP_SLOP_CELLS
        && row.abs_diff(tap.start_row) <= PAGE_TAP_SLOP_CELLS
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HorizontalSwipeDir {
    Prev,
    Next,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WheelZone {
    Viewport,
    TopBar,
    SpaceMenu,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HorizBurstScope {
    /// Suppressed by a blocking modal (`Help`, `Fuzzy`, etc.) or active drag (`dpad_drag` /
    /// `selection.active`); coalesces trailing horizontal inertia across modal/drag dismissal.
    Global,
    /// Accepted tab switch in `Modal::None` (`Viewport` or `TopBar`); coalesces same-direction
    /// horizontal inertia across `Viewport <-> TopBar` without blocking `Modal::SpaceMenu`.
    TabSwitch,
    /// Accepted space switch in `Modal::SpaceMenu`; coalesces same-direction horizontal inertia
    /// inside `SpaceMenu` without blocking explicit tab swipes after closing `SpaceMenu`.
    SpaceSwitch,
    /// Zone-scoped vertical-to-horizontal diagonal bleed suppression (e.g. `Viewport` bleed never
    /// blocks explicit Row 0 `TopBar` horizontal wheel scrolling).
    BleedInZone(WheelZone),
}

impl HorizBurstScope {
    fn accepted_for_zone(zone: WheelZone) -> Self {
        match zone {
            WheelZone::Viewport | WheelZone::TopBar => Self::TabSwitch,
            WheelZone::SpaceMenu => Self::SpaceSwitch,
        }
    }

    fn matches_zone(self, zone: WheelZone) -> bool {
        match self {
            Self::Global => true,
            Self::TabSwitch => matches!(zone, WheelZone::Viewport | WheelZone::TopBar),
            Self::SpaceSwitch => zone == WheelZone::SpaceMenu,
            Self::BleedInZone(WheelZone::Viewport) => {
                matches!(zone, WheelZone::Viewport | WheelZone::TopBar)
            }
            Self::BleedInZone(z) => z == zone,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VertBurstScope {
    Global,
    Zone(WheelZone),
}

impl VertBurstScope {
    fn matches_zone(self, zone: WheelZone) -> bool {
        match self {
            Self::Global => true,
            Self::Zone(z) => z == zone,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct HorizontalWheelGestureState {
    last_viewport_vertical_wheel_at: Option<Instant>,
    last_top_bar_vertical_wheel_at: Option<Instant>,
    last_space_menu_vertical_wheel_at: Option<Instant>,
    last_horiz_dir: Option<HorizontalSwipeDir>,
    last_horiz_event_at: Option<Instant>,
    last_horiz_scope: Option<HorizBurstScope>,
    /// UI zone of the currently active accepted horizontal switch burst, if any.
    active_horiz_switch_zone: Option<WheelZone>,
    last_rejected_vert_scope: Option<VertBurstScope>,
    last_rejected_vert_dir: Option<bool>,
    last_rejected_vert_at: Option<Instant>,
    pending_horiz_scope: Option<HorizBurstScope>,
    pending_horiz_dir: Option<HorizontalSwipeDir>,
    pending_horiz_count: u8,
    pending_horiz_saw_viewport: bool,
    pending_horiz_last_at: Option<Instant>,
}

impl HorizontalWheelGestureState {
    fn clear_pending_horiz(&mut self) {
        self.pending_horiz_scope = None;
        self.pending_horiz_dir = None;
        self.pending_horiz_count = 0;
        self.pending_horiz_saw_viewport = false;
        self.pending_horiz_last_at = None;
    }

    fn clear_pending_horiz_for_zone(&mut self, zone: WheelZone) {
        if self
            .pending_horiz_scope
            .is_some_and(|s| s.matches_zone(zone))
        {
            self.clear_pending_horiz();
        }
    }

    fn record_viewport_vertical_wheel(&mut self, now: Instant) {
        self.clear_pending_horiz_for_zone(WheelZone::Viewport);
        self.last_viewport_vertical_wheel_at = Some(now);
    }

    fn set_zone_vertical_wheel(&mut self, zone: WheelZone, now: Instant) {
        match zone {
            WheelZone::Viewport => self.last_viewport_vertical_wheel_at = Some(now),
            WheelZone::TopBar => self.last_top_bar_vertical_wheel_at = Some(now),
            WheelZone::SpaceMenu => self.last_space_menu_vertical_wheel_at = Some(now),
        }
    }

    fn zone_vertical_wheel_at(&self, zone: WheelZone) -> Option<Instant> {
        match zone {
            WheelZone::Viewport => self.last_viewport_vertical_wheel_at,
            WheelZone::TopBar => self.last_top_bar_vertical_wheel_at,
            WheelZone::SpaceMenu => self.last_space_menu_vertical_wheel_at,
        }
    }

    fn is_zone_locked_by_vertical_scroll(&self, zone: WheelZone, now: Instant) -> bool {
        let within_lock = |ts: Option<Instant>| {
            ts.is_some_and(|t| now.saturating_duration_since(t) < VERT_SCROLL_LOCK_WINDOW)
        };
        if within_lock(self.zone_vertical_wheel_at(zone)) {
            return true;
        }
        matches!(zone, WheelZone::Viewport | WheelZone::TopBar)
            && (within_lock(self.last_viewport_vertical_wheel_at)
                || within_lock(self.last_top_bar_vertical_wheel_at))
    }

    fn record_suppressed_vertical_wheel(&mut self, zone: WheelZone, scroll_up: bool, now: Instant) {
        self.clear_pending_horiz();
        self.set_zone_vertical_wheel(zone, now);
        self.last_rejected_vert_scope = Some(VertBurstScope::Global);
        self.last_rejected_vert_dir = Some(scroll_up);
        self.last_rejected_vert_at = Some(now);
    }

    fn record_suppressed_horizontal_wheel(&mut self, dir: HorizontalSwipeDir, now: Instant) {
        self.clear_pending_horiz();
        let continuing_same_burst = self.last_horiz_dir == Some(dir)
            && self
                .last_horiz_event_at
                .is_some_and(|t| now.saturating_duration_since(t) < HORIZ_WHEEL_QUIET_GAP);
        if !continuing_same_burst {
            self.active_horiz_switch_zone = None;
        }
        self.last_horiz_dir = Some(dir);
        self.last_horiz_event_at = Some(now);
        self.last_horiz_scope = Some(HorizBurstScope::Global);
    }

    /// Guards `TopBar` / `Modal::SpaceMenu` vertical wheel switches when an accepted horizontal
    /// wheel burst in the *same* `WheelZone` is active within `HORIZ_WHEEL_QUIET_GAP` (coalescing
    /// any continuous rejected/suppressed vertical stream across `HORIZ_WHEEL_QUIET_GAP`), and locks
    /// `TopBar` vertical switches within `VERT_SCROLL_LOCK_WINDOW` of a `Viewport` vertical
    /// scroll so upward swipes drifting onto Row 0 never accidentally switch tabs.
    fn accept_vertical_switch_wheel(
        &mut self,
        zone: WheelZone,
        scroll_up: bool,
        now: Instant,
    ) -> bool {
        self.clear_pending_horiz_for_zone(zone);
        let continuing_rejected_vert_burst = self
            .last_rejected_vert_scope
            .is_some_and(|s| s.matches_zone(zone))
            && self.last_rejected_vert_dir == Some(scroll_up)
            && self
                .last_rejected_vert_at
                .is_some_and(|t| now.saturating_duration_since(t) < HORIZ_WHEEL_QUIET_GAP);
        let in_horiz_bleed_guard = self.active_horiz_switch_zone == Some(zone)
            && self
                .last_horiz_event_at
                .is_some_and(|t| now.saturating_duration_since(t) < HORIZ_WHEEL_QUIET_GAP);
        let in_viewport_vert_lock = zone == WheelZone::TopBar
            && self
                .last_viewport_vertical_wheel_at
                .is_some_and(|t| now.saturating_duration_since(t) < VERT_SCROLL_LOCK_WINDOW);
        if continuing_rejected_vert_burst || in_horiz_bleed_guard || in_viewport_vert_lock {
            let next_scope = if continuing_rejected_vert_burst
                && self.last_rejected_vert_scope == Some(VertBurstScope::Global)
            {
                VertBurstScope::Global
            } else {
                VertBurstScope::Zone(zone)
            };
            self.last_rejected_vert_scope = Some(next_scope);
            self.last_rejected_vert_dir = Some(scroll_up);
            self.last_rejected_vert_at = Some(now);
            if self.active_horiz_switch_zone != Some(zone) {
                self.set_zone_vertical_wheel(zone, now);
                if in_viewport_vert_lock {
                    self.last_viewport_vertical_wheel_at = Some(now);
                }
            }
            return false;
        }
        self.last_rejected_vert_scope = None;
        self.last_rejected_vert_dir = None;
        self.last_rejected_vert_at = None;
        self.set_zone_vertical_wheel(zone, now);
        true
    }

    fn accept_horizontal_wheel(
        &mut self,
        dir: HorizontalSwipeDir,
        zone: WheelZone,
        now: Instant,
    ) -> bool {
        let within_horiz_quiet_gap = self
            .last_horiz_event_at
            .is_some_and(|t| now.saturating_duration_since(t) < HORIZ_WHEEL_QUIET_GAP);
        if within_horiz_quiet_gap {
            match self.last_horiz_scope {
                Some(HorizBurstScope::Global) => {
                    self.clear_pending_horiz();
                    self.last_horiz_dir = Some(dir);
                    self.last_horiz_event_at = Some(now);
                    return false;
                }
                Some(scope @ HorizBurstScope::BleedInZone(_)) if scope.matches_zone(zone) => {
                    self.clear_pending_horiz();
                    self.last_horiz_dir = Some(dir);
                    self.last_horiz_event_at = Some(now);
                    self.active_horiz_switch_zone = None;
                    return false;
                }
                Some(scope @ (HorizBurstScope::TabSwitch | HorizBurstScope::SpaceSwitch))
                    if scope.matches_zone(zone) && self.last_horiz_dir == Some(dir) =>
                {
                    self.clear_pending_horiz();
                    self.last_horiz_event_at = Some(now);
                    if self.active_horiz_switch_zone.is_some() {
                        self.active_horiz_switch_zone = Some(zone);
                    }
                    return false;
                }
                _ => {}
            }
        }

        if self.is_zone_locked_by_vertical_scroll(zone, now) {
            self.clear_pending_horiz();
            self.last_horiz_dir = Some(dir);
            self.last_horiz_event_at = Some(now);
            let bleed_zone = if matches!(zone, WheelZone::Viewport | WheelZone::TopBar)
                && self
                    .last_viewport_vertical_wheel_at
                    .is_some_and(|t| now.saturating_duration_since(t) < VERT_SCROLL_LOCK_WINDOW)
            {
                WheelZone::Viewport
            } else {
                zone
            };
            self.last_horiz_scope = Some(HorizBurstScope::BleedInZone(bleed_zone));
            self.active_horiz_switch_zone = None;
            return false;
        }

        let target_scope = HorizBurstScope::accepted_for_zone(zone);
        if self.pending_horiz_scope == Some(target_scope)
            && self.pending_horiz_dir == Some(dir)
            && self
                .pending_horiz_last_at
                .is_some_and(|t| now.saturating_duration_since(t) < HORIZ_SWIPE_ACCUM_WINDOW)
        {
            self.pending_horiz_count = self.pending_horiz_count.saturating_add(1);
            self.pending_horiz_saw_viewport |= zone == WheelZone::Viewport;
            self.pending_horiz_last_at = Some(now);
        } else {
            self.pending_horiz_scope = Some(target_scope);
            self.pending_horiz_dir = Some(dir);
            self.pending_horiz_count = 1;
            self.pending_horiz_saw_viewport = zone == WheelZone::Viewport;
            self.pending_horiz_last_at = Some(now);
        }

        let required = match zone {
            WheelZone::SpaceMenu => SPACE_MENU_HORIZ_SWIPE_THRESHOLD,
            WheelZone::Viewport => VIEWPORT_HORIZ_SWIPE_THRESHOLD,
            WheelZone::TopBar => {
                if self.pending_horiz_saw_viewport {
                    VIEWPORT_HORIZ_SWIPE_THRESHOLD
                } else {
                    TOP_BAR_HORIZ_SWIPE_THRESHOLD
                }
            }
        };
        if self.pending_horiz_count < required {
            return false;
        }

        self.clear_pending_horiz();
        self.last_horiz_dir = Some(dir);
        self.last_horiz_event_at = Some(now);
        self.last_horiz_scope = Some(target_scope);
        self.active_horiz_switch_zone = Some(zone);
        true
    }
}

#[allow(clippy::too_many_arguments)]
fn try_activate_space_slop_tap(
    was_dragged: bool,
    anchor_col: u16,
    anchor_row: u16,
    up_col: u16,
    up_row: u16,
    space_slop_rect: Option<Rect>,
    modal: &mut Modal,
    prefix_active: &mut bool,
    ctrl_active: &mut bool,
    alt_active: &mut bool,
) -> bool {
    if was_dragged
        || anchor_col != up_col
        || anchor_row != up_row
        || !matches!(*modal, Modal::None)
        || !is_in_rect(space_slop_rect, anchor_col, anchor_row)
    {
        return false;
    }
    *prefix_active = false;
    *ctrl_active = false;
    *alt_active = false;
    *modal = Modal::SpaceMenu;
    true
}

fn exit_tab_overview_to_last_tab(core: &CoreClient, world: &mut World, modal: &mut Modal) {
    *modal = Modal::None;
    if let Some(len) = world
        .active_space()
        .map(|s| s.tabs.len())
        .filter(|&n| n > 0)
    {
        world.select_tab_index(len - 1);
        world.mark_active_seen(core);
    }
}

fn step_linear_tab_or_overview(
    dir: HorizontalSwipeDir,
    core: &CoreClient,
    world: &mut World,
    modal: &mut Modal,
) -> bool {
    if matches!(modal, Modal::TabOverview { .. }) {
        return match dir {
            HorizontalSwipeDir::Prev => {
                exit_tab_overview_to_last_tab(core, world, modal);
                true
            }
            HorizontalSwipeDir::Next => false,
        };
    }
    let tab_len = world.active_space().map_or(0, |s| s.tabs.len());
    match dir {
        HorizontalSwipeDir::Prev => {
            let cur = world.active_tab_index();
            if tab_len == 0 || cur == 0 {
                false
            } else {
                world.select_tab_index(cur - 1);
                world.mark_active_seen(core);
                true
            }
        }
        HorizontalSwipeDir::Next => {
            if tab_len == 0 {
                *modal = Modal::default_tab_overview();
                return true;
            }
            let cur = world.active_tab_index();
            if cur + 1 < tab_len {
                world.select_tab_index(cur + 1);
                world.mark_active_seen(core);
            } else {
                *modal = Modal::default_tab_overview();
            }
            true
        }
    }
}

fn scroll_tab_overview_selection(world: &World, modal: &mut Modal, scroll_up: bool) -> bool {
    let Modal::TabOverview {
        ref query,
        ref mut selected,
        ref mut selected_target,
    } = *modal
    else {
        return false;
    };
    let items = collect_overview_items(world, query);
    let cur_sel = resolve_overview_selection(&items, *selected, selected_target);
    let next_idx = if scroll_up {
        cur_sel.saturating_sub(1)
    } else {
        (cur_sel + 1).min(items.len().saturating_sub(1))
    };
    let next_sel = overview_selection_for_index(&items, next_idx);
    let changed = (*selected, selected_target.clone()) != next_sel;
    *selected = next_sel.0;
    *selected_target = next_sel.1;
    changed
}

#[allow(clippy::too_many_arguments)]
fn handle_top_bar_or_space_vertical_wheel(
    scroll_up: bool,
    now: Instant,
    horiz_wheel: &mut HorizontalWheelGestureState,
    core: &CoreClient,
    world: &mut World,
    modal: &mut Modal,
    dpad_drag_active: bool,
    prefix_active: &mut bool,
    ctrl_active: &mut bool,
    alt_active: &mut bool,
    selection: &mut Option<TerminalSelection>,
    selection_screen: &mut Option<vt100::Screen>,
    selection_vt_seq: &mut Option<u64>,
) -> bool {
    let zone = if matches!(modal, Modal::SpaceMenu) {
        WheelZone::SpaceMenu
    } else {
        WheelZone::TopBar
    };
    if dpad_drag_active || selection.as_ref().is_some_and(|s| s.active) {
        horiz_wheel.record_suppressed_vertical_wheel(zone, scroll_up, now);
        return false;
    }
    if !horiz_wheel.accept_vertical_switch_wheel(zone, scroll_up, now) {
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
    if matches!(modal, Modal::SpaceMenu) {
        let prev_sid = world.active_space().map(|s| s.id.clone());
        if scroll_up {
            world.prev_space();
        } else {
            world.next_space();
        }
        let space_changed = world.active_space().map(|s| s.id.as_str()) != prev_sid.as_deref();
        if space_changed {
            world.mark_active_seen(core);
        }
        return space_changed || had_sticky || had_vis_sel;
    }
    let dir = if scroll_up {
        HorizontalSwipeDir::Prev
    } else {
        HorizontalSwipeDir::Next
    };
    let changed = step_linear_tab_or_overview(dir, core, world, modal);
    changed || had_sticky || had_vis_sel
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
    modal: &mut Modal,
    dpad_drag_active: bool,
    prefix_active: &mut bool,
    ctrl_active: &mut bool,
    alt_active: &mut bool,
    selection: &mut Option<TerminalSelection>,
    selection_screen: &mut Option<vt100::Screen>,
    selection_vt_seq: &mut Option<u64>,
) -> bool {
    if dpad_drag_active
        || selection.as_ref().is_some_and(|s| s.active)
        || !matches!(
            modal,
            Modal::None | Modal::TabOverview { .. } | Modal::SpaceMenu
        )
    {
        horiz_wheel.record_suppressed_horizontal_wheel(dir, now);
        return false;
    }
    let zone = if matches!(modal, Modal::SpaceMenu) {
        WheelZone::SpaceMenu
    } else if mev_row < term_rect.y {
        WheelZone::TopBar
    } else {
        WheelZone::Viewport
    };
    if !horiz_wheel.accept_horizontal_wheel(dir, zone, now) {
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
    if matches!(modal, Modal::SpaceMenu) {
        let prev_sid = world.active_space().map(|s| s.id.clone());
        match dir {
            HorizontalSwipeDir::Prev => world.prev_space(),
            HorizontalSwipeDir::Next => world.next_space(),
        }
        let space_changed = world.active_space().map(|s| s.id.as_str()) != prev_sid.as_deref();
        if space_changed {
            world.mark_active_seen(core);
        }
        return space_changed || had_sticky || had_vis_sel;
    }
    let changed = step_linear_tab_or_overview(dir, core, world, modal);
    changed || had_sticky || had_vis_sel
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

const EINK_INPUT_BYPASS_WINDOW: Duration = Duration::from_millis(200);
const EINK_INPUT_FOLLOWUP_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct EinkInputPacer {
    last_drawn_input_seq: u64,
    pty_dirty_interactive: bool,
    pending_echo_seq: Option<u64>,
}

impl EinkInputPacer {
    fn record_active_pty_data(
        &mut self,
        channel: u32,
        now: Instant,
        last_input: Option<crate::core::LastPtyInput>,
    ) {
        if let Some(inp) = last_input {
            if inp.channel == channel
                && now.saturating_duration_since(inp.at) <= EINK_INPUT_BYPASS_WINDOW
            {
                self.pty_dirty_interactive = true;
                if inp.seq != self.last_drawn_input_seq {
                    self.pending_echo_seq = Some(inp.seq);
                }
            }
        }
    }

    fn next_deadline(
        &self,
        pty_dirty: bool,
        eink_mode: bool,
        eink_interval: Duration,
        now: Instant,
        last_draw_at: Option<Instant>,
    ) -> Option<Instant> {
        if !pty_dirty {
            return None;
        }
        if !eink_mode || eink_interval.is_zero() || self.pending_echo_seq.is_some() {
            return Some(now);
        }
        let interval = if self.pty_dirty_interactive {
            EINK_INPUT_FOLLOWUP_INTERVAL.min(eink_interval)
        } else {
            eink_interval
        };
        Some(last_draw_at.map_or(now, |t| t + interval))
    }

    fn clear_zero_diff(&mut self) -> bool {
        let was_interactive = self.pty_dirty_interactive;
        self.pty_dirty_interactive = false;
        self.pending_echo_seq = None;
        was_interactive
    }

    fn on_promoted_or_drawn(&mut self) {
        if let Some(seq) = self.pending_echo_seq.take() {
            self.last_drawn_input_seq = seq;
        }
        self.pty_dirty_interactive = false;
    }

    fn reset_on_channel_switch(&mut self) {
        self.pty_dirty_interactive = false;
        self.pending_echo_seq = None;
    }
}

fn detect_eink_agent_settlement(
    prev: Option<&(String, AgentState)>,
    cur: Option<&(String, AgentState)>,
) -> bool {
    matches!(
        (prev, cur),
        (
            Some((prev_addr, AgentState::Working)),
            Some((cur_addr, cur_state)),
        ) if prev_addr == cur_addr && *cur_state != AgentState::Working
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn promote_due_pty_dirty(
    eink_mode: bool,
    eink_interval: Duration,
    now: Instant,
    pty_dirty: &mut bool,
    eink_pacer: &mut EinkInputPacer,
    dirty: &mut bool,
    last_draw_at: &mut Option<Instant>,
    last_eink_snap: &Option<(String, EinkScreenSnapshot)>,
    active_addr: Option<&str>,
    attached_map: &mut BTreeMap<String, AttachedTerminal>,
    term_rect: Rect,
    scrollback_offset: &mut usize,
    modal: &Modal,
    selection: &Option<TerminalSelection>,
) {
    promote_due_pty_dirty_with_color(
        eink_mode,
        false,
        eink_interval,
        now,
        pty_dirty,
        eink_pacer,
        dirty,
        last_draw_at,
        last_eink_snap,
        active_addr,
        attached_map,
        term_rect,
        scrollback_offset,
        modal,
        selection,
    );
}

#[allow(clippy::too_many_arguments)]
fn promote_due_pty_dirty_with_color(
    eink_mode: bool,
    eink_color: bool,
    eink_interval: Duration,
    now: Instant,
    pty_dirty: &mut bool,
    eink_pacer: &mut EinkInputPacer,
    dirty: &mut bool,
    last_draw_at: &mut Option<Instant>,
    last_eink_snap: &Option<(String, EinkScreenSnapshot)>,
    active_addr: Option<&str>,
    attached_map: &mut BTreeMap<String, AttachedTerminal>,
    term_rect: Rect,
    scrollback_offset: &mut usize,
    modal: &Modal,
    selection: &Option<TerminalSelection>,
) {
    if *dirty || matches!(modal, Modal::Reader { .. }) {
        return;
    }
    let Some(deadline) =
        eink_pacer.next_deadline(*pty_dirty, eink_mode, eink_interval, now, *last_draw_at)
    else {
        return;
    };
    if now < deadline {
        return;
    }
    if eink_mode && matches!(modal, Modal::None) && selection.is_none() {
        if let Some(att) = active_addr.and_then(|a| attached_map.get_mut(a)) {
            att.parser.screen_mut().set_scrollback(*scrollback_offset);
            *scrollback_offset = att.parser.screen().scrollback();
            let initial_in_code_fence = att.viewport_initial_code_fence(*scrollback_offset);
            let snap = capture_eink_screen_snapshot_with_color_and_fence(
                att.parser.screen(),
                term_rect,
                *scrollback_offset,
                eink_color,
                initial_in_code_fence,
            );
            if last_eink_snap
                .as_ref()
                .is_some_and(|(a, prev)| Some(a.as_str()) == active_addr && prev == &snap)
            {
                *pty_dirty = false;
                if !eink_pacer.clear_zero_diff() {
                    *last_draw_at = Some(now);
                }
                return;
            }
        }
    }
    *dirty = true;
    eink_pacer.on_promoted_or_drawn();
}

fn capture_selection_snapshot(
    att: &AttachedTerminal,
    compute_fence: bool,
) -> (vt100::Screen, u64, bool) {
    let mut scr = att.parser.screen().clone();
    let sb = scr.scrollback();
    let initial_fence = if compute_fence {
        att.cached_viewport_fence
            .filter(|(seq, cached_sb, cols, rows, _)| {
                (*seq, *cached_sb, *cols, *rows) == (att.vt_seq, sb, att.cols, att.rows)
            })
            .map(|(_, _, _, _, val)| val)
            .unwrap_or_else(|| scan_viewport_initial_code_fence(&mut scr, VIEWPORT_FENCE_LOOKBACK))
    } else {
        false
    };
    (scr, att.vt_seq, initial_fence)
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
    eink_mode: bool,
    eink_color: bool,
    eink_interval: Duration,
    session_reporter: &mut OuterSessionReporter,
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
        eink_mode,
        eink_color,
        eink_interval,
        session_reporter,
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
    mut eink_mode: bool,
    mut eink_color: bool,
    eink_interval: Duration,
    session_reporter: &mut OuterSessionReporter,
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
    let mut last_space_slop_rect: Option<Rect> = None;
    let mut last_page_up_rect: Option<Rect> = None;
    let mut last_page_down_rect: Option<Rect> = None;
    let mut dpad_drag: Option<FloatingDPadDrag> = None;
    let mut page_tap: Option<PendingPageTurnTap> = None;
    let mut horiz_wheel = HorizontalWheelGestureState::default();
    let mut selection: Option<TerminalSelection> = None;
    let mut selection_screen: Option<vt100::Screen> = None;
    let mut selection_vt_seq: Option<u64> = None;
    let mut selection_initial_fence = false;
    let mut clipboard_buffer = String::new();
    let mut copied_until: Option<Instant> = None;
    let mut pending_sibling_resize_at: Option<Instant> = None;
    let mut pending_attach_retry: Option<(String, Instant)> = None;
    let mut last_usage_rev: u64 = 0;
    let mut has_focused_agent = false;
    let mut dirty = true;
    let mut pty_dirty = false;
    let mut eink_pacer = EinkInputPacer::default();
    let mut last_draw_at: Option<Instant> = None;
    let mut last_eink_snap: Option<(String, EinkScreenSnapshot)> = None;
    let mut pending_reader_rpc: Option<PendingReaderRpc> = None;
    let mut reader_drag: Option<u16> = None;
    let mut reader_open_scope: Option<ReaderScope> = None;
    let mut prev_active_agent_state: Option<(String, AgentState)> = None;
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
        expire_pending_reader_rpc(
            now,
            core,
            world,
            &mut modal,
            &mut pending_reader_rpc,
            &mut dirty,
        );
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
                !matches!(modal, Modal::TabOverview { .. }),
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
                dpad_drag = None;
                page_tap = None;
                selection = None;
                selection_screen = None;
                selection_vt_seq = None;
                pty_dirty = false;
                eink_pacer.reset_on_channel_switch();
                dirty = true;
            }
        }

        if let Some(osc) = session_reporter.sync_sequence(world.active_terminal_address_ref()) {
            let backend = terminal.backend_mut();
            backend.write_all(osc.as_bytes())?;
            backend.flush()?;
        }

        let mut active_channel = active_addr
            .as_ref()
            .and_then(|a| attached_map.get(a))
            .map(|att| att.channel);

        promote_due_pty_dirty_with_color(
            eink_mode,
            eink_color,
            eink_interval,
            Instant::now(),
            &mut pty_dirty,
            &mut eink_pacer,
            &mut dirty,
            &mut last_draw_at,
            &last_eink_snap,
            active_addr.as_deref(),
            attached_map,
            term_rect,
            &mut scrollback_offset,
            &modal,
            &selection,
        );

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
            let reader_rpc_deadline = pending_reader_rpc.as_ref().map(|r| r.deadline);
            let pty_deadline = if eink_mode && !eink_interval.is_zero() {
                eink_pacer.next_deadline(pty_dirty, eink_mode, eink_interval, now, last_draw_at)
            } else {
                None
            };
            for deadline in [
                copied_until,
                pending_sibling_resize_at,
                retry_deadline,
                reader_rpc_deadline,
                pty_deadline,
            ]
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
                        expire_pending_reader_rpc(
                            after,
                            core,
                            world,
                            &mut modal,
                            &mut pending_reader_rpc,
                            &mut dirty,
                        );
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
                        if let Some(att) = attached_map.values_mut().find(|a| a.channel == channel)
                        {
                            att.process_vt_bytes(&data);
                        }
                        let mut cleared_vis_sel = false;
                        if is_active && selection.as_ref().is_some_and(|s| !s.active) {
                            cleared_vis_sel = selection
                                .as_ref()
                                .is_some_and(TerminalSelection::is_non_empty);
                            selection = None;
                            selection_screen = None;
                            selection_vt_seq = None;
                        }
                        if is_active
                            && !matches!(modal, Modal::TabOverview { .. } | Modal::Reader { .. })
                        {
                            if !eink_mode
                                || eink_interval.is_zero()
                                || !first_frame_drawn
                                || cleared_vis_sel
                            {
                                dirty = true;
                            } else {
                                pty_dirty = true;
                                eink_pacer.record_active_pty_data(
                                    channel,
                                    Instant::now(),
                                    core.last_pty_input(),
                                );
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
                            if matches!(modal, Modal::Reader { .. }) {
                                modal = Modal::None;
                                reader_open_scope = None;
                                reader_drag = None;
                                set_pending_reader_rpc(core, &mut pending_reader_rpc, None);
                            }
                        }
                        pending_lifecycle_sync = true;
                    }
                    CoreMessage::TerminalRead {
                        request_id,
                        terminal,
                        result,
                    } => {
                        apply_terminal_read_message(
                            request_id,
                            &terminal,
                            result,
                            world,
                            &mut modal,
                            area,
                            &mut pending_reader_rpc,
                            &mut dirty,
                        );
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
                            page_tap = None;
                            reader_drag = None;
                            cur_size = (w, h);
                            clamp_reader_scroll_on_resize(&mut modal, Rect::new(0, 0, w, h));
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
                                horiz_wheel.clear_pending_horiz();
                                page_tap = None;
                                reader_drag = None;
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
                                        HitTarget::ModalNoop => {
                                            if matches!(modal, Modal::Reader { .. }) {
                                                let (body_rect, _, _) = reader_body_metrics(area);
                                                if is_in_rect(Some(body_rect), mx, my) {
                                                    reader_drag = Some(my);
                                                }
                                            }
                                        }
                                        HitTarget::CancelModal => {
                                            modal = Modal::None;
                                            prefix_active = false;
                                            ctrl_active = false;
                                            alt_active = false;
                                            dirty = true;
                                        }
                                        HitTarget::ReaderPageUp
                                        | HitTarget::ReaderPageDown
                                        | HitTarget::ReaderToggleClean
                                        | HitTarget::ReaderJumpLatest => {
                                            let is_reset_scroll = matches!(
                                                target,
                                                HitTarget::ReaderJumpLatest
                                                    | HitTarget::ReaderToggleClean
                                            );
                                            let (_, changed) =
                                                handle_reader_mouse_hit(target, &mut modal, area);
                                            if changed {
                                                set_reader_user_scrolled(
                                                    &mut pending_reader_rpc,
                                                    !is_reset_scroll,
                                                );
                                                dirty = true;
                                            }
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
                                            set_pending_reader_rpc(
                                                core,
                                                &mut pending_reader_rpc,
                                                None,
                                            );
                                            prefix_active = false;
                                            ctrl_active = false;
                                            alt_active = false;
                                            if execute_action_full(
                                                act,
                                                core,
                                                world,
                                                active_att,
                                                prefix_spec,
                                                sidebar_open,
                                                &mut scrollback_offset,
                                                &mut modal,
                                                term_rect,
                                                area,
                                                &mut eink_mode,
                                                &mut eink_color,
                                                &mut force_clear,
                                                Some(&mut pending_reader_rpc),
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
                                        HitTarget::ExitTabOverview => {
                                            prefix_active = false;
                                            ctrl_active = false;
                                            alt_active = false;
                                            exit_tab_overview_to_last_tab(core, world, &mut modal);
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
                                        HitTarget::SendKey(kev) => {
                                            match handle_key_event_full(
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
                                                area,
                                                &mut eink_mode,
                                                &mut eink_color,
                                                &mut force_clear,
                                                Some(&mut pending_reader_rpc),
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
                                    }
                                } else if matches!(modal, Modal::TabOverview { .. }) {
                                    dpad_drag = None;
                                } else if !matches!(modal, Modal::None) {
                                    dpad_drag = None;
                                    modal = Modal::None;
                                    prefix_active = false;
                                    ctrl_active = false;
                                    alt_active = false;
                                    dirty = true;
                                } else if let Some(dir) = match_eink_page_touch_zone(
                                    last_page_up_rect,
                                    last_page_down_rect,
                                    mx,
                                    my,
                                ) {
                                    dpad_drag = None;
                                    page_tap = Some(PendingPageTurnTap {
                                        dir,
                                        start_col: mx,
                                        start_row: my,
                                    });
                                    selection = None;
                                    selection_screen = None;
                                    selection_vt_seq = None;
                                    if had_vis_sel {
                                        dirty = true;
                                    }
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
                                } else if reader_drag.is_some() {
                                    if handle_reader_drag(
                                        &mut reader_drag,
                                        mev.row,
                                        &mut modal,
                                        area,
                                        eink_mode,
                                    ) {
                                        set_reader_user_scrolled(&mut pending_reader_rpc, true);
                                        dirty = true;
                                    }
                                } else if let Some(ref tap) = page_tap {
                                    if !page_tap_within_slop(tap, mev.column, mev.row) {
                                        let start_col = tap.start_col;
                                        let start_row = tap.start_row;
                                        page_tap = None;
                                        if term_rect.width > 0 && term_rect.height > 0 {
                                            let rel_col = start_col
                                                .saturating_sub(term_rect.x)
                                                .min(term_rect.width.saturating_sub(1));
                                            let rel_row = start_row
                                                .saturating_sub(term_rect.y)
                                                .min(term_rect.height.saturating_sub(1));
                                            let mut sel = TerminalSelection::new(rel_row, rel_col);
                                            if sel.update_drag(term_rect, mev.column, mev.row) {
                                                if sel.is_non_empty() && selection_screen.is_none()
                                                {
                                                    if let Some(att) = active_att {
                                                        let (scr, vt_seq, init_fence) =
                                                            capture_selection_snapshot(
                                                                att,
                                                                eink_mode
                                                                    || is_active_tab_agent(world),
                                                            );
                                                        selection_screen = Some(scr);
                                                        selection_vt_seq = Some(vt_seq);
                                                        selection_initial_fence = init_fence;
                                                    }
                                                }
                                                dirty = true;
                                            }
                                            selection = Some(sel);
                                        }
                                    }
                                } else if let Some(ref mut sel) = selection {
                                    if sel.update_drag(term_rect, mev.column, mev.row) {
                                        if sel.is_non_empty() && selection_screen.is_none() {
                                            if let Some(att) = active_att {
                                                let (scr, vt_seq, init_fence) =
                                                    capture_selection_snapshot(
                                                        att,
                                                        eink_mode || is_active_tab_agent(world),
                                                    );
                                                selection_screen = Some(scr);
                                                selection_vt_seq = Some(vt_seq);
                                                selection_initial_fence = init_fence;
                                            }
                                        }
                                        dirty = true;
                                    }
                                }
                            }
                            MouseEventKind::Up(MouseButton::Left) => {
                                reader_drag = None;
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
                                                match handle_key_event_full(
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
                                                    area,
                                                    &mut eink_mode,
                                                    &mut eink_color,
                                                    &mut force_clear,
                                                    Some(&mut pending_reader_rpc),
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
                                } else if let Some(tap) = page_tap.take() {
                                    if matches!(modal, Modal::None)
                                        && page_tap_within_slop(&tap, mev.column, mev.row)
                                    {
                                        let had_sticky = prefix_active || ctrl_active || alt_active;
                                        prefix_active = false;
                                        ctrl_active = false;
                                        alt_active = false;
                                        let changed = step_linear_tab_or_overview(
                                            tap.dir, core, world, &mut modal,
                                        );
                                        if changed || had_sticky {
                                            dirty = true;
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
                                            let anchor_col =
                                                term_rect.x.saturating_add(sel.anchor.1);
                                            let anchor_row =
                                                term_rect.y.saturating_add(sel.anchor.0);
                                            let was_dragged = sel.dragged || snap.is_some();
                                            let slop_toggled = try_activate_space_slop_tap(
                                                was_dragged,
                                                anchor_col,
                                                anchor_row,
                                                mev.column,
                                                mev.row,
                                                last_space_slop_rect,
                                                &mut modal,
                                                &mut prefix_active,
                                                &mut ctrl_active,
                                                &mut alt_active,
                                            );
                                            selection = None;
                                            if snap.is_some() || slop_toggled {
                                                dirty = true;
                                            }
                                        }
                                    }
                                }
                            }
                            MouseEventKind::ScrollUp => {
                                page_tap = None;
                                let wheel_now = Instant::now();
                                if matches!(modal, Modal::SpaceMenu) {
                                    if handle_top_bar_or_space_vertical_wheel(
                                        true,
                                        wheel_now,
                                        &mut horiz_wheel,
                                        core,
                                        world,
                                        &mut modal,
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
                                } else if let Modal::Fuzzy {
                                    ref query,
                                    ref mut selected,
                                    ref mut selected_tab_id,
                                } = modal
                                {
                                    let fuzzy_zone = if mev.row < term_rect.y {
                                        WheelZone::TopBar
                                    } else {
                                        WheelZone::Viewport
                                    };
                                    horiz_wheel.record_suppressed_vertical_wheel(
                                        fuzzy_zone, true, wheel_now,
                                    );
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
                                } else if matches!(modal, Modal::None | Modal::TabOverview { .. })
                                    && mev.row < term_rect.y
                                {
                                    if handle_top_bar_or_space_vertical_wheel(
                                        true,
                                        wheel_now,
                                        &mut horiz_wheel,
                                        core,
                                        world,
                                        &mut modal,
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
                                } else if matches!(modal, Modal::Reader { .. })
                                    && dpad_drag.is_none()
                                    && !selection.as_ref().is_some_and(|s| s.active)
                                    && mev.row >= area.y
                                    && mev.row < area.y.saturating_add(area.height)
                                {
                                    horiz_wheel.record_viewport_vertical_wheel(wheel_now);
                                    if handle_reader_wheel(true, &mut modal, area, eink_mode) {
                                        set_reader_user_scrolled(&mut pending_reader_rpc, true);
                                        dirty = true;
                                    }
                                } else if matches!(modal, Modal::TabOverview { .. })
                                    && dpad_drag.is_none()
                                    && !selection.as_ref().is_some_and(|s| s.active)
                                    && mev.row >= term_rect.y
                                    && mev.row < term_rect.y.saturating_add(term_rect.height)
                                {
                                    horiz_wheel.record_viewport_vertical_wheel(wheel_now);
                                    if scroll_tab_overview_selection(world, &mut modal, true) {
                                        dirty = true;
                                    }
                                } else if matches!(modal, Modal::None)
                                    && dpad_drag.is_none()
                                    && !selection.as_ref().is_some_and(|s| s.active)
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
                                        let step =
                                            main_scrollback_wheel_step(term_rect.height, eink_mode);
                                        scrollback_offset = scrollback_offset.saturating_add(step);
                                        if scrollback_offset != prev_scroll {
                                            dirty = true;
                                        }
                                    }
                                } else if mev.row < term_rect.y {
                                    horiz_wheel.record_suppressed_vertical_wheel(
                                        WheelZone::TopBar,
                                        true,
                                        wheel_now,
                                    );
                                } else {
                                    horiz_wheel.record_suppressed_vertical_wheel(
                                        WheelZone::Viewport,
                                        true,
                                        wheel_now,
                                    );
                                }
                            }
                            MouseEventKind::ScrollDown => {
                                page_tap = None;
                                let wheel_now = Instant::now();
                                if matches!(modal, Modal::SpaceMenu) {
                                    if handle_top_bar_or_space_vertical_wheel(
                                        false,
                                        wheel_now,
                                        &mut horiz_wheel,
                                        core,
                                        world,
                                        &mut modal,
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
                                } else if let Modal::Fuzzy {
                                    ref query,
                                    ref mut selected,
                                    ref mut selected_tab_id,
                                } = modal
                                {
                                    let fuzzy_zone = if mev.row < term_rect.y {
                                        WheelZone::TopBar
                                    } else {
                                        WheelZone::Viewport
                                    };
                                    horiz_wheel.record_suppressed_vertical_wheel(
                                        fuzzy_zone, false, wheel_now,
                                    );
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
                                } else if matches!(modal, Modal::None | Modal::TabOverview { .. })
                                    && mev.row < term_rect.y
                                {
                                    if handle_top_bar_or_space_vertical_wheel(
                                        false,
                                        wheel_now,
                                        &mut horiz_wheel,
                                        core,
                                        world,
                                        &mut modal,
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
                                } else if matches!(modal, Modal::Reader { .. })
                                    && dpad_drag.is_none()
                                    && !selection.as_ref().is_some_and(|s| s.active)
                                    && mev.row >= area.y
                                    && mev.row < area.y.saturating_add(area.height)
                                {
                                    horiz_wheel.record_viewport_vertical_wheel(wheel_now);
                                    if handle_reader_wheel(false, &mut modal, area, eink_mode) {
                                        set_reader_user_scrolled(&mut pending_reader_rpc, true);
                                        dirty = true;
                                    }
                                } else if matches!(modal, Modal::TabOverview { .. })
                                    && dpad_drag.is_none()
                                    && !selection.as_ref().is_some_and(|s| s.active)
                                    && mev.row >= term_rect.y
                                    && mev.row < term_rect.y.saturating_add(term_rect.height)
                                {
                                    horiz_wheel.record_viewport_vertical_wheel(wheel_now);
                                    if scroll_tab_overview_selection(world, &mut modal, false) {
                                        dirty = true;
                                    }
                                } else if matches!(modal, Modal::None)
                                    && dpad_drag.is_none()
                                    && !selection.as_ref().is_some_and(|s| s.active)
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
                                        let step =
                                            main_scrollback_wheel_step(term_rect.height, eink_mode);
                                        scrollback_offset = scrollback_offset.saturating_sub(step);
                                        if scrollback_offset != prev_scroll {
                                            dirty = true;
                                        }
                                    }
                                } else if mev.row < term_rect.y {
                                    horiz_wheel.record_suppressed_vertical_wheel(
                                        WheelZone::TopBar,
                                        false,
                                        wheel_now,
                                    );
                                } else {
                                    horiz_wheel.record_suppressed_vertical_wheel(
                                        WheelZone::Viewport,
                                        false,
                                        wheel_now,
                                    );
                                }
                            }
                            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
                                page_tap = None;
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
                                    &mut modal,
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
                            let had_drag_or_sel = dpad_drag.as_ref().is_some_and(|d| d.moved)
                                || selection
                                    .as_ref()
                                    .is_some_and(TerminalSelection::is_non_empty);
                            dpad_drag = None;
                            page_tap = None;
                            selection = None;
                            selection_screen = None;
                            selection_vt_seq = None;
                            if had_drag_or_sel {
                                dirty = true;
                            }
                            match handle_key_event_full(
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
                                area,
                                &mut eink_mode,
                                &mut eink_color,
                                &mut force_clear,
                                Some(&mut pending_reader_rpc),
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
            if !matches!(modal, Modal::TabOverview { .. }) {
                world.mark_active_seen(core);
            }
            need_sync_terminals = true;
            dirty = true;
        } else if pending_lifecycle_sync {
            if world.refresh_terminals(core).is_ok() {
                let _ = world.reconcile_and_sync(core);
            }
            if !matches!(modal, Modal::TabOverview { .. }) {
                world.mark_active_seen(core);
            }
            need_sync_terminals = true;
            dirty = true;
        } else if pending_term_refresh {
            let _ = world.refresh_terminals(core);
            if !matches!(modal, Modal::TabOverview { .. }) {
                world.mark_active_seen(core);
            }
            dirty = true;
        } else if check_active_seen && !matches!(modal, Modal::TabOverview { .. }) {
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
        } else if let Modal::TabOverview {
            ref query,
            ref mut selected,
            ref mut selected_target,
        } = modal
        {
            if let OverviewSelection::Tab(ref tid) = *selected_target {
                let in_active_space = world
                    .active_space()
                    .is_some_and(|s| s.tabs.iter().any(|t| &t.id == tid));
                if !in_active_space {
                    let items = collect_overview_items(world, query);
                    (*selected, *selected_target) = overview_selection_for_index(&items, *selected);
                    dirty = true;
                }
            }
        }
        if sync_reader_modal_scope(
            core,
            &mut modal,
            world,
            &mut reader_open_scope,
            &mut reader_drag,
            &mut pending_reader_rpc,
        ) {
            dirty = true;
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

        promote_due_pty_dirty_with_color(
            eink_mode,
            eink_color,
            eink_interval,
            Instant::now(),
            &mut pty_dirty,
            &mut eink_pacer,
            &mut dirty,
            &mut last_draw_at,
            &last_eink_snap,
            active_addr.as_deref(),
            attached_map,
            term_rect,
            &mut scrollback_offset,
            &modal,
            &selection,
        );

        let cur_agent = active_addr
            .as_ref()
            .and_then(|a| world.terminals.get(a))
            .map(|i| (i.address.clone(), i.agent_state()));
        if eink_mode
            && detect_eink_agent_settlement(prev_active_agent_state.as_ref(), cur_agent.as_ref())
        {
            force_clear = true;
            dirty = true;
        }
        prev_active_agent_state = cur_agent;

        if selection_screen.is_none() {
            selection_initial_fence = false;
        }

        if dirty {
            let use_selection_screen =
                selection.as_ref().is_some_and(|s| s.active) && selection_screen.is_some();
            let initial_in_code_fence = if (eink_mode || is_active_tab_agent(world))
                && !matches!(modal, Modal::TabOverview { .. } | Modal::Reader { .. })
            {
                if use_selection_screen {
                    selection_initial_fence
                } else if let Some(att) = active_addr.as_ref().and_then(|a| attached_map.get_mut(a))
                {
                    att.viewport_initial_code_fence(scrollback_offset)
                } else {
                    false
                }
            } else {
                false
            };
            let scr_ref = if use_selection_screen {
                selection_screen.as_ref()
            } else {
                active_addr
                    .as_ref()
                    .and_then(|a| attached_map.get(a))
                    .map(|a| a.parser.screen())
            };
            let copied_flash = copied_until.is_some();
            let mod_dragging = dpad_drag.as_ref().is_some_and(|d| d.is_mod && d.moved);
            let dpad_dragging = dpad_drag.as_ref().is_some_and(|d| !d.is_mod && d.moved);
            queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
            if force_clear {
                terminal.clear()?;
                force_clear = false;
            }
            let mut geom_eink_snapshot = None;
            terminal.draw(|f| {
                let geom = render_ui_full_with_eink_color_and_fence(
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
                    eink_mode,
                    eink_color,
                    initial_in_code_fence,
                );
                last_hits = geom.hits;
                last_mod_rect = geom.mod_rect;
                last_dpad_rect = geom.dpad_rect;
                last_space_slop_rect = geom.space_slop_rect;
                last_page_up_rect = geom.page_up_rect;
                last_page_down_rect = geom.page_down_rect;
                geom_eink_snapshot = geom.eink_snapshot;
            })?;
            execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
            dirty = false;
            pty_dirty = false;
            eink_pacer.on_promoted_or_drawn();
            last_draw_at = Some(Instant::now());
            last_eink_snap = geom_eink_snapshot
                .and_then(|snap| active_addr.as_ref().map(|addr| (addr.clone(), snap)));
            if !first_frame_drawn {
                first_frame_drawn = true;
                need_sync_terminals = true;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_shortcut_in_tab_overview(
    action: Action,
    core: &CoreClient,
    world: &mut World,
    attached: Option<&AttachedTerminal>,
    prefix_spec: &str,
    sidebar_open: &mut bool,
    scrollback_offset: &mut usize,
    modal: &mut Modal,
    term_rect: Rect,
    reader_rect: Rect,
    eink_mode: &mut bool,
    eink_color: &mut bool,
    force_clear: &mut bool,
    mut pending_reader_rpc: Option<&mut Option<PendingReaderRpc>>,
) -> Result<KeyOutcome> {
    match action {
        Action::Detach => {
            *modal = Modal::None;
            Ok(KeyOutcome::Quit)
        }
        Action::SendPrefix | Action::PrevTab => {
            exit_tab_overview_to_last_tab(core, world, modal);
            Ok(KeyOutcome::UiChanged)
        }
        Action::NextTab => {
            step_linear_tab_or_overview(HorizontalSwipeDir::Next, core, world, modal);
            Ok(KeyOutcome::UiChanged)
        }
        Action::SelectTab(idx) => {
            if world.active_space().is_some_and(|s| idx < s.tabs.len()) {
                *modal = Modal::None;
                if execute_action_full(
                    Action::SelectTab(idx),
                    core,
                    world,
                    attached,
                    prefix_spec,
                    sidebar_open,
                    scrollback_offset,
                    modal,
                    term_rect,
                    reader_rect,
                    eink_mode,
                    eink_color,
                    force_clear,
                    pending_reader_rpc.as_deref_mut(),
                )? {
                    return Ok(KeyOutcome::Quit);
                }
            }
            Ok(KeyOutcome::UiChanged)
        }
        other => {
            if !matches!(
                other,
                Action::ToggleSidebar
                    | Action::ToggleHelp
                    | Action::ToggleEink
                    | Action::ToggleEinkColor
                    | Action::RedrawScreen
            ) {
                *modal = Modal::None;
            }
            if execute_action_full(
                other,
                core,
                world,
                attached,
                prefix_spec,
                sidebar_open,
                scrollback_offset,
                modal,
                term_rect,
                reader_rect,
                eink_mode,
                eink_color,
                force_clear,
                pending_reader_rpc,
            )? {
                return Ok(KeyOutcome::Quit);
            }
            if matches!(modal, Modal::None) {
                world.mark_active_seen(core);
            }
            Ok(KeyOutcome::UiChanged)
        }
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn handle_key_event(
    kev: KeyEvent,
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
    let mut eink_mode = false;
    let mut force_clear = false;
    handle_key_event_with_eink(
        kev,
        core,
        world,
        attached,
        prefix_spec,
        sidebar_open,
        prefix_active,
        ctrl_active,
        alt_active,
        scrollback_offset,
        modal,
        term_rect,
        &mut eink_mode,
        &mut force_clear,
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn handle_key_event_with_eink(
    kev: KeyEvent,
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
    eink_mode: &mut bool,
    force_clear: &mut bool,
) -> Result<KeyOutcome> {
    let mut eink_color = false;
    handle_key_event_full(
        kev,
        core,
        world,
        attached,
        prefix_spec,
        sidebar_open,
        prefix_active,
        ctrl_active,
        alt_active,
        scrollback_offset,
        modal,
        term_rect,
        term_rect,
        eink_mode,
        &mut eink_color,
        force_clear,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn handle_key_event_full(
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
    reader_rect: Rect,
    eink_mode: &mut bool,
    eink_color: &mut bool,
    force_clear: &mut bool,
    mut pending_reader_rpc: Option<&mut Option<PendingReaderRpc>>,
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
        Modal::SpaceMenu
        | Modal::TabOverview { .. }
        | Modal::Help
        | Modal::Reader { .. }
        | Modal::None => {}
    }

    // 2. Prefix mode active (`Ctrl+b` pressed previously or `[^B]` tapped)
    // Checked before `SpaceMenu`/`Help`/`TabOverview` so `^b b`, `^b ?`, and `^b p/n` behave cleanly.
    if *prefix_active {
        *prefix_active = false;
        if let Some(action) = match_prefix_action(&kev, prefix_spec) {
            if matches!(modal, Modal::TabOverview { .. }) {
                return handle_shortcut_in_tab_overview(
                    action,
                    core,
                    world,
                    attached,
                    prefix_spec,
                    sidebar_open,
                    scrollback_offset,
                    modal,
                    term_rect,
                    reader_rect,
                    eink_mode,
                    eink_color,
                    force_clear,
                    pending_reader_rpc,
                );
            }
            if matches!(modal, Modal::SpaceMenu | Modal::Help) {
                if action == Action::SendPrefix {
                    *modal = Modal::None;
                    return Ok(KeyOutcome::UiChanged);
                }
                if !matches!(
                    action,
                    Action::ToggleSidebar
                        | Action::ToggleHelp
                        | Action::ToggleEink
                        | Action::ToggleEinkColor
                        | Action::RedrawScreen
                ) {
                    *modal = Modal::None;
                }
            } else if matches!(modal, Modal::Reader { .. }) {
                if action == Action::SendPrefix {
                    return Ok(KeyOutcome::UiChanged);
                }
                if !matches!(
                    action,
                    Action::OpenBookReader
                        | Action::ToggleEink
                        | Action::ToggleEinkColor
                        | Action::RedrawScreen
                        | Action::PageUp
                        | Action::PageDown
                ) {
                    *modal = Modal::None;
                    if let Some(slot) = pending_reader_rpc.as_deref_mut() {
                        set_pending_reader_rpc(core, slot, None);
                    }
                }
            }
            if execute_action_full(
                action,
                core,
                world,
                attached,
                prefix_spec,
                sidebar_open,
                scrollback_offset,
                modal,
                term_rect,
                reader_rect,
                eink_mode,
                eink_color,
                force_clear,
                pending_reader_rpc,
            )? {
                return Ok(KeyOutcome::Quit);
            }
        } else if matches!(modal, Modal::SpaceMenu | Modal::Help) && kev.code == KeyCode::Esc {
            *modal = Modal::None;
        } else if matches!(modal, Modal::TabOverview { .. }) && kev.code == KeyCode::Esc {
            exit_tab_overview_to_last_tab(core, world, modal);
        }
        return Ok(KeyOutcome::UiChanged);
    }

    // 3. Check if prefix key was pressed (`Ctrl+b`)
    if is_prefix_key(&kev, prefix_spec) {
        *prefix_active = true;
        return Ok(KeyOutcome::UiChanged);
    }

    // 4. Handle non-prefix keys inside `SpaceMenu`, `TabOverview`, and `Help` overlays
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
        Modal::TabOverview { .. } if match_direct_action(&kev).is_some() => {
            if let Some(action) = match_direct_action(&kev) {
                return handle_shortcut_in_tab_overview(
                    action,
                    core,
                    world,
                    attached,
                    prefix_spec,
                    sidebar_open,
                    scrollback_offset,
                    modal,
                    term_rect,
                    reader_rect,
                    eink_mode,
                    eink_color,
                    force_clear,
                    pending_reader_rpc,
                );
            }
            return Ok(KeyOutcome::UiChanged);
        }
        Modal::TabOverview {
            query,
            selected,
            selected_target,
        } => {
            let no_ctrl_alt = !kev
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
            match kev.code {
                KeyCode::Esc => {
                    exit_tab_overview_to_last_tab(core, world, modal);
                }
                KeyCode::Left if no_ctrl_alt && query.is_empty() => {
                    exit_tab_overview_to_last_tab(core, world, modal);
                }
                KeyCode::Up | KeyCode::BackTab => {
                    let items = collect_overview_items(world, query);
                    let cur_sel = resolve_overview_selection(&items, *selected, selected_target);
                    (*selected, *selected_target) =
                        overview_selection_for_index(&items, cur_sel.saturating_sub(1));
                }
                KeyCode::Char('p') if kev.modifiers.contains(KeyModifiers::CONTROL) => {
                    let items = collect_overview_items(world, query);
                    let cur_sel = resolve_overview_selection(&items, *selected, selected_target);
                    (*selected, *selected_target) =
                        overview_selection_for_index(&items, cur_sel.saturating_sub(1));
                }
                KeyCode::Down | KeyCode::Tab => {
                    let items = collect_overview_items(world, query);
                    let cur_sel = resolve_overview_selection(&items, *selected, selected_target);
                    (*selected, *selected_target) = overview_selection_for_index(
                        &items,
                        (cur_sel + 1).min(items.len().saturating_sub(1)),
                    );
                }
                KeyCode::Char('n') if kev.modifiers.contains(KeyModifiers::CONTROL) => {
                    let items = collect_overview_items(world, query);
                    let cur_sel = resolve_overview_selection(&items, *selected, selected_target);
                    (*selected, *selected_target) = overview_selection_for_index(
                        &items,
                        (cur_sel + 1).min(items.len().saturating_sub(1)),
                    );
                }
                KeyCode::Home => {
                    let items = collect_overview_items(world, query);
                    (*selected, *selected_target) = overview_selection_for_index(&items, 0);
                }
                KeyCode::End => {
                    let items = collect_overview_items(world, query);
                    (*selected, *selected_target) =
                        overview_selection_for_index(&items, items.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    let items = collect_overview_items(world, query);
                    let cur_sel = resolve_overview_selection(&items, *selected, selected_target);
                    match items.get(cur_sel) {
                        Some(OverviewItem::NewTab { .. }) | None => {
                            *modal = Modal::None;
                            let _ = world.create_new_tab(core);
                        }
                        Some(OverviewItem::Tab {
                            space_id, tab_id, ..
                        }) => {
                            let sid = space_id.clone();
                            let tid = tab_id.clone();
                            *modal = Modal::None;
                            if world.select_tab_by_id(&sid, &tid) {
                                world.mark_active_seen(core);
                            }
                        }
                    }
                }
                KeyCode::Backspace => {
                    query.pop();
                    let items = collect_overview_items(world, query);
                    (*selected, *selected_target) =
                        overview_selection_after_query_change(&items, query);
                }
                KeyCode::Char('u') if kev.modifiers.contains(KeyModifiers::CONTROL) => {
                    query.clear();
                    *selected = 0;
                    *selected_target = OverviewSelection::NewTab;
                }
                KeyCode::Char(c) if no_ctrl_alt => {
                    query.push(c);
                    let items = collect_overview_items(world, query);
                    (*selected, *selected_target) =
                        overview_selection_after_query_change(&items, query);
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
        Modal::Reader { .. } => {
            if let Some(action) = match_direct_action(&kev) {
                *modal = Modal::None;
                if let Some(slot) = pending_reader_rpc.as_deref_mut() {
                    set_pending_reader_rpc(core, slot, None);
                }
                if execute_action_full(
                    action,
                    core,
                    world,
                    attached,
                    prefix_spec,
                    sidebar_open,
                    scrollback_offset,
                    modal,
                    term_rect,
                    reader_rect,
                    eink_mode,
                    eink_color,
                    force_clear,
                    pending_reader_rpc,
                )? {
                    return Ok(KeyOutcome::Quit);
                }
                return Ok(KeyOutcome::UiChanged);
            }
            let no_ctrl_alt = !kev
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
            let step = reader_page_step(reader_rect) as isize;
            let page_mv = match kev.code {
                KeyCode::PageUp | KeyCode::BackTab => Some(ReaderScrollMove::Delta(-step)),
                KeyCode::Char('b') if no_ctrl_alt => Some(ReaderScrollMove::Delta(-step)),
                KeyCode::PageDown => Some(ReaderScrollMove::Delta(step)),
                KeyCode::Char(' ') if no_ctrl_alt => Some(ReaderScrollMove::Delta(step)),
                KeyCode::Home => Some(ReaderScrollMove::Top),
                KeyCode::Char('g') if no_ctrl_alt => Some(ReaderScrollMove::Top),
                KeyCode::End => Some(ReaderScrollMove::Bottom),
                KeyCode::Char('G') if no_ctrl_alt => Some(ReaderScrollMove::Bottom),
                KeyCode::Char('l') if no_ctrl_alt => Some(ReaderScrollMove::Latest),
                _ => None,
            };
            if let Some(mv) = page_mv {
                let changed = move_reader_scroll(modal, reader_rect, mv);
                if let Some(slot) = pending_reader_rpc {
                    if matches!(mv, ReaderScrollMove::Latest) {
                        set_reader_user_scrolled(slot, false);
                    } else if changed {
                        set_reader_user_scrolled(slot, true);
                    }
                }
                return Ok(KeyOutcome::UiChanged);
            }
            let line_delta = match kev.code {
                KeyCode::Up => Some(-1),
                KeyCode::Char('k') if no_ctrl_alt => Some(-1),
                KeyCode::Down => Some(1),
                KeyCode::Char('j') if no_ctrl_alt => Some(1),
                _ => None,
            };
            if let Some(delta) = line_delta {
                if move_reader_scroll(modal, reader_rect, ReaderScrollMove::Delta(delta)) {
                    if let Some(slot) = pending_reader_rpc {
                        set_reader_user_scrolled(slot, true);
                    }
                }
                return Ok(KeyOutcome::UiChanged);
            }
            match kev.code {
                KeyCode::Esc | KeyCode::Char('q' | 'v') if no_ctrl_alt => {
                    *modal = Modal::None;
                    if let Some(slot) = pending_reader_rpc {
                        set_pending_reader_rpc(core, slot, None);
                    }
                }
                KeyCode::Char('c') if kev.modifiers.contains(KeyModifiers::CONTROL) => {
                    *modal = Modal::None;
                    if let Some(slot) = pending_reader_rpc {
                        set_pending_reader_rpc(core, slot, None);
                    }
                }
                KeyCode::Tab | KeyCode::Char('f' | 'c') if no_ctrl_alt => {
                    if toggle_reader_clean_mode(modal, reader_rect) {
                        if let Some(slot) = pending_reader_rpc {
                            set_reader_user_scrolled(slot, false);
                        }
                    }
                }
                KeyCode::Char('r') if no_ctrl_alt => {
                    let clean_mode = match *modal {
                        Modal::Reader { ref doc, .. } => doc.clean_mode,
                        _ => true,
                    };
                    *modal = build_reader_modal_state(
                        core,
                        world,
                        attached,
                        reader_rect,
                        clean_mode,
                        pending_reader_rpc,
                    );
                }
                _ => {}
            }
            return Ok(KeyOutcome::UiChanged);
        }
        _ => {}
    }

    // 5. Direct global shortcuts (`Alt+Left/Right/Up/Down/1..9`, `Ctrl+\`)
    if let Some(action) = match_direct_action(&kev) {
        if execute_action_full(
            action,
            core,
            world,
            attached,
            prefix_spec,
            sidebar_open,
            scrollback_offset,
            modal,
            term_rect,
            reader_rect,
            eink_mode,
            eink_color,
            force_clear,
            pending_reader_rpc,
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

    struct WheelTestHarness {
        core: crate::core::TestCoreClient,
        world: World,
        term_rect: Rect,
        horiz_wheel: HorizontalWheelGestureState,
        modal: Modal,
        prefix_active: bool,
        ctrl_active: bool,
        alt_active: bool,
        selection: Option<TerminalSelection>,
        selection_screen: Option<vt100::Screen>,
        selection_vt_seq: Option<u64>,
    }

    impl WheelTestHarness {
        fn new() -> Self {
            use crate::model::{make_single_terminal_tab, SharedSpace};
            use serde_json::Map;

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

            Self {
                core: CoreClient::dummy_for_tests(),
                world,
                term_rect: Rect::new(0, 1, 80, 22),
                horiz_wheel: HorizontalWheelGestureState::default(),
                modal: Modal::None,
                prefix_active: false,
                ctrl_active: false,
                alt_active: false,
                selection: None,
                selection_screen: None,
                selection_vt_seq: None,
            }
        }

        fn horiz_tick(
            &mut self,
            dir: HorizontalSwipeDir,
            row: u16,
            at: Instant,
            dpad_drag: bool,
        ) -> bool {
            handle_horizontal_wheel_event(
                dir,
                row,
                self.term_rect,
                at,
                &mut self.horiz_wheel,
                &self.core,
                &mut self.world,
                &mut self.modal,
                dpad_drag,
                &mut self.prefix_active,
                &mut self.ctrl_active,
                &mut self.alt_active,
                &mut self.selection,
                &mut self.selection_screen,
                &mut self.selection_vt_seq,
            )
        }

        fn swipe_horiz(
            &mut self,
            dir: HorizontalSwipeDir,
            row: u16,
            ticks: u8,
            start: Instant,
        ) -> bool {
            let mut switched = false;
            for i in 0..ticks {
                let tick_res = self.horiz_tick(
                    dir,
                    row,
                    start + Duration::from_millis(u64::from(i) * 8),
                    false,
                );
                if i + 1 < ticks {
                    assert!(!tick_res, "unexpected early switch on tick {i} of {ticks}");
                } else {
                    switched = tick_res;
                }
            }
            switched
        }

        fn vert_switch(&mut self, scroll_up: bool, at: Instant, dpad_drag: bool) -> bool {
            handle_top_bar_or_space_vertical_wheel(
                scroll_up,
                at,
                &mut self.horiz_wheel,
                &self.core,
                &mut self.world,
                &mut self.modal,
                dpad_drag,
                &mut self.prefix_active,
                &mut self.ctrl_active,
                &mut self.alt_active,
                &mut self.selection,
                &mut self.selection_screen,
                &mut self.selection_vt_seq,
            )
        }
    }

    #[test]
    fn horizontal_wheel_switches_tabs_and_spaces_with_momentum_and_vertical_bleed_guards() {
        let mut h = WheelTestHarness::new();
        assert_eq!(h.world.active_tab_index(), 0);

        h.prefix_active = true;
        h.ctrl_active = true;
        let mut done_sel = TerminalSelection::new(2, 4);
        done_sel.head = (2, 10);
        done_sel.active = false;
        h.selection = Some(done_sel);

        let t0 = Instant::now();

        // 1. ScrollRight (natural swipe left) inside viewport requires VIEWPORT_HORIZ_SWIPE_THRESHOLD
        //    ticks to switch Tab 0 -> Tab 1 and clears sticky modifiers + completed selection highlight.
        assert!(h.swipe_horiz(
            HorizontalSwipeDir::Next,
            10,
            VIEWPORT_HORIZ_SWIPE_THRESHOLD,
            t0
        ));
        assert_eq!(h.world.active_tab_index(), 1);
        assert!(!h.prefix_active && !h.ctrl_active);
        assert!(h.selection.is_none());
        let t_switch1_end = t0 + Duration::from_millis(32);

        // 2. Momentum inertia tail (packets arriving every 16ms..80ms in same direction) is coalesced
        //    so a single trackpad flick or fast thumb swipe never overshoots multiple tabs!
        for ms in [16u64, 40, 90, 150, 230] {
            assert!(!h.horiz_tick(
                HorizontalSwipeDir::Next,
                10,
                t_switch1_end + Duration::from_millis(ms),
                false,
            ));
        }
        assert_eq!(h.world.active_tab_index(), 1);
        let t_inertia_end = t_switch1_end + Duration::from_millis(230);

        // 3. Reversing direction (ScrollLeft) bypasses same-direction quiet gap once threshold is met -> Tab 1 -> Tab 0!
        let t_rev = t_inertia_end + Duration::from_millis(20);
        assert!(h.swipe_horiz(
            HorizontalSwipeDir::Prev,
            10,
            VIEWPORT_HORIZ_SWIPE_THRESHOLD,
            t_rev
        ));
        assert_eq!(h.world.active_tab_index(), 0);
        let t_rev_end = t_rev + Duration::from_millis(32);

        // 4. After HORIZ_WHEEL_QUIET_GAP elapses, a Prev swipe at Tab 0 clamps hard (no wrap!),
        //    then two deliberate Next swipes step Tab 0 -> Tab 1 -> Tab 2.
        let t_left_clamp = t_rev_end + HORIZ_WHEEL_QUIET_GAP;
        assert!(!h.swipe_horiz(
            HorizontalSwipeDir::Prev,
            10,
            VIEWPORT_HORIZ_SWIPE_THRESHOLD,
            t_left_clamp
        ));
        assert_eq!(h.world.active_tab_index(), 0);
        assert_eq!(h.modal, Modal::None);

        let t_to_1 = t_left_clamp + Duration::from_millis(32) + HORIZ_WHEEL_QUIET_GAP;
        assert!(h.swipe_horiz(
            HorizontalSwipeDir::Next,
            10,
            VIEWPORT_HORIZ_SWIPE_THRESHOLD,
            t_to_1
        ));
        assert_eq!(h.world.active_tab_index(), 1);

        let t_to_2 = t_to_1 + Duration::from_millis(32) + HORIZ_WHEEL_QUIET_GAP;
        assert!(h.swipe_horiz(
            HorizontalSwipeDir::Next,
            10,
            VIEWPORT_HORIZ_SWIPE_THRESHOLD,
            t_to_2
        ));
        assert_eq!(h.world.active_tab_index(), 2);
        let t_next_end = t_to_2 + Duration::from_millis(32);

        // 5. Diagonal trackpad bleed during viewport vertical scroll is suppressed within VERT_SCROLL_LOCK_WINDOW
        //    across both Viewport and TopBar (row 0), then explicit TopBar swipes work after the lock expires
        //    AND suppress trailing vertical top-bar packets after entering TabOverview (both packet orderings!).
        let t_vert = t_next_end + Duration::from_secs(1);
        h.horiz_wheel.record_viewport_vertical_wheel(t_vert);
        assert!(!h.horiz_tick(
            HorizontalSwipeDir::Next,
            10,
            t_vert + Duration::from_millis(40),
            false,
        ));
        assert!(!h.swipe_horiz(
            HorizontalSwipeDir::Next,
            0,
            TOP_BAR_HORIZ_SWIPE_THRESHOLD,
            t_vert + Duration::from_millis(50),
        ));
        assert_eq!(h.world.active_tab_index(), 2);
        assert_eq!(h.modal, Modal::None);

        // Once VERT_SCROLL_LOCK_WINDOW + HORIZ_WHEEL_QUIET_GAP elapses, pure TopBar horizontal swipe
        // past the last tab (Tab 2) enters the trailing `TabOverview` page cleanly!
        let t_top_ok = t_vert + VERT_SCROLL_LOCK_WINDOW + HORIZ_WHEEL_QUIET_GAP;
        assert!(h.swipe_horiz(
            HorizontalSwipeDir::Next,
            0,
            TOP_BAR_HORIZ_SWIPE_THRESHOLD,
            t_top_ok
        ));
        assert!(matches!(h.modal, Modal::TabOverview { .. }));
        let t_top_end = t_top_ok + Duration::from_millis(16);
        // Ordering B (`ScrollRight` -> continuous `ScrollDown` stream on row 0):
        // trailing vertical packets stay suppressed even after 180ms until a quiet gap
        for ms in [40_u64, 100, 150, 200, 250] {
            assert!(!h.vert_switch(false, t_top_end + Duration::from_millis(ms), false));
        }
        assert!(matches!(h.modal, Modal::TabOverview { .. }));

        // Swiping Next while already on `TabOverview` clamps hard; swiping Prev exits back to Tab 2,
        // and trailing Prev inertia never overshoots from Tab 2 to Tab 1!
        let t_clamp_overview = t_top_end + Duration::from_millis(400);
        assert!(!h.swipe_horiz(
            HorizontalSwipeDir::Next,
            10,
            VIEWPORT_HORIZ_SWIPE_THRESHOLD,
            t_clamp_overview
        ));
        assert!(matches!(h.modal, Modal::TabOverview { .. }));

        let t_exit_overview = t_clamp_overview + Duration::from_millis(100);
        assert!(h.swipe_horiz(
            HorizontalSwipeDir::Prev,
            10,
            VIEWPORT_HORIZ_SWIPE_THRESHOLD,
            t_exit_overview
        ));
        assert_eq!(h.modal, Modal::None);
        assert_eq!(h.world.active_tab_index(), 2);
        let t_exit_end = t_exit_overview + Duration::from_millis(32);
        for ms in [16_u64, 50, 110, 180] {
            assert!(!h.horiz_tick(
                HorizontalSwipeDir::Prev,
                10,
                t_exit_end + Duration::from_millis(ms),
                false,
            ));
        }
        assert_eq!(h.world.active_tab_index(), 2);

        // Step to Tab 0 first so Ordering A tests `Tab 0 -> Tab 1` with `ScrollDown` -> `ScrollRight` stream
        let t_back1 = t_exit_end + Duration::from_millis(400);
        assert!(h.vert_switch(true, t_back1, false));
        assert_eq!(h.world.active_tab_index(), 1);
        let t_back0 = t_back1 + Duration::from_millis(400);
        assert!(h.vert_switch(true, t_back0, false));
        assert_eq!(h.world.active_tab_index(), 0);

        // Ordering A (`ScrollDown` -> continuous `ScrollRight` stream on row 0):
        // vertical switches Tab 0 -> Tab 1, and rejected horizontal burst stays coalesced past VERT_SCROLL_LOCK_WINDOW
        let t_order_a = t_back0 + Duration::from_millis(500);
        assert!(h.vert_switch(false, t_order_a, false));
        assert_eq!(h.world.active_tab_index(), 1);
        for ms in [40_u64, 100, 150, 250, 350, 450] {
            assert!(!h.horiz_tick(
                HorizontalSwipeDir::Next,
                0,
                t_order_a + Duration::from_millis(ms),
                false,
            ));
        }
        assert_eq!(h.world.active_tab_index(), 1);
        // Rejected horizontal bleed does NOT block subsequent intentional vertical top-bar scrolling
        assert!(h.vert_switch(true, t_order_a + Duration::from_millis(470), false));
        assert_eq!(h.world.active_tab_index(), 0);

        // Viewport vertical scroll followed by continuous `ScrollRight` stream past VERT_SCROLL_LOCK_WINDOW stays suppressed
        let t_vp_bleed = t_order_a + Duration::from_secs(1);
        h.horiz_wheel.record_viewport_vertical_wheel(t_vp_bleed);
        for ms in [40_u64, 120, 210, 300, 390, 460, 520] {
            assert!(!h.horiz_tick(
                HorizontalSwipeDir::Next,
                10,
                t_vp_bleed + Duration::from_millis(ms),
                false,
            ));
        }
        assert_eq!(h.world.active_tab_index(), 0);

        // Typing immediately after a horizontal tab switch does NOT reset momentum suppression
        let t_type = t_vp_bleed + Duration::from_secs(1);
        assert!(h.swipe_horiz(
            HorizontalSwipeDir::Next,
            10,
            VIEWPORT_HORIZ_SWIPE_THRESHOLD,
            t_type
        ));
        assert_eq!(h.world.active_tab_index(), 1);
        let t_type_end = t_type + Duration::from_millis(32);
        let mut sidebar_open = false;
        let mut scrollback_offset = 0;
        handle_key_event(
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
            &h.core,
            &mut h.world,
            None,
            "Ctrl+b",
            &mut sidebar_open,
            &mut h.prefix_active,
            &mut h.ctrl_active,
            &mut h.alt_active,
            &mut scrollback_offset,
            &mut h.modal,
            h.term_rect,
        )
        .unwrap();
        assert!(!h.horiz_tick(
            HorizontalSwipeDir::Next,
            10,
            t_type_end + Duration::from_millis(40),
            false,
        ));
        assert_eq!(h.world.active_tab_index(), 1);

        // Interleaved viewport vertical wheel during a horizontal momentum stream keeps
        // extending horizontal burst coalescing past VERT_SCROLL_LOCK_WINDOW (350ms)
        h.horiz_wheel
            .record_viewport_vertical_wheel(t_type_end + Duration::from_millis(50));
        for ms in [90_u64, 180, 270, 360, 450, 510] {
            assert!(!h.horiz_tick(
                HorizontalSwipeDir::Next,
                10,
                t_type_end + Duration::from_millis(ms),
                false,
            ));
        }
        assert_eq!(h.world.active_tab_index(), 1);

        // 6. Active text selection drag, floating D-pad drag, or blocking modals (Help) block tab switches
        let t_guards = t_type_end + Duration::from_secs(2);
        h.selection = Some(TerminalSelection::new(3, 3));
        assert!(!h.horiz_tick(HorizontalSwipeDir::Next, 10, t_guards, false));
        h.selection = None;
        assert!(!h.horiz_tick(
            HorizontalSwipeDir::Next,
            10,
            t_guards + Duration::from_secs(1),
            true,
        ));
        h.modal = Modal::Help;
        assert!(!h.horiz_tick(
            HorizontalSwipeDir::Next,
            10,
            t_guards + Duration::from_secs(2),
            false,
        ));
        // Trailing inertia immediately after closing Modal::Help stays suppressed
        h.modal = Modal::None;
        assert!(!h.horiz_tick(
            HorizontalSwipeDir::Next,
            10,
            t_guards + Duration::from_secs(2) + Duration::from_millis(40),
            false,
        ));
        assert_eq!(h.world.active_tab_index(), 1);

        // 7. Zone isolation (`Viewport` <-> `TopBar` <-> `SpaceMenu`) and `SpaceMenu` drag/sticky guards:
        // 7a. Viewport horizontal swipe (`row = 10`) does NOT block explicit Row 0 vertical wheel (`row = 0`)
        let t_zones = t_guards + Duration::from_secs(4);
        assert!(h.swipe_horiz(
            HorizontalSwipeDir::Next,
            10,
            VIEWPORT_HORIZ_SWIPE_THRESHOLD,
            t_zones
        ));
        assert_eq!(h.world.active_tab_index(), 2);
        let t_zones_end = t_zones + Duration::from_millis(32);
        assert!(h.vert_switch(true, t_zones_end + Duration::from_millis(60), false));
        assert_eq!(h.world.active_tab_index(), 1);

        // 7b. Row 0 vertical wheel at +60ms does NOT block `Modal::SpaceMenu` horizontal wheel at +100ms,
        //     and accepted space wheel switches clear sticky modifiers + completed selection!
        h.modal = Modal::SpaceMenu;
        h.prefix_active = true;
        h.ctrl_active = true;
        h.alt_active = true;
        let mut space_done_sel = TerminalSelection::new(1, 2);
        space_done_sel.head = (1, 8);
        space_done_sel.active = false;
        h.selection = Some(space_done_sel);
        assert!(h.swipe_horiz(
            HorizontalSwipeDir::Next,
            10,
            SPACE_MENU_HORIZ_SWIPE_THRESHOLD,
            t_zones_end + Duration::from_millis(100),
        ));
        assert_eq!(h.world.active_space().map(|s| s.id.as_str()), Some("sp2"));
        assert!(!h.prefix_active && !h.ctrl_active && !h.alt_active);
        assert!(h.selection.is_none());

        // 7c. Active `dpad_drag` / `selection.active` inside `Modal::SpaceMenu` blocks both
        //     horizontal & vertical space switches (including trailing inertia after drag release).
        let t_space_drag = t_zones_end + Duration::from_secs(1);
        assert!(!h.horiz_tick(HorizontalSwipeDir::Prev, 10, t_space_drag, true));
        assert!(!h.vert_switch(true, t_space_drag + Duration::from_millis(20), true));
        // Trailing vertical & horizontal inertia immediately after releasing D-pad in SpaceMenu stays suppressed
        assert!(!h.vert_switch(true, t_space_drag + Duration::from_millis(60), false));
        assert!(!h.horiz_tick(
            HorizontalSwipeDir::Prev,
            10,
            t_space_drag + Duration::from_millis(70),
            false,
        ));
        assert_eq!(h.world.active_space().map(|s| s.id.as_str()), Some("sp2"));

        // 7d. Switching spaces via wheel in `Modal::SpaceMenu` and immediately closing to `Modal::None`
        //     allows immediate same-direction tab swiping on `Viewport` (`SpaceMenu` <-> `ModalNone` isolation).
        let t_space_to_tab = t_space_drag + Duration::from_secs(1);
        assert!(h.swipe_horiz(
            HorizontalSwipeDir::Prev,
            10,
            SPACE_MENU_HORIZ_SWIPE_THRESHOLD,
            t_space_to_tab,
        ));
        assert_eq!(h.world.active_space().map(|s| s.id.as_str()), Some("sp1"));
        assert_eq!(h.world.active_tab_index(), 1);
        h.modal = Modal::None;
        assert!(h.swipe_horiz(
            HorizontalSwipeDir::Prev,
            10,
            VIEWPORT_HORIZ_SWIPE_THRESHOLD,
            t_space_to_tab + Duration::from_millis(40),
        ));
        assert_eq!(h.world.active_tab_index(), 0);
    }

    #[test]
    fn vertical_swipe_diagonal_arc_wobble_and_topbar_drift_never_mistrigger_tab_switch() {
        let mut h = WheelTestHarness::new();
        assert_eq!(h.world.active_tab_index(), 0);
        let t0 = Instant::now();

        // 1. Leading diagonal ticks (1..2 columns before first vertical row event) are wiped as soon as
        //    vertical scrolling starts, and both directions of thumb wobble stay suppressed.
        assert!(!h.swipe_horiz(
            HorizontalSwipeDir::Next,
            10,
            VIEWPORT_HORIZ_SWIPE_THRESHOLD - 1,
            t0,
        ));
        let t_vert0 = t0 + Duration::from_millis(34);
        h.horiz_wheel.record_viewport_vertical_wheel(t_vert0);
        for (i, dir) in [
            HorizontalSwipeDir::Next,
            HorizontalSwipeDir::Prev,
            HorizontalSwipeDir::Next,
            HorizontalSwipeDir::Prev,
        ]
        .into_iter()
        .enumerate()
        {
            assert!(!h.horiz_tick(
                dir,
                10,
                t_vert0 + Duration::from_millis(((i + 1) * 10) as u64),
                false,
            ));
        }
        assert_eq!(h.world.active_tab_index(), 0);

        // MouseDown also clears any stale partial horizontal swipe ticks
        let t_md = t_vert0 + Duration::from_secs(1);
        assert!(!h.swipe_horiz(
            HorizontalSwipeDir::Next,
            10,
            VIEWPORT_HORIZ_SWIPE_THRESHOLD - 1,
            t_md,
        ));
        h.horiz_wheel.clear_pending_horiz();
        assert!(!h.horiz_tick(
            HorizontalSwipeDir::Next,
            10,
            t_md + Duration::from_millis(32),
            false,
        ));
        assert_eq!(h.world.active_tab_index(), 0);

        // 2. Slow reading cadence (260ms between vertical ticks) keeps VERT_SCROLL_LOCK_WINDOW (350ms)
        //    continuously active, and alternating Prev/Next bleed ticks stay suppressed past 350ms.
        let t_slow = t_md + Duration::from_secs(1);
        for step_ms in [0_u64, 260, 520, 780] {
            let tv = t_slow + Duration::from_millis(step_ms);
            h.horiz_wheel.record_viewport_vertical_wheel(tv);
            let dir = if (step_ms / 260).is_multiple_of(2) {
                HorizontalSwipeDir::Next
            } else {
                HorizontalSwipeDir::Prev
            };
            assert!(!h.horiz_tick(dir, 10, tv + Duration::from_millis(40), false));
        }
        for (i, ms) in [900_u64, 990, 1080, 1170, 1260, 1350]
            .into_iter()
            .enumerate()
        {
            let dir = if i % 2 == 0 {
                HorizontalSwipeDir::Prev
            } else {
                HorizontalSwipeDir::Next
            };
            assert!(!h.horiz_tick(dir, 10, t_slow + Duration::from_millis(ms), false));
        }
        // Even after quiet gap + lock expiry, a <3-tick thumb-lift wobble never switches tabs
        let t_lift = t_slow + Duration::from_millis(1350) + HORIZ_WHEEL_QUIET_GAP;
        assert!(!h.swipe_horiz(
            HorizontalSwipeDir::Prev,
            10,
            VIEWPORT_HORIZ_SWIPE_THRESHOLD - 1,
            t_lift,
        ));
        assert_eq!(h.world.active_tab_index(), 0);

        // 3. Upward viewport swipe drifting onto Row 0 (TopBar) cannot trigger TopBar vertical or
        //    horizontal tab switches, even if the finger lingers on Row 0 past 350ms.
        let t_drift = t_lift + Duration::from_secs(2);
        h.horiz_wheel.record_viewport_vertical_wheel(t_drift);
        for ms in [30_u64, 150, 300, 450, 550] {
            assert!(!h.vert_switch(false, t_drift + Duration::from_millis(ms), false));
        }
        assert!(!h.swipe_horiz(
            HorizontalSwipeDir::Next,
            0,
            VIEWPORT_HORIZ_SWIPE_THRESHOLD,
            t_drift + Duration::from_millis(580),
        ));
        assert_eq!(h.world.active_tab_index(), 0);
        // After a real quiet pause (> VERT_SCROLL_LOCK_WINDOW), explicit Row 0 wheel & swipe work normally
        let t_row0_vert = t_drift + Duration::from_millis(1100);
        assert!(h.vert_switch(false, t_row0_vert, false));
        assert_eq!(h.world.active_tab_index(), 1);
        let t_row0_horiz = t_row0_vert + Duration::from_millis(500);
        assert!(h.swipe_horiz(
            HorizontalSwipeDir::Prev,
            0,
            TOP_BAR_HORIZ_SWIPE_THRESHOLD,
            t_row0_horiz,
        ));
        assert_eq!(h.world.active_tab_index(), 0);

        // 4. Mid-inertia single opposite glitch tick neither switches back nor breaks Next inertia
        //    coalescing, and <3 Prev rebound ticks do not reverse, whereas the 3rd Prev tick does.
        let t_rebound = t_row0_horiz + Duration::from_secs(2);
        assert!(h.swipe_horiz(
            HorizontalSwipeDir::Next,
            10,
            VIEWPORT_HORIZ_SWIPE_THRESHOLD,
            t_rebound,
        ));
        assert_eq!(h.world.active_tab_index(), 1);
        let t_r_end = t_rebound + Duration::from_millis(32);
        assert!(!h.horiz_tick(
            HorizontalSwipeDir::Prev,
            10,
            t_r_end + Duration::from_millis(40),
            false,
        ));
        for ms in [60_u64, 100, 150] {
            assert!(!h.horiz_tick(
                HorizontalSwipeDir::Next,
                10,
                t_r_end + Duration::from_millis(ms),
                false,
            ));
        }
        assert!(!h.swipe_horiz(
            HorizontalSwipeDir::Prev,
            10,
            VIEWPORT_HORIZ_SWIPE_THRESHOLD - 1,
            t_r_end + Duration::from_millis(170),
        ));
        assert_eq!(h.world.active_tab_index(), 1);
        assert!(h.horiz_tick(
            HorizontalSwipeDir::Prev,
            10,
            t_r_end + Duration::from_millis(186),
            false,
        ));
        assert_eq!(h.world.active_tab_index(), 0);

        // 5. Stale partial nudges spaced >= HORIZ_SWIPE_ACCUM_WINDOW apart never accumulate to threshold
        let t_stale = t_r_end + Duration::from_secs(2);
        for offset_ms in [0_u64, 300, 600] {
            assert!(!h.swipe_horiz(
                HorizontalSwipeDir::Next,
                10,
                2,
                t_stale + Duration::from_millis(offset_ms),
            ));
        }
        assert_eq!(h.world.active_tab_index(), 0);

        // 6. Cross-row horizontal swipe starting in Viewport (row 1) and drifting onto TopBar (row 0)
        //    retains the 3-tick Viewport threshold and switches cleanly on the 3rd tick.
        let t_cross = t_stale + Duration::from_secs(2);
        assert!(!h.horiz_tick(HorizontalSwipeDir::Next, 1, t_cross, false));
        assert!(!h.horiz_tick(
            HorizontalSwipeDir::Next,
            0,
            t_cross + Duration::from_millis(8),
            false,
        ));
        assert_eq!(h.world.active_tab_index(), 0);
        assert!(h.horiz_tick(
            HorizontalSwipeDir::Next,
            0,
            t_cross + Duration::from_millis(16),
            false,
        ));
        assert_eq!(h.world.active_tab_index(), 1);
    }

    #[test]
    fn cbt_deferred_wrap_bs_cub_and_vertical_motions_match_xterm_ghostty() {
        // 1. CBT (`CSI Ps Z`) steps backward across 8-column tab stops (0, 8, 16, 24)
        let mut att = AttachedTerminal::new(1, 24, 5);
        att.process_vt_bytes(b"\x1b[1;19H*\x1b[ZA\x1b[2ZB\x1b[99ZC");
        let scr = att.parser.screen();
        // `1;19H` -> col 18 ('*'), after '*' col is 19 -> `\x1b[Z` lands on 16 ('A', col->17),
        // `\x1b[2Z` -> 16 then 8 ('B', col->9), `\x1b[99Z` -> 0 ('C').
        assert_eq!(scr.cell(0, 18).unwrap().contents(), "*");
        assert_eq!(scr.cell(0, 16).unwrap().contents(), "A");
        assert_eq!(scr.cell(0, 8).unwrap().contents(), "B");
        assert_eq!(scr.cell(0, 0).unwrap().contents(), "C");

        // CBT from deferred-wrap (`col == cols == 24`, visual col 23) lands on tab stop 16
        let mut att_wrap_cbt = AttachedTerminal::new(2, 24, 3);
        att_wrap_cbt.process_vt_bytes(&b"x".repeat(24));
        att_wrap_cbt.process_vt_bytes(b"\x1b[Z!");
        assert_eq!(
            att_wrap_cbt.parser.screen().cell(0, 16).unwrap().contents(),
            "!"
        );
        assert_eq!(att_wrap_cbt.parser.screen().cursor_position().0, 0);

        // 2. BS (`0x08`) and CUB (`CSI Ps D`) from deferred-wrap subtract from `cols - 1`, not `cols`
        let mut att_bs = AttachedTerminal::new(3, 10, 3);
        att_bs.process_vt_bytes(b"0123456789\x08X");
        assert_eq!(
            att_bs.parser.screen().contents_between(0, 0, 0, 10),
            "01234567X9"
        );
        assert_eq!(att_bs.parser.screen().cursor_position().0, 0);

        let mut att_cub = AttachedTerminal::new(4, 10, 3);
        att_cub.process_vt_bytes(b"0123456789\x1b[3DY");
        assert_eq!(
            att_cub.parser.screen().contents_between(0, 0, 0, 10),
            "012345Y789"
        );
        assert_eq!(att_cub.parser.screen().cursor_position().0, 0);

        // 3. Bare LF / IND (`ESC D`) / NEL (`ESC E`) / CUU (`CSI A`) clear pending_wrap
        let mut att_vert = AttachedTerminal::new(5, 10, 4);
        att_vert.process_vt_bytes(b"0123456789\x1bDZ\x1bEOK\x1b[2;10H!\x1b[A?");
        let scr_v = att_vert.parser.screen();
        assert_eq!(scr_v.cell(0, 9).unwrap().contents(), "?");
        assert_eq!(scr_v.cell(1, 9).unwrap().contents(), "!");
        assert_eq!(scr_v.contents_between(2, 0, 2, 2), "OK");

        // 4. RI (`ESC M`) at row 0 outside DECSTBM (`\x1b[2;4r`) must NOT scroll rows 1..=3
        let mut att_ri = AttachedTerminal::new(6, 10, 4);
        att_ri.process_vt_bytes(b"\x1b[1;1HT\x1b[2;1HR1\x1b[3;1HR2\x1b[2;4r\x1b[1;1H\x1bM");
        let scr_ri = att_ri.parser.screen();
        assert_eq!(scr_ri.contents_between(0, 0, 0, 1), "T");
        assert_eq!(scr_ri.contents_between(1, 0, 1, 2), "R1");
        assert_eq!(scr_ri.contents_between(2, 0, 2, 2), "R2");
    }

    #[test]
    fn decawm_mode_7_clamps_narrow_wide_and_rep_at_right_margin_without_wrapping() {
        // 1. Narrow & REP overflow under `?7l` (including compound `?7;25l`) stays on row 0
        let mut att = AttachedTerminal::new(1, 10, 3);
        att.process_vt_bytes("\x1b[?7;25l─\x1b[25bZ\x1b[?7;25h".as_bytes());
        let scr = att.parser.screen();
        assert_eq!(scr.cursor_position(), (0, 9));
        assert_eq!(scr.contents_between(0, 0, 0, 10), "─────────Z");
        assert_eq!(scr.contents_between(1, 0, 1, 10), "");

        // 2. Wide CJK exact fit at `cols - 2..=cols - 1` and wide overflow at `cols - 1` under `?7l`
        let mut att_wide = AttachedTerminal::new(2, 10, 3);
        // 5 CJK chars = 10 cols exact fit + 1 extra CJK char that must NOT wrap to row 1
        let wide_line = b"\x1b[?7l\xe4\xb8\xad\xe6\x96\x87\xe6\xb5\x8b\xe8\xaf\x95\xe8\xa1\x8c\xe5\xa4\x9a\x1b[?7h";
        att_wide.process_vt_bytes(wide_line);
        assert_eq!(att_wide.parser.screen().cursor_position(), (0, 9));
        assert_eq!(
            att_wide.parser.screen().contents_between(0, 0, 0, 10),
            "中文测试行"
        );
        assert_eq!(att_wide.parser.screen().contents_between(1, 0, 1, 10), "");

        // 1 ASCII + 5 CJK chars = 11 cols: 5th CJK arrives at col 9 (`cols - 1`) and must NOT wrap
        let mut att_odd = AttachedTerminal::new(3, 10, 3);
        for b in
            b"\x1b[?7lA\xe4\xb8\xad\xe6\x96\x87\xe6\xb5\x8b\xe8\xaf\x95\xe8\xa1\x8c\x1b(B\x1b[?7h"
        {
            att_odd.process_vt_bytes(std::slice::from_ref(b));
        }
        assert_eq!(att_odd.parser.screen().cursor_position(), (0, 9));
        assert_eq!(
            att_odd.parser.screen().contents_between(0, 0, 0, 9),
            "A中文测试"
        );
        assert_eq!(att_odd.parser.screen().contents_between(1, 0, 1, 10), "");

        // 3. `putCellLR` at bottom-right cell `(cols - 1, rows - 1)` never scrolls or sets pending_wrap
        let mut att_lr = AttachedTerminal::new(4, 10, 3);
        att_lr.process_vt_bytes("TOP\r\nMID\r\nBOT\x1b[3;10H\x1b[?7l╯\x1b[?7h".as_bytes());
        assert_eq!(att_lr.parser.screen().cursor_position(), (2, 9));
        assert_eq!(att_lr.parser.screen().contents_between(0, 0, 0, 3), "TOP");
        assert_eq!(att_lr.parser.screen().cell(2, 9).unwrap().contents(), "╯");

        // 4. DECSC/DECRC (`\x1b7`/`\x1b8`) and DECSTR (`\x1b[!p`) preserve/reset DECAWM accurately
        let mut att_save = AttachedTerminal::new(5, 8, 3);
        att_save.process_vt_bytes(b"\x1b[?7l\x1b7\x1b[?7h\x1b81234567890\x1b[?7h");
        assert_eq!(att_save.parser.screen().cursor_position(), (0, 7));
        assert_eq!(
            att_save.parser.screen().contents_between(0, 0, 0, 8),
            "12345670"
        );
        att_save.process_vt_bytes(b"\x1b[?7l\x1b[!p\r123456789");
        assert_eq!(att_save.parser.screen().cursor_position().0, 1);

        // 5. Right-margin combining marks (`\u{0301}`) attach to the base cell under `?7l`
        //    (both single-chunk and 1-byte-at-a-time), and 1-/2-column viewports never underflow.
        let comb_stream = "\x1b[?7l123456789e\u{0301}\u{0327}\x1b[?7h".as_bytes();
        let mut att_comb_whole = AttachedTerminal::new(6, 10, 2);
        att_comb_whole.process_vt_bytes(comb_stream);
        let mut att_comb_split = AttachedTerminal::new(7, 10, 2);
        for b in comb_stream {
            att_comb_split.process_vt_bytes(std::slice::from_ref(b));
        }
        for att_c in [&att_comb_whole, &att_comb_split] {
            let scr_c = att_c.parser.screen();
            assert_eq!(scr_c.cursor_position(), (0, 9));
            assert_eq!(scr_c.cell(0, 9).unwrap().contents(), "e\u{0301}\u{0327}");
            assert_eq!(scr_c.contents_between(1, 0, 1, 10), "");
        }

        // 2-column and 1-column viewports with wide characters + combining marks under `?7l`
        let mut att_two_col = AttachedTerminal::new(8, 2, 2);
        att_two_col.process_vt_bytes("\x1b[?7l中\u{0301}文\u{0302}\x1b[?7h".as_bytes());
        assert_eq!(att_two_col.parser.screen().cursor_position(), (0, 1));
        assert_eq!(
            att_two_col.parser.screen().cell(0, 0).unwrap().contents(),
            "中\u{0301}"
        );
        assert_eq!(att_two_col.parser.screen().contents_between(1, 0, 1, 2), "");

        let mut att_one_col = AttachedTerminal::new(9, 1, 2);
        att_one_col.process_vt_bytes("\x1b[?7lA中\u{0302}\x1b[?7h".as_bytes());
        assert_eq!(att_one_col.parser.screen().cursor_position(), (0, 0));
        assert_eq!(
            att_one_col.parser.screen().cell(0, 0).unwrap().contents(),
            "A"
        );
        assert_eq!(att_one_col.parser.screen().contents_between(1, 0, 1, 1), "");
    }

    #[test]
    fn mode_2027_multi_codepoint_emoji_clusters_align_to_two_columns_across_byte_splits() {
        // Every Mode 2027 emoji cluster below occupies 2 columns in Ghostty / ultraviolet:
        // 👨‍💻 (ZWJ), 👍🏻 (skin tone), 🏳️‍🌈 (VS16+ZWJ), 🤦🏻‍♂️ (skin+ZWJ+gender+VS16),
        // 1️⃣ (keycap), 🇨🇳 (flag pair), ☝🏻 (width-1+skin).
        let frame = concat!(
            "\x1b]0;👨‍💻 ⚙️ title\x07",
            "\x1b[H",
            "│👨‍💻│👍🏻│\r\n",
            "│🏳️‍🌈│🤦🏻‍♂️│\r\n",
            "│1️⃣│🇨🇳│\r\n",
            "│☝🏻│⚙️│"
        )
        .as_bytes();

        let mut att_whole = AttachedTerminal::new(1, 16, 4);
        att_whole.process_vt_bytes(frame);

        let mut att_split = AttachedTerminal::new(2, 16, 4);
        for b in frame {
            att_split.process_vt_bytes(std::slice::from_ref(b));
        }

        for scr in [att_whole.parser.screen(), att_split.parser.screen()] {
            for r in 0..4 {
                assert_eq!(
                    scr.cell(r, 0).unwrap().contents(),
                    "│",
                    "row {r} col 0 border"
                );
                assert_eq!(
                    scr.cell(r, 3).unwrap().contents(),
                    "│",
                    "row {r} col 3 border"
                );
                assert_eq!(
                    scr.cell(r, 6).unwrap().contents(),
                    "│",
                    "row {r} col 6 border"
                );
            }
        }
    }

    #[test]
    fn inline_relative_cursor_autoscroll_never_duplicates_lines_on_cbt_decawm_and_cjk() {
        // Reproduces Bubble Tea v2 `googleCursedRenderer` + `ultraviolet` inline relative-cursor
        // rendering & `writePrepend` auto-scrolling on a 24x6 viewport with CJK text, Mode 2027
        // multi-codepoint emojis, `CBT` (`\x1b[Z`/`\x1b[2Z`), `?7l`/`?7h`, `REP`, and `putCellLR`.
        let mut stream = Vec::new();

        // Initial inline frame (6 rows: rows 0..=5), leaving cursor at bottom-right `(23, 5)`.
        stream.extend_from_slice(
            concat!(
                "╭──────────────────────╮\r\n",
                "\x1b[?7l│ 第一行：你好世界 👨‍💻  │\x1b[?7h\x1b[24G\r\n",
                "\x1b[?7l│ 第二行：状态正常 👍🏻  │\x1b[?7h\x1b[24G\r\n",
                "\x1b[?7l│ 第三行：继续输出 🏳️‍🌈  │\x1b[?7h\x1b[24G\r\n",
                "│                 [OK] │\x1b[2Zstatus\r\n",
                "╰\x1b[?7l─\x1b[21b╯\x1b[?7h"
            )
            .as_bytes(),
        );

        // Auto-scroll tick 1 (`writePrepend`): relative `\r\x1b[5A` from `cur.Y = 5` back to row 0,
        // prepend scrolled-off top border + line 1 into scrollback, clear downward, repaint 6 rows.
        stream.extend_from_slice(
            concat!(
                "\r\x1b[5A",
                "╭──────────────────────╮\x1b[K\r\n",
                "│ 第一行：你好世界 👨‍💻  │\x1b[K\r\n",
                "\x1b[J",
                "\x1b[?7l│ 第二行：状态正常 👍🏻  │\x1b[?7h\x1b[24G\r\n",
                "\x1b[?7l│ 第三行：继续输出 🏳️‍🌈  │\x1b[?7h\x1b[24G\r\n",
                "\x1b[?7l│ 第四行：步骤 1️⃣ 完成 │\x1b[?7h\x1b[24G\r\n",
                "\x1b[?7l│ 第五行：排版对齐 ⚙️  │\x1b[?7h\x1b[24G\r\n",
                "│                 [OK] │\x1b[2Zready!\r\n",
                "╰\x1b[?7l─\x1b[21b╯\x1b[?7h"
            )
            .as_bytes(),
        );

        // Auto-scroll tick 2 (`writePrepend`): relative `\r\x1b[5A` again, prepend line 2, repaint.
        stream.extend_from_slice(
            concat!(
                "\r\x1b[5A",
                "│ 第二行：状态正常 👍🏻  │\x1b[K\r\n",
                "\x1b[J",
                "\x1b[?7l│ 第三行：继续输出 🏳️‍🌈  │\x1b[?7h\x1b[24G\r\n",
                "\x1b[?7l│ 第四行：步骤 1️⃣ 完成 │\x1b[?7h\x1b[24G\r\n",
                "\x1b[?7l│ 第五行：排版对齐 ⚙️  │\x1b[?7h\x1b[24G\r\n",
                "\x1b[?7l│ 第六行：没有重复行！ │\x1b[?7h\x1b[24G\r\n",
                "│                 [OK] │\x1b[2Zdone!!\r\n",
                "╰\x1b[?7l─\x1b[21b╯\x1b[?7h"
            )
            .as_bytes(),
        );

        let mut att_whole = AttachedTerminal::new(1, 24, 6);
        att_whole.process_vt_bytes(&stream);

        let mut att_split = AttachedTerminal::new(2, 24, 6);
        for b in &stream {
            att_split.process_vt_bytes(std::slice::from_ref(b));
        }

        for att in [&mut att_whole, &mut att_split] {
            let scr = att.parser.screen();
            assert_eq!(scr.cursor_position(), (5, 23));
            for r in 0..5 {
                assert_eq!(scr.cell(r, 0).unwrap().contents(), "│", "row {r} left");
                assert_eq!(scr.cell(r, 23).unwrap().contents(), "│", "row {r} right");
            }
            assert_eq!(scr.cell(5, 0).unwrap().contents(), "╰");
            assert_eq!(scr.cell(5, 23).unwrap().contents(), "╯");
            assert!(scr.contents_between(0, 0, 0, 24).contains("第三行"));
            assert!(scr.contents_between(1, 0, 1, 24).contains("第四行"));
            assert!(scr.contents_between(2, 0, 2, 24).contains("第五行"));
            assert!(scr.contents_between(3, 0, 3, 24).contains("第六行"));
            assert_eq!(
                scr.contents_between(4, 0, 4, 24),
                "│       done!!    [OK] │"
            );

            // Inspect scrollback: exactly 3 lines scrolled off, with zero duplicate rows!
            att.parser.screen_mut().set_scrollback(100);
            let sb = att.parser.screen();
            assert_eq!(
                sb.scrollback(),
                3,
                "exact scrollback count without extra wraps"
            );
            assert_eq!(sb.contents_between(0, 0, 0, 24), "╭──────────────────────╮");
            assert!(sb.contents_between(1, 0, 1, 24).contains("第一行"));
            assert!(sb.contents_between(2, 0, 2, 24).contains("第二行"));
        }
    }

    #[test]
    fn space_slop_tap_opens_space_menu_on_stationary_top_left_tap_only() {
        let slop = Some(Rect {
            x: 5,
            y: 1,
            width: 10,
            height: 2,
        });

        // Stationary tap inside `space_slop_rect` (Row 1 or Row 2) clears sticky modifiers and opens SpaceMenu
        let mut modal = Modal::None;
        let mut prefix_active = true;
        let mut ctrl_active = true;
        let mut alt_active = true;
        assert!(try_activate_space_slop_tap(
            false,
            6,
            2,
            6,
            2,
            slop,
            &mut modal,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
        ));
        assert_eq!(modal, Modal::SpaceMenu);
        assert!(!prefix_active && !ctrl_active && !alt_active);

        // Cancelled text-selection drag returning to anchor (`was_dragged = true`, even pre-attach)
        // does NOT open SpaceMenu
        let mut modal = Modal::None;
        assert!(!try_activate_space_slop_tap(
            true,
            6,
            1,
            6,
            1,
            slop,
            &mut modal,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
        ));
        assert_eq!(modal, Modal::None);

        // Dragging straight upward from Row 1 onto Row 0 (clamped back to anchor) still marks `dragged = true`
        // so returning to the anchor on release does NOT open SpaceMenu!
        let mut up_drag_sel = TerminalSelection::new(0, 6);
        let drag_rect = Rect::new(0, 1, 80, 22);
        assert!(!up_drag_sel.update_drag(drag_rect, 6, 0));
        assert!(up_drag_sel.dragged);
        let mut modal = Modal::None;
        assert!(!try_activate_space_slop_tap(
            up_drag_sel.dragged,
            6,
            1,
            6,
            1,
            slop,
            &mut modal,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
        ));
        assert_eq!(modal, Modal::None);

        // Releasing at a different cell even without an intermediate Drag event does NOT open SpaceMenu
        assert!(!try_activate_space_slop_tap(
            false,
            6,
            1,
            7,
            1,
            slop,
            &mut modal,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
        ));

        // Anchor or release outside `space_slop_rect`, or active modal, does NOT open SpaceMenu
        assert!(!try_activate_space_slop_tap(
            false,
            6,
            3,
            6,
            2,
            slop,
            &mut modal,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
        ));
        assert!(!try_activate_space_slop_tap(
            false,
            6,
            1,
            15,
            1,
            slop,
            &mut modal,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
        ));
        let mut help_modal = Modal::Help;
        assert!(!try_activate_space_slop_tap(
            false,
            6,
            1,
            6,
            1,
            slop,
            &mut help_modal,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
        ));
        assert_eq!(help_modal, Modal::Help);
    }

    #[test]
    fn tab_overview_supports_linear_topbar_wheel_filter_navigation_and_shortcuts() {
        let mut h = WheelTestHarness::new();
        let mut t = Instant::now();
        let step = Duration::from_millis(500);

        // 1. TopBar vertical wheel walks linearly: Tab 0 clamps Up, Down steps 0 -> 1 -> 2 -> TabOverview -> clamp
        assert!(!h.vert_switch(true, t, false));
        assert_eq!(h.world.active_tab_index(), 0);
        assert_eq!(h.modal, Modal::None);

        t += step;
        assert!(h.vert_switch(false, t, false));
        assert_eq!(h.world.active_tab_index(), 1);

        t += step;
        assert!(h.vert_switch(false, t, false));
        assert_eq!(h.world.active_tab_index(), 2);

        t += step;
        assert!(h.vert_switch(false, t, false));
        assert!(matches!(h.modal, Modal::TabOverview { .. }));

        t += step;
        assert!(!h.vert_switch(false, t, false));
        assert!(matches!(h.modal, Modal::TabOverview { .. }));

        // TopBar Up from TabOverview exits back to last tab (2), then walks 2 -> 1 -> 0 -> clamp
        t += step;
        assert!(h.vert_switch(true, t, false));
        assert_eq!(h.modal, Modal::None);
        assert_eq!(h.world.active_tab_index(), 2);

        t += step;
        assert!(h.vert_switch(true, t, false));
        assert_eq!(h.world.active_tab_index(), 1);
        t += step;
        assert!(h.vert_switch(true, t, false));
        assert_eq!(h.world.active_tab_index(), 0);
        t += step;
        assert!(!h.vert_switch(true, t, false));
        assert_eq!(h.world.active_tab_index(), 0);

        // 2. Single-tab Space (`sp2`): Prev clamps at 0, Next enters TabOverview, Prev returns to 0
        h.world.next_space();
        assert_eq!(h.world.active_space().map(|s| s.id.as_str()), Some("sp2"));
        assert_eq!(h.world.active_tab_index(), 0);
        t += step;
        assert!(!h.vert_switch(true, t, false));
        assert_eq!(h.modal, Modal::None);
        t += step;
        assert!(h.vert_switch(false, t, false));
        assert!(matches!(h.modal, Modal::TabOverview { .. }));
        t += step;
        assert!(h.vert_switch(true, t, false));
        assert_eq!(h.modal, Modal::None);
        assert_eq!(h.world.active_tab_index(), 0);

        // Return to 3-tab Space (`sp1`) and verify keyboard shortcuts step linearly into TabOverview even without -e
        h.world.prev_space();
        h.world.select_tab_index(0);
        let mut sidebar_open = false;
        let mut scrollback_offset = 0;

        // Alt+Left and ^b p clamp at Tab 0 (never wrap to Tab 2)
        handle_key_event(
            KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
            &h.core,
            &mut h.world,
            None,
            "ctrl+b",
            &mut sidebar_open,
            &mut h.prefix_active,
            &mut h.ctrl_active,
            &mut h.alt_active,
            &mut scrollback_offset,
            &mut h.modal,
            h.term_rect,
        )
        .unwrap();
        assert_eq!(h.world.active_tab_index(), 0);
        assert_eq!(h.modal, Modal::None);

        for kev in [
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
        ] {
            handle_key_event(
                kev,
                &h.core,
                &mut h.world,
                None,
                "ctrl+b",
                &mut sidebar_open,
                &mut h.prefix_active,
                &mut h.ctrl_active,
                &mut h.alt_active,
                &mut scrollback_offset,
                &mut h.modal,
                h.term_rect,
            )
            .unwrap();
        }
        assert_eq!(h.world.active_tab_index(), 0);
        assert_eq!(h.modal, Modal::None);

        // Alt+Right steps 0 -> 1, ^b n steps 1 -> 2, and Alt+Right at Tab 2 enters TabOverview
        handle_key_event(
            KeyEvent::new(KeyCode::Right, KeyModifiers::ALT),
            &h.core,
            &mut h.world,
            None,
            "ctrl+b",
            &mut sidebar_open,
            &mut h.prefix_active,
            &mut h.ctrl_active,
            &mut h.alt_active,
            &mut scrollback_offset,
            &mut h.modal,
            h.term_rect,
        )
        .unwrap();
        assert_eq!(h.world.active_tab_index(), 1);
        assert_eq!(h.modal, Modal::None);

        for kev in [
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
        ] {
            handle_key_event(
                kev,
                &h.core,
                &mut h.world,
                None,
                "ctrl+b",
                &mut sidebar_open,
                &mut h.prefix_active,
                &mut h.ctrl_active,
                &mut h.alt_active,
                &mut scrollback_offset,
                &mut h.modal,
                h.term_rect,
            )
            .unwrap();
        }
        assert_eq!(h.world.active_tab_index(), 2);
        assert_eq!(h.modal, Modal::None);

        handle_key_event(
            KeyEvent::new(KeyCode::Right, KeyModifiers::ALT),
            &h.core,
            &mut h.world,
            None,
            "ctrl+b",
            &mut sidebar_open,
            &mut h.prefix_active,
            &mut h.ctrl_active,
            &mut h.alt_active,
            &mut scrollback_offset,
            &mut h.modal,
            h.term_rect,
        )
        .unwrap();
        assert!(matches!(h.modal, Modal::TabOverview { .. }));
        assert_eq!(h.world.active_tab_index(), 2);

        // ^b p exits TabOverview back to Tab 2; ^b n at Tab 2 re-enters TabOverview; Alt+Left exits back to Tab 2
        for kev in [
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
        ] {
            handle_key_event(
                kev,
                &h.core,
                &mut h.world,
                None,
                "ctrl+b",
                &mut sidebar_open,
                &mut h.prefix_active,
                &mut h.ctrl_active,
                &mut h.alt_active,
                &mut scrollback_offset,
                &mut h.modal,
                h.term_rect,
            )
            .unwrap();
        }
        assert_eq!(h.modal, Modal::None);
        assert_eq!(h.world.active_tab_index(), 2);

        for kev in [
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
        ] {
            handle_key_event(
                kev,
                &h.core,
                &mut h.world,
                None,
                "ctrl+b",
                &mut sidebar_open,
                &mut h.prefix_active,
                &mut h.ctrl_active,
                &mut h.alt_active,
                &mut scrollback_offset,
                &mut h.modal,
                h.term_rect,
            )
            .unwrap();
        }
        assert!(matches!(h.modal, Modal::TabOverview { .. }));

        handle_key_event(
            KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
            &h.core,
            &mut h.world,
            None,
            "ctrl+b",
            &mut sidebar_open,
            &mut h.prefix_active,
            &mut h.ctrl_active,
            &mut h.alt_active,
            &mut scrollback_offset,
            &mut h.modal,
            h.term_rect,
        )
        .unwrap();
        assert_eq!(h.modal, Modal::None);
        assert_eq!(h.world.active_tab_index(), 2);

        h.world.select_tab_index(0);
        assert_eq!(h.world.active_tab_index(), 0);
        assert_eq!(h.modal, Modal::None);

        // 3. Enter TabOverview from Tab 2, test filter typing, paste, selection navigation, Left guard, Esc & Enter
        h.world.select_tab_index(2);
        t += step;
        assert!(h.vert_switch(false, t, false));
        assert!(matches!(
            h.modal,
            Modal::TabOverview {
                selected: 0,
                selected_target: OverviewSelection::NewTab,
                ..
            }
        ));

        // Typing "tw" filters and auto-selects tab `tb2`
        for ch in ['t', 'w'] {
            handle_key_event(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &h.core,
                &mut h.world,
                None,
                "ctrl+b",
                &mut sidebar_open,
                &mut h.prefix_active,
                &mut h.ctrl_active,
                &mut h.alt_active,
                &mut scrollback_offset,
                &mut h.modal,
                h.term_rect,
            )
            .unwrap();
        }
        assert_eq!(
            h.modal,
            Modal::TabOverview {
                query: "tw".to_string(),
                selected: 1,
                selected_target: OverviewSelection::Tab("tb2".to_string()),
            }
        );

        // Left key does NOT exit while query is non-empty
        handle_key_event(
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            &h.core,
            &mut h.world,
            None,
            "ctrl+b",
            &mut sidebar_open,
            &mut h.prefix_active,
            &mut h.ctrl_active,
            &mut h.alt_active,
            &mut scrollback_offset,
            &mut h.modal,
            h.term_rect,
        )
        .unwrap();
        assert!(matches!(h.modal, Modal::TabOverview { .. }));

        // `paste_text_into_app` appends stripped text to filter query
        paste_text_into_app(
            "o\n",
            &h.core,
            &h.world,
            None,
            &mut scrollback_offset,
            &mut h.modal,
        );
        assert_eq!(
            h.modal,
            Modal::TabOverview {
                query: "two".to_string(),
                selected: 1,
                selected_target: OverviewSelection::Tab("tb2".to_string()),
            }
        );

        // Enter opens the selected tab (`tb2`, index 1) and closes TabOverview
        handle_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &h.core,
            &mut h.world,
            None,
            "ctrl+b",
            &mut sidebar_open,
            &mut h.prefix_active,
            &mut h.ctrl_active,
            &mut h.alt_active,
            &mut scrollback_offset,
            &mut h.modal,
            h.term_rect,
        )
        .unwrap();
        assert_eq!(h.modal, Modal::None);
        assert_eq!(h.world.active_tab_index(), 1);

        // Re-enter TabOverview from Tab 2, test Backspace reset + Left exit with empty query
        h.world.select_tab_index(2);
        h.modal = Modal::TabOverview {
            query: "o".to_string(),
            selected: 1,
            selected_target: OverviewSelection::Tab("tb1".to_string()),
        };
        handle_key_event(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &h.core,
            &mut h.world,
            None,
            "ctrl+b",
            &mut sidebar_open,
            &mut h.prefix_active,
            &mut h.ctrl_active,
            &mut h.alt_active,
            &mut scrollback_offset,
            &mut h.modal,
            h.term_rect,
        )
        .unwrap();
        assert_eq!(
            h.modal,
            Modal::TabOverview {
                query: String::new(),
                selected: 0,
                selected_target: OverviewSelection::NewTab,
            }
        );

        // Down / wheel navigation steps into tab list, and Left (with empty query) exits to last tab (2)
        assert!(scroll_tab_overview_selection(&h.world, &mut h.modal, false));
        assert_eq!(
            h.modal,
            Modal::TabOverview {
                query: String::new(),
                selected: 1,
                selected_target: OverviewSelection::Tab("tb1".to_string()),
            }
        );
        handle_key_event(
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            &h.core,
            &mut h.world,
            None,
            "ctrl+b",
            &mut sidebar_open,
            &mut h.prefix_active,
            &mut h.ctrl_active,
            &mut h.alt_active,
            &mut scrollback_offset,
            &mut h.modal,
            h.term_rect,
        )
        .unwrap();
        assert_eq!(h.modal, Modal::None);
        assert_eq!(h.world.active_tab_index(), 2);

        // Esc from TabOverview when on an earlier tab also exits cleanly back to the last tab (2)
        h.world.select_tab_index(0);
        h.modal = Modal::default_tab_overview();
        handle_key_event(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &h.core,
            &mut h.world,
            None,
            "ctrl+b",
            &mut sidebar_open,
            &mut h.prefix_active,
            &mut h.ctrl_active,
            &mut h.alt_active,
            &mut scrollback_offset,
            &mut h.modal,
            h.term_rect,
        )
        .unwrap();
        assert_eq!(h.modal, Modal::None);
        assert_eq!(h.world.active_tab_index(), 2);

        // Alt+Right and ^b n while on TabOverview clamp in TabOverview (never wrap to Tab 0)
        h.modal = Modal::default_tab_overview();
        handle_key_event(
            KeyEvent::new(KeyCode::Right, KeyModifiers::ALT),
            &h.core,
            &mut h.world,
            None,
            "ctrl+b",
            &mut sidebar_open,
            &mut h.prefix_active,
            &mut h.ctrl_active,
            &mut h.alt_active,
            &mut scrollback_offset,
            &mut h.modal,
            h.term_rect,
        )
        .unwrap();
        assert!(matches!(h.modal, Modal::TabOverview { .. }));

        for kev in [
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
        ] {
            handle_key_event(
                kev,
                &h.core,
                &mut h.world,
                None,
                "ctrl+b",
                &mut sidebar_open,
                &mut h.prefix_active,
                &mut h.ctrl_active,
                &mut h.alt_active,
                &mut scrollback_offset,
                &mut h.modal,
                h.term_rect,
            )
            .unwrap();
        }
        assert!(matches!(h.modal, Modal::TabOverview { .. }));

        // Out-of-range Alt+9 on TabOverview keeps TabOverview open; valid Alt+2 exits and selects Tab 1
        h.modal = Modal::default_tab_overview();
        handle_key_event(
            KeyEvent::new(KeyCode::Char('9'), KeyModifiers::ALT),
            &h.core,
            &mut h.world,
            None,
            "ctrl+b",
            &mut sidebar_open,
            &mut h.prefix_active,
            &mut h.ctrl_active,
            &mut h.alt_active,
            &mut scrollback_offset,
            &mut h.modal,
            h.term_rect,
        )
        .unwrap();
        assert!(matches!(h.modal, Modal::TabOverview { .. }));
        assert_eq!(h.world.active_tab_index(), 2);

        handle_key_event(
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::ALT),
            &h.core,
            &mut h.world,
            None,
            "ctrl+b",
            &mut sidebar_open,
            &mut h.prefix_active,
            &mut h.ctrl_active,
            &mut h.alt_active,
            &mut scrollback_offset,
            &mut h.modal,
            h.term_rect,
        )
        .unwrap();
        assert_eq!(h.modal, Modal::None);
        assert_eq!(h.world.active_tab_index(), 1);
    }

    #[test]
    fn resolve_eink_mode_respects_cli_config_and_env_precedence() {
        let env_map = |pairs: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            move |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
        };

        // CLI flags override config and env
        assert!(resolve_eink_mode(
            true,
            false,
            Some(false),
            env_map(&[("HCT_EINK", "0")])
        ));
        assert!(!resolve_eink_mode(
            false,
            true,
            Some(true),
            env_map(&[("HCT_EINK", "1"), ("LC_TERMINAL_PROFILE", "eink")])
        ));

        // Config overrides env
        assert!(resolve_eink_mode(
            false,
            false,
            Some(true),
            env_map(&[("HCT_EINK", "0")])
        ));
        assert!(!resolve_eink_mode(
            false,
            false,
            Some(false),
            env_map(&[("LC_TERMINAL_PROFILE", "eink")])
        ));

        // HCT_EINK explicit booleans take precedence over profile auto-detection
        assert!(resolve_eink_mode(
            false,
            false,
            None,
            env_map(&[("HCT_EINK", "true")])
        ));
        assert!(resolve_eink_mode(
            false,
            false,
            None,
            env_map(&[("HCT_EINK", "yes")])
        ));
        assert!(!resolve_eink_mode(
            false,
            false,
            None,
            env_map(&[("HCT_EINK", "no"), ("LC_TERMINAL_PROFILE", "eink")])
        ));

        // Profile/theme/term auto-detection
        assert!(resolve_eink_mode(
            false,
            false,
            None,
            env_map(&[("LC_TERMINAL_PROFILE", "eink")])
        ));
        assert!(resolve_eink_mode(
            false,
            false,
            None,
            env_map(&[("HCT_THEME", "E-Ink")])
        ));
        assert!(resolve_eink_mode(
            false,
            false,
            None,
            env_map(&[("TERM", "xterm-monochrome")])
        ));
        assert!(!resolve_eink_mode(
            false,
            false,
            None,
            env_map(&[("TERM", "xterm-256color")])
        ));

        // `-e` short flag activates `--eink` and conflicts with `--no-eink`
        let cli_short = Cli::try_parse_from(["hct", "-e"]).unwrap();
        assert!(cli_short.eink);
        assert!(Cli::try_parse_from(["hct", "-e", "--no-eink"]).is_err());
    }

    #[test]
    fn resolve_eink_color_mode_and_combined_precedence() {
        let env_map = |pairs: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            move |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
        };

        // `--eink-color` activates both E-Ink mode and Kaleido color accent, overriding config & env
        assert_eq!(
            resolve_eink_and_color_modes(
                false,
                false,
                true,
                false,
                Some(false),
                Some(false),
                env_map(&[("HCT_EINK", "0"), ("HCT_EINK_COLOR", "0")]),
            ),
            (true, true)
        );

        // `--no-eink-color` forces monochrome even when `-e` or `ui.eink_color = true` or env is set
        assert_eq!(
            resolve_eink_and_color_modes(
                true,
                false,
                false,
                true,
                None,
                Some(true),
                env_map(&[("HCT_EINK_COLOR", "1")]),
            ),
            (true, false)
        );

        // Config `ui.eink_color = Some(true)` alone activates E-Ink + Kaleido unless `ui.eink = Some(false)`
        assert_eq!(
            resolve_eink_and_color_modes(
                false,
                false,
                false,
                false,
                None,
                Some(true),
                env_map(&[])
            ),
            (true, true)
        );
        assert_eq!(
            resolve_eink_and_color_modes(
                false,
                false,
                false,
                false,
                Some(false),
                Some(true),
                env_map(&[]),
            ),
            (false, true)
        );

        // Environment detection (`HCT_EINK_COLOR`, `LC_TERMINAL_PROFILE`, `HCT_THEME`)
        assert_eq!(
            resolve_eink_and_color_modes(
                false,
                false,
                false,
                false,
                None,
                None,
                env_map(&[("HCT_EINK_COLOR", "1")]),
            ),
            (true, true)
        );
        assert_eq!(
            resolve_eink_and_color_modes(
                false,
                false,
                false,
                false,
                None,
                None,
                env_map(&[("LC_TERMINAL_PROFILE", "eink-color")]),
            ),
            (true, true)
        );
        assert_eq!(
            resolve_eink_and_color_modes(
                false,
                false,
                false,
                false,
                None,
                None,
                env_map(&[("HCT_EINK", "0"), ("HCT_EINK_COLOR", "1")]),
            ),
            (false, true)
        );
        assert_eq!(
            resolve_eink_and_color_modes(
                false,
                false,
                false,
                false,
                None,
                None,
                env_map(&[("HCT_THEME", "kaleido")]),
            ),
            (true, true)
        );

        // CLI flag validation
        let cli_color = Cli::try_parse_from(["hct", "--eink-color"]).unwrap();
        assert!(cli_color.eink_color);
        assert!(Cli::try_parse_from(["hct", "--eink-color", "--no-eink-color"]).is_err());
        assert!(Cli::try_parse_from(["hct", "--eink-color", "--no-eink"]).is_err());
    }

    #[test]
    fn toggle_eink_color_shortcut_switches_modes_cleanly() {
        let core = CoreClient::dummy_for_tests();
        let mut world = World::default();
        let mut sidebar_open = false;
        let mut prefix_active = false;
        let mut ctrl_active = false;
        let mut alt_active = false;
        let mut scrollback_offset = 0;
        let mut modal = Modal::Help;
        let term_rect = Rect::new(0, 1, 80, 22);
        let mut eink_mode = false;
        let mut eink_color = false;
        let mut force_clear = false;

        let mut send_prefix_key = |ch: char,
                                   modal: &mut Modal,
                                   eink_mode: &mut bool,
                                   eink_color: &mut bool,
                                   force_clear: &mut bool| {
            let _ = handle_key_event_full(
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
                &core,
                &mut world,
                None,
                "ctrl+b",
                &mut sidebar_open,
                &mut prefix_active,
                &mut ctrl_active,
                &mut alt_active,
                &mut scrollback_offset,
                modal,
                term_rect,
                term_rect,
                eink_mode,
                eink_color,
                force_clear,
                None,
            )
            .unwrap();
            handle_key_event_full(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &core,
                &mut world,
                None,
                "ctrl+b",
                &mut sidebar_open,
                &mut prefix_active,
                &mut ctrl_active,
                &mut alt_active,
                &mut scrollback_offset,
                modal,
                term_rect,
                term_rect,
                eink_mode,
                eink_color,
                force_clear,
                None,
            )
            .unwrap()
        };

        // 1. Dark -> `^b E` enters Kaleido E-Ink directly and keeps Help modal open
        let out1 = send_prefix_key(
            'E',
            &mut modal,
            &mut eink_mode,
            &mut eink_color,
            &mut force_clear,
        );
        assert_eq!(out1, KeyOutcome::UiChanged);
        assert!(eink_mode);
        assert!(eink_color);
        assert!(force_clear);
        assert!(matches!(modal, Modal::Help));

        // 2. Kaleido -> `^b E` returns to pure Monochrome E-Ink
        force_clear = false;
        let _ = send_prefix_key(
            'E',
            &mut modal,
            &mut eink_mode,
            &mut eink_color,
            &mut force_clear,
        );
        assert!(eink_mode);
        assert!(!eink_color);
        assert!(force_clear);

        // 3. Monochrome -> `^b E` turns Kaleido back ON
        let _ = send_prefix_key(
            'E',
            &mut modal,
            &mut eink_mode,
            &mut eink_color,
            &mut force_clear,
        );
        assert!(eink_mode);
        assert!(eink_color);

        // 4. `^b e` turns E-Ink OFF while remembering `eink_color`, then back ON restores Kaleido
        let _ = send_prefix_key(
            'e',
            &mut modal,
            &mut eink_mode,
            &mut eink_color,
            &mut force_clear,
        );
        assert!(!eink_mode);
        assert!(eink_color);
        let _ = send_prefix_key(
            'e',
            &mut modal,
            &mut eink_mode,
            &mut eink_color,
            &mut force_clear,
        );
        assert!(eink_mode);
        assert!(eink_color);
    }

    #[test]
    fn resolve_eink_interval_and_wheel_step_behave_accurately() {
        assert_eq!(
            resolve_eink_interval(None, None),
            Duration::from_millis(250)
        );
        assert_eq!(
            resolve_eink_interval(Some(0), Some(400)),
            Duration::from_millis(0)
        );
        assert_eq!(
            resolve_eink_interval(None, Some(350)),
            Duration::from_millis(350)
        );
        assert_eq!(
            resolve_eink_interval(Some(9000), None),
            Duration::from_millis(2000)
        );

        assert_eq!(main_scrollback_wheel_step(24, false), 3);
        assert_eq!(main_scrollback_wheel_step(24, true), 12);
        assert_eq!(main_scrollback_wheel_step(4, true), 3);
    }

    #[test]
    fn eink_shortcuts_and_frame_pacing_promote_real_changes_and_elide_spinners() {
        let core = CoreClient::dummy_for_tests();
        let mut world = World::default();
        let mut sidebar_open = false;
        let mut prefix_active = false;
        let mut ctrl_active = false;
        let mut alt_active = false;
        let mut scrollback_offset = 0usize;
        let mut modal = Modal::None;
        let term_rect = Rect::new(0, 1, 80, 22);
        let mut eink_mode = false;
        let mut force_clear = false;

        let ctrl_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        let mut press = |kev,
                         world: &mut World,
                         prefix_active: &mut bool,
                         ctrl_active: &mut bool,
                         alt_active: &mut bool,
                         scrollback_offset: &mut usize,
                         modal: &mut Modal,
                         eink_mode: &mut bool,
                         force_clear: &mut bool| {
            handle_key_event_with_eink(
                kev,
                &core,
                world,
                None,
                "ctrl+b",
                &mut sidebar_open,
                prefix_active,
                ctrl_active,
                alt_active,
                scrollback_offset,
                modal,
                term_rect,
                eink_mode,
                force_clear,
            )
            .unwrap()
        };

        // ^b e toggles E-Ink ON and requests force_clear
        press(
            ctrl_b,
            &mut world,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut scrollback_offset,
            &mut modal,
            &mut eink_mode,
            &mut force_clear,
        );
        press(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut world,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut scrollback_offset,
            &mut modal,
            &mut eink_mode,
            &mut force_clear,
        );
        assert!(eink_mode);
        assert!(force_clear);

        // ^b r triggers clean redraw in E-Ink mode
        force_clear = false;
        press(
            ctrl_b,
            &mut world,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut scrollback_offset,
            &mut modal,
            &mut eink_mode,
            &mut force_clear,
        );
        press(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &mut world,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut scrollback_offset,
            &mut modal,
            &mut eink_mode,
            &mut force_clear,
        );
        assert!(force_clear);

        // ^b PageUp / PageDown jump by (term_rect.height - 2)
        press(
            ctrl_b,
            &mut world,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut scrollback_offset,
            &mut modal,
            &mut eink_mode,
            &mut force_clear,
        );
        press(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            &mut world,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut scrollback_offset,
            &mut modal,
            &mut eink_mode,
            &mut force_clear,
        );
        assert_eq!(scrollback_offset, 20);
        press(
            ctrl_b,
            &mut world,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut scrollback_offset,
            &mut modal,
            &mut eink_mode,
            &mut force_clear,
        );
        press(
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            &mut world,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut scrollback_offset,
            &mut modal,
            &mut eink_mode,
            &mut force_clear,
        );
        assert_eq!(scrollback_offset, 0);

        // ^b R still opens RenameSpace prompt
        world.doc.spaces.push(crate::model::SharedSpace {
            id: "sp1".to_string(),
            name: "main".to_string(),
            ..Default::default()
        });
        press(
            ctrl_b,
            &mut world,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut scrollback_offset,
            &mut modal,
            &mut eink_mode,
            &mut force_clear,
        );
        press(
            KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT),
            &mut world,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut scrollback_offset,
            &mut modal,
            &mut eink_mode,
            &mut force_clear,
        );
        assert!(matches!(
            modal,
            Modal::Prompt {
                kind: PromptKind::RenameSpace { .. },
                ..
            }
        ));

        // Frame pacing & zero-diff spinner elision
        let mut attached_map: BTreeMap<String, AttachedTerminal> = BTreeMap::new();
        let mut att = AttachedTerminal::new(1, term_rect.width, term_rect.height);
        att.process_vt_bytes("\x1b[H⠋ Thinking...".as_bytes());
        let baseline_snap = capture_eink_screen_snapshot(att.parser.screen(), term_rect, 0);
        attached_map.insert("term-1".to_string(), att);
        let last_snap = Some(("term-1".to_string(), baseline_snap));

        // Spinner advance before interval -> held in pty_dirty
        attached_map
            .get_mut("term-1")
            .unwrap()
            .process_vt_bytes("\x1b[H⠙ Thinking...".as_bytes());
        let t0 = Instant::now();
        let mut last_draw_at = Some(t0);
        let mut pty_dirty = true;
        let mut eink_pacer = EinkInputPacer::default();
        let mut dirty = false;
        let interval = Duration::from_millis(250);
        promote_due_pty_dirty(
            true,
            interval,
            t0 + Duration::from_millis(50),
            &mut pty_dirty,
            &mut eink_pacer,
            &mut dirty,
            &mut last_draw_at,
            &last_snap,
            Some("term-1"),
            &mut attached_map,
            term_rect,
            &mut scrollback_offset,
            &Modal::None,
            &None,
        );
        assert!(pty_dirty);
        assert!(!dirty);

        // Once interval elapses, identical spinner snapshot is elided cleanly
        let t1 = t0 + Duration::from_millis(300);
        promote_due_pty_dirty(
            true,
            interval,
            t1,
            &mut pty_dirty,
            &mut eink_pacer,
            &mut dirty,
            &mut last_draw_at,
            &last_snap,
            Some("term-1"),
            &mut attached_map,
            term_rect,
            &mut scrollback_offset,
            &Modal::None,
            &None,
        );
        assert!(!pty_dirty);
        assert!(!dirty);
        assert_eq!(last_draw_at, Some(t1));

        // Real text update after interval promotes dirty = true
        attached_map
            .get_mut("term-1")
            .unwrap()
            .process_vt_bytes(b"\x1b[2;1HDone");
        pty_dirty = true;
        promote_due_pty_dirty(
            true,
            interval,
            t1 + Duration::from_millis(300),
            &mut pty_dirty,
            &mut eink_pacer,
            &mut dirty,
            &mut last_draw_at,
            &last_snap,
            Some("term-1"),
            &mut attached_map,
            term_rect,
            &mut scrollback_offset,
            &Modal::None,
            &None,
        );
        assert!(dirty);
    }

    #[test]
    fn eink_forwarded_pty_input_bypasses_passive_frame_pacing() {
        let core = CoreClient::dummy_for_tests();
        let mut world = World::default();
        let mut sidebar_open = false;
        let mut prefix_active = false;
        let mut ctrl_active = false;
        let mut alt_active = false;
        let mut scrollback_offset = 0usize;
        let mut modal = Modal::None;
        let term_rect = Rect::new(0, 1, 80, 22);
        let mut eink_mode = true;
        let mut force_clear = false;
        let interval = Duration::from_millis(2000);

        let mut attached_map: BTreeMap<String, AttachedTerminal> = BTreeMap::new();
        let att = AttachedTerminal::new(9, term_rect.width, term_rect.height);
        let baseline_snap = capture_eink_screen_snapshot(att.parser.screen(), term_rect, 0);
        attached_map.insert("term-1".to_string(), att);
        let mut last_snap = Some(("term-1".to_string(), baseline_snap));

        // 1. Regular key forwarded to PTY records last_pty_input on that channel
        let outcome = {
            let att_ref = attached_map.get("term-1");
            handle_key_event_with_eink(
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
                &core,
                &mut world,
                att_ref,
                "ctrl+b",
                &mut sidebar_open,
                &mut prefix_active,
                &mut ctrl_active,
                &mut alt_active,
                &mut scrollback_offset,
                &mut modal,
                term_rect,
                &mut eink_mode,
                &mut force_clear,
            )
            .unwrap()
        };
        assert_eq!(outcome, KeyOutcome::ForwardedToPty);
        let inp1 = core
            .last_pty_input()
            .expect("send_input must record LastPtyInput");
        assert_eq!(inp1.channel, 9);

        // Even if last draw was 10ms before keypress and interval is 2000ms, echo promotes immediately
        let draw0 = inp1
            .at
            .checked_sub(Duration::from_millis(10))
            .unwrap_or(inp1.at);
        let t_echo1 = draw0 + Duration::from_millis(12);
        let mut last_draw_at = Some(draw0);
        let mut pty_dirty = true;
        let mut dirty = false;
        let mut eink_pacer = EinkInputPacer::default();
        attached_map
            .get_mut("term-1")
            .unwrap()
            .process_vt_bytes(b"a");
        eink_pacer.record_active_pty_data(9, t_echo1, core.last_pty_input());
        assert_eq!(
            eink_pacer.next_deadline(pty_dirty, true, interval, t_echo1, last_draw_at),
            Some(t_echo1)
        );
        promote_due_pty_dirty(
            true,
            interval,
            t_echo1,
            &mut pty_dirty,
            &mut eink_pacer,
            &mut dirty,
            &mut last_draw_at,
            &last_snap,
            Some("term-1"),
            &mut attached_map,
            term_rect,
            &mut scrollback_offset,
            &Modal::None,
            &None,
        );
        assert!(dirty);

        // Simulate completing draw after first echo
        dirty = false;
        eink_pacer.on_promoted_or_drawn();
        last_draw_at = Some(t_echo1);
        last_snap = Some((
            "term-1".to_string(),
            capture_eink_screen_snapshot(
                attached_map.get("term-1").unwrap().parser.screen(),
                term_rect,
                0,
            ),
        ));

        // 2. Rapid follow-up highlight/autosuggestion chunk within 200ms schedules at 16ms, not 2000ms
        let t_follow = t_echo1 + Duration::from_millis(5);
        attached_map
            .get_mut("term-1")
            .unwrap()
            .process_vt_bytes(b"bc");
        pty_dirty = true;
        eink_pacer.record_active_pty_data(9, t_follow, core.last_pty_input());
        assert_eq!(
            eink_pacer.next_deadline(pty_dirty, true, interval, t_follow, last_draw_at),
            Some(t_echo1 + EINK_INPUT_FOLLOWUP_INTERVAL)
        );
        promote_due_pty_dirty(
            true,
            interval,
            t_follow,
            &mut pty_dirty,
            &mut eink_pacer,
            &mut dirty,
            &mut last_draw_at,
            &last_snap,
            Some("term-1"),
            &mut attached_map,
            term_rect,
            &mut scrollback_offset,
            &Modal::None,
            &None,
        );
        assert!(!dirty);
        let t_follow_due = t_echo1 + EINK_INPUT_FOLLOWUP_INTERVAL;
        promote_due_pty_dirty(
            true,
            interval,
            t_follow_due,
            &mut pty_dirty,
            &mut eink_pacer,
            &mut dirty,
            &mut last_draw_at,
            &last_snap,
            Some("term-1"),
            &mut attached_map,
            term_rect,
            &mut scrollback_offset,
            &Modal::None,
            &None,
        );
        assert!(dirty);

        // Complete follow-up draw and capture updated snapshot
        dirty = false;
        eink_pacer.on_promoted_or_drawn();
        last_draw_at = Some(t_follow_due);
        last_snap = Some((
            "term-1".to_string(),
            capture_eink_screen_snapshot(
                attached_map.get("term-1").unwrap().parser.screen(),
                term_rect,
                0,
            ),
        ));

        // 3. Invisible zero-diff chunk after next keypress must NOT consume last_drawn_input_seq
        let outcome2 = {
            let att_ref = attached_map.get("term-1");
            handle_key_event_with_eink(
                KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
                &core,
                &mut world,
                att_ref,
                "ctrl+b",
                &mut sidebar_open,
                &mut prefix_active,
                &mut ctrl_active,
                &mut alt_active,
                &mut scrollback_offset,
                &mut modal,
                term_rect,
                &mut eink_mode,
                &mut force_clear,
            )
            .unwrap()
        };
        assert_eq!(outcome2, KeyOutcome::ForwardedToPty);
        let inp2 = core.last_pty_input().unwrap();
        assert_ne!(inp2.seq, inp1.seq);

        let t_noop = inp2.at + Duration::from_millis(1);
        attached_map
            .get_mut("term-1")
            .unwrap()
            .process_vt_bytes(b"\x1b[?25l\x1b[?25h");
        pty_dirty = true;
        eink_pacer.record_active_pty_data(9, t_noop, core.last_pty_input());
        promote_due_pty_dirty(
            true,
            interval,
            t_noop,
            &mut pty_dirty,
            &mut eink_pacer,
            &mut dirty,
            &mut last_draw_at,
            &last_snap,
            Some("term-1"),
            &mut attached_map,
            term_rect,
            &mut scrollback_offset,
            &Modal::None,
            &None,
        );
        assert!(!pty_dirty);
        assert!(!dirty);

        // Real cursor movement 3ms later still gets 0ms immediate promotion
        let t_cursor = inp2.at + Duration::from_millis(4);
        attached_map
            .get_mut("term-1")
            .unwrap()
            .process_vt_bytes(b"\x1b[D");
        pty_dirty = true;
        eink_pacer.record_active_pty_data(9, t_cursor, core.last_pty_input());
        assert_eq!(
            eink_pacer.next_deadline(pty_dirty, true, interval, t_cursor, last_draw_at),
            Some(t_cursor)
        );
        promote_due_pty_dirty(
            true,
            interval,
            t_cursor,
            &mut pty_dirty,
            &mut eink_pacer,
            &mut dirty,
            &mut last_draw_at,
            &last_snap,
            Some("term-1"),
            &mut attached_map,
            term_rect,
            &mut scrollback_offset,
            &Modal::None,
            &None,
        );
        assert!(dirty);

        // 4. Passive output 300ms after input (> 200ms window) is throttled by full eink_interval
        dirty = false;
        pty_dirty = true;
        eink_pacer.on_promoted_or_drawn();
        last_draw_at = Some(t_cursor);
        let t_passive = inp2.at + Duration::from_millis(300);
        attached_map
            .get_mut("term-1")
            .unwrap()
            .process_vt_bytes(b"\r\npassive");
        eink_pacer.record_active_pty_data(9, t_passive, core.last_pty_input());
        assert_eq!(
            eink_pacer.next_deadline(pty_dirty, true, interval, t_passive, last_draw_at),
            Some(t_cursor + interval)
        );
        promote_due_pty_dirty(
            true,
            interval,
            t_passive,
            &mut pty_dirty,
            &mut eink_pacer,
            &mut dirty,
            &mut last_draw_at,
            &last_snap,
            Some("term-1"),
            &mut attached_map,
            term_rect,
            &mut scrollback_offset,
            &Modal::None,
            &None,
        );
        assert!(pty_dirty);
        assert!(!dirty);
    }

    #[test]
    fn eink_agent_settlement_detects_completion_after_mark_active_seen() {
        use crate::model::{make_single_terminal_tab, SharedSpace, TerminalInfo};

        let core = CoreClient::dummy_for_tests();
        let mut world = World::default();
        world.doc.spaces.push(SharedSpace {
            id: "sp1".to_string(),
            name: "main".to_string(),
            tabs: vec![make_single_terminal_tab("tab1", "agent", "t1")],
            ..Default::default()
        });
        world.terminals.insert(
            "t1".to_string(),
            TerminalInfo {
                address: "t1".to_string(),
                status_word: Some("working".to_string()),
                turn: 1,
                ..Default::default()
            },
        );

        let cur_for = |w: &World| {
            w.active_terminal_address()
                .and_then(|a| w.terminals.get(&a))
                .map(|i| (i.address.clone(), i.agent_state()))
        };

        let prev_working = cur_for(&world);
        assert_eq!(prev_working, Some(("t1".to_string(), AgentState::Working)));

        // Normal completion (`working` -> `idle`) followed immediately by `mark_active_seen`
        {
            let info = world.terminals.get_mut("t1").unwrap();
            info.status_word = Some("idle".to_string());
            info.turn = 2;
        }
        let cur_unseen = cur_for(&world);
        assert_eq!(cur_unseen, Some(("t1".to_string(), AgentState::Done)));
        assert!(detect_eink_agent_settlement(
            prev_working.as_ref(),
            cur_unseen.as_ref()
        ));

        world.mark_active_seen(&core);
        let cur_idle_seen = cur_for(&world);
        assert_eq!(cur_idle_seen, Some(("t1".to_string(), AgentState::Idle)));
        assert!(detect_eink_agent_settlement(
            prev_working.as_ref(),
            cur_idle_seen.as_ref()
        ));

        // Cleared `status_word = None` after `mark_active_seen` resolves to `AgentState::None`
        {
            let info = world.terminals.get_mut("t1").unwrap();
            info.status_word = None;
            info.turn = 3;
        }
        world.mark_active_seen(&core);
        let cur_none_seen = cur_for(&world);
        assert_eq!(cur_none_seen, Some(("t1".to_string(), AgentState::None)));
        assert!(detect_eink_agent_settlement(
            prev_working.as_ref(),
            cur_none_seen.as_ref()
        ));

        // Transition to Blocked also settles
        let cur_blocked = Some(("t1".to_string(), AgentState::Blocked));
        assert!(detect_eink_agent_settlement(
            prev_working.as_ref(),
            cur_blocked.as_ref()
        ));

        // Non-settlements return false
        assert!(!detect_eink_agent_settlement(
            prev_working.as_ref(),
            prev_working.as_ref()
        ));
        assert!(!detect_eink_agent_settlement(
            cur_idle_seen.as_ref(),
            prev_working.as_ref()
        ));
        assert!(!detect_eink_agent_settlement(
            prev_working.as_ref(),
            Some(&("t2".to_string(), AgentState::Idle))
        ));
        assert!(!detect_eink_agent_settlement(prev_working.as_ref(), None));
    }

    #[test]
    fn eink_page_turn_touch_tap_tolerates_thumb_jitter_and_preserves_drag_selection() {
        let left_zone = Some(Rect::new(0, 7, 8, 8));
        let right_zone = Some(Rect::new(92, 7, 8, 8));

        // Expanded touch zones hit across 8 cols x 8 rows while leaving middle viewport clear
        assert_eq!(
            match_eink_page_touch_zone(left_zone, right_zone, 0, 10),
            Some(HorizontalSwipeDir::Prev)
        );
        assert_eq!(
            match_eink_page_touch_zone(left_zone, right_zone, 7, 14),
            Some(HorizontalSwipeDir::Prev)
        );
        assert_eq!(
            match_eink_page_touch_zone(left_zone, right_zone, 92, 7),
            Some(HorizontalSwipeDir::Next)
        );
        assert_eq!(
            match_eink_page_touch_zone(left_zone, right_zone, 99, 14),
            Some(HorizontalSwipeDir::Next)
        );
        assert_eq!(
            match_eink_page_touch_zone(left_zone, right_zone, 20, 10),
            None
        );

        // Edge taps follow PR #57 linear tab navigation:
        // Prev clamps at Tab 0 (never wraps), Next advances past last tab into TabOverview,
        // and Prev / ExitTabOverview returns cleanly to the last tab.
        let core = CoreClient::dummy_for_tests();
        let mut world = World::default();
        world.doc.spaces = vec![crate::model::SharedSpace {
            id: "sp1".to_string(),
            name: "work".to_string(),
            folder: String::new(),
            tabs: vec![
                crate::model::make_single_terminal_tab("t1", "tab1", "term1"),
                crate::model::make_single_terminal_tab("t2", "tab2", "term2"),
                crate::model::make_single_terminal_tab("t3", "tab3", "term3"),
            ],
            rest: serde_json::Map::new(),
        }];
        world
            .active_tab_by_space
            .insert("sp1".to_string(), "t1".to_string());
        let mut modal = Modal::None;
        let term_rect = Rect::new(0, 1, 100, 22);

        let left_dir = match_eink_page_touch_zone(left_zone, right_zone, 0, 10).unwrap();
        assert!(!step_linear_tab_or_overview(
            left_dir, &core, &mut world, &mut modal
        ));
        assert_eq!(world.active_tab_index(), 0);
        assert_eq!(modal, Modal::None);

        let right_dir = match_eink_page_touch_zone(left_zone, right_zone, 99, 10).unwrap();
        assert!(step_linear_tab_or_overview(
            right_dir, &core, &mut world, &mut modal
        ));
        assert_eq!(world.active_tab_index(), 1);
        assert!(step_linear_tab_or_overview(
            right_dir, &core, &mut world, &mut modal
        ));
        assert_eq!(world.active_tab_index(), 2);
        assert_eq!(modal, Modal::None);

        // Past the last tab -> enters trailing Modal::TabOverview
        assert!(step_linear_tab_or_overview(
            right_dir, &core, &mut world, &mut modal
        ));
        assert!(matches!(modal, Modal::TabOverview { .. }));

        // Next while on TabOverview clamps
        assert!(!step_linear_tab_or_overview(
            right_dir, &core, &mut world, &mut modal
        ));
        assert!(matches!(modal, Modal::TabOverview { .. }));

        // Prev / [‹] on TabOverview exits back to the last tab (Tab 2)
        assert!(step_linear_tab_or_overview(
            left_dir, &core, &mut world, &mut modal
        ));
        assert_eq!(modal, Modal::None);
        assert_eq!(world.active_tab_index(), 2);

        assert!(step_linear_tab_or_overview(
            right_dir, &core, &mut world, &mut modal
        ));
        assert!(matches!(modal, Modal::TabOverview { .. }));
        exit_tab_overview_to_last_tab(&core, &mut world, &mut modal);
        assert_eq!(modal, Modal::None);
        assert_eq!(world.active_tab_index(), 2);

        // Tap slop absorbs ±1 col / ±1 row thumb jitter and promotes >= 2 cell drags
        let tap = PendingPageTurnTap {
            dir: HorizontalSwipeDir::Prev,
            start_col: 4,
            start_row: 10,
        };
        assert!(page_tap_within_slop(&tap, 4, 10));
        assert!(page_tap_within_slop(&tap, 5, 11));
        assert!(page_tap_within_slop(&tap, 3, 9));
        assert!(!page_tap_within_slop(&tap, 6, 10));
        assert!(!page_tap_within_slop(&tap, 4, 12));

        // Dragging out from col 0 inside the edge zone hands off to TerminalSelection at col 0
        let mut att = AttachedTerminal::new(1, term_rect.width, term_rect.height);
        att.process_vt_bytes(b"\x1b[10;1HSeeThrough line");
        let start_tap = PendingPageTurnTap {
            dir: HorizontalSwipeDir::Prev,
            start_col: 0,
            start_row: 10,
        };
        assert!(!page_tap_within_slop(&start_tap, 9, 10));
        let rel_col = start_tap.start_col.saturating_sub(term_rect.x);
        let rel_row = start_tap.start_row.saturating_sub(term_rect.y);
        let mut sel = TerminalSelection::new(rel_row, rel_col);
        assert!(sel.update_drag(term_rect, 9, 10));
        assert_eq!(sel.extract_text(att.parser.screen()), "SeeThrough");
    }

    fn reader_test_world() -> World {
        use crate::model::{make_single_terminal_tab, SharedSpace, TerminalInfo};

        let mut world = World::default();
        world.terminals.insert(
            "t1".to_string(),
            TerminalInfo {
                address: "t1".to_string(),
                title: "demo".to_string(),
                status_word: Some("idle".to_string()),
                status_app: Some("codex".to_string()),
                ..Default::default()
            },
        );
        world.terminals.insert(
            "t2".to_string(),
            TerminalInfo {
                address: "t2".to_string(),
                title: "shell".to_string(),
                ..Default::default()
            },
        );
        world.doc.spaces.push(SharedSpace {
            id: "s1".to_string(),
            name: "main".to_string(),
            folder: String::new(),
            tabs: vec![
                make_single_terminal_tab("tab1", "agy", "t1"),
                make_single_terminal_tab("tab2", "sh", "t2"),
            ],
            rest: Default::default(),
        });
        world
    }

    #[test]
    fn book_reader_shortcut_opens_reader_modal_and_ignores_shift_b() {
        let core = CoreClient::dummy_for_tests();
        let mut world = reader_test_world();
        let mut sidebar_open = false;
        let mut prefix_active = false;
        let mut ctrl_active = false;
        let mut alt_active = false;
        let mut scrollback_offset = 0;
        let mut modal = Modal::Help;
        let term_rect = Rect::new(0, 1, 80, 22);
        let mut eink_mode = true;
        let mut eink_color = false;
        let mut force_clear = false;
        let mut pending_reader_rpc: Option<PendingReaderRpc> = None;

        let ctrl_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        for key in [
            ctrl_b,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
        ] {
            let out = handle_key_event_full(
                key,
                &core,
                &mut world,
                None,
                "ctrl+b",
                &mut sidebar_open,
                &mut prefix_active,
                &mut ctrl_active,
                &mut alt_active,
                &mut scrollback_offset,
                &mut modal,
                term_rect,
                term_rect,
                &mut eink_mode,
                &mut eink_color,
                &mut force_clear,
                Some(&mut pending_reader_rpc),
            )
            .unwrap();
            assert_eq!(out, KeyOutcome::UiChanged);
        }
        assert!(matches!(modal, Modal::Reader { .. }));
        assert!(pending_reader_rpc.is_none());

        // `^b B` is unbound and does not leave or mutate `Modal::Reader`
        for key in [
            ctrl_b,
            KeyEvent::new(KeyCode::Char('B'), KeyModifiers::SHIFT),
        ] {
            let out = handle_key_event_full(
                key,
                &core,
                &mut world,
                None,
                "ctrl+b",
                &mut sidebar_open,
                &mut prefix_active,
                &mut ctrl_active,
                &mut alt_active,
                &mut scrollback_offset,
                &mut modal,
                term_rect,
                term_rect,
                &mut eink_mode,
                &mut eink_color,
                &mut force_clear,
                Some(&mut pending_reader_rpc),
            )
            .unwrap();
            assert_eq!(out, KeyOutcome::UiChanged);
        }
        assert!(matches!(modal, Modal::Reader { .. }));
    }

    #[test]
    fn large_osc52_sequence_chunks_tmux_and_screen_dcs_cleanly_without_duplication() {
        let large_text = "x".repeat(47_500);
        let expected_b64 = base64_encode(large_text.as_bytes());
        assert!(expected_b64.len() > 63_000);
        let expected_osc = format!("\x1b]52;c;{expected_b64}\x07");

        // 1. tmux: chunked into <=77-byte DCS frames with doubled ESC and no trailing raw OSC
        let tmux_seq = format_osc52_sequence(&expected_b64, true, false);
        assert!(tmux_seq.starts_with("\x1bPtmux;\x1b\x1b]52;c;"));
        assert!(tmux_seq.ends_with("\x1b\\"));
        let mut tmux_frames = 0usize;
        let mut tmux_unwrapped = String::with_capacity(expected_osc.len());
        let mut rest = tmux_seq.as_str();
        while !rest.is_empty() {
            let body_plus = rest
                .strip_prefix("\x1bPtmux;")
                .expect("every segment in tmux passthrough must start with ESC P tmux;");
            let end_idx = body_plus
                .find("\x1b\\")
                .expect("every tmux DCS chunk must end with ST");
            let frame_inner = &body_plus[..end_idx];
            assert!(
                frame_inner.len() <= 78,
                "tmux DCS chunk unexpectedly large: {}",
                frame_inner.len()
            );
            tmux_unwrapped.push_str(&frame_inner.replace("\x1b\x1b", "\x1b"));
            tmux_frames += 1;
            rest = &body_plus[end_idx + 2..];
        }
        assert!(tmux_frames > 800, "expected >800 chunks, got {tmux_frames}");
        assert_eq!(tmux_unwrapped, expected_osc);

        // 2. screen: chunked into <=76-byte DCS frames and reconstructs exact OSC
        let screen_seq = format_osc52_sequence(&expected_b64, false, true);
        assert!(screen_seq.starts_with("\x1bP\x1b]52;c;"));
        assert!(screen_seq.ends_with("\x1b\\"));
        let mut screen_frames = 0usize;
        let mut screen_unwrapped = String::with_capacity(expected_osc.len());
        let mut rest_scr = screen_seq.as_str();
        while !rest_scr.is_empty() {
            let body_plus = rest_scr
                .strip_prefix("\x1bP")
                .expect("every segment in screen passthrough must start with ESC P");
            let end_idx = body_plus
                .find("\x1b\\")
                .expect("every screen DCS chunk must end with ST");
            let frame_inner = &body_plus[..end_idx];
            assert!(frame_inner.len() <= 76);
            screen_unwrapped.push_str(frame_inner);
            screen_frames += 1;
            rest_scr = &body_plus[end_idx + 2..];
        }
        assert!(
            screen_frames > 800,
            "expected >800 chunks, got {screen_frames}"
        );
        assert_eq!(screen_unwrapped, expected_osc);

        // Inner ESC anywhere inside an OSC body is also doubled per tmux DCS chunk.
        let inner_esc_tmux = format_single_osc_sequence("52;c;a\x1bb", true, false);
        assert_eq!(
            inner_esc_tmux,
            "\x1bPtmux;\x1b\x1b]52;c;a\x1b\x1bb\x07\x1b\\"
        );
    }

    #[test]
    fn reader_modal_keyboard_navigation_toggles_refresh_and_exit() {
        let core = CoreClient::dummy_for_tests();
        let mut world = reader_test_world();
        let mut attached = AttachedTerminal::new(1, 60, 24);
        let mut payload = String::from("> explain carefully\r\n");
        payload.push_str("\u{25cb} Thought for 2s\r\n");
        for i in 1..=35 {
            payload.push_str(&format!("Paragraph {i}: readable response line.\r\n\r\n"));
        }
        attached.process_vt_bytes(payload.as_bytes());

        let term_rect = Rect::new(0, 1, 60, 14);
        let mut sidebar_open = false;
        let mut prefix_active = false;
        let mut ctrl_active = false;
        let mut alt_active = false;
        let mut scrollback_offset = 0;
        let mut eink_mode = false;
        let mut eink_color = false;
        let mut force_clear = false;
        let mut pending_reader_rpc: Option<PendingReaderRpc> = None;
        let mut modal = build_reader_modal_state(
            &core,
            &world,
            Some(&attached),
            term_rect,
            true,
            Some(&mut pending_reader_rpc),
        );

        let initial_scroll = match modal {
            Modal::Reader {
                ref doc, scroll, ..
            } => {
                assert!(doc.is_agent);
                assert!(doc.clean_mode);
                assert!(doc.hidden_process_lines >= 1);
                scroll
            }
            _ => panic!("expected Modal::Reader"),
        };

        macro_rules! send_key {
            ($code:expr, $mods:expr) => {
                handle_key_event_full(
                    KeyEvent::new($code, $mods),
                    &core,
                    &mut world,
                    Some(&attached),
                    "ctrl+b",
                    &mut sidebar_open,
                    &mut prefix_active,
                    &mut ctrl_active,
                    &mut alt_active,
                    &mut scrollback_offset,
                    &mut modal,
                    term_rect,
                    term_rect,
                    &mut eink_mode,
                    &mut eink_color,
                    &mut force_clear,
                    Some(&mut pending_reader_rpc),
                )
                .unwrap()
            };
        }

        // Line step down/up, End/Home, Latest anchor
        assert_eq!(
            send_key!(KeyCode::Char('j'), KeyModifiers::NONE),
            KeyOutcome::UiChanged
        );
        let after_j = match modal {
            Modal::Reader { scroll, .. } => scroll,
            _ => panic!("expected Modal::Reader"),
        };
        assert!(after_j > initial_scroll);
        send_key!(KeyCode::Char('k'), KeyModifiers::NONE);
        match modal {
            Modal::Reader { scroll, .. } => assert_eq!(scroll, initial_scroll),
            _ => panic!("expected Modal::Reader"),
        }
        send_key!(KeyCode::Char('G'), KeyModifiers::SHIFT);
        let at_bottom = match modal {
            Modal::Reader { scroll, .. } => scroll,
            _ => panic!("expected Modal::Reader"),
        };
        assert!(at_bottom >= after_j);
        send_key!(KeyCode::Char('g'), KeyModifiers::NONE);
        match modal {
            Modal::Reader { scroll, .. } => assert_eq!(scroll, 0),
            _ => panic!("expected Modal::Reader"),
        }
        send_key!(KeyCode::Char(' '), KeyModifiers::NONE);
        let after_space = match modal {
            Modal::Reader { scroll, .. } => scroll,
            _ => panic!("expected Modal::Reader"),
        };
        assert!(after_space > 0);
        send_key!(KeyCode::Char('b'), KeyModifiers::NONE);
        match modal {
            Modal::Reader { scroll, .. } => assert_eq!(scroll, 0),
            _ => panic!("expected Modal::Reader"),
        }
        send_key!(KeyCode::Char('l'), KeyModifiers::NONE);
        match modal {
            Modal::Reader { scroll, .. } => assert_eq!(scroll, initial_scroll),
            _ => panic!("expected Modal::Reader"),
        }

        // Toggle Clean <-> Full via 'c', refresh via 'r'
        send_key!(KeyCode::Char('c'), KeyModifiers::NONE);
        match modal {
            Modal::Reader { ref doc, .. } => {
                assert!(!doc.clean_mode);
                assert_eq!(doc.hidden_process_lines, 0);
            }
            _ => panic!("expected Modal::Reader"),
        }

        attached.process_vt_bytes(b"fresh line 36\r\n");
        send_key!(KeyCode::Char('r'), KeyModifiers::NONE);
        match modal {
            Modal::Reader {
                ref doc,
                ref raw_text,
                ..
            } => {
                assert!(!doc.clean_mode); // refresh preserves Clean/Full setting
                assert!(raw_text.contains("fresh line 36"));
            }
            _ => panic!("expected Modal::Reader"),
        }

        // Prefix ^b PgDn keeps Modal::Reader open; ^b n closes Modal::Reader and switches tab
        send_key!(KeyCode::Char('b'), KeyModifiers::CONTROL);
        send_key!(KeyCode::PageDown, KeyModifiers::NONE);
        assert!(matches!(modal, Modal::Reader { .. }));
        send_key!(KeyCode::Char('b'), KeyModifiers::CONTROL);
        send_key!(KeyCode::Char('n'), KeyModifiers::NONE);
        assert!(matches!(modal, Modal::None));
        assert_eq!(world.active_tab_index(), 1);

        // Close via Ctrl+c and direct Alt+Right
        world.select_tab_by_id("s1", "tab1");
        modal = build_reader_modal_state(
            &core,
            &world,
            Some(&attached),
            term_rect,
            true,
            Some(&mut pending_reader_rpc),
        );
        send_key!(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(modal, Modal::None));

        modal = build_reader_modal_state(
            &core,
            &world,
            Some(&attached),
            term_rect,
            true,
            Some(&mut pending_reader_rpc),
        );
        send_key!(KeyCode::Right, KeyModifiers::ALT);
        assert!(matches!(modal, Modal::None));
        assert_eq!(world.active_tab_index(), 1);
    }

    #[test]
    fn reader_modal_mouse_wheel_touch_drag_resize_and_scope_sync() {
        let core = CoreClient::dummy_for_tests();
        let mut world = reader_test_world();
        let mut attached = AttachedTerminal::new(1, 60, 24);
        let mut payload = String::new();
        for i in 1..=50 {
            payload.push_str(&format!("reader line {i}\r\n"));
        }
        attached.process_vt_bytes(payload.as_bytes());
        let small_rect = Rect::new(0, 1, 60, 12);
        let mut modal =
            build_reader_modal_state(&core, &world, Some(&attached), small_rect, true, None);
        move_reader_scroll(&mut modal, small_rect, ReaderScrollMove::Top);

        let (handled, dirty) =
            handle_reader_mouse_hit(HitTarget::ReaderPageDown, &mut modal, small_rect);
        assert!(handled && dirty);
        let page_pos = match modal {
            Modal::Reader { scroll, .. } => scroll,
            _ => panic!("expected Modal::Reader"),
        };
        assert!(page_pos > 0);

        assert!(handle_reader_wheel(true, &mut modal, small_rect, false));
        let after_wheel_up = match modal {
            Modal::Reader { scroll, .. } => scroll,
            _ => panic!("expected Modal::Reader"),
        };
        assert_eq!(after_wheel_up, page_pos.saturating_sub(3));

        // Touch drag in Normal mode scrolls smoothly per row; in E-Ink mode 1 swipe = 1 page flip
        let mut drag = Some(10u16);
        assert!(handle_reader_drag(
            &mut drag, 8, &mut modal, small_rect, false
        ));
        assert_eq!(drag, Some(8));

        let mut eink_drag = Some(10u16);
        assert!(!handle_reader_drag(
            &mut eink_drag,
            9,
            &mut modal,
            small_rect,
            true,
        )); // below threshold 2
        assert!(handle_reader_drag(
            &mut eink_drag,
            8,
            &mut modal,
            small_rect,
            true,
        ));
        assert_eq!(eink_drag, None);

        // Resize taller clamps bottom scroll cleanly
        move_reader_scroll(&mut modal, small_rect, ReaderScrollMove::Bottom);
        let tall_rect = Rect::new(0, 1, 60, 40);
        clamp_reader_scroll_on_resize(&mut modal, tall_rect);
        let (_, max_after) = match modal {
            Modal::Reader { ref doc, .. } => reader_initial_scroll(doc, tall_rect),
            _ => panic!("expected Modal::Reader"),
        };
        match modal {
            Modal::Reader { scroll, .. } => assert!(scroll <= max_after),
            _ => panic!("expected Modal::Reader"),
        }

        // Scope sync captures initial scope, stays open while unchanged, closes on tab switch
        let mut scope = None;
        let mut drag_opt = Some(5u16);
        let mut rpc_opt = Some(PendingReaderRpc {
            request_id: 77,
            terminal: "t1".to_string(),
            user_scrolled: false,
            deadline: Instant::now() + READER_RPC_TIMEOUT,
        });
        assert!(!sync_reader_modal_scope(
            &core,
            &mut modal,
            &world,
            &mut scope,
            &mut drag_opt,
            &mut rpc_opt,
        ));
        assert!(matches!(modal, Modal::Reader { .. }));
        assert!(scope.is_some());
        assert!(rpc_opt.is_some());

        world.select_tab_by_id("s1", "tab2");
        let sync_dirty = sync_reader_modal_scope(
            &core,
            &mut modal,
            &world,
            &mut scope,
            &mut drag_opt,
            &mut rpc_opt,
        );
        assert!(matches!(modal, Modal::None));
        assert!(scope.is_none());
        assert!(drag_opt.is_none());
        assert!(rpc_opt.is_none());
        assert!(sync_dirty);
    }

    #[test]
    fn format_read_source_note_handles_complete_screen_and_partial_why_combinations() {
        use crate::core::TerminalReadResult;

        assert_eq!(
            format_read_source_note(&TerminalReadResult {
                text: String::new(),
                complete: true,
                how: "stitched".to_string(),
                why: None,
            }),
            None
        );
        assert_eq!(
            format_read_source_note(&TerminalReadResult {
                text: String::new(),
                complete: true,
                how: "screen".to_string(),
                why: None,
            })
            .as_deref(),
            Some("screen")
        );
        assert_eq!(
            format_read_source_note(&TerminalReadResult {
                text: String::new(),
                complete: false,
                how: "screen".to_string(),
                why: Some("working".to_string()),
            })
            .as_deref(),
            Some("screen · working")
        );
        assert_eq!(
            format_read_source_note(&TerminalReadResult {
                text: String::new(),
                complete: false,
                how: "stitched".to_string(),
                why: Some("blocked".to_string()),
            })
            .as_deref(),
            Some("partial · blocked")
        );
        assert_eq!(
            format_read_source_note(&TerminalReadResult {
                text: String::new(),
                complete: false,
                how: "stitched".to_string(),
                why: None,
            })
            .as_deref(),
            Some("partial")
        );
    }

    #[test]
    fn async_terminal_read_upgrades_modal_preserves_scroll_and_clears_syncing_on_error() {
        use crate::core::TerminalReadResult;

        let mut core = CoreClient::dummy_for_tests();
        core.0.set_skip_rpc_for_tests(false);
        let mut world = reader_test_world();
        let mut attached = AttachedTerminal::new(1, 60, 14);
        attached.process_vt_bytes(b"Local fallback line\r\n");
        let term_rect = Rect::new(0, 1, 60, 14);

        // 0. Action::OpenBookReader opens immediately with local fallback + "syncing…" + in-flight PendingReaderRpc
        let mut sidebar_open = false;
        let mut scrollback_offset = 0;
        let mut modal = Modal::None;
        let mut eink_mode = true;
        let mut eink_color = false;
        let mut force_clear = false;
        let mut pending_rpc: Option<PendingReaderRpc> = None;
        execute_action_full(
            Action::OpenBookReader,
            &core,
            &mut world,
            Some(&attached),
            "ctrl+b",
            &mut sidebar_open,
            &mut scrollback_offset,
            &mut modal,
            term_rect,
            term_rect,
            &mut eink_mode,
            &mut eink_color,
            &mut force_clear,
            Some(&mut pending_rpc),
        )
        .unwrap();
        let rpc_open = pending_rpc
            .as_ref()
            .expect("async request registered")
            .clone();
        assert_eq!(rpc_open.terminal, "t1");
        match modal {
            Modal::Reader {
                ref doc,
                ref source_note,
                ..
            } => {
                assert_eq!(source_note.as_deref(), Some("syncing…"));
                assert_eq!(doc.source_note.as_deref(), Some("syncing…"));
            }
            _ => panic!("expected Modal::Reader"),
        }

        // 1. Stale or wrong-terminal responses are ignored
        let mut dirty = false;

        apply_terminal_read_message(
            rpc_open.request_id.wrapping_sub(1),
            "t1",
            Ok(TerminalReadResult {
                text: "stale".to_string(),
                complete: true,
                how: "stitched".to_string(),
                why: None,
            }),
            &world,
            &mut modal,
            term_rect,
            &mut pending_rpc,
            &mut dirty,
        );
        assert!(!dirty);
        assert_eq!(
            pending_rpc.as_ref().unwrap().request_id,
            rpc_open.request_id
        );

        // Toggle Full mode while RPC is still in-flight; arriving reply preserves user's Full choice
        assert!(toggle_reader_clean_mode(&mut modal, term_rect));

        // 2. Matching async Ok upgrades raw_text, preserves user's Full mode (`clean_mode: false`),
        //    and applies complete/why source_note while anchoring to latest when `!user_scrolled`.
        let mut rich = String::from("> prompt\n\u{25cb} Thought for 1s\n");
        for i in 1..=25 {
            rich.push_str(&format!("Paragraph {i}: async stitched response.\n\n"));
        }
        apply_terminal_read_message(
            rpc_open.request_id,
            "t1",
            Ok(TerminalReadResult {
                text: rich.clone(),
                complete: false,
                how: "stitched".to_string(),
                why: None,
            }),
            &world,
            &mut modal,
            term_rect,
            &mut pending_rpc,
            &mut dirty,
        );
        assert!(dirty);
        assert!(pending_rpc.is_none());
        let expected_anchor = match modal {
            Modal::Reader {
                ref doc,
                scroll,
                ref source_note,
                ..
            } => {
                assert!(!doc.clean_mode);
                assert_eq!(source_note.as_deref(), Some("partial"));
                assert_eq!(doc.source_note.as_deref(), Some("partial"));
                assert!(scroll > 0);
                scroll
            }
            _ => panic!("expected Modal::Reader"),
        };

        // 3. When user scrolled away (`user_scrolled: true`), async upgrade preserves scroll (clamped)
        if let Modal::Reader { ref mut scroll, .. } = modal {
            *scroll = 7;
        }
        pending_rpc = Some(PendingReaderRpc {
            request_id: 11,
            terminal: "t1".to_string(),
            user_scrolled: true,
            deadline: Instant::now() + READER_RPC_TIMEOUT,
        });
        dirty = false;
        apply_terminal_read_message(
            11,
            "t1",
            Ok(TerminalReadResult {
                text: rich,
                complete: true,
                how: "stitched".to_string(),
                why: None,
            }),
            &world,
            &mut modal,
            term_rect,
            &mut pending_rpc,
            &mut dirty,
        );
        assert!(dirty);
        assert_ne!(expected_anchor, 7);
        match modal {
            Modal::Reader {
                ref doc,
                scroll,
                ref source_note,
                ..
            } => {
                assert_eq!(scroll, 7);
                assert!(source_note.is_none());
                assert!(doc.source_note.is_none());
            }
            _ => panic!("expected Modal::Reader"),
        }

        // 4. Async Err clears "syncing…" badge without dropping existing content and sets dirty = true
        if let Modal::Reader {
            ref mut doc,
            ref mut source_note,
            ..
        } = modal
        {
            *source_note = Some("syncing…".to_string());
            doc.source_note = Some("syncing…".to_string());
        }
        pending_rpc = Some(PendingReaderRpc {
            request_id: 12,
            terminal: "t1".to_string(),
            user_scrolled: true,
            deadline: Instant::now() + READER_RPC_TIMEOUT,
        });
        dirty = false;
        apply_terminal_read_message(
            12,
            "t1",
            Err("timeout".to_string()),
            &world,
            &mut modal,
            term_rect,
            &mut pending_rpc,
            &mut dirty,
        );
        assert!(dirty);
        match modal {
            Modal::Reader {
                ref doc,
                ref raw_text,
                ref source_note,
                ..
            } => {
                assert!(raw_text.contains("async stitched response"));
                assert!(source_note.is_none());
                assert!(doc.source_note.is_none());
            }
            _ => panic!("expected Modal::Reader"),
        }

        // 4b. Byte-identical Ok reply that changes badge ("syncing…" -> None or "screen · working")
        //     must still set dirty = true and update the source note cleanly.
        let cur_text = match modal {
            Modal::Reader { ref raw_text, .. } => raw_text.clone(),
            _ => panic!("expected Modal::Reader"),
        };
        if let Modal::Reader {
            ref mut doc,
            ref mut source_note,
            ..
        } = modal
        {
            *source_note = Some("syncing…".to_string());
            doc.source_note = Some("syncing…".to_string());
        }
        pending_rpc = Some(PendingReaderRpc {
            request_id: 15,
            terminal: "t1".to_string(),
            user_scrolled: true,
            deadline: Instant::now() + READER_RPC_TIMEOUT,
        });
        dirty = false;
        apply_terminal_read_message(
            15,
            "t1",
            Ok(TerminalReadResult {
                text: cur_text.clone(),
                complete: true,
                how: "stitched".to_string(),
                why: None,
            }),
            &world,
            &mut modal,
            term_rect,
            &mut pending_rpc,
            &mut dirty,
        );
        assert!(dirty);
        match modal {
            Modal::Reader {
                ref doc,
                ref source_note,
                ..
            } => {
                assert!(source_note.is_none());
                assert!(doc.source_note.is_none());
            }
            _ => panic!("expected Modal::Reader"),
        }

        if let Modal::Reader {
            ref mut doc,
            ref mut source_note,
            ..
        } = modal
        {
            *source_note = Some("syncing…".to_string());
            doc.source_note = Some("syncing…".to_string());
        }
        pending_rpc = Some(PendingReaderRpc {
            request_id: 16,
            terminal: "t1".to_string(),
            user_scrolled: true,
            deadline: Instant::now() + READER_RPC_TIMEOUT,
        });
        dirty = false;
        apply_terminal_read_message(
            16,
            "t1",
            Ok(TerminalReadResult {
                text: cur_text,
                complete: false,
                how: "screen".to_string(),
                why: Some("working".to_string()),
            }),
            &world,
            &mut modal,
            term_rect,
            &mut pending_rpc,
            &mut dirty,
        );
        assert!(dirty);
        match modal {
            Modal::Reader {
                ref doc,
                ref source_note,
                ..
            } => {
                assert_eq!(source_note.as_deref(), Some("screen · working"));
                assert_eq!(doc.source_note.as_deref(), Some("screen · working"));
            }
            _ => panic!("expected Modal::Reader"),
        }
    }

    #[test]
    fn expire_pending_reader_rpc_clears_syncing_badge() {
        let mut core = CoreClient::dummy_for_tests();
        core.0.set_skip_rpc_for_tests(false);
        let mut world = reader_test_world();
        let mut attached = AttachedTerminal::new(1, 60, 14);
        attached.process_vt_bytes(b"Local screen content\r\n");
        let term_rect = Rect::new(0, 1, 60, 14);

        let mut pending_rpc = None;
        let mut modal = build_reader_modal_state(
            &core,
            &world,
            Some(&attached),
            term_rect,
            true,
            Some(&mut pending_rpc),
        );
        let rpc = pending_rpc.as_ref().unwrap().clone();
        let mut dirty = false;

        // Before deadline: remains pending with "syncing…"
        let before = rpc.deadline - Duration::from_millis(100);
        expire_pending_reader_rpc(
            before,
            &core,
            &world,
            &mut modal,
            &mut pending_rpc,
            &mut dirty,
        );
        assert!(pending_rpc.is_some());
        assert!(!dirty);

        // At deadline for open Modal::Reader: clears "syncing…" badge and preserves content
        expire_pending_reader_rpc(
            rpc.deadline,
            &core,
            &world,
            &mut modal,
            &mut pending_rpc,
            &mut dirty,
        );
        assert!(pending_rpc.is_none());
        assert!(dirty);
        match modal {
            Modal::Reader {
                ref doc,
                ref source_note,
                ..
            } => {
                assert!(source_note.is_none());
                assert!(doc.source_note.is_none());
                assert!(!doc.lines.is_empty());
            }
            _ => panic!("expected Modal::Reader"),
        }

        // Wrong active terminal on expiry cancels safely without mutating the other tab's modal
        let stale_id = core.request_terminal_read("t1", 450).unwrap();
        let stale_deadline = Instant::now() + READER_RPC_TIMEOUT;
        pending_rpc = Some(PendingReaderRpc {
            request_id: stale_id,
            terminal: "t1".to_string(),
            user_scrolled: false,
            deadline: stale_deadline,
        });
        world.select_tab_by_id("s1", "tab2");
        dirty = false;
        expire_pending_reader_rpc(
            stale_deadline,
            &core,
            &world,
            &mut modal,
            &mut pending_rpc,
            &mut dirty,
        );
        assert!(pending_rpc.is_none());
        assert!(!dirty);
    }

    #[test]
    fn attached_terminal_caches_viewport_fence_across_seq_scrollback_and_resize() {
        let mut att = AttachedTerminal::new(1, 40, 4);
        att.process_vt_bytes(
            b"```rust\r\nline1\r\nline2\r\nline3\r\n\xe2\x97\x8b Thought for 1s\r\n",
        );
        assert!(att.viewport_initial_code_fence(0));
        assert_eq!(
            att.cached_viewport_fence,
            Some((att.vt_seq, 0, 40, 4, true))
        );

        // Calling again with same key uses cache (even if cached entry were inspected directly)
        let saved_seq = att.vt_seq;
        assert!(att.viewport_initial_code_fence(0));
        assert_eq!(att.vt_seq, saved_seq);

        // Capture selection snapshot preserves `initial_in_code_fence = true`
        let (_scr, seq, init_fence) = capture_selection_snapshot(&att, true);
        assert_eq!(seq, saved_seq);
        assert!(init_fence);

        // Scrolling back to show the opening ``` line changes cache key and returns false
        att.parser.screen_mut().set_scrollback(2);
        assert!(!att.viewport_initial_code_fence(2));
        assert_eq!(
            att.cached_viewport_fence,
            Some((saved_seq, 2, 40, 4, false))
        );

        // Closing the fence and scrolling it above the viewport via new PTY bytes increments `vt_seq`
        // and updates cached result to false
        att.parser.screen_mut().set_scrollback(0);
        att.process_vt_bytes(b"```\r\nafter1\r\nafter2\r\nafter3\r\n");
        assert!(!att.viewport_initial_code_fence(0));
        assert_ne!(att.vt_seq, saved_seq);

        // Resize clears `cached_viewport_fence`
        att.resize(50, 5);
        assert_eq!(att.cached_viewport_fence, None);

        // E-Ink snapshot pacing detects pure fence flip (`initial_in_code_fence` false -> true)
        // even when visible viewport cells are identical
        let term_rect = Rect::new(0, 1, 40, 4);
        let mut att2 = AttachedTerminal::new(2, term_rect.width, term_rect.height);
        att2.process_vt_bytes(
            b"```rust\r\n```\r\nline1\r\nline2\r\nline3\r\n\xe2\x97\x8b Thought for 1s",
        );
        assert!(!att2.viewport_initial_code_fence(0));
        let snap_outside = capture_eink_screen_snapshot_with_color_and_fence(
            att2.parser.screen(),
            term_rect,
            0,
            false,
            false,
        );
        let mut att3 = AttachedTerminal::new(3, term_rect.width, term_rect.height);
        att3.process_vt_bytes(b"```rust\r\nline1\r\nline2\r\nline3\r\n\xe2\x97\x8b Thought for 1s");
        let mut map = BTreeMap::new();
        map.insert("t1".to_string(), att3);
        let now = Instant::now();
        let mut pty_dirty = true;
        let mut pacer = EinkInputPacer::default();
        let mut dirty = false;
        let mut last_draw_at = Some(now - Duration::from_secs(1));
        let mut sb = 0usize;
        let last_snap = Some(("t1".to_string(), snap_outside));
        promote_due_pty_dirty_with_color(
            true,
            false,
            Duration::from_millis(250),
            now,
            &mut pty_dirty,
            &mut pacer,
            &mut dirty,
            &mut last_draw_at,
            &last_snap,
            Some("t1"),
            &mut map,
            term_rect,
            &mut sb,
            &Modal::None,
            &None,
        );
        assert!(
            dirty,
            "fence state flip must promote dirty instead of zero-diff elision"
        );
    }

    #[test]
    fn detect_multiplexer_env_with_handles_empty_whitespace_and_active_values() {
        let env_from = |pairs: &[(&str, &str)]| {
            let map: BTreeMap<&str, &str> = pairs.iter().copied().collect();
            detect_multiplexer_env_with(|k| map.get(k).map(|v| (*v).to_string()))
        };

        assert_eq!(env_from(&[]), (false, false));
        assert_eq!(env_from(&[("TMUX", ""), ("STY", "   ")]), (false, false));
        assert_eq!(env_from(&[("TMUX", "session,42,0")]), (true, false));
        assert_eq!(env_from(&[("STY", "1234.pts-0.box")]), (false, true));
        assert_eq!(
            env_from(&[("TMUX", "session,42,0"), ("STY", "1234.pts-0.box")]),
            (true, true)
        );
    }

    #[test]
    fn resolve_herdr_session_handles_cli_precedence_trimming_and_empty() {
        let env_with = |val: Option<&str>| {
            let owned = val.map(str::to_string);
            move |k: &str| {
                if k == "HC_SESSION" {
                    owned.clone()
                } else {
                    None
                }
            }
        };

        assert_eq!(
            resolve_herdr_session(Some("work"), env_with(Some("from-env"))),
            Some("work".to_string())
        );
        assert_eq!(
            resolve_herdr_session(Some("  work  "), env_with(None)),
            Some("work".to_string())
        );
        // Explicit empty/whitespace `--session` overrides `$HC_SESSION` just like CoreClient::connect_with_sender
        assert_eq!(
            resolve_herdr_session(Some("   "), env_with(Some("from-env"))),
            None
        );
        assert_eq!(resolve_herdr_session(Some(""), env_with(None)), None);
        assert_eq!(
            resolve_herdr_session(None, env_with(Some("  from-env  "))),
            Some("from-env".to_string())
        );
        assert_eq!(resolve_herdr_session(None, env_with(Some("   "))), None);
        assert_eq!(resolve_herdr_session(None, env_with(None)), None);

        // `connect_session_arg` forwards trimmed session names to `CoreClient` and overrides
        // inherited `$HC_SESSION` with `Some("")` when `--session` was explicitly blank.
        assert_eq!(
            connect_session_arg(Some("  work  "), Some("work")),
            Some("work")
        );
        assert_eq!(connect_session_arg(Some("   "), None), Some(""));
        assert_eq!(
            connect_session_arg(None, Some("from-env")),
            Some("from-env")
        );
        assert_eq!(connect_session_arg(None, None), None);
    }

    #[test]
    fn format_osc1337_user_var_and_clear_support_plain_tmux_and_screen_dcs() {
        assert_eq!(base64_encode(b"t7"), "dDc=");
        assert_eq!(base64_encode(b"machine/t7"), "bWFjaGluZS90Nw==");

        let plain_term = format_osc1337_user_var("herdr_terminal", "t7", false, false);
        assert_eq!(plain_term, "\x1b]1337;SetUserVar=herdr_terminal=dDc=\x07");

        let plain_remote = format_osc1337_user_var("herdr_terminal", "machine/t7", false, false);
        assert_eq!(
            plain_remote,
            "\x1b]1337;SetUserVar=herdr_terminal=bWFjaGluZS90Nw==\x07"
        );

        let plain_clear = format_herdr_session_clear_osc(false, false);
        assert_eq!(
            plain_clear,
            "\x1b]1337;SetUserVar=herdr_terminal=\x07\x1b]1337;SetUserVar=herdr_session=\x07"
        );

        // Long payload exercises tmux DCS chunking (<=76 pre-escape bytes) and ESC doubling.
        let long_val = "node-alpha/".to_string() + &"x".repeat(90);
        let expected_b64 = base64_encode(long_val.as_bytes());
        let expected_osc = format!("\x1b]1337;SetUserVar=herdr_terminal={expected_b64}\x07");

        let tmux_seq = format_osc1337_user_var("herdr_terminal", &long_val, true, false);
        assert!(tmux_seq.starts_with("\x1bPtmux;\x1b\x1b]1337;SetUserVar="));
        assert!(tmux_seq.ends_with("\x1b\\"));
        let mut tmux_frames = 0usize;
        let mut tmux_unwrapped = String::new();
        let mut rest_tmux = tmux_seq.as_str();
        while !rest_tmux.is_empty() {
            let body_plus = rest_tmux
                .strip_prefix("\x1bPtmux;")
                .expect("every tmux DCS chunk must start with ESC P tmux;");
            let end_idx = body_plus
                .find("\x1b\\")
                .expect("every tmux DCS chunk must end with ST");
            let frame_inner = &body_plus[..end_idx];
            let unescaped = frame_inner.replace("\x1b\x1b", "\x1b");
            assert!(unescaped.len() <= 76);
            assert_eq!(
                frame_inner.matches('\x1b').count(),
                unescaped.matches('\x1b').count() * 2
            );
            tmux_unwrapped.push_str(&unescaped);
            tmux_frames += 1;
            rest_tmux = &body_plus[end_idx + 2..];
        }
        assert!(
            tmux_frames >= 2,
            "expected chunked tmux frames, got {tmux_frames}"
        );
        assert_eq!(tmux_unwrapped, expected_osc);

        // Screen DCS passthrough chunks into <=76-byte frames.
        let screen_seq = format_osc1337_user_var("herdr_terminal", &long_val, false, true);
        assert!(screen_seq.starts_with("\x1bP\x1b]1337;SetUserVar="));
        assert!(screen_seq.ends_with("\x1b\\"));
        let mut screen_frames = 0usize;
        let mut screen_unwrapped = String::new();
        let mut rest_screen = screen_seq.as_str();
        while !rest_screen.is_empty() {
            let body_plus = rest_screen
                .strip_prefix("\x1bP")
                .expect("every screen DCS chunk must start with ESC P");
            let end_idx = body_plus
                .find("\x1b\\")
                .expect("every screen DCS chunk must end with ST");
            let frame_inner = &body_plus[..end_idx];
            assert!(frame_inner.len() <= 76);
            screen_unwrapped.push_str(frame_inner);
            screen_frames += 1;
            rest_screen = &body_plus[end_idx + 2..];
        }
        assert!(
            screen_frames >= 2,
            "expected chunked screen frames, got {screen_frames}"
        );
        assert_eq!(screen_unwrapped, expected_osc);

        // Clear helper emits herdr_terminal then herdr_session through tmux/screen DCS wrappers too.
        let tmux_clear = format_herdr_session_clear_osc(true, false);
        assert_eq!(
            tmux_clear,
            format!(
                "{}{}",
                format_osc1337_user_var("herdr_terminal", "", true, false),
                format_osc1337_user_var("herdr_session", "", true, false),
            )
        );
        let screen_clear = format_herdr_session_clear_osc(false, true);
        assert_eq!(
            screen_clear,
            format!(
                "{}{}",
                format_osc1337_user_var("herdr_terminal", "", false, true),
                format_osc1337_user_var("herdr_session", "", false, true),
            )
        );
    }

    #[test]
    fn outer_session_reporter_tracks_session_dedup_focus_switches_and_clear() {
        let mut reporter = OuterSessionReporter::new(Some("work".to_string()), false, false);

        // 1. Initial sync announces both herdr_session and herdr_terminal
        let first = reporter.sync_sequence(Some("t7")).expect("initial sync");
        let work_b64 = base64_encode(b"work");
        assert_eq!(
            first,
            format!(
                "\x1b]1337;SetUserVar=herdr_session={work_b64}\x07\
                 \x1b]1337;SetUserVar=herdr_terminal=dDc=\x07"
            )
        );

        // 2. Unchanged focus (including surrounding whitespace) is deduplicated
        assert_eq!(reporter.sync_sequence(Some("t7")), None);
        assert_eq!(reporter.sync_sequence(Some("  t7  ")), None);

        // 3. Terminal focus switch emits ONLY herdr_terminal
        let switched = reporter
            .sync_sequence(Some("machine/t7"))
            .expect("terminal switch");
        assert_eq!(
            switched,
            "\x1b]1337;SetUserVar=herdr_terminal=bWFjaGluZS90Nw==\x07"
        );
        assert_eq!(reporter.sync_sequence(Some("machine/t7")), None);

        // 4. Switching to an empty Space (None or whitespace) clears herdr_terminal only
        let cleared_term = reporter.sync_sequence(None).expect("empty space clear");
        assert_eq!(cleared_term, "\x1b]1337;SetUserVar=herdr_terminal=\x07");
        assert_eq!(reporter.sync_sequence(None), None);
        assert_eq!(reporter.sync_sequence(Some("   ")), None);

        // 5. Recovering from empty Space back to a live terminal emits herdr_terminal
        let recovered = reporter
            .sync_sequence(Some("t2"))
            .expect("recover terminal");
        assert_eq!(
            recovered,
            format!(
                "\x1b]1337;SetUserVar=herdr_terminal={}\x07",
                base64_encode(b"t2")
            )
        );

        // 6. clear_sequence clears both vars and resets state so next sync re-announces both
        let cleared_all = reporter.clear_sequence();
        assert_eq!(
            cleared_all,
            "\x1b]1337;SetUserVar=herdr_terminal=\x07\x1b]1337;SetUserVar=herdr_session=\x07"
        );
        let reannounced = reporter
            .sync_sequence(Some("t2"))
            .expect("reannounce after clear");
        assert_eq!(
            reannounced,
            format!(
                "\x1b]1337;SetUserVar=herdr_session={work_b64}\x07\
                 \x1b]1337;SetUserVar=herdr_terminal={}\x07",
                base64_encode(b"t2")
            )
        );

        // 7. Default (None) session emits empty herdr_session= on first sync to clear stale values
        let mut default_reporter = OuterSessionReporter::new(None, false, false);
        let default_first = default_reporter
            .sync_sequence(None)
            .expect("default session initial clear");
        assert_eq!(
            default_first,
            "\x1b]1337;SetUserVar=herdr_session=\x07\x1b]1337;SetUserVar=herdr_terminal=\x07"
        );
        assert_eq!(default_reporter.sync_sequence(None), None);
    }

    #[test]
    fn write_terminal_leave_sequence_with_mux_preserves_order_and_appends_session_clear() {
        for (in_tmux, in_screen) in [(false, false), (true, false), (false, true)] {
            let mut buf = Vec::new();
            write_terminal_leave_sequence_with_mux(&mut buf, in_tmux, in_screen).unwrap();
            let leave_str = String::from_utf8(buf).expect("UTF-8 terminal leave sequence");
            let disable_mouse_idx = leave_str
                .find("\x1b[?1000l")
                .expect("DisableMouseCapture on leave");
            let clear_idx = leave_str
                .find("\x1b[2J")
                .expect("Clear(ClearType::All) before LeaveAlternateScreen");
            let alt_leave_idx = leave_str
                .find("\x1b[?1049l")
                .expect("LeaveAlternateScreen on leave");
            let show_idx = leave_str
                .rfind("\x1b[?25h")
                .expect("Show cursor after LeaveAlternateScreen");
            assert!(disable_mouse_idx < clear_idx);
            assert!(clear_idx < alt_leave_idx);
            assert!(alt_leave_idx < show_idx);

            let expected_clear = format_herdr_session_clear_osc(in_tmux, in_screen);
            assert!(
                leave_str.ends_with(&expected_clear),
                "expected leave sequence for mux=({in_tmux}, {in_screen}) to end with {expected_clear:?}"
            );
            let clear_start = leave_str.len() - expected_clear.len();
            assert!(show_idx < clear_start);
        }
    }

    #[test]
    fn reader_modal_uses_fullscreen_reader_rect_even_when_term_rect_is_smaller() {
        let core = CoreClient::dummy_for_tests();
        let mut world = reader_test_world();
        let mut attached = AttachedTerminal::new(1, 60, 28);
        let mut payload = String::from("> Turn 1\r\n");
        for i in 1..=35 {
            payload.push_str(&format!("Turn 1 paragraph {i:02}.\r\n\r\n"));
        }
        payload.push_str("> Turn 2\r\n");
        for i in 1..=8 {
            payload.push_str(&format!("Turn 2 paragraph {i:02}.\r\n\r\n"));
        }
        attached.process_vt_bytes(payload.as_bytes());

        let term_rect = Rect::new(0, 1, 60, 10);
        let reader_rect = Rect::new(0, 0, 60, 30);
        let mut sidebar_open = false;
        let mut prefix_active = false;
        let mut ctrl_active = false;
        let mut alt_active = false;
        let mut scrollback_offset = 0;
        let mut modal = Modal::None;
        let mut eink_mode = false;
        let mut eink_color = false;
        let mut force_clear = false;

        // Open Reader via Action::OpenBookReader -> initial scroll uses `reader_rect`
        assert!(!execute_action_full(
            Action::OpenBookReader,
            &core,
            &mut world,
            Some(&attached),
            "ctrl+b",
            &mut sidebar_open,
            &mut scrollback_offset,
            &mut modal,
            term_rect,
            reader_rect,
            &mut eink_mode,
            &mut eink_color,
            &mut force_clear,
            None,
        )
        .unwrap());

        let (doc_clone, open_scroll) = match modal {
            Modal::Reader {
                ref doc, scroll, ..
            } => (doc.clone(), scroll),
            _ => panic!("expected Modal::Reader"),
        };
        let (expected_init_full, max_full) = reader_initial_scroll(&doc_clone, reader_rect);
        let (expected_init_term, max_term) = reader_initial_scroll(&doc_clone, term_rect);
        assert!(max_full < max_term);
        assert_eq!(open_scroll, expected_init_full);
        assert_ne!(expected_init_full, expected_init_term);

        // PageDown / PageUp in Reader step by full `reader_page_step(reader_rect)`
        move_reader_scroll(&mut modal, reader_rect, ReaderScrollMove::Top);
        handle_key_event_full(
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            &core,
            &mut world,
            Some(&attached),
            "ctrl+b",
            &mut sidebar_open,
            &mut prefix_active,
            &mut ctrl_active,
            &mut alt_active,
            &mut scrollback_offset,
            &mut modal,
            term_rect,
            reader_rect,
            &mut eink_mode,
            &mut eink_color,
            &mut force_clear,
            None,
        )
        .unwrap();
        let after_pgdn = match modal {
            Modal::Reader { scroll, .. } => scroll,
            _ => panic!("expected Modal::Reader"),
        };
        let expected_step = reader_page_step(reader_rect).min(max_full);
        assert_eq!(after_pgdn, expected_step);
        assert!(after_pgdn > reader_page_step(term_rect));
    }
}
