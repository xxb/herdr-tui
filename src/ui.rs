//! TUI rendering (`ratatui` + `vt100`) with Collapsible 2-Row Left Sidebar,
//! Herdr-style Bottom Touchbar, touch/mouse hit-testing regions, and modal overlays.

use crate::keys::Action;
use crate::model::{AgentState, World};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Widget},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HitTarget {
    ToggleSidebar,
    ToggleSpaceCollapse(String),
    SelectSpace(String),
    SelectTab { space_id: String, tab_id: String },
    NewTab,
    NewSpacePrompt,
    TogglePrefix,
    ExitScrollback,
    ToggleHelp,
    ConfirmCloseYes { tab_id: String },
    SubmitPrompt,
    CancelModal,
    ModalNoop,
    TriggerAction(Action),
    SendKey(KeyEvent),
}

#[derive(Clone, Debug)]
pub struct HitRegion {
    pub rect: Rect,
    pub target: HitTarget,
}

pub fn find_hit_target(hits: &[HitRegion], col: u16, row: u16) -> Option<HitTarget> {
    hits.iter()
        .rev()
        .find(|h| {
            col >= h.rect.x
                && col < h.rect.x.saturating_add(h.rect.width)
                && row >= h.rect.y
                && row < h.rect.y.saturating_add(h.rect.height)
        })
        .map(|h| h.target.clone())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Modal {
    None,
    Help,
    ConfirmCloseTab {
        tab_id: String,
        tab_title: String,
    },
    Prompt {
        kind: PromptKind,
        input: String,
    },
    Fuzzy {
        query: String,
        selected: usize,
        selected_tab_id: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptKind {
    RenameTab { tab_id: String },
    RenameSpace { space_id: String },
    NewSpace,
}

#[derive(Clone, Debug)]
pub struct FuzzyItem {
    pub space_id: String,
    pub tab_id: String,
    pub space_name: String,
    pub tab_name: String,
    pub term_addr: String,
    pub state: AgentState,
    pub cwd: String,
}

pub fn resolve_fuzzy_selection(
    items: &[FuzzyItem],
    selected: usize,
    selected_tab_id: Option<&str>,
) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    if let Some(tid) = selected_tab_id {
        if let Some(pos) = items.iter().position(|it| it.tab_id == tid) {
            return Some(pos);
        }
    }
    Some(selected.min(items.len() - 1))
}

fn truncate_to_width(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let target = max_width - 1;
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > target {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

pub fn collect_fuzzy_items(world: &World, query: &str) -> Vec<FuzzyItem> {
    let q = query.trim().to_lowercase();
    let mut out = Vec::new();
    for space in &world.doc.spaces {
        let sname = world.space_display_name(space);
        for (ti, tab) in space.tabs.iter().enumerate() {
            let tname = world.tab_display_name(space, ti);
            let addr = world.active_terminal_for_tab(tab).unwrap_or_default();
            let cwd = world
                .terminals
                .get(&addr)
                .map(|info| info.effective_cwd().to_string())
                .unwrap_or_else(|| space.folder.clone());
            let state = world.tab_state(tab);
            let hay = format!(
                "{} {} {} {} {}",
                sname.to_lowercase(),
                tname.to_lowercase(),
                addr.to_lowercase(),
                state.label(),
                cwd.to_lowercase()
            );
            if q.is_empty() || fuzzy_subsequence(&hay, &q) {
                out.push(FuzzyItem {
                    space_id: space.id.clone(),
                    tab_id: tab.id.clone(),
                    space_name: sname.clone(),
                    tab_name: tname,
                    term_addr: addr,
                    state,
                    cwd,
                });
            }
        }
    }
    out
}

fn fuzzy_subsequence(hay: &str, needle: &str) -> bool {
    let mut chars = hay.chars();
    for n in needle.chars() {
        if !chars.any(|c| c == n) {
            return false;
        }
    }
    true
}

pub fn state_color(state: AgentState) -> Color {
    match state {
        AgentState::Blocked => Color::LightRed,
        AgentState::Working => Color::LightYellow,
        AgentState::Done => Color::LightGreen,
        AgentState::Idle => Color::Cyan,
        AgentState::None => Color::DarkGray,
    }
}

#[allow(dead_code)]
pub struct UiGeometry {
    pub term_rect: Rect,
    pub hits: Vec<HitRegion>,
}

pub fn bottom_bar_height(area: Rect, touch_mode: bool) -> u16 {
    if area.height <= 1 {
        0
    } else if touch_mode && area.height >= 6 {
        2
    } else {
        1
    }
}

pub fn compute_terminal_rect(
    area: Rect,
    sidebar_open: bool,
    sidebar_width: u16,
    touch_mode: bool,
) -> Rect {
    let bb_h = bottom_bar_height(area, touch_mode);
    let body_height = area.height.saturating_sub(bb_h).max(1);
    let sw = if sidebar_open && area.width > 12 {
        sidebar_width.min(area.width.saturating_sub(12))
    } else {
        0
    };
    Rect {
        x: area.x + sw,
        y: area.y,
        width: area.width.saturating_sub(sw).max(1),
        height: body_height,
    }
}

fn pad_to_width(s: &str, width: usize) -> String {
    let truncated = truncate_to_width(s, width);
    let cur_w = truncated.width();
    if cur_w < width {
        let mut out = truncated;
        out.extend(std::iter::repeat_n(' ', width - cur_w));
        out
    } else {
        truncated
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render_ui(
    f: &mut Frame,
    world: &World,
    screen: Option<&vt100::Screen>,
    sidebar_open: bool,
    sidebar_width: u16,
    touch_mode: bool,
    prefix_active: bool,
    scrollback_offset: usize,
    modal: &Modal,
) -> UiGeometry {
    let area = f.area();
    let mut hits = Vec::new();
    let bb_h = bottom_bar_height(area, touch_mode);

    let term_rect = compute_terminal_rect(area, sidebar_open, sidebar_width, touch_mode);
    let actual_sidebar_w = term_rect.x.saturating_sub(area.x);

    if actual_sidebar_w > 0 && term_rect.height > 0 {
        let side_rect = Rect {
            x: area.x,
            y: area.y,
            width: actual_sidebar_w,
            height: term_rect.height,
        };
        render_sidebar(f, side_rect, world, &mut hits);
    }

    if let Some(scr) = screen {
        f.render_widget(VtScreenWidget { screen: scr }, term_rect);
        if matches!(modal, Modal::None) && scrollback_offset == 0 && !scr.hide_cursor() {
            let (row, col) = scr.cursor_position();
            let cx = term_rect.x.saturating_add(col);
            let cy = term_rect.y.saturating_add(row);
            if cx < term_rect.x + term_rect.width && cy < term_rect.y + term_rect.height {
                f.set_cursor_position((cx, cy));
            }
        }
    } else {
        let placeholder = Paragraph::new("Connecting to terminal...")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center);
        f.render_widget(placeholder, term_rect);
    }

    let bottom_rect = Rect {
        x: area.x,
        y: area.y + term_rect.height,
        width: area.width,
        height: bb_h,
    };

    // Render bottom touchbar first when no modal is open, or render modal then bottom touchbar
    // so bottom touchbar terminal keys (Esc, arrows, etc.) remain usable or visually anchored.
    match modal {
        Modal::None => {
            render_bottom_touchbar(
                f,
                bottom_rect,
                world,
                sidebar_open,
                prefix_active,
                scrollback_offset,
                &mut hits,
            );
        }
        Modal::Help => {
            render_bottom_touchbar(
                f,
                bottom_rect,
                world,
                sidebar_open,
                prefix_active,
                scrollback_offset,
                &mut hits,
            );
            render_help_modal(f, area, &mut hits);
        }
        Modal::ConfirmCloseTab {
            tab_id,
            tab_title,
        } => {
            render_bottom_touchbar(
                f,
                bottom_rect,
                world,
                sidebar_open,
                prefix_active,
                scrollback_offset,
                &mut hits,
            );
            render_confirm_close_modal(f, area, tab_id, tab_title, &mut hits);
        }
        Modal::Prompt { kind, input } => {
            render_bottom_touchbar(
                f,
                bottom_rect,
                world,
                sidebar_open,
                prefix_active,
                scrollback_offset,
                &mut hits,
            );
            render_prompt_modal(f, area, kind, input, &mut hits);
        }
        Modal::Fuzzy {
            query,
            selected,
            selected_tab_id,
        } => {
            render_bottom_touchbar(
                f,
                bottom_rect,
                world,
                sidebar_open,
                prefix_active,
                scrollback_offset,
                &mut hits,
            );
            render_fuzzy_modal(
                f,
                area,
                world,
                query,
                *selected,
                selected_tab_id.as_deref(),
                &mut hits,
            );
        }
    }

    UiGeometry { term_rect, hits }
}

fn draw_touchbar_pill(
    buf: &mut Buffer,
    x: u16,
    rect: Rect,
    label: &str,
    style: Style,
    emphasized: bool,
) {
    if rect.height == 0 {
        return;
    }
    let lw = label.width() as u16;
    buf.set_string(x, rect.y, label, style);
    if rect.height >= 2 && lw > 0 {
        let line_ch = if emphasized { "═" } else { "─" };
        let mut sub = String::with_capacity(lw as usize * 3);
        if lw >= 2 {
            sub.push(' ');
            for _ in 0..(lw - 2) {
                sub.push_str(line_ch);
            }
            sub.push(' ');
        } else {
            sub.push_str(line_ch);
        }
        buf.set_string(x, rect.y + 1, &sub, style);
    }
}

fn render_bottom_touchbar(
    f: &mut Frame,
    rect: Rect,
    world: &World,
    sidebar_open: bool,
    prefix_active: bool,
    scrollback_offset: usize,
    hits: &mut Vec<HitRegion>,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let bg_bar = Style::default().bg(Color::Rgb(20, 22, 34)).fg(Color::Gray);
    f.render_widget(Block::default().style(bg_bar), rect);

    let mut x = rect.x;
    let max_x = rect.x + rect.width;
    let buf = f.buffer_mut();

    // Reserve right-side space for optional `[▼ BOT]` and `[ ? ]`
    let scroll_lbl = if scrollback_offset > 0 {
        Some(" [▼ BOT] ")
    } else {
        None
    };
    let help_lbl = " [ ? ] ";
    let scroll_w = scroll_lbl.map_or(0, |s| s.width() as u16);
    let help_w = help_lbl.width() as u16;
    let right_reserved = (scroll_w + help_w).min(rect.width.saturating_sub(4));
    let avail_end = max_x.saturating_sub(right_reserved);

    // 1. Far Left: compact 4-column icon-only Sidebar Toggle (` ◄≡ ` or ` ≡► `)
    let icon_lbl = if sidebar_open { " ◄≡ " } else { " ≡► " };
    let max_sb_budget = avail_end.saturating_sub(x);
    let sb_label = if (icon_lbl.width() as u16) <= max_sb_budget {
        icon_lbl.to_string()
    } else {
        truncate_to_width(icon_lbl, max_sb_budget as usize)
    };
    let sb_w = sb_label.width() as u16;
    if sb_w > 0 && x + sb_w <= max_x {
        let style = Style::default()
            .bg(if sidebar_open {
                Color::Rgb(52, 58, 92)
            } else {
                Color::Rgb(36, 40, 62)
            })
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);
        draw_touchbar_pill(buf, x, rect, &sb_label, style, sidebar_open);
        hits.push(HitRegion {
            rect: Rect {
                x,
                y: rect.y,
                width: sb_w,
                height: rect.height,
            },
            target: HitTarget::ToggleSidebar,
        });
        x += sb_w;
    }

    if x < avail_end {
        for r in 0..rect.height {
            buf.set_string(x, rect.y + r, "│", bg_bar.fg(Color::Rgb(55, 60, 82)));
        }
        x += 1;
    }

    // 2. Left Tab Status Dots (` ● ○ ✓ `, similar to legacy Herdr):
    // Fixed minimum zone width on >=76 col terminals so key buttons never jitter when tabs are 1..3.
    let dots_start_x = x;
    if let Some(space) = world.active_space() {
        let active_ti = world.active_tab_index();
        let space_id = space.id.clone();
        let compact_dots = space.tabs.len() > 3;
        for (ti, tab) in space.tabs.iter().enumerate() {
            let st = world.tab_state(tab);
            let pill = if compact_dots {
                format!("{} ", st.symbol())
            } else {
                format!(" {} ", st.symbol())
            };
            let pw = pill.width() as u16;
            if x + pw > avail_end {
                break;
            }
            let is_active = ti == active_ti;
            let pstyle = Style::default()
                .bg(if is_active {
                    Color::Rgb(68, 78, 128)
                } else {
                    Color::Rgb(20, 22, 34)
                })
                .fg(state_color(st))
                .add_modifier(Modifier::BOLD);
            draw_touchbar_pill(buf, x, rect, &pill, pstyle, is_active);
            hits.push(HitRegion {
                rect: Rect {
                    x,
                    y: rect.y,
                    width: pw,
                    height: rect.height,
                },
                target: HitTarget::SelectTab {
                    space_id: space_id.clone(),
                    tab_id: tab.id.clone(),
                },
            });
            x += pw;
        }
    }
    if rect.width >= 76 {
        let min_dots_end = (dots_start_x + 9).min(avail_end);
        if x < min_dots_end {
            x = min_dots_end;
        }
    }
    if x > dots_start_x && x < avail_end {
        for r in 0..rect.height {
            buf.set_string(x, rect.y + r, "│", bg_bar.fg(Color::Rgb(55, 60, 82)));
        }
        x += 1;
    }

    // 3. Constant-width Herdr App-style Touchbar Keys (`[^B]`, `[Esc]`, `[Tab]`, `[^C]`, `[←]`, `[↓]`, `[↑]`, `[→]`)
    // All labels have invariant character widths so toggling `^B` or scrolling never causes horizontal jitter.
    let key_buttons: [(&str, HitTarget, bool); 8] = [
        (" [^B] ", HitTarget::TogglePrefix, prefix_active),
        (
            " [Esc] ",
            HitTarget::SendKey(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            false,
        ),
        (
            " [Tab] ",
            HitTarget::SendKey(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            false,
        ),
        (
            " [^C] ",
            HitTarget::SendKey(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            false,
        ),
        (
            " [←] ",
            HitTarget::SendKey(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            false,
        ),
        (
            " [↓] ",
            HitTarget::SendKey(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            false,
        ),
        (
            " [↑] ",
            HitTarget::SendKey(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            false,
        ),
        (
            " [→] ",
            HitTarget::SendKey(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            false,
        ),
    ];

    for (lbl, target, active_highlight) in key_buttons {
        let kw = lbl.width() as u16;
        if x + kw > avail_end {
            break;
        }
        let kstyle = if active_highlight {
            Style::default()
                .bg(Color::LightYellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .bg(Color::Rgb(34, 38, 58))
                .fg(Color::Rgb(220, 225, 240))
                .add_modifier(Modifier::BOLD)
        };
        draw_touchbar_pill(buf, x, rect, lbl, kstyle, active_highlight);
        hits.push(HitRegion {
            rect: Rect {
                x,
                y: rect.y,
                width: kw,
                height: rect.height,
            },
            target,
        });
        x += kw;

        if x < avail_end {
            for r in 0..rect.height {
                buf.set_string(x, rect.y + r, "│", bg_bar.fg(Color::Rgb(50, 54, 74)));
            }
            x += 1;
        }
    }

    // 4. Right: optional `[▼ BOT]` and `[ ? ]`
    let mut rx = avail_end;
    if let Some(slbl) = scroll_lbl {
        if rx + scroll_w <= max_x {
            let sstyle = Style::default()
                .bg(Color::LightCyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD);
            draw_touchbar_pill(buf, rx, rect, slbl, sstyle, true);
            hits.push(HitRegion {
                rect: Rect {
                    x: rx,
                    y: rect.y,
                    width: scroll_w,
                    height: rect.height,
                },
                target: HitTarget::ExitScrollback,
            });
            rx += scroll_w;
        }
    }

    if rx + help_w <= max_x {
        let hstyle = Style::default()
            .bg(Color::Rgb(38, 42, 64))
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD);
        draw_touchbar_pill(buf, rx, rect, help_lbl, hstyle, false);
        hits.push(HitRegion {
            rect: Rect {
                x: rx,
                y: rect.y,
                width: help_w,
                height: rect.height,
            },
            target: HitTarget::ToggleHelp,
        });
    }
}

struct SidebarCard {
    row1: String,
    row2: String,
    dot_col: Option<(u16, AgentState)>,
    is_active: bool,
    is_space: bool,
    collapse_space_id: Option<String>,
    target: HitTarget,
}

fn shorten_home_path(path: &str) -> String {
    if path.is_empty() {
        return "~".to_string();
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() && path.starts_with(&home) {
            let rest = &path[home.len()..];
            return if rest.is_empty() {
                "~".to_string()
            } else {
                format!("~{rest}")
            };
        }
    }
    path.to_string()
}

fn render_sidebar(f: &mut Frame, rect: Rect, world: &World, hits: &mut Vec<HitRegion>) {
    let bg = Style::default()
        .bg(Color::Rgb(18, 20, 30))
        .fg(Color::Rgb(200, 205, 220));
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(Color::Rgb(55, 60, 85)))
        .style(bg);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let buf = f.buffer_mut();
    let mut y = inner.y;
    let max_y = inner.y + inner.height;
    let w = inner.width as usize;

    // Header (1 row)
    if y < max_y {
        let hdr = pad_to_width(" SPACES & TABS", w);
        buf.set_string(
            inner.x,
            y,
            &hdr,
            Style::default()
                .bg(Color::Rgb(26, 28, 44))
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        y += 1;
    }

    // Reserve footer space with vertical separation rows between `[+ New Tab]`, `[+ New Space]`,
    // and the bottom touchbar's `◄≡` collapse button to prevent accidental mis-taps.
    let footer_rows = if max_y.saturating_sub(y) >= 10 {
        7
    } else if max_y.saturating_sub(y) >= 6 {
        4
    } else if max_y.saturating_sub(y) >= 4 {
        2
    } else {
        0
    };
    let list_end_y = max_y.saturating_sub(footer_rows);

    // Build 2-row cards for all Spaces and (when not collapsed) their child Tabs
    let mut cards: Vec<SidebarCard> = Vec::new();
    let mut active_card_idx: usize = 0;

    for (si, space) in world.doc.spaces.iter().enumerate() {
        let is_active_space = si == world.active_space_idx;
        let is_collapsed = world.is_space_collapsed(&space.id);
        if is_active_space {
            active_card_idx = cards.len();
        }
        let sname = world.space_display_name(space);
        let sstate = world.space_state(space);
        let arrow = if is_collapsed { "▸" } else { "▾" };
        let prefix = format!(" {arrow} ");
        let dot_x = prefix.width() as u16;
        let collapsed_suffix = if is_collapsed {
            format!(" ({})", space.tabs.len())
        } else {
            String::new()
        };
        let row1 = format!("{prefix}{} {sname}{collapsed_suffix}", sstate.symbol());
        let folder = shorten_home_path(&space.folder);
        let row2 = format!("     {folder}");

        cards.push(SidebarCard {
            row1,
            row2,
            dot_col: Some((dot_x, sstate)),
            is_active: is_active_space,
            is_space: true,
            collapse_space_id: Some(space.id.clone()),
            target: HitTarget::SelectSpace(space.id.clone()),
        });

        if is_collapsed {
            continue;
        }

        let active_ti = if is_active_space {
            Some(world.active_tab_index())
        } else {
            None
        };

        for (ti, tab) in space.tabs.iter().enumerate() {
            let is_active_tab = active_ti == Some(ti);
            if is_active_tab {
                active_card_idx = cards.len();
            }
            let tname = world.tab_display_name(space, ti);
            let st = world.tab_state(tab);
            let panes = world.tab_terminals(tab).len();
            let pane_suffix = if panes > 1 {
                format!(" ({panes})")
            } else {
                String::new()
            };
            let prefix = format!("   {}:", ti + 1);
            let dot_x = prefix.width() as u16;
            let row1 = format!("{prefix}{} {tname}{pane_suffix}", st.symbol());

            let subtitle = world
                .active_terminal_for_tab(tab)
                .and_then(|addr| {
                    world.terminals.get(&addr).map(|info| {
                        let cwd = shorten_home_path(info.effective_cwd());
                        format!("      {addr} · {cwd}")
                    })
                })
                .unwrap_or_else(|| "      shell".to_string());

            cards.push(SidebarCard {
                row1,
                row2: subtitle,
                dot_col: Some((dot_x, st)),
                is_active: is_active_tab,
                is_space: false,
                collapse_space_id: None,
                target: HitTarget::SelectTab {
                    space_id: space.id.clone(),
                    tab_id: tab.id.clone(),
                },
            });
        }
    }

    // Calculate visible 2-row card window so `active_card_idx` stays in view
    let avail_rows = list_end_y.saturating_sub(y) as usize;
    let visible_cards = avail_rows / 2;
    let start_card = if visible_cards == 0 || active_card_idx < visible_cards {
        0
    } else {
        active_card_idx + 1 - visible_cards
    };

    for card in cards.into_iter().skip(start_card) {
        if y + 2 > list_end_y {
            break;
        }
        let (r1_style, r2_style) = match (card.is_space, card.is_active) {
            (true, true) => (
                Style::default()
                    .bg(Color::Rgb(42, 48, 76))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
                Style::default()
                    .bg(Color::Rgb(42, 48, 76))
                    .fg(Color::Rgb(150, 165, 200)),
            ),
            (true, false) => (
                Style::default()
                    .bg(Color::Rgb(26, 30, 46))
                    .fg(Color::Rgb(190, 198, 220))
                    .add_modifier(Modifier::BOLD),
                Style::default()
                    .bg(Color::Rgb(26, 30, 46))
                    .fg(Color::Rgb(120, 130, 155)),
            ),
            (false, true) => (
                Style::default()
                    .bg(Color::Rgb(68, 78, 128))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
                Style::default()
                    .bg(Color::Rgb(68, 78, 128))
                    .fg(Color::Rgb(200, 210, 240)),
            ),
            (false, false) => (
                Style::default()
                    .bg(Color::Rgb(20, 22, 34))
                    .fg(Color::Rgb(170, 178, 200)),
                Style::default()
                    .bg(Color::Rgb(20, 22, 34))
                    .fg(Color::Rgb(110, 118, 142)),
            ),
        };

        buf.set_string(inner.x, y, &pad_to_width(&card.row1, w), r1_style);
        buf.set_string(inner.x, y + 1, &pad_to_width(&card.row2, w), r2_style);

        if let Some((dot_off, st)) = card.dot_col {
            if inner.x + dot_off < inner.x + inner.width {
                buf.set_string(
                    inner.x + dot_off,
                    y,
                    st.symbol(),
                    r1_style.fg(state_color(st)).add_modifier(Modifier::BOLD),
                );
            }
        }

        hits.push(HitRegion {
            rect: Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 2,
            },
            target: card.target,
        });
        if let Some(space_id) = card.collapse_space_id {
            hits.push(HitRegion {
                rect: Rect {
                    x: inner.x,
                    y,
                    width: 4.min(inner.width),
                    height: 2,
                },
                target: HitTarget::ToggleSpaceCollapse(space_id),
            });
        }
        y += 2;
    }

    // Render Footer Buttons (`[+ New Tab]` and `[+ New Space]`) with empty separator rows
    // between them and above the bottom touchbar's `◄≡` collapse button.
    if footer_rows == 7 {
        let tab_y = max_y - 6;
        let tab_r1 = pad_to_width(" [ + New Tab    (^b c) ]", w);
        let tab_r2 = pad_to_width("   open shell in space", w);
        buf.set_string(
            inner.x,
            tab_y,
            &tab_r1,
            Style::default()
                .bg(Color::Rgb(32, 54, 46))
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        );
        buf.set_string(
            inner.x,
            tab_y + 1,
            &tab_r2,
            Style::default()
                .bg(Color::Rgb(32, 54, 46))
                .fg(Color::Rgb(140, 190, 165)),
        );
        hits.push(HitRegion {
            rect: Rect {
                x: inner.x,
                y: tab_y,
                width: inner.width,
                height: 2,
            },
            target: HitTarget::NewTab,
        });

        let sp_y = max_y - 3;
        let sp_r1 = pad_to_width(" [ + New Space  (^b C) ]", w);
        let sp_r2 = pad_to_width("   create workspace", w);
        buf.set_string(
            inner.x,
            sp_y,
            &sp_r1,
            Style::default()
                .bg(Color::Rgb(30, 44, 62))
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        );
        buf.set_string(
            inner.x,
            sp_y + 1,
            &sp_r2,
            Style::default()
                .bg(Color::Rgb(30, 44, 62))
                .fg(Color::Rgb(140, 175, 205)),
        );
        hits.push(HitRegion {
            rect: Rect {
                x: inner.x,
                y: sp_y,
                width: inner.width,
                height: 2,
            },
            target: HitTarget::NewSpacePrompt,
        });
    } else if footer_rows == 4 {
        let tab_y = max_y - 4;
        buf.set_string(
            inner.x,
            tab_y,
            &pad_to_width(" [ + New Tab (^b c) ]", w),
            Style::default()
                .bg(Color::Rgb(32, 54, 46))
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        );
        hits.push(HitRegion {
            rect: Rect {
                x: inner.x,
                y: tab_y,
                width: inner.width,
                height: 1,
            },
            target: HitTarget::NewTab,
        });
        let sp_y = max_y - 2;
        buf.set_string(
            inner.x,
            sp_y,
            &pad_to_width(" [ + New Space (^b C) ]", w),
            Style::default()
                .bg(Color::Rgb(30, 44, 62))
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        );
        hits.push(HitRegion {
            rect: Rect {
                x: inner.x,
                y: sp_y,
                width: inner.width,
                height: 1,
            },
            target: HitTarget::NewSpacePrompt,
        });
    } else if footer_rows == 2 {
        let tab_y = max_y - 2;
        buf.set_string(
            inner.x,
            tab_y,
            &pad_to_width(" [ + New Tab (^b c) ]", w),
            Style::default()
                .bg(Color::Rgb(32, 54, 46))
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
        );
        hits.push(HitRegion {
            rect: Rect {
                x: inner.x,
                y: tab_y,
                width: inner.width,
                height: 1,
            },
            target: HitTarget::NewTab,
        });
        buf.set_string(
            inner.x,
            tab_y + 1,
            &pad_to_width(" [ + New Space (^b C) ]", w),
            Style::default()
                .bg(Color::Rgb(30, 44, 62))
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        );
        hits.push(HitRegion {
            rect: Rect {
                x: inner.x,
                y: tab_y + 1,
                width: inner.width,
                height: 1,
            },
            target: HitTarget::NewSpacePrompt,
        });
    }
}

struct VtScreenWidget<'a> {
    screen: &'a vt100::Screen,
}

impl Widget for VtScreenWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (scr_rows, scr_cols) = self.screen.size();
        let rows = area.height.min(scr_rows);
        let cols = area.width.min(scr_cols);

        for r in 0..rows {
            let mut c = 0;
            while c < cols {
                let Some(cell) = self.screen.cell(r, c) else {
                    c += 1;
                    continue;
                };
                if cell.is_wide_continuation() {
                    c += 1;
                    continue;
                }
                let x = area.x + c;
                let y = area.y + r;
                let mut style = Style::default()
                    .fg(map_vt_color(cell.fgcolor()))
                    .bg(map_vt_color(cell.bgcolor()));

                if cell.bold() {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.italic() {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if cell.underline() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if cell.inverse() {
                    style = style.add_modifier(Modifier::REVERSED);
                }

                let contents = cell.contents();
                let sym = if contents.is_empty() { " " } else { &contents };
                if let Some(dst) = buf.cell_mut((x, y)) {
                    dst.set_symbol(sym);
                    dst.set_style(style);
                }
                if cell.is_wide() && c + 1 < cols {
                    if let Some(dst_next) = buf.cell_mut((x + 1, y)) {
                        dst_next.set_symbol("");
                        dst_next.set_style(style);
                    }
                    c += 2;
                } else {
                    c += 1;
                }
            }
        }
    }
}

fn map_vt_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn centered_rect(width: u16, height: u16, r: Rect) -> Rect {
    let w = width.min(r.width.saturating_sub(2)).max(10);
    let h = height.min(r.height.saturating_sub(2)).max(3);
    Rect {
        x: r.x + (r.width.saturating_sub(w)) / 2,
        y: r.y + (r.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn draw_modal_close_button(buf: &mut Buffer, popup: Rect, hits: &mut Vec<HitRegion>) {
    let close_lbl = " [ ✕ ] ";
    let cw = close_lbl.width() as u16;
    if popup.width > cw + 4 {
        let cx = popup.x + popup.width - cw - 2;
        let cy = popup.y;
        buf.set_string(
            cx,
            cy,
            close_lbl,
            Style::default()
                .bg(Color::Rgb(85, 36, 46))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );
        hits.push(HitRegion {
            rect: Rect {
                x: cx,
                y: cy,
                width: cw,
                height: 1,
            },
            target: HitTarget::CancelModal,
        });
    }
}

fn render_help_modal(f: &mut Frame, area: Rect, hits: &mut Vec<HitRegion>) {
    hits.clear();
    let popup = centered_rect(68, 26, area);
    f.render_widget(Clear, popup);
    hits.push(HitRegion {
        rect: popup,
        target: HitTarget::ModalNoop,
    });

    let lines = vec![
        Line::from(Span::styled(
            "  Touch Quick Actions (Tap to execute):",
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(""),
        Line::from(""),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "  Keyboard & Touch Gesture Shortcuts:",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from("  Tap ● ○ ✓ on Touchbar    Switch directly to Tab (no body swipe conflict)"),
        Line::from("  Tap ▾ / ▸ on Space       Collapse / expand Space's child Tabs in sidebar"),
        Line::from("  Prefix Key               Ctrl+b or Tap [^B] on bottom touchbar"),
        Line::from("  Next / Prev Tab          ^b n / ^b p   or  Alt+Right / Alt+Left"),
        Line::from("  Switch Tab 1..9          ^b 1..9       or  Alt+1..9"),
        Line::from("  New Tab in Space         ^b c          or  Tap [+ New Tab] in sidebar"),
        Line::from("  Rename / Close Tab       ^b , (rename) /   ^b x (close)"),
        Line::from("  Next / Prev Space        ^b Down / Up  or  Alt+Down / Alt+Up"),
        Line::from("  New / Rename Space       ^b C (new)    /   ^b $ (rename)"),
        Line::from("  Toggle Left Sidebar      ^b b          or  Tap ◄≡ / ≡► on touchbar"),
        Line::from("  Quick Fuzzy Switcher     ^b w          or  ^b f"),
        Line::from("  Jump Next Alert Agent    ^b a          (◉ blocked / ✓ done)"),
        Line::from("  Scrollback / History     ^b [          or  Tap [▼ BOT] to exit"),
        Line::from("  Detach (keep running)    ^b q / ^b d   or  Ctrl+\\"),
        Line::from(""),
        Line::from(Span::styled(
            "  Tap outside, [ ✕ ], Esc, Enter, or ? to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let block = Block::default()
        .title(" Quick Actions & Help ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Rgb(22, 24, 36)).fg(Color::White));
    let inner = block.inner(popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);

    let buf = f.buffer_mut();
    draw_modal_close_button(buf, popup, hits);

    // Render 4x3 grid of touch quick-action buttons in rows 1..=4 of `inner`
    let quick_rows: [[(&str, HitTarget); 3]; 4] = [
        [
            (" [ + New Tab    ] ", HitTarget::TriggerAction(Action::NewTab)),
            (
                " [ ✕ Close Tab  ] ",
                HitTarget::TriggerAction(Action::CloseTab),
            ),
            (
                " [ ✎ Rename Tab ] ",
                HitTarget::TriggerAction(Action::RenameTab),
            ),
        ],
        [
            (
                " [ 🔍 Fuzzy Find ] ",
                HitTarget::TriggerAction(Action::FuzzyFinder),
            ),
            (
                " [ ⚡ Next Alert ] ",
                HitTarget::TriggerAction(Action::JumpAlertAgent),
            ),
            (
                " [ ⇕ Scrollback ] ",
                HitTarget::TriggerAction(Action::EnterScrollback),
            ),
        ],
        [
            (
                " [ + New Space  ] ",
                HitTarget::TriggerAction(Action::NewSpace),
            ),
            (
                " [ ✎ Ren Space  ] ",
                HitTarget::TriggerAction(Action::RenameSpace),
            ),
            (
                " [ ⇄ Next Space ] ",
                HitTarget::TriggerAction(Action::NextSpace),
            ),
        ],
        [
            (
                " [ ◧ Sidebar    ] ",
                HitTarget::TriggerAction(Action::ToggleSidebar),
            ),
            (" [ ⏏ Detach hct ] ", HitTarget::TriggerAction(Action::Detach)),
            (" [ ✕ Close Help ] ", HitTarget::CancelModal),
        ],
    ];

    for (r_idx, row_btns) in quick_rows.into_iter().enumerate() {
        let by = inner.y + 1 + r_idx as u16;
        if by >= inner.y + inner.height {
            break;
        }
        let mut bx = inner.x + 2;
        for (lbl, target) in row_btns {
            let bw = lbl.width() as u16;
            if bx + bw > inner.x + inner.width {
                break;
            }
            let bstyle = Style::default()
                .bg(Color::Rgb(42, 52, 82))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD);
            buf.set_string(bx, by, lbl, bstyle);
            hits.push(HitRegion {
                rect: Rect {
                    x: bx,
                    y: by,
                    width: bw,
                    height: 1,
                },
                target,
            });
            bx += bw + 2;
        }
    }
}

fn render_confirm_close_modal(
    f: &mut Frame,
    area: Rect,
    tab_id: &str,
    tab_title: &str,
    hits: &mut Vec<HitRegion>,
) {
    hits.clear();
    let popup = centered_rect(56, 8, area);
    f.render_widget(Clear, popup);
    hits.push(HitRegion {
        rect: popup,
        target: HitTarget::ModalNoop,
    });

    let lines = vec![
        Line::from(""),
        Line::from(format!("  Close tab \"{tab_title}\" and kill its terminal?")),
    ];
    let block = Block::default()
        .title(" Close Tab ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightRed))
        .style(Style::default().bg(Color::Rgb(28, 20, 28)).fg(Color::White));
    let inner = block.inner(popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);

    let buf = f.buffer_mut();
    draw_modal_close_button(buf, popup, hits);

    if inner.height >= 4 {
        let by = inner.y + 3;
        let yes_lbl = " [ ✓ Confirm Close (Y) ] ";
        let no_lbl = " [ ✕ Cancel (N) ] ";
        let yw = yes_lbl.width() as u16;
        let nw = no_lbl.width() as u16;
        let bx = inner.x + 2;
        if bx + yw <= inner.x + inner.width {
            buf.set_string(
                bx,
                by,
                yes_lbl,
                Style::default()
                    .bg(Color::Rgb(110, 36, 44))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            );
            hits.push(HitRegion {
                rect: Rect {
                    x: bx,
                    y: by,
                    width: yw,
                    height: 1,
                },
                target: HitTarget::ConfirmCloseYes {
                    tab_id: tab_id.to_string(),
                },
            });
        }
        let nx = bx + yw + 3;
        if nx + nw <= inner.x + inner.width {
            buf.set_string(
                nx,
                by,
                no_lbl,
                Style::default()
                    .bg(Color::Rgb(48, 52, 72))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            );
            hits.push(HitRegion {
                rect: Rect {
                    x: nx,
                    y: by,
                    width: nw,
                    height: 1,
                },
                target: HitTarget::CancelModal,
            });
        }
    }
}

fn render_prompt_modal(
    f: &mut Frame,
    area: Rect,
    kind: &PromptKind,
    input: &str,
    hits: &mut Vec<HitRegion>,
) {
    hits.clear();
    let popup = centered_rect(56, 8, area);
    f.render_widget(Clear, popup);
    hits.push(HitRegion {
        rect: popup,
        target: HitTarget::ModalNoop,
    });

    let title = match kind {
        PromptKind::RenameTab { .. } => " Rename Tab (empty = auto) ",
        PromptKind::RenameSpace { .. } => " Rename Space (empty = folder name) ",
        PromptKind::NewSpace => " New Space Name ",
    };
    let lines = vec![Line::from(""), Line::from(format!("  > {input}▏"))];
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Rgb(22, 24, 36)).fg(Color::White));
    let inner = block.inner(popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);

    let buf = f.buffer_mut();
    draw_modal_close_button(buf, popup, hits);

    if inner.height >= 4 {
        let by = inner.y + 3;
        let ok_lbl = " [ ✓ Confirm (Enter) ] ";
        let cancel_lbl = " [ ✕ Cancel (Esc) ] ";
        let ow = ok_lbl.width() as u16;
        let cw = cancel_lbl.width() as u16;
        let bx = inner.x + 2;
        if bx + ow <= inner.x + inner.width {
            buf.set_string(
                bx,
                by,
                ok_lbl,
                Style::default()
                    .bg(Color::Rgb(36, 82, 58))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            );
            hits.push(HitRegion {
                rect: Rect {
                    x: bx,
                    y: by,
                    width: ow,
                    height: 1,
                },
                target: HitTarget::SubmitPrompt,
            });
        }
        let cx = bx + ow + 3;
        if cx + cw <= inner.x + inner.width {
            buf.set_string(
                cx,
                by,
                cancel_lbl,
                Style::default()
                    .bg(Color::Rgb(48, 52, 72))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            );
            hits.push(HitRegion {
                rect: Rect {
                    x: cx,
                    y: by,
                    width: cw,
                    height: 1,
                },
                target: HitTarget::CancelModal,
            });
        }
    }
}

fn render_fuzzy_modal(
    f: &mut Frame,
    area: Rect,
    world: &World,
    query: &str,
    selected: usize,
    selected_tab_id: Option<&str>,
    hits: &mut Vec<HitRegion>,
) {
    hits.clear();
    let popup = centered_rect(72, 18, area);
    f.render_widget(Clear, popup);
    hits.push(HitRegion {
        rect: popup,
        target: HitTarget::ModalNoop,
    });

    let block = Block::default()
        .title(" Quick Switch Space / Tab (^b w) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Rgb(22, 24, 36)).fg(Color::White));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(inner);

    let prompt = Paragraph::new(format!(" › {query}▏"))
        .style(Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD));
    f.render_widget(prompt, chunks[0]);

    let items = collect_fuzzy_items(world, query);
    let sel = resolve_fuzzy_selection(&items, selected, selected_tab_id).unwrap_or(0);
    let list_items: Vec<ListItem> = items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let st_col = state_color(it.state);
            let style = if i == sel {
                Style::default()
                    .bg(Color::Rgb(65, 75, 120))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let line = Line::from(vec![
                Span::styled(if i == sel { " ▸ " } else { "   " }, style),
                Span::styled(
                    format!("{} ", it.state.symbol()),
                    style.fg(st_col).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("[{}] ", it.space_name), style.fg(Color::Cyan)),
                Span::styled(format!("{} ", it.tab_name), style),
                Span::styled(
                    format!("({}) {}", it.term_addr, it.cwd),
                    style.fg(Color::DarkGray),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();

    let mut list_state = ListState::default();
    if !list_items.is_empty() {
        list_state.select(Some(sel));
    }
    f.render_stateful_widget(List::new(list_items), chunks[1], &mut list_state);

    let buf = f.buffer_mut();
    draw_modal_close_button(buf, popup, hits);

    // Register touch hit regions for each visible row in the Fuzzy list
    let offset = list_state.offset();
    for row_idx in 0..(chunks[1].height as usize) {
        if let Some(it) = items.get(offset + row_idx) {
            hits.push(HitRegion {
                rect: Rect {
                    x: chunks[1].x,
                    y: chunks[1].y + row_idx as u16,
                    width: chunks[1].width,
                    height: 1,
                },
                target: HitTarget::SelectTab {
                    space_id: it.space_id.clone(),
                    tab_id: it.tab_id.clone(),
                },
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn bottom_touchbar_and_2row_sidebar_expose_touch_hits() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let world = World::default();
        let mut hits = Vec::new();

        terminal
            .draw(|f| {
                let geom = render_ui(
                    f,
                    &world,
                    None,
                    true, // sidebar_open = true
                    24,
                    true, // touch_mode = true (2-row bottom touchbar)
                    false,
                    42, // scrollback_offset > 0
                    &Modal::None,
                );
                hits = geom.hits;
                assert_eq!(geom.term_rect.y, 0);
                assert_eq!(geom.term_rect.height, 22);
            })
            .unwrap();

        // Sidebar toggle on the bottom touchbar (rows 22 and 23) should hit ToggleSidebar
        assert_eq!(
            find_hit_target(&hits, 1, 22),
            Some(HitTarget::ToggleSidebar)
        );
        assert_eq!(
            find_hit_target(&hits, 1, 23),
            Some(HitTarget::ToggleSidebar)
        );

        // Scrollback badge, Prefix button, terminal keys (Esc/Tab/^C/Arrows), and Help button are on the bottom touchbar
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::ExitScrollback && h.rect.height == 2));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::TogglePrefix && h.rect.height == 2));
        assert!(hits.iter().any(|h| {
            h.target == HitTarget::SendKey(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        }));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::ToggleHelp && h.rect.height == 2));

        // Sidebar footer has 2-row tall [+ New Tab] and [+ New Space] buttons with 1-row separator gaps
        let new_tab_hit = hits
            .iter()
            .find(|h| h.target == HitTarget::NewTab && h.rect.height == 2)
            .expect("expected 2-row NewTab hit in sidebar");
        let new_space_hit = hits
            .iter()
            .find(|h| h.target == HitTarget::NewSpacePrompt && h.rect.height == 2)
            .expect("expected 2-row NewSpacePrompt hit in sidebar");
        assert!(new_space_hit.rect.y >= new_tab_hit.rect.y + new_tab_hit.rect.height + 1);
        assert!(22 >= new_space_hit.rect.y + new_space_hit.rect.height + 1);
    }

    #[test]
    fn touchbar_keys_do_not_jitter_across_state_toggles() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let world = World::default();

        let mut prefix_x_1 = 0;
        let mut prefix_x_2 = 0;
        terminal
            .draw(|f| {
                let geom = render_ui(f, &world, None, true, 24, false, false, 0, &Modal::None);
                prefix_x_1 = geom
                    .hits
                    .iter()
                    .find(|h| h.target == HitTarget::TogglePrefix)
                    .unwrap()
                    .rect
                    .x;
            })
            .unwrap();
        terminal
            .draw(|f| {
                let geom = render_ui(f, &world, None, false, 24, false, true, 15, &Modal::None);
                prefix_x_2 = geom
                    .hits
                    .iter()
                    .find(|h| h.target == HitTarget::TogglePrefix)
                    .unwrap()
                    .rect
                    .x;
            })
            .unwrap();
        assert_eq!(prefix_x_1, prefix_x_2);
    }

    #[test]
    fn modals_capture_hits_and_expose_action_buttons() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let world = World::default();
        let mut hits = Vec::new();

        terminal
            .draw(|f| {
                let geom = render_ui(
                    f,
                    &world,
                    None,
                    false,
                    24,
                    false,
                    false,
                    0,
                    &Modal::ConfirmCloseTab {
                        tab_id: "tab-123".to_string(),
                        tab_title: "zsh".to_string(),
                    },
                );
                hits = geom.hits;
            })
            .unwrap();

        // Top bar hits are cleared while modal is open so clicks outside dismiss the modal
        assert_eq!(find_hit_target(&hits, 0, 0), None);
        assert!(hits.iter().any(|h| {
            h.target
                == HitTarget::ConfirmCloseYes {
                    tab_id: "tab-123".to_string(),
                }
        }));
        assert!(hits.iter().any(|h| h.target == HitTarget::CancelModal));
    }

    #[test]
    fn narrow_terminal_retains_compact_sidebar_toggle() {
        let backend = TestBackend::new(30, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let world = World::default();
        let mut hits = Vec::new();

        terminal
            .draw(|f| {
                let geom = render_ui(
                    f,
                    &world,
                    None,
                    false,
                    24,
                    false,
                    false,
                    0,
                    &Modal::None,
                );
                hits = geom.hits;
            })
            .unwrap();

        assert_eq!(
            find_hit_target(&hits, 1, 11),
            Some(HitTarget::ToggleSidebar)
        );
    }
}
