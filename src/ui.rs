//! TUI rendering (`ratatui` + `vt100`) with Top Tab Bar, Collapsible Left Sidebar,
//! touch/mouse hit-testing regions, and modal overlays.

use crate::keys::Action;
use crate::model::{AgentState, World};
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
    NextSpace,
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

pub fn top_bar_height(area: Rect, touch_mode: bool) -> u16 {
    if area.height == 0 {
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
    let tb_h = top_bar_height(area, touch_mode);
    let body_height = area.height.saturating_sub(tb_h).max(1);
    let body_y = area.y + tb_h.min(area.height.saturating_sub(1));
    let sw = if sidebar_open && area.width > sidebar_width + 20 {
        sidebar_width
    } else {
        0
    };
    Rect {
        x: area.x + sw,
        y: body_y,
        width: area.width.saturating_sub(sw).max(1),
        height: body_height,
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
    let tb_h = top_bar_height(area, touch_mode);

    let top_rect = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: tb_h,
    };

    render_top_bar(
        f,
        top_rect,
        world,
        sidebar_open,
        prefix_active,
        scrollback_offset,
        &mut hits,
    );

    let term_rect = compute_terminal_rect(area, sidebar_open, sidebar_width, touch_mode);
    let actual_sidebar_w = term_rect.x.saturating_sub(area.x);

    if actual_sidebar_w > 0 && area.height > tb_h {
        let side_rect = Rect {
            x: area.x,
            y: area.y + tb_h,
            width: actual_sidebar_w,
            height: area.height - tb_h,
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

    match modal {
        Modal::None => {}
        Modal::Help => render_help_modal(f, area, &mut hits),
        Modal::ConfirmCloseTab {
            tab_id,
            tab_title,
        } => render_confirm_close_modal(f, area, tab_id, tab_title, &mut hits),
        Modal::Prompt { kind, input } => render_prompt_modal(f, area, kind, input, &mut hits),
        Modal::Fuzzy {
            query,
            selected,
            selected_tab_id,
        } => render_fuzzy_modal(
            f,
            area,
            world,
            query,
            *selected,
            selected_tab_id.as_deref(),
            &mut hits,
        ),
    }

    UiGeometry { term_rect, hits }
}

fn draw_top_bar_pill(
    buf: &mut Buffer,
    x: u16,
    rect: Rect,
    label: &str,
    style: Style,
    emphasized_underline: bool,
) {
    if rect.height == 0 {
        return;
    }
    let lw = label.width() as u16;
    buf.set_string(x, rect.y, label, style);
    if rect.height >= 2 && lw > 0 {
        let line_ch = if emphasized_underline { "═" } else { "─" };
        let mut bottom = String::with_capacity(lw as usize * 3);
        if lw >= 2 {
            bottom.push(' ');
            for _ in 0..(lw - 2) {
                bottom.push_str(line_ch);
            }
            bottom.push(' ');
        } else {
            bottom.push_str(line_ch);
        }
        buf.set_string(x, rect.y + 1, &bottom, style);
    }
}

fn render_top_bar(
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
    let bg_bar = Style::default().bg(Color::Rgb(24, 24, 36)).fg(Color::Gray);
    f.render_widget(Block::default().style(bg_bar), rect);

    let mut x = rect.x;
    let max_x = rect.x + rect.width;
    let buf = f.buffer_mut();

    // 1. Sidebar toggle button `[≡]`
    let sb_label = if sidebar_open { " ◄≡ " } else { " ≡► " };
    let sb_w = sb_label.width() as u16;
    if x + sb_w <= max_x {
        let style = Style::default()
            .bg(if sidebar_open {
                Color::Rgb(55, 55, 85)
            } else {
                Color::Rgb(36, 36, 54)
            })
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);
        draw_top_bar_pill(buf, x, rect, sb_label, style, sidebar_open);
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

    // 2. Active Space badge `[Space]`
    if let Some(space) = world.active_space() {
        let sname = truncate_to_width(&world.space_display_name(space), 14);
        let sstate = world.space_state(space);
        let badge = format!(" [{} {}] ", sstate.symbol(), sname);
        let bw = badge.width() as u16;
        if x + bw <= max_x {
            let style = Style::default()
                .bg(Color::Rgb(42, 46, 68))
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD);
            draw_top_bar_pill(buf, x, rect, &badge, style, true);
            hits.push(HitRegion {
                rect: Rect {
                    x,
                    y: rect.y,
                    width: bw,
                    height: rect.height,
                },
                target: HitTarget::NextSpace,
            });
            x += bw;
        }
    }

    // Reserve right touch buttons: optional `[SCROLL -N ▼]`, `[^B]`, and `[ ? ]`
    let scroll_lbl = if scrollback_offset > 0 {
        Some(format!(" [SCROLL -{scrollback_offset} ▼] "))
    } else {
        None
    };
    let prefix_lbl = if prefix_active { " [^B*] " } else { " [^B] " };
    let help_lbl = " [ ? ] ";

    let scroll_w = scroll_lbl.as_ref().map_or(0, |s| s.width() as u16);
    let prefix_w = prefix_lbl.width() as u16;
    let help_w = help_lbl.width() as u16;
    let total_right_w = scroll_w + prefix_w + help_w;
    let avail_end = max_x.saturating_sub(total_right_w);

    // 3. Tabs strip in active Space (scrolled so active_tab_index is visible)
    if let Some(space) = world.active_space() {
        let active_ti = world.active_tab_index();
        let space_id = space.id.clone();

        // Precompute tab labels
        let tab_items: Vec<(usize, String, String, AgentState, bool)> = space
            .tabs
            .iter()
            .enumerate()
            .map(|(ti, tab)| {
                let name = truncate_to_width(&world.tab_display_name(space, ti), 18);
                let panes = world.tab_terminals(tab).len();
                let pane_suffix = if panes > 1 {
                    format!("({panes})")
                } else {
                    String::new()
                };
                let st = world.tab_state(tab);
                let label = format!(" {}:{} {}{} ", ti + 1, st.symbol(), name, pane_suffix);
                (ti, tab.id.clone(), label, st, ti == active_ti)
            })
            .collect();

        // Find start index so that active_ti fits in `avail_end - x - 5`
        let plus_w = 5u16; // ` [+] `
        let budget = avail_end.saturating_sub(x).saturating_sub(plus_w);
        let mut start_idx = 0;
        if !tab_items.is_empty() {
            let mut sum = 0u16;
            for &(ti, _, ref lbl, _, _) in &tab_items[..=active_ti.min(tab_items.len() - 1)] {
                sum += lbl.width() as u16 + 1;
                while sum > budget && start_idx < ti {
                    sum = sum.saturating_sub(tab_items[start_idx].2.width() as u16 + 1);
                    start_idx += 1;
                }
            }
        }

        let max_tab_end = avail_end.saturating_sub(plus_w);
        for (vis_idx, &(ti, ref tid, ref lbl, st, is_active)) in
            tab_items.iter().skip(start_idx).enumerate()
        {
            let remaining = max_tab_end.saturating_sub(x);
            let rendered_lbl = if (lbl.width() as u16) <= remaining {
                lbl.clone()
            } else if vis_idx == 0 && remaining >= 4 {
                truncate_to_width(lbl, remaining as usize)
            } else {
                break;
            };
            let lw = rendered_lbl.width() as u16;
            let tab_bg = if is_active {
                Color::Rgb(72, 82, 130)
            } else {
                Color::Rgb(32, 34, 48)
            };
            let tab_fg = if is_active {
                Color::White
            } else {
                Color::Rgb(190, 195, 215)
            };
            let mut style = Style::default().bg(tab_bg).fg(tab_fg);
            if is_active {
                style = style.add_modifier(Modifier::BOLD);
            }
            draw_top_bar_pill(buf, x, rect, &rendered_lbl, style, is_active);

            // Colorize the status dot inside the tab pill if it wasn't truncated out
            let num_prefix = format!(" {}:", ti + 1);
            let dot_x = x + (num_prefix.width() as u16);
            if dot_x < x + lw {
                buf.set_string(
                    dot_x,
                    rect.y,
                    st.symbol(),
                    style.fg(state_color(st)).add_modifier(Modifier::BOLD),
                );
            }

            hits.push(HitRegion {
                rect: Rect {
                    x,
                    y: rect.y,
                    width: lw,
                    height: rect.height,
                },
                target: HitTarget::SelectTab {
                    space_id: space_id.clone(),
                    tab_id: tid.clone(),
                },
            });
            x += lw;

            if x < avail_end {
                for r in 0..rect.height {
                    buf.set_string(x, rect.y + r, "│", bg_bar.fg(Color::Rgb(60, 60, 80)));
                }
                x += 1;
            }
        }

        // 4. `[+]` New Tab button
        let plus_lbl = " [+] ";
        let pw = plus_lbl.width() as u16;
        if x + pw <= avail_end {
            let pstyle = Style::default()
                .bg(Color::Rgb(36, 56, 46))
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD);
            draw_top_bar_pill(buf, x, rect, plus_lbl, pstyle, false);
            hits.push(HitRegion {
                rect: Rect {
                    x,
                    y: rect.y,
                    width: pw,
                    height: rect.height,
                },
                target: HitTarget::NewTab,
            });
        }
    }

    // 5. Right touch buttons: `[SCROLL -N ▼]`, `[^B]`, and `[ ? ]`
    let mut rx = avail_end;
    if let Some(ref slbl) = scroll_lbl {
        if rx + scroll_w <= max_x {
            let sstyle = Style::default()
                .bg(Color::LightCyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD);
            draw_top_bar_pill(buf, rx, rect, slbl, sstyle, true);
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

    if rx + prefix_w <= max_x {
        let pstyle = if prefix_active {
            Style::default()
                .bg(Color::LightYellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .bg(Color::Rgb(44, 46, 68))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        };
        draw_top_bar_pill(buf, rx, rect, prefix_lbl, pstyle, prefix_active);
        hits.push(HitRegion {
            rect: Rect {
                x: rx,
                y: rect.y,
                width: prefix_w,
                height: rect.height,
            },
            target: HitTarget::TogglePrefix,
        });
        rx += prefix_w;
    }

    if rx + help_w <= max_x {
        let hstyle = Style::default()
            .bg(Color::Rgb(36, 36, 54))
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD);
        draw_top_bar_pill(buf, rx, rect, help_lbl, hstyle, false);
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

fn render_sidebar(f: &mut Frame, rect: Rect, world: &World, hits: &mut Vec<HitRegion>) {
    let bg = Style::default()
        .bg(Color::Rgb(20, 22, 32))
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

    // Header
    if y < max_y {
        let hdr = " SPACES & AGENTS";
        buf.set_string(
            inner.x,
            y,
            hdr,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        y += 1;
    }

    for (si, space) in world.doc.spaces.iter().enumerate() {
        if y >= max_y.saturating_sub(1) {
            break;
        }
        let is_active_space = si == world.active_space_idx;
        let sname = world.space_display_name(space);
        let sstate = world.space_state(space);
        let arrow = if is_active_space { "▾" } else { "▸" };
        let line = format!(" {arrow} {} {sname}", sstate.symbol());
        let clipped: String = line.chars().take(inner.width as usize).collect();

        let s_style = if is_active_space {
            Style::default()
                .bg(Color::Rgb(42, 48, 76))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Rgb(180, 185, 205))
        };
        buf.set_string(
            inner.x,
            y,
            &format!("{:<width$}", clipped, width = inner.width as usize),
            s_style,
        );
        hits.push(HitRegion {
            rect: Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            },
            target: HitTarget::SelectSpace(space.id.clone()),
        });
        y += 1;

        let active_ti = if is_active_space {
            Some(world.active_tab_index())
        } else {
            None
        };

        for (ti, tab) in space.tabs.iter().enumerate() {
            if y >= max_y.saturating_sub(1) {
                break;
            }
            let is_active_tab = active_ti == Some(ti);
            let tname = world.tab_display_name(space, ti);
            let st = world.tab_state(tab);
            let row_str = format!("   {}:{} {}", ti + 1, st.symbol(), tname);
            let row_clipped: String = row_str.chars().take(inner.width as usize).collect();

            let t_style = if is_active_tab {
                Style::default()
                    .bg(Color::Rgb(68, 78, 125))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Rgb(165, 170, 190))
            };
            buf.set_string(
                inner.x,
                y,
                &format!("{:<width$}", row_clipped, width = inner.width as usize),
                t_style,
            );
            // Highlight status dot
            let prefix_len = format!("   {}:", ti + 1).width() as u16;
            if inner.x + prefix_len < inner.x + inner.width {
                buf.set_string(
                    inner.x + prefix_len,
                    y,
                    st.symbol(),
                    t_style.fg(state_color(st)).add_modifier(Modifier::BOLD),
                );
            }

            hits.push(HitRegion {
                rect: Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
                target: HitTarget::SelectTab {
                    space_id: space.id.clone(),
                    tab_id: tab.id.clone(),
                },
            });
            y += 1;
        }
    }

    // Footer button `[+ New Space]`
    if y < max_y {
        let btn = " [+ New Space (^b C)]";
        let clipped: String = btn.chars().take(inner.width as usize).collect();
        buf.set_string(
            inner.x,
            max_y - 1,
            &format!("{:<width$}", clipped, width = inner.width as usize),
            Style::default()
                .bg(Color::Rgb(30, 44, 38))
                .fg(Color::LightGreen),
        );
        hits.push(HitRegion {
            rect: Rect {
                x: inner.x,
                y: max_y - 1,
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
        Line::from("  Swipe Left / Right       In terminal body -> Next / Prev Tab"),
        Line::from("  Prefix Key               Ctrl+b or Tap [^B] on top bar"),
        Line::from("  Next / Prev Tab          ^b n / ^b p   or  Alt+Right / Alt+Left"),
        Line::from("  Switch Tab 1..9          ^b 1..9       or  Alt+1..9"),
        Line::from("  New Tab in Space         ^b c          or  Tap [+] on top bar"),
        Line::from("  Rename / Close Tab       ^b , (rename) /   ^b x (close)"),
        Line::from("  Next / Prev Space        ^b Down / Up  or  Alt+Down / Alt+Up"),
        Line::from("  New / Rename Space       ^b C (new)    /   ^b $ (rename)"),
        Line::from("  Toggle Left Sidebar      ^b b          or  Tap [≡] on top bar"),
        Line::from("  Quick Fuzzy Switcher     ^b w          or  ^b f"),
        Line::from("  Jump Next Alert Agent    ^b a          (◉ blocked / ✓ done)"),
        Line::from("  Scrollback / History     ^b [          or  Tap [SCROLL ▼] to exit"),
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
    fn top_bar_touch_mode_doubles_hit_height_and_exposes_buttons() {
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
                    false,
                    24,
                    true, // touch_mode = true (2-row top bar)
                    false,
                    42, // scrollback_offset > 0
                    &Modal::None,
                );
                hits = geom.hits;
                assert_eq!(geom.term_rect.y, 2);
            })
            .unwrap();

        // Sidebar toggle on row 0 and row 1 should both hit ToggleSidebar
        assert_eq!(
            find_hit_target(&hits, 1, 0),
            Some(HitTarget::ToggleSidebar)
        );
        assert_eq!(
            find_hit_target(&hits, 1, 1),
            Some(HitTarget::ToggleSidebar)
        );

        // Scrollback badge, Prefix button, and Help button should all be present with height 2
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::ExitScrollback && h.rect.height == 2));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::TogglePrefix && h.rect.height == 2));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::ToggleHelp && h.rect.height == 2));
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
}
