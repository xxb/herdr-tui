//! Unified lightweight reader extraction, viewport wrapping, and ShellDeck JSON export.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Maximum terminal rows (scrollback + live) scanned in Agent Clean mode.
pub const AGENT_MAX_SCAN_ROWS: usize = 600;
/// Maximum recent user/assistant turns retained in Agent Clean mode.
pub const AGENT_MAX_TURNS: usize = 3;
/// Maximum terminal rows scanned in Agent Full mode.
pub const FULL_MAX_SCAN_ROWS: usize = 420;
/// Maximum terminal rows scanned in Shell mode.
pub const SHELL_MAX_SCAN_ROWS: usize = 260;
/// Maximum logical lines retained in Shell mode.
pub const SHELL_MAX_CLEAN_LINES: usize = 200;
/// Maximum byte length of serialized ShellDeck v1 reader JSON payloads.
pub const MAX_SHELLDECK_JSON_BYTES: usize = 48_000;

/// Semantic text run style shared by `Modal::Reader` and ShellDeck JSON export.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ReaderRunStyle {
    Body = 0,
    Bold = 1,
    Muted = 2,
    Code = 3,
}

impl ReaderRunStyle {
    /// Convert style to ShellDeck v1 numeric run code (`Body`/`Code` -> `0`, `Bold` -> `1`, `Muted` -> `2`).
    pub fn to_shelldeck_code(self) -> u8 {
        match self {
            Self::Body | Self::Code => 0,
            Self::Bold => 1,
            Self::Muted => 2,
        }
    }
}

/// Logical line representation inside an extracted [`ReaderDoc`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReaderLineKind {
    Text(Vec<(String, ReaderRunStyle)>),
    Blank,
    Divider,
}

/// Cleaned logical reader document extracted from a `vt100::Screen`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReaderDoc {
    pub title: String,
    pub is_agent: bool,
    pub clean_mode: bool,
    pub lines: Vec<ReaderLineKind>,
    pub anchor_line: usize,
    pub hidden_process_lines: usize,
}

/// Wrapped visual rows derived from a [`ReaderDoc`] for a target viewport width.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrappedReaderView {
    pub rows: Vec<ReaderLineKind>,
    pub anchor_visual_row: usize,
}

struct ScrollbackGuard<'a> {
    screen: &'a mut vt100::Screen,
    orig_sb: usize,
}

impl Drop for ScrollbackGuard<'_> {
    fn drop(&mut self) {
        self.screen.set_scrollback(self.orig_sb);
    }
}

#[derive(Clone)]
struct RawLogicalLine {
    in_live: bool,
    is_process: bool,
    is_code_body: bool,
    is_fence: bool,
    runs: Vec<(String, ReaderRunStyle)>,
}

#[derive(Clone)]
enum ProcessedLineKind {
    Text {
        runs: Vec<(String, ReaderRunStyle)>,
        text: String,
        all_muted: bool,
    },
    Blank,
    Divider,
}

#[derive(Clone)]
struct ProcessedLine {
    in_live: bool,
    is_process: bool,
    is_code_body: bool,
    is_fence: bool,
    is_turn_start: bool,
    is_shell_prompt: bool,
    kind: ProcessedLineKind,
}

struct ScanState {
    rows_u16: u16,
    cols: u16,
    is_agent: bool,
    prev_wrapped: bool,
    prev_is_process: bool,
    in_code_fence: bool,
    cur_in_live: bool,
    cur_is_process: bool,
    cur_is_code_body: bool,
    cur_is_fence: bool,
    cur_runs: Vec<(String, ReaderRunStyle)>,
    logical_lines: Vec<RawLogicalLine>,
}

impl ScanState {
    fn new(rows_u16: u16, cols: u16, is_agent: bool) -> Self {
        Self {
            rows_u16,
            cols,
            is_agent,
            prev_wrapped: false,
            prev_is_process: false,
            in_code_fence: false,
            cur_in_live: false,
            cur_is_process: false,
            cur_is_code_body: false,
            cur_is_fence: false,
            cur_runs: Vec::new(),
            logical_lines: Vec::new(),
        }
    }

    fn scan_visible_row(&mut self, scr: &vt100::Screen, r: u16, in_live: bool) {
        let row_is_fence =
            !self.prev_wrapped && vt_row_trimmed_starts_with_fence_fallback(scr, r, self.cols);
        if row_is_fence {
            self.in_code_fence = !self.in_code_fence;
        }

        let row_is_process = self.is_agent
            && !row_is_fence
            && !self.in_code_fence
            && classify_process_row_fallback(
                scr,
                r,
                self.rows_u16,
                self.cols,
                self.prev_wrapped,
                self.prev_is_process,
            );

        if !self.prev_wrapped {
            self.cur_in_live = in_live;
            self.cur_is_process = row_is_process;
            self.cur_is_fence = row_is_fence;
            self.cur_is_code_body = self.in_code_fence && !row_is_fence;
        } else if in_live {
            self.cur_in_live = true;
        }

        let mut prev_was_wide = false;
        let mut prev_col: u16 = 0;
        for col in 0..self.cols {
            if prev_was_wide {
                prev_was_wide = false;
                continue;
            }
            let Some(cell) = scr.cell(r, col) else {
                continue;
            };
            prev_was_wide = cell.is_wide();
            if cell.has_contents() {
                let style = if self.cur_is_code_body {
                    ReaderRunStyle::Code
                } else if row_is_process || cell.dim() {
                    ReaderRunStyle::Muted
                } else if cell.bold() {
                    ReaderRunStyle::Bold
                } else if is_muted_cell_fallback(cell) {
                    ReaderRunStyle::Muted
                } else {
                    ReaderRunStyle::Body
                };
                if col > prev_col {
                    let gap_style = self.cur_runs.last().map_or(style, |r| r.1);
                    push_run(
                        &mut self.cur_runs,
                        &" ".repeat(usize::from(col - prev_col)),
                        gap_style,
                    );
                }
                prev_col = col + if cell.is_wide() { 2 } else { 1 };
                push_run(&mut self.cur_runs, cell.contents(), style);
            }
        }

        self.prev_wrapped = scr.row_wrapped(r);
        self.prev_is_process = row_is_process;

        if !self.prev_wrapped {
            self.logical_lines.push(RawLogicalLine {
                in_live: self.cur_in_live,
                is_process: self.cur_is_process,
                is_code_body: self.cur_is_code_body,
                is_fence: self.cur_is_fence,
                runs: std::mem::take(&mut self.cur_runs),
            });
        }
    }

    fn into_lines(mut self) -> Vec<RawLogicalLine> {
        if !self.cur_runs.is_empty() {
            self.logical_lines.push(RawLogicalLine {
                in_live: self.cur_in_live,
                is_process: self.cur_is_process,
                is_code_body: self.cur_is_code_body,
                is_fence: self.cur_is_fence,
                runs: self.cur_runs,
            });
        }
        self.logical_lines
    }
}

