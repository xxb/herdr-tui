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
    collect_fuzzy_items, compute_terminal_rect, render_ui, HitRegion, HitTarget, Modal, PromptKind,
};
use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};
use std::io;
use std::time::{Duration, Instant};

const SCROLLBACK_LINES: usize = 5000;
const MAX_MSGS_PER_TICK: usize = 64;

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
}

struct AttachedTerminal {
    address: String,
    channel: u32,
    parser: vt100::Parser,
    cols: u16,
    rows: u16,
}

fn sanitize_vt_bytes(payload: &[u8]) -> Vec<u8> {
    let mut out = payload.to_vec();
    for seq in [&b"\x1b[?9001h"[..], b"\x1b[?9001l"] {
        while let Some(pos) = out.windows(seq.len()).position(|w| w == seq) {
            out.drain(pos..pos + seq.len());
        }
    }
    out
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

    let (core, core_rx) = CoreClient::connect(cli.session.as_deref())?;
    let snapshot = core.subscribe_events()?;

    let mut world = World::default();
    world.load_initial(&core, &snapshot)?;

    // Setup terminal
    enable_raw_mode()?;
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

fn sync_attached_terminal(
    core: &CoreClient,
    world: &mut World,
    attached: &mut Option<AttachedTerminal>,
    term_rect: Rect,
    scrollback_offset: &mut usize,
) {
    let cols = term_rect.width.max(1);
    let rows = term_rect.height.max(1);
    let want_addr = world.active_terminal_address();

    let Some(addr) = want_addr else {
        if let Some(prev) = attached.take() {
            core.detach_channel(prev.channel);
        }
        return;
    };

    if let Some(cur) = attached.as_mut() {
        if cur.address == addr {
            if cur.cols != cols || cur.rows != rows {
                cur.cols = cols;
                cur.rows = rows;
                cur.parser.set_size(rows, cols);
                core.resize_channel(cur.channel, cols, rows);
            }
            return;
        }
        core.detach_channel(cur.channel);
        *attached = None;
    }

    *scrollback_offset = 0;
    if let Ok(ch) = core.attach_terminal(&addr, cols, rows, 200) {
        let parser = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
        *attached = Some(AttachedTerminal {
            address: addr,
            channel: ch,
            parser,
            cols,
            rows,
        });
        world.mark_active_seen(core);
    }
}

fn execute_action(
    action: Action,
    core: &CoreClient,
    world: &mut World,
    attached: &Option<AttachedTerminal>,
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
            *modal = Modal::Fuzzy {
                query: String::new(),
                selected: 0,
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

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    core: &CoreClient,
    core_rx: & std::sync::mpsc::Receiver<CoreMessage>,
    world: &mut World,
    prefix_spec: &str,
    sidebar_open: &mut bool,
    sidebar_width: u16,
    mobile_threshold: u16,
) -> Result<()> {
    let mut attached: Option<AttachedTerminal> = None;
    let mut prefix_active = false;
    let mut scrollback_offset: usize = 0;
    let mut modal = Modal::None;
    let mut last_hits: Vec<HitRegion> = Vec::new();
    let mut dirty = true;
    let mut prev_width = terminal.size()?.width;

    loop {
        let size = terminal.size()?;
        if size.width != prev_width {
            if size.width < mobile_threshold {
                *sidebar_open = false;
            }
            prev_width = size.width;
            dirty = true;
        }
        let area = Rect::new(0, 0, size.width, size.height);
        let term_rect = compute_terminal_rect(area, *sidebar_open, sidebar_width);

        sync_attached_terminal(
            core,
            world,
            &mut attached,
            term_rect,
            &mut scrollback_offset,
        );

        if let Some(att) = attached.as_mut() {
            att.parser.set_scrollback(scrollback_offset);
            scrollback_offset = att.parser.screen().scrollback();
        }

        if dirty {
            let scr_ref = attached.as_ref().map(|a| a.parser.screen());
            terminal.draw(|f| {
                let geom = render_ui(
                    f,
                    world,
                    scr_ref,
                    *sidebar_open,
                    sidebar_width,
                    prefix_active,
                    scrollback_offset,
                    &modal,
                );
                last_hits = geom.hits;
            })?;
            dirty = false;
        }

        // Drain pending messages from `herdr-core` with a per-tick count and time budget
        // so high-volume PTY output cannot starve rendering or keyboard input.
        let drain_start = Instant::now();
        let mut drained = 0usize;
        let mut more_messages = false;
        while let Ok(msg) = core_rx.try_recv() {
            dirty = true;
            drained += 1;
            match msg {
                CoreMessage::TerminalData { channel, data } => {
                    core.ack_credit(channel, data.len() as u32);
                    if let Some(att) = attached.as_mut() {
                        if att.channel == channel {
                            let clean = sanitize_vt_bytes(&data);
                            att.parser.process(&clean);
                        }
                    }
                }
                CoreMessage::TerminalClosed { channel, .. } => {
                    if attached.as_ref().is_some_and(|a| a.channel == channel) {
                        attached = None;
                        if world.refresh_terminals(core).is_ok() {
                            let _ = world.reconcile_and_sync(core);
                        }
                    }
                }
                CoreMessage::Event(ev) => {
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
                || drain_start.elapsed() >= Duration::from_millis(8)
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
        }

        let poll_timeout = if more_messages {
            Duration::ZERO
        } else {
            Duration::from_millis(16)
        };
        if !event::poll(poll_timeout)? {
            continue;
        }

        let ev = event::read()?;
        dirty = true;

        match ev {
            Event::Resize(w, _) => {
                if w < mobile_threshold {
                    *sidebar_open = false;
                }
                prev_width = w;
            }
            Event::Paste(text) => {
                if matches!(modal, Modal::None) && scrollback_offset == 0 {
                    if let Some(att) = &attached {
                        if att.parser.screen().bracketed_paste() {
                            let mut payload = b"\x1b[200~".to_vec();
                            payload.extend_from_slice(text.as_bytes());
                            payload.extend_from_slice(b"\x1b[201~");
                            core.send_input(att.channel, &payload);
                        } else {
                            core.send_input(att.channel, text.as_bytes());
                        }
                    }
                } else if let Modal::Prompt { input, .. } = &mut modal {
                    input.push_str(&text);
                } else if let Modal::Fuzzy { query, selected } = &mut modal {
                    query.push_str(&text);
                    *selected = 0;
                }
            }
            Event::Mouse(mev) => {
                match mev.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        let mx = mev.column;
                        let my = mev.row;
                        if let Some(hit) = last_hits.iter().find(|h| {
                            mx >= h.rect.x
                                && mx < h.rect.x + h.rect.width
                                && my >= h.rect.y
                                && my < h.rect.y + h.rect.height
                        }) {
                            let target = hit.target.clone();
                            modal = Modal::None;
                            prefix_active = false;
                            match target {
                                HitTarget::ToggleSidebar => *sidebar_open = !*sidebar_open,
                                HitTarget::NextSpace => {
                                    world.next_space();
                                    world.mark_active_seen(core);
                                }
                                HitTarget::SelectSpace(space_id) => {
                                    if world.select_space_by_id(&space_id) {
                                        world.mark_active_seen(core);
                                    }
                                }
                                HitTarget::SelectTab { space_id, tab_id } => {
                                    if world.select_tab_by_id(&space_id, &tab_id) {
                                        world.mark_active_seen(core);
                                    }
                                }
                                HitTarget::NewTab => {
                                    let _ = world.create_new_tab(core);
                                }
                                HitTarget::NewSpacePrompt => {
                                    modal = Modal::Prompt {
                                        kind: PromptKind::NewSpace,
                                        input: String::new(),
                                    };
                                }
                                HitTarget::ToggleHelp => {
                                    modal = Modal::Help;
                                }
                            }
                        } else if !matches!(modal, Modal::None) {
                            modal = Modal::None;
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        if matches!(modal, Modal::None) {
                            scrollback_offset = scrollback_offset.saturating_add(3);
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if matches!(modal, Modal::None) {
                            scrollback_offset = scrollback_offset.saturating_sub(3);
                        }
                    }
                    _ => {}
                }
            }
            Event::Key(kev) if kev.kind == KeyEventKind::Press => {
                // 1. Handle active Modal first
                match &mut modal {
                    Modal::Help => {
                        modal = Modal::None;
                        continue;
                    }
                    Modal::ConfirmCloseTab { tab_id, .. } => {
                        let target_id = tab_id.clone();
                        match kev.code {
                            KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                                modal = Modal::None;
                                let _ = world.close_tab_by_id(core, &target_id);
                            }
                            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                                modal = Modal::None;
                            }
                            _ => {}
                        }
                        continue;
                    }
                    Modal::Prompt { kind, input } => {
                        let k = kind.clone();
                        match kev.code {
                            KeyCode::Esc => {
                                modal = Modal::None;
                            }
                            KeyCode::Enter => {
                                let text = input.clone();
                                modal = Modal::None;
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
                            KeyCode::Char(c) if !kev.modifiers.contains(KeyModifiers::CONTROL) => {
                                input.push(c);
                            }
                            _ => {}
                        }
                        continue;
                    }
                    Modal::Fuzzy { query, selected } => {
                        let items = collect_fuzzy_items(world, query);
                        match kev.code {
                            KeyCode::Esc => {
                                modal = Modal::None;
                            }
                            KeyCode::Up => {
                                *selected = selected.saturating_sub(1);
                            }
                            KeyCode::Down => {
                                if !items.is_empty() {
                                    *selected = (*selected + 1).min(items.len() - 1);
                                }
                            }
                            KeyCode::Char('p') if kev.modifiers.contains(KeyModifiers::CONTROL) => {
                                *selected = selected.saturating_sub(1);
                            }
                            KeyCode::Char('n') if kev.modifiers.contains(KeyModifiers::CONTROL) => {
                                if !items.is_empty() {
                                    *selected = (*selected + 1).min(items.len() - 1);
                                }
                            }
                            KeyCode::Enter => {
                                if let Some(chosen) = items.get(*selected) {
                                    if world.select_tab_by_id(&chosen.space_id, &chosen.tab_id) {
                                        world.mark_active_seen(core);
                                    }
                                }
                                modal = Modal::None;
                            }
                            KeyCode::Backspace => {
                                query.pop();
                                *selected = 0;
                            }
                            KeyCode::Char(c) if !kev.modifiers.contains(KeyModifiers::CONTROL) => {
                                query.push(c);
                                *selected = 0;
                            }
                            _ => {}
                        }
                        continue;
                    }
                    Modal::None => {}
                }

                // 2. Prefix mode active (`Ctrl+b` pressed previously)
                if prefix_active {
                    prefix_active = false;
                    if let Some(action) = match_prefix_action(&kev, prefix_spec) {
                        if execute_action(
                            action,
                            core,
                            world,
                            &attached,
                            prefix_spec,
                            sidebar_open,
                            &mut scrollback_offset,
                            &mut modal,
                        )? {
                            return Ok(());
                        }
                    }
                    continue;
                }

                // 3. Check if prefix key was pressed
                if is_prefix_key(&kev, prefix_spec) {
                    prefix_active = true;
                    continue;
                }

                // 4. Direct global shortcuts (`Alt+Left/Right/Up/Down/1..9`, `Ctrl+\`)
                if let Some(action) = match_direct_action(&kev) {
                    if execute_action(
                        action,
                        core,
                        world,
                        &attached,
                        prefix_spec,
                        sidebar_open,
                        &mut scrollback_offset,
                        &mut modal,
                    )? {
                        return Ok(());
                    }
                    continue;
                }

                // 5. Scrollback mode navigation
                if scrollback_offset > 0 {
                    match kev.code {
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                            scrollback_offset = 0;
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            scrollback_offset = scrollback_offset.saturating_add(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            scrollback_offset = scrollback_offset.saturating_sub(1);
                        }
                        KeyCode::PageUp | KeyCode::Char('u') => {
                            scrollback_offset =
                                scrollback_offset.saturating_add((term_rect.height as usize / 2).max(1));
                        }
                        KeyCode::PageDown | KeyCode::Char('d') => {
                            scrollback_offset =
                                scrollback_offset.saturating_sub((term_rect.height as usize / 2).max(1));
                        }
                        KeyCode::Char('g') => {
                            scrollback_offset = SCROLLBACK_LINES;
                        }
                        KeyCode::Char('G') => {
                            scrollback_offset = 0;
                        }
                        _ => {}
                    }
                    continue;
                }

                // 6. Forward regular keyboard input to the active terminal
                if let Some(att) = &attached {
                    let app_cursor = att.parser.screen().application_cursor();
                    let bytes = encode_key_event(&kev, app_cursor);
                    if !bytes.is_empty() {
                        core.send_input(att.channel, &bytes);
                    }
                }
            }
            _ => {}
        }
    }
}
