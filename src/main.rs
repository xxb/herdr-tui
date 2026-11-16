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
    collect_fuzzy_items, compute_terminal_rect, find_hit_target, is_in_rect, is_on_dpad_disc,
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
const HORIZ_WHEEL_QUIET_GAP: Duration = Duration::from_millis(180);
const VERT_SCROLL_LOCK_WINDOW: Duration = Duration::from_millis(350);
const HORIZ_SWIPE_ACCUM_WINDOW: Duration = Duration::from_millis(250);
const VIEWPORT_HORIZ_SWIPE_THRESHOLD: u8 = 3;
const TOP_BAR_HORIZ_SWIPE_THRESHOLD: u8 = 1;
const SPACE_MENU_HORIZ_SWIPE_THRESHOLD: u8 = 1;

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
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.main_scroll_top = 0;
        self.dropped_wide_at_margin = false;
        self.parser.set_size(non_zero_u16(rows), non_zero_u16(cols));
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
                cur.resize(cols, rows);
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
        || !matches!(*modal, Modal::None)
        || !is_in_rect(space_slop_rect, anchor_col, anchor_row)
        || !is_in_rect(space_slop_rect, up_col, up_row)
    {
        return false;
    }
    *prefix_active = false;
    *ctrl_active = false;
    *alt_active = false;
    *modal = Modal::SpaceMenu;
    true
}

#[allow(clippy::too_many_arguments)]
fn handle_top_bar_or_space_vertical_wheel(
    scroll_up: bool,
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
    let prev_ti = world.active_tab_index();
    if scroll_up {
        world.prev_tab();
    } else {
        world.next_tab();
    }
    let tab_changed = world.active_tab_index() != prev_ti;
    if tab_changed {
        world.mark_active_seen(core);
    }
    tab_changed || had_sticky || had_vis_sel
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
    if dpad_drag_active
        || selection.as_ref().is_some_and(|s| s.active)
        || !matches!(modal, Modal::None | Modal::SpaceMenu)
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
    let mut last_space_slop_rect: Option<Rect> = None;
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
                                horiz_wheel.clear_pending_horiz();
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
                                            sel.dragged = true;
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
                                            if was_dragged || slop_toggled {
                                                dirty = true;
                                            }
                                        }
                                    }
                                }
                            }
                            MouseEventKind::ScrollUp => {
                                let wheel_now = Instant::now();
                                if matches!(modal, Modal::SpaceMenu) {
                                    if handle_top_bar_or_space_vertical_wheel(
                                        true,
                                        wheel_now,
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
                                } else if matches!(modal, Modal::None) && mev.row < term_rect.y {
                                    if handle_top_bar_or_space_vertical_wheel(
                                        true,
                                        wheel_now,
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
                                        scrollback_offset = scrollback_offset.saturating_add(3);
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
                                let wheel_now = Instant::now();
                                if matches!(modal, Modal::SpaceMenu) {
                                    if handle_top_bar_or_space_vertical_wheel(
                                        false,
                                        wheel_now,
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
                                } else if matches!(modal, Modal::None) && mev.row < term_rect.y {
                                    if handle_top_bar_or_space_vertical_wheel(
                                        false,
                                        wheel_now,
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
                                        scrollback_offset = scrollback_offset.saturating_sub(3);
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
                last_space_slop_rect = geom.space_slop_rect;
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
                &self.modal,
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
                &self.modal,
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

        // 4. After HORIZ_WHEEL_QUIET_GAP elapses, a second deliberate swipe in the same direction steps cleanly -> Tab 2 (wrap)
        let t_next = t_rev_end + HORIZ_WHEEL_QUIET_GAP;
        assert!(h.swipe_horiz(
            HorizontalSwipeDir::Prev,
            10,
            VIEWPORT_HORIZ_SWIPE_THRESHOLD,
            t_next
        ));
        assert_eq!(h.world.active_tab_index(), 2);
        let t_next_end = t_next + Duration::from_millis(32);

        // 5. Diagonal trackpad bleed during viewport vertical scroll is suppressed within VERT_SCROLL_LOCK_WINDOW
        //    across both Viewport and TopBar (row 0), then explicit TopBar swipes work after the lock expires
        //    AND suppress trailing vertical top-bar packets after a horizontal switch (both packet orderings!).
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

        // Once VERT_SCROLL_LOCK_WINDOW + HORIZ_WHEEL_QUIET_GAP elapses, pure TopBar horizontal swipe switches cleanly
        let t_top_ok = t_vert + VERT_SCROLL_LOCK_WINDOW + HORIZ_WHEEL_QUIET_GAP;
        assert!(h.swipe_horiz(
            HorizontalSwipeDir::Next,
            0,
            TOP_BAR_HORIZ_SWIPE_THRESHOLD,
            t_top_ok
        ));
        assert_eq!(h.world.active_tab_index(), 0);
        let t_top_end = t_top_ok + Duration::from_millis(16);
        // Ordering B (`ScrollRight` -> continuous `ScrollDown` stream on row 0):
        // trailing vertical packets stay suppressed even after 180ms until a quiet gap
        for ms in [40_u64, 100, 150, 200, 250] {
            assert!(!h.vert_switch(false, t_top_end + Duration::from_millis(ms), false));
        }
        assert_eq!(h.world.active_tab_index(), 0);

        // Ordering A (`ScrollDown` -> continuous `ScrollRight` stream on row 0):
        // vertical switches Tab 0 -> Tab 1, and rejected horizontal burst stays coalesced past VERT_SCROLL_LOCK_WINDOW
        let t_order_a = t_top_end + Duration::from_millis(500);
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
}