fn push_run(runs: &mut Vec<(String, ReaderRunStyle)>, text: &str, style: ReaderRunStyle) {
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

fn join_runs(runs: &[(String, ReaderRunStyle)]) -> String {
    runs.iter().map(|(t, _)| t.as_str()).collect()
}

fn trim_trailing_runs(runs: &mut Vec<(String, ReaderRunStyle)>) {
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

fn trim_leading_whitespace_runs(runs: &mut Vec<(String, ReaderRunStyle)>) {
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

fn strip_leading_char(runs: &mut Vec<(String, ReaderRunStyle)>, ch: char) -> bool {
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

fn strip_trailing_box_edge(runs: &mut Vec<(String, ReaderRunStyle)>) {
    trim_trailing_runs(runs);
    let Some((last, _)) = runs.last_mut() else {
        return;
    };
    if let Some(rest) = last.strip_suffix(['│', '┃', '║']) {
        *last = rest.to_string();
        if last.is_empty() {
            runs.pop();
        }
        trim_trailing_runs(runs);
    }
}

fn strip_leading_box_rule_chars(runs: &mut Vec<(String, ReaderRunStyle)>) {
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

fn strip_trailing_box_rule_chars(runs: &mut Vec<(String, ReaderRunStyle)>) {
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

fn is_horizontal_rule_text(trimmed: &str) -> bool {
    let mut rule_chars = 0usize;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            continue;
        }
        if matches!(ch, '─' | '━' | '═') {
            rule_chars += 1;
        } else {
            return false;
        }
    }
    rule_chars >= 3
}

fn is_pure_box_drawing_text(trimmed: &str) -> bool {
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| c.is_whitespace() || ('\u{2500}'..='\u{257F}').contains(&c))
}

fn clean_logical_line(
    mut runs: Vec<(String, ReaderRunStyle)>,
    is_code_body: bool,
) -> Option<ProcessedLineKind> {
    trim_trailing_runs(&mut runs);
    if runs.is_empty() {
        return Some(ProcessedLineKind::Blank);
    }

    let joined = join_runs(&runs);
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        return Some(ProcessedLineKind::Blank);
    }

    if !is_code_body && is_pure_box_drawing_text(trimmed) {
        return if is_horizontal_rule_text(trimmed) {
            Some(ProcessedLineKind::Divider)
        } else {
            None
        };
    }

    let has_matching_vertical_edges = trimmed.starts_with(['│', '┃', '║'])
        && trimmed.ends_with(['│', '┃', '║'])
        && trimmed.chars().count() >= 2;

    if (!is_code_body && trimmed.starts_with(['│', '┃', '║'])) || has_matching_vertical_edges
    {
        trim_leading_whitespace_runs(&mut runs);
        for edge in ['│', '┃', '║'] {
            if strip_leading_char(&mut runs, edge) {
                let _ = strip_leading_char(&mut runs, ' ');
                break;
            }
        }
        strip_trailing_box_edge(&mut runs);
    } else if !is_code_body
        && ((trimmed.starts_with(['╭', '┌', '┏', '╔']) && trimmed.ends_with(['╮', '┐', '┓', '╗']))
            || (trimmed.starts_with(['╰', '└', '┗', '╚'])
                && trimmed.ends_with(['╯', '┘', '┛', '╝'])))
    {
        strip_leading_box_rule_chars(&mut runs);
        strip_trailing_box_rule_chars(&mut runs);
    }

    trim_trailing_runs(&mut runs);
    let remaining = join_runs(&runs);
    let rem_trimmed = remaining.trim();
    if rem_trimmed.is_empty() {
        return Some(ProcessedLineKind::Blank);
    }
    if !is_code_body && is_pure_box_drawing_text(rem_trimmed) {
        return if is_horizontal_rule_text(rem_trimmed) {
            Some(ProcessedLineKind::Divider)
        } else {
            None
        };
    }

    let all_muted = runs
        .iter()
        .all(|(t, s)| t.trim().is_empty() || *s == ReaderRunStyle::Muted);
    Some(ProcessedLineKind::Text {
        runs,
        text: remaining,
        all_muted,
    })
}

fn scan_screen_logical_lines(
    screen: &mut vt100::Screen,
    max_scan_rows: usize,
    is_agent: bool,
) -> Vec<RawLogicalLine> {
    let orig_sb = screen.scrollback();
    let guard = ScrollbackGuard { screen, orig_sb };

    let (rows_nz, cols_nz) = guard.screen.size();
    let rows_u16 = rows_nz.get();
    let rows = usize::from(rows_u16);
    let cols = cols_nz.get();
    let total_sb = guard.screen.scrollback_len();
    let hist_rows = total_sb.min(max_scan_rows.saturating_sub(rows));

    let mut state = ScanState::new(rows_u16, cols, is_agent);

    let mut cur_abs = total_sb.saturating_sub(hist_rows);
    while cur_abs < total_sb {
        let sb = total_sb - cur_abs;
        guard.screen.set_scrollback(sb);
        let take = (total_sb - cur_abs).min(rows);
        for r in 0..take {
            state.scan_visible_row(guard.screen, r as u16, false);
        }
        cur_abs += take;
    }

    guard.screen.set_scrollback(0);
    let live_start = rows.saturating_sub(max_scan_rows);
    for r in live_start..rows {
        state.scan_visible_row(guard.screen, r as u16, true);
    }

    state.into_lines()
}

fn build_processed_lines(raw_lines: Vec<RawLogicalLine>) -> Vec<ProcessedLine> {
    let mut out = Vec::with_capacity(raw_lines.len());
    for raw in raw_lines {
        let Some(kind) = clean_logical_line(raw.runs, raw.is_code_body) else {
            continue;
        };
        out.push(ProcessedLine {
            in_live: raw.in_live,
            is_process: raw.is_process,
            is_code_body: raw.is_code_body,
            is_fence: raw.is_fence,
            is_turn_start: false,
            is_shell_prompt: false,
            kind,
        });
    }
    out
}

fn is_markdown_heading(text: &str) -> bool {
    let indent = text.bytes().take_while(|&b| b == b' ').count();
    if indent > 2 {
        return false;
    }
    let rest = &text[indent..];
    let hashes = rest.bytes().take_while(|&b| b == b'#').count();
    (1..=6).contains(&hashes) && rest.as_bytes().get(hashes) == Some(&b' ')
}

fn promote_line_style(runs: &mut [(String, ReaderRunStyle)], style: ReaderRunStyle) {
    for (_, run_style) in runs.iter_mut() {
        *run_style = style;
    }
}

fn starts_with_role_label(s: &str, label: &str) -> bool {
    s.strip_prefix(label)
        .is_some_and(|after| after.is_empty() || after.starts_with(char::is_whitespace))
}

fn is_agent_user_prompt(text: &str) -> bool {
    let indent = text.bytes().take_while(|&b| b == b' ').count();
    if indent > 2 {
        return false;
    }
    let rest = &text[indent..];
    rest.starts_with("❯ ")
        || rest.starts_with("› ")
        || rest.starts_with("> ")
        || starts_with_role_label(rest, "You:")
        || starts_with_role_label(rest, "User:")
}

fn is_empty_agent_prompt_text(trimmed: &str) -> bool {
    matches!(trimmed, "❯" | "›" | ">")
}

fn starts_with_process_marker(trimmed: &str) -> bool {
    ["○ ", "● ", "◈ ", "• ", "⎿", "↳", "└ "]
        .iter()
        .any(|m| trimmed.starts_with(m))
}

fn is_explicit_trailing_chrome(line: &ProcessedLine, text: &str) -> bool {
    if line.is_code_body || line.is_fence {
        return false;
    }
    let trimmed = text.trim();
    if is_empty_agent_prompt_text(trimmed) {
        return true;
    }
    if trimmed.chars().count() > 120 {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("type a message")
        || lower.contains("send a message")
        || lower.contains("ask anything")
        || lower.contains("enter a message")
        || lower.contains("? for shortcuts")
        || lower.contains("shift+tab")
        || lower.contains("ctrl+c to exit")
        || lower.contains("ctrl+d to exit")
        || lower.contains("/help for commands")
    {
        return true;
    }
    !starts_with_process_marker(trimmed)
        && (lower.contains("(esc to interrupt)")
            || lower.contains("(esc to cancel)")
            || lower.contains("(ctrl+o to expand)")
            || lower.contains("(ctrl+r to expand)")
            || lower.contains("(ctrl+o to show")
            || lower == "esc to interrupt")
}

fn looks_like_muted_footer(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.chars().count() > 140 {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    trimmed.contains(" · ")
        || trimmed.contains(" | ")
        || lower.contains("context")
        || lower.contains("tokens")
        || lower.contains("shortcuts")
        || lower.contains("shift+tab")
        || lower.contains("ctrl+")
        || lower.contains("esc ")
        || (trimmed.contains('%') && trimmed.chars().count() <= 80)
}

fn strip_trailing_agent_chrome(items: &mut Vec<ProcessedLine>) {
    let mut saw_chrome_or_prompt = false;
    while let Some(last) = items.last() {
        let strip = match &last.kind {
            ProcessedLineKind::Blank | ProcessedLineKind::Divider => true,
            ProcessedLineKind::Text {
                text, all_muted, ..
            } => {
                let is_chrome = is_explicit_trailing_chrome(last, text)
                    || (!last.is_code_body
                        && !last.is_fence
                        && !last.is_process
                        && *all_muted
                        && last.in_live
                        && (saw_chrome_or_prompt || looks_like_muted_footer(text)));
                if is_chrome {
                    saw_chrome_or_prompt = true;
                }
                is_chrome
            }
        };
        if strip {
            items.pop();
        } else {
            break;
        }
    }
}

fn fence_parity_across<'a, I>(lines: I) -> bool
where
    I: IntoIterator<Item = &'a ProcessedLine>,
{
    let mut inside = false;
    for item in lines {
        if item.is_fence {
            inside = !inside;
        }
    }
    inside
}

fn make_synthetic_fence(in_live: bool) -> ProcessedLine {
    ProcessedLine {
        in_live,
        is_process: false,
        is_code_body: false,
        is_fence: true,
        is_turn_start: false,
        is_shell_prompt: false,
        kind: ProcessedLineKind::Text {
            runs: vec![("```".to_string(), ReaderRunStyle::Body)],
            text: "```".to_string(),
            all_muted: false,
        },
    }
}

fn trim_to_recent_turns(items: Vec<ProcessedLine>, max_turns: usize) -> Vec<ProcessedLine> {
    let turn_starts: Vec<usize> = items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| match &item.kind {
            ProcessedLineKind::Text { text, .. }
                if !item.is_process
                    && !item.is_code_body
                    && !item.is_fence
                    && is_agent_user_prompt(text) =>
            {
                Some(idx)
            }
            _ => None,
        })
        .collect();

    if turn_starts.len() <= max_turns {
        return items;
    }

    let cut_idx = turn_starts[turn_starts.len() - max_turns];
    let inside_dropped = fence_parity_across(&items[..cut_idx]);
    let mut kept = Vec::with_capacity(items.len() - cut_idx + usize::from(inside_dropped));
    if inside_dropped {
        let in_live = items.get(cut_idx).is_some_and(|l| l.in_live);
        kept.push(make_synthetic_fence(in_live));
    }
    kept.extend(items.into_iter().skip(cut_idx));
    kept
}

fn normalize_spacing_and_style(
    items: Vec<ProcessedLine>,
    bold_headings: bool,
) -> Vec<ProcessedLine> {
    let mut res: Vec<ProcessedLine> = Vec::with_capacity(items.len() + 8);
    let mut in_fence_block = false;
    let mut need_blank_after = false;

    for mut item in items {
        match &mut item.kind {
            ProcessedLineKind::Blank => {
                if !res.is_empty()
                    && !matches!(res.last().map(|l| &l.kind), Some(ProcessedLineKind::Blank))
                {
                    res.push(item);
                    need_blank_after = false;
                }
            }
            ProcessedLineKind::Divider => {
                if matches!(
                    res.last().map(|l| &l.kind),
                    Some(ProcessedLineKind::Divider)
                ) || (res.len() >= 2
                    && matches!(res.last().map(|l| &l.kind), Some(ProcessedLineKind::Blank))
                    && matches!(
                        res.get(res.len() - 2).map(|l| &l.kind),
                        Some(ProcessedLineKind::Divider)
                    ))
                {
                    need_blank_after = true;
                    continue;
                }
                if !res.is_empty()
                    && !matches!(res.last().map(|l| &l.kind), Some(ProcessedLineKind::Blank))
                {
                    res.push(ProcessedLine {
                        in_live: item.in_live,
                        is_process: false,
                        is_code_body: false,
                        is_fence: false,
                        is_turn_start: false,
                        is_shell_prompt: false,
                        kind: ProcessedLineKind::Blank,
                    });
                }
                res.push(item);
                need_blank_after = true;
            }
            ProcessedLineKind::Text { runs, text, .. } => {
                if bold_headings
                    && !item.is_process
                    && !item.is_code_body
                    && !item.is_fence
                    && is_markdown_heading(text)
                {
                    promote_line_style(runs, ReaderRunStyle::Bold);
                }

                let opening_fence = item.is_fence && !in_fence_block;
                let closing_fence = item.is_fence && in_fence_block;

                if (need_blank_after || opening_fence)
                    && !res.is_empty()
                    && !matches!(res.last().map(|l| &l.kind), Some(ProcessedLineKind::Blank))
                {
                    res.push(ProcessedLine {
                        in_live: item.in_live,
                        is_process: false,
                        is_code_body: false,
                        is_fence: false,
                        is_turn_start: false,
                        is_shell_prompt: false,
                        kind: ProcessedLineKind::Blank,
                    });
                }
                need_blank_after = false;

                if opening_fence {
                    in_fence_block = true;
                } else if closing_fence {
                    in_fence_block = false;
                    need_blank_after = true;
                }

                res.push(item);
            }
        }
    }

    while res.first().is_some_and(|l| {
        matches!(
            l.kind,
            ProcessedLineKind::Blank | ProcessedLineKind::Divider
        )
    }) {
        res.remove(0);
    }
    while res.last().is_some_and(|l| {
        matches!(
            l.kind,
            ProcessedLineKind::Blank | ProcessedLineKind::Divider
        )
    }) {
        res.pop();
    }

    res
}

fn finalize_reader_doc(
    title: &str,
    is_agent: bool,
    clean_mode: bool,
    items: Vec<ProcessedLine>,
    anchor_line: usize,
    hidden_process_lines: usize,
) -> ReaderDoc {
    let lines = items
        .into_iter()
        .map(|item| match item.kind {
            ProcessedLineKind::Text { runs, .. } => ReaderLineKind::Text(runs),
            ProcessedLineKind::Blank => ReaderLineKind::Blank,
            ProcessedLineKind::Divider => ReaderLineKind::Divider,
        })
        .collect::<Vec<_>>();
    let clamped_anchor = if lines.is_empty() {
        0
    } else {
        anchor_line.min(lines.len() - 1)
    };
    ReaderDoc {
        title: title.to_string(),
        is_agent,
        clean_mode,
        lines,
        anchor_line: clamped_anchor,
        hidden_process_lines,
    }
}

fn extract_agent_clean(title: &str, raw_lines: Vec<RawLogicalLine>) -> ReaderDoc {
    let mut items = build_processed_lines(raw_lines);
    strip_trailing_agent_chrome(&mut items);
    let items = trim_to_recent_turns(items, AGENT_MAX_TURNS);

    let mut collapsed = Vec::with_capacity(items.len());
    let mut hidden_process_lines = 0usize;
    let mut pending_process_divider = false;
    let mut saw_text = false;

    for mut item in items {
        if item.is_process {
            hidden_process_lines += 1;
            pending_process_divider = true;
            continue;
        }
        match &mut item.kind {
            ProcessedLineKind::Blank => {
                if !pending_process_divider {
                    collapsed.push(item);
                }
            }
            ProcessedLineKind::Divider => {
                pending_process_divider = false;
                collapsed.push(item);
            }
            ProcessedLineKind::Text { runs, text, .. } => {
                if pending_process_divider {
                    if saw_text {
                        collapsed.push(ProcessedLine {
                            in_live: item.in_live,
                            is_process: false,
                            is_code_body: false,
                            is_fence: false,
                            is_turn_start: false,
                            is_shell_prompt: false,
                            kind: ProcessedLineKind::Divider,
                        });
                    }
                    pending_process_divider = false;
                }
                if !item.is_code_body && !item.is_fence && is_agent_user_prompt(text) {
                    item.is_turn_start = true;
                    promote_line_style(runs, ReaderRunStyle::Bold);
                }
                saw_text = true;
                collapsed.push(item);
            }
        }
    }

    let normalized = normalize_spacing_and_style(collapsed, true);

    let last_turn_idx = normalized.iter().rposition(|l| l.is_turn_start);
    let anchor_line = if let Some(turn_idx) = last_turn_idx {
        normalized
            .iter()
            .enumerate()
            .skip(turn_idx + 1)
            .find(|(_, l)| matches!(l.kind, ProcessedLineKind::Text { .. }))
            .map(|(i, _)| i)
            .unwrap_or(turn_idx)
    } else {
        normalized
            .iter()
            .position(|l| l.in_live && matches!(l.kind, ProcessedLineKind::Text { .. }))
            .unwrap_or_else(|| normalized.len().saturating_sub(1))
    };

    finalize_reader_doc(
        title,
        true,
        true,
        normalized,
        anchor_line,
        hidden_process_lines,
    )
}

fn extract_agent_full(title: &str, raw_lines: Vec<RawLogicalLine>) -> ReaderDoc {
    let mut items = build_processed_lines(raw_lines);
    for item in &mut items {
        if !item.is_code_body && !item.is_fence {
            if let ProcessedLineKind::Text { text, .. } = &item.kind {
                if !item.is_process && is_agent_user_prompt(text) {
                    item.is_turn_start = true;
                }
            }
        }
    }

    let normalized = normalize_spacing_and_style(items, true);
    let anchor_line = normalized
        .iter()
        .position(|l| l.in_live && matches!(l.kind, ProcessedLineKind::Text { .. }))
        .or_else(|| {
            normalized
                .iter()
                .rposition(|l| l.is_turn_start)
                .and_then(|turn_idx| {
                    normalized
                        .iter()
                        .enumerate()
                        .skip(turn_idx + 1)
                        .find(|(_, l)| matches!(l.kind, ProcessedLineKind::Text { .. }))
                        .map(|(i, _)| i)
                        .or(Some(turn_idx))
                })
        })
        .unwrap_or_else(|| normalized.len().saturating_sub(1));

    finalize_reader_doc(title, true, false, normalized, anchor_line, 0)
}

fn is_valid_shell_prompt_prefix(prefix: &str) -> bool {
    let chars = prefix.chars().count();
    if chars == 0 || chars > 64 {
        return false;
    }
    if prefix
        .chars()
        .any(|c| matches!(c, '"' | '\'' | '{' | '}' | ';' | '\t' | '='))
    {
        return false;
    }
    let trimmed_end = prefix.trim_end();
    if trimmed_end.is_empty() {
        return false;
    }
    !trimmed_end.ends_with(',')
}

fn is_shell_prompt_line(text: &str) -> bool {
    let indent = text.bytes().take_while(|&b| b == b' ').count();
    if indent > 2 {
        return false;
    }
    let rest = &text[indent..];
    for marker in ["$ ", "% ", "❯ ", "➜ "] {
        if let Some(after) = rest.strip_prefix(marker) {
            return !after.trim().is_empty();
        }
    }
    for marker in ["$ ", "❯ "] {
        if let Some(pos) = rest.find(marker) {
            let prefix = &rest[..pos];
            let after = &rest[pos + marker.len()..];
            if !after.trim().is_empty() && is_valid_shell_prompt_prefix(prefix) {
                return true;
            }
        }
    }
    false
}

fn cap_shell_lines_preserving_fences(
    items: Vec<ProcessedLine>,
    max_lines: usize,
) -> Vec<ProcessedLine> {
    if items.len() <= max_lines {
        return items;
    }

    let mut start_idx = items.len() - max_lines;
    let mut inside_dropped = fence_parity_across(&items[..start_idx]);
    while start_idx < items.len() {
        let kept_len = items.len() - start_idx + usize::from(inside_dropped);
        if kept_len <= max_lines {
            break;
        }
        if items[start_idx].is_fence {
            inside_dropped = !inside_dropped;
        }
        start_idx += 1;
    }

    let mut kept = Vec::with_capacity(items.len() - start_idx + usize::from(inside_dropped));
    if inside_dropped {
        let in_live = items.get(start_idx).is_some_and(|l| l.in_live);
        kept.push(make_synthetic_fence(in_live));
    }
    kept.extend(items.into_iter().skip(start_idx));

    while kept.first().is_some_and(|l| {
        matches!(
            l.kind,
            ProcessedLineKind::Blank | ProcessedLineKind::Divider
        )
    }) {
        kept.remove(0);
    }
    kept
}

fn extract_shell_doc(title: &str, raw_lines: Vec<RawLogicalLine>, clean_mode: bool) -> ReaderDoc {
    let items = build_processed_lines(raw_lines);
    let mut with_dividers = Vec::with_capacity(items.len() + 8);
    let mut has_output_since_prompt = false;

    for mut item in items {
        match &mut item.kind {
            ProcessedLineKind::Blank => {
                with_dividers.push(item);
            }
            ProcessedLineKind::Divider => {
                has_output_since_prompt = false;
                with_dividers.push(item);
            }
            ProcessedLineKind::Text { runs, text, .. } => {
                if !item.is_code_body && !item.is_fence && is_shell_prompt_line(text) {
                    if has_output_since_prompt {
                        with_dividers.push(ProcessedLine {
                            in_live: item.in_live,
                            is_process: false,
                            is_code_body: false,
                            is_fence: false,
                            is_turn_start: false,
                            is_shell_prompt: false,
                            kind: ProcessedLineKind::Divider,
                        });
                    }
                    item.is_shell_prompt = true;
                    promote_line_style(runs, ReaderRunStyle::Bold);
                    has_output_since_prompt = false;
                } else {
                    has_output_since_prompt = true;
                }
                with_dividers.push(item);
            }
        }
    }

    let normalized = normalize_spacing_and_style(with_dividers, false);
    let capped = cap_shell_lines_preserving_fences(normalized, SHELL_MAX_CLEAN_LINES);

    let last_prompt_idx = capped.iter().rposition(|l| l.is_shell_prompt);
    let anchor_line = last_prompt_idx.unwrap_or_else(|| {
        let tail_anchor = capped.len().saturating_sub(1);
        if let Some(live_idx) = capped
            .iter()
            .position(|l| l.in_live && matches!(l.kind, ProcessedLineKind::Text { .. }))
        {
            live_idx.max(capped.len().saturating_sub(24))
        } else {
            tail_anchor
        }
    });

    finalize_reader_doc(title, false, clean_mode, capped, anchor_line, 0)
}

/// Extract a cleaned, structured [`ReaderDoc`] from a terminal `vt100::Screen`.
///
/// Always restores `screen`'s original scrollback offset before returning.
pub fn extract_reader_doc(
    screen: &mut vt100::Screen,
    title: &str,
    is_agent: bool,
    clean_mode: bool,
) -> ReaderDoc {
    let max_scan_rows = if is_agent {
        if clean_mode {
            AGENT_MAX_SCAN_ROWS
        } else {
            FULL_MAX_SCAN_ROWS
        }
    } else {
        SHELL_MAX_SCAN_ROWS
    };

    let raw_lines = scan_screen_logical_lines(screen, max_scan_rows, is_agent);

    if is_agent {
        if clean_mode {
            extract_agent_clean(title, raw_lines)
        } else {
            extract_agent_full(title, raw_lines)
        }
    } else {
        extract_shell_doc(title, raw_lines, clean_mode)
    }
}

struct WrapCluster {
    text: String,
    style: ReaderRunStyle,
    width: usize,
    is_ascii_ws: bool,
}

fn is_regional_indicator(ch: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&ch)
}

fn is_cluster_continuation(ch: char) -> bool {
    matches!(ch, '\u{FE00}'..='\u{FE0F}' | '\u{20E3}' | '\u{1F3FB}'..='\u{1F3FF}' | '\u{E0020}'..='\u{E007F}' | '\u{E0100}'..='\u{E01EF}')
        || (!ch.is_control() && ch != '\u{200D}' && UnicodeWidthChar::width(ch).unwrap_or(0) == 0)
}

fn push_text_clusters(text: &str, style: ReaderRunStyle, atoms: &mut Vec<WrapCluster>) {
    let mut chars = text.chars().peekable();
    while let Some(first) = chars.next() {
        let mut cluster = String::new();
        cluster.push(first);

        if is_regional_indicator(first) && chars.peek().is_some_and(|&c| is_regional_indicator(c)) {
            if let Some(second) = chars.next() {
                cluster.push(second);
            }
        }

        while let Some(&next) = chars.peek() {
            if next == '\u{200D}' {
                cluster.push(chars.next().unwrap());
                if let Some(&joined) = chars.peek() {
                    if !joined.is_control() {
                        cluster.push(chars.next().unwrap());
                    }
                }
            } else if is_cluster_continuation(next) {
                cluster.push(chars.next().unwrap());
            } else {
                break;
            }
        }

        let is_ascii_ws = cluster.len() == 1 && cluster.as_bytes()[0].is_ascii_whitespace();
        let width = UnicodeWidthStr::width(cluster.as_str());
        atoms.push(WrapCluster {
            text: cluster,
            style,
            width,
            is_ascii_ws,
        });
    }
}

fn emit_cluster_slice(
    atoms: &[WrapCluster],
    prefix: Option<(&str, ReaderRunStyle)>,
) -> ReaderLineKind {
    let mut runs: Vec<(String, ReaderRunStyle)> = Vec::new();
    if let Some((pfx, pfx_style)) = prefix {
        push_run(&mut runs, pfx, pfx_style);
    }
    for atom in atoms {
        push_run(&mut runs, &atom.text, atom.style);
    }
    ReaderLineKind::Text(runs)
}

fn wrap_styled_runs(
    runs: &[(String, ReaderRunStyle)],
    content_width: usize,
    out: &mut Vec<ReaderLineKind>,
) {
    let is_code = runs.iter().any(|(_, s)| *s == ReaderRunStyle::Code);
    let (prefix, avail_w): (Option<(&str, ReaderRunStyle)>, usize) =
        if is_code && content_width >= 6 {
            (Some(("│ ", ReaderRunStyle::Muted)), content_width - 2)
        } else if is_code && content_width >= 4 {
            (Some((" ", ReaderRunStyle::Muted)), content_width - 1)
        } else {
            (None, content_width)
        };

    let mut atoms: Vec<WrapCluster> = Vec::new();
    for (text, style) in runs {
        push_text_clusters(text, *style, &mut atoms);
    }

    if atoms.is_empty() {
        out.push(emit_cluster_slice(&[], prefix));
        return;
    }

    let mut pos = 0usize;
    while pos < atoms.len() {
        let mut i = pos;
        let mut w = 0usize;
        let mut last_ascii_ws: Option<usize> = None;

        while i < atoms.len() {
            let cw = atoms[i].width;
            if i > pos && w + cw > avail_w {
                break;
            }
            if i > pos && atoms[i].is_ascii_ws {
                last_ascii_ws = Some(i);
            }
            w += cw;
            i += 1;
        }

        if i == atoms.len() {
            out.push(emit_cluster_slice(&atoms[pos..], prefix));
            break;
        }

        if atoms[i].is_ascii_ws && atoms[pos..i].iter().any(|c| !c.is_ascii_ws) {
            out.push(emit_cluster_slice(&atoms[pos..i], prefix));
            pos = i + 1;
            if !is_code {
                while pos < atoms.len() && atoms[pos].is_ascii_ws {
                    pos += 1;
                }
            }
            continue;
        }

        if let Some(ws_idx) = last_ascii_ws {
            if atoms[pos..ws_idx].iter().any(|c| !c.is_ascii_ws) {
                let mut row_end = ws_idx;
                while row_end > pos && atoms[row_end - 1].is_ascii_ws {
                    row_end -= 1;
                }
                out.push(emit_cluster_slice(&atoms[pos..row_end], prefix));
                pos = ws_idx + 1;
                if !is_code {
                    while pos < atoms.len() && atoms[pos].is_ascii_ws {
                        pos += 1;
                    }
                }
                continue;
            }
        }

        out.push(emit_cluster_slice(&atoms[pos..i], prefix));
        pos = i;
    }
}

/// Wrap logical lines of a [`ReaderDoc`] to `content_width` terminal columns.
///
/// Uses `unicode_width` for CJK-safe width calculation, prefers ASCII whitespace
/// word boundaries, prefixes `Code` lines cleanly, and tracks `anchor_visual_row`.
pub fn wrap_reader_doc(doc: &ReaderDoc, content_width: u16) -> WrappedReaderView {
    let max_w = usize::from(content_width.max(1));
    let mut rows = Vec::with_capacity(doc.lines.len());
    let mut anchor_visual_row = 0usize;

    for (line_idx, line) in doc.lines.iter().enumerate() {
        if line_idx == doc.anchor_line {
            anchor_visual_row = rows.len();
        }
        match line {
            ReaderLineKind::Blank => rows.push(ReaderLineKind::Blank),
            ReaderLineKind::Divider => rows.push(ReaderLineKind::Divider),
            ReaderLineKind::Text(runs) => wrap_styled_runs(runs, max_w, &mut rows),
        }
    }

    if doc.lines.is_empty() {
        anchor_visual_row = 0;
    } else if doc.anchor_line >= doc.lines.len() {
        anchor_visual_row = rows.len().saturating_sub(1);
    }

    WrappedReaderView {
        rows,
        anchor_visual_row,
    }
}

#[derive(serde::Serialize)]
struct ShellDeckLine {
    r: Vec<(String, u8)>,
}

#[derive(serde::Serialize)]
struct ShellDeckPayload<'a> {
    v: u8,
    title: &'a str,
    anchor: usize,
    lines: Vec<ShellDeckLine>,
}

fn is_reader_line_fence(line: &ReaderLineKind) -> bool {
    match line {
        ReaderLineKind::Text(runs) => {
            let joined: String = runs.iter().map(|(t, _)| t.as_str()).collect();
            joined.trim_start().starts_with("```")
        }
        ReaderLineKind::Blank | ReaderLineKind::Divider => false,
    }
}

fn build_shelldeck_json(doc: &ReaderDoc, converted: &[ShellDeckLine], start_idx: usize) -> String {
    let mut inside_dropped_fence = false;
    for dropped in &doc.lines[..start_idx] {
        if is_reader_line_fence(dropped) {
            inside_dropped_fence = !inside_dropped_fence;
        }
    }

    let slice = &converted[start_idx..];
    let mut lines_out = Vec::with_capacity(slice.len() + usize::from(inside_dropped_fence));
    if inside_dropped_fence {
        lines_out.push(ShellDeckLine {
            r: vec![("```".to_string(), 0)],
        });
    }
    for line in slice {
        lines_out.push(ShellDeckLine { r: line.r.clone() });
    }

    let anchor = if lines_out.is_empty() {
        0
    } else if doc.anchor_line >= start_idx {
        ((doc.anchor_line - start_idx) + usize::from(inside_dropped_fence)).min(lines_out.len() - 1)
    } else {
        usize::from(inside_dropped_fence).min(lines_out.len() - 1)
    };

    let payload = ShellDeckPayload {
        v: 1,
        title: &doc.title,
        anchor,
        lines: lines_out,
    };
    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
}

/// Serialize a [`ReaderDoc`] into ShellDeck v1 reader JSON bounded to [`MAX_SHELLDECK_JSON_BYTES`].
///
/// Preserves opening/closing ```` ``` ```` code fence parity when older lines are trimmed.
pub fn format_reader_doc_json(doc: &ReaderDoc) -> String {
    let converted: Vec<ShellDeckLine> = doc
        .lines
        .iter()
        .map(|line| match line {
            ReaderLineKind::Text(runs) => ShellDeckLine {
                r: runs
                    .iter()
                    .map(|(t, s)| (t.clone(), s.to_shelldeck_code()))
                    .collect(),
            },
            ReaderLineKind::Blank => ShellDeckLine { r: Vec::new() },
            ReaderLineKind::Divider => ShellDeckLine {
                r: vec![("────────".to_string(), 2)],
            },
        })
        .collect();

    let full_json = build_shelldeck_json(doc, &converted, 0);
    if full_json.len() <= MAX_SHELLDECK_JSON_BYTES || doc.lines.len() <= 1 {
        return full_json;
    }

    let mut lo = 1usize;
    let mut hi = doc.lines.len() - 1;
    let mut best_json = build_shelldeck_json(doc, &converted, hi);
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let candidate = build_shelldeck_json(doc, &converted, mid);
        if candidate.len() <= MAX_SHELLDECK_JSON_BYTES {
            best_json = candidate;
            if mid == 0 {
                break;
            }
            hi = mid - 1;
        } else {
            lo = mid + 1;
        }
    }
    best_json
}

