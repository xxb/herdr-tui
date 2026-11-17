//! TUI rendering (`ratatui` + `vt100`) with Top `Space / Tab1 / Tab2 / [+]` Bar,
//! Secondary Space Switcher Popover, Dual Floating Draggable `+` Cross Pads,
//! Read-Only Bottom Agent Status Bar, and modal overlays.

use crate::keys::Action;
use crate::model::{AgentState, SharedSpace, World};
use crate::usage::{
    render_eink_progress_bar, render_progress_bar, AgentKind, AgentQuota, QuotaMetric,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Widget},
    Frame,
};
use unicode_width::UnicodeWidthStr;

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
pub enum OverviewSelection {
    NewTab,
    Tab(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Modal {
    None,
    SpaceMenu,
    TabOverview {
        query: String,
        selected: usize,
        selected_target: OverviewSelection,
    },
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

impl Modal {
    pub fn default_tab_overview() -> Self {
        Self::TabOverview {
            query: String::new(),
            selected: 0,
            selected_target: OverviewSelection::NewTab,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverviewItem {
    NewTab {
        cwd: String,
    },
    Tab {
        space_id: String,
        tab_id: String,
        tab_number: usize,
        tab_name: String,
        term_addr: String,
        state: AgentState,
        cwd: String,
        detail: String,
    },
}

pub fn collect_overview_items(world: &World, query: &str) -> Vec<OverviewItem> {
    let q = query.trim().to_lowercase();
    let new_tab_cwd = world
        .active_terminal_address()
        .and_then(|addr| world.terminals.get(&addr))
        .map(|info| info.effective_cwd().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            world
                .active_space()
                .map(|s| s.folder.clone())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_default();

    let mut out = vec![OverviewItem::NewTab { cwd: new_tab_cwd }];
    let Some(space) = world.active_space() else {
        return out;
    };
    for (ti, tab) in space.tabs.iter().enumerate() {
        let tab_number = ti + 1;
        let tab_name = world.tab_display_name(space, ti);
        let term_addr = world.active_terminal_for_tab(tab).unwrap_or_default();
        let info = world.terminals.get(&term_addr);
        let cwd = info
            .map(|i| i.effective_cwd().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| space.folder.clone());
        let state = world.tab_state(tab);
        let detail = info
            .and_then(|i| {
                i.status_msg
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        (!i.title.is_empty() && i.title != tab_name).then_some(i.title.as_str())
                    })
            })
            .unwrap_or("")
            .to_string();
        let hay = format!(
            "{} {} {} {} {} {}",
            tab_number,
            tab_name.to_lowercase(),
            term_addr.to_lowercase(),
            state.label(),
            cwd.to_lowercase(),
            detail.to_lowercase()
        );
        if q.is_empty() || fuzzy_subsequence(&hay, &q) {
            out.push(OverviewItem::Tab {
                space_id: space.id.clone(),
                tab_id: tab.id.clone(),
                tab_number,
                tab_name,
                term_addr,
                state,
                cwd,
                detail,
            });
        }
    }
    out
}

pub fn resolve_overview_selection(
    items: &[OverviewItem],
    selected: usize,
    target: &OverviewSelection,
) -> usize {
    if items.is_empty() {
        return 0;
    }
    match target {
        OverviewSelection::NewTab => 0,
        OverviewSelection::Tab(tid) => items
            .iter()
            .position(|it| matches!(it, OverviewItem::Tab { tab_id, .. } if tab_id == tid))
            .unwrap_or_else(|| selected.min(items.len() - 1)),
    }
}

pub fn overview_selection_for_index(
    items: &[OverviewItem],
    idx: usize,
) -> (usize, OverviewSelection) {
    if items.is_empty() {
        return (0, OverviewSelection::NewTab);
    }
    let clamped = idx.min(items.len() - 1);
    let target = match &items[clamped] {
        OverviewItem::NewTab { .. } => OverviewSelection::NewTab,
        OverviewItem::Tab { tab_id, .. } => OverviewSelection::Tab(tab_id.clone()),
    };
    (clamped, target)
}

pub fn overview_selection_after_query_change(
    items: &[OverviewItem],
    query: &str,
) -> (usize, OverviewSelection) {
    if query.trim().is_empty() {
        (0, OverviewSelection::NewTab)
    } else if items.len() > 1 {
        overview_selection_for_index(items, 1)
    } else {
        (0, OverviewSelection::NewTab)
    }
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
    let mut end_byte = 0usize;
    for (idx, ch) in s.char_indices() {
        let next = idx + ch.len_utf8();
        if s[..next].width() > target {
            break;
        }
        end_byte = next;
    }
    let mut prefix = s[..end_byte].to_string();
    let mut out = format!("{prefix}…");
    while out.width() > max_width && !prefix.is_empty() {
        prefix.pop();
        out = format!("{prefix}…");
    }
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
        AgentState::Blocked => HERDR_RED,
        AgentState::Working => HERDR_YELLOW,
        AgentState::Done => HERDR_TEAL,
        AgentState::Idle => HERDR_GREEN,
        AgentState::None => HERDR_OVERLAY1,
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
    pub space_slop_rect: Option<Rect>,
}

pub fn is_in_rect(rect: Option<Rect>, col: u16, row: u16) -> bool {
    rect.is_some_and(|r| {
        col >= r.x
            && col < r.x.saturating_add(r.width)
            && row >= r.y
            && row < r.y.saturating_add(r.height)
    })
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
    pub dragged: bool,
}

impl TerminalSelection {
    pub fn new(row: u16, col: u16) -> Self {
        Self {
            anchor: (row, col),
            head: (row, col),
            active: true,
            dragged: false,
        }
    }

    pub fn is_non_empty(&self) -> bool {
        self.anchor != self.head
    }

    pub fn update_drag(&mut self, term_rect: Rect, mouse_col: u16, mouse_row: u16) -> bool {
        if !self.active || term_rect.width == 0 || term_rect.height == 0 {
            return false;
        }
        let anchor_col = term_rect.x.saturating_add(self.anchor.1);
        let anchor_row = term_rect.y.saturating_add(self.anchor.0);
        if mouse_col != anchor_col || mouse_row != anchor_row {
            self.dragged = true;
        }
        let rel_col = mouse_col
            .saturating_sub(term_rect.x)
            .min(term_rect.width.saturating_sub(1));
        let rel_row = mouse_row
            .saturating_sub(term_rect.y)
            .min(term_rect.height.saturating_sub(1));
        let new_head = (rel_row, rel_col);
        if self.head != new_head {
            self.head = new_head;
            true
        } else {
            false
        }
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
        let end_row = er.min(scr_rows.get() - 1);
        let end_col_excl = ec.min(scr_cols.get() - 1).saturating_add(1);
        screen.contents_between(sr, sc, end_row, end_col_excl)
    }
}

pub fn top_bar_height(area: Rect, _touch_mode: bool) -> u16 {
    if area.height <= 1 {
        0
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

#[cfg(test)]
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
    render_ui_with_pads_and_quota(
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
        mod_pos,
        mod_dragging,
        dpad_pos,
        dpad_dragging,
        None,
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn render_ui_with_pads_and_quota(
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
    focused_quota: Option<&AgentQuota>,
) -> UiGeometry {
    render_ui_full(
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
        mod_pos,
        mod_dragging,
        dpad_pos,
        dpad_dragging,
        focused_quota,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn render_ui_full(
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
    focused_quota: Option<&AgentQuota>,
    eink_mode: bool,
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

    let tab_overview_open = matches!(modal, Modal::TabOverview { .. });

    if tab_overview_open {
        // TabOverview owns the full terminal viewport; avoid painting stale background VT cells.
    } else if let Some(scr) = screen {
        f.render_widget(
            VtScreenWidget {
                screen: scr,
                selection,
                eink_mode,
            },
            term_rect,
        );
        if matches!(modal, Modal::None) && scrollback_offset == 0 && !scr.hide_cursor() {
            let (row, col) = scr.cursor_position();
            let clamped_col = col.min(term_rect.width.saturating_sub(1));
            let cx = term_rect.x.saturating_add(clamped_col);
            let cy = term_rect.y.saturating_add(row);
            if cx < term_rect.x + term_rect.width && cy < term_rect.y + term_rect.height {
                f.set_cursor_position((cx, cy));
            }
        }
    } else {
        if eink_mode {
            f.render_widget(
                Block::default().style(Style::default().bg(EINK_PAPER).fg(EINK_INK)),
                term_rect,
            );
        }
        let placeholder = Paragraph::new("Connecting to terminal...")
            .style(if eink_mode {
                Style::default().bg(EINK_PAPER).fg(EINK_INK_MUTED)
            } else {
                Style::default().fg(Color::DarkGray)
            })
            .alignment(Alignment::Center);
        f.render_widget(placeholder, term_rect);
    }

    let mut mod_rect = None;
    let mut dpad_rect = None;
    let space_menu_open = matches!(modal, Modal::SpaceMenu);

    let space_bounds = render_top_tabbar(
        f,
        top_rect,
        world,
        space_menu_open,
        tab_overview_open,
        eink_mode,
        &mut hits,
    );
    let space_slop_rect = if matches!(modal, Modal::None) && term_rect.height > 0 {
        space_bounds.and_then(|(sx, send)| {
            let slop_x = sx.max(term_rect.x);
            let slop_end = send.min(term_rect.x.saturating_add(term_rect.width));
            let slop_w = slop_end.saturating_sub(slop_x);
            let slop_h = if term_rect.height >= 4 { 2 } else { 1 };
            (slop_w > 0).then_some(Rect {
                x: slop_x,
                y: term_rect.y,
                width: slop_w,
                height: slop_h,
            })
        })
    } else {
        None
    };
    render_bottom_status_bar(
        f,
        bottom_rect,
        if tab_overview_open {
            None
        } else {
            focused_quota
        },
        if tab_overview_open {
            0
        } else {
            scrollback_offset
        },
        copied_flash,
        eink_mode,
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
                    eink_mode,
                    &mut hits,
                );
                dpad_rect = render_floating_dpad(
                    f,
                    term_rect,
                    dpad_pos,
                    dpad_dragging,
                    eink_mode,
                    &mut hits,
                );
            }
        }
        Modal::SpaceMenu => {
            render_space_menu_modal(f, area, top_h, world, eink_mode, &mut hits);
        }
        Modal::TabOverview {
            query,
            selected,
            selected_target,
        } => {
            render_tab_overview_page(
                f,
                term_rect,
                world,
                query,
                *selected,
                selected_target,
                eink_mode,
                &mut hits,
            );
        }
        Modal::Help => {
            render_help_modal(f, area, eink_mode, &mut hits);
        }
        Modal::ConfirmCloseTab { tab_id, tab_title } => {
            render_confirm_close_modal(f, area, tab_id, tab_title, eink_mode, &mut hits);
        }
        Modal::Prompt { kind, input } => {
            render_prompt_modal(f, area, kind, input, eink_mode, &mut hits);
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
                eink_mode,
                &mut hits,
            );
        }
    }

    UiGeometry {
        term_rect,
        hits,
        mod_rect,
        dpad_rect,
        space_slop_rect,
    }
}

// 300 PPI Carta 4-bit neutral LUT tokens (R == G == B, multiples of 0x11)
pub const EINK_PAPER: Color = Color::Rgb(255, 255, 255); // #ffffff
pub const EINK_INK: Color = Color::Rgb(0, 0, 0); // #000000
pub const EINK_INK_MUTED: Color = Color::Rgb(51, 51, 51); // #333333
pub const EINK_SURFACE: Color = Color::Rgb(238, 238, 238); // #eeeeee
pub const EINK_PILL_BG: Color = Color::Rgb(221, 221, 221); // #dddddd

// Classic `herdr` Catppuccin Mocha palette tokens (`Palette::catppuccin()`)
const HERDR_ACCENT: Color = Color::Rgb(137, 180, 250); // #89b4fa (Blue — focused tab bg)
const HERDR_PANEL_BG: Color = Color::Rgb(24, 24, 37); // #181825 (Mantle — bar bg & focused tab fg)
const HERDR_SURFACE_DIM: Color = Color::Rgb(30, 30, 46); // #1e1e2e (Base)
const HERDR_SURFACE0: Color = Color::Rgb(49, 50, 68); // #313244 (Inactive tab pill bg)
const HERDR_SURFACE1: Color = Color::Rgb(69, 71, 90); // #45475a (Open menu / dim border)
const HERDR_OVERLAY0: Color = Color::Rgb(108, 112, 134); // #6c7086 (Subtle divider)
const HERDR_OVERLAY1: Color = Color::Rgb(127, 132, 156); // #7f849c
const HERDR_SUBTEXT0: Color = Color::Rgb(166, 173, 200); // #a6adc8 (Inactive tab / help fg)
const HERDR_TEXT: Color = Color::Rgb(205, 214, 244); // #cdd6f4
const HERDR_MAUVE: Color = Color::Rgb(203, 166, 247); // #cba6f7 (Space selector fg)
const HERDR_GREEN: Color = Color::Rgb(166, 227, 161); // #a6e3a1 (New tab `+` / Idle fg)
const HERDR_YELLOW: Color = Color::Rgb(249, 226, 175); // #f9e2af (Working state)
const HERDR_RED: Color = Color::Rgb(243, 139, 168); // #f38ba8 (Blocked state / Detach fg)
const HERDR_TEAL: Color = Color::Rgb(148, 226, 213); // #94e2d5 (Done state)

const TOPBAR_UNDERLINE_DIM: Color = HERDR_SURFACE1;
const TOPBAR_UNDERLINE_ACTIVE: Color = HERDR_ACCENT;

struct PlannedTabBar {
    left_badge: Option<String>,
    right_badge: Option<String>,
    pills: Vec<(String, String, AgentState, bool)>,
}

fn joined_pills_width(labels: &[String]) -> u16 {
    if labels.is_empty() {
        0
    } else {
        let sum: usize = labels.iter().map(|s| s.width()).sum();
        sum.saturating_add(labels.len() - 1).min(u16::MAX as usize) as u16
    }
}

fn overflow_badge_label(hidden_count: usize, is_left: bool, compact: bool) -> Option<String> {
    if hidden_count == 0 {
        None
    } else if compact {
        Some(if is_left {
            format!("‹{hidden_count}")
        } else {
            format!("{hidden_count}›")
        })
    } else {
        Some(if is_left {
            format!(" ‹{hidden_count} ")
        } else {
            format!(" {hidden_count}› ")
        })
    }
}

fn eink_tab_prefix(state: AgentState, is_active: bool) -> &'static str {
    match state {
        AgentState::Blocked => "!",
        AgentState::Working if !is_active => "*",
        AgentState::Done if !is_active => "✓",
        _ => "",
    }
}

fn tab_pill_style(st: AgentState, is_active: bool, eink_mode: bool) -> Style {
    if eink_mode {
        return if is_active {
            Style::default()
                .bg(EINK_INK)
                .fg(EINK_PAPER)
                .add_modifier(Modifier::BOLD)
        } else {
            match st {
                AgentState::Blocked | AgentState::Done => Style::default()
                    .bg(EINK_PILL_BG)
                    .fg(EINK_INK)
                    .add_modifier(Modifier::BOLD),
                AgentState::Working => Style::default()
                    .bg(EINK_SURFACE)
                    .fg(EINK_INK)
                    .add_modifier(Modifier::BOLD),
                AgentState::Idle | AgentState::None => {
                    Style::default().bg(EINK_SURFACE).fg(EINK_INK_MUTED)
                }
            }
        };
    }
    if is_active {
        Style::default()
            .bg(HERDR_ACCENT)
            .fg(HERDR_PANEL_BG)
            .add_modifier(Modifier::BOLD)
    } else {
        match st {
            AgentState::Blocked => Style::default()
                .bg(Color::Rgb(64, 38, 50))
                .fg(HERDR_RED)
                .add_modifier(Modifier::BOLD),
            AgentState::Done => Style::default()
                .bg(Color::Rgb(34, 54, 52))
                .fg(HERDR_TEAL)
                .add_modifier(Modifier::BOLD),
            AgentState::Working => Style::default().bg(Color::Rgb(56, 50, 38)).fg(HERDR_YELLOW),
            AgentState::Idle | AgentState::None => {
                Style::default().bg(HERDR_SURFACE0).fg(HERDR_SUBTEXT0)
            }
        }
    }
}

fn plan_top_tabbar_pills(
    world: &World,
    space: &SharedSpace,
    rect_width: u16,
    avail_tabs_w: u16,
    eink_mode: bool,
) -> PlannedTabBar {
    let total_tabs = space.tabs.len();
    if total_tabs == 0 || avail_tabs_w == 0 {
        return PlannedTabBar {
            left_badge: None,
            right_badge: None,
            pills: Vec::new(),
        };
    }

    let active_ti = world.active_tab_index().min(total_tabs - 1);
    let full_names: Vec<String> = (0..total_tabs)
        .map(|ti| world.tab_display_name(space, ti))
        .collect();
    let states: Vec<AgentState> = space.tabs.iter().map(|tab| world.tab_state(tab)).collect();

    let state_prefix = |ti: usize| {
        if eink_mode {
            eink_tab_prefix(states[ti], ti == active_ti)
        } else {
            ""
        }
    };
    let index_only_lbl = |ti: usize| format!(" {}{} ", state_prefix(ti), ti + 1);
    let titled_lbl = |ti: usize, max_name_w: usize| {
        let tname = truncate_to_width(&full_names[ti], max_name_w);
        if tname.is_empty() {
            index_only_lbl(ti)
        } else {
            format!(" {}{}:{} ", state_prefix(ti), ti + 1, tname)
        }
    };
    let build_all_pills = |labels: Vec<String>| PlannedTabBar {
        left_badge: None,
        right_badge: None,
        pills: labels
            .into_iter()
            .enumerate()
            .map(|(ti, lbl)| (space.tabs[ti].id.clone(), lbl, states[ti], ti == active_ti))
            .collect(),
    };

    // Stage 1 & 2: Try fitting all tabs with full or progressively shorter titles.
    let title_widths: &[usize] = if rect_width >= 64 {
        &[14, 10, 7, 4]
    } else if rect_width >= 40 {
        &[8, 5, 3]
    } else {
        &[]
    };
    for &max_w in title_widths {
        let labels: Vec<String> = (0..total_tabs).map(|ti| titled_lbl(ti, max_w)).collect();
        if joined_pills_width(&labels) <= avail_tabs_w {
            return build_all_pills(labels);
        }
    }

    // Stage 3: Accordion / Fisheye mode — fold inactive tabs to `[ N ]` so all tabs stay visible
    // in fixed order while expanding the active tab's title into the remaining budget.
    let mut idx_labels: Vec<String> = (0..total_tabs).map(index_only_lbl).collect();
    let all_idx_w = joined_pills_width(&idx_labels);
    if all_idx_w <= avail_tabs_w {
        let inactive_plus_gaps_w = all_idx_w.saturating_sub(idx_labels[active_ti].width() as u16);
        let active_budget = avail_tabs_w.saturating_sub(inactive_plus_gaps_w) as usize;
        let framing = (active_ti + 1).to_string().len() + 3 + state_prefix(active_ti).width();
        if rect_width >= 36 && active_budget >= framing + 2 {
            let max_active_name_w = (active_budget - framing).min(16);
            let candidate = titled_lbl(active_ti, max_active_name_w);
            if candidate.width() <= active_budget {
                idx_labels[active_ti] = candidate;
            }
        }
        return build_all_pills(idx_labels);
    }

    // Stage 4: Extreme overflow fallback — center a balanced window around `active_ti`
    // flanked by clickable `[ ‹N ]` / `[ N› ]` hidden-count badges.
    let mut compact_badges = avail_tabs_w < 18;
    let mut stage4_labels = idx_labels;
    if rect_width >= 44 && avail_tabs_w >= 28 {
        stage4_labels[active_ti] = titled_lbl(active_ti, 6);
    }

    let window_width = |start: usize, end: usize, labels: &[String], compact: bool| -> u16 {
        let mut total = 0usize;
        let mut items = 0usize;
        if let Some(lb) = overflow_badge_label(start, true, compact) {
            total = total.saturating_add(lb.width());
            items += 1;
        }
        for lbl in &labels[start..=end] {
            total = total.saturating_add(lbl.width());
            items += 1;
        }
        if let Some(rb) = overflow_badge_label(total_tabs - 1 - end, false, compact) {
            total = total.saturating_add(rb.width());
            items += 1;
        }
        total
            .saturating_add(items.saturating_sub(1))
            .min(u16::MAX as usize) as u16
    };

    if window_width(active_ti, active_ti, &stage4_labels, compact_badges) > avail_tabs_w {
        stage4_labels[active_ti] = index_only_lbl(active_ti);
    }
    if window_width(active_ti, active_ti, &stage4_labels, compact_badges) > avail_tabs_w {
        compact_badges = true;
    }
    if window_width(active_ti, active_ti, &stage4_labels, compact_badges) > avail_tabs_w {
        let max_w = avail_tabs_w as usize;
        let fallback = if stage4_labels[active_ti].width() <= max_w {
            stage4_labels[active_ti].clone()
        } else {
            let unpadded = format!("{}{}", state_prefix(active_ti), active_ti + 1);
            if unpadded.width() <= max_w {
                unpadded
            } else {
                truncate_to_width(&unpadded, max_w)
            }
        };
        let pills = if fallback.is_empty() {
            Vec::new()
        } else {
            vec![(
                space.tabs[active_ti].id.clone(),
                fallback,
                states[active_ti],
                true,
            )]
        };
        return PlannedTabBar {
            left_badge: None,
            right_badge: None,
            pills,
        };
    }

    let mut vis_start = active_ti;
    let mut vis_end = active_ti;
    loop {
        let mut progressed = false;
        if vis_end + 1 < total_tabs {
            if window_width(vis_start, vis_end + 1, &stage4_labels, compact_badges) <= avail_tabs_w
            {
                vis_end += 1;
                progressed = true;
            } else if vis_end + 3 == total_tabs
                && window_width(vis_start, total_tabs - 1, &stage4_labels, compact_badges)
                    <= avail_tabs_w
            {
                vis_end = total_tabs - 1;
                progressed = true;
            }
        }
        if vis_start > 0 {
            if window_width(vis_start - 1, vis_end, &stage4_labels, compact_badges) <= avail_tabs_w
            {
                vis_start -= 1;
                progressed = true;
            } else if vis_start == 2
                && window_width(0, vis_end, &stage4_labels, compact_badges) <= avail_tabs_w
            {
                vis_start = 0;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }

    let left_badge = overflow_badge_label(vis_start, true, compact_badges);
    let right_badge = overflow_badge_label(total_tabs - 1 - vis_end, false, compact_badges);
    let pills = (vis_start..=vis_end)
        .map(|ti| {
            (
                space.tabs[ti].id.clone(),
                stage4_labels[ti].clone(),
                states[ti],
                ti == active_ti,
            )
        })
        .collect();

    PlannedTabBar {
        left_badge,
        right_badge,
        pills,
    }
}

fn draw_touchbar_pill(
    buf: &mut Buffer,
    x: u16,
    rect: Rect,
    label: &str,
    style: Style,
    emphasized: bool,
    eink_mode: bool,
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
        let (top_fg, bot_fg) = if eink_mode {
            (EINK_INK, EINK_INK)
        } else {
            (
                if emphasized { HERDR_TEXT } else { HERDR_ACCENT },
                if emphasized {
                    HERDR_ACCENT
                } else {
                    HERDR_SURFACE1
                },
            )
        };
        let top_line = "▔".repeat(lw as usize);
        let bot_line = "▁".repeat(lw as usize);
        buf.set_string(x, rect.y, &top_line, style.fg(top_fg));
        buf.set_string(x, rect.y + 1, label, style);
        buf.set_string(x, rect.y + 2, &bot_line, style.fg(bot_fg));
    } else if rect.height == 2 && lw > 0 {
        // 2-row keycap pill:
        // - Row 0 (`rect.y`): vertically centered glyph inside the colored pill (`style`)
        // - Row 1 (`rect.y + 1`): crisp bottom gloss highlight (`▔`) hugging the lower edge
        //   of Row 0 against the dark bar background so text never appears pushed down.
        let (gloss_bg, bot_gloss_fg) = if eink_mode {
            (EINK_PAPER, EINK_INK)
        } else {
            (
                HERDR_PANEL_BG,
                if emphasized {
                    HERDR_ACCENT
                } else {
                    HERDR_SURFACE1
                },
            )
        };
        let bot_line = "▔".repeat(lw as usize);
        buf.set_string(x, rect.y, label, style);
        buf.set_string(
            x,
            rect.y + 1,
            &bot_line,
            Style::default().bg(gloss_bg).fg(bot_gloss_fg),
        );
    } else {
        let ul_color = if eink_mode {
            EINK_INK
        } else if emphasized {
            TOPBAR_UNDERLINE_ACTIVE
        } else {
            TOPBAR_UNDERLINE_DIM
        };
        let pill_style = style
            .add_modifier(Modifier::UNDERLINED)
            .underline_color(ul_color);
        buf.set_string(x, rect.y, label, pill_style);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_top_tabbar(
    f: &mut Frame,
    rect: Rect,
    world: &World,
    space_menu_open: bool,
    tab_overview_open: bool,
    eink_mode: bool,
    hits: &mut Vec<HitRegion>,
) -> Option<(u16, u16)> {
    if rect.width == 0 || rect.height == 0 {
        return None;
    }
    let bar_bg = if eink_mode {
        EINK_PAPER
    } else {
        HERDR_PANEL_BG
    };
    let bar_fg = if eink_mode { EINK_INK } else { HERDR_TEXT };
    let bar_ul = if eink_mode {
        EINK_INK
    } else {
        TOPBAR_UNDERLINE_DIM
    };
    let mut bg_bar = Style::default().bg(bar_bg).fg(bar_fg);
    if rect.height == 1 {
        bg_bar = bg_bar
            .add_modifier(Modifier::UNDERLINED)
            .underline_color(bar_ul);
    }
    f.render_widget(Block::default().style(bg_bar), rect);

    let mut x = rect.x;
    let max_x = rect.x + rect.width;
    let buf = f.buffer_mut();

    if rect.height >= 3 {
        let full_line = "▔".repeat(rect.width as usize);
        buf.set_string(
            rect.x,
            rect.y,
            &full_line,
            Style::default()
                .bg(bar_bg)
                .fg(if eink_mode { EINK_INK } else { HERDR_ACCENT }),
        );
    } else if rect.height == 2 {
        let full_line = "▔".repeat(rect.width as usize);
        buf.set_string(
            rect.x,
            rect.y + 1,
            &full_line,
            Style::default()
                .bg(bar_bg)
                .fg(if eink_mode { EINK_INK } else { HERDR_SURFACE1 }),
        );
    }

    let detach_lbl = if rect.width < 48 { " ⏏ " } else { "  ⏏  " };
    let detach_w = detach_lbl.width() as u16;
    let help_lbl = if rect.width < 48 { " ? " } else { "  ?  " };
    let help_w = help_lbl.width() as u16;
    let plus_lbl = if rect.width < 48 { " + " } else { "  +  " };
    let plus_w = plus_lbl.width() as u16;
    let avail_end = max_x.saturating_sub(help_w.min(rect.width.saturating_sub(4)));

    // 0. Far Left: Detach button (`[ ⏏ ]`) — equivalent to `^b q` (`Action::Detach`)
    if detach_w > 0 && x + detach_w <= max_x {
        let dstyle = if eink_mode {
            Style::default()
                .bg(EINK_SURFACE)
                .fg(EINK_INK)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .bg(Color::Rgb(42, 30, 42))
                .fg(HERDR_RED)
                .add_modifier(Modifier::BOLD)
        };
        draw_touchbar_pill(buf, x, rect, detach_lbl, dstyle, false, eink_mode);
        hits.push(HitRegion {
            rect: Rect {
                x,
                y: rect.y,
                width: detach_w,
                height: rect.height,
            },
            target: HitTarget::TriggerAction(Action::Detach),
        });
        x += detach_w;
        if rect.width >= 48 && x + 1 < avail_end {
            x += 1;
        }
    }
    let space_hit_start = rect
        .x
        .saturating_add(if detach_w > 0 && rect.width >= detach_w {
            detach_w
        } else {
            0
        });

    // 1. Space selector button (`[ ▾ SpaceName ]`) — opens secondary Space popover menu
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
    } else if matches!(sstate, AgentState::Blocked | AgentState::Done) {
        format!(
            " {arrow} {} {} ",
            sstate.symbol(),
            truncate_to_width(&sname, max_space_w.saturating_sub(6))
        )
    } else {
        format!(
            " {arrow} {} ",
            truncate_to_width(&sname, max_space_w.saturating_sub(4))
        )
    };
    let space_fit = avail_end.saturating_sub(x) as usize;
    let space_budget = avail_end.saturating_sub(x).saturating_sub(plus_w + 2);
    let space_lbl = if (raw_space_lbl.width() as u16) <= space_budget {
        raw_space_lbl
    } else {
        truncate_to_width(
            &raw_space_lbl,
            (space_budget as usize).max(4).min(space_fit),
        )
    };
    let space_w = space_lbl.width() as u16;
    let mut drew_space = false;
    if space_w > 0 && x + space_w <= max_x {
        let style = if eink_mode {
            if space_menu_open {
                Style::default()
                    .bg(EINK_INK)
                    .fg(EINK_PAPER)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .bg(EINK_SURFACE)
                    .fg(EINK_INK)
                    .add_modifier(Modifier::BOLD)
            }
        } else {
            Style::default()
                .bg(if space_menu_open {
                    HERDR_SURFACE1
                } else {
                    Color::Rgb(36, 36, 54)
                })
                .fg(HERDR_MAUVE)
                .add_modifier(Modifier::BOLD)
        };
        draw_touchbar_pill(
            buf,
            x,
            rect,
            &space_lbl,
            style,
            eink_mode && space_menu_open,
            eink_mode,
        );
        x += space_w;
        drew_space = true;
    }

    // Subtle slash divider between Space button and Tab pills
    if x + 1 < avail_end {
        let div_y = if rect.height >= 3 { rect.y + 1 } else { rect.y };
        let mut div_style = Style::default().bg(bar_bg).fg(if eink_mode {
            EINK_INK_MUTED
        } else {
            HERDR_OVERLAY0
        });
        if rect.height == 1 {
            div_style = div_style
                .add_modifier(Modifier::UNDERLINED)
                .underline_color(bar_ul);
        }
        buf.set_string(x, div_y, "╱", div_style);
        x += 1;
    }

    // Invisibly extend Space's Row-0 hit box across adjacent gap + `╱` divider (zero visual change).
    let mut space_drawn_bounds: Option<(u16, u16)> = None;
    if drew_space {
        let space_hit_end = x;
        let space_hit_w = space_hit_end.saturating_sub(space_hit_start);
        if space_hit_w > 0 {
            hits.push(HitRegion {
                rect: Rect {
                    x: space_hit_start,
                    y: rect.y,
                    width: space_hit_w,
                    height: rect.height,
                },
                target: HitTarget::ToggleSpaceMenu,
            });
            space_drawn_bounds = Some((space_hit_start, space_hit_end));
        }
    }

    // 2. Horizontal Tab pills (`space / tab1 / tab2 ...`) + trailing `[ + ]` button
    let tabs_limit_x = avail_end.saturating_sub(plus_w + 1);
    if let Some(space) = world.active_space() {
        let space_id = space.id.clone();
        let avail_tabs_w = tabs_limit_x.saturating_sub(x);
        let plan = plan_top_tabbar_pills(world, space, rect.width, avail_tabs_w, eink_mode);
        let overflow_style = if eink_mode {
            Style::default()
                .bg(EINK_SURFACE)
                .fg(EINK_INK)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .bg(HERDR_SURFACE0)
                .fg(HERDR_ACCENT)
                .add_modifier(Modifier::BOLD)
        };

        let mut drew_any = false;
        if let Some(lb) = plan.left_badge {
            let bw = lb.width() as u16;
            if bw > 0 && x + bw <= tabs_limit_x {
                draw_touchbar_pill(buf, x, rect, &lb, overflow_style, false, eink_mode);
                hits.push(HitRegion {
                    rect: Rect {
                        x,
                        y: rect.y,
                        width: bw,
                        height: rect.height,
                    },
                    target: HitTarget::TriggerAction(Action::FuzzyFinder),
                });
                x += bw;
                drew_any = true;
            }
        }

        for (tab_id, lbl, st, is_active) in plan.pills {
            let pw = lbl.width() as u16;
            if pw == 0 {
                continue;
            }
            let next_x = x + u16::from(drew_any);
            if next_x + pw > tabs_limit_x {
                break;
            }
            x = next_x;
            let pill_active = is_active && !tab_overview_open;
            let tstyle = tab_pill_style(st, pill_active, eink_mode);
            draw_touchbar_pill(buf, x, rect, &lbl, tstyle, pill_active, eink_mode);
            hits.push(HitRegion {
                rect: Rect {
                    x,
                    y: rect.y,
                    width: pw,
                    height: rect.height,
                },
                target: HitTarget::SelectTab {
                    space_id: space_id.clone(),
                    tab_id,
                },
            });
            x += pw;
            drew_any = true;
        }

        if let Some(rb) = plan.right_badge {
            let bw = rb.width() as u16;
            let next_x = x + u16::from(drew_any);
            if bw > 0 && next_x + bw <= tabs_limit_x {
                x = next_x;
                draw_touchbar_pill(buf, x, rect, &rb, overflow_style, false, eink_mode);
                hits.push(HitRegion {
                    rect: Rect {
                        x,
                        y: rect.y,
                        width: bw,
                        height: rect.height,
                    },
                    target: HitTarget::TriggerAction(Action::FuzzyFinder),
                });
                x += bw;
                drew_any = true;
            }
        }

        if drew_any && x + 1 + plus_w <= avail_end {
            x += 1;
        }
    }

    // 3. `[ + ]` button immediately after the tabs to add a new Tab
    if x + plus_w <= avail_end {
        let pstyle = if eink_mode {
            if tab_overview_open {
                Style::default()
                    .bg(EINK_INK)
                    .fg(EINK_PAPER)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .bg(EINK_SURFACE)
                    .fg(EINK_INK)
                    .add_modifier(Modifier::BOLD)
            }
        } else if tab_overview_open {
            Style::default()
                .bg(HERDR_ACCENT)
                .fg(HERDR_PANEL_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .bg(Color::Rgb(32, 46, 42))
                .fg(HERDR_GREEN)
                .add_modifier(Modifier::BOLD)
        };
        draw_touchbar_pill(buf, x, rect, plus_lbl, pstyle, tab_overview_open, eink_mode);
        hits.push(HitRegion {
            rect: Rect {
                x,
                y: rect.y,
                width: plus_w,
                height: rect.height,
            },
            target: HitTarget::NewTab,
        });
        x += plus_w;
    }

    // 4. Far Right: `[ ? ]` Quick Actions & Help button
    let rx = max_x.saturating_sub(help_w);
    if rx >= x && rx + help_w <= max_x {
        let hstyle = if eink_mode {
            Style::default()
                .bg(EINK_SURFACE)
                .fg(EINK_INK)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .bg(Color::Rgb(36, 36, 54))
                .fg(HERDR_SUBTEXT0)
                .add_modifier(Modifier::BOLD)
        };
        draw_touchbar_pill(buf, rx, rect, help_lbl, hstyle, false, eink_mode);
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

    space_drawn_bounds
}

fn quota_remaining_color(remaining_percent: f64) -> Color {
    if remaining_percent <= 15.0 {
        Color::Rgb(247, 118, 142)
    } else if remaining_percent <= 40.0 {
        Color::Rgb(224, 175, 104)
    } else {
        Color::Rgb(115, 218, 146)
    }
}

fn agent_badge_colors(agent: AgentKind) -> (Color, Color) {
    match agent {
        AgentKind::Codex => (Color::Rgb(24, 52, 74), Color::Rgb(125, 207, 255)),
        AgentKind::Claude => (Color::Rgb(62, 38, 28), Color::Rgb(245, 169, 127)),
        AgentKind::OpenCode => (Color::Rgb(28, 54, 44), Color::Rgb(158, 206, 106)),
        AgentKind::Antigravity => (Color::Rgb(48, 36, 74), Color::Rgb(187, 154, 247)),
        AgentKind::Freebuff => (Color::Rgb(56, 48, 26), Color::Rgb(224, 175, 104)),
    }
}

fn metric_display_width(
    m: &QuotaMetric,
    bar_w: usize,
    show_detail: bool,
    eink_mode: bool,
) -> usize {
    let pct = m.remaining_percent.clamp(0.0, 100.0).round() as u64;
    let pct_str = if eink_mode && m.remaining_percent <= 15.0 {
        format!("{pct}%!")
    } else {
        format!("{pct}%")
    };
    let bar_part = if bar_w > 0 { bar_w + 1 } else { 0 };
    let detail_part = if show_detail {
        m.detail.as_ref().map_or(0, |d| 3 + d.width())
    } else {
        0
    };
    m.window_label.width() + 1 + bar_part + pct_str.width() + detail_part
}

fn push_clamped_str(buf: &mut Buffer, x: &mut u16, y: u16, avail_end: u16, s: &str, style: Style) {
    if *x >= avail_end || s.is_empty() {
        return;
    }
    let rem = (avail_end - *x) as usize;
    let clipped = truncate_to_width(s, rem);
    let w = clipped.width() as u16;
    if w > 0 {
        buf.set_string(*x, y, &clipped, style);
        *x = (*x).saturating_add(w);
    }
}

fn render_bottom_status_bar(
    f: &mut Frame,
    rect: Rect,
    focused_quota: Option<&AgentQuota>,
    scrollback_offset: usize,
    copied_flash: bool,
    eink_mode: bool,
    hits: &mut Vec<HitRegion>,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let bar_bg = if eink_mode {
        EINK_PAPER
    } else {
        HERDR_PANEL_BG
    };
    let base_style =
        Style::default()
            .bg(bar_bg)
            .fg(if eink_mode { EINK_INK } else { HERDR_SUBTEXT0 });
    f.render_widget(Block::default().style(base_style), rect);

    let buf = f.buffer_mut();
    let max_x = rect.x + rect.width;
    let mut x = rect.x;

    // Reserve right-side space for optional `✓ Copied`, E-Ink `[ ▲Pg ]` / `[ ▼Pg ]` pills, and `▼ BOT (+N)`
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
    let pg_up_lbl = " ▲Pg ";
    let pg_dn_lbl = " ▼Pg ";
    let pg_up_w = pg_up_lbl.width() as u16;
    let pg_dn_w = pg_dn_lbl.width() as u16;
    let pg_gap_before = u16::from(copied_lbl.is_some());
    let pg_gap_after = u16::from(scroll_lbl.is_some());
    let pg_pair_w = pg_gap_before + pg_up_w + 1 + pg_dn_w + pg_gap_after;
    let show_pg_btns =
        eink_mode && rect.width >= 32 && copied_w + scroll_w + pg_pair_w <= rect.width;
    let pg_w = if show_pg_btns { pg_pair_w } else { 0 };
    let right_reserved = copied_w + pg_w + scroll_w;
    let avail_end = max_x.saturating_sub(right_reserved);

    // Strictly render ONLY the currently focused agent's quota usage on this single row.
    // When no agent is focused (e.g. a plain shell tab), keep the left side clean without
    // repeating status/cwd/turn info that the terminal already displays.
    if let Some(quota) = focused_quota {
        let (badge_bg, badge_fg) = if eink_mode {
            (EINK_INK, EINK_PAPER)
        } else {
            agent_badge_colors(quota.agent)
        };
        let badge_lbl = if avail_end.saturating_sub(x) < 22 {
            match quota.agent {
                AgentKind::Codex => " Cdx ".to_string(),
                AgentKind::Claude => " Clau ".to_string(),
                AgentKind::OpenCode => " OC ".to_string(),
                AgentKind::Antigravity => " Agy ".to_string(),
                AgentKind::Freebuff => " Fb ".to_string(),
            }
        } else {
            format!(" {} ", quota.agent.display_name())
        };
        push_clamped_str(
            buf,
            &mut x,
            rect.y,
            avail_end,
            &badge_lbl,
            Style::default()
                .bg(badge_bg)
                .fg(badge_fg)
                .add_modifier(Modifier::BOLD),
        );

        if x < avail_end {
            push_clamped_str(buf, &mut x, rect.y, avail_end, " ", base_style);
        }

        if quota.metrics.is_empty() {
            push_clamped_str(
                buf,
                &mut x,
                rect.y,
                avail_end,
                "剩余 --",
                Style::default().bg(bar_bg).fg(if eink_mode {
                    EINK_INK_MUTED
                } else {
                    Color::Rgb(126, 136, 168)
                }),
            );
        } else if x < avail_end {
            let avail_w = (avail_end - x) as usize;
            const TIERS: [(usize, bool, &str); 5] = [
                (8, true, "  ·  "),
                (6, true, " · "),
                (8, false, "  ·  "),
                (5, false, " · "),
                (0, false, " · "),
            ];
            let (bar_w, show_detail, sep) = TIERS
                .into_iter()
                .find(|(bw, sd, sp)| {
                    let sep_total = sp.width() * quota.metrics.len().saturating_sub(1);
                    let metrics_total: usize = quota
                        .metrics
                        .iter()
                        .map(|m| metric_display_width(m, *bw, *sd, eink_mode))
                        .sum();
                    metrics_total + sep_total <= avail_w
                })
                .unwrap_or((0, false, " · "));

            let (lbl_style, track_style, detail_style, sep_style) = if eink_mode {
                (
                    Style::default()
                        .bg(bar_bg)
                        .fg(EINK_INK)
                        .add_modifier(Modifier::BOLD),
                    Style::default().bg(bar_bg).fg(EINK_INK_MUTED),
                    Style::default().bg(bar_bg).fg(EINK_INK_MUTED),
                    Style::default().bg(bar_bg).fg(EINK_INK_MUTED),
                )
            } else {
                (
                    Style::default()
                        .bg(bar_bg)
                        .fg(Color::Rgb(192, 202, 245))
                        .add_modifier(Modifier::BOLD),
                    Style::default().bg(bar_bg).fg(Color::Rgb(56, 64, 94)),
                    Style::default().bg(bar_bg).fg(Color::Rgb(132, 142, 178)),
                    Style::default().bg(bar_bg).fg(Color::Rgb(86, 94, 126)),
                )
            };

            for (idx, m) in quota.metrics.iter().enumerate() {
                if x >= avail_end {
                    break;
                }
                if idx > 0 {
                    push_clamped_str(buf, &mut x, rect.y, avail_end, sep, sep_style);
                }
                push_clamped_str(buf, &mut x, rect.y, avail_end, &m.window_label, lbl_style);
                push_clamped_str(buf, &mut x, rect.y, avail_end, " ", base_style);

                let (fill_style, pct_style, pct_suffix) = if eink_mode {
                    let low = m.remaining_percent <= 15.0;
                    (
                        Style::default().bg(bar_bg).fg(EINK_INK),
                        if low {
                            Style::default()
                                .bg(EINK_INK)
                                .fg(EINK_PAPER)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                                .bg(bar_bg)
                                .fg(EINK_INK)
                                .add_modifier(Modifier::BOLD)
                        },
                        if low { "!" } else { "" },
                    )
                } else {
                    let u_col = quota_remaining_color(m.remaining_percent);
                    (
                        Style::default().bg(bar_bg).fg(u_col),
                        Style::default()
                            .bg(bar_bg)
                            .fg(u_col)
                            .add_modifier(Modifier::BOLD),
                        "",
                    )
                };

                if bar_w > 0 {
                    if eink_mode {
                        let bar = render_eink_progress_bar(m.remaining_percent, bar_w);
                        let filled_part: String = bar.chars().take_while(|&c| c != '─').collect();
                        let empty_part: String = bar.chars().skip_while(|&c| c != '─').collect();
                        push_clamped_str(buf, &mut x, rect.y, avail_end, &filled_part, fill_style);
                        push_clamped_str(buf, &mut x, rect.y, avail_end, &empty_part, track_style);
                    } else {
                        let bar = render_progress_bar(m.remaining_percent, bar_w);
                        let filled_part: String = bar.chars().take_while(|&c| c != '░').collect();
                        let empty_part: String = bar.chars().skip_while(|&c| c != '░').collect();
                        push_clamped_str(buf, &mut x, rect.y, avail_end, &filled_part, fill_style);
                        push_clamped_str(buf, &mut x, rect.y, avail_end, &empty_part, track_style);
                    }
                    push_clamped_str(buf, &mut x, rect.y, avail_end, " ", base_style);
                }

                let pct = m.remaining_percent.clamp(0.0, 100.0).round() as u64;
                let pct_str = format!("{pct}%{pct_suffix}");
                push_clamped_str(buf, &mut x, rect.y, avail_end, &pct_str, pct_style);

                if show_detail {
                    if let Some(ref d) = m.detail {
                        let d_str = format!(" ({d})");
                        push_clamped_str(buf, &mut x, rect.y, avail_end, &d_str, detail_style);
                    }
                }
            }
        }
    }

    let mut rx = avail_end;
    if let Some(clbl) = copied_lbl {
        let cw = clbl.width() as u16;
        if rx + cw <= max_x {
            buf.set_string(
                rx,
                rect.y,
                clbl,
                if eink_mode {
                    Style::default()
                        .bg(EINK_INK)
                        .fg(EINK_PAPER)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .bg(Color::Rgb(158, 206, 106))
                        .fg(Color::Rgb(20, 22, 32))
                        .add_modifier(Modifier::BOLD)
                },
            );
            rx += cw;
        }
    }
    if show_pg_btns {
        if copied_lbl.is_some() && rx < max_x {
            buf.set_string(rx, rect.y, " ", base_style);
            rx += 1;
        }
        let pg_style = Style::default()
            .bg(EINK_SURFACE)
            .fg(EINK_INK)
            .add_modifier(Modifier::BOLD);
        if rx + pg_up_w <= max_x {
            buf.set_string(rx, rect.y, pg_up_lbl, pg_style);
            hits.push(HitRegion {
                rect: Rect {
                    x: rx,
                    y: rect.y,
                    width: pg_up_w,
                    height: rect.height,
                },
                target: HitTarget::TriggerAction(Action::PageUp),
            });
            rx += pg_up_w;
        }
        if rx < max_x {
            buf.set_string(rx, rect.y, " ", base_style);
            rx += 1;
        }
        if rx + pg_dn_w <= max_x {
            buf.set_string(rx, rect.y, pg_dn_lbl, pg_style);
            hits.push(HitRegion {
                rect: Rect {
                    x: rx,
                    y: rect.y,
                    width: pg_dn_w,
                    height: rect.height,
                },
                target: HitTarget::TriggerAction(Action::PageDown),
            });
            rx += pg_dn_w;
        }
        if scroll_lbl.is_some() && rx < max_x {
            buf.set_string(rx, rect.y, " ", base_style);
            rx += 1;
        }
    }
    if let Some(slbl) = scroll_lbl {
        let sw = slbl.width() as u16;
        if rx + sw <= max_x {
            buf.set_string(
                rx,
                rect.y,
                &slbl,
                if eink_mode {
                    Style::default()
                        .bg(EINK_INK)
                        .fg(EINK_PAPER)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .bg(Color::Rgb(125, 207, 255))
                        .fg(Color::Rgb(20, 22, 32))
                        .add_modifier(Modifier::BOLD)
                },
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
    eink_mode: bool,
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
            let (cell_bg, eink_fg) = if eink_mode {
                let inverted = dragging || sector_active;
                if inverted {
                    (EINK_INK, EINK_PAPER)
                } else {
                    (EINK_PAPER, EINK_INK)
                }
            } else {
                let base_bg = color_to_rgb(cell.bg, (18, 20, 30));
                let (tint, alpha) = if sector_active {
                    (active_tint, active_alpha)
                } else if is_center {
                    (center_tint, center_alpha)
                } else {
                    (base_tint, base_alpha)
                };
                (blend_rgb(base_bg, tint, alpha), EINK_INK)
            };

            let (b_fg, k_fg, ctl_col, pfx_col, alt_col) = if eink_mode {
                (eink_fg, eink_fg, eink_fg, eink_fg, eink_fg)
            } else {
                (
                    border_fg,
                    key_fg,
                    if ctrl_active { active_fg } else { mod_fg },
                    if prefix_active { active_fg } else { mod_fg },
                    if alt_active { active_fg } else { mod_fg },
                )
            };

            let glyph: Option<(&str, Color, bool)> = if full_15x7 {
                match (dy, dx) {
                    // Row 0: top of Esc box
                    (0, 5) => Some(("╭", b_fg, false)),
                    (0, 6..=8) => Some(("─", b_fg, false)),
                    (0, 9) => Some(("╮", b_fg, false)),
                    // Row 1: Esc label
                    (1, 5) | (1, 9) => Some(("│", b_fg, false)),
                    (1, 6) => Some(("E", k_fg, true)),
                    (1, 7) => Some(("s", k_fg, true)),
                    (1, 8) => Some(("c", k_fg, true)),
                    // Row 2: top of Left / Center / Right
                    (2, 0) | (2, 10) => Some(("╭", b_fg, false)),
                    (2, 4) | (2, 14) => Some(("╮", b_fg, false)),
                    (2, 5) => Some(("├", b_fg, false)),
                    (2, 9) => Some(("┤", b_fg, false)),
                    (2, 1..=3) | (2, 6..=8) | (2, 11..=13) => Some(("─", b_fg, false)),
                    // Row 3: Ctl / ^B / Alt labels
                    (3, 0) | (3, 4) | (3, 5) | (3, 9) | (3, 10) | (3, 14) => {
                        Some(("│", b_fg, false))
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
                    (4, 0) | (4, 10) => Some(("╰", b_fg, false)),
                    (4, 4) | (4, 14) => Some(("╯", b_fg, false)),
                    (4, 5) => Some(("├", b_fg, false)),
                    (4, 9) => Some(("┤", b_fg, false)),
                    (4, 1..=3) | (4, 6..=8) | (4, 11..=13) => Some(("─", b_fg, false)),
                    // Row 5: Tab label
                    (5, 5) | (5, 9) => Some(("│", b_fg, false)),
                    (5, 6) => Some(("T", k_fg, true)),
                    (5, 7) => Some(("a", k_fg, true)),
                    (5, 8) => Some(("b", k_fg, true)),
                    // Row 6: bottom of Tab box
                    (6, 5) => Some(("╰", b_fg, false)),
                    (6, 6..=8) => Some(("─", b_fg, false)),
                    (6, 9) => Some(("╯", b_fg, false)),
                    _ => None,
                }
            } else if dy == up_label_y && dx == center_label_x {
                Some(("E", k_fg, true))
            } else if dy == down_label_y && dx == center_label_x {
                Some(("T", k_fg, true))
            } else if dy == mid_label_y && dx == left_label_x {
                Some(("C", ctl_col, true))
            } else if dy == mid_label_y && dx == center_label_x {
                Some(("B", pfx_col, true))
            } else if dy == mid_label_y && dx == right_label_x {
                Some(("A", alt_col, true))
            } else {
                None
            };

            let base_fg = color_to_rgb(cell.fg, (169, 177, 214));
            let (tint, _) = if sector_active {
                (active_tint, active_alpha)
            } else if is_center {
                (center_tint, center_alpha)
            } else {
                (base_tint, base_alpha)
            };
            cell.set_bg(cell_bg);
            if let Some((sym, fg, bold)) = glyph {
                cell.set_symbol(sym);
                cell.set_fg(fg);
                if bold {
                    cell.modifier.insert(Modifier::BOLD);
                } else {
                    cell.modifier.remove(Modifier::BOLD);
                }
            } else if eink_mode {
                cell.set_symbol(" ");
                cell.set_fg(eink_fg);
                cell.modifier.remove(Modifier::BOLD);
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
    eink_mode: bool,
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
            let (cell_bg, eink_fg) = if eink_mode {
                if dragging {
                    (EINK_INK, EINK_PAPER)
                } else {
                    (EINK_PAPER, EINK_INK)
                }
            } else {
                (blend_rgb(base_bg, tint, alpha), EINK_INK)
            };
            let (b_fg, a_fg, p_fg) = if eink_mode {
                (eink_fg, eink_fg, eink_fg)
            } else {
                (border_fg, arrow_fg, paste_fg)
            };

            let glyph: Option<(&str, Color, bool)> = if full_15x7 {
                match (dy, dx) {
                    // Row 0: top of Up box
                    (0, 5) => Some(("╭", b_fg, false)),
                    (0, 6..=8) => Some(("─", b_fg, false)),
                    (0, 9) => Some(("╮", b_fg, false)),
                    // Row 1: Up label
                    (1, 5) | (1, 9) => Some(("│", b_fg, false)),
                    (1, 7) => Some(("↑", a_fg, true)),
                    // Row 2: top of Left / Center / Right
                    (2, 0) | (2, 10) => Some(("╭", b_fg, false)),
                    (2, 4) | (2, 14) => Some(("╮", b_fg, false)),
                    (2, 5) => Some(("├", b_fg, false)),
                    (2, 9) => Some(("┤", b_fg, false)),
                    (2, 1..=3) | (2, 6..=8) | (2, 11..=13) => Some(("─", b_fg, false)),
                    // Row 3: Left / Paste / Right labels
                    (3, 0) | (3, 4) | (3, 5) | (3, 9) | (3, 10) | (3, 14) => {
                        Some(("│", b_fg, false))
                    }
                    (3, 2) => Some(("←", a_fg, true)),
                    (3, 7) => Some(("⎘", p_fg, true)),
                    (3, 12) => Some(("→", a_fg, true)),
                    // Row 4: bottom of Left / Center / Right
                    (4, 0) | (4, 10) => Some(("╰", b_fg, false)),
                    (4, 4) | (4, 14) => Some(("╯", b_fg, false)),
                    (4, 5) => Some(("├", b_fg, false)),
                    (4, 9) => Some(("┤", b_fg, false)),
                    (4, 1..=3) | (4, 6..=8) | (4, 11..=13) => Some(("─", b_fg, false)),
                    // Row 5: Down label
                    (5, 5) | (5, 9) => Some(("│", b_fg, false)),
                    (5, 7) => Some(("↓", a_fg, true)),
                    // Row 6: bottom of Down box
                    (6, 5) => Some(("╰", b_fg, false)),
                    (6, 6..=8) => Some(("─", b_fg, false)),
                    (6, 9) => Some(("╯", b_fg, false)),
                    _ => None,
                }
            } else if dy == up_label_y && dx == center_label_x {
                Some(("↑", a_fg, true))
            } else if dy == down_label_y && dx == center_label_x {
                Some(("↓", a_fg, true))
            } else if dy == mid_label_y && dx == left_label_x {
                Some(("←", a_fg, true))
            } else if dy == mid_label_y && dx == center_label_x {
                Some(("⎘", p_fg, true))
            } else if dy == mid_label_y && dx == right_label_x {
                Some(("→", a_fg, true))
            } else {
                None
            };

            cell.set_bg(cell_bg);
            if let Some((sym, fg, bold)) = glyph {
                cell.set_symbol(sym);
                cell.set_fg(fg);
                if bold {
                    cell.modifier.insert(Modifier::BOLD);
                } else {
                    cell.modifier.remove(Modifier::BOLD);
                }
            } else if eink_mode {
                cell.set_symbol(" ");
                cell.set_fg(eink_fg);
                cell.modifier.remove(Modifier::BOLD);
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
    eink_mode: bool,
    hits: &mut Vec<HitRegion>,
) {
    hits.clear();
    let space_cnt = world.doc.spaces.len().max(1) as u16;
    let desired_h = (space_cnt * 2 + 5).max(7);
    let pop_y = (area.y + top_h).min(area.y + area.height.saturating_sub(3));
    let avail_h = (area.y + area.height).saturating_sub(pop_y);
    let pop_h = desired_h.min(avail_h).max(3.min(avail_h));
    let pop_w = 40u16
        .min(area.width.saturating_sub(2))
        .max(12.min(area.width));
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
        .border_style(Style::default().fg(if eink_mode { EINK_INK } else { HERDR_ACCENT }))
        .style(if eink_mode {
            Style::default().bg(EINK_PAPER).fg(EINK_INK)
        } else {
            Style::default().bg(HERDR_PANEL_BG).fg(HERDR_TEXT)
        });
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let buf = f.buffer_mut();
    draw_modal_close_button(buf, popup, eink_mode, hits);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let max_y = inner.y + inner.height;
    let footer_h: u16 = if inner.height >= 4 { 2 } else { 1 };
    let list_end_y = max_y.saturating_sub(footer_h);
    let w = inner.width as usize;

    let total_spaces = world.doc.spaces.len();
    let use_two_row = list_end_y.saturating_sub(inner.y) >= 2;
    let row_step: usize = if use_two_row { 2 } else { 1 };
    let visible_spaces = (list_end_y.saturating_sub(inner.y) as usize / row_step).max(1);
    let spaces_overflow = total_spaces > visible_spaces;
    let lookahead_si = if visible_spaces >= 2 {
        (world.active_space_idx + 1).min(total_spaces.saturating_sub(1))
    } else {
        world.active_space_idx
    };
    let start_si = if lookahead_si < visible_spaces {
        0
    } else {
        lookahead_si + 1 - visible_spaces
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

        let (r1_style, r2_style) = if eink_mode {
            if is_active {
                (
                    Style::default()
                        .bg(EINK_INK)
                        .fg(EINK_PAPER)
                        .add_modifier(Modifier::BOLD),
                    Style::default().bg(EINK_INK).fg(EINK_PAPER),
                )
            } else {
                (
                    Style::default()
                        .bg(EINK_PAPER)
                        .fg(EINK_INK)
                        .add_modifier(Modifier::BOLD),
                    Style::default().bg(EINK_PAPER).fg(EINK_INK_MUTED),
                )
            }
        } else if is_active {
            (
                Style::default()
                    .bg(HERDR_SURFACE0)
                    .fg(HERDR_TEXT)
                    .add_modifier(Modifier::BOLD),
                Style::default().bg(HERDR_SURFACE0).fg(HERDR_SUBTEXT0),
            )
        } else {
            (
                Style::default()
                    .bg(HERDR_SURFACE_DIM)
                    .fg(HERDR_SUBTEXT0)
                    .add_modifier(Modifier::BOLD),
                Style::default().bg(HERDR_SURFACE_DIM).fg(HERDR_OVERLAY1),
            )
        };

        buf.set_string(inner.x, y, pad_to_width(&row1, w), r1_style);
        if use_two_row {
            buf.set_string(inner.x, y + 1, pad_to_width(&row2, w), r2_style);
        }
        if is_active {
            let accent = if eink_mode { EINK_PAPER } else { HERDR_ACCENT };
            buf.set_string(inner.x, y, "▍", r1_style.fg(accent));
            if use_two_row {
                buf.set_string(inner.x, y + 1, "▍", r2_style.fg(accent));
            }
        }
        if inner.x + dot_x < inner.x + inner.width {
            let sym_fg = if eink_mode {
                if is_active {
                    EINK_PAPER
                } else {
                    EINK_INK
                }
            } else {
                state_color(sstate)
            };
            buf.set_string(
                inner.x + dot_x,
                y,
                sstate.symbol(),
                r1_style.fg(sym_fg).add_modifier(Modifier::BOLD),
            );
        }

        let target_space_id = if is_active && spaces_overflow && total_spaces > 1 {
            world.doc.spaces[(si + 1) % total_spaces].id.clone()
        } else {
            space.id.clone()
        };
        hits.push(HitRegion {
            rect: Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: row_step as u16,
            },
            target: HitTarget::SelectSpace(target_space_id),
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
        if eink_mode {
            Style::default()
                .bg(EINK_SURFACE)
                .fg(EINK_INK)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .bg(Color::Rgb(26, 44, 64))
                .fg(Color::Rgb(125, 207, 255))
                .add_modifier(Modifier::BOLD)
        },
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

#[allow(clippy::too_many_arguments)]
fn render_tab_overview_page(
    f: &mut Frame,
    term_rect: Rect,
    world: &World,
    query: &str,
    selected: usize,
    selected_target: &OverviewSelection,
    eink_mode: bool,
    hits: &mut Vec<HitRegion>,
) {
    if term_rect.width == 0 || term_rect.height == 0 {
        return;
    }

    let page_bg = if eink_mode {
        EINK_PAPER
    } else {
        HERDR_SURFACE_DIM
    };
    let page_fg = if eink_mode { EINK_INK } else { HERDR_TEXT };
    let input_bg = if eink_mode {
        EINK_SURFACE
    } else {
        HERDR_PANEL_BG
    };

    // Do NOT clear `hits`: Top Bar (`[ ⏏ ]`, `[ ▾ Space ]`, tab pills, `[ + ]`, `[ ? ]`)
    // remains live above `term_rect`. Push `ModalNoop` on `term_rect` first so item rows pushed
    // afterward take priority in `find_hit_target`.
    f.render_widget(
        Block::default().style(Style::default().bg(page_bg).fg(page_fg)),
        term_rect,
    );
    hits.push(HitRegion {
        rect: term_rect,
        target: HitTarget::ModalNoop,
    });

    let col_w = if term_rect.width >= 80 {
        term_rect.width.saturating_sub(8).min(78)
    } else if term_rect.width >= 24 {
        term_rect.width.saturating_sub(2)
    } else {
        term_rect.width
    };
    let col_x = term_rect.x + (term_rect.width.saturating_sub(col_w)) / 2;
    let pad_v = u16::from(term_rect.height >= 12);
    let content_y = term_rect.y + pad_v;
    let content_end_y = (term_rect.y + term_rect.height).saturating_sub(pad_v);
    let avail_h = content_end_y.saturating_sub(content_y);
    let cw = col_w as usize;
    if cw == 0 || avail_h == 0 {
        return;
    }

    let items = collect_overview_items(world, query);
    let sel = resolve_overview_selection(&items, selected, selected_target);
    let (space_name, total_tabs) = world
        .active_space()
        .map(|s| (world.space_display_name(s), s.tabs.len()))
        .unwrap_or_else(|| ("space".to_string(), 0));
    let buf = f.buffer_mut();

    let (list_y, list_end_y) = if avail_h >= 8 {
        let tab_word = if total_tabs == 1 { "tab" } else { "tabs" };
        let left_hdr = format!(" ❖ {space_name} ({total_tabs} {tab_word})");
        let right_hdr = "← swipe / Esc back ";
        buf.set_string(
            col_x,
            content_y,
            pad_to_width(&left_hdr, cw),
            Style::default()
                .bg(page_bg)
                .fg(if eink_mode { EINK_INK } else { HERDR_MAUVE })
                .add_modifier(Modifier::BOLD),
        );
        if left_hdr.width() + 2 + right_hdr.width() <= cw {
            let rx = col_x + col_w - right_hdr.width() as u16;
            buf.set_string(
                rx,
                content_y,
                right_hdr,
                Style::default().bg(page_bg).fg(if eink_mode {
                    EINK_INK_MUTED
                } else {
                    HERDR_OVERLAY0
                }),
            );
        }

        let filter_y = content_y + 1;
        buf.set_string(
            col_x,
            filter_y,
            " ".repeat(cw),
            Style::default().bg(input_bg).fg(page_fg),
        );
        let prompt_prefix = truncate_to_width(" › ", cw);
        let pw = prompt_prefix.width() as u16;
        if pw > 0 {
            buf.set_string(
                col_x,
                filter_y,
                &prompt_prefix,
                Style::default()
                    .bg(input_bg)
                    .fg(if eink_mode { EINK_INK } else { HERDR_ACCENT })
                    .add_modifier(Modifier::BOLD),
            );
        }
        let rem_w = cw.saturating_sub(pw as usize);
        if rem_w > 0 {
            let (input_txt, input_fg) = if query.is_empty() {
                (
                    "Filter tabs, or Enter for + New Tab…".to_string(),
                    if eink_mode {
                        EINK_INK_MUTED
                    } else {
                        HERDR_OVERLAY1
                    },
                )
            } else {
                (format!("{query}▏"), page_fg)
            };
            buf.set_string(
                col_x + pw,
                filter_y,
                truncate_to_width(&input_txt, rem_w),
                Style::default().bg(input_bg).fg(input_fg),
            );
        }

        let hint_y = content_end_y - 1;
        let hint_txt = truncate_to_width(
            " ↑/↓ select · Enter open · Type filter · ← swipe / Esc exit",
            cw,
        );
        buf.set_string(
            col_x,
            hint_y,
            &hint_txt,
            Style::default().bg(page_bg).fg(if eink_mode {
                EINK_INK_MUTED
            } else {
                HERDR_OVERLAY0
            }),
        );
        (content_y + 3, hint_y)
    } else if avail_h >= 4 {
        let bar_txt = if query.is_empty() {
            format!(" ❖ {space_name} · › type to filter…")
        } else {
            format!(" ❖ {space_name} · › {query}▏")
        };
        let fg = if eink_mode {
            if query.is_empty() {
                EINK_INK_MUTED
            } else {
                EINK_INK
            }
        } else if query.is_empty() {
            HERDR_SUBTEXT0
        } else {
            HERDR_TEXT
        };
        buf.set_string(
            col_x,
            content_y,
            pad_to_width(&bar_txt, cw),
            Style::default().bg(input_bg).fg(fg),
        );
        (content_y + 1, content_end_y)
    } else {
        (content_y, content_end_y)
    };

    let list_h = list_end_y.saturating_sub(list_y);
    if list_h == 0 {
        return;
    }

    let use_two_row = list_h >= 6;
    let row_step: usize = if use_two_row { 2 } else { 1 };
    let visible_items = ((list_h as usize) / row_step).max(1);
    let lookahead_idx = if visible_items >= 2 {
        (sel + 1).min(items.len().saturating_sub(1))
    } else {
        sel
    };
    let start_idx = if lookahead_idx < visible_items {
        0
    } else {
        lookahead_idx + 1 - visible_items
    };

    let mut y = list_y;
    for (idx, item) in items.iter().enumerate().skip(start_idx) {
        if y + (row_step as u16) > list_end_y {
            break;
        }
        let is_sel = idx == sel;
        let row_bg = if eink_mode {
            if is_sel {
                EINK_INK
            } else {
                EINK_PAPER
            }
        } else if is_sel {
            HERDR_SURFACE0
        } else {
            HERDR_SURFACE_DIM
        };
        let marker = if is_sel { "▸" } else { " " };

        match item {
            OverviewItem::NewTab { cwd } => {
                let r1_title = format!(" {marker} + New Tab");
                let r1_fg = if eink_mode {
                    if is_sel {
                        EINK_PAPER
                    } else {
                        EINK_INK
                    }
                } else {
                    HERDR_GREEN
                };
                let r1_style = Style::default()
                    .bg(row_bg)
                    .fg(r1_fg)
                    .add_modifier(Modifier::BOLD);
                let r2_fg = if eink_mode {
                    if is_sel {
                        EINK_PAPER
                    } else {
                        EINK_INK_MUTED
                    }
                } else if is_sel {
                    HERDR_SUBTEXT0
                } else {
                    HERDR_OVERLAY1
                };
                let r2_style = Style::default().bg(row_bg).fg(r2_fg);
                buf.set_string(col_x, y, pad_to_width(&r1_title, cw), r1_style);
                let hint = "Enter / ^b c ";
                if cw >= 32 && r1_title.width() + 2 + hint.width() <= cw {
                    let hx = col_x + col_w - hint.width() as u16;
                    buf.set_string(hx, y, hint, r2_style);
                }
                if is_sel {
                    buf.set_string(col_x, y, "▍", r1_style);
                }
                if use_two_row {
                    let short_cwd = shorten_home_path(cwd);
                    let r2_text = format!("     new…  {short_cwd}");
                    buf.set_string(col_x, y + 1, pad_to_width(&r2_text, cw), r2_style);
                    if is_sel {
                        buf.set_string(col_x, y + 1, "▍", r2_style.fg(r1_fg));
                    }
                }
                hits.push(HitRegion {
                    rect: Rect {
                        x: col_x,
                        y,
                        width: col_w,
                        height: row_step as u16,
                    },
                    target: HitTarget::NewTab,
                });
            }
            OverviewItem::Tab {
                space_id,
                tab_id,
                tab_number,
                tab_name,
                term_addr,
                state,
                cwd,
                detail,
            } => {
                let prefix = format!(" {marker} ");
                let sym_x = prefix.width() as u16;
                let sym = state.symbol();
                let addr_part = if term_addr.is_empty() {
                    String::new()
                } else {
                    format!(" ({term_addr})")
                };
                let r1_text = format!("{prefix}{sym} {tab_number}:{tab_name}{addr_part}");
                let (r1_style, r2_style) = if eink_mode {
                    if is_sel {
                        (
                            Style::default()
                                .bg(EINK_INK)
                                .fg(EINK_PAPER)
                                .add_modifier(Modifier::BOLD),
                            Style::default().bg(EINK_INK).fg(EINK_PAPER),
                        )
                    } else {
                        (
                            Style::default().bg(EINK_PAPER).fg(EINK_INK),
                            Style::default().bg(EINK_PAPER).fg(EINK_INK_MUTED),
                        )
                    }
                } else if is_sel {
                    (
                        Style::default()
                            .bg(HERDR_SURFACE0)
                            .fg(HERDR_TEXT)
                            .add_modifier(Modifier::BOLD),
                        Style::default().bg(HERDR_SURFACE0).fg(HERDR_SUBTEXT0),
                    )
                } else {
                    (
                        Style::default().bg(HERDR_SURFACE_DIM).fg(HERDR_SUBTEXT0),
                        Style::default().bg(HERDR_SURFACE_DIM).fg(HERDR_OVERLAY1),
                    )
                };
                let st_fg = if eink_mode {
                    if is_sel {
                        EINK_PAPER
                    } else {
                        EINK_INK
                    }
                } else {
                    state_color(*state)
                };
                let accent_fg = if eink_mode { EINK_PAPER } else { HERDR_ACCENT };
                buf.set_string(col_x, y, pad_to_width(&r1_text, cw), r1_style);
                if sym_x + (sym.width() as u16) <= col_w {
                    buf.set_string(
                        col_x + sym_x,
                        y,
                        sym,
                        r1_style.fg(st_fg).add_modifier(Modifier::BOLD),
                    );
                }
                if cw >= 40
                    && matches!(
                        state,
                        AgentState::Blocked | AgentState::Done | AgentState::Working
                    )
                {
                    let badge = format!(" {} ", state.label());
                    if r1_text.width() + 2 + badge.width() <= cw {
                        let bx = col_x + col_w - badge.width() as u16;
                        buf.set_string(
                            bx,
                            y,
                            &badge,
                            Style::default()
                                .bg(row_bg)
                                .fg(st_fg)
                                .add_modifier(Modifier::BOLD),
                        );
                    }
                }
                if is_sel {
                    buf.set_string(col_x, y, "▍", r1_style.fg(accent_fg));
                }
                if use_two_row {
                    let short_cwd = shorten_home_path(cwd);
                    let r2_text = if detail.is_empty() {
                        format!("     {short_cwd}")
                    } else {
                        format!("     {short_cwd} · {detail}")
                    };
                    buf.set_string(col_x, y + 1, pad_to_width(&r2_text, cw), r2_style);
                    if is_sel {
                        buf.set_string(col_x, y + 1, "▍", r2_style.fg(accent_fg));
                    }
                }
                hits.push(HitRegion {
                    rect: Rect {
                        x: col_x,
                        y,
                        width: col_w,
                        height: row_step as u16,
                    },
                    target: HitTarget::SelectTab {
                        space_id: space_id.clone(),
                        tab_id: tab_id.clone(),
                    },
                });
            }
        }
        y += row_step as u16;
    }

    if !query.trim().is_empty() && items.len() == 1 && y < list_end_y {
        buf.set_string(
            col_x,
            y,
            pad_to_width("   No matching tabs in this Space", cw),
            Style::default().bg(page_bg).fg(if eink_mode {
                EINK_INK_MUTED
            } else {
                HERDR_OVERLAY1
            }),
        );
    }
}

struct VtScreenWidget<'a> {
    screen: &'a vt100::Screen,
    selection: Option<&'a TerminalSelection>,
    eink_mode: bool,
}

fn is_braille_char(s: &str) -> bool {
    let mut chars = s.chars();
    matches!((chars.next(), chars.next()), (Some(ch), None) if ('\u{2800}'..='\u{28FF}').contains(&ch))
}

fn resolve_eink_cell_symbol<'a>(
    screen: &vt100::Screen,
    r: u16,
    c: u16,
    rows: u16,
    cols: u16,
    raw_sym: &'a str,
) -> &'a str {
    let mut chars = raw_sym.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        if ('\u{2801}'..='\u{28FF}').contains(&ch) {
            let left_braille = c > 0
                && screen
                    .cell(r, c - 1)
                    .is_some_and(|lc| is_braille_char(lc.contents()));
            let right_braille = c + 1 < cols
                && screen
                    .cell(r, c + 1)
                    .is_some_and(|rc| is_braille_char(rc.contents()));
            let up_braille = r > 0
                && screen
                    .cell(r - 1, c)
                    .is_some_and(|uc| is_braille_char(uc.contents()));
            let down_braille = r + 1 < rows
                && screen
                    .cell(r + 1, c)
                    .is_some_and(|dc| is_braille_char(dc.contents()));
            if !left_braille && !right_braille && !up_braille && !down_braille {
                return "◈";
            }
        } else if matches!(ch, '◐' | '◓' | '◑' | '◒' | '◴' | '◷' | '◶' | '◵') {
            return "◈";
        }
    }
    raw_sym
}

fn xterm256_rgb(idx: u8) -> (u8, u8, u8) {
    match idx {
        0 => (0, 0, 0),
        1 => (128, 0, 0),
        2 => (0, 128, 0),
        3 => (128, 128, 0),
        4 => (0, 0, 128),
        5 => (128, 0, 128),
        6 => (0, 128, 128),
        7 => (192, 192, 192),
        8 => (128, 128, 128),
        9 => (255, 0, 0),
        10 => (0, 255, 0),
        11 => (255, 255, 0),
        12 => (0, 0, 255),
        13 => (255, 0, 255),
        14 => (0, 255, 255),
        15 => (255, 255, 255),
        16..=231 => {
            const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            let v = idx - 16;
            let r = v / 36;
            let g = (v % 36) / 6;
            let b = v % 6;
            (STEPS[r as usize], STEPS[g as usize], STEPS[b as usize])
        }
        232..=255 => {
            let gray = 8 + (idx - 232) * 10;
            (gray, gray, gray)
        }
    }
}

fn vt_color_rgb(c: vt100::Color) -> Option<(u8, u8, u8)> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(xterm256_rgb(i)),
        vt100::Color::Rgb(r, g, b) => Some((r, g, b)),
    }
}

fn rgb_luma(r: u8, g: u8, b: u8) -> u8 {
    ((299u32 * u32::from(r) + 587u32 * u32::from(g) + 114u32 * u32::from(b)) / 1000) as u8
}

fn resolve_eink_vt_colors_and_mods(cell: &vt100::Cell, selected: bool) -> (Color, Color, Modifier) {
    if selected || cell.inverse() {
        return (EINK_PAPER, EINK_INK, Modifier::BOLD);
    }
    let bg_col = match vt_color_rgb(cell.bgcolor()) {
        None => EINK_PAPER,
        Some((r, g, b)) => match rgb_luma(r, g, b) {
            0..=35 => EINK_PAPER,
            36..=95 => EINK_SURFACE,
            96..=224 => EINK_PILL_BG,
            _ => EINK_PAPER,
        },
    };
    let fg_col = if cell.dim() {
        EINK_INK_MUTED
    } else {
        match cell.fgcolor() {
            vt100::Color::Default => EINK_INK,
            c => match vt_color_rgb(c) {
                Some((r, g, b)) => {
                    let luma = rgb_luma(r, g, b);
                    let chroma = r.max(g).max(b) - r.min(g).min(b);
                    if matches!(c, vt100::Color::Idx(8 | 238..=249))
                        || (chroma <= 18 && (70..=180).contains(&luma))
                    {
                        EINK_INK_MUTED
                    } else {
                        EINK_INK
                    }
                }
                None => EINK_INK,
            },
        }
    };
    let mut mods = Modifier::empty();
    if cell.bold() {
        mods |= Modifier::BOLD;
    }
    if cell.italic() {
        mods |= Modifier::ITALIC;
    }
    if cell.underline() {
        mods |= Modifier::UNDERLINED;
    }
    (fg_col, bg_col, mods)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EinkSnapSym {
    Inline { bytes: [u8; 15], len: u8 },
    Hashed(u64),
}

impl EinkSnapSym {
    fn from_str(s: &str) -> Self {
        let b = s.as_bytes();
        if b.len() <= 15 {
            let mut bytes = [0u8; 15];
            bytes[..b.len()].copy_from_slice(b);
            Self::Inline {
                bytes,
                len: b.len() as u8,
            }
        } else {
            let mut h: u64 = 0xcbf29ce484222325;
            for &byte in b {
                h ^= u64::from(byte);
                h = h.wrapping_mul(0x100000001b3);
            }
            Self::Hashed(h)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EinkSnapCell {
    sym: EinkSnapSym,
    fg: Color,
    bg: Color,
    mods: Modifier,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EinkScreenSnapshot {
    rows: u16,
    cols: u16,
    scrollback_offset: usize,
    cursor: Option<(u16, u16)>,
    cells: Vec<EinkSnapCell>,
}

fn normalized_narrow_vt_symbol<'a>(
    screen: &vt100::Screen,
    r: u16,
    c: u16,
    rows: u16,
    cols: u16,
    raw_sym: &'a str,
    eink_mode: bool,
) -> &'a str {
    let effective = if eink_mode {
        resolve_eink_cell_symbol(screen, r, c, rows, cols, raw_sym)
    } else {
        raw_sym
    };
    if effective.is_empty() || effective == " " {
        " "
    } else if (effective.len() == 1 && effective.as_bytes()[0].is_ascii_graphic())
        || effective.width() == 1
    {
        effective
    } else {
        " "
    }
}

pub fn capture_eink_screen_snapshot(
    screen: &vt100::Screen,
    area: Rect,
    scrollback_offset: usize,
) -> EinkScreenSnapshot {
    let (scr_cols, scr_rows) = {
        let (r, c) = screen.size();
        (c.get(), r.get())
    };
    let (rows, cols) = vt_render_dims((scr_cols, scr_rows), area);
    let mut cells = Vec::with_capacity(usize::from(rows) * usize::from(cols));
    for r in 0..rows {
        let mut c = 0;
        while c < cols {
            let Some(cell) = screen.cell(r, c) else {
                c += 1;
                continue;
            };
            if cell.is_wide_continuation() {
                c += 1;
                continue;
            }
            let (fg, bg, mods) = resolve_eink_vt_colors_and_mods(cell, false);
            let raw_sym = if cell.has_contents() {
                cell.contents()
            } else {
                ""
            };
            if cell.is_wide() {
                if c + 1 >= cols {
                    cells.push(EinkSnapCell {
                        sym: EinkSnapSym::from_str(" "),
                        fg,
                        bg,
                        mods,
                    });
                    c += 1;
                } else {
                    let sym = if raw_sym.width() == 2 { raw_sym } else { " " };
                    cells.push(EinkSnapCell {
                        sym: EinkSnapSym::from_str(sym),
                        fg,
                        bg,
                        mods,
                    });
                    cells.push(EinkSnapCell {
                        sym: EinkSnapSym::from_str(""),
                        fg,
                        bg,
                        mods,
                    });
                    c += 2;
                }
            } else {
                let sym = normalized_narrow_vt_symbol(screen, r, c, rows, cols, raw_sym, true);
                cells.push(EinkSnapCell {
                    sym: EinkSnapSym::from_str(sym),
                    fg,
                    bg,
                    mods,
                });
                c += 1;
            }
        }
    }
    let cursor =
        (scrollback_offset == 0 && !screen.hide_cursor() && rows > 0 && cols > 0).then(|| {
            let (cr, cc) = screen.cursor_position();
            (cr.min(rows - 1), cc.min(cols - 1))
        });
    EinkScreenSnapshot {
        rows,
        cols,
        scrollback_offset,
        cursor,
        cells,
    }
}

/// Resolve how many rows and columns of the vt100 screen to paint into `area`.
///
/// The screen and the viewport are expected to agree: `term_rect` is what
/// `sync_attached_terminals` hands to the core channel as the pty size, so a
/// disagreement means a frame was drawn between the viewport changing and the
/// channel resize landing.
///
/// Clamping is safe in that window because `ratatui` resets the back buffer on
/// every `swap_buffers` and clears the region on `autoresize`, so cells this
/// function declines to paint are already blank rather than stale. What the
/// clamp must not do is paint outside `area`, which would corrupt the tab bar or
/// the status bar.
///
/// Returns `(rows, cols)`.
fn vt_render_dims(screen: (u16, u16), area: Rect) -> (u16, u16) {
    let (scr_cols, scr_rows) = screen;
    (area.height.min(scr_rows), area.width.min(scr_cols))
}

impl Widget for VtScreenWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if self.eink_mode {
            for y in area.y..area.y.saturating_add(area.height) {
                for x in area.x..area.x.saturating_add(area.width) {
                    if let Some(dst) = buf.cell_mut((x, y)) {
                        dst.reset();
                        dst.set_symbol(" ");
                        dst.set_bg(EINK_PAPER);
                        dst.set_fg(EINK_INK);
                    }
                }
            }
        }
        let (scr_cols, scr_rows) = {
            let (r, c) = self.screen.size();
            (c.get(), r.get())
        };
        let (rows, cols) = vt_render_dims((scr_cols, scr_rows), area);
        let active_sel = self.selection.filter(|s| s.is_non_empty());
        let sel_bounds = active_sel.map(|s| s.normalized());

        for r in 0..rows {
            let row_in_sel = sel_bounds.is_some_and(|((sr, _), (er, _))| r >= sr && r <= er);
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
                let is_wide = cell.is_wide();
                let selected = row_in_sel
                    && active_sel.is_some_and(|sel| {
                        sel.contains(r, c) || (is_wide && sel.contains(r, c + 1))
                    });

                let style = if self.eink_mode {
                    let (fg_col, bg_col, mods) = resolve_eink_vt_colors_and_mods(cell, selected);
                    Style::default().fg(fg_col).bg(bg_col).add_modifier(mods)
                } else if selected {
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
                    if cell.dim() {
                        st = st.add_modifier(Modifier::DIM);
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

                let raw_sym = if cell.has_contents() {
                    cell.contents()
                } else {
                    ""
                };
                if is_wide {
                    if c + 1 >= cols {
                        if let Some(dst) = buf.cell_mut((x, y)) {
                            dst.reset();
                            dst.set_symbol(" ");
                            dst.set_style(style);
                        }
                        c += 1;
                    } else {
                        if let Some(dst) = buf.cell_mut((x, y)) {
                            dst.reset();
                            let sym = if raw_sym.width() == 2 { raw_sym } else { " " };
                            dst.set_symbol(sym);
                            dst.set_style(style);
                        }
                        if let Some(dst_next) = buf.cell_mut((x + 1, y)) {
                            dst_next.reset();
                            dst_next.set_style(style);
                        }
                        c += 2;
                    }
                } else {
                    let sym = normalized_narrow_vt_symbol(
                        self.screen,
                        r,
                        c,
                        rows,
                        cols,
                        raw_sym,
                        self.eink_mode,
                    );
                    if let Some(dst) = buf.cell_mut((x, y)) {
                        dst.reset();
                        dst.set_symbol(sym);
                        dst.set_style(style);
                    }
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

fn draw_modal_close_button(
    buf: &mut Buffer,
    popup: Rect,
    eink_mode: bool,
    hits: &mut Vec<HitRegion>,
) {
    let close_lbl = " ✕ ";
    let cw = close_lbl.width() as u16;
    if popup.width > cw + 4 {
        let cx = popup.x + popup.width - cw - 2;
        let cy = popup.y;
        buf.set_string(
            cx,
            cy,
            close_lbl,
            if eink_mode {
                Style::default()
                    .bg(EINK_SURFACE)
                    .fg(EINK_INK)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .bg(Color::Rgb(96, 38, 52))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            },
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

fn render_help_modal(f: &mut Frame, area: Rect, eink_mode: bool, hits: &mut Vec<HitRegion>) {
    hits.clear();
    let popup = centered_rect(74, 32, area);
    f.render_widget(Clear, popup);
    hits.push(HitRegion {
        rect: popup,
        target: HitTarget::ModalNoop,
    });

    let version_badge = format!(" hct {} ", crate::BUILD_VERSION);
    let mut block = Block::default()
        .title(" Quick Actions & Help ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if eink_mode {
            EINK_INK
        } else {
            Color::Rgb(122, 162, 247)
        }))
        .style(if eink_mode {
            Style::default().bg(EINK_PAPER).fg(EINK_INK)
        } else {
            Style::default().bg(Color::Rgb(20, 22, 34)).fg(Color::White)
        });
    if usize::from(popup.width) > version_badge.width() + 2 {
        block = block.title_bottom(
            Line::from(Span::styled(
                version_badge,
                Style::default().fg(if eink_mode {
                    EINK_INK_MUTED
                } else {
                    Color::Rgb(147, 153, 178)
                }),
            ))
            .right_aligned(),
        );
    }
    let inner = block.inner(popup);

    // Reflow the 16 touch quick-action buttons across available inner width so narrow mobile
    // viewports wrap buttons onto additional rows instead of dropping right-hand columns.
    let compact_btns = inner.width < 58;
    let (eink_wide, eink_short) = if eink_mode {
        ("  ◐ E-Ink: ON   ", " ◐E:ON ")
    } else {
        ("  ◐ E-Ink: OFF  ", " ◐E:OF ")
    };
    let quick_actions: [(&str, &str, HitTarget); 16] = [
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
            "  ▲ Page Up     ",
            " ▲PgUp ",
            HitTarget::TriggerAction(Action::PageUp),
        ),
        (
            "  ▼ Page Down   ",
            " ▼PgDn ",
            HitTarget::TriggerAction(Action::PageDown),
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
            eink_wide,
            eink_short,
            HitTarget::TriggerAction(Action::ToggleEink),
        ),
        (
            "  ↻ Redraw      ",
            " ↻Draw ",
            HitTarget::TriggerAction(Action::RedrawScreen),
        ),
        (
            "  ⏏ Detach hct  ",
            " ⏏Exit ",
            HitTarget::TriggerAction(Action::Detach),
        ),
        ("  ✕ Close Help  ", " ✕Help ", HitTarget::CancelModal),
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
            .fg(if eink_mode {
                EINK_INK
            } else {
                Color::LightCyan
            })
            .add_modifier(Modifier::BOLD),
    ))];
    for _ in 0..=btn_rows_count {
        lines.push(Line::from(""));
    }
    lines.extend([
        Line::from(Span::styled(
            "  Keyboard & Touch Shortcuts:",
            Style::default()
                .fg(if eink_mode { EINK_INK } else { Color::Cyan })
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  Top Bar [ ⏏ ]            Detach hct while keeping sessions running (^b q)"),
        Line::from("  Top Bar [ ▾ Space ]      Open secondary Space popover menu / + New Space"),
        Line::from("  Top Bar [ Tab ] / [ + ]  Switch directly to Tab or add a new Tab"),
        Line::from("  Left Floating Cross (+)  Tap Esc / Ctl / ^B / Alt / Tab (drag to move)"),
        Line::from("  Right Floating Cross (+) Tap ↑ / ← / ⎘ (Paste) / → / ↓ (drag to move)"),
        Line::from("  Clipboard Copy / Paste   Drag text to copy (OSC 52); Tap ⎘ to paste"),
        Line::from("  Next / Prev Tab          ^b n / ^b p, Alt+Right / Left, or Horiz Swipe"),
        Line::from("  Switch Tab 1..9          ^b 1..9       or  Alt+1..9"),
        Line::from("  New Tab in Space         ^b c          or  Tap [ + ] on top tab bar"),
        Line::from("  Rename / Close Tab       ^b , (rename) /   ^b x (close)"),
        Line::from("  Next / Prev Space        ^b Down / Up  or  Alt+Down / Alt+Up"),
        Line::from("  New / Rename Space       ^b C (new)    /   ^b $ (rename)"),
        Line::from("  Toggle Space Menu        ^b b          or  Tap [ ▾ Space ] on top bar"),
        Line::from("  Quick Fuzzy Switcher     ^b w          or  ^b f"),
        Line::from("  Jump Next Alert Agent    ^b a          (◉ blocked / ✓ done)"),
        Line::from("  Scrollback / History     ^b [          or  Tap [▼ BOT] on status bar"),
        Line::from("  Page Up / Down           ^b PgUp / PgDn /  Tap [▲Pg] / [▼Pg] (--eink / ?)"),
        Line::from("  E-Ink / Clean Redraw     ^b e (E-Ink)  /   ^b r (clean redraw)"),
        Line::from("  Detach (keep running)    ^b q / ^b d   or  Ctrl+\\ / Tap [ ⏏ ]"),
        Line::from(""),
        Line::from(Span::styled(
            "  Tap outside, [ ✕ ], Esc, Enter, or ? to close",
            Style::default().fg(if eink_mode {
                EINK_INK_MUTED
            } else {
                Color::DarkGray
            }),
        )),
    ]);
    f.render_widget(Paragraph::new(lines).block(block), popup);

    let buf = f.buffer_mut();
    draw_modal_close_button(buf, popup, eink_mode, hits);

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
        let bstyle = if eink_mode {
            if matches!(target, HitTarget::TriggerAction(Action::ToggleEink)) {
                Style::default()
                    .bg(EINK_INK)
                    .fg(EINK_PAPER)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .bg(EINK_SURFACE)
                    .fg(EINK_INK)
                    .add_modifier(Modifier::BOLD)
            }
        } else {
            Style::default()
                .bg(Color::Rgb(42, 52, 82))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        };
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
    eink_mode: bool,
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
        Line::from(format!(
            "  Close tab \"{tab_title}\" and kill its terminal?"
        )),
    ];
    let block = Block::default()
        .title(" Close Tab ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if eink_mode { EINK_INK } else { Color::LightRed }))
        .style(if eink_mode {
            Style::default().bg(EINK_PAPER).fg(EINK_INK)
        } else {
            Style::default().bg(Color::Rgb(28, 20, 28)).fg(Color::White)
        });
    let inner = block.inner(popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);

    let buf = f.buffer_mut();
    draw_modal_close_button(buf, popup, eink_mode, hits);

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
                if eink_mode {
                    Style::default()
                        .bg(EINK_INK)
                        .fg(EINK_PAPER)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .bg(Color::Rgb(110, 36, 44))
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                },
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
                if eink_mode {
                    Style::default()
                        .bg(EINK_SURFACE)
                        .fg(EINK_INK)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .bg(Color::Rgb(48, 52, 72))
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                },
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
    eink_mode: bool,
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
        .border_style(Style::default().fg(if eink_mode {
            EINK_INK
        } else {
            Color::Rgb(122, 162, 247)
        }))
        .style(if eink_mode {
            Style::default().bg(EINK_PAPER).fg(EINK_INK)
        } else {
            Style::default().bg(Color::Rgb(20, 22, 34)).fg(Color::White)
        });
    let inner = block.inner(popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);

    let buf = f.buffer_mut();
    draw_modal_close_button(buf, popup, eink_mode, hits);

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
                if eink_mode {
                    Style::default()
                        .bg(EINK_INK)
                        .fg(EINK_PAPER)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .bg(Color::Rgb(36, 82, 58))
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                },
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
                if eink_mode {
                    Style::default()
                        .bg(EINK_SURFACE)
                        .fg(EINK_INK)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .bg(Color::Rgb(48, 52, 72))
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                },
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

#[allow(clippy::too_many_arguments)]
fn render_fuzzy_modal(
    f: &mut Frame,
    area: Rect,
    world: &World,
    query: &str,
    selected: usize,
    selected_tab_id: Option<&str>,
    eink_mode: bool,
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
        .border_style(Style::default().fg(if eink_mode {
            EINK_INK
        } else {
            Color::Rgb(122, 162, 247)
        }))
        .style(if eink_mode {
            Style::default().bg(EINK_PAPER).fg(EINK_INK)
        } else {
            Style::default().bg(Color::Rgb(20, 22, 34)).fg(Color::White)
        });
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(inner);

    let prompt = Paragraph::new(format!(" › {query}▏")).style(if eink_mode {
        Style::default()
            .bg(EINK_PAPER)
            .fg(EINK_INK)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD)
    });
    f.render_widget(prompt, chunks[0]);

    let items = collect_fuzzy_items(world, query);
    let sel = resolve_fuzzy_selection(&items, selected, selected_tab_id).unwrap_or(0);
    let list_items: Vec<ListItem> = items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let is_sel = i == sel;
            if eink_mode {
                let row_style = if is_sel {
                    Style::default()
                        .bg(EINK_INK)
                        .fg(EINK_PAPER)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().bg(EINK_PAPER).fg(EINK_INK)
                };
                let muted_style = if is_sel {
                    row_style
                } else {
                    row_style.fg(EINK_INK_MUTED)
                };
                let line = Line::from(vec![
                    Span::styled(if is_sel { " ▸ " } else { "   " }, row_style),
                    Span::styled(
                        format!("{} ", it.state.symbol()),
                        row_style.add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("[{}] ", it.space_name),
                        row_style.add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("{} ", it.tab_name), row_style),
                    Span::styled(format!("({}) {}", it.term_addr, it.cwd), muted_style),
                ]);
                ListItem::new(line).style(row_style)
            } else {
                let st_col = state_color(it.state);
                let style = if is_sel {
                    Style::default()
                        .bg(Color::Rgb(65, 75, 120))
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                let line = Line::from(vec![
                    Span::styled(if is_sel { " ▸ " } else { "   " }, style),
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
            }
        })
        .collect();

    let mut list_state = ListState::default();
    if !list_items.is_empty() {
        list_state.select(Some(sel));
    }
    f.render_stateful_widget(List::new(list_items), chunks[1], &mut list_state);

    let buf = f.buffer_mut();
    draw_modal_close_button(buf, popup, eink_mode, hits);

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
    use crate::model::{SharedTab, TerminalInfo};
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
        let mut space_slop_out = None;

        terminal
            .draw(|f| {
                let geom = render_ui(
                    f,
                    &world,
                    None,
                    false,
                    24,
                    true, // touch_mode = true (1-row top tabbar + 1-row bottom status bar)
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
                space_slop_out = geom.space_slop_rect;
                assert_eq!(geom.term_rect.x, 0);
                assert_eq!(geom.term_rect.width, 100);
                assert_eq!(geom.term_rect.y, 1);
                assert_eq!(geom.term_rect.height, 22);
            })
            .unwrap();

        // Far-left button (`[ ⏏ ]`) on row 0 triggers Detach (`^b q`)
        assert_eq!(
            find_hit_target(&hits, 1, 0),
            Some(HitTarget::TriggerAction(Action::Detach))
        );
        assert_eq!(
            find_hit_target(&hits, 4, 0),
            Some(HitTarget::TriggerAction(Action::Detach))
        );

        // Top Bar Space selector absorbs both the left gap (col 5) and `╱` divider (col 14)
        // without changing visual rendering or overlapping Detach / first Tab pill
        assert_eq!(
            find_hit_target(&hits, 5, 0),
            Some(HitTarget::ToggleSpaceMenu)
        );
        assert_eq!(
            find_hit_target(&hits, 7, 0),
            Some(HitTarget::ToggleSpaceMenu)
        );
        let buf_row0 = terminal.backend().buffer();
        let slash_col = (0..100u16)
            .find(|&x| buf_row0[(x, 0)].symbol() == "╱")
            .expect("expected divider glyph on row 0");
        assert_eq!(
            find_hit_target(&hits, slash_col, 0),
            Some(HitTarget::ToggleSpaceMenu)
        );
        assert!(matches!(
            find_hit_target(&hits, slash_col + 1, 0),
            Some(HitTarget::SelectTab { .. })
        ));
        let slop = space_slop_out.expect("expected invisible vertical Space hit-slop rect");
        assert_eq!(slop.y, 1);
        assert_eq!(slop.height, 2);
        assert!(!is_in_rect(Some(slop), 4, 1));
        assert!(is_in_rect(Some(slop), 5, 1));
        assert!(is_in_rect(Some(slop), slash_col, 2));
        assert!(!is_in_rect(Some(slop), slash_col + 1, 1));
        assert!(!is_in_rect(Some(slop), 7, 3));

        // Top Bar exposes compact 1-row Tab pill, [ + ] (NewTab), and [ ? ] (ToggleHelp)
        assert!(hits
            .iter()
            .any(|h| matches!(h.target, HitTarget::SelectTab { .. }) && h.rect.height == 1));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::NewTab && h.rect.height == 1 && h.rect.y == 0));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::ToggleHelp && h.rect.height == 1 && h.rect.y == 0));

        // On the compact 1-row Top Tab Bar, Row 0 holds pill labels + slash divider with
        // emphasized bottom underline on the active tab, and Row 1 belongs immediately to
        // the terminal viewport (no blank gap row).
        let buf = terminal.backend().buffer();
        let row0: String = (0..100).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(row0.contains('⏏'));
        assert!(row0.contains("main"));
        assert!(row0.contains('╱'));
        assert!(row0.contains("1:agent"));
        assert!(row0.contains('+'));
        assert!(row0.contains('?'));
        assert!(!row0.contains('●'));
        assert!(!row0.contains('○'));
        // Every cell across the entire 1-row Top Tab Bar carries `Modifier::UNDERLINED` with
        // `TOPBAR_UNDERLINE_DIM`, while ONLY the active Tab highlights in `TOPBAR_UNDERLINE_ACTIVE`.
        for col in 0..100u16 {
            assert!(
                buf[(col, 0)].modifier.contains(Modifier::UNDERLINED),
                "expected col {col} on row 0 to be underlined"
            );
        }
        let active_byte = row0
            .find("1:agent")
            .expect("expected active tab label on row 0");
        let active_x = row0[..active_byte].chars().count() as u16;
        assert_eq!(buf[(active_x, 0)].underline_color, TOPBAR_UNDERLINE_ACTIVE);
        let inactive_byte = row0
            .find("2:shell")
            .expect("expected inactive tab label on row 0");
        let inactive_x = row0[..inactive_byte].chars().count() as u16;
        assert_eq!(buf[(inactive_x, 0)].underline_color, TOPBAR_UNDERLINE_DIM);
        assert_eq!(buf[(7, 0)].underline_color, TOPBAR_UNDERLINE_DIM);
        assert_eq!(buf[(80, 0)].underline_color, TOPBAR_UNDERLINE_DIM);

        // Bottom 1-row read-only Agent Status Bar exposes ExitScrollback on row 23
        assert!(hits.iter().any(|h| h.target == HitTarget::ExitScrollback
            && h.rect.y == 23
            && h.rect.height == 1));

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
                assert!(geom.space_slop_rect.is_none());
            })
            .unwrap();

        assert!(hits
            .iter()
            .any(|h| matches!(h.target, HitTarget::SelectSpace(_))));
        assert!(hits.iter().any(|h| h.target == HitTarget::NewSpacePrompt));
        assert!(hits.iter().any(|h| h.target == HitTarget::CancelModal));
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
                assert!(geom.space_slop_rect.is_none());
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
            Some(HitTarget::TriggerAction(Action::Detach))
        );
        assert_eq!(
            find_hit_target(&hits_no_touch, 4, 0),
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
    fn accordion_top_tabs_fold_inactive_pills_and_keep_all_tabs_visible_and_colored() {
        let mut world = World::default();
        world.doc.spaces.push(SharedSpace {
            id: "space-1".to_string(),
            name: "main".to_string(),
            tabs: (1..=8)
                .map(|i| SharedTab {
                    id: format!("tab-{i}"),
                    name: format!("agent-{i}"),
                    root: serde_json::json!({ "content": { "terminal": format!("term-{i}") } }),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });
        world.terminals.insert(
            "term-1".to_string(),
            TerminalInfo {
                address: "term-1".to_string(),
                state: "live".to_string(),
                status_word: Some("blocked".to_string()),
                ..Default::default()
            },
        );
        world.terminals.insert(
            "term-4".to_string(),
            TerminalInfo {
                address: "term-4".to_string(),
                state: "live".to_string(),
                status_word: Some("idle".to_string()),
                turn: 3,
                seen_turn: Some(1), // unread finished turn -> AgentState::Done
                ..Default::default()
            },
        );
        world.terminals.insert(
            "term-5".to_string(),
            TerminalInfo {
                address: "term-5".to_string(),
                state: "live".to_string(),
                status_word: Some("working".to_string()),
                ..Default::default()
            },
        );
        world.select_tab_index(2); // active = tab-3 ("agent-3")

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
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

        let buf = terminal.backend().buffer();
        let row0: String = (0..80).map(|x| buf[(x, 0)].symbol()).collect();
        // Active tab expands its title while inactive tabs fold to `[ N ]` without circles or scrolling
        assert!(row0.contains(" 3:agent-3 "));
        assert!(!row0.contains("1:agent-1"));
        assert!(!row0.contains('●'));
        assert!(!row0.contains('○'));
        assert!(!row0.contains('‹'));
        assert!(!row0.contains('›'));

        // Every single tab (`tab-1` .. `tab-8`) remains directly selectable on row 0
        for i in 1..=8 {
            let tid = format!("tab-{i}");
            let region = hits
                .iter()
                .find(|h| {
                    h.target
                        == HitTarget::SelectTab {
                            space_id: "space-1".to_string(),
                            tab_id: tid.clone(),
                        }
                })
                .unwrap_or_else(|| panic!("expected SelectTab hit region for {tid}"));
            if i == 1 {
                assert_eq!(buf[(region.rect.x + 1, 0)].fg, HERDR_RED);
            } else if i == 2 {
                assert_eq!(buf[(region.rect.x + 1, 0)].bg, HERDR_SURFACE0);
                assert_eq!(buf[(region.rect.x + 1, 0)].fg, HERDR_SUBTEXT0);
            } else if i == 3 {
                assert_eq!(buf[(region.rect.x + 1, 0)].bg, HERDR_ACCENT);
                assert_eq!(buf[(region.rect.x + 1, 0)].fg, HERDR_PANEL_BG);
                assert_eq!(
                    buf[(region.rect.x + 1, 0)].underline_color,
                    TOPBAR_UNDERLINE_ACTIVE
                );
            } else if i == 4 {
                assert_eq!(buf[(region.rect.x + 1, 0)].fg, HERDR_TEAL);
            } else if i == 5 {
                assert_eq!(buf[(region.rect.x + 1, 0)].fg, HERDR_YELLOW);
            }
        }
    }

    #[test]
    fn extreme_overflow_top_tabs_show_count_badges_opening_fuzzy_switcher() {
        let mut world = World::default();
        world.doc.spaces.push(SharedSpace {
            id: "space-1".to_string(),
            name: "main".to_string(),
            tabs: (1..=12)
                .map(|i| SharedTab {
                    id: format!("tab-{i}"),
                    name: format!("agent-{i}"),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });
        world.select_tab_index(5); // active = tab-6 (middle of 12 tabs)

        let backend = TestBackend::new(42, 12);
        let mut terminal = Terminal::new(backend).unwrap();
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

        let buf = terminal.backend().buffer();
        let row0: String = (0..42).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(row0.contains('‹'), "expected left overflow badge: {row0:?}");
        assert!(
            row0.contains('›'),
            "expected right overflow badge: {row0:?}"
        );
        assert!(row0.contains(" 6 "), "expected active tab 6 pill: {row0:?}");
        assert!(!hits
            .iter()
            .any(|h| h.target == HitTarget::TriggerAction(Action::NextTab)));
        assert_eq!(
            hits.iter()
                .filter(|h| h.target == HitTarget::TriggerAction(Action::FuzzyFinder))
                .count(),
            2,
            "both left and right overflow badges should open FuzzyFinder"
        );
        assert!(hits.iter().any(|h| {
            h.target
                == HitTarget::SelectTab {
                    space_id: "space-1".to_string(),
                    tab_id: "tab-6".to_string(),
                }
        }));

        // At tab-1 (leftmost), no left badge is shown; only right overflow badge appears
        world.select_tab_index(0);
        terminal
            .draw(|f| {
                hits = render_ui(
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
                )
                .hits;
            })
            .unwrap();
        let row0_first: String = (0..42)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect();
        assert!(!row0_first.contains('‹'));
        assert!(row0_first.contains('›'));
        assert_eq!(
            hits.iter()
                .filter(|h| h.target == HitTarget::TriggerAction(Action::FuzzyFinder))
                .count(),
            1
        );
    }

    #[test]
    fn vs16_emoji_and_ultra_narrow_top_tabbar_preserve_width_and_index_visibility() {
        for sample in ["❤️❤️❤️❤️", "⚠️⚙️❤️", "中文標題測試", "mix❤️中a"]
        {
            for max_w in 0..=8 {
                let t = truncate_to_width(sample, max_w);
                assert!(
                    t.width() <= max_w,
                    "truncate_to_width({sample:?}, {max_w}) = {t:?} has width {} > {max_w}",
                    t.width()
                );
            }
        }

        // Accordion Stage 3 with VS16 emoji active tab title keeps all trailing tabs visible
        let mut world = World::default();
        world.doc.spaces.push(SharedSpace {
            id: "space-1".to_string(),
            name: "main".to_string(),
            tabs: (1..=6)
                .map(|i| SharedTab {
                    id: format!("tab-{i}"),
                    name: if i == 2 {
                        "❤️❤️❤️❤️❤️❤️".to_string()
                    } else {
                        format!("agent-{i}")
                    },
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });
        world.select_tab_index(1);

        let backend = TestBackend::new(46, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut hits = Vec::new();
        terminal
            .draw(|f| {
                hits = render_ui(
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
                )
                .hits;
            })
            .unwrap();
        for i in 1..=6 {
            let tid = format!("tab-{i}");
            assert!(
                hits.iter().any(|h| {
                    h.target
                        == HitTarget::SelectTab {
                            space_id: "space-1".to_string(),
                            tab_id: tid.clone(),
                        }
                }),
                "expected {tid} to remain visible when active tab has VS16 emoji"
            );
        }

        // Ultra-narrow (`avail_tabs_w = 1..=2`) fallback displays unpadded index `"2"` instead of `"…"`
        let tiny_backend = TestBackend::new(21, 10);
        let mut tiny_term = Terminal::new(tiny_backend).unwrap();
        let mut tiny_hits = Vec::new();
        tiny_term
            .draw(|f| {
                tiny_hits = render_ui(
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
                )
                .hits;
            })
            .unwrap();
        let active_hit = tiny_hits
            .iter()
            .find(|h| {
                h.target
                    == HitTarget::SelectTab {
                        space_id: "space-1".to_string(),
                        tab_id: "tab-2".to_string(),
                    }
            })
            .expect("expected active tab hit on 21-col viewport");
        let tiny_buf = tiny_term.backend().buffer();
        let pill_text: String = (active_hit.rect.x..active_hit.rect.x + active_hit.rect.width)
            .map(|x| tiny_buf[(x, 0)].symbol())
            .collect();
        assert!(
            pill_text.contains('2'),
            "expected unpadded active index in {pill_text:?}"
        );
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

        // Constrained viewports (e.g. 80x8 with 2-row top tabbar -> 5-row viewport, or 30x8)
        // scale down the + cross so all 4 directions and centered Paste remain accessible.
        for (w, h, sidebar_open) in [(80u16, 8u16, false), (30u16, 8u16, true)] {
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
                assert!(geom.space_slop_rect.is_none());
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
            .any(|h| h.target == HitTarget::TriggerAction(Action::EnterScrollback)));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::TriggerAction(Action::PageUp)));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::TriggerAction(Action::PageDown)));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::TriggerAction(Action::Detach)));
    }

    #[test]
    fn help_modal_displays_build_version_on_wide_viewport() {
        let backend = TestBackend::new(80, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let world = World::default();

        terminal
            .draw(|f| {
                let _ = render_ui(
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
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        let rendered: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains(&format!("hct {}", crate::BUILD_VERSION)));
    }

    #[test]
    fn terminal_selection_extracts_single_and_multiline_text() {
        let nz = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let mut parser = vt100::Parser::new(nz(10), nz(12), 100);
        parser.process(b"hello world\r\nsecond line");
        let sel = TerminalSelection {
            anchor: (0, 6),
            head: (1, 5),
            active: false,
            dragged: true,
        };
        assert_eq!(sel.extract_text(parser.screen()), "world\nsecond");

        // Soft-wrapped lines (no `\r\n`) should be joined without inserting a spurious newline
        let mut wrap_parser = vt100::Parser::new(nz(10), nz(10), 100);
        wrap_parser.process(b"abcdefghijklmno");
        let wrap_sel = TerminalSelection {
            anchor: (0, 0),
            head: (1, 4),
            active: false,
            dragged: true,
        };
        assert_eq!(
            wrap_sel.extract_text(wrap_parser.screen()),
            "abcdefghijklmno"
        );
    }

    #[test]
    fn vt_screen_widget_renders_sgr2_dim_and_preserves_other_modifiers() {
        let nz = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let mut parser = vt100::Parser::new(nz(4), nz(50), 0);
        parser.process(b"\x1b[2mfaint\x1b[0m \x1b[2;3mdim_it\x1b[22m it_only\x1b[0m plain");

        let backend = TestBackend::new(50, 4);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            f.render_widget(
                VtScreenWidget {
                    screen: parser.screen(),
                    selection: None,
                    eink_mode: false,
                },
                f.area(),
            );
        })
        .unwrap();

        let buf = term.backend().buffer();
        // "faint" at col 0..5 -> DIM only
        assert_eq!(buf[(0, 0)].symbol(), "f");
        assert_eq!(buf[(0, 0)].modifier, Modifier::DIM);
        // "dim_it" at col 6..12 -> DIM | ITALIC
        assert_eq!(buf[(6, 0)].symbol(), "d");
        assert_eq!(buf[(6, 0)].modifier, Modifier::DIM | Modifier::ITALIC);
        // "it_only" at col 13..20 (after SGR 22 normal intensity) -> ITALIC only
        assert_eq!(buf[(13, 0)].symbol(), "i");
        assert_eq!(buf[(13, 0)].modifier, Modifier::ITALIC);
        // "plain" at col 21..26 -> empty modifiers
        assert_eq!(buf[(21, 0)].symbol(), "p");
        assert_eq!(buf[(21, 0)].modifier, Modifier::empty());
    }

    #[test]
    fn vt_render_dims_clamps_to_the_smaller_of_screen_and_viewport() {
        let area = |w, h| Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };

        // Matching sizes paint everything.
        assert_eq!(vt_render_dims((80, 24), area(80, 24)), (24, 80));

        // Screen larger than the viewport (shrink before the channel resize landed):
        // clip to the viewport so nothing spills into the tab or status bar.
        assert_eq!(vt_render_dims((120, 40), area(80, 24)), (24, 80));

        // Viewport larger than the screen (grow before the parser was resized):
        // paint only what the screen has, leaving the rest to the cleared buffer.
        assert_eq!(vt_render_dims((80, 24), area(120, 40)), (24, 80));

        // Zero-sized viewport paints nothing rather than underflowing.
        assert_eq!(vt_render_dims((80, 24), area(0, 0)), (0, 0));
    }

    #[test]
    fn vt_screen_widget_leaves_rows_beyond_the_screen_blank_when_viewport_is_taller() {
        let nz = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let mut parser = vt100::Parser::new(nz(2), nz(10), 0);
        parser.process(b"top\r\nmid");

        // Viewport is taller than the 2-row screen: the extra rows must stay blank
        // and must not repeat or corrupt the screen contents.
        let backend = TestBackend::new(10, 5);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            f.render_widget(
                VtScreenWidget {
                    screen: parser.screen(),
                    selection: None,
                    eink_mode: false,
                },
                f.area(),
            );
        })
        .unwrap();

        let buf = term.backend().buffer();
        assert_eq!(buf[(0, 0)].symbol(), "t");
        assert_eq!(buf[(0, 1)].symbol(), "m");
        for y in 2..5 {
            for x in 0..10 {
                assert_eq!(
                    buf[(x, y)].symbol(),
                    " ",
                    "row {y} col {x} should be blank, not stale or duplicated content"
                );
            }
        }
    }

    #[test]
    fn vt_screen_widget_does_not_paint_past_a_shorter_viewport() {
        let nz = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let mut parser = vt100::Parser::new(nz(5), nz(10), 0);
        parser.process(b"r0\r\nr1\r\nr2\r\nr3\r\nr4");

        // Viewport shows only 2 of the 5 rows; the widget must clip, not overflow.
        let backend = TestBackend::new(10, 2);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            f.render_widget(
                VtScreenWidget {
                    screen: parser.screen(),
                    selection: None,
                    eink_mode: false,
                },
                f.area(),
            );
        })
        .unwrap();

        let buf = term.backend().buffer();
        assert_eq!(buf[(0, 0)].symbol(), "r");
        assert_eq!(buf[(1, 0)].symbol(), "0");
        assert_eq!(buf[(0, 1)].symbol(), "r");
        assert_eq!(buf[(1, 1)].symbol(), "1");
        // Buffer is exactly 2 rows tall, so reaching here without a panic is the
        // assertion that nothing was written outside the area.
    }

    #[test]
    fn vt_screen_widget_resets_wide_trailing_cells_and_clamps_right_edge_wide_glyph() {
        let nz = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let mut parser = vt100::Parser::new(nz(2), nz(6), 0);
        parser.process("中A漢".as_bytes());

        // Render into a 5-column viewport so "漢" at col 3..5 fits on 6-col, while at 4-col it hits right edge
        let backend = TestBackend::new(4, 2);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            f.render_widget(
                VtScreenWidget {
                    screen: parser.screen(),
                    selection: None,
                    eink_mode: false,
                },
                f.area(),
            );
        })
        .unwrap();

        let buf = term.backend().buffer();
        assert_eq!(buf[(0, 0)].symbol(), "中");
        // Trailing cell of wide glyph uses reset (" "), never zero-width ""
        assert_eq!(buf[(1, 0)].symbol(), " ");
        assert_eq!(buf[(2, 0)].symbol(), "A");
        // Wide glyph at col 3 (where c + 1 >= 4) is clamped to a space so it never overflows
        assert_eq!(buf[(3, 0)].symbol(), " ");
    }

    #[test]
    fn bottom_bar_renders_focused_agent_quota_on_single_row_and_omits_redundant_shell_info() {
        let world = sample_world();
        let quota = AgentQuota {
            agent: AgentKind::Codex,
            metrics: vec![
                QuotaMetric {
                    window_label: "5h".to_string(),
                    remaining_percent: 49.0,
                    detail: Some("2h14m".to_string()),
                },
                QuotaMetric {
                    window_label: "7d".to_string(),
                    remaining_percent: 8.0,
                    detail: Some("4d12h".to_string()),
                },
            ],
        };

        // 1. Desktop width (`100x24`): shows full progress bars, percentages, and reset details on row 23
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut hits = Vec::new();
        terminal
            .draw(|f| {
                let geom = render_ui_with_pads_and_quota(
                    f,
                    &world,
                    None,
                    false,
                    24,
                    false,
                    false,
                    false,
                    false,
                    2,
                    None,
                    true,
                    &Modal::None,
                    None,
                    false,
                    None,
                    false,
                    Some(&quota),
                );
                hits = geom.hits;
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        let bot_line: String = (0..100).map(|x| buf[(x, 23)].symbol()).collect();
        assert!(bot_line.contains("Codex"));
        assert!(bot_line.contains("5h ███▉░░░░ 49% (2h14m)"));
        assert!(bot_line.contains("7d ▋░░░░░░░ 8% (4d12h)"));
        assert!(bot_line.contains("✓ Copied"));
        assert!(bot_line.contains("▼ BOT (+2)"));
        assert!(!bot_line.contains("WORKING"));
        assert!(!bot_line.contains("turns:"));
        assert!(hits.iter().any(|h| h.target == HitTarget::ExitScrollback
            && h.rect.y == 23
            && h.rect.height == 1));

        // 2. Narrow width (`42x12`): drops optional detail so both 5h and 7d percentages fit on 1 row
        let narrow_backend = TestBackend::new(42, 12);
        let mut narrow_term = Terminal::new(narrow_backend).unwrap();
        narrow_term
            .draw(|f| {
                render_ui_with_pads_and_quota(
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
                    None,
                    false,
                    None,
                    false,
                    Some(&quota),
                );
            })
            .unwrap();
        let nbuf = narrow_term.backend().buffer();
        let nline: String = (0..42).map(|x| nbuf[(x, 11)].symbol()).collect();
        assert!(nline.contains("49%"));
        assert!(nline.contains("8%"));

        // 3. Plain shell (`focused_quota = None`): left side is clean with no redundant SHELL/cwd text
        let shell_backend = TestBackend::new(80, 12);
        let mut shell_term = Terminal::new(shell_backend).unwrap();
        shell_term
            .draw(|f| {
                render_ui_with_pads_and_quota(
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
                    None,
                    false,
                    None,
                    false,
                    None,
                );
            })
            .unwrap();
        let sbuf = shell_term.backend().buffer();
        let sline: String = (0..80).map(|x| sbuf[(x, 11)].symbol()).collect();
        assert_eq!(sline.trim(), "");
    }

    #[test]
    fn tab_overview_renders_full_viewport_highlights_plus_pill_and_preserves_topbar_hits() {
        use crate::model::{make_single_terminal_tab, TerminalInfo};
        use crate::usage::{AgentKind, AgentQuota, QuotaMetric};

        let mut world = World::default();
        world.doc.spaces.push(SharedSpace {
            id: "space-1".to_string(),
            name: "main".to_string(),
            folder: "/tmp/work".to_string(),
            tabs: vec![
                make_single_terminal_tab("tab-1", "agent", "t1"),
                make_single_terminal_tab("tab-2", "shell", "t2"),
            ],
            ..Default::default()
        });
        world.doc.spaces.push(SharedSpace {
            id: "space-2".to_string(),
            name: "other".to_string(),
            folder: "/tmp/other".to_string(),
            tabs: vec![make_single_terminal_tab("tab-other", "hidden-tab", "t3")],
            ..Default::default()
        });
        world.terminals.insert(
            "t1".to_string(),
            TerminalInfo {
                address: "t1".to_string(),
                cwd: "/tmp/work".to_string(),
                status_word: Some("blocked".to_string()),
                status_msg: Some("Approve tool call".to_string()),
                ..Default::default()
            },
        );
        world.terminals.insert(
            "t2".to_string(),
            TerminalInfo {
                address: "t2".to_string(),
                cwd: "/tmp/work/sub".to_string(),
                ..Default::default()
            },
        );
        world.terminals.insert(
            "t3".to_string(),
            TerminalInfo {
                address: "t3".to_string(),
                cwd: "/tmp/other".to_string(),
                ..Default::default()
            },
        );
        world.select_tab_index(1);

        let quota = AgentQuota {
            agent: AgentKind::Codex,
            metrics: vec![QuotaMetric {
                window_label: "5h".to_string(),
                remaining_percent: 80.0,
                detail: None,
            }],
        };

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut hits = Vec::new();
        terminal
            .draw(|f| {
                let geom = render_ui_with_pads_and_quota(
                    f,
                    &world,
                    None,
                    false,
                    24,
                    true,
                    false,
                    false,
                    false,
                    15,
                    None,
                    false,
                    &Modal::default_tab_overview(),
                    None,
                    false,
                    None,
                    false,
                    Some(&quota),
                );
                assert!(geom.mod_rect.is_none());
                assert!(geom.dpad_rect.is_none());
                assert!(geom.space_slop_rect.is_none());
                hits = geom.hits;
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        let row0: String = (0..100).map(|x| buf[(x, 0)].symbol()).collect();
        let plus_byte = row0.find(" + ").expect("expected trailing [ + ] pill");
        let plus_x = row0[..plus_byte].chars().count() as u16;
        assert_eq!(buf[(plus_x, 0)].bg, HERDR_ACCENT);
        assert_eq!(buf[(plus_x, 0)].fg, HERDR_PANEL_BG);
        assert_eq!(buf[(plus_x, 0)].underline_color, TOPBAR_UNDERLINE_ACTIVE);

        let tab2_byte = row0.find("2:shell").expect("expected 2:shell pill");
        let tab2_x = row0[..tab2_byte].chars().count() as u16;
        assert_ne!(buf[(tab2_x, 0)].bg, HERDR_ACCENT);
        assert_eq!(buf[(tab2_x, 0)].underline_color, TOPBAR_UNDERLINE_DIM);

        // TopBar hits remain active alongside viewport overview hits
        assert_eq!(
            find_hit_target(&hits, 1, 0),
            Some(HitTarget::TriggerAction(Action::Detach))
        );
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::ToggleSpaceMenu && h.rect.y == 0));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::NewTab && h.rect.y == 0));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::ToggleHelp && h.rect.y == 0));
        assert!(hits
            .iter()
            .any(|h| h.target == HitTarget::NewTab && h.rect.y >= 1 && h.rect.y < 23));
        assert!(hits.iter().any(|h| {
            h.target
                == HitTarget::SelectTab {
                    space_id: "space-1".to_string(),
                    tab_id: "tab-1".to_string(),
                }
                && h.rect.y >= 1
        }));
        assert!(hits.iter().any(|h| {
            h.target
                == HitTarget::SelectTab {
                    space_id: "space-1".to_string(),
                    tab_id: "tab-2".to_string(),
                }
                && h.rect.y >= 1
        }));
        assert!(!hits.iter().any(|h| {
            matches!(
                h.target,
                HitTarget::SelectTab { ref tab_id, .. } if tab_id == "tab-other"
            )
        }));
        assert_eq!(find_hit_target(&hits, 0, 12), Some(HitTarget::ModalNoop));

        // Bottom bar suppresses background quota and scrollback badge while overview is open
        let bot_line: String = (0..100).map(|x| buf[(x, 23)].symbol()).collect();
        assert!(!bot_line.contains("Codex"));
        assert!(!bot_line.contains("BOT"));
        assert!(!hits.iter().any(|h| h.target == HitTarget::ExitScrollback));

        let rendered: String = (0..24)
            .map(|y| (0..100).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("+ New Tab"));
        assert!(rendered.contains("1:agent"));
        assert!(rendered.contains("2:shell"));
        assert!(rendered.contains("Approve tool call"));
        assert!(!rendered.contains("hidden-tab"));

        // Filtered query preserves real tab numbers and shows empty state when nothing matches
        terminal
            .draw(|f| {
                render_ui(
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
                    &Modal::TabOverview {
                        query: "sh".to_string(),
                        selected: 1,
                        selected_target: OverviewSelection::Tab("tab-2".to_string()),
                    },
                );
            })
            .unwrap();
        let filt_buf = terminal.backend().buffer();
        let filt_vp: String = (1..23)
            .map(|y| {
                (0..100)
                    .map(|x| filt_buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(filt_vp.contains("2:shell"));
        assert!(!filt_vp.contains("1:agent"));

        terminal
            .draw(|f| {
                render_ui(
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
                    &Modal::TabOverview {
                        query: "zzz".to_string(),
                        selected: 0,
                        selected_target: OverviewSelection::NewTab,
                    },
                );
            })
            .unwrap();
        let empty_buf = terminal.backend().buffer();
        let empty_vp: String = (1..23)
            .map(|y| {
                (0..100)
                    .map(|x| empty_buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(empty_vp.contains("No matching tabs in this Space"));

        // Compact / ultra-short viewports never panic and keep + New Tab clickable
        for (w, h) in [(20u16, 5u16), (16u16, 4u16)] {
            let mut small_term = Terminal::new(TestBackend::new(w, h)).unwrap();
            let mut small_hits = Vec::new();
            small_term
                .draw(|f| {
                    small_hits = render_ui(
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
                        &Modal::default_tab_overview(),
                    )
                    .hits;
                })
                .unwrap();
            assert!(small_hits
                .iter()
                .any(|hit| hit.target == HitTarget::NewTab && hit.rect.y >= 1));
        }
    }

    fn assert_strict_eink_buffer(buf: &Buffer) {
        let is_neutral_carta_color = |c: Color| match c {
            Color::Rgb(r, g, b) => r == g && g == b && matches!(r, 0 | 51 | 221 | 238 | 255),
            _ => false,
        };
        for y in buf.area.y..buf.area.y + buf.area.height {
            for x in buf.area.x..buf.area.x + buf.area.width {
                let cell = &buf[(x, y)];
                assert!(
                    !cell.modifier.contains(Modifier::DIM),
                    "cell ({x}, {y}) symbol {:?} unexpectedly retained Modifier::DIM in E-Ink mode",
                    cell.symbol()
                );
                assert!(
                    is_neutral_carta_color(cell.fg),
                    "cell ({x}, {y}) symbol {:?} has non-Carta fg {:?}",
                    cell.symbol(),
                    cell.fg
                );
                assert!(
                    is_neutral_carta_color(cell.bg),
                    "cell ({x}, {y}) symbol {:?} has non-Carta bg {:?}",
                    cell.symbol(),
                    cell.bg
                );
            }
        }
    }

    #[test]
    fn eink_mode_quantizes_chrome_modals_crosses_and_vt_to_neutral_carta_lut() {
        let mut world = World::default();
        world.doc.spaces.push(SharedSpace {
            id: "space-1".to_string(),
            name: "main".to_string(),
            folder: "/tmp/work".to_string(),
            tabs: vec![
                SharedTab {
                    id: "tab-blk".to_string(),
                    name: "agy-blk".to_string(),
                    root: serde_json::json!({"content": {"terminal": "t-blk"}}),
                    ..Default::default()
                },
                SharedTab {
                    id: "tab-wrk".to_string(),
                    name: "agy-wrk".to_string(),
                    root: serde_json::json!({"content": {"terminal": "t-wrk"}}),
                    ..Default::default()
                },
                SharedTab {
                    id: "tab-don".to_string(),
                    name: "agy-don".to_string(),
                    root: serde_json::json!({"content": {"terminal": "t-don"}}),
                    ..Default::default()
                },
                SharedTab {
                    id: "tab-idl".to_string(),
                    name: "sh-idl".to_string(),
                    root: serde_json::json!({"content": {"terminal": "t-idl"}}),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        world.terminals.insert(
            "t-blk".to_string(),
            TerminalInfo {
                address: "t-blk".to_string(),
                status_word: Some("blocked".to_string()),
                status_msg: Some("Approve command".to_string()),
                ..Default::default()
            },
        );
        world.terminals.insert(
            "t-wrk".to_string(),
            TerminalInfo {
                address: "t-wrk".to_string(),
                status_word: Some("working".to_string()),
                ..Default::default()
            },
        );
        world.terminals.insert(
            "t-don".to_string(),
            TerminalInfo {
                address: "t-don".to_string(),
                status_word: Some("idle".to_string()),
                turn: 3,
                seen_turn: Some(1),
                ..Default::default()
            },
        );
        world.terminals.insert(
            "t-idl".to_string(),
            TerminalInfo {
                address: "t-idl".to_string(),
                status_word: Some("idle".to_string()),
                turn: 2,
                seen_turn: Some(2),
                ..Default::default()
            },
        );
        world.select_tab_index(3);

        let nz = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let mut parser = vt100::Parser::new(nz(22), nz(100), 0);
        parser.process(
            concat!(
                "⠋ Thinking\r\n",
                "◐ Loading\r\n",
                "⣀⣤ Spark\r\n",
                "\x1b[2mDimComment\x1b[0m ",
                "\x1b[38;2;255;220;120;48;2;20;24;36mDarkBg\x1b[0m ",
                "\x1b[7mInv\x1b[0m\r\n",
                "⣿ Vertical\r\n",
                "⣤ Column\r\n",
                "◜ CornerArc",
            )
            .as_bytes(),
        );

        let quota = AgentQuota {
            agent: AgentKind::Antigravity,
            metrics: vec![
                QuotaMetric {
                    window_label: "5h".to_string(),
                    remaining_percent: 12.0,
                    detail: Some("1h10m".to_string()),
                },
                QuotaMetric {
                    window_label: "7d".to_string(),
                    remaining_percent: 85.0,
                    detail: None,
                },
            ],
        };

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let mut geom_hits = Vec::new();
        let mut mod_rect_out = None;
        terminal
            .draw(|f| {
                let geom = render_ui_full(
                    f,
                    &world,
                    Some(parser.screen()),
                    false,
                    24,
                    true,
                    true,
                    false,
                    false,
                    5,
                    None,
                    true,
                    &Modal::None,
                    None,
                    false,
                    None,
                    false,
                    Some(&quota),
                    true,
                );
                geom_hits = geom.hits;
                mod_rect_out = geom.mod_rect;
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        assert_strict_eink_buffer(buf);

        let top_row: String = (0..100).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(top_row.contains("!1:"));
        assert!(top_row.contains("*2:"));
        assert!(top_row.contains("✓3:"));

        let bot_row: String = (0..100).map(|x| buf[(x, 23)].symbol()).collect();
        assert!(bot_row.contains("12%!"));
        assert!(bot_row.contains('█'));
        assert!(bot_row.contains('─'));
        assert!(!bot_row.contains('░'));
        assert!(bot_row.contains("▲Pg"));
        assert!(bot_row.contains("▼Pg"));
        assert!(geom_hits
            .iter()
            .any(|h| h.target == HitTarget::TriggerAction(Action::PageUp)));
        assert!(geom_hits
            .iter()
            .any(|h| h.target == HitTarget::TriggerAction(Action::PageDown)));
        assert!(geom_hits
            .iter()
            .any(|h| h.target == HitTarget::ExitScrollback));

        // Verify VtScreenWidget spinner freezing, vertical/horizontal sparkline & corner-arc
        // preservation, DIM removal, and RGB de-fog
        assert_eq!(buf[(0, 1)].symbol(), "◈");
        assert_eq!(buf[(0, 2)].symbol(), "◈");
        assert_eq!(buf[(0, 3)].symbol(), "⣀");
        assert_eq!(buf[(1, 3)].symbol(), "⣤");
        assert_eq!(buf[(0, 4)].symbol(), "D");
        assert_eq!(buf[(0, 4)].fg, EINK_INK_MUTED);
        assert_eq!(buf[(11, 4)].symbol(), "D");
        assert_eq!(buf[(11, 4)].bg, EINK_PAPER);
        assert_eq!(buf[(11, 4)].fg, EINK_INK);
        assert_eq!(buf[(18, 4)].symbol(), "I");
        assert_eq!(buf[(18, 4)].bg, EINK_INK);
        assert_eq!(buf[(18, 4)].fg, EINK_PAPER);
        assert_eq!(buf[(0, 5)].symbol(), "⣿");
        assert_eq!(buf[(0, 6)].symbol(), "⣤");
        assert_eq!(buf[(0, 7)].symbol(), "◜");

        // Verify left modifier cross corners are hollow (row 0, col 0 relative to disc keeps EINK_PAPER)
        // and active center `^B` sector is inverted (`EINK_INK` bg / `EINK_PAPER` fg).
        let mod_rect = mod_rect_out.expect("mod cross rect should be present");
        assert_eq!(buf[(mod_rect.x, mod_rect.y)].bg, EINK_PAPER);
        assert_eq!(buf[(mod_rect.x + 7, mod_rect.y + 3)].bg, EINK_INK);
        assert_eq!(buf[(mod_rect.x + 7, mod_rect.y + 3)].fg, EINK_PAPER);

        // Active Blocked tab also keeps `!` prefix
        world.select_tab_index(0);
        terminal
            .draw(|f| {
                let _ = render_ui_full(
                    f,
                    &world,
                    Some(parser.screen()),
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
                    None,
                    false,
                    None,
                    false,
                    None,
                    true,
                );
            })
            .unwrap();
        let active_blk_top: String = (0..100)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect();
        assert!(active_blk_top.contains("!1:"));

        // All modals strictly obey the Carta neutral LUT & Help exposes ToggleEink / RedrawScreen hits
        let modals = [
            Modal::Help,
            Modal::SpaceMenu,
            Modal::default_tab_overview(),
            Modal::Fuzzy {
                query: String::new(),
                selected: 0,
                selected_tab_id: Some("tab-blk".to_string()),
            },
            Modal::ConfirmCloseTab {
                tab_id: "tab-blk".to_string(),
                tab_title: "agy-blk".to_string(),
            },
            Modal::Prompt {
                kind: PromptKind::NewSpace,
                input: "work".to_string(),
            },
        ];
        for modal in &modals {
            let mut modal_hits = Vec::new();
            terminal
                .draw(|f| {
                    modal_hits = render_ui_full(
                        f,
                        &world,
                        Some(parser.screen()),
                        false,
                        24,
                        false,
                        false,
                        false,
                        false,
                        0,
                        None,
                        false,
                        modal,
                        None,
                        false,
                        None,
                        false,
                        Some(&quota),
                        true,
                    )
                    .hits;
                })
                .unwrap();
            assert_strict_eink_buffer(terminal.backend().buffer());
            if matches!(modal, Modal::Help) {
                assert!(modal_hits
                    .iter()
                    .any(|h| h.target == HitTarget::TriggerAction(Action::ToggleEink)));
                assert!(modal_hits
                    .iter()
                    .any(|h| h.target == HitTarget::TriggerAction(Action::RedrawScreen)));
                assert!(modal_hits
                    .iter()
                    .any(|h| h.target == HitTarget::TriggerAction(Action::PageUp)));
                assert!(modal_hits
                    .iter()
                    .any(|h| h.target == HitTarget::TriggerAction(Action::PageDown)));
            }
        }
    }

    #[test]
    fn eink_screen_snapshot_coalesces_spinners_and_detects_real_changes() {
        let nz = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let mut parser = vt100::Parser::new(nz(6), nz(30), 100);
        let area = Rect::new(0, 1, 30, 6);

        parser.process("\x1b[?25l\x1b[H⠋ Thinking...".as_bytes());
        let snap1 = capture_eink_screen_snapshot(parser.screen(), area, 0);

        parser.process("\x1b[H⠙ Thinking...".as_bytes());
        let snap2 = capture_eink_screen_snapshot(parser.screen(), area, 0);

        parser.process("\x1b[H⠹ Thinking...".as_bytes());
        let snap3 = capture_eink_screen_snapshot(parser.screen(), area, 0);

        assert_eq!(snap1, snap2);
        assert_eq!(snap2, snap3);

        parser.process(b"\x1b[2;1HReady");
        let snap4 = capture_eink_screen_snapshot(parser.screen(), area, 0);
        assert_ne!(snap1, snap4);

        parser.process(b"\x1b[?25h\x1b[2;6H");
        let snap5 = capture_eink_screen_snapshot(parser.screen(), area, 0);
        assert_ne!(snap4, snap5);

        let snap6 = capture_eink_screen_snapshot(parser.screen(), area, 1);
        assert_ne!(snap5, snap6);
    }
}
