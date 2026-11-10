//! TUI rendering (`ratatui` + `vt100`) with Top Tab Bar, Collapsible Left Sidebar,
//! touch/mouse hit-testing regions, and modal overlays.

use crate::model::{AgentState, World};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Widget},
    Frame,
};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug)]
pub enum HitTarget {
    ToggleSidebar,
    NextSpace,
    SelectSpace(usize),
    SelectTab { space_idx: usize, tab_idx: usize },
    NewTab,
    NewSpacePrompt,
    ToggleHelp,
}

#[derive(Clone, Debug)]
pub struct HitRegion {
    pub rect: Rect,
    pub target: HitTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Modal {
    None,
    Help,
    ConfirmCloseTab,
    Prompt {
        kind: PromptKind,
        input: String,
    },
    Fuzzy {
        query: String,
        selected: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    RenameTab,
    RenameSpace,
    NewSpace,
}

#[derive(Clone, Debug)]
pub struct FuzzyItem {
    pub space_idx: usize,
    pub tab_idx: usize,
    pub space_name: String,
    pub tab_name: String,
    pub term_addr: String,
    pub state: AgentState,
    pub cwd: String,
}

pub fn collect_fuzzy_items(world: &World, query: &str) -> Vec<FuzzyItem> {
    let q = query.trim().to_lowercase();
    let mut out = Vec::new();
    for (si, space) in world.doc.spaces.iter().enumerate() {
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
                    space_idx: si,
                    tab_idx: ti,
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

pub fn compute_terminal_rect(area: Rect, sidebar_open: bool, sidebar_width: u16) -> Rect {
    let body_height = area.height.saturating_sub(1).max(1);
    let body_y = area.y + 1;
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

pub fn render_ui(
    f: &mut Frame,
    world: &World,
    screen: Option<&vt100::Screen>,
    sidebar_open: bool,
    sidebar_width: u16,
    prefix_active: bool,
    scrollback_offset: usize,
    modal: &Modal,
) -> UiGeometry {
    let area = f.area();
    let mut hits = Vec::new();

    let top_rect = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1.min(area.height),
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

    let term_rect = compute_terminal_rect(area, sidebar_open, sidebar_width);
    let actual_sidebar_w = term_rect.x.saturating_sub(area.x);

    if actual_sidebar_w > 0 && area.height > 1 {
        let side_rect = Rect {
            x: area.x,
            y: area.y + 1,
            width: actual_sidebar_w,
            height: area.height - 1,
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
        Modal::Help => render_help_modal(f, area),
        Modal::ConfirmCloseTab => render_confirm_close_modal(f, area, world),
        Modal::Prompt { kind, input } => render_prompt_modal(f, area, *kind, input),
        Modal::Fuzzy { query, selected } => render_fuzzy_modal(f, area, world, query, *selected),
    }

    UiGeometry { term_rect, hits }
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
        buf.set_string(x, rect.y, sb_label, style);
        hits.push(HitRegion {
            rect: Rect {
                x,
                y: rect.y,
                width: sb_w,
                height: 1,
            },
            target: HitTarget::ToggleSidebar,
        });
        x += sb_w;
    }

    // 2. Active Space badge `[Space]`
    if let Some(space) = world.active_space() {
        let sname = world.space_display_name(space);
        let sstate = world.space_state(space);
        let badge = format!(" [{} {}] ", sstate.symbol(), sname);
        let bw = badge.width() as u16;
        if x + bw <= max_x {
            let style = Style::default()
                .bg(Color::Rgb(42, 46, 68))
                .fg( Color::Cyan )
                .add_modifier(Modifier::BOLD);
            buf.set_string(x, rect.y, &badge, style);
            hits.push(HitRegion {
                rect: Rect {
                    x,
                    y: rect.y,
                    width: bw,
                    height: 1,
                },
                target: HitTarget::NextSpace,
            });
            x += bw;
        }
    }

    // Reserve right status area width
    let right_text = if prefix_active {
        " PREFIX (^b) "
    } else if scrollback_offset > 0 {
        " SCROLL (q:exit) "
    } else {
        " ^b ?:Help "
    };
    let right_w = right_text.width() as u16;
    let avail_end = max_x.saturating_sub(right_w);

    // 3. Tabs strip in active Space (scrolled so active_tab_index is visible)
    if let Some(space) = world.active_space() {
        let active_ti = world.active_tab_index();
        let space_idx = world.active_space_idx;

        // Precompute tab labels
        let tab_items: Vec<(usize, String, AgentState, bool)> = space
            .tabs
            .iter()
            .enumerate()
            .map(|(ti, tab)| {
                let name = world.tab_display_name(space, ti);
                let panes = world.tab_terminals(tab).len();
                let pane_suffix = if panes > 1 {
                    format!("({panes})")
                } else {
                    String::new()
                };
                let st = world.tab_state(tab);
                let label = format!(" {}:{} {}{} ", ti + 1, st.symbol(), name, pane_suffix);
                (ti, label, st, ti == active_ti)
            })
            .collect();

        // Find start index so that active_ti fits in `avail_end - x - 5`
        let plus_w = 5u16; // ` [+] `
        let budget = avail_end.saturating_sub(x).saturating_sub(plus_w);
        let mut start_idx = 0;
        if !tab_items.is_empty() {
            let mut sum = 0u16;
            for &(ti, ref lbl, _, _) in &tab_items[..=active_ti.min(tab_items.len() - 1)] {
                sum += lbl.width() as u16 + 1;
                while sum > budget && start_idx < ti {
                    sum = sum.saturating_sub(tab_items[start_idx].1.width() as u16 + 1);
                    start_idx += 1;
                }
            }
        }

        for &(ti, ref lbl, st, is_active) in tab_items.iter().skip(start_idx) {
            let lw = lbl.width() as u16;
            if x + lw + plus_w > avail_end {
                break;
            }
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
            buf.set_string(x, rect.y, lbl, style);

            // Colorize the status dot inside the tab pill
            let num_prefix = format!(" {}:", ti + 1);
            let dot_x = x + (num_prefix.width() as u16);
            if dot_x < avail_end {
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
                    height: 1,
                },
                target: HitTarget::SelectTab {
                    space_idx,
                    tab_idx: ti,
                },
            });
            x += lw;

            if x < avail_end {
                buf.set_string(x, rect.y, "│", bg_bar.fg(Color::Rgb(60, 60, 80)));
                x += 1;
            }
        }

        // 4. `[+]` New Tab button
        let plus_lbl = " [+] ";
        let pw = plus_lbl.width() as u16;
        if x + pw <= avail_end {
            buf.set_string(
                x,
                rect.y,
                plus_lbl,
                Style::default()
                    .bg(Color::Rgb(36, 56, 46))
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
            );
            hits.push(HitRegion {
                rect: Rect {
                    x,
                    y: rect.y,
                    width: pw,
                    height: 1,
                },
                target: HitTarget::NewTab,
            });
        }
    }

    // 5. Right status pill
    if avail_end + right_w <= max_x {
        let rstyle = if prefix_active {
            Style::default()
                .bg(Color::LightYellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else if scrollback_offset > 0 {
            Style::default()
                .bg(Color::LightCyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(Color::Rgb(36, 36, 54)).fg(Color::Gray)
        };
        buf.set_string(avail_end, rect.y, right_text, rstyle);
        hits.push(HitRegion {
            rect: Rect {
                x: avail_end,
                y: rect.y,
                width: right_w,
                height: 1,
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
            target: HitTarget::SelectSpace(si),
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
                    space_idx: si,
                    tab_idx: ti,
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

fn render_help_modal(f: &mut Frame, area: Rect) {
    let popup = centered_rect(64, 21, area);
    f.render_widget(Clear, popup);
    let lines = vec![
        Line::from(Span::styled(
            "herdr-tui (hct) Keyboard & Touch Shortcuts",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  Prefix Key               Ctrl+b (press twice to send literal ^b)"),
        Line::from("  Next / Prev Tab          ^b n / ^b p   or  Alt+Right / Alt+Left"),
        Line::from("  Switch Tab 1..9          ^b 1..9       or  Alt+1..9"),
        Line::from("  New Tab in Space         ^b c          or  Tap [+] on top bar"),
        Line::from("  Rename Current Tab       ^b ,"),
        Line::from("  Close Current Tab        ^b x"),
        Line::from("  Next / Prev Space        ^b Down / Up  or  Alt+Down / Alt+Up"),
        Line::from("  New / Rename Space       ^b C (new)    /   ^b $ (rename)"),
        Line::from("  Toggle Left Sidebar      ^b b          or  Tap [≡] on top bar"),
        Line::from("  Quick Fuzzy Switcher     ^b w          or  ^b f"),
        Line::from("  Jump Next Alert Agent    ^b a          (◉ blocked / ✓ done)"),
        Line::from("  Cycle Pane in Split Tab  ^b o"),
        Line::from("  Scrollback / History     ^b [          or  Mouse Wheel Up/Down"),
        Line::from("  Detach (keep running)    ^b q / ^b d   or  Ctrl+\\"),
        Line::from(""),
        Line::from(Span::styled(
            "Press Esc, Enter, or ? to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Rgb(22, 24, 36)).fg(Color::White));
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

fn render_confirm_close_modal(f: &mut Frame, area: Rect, world: &World) {
    let popup = centered_rect(50, 7, area);
    f.render_widget(Clear, popup);
    let tab_title = world
        .active_space()
        .map(|s| world.tab_display_name(s, world.active_tab_index()))
        .unwrap_or_else(|| "current tab".to_string());
    let lines = vec![
        Line::from(""),
        Line::from(format!("  Close tab \"{tab_title}\" and kill its terminal?")),
        Line::from(""),
        Line::from(Span::styled(
            "  [y / Enter] Confirm     [n / Esc] Cancel",
            Style::default().fg(Color::LightYellow),
        )),
    ];
    let block = Block::default()
        .title(" Close Tab ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightRed))
        .style(Style::default().bg(Color::Rgb(28, 20, 28)).fg(Color::White));
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

fn render_prompt_modal(f: &mut Frame, area: Rect, kind: PromptKind, input: &str) {
    let popup = centered_rect(52, 6, area);
    f.render_widget(Clear, popup);
    let title = match kind {
        PromptKind::RenameTab => " Rename Tab (empty = auto) ",
        PromptKind::RenameSpace => " Rename Space (empty = folder name) ",
        PromptKind::NewSpace => " New Space Name ",
    };
    let lines = vec![
        Line::from(""),
        Line::from(format!("  > {input}▏")),
        Line::from(""),
        Line::from(Span::styled(
            "  [Enter] Save     [Esc] Cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Rgb(22, 24, 36)).fg(Color::White));
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

fn render_fuzzy_modal(f: &mut Frame, area: Rect, world: &World, query: &str, selected: usize) {
    let popup = centered_rect(72, 18, area);
    f.render_widget(Clear, popup);
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
    let sel = selected.min(items.len().saturating_sub(1));
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

    f.render_widget(List::new(list_items), chunks[1]);
}