// ---------------------------------------------------------------------------
// TODO(phase-2b): Replace this temporary adapter block with direct calls to
// `crate::ui::{classify_eink_process_row, is_eink_muted_cell, vt_row_trimmed_starts_with_fence}`
// once Batch 1 has merged into `origin/main`.
// ---------------------------------------------------------------------------

fn xterm256_rgb_fallback(idx: u8) -> (u8, u8, u8) {
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

fn is_muted_cell_fallback(cell: &vt100::Cell) -> bool {
    if cell.dim() {
        return true;
    }
    if cell.bold() {
        return false;
    }
    let rgb = match cell.fgcolor() {
        vt100::Color::Default => return false,
        vt100::Color::Idx(i) => xterm256_rgb_fallback(i),
        vt100::Color::Rgb(r, g, b) => (r, g, b),
    };
    let (r, g, b) = rgb;
    let luma =
        ((299u32 * u32::from(r) + 587u32 * u32::from(g) + 114u32 * u32::from(b)) / 1000) as u8;
    let chroma = r.max(g).max(b) - r.min(g).min(b);
    matches!(cell.fgcolor(), vt100::Color::Idx(8 | 238..=249))
        || (chroma <= 42 && (70..=180).contains(&luma))
}

struct ScannedRowCell {
    col: u16,
    byte_start: usize,
    is_space: bool,
    is_muted: bool,
}

struct ScannedRow {
    text: String,
    cells: Vec<ScannedRowCell>,
    first_idx: usize,
    last_idx: usize,
    leading_spaces: u16,
    all_non_space_muted: bool,
}

impl ScannedRow {
    fn trimmed(&self) -> (&str, usize) {
        let start = self.cells[self.first_idx].byte_start;
        let end = self
            .cells
            .get(self.last_idx + 1)
            .map_or(self.text.len(), |c| c.byte_start);
        (&self.text[start..end], start)
    }
}

fn scan_row_text_fallback(screen: &vt100::Screen, r: u16, cols: u16) -> Option<ScannedRow> {
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
        let is_muted = !is_space && is_muted_cell_fallback(cell);
        cells.push(ScannedRowCell {
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
    Some(ScannedRow {
        text,
        cells,
        first_idx,
        last_idx,
        leading_spaces,
        all_non_space_muted,
    })
}

fn vt_row_trimmed_starts_with_fence_fallback(screen: &vt100::Screen, r: u16, cols: u16) -> bool {
    scan_row_text_fallback(screen, r, cols).is_some_and(|scan| {
        let (trimmed, _) = scan.trimmed();
        trimmed
            .trim_start_matches(['│', '┃', '║', ' '])
            .starts_with("```")
    })
}

fn row_has_box_frame_fallback(trimmed: &str) -> bool {
    let check_str = trimmed.strip_prefix("└ ").unwrap_or(trimmed);
    if check_str.chars().any(|ch| {
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
    }) {
        return true;
    }
    (trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() >= 3)
        || ((trimmed.starts_with('|') || trimmed.ends_with('|')) && trimmed.contains(" | "))
}

fn looks_like_numbered_code_line_fallback(trimmed: &str) -> bool {
    let bytes = trimmed.as_bytes();
    let mut digits = 0usize;
    while digits < bytes.len() && digits < 6 && bytes[digits].is_ascii_digit() {
        digits += 1;
    }
    if digits == 0 || digits >= bytes.len() {
        return false;
    }
    matches!(
        trimmed[digits..].chars().next(),
        Some(' ' | ':' | '│' | '|')
    )
}

fn is_collapsed_action_summary_row_fallback(trimmed: &str) -> bool {
    const NOUNS: &[&str] = &[
        "action", "actions", "step", "steps", "tool", "tools", "read", "reads", "edit", "edits",
        "command", "commands", "call", "calls", "file", "files", "search", "searches", "message",
        "messages", "item", "items",
    ];
    let Some(rest) = trimmed
        .strip_prefix("… ")
        .or_else(|| trimmed.strip_prefix("... "))
    else {
        return false;
    };
    let rest = rest.trim_start();
    let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return false;
    }
    let Some(after_more) = rest[digits..].strip_prefix(" more ") else {
        return false;
    };
    let noun = after_more
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .unwrap_or("");
    NOUNS.iter().any(|&n| noun.eq_ignore_ascii_case(n))
}

fn has_tui_process_hint_fallback(trimmed: &str) -> bool {
    if trimmed.contains("\"(ctrl+") || trimmed.contains("\"(esc ") {
        return false;
    }
    trimmed.contains("(ctrl+o to expand)")
        || trimmed.contains("(ctrl+r to expand)")
        || trimmed.contains("(ctrl+o to show")
        || trimmed.contains("(esc to interrupt)")
        || trimmed.contains("(esc to cancel)")
}

fn is_braille_char_fb(s: &str) -> bool {
    let mut chars = s.chars();
    matches!((chars.next(), chars.next()), (Some(ch), None) if ('\u{2800}'..='\u{28FF}').contains(&ch))
}

fn is_arc_or_box_char_fb(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some(ch), None) if ('\u{2500}'..='\u{257F}').contains(&ch) || ('\u{25DC}'..='\u{25E1}').contains(&ch)
    )
}

