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
    pub page_up_rect: Option<Rect>,
    pub page_down_rect: Option<Rect>,
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

#[cfg(test)]
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
    render_ui_full_with_eink_color(
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
        eink_mode,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn render_ui_full_with_eink_color(
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
    eink_color: bool,
) -> UiGeometry {
    let effective_eink_color = eink_mode && eink_color;
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
                eink_color: effective_eink_color,
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
    let mut page_up_rect = None;
    let mut page_down_rect = None;
    let space_menu_open = matches!(modal, Modal::SpaceMenu);

    let space_bounds = render_top_tabbar(
        f,
        top_rect,
        world,
        space_menu_open,
        tab_overview_open,
        eink_mode,
        effective_eink_color,
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
        effective_eink_color,
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
            if eink_mode {
                let (pg_up, pg_dn) =
                    render_eink_page_turn_tabs(f, term_rect, mod_rect, dpad_rect, space_slop_rect);
                page_up_rect = pg_up;
                page_down_rect = pg_dn;
            }
        }
        Modal::SpaceMenu => {
            render_space_menu_modal(
                f,
                area,
                top_h,
                world,
                eink_mode,
                effective_eink_color,
                &mut hits,
            );
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
                effective_eink_color,
                &mut hits,
            );
        }
        Modal::Help => {
            render_help_modal(f, area, eink_mode, effective_eink_color, &mut hits);
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
                effective_eink_color,
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
        page_up_rect,
        page_down_rect,
    }
}

// 300 PPI Carta 4-bit neutral LUT tokens (R == G == B, multiples of 0x11)
pub const EINK_PAPER: Color = Color::Rgb(255, 255, 255); // #ffffff
pub const EINK_INK: Color = Color::Rgb(0, 0, 0); // #000000
pub const EINK_INK_MUTED: Color = Color::Rgb(102, 102, 102); // #666666
pub const EINK_INK_PROCESS: Color = Color::Rgb(119, 119, 119); // #777777 (italic AI process/tool rows)
pub const EINK_SURFACE: Color = Color::Rgb(238, 238, 238); // #eeeeee
pub const EINK_PILL_BG: Color = Color::Rgb(221, 221, 221); // #dddddd

// Kaleido Color E-Ink high-contrast semantic overprint inks (>= 8.9:1 on #ffffff)
pub const KALEIDO_RED: Color = Color::Rgb(153, 0, 0); // #990000 (Blocked / Diff Remove / Low Quota)
pub const KALEIDO_GREEN: Color = Color::Rgb(0, 85, 34); // #005522 (Done / Diff Add)
pub const KALEIDO_BLUE: Color = Color::Rgb(0, 51, 136); // #003388 (Working / Diff Hunk)

