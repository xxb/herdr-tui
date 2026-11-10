mod core;
mod keys;
mod model;
mod ui;
mod wire;

use crate::core::{CoreClient, CoreMessage};
use crate::keys::{
    encode_key_event, is_prefix_key, match_direct_action, match_prefix_action, prefix_bytes,
    Action, FileConfig,
};
use crate::model::World;
use crate::ui::{
    collect_fuzzy_items, compute_terminal_rect, find_hit_target, is_on_dpad_disc,
    render_ui_with_dpad, resolve_dpad_disc_rect, resolve_fuzzy_selection, HitRegion, HitTarget,
    Modal, PromptKind, TerminalSelection,
};
use anyhow::Result;
use clap::Parser;
use crossterm::{
    cursor::Show,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

const SCROLLBACK_LINES: usize = 2000;
const ATTACH_HISTORY_LINES: usize = 0;
const MAX_MSGS_PER_TICK: usize = 256;

struct TerminalCleanupGuard;

impl Drop for TerminalCleanupGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            Show
        );
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "hct",
    version,
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

    /// Enable 2-row large touch top bar mode for iPad / tablet SSH clients
    #[arg(long)]
    touch: bool,

    /// Disable 2-row large touch top bar mode
    #[arg(long, conflicts_with = "touch")]
    no_touch: bool,
}

struct AttachedTerminal {
    channel: u32,
    parser: vt100::Parser,
    cols: u16,
    rows: u16,
    utf8_tail: Vec<u8>,
    prev_width1_symbol: bool,
}

impl AttachedTerminal {
    fn new(channel: u32, cols: u16, rows: u16) -> Self {
        Self {
            channel,
            parser: vt100::Parser::new(rows, cols, SCROLLBACK_LINES),
            cols,
            rows,
            utf8_tail: Vec::new(),
            prev_width1_symbol: false,
        }
    }