fn has_cardinal_neighbor_fb<F: Fn(&str) -> bool>(
    screen: &vt100::Screen,
    r: u16,
    c: u16,
    rows: u16,
    cols: u16,
    pred: F,
) -> bool {
    (c > 0 && screen.cell(r, c - 1).is_some_and(|lc| pred(lc.contents())))
        || (c + 1 < cols && screen.cell(r, c + 1).is_some_and(|rc| pred(rc.contents())))
        || (r > 0 && screen.cell(r - 1, c).is_some_and(|uc| pred(uc.contents())))
        || (r + 1 < rows && screen.cell(r + 1, c).is_some_and(|dc| pred(dc.contents())))
}

fn is_leading_spinner_status_row_fallback(
    screen: &vt100::Screen,
    r: u16,
    rows: u16,
    cols: u16,
    first_col: u16,
    trimmed: &str,
) -> bool {
    let Some(first_cell) = screen.cell(r, first_col) else {
        return false;
    };
    if !first_cell.has_contents() {
        return false;
    }
    let mut chars = first_cell.contents().chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else {
        return false;
    };
    let is_spinner = if ('\u{2801}'..='\u{28FF}').contains(&ch) {
        !has_cardinal_neighbor_fb(screen, r, first_col, rows, cols, is_braille_char_fb)
    } else if ('\u{25DC}'..='\u{25E1}').contains(&ch) {
        !has_cardinal_neighbor_fb(screen, r, first_col, rows, cols, is_arc_or_box_char_fb)
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

fn has_muted_parenthetical_tail_fb(scan: &ScannedRow, rest: &str, rest_byte: usize) -> bool {
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

fn starts_with_tool_call_syntax_fb(rest: &str) -> bool {
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

fn classify_process_row_fallback(
    screen: &vt100::Screen,
    r: u16,
    rows: u16,
    cols: u16,
    prev_wrapped: bool,
    prev_is_process: bool,
) -> bool {
    let Some(scan) = scan_row_text_fallback(screen, r, cols) else {
        return false;
    };
    let (trimmed, trimmed_byte_start) = scan.trimmed();
    let default_col = scan.cells[scan.first_idx].col;
    let (content_trimmed, content_byte_start, content_first_col) = if let Some(bar_ch) =
        trimmed.chars().next()
    {
        if matches!(bar_ch, '┃' | '▎' | '▍' | '▌' | '▋' | '▊' | '▉' | '▏' | '▕') {
            let after_bar = &trimmed[bar_ch.len_utf8()..];
            let inner = after_bar.trim_start();
            if after_bar.starts_with(' ') && !inner.is_empty() {
                let inner_byte =
                    trimmed_byte_start + bar_ch.len_utf8() + (after_bar.len() - inner.len());
                let inner_col = scan.cells[scan.first_idx..=scan.last_idx]
                    .iter()
                    .find(|c| c.byte_start == inner_byte)
                    .map_or(default_col, |c| c.col);
                (inner, inner_byte, inner_col)
            } else {
                (trimmed, trimmed_byte_start, default_col)
            }
        } else {
            (trimmed, trimmed_byte_start, default_col)
        }
    } else {
        (trimmed, trimmed_byte_start, default_col)
    };

    if trimmed.is_empty() || row_has_box_frame_fallback(content_trimmed) {
        return false;
    }
    if prev_wrapped && prev_is_process {
        return true;
    }
    if looks_like_numbered_code_line_fallback(content_trimmed) {
        return false;
    }
    if is_collapsed_action_summary_row_fallback(content_trimmed) {
        return true;
    }
    if prev_is_process
        && (content_trimmed.starts_with('⎿')
            || content_trimmed.starts_with('↳')
            || content_trimmed.starts_with("└ ")
            || (scan.leading_spaces >= 2 && scan.all_non_space_muted))
    {
        return true;
    }
    if has_tui_process_hint_fallback(content_trimmed) {
        return true;
    }
    if is_leading_spinner_status_row_fallback(
        screen,
        r,
        rows,
        cols,
        content_first_col,
        content_trimmed,
    ) {
        return true;
    }
    if scan.leading_spaces <= 4
        && content_trimmed
            .strip_prefix("Thought for ")
            .is_some_and(|s| s.starts_with(|c: char| c.is_ascii_digit()))
    {
        return true;
    }

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

    if let Some(after) = content_trimmed.strip_prefix("• ") {
        let rest = after.trim_start();
        if rest.is_empty() {
            return false;
        }
        let rest_byte = content_byte_start + "• ".len() + (after.len() - rest.len());
        return CODEX_VERBS.iter().any(|v| rest.starts_with(v))
            && (scan.all_non_space_muted
                || has_muted_parenthetical_tail_fb(&scan, rest, rest_byte));
    }

    let Some((marker, after)) = PROCESS_MARKERS
        .iter()
        .find_map(|&m| content_trimmed.strip_prefix(m).map(|rest| (m, rest)))
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
        || starts_with_tool_call_syntax_fb(rest)
    {
        return true;
    }
    let rest_byte = content_byte_start + marker.len() + (after.len() - rest.len());
    if matches!(marker, "○ " | "● " | "◈ ")
        && has_muted_parenthetical_tail_fb(&scan, rest, rest_byte)
    {
        return true;
    }
    matches!(marker, "⎿" | "↳") && scan.all_non_space_muted
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn nz(v: u16) -> std::num::NonZeroU16 {
        std::num::NonZeroU16::new(v).unwrap()
    }

    fn line_text(line: &ReaderLineKind) -> String {
        match line {
            ReaderLineKind::Text(runs) => runs.iter().map(|(t, _)| t.as_str()).collect(),
            ReaderLineKind::Blank => String::new(),
            ReaderLineKind::Divider => "────────".to_string(),
        }
    }

    #[test]
    fn agent_clean_hides_process_rows_keeps_latest_three_turns_and_anchors_body() {
        let mut parser = vt100::Parser::new(nz(40), nz(80), 400);
        let script = concat!(
            "❯ Turn 1\r\n",
            "● Thought for 1s\r\n",
            "Answer 1\r\n\r\n",
            "❯ Turn 2\r\n",
            "● Used Read(src/main.rs)\r\n",
            "Answer 2\r\n\r\n",
            "❯ Turn 3\r\n",
            "● Ran cargo test\r\n",
            "Answer 3\r\n\r\n",
            "❯ Turn 4 latest\r\n",
            "○ Thought for 2s\r\n",
            "● Used Search(pattern)\r\n",
            "… 10 more actions (3 reads, 6 edits, 1 tool) …\r\n",
            "## Summary\r\n",
            "Here is the clean answer.\r\n",
            "```rust\r\n",
            "fn ok() {}\r\n",
            "```\r\n",
            "────────────────────────────────────────\r\n",
            "╭──────────────────────╮\r\n",
            "│ ❯ Type a message...  │\r\n",
            "╰──────────────────────╯\r\n",
            "\x1b[2m? for shortcuts · shift+tab to cycle\x1b[0m"
        );
        parser.process(script.as_bytes());

        let doc = extract_reader_doc(parser.screen_mut(), "agent-tab", true, true);
        assert!(doc.is_agent);
        assert!(doc.clean_mode);
        assert_eq!(doc.hidden_process_lines, 5);

        let texts: Vec<String> = doc.lines.iter().map(line_text).collect();
        // Turn 1 trimmed off, Turns 2..=4 retained.
        assert!(!texts.iter().any(|t| t == "❯ Turn 1"));
        assert!(!texts.iter().any(|t| t == "Answer 1"));
        assert!(texts.iter().any(|t| t == "❯ Turn 2"));
        assert!(texts.iter().any(|t| t == "❯ Turn 3"));
        assert!(texts.iter().any(|t| t == "❯ Turn 4 latest"));

        // Process lines hidden; trailing chrome stripped; dividers collapsed between prompt & body.
        assert!(!texts.iter().any(|t| t.contains("Thought for")));
        assert!(!texts.iter().any(|t| t.contains("10 more actions")));
        assert!(!texts.iter().any(|t| t.contains("Type a message")));
        assert!(!texts.iter().any(|t| t.contains("? for shortcuts")));
        assert!(doc.lines.contains(&ReaderLineKind::Divider));

        // Heading styled Bold, code body styled Code, and fences balanced.
        let summary_idx = texts.iter().position(|t| t == "## Summary").unwrap();
        assert_eq!(
            doc.lines[summary_idx],
            ReaderLineKind::Text(vec![("## Summary".to_string(), ReaderRunStyle::Bold)])
        );
        let code_idx = texts.iter().position(|t| t == "fn ok() {}").unwrap();
        assert_eq!(
            doc.lines[code_idx],
            ReaderLineKind::Text(vec![("fn ok() {}".to_string(), ReaderRunStyle::Code)])
        );
        let fence_count = texts
            .iter()
            .filter(|t| t.trim_start().starts_with("```"))
            .count();
        assert_eq!(fence_count, 2);

        // Anchor points at the first non-blank/non-divider line of Turn 4's assistant body ("## Summary").
        assert_eq!(doc.anchor_line, summary_idx);
    }

    #[test]
    fn agent_full_keeps_all_process_rows_as_muted() {
        let mut parser = vt100::Parser::new(nz(20), nz(80), 100);
        parser.process(
            concat!(
                "❯ Inspect code\r\n",
                "○ Thought for 3s\r\n",
                "● Used Read(src/lib.rs)\r\n",
                "… 4 more steps …\r\n",
                "Final response.\r\n"
            )
            .as_bytes(),
        );

        let doc = extract_reader_doc(parser.screen_mut(), "full-tab", true, false);
        assert!(doc.is_agent);
        assert!(!doc.clean_mode);
        assert_eq!(doc.hidden_process_lines, 0);

        let texts: Vec<String> = doc.lines.iter().map(line_text).collect();
        let thought_idx = texts.iter().position(|t| t == "○ Thought for 3s").unwrap();
        let tool_idx = texts
            .iter()
            .position(|t| t == "● Used Read(src/lib.rs)")
            .unwrap();
        let collapsed_idx = texts.iter().position(|t| t == "… 4 more steps …").unwrap();
        for idx in [thought_idx, tool_idx, collapsed_idx] {
            match &doc.lines[idx] {
                ReaderLineKind::Text(runs) => {
                    assert!(runs.iter().all(|(_, s)| *s == ReaderRunStyle::Muted));
                }
                other => panic!("expected Text muted line, got {other:?}"),
            }
        }
        assert!(texts.iter().any(|t| t == "Final response."));
    }

    #[test]
    fn shell_mode_caps_200_lines_adds_command_dividers_and_preserves_process_lines() {
        let mut parser = vt100::Parser::new(nz(24), nz(80), 300);
        parser.process(b"$ echo start\r\n");
        parser.process(b"```sh\r\n");
        for i in 0..220 {
            parser.process(format!("line {i:03}\r\n").as_bytes());
        }
        parser.process(b"```\r\n");
        parser.process(b"~/proj$ git status\r\n");
        parser.process("● Running script\r\n".as_bytes());
        parser.process(b"clean tree");

        let doc = extract_reader_doc(parser.screen_mut(), "sh", false, true);
        assert!(!doc.is_agent);
        assert_eq!(doc.hidden_process_lines, 0);
        assert!(doc.lines.len() <= SHELL_MAX_CLEAN_LINES);

        let texts: Vec<String> = doc.lines.iter().map(line_text).collect();
        // Shell mode keeps process-looking output rather than hiding it.
        assert!(texts.iter().any(|t| t == "● Running script"));

        // Fence parity preserved across the 200-line trim.
        let fence_count = texts
            .iter()
            .filter(|t| t.trim_start().starts_with("```"))
            .count();
        assert_eq!(fence_count % 2, 0);

        // Divider inserted before `~/proj$ git status` preceded by output, prompt styled Bold, and anchored.
        let prompt_idx = texts
            .iter()
            .position(|t| t == "~/proj$ git status")
            .unwrap();
        assert_eq!(
            doc.lines[prompt_idx],
            ReaderLineKind::Text(vec![(
                "~/proj$ git status".to_string(),
                ReaderRunStyle::Bold
            )])
        );
        assert!(prompt_idx >= 2);
        assert!(
            doc.lines[prompt_idx - 1] == ReaderLineKind::Divider
                || doc.lines[prompt_idx - 2] == ReaderLineKind::Divider
        );
        assert_eq!(doc.anchor_line, prompt_idx);
    }

    #[test]
    fn wrap_reader_doc_handles_cjk_widths_word_wrap_code_prefix_and_anchor_tracking() {
        let doc = ReaderDoc {
            title: "wrap".to_string(),
            is_agent: true,
            clean_mode: true,
            lines: vec![
                ReaderLineKind::Text(vec![(
                    "alpha beta gamma delta".to_string(),
                    ReaderRunStyle::Body,
                )]),
                ReaderLineKind::Blank,
                ReaderLineKind::Text(vec![(
                    "中文宽字符测试继续换行".to_string(),
                    ReaderRunStyle::Bold,
                )]),
                ReaderLineKind::Text(vec![(
                    "let long_value = compute(123456);".to_string(),
                    ReaderRunStyle::Code,
                )]),
            ],
            anchor_line: 2,
            hidden_process_lines: 0,
        };

        let wrapped = wrap_reader_doc(&doc, 12);
        // "alpha beta" (10) + "gamma delta" (11) -> 2 rows, Blank -> row 2, anchor line 2 -> row 3.
        assert_eq!(wrapped.anchor_visual_row, 3);
        assert_eq!(line_text(&wrapped.rows[0]), "alpha beta");
        assert_eq!(line_text(&wrapped.rows[1]), "gamma delta");
        assert_eq!(wrapped.rows[2], ReaderLineKind::Blank);

        for row in &wrapped.rows {
            if let ReaderLineKind::Text(runs) = row {
                let s: String = runs.iter().map(|(t, _)| t.as_str()).collect();
                assert!(
                    s.width() <= 12,
                    "visual row `{s}` exceeded width 12 ({})",
                    s.width()
                );
            }
        }

        // Code line rows should carry clean `│ ` prefix.
        let code_rows: Vec<String> = wrapped
            .rows
            .iter()
            .filter(|r| match r {
                ReaderLineKind::Text(runs) => {
                    runs.iter().any(|(_, st)| *st == ReaderRunStyle::Code)
                }
                _ => false,
            })
            .map(line_text)
            .collect();
        assert!(code_rows.len() >= 2);
        assert!(code_rows.iter().all(|r| r.starts_with("│ ")));
    }

    #[test]
    fn extract_reader_doc_always_restores_scrollback() {
        let mut parser = vt100::Parser::new(nz(4), nz(30), 40);
        for i in 0..12 {
            parser.process(format!("row {i}\r\n").as_bytes());
        }
        parser.screen_mut().set_scrollback(5);
        let _ = extract_reader_doc(parser.screen_mut(), "sb", true, true);
        assert_eq!(parser.screen().scrollback(), 5);
    }

    #[test]
    fn format_reader_doc_json_serializes_v1_schema_and_caps_48kb_with_fence_parity() {
        let sample = ReaderDoc {
            title: "demo".to_string(),
            is_agent: true,
            clean_mode: true,
            lines: vec![
                ReaderLineKind::Text(vec![("# Heading".to_string(), ReaderRunStyle::Bold)]),
                ReaderLineKind::Blank,
                ReaderLineKind::Divider,
                ReaderLineKind::Text(vec![
                    ("body ".to_string(), ReaderRunStyle::Body),
                    ("muted".to_string(), ReaderRunStyle::Muted),
                ]),
                ReaderLineKind::Text(vec![("code()".to_string(), ReaderRunStyle::Code)]),
            ],
            anchor_line: 3,
            hidden_process_lines: 2,
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&format_reader_doc_json(&sample)).unwrap();
        assert_eq!(parsed["v"], 1);
        assert_eq!(parsed["title"], "demo");
        assert_eq!(parsed["anchor"], 3);
        let lines = parsed["lines"].as_array().unwrap();
        assert_eq!(lines[0]["r"][0][1], 1); // Bold -> 1
        assert_eq!(lines[1]["r"].as_array().unwrap().len(), 0); // Blank -> []
        assert_eq!(lines[2]["r"][0][0], "────────");
        assert_eq!(lines[2]["r"][0][1], 2); // Divider -> ["────────", 2]
        assert_eq!(lines[3]["r"][0][1], 0); // Body -> 0
        assert_eq!(lines[3]["r"][1][1], 2); // Muted -> 2
        assert_eq!(lines[4]["r"][0][1], 0); // Code -> 0

        // Large document (> 48KB) inside a ``` code fence trims cleanly with even fence parity.
        let mut huge_lines = vec![ReaderLineKind::Text(vec![(
            "```rust".to_string(),
            ReaderRunStyle::Body,
        )])];
        for i in 0..800 {
            huge_lines.push(ReaderLineKind::Text(vec![(
                format!(
                    "// line {i:04} 0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                ),
                ReaderRunStyle::Code,
            )]));
        }
        huge_lines.push(ReaderLineKind::Text(vec![(
            "```".to_string(),
            ReaderRunStyle::Body,
        )]));
        huge_lines.push(ReaderLineKind::Text(vec![(
            "After code".to_string(),
            ReaderRunStyle::Body,
        )]));

        let huge_doc = ReaderDoc {
            title: "huge".to_string(),
            is_agent: true,
            clean_mode: true,
            lines: huge_lines,
            anchor_line: 802,
            hidden_process_lines: 0,
        };
        let json = format_reader_doc_json(&huge_doc);
        assert!(json.len() <= MAX_SHELLDECK_JSON_BYTES);
        let huge_parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let fence_count = huge_parsed["lines"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| {
                l["r"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|run| run[0].as_str().unwrap())
                    .collect::<String>()
                    .trim_start()
                    .starts_with("```")
            })
            .count();
        assert_eq!(fence_count % 2, 0);
    }

    #[test]
    fn agent_clean_strips_box_frames_rules_and_all_trailing_chrome_variants() {
        let mut parser = vt100::Parser::new(nz(20), nz(60), 40);
        parser.process(
            concat!(
                "╭─── Notes ───╮\r\n",
                "│ > Ask one   │\r\n",
                "╰─────────────╯\r\n",
                "You: Follow-up\r\n",
                "User: Final question\r\n",
                "# Top Heading\r\n",
                "━━━━━━━━━━━━━━\r\n",
                "```md\r\n",
                "> quoted inside fence is not a turn\r\n",
                "```\r\n",
                "After fence.\r\n",
                "══════════════\r\n",
                "› Send a message...\r\n",
                "(ctrl+o to expand)\r\n",
                "(esc to interrupt)\r\n",
                "❯\r\n",
                "\x1b[2mclaude-sonnet · 90% context\x1b[0m"
            )
            .as_bytes(),
        );

        let doc = extract_reader_doc(parser.screen_mut(), "variants", true, true);
        let texts: Vec<String> = doc.lines.iter().map(line_text).collect();

        assert!(texts.iter().any(|t| t == "Notes"));
        assert!(texts.iter().any(|t| t == "> Ask one"));
        assert!(texts.iter().any(|t| t == "You: Follow-up"));
        assert!(texts.iter().any(|t| t == "User: Final question"));
        assert!(!texts.iter().any(|t| t.contains("Send a message")));
        assert!(!texts.iter().any(|t| t.contains("ctrl+o to expand")));
        assert!(!texts.iter().any(|t| t.contains("esc to interrupt")));
        assert!(!texts.iter().any(|t| t.contains("90% context")));
        assert!(!texts.iter().any(|t| t == "❯"));

        // `>` inside code fence must NOT act as a user prompt turn, so `# Top Heading` remains the anchor.
        let heading_idx = texts.iter().position(|t| t == "# Top Heading").unwrap();
        assert_eq!(doc.anchor_line, heading_idx);
        assert!(doc.lines.contains(&ReaderLineKind::Divider));
    }

    #[test]
    fn shell_mode_anchors_near_tail_when_no_prompts_and_handles_narrow_cjk_wrap() {
        let mut parser = vt100::Parser::new(nz(24), nz(50), 300);
        for i in 0..240 {
            parser.process(format!("log entry {i:03}\r\n").as_bytes());
        }
        let doc = extract_reader_doc(parser.screen_mut(), "logs", false, true);
        assert_eq!(doc.lines.len(), SHELL_MAX_CLEAN_LINES);
        assert!(doc.anchor_line >= SHELL_MAX_CLEAN_LINES.saturating_sub(24));

        // Very narrow width (3 columns) with CJK + Code runs must not loop or panic.
        let narrow_doc = ReaderDoc {
            title: "narrow".to_string(),
            is_agent: true,
            clean_mode: true,
            lines: vec![
                ReaderLineKind::Text(vec![("中a文b".to_string(), ReaderRunStyle::Body)]),
                ReaderLineKind::Text(vec![("fn x()".to_string(), ReaderRunStyle::Code)]),
            ],
            anchor_line: 1,
            hidden_process_lines: 0,
        };
        let wrapped = wrap_reader_doc(&narrow_doc, 3);
        assert!(wrapped.rows.len() >= 3);
        assert_eq!(wrapped.anchor_visual_row, 2);
    }

    #[test]
    fn shell_percentage_logs_do_not_false_trigger_prompts_and_long_prose_not_stripped() {
        let mut parser = vt100::Parser::new(nz(20), nz(80), 40);
        parser.process(
            concat!(
                "$ cargo test\r\n",
                "Progress: 50% complete\r\n",
                "100% passed\r\n",
                "CPU 12% avg\r\n",
                "% echo zsh_prompt\r\n",
                "➜ echo starship_prompt\r\n",
                "~/repo❯ echo prefixed_chevron"
            )
            .as_bytes(),
        );
        let doc = extract_reader_doc(parser.screen_mut(), "pct", false, true);
        let texts: Vec<String> = doc.lines.iter().map(line_text).collect();

        // Percentage output lines must remain Body (not Bold prompts) and must NOT hijack anchor.
        for pct_line in ["Progress: 50% complete", "100% passed", "CPU 12% avg"] {
            let idx = texts.iter().position(|t| t == pct_line).unwrap();
            assert_eq!(
                doc.lines[idx],
                ReaderLineKind::Text(vec![(pct_line.to_string(), ReaderRunStyle::Body)])
            );
        }
        let last_prompt_idx = texts
            .iter()
            .position(|t| t == "~/repo❯ echo prefixed_chevron")
            .unwrap();
        assert_eq!(doc.anchor_line, last_prompt_idx);
        assert_eq!(
            doc.lines[last_prompt_idx],
            ReaderLineKind::Text(vec![(
                "~/repo❯ echo prefixed_chevron".to_string(),
                ReaderRunStyle::Bold
            )])
        );

        // Long trailing prose (> 120 chars) mentioning `shift+tab` or `? for shortcuts` stays intact.
        let mut agent_parser = vt100::Parser::new(nz(10), nz(180), 20);
        let long_prose = "In this long explanation we carefully describe how users press shift+tab or check ? for shortcuts in interactive terminal sessions across multiple workflows.";
        assert!(long_prose.chars().count() > 120);
        agent_parser
            .process(format!("❯ Explain keys\r\n{long_prose}\r\n? for shortcuts").as_bytes());
        let agent_doc = extract_reader_doc(agent_parser.screen_mut(), "prose", true, true);
        let agent_texts: Vec<String> = agent_doc.lines.iter().map(line_text).collect();
        assert!(agent_texts.iter().any(|t| t == long_prose));
        assert!(!agent_texts.iter().any(|t| t == "? for shortcuts"));
    }

    #[test]
    fn wrap_reader_doc_keeps_zwj_emoji_clusters_intact_at_narrow_widths() {
        let doc = ReaderDoc {
            title: "emoji".to_string(),
            is_agent: true,
            clean_mode: true,
            lines: vec![ReaderLineKind::Text(vec![(
                "ab👨‍💻cd 👍🏽 🇺🇸".to_string(),
                ReaderRunStyle::Body,
            )])],
            anchor_line: 0,
            hidden_process_lines: 0,
        };

        for width in [2u16, 3, 4] {
            let wrapped = wrap_reader_doc(&doc, width);
            let rows: Vec<String> = wrapped.rows.iter().map(line_text).collect();
            assert!(
                rows.iter().any(|r| r.contains("👨‍💻")),
                "ZWJ cluster split at width {width}: {rows:?}"
            );
            assert!(
                rows.iter().any(|r| r.contains("👍🏽")),
                "skin-tone cluster split at width {width}: {rows:?}"
            );
            assert!(
                rows.iter().any(|r| r.contains("🇺🇸")),
                "regional flag cluster split at width {width}: {rows:?}"
            );
            assert!(!rows.iter().any(|r| r.ends_with('\u{200D}')));
        }
    }
}