fn kaleido_state_color(state: AgentState) -> Option<Color> {
    match state {
        AgentState::Blocked => Some(KALEIDO_RED),
        AgentState::Done => Some(KALEIDO_GREEN),
        AgentState::Working => Some(KALEIDO_BLUE),
        AgentState::Idle | AgentState::None => None,
    }
}

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
                .bg(EINK_PAPER)
                .fg(EINK_INK)
                .add_modifier(Modifier::BOLD)
        } else {
            match st {
                AgentState::Blocked | AgentState::Done | AgentState::Working => Style::default()
                    .bg(EINK_PAPER)
                    .fg(EINK_INK)
                    .add_modifier(Modifier::BOLD),
                AgentState::Idle | AgentState::None => {
                    Style::default().bg(EINK_PAPER).fg(EINK_INK_MUTED)
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
    } else if eink_mode {
        let pill_style = if emphasized {
            style
                .add_modifier(Modifier::UNDERLINED)
                .underline_color(EINK_INK)
        } else {
            style
        };
        buf.set_string(x, rect.y, label, pill_style);
    } else {
        let ul_color = if emphasized {
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
    eink_color: bool,
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
    if rect.height == 1 && !eink_mode {
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
            Style::default()
                .bg(if space_menu_open {
                    EINK_PAPER
                } else {
                    EINK_SURFACE
                })
                .fg(EINK_INK)
                .add_modifier(Modifier::BOLD)
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
        if eink_mode
            && eink_color
            && rect.width >= 36
            && matches!(sstate, AgentState::Blocked | AgentState::Done)
        {
            if let Some(sym_fg) = kaleido_state_color(sstate) {
                let prefix_w = format!(" {arrow} ").width() as u16;
                let sym_w = sstate.symbol().width() as u16;
                let label_y = if rect.height >= 3 { rect.y + 1 } else { rect.y };
                if sym_w > 0
                    && prefix_w + sym_w <= space_w
                    && buf
                        .cell((x + prefix_w, label_y))
                        .is_some_and(|c| c.symbol() == sstate.symbol())
                {
                    for dx in 0..sym_w {
                        if let Some(cell) = buf.cell_mut((x + prefix_w + dx, label_y)) {
                            cell.set_fg(sym_fg);
                        }
                    }
                }
            }
        }
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
        if rect.height == 1 && !eink_mode {
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
            if eink_mode && eink_color {
                let prefix = eink_tab_prefix(st, is_active);
                if !prefix.is_empty() {
                    if let Some(badge_fg) = kaleido_state_color(st) {
                        let rel_x_opt = if lbl.starts_with(' ') && lbl[1..].starts_with(prefix) {
                            Some(1u16)
                        } else if lbl.starts_with(prefix) {
                            Some(0u16)
                        } else {
                            None
                        };
                        if let Some(rel_x) = rel_x_opt {
                            let label_y = if rect.height >= 3 { rect.y + 1 } else { rect.y };
                            if buf
                                .cell((x + rel_x, label_y))
                                .is_some_and(|c| c.symbol() == prefix)
                            {
                                for dx in 0..(prefix.width() as u16) {
                                    if let Some(cell) = buf.cell_mut((x + rel_x + dx, label_y)) {
                                        cell.set_fg(badge_fg);
                                    }
                                }
                            }
                        }
                    }
                }
            }
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
            Style::default()
                .bg(if tab_overview_open {
                    EINK_PAPER
                } else {
                    EINK_SURFACE
                })
                .fg(EINK_INK)
                .add_modifier(Modifier::BOLD)
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

fn is_eink_low_quota(remaining_percent: f64, eink_color: bool) -> bool {
    if eink_color {
        remaining_percent < 15.0
    } else {
        remaining_percent <= 15.0
    }
}

fn metric_display_width(
    m: &QuotaMetric,
    bar_w: usize,
    show_detail: bool,
    eink_mode: bool,
    eink_color: bool,
) -> usize {
    let pct = m.remaining_percent.clamp(0.0, 100.0).round() as u64;
    let pct_str = if eink_mode && is_eink_low_quota(m.remaining_percent, eink_color) {
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

#[allow(clippy::too_many_arguments)]
fn render_bottom_status_bar(
    f: &mut Frame,
    rect: Rect,
    focused_quota: Option<&AgentQuota>,
    scrollback_offset: usize,
    copied_flash: bool,
    eink_mode: bool,
    eink_color: bool,
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
                        .map(|m| metric_display_width(m, *bw, *sd, eink_mode, eink_color))
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
                    let low = is_eink_low_quota(m.remaining_percent, eink_color);
                    if eink_color && low {
                        (
                            Style::default().bg(bar_bg).fg(KALEIDO_RED),
                            Style::default()
                                .bg(bar_bg)
                                .fg(KALEIDO_RED)
                                .add_modifier(Modifier::BOLD),
                            "!",
                        )
                    } else {
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
                    }
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

fn rects_intersect(a: Rect, b: Rect) -> bool {
    a.x < b.x.saturating_add(b.width)
        && b.x < a.x.saturating_add(a.width)
        && a.y < b.y.saturating_add(b.height)
        && b.y < a.y.saturating_add(a.height)
}

fn resolve_eink_page_tab_rect(
    term_rect: Rect,
    is_left: bool,
    obstacle: Option<Rect>,
) -> Option<Rect> {
    if term_rect.width < 12 || term_rect.height < 3 {
        return None;
    }
    let tab_w: u16 = if term_rect.width < 48 { 2 } else { 3 };
    let tab_h: u16 = if term_rect.height >= 9 {
        5
    } else if term_rect.height >= 5 {
        3
    } else {
        1
    };
    let x = if is_left {
        term_rect.x
    } else {
        term_rect.x + term_rect.width - tab_w
    };
    let min_y = term_rect.y;
    let max_y = term_rect.y + term_rect.height - tab_h;
    let ideal_y = term_rect.y + (term_rect.height - tab_h) / 2;

    let Some(obs) = obstacle else {
        return Some(Rect {
            x,
            y: ideal_y,
            width: tab_w,
            height: tab_h,
        });
    };

    let mut candidates = [None; 5];
    candidates[0] = Some(ideal_y);
    candidates[1] = obs.y.checked_sub(tab_h + 1).filter(|&y| y >= min_y);
    candidates[2] = obs.y.checked_sub(tab_h).filter(|&y| y >= min_y);
    let below_gap = obs.y.saturating_add(obs.height).saturating_add(1);
    if below_gap <= max_y {
        candidates[3] = Some(below_gap);
    }
    let below_flush = obs.y.saturating_add(obs.height);
    if below_flush <= max_y {
        candidates[4] = Some(below_flush);
    }

    for cand_y in candidates.into_iter().flatten() {
        let rect = Rect {
            x,
            y: cand_y.clamp(min_y, max_y),
            width: tab_w,
            height: tab_h,
        };
        if !rects_intersect(rect, obs) {
            return Some(rect);
        }
    }
    None
}

fn paint_eink_page_tab(buf: &mut Buffer, rect: Rect, is_left: bool) {
    let mid_y = rect.y + rect.height / 2;
    let icon_x = if is_left {
        rect.x
    } else {
        rect.x + rect.width.saturating_sub(1)
    };
    let chevron = if is_left { "‹" } else { "›" };

    for y in rect.y..rect.y.saturating_add(rect.height) {
        for x in rect.x..rect.x.saturating_add(rect.width) {
            if x >= buf.area.width || y >= buf.area.height {
                continue;
            }
            if x == icon_x && y == mid_y {
                let was_wide_lead = buf[(x, y)].symbol().width() == 2;
                let was_wide_cont = x > 0 && buf[(x - 1, y)].symbol().width() == 2;
                if was_wide_cont {
                    let prev = &mut buf[(x - 1, y)];
                    prev.set_symbol(" ");
                }
                let cell = &mut buf[(x, y)];
                cell.set_symbol(chevron);
                cell.set_bg(EINK_SURFACE);
                cell.set_fg(EINK_INK);
                cell.modifier.insert(Modifier::BOLD);
                if was_wide_lead && x + 1 < buf.area.width {
                    let next = &mut buf[(x + 1, y)];
                    next.set_symbol(" ");
                    if x + 1 < rect.x + rect.width {
                        next.set_bg(EINK_SURFACE);
                        if next.fg == EINK_PAPER {
                            next.set_fg(EINK_INK);
                        }
                    }
                }
            } else {
                let cell = &mut buf[(x, y)];
                cell.set_bg(EINK_SURFACE);
                if cell.fg == EINK_PAPER {
                    cell.set_fg(EINK_INK);
                }
            }
        }
    }
}

fn expand_eink_page_touch_rect(
    term_rect: Rect,
    visual: Rect,
    is_left: bool,
    obstacle: Option<Rect>,
    top_obstacle: Option<Rect>,
) -> Rect {
    let max_side_w = (term_rect.width / 5).max(visual.width);
    let desired_w = (term_rect.width / 12).clamp(6, 8);
    let mut touch_w = desired_w.min(max_side_w).max(visual.width);

    let desired_h: u16 = if term_rect.height >= 26 {
        11
    } else if term_rect.height >= 15 {
        9
    } else if term_rect.height >= 9 {
        7
    } else if term_rect.height >= 5 {
        5
    } else {
        visual.height
    };
    let max_h = (term_rect.height * 2 / 5).max(visual.height);
    let target_h = desired_h.min(max_h).max(visual.height);
    let extra_y = target_h - visual.height;
    let mut top = visual.y.saturating_sub(extra_y / 2).max(term_rect.y);
    let mut bot =
        (visual.y + visual.height + (extra_y - extra_y / 2)).min(term_rect.y + term_rect.height);

    let cand_x = |w: u16| {
        if is_left {
            term_rect.x
        } else {
            term_rect.x + term_rect.width - w
        }
    };

    if let Some(slop) = top_obstacle {
        let cand = Rect::new(cand_x(touch_w), top, touch_w, bot.saturating_sub(top));
        if rects_intersect(cand, slop) && slop.y + slop.height <= visual.y {
            top = top.max(slop.y + slop.height);
        }
    }

    if let Some(obs) = obstacle {
        let cand = Rect::new(cand_x(touch_w), top, touch_w, bot.saturating_sub(top));
        if rects_intersect(cand, obs) {
            if obs.y + obs.height <= visual.y {
                top = top.max(obs.y + obs.height);
            } else if obs.y >= visual.y + visual.height {
                bot = bot.min(obs.y);
            } else if is_left && obs.x >= visual.x + visual.width {
                touch_w = obs.x - term_rect.x;
            } else if !is_left && obs.x + obs.width <= visual.x {
                let new_x = obs.x + obs.width;
                touch_w = (term_rect.x + term_rect.width).saturating_sub(new_x);
            }
        }
    }

    Rect::new(
        cand_x(touch_w),
        top,
        touch_w,
        bot.saturating_sub(top).max(visual.height),
    )
}

fn render_eink_page_turn_tabs(
    f: &mut Frame,
    term_rect: Rect,
    mod_rect: Option<Rect>,
    dpad_rect: Option<Rect>,
    space_slop_rect: Option<Rect>,
) -> (Option<Rect>, Option<Rect>) {
    let buf = f.buffer_mut();
    let left_touch = resolve_eink_page_tab_rect(term_rect, true, mod_rect).map(|vis| {
        paint_eink_page_tab(buf, vis, true);
        expand_eink_page_touch_rect(term_rect, vis, true, mod_rect, space_slop_rect)
    });
    let right_touch = resolve_eink_page_tab_rect(term_rect, false, dpad_rect).map(|vis| {
        paint_eink_page_tab(buf, vis, false);
        expand_eink_page_touch_rect(term_rect, vis, false, dpad_rect, None)
    });
    (left_touch, right_touch)
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

#[allow(clippy::too_many_arguments)]
fn render_space_menu_modal(
    f: &mut Frame,
    area: Rect,
    top_h: u16,
    world: &World,
    eink_mode: bool,
    eink_color: bool,
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
                } else if eink_color {
                    kaleido_state_color(sstate).unwrap_or(EINK_INK)
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
    eink_color: bool,
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
                    } else if eink_color {
                        kaleido_state_color(*state).unwrap_or(EINK_INK)
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
    eink_color: bool,
}

fn is_braille_char(s: &str) -> bool {
    let mut chars = s.chars();
    matches!((chars.next(), chars.next()), (Some(ch), None) if ('\u{2800}'..='\u{28FF}').contains(&ch))
}

fn is_arc_or_box_drawing_char(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some(ch), None)
            if ('\u{2500}'..='\u{257F}').contains(&ch)
                || ('\u{25DC}'..='\u{25E1}').contains(&ch)
    )
}

fn has_cardinal_neighbor<F>(
    screen: &vt100::Screen,
    r: u16,
    c: u16,
    rows: u16,
    cols: u16,
    pred: F,
) -> bool
where
    F: Fn(&str) -> bool,
{
    (c > 0 && screen.cell(r, c - 1).is_some_and(|lc| pred(lc.contents())))
        || (c + 1 < cols && screen.cell(r, c + 1).is_some_and(|rc| pred(rc.contents())))
        || (r > 0 && screen.cell(r - 1, c).is_some_and(|uc| pred(uc.contents())))
        || (r + 1 < rows && screen.cell(r + 1, c).is_some_and(|dc| pred(dc.contents())))
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
            if !has_cardinal_neighbor(screen, r, c, rows, cols, is_braille_char) {
                return "◈";
            }
        } else if ('\u{25DC}'..='\u{25E1}').contains(&ch) {
            if !has_cardinal_neighbor(screen, r, c, rows, cols, is_arc_or_box_drawing_char) {
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

fn is_eink_muted_cell(cell: &vt100::Cell) -> bool {
    if cell.dim() {
        return true;
    }
    if cell.bold() {
        return false;
    }
    match cell.fgcolor() {
        vt100::Color::Default => false,
        c => match vt_color_rgb(c) {
            Some((r, g, b)) => {
                let luma = rgb_luma(r, g, b);
                let chroma = r.max(g).max(b) - r.min(g).min(b);
                matches!(c, vt100::Color::Idx(8 | 238..=249))
                    || (chroma <= 42 && (70..=180).contains(&luma))
            }
            None => false,
        },
    }
}

struct ScannedEinkCell {
    col: u16,
    byte_start: usize,
    is_space: bool,
    is_muted: bool,
}

struct ScannedEinkRow {
    text: String,
    cells: Vec<ScannedEinkCell>,
    first_idx: usize,
    last_idx: usize,
    leading_spaces: u16,
    all_non_space_muted: bool,
}

impl ScannedEinkRow {
    fn trimmed(&self) -> (&str, usize) {
        let start = self.cells[self.first_idx].byte_start;
        let end = self
            .cells
            .get(self.last_idx + 1)
            .map_or(self.text.len(), |c| c.byte_start);
        (&self.text[start..end], start)
    }
}

fn scan_eink_row_text(screen: &vt100::Screen, r: u16, cols: u16) -> Option<ScannedEinkRow> {
    let mut text = String::with_capacity(usize::from(cols));
    let mut cells = Vec::with_capacity(usize::from(cols));
    for c in 0..cols {
        let Some(cell) = screen.cell(r, c) else {
            continue;
        };
        if cell.is_wide_continuation() {
            continue;
        }
        let raw_sym = if cell.has_contents() {
            cell.contents()
        } else {
            " "
        };
        let is_space = raw_sym.is_empty() || raw_sym == " ";
        let sym = if is_space { " " } else { raw_sym };
        let byte_start = text.len();
        text.push_str(sym);
        let is_muted = !is_space && is_eink_muted_cell(cell);
        cells.push(ScannedEinkCell {
            col: c,
            byte_start,
            is_space,
            is_muted,
        });
    }
    let first_idx = cells.iter().position(|c| !c.is_space)?;
    let last_idx = cells.iter().rposition(|c| !c.is_space).unwrap_or(first_idx);
    let leading_spaces = cells[first_idx].col;
    let all_non_space_muted = cells[first_idx..=last_idx]
        .iter()
        .filter(|c| !c.is_space)
        .all(|c| c.is_muted);
    Some(ScannedEinkRow {
        text,
        cells,
        first_idx,
        last_idx,
        leading_spaces,
        all_non_space_muted,
    })
}

fn is_structural_box_char(ch: char) -> bool {
    matches!(
        ch,
        '─' | '━'
            | '│'
            | '┃'
            | '╭'
            | '╮'
            | '╰'
            | '╯'
            | '┌'
            | '┐'
            | '└'
            | '┘'
            | '├'
            | '┤'
            | '┬'
            | '┴'
            | '┼'
            | '═'
            | '║'
            | '┏'
            | '┓'
            | '┗'
            | '┛'
    )
}

fn row_has_box_frame(trimmed: &str) -> bool {
    let check_str = trimmed.strip_prefix("└ ").unwrap_or(trimmed);
    if check_str.chars().any(is_structural_box_char) {
        return true;
    }
    (trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() >= 3)
        || ((trimmed.starts_with('|') || trimmed.ends_with('|')) && trimmed.contains(" | "))
}

fn looks_like_numbered_code_line(trimmed: &str) -> bool {
    let bytes = trimmed.as_bytes();
    let mut digits = 0usize;
    while digits < bytes.len() && digits < 6 && bytes[digits].is_ascii_digit() {
        digits += 1;
    }
    if digits == 0 || digits >= bytes.len() {
        return false;
    }
    let next = trimmed[digits..].chars().next();
    matches!(next, Some(' ' | ':' | '│' | '|'))
}

fn has_tui_process_hint(trimmed: &str) -> bool {
    trimmed.contains("(ctrl+o to expand)")
        || trimmed.contains("(ctrl+r to expand)")
        || trimmed.contains("(ctrl+o to show")
        || trimmed.contains("(esc to interrupt)")
        || trimmed.contains("(esc to cancel)")
}

fn is_leading_spinner_status_row(
    screen: &vt100::Screen,
    r: u16,
    rows: u16,
    cols: u16,
    scan: &ScannedEinkRow,
    trimmed: &str,
) -> bool {
    let first_col = scan.cells[scan.first_idx].col;
    let Some(first_cell) = screen.cell(r, first_col) else {
        return false;
    };
    if !first_cell.has_contents() {
        return false;
    }
    let sym = first_cell.contents();
    let mut chars = sym.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else {
        return false;
    };
    let is_spinner = if ('\u{2801}'..='\u{28FF}').contains(&ch) {
        !has_cardinal_neighbor(screen, r, first_col, rows, cols, is_braille_char)
    } else if ('\u{25DC}'..='\u{25E1}').contains(&ch) {
        !has_cardinal_neighbor(screen, r, first_col, rows, cols, is_arc_or_box_drawing_char)
    } else {
        matches!(
            ch,
            '◐' | '◓' | '◑' | '◒' | '◴' | '◷' | '◶' | '◵' | '✻' | '∴'
        )
    };
    if !is_spinner {
        return false;
    }
    let after = &trimmed[ch.len_utf8()..];
    after.starts_with(char::is_whitespace) && after.trim().chars().any(char::is_alphabetic)
}

fn starts_with_tool_call_syntax(rest: &str) -> bool {
    let Some(paren_idx) = rest.find('(') else {
        return false;
    };
    if !(2..=28).contains(&paren_idx) || paren_idx + 1 >= rest.len() {
        return false;
    }
    let name = &rest[..paren_idx];
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn has_muted_parenthetical_tail(scan: &ScannedEinkRow, rest: &str, rest_byte: usize) -> bool {
    let Some(rel_paren) = rest.find(" (") else {
        return false;
    };
    let header_chars = rest[..rel_paren].chars().count();
    if !(1..=32).contains(&header_chars) {
        return false;
    }
    let paren_byte = rest_byte + rel_paren + 1;
    let mut count = 0usize;
    for cell in &scan.cells[scan.first_idx..=scan.last_idx] {
        if cell.byte_start >= paren_byte && !cell.is_space {
            if !cell.is_muted {
                return false;
            }
            count += 1;
        }
    }
    count >= 3
}

fn classify_status_lead_process_row(
    scan: &ScannedEinkRow,
    trimmed: &str,
    trimmed_byte_start: usize,
) -> bool {
    const PROCESS_MARKERS: &[&str] = &["○ ", "● ", "◈ ", "⎿", "↳"];
    const PROCESS_VERBS: &[&str] = &[
        "Used ",
        "Scheduling ",
        "Scheduled ",
        "Reading ",
        "Searching ",
        "Searched ",
        "Running ",
        "Ran ",
        "Editing ",
        "Edited ",
        "Updated ",
        "Writing ",
        "Wrote ",
        "Calling ",
        "Invoking ",
        "Launching ",
        "Subagent ",
        "Explored ",
    ];
    const CODEX_VERBS: &[&str] = &["Ran ", "Explored ", "Searched ", "Edited ", "Updated "];

    if let Some(after) = trimmed.strip_prefix("• ") {
        let rest = after.trim_start();
        if rest.is_empty() {
            return false;
        }
        let rest_byte = trimmed_byte_start + "• ".len() + (after.len() - rest.len());
        return CODEX_VERBS.iter().any(|v| rest.starts_with(v))
            && (scan.all_non_space_muted || has_muted_parenthetical_tail(scan, rest, rest_byte));
    }

    let Some((marker, after)) = PROCESS_MARKERS
        .iter()
        .find_map(|&m| trimmed.strip_prefix(m).map(|rest| (m, rest)))
    else {
        return false;
    };
    let rest = after.trim_start();
    if rest.is_empty() {
        return false;
    }
    if rest.starts_with("Thought for ")
        || rest.starts_with("Thought (")
        || rest == "Thought"
        || rest.starts_with("Thinking")
        || rest.starts_with("Reasoning")
    {
        return true;
    }
    if PROCESS_VERBS.iter().any(|v| rest.starts_with(v))
        || rest.starts_with("Scheduling...")
        || rest.starts_with("Scheduling…")
        || starts_with_tool_call_syntax(rest)
    {
        return true;
    }
    let rest_byte = trimmed_byte_start + marker.len() + (after.len() - rest.len());
    if matches!(marker, "○ " | "● " | "◈ ") && has_muted_parenthetical_tail(scan, rest, rest_byte)
    {
        return true;
    }
    matches!(marker, "⎿" | "↳") && scan.all_non_space_muted
}

fn classify_eink_process_row(
    screen: &vt100::Screen,
    r: u16,
    rows: u16,
    cols: u16,
    prev_wrapped: bool,
    prev_is_process: bool,
) -> bool {
    let Some(scan) = scan_eink_row_text(screen, r, cols) else {
        return false;
    };
    let (trimmed, trimmed_byte_start) = scan.trimmed();
    if trimmed.is_empty() || row_has_box_frame(trimmed) {
        return false;
    }
    if prev_wrapped && prev_is_process {
        return true;
    }
    if prev_is_process
        && !looks_like_numbered_code_line(trimmed)
        && (trimmed.starts_with('⎿')
            || trimmed.starts_with('↳')
            || trimmed.starts_with("└ ")
            || (scan.leading_spaces >= 2 && scan.all_non_space_muted))
    {
        return true;
    }
    if has_tui_process_hint(trimmed) {
        return true;
    }
    if is_leading_spinner_status_row(screen, r, rows, cols, &scan, trimmed) {
        return true;
    }
    if scan.leading_spaces <= 4
        && trimmed
            .strip_prefix("Thought for ")
            .is_some_and(|s| s.starts_with(|c: char| c.is_ascii_digit()))
    {
        return true;
    }
    classify_status_lead_process_row(&scan, trimmed, trimmed_byte_start)
}

fn detect_eink_process_rows(screen: &vt100::Screen, rows: u16, cols: u16) -> Vec<bool> {
    let mut out = vec![false; usize::from(rows)];
    let mut prev_wrapped = false;
    let mut prev_is_process = false;
    for r in 0..rows {
        let is_proc =
            classify_eink_process_row(screen, r, rows, cols, prev_wrapped, prev_is_process);
        out[usize::from(r)] = is_proc;
        prev_wrapped = screen.row_wrapped(r);
        prev_is_process = is_proc;
    }
    out
}

fn vt_row_trimmed_starts_with_fence(screen: &vt100::Screen, r: u16, cols: u16) -> bool {
    scan_eink_row_text(screen, r, cols).is_some_and(|scan| {
        let (trimmed, _) = scan.trimmed();
        trimmed
            .trim_start_matches(['│', '┃', '║', ' '])
            .starts_with("```")
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffHue {
    Red,
    Green,
    BlueCyan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EinkDiffRowKind {
    Add,
    Remove,
    Hunk,
}

impl EinkDiffRowKind {
    fn kaleido_color(self) -> Color {
        match self {
            Self::Add => KALEIDO_GREEN,
            Self::Remove => KALEIDO_RED,
            Self::Hunk => KALEIDO_BLUE,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EinkDiffRowHighlight {
    kind: EinkDiffRowKind,
    marker_start_col: u16,
    marker_end_col_excl: u16,
    side_bar_col: Option<u16>,
    force_paper_bg: bool,
}

impl EinkDiffRowHighlight {
    fn is_marker_col(self, col: u16) -> bool {
        (col >= self.marker_start_col && col < self.marker_end_col_excl)
            || self.side_bar_col == Some(col)
    }
}

fn classify_rgb_diff_hue(r: u8, g: u8, b: u8, is_bg: bool) -> Option<DiffHue> {
    if is_bg {
        let is_red_bg = r >= 32
            && r >= g.saturating_add(14)
            && r >= b.saturating_add(12)
            && (u16::from(g) * 3 <= u16::from(r) * 2
                || (r >= g.saturating_add(16) && b <= g.saturating_add(6)));
        if is_red_bg {
            return Some(DiffHue::Red);
        }
        if g >= 30 && g >= r.saturating_add(12) && g >= b.saturating_add(6) {
            return Some(DiffHue::Green);
        }
        if b >= 36 && b >= r.saturating_add(14) && b >= g {
            return Some(DiffHue::BlueCyan);
        }
        None
    } else {
        let is_red_fg = r >= 85
            && r >= g.saturating_add(32)
            && r >= b.saturating_add(24)
            && u16::from(g) * 3 <= u16::from(r) * 2;
        if is_red_fg {
            return Some(DiffHue::Red);
        }
        if g >= 75 && g >= r.saturating_add(28) && g >= b.saturating_add(18) {
            return Some(DiffHue::Green);
        }
        if b >= 85 && b >= r.saturating_add(25) {
            return Some(DiffHue::BlueCyan);
        }
        None
    }
}

fn classify_vt_diff_hue(c: vt100::Color, is_bg: bool) -> Option<DiffHue> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(1 | 9) => Some(DiffHue::Red),
        vt100::Color::Idx(2 | 10) => Some(DiffHue::Green),
        vt100::Color::Idx(4 | 6 | 12 | 14) => Some(DiffHue::BlueCyan),
        vt100::Color::Idx(i) => {
            let (r, g, b) = xterm256_rgb(i);
            classify_rgb_diff_hue(r, g, b, is_bg)
        }
        vt100::Color::Rgb(r, g, b) => classify_rgb_diff_hue(r, g, b, is_bg),
    }
}

fn is_diff_box_border_sym(s: &str) -> bool {
    matches!(s, "│" | "┃" | "║" | "▎" | "▍" | "▌" | "▋" | "▊" | "▉")
}

fn is_diff_thick_bar_sym(s: &str) -> bool {
    matches!(s, "┃" | "▎" | "▍" | "▌" | "▋" | "▊" | "▉")
}

fn vt_cell_ascii_byte(screen: &vt100::Screen, r: u16, c: u16) -> Option<u8> {
    let cell = screen.cell(r, c)?;
    if cell.is_wide() || cell.is_wide_continuation() {
        return None;
    }
    let s = if cell.has_contents() {
        cell.contents()
    } else {
        " "
    };
    let b = s.as_bytes();
    (b.len() == 1).then_some(b[0])
}

fn vt_row_ascii_slice(screen: &vt100::Screen, r: u16, start_col: u16, end_col: u16) -> String {
    let mut out = String::new();
    for c in start_col..end_col {
        if let Some(cell) = screen.cell(r, c) {
            if cell.is_wide_continuation() {
                continue;
            }
            if cell.has_contents() {
                out.push_str(cell.contents());
            } else {
                out.push(' ');
            }
        }
    }
    out
}

fn looks_like_unified_hunk_header(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("@@ ") else {
        return false;
    };
    let Some(close_idx) = rest.find(" @@") else {
        return false;
    };
    let range = rest[..close_idx].trim();
    range.starts_with('-') && range.contains('+')
}

fn is_diff_metadata_line(s: &str) -> bool {
    const META_PREFIXES: &[&str] = &[
        "index ",
        "new file ",
        "deleted file ",
        "old mode ",
        "new mode ",
        "rename ",
        "similarity ",
        "copy from ",
        "copy to ",
        "Binary files ",
    ];
    META_PREFIXES.iter().any(|p| s.starts_with(p))
}

fn is_decorative_banner_tail(rest: &str) -> bool {
    rest.ends_with("---") || rest.ends_with("+++") || rest.ends_with("===")
}

fn detect_eink_diff_rows(
    screen: &vt100::Screen,
    rows: u16,
    cols: u16,
) -> Vec<Option<EinkDiffRowHighlight>> {
    let mut out: Vec<Option<EinkDiffRowHighlight>> = vec![None; usize::from(rows)];
    let mut in_diff_context = false;
    let mut in_uncolored_hunk = false;
    let mut hunk_base_col: Option<u16> = None;

    for r in 0..rows {
        if r > 0 && screen.row_wrapped(r - 1) {
            if let Some(prev) = out[usize::from(r - 1)] {
                out[usize::from(r)] = Some(EinkDiffRowHighlight {
                    kind: prev.kind,
                    marker_start_col: 0,
                    marker_end_col_excl: 0,
                    side_bar_col: None,
                    force_paper_bg: true,
                });
                continue;
            }
        }

        let mut c = 0u16;
        let mut colored_bar: Option<(u16, DiffHue)> = None;
        while c < cols && vt_cell_ascii_byte(screen, r, c) == Some(b' ') {
            c += 1;
        }
        if c < cols {
            if let Some(cell) = screen.cell(r, c) {
                let sym = if cell.has_contents() {
                    cell.contents()
                } else {
                    ""
                };
                if is_diff_box_border_sym(sym) {
                    let hue = classify_vt_diff_hue(cell.fgcolor(), false)
                        .or_else(|| classify_vt_diff_hue(cell.bgcolor(), true));
                    if is_diff_thick_bar_sym(sym) {
                        if let Some(h @ (DiffHue::Red | DiffHue::Green)) = hue {
                            colored_bar = Some((c, h));
                        }
                    }
                    c += 1;
                    while c < cols && vt_cell_ascii_byte(screen, r, c) == Some(b' ') {
                        c += 1;
                    }
                }
            }
        }

        // Skip up to two optional line-number tokens (`123`, `123:`, `123 │`, `10  11 │`).
        for _ in 0..2 {
            let saved = c;
            let mut digits = 0u16;
            while c < cols
                && digits < 6
                && vt_cell_ascii_byte(screen, r, c).is_some_and(|b| b.is_ascii_digit())
            {
                digits += 1;
                c += 1;
            }
            if digits == 0 {
                c = saved;
                break;
            }
            let mut after_num = c;
            let mut had_sep = false;
            if c < cols {
                let mut scan = c;
                while scan < cols && vt_cell_ascii_byte(screen, r, scan) == Some(b' ') {
                    scan += 1;
                }
                if scan < cols {
                    let sep = screen.cell(r, scan).map_or("", |cell| cell.contents());
                    if matches!(sep, ":" | "|" | "│") {
                        c = scan + 1;
                        after_num = c;
                        had_sep = true;
                    }
                }
            }
            while c < cols && vt_cell_ascii_byte(screen, r, c) == Some(b' ') {
                c += 1;
            }
            let next_b = vt_cell_ascii_byte(screen, r, c);
            if !had_sep && c == after_num && !matches!(next_b, Some(b'+') | Some(b'-')) {
                c = saved;
                break;
            }
        }

        if c >= cols {
            if let Some((bc, hue)) = colored_bar {
                let kind = match hue {
                    DiffHue::Green => EinkDiffRowKind::Add,
                    DiffHue::Red => EinkDiffRowKind::Remove,
                    DiffHue::BlueCyan => EinkDiffRowKind::Hunk,
                };
                in_diff_context = true;
                in_uncolored_hunk = false;
                out[usize::from(r)] = Some(EinkDiffRowHighlight {
                    kind,
                    marker_start_col: bc,
                    marker_end_col_excl: bc + 1,
                    side_bar_col: None,
                    force_paper_bg: true,
                });
            } else if !in_uncolored_hunk {
                in_diff_context = false;
            }
            continue;
        }

        let probe_end = (c + 64).min(cols);
        let tail_ascii = vt_row_ascii_slice(screen, r, c, probe_end);
        let trimmed_end_tail = tail_ascii.trim_end();

        if trimmed_end_tail.starts_with("diff --git ") || trimmed_end_tail.starts_with("diff -u ") {
            in_diff_context = true;
            in_uncolored_hunk = false;
            hunk_base_col = None;
            continue;
        }

        if let Some(rest) = trimmed_end_tail.strip_prefix("@@ ") {
            if let Some(close_rel) = rest.find(" @@") {
                let marker_cell_hue = screen.cell(r, c).and_then(|cell| {
                    classify_vt_diff_hue(cell.fgcolor(), false)
                        .or_else(|| classify_vt_diff_hue(cell.bgcolor(), true))
                });
                if marker_cell_hue == Some(DiffHue::BlueCyan)
                    || looks_like_unified_hunk_header(trimmed_end_tail)
                {
                    let inner_w = rest[..close_rel].width() as u16;
                    let end_col = (c + 3 + inner_w + 3).min(cols);
                    in_diff_context = true;
                    in_uncolored_hunk = true;
                    hunk_base_col = Some(c);
                    out[usize::from(r)] = Some(EinkDiffRowHighlight {
                        kind: EinkDiffRowKind::Hunk,
                        marker_start_col: c,
                        marker_end_col_excl: end_col,
                        side_bar_col: None,
                        force_paper_bg: true,
                    });
                    continue;
                }
            }
        }

        if trimmed_end_tail.starts_with("--- ") || trimmed_end_tail.starts_with("+++ ") {
            let is_add_hdr = trimmed_end_tail.starts_with("+++ ");
            let rest = trimmed_end_tail[4..].trim();
            let decorative_banner = is_decorative_banner_tail(rest);
            let paired_next_add = !is_add_hdr
                && r + 1 < rows
                && !rest.is_empty()
                && !decorative_banner
                && vt_row_ascii_slice(screen, r + 1, c, (c + 8).min(cols)).starts_with("+++ ");
            let paired_next_hunk = paired_next_add
                && r + 2 < rows
                && vt_row_ascii_slice(screen, r + 2, c, (c + 8).min(cols)).starts_with("@@ ");
            let has_path_shape = !rest.is_empty()
                && !decorative_banner
                && (!in_uncolored_hunk
                    && (rest.starts_with("a/")
                        || rest.starts_with("b/")
                        || rest.starts_with("/dev/null")
                        || paired_next_add
                        || in_diff_context)
                    || paired_next_hunk);
            let marker_hue = screen.cell(r, c).and_then(|cell| {
                classify_vt_diff_hue(cell.fgcolor(), false)
                    .or_else(|| classify_vt_diff_hue(cell.bgcolor(), true))
            });
            let color_matches = if is_add_hdr {
                marker_hue == Some(DiffHue::Green)
            } else {
                marker_hue == Some(DiffHue::Red)
            };
            if (!in_uncolored_hunk && color_matches) || has_path_shape {
                in_diff_context = true;
                in_uncolored_hunk = false;
                hunk_base_col = None;
                out[usize::from(r)] = Some(EinkDiffRowHighlight {
                    kind: if is_add_hdr {
                        EinkDiffRowKind::Add
                    } else {
                        EinkDiffRowKind::Remove
                    },
                    marker_start_col: c,
                    marker_end_col_excl: (c + 3).min(cols),
                    side_bar_col: colored_bar.map(|(bc, _)| bc),
                    force_paper_bg: true,
                });
                continue;
            }
        }

        let marker_byte = vt_cell_ascii_byte(screen, r, c);
        if matches!(marker_byte, Some(b'+') | Some(b'-')) {
            let is_add = marker_byte == Some(b'+');
            let want_hue = if is_add { DiffHue::Green } else { DiffHue::Red };
            let next_byte = if c + 1 < cols {
                vt_cell_ascii_byte(screen, r, c + 1)
            } else {
                None
            };
            let same_next = next_byte == marker_byte;
            let all_marker_rule = same_next
                && trimmed_end_tail
                    .chars()
                    .all(|ch| ch == if is_add { '+' } else { '-' } || ch.is_whitespace());
            let decorative_banner = trimmed_end_tail
                .get(4..)
                .is_some_and(|s| is_decorative_banner_tail(s.trim()));

            let cell_opt = screen.cell(r, c);
            let fg_hue = cell_opt.and_then(|cell| classify_vt_diff_hue(cell.fgcolor(), false));
            let bg_hue = cell_opt.and_then(|cell| classify_vt_diff_hue(cell.bgcolor(), true));
            let bar_matches = colored_bar.is_some_and(|(_, h)| h == want_hue);
            let color_matches = fg_hue == Some(want_hue) || bg_hue == Some(want_hue) || bar_matches;
            let uncolored_ok = in_uncolored_hunk
                && hunk_base_col == Some(c)
                && fg_hue.is_none()
                && bg_hue.is_none();
            let prev_was_diff = r > 0
                && matches!(
                    out[usize::from(r - 1)].map(|d| d.kind),
                    Some(EinkDiffRowKind::Add | EinkDiffRowKind::Remove)
                );

            let confirmed = if same_next {
                let uncolored_double_in_hunk = uncolored_ok
                    && !decorative_banner
                    && (!all_marker_rule || trimmed_end_tail.len() >= 4);
                let colored_double = color_matches
                    && (in_diff_context || prev_was_diff)
                    && (!all_marker_rule || in_uncolored_hunk);
                uncolored_double_in_hunk || colored_double
            } else {
                color_matches || uncolored_ok
            };

            if confirmed {
                in_diff_context = true;
                if hunk_base_col != Some(c) {
                    in_uncolored_hunk = false;
                }
                out[usize::from(r)] = Some(EinkDiffRowHighlight {
                    kind: if is_add {
                        EinkDiffRowKind::Add
                    } else {
                        EinkDiffRowKind::Remove
                    },
                    marker_start_col: c,
                    marker_end_col_excl: c + 1,
                    side_bar_col: colored_bar.map(|(bc, _)| bc),
                    force_paper_bg: true,
                });
                continue;
            }
        }

        if let Some((bc, hue)) = colored_bar {
            if matches!(hue, DiffHue::Green | DiffHue::Red) {
                in_diff_context = true;
                in_uncolored_hunk = false;
                out[usize::from(r)] = Some(EinkDiffRowHighlight {
                    kind: if hue == DiffHue::Green {
                        EinkDiffRowKind::Add
                    } else {
                        EinkDiffRowKind::Remove
                    },
                    marker_start_col: bc,
                    marker_end_col_excl: bc + 1,
                    side_bar_col: None,
                    force_paper_bg: true,
                });
                continue;
            }
        }

        if in_uncolored_hunk {
            let base_byte = hunk_base_col.and_then(|bc| vt_cell_ascii_byte(screen, r, bc));
            if !matches!(base_byte, Some(b' ') | Some(b'\\')) {
                in_uncolored_hunk = false;
                hunk_base_col = None;
                if !is_diff_metadata_line(trimmed_end_tail) {
                    in_diff_context = false;
                }
            }
        } else if !is_diff_metadata_line(trimmed_end_tail) {
            in_diff_context = false;
        }
    }

    out
}

fn resolve_eink_vt_bg(cell: &vt100::Cell) -> Color {
    match vt_color_rgb(cell.bgcolor()) {
        None => EINK_PAPER,
        Some((r, g, b)) => match rgb_luma(r, g, b) {
            0..=35 => EINK_PAPER,
            36..=95 => EINK_SURFACE,
            96..=224 => EINK_PILL_BG,
            _ => EINK_PAPER,
        },
    }
}

fn resolve_eink_standard_fg_and_mods(cell: &vt100::Cell, bg_col: Color) -> (Color, Modifier) {
    let is_muted = is_eink_muted_cell(cell);
    // Keep at least >= 4.5:1 contrast when text sits inside a mid-light pill background (#dddddd).
    let fg_col = if is_muted && bg_col != EINK_PILL_BG {
        EINK_INK_MUTED
    } else {
        EINK_INK
    };
    let mut mods = Modifier::empty();
    // Preserve BOLD only on foreground ink/headings so muted text (#666666) stays light.
    if cell.bold() && !is_muted {
        mods |= Modifier::BOLD;
    }
    if cell.italic() {
        mods |= Modifier::ITALIC;
    }
    if cell.underline() {
        mods |= Modifier::UNDERLINED;
    }
    (fg_col, mods)
}

fn resolve_eink_vt_colors_and_mods_with_diff(
    cell: &vt100::Cell,
    selected: bool,
    col: u16,
    row_diff: Option<EinkDiffRowHighlight>,
    row_is_process: bool,
) -> (Color, Color, Modifier) {
    if selected || cell.inverse() {
        return (EINK_PAPER, EINK_INK, Modifier::BOLD);
    }
    if let Some(diff) = row_diff {
        let bg_col = if diff.force_paper_bg {
            EINK_PAPER
        } else {
            resolve_eink_vt_bg(cell)
        };
        if diff.is_marker_col(col) {
            let mut mods = Modifier::BOLD;
            if cell.italic() {
                mods |= Modifier::ITALIC;
            }
            if cell.underline() {
                mods |= Modifier::UNDERLINED;
            }
            return (diff.kind.kaleido_color(), EINK_PAPER, mods);
        }
        let (fg_col, mods) = resolve_eink_standard_fg_and_mods(cell, bg_col);
        return (fg_col, bg_col, mods);
    }
    resolve_eink_vt_colors_and_mods(cell, false, row_is_process)
}

fn resolve_eink_vt_colors_and_mods(
    cell: &vt100::Cell,
    selected: bool,
    row_is_process: bool,
) -> (Color, Color, Modifier) {
    if selected || cell.inverse() {
        return (EINK_PAPER, EINK_INK, Modifier::BOLD);
    }
    let bg_col = resolve_eink_vt_bg(cell);
    if row_is_process {
        let fg_col = if bg_col != EINK_PILL_BG {
            EINK_INK_PROCESS
        } else {
            EINK_INK
        };
        let mut mods = Modifier::ITALIC;
        if cell.underline() {
            mods |= Modifier::UNDERLINED;
        }
        return (fg_col, bg_col, mods);
    }
    let (fg_col, mods) = resolve_eink_standard_fg_and_mods(cell, bg_col);
    (fg_col, bg_col, mods)
}

#[derive(serde::Serialize)]
struct CleanReaderLine {
    r: Vec<(String, u8)>,
}

#[derive(serde::Serialize)]
struct CleanReaderPayload<'a> {
    v: u8,
    title: &'a str,
    anchor: usize,
    lines: Vec<CleanReaderLine>,
}

fn push_reader_run(runs: &mut Vec<(String, u8)>, text: &str, style: u8) {
    if text.is_empty() {
        return;
    }
    if let Some((last_txt, last_style)) = runs.last_mut() {
        if *last_style == style {
            last_txt.push_str(text);
            return;
        }
    }
    runs.push((text.to_string(), style));
}

fn trim_trailing_reader_runs(runs: &mut Vec<(String, u8)>) {
    while let Some((last_text, _)) = runs.last_mut() {
        let trimmed_len = last_text.trim_end().len();
        last_text.truncate(trimmed_len);
        if last_text.is_empty() {
            runs.pop();
        } else {
            break;
        }
    }
}

fn strip_leading_char(runs: &mut Vec<(String, u8)>, ch: char) -> bool {
    let Some((first, _)) = runs.first_mut() else {
        return false;
    };
    if let Some(rest) = first.strip_prefix(ch) {
        *first = rest.to_string();
        if first.is_empty() {
            runs.remove(0);
        }
        return true;
    }
    false
}

fn strip_trailing_box_edge(runs: &mut Vec<(String, u8)>) {
    trim_trailing_reader_runs(runs);
    let Some((last, _)) = runs.last_mut() else {
        return;
    };
    if let Some(rest) = last.strip_suffix(['│', '┃', '║']) {
        *last = rest.to_string();
        if last.is_empty() {
            runs.pop();
        }
        trim_trailing_reader_runs(runs);
    }
}

fn trim_leading_whitespace_runs(runs: &mut Vec<(String, u8)>) {
    while runs
        .first()
        .is_some_and(|(t, _)| t.starts_with(char::is_whitespace))
    {
        let first_ref = &mut runs[0].0;
        let rest = first_ref.trim_start().to_string();
        *first_ref = rest;
        if first_ref.is_empty() {
            runs.remove(0);
        }
    }
}

fn strip_leading_box_rule_chars(runs: &mut Vec<(String, u8)>) {
    while let Some((first, _)) = runs.first_mut() {
        let trimmed = first.trim_start_matches(|c: char| {
            c.is_whitespace() || ('\u{2500}'..='\u{257F}').contains(&c)
        });
        if trimmed.len() == first.len() {
            break;
        }
        *first = trimmed.to_string();
        if first.is_empty() {
            runs.remove(0);
        } else {
            break;
        }
    }
}

fn strip_trailing_box_rule_chars(runs: &mut Vec<(String, u8)>) {
    while let Some((last, _)) = runs.last_mut() {
        let trimmed = last.trim_end_matches(|c: char| {
            c.is_whitespace() || ('\u{2500}'..='\u{257F}').contains(&c)
        });
        if trimmed.len() == last.len() {
            break;
        }
        *last = trimmed.to_string();
        if last.is_empty() {
            runs.pop();
        } else {
            break;
        }
    }
}

fn clean_reader_logical_line(runs: &mut Vec<(String, u8)>) -> Option<Vec<(String, u8)>> {
    trim_trailing_reader_runs(runs);
    if runs.is_empty() {
        return Some(Vec::new());
    }

    let joined: String = runs.iter().map(|(t, _)| t.as_str()).collect();
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        return Some(Vec::new());
    }
    if trimmed
        .chars()
        .all(|c| c.is_whitespace() || ('\u{2500}'..='\u{257F}').contains(&c))
    {
        return None;
    }

    // Strip Unicode vertical card frames (`│ text │`, `┃ text ┃`, `║ text ║`) without touching ASCII `|`,
    // and unwrap titled box top headers like `╭─── Summary ───╮`.
    if trimmed.starts_with(['│', '┃', '║']) {
        trim_leading_whitespace_runs(runs);
        for edge in ['│', '┃', '║'] {
            if strip_leading_char(runs, edge) {
                let _ = strip_leading_char(runs, ' ');
                break;
            }
        }
        strip_trailing_box_edge(runs);
    } else if trimmed.starts_with(['╭', '┌', '┏', '╔']) && trimmed.ends_with(['╮', '┐', '┓', '╗'])
    {
        strip_leading_box_rule_chars(runs);
        strip_trailing_box_rule_chars(runs);
    }

    let remaining_blank = runs.iter().all(|(t, _)| t.trim().is_empty());
    if remaining_blank {
        Some(Vec::new())
    } else {
        Some(std::mem::take(runs))
    }
}

fn is_reader_fence_line(runs: &[(String, u8)]) -> bool {
    let joined: String = runs.iter().map(|(t, _)| t.as_str()).collect();
    joined.trim_start().starts_with("```")
}

/// Extract clean logical lines (joining soft-wrapped rows across scrollback + live screen)
/// with 3-tier E-Ink style codes (`0` = body `#000000`, `1` = bold heading/emphasis,
/// `2` = muted process `#666666`), stripped of Unicode TUI box frames and bounded for OSC 777.
pub fn extract_clean_reader_json(screen: &mut vt100::Screen, title: &str) -> String {
    const MAX_SCAN_ROWS: usize = 420;
    const MAX_JSON_BYTES: usize = 48_000;

    let (rows_nz, cols_nz) = screen.size();
    let rows_u16 = rows_nz.get();
    let rows = usize::from(rows_u16);
    let cols = cols_nz.get();
    let total_sb = screen.scrollback_len();
    let hist_rows = total_sb.min(MAX_SCAN_ROWS.saturating_sub(rows));
    let orig_sb = screen.scrollback();

    struct RawLogical {
        in_live: bool,
        runs: Vec<(String, u8)>,
    }

    struct ReaderProcessTracker {
        prev_wrapped: bool,
        prev_is_process: bool,
        in_code_fence: bool,
    }

    let mut logical_lines: Vec<RawLogical> = Vec::new();
    let mut cur_runs: Vec<(String, u8)> = Vec::new();
    let mut cur_in_live = false;
    let mut proc_tracker = ReaderProcessTracker {
        prev_wrapped: false,
        prev_is_process: false,
        in_code_fence: false,
    };

    let append_visible_row = |scr: &vt100::Screen,
                              r: u16,
                              in_live: bool,
                              cur_runs: &mut Vec<(String, u8)>,
                              cur_in_live: &mut bool,
                              proc_state: &mut ReaderProcessTracker,
                              out: &mut Vec<RawLogical>| {
        if cur_runs.is_empty() && in_live {
            *cur_in_live = true;
        }
        let row_is_fence =
            !proc_state.prev_wrapped && vt_row_trimmed_starts_with_fence(scr, r, cols);
        if row_is_fence {
            proc_state.in_code_fence = !proc_state.in_code_fence;
        }
        let row_is_process = !row_is_fence
            && !proc_state.in_code_fence
            && classify_eink_process_row(
                scr,
                r,
                rows_u16,
                cols,
                proc_state.prev_wrapped,
                proc_state.prev_is_process,
            );
        let mut prev_was_wide = false;
        let mut prev_col: u16 = 0;
        for col in 0..cols {
            if prev_was_wide {
                prev_was_wide = false;
                continue;
            }
            let Some(cell) = scr.cell(r, col) else {
                continue;
            };
            prev_was_wide = cell.is_wide();
            if cell.has_contents() {
                let style = if row_is_process || cell.dim() {
                    2
                } else if cell.bold() {
                    1
                } else if is_eink_muted_cell(cell) {
                    2
                } else {
                    0
                };
                if col > prev_col {
                    let gap_style = cur_runs.last().map_or(style, |r| r.1);
                    push_reader_run(
                        cur_runs,
                        &" ".repeat(usize::from(col - prev_col)),
                        gap_style,
                    );
                }
                prev_col = col + if cell.is_wide() { 2 } else { 1 };
                push_reader_run(cur_runs, cell.contents(), style);
            }
        }
        proc_state.prev_wrapped = scr.row_wrapped(r);
        proc_state.prev_is_process = row_is_process;
        if !scr.row_wrapped(r) {
            out.push(RawLogical {
                in_live: *cur_in_live || in_live,
                runs: std::mem::take(cur_runs),
            });
            *cur_in_live = false;
        }
    };

    let mut cur_abs = total_sb.saturating_sub(hist_rows);
    while cur_abs < total_sb {
        let sb = total_sb - cur_abs;
        screen.set_scrollback(sb);
        let take = (total_sb - cur_abs).min(rows);
        for r in 0..take {
            append_visible_row(
                screen,
                r as u16,
                false,
                &mut cur_runs,
                &mut cur_in_live,
                &mut proc_tracker,
                &mut logical_lines,
            );
        }
        cur_abs += take;
    }

    screen.set_scrollback(0);
    for r in 0..rows {
        append_visible_row(
            screen,
            r as u16,
            true,
            &mut cur_runs,
            &mut cur_in_live,
            &mut proc_tracker,
            &mut logical_lines,
        );
    }
    if !cur_runs.is_empty() {
        logical_lines.push(RawLogical {
            in_live: cur_in_live,
            runs: cur_runs,
        });
    }
    screen.set_scrollback(orig_sb);

    struct CleanLine {
        in_live: bool,
        runs: Vec<(String, u8)>,
    }
    let mut cleaned: Vec<CleanLine> = Vec::with_capacity(logical_lines.len());
    let mut prev_blank = true;
    for mut raw in logical_lines {
        let Some(runs) = clean_reader_logical_line(&mut raw.runs) else {
            continue;
        };
        let is_blank = runs.is_empty();
        if is_blank && prev_blank {
            continue;
        }
        prev_blank = is_blank;
        cleaned.push(CleanLine {
            in_live: raw.in_live,
            runs,
        });
    }
    while cleaned.last().is_some_and(|l| l.runs.is_empty()) {
        cleaned.pop();
    }

    let mut start_idx = 0usize;
    loop {
        let mut inside_dropped_fence = false;
        for dropped in &cleaned[..start_idx] {
            if is_reader_fence_line(&dropped.runs) {
                inside_dropped_fence = !inside_dropped_fence;
            }
        }
        let slice = &cleaned[start_idx..];
        let mut anchor = slice
            .iter()
            .position(|l| l.in_live)
            .unwrap_or_else(|| slice.len().saturating_sub(1));
        let mut lines_out = Vec::with_capacity(slice.len() + usize::from(inside_dropped_fence));
        if inside_dropped_fence {
            lines_out.push(CleanReaderLine {
                r: vec![("```".to_string(), 0)],
            });
            anchor += 1;
        }
        lines_out.extend(slice.iter().map(|l| CleanReaderLine { r: l.runs.clone() }));
        let payload = CleanReaderPayload {
            v: 1,
            title,
            anchor,
            lines: lines_out,
        };
        let json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
        if json.len() <= MAX_JSON_BYTES || start_idx + 1 >= cleaned.len() {
            return json;
        }
        let step = ((cleaned.len() - start_idx) / 8).max(1);
        start_idx = (start_idx + step).min(cleaned.len() - 1);
    }
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

#[cfg(test)]
pub fn capture_eink_screen_snapshot(
    screen: &vt100::Screen,
    area: Rect,
    scrollback_offset: usize,
) -> EinkScreenSnapshot {
    capture_eink_screen_snapshot_with_color(screen, area, scrollback_offset, false)
}

pub fn capture_eink_screen_snapshot_with_color(
    screen: &vt100::Screen,
    area: Rect,
    scrollback_offset: usize,
    eink_color: bool,
) -> EinkScreenSnapshot {
    let (scr_cols, scr_rows) = {
        let (r, c) = screen.size();
        (c.get(), r.get())
    };
    let (rows, cols) = vt_render_dims((scr_cols, scr_rows), area);
    let diff_rows = if eink_color {
        Some(detect_eink_diff_rows(screen, rows, cols))
    } else {
        None
    };
    let process_rows = detect_eink_process_rows(screen, rows, cols);
    let mut cells = Vec::with_capacity(usize::from(rows) * usize::from(cols));
    for r in 0..rows {
        let row_diff = diff_rows
            .as_ref()
            .and_then(|rows_vec| rows_vec[usize::from(r)]);
        let row_is_process = process_rows[usize::from(r)];
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
            let (fg, bg, mods) = if eink_color {
                resolve_eink_vt_colors_and_mods_with_diff(cell, false, c, row_diff, row_is_process)
            } else {
                resolve_eink_vt_colors_and_mods(cell, false, row_is_process)
            };
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
        let diff_rows = if self.eink_mode && self.eink_color {
            Some(detect_eink_diff_rows(self.screen, rows, cols))
        } else {
            None
        };
        let process_rows = if self.eink_mode {
            Some(detect_eink_process_rows(self.screen, rows, cols))
        } else {
            None
        };
        let active_sel = self.selection.filter(|s| s.is_non_empty());
        let sel_bounds = active_sel.map(|s| s.normalized());

        for r in 0..rows {
            let row_diff = diff_rows
                .as_ref()
                .and_then(|rows_vec| rows_vec[usize::from(r)]);
            let row_is_process = process_rows
                .as_ref()
                .is_some_and(|rows_vec| rows_vec[usize::from(r)]);
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
                    let (fg_col, bg_col, mods) = if self.eink_color {
                        resolve_eink_vt_colors_and_mods_with_diff(
                            cell,
                            selected,
                            c,
                            row_diff,
                            row_is_process,
                        )
                    } else {
                        resolve_eink_vt_colors_and_mods(cell, selected, row_is_process)
                    };
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

fn render_help_modal(
    f: &mut Frame,
    area: Rect,
    eink_mode: bool,
    eink_color: bool,
    hits: &mut Vec<HitRegion>,
) {
    hits.clear();
    let popup = centered_rect(74, 34, area);
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

    // Reflow the 18 touch quick-action buttons across available inner width so narrow mobile
    // viewports wrap buttons onto additional rows instead of dropping right-hand columns.
    let compact_btns = inner.width < 58;
    let (eink_wide, eink_short) = if eink_mode {
        ("  ◐ E-Ink: ON   ", " ◐E:ON ")
    } else {
        ("  ◐ E-Ink: OFF  ", " ◐E:OF ")
    };
    let (kaleido_wide, kaleido_short) = if eink_mode && eink_color {
        ("  ◈ Kaleido: ON ", " ◈K:ON ")
    } else {
        ("  ◈ Kaleido: OFF", " ◈K:OF ")
    };
    let quick_actions: [(&str, &str, HitTarget); 18] = [
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
            kaleido_wide,
            kaleido_short,
            HitTarget::TriggerAction(Action::ToggleEinkColor),
        ),
        (
            "  📖 Book View  ",
            " 📖Book ",
            HitTarget::TriggerAction(Action::OpenBookReader),
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
        Line::from("  Top Bar [ ⏏ ]            Detach hct (keep sessions running) (^b q)"),
        Line::from("  Top Bar [ ▾ Space ]      Open Space popover menu / + New Space"),
        Line::from("  Top Bar [ Tab ] / [ + ]  Switch directly to Tab or add a new Tab"),
        Line::from("  Left Floating Cross (+)  Tap Esc / Ctl / ^B / Alt / Tab (drag to move)"),
        Line::from("  Right Floating Cross (+) Tap ↑ / ← / ⎘ (Paste) / → / ↓ (drag to move)"),
        Line::from("  Clipboard Copy / Paste   Drag text to copy (OSC 52); Tap ⎘ to paste"),
        Line::from("  Next / Prev Tab          ^b n / ^b p, Alt+Right / Left, or [‹] / [›]"),
        Line::from("  Switch Tab 1..9          ^b 1..9       or  Alt+1..9"),
        Line::from("  New Tab in Space         ^b c          or  Tap [ + ] on top tab bar"),
        Line::from("  Rename / Close Tab       ^b , (rename) /   ^b x (close)"),
        Line::from("  Next / Prev Space        ^b Down / Up  or  Alt+Down / Alt+Up"),
        Line::from("  New / Rename Space       ^b C (new)    /   ^b $ (rename)"),
        Line::from("  Toggle Space Menu        ^b b          or  Tap [ ▾ Space ] on top bar"),
        Line::from("  Quick Fuzzy Switcher     ^b w          or  ^b f"),
        Line::from("  Jump Next Alert Agent    ^b a          (◉ blocked / ✓ done)"),
        Line::from("  Scrollback / History     ^b [          or  Tap [▼ BOT] on status bar"),
        Line::from("  Page Up / Down           ^b PgUp / PgDn / Shift+PgUp (or in [ ? ])"),
        Line::from("  E-Ink / Clean Redraw     ^b e (E-Ink)  /   ^b r (clean redraw)"),
        Line::from("  Kaleido Color Accent     ^b E (95% mono + 5% overprint accents)"),
        Line::from("  Book Reader (ShellDeck)  ^b v / ^b B   or  Tap [ 📖 Book View ]"),
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
            let active_toggle = matches!(target, HitTarget::TriggerAction(Action::ToggleEink))
                || (eink_color
                    && matches!(target, HitTarget::TriggerAction(Action::ToggleEinkColor)));
            if active_toggle {
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
    eink_color: bool,
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
                let sym_style = if !is_sel && eink_color {
                    row_style
                        .fg(kaleido_state_color(it.state).unwrap_or(EINK_INK))
                        .add_modifier(Modifier::BOLD)
                } else {
                    row_style.add_modifier(Modifier::BOLD)
                };
                let line = Line::from(vec![
                    Span::styled(if is_sel { " ▸ " } else { "   " }, row_style),
                    Span::styled(format!("{} ", it.state.symbol()), sym_style),
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
                    eink_color: false,
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
                    eink_color: false,
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
                    eink_color: false,
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
                    eink_color: false,
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
            Color::Rgb(r, g, b) => r == g && g == b && matches!(r, 0 | 102 | 119 | 221 | 238 | 255),
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
                "◜ CornerArc\x1b[7;21H◜─Box\x1b[8;21H│",
                "\x1b[9;1HSeeThrough",
                "\x1b[11;1H中A\x1b[11;99H中",
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
        let mut page_up_out = None;
        let mut page_down_out = None;
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
                page_up_out = geom.page_up_rect;
                page_down_out = geom.page_down_rect;
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        assert_strict_eink_buffer(buf);

        let top_row: String = (0..100).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(top_row.contains("!1:"));
        assert!(top_row.contains("*2:"));
        assert!(top_row.contains("✓3:"));
        let active_byte = top_row.find("4:sh-idl").expect("active tab in top row");
        let active_x = top_row[..active_byte].chars().count() as u16;
        assert_eq!(buf[(active_x, 0)].bg, EINK_PAPER);
        assert_eq!(buf[(active_x, 0)].fg, EINK_INK);
        assert!(buf[(active_x, 0)]
            .modifier
            .contains(Modifier::UNDERLINED | Modifier::BOLD));
        let inactive_byte = top_row
            .find("!1:")
            .expect("inactive blocked tab in top row");
        let inactive_x = top_row[..inactive_byte].chars().count() as u16;
        assert_eq!(buf[(inactive_x, 0)].bg, EINK_PAPER);
        assert!(!buf[(inactive_x, 0)].modifier.contains(Modifier::UNDERLINED));

        let bot_row: String = (0..100).map(|x| buf[(x, 23)].symbol()).collect();
        assert!(bot_row.contains("12%!"));
        assert!(bot_row.contains('█'));
        assert!(bot_row.contains('─'));
        assert!(!bot_row.contains('░'));
        assert!(!bot_row.contains("▲Pg"));
        assert!(!bot_row.contains("▼Pg"));
        assert!(!geom_hits.iter().any(|h| matches!(
            h.target,
            HitTarget::TriggerAction(Action::PageUp | Action::PageDown)
        )));
        assert_eq!(page_up_out, Some(Rect::new(0, 8, 8, 7)));
        assert_eq!(page_down_out, Some(Rect::new(92, 8, 8, 7)));
        assert!(geom_hits
            .iter()
            .any(|h| h.target == HitTarget::ExitScrollback));

        // Left & right vertically-centered see-through book page-turn tabs preserve text
        // outside the centered chevron and sanitize half-wide CJK glyphs under the chevron,
        // while keeping visual width at 3 cols even with the 8-col expanded touch hit zone.
        assert_eq!(buf[(0, 9)].symbol(), "S");
        assert_eq!(buf[(0, 9)].bg, EINK_SURFACE);
        assert_eq!(buf[(1, 9)].symbol(), "e");
        assert_eq!(buf[(1, 9)].bg, EINK_SURFACE);
        assert_eq!(buf[(3, 9)].bg, EINK_PAPER);
        assert_eq!(buf[(0, 11)].symbol(), "‹");
        assert_eq!(buf[(0, 11)].bg, EINK_SURFACE);
        assert_eq!(buf[(0, 11)].fg, EINK_INK);
        assert_eq!(buf[(1, 11)].symbol(), " ");
        assert_eq!(buf[(1, 11)].bg, EINK_SURFACE);
        assert_eq!(buf[(2, 11)].symbol(), "A");
        assert_eq!(buf[(2, 11)].bg, EINK_SURFACE);
        assert_eq!(buf[(98, 11)].symbol(), " ");
        assert_eq!(buf[(98, 11)].bg, EINK_SURFACE);
        assert_eq!(buf[(99, 11)].symbol(), "›");
        assert_eq!(buf[(99, 11)].bg, EINK_SURFACE);

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
        // Isolated arc spinner freezes to ◈ while decorative corner abutting box-drawing survives.
        assert_eq!(buf[(0, 7)].symbol(), "◈");
        assert_eq!(buf[(20, 7)].symbol(), "◜");
        assert_eq!(buf[(20, 8)].symbol(), "│");

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
                    .any(|h| h.target == HitTarget::TriggerAction(Action::OpenBookReader)));
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

        // Small narrow mobile viewport (40x24): slimmer 2-col visual bookmark with 6-col
        // expanded touch zones leaving the central 70% of the viewport untouched.
        let mut narrow_term = Terminal::new(TestBackend::new(40, 24)).unwrap();
        let mut narrow_up = None;
        let mut narrow_dn = None;
        narrow_term
            .draw(|f| {
                let geom = render_ui_full(
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
                    true,
                );
                narrow_up = geom.page_up_rect;
                narrow_dn = geom.page_down_rect;
            })
            .unwrap();
        let nbuf = narrow_term.backend().buffer();
        assert_eq!(nbuf[(0, 9)].bg, EINK_SURFACE);
        assert_eq!(nbuf[(1, 9)].bg, EINK_SURFACE);
        assert_eq!(nbuf[(2, 9)].bg, EINK_PAPER);
        assert_eq!(nbuf[(38, 11)].bg, EINK_SURFACE);
        assert_eq!(nbuf[(39, 11)].symbol(), "›");
        assert_eq!(nbuf[(37, 11)].bg, EINK_PAPER);
        assert_eq!(narrow_up, Some(Rect::new(0, 8, 6, 8)));
        assert_eq!(narrow_dn, Some(Rect::new(34, 8, 6, 8)));
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

        // Arc spinner sequence (`◜◠◝◞◡◟`) freezes to `◈` when isolated so snapshots match,
        // while adjacent arc/box-drawing decorative corners preserve their original glyph.
        let mut arc_parser = vt100::Parser::new(nz(4), nz(30), 0);
        arc_parser.process("\x1b[?25l\x1b[H◜ Thinking...".as_bytes());
        let arc_snap1 = capture_eink_screen_snapshot(arc_parser.screen(), area, 0);
        for glyph in ["◠", "◝", "◞", "◡", "◟"] {
            arc_parser.process(format!("\x1b[H{glyph} Thinking...").as_bytes());
            let next_snap = capture_eink_screen_snapshot(arc_parser.screen(), area, 0);
            assert_eq!(arc_snap1, next_snap);
        }
        arc_parser.process("\x1b[2;1H◜─Box".as_bytes());
        let corner_snap = capture_eink_screen_snapshot(arc_parser.screen(), area, 0);
        assert_ne!(arc_snap1, corner_snap);
        assert_eq!(
            resolve_eink_cell_symbol(arc_parser.screen(), 1, 0, 4, 30, "◜"),
            "◜"
        );
        // Adjacent arc (`◜◠`) and vertical box-drawing neighbor (`◜` above `│`) also survive.
        arc_parser.process("\x1b[3;1H◜◠\x1b[3;10H◜\x1b[4;10H│".as_bytes());
        assert_eq!(
            resolve_eink_cell_symbol(arc_parser.screen(), 2, 0, 4, 30, "◜"),
            "◜"
        );
        assert_eq!(
            resolve_eink_cell_symbol(arc_parser.screen(), 2, 1, 4, 30, "◠"),
            "◠"
        );
        assert_eq!(
            resolve_eink_cell_symbol(arc_parser.screen(), 2, 9, 4, 30, "◜"),
            "◜"
        );
    }

    #[test]
    fn help_modal_shortcut_lines_fit_80_col_terminal_without_clipping() {
        let world = World::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 36)).unwrap();
        terminal
            .draw(|f| {
                let _ = render_ui_full(
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
                    None,
                    false,
                    None,
                    false,
                    None,
                    true,
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..36)
            .map(|y| {
                let mut line = String::new();
                let mut x = 0u16;
                while x < 80 {
                    let sym = buf[(x, y)].symbol();
                    line.push_str(sym);
                    x += sym.width().max(1) as u16;
                }
                line
            })
            .collect();
        let joined = rows.join("\n");
        assert!(joined.contains("Detach hct (keep sessions running) (^b q)"));
        assert!(joined.contains("Open Space popover menu / + New Space"));
        assert!(joined.contains("Alt+Right / Left, or [‹] / [›]"));
        assert!(joined.contains("^b PgUp / PgDn / Shift+PgUp (or in [ ? ])"));
        assert!(joined.contains("Tap [ 📖 Book View ]"));
        assert!(joined.contains("Tap outside, [ ✕ ], Esc, Enter, or ? to close"));
    }

    #[test]
    fn eink_vt_screen_enforces_three_tier_hierarchy_for_process_body_and_bold() {
        let nz = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let mut parser = vt100::Parser::new(nz(4), nz(50), 0);
        // Line 0: Bold + dim process (stays muted #666666 without BOLD), plain body, pure bold heading,
        //         bold mid-gray #94a3b8 heading (promoted to EINK_INK + BOLD), and non-bold #94a3b8 (muted).
        // Line 1: Dim inside mid-light pill bg (~#d4d4d4 -> EINK_PILL_BG) promotes to EINK_INK for contrast.
        parser.process(
            b"\x1b[1;2mstep\x1b[0m body \x1b[1mTitle\x1b[0m \x1b[1;38;2;148;163;184mSlateH\x1b[0m \x1b[38;2;148;163;184mslate\x1b[0m\r\n\x1b[2;48;2;210;210;210mpill\x1b[0m",
        );

        let backend = TestBackend::new(50, 4);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            f.render_widget(
                VtScreenWidget {
                    screen: parser.screen(),
                    selection: None,
                    eink_mode: true,
                    eink_color: false,
                },
                f.area(),
            );
        })
        .unwrap();

        let buf = term.backend().buffer();
        // Process text (#666666, non-bold, non-DIM)
        assert_eq!(buf[(0, 0)].symbol(), "s");
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(102, 102, 102));
        assert!(!buf[(0, 0)].modifier.contains(Modifier::BOLD));
        assert!(!buf[(0, 0)].modifier.contains(Modifier::DIM));

        // Plain body (#000000, non-bold)
        assert_eq!(buf[(5, 0)].symbol(), "b");
        assert_eq!(buf[(5, 0)].fg, EINK_INK);
        assert!(!buf[(5, 0)].modifier.contains(Modifier::BOLD));

        // Heading / emphasis (#000000 + BOLD)
        assert_eq!(buf[(10, 0)].symbol(), "T");
        assert_eq!(buf[(10, 0)].fg, EINK_INK);
        assert!(buf[(10, 0)].modifier.contains(Modifier::BOLD));

        // Bold mid-gray (#94a3b8 without DIM) keeps EINK_INK + BOLD heading hierarchy
        assert_eq!(buf[(16, 0)].symbol(), "S");
        assert_eq!(buf[(16, 0)].fg, EINK_INK);
        assert!(buf[(16, 0)].modifier.contains(Modifier::BOLD));

        // Non-bold mid-gray (#94a3b8) stays muted #666666 without BOLD
        assert_eq!(buf[(23, 0)].symbol(), "s");
        assert_eq!(buf[(23, 0)].fg, EINK_INK_MUTED);
        assert!(!buf[(23, 0)].modifier.contains(Modifier::BOLD));

        // Muted on EINK_PILL_BG promotes to EINK_INK
        assert_eq!(buf[(0, 1)].symbol(), "p");
        assert_eq!(buf[(0, 1)].bg, EINK_PILL_BG);
        assert_eq!(buf[(0, 1)].fg, EINK_INK);
    }

    #[test]
    fn extract_clean_reader_json_joins_cjk_wraps_strips_boxes_and_restores_scrollback() {
        let nz = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let mut parser = vt100::Parser::new(nz(4), nz(10), 40);
        // Emit scrollback rows (with box frame and wrapped CJK text), then live screen rows.
        parser.process("┌────────┐\r\n".as_bytes());
        parser.process("\x1b[2m思考过程\x1b[0m\r\n".as_bytes());
        parser.process("└────────┘\r\n".as_bytes());
        parser.process("中文连续文本测试换行\r\n".as_bytes());
        parser.process("│ \x1b[1m# 结论\x1b[0m │\r\n".as_bytes());
        parser.process("正文已完成".as_bytes());

        parser.screen_mut().set_scrollback(2);
        let json_str = extract_clean_reader_json(parser.screen_mut(), "agy-1");
        // Caller's scrollback offset is preserved.
        assert_eq!(parser.screen().scrollback(), 2);

        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["v"], 1);
        assert_eq!(parsed["title"], "agy-1");

        let lines = parsed["lines"].as_array().unwrap();
        let joined_lines: Vec<String> = lines
            .iter()
            .map(|l| {
                l["r"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|run| run[0].as_str().unwrap())
                    .collect::<String>()
            })
            .collect();

        // Box horizontal rules removed; CJK soft wrap rejoined without bogus space; │ # 结论 │ stripped cleanly.
        assert!(joined_lines.iter().any(|s| s == "思考过程"));
        assert!(joined_lines.iter().any(|s| s == "中文连续文本测试换行"));
        assert!(joined_lines.iter().any(|s| s == "# 结论"));
        assert!(joined_lines.iter().any(|s| s == "正文已完成"));
        assert!(!joined_lines
            .iter()
            .any(|s| s.contains('┌') || s.contains('└')));

        // Anchor points inside bounds to the start of the live screen slice.
        let anchor = parsed["anchor"].as_u64().unwrap() as usize;
        assert!(anchor < lines.len());

        // Style codes: 2 = muted for "思考过程", 1 = bold for "# 结论".
        let proc_line = lines.iter().find(|l| l["r"][0][0] == "思考过程").unwrap();
        assert_eq!(proc_line["r"][0][1], 2);

        let bold_line = lines.iter().find(|l| l["r"][0][0] == "# 结论").unwrap();
        assert_eq!(bold_line["r"][0][1], 1);
    }

    #[test]
    fn extract_clean_reader_json_handles_indented_colored_box_borders_slate_muted_and_fence_parity()
    {
        let nz = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let mut parser = vt100::Parser::new(nz(8), nz(60), 420);
        parser.process("╭─── Summary ───╮\r\n".as_bytes());
        // Leading spaces in default style followed by colored `│` border + bold content
        parser.process(
            "\x1b[2C\x1b[90m│\x1b[0m \x1b[1m# 结论\x1b[0m \x1b[90m│\x1b[0m\r\n".as_bytes(),
        );
        // Slate cool-gray muted (#94a3b8) vs bright white (#ffffff) and blue (#58a6ff)
        parser.process(
            "\x1b[38;2;148;163;184mslate_step\x1b[0m \x1b[38;2;255;255;255mwhite_body\x1b[0m \x1b[1;38;2;88;166;255mblue_bold\x1b[0m\r\n"
                .as_bytes(),
        );
        // Very long history (> 48KB) containing a code fence whose opening is trimmed while closing stays
        parser.process("```rust\r\n".as_bytes());
        let filler = "let value_xyz_0123456789_abcdef_0123456789 = 42;\r\n";
        for i in 0..360 {
            parser.process(format!("// line {i:03} {filler}").as_bytes());
        }
        parser.process("```\r\n".as_bytes());
        parser.process("# Final After Long Code\r\n".as_bytes());

        let json_str = extract_clean_reader_json(parser.screen_mut(), "fence-check");
        assert!(json_str.len() <= 48_000);
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let lines = parsed["lines"].as_array().unwrap();
        let joined_lines: Vec<String> = lines
            .iter()
            .map(|l| {
                l["r"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|run| run[0].as_str().unwrap())
                    .collect::<String>()
            })
            .collect();

        // Count ``` fence toggles — parity must be even (synthetic opening fence added if trimmed mid-block)
        let fence_count = joined_lines
            .iter()
            .filter(|s| s.trim_start().starts_with("```"))
            .count();
        assert_eq!(
            fence_count % 2,
            0,
            "fence parity must remain balanced: {joined_lines:?}"
        );
        assert!(joined_lines.iter().any(|s| s == "# Final After Long Code"));

        // Verify indented colored box border + titled box rule + slate vs bright white/blue/bold-slate styles on small screen
        let mut small = vt100::Parser::new(nz(6), nz(80), 20);
        small.process("╭─── Summary ───╮\r\n".as_bytes());
        small.process(
            "\x1b[2C\x1b[90m│\x1b[0m \x1b[1m# 结论\x1b[0m \x1b[90m│\x1b[0m\r\n".as_bytes(),
        );
        small.process(
            "\x1b[38;2;148;163;184mslate_step\x1b[0m \x1b[38;2;255;255;255mwhite_body\x1b[0m \x1b[1;38;2;88;166;255mblue_bold\x1b[0m \x1b[1;38;2;148;163;184mbold_slate\x1b[0m \x1b[1;2;38;2;148;163;184mdim_slate\x1b[0m"
                .as_bytes(),
        );
        let small_json: serde_json::Value =
            serde_json::from_str(&extract_clean_reader_json(small.screen_mut(), "small")).unwrap();
        let small_lines = small_json["lines"].as_array().unwrap();
        let small_text: Vec<String> = small_lines
            .iter()
            .map(|l| {
                l["r"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|run| run[0].as_str().unwrap())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(small_text[0], "Summary");
        assert_eq!(small_text[1], "# 结论");
        let style_line = small_lines[2]["r"].as_array().unwrap();
        assert_eq!(style_line[0][0], "slate_step");
        assert_eq!(style_line[0][1], 2);
        assert!(style_line
            .iter()
            .any(|r| r[0].as_str().unwrap().contains("white_body") && r[1] == 0));
        assert!(style_line
            .iter()
            .any(|r| r[0].as_str().unwrap().contains("blue_bold") && r[1] == 1));
        assert!(style_line
            .iter()
            .any(|r| r[0].as_str().unwrap().contains("bold_slate") && r[1] == 1));
        assert!(style_line
            .iter()
            .any(|r| r[0].as_str().unwrap().contains("dim_slate") && r[1] == 2));

        // When live screen is blank after scrollback, anchor points to the tail of scrollback rather than line 0.
        let mut blank_live = vt100::Parser::new(nz(3), nz(40), 20);
        blank_live.process("older turn 1\r\nolder turn 2\r\n\r\n\r\n\r\n".as_bytes());
        let blank_json: serde_json::Value = serde_json::from_str(&extract_clean_reader_json(
            blank_live.screen_mut(),
            "blank-live",
        ))
        .unwrap();
        let blank_lines = blank_json["lines"].as_array().unwrap();
        assert_eq!(blank_lines.len(), 2);
        assert_eq!(blank_json["anchor"], 1);
    }

    #[test]
    fn bold_slate_gray_heading_stays_bold_and_ink_in_live_eink_and_reader_export() {
        let nz = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let mut parser = vt100::Parser::new(nz(5), nz(64), 20);
        // 1. Non-DIM bold slate gray (#94a3b8: chroma 36 <= 42, luma 160 in 70..=180)
        // 2. Non-bold slate gray (#94a3b8) -> muted
        // 3. DIM + bold slate gray (#94a3b8) -> muted (DIM wins)
        // 4. Non-DIM bold xterm gray 244 -> bold
        parser.process(
            b"\x1b[1;38;2;148;163;184m# SlateHeading\x1b[0m\r\n\
              \x1b[38;2;148;163;184mslate_muted\x1b[0m\r\n\
              \x1b[1;2;38;2;148;163;184mdim_bold_slate\x1b[0m\r\n\
              \x1b[1;38;5;244m# Gray244Bold\x1b[0m",
        );

        let backend = TestBackend::new(64, 5);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            f.render_widget(
                VtScreenWidget {
                    screen: parser.screen(),
                    selection: None,
                    eink_mode: true,
                    eink_color: false,
                },
                f.area(),
            );
        })
        .unwrap();

        let buf = term.backend().buffer();
        // Bold #94a3b8 -> EINK_INK + Modifier::BOLD
        assert_eq!(buf[(0, 0)].symbol(), "#");
        assert_eq!(buf[(0, 0)].fg, EINK_INK);
        assert!(buf[(0, 0)].modifier.contains(Modifier::BOLD));

        // Non-bold #94a3b8 -> EINK_INK_MUTED without BOLD
        assert_eq!(buf[(0, 1)].symbol(), "s");
        assert_eq!(buf[(0, 1)].fg, EINK_INK_MUTED);
        assert!(!buf[(0, 1)].modifier.contains(Modifier::BOLD));

        // DIM + bold #94a3b8 -> EINK_INK_MUTED without BOLD
        assert_eq!(buf[(0, 2)].symbol(), "d");
        assert_eq!(buf[(0, 2)].fg, EINK_INK_MUTED);
        assert!(!buf[(0, 2)].modifier.contains(Modifier::BOLD));

        // Non-DIM bold xterm 244 -> EINK_INK + Modifier::BOLD
        assert_eq!(buf[(0, 3)].symbol(), "#");
        assert_eq!(buf[(0, 3)].fg, EINK_INK);
        assert!(buf[(0, 3)].modifier.contains(Modifier::BOLD));

        // Clean Reader JSON export matches exact same 3-tier classification
        let parsed: serde_json::Value =
            serde_json::from_str(&extract_clean_reader_json(parser.screen_mut(), "slate")).unwrap();
        let lines = parsed["lines"].as_array().unwrap();
        assert_eq!(lines[0]["r"][0][0], "# SlateHeading");
        assert_eq!(lines[0]["r"][0][1], 1);
        assert_eq!(lines[1]["r"][0][0], "slate_muted");
        assert_eq!(lines[1]["r"][0][1], 2);
        assert_eq!(lines[2]["r"][0][0], "dim_bold_slate");
        assert_eq!(lines[2]["r"][0][1], 2);
        assert_eq!(lines[3]["r"][0][0], "# Gray244Bold");
        assert_eq!(lines[3]["r"][0][1], 1);
    }

    fn kaleido_world_for_tabs() -> World {
        use crate::model::{make_single_terminal_tab, SharedSpace, TerminalInfo};
        let mut world = World::default();
        world.doc.spaces = vec![SharedSpace {
            id: "sp1".to_string(),
            name: "work".to_string(),
            folder: String::new(),
            tabs: vec![
                make_single_terminal_tab("tb1", "blocked-tab", "term-blocked"),
                make_single_terminal_tab("tb2", "working-tab", "term-working"),
                make_single_terminal_tab("tb3", "done-tab", "term-done"),
                make_single_terminal_tab("tb4", "idle-tab", "term-idle"),
            ],
            rest: serde_json::Map::new(),
        }];
        world
            .active_tab_by_space
            .insert("sp1".to_string(), "tb4".to_string());
        let mk_term = |addr: &str, status: &str, turn: u64, seen: Option<u64>| TerminalInfo {
            address: addr.to_string(),
            name: Some(addr.to_string()),
            state: "live".to_string(),
            cwd: "/tmp".to_string(),
            fg_cwd: None,
            fg_name: None,
            title: addr.to_string(),
            status_word: Some(status.to_string()),
            status_app: Some("codex".to_string()),
            status_msg: None,
            turn,
            seen_turn: seen,
            pinned: false,
            parent: None,
        };
        world.terminals.insert(
            "term-blocked".to_string(),
            mk_term("term-blocked", "blocked", 3, Some(2)),
        );
        world.terminals.insert(
            "term-working".to_string(),
            mk_term("term-working", "working", 3, Some(2)),
        );
        world.terminals.insert(
            "term-done".to_string(),
            mk_term("term-done", "done", 4, Some(3)),
        );
        world.terminals.insert(
            "term-idle".to_string(),
            mk_term("term-idle", "done", 2, Some(2)),
        );
        world
    }

    #[test]
    fn kaleido_top_bar_colors_agent_markers_and_keeps_titles_on_white_paper() {
        let world = kaleido_world_for_tabs();
        let backend = TestBackend::new(120, 12);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            let _ = render_ui_full_with_eink_color(
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
                true,
                true,
            );
        })
        .unwrap();

        let buf = term.backend().buffer();
        let mut row0 = String::new();
        for x in 0..120u16 {
            let cell = &buf[(x, 0)];
            row0.push_str(cell.symbol());
            // Never tint backgrounds in Kaleido mode
            assert!(
                matches!(cell.bg, EINK_PAPER | EINK_SURFACE | EINK_INK),
                "unexpected bg {:?} at col {x}",
                cell.bg
            );
        }

        let find_char_fg = |needle: &str| -> Color {
            for x in 0..120u16 {
                if buf[(x, 0)].symbol() == needle {
                    return buf[(x, 0)].fg;
                }
            }
            panic!("missing '{needle}' in top row: {row0}");
        };
        assert_eq!(find_char_fg("!"), KALEIDO_RED);
        assert_eq!(find_char_fg("*"), KALEIDO_BLUE);
        assert_eq!(find_char_fg("✓"), KALEIDO_GREEN);

        // Tab title text stays strict neutral black ink on white paper
        let b_pos = row0.find("blocked-tab").expect("blocked-tab title");
        let title_col = row0[..b_pos].chars().count() as u16;
        assert_eq!(buf[(title_col, 0)].fg, EINK_INK);
        assert_eq!(buf[(title_col, 0)].bg, EINK_PAPER);

        // Monochrome mode (`eink_color = false`) keeps state symbols neutral
        term.draw(|f| {
            let _ = render_ui_full_with_eink_color(
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
                true,
                false,
            );
        })
        .unwrap();
        let buf_mono = term.backend().buffer();
        for x in 0..120u16 {
            assert_eq!(
                color_rgb(buf_mono[(x, 0)].fg).0,
                color_rgb(buf_mono[(x, 0)].fg).1
            );
            assert_eq!(
                color_rgb(buf_mono[(x, 0)].fg).1,
                color_rgb(buf_mono[(x, 0)].fg).2
            );
        }
    }

    fn color_rgb(c: Color) -> (u8, u8, u8) {
        match c {
            Color::Rgb(r, g, b) => (r, g, b),
            Color::White => (255, 255, 255),
            Color::Black => (0, 0, 0),
            other => panic!("expected RGB/neutral color, got {other:?}"),
        }
    }

    #[test]
    fn kaleido_bottom_quota_bar_highlights_below_15_percent_and_keeps_15_and_above_monochrome() {
        use crate::usage::{AgentKind, AgentQuota, QuotaMetric};
        let world = kaleido_world_for_tabs();
        let backend = TestBackend::new(100, 8);
        let mut term = Terminal::new(backend).unwrap();

        let render_with_pct = |term: &mut Terminal<TestBackend>, pct: f64, eink_color: bool| {
            let quota = AgentQuota {
                agent: AgentKind::Codex,
                metrics: vec![QuotaMetric {
                    window_label: "5h".to_string(),
                    remaining_percent: pct,
                    detail: Some("2h10m".to_string()),
                }],
            };
            term.draw(|f| {
                let _ = render_ui_full_with_eink_color(
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
                    true,
                    eink_color,
                );
            })
            .unwrap();
        };

        // Below 15% (14.9% -> rounds to 15%? Use 14.0% and 14.4% too, and check < 15.0 threshold)
        render_with_pct(&mut term, 14.2, true);
        let buf = term.backend().buffer();
        let row_y = 7u16;
        let row_text: String = (0..100u16).map(|x| buf[(x, row_y)].symbol()).collect();
        assert!(row_text.contains("14%!"), "expected '14%!' in {row_text}");
        let mut saw_red_bar = false;
        let mut saw_red_pct = false;
        for x in 0..100u16 {
            let cell = &buf[(x, row_y)];
            // Bottom bar backgrounds stay pure neutral Carta monochrome (no red/tinted bg)
            assert!(
                matches!(cell.bg, EINK_PAPER | EINK_INK),
                "unexpected bg {:?} at col {x}",
                cell.bg
            );
            if cell.symbol() == "█" || cell.symbol() == "▌" {
                assert_eq!(cell.fg, KALEIDO_RED);
                assert_eq!(cell.bg, EINK_PAPER);
                saw_red_bar = true;
            }
            if cell.symbol() == "─" {
                assert_eq!(cell.fg, EINK_INK_MUTED);
                assert_eq!(cell.bg, EINK_PAPER);
            }
            if cell.symbol() == "!" && cell.fg == KALEIDO_RED {
                assert_eq!(cell.bg, EINK_PAPER);
                saw_red_pct = true;
            }
        }
        assert!(saw_red_bar && saw_red_pct);

        // Exactly 15.0% stays strictly monochrome (threshold is < 15.0)
        render_with_pct(&mut term, 15.0, true);
        let buf_15 = term.backend().buffer();
        for x in 0..100u16 {
            let (r, g, b) = color_rgb(buf_15[(x, row_y)].fg);
            assert!(
                r == g && g == b,
                "expected neutral gray at 15.0%, got {:?}",
                buf_15[(x, row_y)].fg
            );
            assert!(matches!(buf_15[(x, row_y)].bg, EINK_PAPER | EINK_INK));
        }

        // 85.0% stays strictly monochrome
        render_with_pct(&mut term, 85.0, true);
        let buf_85 = term.backend().buffer();
        for x in 0..100u16 {
            let (r, g, b) = color_rgb(buf_85[(x, row_y)].fg);
            assert!(r == g && g == b);
        }
    }

    #[test]
    fn kaleido_diff_markers_highlight_only_markers_and_sidebars_on_white_paper() {
        let nz = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let mut parser = vt100::Parser::new(nz(12), nz(60), 20);
        parser.process(
            b"\x1b[38;5;208;48;2;20;60;20m--- a/lib.rs\x1b[0m\r\n\
              \x1b[32;48;2;20;70;20m+++ b/lib.rs\x1b[0m\r\n\
              \x1b[36m@@ -1,3 +1,4 @@ fn demo()\x1b[0m\r\n\
              \x1b[31;48;2;70;20;20m-let old_val = 1;\x1b[0m\r\n\
              \x1b[32;48;2;20;70;20m+let new_val = 2;\x1b[0m\r\n\
              \x1b[38;2;220;220;220m unchanged_ctx();\x1b[0m\r\n\
              \x1b[32m  42 + boxed_add();\x1b[0m\r\n\
              \x1b[31m  41 - boxed_del();\x1b[0m\r\n\
              \x1b[38;2;40;160;80m\xe2\x96\x8e\x1b[0m bar_add();\r\n\
              ++i;\r\n\
              - plain markdown bullet",
        );

        let backend = TestBackend::new(60, 12);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            f.render_widget(
                VtScreenWidget {
                    screen: parser.screen(),
                    selection: None,
                    eink_mode: true,
                    eink_color: true,
                },
                f.area(),
            );
        })
        .unwrap();

        let buf = term.backend().buffer();
        // Every background cell stays crisp pure white paper (no red/green bg wash!)
        for y in 0..12u16 {
            for x in 0..60u16 {
                assert_eq!(buf[(x, y)].bg, EINK_PAPER, "row {y} col {x} leaked bg");
            }
        }

        // Row 0: "---" red, file path neutral
        assert_eq!(buf[(0, 0)].fg, KALEIDO_RED);
        assert_eq!(buf[(2, 0)].fg, KALEIDO_RED);
        assert_eq!(buf[(4, 0)].fg, EINK_INK); // 'a' stays sharp black

        // Row 1: "+++" green, file path neutral
        assert_eq!(buf[(0, 1)].fg, KALEIDO_GREEN);
        assert_eq!(buf[(2, 1)].fg, KALEIDO_GREEN);
        assert_eq!(buf[(4, 1)].fg, EINK_INK);

        // Row 2: "@@ -1,3 +1,4 @@" blue hunk marker, trailing "fn demo()" neutral
        assert_eq!(buf[(0, 2)].fg, KALEIDO_BLUE);
        assert_eq!(buf[(14, 2)].fg, KALEIDO_BLUE);
        let fn_pos = (0..60u16).find(|&x| buf[(x, 2)].symbol() == "f").unwrap();
        assert_eq!(buf[(fn_pos, 2)].fg, EINK_INK);

        // Row 3: '-' marker KALEIDO_RED + BOLD, code "let old_val = 1;" strictly monochrome
        assert_eq!(buf[(0, 3)].fg, KALEIDO_RED);
        assert!(buf[(0, 3)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(1, 3)].symbol(), "l");
        assert_eq!(buf[(1, 3)].fg, EINK_INK);

        // Row 4: '+' marker KALEIDO_GREEN + BOLD, code "let new_val = 2;" strictly monochrome
        assert_eq!(buf[(0, 4)].fg, KALEIDO_GREEN);
        assert!(buf[(0, 4)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(1, 4)].symbol(), "l");
        assert_eq!(buf[(1, 4)].fg, EINK_INK);

        // Row 6/7: numbered agent diff markers (`42 +`, `41 -`)
        assert_eq!(buf[(5, 6)].symbol(), "+");
        assert_eq!(buf[(5, 6)].fg, KALEIDO_GREEN);
        assert_eq!(buf[(7, 6)].fg, EINK_INK);
        assert_eq!(buf[(5, 7)].symbol(), "-");
        assert_eq!(buf[(5, 7)].fg, KALEIDO_RED);
        assert_eq!(buf[(7, 7)].fg, EINK_INK);

        // Row 8: colored side bar '▎' -> KALEIDO_GREEN, body text stays black
        assert_eq!(buf[(0, 8)].symbol(), "▎");
        assert_eq!(buf[(0, 8)].fg, KALEIDO_GREEN);
        assert_eq!(buf[(2, 8)].symbol(), "b");
        assert_eq!(buf[(2, 8)].fg, EINK_INK);

        // Row 9 ("++i;") and Row 10 ("- plain markdown bullet"): outside hunk/uncolored -> neutral
        assert_eq!(buf[(0, 9)].fg, EINK_INK);
        assert_eq!(buf[(0, 10)].fg, EINK_INK);

        // Monochrome snapshot vs Kaleido snapshot differ when diff rows are present
        let snap_mono = capture_eink_screen_snapshot_with_color(
            parser.screen(),
            Rect::new(0, 0, 60, 12),
            0,
            false,
        );
        let snap_kaleido = capture_eink_screen_snapshot_with_color(
            parser.screen(),
            Rect::new(0, 0, 60, 12),
            0,
            true,
        );
        assert_ne!(snap_mono, snap_kaleido);
    }

    #[test]
    fn kaleido_help_modal_renders_toggle_and_shortcut_within_80_cols() {
        let world = kaleido_world_for_tabs();
        let backend = TestBackend::new(80, 36);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            let _ = render_ui_full_with_eink_color(
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
                None,
                false,
                None,
                false,
                None,
                true,
                true,
            );
        })
        .unwrap();

        let buf = term.backend().buffer();
        let mut full_text = String::new();
        for y in 0..36u16 {
            for x in 0..80u16 {
                full_text.push_str(buf[(x, y)].symbol());
            }
            full_text.push('\n');
        }
        assert!(
            full_text.contains("Kaleido: ON"),
            "missing Kaleido button:\n{full_text}"
        );
        assert!(
            full_text.contains("^b E"),
            "missing ^b E shortcut:\n{full_text}"
        );
    }

    #[test]
    fn kaleido_diff_handles_uncolored_doubles_spaced_line_nums_and_wrapped_hunks() {
        let nz = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let mut parser = vt100::Parser::new(nz(12), nz(44), 20);
        parser.process(
            b"--- old.txt\r\n\
              +++ new.txt\r\n\
              \x1b[36;48;2;20;40;65m@@ -1,4 +1,4 @@ fn very_long_hunk_signature_wraps_here()\x1b[0m\r\n\
              ++counter;\r\n\
              --sql_comment\r\n\
              --- removed_sql\r\n\
              +tail_add\r\n\
              \x1b[32m  42 \xe2\x94\x82 + spaced_num_add();\x1b[0m\r\n\
              \x1b[31m  10  11 \xe2\x94\x82 - dual_num_del();\x1b[0m\r\n\
              \x1b[31m----\x1b[0m",
        );

        let backend = TestBackend::new(44, 12);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            f.render_widget(
                VtScreenWidget {
                    screen: parser.screen(),
                    selection: None,
                    eink_mode: true,
                    eink_color: true,
                },
                f.area(),
            );
        })
        .unwrap();

        let buf = term.backend().buffer();
        for y in 0..12u16 {
            for x in 0..44u16 {
                assert_eq!(buf[(x, y)].bg, EINK_PAPER, "row {y} col {x} leaked bg");
            }
        }

        // Row 0 & 1: uncolored `diff -u old.txt new.txt` headers
        assert_eq!(buf[(0, 0)].fg, KALEIDO_RED);
        assert_eq!(buf[(2, 0)].fg, KALEIDO_RED);
        assert_eq!(buf[(4, 0)].fg, EINK_INK);
        assert_eq!(buf[(0, 1)].fg, KALEIDO_GREEN);
        assert_eq!(buf[(2, 1)].fg, KALEIDO_GREEN);

        // Row 2 & wrapped Row 3: hunk header + wrapped continuation keeps white bg
        assert_eq!(buf[(0, 2)].fg, KALEIDO_BLUE);
        assert_eq!(buf[(0, 3)].bg, EINK_PAPER);
        assert_eq!(buf[(0, 3)].fg, EINK_INK);

        // Row 4: `++counter;` inside uncolored hunk -> col 0 '+' green, col 1 '+' black
        assert_eq!(buf[(0, 4)].symbol(), "+");
        assert_eq!(buf[(0, 4)].fg, KALEIDO_GREEN);
        assert_eq!(buf[(1, 4)].symbol(), "+");
        assert_eq!(buf[(1, 4)].fg, EINK_INK);

        // Row 5: `--sql_comment` -> col 0 '-' red, col 1 '-' black
        assert_eq!(buf[(0, 5)].symbol(), "-");
        assert_eq!(buf[(0, 5)].fg, KALEIDO_RED);
        assert_eq!(buf[(1, 5)].fg, EINK_INK);

        // Row 6: `--- removed_sql` inside hunk -> ONLY col 0 '-' red (not "---" file header)
        assert_eq!(buf[(0, 6)].symbol(), "-");
        assert_eq!(buf[(0, 6)].fg, KALEIDO_RED);
        assert_eq!(buf[(1, 6)].symbol(), "-");
        assert_eq!(buf[(1, 6)].fg, EINK_INK);

        // Row 7: `+tail_add` after `++`/`--` still inside uncolored hunk!
        assert_eq!(buf[(0, 7)].symbol(), "+");
        assert_eq!(buf[(0, 7)].fg, KALEIDO_GREEN);

        // Row 8: `  42 │ + spaced_num_add();` -> '+' at col 7 is green
        let plus_x = (0..44u16).find(|&x| buf[(x, 8)].symbol() == "+").unwrap();
        assert_eq!(buf[(plus_x, 8)].fg, KALEIDO_GREEN);

        // Row 9: `  10  11 │ - dual_num_del();` -> '-' at col 11 is red
        let minus_x = (0..44u16).find(|&x| buf[(x, 9)].symbol() == "-").unwrap();
        assert_eq!(buf[(minus_x, 9)].fg, KALEIDO_RED);
    }

    #[test]
    fn kaleido_diff_context_resets_after_diff_so_banners_stay_monochrome() {
        let nz = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let mut parser = vt100::Parser::new(nz(8), nz(50), 20);
        parser.process(
            b"\x1b[32m+added_code();\x1b[0m\r\n\
              \r\n\
              --- Results ---\r\n\
              +++ Summary +++",
        );

        let backend = TestBackend::new(50, 8);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            f.render_widget(
                VtScreenWidget {
                    screen: parser.screen(),
                    selection: None,
                    eink_mode: true,
                    eink_color: true,
                },
                f.area(),
            );
        })
        .unwrap();

        let buf = term.backend().buffer();
        assert_eq!(buf[(0, 0)].fg, KALEIDO_GREEN);
        assert_eq!(buf[(0, 2)].fg, EINK_INK);
        assert_eq!(buf[(0, 3)].fg, EINK_INK);
    }

    #[test]
    fn kaleido_narrow_top_bar_never_tints_ellipsis_on_truncated_space() {
        let mut world = kaleido_world_for_tabs();
        world.doc.spaces[0].name = "extremely-long-space-name".to_string();
        world
            .active_tab_by_space
            .insert("sp1".to_string(), "tb1".to_string());
        let backend = TestBackend::new(36, 6);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            let _ = render_ui_full_with_eink_color(
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
                true,
                true,
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        for x in 0..36u16 {
            if buf[(x, 0)].symbol() == "…" {
                assert_eq!(
                    buf[(x, 0)].fg,
                    EINK_INK,
                    "truncated ellipsis at col {x} must remain neutral ink"
                );
            }
        }
    }

    #[test]
    fn eink_demotes_agent_process_thought_tool_and_spinner_rows_to_italic_process_gray() {
        let nz = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let rows = 14u16;
        let cols = 72u16;
        let mut parser = vt100::Parser::new(nz(rows), nz(cols), 40);
        parser.process(
            concat!(
                "\x1b[38;2;148;163;184m○ Thought for 5.6s (The duplicate notification timing...)\x1b[0m\r\n",
                "\x1b[1;32m●\x1b[0m \x1b[1mUsed Swarm\x1b[0m \x1b[90m(Checked swarm status (xBoard 几何吸附、...)\x1b[0m\r\n",
                "\x1b[1;36m●\x1b[0m \x1b[1mScheduling 1 timer\x1b[0m \x1b[90m(240s: Chec...) (ctrl+o to expand)\x1b[0m\r\n",
                "\x1b[34m⣷\x1b[0m  \x1b[1mScheduling...\x1b[0m\r\n",
                "\x1b[1;37m● Update(src/ui.rs)\x1b[0m\r\n",
                "  ⎿  \x1b[90mUpdated src/ui.rs with 3 additions\x1b[0m\r\n",
                "\x1b[90m      40   unchanged_ctx();\x1b[0m\r\n",
                "● 这是正式回答正文 (补充说明)\r\n",
                "- standard markdown bullet\r\n",
                "• normal list bullet\r\n",
            )
            .as_bytes(),
        );

        for eink_color in [false, true] {
            let backend = TestBackend::new(cols, rows);
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|f| {
                f.render_widget(
                    VtScreenWidget {
                        screen: parser.screen(),
                        selection: None,
                        eink_mode: true,
                        eink_color,
                    },
                    f.area(),
                );
            })
            .unwrap();

            let buf = term.backend().buffer();
            // Rows 0..=5 are AI process/tool/spinner/sub-step rows -> all cells #777777 + ITALIC without BOLD/DIM
            for y in 0..=5u16 {
                let mut x = 0u16;
                while x < cols {
                    let cell = &buf[(x, y)];
                    assert_eq!(
                        cell.fg, EINK_INK_PROCESS,
                        "eink_color={eink_color} row {y} col {x} ({:?}) should use EINK_INK_PROCESS",
                        cell.symbol()
                    );
                    assert!(
                        cell.modifier.contains(Modifier::ITALIC),
                        "eink_color={eink_color} row {y} col {x} should be italic"
                    );
                    assert!(
                        !cell.modifier.contains(Modifier::BOLD),
                        "eink_color={eink_color} row {y} col {x} must drop BOLD"
                    );
                    assert!(
                        !cell.modifier.contains(Modifier::DIM),
                        "eink_color={eink_color} row {y} col {x} must drop DIM"
                    );
                    x += cell.symbol().width().max(1) as u16;
                }
            }
            // Wide CJK chars inside `几何吸附` remain intact with EINK_INK_PROCESS + ITALIC
            let mut row1_txt = String::new();
            let mut rx = 0u16;
            while rx < cols {
                let sym = buf[(rx, 1)].symbol();
                row1_txt.push_str(sym);
                rx += sym.width().max(1) as u16;
            }
            assert!(row1_txt.contains("几何吸附"));
            // Leading isolated spinner on row 3 freezes to ◈ and uses process italic gray
            assert_eq!(buf[(0, 3)].symbol(), "◈");
            assert_eq!(buf[(0, 3)].fg, EINK_INK_PROCESS);
            assert!(buf[(0, 3)].modifier.contains(Modifier::ITALIC));

            // Row 6: numbered diff/code context under process sub-step stays upright #666666 without ITALIC
            assert_eq!(buf[(6, 6)].symbol(), "4");
            assert_eq!(buf[(6, 6)].fg, EINK_INK_MUTED);
            assert!(!buf[(6, 6)].modifier.contains(Modifier::ITALIC));

            // Row 7: assistant reply starting with ● and non-muted `(补充说明)` stays crisp black upright prose
            assert_eq!(buf[(0, 7)].symbol(), "●");
            assert_eq!(buf[(0, 7)].fg, EINK_INK);
            assert!(!buf[(0, 7)].modifier.contains(Modifier::ITALIC));

            // Row 8 & 9: normal Markdown bullets stay crisp black upright prose
            assert_eq!(buf[(0, 8)].fg, EINK_INK);
            assert!(!buf[(0, 8)].modifier.contains(Modifier::ITALIC));
            assert_eq!(buf[(0, 9)].fg, EINK_INK);
            assert!(!buf[(0, 9)].modifier.contains(Modifier::ITALIC));

            // Snapshots match rendered colors & modifiers cleanly
            let area = Rect::new(0, 0, cols, rows);
            let snap =
                capture_eink_screen_snapshot_with_color(parser.screen(), area, 0, eink_color);
            assert_eq!(snap.cells[0].fg, EINK_INK_PROCESS);
            assert!(snap.cells[0].mods.contains(Modifier::ITALIC));
        }

        // Narrow screen soft-wrapped and indented process continuations
        let narrow_cols = 34u16;
        let narrow_rows = 8u16;
        let mut narrow = vt100::Parser::new(nz(narrow_rows), nz(narrow_cols), 20);
        narrow.process(
            concat!(
                "\x1b[1;32m●\x1b[0m \x1b[1mUsed Swarm\x1b[0m \x1b[90m(Checked swarm status xBoard 几何吸附 wrapped)\x1b[0m\r\n",
                "  \x1b[90mwaiting for 2 peers\x1b[0m\r\n",
                "正文结论。\r\n",
            )
            .as_bytes(),
        );
        let mut narrow_term = Terminal::new(TestBackend::new(narrow_cols, narrow_rows)).unwrap();
        narrow_term
            .draw(|f| {
                f.render_widget(
                    VtScreenWidget {
                        screen: narrow.screen(),
                        selection: None,
                        eink_mode: true,
                        eink_color: false,
                    },
                    f.area(),
                );
            })
            .unwrap();
        let nbuf = narrow_term.backend().buffer();
        for y in 0..=2u16 {
            let mut x = 0u16;
            while x < narrow_cols {
                let cell = &nbuf[(x, y)];
                assert_eq!(cell.fg, EINK_INK_PROCESS);
                assert!(cell.modifier.contains(Modifier::ITALIC));
                x += cell.symbol().width().max(1) as u16;
            }
        }
        assert_eq!(nbuf[(0, 3)].symbol(), "正");
        assert_eq!(nbuf[(0, 3)].fg, EINK_INK);
        assert!(!nbuf[(0, 3)].modifier.contains(Modifier::ITALIC));

        // Clean Reader JSON export classifies whole process lines as style 2 (MUTED)
        let reader_json: serde_json::Value =
            serde_json::from_str(&extract_clean_reader_json(parser.screen_mut(), "proc")).unwrap();
        let reader_lines = reader_json["lines"].as_array().unwrap();
        for (idx, line) in reader_lines.iter().enumerate().take(6) {
            let runs = line["r"].as_array().unwrap();
            assert!(
                runs.iter().all(|run| run[1] == 2),
                "reader line {idx} must export entirely as muted style 2: {runs:?}"
            );
        }
        let body_runs = reader_lines
            .iter()
            .find(|l| {
                l["r"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r[0].as_str().unwrap().contains("这是正式回答正文"))
            })
            .unwrap()["r"]
            .as_array()
            .unwrap();
        assert_eq!(body_runs[0][1], 0);
    }
}