    fn process_vt_bytes(&mut self, payload: &[u8]) {
        let clean = sanitize_vt_stream(
            payload,
            &mut self.utf8_tail,
            &mut self.prev_width1_symbol,
        );
        if !clean.is_empty() {
            self.parser.process(&clean);
        }
    }
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

fn trailing_incomplete_utf8_len(buf: &[u8]) -> usize {
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

    let trailing = trailing_incomplete_utf8_len(&combined);
    let valid_len = combined.len() - trailing;
    if trailing > 0 {
        utf8_tail.extend_from_slice(&combined[valid_len..]);
    }
    let slice = &combined[..valid_len];
    if slice.is_empty() {
        return Cow::Borrowed(&[]);
    }

    let has_9001 = slice.windows(7).any(|w| w == b"\x1b[?9001");
    let has_fe0f = slice.windows(3).any(|w| w == b"\xef\xb8\x8f");

    if !has_9001 && !has_fe0f {
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
        if has_9001
            && i + 8 <= slice.len()
            && (&slice[i..i + 8] == b"\x1b[?9001h" || &slice[i..i + 8] == b"\x1b[?9001l")
        {
            *prev_width1_symbol = false;
            i += 8;
            continue;
        }
        if has_fe0f && i + 3 <= slice.len() && &slice[i..i + 3] == b"\xef\xb8\x8f" {
            // U+FE0F (Variation Selector-16) requests 2-column emoji presentation for preceding
            // 1-column symbols (such as U+2699 '⚙' or U+26A0 '⚠'). Because the `vt100` crate
            // measures codepoints individually via `unicode-width` (where '⚙' is width 1 and
            // U+FE0F is width 0), replacing U+FE0F with an ASCII space after a width-1 non-ASCII
            // symbol keeps column positions aligned with Ghostty, modern terminals, and CLI tools.
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

fn copy_to_clipboard(text: &str) {
    if text.is_empty() {
        return;
    }
    // 1. Emit OSC 52 clipboard escape sequence directly to the outer SSH / terminal stdout
    // so remote SSH clients (iPad Termius, Blink, ShellFish, WezTerm, Kitty, iTerm2, Ghostty)
    // automatically update the client device's native clipboard.
    let b64 = base64_encode(text.as_bytes());
    let mut stdout = io::stdout();
    if std::env::var_os("TMUX").is_some() {
        let _ = write!(
            stdout,
            "\x1bPtmux;\x1b\x1b]52;c;{b64}\x07\x1b\\\x1b]52;c;{b64}\x07"
        );
    } else {
        let _ = write!(stdout, "\x1b]52;c;{b64}\x07\x1b]52;;{b64}\x07");
    }
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
            if let Ok(output) = Command::new(prog)
                .args(*args)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
            {
                if output.status.success() {
                    let s = String::from_utf8_lossy(&output.stdout).into_owned();
                    if !s.is_empty() {
                        return s;
                    }
                }
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
        file_cfg.ui.touch_mode.unwrap_or(true)
    };

    let (core, core_rx) = CoreClient::connect(cli.session.as_deref())?;
    let snapshot = core.subscribe_events()?;

    let mut world = World::default();
    world.load_initial(&core, &snapshot)?;

    // Setup terminal with an immediate RAII guard so any setup or runtime error
    // restores raw mode, alternate screen, mouse capture, and cursor visibility.
    enable_raw_mode()?;
    let _cleanup_guard = TerminalCleanupGuard;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

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

    let res = run_app(
        &mut terminal,
        &core,
        &core_rx,
        &mut world,
        &prefix_spec,
        &mut sidebar_open,
        sidebar_width,
        mobile_threshold,
        touch_mode,
    );

    // Restore terminal cleanly
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    res
}

fn sync_attached_terminals(
    core: &CoreClient,
    world: &mut World,
    attached_map: &mut BTreeMap<String, AttachedTerminal>,
    active_addr: &mut Option<String>,
    term_rect: Rect,
    scrollback_offset: &mut usize,
    preattach_siblings: bool,
) -> bool {
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

    let mut newly_attached_active = false;
    if let Some(ref addr) = want_addr {
        if let Some(cur) = attached_map.get_mut(addr) {
            if cur.cols != cols || cur.rows != rows {
                cur.cols = cols;
                cur.rows = rows;
                cur.parser.set_size(rows, cols);
                core.resize_channel(cur.channel, cols, rows);
            }
            if active_addr.as_ref() != Some(addr) {
                core.set_focus(cur.channel, true);
                world.mark_active_seen(core);
            }
        } else if let Ok(ch) = core.attach_terminal(addr, cols, rows, ATTACH_HISTORY_LINES) {
            core.set_focus(ch, true);
            attached_map.insert(addr.clone(), AttachedTerminal::new(ch, cols, rows));
            world.mark_active_seen(core);
            newly_attached_active = true;
        }
    }
    *active_addr = want_addr;

    // Pre-attach and keep sibling terminals in the active space warm at `(cols, rows)`
    // so switching tabs is instantaneous (0ms) and background tabs never revert to
    // 200x120 headless size or trigger slow SIGWINCH reflows on tab switch.
    if preattach_siblings {
        let mut sibling_addrs = Vec::new();
        if let Some(space) = world.active_space() {
            for tab in &space.tabs {
                for addr in world.tab_terminals(tab) {
                    if !sibling_addrs.contains(&addr) {
                        sibling_addrs.push(addr);
                    }
                }
            }
        }
        let mut attached_any_sibling = false;
        for sib_addr in sibling_addrs {
            if Some(&sib_addr) == active_addr.as_ref() {
                continue;
            }
            if let Some(sib) = attached_map.get_mut(&sib_addr) {
                if sib.cols != cols || sib.rows != rows {
                    sib.cols = cols;
                    sib.rows = rows;
                    sib.parser.set_size(rows, cols);
                    core.resize_channel(sib.channel, cols, rows);
                }
            } else if world.terminals.contains_key(&sib_addr) {
                if let Ok(ch) = core.attach_terminal(&sib_addr, cols, rows, ATTACH_HISTORY_LINES) {
                    core.set_focus(ch, false);
                    attached_map.insert(sib_addr, AttachedTerminal::new(ch, cols, rows));
                    attached_any_sibling = true;
                }
            }
        }
        if attached_any_sibling {
            if let Some(act) = active_addr.as_ref().and_then(|a| attached_map.get(a)) {
                core.set_focus(act.channel, true);
            }
        }
    }

    newly_attached_active
}

fn execute_action(
    action: Action,
    core: &CoreClient,
    world: &mut World,
    attached: Option<&AttachedTerminal>,
    prefix_spec: &str,
    sidebar_open: &mut bool,
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
            *sidebar_open = !*sidebar_open;
        }
        Action::FuzzyFinder => {
            let items = collect_fuzzy_items(world, "");
            let selected_tab_id = items.first().map(|it| it.tab_id.clone());
            *modal = Modal::Fuzzy {
                query: String::new(),
                selected: 0,
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
    start_mouse: (u16, u16),
    start_pos: (u16, u16),
    moved: bool,
    tap_target: Option<HitTarget>,
}

#[allow(clippy::too_many_arguments)]
fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    core: &CoreClient,
    core_rx: &std::sync::mpsc::Receiver<CoreMessage>,
    world: &mut World,
    prefix_spec: &str,
    sidebar_open: &mut bool,
    sidebar_width: u16,
    mobile_threshold: u16,
    touch_mode: bool,
) -> Result<()> {
    let mut attached_map: BTreeMap<String, AttachedTerminal> = BTreeMap::new();
    let mut active_addr: Option<String> = None;
    let mut first_frame_drawn = false;
    let mut prefix_active = false;
    let mut ctrl_active = false;
    let mut alt_active = false;
    let mut scrollback_offset: usize = 0;
    let mut modal = Modal::None;
    let mut last_hits: Vec<HitRegion> = Vec::new();
    let mut dpad_pos: Option<(u16, u16)> = None;
    let mut last_dpad_rect: Option<Rect> = None;
    let mut dpad_drag: Option<FloatingDPadDrag> = None;
    let mut selection: Option<TerminalSelection> = None;
    let mut selection_screen: Option<vt100::Screen> = None;
    let mut clipboard_buffer = String::new();
    let mut copied_until: Option<Instant> = None;
    let mut dirty = true;
    let mut prev_width = terminal.size()?.width;

    loop {
        if copied_until.is_some_and(|t| Instant::now() >= t) {
            copied_until = None;
            dirty = true;
        }
        let size = terminal.size()?;
        if size.width != prev_width {
            if size.width < mobile_threshold {
                *sidebar_open = false;
            }
            prev_width = size.width;
            dirty = true;
        }
        let area = Rect::new(0, 0, size.width, size.height);
        let term_rect = compute_terminal_rect(area, *sidebar_open, sidebar_width, touch_mode);

        let prev_addr = active_addr.clone();
        let newly_attached = sync_attached_terminals(
            core,
            world,
            &mut attached_map,
            &mut active_addr,
            term_rect,
            &mut scrollback_offset,
            first_frame_drawn,
        );
        if active_addr != prev_addr {
            selection = None;
            selection_screen = None;
            dirty = true;
        }

        let active_channel = active_addr
            .as_ref()
            .and_then(|a| attached_map.get(a))
            .map(|att| att.channel);

        // Drain pending messages from `herdr-core` BEFORE rendering so that initial attach
        // history replays and multi-frame output bursts are coalesced into a single screen draw
        // without flashing a blank screen or intermediate scrollback frames.
        let drain_start = Instant::now();
        let mut drained = 0usize;
        let mut more_messages = false;
        let mut coalesce_burst = newly_attached;
        loop {
            let next_msg = if drained == 0 && newly_attached {
                core_rx.recv_timeout(Duration::from_millis(25)).ok()
            } else if let Ok(msg) = core_rx.try_recv() {
                Some(msg)
            } else if coalesce_burst && drain_start.elapsed() < Duration::from_millis(14) {
                core_rx.recv_timeout(Duration::from_millis(2)).ok()
            } else {
                None
            };
            let Some(msg) = next_msg else {
                break;
            };
            drained += 1;
            match msg {
                CoreMessage::TerminalData { channel, data } => {
                    core.ack_credit(channel, data.len() as u32);
                    let is_active = active_channel == Some(channel);
                    if is_active {
                        dirty = true;
                        if data.len() >= 4096 {
                            coalesce_burst = true;
                        }
                    }
                    if let Some(att) = attached_map.values_mut().find(|a| a.channel == channel) {
                        att.process_vt_bytes(&data);
                        if is_active && selection.as_ref().is_some_and(|s| !s.active) {
                            selection = None;
                            selection_screen = None;
                        }
                    }
                }
                CoreMessage::TerminalClosed { channel, .. } => {
                    dirty = true;
                    attached_map.retain(|_, a| a.channel != channel);
                    if active_channel == Some(channel) {
                        active_addr = None;
                    }
                    if world.refresh_terminals(core).is_ok() {
                        let _ = world.reconcile_and_sync(core);
                    }
                }
                CoreMessage::Event(ev) => {
                    dirty = true;
                    let method = ev
                        .get("method")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    let ev_type = ev
                        .get("params")
                        .and_then(|p| p.get("type"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    if method == "missed" || ev_type == "doc" || ev_type == "missed" {
                        if world.fetch_doc(core).is_ok() && world.refresh_terminals(core).is_ok() {
                            let _ = world.reconcile_and_sync(core);
                        }
                    } else {
                        match ev_type {
                            "created" | "started" | "finished" => {
                                if world.refresh_terminals(core).is_ok() {
                                    let _ = world.reconcile_and_sync(core);
                                }
                            }
                            "status" | "turn" | "title" | "labels" | "foreground" | "renamed" => {
                                let _ = world.refresh_terminals(core);
                                world.mark_active_seen(core);
                            }
                            _ => {}
                        }
                    }
                }
                CoreMessage::Disconnected => return Ok(()),
            }
            if drained >= MAX_MSGS_PER_TICK
                || drain_start.elapsed() >= Duration::from_millis(16)
            {
                more_messages = true;
                break;
            }
        }

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
            att.parser.set_scrollback(scrollback_offset);
            scrollback_offset = att.parser.screen().scrollback();
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
            let dpad_dragging = dpad_drag.as_ref().is_some_and(|d| d.moved);
            terminal.draw(|f| {
                let geom = render_ui_with_dpad(
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
                    dpad_pos,
                    dpad_dragging,
                );
                last_hits = geom.hits;
                last_dpad_rect = geom.dpad_rect;
            })?;
            dirty = false;
            if !first_frame_drawn {
                first_frame_drawn = true;
                continue;
            }
        }

        let poll_timeout = if more_messages {
            Duration::ZERO
        } else {
            Duration::from_millis(4)
        };
        if !event::poll(poll_timeout)? {
            continue;
        }

        let ev = event::read()?;
        dirty = true;
        let active_att = active_addr.as_ref().and_then(|a| attached_map.get(a));

        match ev {
            Event::Resize(w, _) => {
                if w < mobile_threshold {
                    *sidebar_open = false;
                }
                prev_width = w;
            }
            Event::Paste(text) => {
                if !text.is_empty() {
                    clipboard_buffer = text.clone();
                }
                paste_text_into_app(
                    &text,
                    core,
                    world,
                    active_att,
                    &mut scrollback_offset,
                    &mut modal,
                );
            }
            Event::Mouse(mev) => {
                match mev.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        let mx = mev.column;
                        let my = mev.row;
                        if is_on_dpad_disc(last_dpad_rect, mx, my) {
                            selection = None;
                            selection_screen = None;
                            if let Some(disc) = last_dpad_rect {
                                dpad_drag = Some(FloatingDPadDrag {
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
                            match target {
                                HitTarget::ModalNoop => {}
                                HitTarget::CancelModal => {
                                    modal = Modal::None;
                                    prefix_active = false;
                                    ctrl_active = false;
                                    alt_active = false;
                                }
                                HitTarget::ConfirmCloseYes { tab_id } => {
                                    modal = Modal::None;
                                    prefix_active = false;
                                    ctrl_active = false;
                                    alt_active = false;
                                    let _ = world.close_tab_by_id(core, &tab_id);
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
                                                let _ = world.rename_tab_by_id(core, &tab_id, &input);
                                            }
                                            PromptKind::RenameSpace { space_id } => {
                                                let _ =
                                                    world.rename_space_by_id(core, &space_id, &input);
                                            }
                                            PromptKind::NewSpace => {
                                                let _ = world.create_new_space(core, &input);
                                            }
                                        }
                                    }
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
                                        return Ok(());
                                    }
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
                                }
                                HitTarget::ToggleCtrl => {
                                    modal = Modal::None;
                                    prefix_active = false;
                                    ctrl_active = !ctrl_active;
                                }
                                HitTarget::ToggleAlt => {
                                    modal = Modal::None;
                                    prefix_active = false;
                                    alt_active = !alt_active;
                                }
                                HitTarget::PasteClipboard => {
                                    prefix_active = false;
                                    ctrl_active = false;
                                    alt_active = false;
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
                                }
                                HitTarget::ExitScrollback => {
                                    modal = Modal::None;
                                    prefix_active = false;
                                    ctrl_active = false;
                                    alt_active = false;
                                    scrollback_offset = 0;
                                }
                                HitTarget::ToggleSidebar => {
                                    modal = Modal::None;
                                    prefix_active = false;
                                    ctrl_active = false;
                                    alt_active = false;
                                    *sidebar_open = !*sidebar_open;
                                }
                                HitTarget::ToggleSpaceCollapse(space_id) => {
                                    modal = Modal::None;
                                    prefix_active = false;
                                    ctrl_active = false;
                                    alt_active = false;
                                    world.toggle_space_collapsed(&space_id);
                                }
                                HitTarget::SelectSpace(space_id) => {
                                    modal = Modal::None;
                                    prefix_active = false;
                                    ctrl_active = false;
                                    alt_active = false;
                                    if world.select_space_by_id(&space_id) {
                                        world.mark_active_seen(core);
                                    }
                                }
                                HitTarget::SelectTab { space_id, tab_id } => {
                                    modal = Modal::None;
                                    prefix_active = false;
                                    ctrl_active = false;
                                    alt_active = false;
                                    if world.select_tab_by_id(&space_id, &tab_id) {
                                        world.mark_active_seen(core);
                                    }
                                }
                                HitTarget::NewTab => {
                                    modal = Modal::None;
                                    prefix_active = false;
                                    ctrl_active = false;
                                    alt_active = false;
                                    let _ = world.create_new_tab(core);
                                }
                                HitTarget::NewSpacePrompt => {
                                    prefix_active = false;
                                    ctrl_active = false;
                                    alt_active = false;
                                    modal = Modal::Prompt {
                                        kind: PromptKind::NewSpace,
                                        input: String::new(),
                                    };
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
                                }
                                HitTarget::SendKey(kev) => {
                                    if handle_key_event(
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
                                        return Ok(());
                                    }
                                }
                            }
                        } else if !matches!(modal, Modal::None) {
                            dpad_drag = None;
                            modal = Modal::None;
                            prefix_active = false;
                            ctrl_active = false;
                            alt_active = false;
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
                        } else {
                            dpad_drag = None;
                            selection = None;
                            selection_screen = None;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some(ref mut drag) = dpad_drag {
                            let dx = mev.column as i32 - drag.start_mouse.0 as i32;
                            let dy = mev.row as i32 - drag.start_mouse.1 as i32;
                            if drag.moved || dx.abs() >= 2 || dy.abs() >= 1 {
                                drag.moved = true;
                                let nx = (drag.start_pos.0 as i32 + dx).max(0) as u16;
                                let ny = (drag.start_pos.1 as i32 + dy).max(0) as u16;
                                if let Some(clamped) =
                                    resolve_dpad_disc_rect(term_rect, Some((nx, ny)))
                                {
                                    dpad_pos = Some((clamped.x, clamped.y));
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
                                sel.head = (rel_row, rel_col);
                                if sel.is_non_empty() && selection_screen.is_none() {
                                    selection_screen =
                                        active_att.map(|a| a.parser.screen().clone());
                                }
                            }
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if let Some(drag) = dpad_drag.take() {
                            if !drag.moved && matches!(modal, Modal::None) {
                                match drag.tap_target {
                                    Some(HitTarget::PasteClipboard) => {
                                        prefix_active = false;
                                        ctrl_active = false;
                                        alt_active = false;
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
                                    }
                                    Some(HitTarget::SendKey(kev)) => {
                                        if handle_key_event(
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
                                            return Ok(());
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
                                if sel.is_non_empty() {
                                    let scr = snap
                                        .as_ref()
                                        .or_else(|| active_att.map(|a| a.parser.screen()));
                                    if let Some(screen) = scr {
                                        let text = sel.extract_text(screen);
                                        if !text.is_empty() {
                                            clipboard_buffer = text.clone();
                                            copy_to_clipboard(&text);
                                            copied_until =
                                                Some(Instant::now() + Duration::from_secs(2));
                                        }
                                    }
                                    if snap.as_ref().is_some_and(|s| {
                                        active_att.is_some_and(|a| {
                                            s.contents() != a.parser.screen().contents()
                                        })
                                    }) {
                                        selection = None;
                                    }
                                } else {
                                    selection = None;
                                }
                            }
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        if let Modal::Fuzzy {
                            ref query,
                            ref mut selected,
                            ref mut selected_tab_id,
                        } = modal
                        {
                            let items = collect_fuzzy_items(world, query);
                            let cur_sel = resolve_fuzzy_selection(
                                &items,
                                *selected,
                                selected_tab_id.as_deref(),
                            )
                            .unwrap_or(0);
                            *selected = cur_sel.saturating_sub(1);
                            *selected_tab_id = items.get(*selected).map(|it| it.tab_id.clone());
                        } else if matches!(modal, Modal::None)
                            && mev.row < term_rect.y.saturating_add(term_rect.height)
                            && !is_on_dpad_disc(last_dpad_rect, mev.column, mev.row)
                        {
                            selection = None;
                            selection_screen = None;
                            scrollback_offset = scrollback_offset.saturating_add(3);
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if let Modal::Fuzzy {
                            ref query,
                            ref mut selected,
                            ref mut selected_tab_id,
                        } = modal
                        {
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
                            }
                        } else if matches!(modal, Modal::None)
                            && mev.row < term_rect.y.saturating_add(term_rect.height)
                            && !is_on_dpad_disc(last_dpad_rect, mev.column, mev.row)
                        {
                            selection = None;
                            selection_screen = None;
                            scrollback_offset = scrollback_offset.saturating_sub(3);
                        }
                    }
                    _ => {}
                }
            }
            Event::Key(kev) if kev.kind == KeyEventKind::Press => {
                dpad_drag = None;
                selection = None;
                selection_screen = None;
                if handle_key_event(
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
                    return Ok(());
                }
            }
            _ => {}
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
) -> Result<bool> {
    if *ctrl_active {
        kev.modifiers.insert(KeyModifiers::CONTROL);
        *ctrl_active = false;
    }
    if *alt_active {
        kev.modifiers.insert(KeyModifiers::ALT);
        *alt_active = false;
    }
    // 1. Handle active Modal first
    match modal {
        Modal::Help => {
            *modal = Modal::None;
            return Ok(false);
        }
        Modal::ConfirmCloseTab { tab_id, .. } => {
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
            return Ok(false);
        }
        Modal::Prompt { kind, input } => {
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
            return Ok(false);
        }
        Modal::Fuzzy {
            query,
            selected,
            selected_tab_id,
        } => {
            let items = collect_fuzzy_items(world, query);
            let cur_sel = resolve_fuzzy_selection(&items, *selected, selected_tab_id.as_deref())
                .unwrap_or(0);
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
            return Ok(false);
        }
        Modal::None => {}
    }

    // 2. Prefix mode active (`Ctrl+b` pressed previously or `[^B]` tapped)
    if *prefix_active {
        *prefix_active = false;
        if let Some(action) = match_prefix_action(&kev, prefix_spec) {
            return execute_action(
                action,
                core,
                world,
                attached,
                prefix_spec,
                sidebar_open,
                scrollback_offset,
                modal,
            );
        }
        return Ok(false);
    }

    // 3. Check if prefix key was pressed
    if is_prefix_key(&kev, prefix_spec) {
        *prefix_active = true;
        return Ok(false);
    }

    // 4. Direct global shortcuts (`Alt+Left/Right/Up/Down/1..9`, `Ctrl+\`)
    if let Some(action) = match_direct_action(&kev) {
        return execute_action(
            action,
            core,
            world,
            attached,
            prefix_spec,
            sidebar_open,
            scrollback_offset,
            modal,
        );
    }

    // 5. Scrollback mode navigation
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
        return Ok(false);
    }

    // 6. Forward regular keyboard input to the active terminal
    if let Some(att) = attached {
        let app_cursor = att.parser.screen().application_cursor();
        let bytes = encode_key_event(&kev, app_cursor);
        if !bytes.is_empty() {
            core.send_input(att.channel, &bytes);
        }
    }
    Ok(false)
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
}
