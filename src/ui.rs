//! TUI rendering (`ratatui` + `vt100`) with Top `Space / Tab1 / Tab2 / [+]` Bar,
//! Secondary Space Switcher Popover, Dual Floating Draggable `+` Cross Pads,
//! Read-Only Bottom Agent Status Bar, and modal overlays.

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

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HitTarget {
    ToggleSpaceMenu,
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
    SpaceMenu,
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

pub const DPAD_DISC_WIDTH: u16 = 15;
pub const DPAD_DISC_HEIGHT: u16 = 7;

fn dpad_cross_metrics(r: Rect) -> (u16, u16, u16) {
    let arm_w = (r.width / 3).max(1);
    let (arm_h, mid_h) = if r.height >= 7 {
        (2, 3)
    } else if r.height >= 5 {
        (1, 3)
    } else {
        (1, 1)
    };
    (arm_w, arm_h, mid_h)
}

#[allow(dead_code)]
pub struct UiGeometry {
    pub term_rect: Rect,
    pub hits: Vec<HitRegion>,
    pub mod_rect: Option<Rect>,
    pub dpad_rect: Option<Rect>,
}

fn cross_pad_dimensions(term_rect: Rect) -> Option<(u16, u16)> {
    if term_rect.width < 6 || term_rect.height < 3 {
        return None;
    }
    let pad_w = if term_rect.width >= DPAD_DISC_WIDTH * 2 {
        DPAD_DISC_WIDTH
    } else if term_rect.width >= 18 {
        9
    } else {
        3
    };
    let pad_h = if term_rect.height >= DPAD_DISC_HEIGHT {
        DPAD_DISC_HEIGHT
    } else if term_rect.height >= 5 {
        5
    } else {
        3
    };
    Some((pad_w, pad_h))
}

pub fn resolve_mod_cross_rect(term_rect: Rect, custom_pos: Option<(u16, u16)>) -> Option<Rect> {
    let (pad_w, pad_h) = cross_pad_dimensions(term_rect)?;
    let max_x = term_rect.x + term_rect.width.saturating_sub(pad_w);
    let max_y = term_rect.y + term_rect.height.saturating_sub(pad_h);
    let default_x = if term_rect.width > pad_w * 2 + 1 {
        term_rect.x + 1
    } else {
        term_rect.x
    };
    let default_y = max_y.saturating_sub(1).max(term_rect.y);
    let (px, py) = custom_pos.unwrap_or((default_x, default_y));
    Some(Rect {
        x: px.clamp(term_rect.x, max_x),
        y: py.clamp(term_rect.y, max_y),
        width: pad_w,
        height: pad_h,
    })
}

pub fn resolve_dpad_disc_rect(term_rect: Rect, custom_pos: Option<(u16, u16)>) -> Option<Rect> {
    let (pad_w, pad_h) = cross_pad_dimensions(term_rect)?;
    let max_x = term_rect.x + term_rect.width.saturating_sub(pad_w);
    let max_y = term_rect.y + term_rect.height.saturating_sub(pad_h);
    let default_x = if term_rect.width > pad_w * 2 + 1 {
        max_x.saturating_sub(1).max(term_rect.x)
    } else {
        max_x
    };
    let default_y = max_y.saturating_sub(1).max(term_rect.y);
    let (px, py) = custom_pos.unwrap_or((default_x, default_y));
    Some(Rect {
        x: px.clamp(term_rect.x, max_x),
        y: py.clamp(term_rect.y, max_y),
        width: pad_w,
        height: pad_h,
    })
}

pub fn is_on_dpad_disc(disc_rect: Option<Rect>, col: u16, row: u16) -> bool {
    let Some(r) = disc_rect else {
        return false;
    };
    if col < r.x || col >= r.x + r.width || row < r.y || row >= r.y + r.height {
        return false;
    }
    let dx = col - r.x;
    let dy = row - r.y;
    let (arm_w, arm_h, mid_h) = dpad_cross_metrics(r);
    (arm_w..arm_w * 2).contains(&dx) || (arm_h..arm_h + mid_h).contains(&dy)
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

pub fn top_bar_height(area: Rect, _touch_mode: bool) -> u16 {
    if area.height <= 1 {
        0
    } else if area.height >= 10 {
        3
    } else if area.height >= 6 {
        2
    } else {
        1
    }
}

pub fn bottom_bar_height(area: Rect, _touch_mode: bool) -> u16 {
    if area.height >= 3 {
        1
    } else {
        0
    }
}

pub fn compute_terminal_rect(
    area: Rect,
    _sidebar_open: bool,
    _sidebar_width: u16,
    touch_mode: bool,
) -> Rect {
    let top_h = top_bar_height(area, touch_mode);
    let bot_h = bottom_bar_height(area, touch_mode);
    let body_height = area.height.saturating_sub(top_h + bot_h).max(1);
    Rect {
        x: area.x,
        y: area.y + top_h,
        width: area.width.max(1),
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

#[cfg(test)]
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
    render_ui_with_pads(
        f,
        world,
        screen,
        sidebar_open,
        sidebar_width,
        touch_mode,
        prefix_active,
        ctrl_active,
        alt_active,
        scrollback_offset,
        selection,
        copied_flash,
        modal,
        None,
        false,
        None,
        false,
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn render_ui_with_dpad(
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
    dpad_pos: Option<(u16, u16)>,
    dpad_dragging: bool,
) -> UiGeometry {
    render_ui_with_pads(
        f,
        world,
        screen,
        sidebar_open,
        sidebar_width,
        touch_mode,
        prefix_active,
        ctrl_active,
        alt_active,
        scrollback_offset,
        selection,
        copied_flash,
        modal,
        None,
        false,
        dpad_pos,
        dpad_dragging,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn render_ui_with_pads(
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
    mod_pos: Option<(u16, u16)>,
    mod_dragging: bool,
    dpad_pos: Option<(u16, u16)>,
    dpad_dragging: bool,
) -> UiGeometry {
    let area = f.area();
    let mut hits = Vec::new();
    let top_h = top_bar_height(area, touch_mode);
    let bot_h = bottom_bar_height(area, touch_mode);
    let term_rect = compute_terminal_rect(area, sidebar_open, sidebar_width, touch_mode);

    let top_rect = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: top_h,
    };
    let bottom_rect = Rect {
        x: area.x,
        y: term_rect.y + term_rect.height,
        width: area.width,
        height: bot_h,
    };

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

    let mut mod_rect = None;
    let mut dpad_rect = None;
    let space_menu_open = matches!(modal, Modal::SpaceMenu);

    render_top_tabbar(f, top_rect, world, space_menu_open, &mut hits);
    render_bottom_status_bar(
        f,
        bottom_rect,
        world,
        scrollback_offset,
        copied_flash,
        &mut hits,
    );

    match modal {
        Modal::None => {
            if touch_mode {
                mod_rect = render_floating_mod_cross(
                    f,
                    term_rect,
                    mod_pos,
                    mod_dragging,
                    prefix_active,
                    ctrl_active,
                    alt_active,
                    &mut hits,
                );
                dpad_rect = render_floating_dpad(f, term_rect, dpad_pos, dpad_dragging, &mut hits);
            }
        }
        Modal::SpaceMenu => {
            render_space_menu_modal(f, area, top_h, world, &mut hits);
        }
        Modal::Help => {
            render_help_modal(f, area, &mut hits);
        }
        Modal::ConfirmCloseTab {
            tab_id,
            tab_title,
        } => {
            render_confirm_close_modal(f, area, tab_id, tab_title, &mut hits);
        }
        Modal::Prompt { kind, input } => {
            render_prompt_modal(f, area, kind, input, &mut hits);
        }
        Modal::Fuzzy {
            query,
            selected,
            selected_tab_id,
        } => {
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

    UiGeometry {
        term_rect,
        hits,
        mod_rect,
        dpad_rect,
    }
}

fn draw_touchbar_pill(
    buf: &mut Buffer,
    x: u16,
    rect: Rect,
    label: &str,
    style: Style,
    emphasized: bool,
) {
    if rect.height == 0 || label.is_empty() {
        return;
    }
    let lw = label.width() as u16;
    if rect.height >= 3 && lw > 0 {
        // 3-row keycap pill with zero black border above/below:
        // - Row 0 (`rect.y`): continuous highlighted top divider line (`▔`) on `style` (`keycap_bg`)
        // - Row 1 (`rect.y + 1`): dead-center button label on `style` (`keycap_bg`)
        // - Row 2 (`rect.y + 2`): subtle bottom bevel (`▁`) on `style` (`keycap_bg`)
        let top_fg = if emphasized {
            Color::Rgb(245, 194, 118)
        } else {
            Color::Rgb(118, 162, 247)
        };
        let bot_fg = if emphasized {
            Color::Rgb(210, 155, 80)
        } else {
            Color::Rgb(54, 62, 92)
        };
        let top_line = "▔".repeat(lw as usize);
        let bot_line = "▁".repeat(lw as usize);
        buf.set_string(x, rect.y, &top_line, style.fg(top_fg));
        buf.set_string(x, rect.y + 1, label, style);
        buf.set_string(x, rect.y + 2, &bot_line, style.fg(bot_fg));
    } else if rect.height == 2 && lw > 0 {
        let top_fg = if emphasized {
            Color::Rgb(245, 194, 118)
        } else {
            Color::Rgb(118, 162, 247)
        };
        let top_line = "▔".repeat(lw as usize);
        buf.set_string(x, rect.y, &top_line, style.fg(top_fg));
        buf.set_string(x, rect.y + 1, label, style);
    } else {
        buf.set_string(x, rect.y, label, style);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_top_tabbar(
    f: &mut Frame,
    rect: Rect,
    world: &World,
    space_menu_open: bool,
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

    if rect.height >= 2 {
        let full_line = "▔".repeat(rect.width as usize);
        buf.set_string(
            rect.x,
            rect.y,
            &full_line,
            Style::default()
                .bg(Color::Rgb(12, 14, 22))
                .fg(Color::Rgb(82, 114, 184)),
        );
    }

    let help_lbl = if rect.width < 48 { " ? " } else { "  ?  " };
    let help_w = help_lbl.width() as u16;
    let plus_lbl = if rect.width < 48 { " + " } else { "  +  " };
    let plus_w = plus_lbl.width() as u16;
    let avail_end = max_x.saturating_sub(help_w.min(rect.width.saturating_sub(4)));

    // 1. Far Left: Space selector button (`[ ▾ SpaceName ]`) — opens secondary Space popover menu
    let (sname, sstate) = world
        .active_space()
        .map(|s| (world.space_display_name(s), world.space_state(s)))
        .unwrap_or_else(|| ("space".to_string(), AgentState::None));
    let arrow = if space_menu_open { "▴" } else { "▾" };
    let max_space_w = if rect.width < 36 {
        10usize
    } else if rect.width < 60 {
        14usize
    } else {
        20usize
    };
    let raw_space_lbl = if rect.width < 36 {
        format!(" {arrow} {} ", truncate_to_width(&sname, 5))
    } else {
        format!(
            " {arrow} {} {} ",
            sstate.symbol(),
            truncate_to_width(&sname, max_space_w.saturating_sub(6))
        )
    };
    let space_budget = avail_end.saturating_sub(x).saturating_sub(plus_w + 2);
    let space_lbl = if (raw_space_lbl.width() as u16) <= space_budget {
        raw_space_lbl
    } else {
        truncate_to_width(&raw_space_lbl, space_budget.max(4) as usize)
    };
    let space_w = space_lbl.width() as u16;
    if space_w > 0 && x + space_w <= max_x {
        let style = Style::default()
            .bg(if space_menu_open {
                Color::Rgb(56, 68, 110)
            } else {
                Color::Rgb(32, 38, 60)
            })
            .fg(Color::Rgb(125, 207, 255))
            .add_modifier(Modifier::BOLD);
        draw_touchbar_pill(buf, x, rect, &space_lbl, style, space_menu_open);
        hits.push(HitRegion {
            rect: Rect {
                x,
                y: rect.y,
                width: space_w,
                height: rect.height,
            },
            target: HitTarget::ToggleSpaceMenu,
        });
        x += space_w;
    }

    // Subtle slash divider between Space button and Tab pills
    if x + 1 < avail_end {
        let div_y = if rect.height >= 2 {
            rect.y + 1
        } else {
            rect.y
        };
        buf.set_string(
            x,
            div_y,
            "╱",
            Style::default()
                .bg(Color::Rgb(12, 14, 22))
                .fg(Color::Rgb(72, 82, 118)),
        );
        x += 1;
    }

    // 2. Horizontal Tab pills (`space / tab1 / tab2 ...`) + trailing `[ + ]` button
    let tabs_limit_x = avail_end.saturating_sub(plus_w + 1);
    if let Some(space) = world.active_space() {
        let active_ti = world.active_tab_index();
        let space_id = space.id.clone();
        let total_tabs = space.tabs.len();
        let compact_tabs = rect.width < 64 || total_tabs > 4;
        let max_tab_name_w = if rect.width < 44 {
            4usize
        } else if compact_tabs {
            8usize
        } else {
            14usize
        };

        // Build formatted labels for all tabs so we can window around `active_ti` if needed
        let mut tab_pills: Vec<(usize, String, String, AgentState, bool)> =
            Vec::with_capacity(total_tabs);
        for (ti, tab) in space.tabs.iter().enumerate() {
            let is_active = ti == active_ti;
            let st = world.tab_state(tab);
            let tname = truncate_to_width(&world.tab_display_name(space, ti), max_tab_name_w);
            let icon = if matches!(st, AgentState::Blocked | AgentState::Working | AgentState::Done)
            {
                st.symbol()
            } else if is_active {
                "●"
            } else {
                "○"
            };
            let lbl = if rect.width < 40 {
                format!(" {icon}{} ", ti + 1)
            } else {
                format!(" {icon} {}:{} ", ti + 1, tname)
            };
            tab_pills.push((ti, tab.id.clone(), lbl, st, is_active));
        }

        // Ensure `active_ti` (and `active_ti + 1` whenever both fit) is within the visible slice
        // so selecting the rightmost visible tab reveals the next hidden tab on the right.
        let avail_tabs_w = tabs_limit_x.saturating_sub(x);
        let total_tabs_w: u16 = tab_pills
            .iter()
            .enumerate()
            .map(|(i, (_, _, lbl, _, _))| lbl.width() as u16 + u16::from(i + 1 < total_tabs))
            .sum();
        let tabs_overflow = total_tabs_w > avail_tabs_w;
        let mut start_idx = 0usize;
        if avail_tabs_w > 0 && !tab_pills.is_empty() {
            let lookahead_ti = if active_ti + 1 < total_tabs {
                let pair_w: u16 = tab_pills[active_ti..=active_ti + 1]
                    .iter()
                    .enumerate()
                    .map(|(i, (_, _, lbl, _, _))| lbl.width() as u16 + u16::from(i == 0))
                    .sum();
                if pair_w <= avail_tabs_w {
                    active_ti + 1
                } else {
                    active_ti
                }
            } else {
                active_ti
            };
            while start_idx < active_ti {
                let slice = &tab_pills[start_idx..=lookahead_ti];
                let needed: u16 = slice
                    .iter()
                    .enumerate()
                    .map(|(i, (_, _, lbl, _, _))| {
                        lbl.width() as u16 + u16::from(i + 1 < slice.len())
                    })
                    .sum();
                if needed <= avail_tabs_w {
                    break;
                }
                start_idx += 1;
            }
        }

        for (_ti, tab_id, lbl, st, is_active) in tab_pills.into_iter().skip(start_idx) {
            let pw = lbl.width() as u16;
            if x + pw > tabs_limit_x {
                break;
            }
            let fg_col = if is_active {
                Color::White
            } else if matches!(st, AgentState::Blocked | AgentState::Done) {
                state_color(st)
            } else {
                Color::Rgb(160, 170, 205)
            };
            let bg_col = if is_active {
                Color::Rgb(58, 70, 114)
            } else {
                Color::Rgb(24, 28, 44)
            };
            let tstyle = Style::default().bg(bg_col).fg(fg_col).add_modifier(
                if is_active {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                },
            );
            draw_touchbar_pill(buf, x, rect, &lbl, tstyle, is_active);
            hits.push(HitRegion {
                rect: Rect {
                    x,
                    y: rect.y,
                    width: pw,
                    height: rect.height,
                },
                target: if is_active && tabs_overflow && total_tabs > 1 {
                    HitTarget::TriggerAction(Action::NextTab)
                } else {
                    HitTarget::SelectTab {
                        space_id: space_id.clone(),
                        tab_id,
                    }
                },
            });
            x += pw;
            if x < tabs_limit_x {
                x += 1;
            }
        }
    }

    // 3. `[ + ]` button immediately after the tabs to add a new Tab
    if x + plus_w <= avail_end {
        let pstyle = Style::default()
            .bg(Color::Rgb(28, 52, 44))
            .fg(Color::Rgb(158, 206, 106))
            .add_modifier(Modifier::BOLD);
        draw_touchbar_pill(buf, x, rect, plus_lbl, pstyle, false);
        hits.push(HitRegion {
            rect: Rect {
                x,
                y: rect.y,
                width: plus_w,
                height: rect.height,
            },
            target: HitTarget::NewTab,
        });
    }

    // 4. Far Right: `[ ? ]` Quick Actions & Help button
    let rx = max_x.saturating_sub(help_w);
    if rx >= x && rx + help_w <= max_x {
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

fn render_bottom_status_bar(
    f: &mut Frame,
    rect: Rect,
    world: &World,
    scrollback_offset: usize,
    copied_flash: bool,
    hits: &mut Vec<HitRegion>,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let bar_bg = Color::Rgb(14, 16, 24);
    let base_style = Style::default().bg(bar_bg).fg(Color::Rgb(160, 168, 200));
    f.render_widget(Block::default().style(base_style), rect);

    let buf = f.buffer_mut();
    let max_x = rect.x + rect.width;
    let mut x = rect.x;

    let active_state = world
        .active_space()
        .and_then(|s| s.tabs.get(world.active_tab_index()))
        .map(|t| world.tab_state(t))
        .unwrap_or(AgentState::None);

    let (badge_lbl, badge_bg, badge_fg) = match active_state {
        AgentState::Blocked => (
            " ◉ BLOCKED ",
            Color::Rgb(92, 28, 42),
            Color::Rgb(247, 118, 142),
        ),
        AgentState::Working => (
            " ● WORKING ",
            Color::Rgb(24, 52, 74),
            Color::Rgb(125, 207, 255),
        ),
        AgentState::Done => (
            " ✓ DONE ",
            Color::Rgb(26, 56, 38),
            Color::Rgb(158, 206, 106),
        ),
        AgentState::Idle => (
            " ○ IDLE ",
            Color::Rgb(42, 38, 26),
            Color::Rgb(224, 175, 104),
        ),
        AgentState::None => (
            " ○ SHELL ",
            Color::Rgb(26, 30, 46),
            Color::Rgb(140, 150, 185),
        ),
    };

    let bw = badge_lbl.width() as u16;
    if x + bw <= max_x {
        buf.set_string(
            x,
            rect.y,
            badge_lbl,
            Style::default()
                .bg(badge_bg)
                .fg(badge_fg)
                .add_modifier(Modifier::BOLD),
        );
        x += bw;
    }

    // Reserve right-side space for optional `✓ Copied` and `▼ BOT (+N)` indicators
    let copied_lbl = if copied_flash {
        Some(if rect.width < 60 {
            " ✓ "
        } else {
            " ✓ Copied "
        })
    } else {
        None
    };
    let scroll_lbl = if scrollback_offset > 0 {
        Some(if rect.width < 60 {
            " ▼ BOT ".to_string()
        } else {
            format!(" ▼ BOT (+{scrollback_offset}) ")
        })
    } else {
        None
    };
    let copied_w = copied_lbl.map_or(0, |s| s.width() as u16);
    let scroll_w = scroll_lbl.as_ref().map_or(0, |s| s.width() as u16);
    let right_reserved = copied_w + scroll_w;
    let avail_end = max_x.saturating_sub(right_reserved);

    // Count workspace agent states & turns for the read-only Agent Status Bar
    let mut working_cnt = 0usize;
    let mut blocked_cnt = 0usize;
    let mut done_cnt = 0usize;
    let mut total_turns = 0u64;
    for info in world.terminals.values() {
        total_turns += info.turn;
        match info.agent_state() {
            AgentState::Working => working_cnt += 1,
            AgentState::Blocked => blocked_cnt += 1,
            AgentState::Done => done_cnt += 1,
            _ => {}
        }
    }

    let detail = world
        .active_terminal_address()
        .and_then(|addr| {
            world.terminals.get(&addr).map(|info| {
                let cwd = shorten_home_path(info.effective_cwd());
                let title = if info.title.is_empty() {
                    addr.as_str()
                } else {
                    info.title.as_str()
                };
                if info.turn > 0 {
                    format!(" {title} · {cwd} · turn #{}", info.turn)
                } else {
                    format!(" {title} · {cwd}")
                }
            })
        })
        .unwrap_or_else(|| " shell".to_string());

    let usage_summary = format!(
        " ◉{blocked_cnt} ●{working_cnt} ✓{done_cnt} · turns:{total_turns} "
    );
    let usage_w = usage_summary.width() as u16;
    let detail_end = if avail_end.saturating_sub(x) > usage_w + 12 {
        avail_end.saturating_sub(usage_w)
    } else {
        avail_end
    };

    if x < detail_end {
        let d_trunc = truncate_to_width(&detail, (detail_end - x) as usize);
        buf.set_string(x, rect.y, &d_trunc, base_style);
    }
    if detail_end < avail_end {
        buf.set_string(
            detail_end,
            rect.y,
            &usage_summary,
            Style::default()
                .bg(Color::Rgb(20, 24, 36))
                .fg(Color::Rgb(138, 162, 218)),
        );
    }

    let mut rx = avail_end;
    if let Some(clbl) = copied_lbl {
        let cw = clbl.width() as u16;
        if rx + cw <= max_x {
            buf.set_string(
                rx,
                rect.y,
                clbl,
                Style::default()
                    .bg(Color::Rgb(158, 206, 106))
                    .fg(Color::Rgb(20, 22, 32))
                    .add_modifier(Modifier::BOLD),
            );
            rx += cw;
        }
    }
    if let Some(slbl) = scroll_lbl {
        let sw = slbl.width() as u16;
        if rx + sw <= max_x {
            buf.set_string(
                rx,
                rect.y,
                &slbl,
                Style::default()
                    .bg(Color::Rgb(125, 207, 255))
                    .fg(Color::Rgb(20, 22, 32))
                    .add_modifier(Modifier::BOLD),
            );
            hits.push(HitRegion {
                rect: Rect {
                    x: rx,
                    y: rect.y,
                    width: sw,
                    height: rect.height,
                },
                target: HitTarget::ExitScrollback,
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_floating_mod_cross(
    f: &mut Frame,
    term_rect: Rect,
    mod_pos: Option<(u16, u16)>,
    dragging: bool,
    prefix_active: bool,
    ctrl_active: bool,
    alt_active: bool,
    hits: &mut Vec<HitRegion>,
) -> Option<Rect> {
    let disc = resolve_mod_cross_rect(term_rect, mod_pos)?;
    let buf = f.buffer_mut();

    let base_tint = if dragging {
        (64, 58, 114)
    } else {
        (46, 42, 86)
    };
    let base_alpha = if dragging { 82 } else { 72 };
    let center_tint = if dragging {
        (78, 62, 128)
    } else {
        (58, 46, 98)
    };
    let center_alpha = if dragging { 86 } else { 78 };
    let active_tint = (214, 162, 84);
    let active_alpha = 88;

    let border_fg = if dragging {
        Color::Rgb(187, 154, 247)
    } else {
        Color::Rgb(124, 112, 178)
    };
    let key_fg = Color::Rgb(230, 238, 255);
    let mod_fg = Color::Rgb(210, 190, 255);
    let active_fg = Color::Rgb(255, 248, 220);

    // 15x7 Compact Cross (+) for Modifier Keys with 4 hollow corners:
    // col: 012345678901234
    // r0:       ╭───╮     
    // r1:       │Esc│     
    // r2:  ╭───╮├───┤╭───╮
    // r3:  │Ctl││^B ││Alt│
    // r4:  ╰───╯├───┤╰───╯
    // r5:       │Tab│     
    // r6:       ╰───╯     
    let (arm_w, arm_h, mid_h) = dpad_cross_metrics(disc);
    let mid_col_end = arm_w * 2;
    let mid_row_end = arm_h + mid_h;
    let up_label_y = arm_h.saturating_sub(1);
    let mid_label_y = arm_h + mid_h / 2;
    let down_label_y = mid_row_end;
    let left_label_x = arm_w / 2;
    let center_label_x = arm_w + arm_w / 2;
    let right_label_x = mid_col_end + arm_w / 2;
    let full_15x7 = disc.width == DPAD_DISC_WIDTH && disc.height == DPAD_DISC_HEIGHT;

    for dy in 0..disc.height {
        for dx in 0..disc.width {
            let in_cross = (arm_w..mid_col_end).contains(&dx) || (arm_h..mid_row_end).contains(&dy);
            if !in_cross {
                continue;
            }
            let x = disc.x + dx;
            let y = disc.y + dy;
            if x >= buf.area.width || y >= buf.area.height {
                continue;
            }
            let is_center =
                (arm_w..mid_col_end).contains(&dx) && (arm_h..mid_row_end).contains(&dy);
            let is_left = dx < arm_w && (arm_h..mid_row_end).contains(&dy);
            let is_right = dx >= mid_col_end && (arm_h..mid_row_end).contains(&dy);

            let sector_active = (is_center && prefix_active)
                || (is_left && ctrl_active)
                || (is_right && alt_active);

            let cell = &mut buf[(x, y)];
            let base_bg = color_to_rgb(cell.bg, (18, 20, 30));
            let base_fg = color_to_rgb(cell.fg, (169, 177, 214));
            let (tint, alpha) = if sector_active {
                (active_tint, active_alpha)
            } else if is_center {
                (center_tint, center_alpha)
            } else {
                (base_tint, base_alpha)
            };
            let blended_bg = blend_rgb(base_bg, tint, alpha);

            let ctl_col = if ctrl_active { active_fg } else { mod_fg };
            let pfx_col = if prefix_active { active_fg } else { mod_fg };
            let alt_col = if alt_active { active_fg } else { mod_fg };

            let glyph: Option<(&str, Color, bool)> = if full_15x7 {
                match (dy, dx) {
                    // Row 0: top of Esc box
                    (0, 5) => Some(("╭", border_fg, false)),
                    (0, 6..=8) => Some(("─", border_fg, false)),
                    (0, 9) => Some(("╮", border_fg, false)),
                    // Row 1: Esc label
                    (1, 5) | (1, 9) => Some(("│", border_fg, false)),
                    (1, 6) => Some(("E", key_fg, true)),
                    (1, 7) => Some(("s", key_fg, true)),
                    (1, 8) => Some(("c", key_fg, true)),
                    // Row 2: top of Left / Center / Right
                    (2, 0) | (2, 10) => Some(("╭", border_fg, false)),
                    (2, 4) | (2, 14) => Some(("╮", border_fg, false)),
                    (2, 5) => Some(("├", border_fg, false)),
                    (2, 9) => Some(("┤", border_fg, false)),
                    (2, 1..=3) | (2, 6..=8) | (2, 11..=13) => Some(("─", border_fg, false)),
                    // Row 3: Ctl / ^B / Alt labels
                    (3, 0) | (3, 4) | (3, 5) | (3, 9) | (3, 10) | (3, 14) => {
                        Some(("│", border_fg, false))
                    }
                    (3, 1) => Some(("C", ctl_col, true)),
                    (3, 2) => Some(("t", ctl_col, true)),
                    (3, 3) => Some(("l", ctl_col, true)),
                    (3, 6) => Some(("^", pfx_col, true)),
                    (3, 7) => Some(("B", pfx_col, true)),
                    (3, 8) => Some((" ", pfx_col, false)),
                    (3, 11) => Some(("A", alt_col, true)),
                    (3, 12) => Some(("l", alt_col, true)),
                    (3, 13) => Some(("t", alt_col, true)),
                    // Row 4: bottom of Left / Center / Right
                    (4, 0) | (4, 10) => Some(("╰", border_fg, false)),
                    (4, 4) | (4, 14) => Some(("╯", border_fg, false)),
                    (4, 5) => Some(("├", border_fg, false)),
                    (4, 9) => Some(("┤", border_fg, false)),
                    (4, 1..=3) | (4, 6..=8) | (4, 11..=13) => Some(("─", border_fg, false)),
                    // Row 5: Tab label
                    (5, 5) | (5, 9) => Some(("│", border_fg, false)),
                    (5, 6) => Some(("T", key_fg, true)),
                    (5, 7) => Some(("a", key_fg, true)),
                    (5, 8) => Some(("b", key_fg, true)),
                    // Row 6: bottom of Tab box
                    (6, 5) => Some(("╰", border_fg, false)),
                    (6, 6..=8) => Some(("─", border_fg, false)),
                    (6, 9) => Some(("╯", border_fg, false)),
                    _ => None,
                }
            } else if dy == up_label_y && dx == center_label_x {
                Some(("E", key_fg, true))
            } else if dy == down_label_y && dx == center_label_x {
                Some(("T", key_fg, true))
            } else if dy == mid_label_y && dx == left_label_x {
                Some(("C", ctl_col, true))
            } else if dy == mid_label_y && dx == center_label_x {
                Some(("B", pfx_col, true))
            } else if dy == mid_label_y && dx == right_label_x {
                Some(("A", alt_col, true))
            } else {
                None
            };

            cell.set_bg(blended_bg);
            if let Some((sym, fg, bold)) = glyph {
                cell.set_symbol(sym);
                cell.set_fg(fg);
                if bold {
                    cell.modifier.insert(Modifier::BOLD);
                } else {
                    cell.modifier.remove(Modifier::BOLD);
                }
            } else {
                if cell.symbol().width() != 1 {
                    cell.set_symbol(" ");
                }
                let ghost_fg = blend_rgb(base_fg, tint, 55);
                cell.set_fg(ghost_fg);
                cell.modifier.remove(Modifier::BOLD);
            }
        }
    }

    // Register 5 hit regions covering 100% of the Left Modifier Cross (+):
    // Top arm (Esc)
    hits.push(HitRegion {
        rect: Rect {
            x: disc.x + arm_w,
            y: disc.y,
            width: arm_w,
            height: arm_h,
        },
        target: HitTarget::SendKey(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    });
    // Bottom arm (Tab)
    hits.push(HitRegion {
        rect: Rect {
            x: disc.x + arm_w,
            y: disc.y + mid_row_end,
            width: arm_w,
            height: arm_h,
        },
        target: HitTarget::SendKey(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
    });
    // Left arm (Ctrl)
    hits.push(HitRegion {
        rect: Rect {
            x: disc.x,
            y: disc.y + arm_h,
            width: arm_w,
            height: mid_h,
        },
        target: HitTarget::ToggleCtrl,
    });
    // Right arm (Alt)
    hits.push(HitRegion {
        rect: Rect {
            x: disc.x + mid_col_end,
            y: disc.y + arm_h,
            width: arm_w,
            height: mid_h,
        },
        target: HitTarget::ToggleAlt,
    });
    // Center square (^B Prefix)
    hits.push(HitRegion {
        rect: Rect {
            x: disc.x + arm_w,
            y: disc.y + arm_h,
            width: arm_w,
            height: mid_h,
        },
        target: HitTarget::TogglePrefix,
    });

    Some(disc)
}

fn color_to_rgb(c: Color, default_rgb: (u8, u8, u8)) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (20, 22, 32),
        Color::Red => (247, 118, 142),
        Color::Green => (158, 206, 106),
        Color::Yellow => (224, 175, 104),
        Color::Blue => (122, 162, 247),
        Color::Magenta => (187, 154, 247),
        Color::Cyan => (125, 207, 255),
        Color::Gray => (169, 177, 214),
        Color::DarkGray => (86, 95, 137),
        Color::LightRed => (255, 137, 157),
        Color::LightGreen => (185, 242, 124),
        Color::LightYellow => (255, 208, 130),
        Color::LightBlue => (145, 184, 255),
        Color::LightMagenta => (209, 178, 255),
        Color::LightCyan => (157, 225, 255),
        Color::White => (220, 228, 252),
        _ => default_rgb,
    }
}

fn blend_rgb(base: (u8, u8, u8), overlay: (u8, u8, u8), alpha_pct: u16) -> Color {
    let a = alpha_pct.min(100);
    let inv = 100 - a;
    let r = ((base.0 as u16 * inv + overlay.0 as u16 * a) / 100) as u8;
    let g = ((base.1 as u16 * inv + overlay.1 as u16 * a) / 100) as u8;
    let b = ((base.2 as u16 * inv + overlay.2 as u16 * a) / 100) as u8;
    Color::Rgb(r, g, b)
}

fn render_floating_dpad(
    f: &mut Frame,
    term_rect: Rect,
    dpad_pos: Option<(u16, u16)>,
    dragging: bool,
    hits: &mut Vec<HitRegion>,
) -> Option<Rect> {
    let disc = resolve_dpad_disc_rect(term_rect, dpad_pos)?;
    let buf = f.buffer_mut();

    let arrow_tint = if dragging {
        (58, 76, 124)
    } else {
        (42, 56, 94)
    };
    let arrow_alpha = if dragging { 82 } else { 72 };

    let center_tint = if dragging {
        (48, 106, 122)
    } else {
        (36, 84, 98)
    };
    let center_alpha = if dragging { 86 } else { 78 };

    let border_fg = if dragging {
        Color::Rgb(125, 207, 255)
    } else {
        Color::Rgb(106, 124, 178)
    };
    let arrow_fg = Color::Rgb(230, 238, 255);
    let paste_fg = Color::Rgb(198, 248, 228);

    // 15x7 Compact Cross (+) (or scaled 9/15 x 3/5/7 cross on constrained viewports),
    // with 4 hollow corners (100% transparent, no wasted space):
    // col: 012345678901234
    // r0:       ╭───╮     
    // r1:       │ ↑ │     
    // r2:  ╭───╮├───┤╭───╮
    // r3:  │ ← ││ ⎘ ││ → │
    // r4:  ╰───╯├───┤╰───╯
    // r5:       │ ↓ │     
    // r6:       ╰───╯     
    let (arm_w, arm_h, mid_h) = dpad_cross_metrics(disc);
    let mid_col_end = arm_w * 2;
    let mid_row_end = arm_h + mid_h;
    let up_label_y = arm_h.saturating_sub(1);
    let mid_label_y = arm_h + mid_h / 2;
    let down_label_y = mid_row_end;
    let left_label_x = arm_w / 2;
    let center_label_x = arm_w + arm_w / 2;
    let right_label_x = mid_col_end + arm_w / 2;
    let full_15x7 = disc.width == DPAD_DISC_WIDTH && disc.height == DPAD_DISC_HEIGHT;

    for dy in 0..disc.height {
        for dx in 0..disc.width {
            let in_cross = (arm_w..mid_col_end).contains(&dx) || (arm_h..mid_row_end).contains(&dy);
            if !in_cross {
                continue;
            }
            let x = disc.x + dx;
            let y = disc.y + dy;
            if x >= buf.area.width || y >= buf.area.height {
                continue;
            }
            let is_center =
                (arm_w..mid_col_end).contains(&dx) && (arm_h..mid_row_end).contains(&dy);
            let cell = &mut buf[(x, y)];
            let base_bg = color_to_rgb(cell.bg, (18, 20, 30));
            let base_fg = color_to_rgb(cell.fg, (169, 177, 214));
            let (tint, alpha) = if is_center {
                (center_tint, center_alpha)
            } else {
                (arrow_tint, arrow_alpha)
            };
            let blended_bg = blend_rgb(base_bg, tint, alpha);

            let glyph: Option<(&str, Color, bool)> = if full_15x7 {
                match (dy, dx) {
                    // Row 0: top of Up box
                    (0, 5) => Some(("╭", border_fg, false)),
                    (0, 6..=8) => Some(("─", border_fg, false)),
                    (0, 9) => Some(("╮", border_fg, false)),
                    // Row 1: Up label
                    (1, 5) | (1, 9) => Some(("│", border_fg, false)),
                    (1, 7) => Some(("↑", arrow_fg, true)),
                    // Row 2: top of Left / Center / Right
                    (2, 0) | (2, 10) => Some(("╭", border_fg, false)),
                    (2, 4) | (2, 14) => Some(("╮", border_fg, false)),
                    (2, 5) => Some(("├", border_fg, false)),
                    (2, 9) => Some(("┤", border_fg, false)),
                    (2, 1..=3) | (2, 6..=8) | (2, 11..=13) => Some(("─", border_fg, false)),
                    // Row 3: Left / Paste / Right labels
                    (3, 0) | (3, 4) | (3, 5) | (3, 9) | (3, 10) | (3, 14) => {
                        Some(("│", border_fg, false))
                    }
                    (3, 2) => Some(("←", arrow_fg, true)),
                    (3, 7) => Some(("⎘", paste_fg, true)),
                    (3, 12) => Some(("→", arrow_fg, true)),
                    // Row 4: bottom of Left / Center / Right
                    (4, 0) | (4, 10) => Some(("╰", border_fg, false)),
                    (4, 4) | (4, 14) => Some(("╯", border_fg, false)),
                    (4, 5) => Some(("├", border_fg, false)),
                    (4, 9) => Some(("┤", border_fg, false)),
                    (4, 1..=3) | (4, 6..=8) | (4, 11..=13) => Some(("─", border_fg, false)),
                    // Row 5: Down label
                    (5, 5) | (5, 9) => Some(("│", border_fg, false)),
                    (5, 7) => Some(("↓", arrow_fg, true)),
                    // Row 6: bottom of Down box
                    (6, 5) => Some(("╰", border_fg, false)),
                    (6, 6..=8) => Some(("─", border_fg, false)),
                    (6, 9) => Some(("╯", border_fg, false)),
                    _ => None,
                }
            } else if dy == up_label_y && dx == center_label_x {
                Some(("↑", arrow_fg, true))
            } else if dy == down_label_y && dx == center_label_x {
                Some(("↓", arrow_fg, true))
            } else if dy == mid_label_y && dx == left_label_x {
                Some(("←", arrow_fg, true))
            } else if dy == mid_label_y && dx == center_label_x {
                Some(("⎘", paste_fg, true))
            } else if dy == mid_label_y && dx == right_label_x {
                Some(("→", arrow_fg, true))
            } else {
                None
            };

            cell.set_bg(blended_bg);
            if let Some((sym, fg, bold)) = glyph {
                cell.set_symbol(sym);
                cell.set_fg(fg);
                if bold {
                    cell.modifier.insert(Modifier::BOLD);
                } else {
                    cell.modifier.remove(Modifier::BOLD);
                }
            } else {
                if cell.symbol().width() != 1 {
                    cell.set_symbol(" ");
                }
                let ghost_fg = blend_rgb(base_fg, tint, 55);
                cell.set_fg(ghost_fg);
                cell.modifier.remove(Modifier::BOLD);
            }
        }
    }

    // Register 5 hit regions covering 100% of the Compact Cross (+):
    // Top arm (Up)
    hits.push(HitRegion {
        rect: Rect {
            x: disc.x + arm_w,
            y: disc.y,
            width: arm_w,
            height: arm_h,
        },
        target: HitTarget::SendKey(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
    });
    // Bottom arm (Down)
    hits.push(HitRegion {
        rect: Rect {
            x: disc.x + arm_w,
            y: disc.y + mid_row_end,
            width: arm_w,
            height: arm_h,
        },
        target: HitTarget::SendKey(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
    });
    // Left arm (Left)
    hits.push(HitRegion {
        rect: Rect {
            x: disc.x,
            y: disc.y + arm_h,
            width: arm_w,
            height: mid_h,
        },
        target: HitTarget::SendKey(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
    });
    // Right arm (Right)
    hits.push(HitRegion {
        rect: Rect {
            x: disc.x + mid_col_end,
            y: disc.y + arm_h,
            width: arm_w,
            height: mid_h,
        },
        target: HitTarget::SendKey(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
    });
    // Center square (PasteClipboard)
    hits.push(HitRegion {
        rect: Rect {
            x: disc.x + arm_w,
            y: disc.y + arm_h,
            width: arm_w,
            height: mid_h,
        },
        target: HitTarget::PasteClipboard,
    });

    Some(disc)
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

fn render_space_menu_modal(
    f: &mut Frame,
    area: Rect,
    top_h: u16,
    world: &World,
    hits: &mut Vec<HitRegion>,
) {
    hits.clear();
    let space_cnt = world.doc.spaces.len().max(1) as u16;
    let desired_h = (space_cnt * 2 + 5).max(7);
    let pop_y = (area.y + top_h).min(area.y + area.height.saturating_sub(3));
    let avail_h = (area.y + area.height).saturating_sub(pop_y);
    let pop_h = desired_h.min(avail_h).max(3.min(avail_h));
    let pop_w = 40u16.min(area.width.saturating_sub(2)).max(12.min(area.width));
    let pop_x = (area.x + 1).min(area.x + area.width.saturating_sub(pop_w));
    let popup = Rect {
        x: pop_x,
        y: pop_y,
        width: pop_w,
        height: pop_h,
    };

    f.render_widget(Clear, popup);
    hits.push(HitRegion {
        rect: popup,
        target: HitTarget::ModalNoop,
    });

    let block = Block::default()
        .title(" ❖ Spaces ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(125, 207, 255)))
        .style(
            Style::default()
                .bg(Color::Rgb(18, 20, 32))
                .fg(Color::Rgb(200, 205, 220)),
        );
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let buf = f.buffer_mut();
    draw_modal_close_button(buf, popup, hits);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let max_y = inner.y + inner.height;
    let footer_h: u16 = if inner.height >= 4 { 2 } else { 1 };
    let list_end_y = max_y.saturating_sub(footer_h);
    let w = inner.width as usize;

    let use_two_row = list_end_y.saturating_sub(inner.y) >= 2;
    let row_step: usize = if use_two_row { 2 } else { 1 };
    let visible_spaces = (list_end_y.saturating_sub(inner.y) as usize / row_step).max(1);
    let start_si = if world.active_space_idx < visible_spaces {
        0
    } else {
        world.active_space_idx + 1 - visible_spaces
    };

    let mut y = inner.y;
    for (si, space) in world.doc.spaces.iter().enumerate().skip(start_si) {
        if y + (row_step as u16) > list_end_y {
            break;
        }
        let is_active = si == world.active_space_idx;
        let sname = world.space_display_name(space);
        let sstate = world.space_state(space);
        let tab_cnt = space.tabs.len();
        let marker = if is_active { "▸" } else { " " };
        let prefix = format!(" {marker} ");
        let dot_x = prefix.width() as u16;
        let row1 = format!("{prefix}{} {sname} ({tab_cnt})", sstate.symbol());
        let folder = shorten_home_path(&space.folder);
        let row2 = format!("     {folder}");

        let (r1_style, r2_style) = if is_active {
            (
                Style::default()
                    .bg(Color::Rgb(46, 56, 92))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
                Style::default()
                    .bg(Color::Rgb(46, 56, 92))
                    .fg(Color::Rgb(165, 180, 220)),
            )
        } else {
            (
                Style::default()
                    .bg(Color::Rgb(24, 28, 44))
                    .fg(Color::Rgb(190, 198, 220))
                    .add_modifier(Modifier::BOLD),
                Style::default()
                    .bg(Color::Rgb(24, 28, 44))
                    .fg(Color::Rgb(115, 125, 155)),
            )
        };

        buf.set_string(inner.x, y, &pad_to_width(&row1, w), r1_style);
        if use_two_row {
            buf.set_string(inner.x, y + 1, &pad_to_width(&row2, w), r2_style);
        }
        if is_active {
            let accent = Color::Rgb(125, 207, 255);
            buf.set_string(inner.x, y, "▍", r1_style.fg(accent));
            if use_two_row {
                buf.set_string(inner.x, y + 1, "▍", r2_style.fg(accent));
            }
        }
        if inner.x + dot_x < inner.x + inner.width {
            buf.set_string(
                inner.x + dot_x,
                y,
                sstate.symbol(),
                r1_style.fg(state_color(sstate)).add_modifier(Modifier::BOLD),
            );
        }

        hits.push(HitRegion {
            rect: Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: row_step as u16,
            },
            target: HitTarget::SelectSpace(space.id.clone()),
        });
        y += row_step as u16;
    }

    // Bottom `[ + New Space  ^b C ]` action button inside the Space popover
    let btn_y = max_y.saturating_sub(1);
    let btn_lbl = pad_to_width(" ▎ + New Space          ^b C ", w);
    buf.set_string(
        inner.x,
        btn_y,
        &btn_lbl,
        Style::default()
            .bg(Color::Rgb(26, 44, 64))
            .fg(Color::Rgb(125, 207, 255))
            .add_modifier(Modifier::BOLD),
    );
    hits.push(HitRegion {
        rect: Rect {
            x: inner.x,
            y: btn_y,
            width: inner.width,
            height: 1,
        },
        target: HitTarget::NewSpacePrompt,
    });
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
        Line::from("  Top Bar [ ▾ Space ]      Open secondary Space popover menu / + New Space"),
        Line::from("  Top Bar [ Tab ] / [ + ]  Switch directly to Tab or add a new Tab"),
        Line::from("  Left Floating Cross (+)  Tap Esc / Ctl / ^B / Alt / Tab (drag to move)"),
        Line::from("  Right Floating Cross (+) Tap ↑ / ← / ⎘ (Paste) / → / ↓ (drag to move)"),
        Line::from("  Clipboard Copy / Paste   Drag text to copy (OSC 52); Tap ⎘ to paste"),
        Line::from("  Next / Prev Tab          ^b n / ^b p   or  Alt+Right / Alt+Left"),
        Line::from("  Switch Tab 1..9          ^b 1..9       or  Alt+1..9"),
        Line::from("  New Tab in Space         ^b c          or  Tap [ + ] on top tab bar"),
        Line::from("  Rename / Close Tab       ^b , (rename) /   ^b x (close)"),
        Line::from("  Next / Prev Space        ^b Down / Up  or  Alt+Down / Alt+Up"),
        Line::from("  New / Rename Space       ^b C (new)    /   ^b $ (rename)"),
        Line::from("  Toggle Space Menu        ^b b          or  Tap [ ▾ Space ] on top bar"),
        Line::from("  Quick Fuzzy Switcher     ^b w          or  ^b f"),
        Line::from("  Jump Next Alert Agent    ^b a          (◉ blocked / ✓ done)"),
        Line::from("  Scrollback / History     ^b [          or  Tap [▼ BOT] on status bar"),
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
    use crate::model::{SharedSpace, SharedTab};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn sample_world() -> World {
        let mut world = World::default();
        world.doc.spaces.push(SharedSpace {
            id: "space-1".to_string(),
            name: "main".to_string(),
            folder: "/tmp/work".to_string(),
            tabs: vec![
                SharedTab {
                    id: "tab-1".to_string(),
                    name: "agent".to_string(),
                    ..Default::default()
                },
                SharedTab {
                    id: "tab-2".to_string(),
                    name: "shell".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        world
    }

    #[test]
    fn top_tabbar_bottom_status_bar_and_dual_floating_crosses_expose_touch_hits() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let world = sample_world();
        let mut hits = Vec::new();
        let mut mod_rect_out = None;
        let mut dpad_rect_out = None;

        terminal
            .draw(|f| {
                let geom = render_ui(
                    f,
                    &world,
                    None,
                    false,
                    24,
                    true, // touch_mode = true (3-row top tabbar + 1-row bottom status bar)
                    false,
                    false,
                    false,
                    42, // scrollback_offset > 0
                    None,
                    false,
                    &Modal::None,
                );
                hits = geom.hits;
                mod_rect_out = geom.mod_rect;
                dpad_rect_out = geom.dpad_rect;
                assert_eq!(geom.term_rect.x, 0);
                assert_eq!(geom.term_rect.width, 100);
                assert_eq!(geom.term_rect.y, 3);
                assert_eq!(geom.term_rect.height, 20);
            })
            .unwrap();

        // Top Bar Space selector on rows 0..2 should hit ToggleSpaceMenu
        assert_eq!(
            find_hit_target(&hits, 1, 0),
            Some(HitTarget::ToggleSpaceMenu)
        );
        assert_eq!(
            find_hit_target(&hits, 1, 2),
            Some(HitTarget::ToggleSpaceMenu)
        );

        // Top Bar exposes Tab pill, [ + ] (NewTab), and [ ? ] (ToggleHelp)
        assert!(hits
            .iter()
            .any(|h| matches!(h.target, HitTarget::SelectTab { .. }) && h.rect.height == 3));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::NewTab && h.rect.height == 3 && h.rect.y == 0));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::ToggleHelp && h.rect.height == 3 && h.rect.y == 0));

        // Bottom 1-row read-only Agent Status Bar exposes ExitScrollback on row 23
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::ExitScrollback && h.rect.y == 23 && h.rect.height == 1));

        // Left floating Modifier Cross (+) exposes Esc, Ctl, ^B, Alt, Tab
        let mrect = mod_rect_out.expect("expected left modifier cross rect");
        assert_eq!(mrect.width, DPAD_DISC_WIDTH);
        assert_eq!(mrect.height, DPAD_DISC_HEIGHT);
        assert_eq!(
            find_hit_target(&hits, mrect.x + 7, mrect.y + 1),
            Some(HitTarget::SendKey(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE
            )))
        );
        assert_eq!(
            find_hit_target(&hits, mrect.x + 2, mrect.y + 3),
            Some(HitTarget::ToggleCtrl)
        );
        assert_eq!(
            find_hit_target(&hits, mrect.x + 7, mrect.y + 3),
            Some(HitTarget::TogglePrefix)
        );
        assert_eq!(
            find_hit_target(&hits, mrect.x + 12, mrect.y + 3),
            Some(HitTarget::ToggleAlt)
        );
        assert_eq!(
            find_hit_target(&hits, mrect.x + 7, mrect.y + 5),
            Some(HitTarget::SendKey(KeyEvent::new(
                KeyCode::Tab,
                KeyModifiers::NONE
            )))
        );

        // Right floating D-pad Cross (+) exposes Up, Left, Paste, Right, Down
        let drect = dpad_rect_out.expect("expected right dpad cross rect");
        assert_eq!(drect.width, DPAD_DISC_WIDTH);
        assert_eq!(drect.height, DPAD_DISC_HEIGHT);
        assert!(mrect.x + mrect.width <= drect.x);
        assert_eq!(
            find_hit_target(&hits, drect.x + 7, drect.y + 3),
            Some(HitTarget::PasteClipboard)
        );
        for arrow in [KeyCode::Left, KeyCode::Down, KeyCode::Up, KeyCode::Right] {
            assert!(hits.iter().any(|h| {
                h.target == HitTarget::SendKey(KeyEvent::new(arrow, KeyModifiers::NONE))
                    && h.rect.width == 5
                    && h.rect.height >= 2
            }));
        }
    }

    #[test]
    fn space_menu_popover_exposes_spaces_and_new_space_prompt() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let world = sample_world();
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
                    &Modal::SpaceMenu,
                );
                hits = geom.hits;
                assert!(geom.mod_rect.is_none());
                assert!(geom.dpad_rect.is_none());
            })
            .unwrap();

        assert!(hits
            .iter()
            .any(|h| matches!(h.target, HitTarget::SelectSpace(_))));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::NewSpacePrompt));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::CancelModal));
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
    fn narrow_terminal_retains_compact_top_space_and_plus_buttons_and_respects_touch_flag() {
        let backend = TestBackend::new(30, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let world = World::default();
        let mut hits_no_touch = Vec::new();
        let mut geom_no_touch = None;

        // Without `--touch` (`touch_mode = false`), the two floating `+` crosses are hidden
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
                hits_no_touch = geom.hits.clone();
                geom_no_touch = Some(geom);
            })
            .unwrap();

        let g_no = geom_no_touch.unwrap();
        assert!(g_no.mod_rect.is_none());
        assert!(g_no.dpad_rect.is_none());
        assert_eq!(
            find_hit_target(&hits_no_touch, 1, 0),
            Some(HitTarget::ToggleSpaceMenu)
        );
        assert!(hits_no_touch.iter().any(|h| h.target == HitTarget::NewTab));
        assert!(!hits_no_touch
            .iter()
            .any(|h| h.target == HitTarget::PasteClipboard));

        // With `--touch` (`touch_mode = true`), both floating `+` crosses are rendered
        let mut geom_touch = None;
        terminal
            .draw(|f| {
                geom_touch = Some(render_ui(
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
                    &Modal::None,
                ));
            })
            .unwrap();
        let g_yes = geom_touch.unwrap();
        assert!(g_yes.mod_rect.is_some());
        assert!(g_yes.dpad_rect.is_some());
        assert!(g_yes
            .hits
            .iter()
            .any(|h| h.target == HitTarget::PasteClipboard));
        assert!(g_yes
            .hits
            .iter()
            .any(|h| h.target == HitTarget::TogglePrefix));
    }

    #[test]
    fn overflowing_top_tabs_reveal_next_tab_and_allow_cycling_forward() {
        let backend = TestBackend::new(42, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut world = World::default();
        world.doc.spaces.push(SharedSpace {
            id: "space-1".to_string(),
            name: "main".to_string(),
            tabs: (1..=6)
                .map(|i| SharedTab {
                    id: format!("tab-{i}"),
                    name: format!("agent-{i}"),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });
        world.select_tab_index(1); // active = tab-2

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

        // Tapping the active tab when tabs overflow cycles to NextTab, and the next tab (`tab-3`) is exposed
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::TriggerAction(Action::NextTab)));
        assert!(hits.iter().any(|h| {
            h.target
                == HitTarget::SelectTab {
                    space_id: "space-1".to_string(),
                    tab_id: "tab-3".to_string(),
                }
        }));
    }

    #[test]
    fn floating_square_dpad_exposes_four_directions_and_centered_paste_and_clamps_position() {
        for width in [64u16, 42u16, 30u16] {
            let backend = TestBackend::new(width, 20);
            let mut terminal = Terminal::new(backend).unwrap();
            let world = World::default();
            let mut geom_out = None;

            terminal
                .draw(|f| {
                    geom_out = Some(render_ui_with_dpad(
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
                        &Modal::None,
                        Some((999, 999)),
                        true,
                    ));
                })
                .unwrap();

            let geom = geom_out.unwrap();
            let disc = geom.dpad_rect.expect("expected floating dpad rect");
            assert_eq!(disc.width, DPAD_DISC_WIDTH);
            assert_eq!(disc.height, DPAD_DISC_HEIGHT);
            assert!(disc.x + disc.width <= geom.term_rect.x + geom.term_rect.width);
            assert!(disc.y + disc.height <= geom.term_rect.y + geom.term_rect.height);

            // Verify 5 sectors on the 15x7 Compact Cross (+):
            // Up (top arm), Down (bottom arm), Left (left arm), Right (right arm), Paste (center)
            assert_eq!(
                find_hit_target(&geom.hits, disc.x + 7, disc.y + 1),
                Some(HitTarget::SendKey(KeyEvent::new(
                    KeyCode::Up,
                    KeyModifiers::NONE
                )))
            );
            assert_eq!(
                find_hit_target(&geom.hits, disc.x + 7, disc.y + 5),
                Some(HitTarget::SendKey(KeyEvent::new(
                    KeyCode::Down,
                    KeyModifiers::NONE
                )))
            );
            assert_eq!(
                find_hit_target(&geom.hits, disc.x + 2, disc.y + 3),
                Some(HitTarget::SendKey(KeyEvent::new(
                    KeyCode::Left,
                    KeyModifiers::NONE
                )))
            );
            assert_eq!(
                find_hit_target(&geom.hits, disc.x + 12, disc.y + 3),
                Some(HitTarget::SendKey(KeyEvent::new(
                    KeyCode::Right,
                    KeyModifiers::NONE
                )))
            );
            assert_eq!(
                find_hit_target(&geom.hits, disc.x + 7, disc.y + 3),
                Some(HitTarget::PasteClipboard)
            );
            // Hollow corners (0,0) and (14,6) are outside the cross mask (transparent to terminal)
            assert!(!is_on_dpad_disc(Some(disc), disc.x, disc.y));
            assert!(!is_on_dpad_disc(Some(disc), disc.x + 14, disc.y + 6));
            assert!(is_on_dpad_disc(Some(disc), disc.x + 7, disc.y + 3));
        }

        // Constrained viewports (e.g. 80x9 with 3-row touchbar -> 6-row viewport, or 30x8 with open sidebar)
        // scale down the + cross so all 4 directions and centered Paste remain accessible.
        for (w, h, sidebar_open) in [(80u16, 9u16, false), (30u16, 8u16, true)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();
            let world = World::default();
            let mut geom_out = None;
            terminal
                .draw(|f| {
                    geom_out = Some(render_ui_with_dpad(
                        f,
                        &world,
                        None,
                        sidebar_open,
                        24,
                        true,
                        false,
                        false,
                        false,
                        0,
                        None,
                        false,
                        &Modal::None,
                        None,
                        false,
                    ));
                })
                .unwrap();
            let geom = geom_out.unwrap();
            assert!(geom.dpad_rect.is_some());
            for arrow in [KeyCode::Up, KeyCode::Down, KeyCode::Left, KeyCode::Right] {
                assert!(geom.hits.iter().any(|hit| {
                    hit.target == HitTarget::SendKey(KeyEvent::new(arrow, KeyModifiers::NONE))
                }));
            }
            assert!(geom
                .hits
                .iter()
                .any(|hit| hit.target == HitTarget::PasteClipboard));
        }
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
