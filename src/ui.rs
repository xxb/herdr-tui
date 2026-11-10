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
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Widget},
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
    ToggleCtrl,
    ToggleAlt,
    PasteClipboard,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSelection {
    pub anchor: (u16, u16),
    pub head: (u16, u16),
    pub active: bool,
}

impl TerminalSelection {
    pub fn new(row: u16, col: u16) -> Self {
        Self {
            anchor: (row, col),
            head: (row, col),
            active: true,
        }
    }

    pub fn is_non_empty(&self) -> bool {
        self.anchor != self.head
    }

    pub fn normalized(&self) -> ((u16, u16), (u16, u16)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    pub fn contains(&self, row: u16, col: u16) -> bool {
        if !self.is_non_empty() {
            return false;
        }
        let ((sr, sc), (er, ec)) = self.normalized();
        if row < sr || row > er {
            return false;
        }
        if sr == er {
            col >= sc && col <= ec
        } else if row == sr {
            col >= sc
        } else if row == er {
            col <= ec
        } else {
            true
        }
    }

    pub fn extract_text(&self, screen: &vt100::Screen) -> String {
        if !self.is_non_empty() {
            return String::new();
        }
        let ((sr, sc), (er, ec)) = self.normalized();
        let (scr_rows, scr_cols) = screen.size();
        if scr_rows == 0 || scr_cols == 0 {
            return String::new();
        }
        let end_row = er.min(scr_rows.saturating_sub(1));
        let end_col_excl = ec.min(scr_cols.saturating_sub(1)).saturating_add(1);
        screen.contents_between(sr, sc, end_row, end_col_excl)
    }
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
    ctrl_active: bool,
    alt_active: bool,
    scrollback_offset: usize,
    selection: Option<&TerminalSelection>,
    copied_flash: bool,
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
        f.render_widget(
            VtScreenWidget {
                screen: scr,
                selection,
            },
            term_rect,
        );
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
                ctrl_active,
                alt_active,
                scrollback_offset,
                copied_flash,
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
                ctrl_active,
                alt_active,
                scrollback_offset,
                copied_flash,
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
                ctrl_active,
                alt_active,
                scrollback_offset,
                copied_flash,
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
                ctrl_active,
                alt_active,
                scrollback_offset,
                copied_flash,
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
                ctrl_active,
                alt_active,
                scrollback_offset,
                copied_flash,
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
    if rect.height >= 2 && lw > 0 {
        // Row 0 (`rect.y`): continuous highlighted MacBook Touch Bar top divider line (`━`),
        // with brighter accent over each keycap pill (and warm gold when armed/active).
        let top_fg = if emphasized {
            Color::Rgb(245, 194, 118)
        } else {
            Color::Rgb(110, 150, 235)
        };
        let top_line = "━".repeat(lw as usize);
        buf.set_string(
            x,
            rect.y,
            &top_line,
            Style::default()
                .bg(Color::Rgb(12, 14, 22))
                .fg(top_fg)
                .add_modifier(Modifier::BOLD),
        );
        // Row 1 (`rect.y + 1`): centered 1-row OLED keycap pill
        buf.set_string(x, rect.y + 1, label, style);
    } else {
        buf.set_string(x, rect.y, label, style);
    }
}

fn render_bottom_touchbar(
    f: &mut Frame,
    rect: Rect,
    world: &World,
    sidebar_open: bool,
    prefix_active: bool,
    ctrl_active: bool,
    alt_active: bool,
    scrollback_offset: usize,
    copied_flash: bool,
    hits: &mut Vec<HitRegion>,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let bg_bar = Style::default()
        .bg(Color::Rgb(12, 14, 22))
        .fg(Color::Rgb(169, 177, 214));
    f.render_widget(Block::default().style(bg_bar), rect);

    let mut x = rect.x;
    let max_x = rect.x + rect.width;
    let buf = f.buffer_mut();

    // Draw continuous highlighted top divider line across the entire MacBook Touch Bar (`rect.y`)
    // when in 2-row mode so the bar forms a unified screen strip separated from the terminal above.
    if rect.height >= 2 {
        let full_line = "━".repeat(rect.width as usize);
        buf.set_string(
            rect.x,
            rect.y,
            &full_line,
            Style::default()
                .bg(Color::Rgb(12, 14, 22))
                .fg(Color::Rgb(72, 98, 158)),
        );
    }

    // Reserve right-side space for optional `✓ Copied`, `▼ BOT`, and `?` keycaps
    let narrow_bar = rect.width < 76;
    let copied_lbl = if copied_flash {
        Some(if narrow_bar { " ✓ " } else { " ✓ Copied " })
    } else {
        None
    };
    let scroll_lbl = if scrollback_offset > 0 {
        Some(if narrow_bar { " ▼ " } else { " ▼ BOT " })
    } else {
        None
    };
    let help_lbl = if narrow_bar { " ? " } else { "  ?  " };
    let copied_w = copied_lbl.map_or(0, |s| s.width() as u16 + 1);
    let scroll_w = scroll_lbl.map_or(0, |s| s.width() as u16 + 1);
    let help_w = help_lbl.width() as u16;
    let right_reserved = (copied_w + scroll_w + help_w).min(rect.width.saturating_sub(4));
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
                Color::Rgb(51, 59, 94)
            } else {
                Color::Rgb(31, 35, 53)
            })
            .fg(Color::Rgb(192, 202, 245))
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
        x += 1;
    }

    // 2. Left Space Name + Hollow/Solid Tab Circles (`●` = active tab, `○` = inactive tab):
    // When `rect.height >= 2`, row 0 shows the current Space name and row 1 underneath it shows `● ○ ○`.
    // Compute `dots_budget` strictly from the state-independent `base_end` so `^B` and all key buttons
    // stay at the exact same column regardless of `copied_flash` or `scrollback_offset`.
    let dots_start_x = x;
    let base_end = max_x.saturating_sub(help_w.min(rect.width.saturating_sub(4)));
    let min_key_reserve: u16 = 29;
    let dots_budget = base_end
        .saturating_sub(dots_start_x)
        .saturating_sub(min_key_reserve)
        .min(11);
    let dots_limit_x = (dots_start_x + dots_budget).min(max_x);

    if dots_budget >= 2 {
        if let Some(space) = world.active_space() {
            let sname = world.space_display_name(space);
            let active_ti = world.active_tab_index();
            let space_id = space.id.clone();
            let total_tabs = space.tabs.len();
            let zone_bg = Style::default()
                .bg(Color::Rgb(24, 27, 42))
                .fg(Color::Rgb(125, 207, 255))
                .add_modifier(Modifier::BOLD);

            if rect.height >= 2 {
                // Top row: current Space name
                let sname_padded = pad_to_width(&format!(" {sname}"), dots_budget as usize);
                buf.set_string(dots_start_x, rect.y, &sname_padded, zone_bg);
                // Bottom row background fill
                buf.set_string(
                    dots_start_x,
                    rect.y + 1,
                    &" ".repeat(dots_budget as usize),
                    Style::default().bg(Color::Rgb(24, 27, 42)),
                );
                // Default hit target across the Space+Dots widget cycles to the next tab
                hits.push(HitRegion {
                    rect: Rect {
                        x: dots_start_x,
                        y: rect.y,
                        width: dots_budget,
                        height: rect.height,
                    },
                    target: HitTarget::TriggerAction(Action::NextTab),
                });

                // Bottom row: solid `●` for current tab, hollow `○` for other tabs in this Space
                let compact_dots = total_tabs > 5 || (dots_budget as usize) < total_tabs * 2;
                let dot_step: usize = if compact_dots { 1 } else { 2 };
                let max_visible_dots = (dots_budget.saturating_sub(1) as usize / dot_step).max(1);
                let start_ti = if total_tabs <= max_visible_dots || active_ti < max_visible_dots {
                    0
                } else {
                    (active_ti + 1 - max_visible_dots)
                        .min(total_tabs.saturating_sub(max_visible_dots))
                };

                let mut dx = dots_start_x + 1;
                for (ti, tab) in space
                    .tabs
                    .iter()
                    .enumerate()
                    .skip(start_ti)
                    .take(max_visible_dots)
                {
                    let is_active = ti == active_ti;
                    let circle = if is_active { "●" } else { "○" };
                    let pill = if compact_dots {
                        circle.to_string()
                    } else {
                        format!("{circle} ")
                    };
                    let pw = pill.width() as u16;
                    if dx + pw > dots_limit_x {
                        break;
                    }
                    let st = world.tab_state(tab);
                    let fg_col = if is_active {
                        Color::Rgb(125, 207, 255)
                    } else if matches!(st, AgentState::Blocked | AgentState::Done) {
                        state_color(st)
                    } else {
                        Color::Rgb(105, 115, 152)
                    };
                    let dstyle = Style::default()
                        .bg(Color::Rgb(24, 27, 42))
                        .fg(fg_col)
                        .add_modifier(if is_active {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        });
                    buf.set_string(dx, rect.y + 1, &pill, dstyle);
                    hits.push(HitRegion {
                        rect: Rect {
                            x: dx,
                            y: rect.y,
                            width: pw,
                            height: rect.height,
                        },
                        target: if is_active && total_tabs > 1 {
                            HitTarget::TriggerAction(Action::NextTab)
                        } else {
                            HitTarget::SelectTab {
                                space_id: space_id.clone(),
                                tab_id: tab.id.clone(),
                            }
                        },
                    });
                    dx += pw;
                }
                x = dots_limit_x;
            } else {
                // 1-row fallback (`--no-touch`): compact `●`/`○` circles
                let compact_dots = total_tabs > 3 || (dots_budget as usize) < total_tabs * 3;
                let dot_step: usize = if compact_dots { 2 } else { 3 };
                let max_visible_dots = (dots_budget as usize / dot_step).max(1);
                let start_ti = if total_tabs <= max_visible_dots || active_ti < max_visible_dots {
                    0
                } else {
                    (active_ti + 1 - max_visible_dots)
                        .min(total_tabs.saturating_sub(max_visible_dots))
                };

                for (ti, tab) in space
                    .tabs
                    .iter()
                    .enumerate()
                    .skip(start_ti)
                    .take(max_visible_dots)
                {
                    let is_active = ti == active_ti;
                    let circle = if is_active { "●" } else { "○" };
                    let pill = if compact_dots {
                        format!("{circle} ")
                    } else {
                        format!(" {circle} ")
                    };
                    let pw = pill.width() as u16;
                    if x + pw > dots_limit_x {
                        break;
                    }
                    let pstyle = Style::default()
                        .bg(if is_active {
                            Color::Rgb(51, 59, 94)
                        } else {
                            Color::Rgb(24, 27, 42)
                        })
                        .fg(if is_active {
                            Color::Rgb(125, 207, 255)
                        } else {
                            Color::Rgb(105, 115, 152)
                        })
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
        }
    }
    if dots_budget >= 2 && x < dots_limit_x {
        x = dots_limit_x;
    }
    if x > dots_start_x && x < avail_end {
        x += 1;
    }

    // 3. Modern Keycap Pills (`^B`, `Esc`, `Ctrl`, `Alt`, `Tab`, `←`, `↓`, `↑`, `→`, `⎘`)
    // Clean rounded/beveled keycap look without ASCII `[]` brackets or `│` pipes.
    // On narrow mobile viewports (`rect.width < 76`), reflow across the 2 touchbar rows (or compact
    // 1-row pills when `--no-touch`) so all 10 keys remain reachable even on 40-column screens.
    let keycap_style = |active_highlight: bool, kind: u8| -> Style {
        if active_highlight {
            Style::default()
                .bg(Color::Rgb(224, 175, 104))
                .fg(Color::Rgb(20, 22, 32))
                .add_modifier(Modifier::BOLD)
        } else {
            match kind {
                1 => Style::default()
                    .bg(Color::Rgb(38, 42, 64))
                    .fg(Color::Rgb(187, 154, 247))
                    .add_modifier(Modifier::BOLD),
                2 => Style::default()
                    .bg(Color::Rgb(29, 33, 50))
                    .fg(Color::Rgb(192, 202, 245))
                    .add_modifier(Modifier::BOLD),
                3 => Style::default()
                    .bg(Color::Rgb(28, 44, 54))
                    .fg(Color::Rgb(115, 218, 202))
                    .add_modifier(Modifier::BOLD),
                _ => Style::default()
                    .bg(Color::Rgb(34, 38, 57))
                    .fg(Color::Rgb(192, 202, 245))
                    .add_modifier(Modifier::BOLD),
            }
        }
    };

    if narrow_bar && rect.height >= 2 {
        let top_keys: [(&str, HitTarget, bool, u8); 5] = [
            ("^B", HitTarget::TogglePrefix, prefix_active, 1),
            (
                "Esc",
                HitTarget::SendKey(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                false,
                0,
            ),
            ("Ctl", HitTarget::ToggleCtrl, ctrl_active, 1),
            ("Alt", HitTarget::ToggleAlt, alt_active, 1),
            (
                "Tab",
                HitTarget::SendKey(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
                false,
                0,
            ),
        ];
        let bot_keys: [(&str, HitTarget, bool, u8); 5] = [
            (
                " ← ",
                HitTarget::SendKey(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
                false,
                2,
            ),
            (
                " ↓ ",
                HitTarget::SendKey(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
                false,
                2,
            ),
            (
                " ↑ ",
                HitTarget::SendKey(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
                false,
                2,
            ),
            (
                " → ",
                HitTarget::SendKey(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
                false,
                2,
            ),
            (" ⎘ ", HitTarget::PasteClipboard, false, 3),
        ];
        for (row_off, row_keys) in [(0u16, top_keys), (1u16, bot_keys)] {
            let mut kx = x;
            for (lbl, target, active_highlight, kind) in row_keys {
                let kw = lbl.width() as u16;
                if kx + kw > avail_end {
                    break;
                }
                let kstyle = keycap_style(active_highlight, kind);
                buf.set_string(kx, rect.y + row_off, lbl, kstyle);
                hits.push(HitRegion {
                    rect: Rect {
                        x: kx,
                        y: rect.y + row_off,
                        width: kw,
                        height: 1,
                    },
                    target,
                });
                kx += kw;
                if kx < avail_end {
                    kx += 1;
                }
            }
        }
    } else {
        let wide_keys = rect.width >= 108;
        let key_buttons: [(&str, HitTarget, bool, u8); 10] = [
            (
                if narrow_bar {
                    "^B"
                } else if wide_keys {
                    "  ^B  "
                } else {
                    " ^B "
                },
                HitTarget::TogglePrefix,
                prefix_active,
                1,
            ),
            (
                if narrow_bar { "Es" } else { " Esc " },
                HitTarget::SendKey(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                false,
                0,
            ),
            (
                if narrow_bar { "Ct" } else { " Ctrl " },
                HitTarget::ToggleCtrl,
                ctrl_active,
                1,
            ),
            (
                if narrow_bar { "Al" } else { " Alt " },
                HitTarget::ToggleAlt,
                alt_active,
                1,
            ),
            (
                if narrow_bar { "Tb" } else { " Tab " },
                HitTarget::SendKey(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
                false,
                0,
            ),
            (
                if narrow_bar {
                    "←"
                } else if wide_keys {
                    "  ←  "
                } else {
                    " ← "
                },
                HitTarget::SendKey(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
                false,
                2,
            ),
            (
                if narrow_bar {
                    "↓"
                } else if wide_keys {
                    "  ↓  "
                } else {
                    " ↓ "
                },
                HitTarget::SendKey(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
                false,
                2,
            ),
            (
                if narrow_bar {
                    "↑"
                } else if wide_keys {
                    "  ↑  "
                } else {
                    " ↑ "
                },
                HitTarget::SendKey(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
                false,
                2,
            ),
            (
                if narrow_bar {
                    "→"
                } else if wide_keys {
                    "  →  "
                } else {
                    " → "
                },
                HitTarget::SendKey(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
                false,
                2,
            ),
            (
                if narrow_bar {
                    "⎘"
                } else if wide_keys {
                    "  ⎘  "
                } else {
                    " ⎘ "
                },
                HitTarget::PasteClipboard,
                false,
                3,
            ),
        ];

        for (i, (lbl, target, active_highlight, kind)) in key_buttons.into_iter().enumerate() {
            let kw = lbl.width() as u16;
            if x + kw > avail_end {
                break;
            }
            let kstyle = if narrow_bar && !active_highlight && i % 2 == 1 {
                keycap_style(false, 2)
            } else {
                keycap_style(active_highlight, kind)
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

            if !narrow_bar && x < avail_end {
                x += 1;
            }
        }
    }

    // 4. Right: optional `✓ Copied`, `▼ BOT`, and `?` keycaps
    let mut rx = avail_end;
    if let Some(clbl) = copied_lbl {
        let cw = clbl.width() as u16;
        if rx + cw <= max_x {
            let cstyle = Style::default()
                .bg(Color::Rgb(158, 206, 106))
                .fg(Color::Rgb(20, 22, 32))
                .add_modifier(Modifier::BOLD);
            draw_touchbar_pill(buf, rx, rect, clbl, cstyle, true);
            rx = (rx + cw + 1).min(max_x);
        }
    }
    if let Some(slbl) = scroll_lbl {
        let sw = slbl.width() as u16;
        if rx + sw <= max_x {
            let sstyle = Style::default()
                .bg(Color::Rgb(125, 207, 255))
                .fg(Color::Rgb(20, 22, 32))
                .add_modifier(Modifier::BOLD);
            draw_touchbar_pill(buf, rx, rect, slbl, sstyle, true);
            hits.push(HitRegion {
                rect: Rect {
                    x: rx,
                    y: rect.y,
                    width: sw,
                    height: rect.height,
                },
                target: HitTarget::ExitScrollback,
            });
            rx = (rx + sw + 1).min(max_x);
        }
    }

    if rx + help_w <= max_x {
        let hstyle = Style::default()
            .bg(Color::Rgb(34, 40, 62))
            .fg(Color::Rgb(125, 207, 255))
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
        let hdr = pad_to_width(" ❖ WORKSPACES", w);
        buf.set_string(
            inner.x,
            y,
            &hdr,
            Style::default()
                .bg(Color::Rgb(22, 24, 37))
                .fg(Color::Rgb(122, 162, 247))
                .add_modifier(Modifier::BOLD),
        );
        y += 1;
    }

    // Reserve footer space for 2-row visually centered (`▄` + text + `▀`) `[+ New Tab]` and `[+ New Space]`
    // buttons with tight margin between them and above the bottom touchbar.
    let footer_rows = if max_y.saturating_sub(y) >= 9 {
        6
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

        if card.is_active {
            let accent = if card.is_space {
                Color::Rgb(125, 207, 255)
            } else {
                Color::Rgb(122, 162, 247)
            };
            buf.set_string(inner.x, y, "▍", r1_style.fg(accent));
            buf.set_string(inner.x, y + 1, "▍", r2_style.fg(accent));
        }

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

    // Render 2-row tall, vertically centered Footer Buttons (`+ New Tab` and `+ New Space`)
    // using half-block top (`▄`) and bottom (`▀`) caps so the text row sits dead-center vertically.
    let pad_x: u16 = if inner.width > 6 { 1 } else { 0 };
    let btn_x = inner.x + pad_x;
    let btn_w = inner.width.saturating_sub(pad_x * 2);
    let btn_w_usize = btn_w as usize;
    let sb_bg = Color::Rgb(18, 20, 30);

    if footer_rows == 6 && btn_w > 0 {
        let half_top = "▄".repeat(btn_w_usize);
        let half_bot = "▀".repeat(btn_w_usize);

        let tab_y = max_y - 6;
        let tab_bg = Color::Rgb(28, 48, 42);
        let tab_fg = Color::Rgb(158, 206, 106);
        buf.set_string(
            btn_x,
            tab_y,
            &half_top,
            Style::default().fg(tab_bg).bg(sb_bg),
        );
        buf.set_string(
            btn_x,
            tab_y + 1,
            &pad_to_width(" ▎ + New Tab    ^b c ", btn_w_usize),
            Style::default()
                .bg(tab_bg)
                .fg(tab_fg)
                .add_modifier(Modifier::BOLD),
        );
        buf.set_string(
            btn_x,
            tab_y + 2,
            &half_bot,
            Style::default().fg(tab_bg).bg(sb_bg),
        );
        hits.push(HitRegion {
            rect: Rect {
                x: btn_x,
                y: tab_y,
                width: btn_w,
                height: 3,
            },
            target: HitTarget::NewTab,
        });

        let sp_y = max_y - 3;
        let sp_bg = Color::Rgb(26, 38, 56);
        let sp_fg = Color::Rgb(125, 207, 255);
        buf.set_string(
            btn_x,
            sp_y,
            &half_top,
            Style::default().fg(sp_bg).bg(sb_bg),
        );
        buf.set_string(
            btn_x,
            sp_y + 1,
            &pad_to_width(" ▎ + New Space  ^b C ", btn_w_usize),
            Style::default()
                .bg(sp_bg)
                .fg(sp_fg)
                .add_modifier(Modifier::BOLD),
        );
        buf.set_string(
            btn_x,
            sp_y + 2,
            &half_bot,
            Style::default().fg(sp_bg).bg(sb_bg),
        );
        hits.push(HitRegion {
            rect: Rect {
                x: btn_x,
                y: sp_y,
                width: btn_w,
                height: 3,
            },
            target: HitTarget::NewSpacePrompt,
        });
    } else if footer_rows == 2 && btn_w > 0 {
        let tab_y = max_y - 2;
        buf.set_string(
            btn_x,
            tab_y,
            &pad_to_width(" ▎ + New Tab    ^b c ", btn_w_usize),
            Style::default()
                .bg(Color::Rgb(28, 48, 42))
                .fg(Color::Rgb(158, 206, 106))
                .add_modifier(Modifier::BOLD),
        );
        hits.push(HitRegion {
            rect: Rect {
                x: btn_x,
                y: tab_y,
                width: btn_w,
                height: 1,
            },
            target: HitTarget::NewTab,
        });
        buf.set_string(
            btn_x,
            tab_y + 1,
            &pad_to_width(" ▎ + New Space  ^b C ", btn_w_usize),
            Style::default()
                .bg(Color::Rgb(26, 38, 56))
                .fg(Color::Rgb(125, 207, 255))
                .add_modifier(Modifier::BOLD),
        );
        hits.push(HitRegion {
            rect: Rect {
                x: btn_x,
                y: tab_y + 1,
                width: btn_w,
                height: 1,
            },
            target: HitTarget::NewSpacePrompt,
        });
    }
}

struct VtScreenWidget<'a> {
    screen: &'a vt100::Screen,
    selection: Option<&'a TerminalSelection>,
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
                let selected = self
                    .selection
                    .is_some_and(|sel| sel.contains(r, c) || (cell.is_wide() && sel.contains(r, c + 1)));

                let mut style = if selected {
                    Style::default()
                        .bg(Color::Rgb(58, 114, 200))
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    let mut st = Style::default()
                        .fg(map_vt_color(cell.fgcolor()))
                        .bg(map_vt_color(cell.bgcolor()));

                    if cell.bold() {
                        st = st.add_modifier(Modifier::BOLD);
                    }
                    if cell.italic() {
                        st = st.add_modifier(Modifier::ITALIC);
                    }
                    if cell.underline() {
                        st = st.add_modifier(Modifier::UNDERLINED);
                    }
                    if cell.inverse() {
                        st = st.add_modifier(Modifier::REVERSED);
                    }
                    st
                };
                let _ = &mut style;

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
    let close_lbl = " ✕ ";
    let cw = close_lbl.width() as u16;
    if popup.width > cw + 4 {
        let cx = popup.x + popup.width - cw - 2;
        let cy = popup.y;
        buf.set_string(
            cx,
            cy,
            close_lbl,
            Style::default()
                .bg(Color::Rgb(96, 38, 52))
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
    let popup = centered_rect(68, 28, area);
    f.render_widget(Clear, popup);
    hits.push(HitRegion {
        rect: popup,
        target: HitTarget::ModalNoop,
    });

    let block = Block::default()
        .title(" Quick Actions & Help ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(122, 162, 247)))
        .style(Style::default().bg(Color::Rgb(20, 22, 34)).fg(Color::White));
    let inner = block.inner(popup);

    // Reflow the 12 touch quick-action buttons across available inner width so narrow mobile
    // viewports wrap buttons onto additional rows instead of dropping right-hand columns.
    let compact_btns = inner.width < 58;
    let quick_actions: [(&str, &str, HitTarget); 12] = [
        (
            "  + New Tab     ",
            " +Tab ",
            HitTarget::TriggerAction(Action::NewTab),
        ),
        (
            "  ✕ Close Tab   ",
            " ✕Tab ",
            HitTarget::TriggerAction(Action::CloseTab),
        ),
        (
            "  ✎ Rename Tab  ",
            " ✎Tab ",
            HitTarget::TriggerAction(Action::RenameTab),
        ),
        (
            "  🔍 Fuzzy Find  ",
            " 🔍Find ",
            HitTarget::TriggerAction(Action::FuzzyFinder),
        ),
        (
            "  ⚡ Next Alert  ",
            " ⚡Alert ",
            HitTarget::TriggerAction(Action::JumpAlertAgent),
        ),
        (
            "  ⇕ Scrollback  ",
            " ⇕Scrl ",
            HitTarget::TriggerAction(Action::EnterScrollback),
        ),
        (
            "  + New Space   ",
            " +Spc ",
            HitTarget::TriggerAction(Action::NewSpace),
        ),
        (
            "  ✎ Ren Space   ",
            " ✎Spc ",
            HitTarget::TriggerAction(Action::RenameSpace),
        ),
        (
            "  ⇄ Next Space  ",
            " ⇄Spc ",
            HitTarget::TriggerAction(Action::NextSpace),
        ),
        (
            "  ◧ Sidebar     ",
            " ◧Bar ",
            HitTarget::TriggerAction(Action::ToggleSidebar),
        ),
        (
            "  ⏏ Detach hct  ",
            " ⏏Exit ",
            HitTarget::TriggerAction(Action::Detach),
        ),
        (
            "  ✕ Close Help  ",
            " ✕Help ",
            HitTarget::CancelModal,
        ),
    ];

    let pad_x: u16 = if compact_btns { 1 } else { 2 };
    let gap_x: u16 = if compact_btns { 1 } else { 2 };
    let mut placed_btns: Vec<(u16, u16, &str, HitTarget)> = Vec::with_capacity(quick_actions.len());
    let mut cur_row: u16 = 0;
    let mut cur_x: u16 = pad_x;
    for (wide_lbl, short_lbl, target) in quick_actions {
        let lbl = if compact_btns { short_lbl } else { wide_lbl };
        let bw = lbl.width() as u16;
        if cur_x > pad_x && cur_x + bw > inner.width {
            cur_row += 1;
            cur_x = pad_x;
        }
        placed_btns.push((cur_x, cur_row, lbl, target));
        cur_x += bw + gap_x;
    }
    let btn_rows_count = (cur_row + 1) as usize;

    let mut lines = vec![Line::from(Span::styled(
        "  Touch Quick Actions (Tap to execute):",
        Style::default()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD),
    ))];
    for _ in 0..=btn_rows_count {
        lines.push(Line::from(""));
    }
    lines.extend([
        Line::from(Span::styled(
            "  Keyboard & Touch Shortcuts:",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from("  Tap ● ○ ✓ on Touchbar    Switch directly to Tab (no body swipe conflict)"),
        Line::from("  Tap ▾ / ▸ on Space       Collapse / expand Space's child Tabs in sidebar"),
        Line::from("  Prefix / Modifiers       Tap [^B], [Ctrl], or [Alt] on bottom touchbar"),
        Line::from("  Clipboard Copy / Paste   Drag text to copy (OSC 52); Tap ⎘ to paste"),
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
    ]);
    f.render_widget(Paragraph::new(lines).block(block), popup);

    let buf = f.buffer_mut();
    draw_modal_close_button(buf, popup, hits);

    for (rel_x, rel_row, lbl, target) in placed_btns {
        let by = inner.y + 1 + rel_row;
        if by >= inner.y + inner.height {
            break;
        }
        let bx = inner.x + rel_x;
        let bw = lbl.width() as u16;
        if bx + bw > inner.x + inner.width {
            continue;
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
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::LightRed))
        .style(Style::default().bg(Color::Rgb(28, 20, 28)).fg(Color::White));
    let inner = block.inner(popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);

    let buf = f.buffer_mut();
    draw_modal_close_button(buf, popup, hits);

    if inner.height >= 4 {
        let by = inner.y + 3;
        let yes_lbl = "  ✓ Confirm Close (Y)  ";
        let no_lbl = "  ✕ Cancel (N)  ";
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
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(122, 162, 247)))
        .style(Style::default().bg(Color::Rgb(20, 22, 34)).fg(Color::White));
    let inner = block.inner(popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);

    let buf = f.buffer_mut();
    draw_modal_close_button(buf, popup, hits);

    if inner.height >= 4 {
        let by = inner.y + 3;
        let ok_lbl = "  ✓ Confirm (Enter)  ";
        let cancel_lbl = "  ✕ Cancel (Esc)  ";
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
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(122, 162, 247)))
        .style(Style::default().bg(Color::Rgb(20, 22, 34)).fg(Color::White));
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
                    false,
                    false,
                    42, // scrollback_offset > 0
                    None,
                    false,
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

        // Scrollback badge, Prefix/Ctrl/Alt buttons, terminal keys (Esc/Tab/Arrows/粘貼), and Help button are on the bottom touchbar
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::ExitScrollback && h.rect.height == 2));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::TogglePrefix && h.rect.height == 2));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::ToggleCtrl && h.rect.height == 2));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::ToggleAlt && h.rect.height == 2));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::PasteClipboard && h.rect.height == 2));
        assert!(!hits.iter().any(|h| {
            h.target == HitTarget::SendKey(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        }));
        assert!(hits.iter().any(|h| {
            h.target == HitTarget::SendKey(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        }));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::ToggleHelp && h.rect.height == 2));

        // Sidebar footer has vertically centered [+ New Tab] and [+ New Space] buttons
        let new_tab_hit = hits
            .iter()
            .find(|h| h.target == HitTarget::NewTab && h.rect.height >= 2)
            .expect("expected multi-row NewTab hit in sidebar");
        let new_space_hit = hits
            .iter()
            .find(|h| h.target == HitTarget::NewSpacePrompt && h.rect.height >= 2)
            .expect("expected multi-row NewSpacePrompt hit in sidebar");
        assert!(new_tab_hit.rect.x >= 1);
        assert_eq!(
            new_space_hit.rect.y,
            new_tab_hit.rect.y + new_tab_hit.rect.height
        );
        assert!(22 >= new_space_hit.rect.y + new_space_hit.rect.height);
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
                let geom = render_ui(
                    f,
                    &world,
                    None,
                    true,
                    24,
                    true,
                    false,
                    false,
                    false,
                    0,
                    None,
                    false,
                    &Modal::None,
                );
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
                let geom = render_ui(
                    f,
                    &world,
                    None,
                    false,
                    24,
                    true,
                    true,
                    true,
                    true,
                    15,
                    None,
                    true,
                    &Modal::None,
                );
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
                    true,
                    false,
                    false,
                    false,
                    0,
                    None,
                    false,
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
                    false,
                    false,
                    0,
                    None,
                    false,
                    &Modal::None,
                );
                hits = geom.hits;
            })
            .unwrap();

        assert_eq!(
            find_hit_target(&hits, 1, 11),
            Some(HitTarget::ToggleSidebar)
        );
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::PasteClipboard));
    }

    #[test]
    fn narrow_terminal_reflows_all_help_quick_actions() {
        let backend = TestBackend::new(40, 24);
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
                    false,
                    false,
                    0,
                    None,
                    false,
                    &Modal::Help,
                );
                hits = geom.hits;
            })
            .unwrap();

        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::TriggerAction(Action::CloseTab)));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::TriggerAction(Action::RenameTab)));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::TriggerAction(Action::RenameSpace)));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::TriggerAction(Action::Detach)));
    }

    #[test]
    fn terminal_selection_extracts_single_and_multiline_text() {
        let mut parser = vt100::Parser::new(10, 12, 100);
        parser.process(b"hello world\r\nsecond line");
        let sel = TerminalSelection {
            anchor: (0, 6),
            head: (1, 5),
            active: false,
        };
        assert_eq!(sel.extract_text(parser.screen()), "world\nsecond");

        // Soft-wrapped lines (no `\r\n`) should be joined without inserting a spurious newline
        let mut wrap_parser = vt100::Parser::new(10, 10, 100);
        wrap_parser.process(b"abcdefghijklmno");
        let wrap_sel = TerminalSelection {
            anchor: (0, 0),
            head: (1, 4),
            active: false,
        };
        assert_eq!(
            wrap_sel.extract_text(wrap_parser.screen()),
            "abcdefghijklmno"
        );
    }
}
