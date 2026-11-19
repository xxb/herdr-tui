//! Lightweight `terminal.read`-first reader extraction, reflow, wrapping, and ShellDeck export.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const AGENT_MAX_SCAN_ROWS: usize = 600;
pub const AGENT_MAX_TURNS: usize = 3;
pub const FULL_MAX_SCAN_ROWS: usize = 420;
pub const SHELL_MAX_SCAN_ROWS: usize = 260;
pub const SHELL_MAX_CLEAN_LINES: usize = 200;
pub const MAX_SHELLDECK_JSON_BYTES: usize = 48_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ReaderRunStyle {
    Body = 0,
    Bold = 1,
    Muted = 2,
    Code = 3,
}

impl ReaderRunStyle {
    pub fn to_shelldeck_code(self) -> u8 {
        match self {
            Self::Body | Self::Code => 0,
            Self::Bold => 1,
            Self::Muted => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReaderLineKind {
    Text(Vec<(String, ReaderRunStyle)>),
    Blank,
    Divider,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReaderDoc {
    pub title: String,
    pub is_agent: bool,
    pub clean_mode: bool,
    pub source_note: Option<String>,
    pub lines: Vec<ReaderLineKind>,
    pub anchor_line: usize,
    pub hidden_process_lines: usize,
}

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

/// Offline fallback: extract soft-wrap-joined plain text from a `vt100::Screen`
/// and always restore the caller's original scrollback offset.
pub fn extract_plain_text_from_screen(screen: &mut vt100::Screen, max_rows: usize) -> String {
    let orig_sb = screen.scrollback();
    let guard = ScrollbackGuard { screen, orig_sb };
    let (rows_nz, cols_nz) = guard.screen.size();
    let (rows, cols) = (usize::from(rows_nz.get()), cols_nz.get());
    let total_sb = guard.screen.scrollback_len();
    let hist_rows = total_sb.min(max_rows.saturating_sub(rows));

    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut push_row = |scr: &vt100::Screen, r: u16| {
        let (mut prev_wide, mut prev_col) = (false, 0u16);
        for c in 0..cols {
            if prev_wide {
                prev_wide = false;
                continue;
            }
            let Some(cell) = scr.cell(r, c) else { continue };
            prev_wide = cell.is_wide();
            if cell.has_contents() {
                if c > prev_col {
                    cur.extend(std::iter::repeat_n(' ', usize::from(c - prev_col)));
                }
                prev_col = c + if cell.is_wide() { 2 } else { 1 };
                cur.push_str(cell.contents());
            }
        }
        if !scr.row_wrapped(r) {
            lines.push(cur.trim_end().to_string());
            cur.clear();
        }
    };

    let mut cur_abs = total_sb.saturating_sub(hist_rows);
    while cur_abs < total_sb {
        guard.screen.set_scrollback(total_sb - cur_abs);
        let take = (total_sb - cur_abs).min(rows);
        for r in 0..take {
            push_row(guard.screen, r as u16);
        }
        cur_abs += take;
    }
    guard.screen.set_scrollback(0);
    for r in rows.saturating_sub(max_rows)..rows {
        push_row(guard.screen, r as u16);
    }
    if !cur.is_empty() {
        lines.push(cur.trim_end().to_string());
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

#[derive(Clone)]
enum RawKind {
    Text(String),
    Blank,
    Divider,
}

#[derive(Clone)]
struct RawLine {
    kind: RawKind,
    is_fence: bool,
    is_code_body: bool,
    is_process: bool,
    is_turn_start: bool,
    is_shell_prompt: bool,
}

impl RawLine {
    fn blank() -> Self {
        Self::meta(RawKind::Blank)
    }

    fn divider() -> Self {
        Self::meta(RawKind::Divider)
    }

    fn meta(kind: RawKind) -> Self {
        Self {
            kind,
            is_fence: false,
            is_code_body: false,
            is_process: false,
            is_turn_start: false,
            is_shell_prompt: false,
        }
    }

    fn text(&self) -> &str {
        match &self.kind {
            RawKind::Text(s) => s,
            RawKind::Blank | RawKind::Divider => "",
        }
    }
}

fn is_box_or_rule_char(c: char) -> bool {
    c.is_whitespace() || ('\u{2500}'..='\u{257F}').contains(&c)
}

fn is_horizontal_rule(s: &str) -> bool {
    let mut rules = 0usize;
    for c in s.chars() {
        if matches!(c, '─' | '━' | '═') {
            rules += 1;
        } else if !c.is_whitespace() {
            return false;
        }
    }
    rules >= 3
}

fn clean_box_and_rule_line(raw: &str, in_code_fence: bool) -> Option<RawKind> {
    let mut s = raw.trim_end();
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Some(RawKind::Blank);
    }
    if !in_code_fence && trimmed.chars().all(is_box_or_rule_char) {
        return Some(if is_horizontal_rule(trimmed) {
            RawKind::Divider
        } else {
            RawKind::Blank
        });
    }
    let has_vert_edges = trimmed.starts_with(['│', '┃', '║'])
        && trimmed.ends_with(['│', '┃', '║'])
        && trimmed.chars().count() >= 2;
    if (!in_code_fence && trimmed.starts_with(['│', '┃', '║'])) || has_vert_edges {
        let inner = trimmed.trim_start();
        let after_left = inner.get(inner.chars().next()?.len_utf8()..).unwrap_or("");
        let mut unwrapped = after_left
            .strip_prefix(' ')
            .unwrap_or(after_left)
            .trim_end();
        if let Some(last) = unwrapped.chars().next_back() {
            if matches!(last, '│' | '┃' | '║') {
                unwrapped = unwrapped[..unwrapped.len() - last.len_utf8()].trim_end();
            }
        }
        s = unwrapped;
    } else if !in_code_fence
        && ((trimmed.starts_with(['╭', '┌', '┏', '╔']) && trimmed.ends_with(['╮', '┐', '┓', '╗']))
            || (trimmed.starts_with(['╰', '└', '┗', '╚'])
                && trimmed.ends_with(['╯', '┘', '┛', '╝'])))
    {
        s = trimmed.trim_matches(is_box_or_rule_char).trim();
    }
    let rem = s.trim();
    if rem.is_empty() {
        return Some(RawKind::Blank);
    }
    if !in_code_fence && rem.chars().all(is_box_or_rule_char) {
        return is_horizontal_rule(rem).then_some(RawKind::Divider);
    }
    Some(RawKind::Text(s.to_string()))
}

fn looks_like_numbered_code_line(trimmed: &str) -> bool {
    let digits = trimmed
        .bytes()
        .take(6)
        .take_while(|b| b.is_ascii_digit())
        .count();
    digits > 0
        && digits < trimmed.len()
        && matches!(
            trimmed[digits..].chars().next(),
            Some(' ' | ':' | '│' | '|')
        )
}

fn is_collapsed_action_summary_row(trimmed: &str) -> bool {
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
    let Some(after) = (digits > 0)
        .then(|| rest[digits..].strip_prefix(" more "))
        .flatten()
    else {
        return false;
    };
    let noun = after
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .unwrap_or("");
    NOUNS.iter().any(|&n| noun.eq_ignore_ascii_case(n))
}

fn starts_with_tool_call_syntax(rest: &str) -> bool {
    let Some(paren) = rest.find('(') else {
        return false;
    };
    (2..=28).contains(&paren)
        && paren + 1 < rest.len()
        && rest[..paren].chars().enumerate().all(|(i, c)| {
            c.is_ascii_alphabetic()
                || (i > 0 && (c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
        })
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
        || ["You:", "User:"].iter().any(|lbl| {
            rest.strip_prefix(lbl)
                .is_some_and(|a| a.is_empty() || a.starts_with(char::is_whitespace))
        })
}

fn is_plain_text_process_row(line: &str, prev_is_process: bool) -> bool {
    const PROCESS_VERBS: &[&str] = &[
        "Ran ",
        "Used ",
        "Explored ",
        "Scheduled ",
        "Scheduling ",
        "Scheduling...",
        "Scheduling…",
        "Updated ",
        "Reading ",
        "Searching ",
        "Searched ",
        "Running ",
        "Editing ",
        "Edited ",
        "Writing ",
        "Wrote ",
        "Calling ",
        "Invoking ",
        "Launching ",
        "Subagent ",
    ];
    const CODEX_VERBS: &[&str] = &["Ran ", "Explored ", "Searched ", "Edited ", "Updated "];
    let leading_spaces = line.bytes().take_while(|&b| b == b' ').count();
    let trimmed = line.trim();
    let content = trimmed
        .strip_prefix(['┃', '▎', '▍', '▌', '▋', '▊', '▉', '▏'])
        .and_then(|s| s.strip_prefix(' '))
        .map_or(trimmed, str::trim_start);
    if content.is_empty() || looks_like_numbered_code_line(content) {
        return false;
    }
    if is_collapsed_action_summary_row(content)
        || content.starts_with("└ Tip:")
        || content.starts_with('⎿')
        || content.starts_with('↳')
        || (leading_spaces <= 4
            && content
                .strip_prefix("Thought for ")
                .is_some_and(|s| s.starts_with(|c: char| c.is_ascii_digit())))
    {
        return true;
    }
    if !content.contains("\"(ctrl+")
        && !content.contains("\"(esc ")
        && [
            "(ctrl+o to expand)",
            "(ctrl+r to expand)",
            "(ctrl+o to show",
            "(esc to interrupt)",
            "(esc to cancel)",
        ]
        .iter()
        .any(|h| content.contains(h))
    {
        return true;
    }
    if let Some(ch) = content.chars().next() {
        let is_spin = ('\u{2801}'..='\u{28FF}').contains(&ch)
            || ('\u{25DC}'..='\u{25E1}').contains(&ch)
            || matches!(
                ch,
                '◐' | '◓' | '◑' | '◒' | '◴' | '◷' | '◶' | '◵' | '✻' | '∴'
            );
        let after = &content[ch.len_utf8()..];
        if is_spin
            && after.starts_with(char::is_whitespace)
            && after.trim().chars().any(char::is_alphabetic)
        {
            return true;
        }
    }
    if let Some(after) = content.strip_prefix("• ") {
        if CODEX_VERBS
            .iter()
            .any(|v| after.trim_start().starts_with(v))
        {
            return true;
        }
    }
    if let Some((marker, after)) = ["○ ", "▸ ", "● ", "◈ "]
        .iter()
        .find_map(|&m| content.strip_prefix(m).map(|r| (m, r.trim_start())))
    {
        if after == "Thought"
            || after.starts_with("Thought for ")
            || after.starts_with("Thought (")
            || after.starts_with("Thinking")
            || after.starts_with("Reasoning")
            || (marker != "▸ "
                && (PROCESS_VERBS.iter().any(|v| after.starts_with(v))
                    || starts_with_tool_call_syntax(after)))
        {
            return true;
        }
    }
    prev_is_process
        && !is_agent_user_prompt(line)
        && !is_markdown_heading(line)
        && (content.starts_with("└ ") || leading_spaces >= 2)
}

fn is_markdown_heading(text: &str) -> bool {
    let indent = text.bytes().take_while(|&b| b == b' ').count();
    let rest = &text[indent..];
    let hashes = rest.bytes().take_while(|&b| b == b'#').count();
    indent <= 2 && (1..=6).contains(&hashes) && rest.as_bytes().get(hashes) == Some(&b' ')
}

fn is_trailing_chrome(line: &RawLine) -> bool {
    if line.is_code_body || line.is_fence {
        return false;
    }
    let trimmed = line.text().trim();
    if matches!(trimmed, "❯" | "›" | ">") {
        return true;
    }
    if trimmed.chars().count() > 120 {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if [
        "type a message",
        "send a message",
        "ask anything",
        "enter a message",
        "? for shortcuts",
        "shift+tab",
        "ctrl+c to exit",
        "ctrl+d to exit",
        "/help for commands",
    ]
    .iter()
    .any(|p| lower.contains(p))
        || lower.starts_with("auto ·")
        || (trimmed.contains(" · ")
            && [
                "auto",
                "esc to",
                "shortcuts",
                "context",
                "tokens",
                "shift+tab",
                "ctrl+",
            ]
            .iter()
            .any(|k| lower.contains(k)))
    {
        return true;
    }
    let is_proc_bullet = ["○ ", "▸ ", "● ", "◈ ", "• ", "⎿", "↳", "└ "]
        .iter()
        .any(|m| trimmed.starts_with(m));
    !is_proc_bullet
        && [
            "esc to cancel",
            "(esc to cancel)",
            "esc to interrupt",
            "(esc to interrupt)",
            "(ctrl+o to expand)",
            "(ctrl+r to expand)",
            "(ctrl+o to show",
        ]
        .iter()
        .any(|h| lower.contains(h))
}

fn is_cjk_join_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{1100}'..='\u{11FF}'
            | '\u{3000}'..='\u{30FF}'
            | '\u{31F0}'..='\u{31FF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{AC00}'..='\u{D7AF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FF00}'..='\u{FFEF}'
            | '\u{20000}'..='\u{2A6DF}'
    )
}

fn is_bullet_or_list(trimmed: &str) -> bool {
    if ["- ", "* ", "+ ", "• ", "◦ ", "▪ "]
        .iter()
        .any(|p| trimmed.starts_with(p))
    {
        return true;
    }
    let digits = trimmed
        .bytes()
        .take(4)
        .take_while(|b| b.is_ascii_digit())
        .count();
    digits > 0
        && matches!(trimmed.as_bytes().get(digits), Some(b'.' | b')'))
        && trimmed.as_bytes().get(digits + 1) == Some(&b' ')
}

fn is_reflowable_prose(line: &RawLine) -> bool {
    let RawKind::Text(s) = &line.kind else {
        return false;
    };
    if line.is_code_body || line.is_fence || line.is_process || line.is_turn_start {
        return false;
    }
    let trimmed = s.trim_start();
    !trimmed.is_empty()
        && !is_markdown_heading(s)
        && !is_bullet_or_list(trimmed)
        && !is_agent_user_prompt(s)
        && !(trimmed == ">" || trimmed.starts_with("> "))
        && !(trimmed.starts_with('|') && (trimmed.ends_with('|') || trimmed.contains(" | ")))
}

fn join_prose(dst: &mut String, next: &str) {
    let prev = dst.trim_end();
    let prev_len = prev.len();
    let join_tight = prev
        .chars()
        .next_back()
        .zip(next.trim_start().chars().next())
        .is_some_and(|(a, b)| is_cjk_join_char(a) && is_cjk_join_char(b));
    dst.truncate(prev_len);
    if !join_tight {
        dst.push(' ');
    }
    dst.push_str(next.trim_start());
}

fn cap_lines_preserving_fences(
    lines: Vec<ReaderLineKind>,
    max_lines: usize,
    anchors: [Option<usize>; 3],
) -> (Vec<ReaderLineKind>, [Option<usize>; 3]) {
    if lines.len() <= max_lines {
        return (lines, anchors);
    }
    let mut start_idx = lines.len() - max_lines;
    let mut inside = lines[..start_idx]
        .iter()
        .filter(|l| is_fence_line(l))
        .count()
        % 2
        == 1;
    while start_idx < lines.len() && lines.len() - start_idx + usize::from(inside) > max_lines {
        if is_fence_line(&lines[start_idx]) {
            inside = !inside;
        }
        start_idx += 1;
    }
    let mut kept = Vec::with_capacity(lines.len() - start_idx + usize::from(inside));
    if inside {
        kept.push(ReaderLineKind::Text(vec![(
            "```".to_string(),
            ReaderRunStyle::Body,
        )]));
    }
    kept.extend(lines.into_iter().skip(start_idx));
    let mut lead = 0usize;
    while kept
        .get(lead)
        .is_some_and(|l| matches!(l, ReaderLineKind::Blank | ReaderLineKind::Divider))
    {
        lead += 1;
    }
    if lead > 0 {
        kept.drain(..lead);
    }
    let shift = |opt: Option<usize>| {
        opt.filter(|&i| i >= start_idx)
            .map(|i| (i - start_idx + usize::from(inside)).saturating_sub(lead))
    };
    (
        kept,
        [shift(anchors[0]), shift(anchors[1]), shift(anchors[2])],
    )
}

fn normalize_and_style(
    lines: Vec<RawLine>,
    bold_headings: bool,
    reflow: bool,
) -> (
    Vec<ReaderLineKind>,
    Option<usize>,
    Option<usize>,
    Option<usize>,
) {
    let mut staged: Vec<RawLine> = Vec::with_capacity(lines.len() + 8);
    let (mut in_fence, mut need_blank) = (false, false);

    for item in lines {
        match item.kind {
            RawKind::Blank => {
                if !staged.is_empty()
                    && !matches!(staged.last().map(|l| &l.kind), Some(RawKind::Blank))
                {
                    staged.push(item);
                    need_blank = false;
                }
            }
            RawKind::Divider => {
                let prev_div = matches!(staged.last().map(|l| &l.kind), Some(RawKind::Divider))
                    || (staged.len() >= 2
                        && matches!(staged.last().map(|l| &l.kind), Some(RawKind::Blank))
                        && matches!(staged[staged.len() - 2].kind, RawKind::Divider));
                if prev_div {
                    need_blank = true;
                    continue;
                }
                if !staged.is_empty()
                    && !matches!(staged.last().map(|l| &l.kind), Some(RawKind::Blank))
                {
                    staged.push(RawLine::blank());
                }
                staged.push(item);
                need_blank = true;
            }
            RawKind::Text(ref s) => {
                let opening = item.is_fence && !in_fence;
                let closing = item.is_fence && in_fence;
                if !need_blank
                    && reflow
                    && !opening
                    && !closing
                    && staged.last().is_some_and(is_reflowable_prose)
                    && is_reflowable_prose(&item)
                {
                    if let Some(RawLine {
                        kind: RawKind::Text(prev_s),
                        ..
                    }) = staged.last_mut()
                    {
                        join_prose(prev_s, s);
                        continue;
                    }
                }
                if (need_blank || opening)
                    && !staged.is_empty()
                    && !matches!(staged.last().map(|l| &l.kind), Some(RawKind::Blank))
                {
                    staged.push(RawLine::blank());
                }
                need_blank = false;
                if opening {
                    in_fence = true;
                } else if closing {
                    in_fence = false;
                    need_blank = true;
                }
                staged.push(item);
            }
        }
    }

    while staged
        .first()
        .is_some_and(|l| matches!(l.kind, RawKind::Blank | RawKind::Divider))
    {
        staged.remove(0);
    }
    while staged
        .last()
        .is_some_and(|l| matches!(l.kind, RawKind::Blank | RawKind::Divider))
    {
        staged.pop();
    }

    let mut last_turn = None;
    let mut last_shell = None;
    let mut first_body_after_last_turn = None;
    let mut first_non_proc_after_last_turn = None;
    let out = staged
        .iter()
        .enumerate()
        .map(|(i, l)| match &l.kind {
            RawKind::Blank => ReaderLineKind::Blank,
            RawKind::Divider => ReaderLineKind::Divider,
            RawKind::Text(s) => {
                if l.is_turn_start {
                    last_turn = Some(i);
                    first_body_after_last_turn = None;
                    first_non_proc_after_last_turn = None;
                } else if last_turn.is_some() {
                    if first_body_after_last_turn.is_none() {
                        first_body_after_last_turn = Some(i);
                    }
                    if !l.is_process && first_non_proc_after_last_turn.is_none() {
                        first_non_proc_after_last_turn = Some(i);
                    }
                }
                if l.is_shell_prompt {
                    last_shell = Some(i);
                }
                let style = if l.is_code_body {
                    ReaderRunStyle::Code
                } else if l.is_process {
                    ReaderRunStyle::Muted
                } else if l.is_turn_start
                    || l.is_shell_prompt
                    || (bold_headings && !l.is_fence && is_markdown_heading(s))
                {
                    ReaderRunStyle::Bold
                } else {
                    ReaderRunStyle::Body
                };
                ReaderLineKind::Text(vec![(s.clone(), style)])
            }
        })
        .collect();
    (
        out,
        last_turn,
        first_non_proc_after_last_turn.or(first_body_after_last_turn),
        last_shell,
    )
}

fn is_valid_shell_prompt_prefix(prefix: &str) -> bool {
    let chars = prefix.chars().count();
    chars > 0
        && chars <= 64
        && !prefix
            .chars()
            .any(|c| matches!(c, '"' | '\'' | '{' | '}' | ';' | '\t' | '='))
        && !prefix.trim_end().is_empty()
        && !prefix.trim_end().ends_with(',')
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
            if !rest[pos + marker.len()..].trim().is_empty()
                && is_valid_shell_prompt_prefix(&rest[..pos])
            {
                return true;
            }
        }
    }
    false
}

/// Build a structured [`ReaderDoc`] from plain terminal text (from `hc terminal.read` or screen fallback).
pub fn build_reader_doc_from_text(
    text: &str,
    title: &str,
    is_agent: bool,
    clean_mode: bool,
    source_note: Option<String>,
) -> ReaderDoc {
    let mut parsed = Vec::new();
    let (mut in_code_fence, mut prev_is_process) = (false, false);

    for raw in text.lines() {
        let Some(kind) = clean_box_and_rule_line(raw, in_code_fence) else {
            continue;
        };
        let is_fence = matches!(&kind, RawKind::Text(s) if s.trim_start().starts_with("```"));
        if is_fence {
            in_code_fence = !in_code_fence;
        }
        let is_code_body = in_code_fence && !is_fence;
        let is_process = match &kind {
            RawKind::Text(s) if is_agent && !is_fence && !is_code_body => {
                is_plain_text_process_row(s, prev_is_process)
            }
            _ => false,
        };
        prev_is_process = is_process;
        let is_turn_start = is_agent
            && !is_process
            && !is_fence
            && !is_code_body
            && matches!(&kind, RawKind::Text(s) if is_agent_user_prompt(s));
        let is_shell_prompt = !is_agent
            && !is_fence
            && !is_code_body
            && matches!(&kind, RawKind::Text(s) if is_shell_prompt_line(s));
        parsed.push(RawLine {
            kind,
            is_fence,
            is_code_body,
            is_process,
            is_turn_start,
            is_shell_prompt,
        });
    }

    if !is_agent {
        let mut with_div = Vec::with_capacity(parsed.len() + 8);
        let mut has_output = false;
        for line in parsed {
            match &line.kind {
                RawKind::Blank => with_div.push(line),
                RawKind::Divider => {
                    has_output = false;
                    with_div.push(line);
                }
                RawKind::Text(_) => {
                    if line.is_shell_prompt {
                        if has_output {
                            with_div.push(RawLine::divider());
                        }
                        has_output = false;
                    } else {
                        has_output = true;
                    }
                    with_div.push(line);
                }
            }
        }
        let (lines, _, _, last_shell) = normalize_and_style(with_div, false, false);
        let (lines, [_, _, last_shell]) =
            cap_lines_preserving_fences(lines, SHELL_MAX_CLEAN_LINES, [None, None, last_shell]);
        let anchor_line = last_shell
            .map(|i| i.min(lines.len().saturating_sub(1)))
            .unwrap_or_else(|| {
                lines
                    .len()
                    .saturating_sub(24)
                    .min(lines.len().saturating_sub(1))
            });
        return ReaderDoc {
            title: title.to_string(),
            is_agent: false,
            clean_mode,
            source_note,
            lines,
            anchor_line,
            hidden_process_lines: 0,
        };
    }

    if !clean_mode {
        let (lines, last_turn, body_after, _) = normalize_and_style(parsed, true, false);
        let (lines, [last_turn, body_after, _]) =
            cap_lines_preserving_fences(lines, FULL_MAX_SCAN_ROWS, [last_turn, body_after, None]);
        let anchor_line = body_after.or(last_turn).unwrap_or_else(|| {
            lines
                .len()
                .saturating_sub(24)
                .min(lines.len().saturating_sub(1))
        });
        return ReaderDoc {
            title: title.to_string(),
            is_agent: true,
            clean_mode: false,
            source_note,
            lines,
            anchor_line,
            hidden_process_lines: 0,
        };
    }

    while parsed.last().is_some_and(|l| {
        matches!(l.kind, RawKind::Blank | RawKind::Divider) || is_trailing_chrome(l)
    }) {
        parsed.pop();
    }
    let turn_indices: Vec<usize> = parsed
        .iter()
        .enumerate()
        .filter_map(|(i, l)| l.is_turn_start.then_some(i))
        .collect();
    let trimmed = if turn_indices.len() > AGENT_MAX_TURNS {
        let start_idx = turn_indices[turn_indices.len() - AGENT_MAX_TURNS];
        let inside = parsed[..start_idx].iter().filter(|l| l.is_fence).count() % 2 == 1;
        let mut out = Vec::with_capacity(parsed.len() - start_idx + usize::from(inside));
        if inside {
            out.push(RawLine {
                kind: RawKind::Text("```".to_string()),
                is_fence: true,
                ..RawLine::meta(RawKind::Blank)
            });
        }
        out.extend(parsed.drain(start_idx..));
        out
    } else {
        parsed
    };

    let (mut collapsed, mut hidden_process_lines, mut pending_div, mut saw_text) =
        (Vec::with_capacity(trimmed.len()), 0usize, false, false);
    for line in trimmed {
        if line.is_process {
            hidden_process_lines += 1;
            pending_div = true;
            continue;
        }
        match line.kind {
            RawKind::Blank => {
                if !pending_div {
                    collapsed.push(line);
                }
            }
            RawKind::Divider => {
                pending_div = false;
                collapsed.push(line);
            }
            RawKind::Text(_) => {
                if pending_div {
                    if saw_text {
                        collapsed.push(RawLine::divider());
                    }
                    pending_div = false;
                }
                saw_text = true;
                collapsed.push(line);
            }
        }
    }

    let (lines, last_turn, body_after, _) = normalize_and_style(collapsed, true, true);
    let raw_anchor = body_after.or(last_turn).unwrap_or(0);
    let anchor_line = if lines.is_empty() {
        0
    } else {
        raw_anchor.min(lines.len() - 1)
    };
    ReaderDoc {
        title: title.to_string(),
        is_agent: true,
        clean_mode: true,
        source_note,
        lines,
        anchor_line,
        hidden_process_lines,
    }
}

struct WrapCluster {
    text: String,
    style: ReaderRunStyle,
    width: usize,
    is_ascii_ws: bool,
}

fn push_text_clusters(text: &str, style: ReaderRunStyle, atoms: &mut Vec<WrapCluster>) {
    let mut chars = text.chars().peekable();
    while let Some(first) = chars.next() {
        let mut cluster = String::from(first);
        if ('\u{1F1E6}'..='\u{1F1FF}').contains(&first)
            && chars
                .peek()
                .is_some_and(|&c| ('\u{1F1E6}'..='\u{1F1FF}').contains(&c))
        {
            cluster.push(chars.next().unwrap());
        }
        while let Some(&next) = chars.peek() {
            if next == '\u{200D}' {
                cluster.push(chars.next().unwrap());
                if chars.peek().is_some_and(|c| !c.is_control()) {
                    cluster.push(chars.next().unwrap());
                }
            } else if matches!(
                next,
                '\u{FE00}'..='\u{FE0F}'
                    | '\u{20E3}'
                    | '\u{1F3FB}'..='\u{1F3FF}'
                    | '\u{E0020}'..='\u{E007F}'
                    | '\u{E0100}'..='\u{E01EF}'
            ) || (!next.is_control()
                && next != '\u{200D}'
                && UnicodeWidthChar::width(next).unwrap_or(0) == 0)
            {
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
    let mut push = |t: &str, st: ReaderRunStyle| {
        if t.is_empty() {
            return;
        }
        if let Some((last_t, _)) = runs.last_mut().filter(|(_, s)| *s == st) {
            last_t.push_str(t);
        } else {
            runs.push((t.to_string(), st));
        }
    };
    if let Some((pfx, pfx_st)) = prefix {
        push(pfx, pfx_st);
    }
    for atom in atoms {
        push(&atom.text, atom.style);
    }
    ReaderLineKind::Text(runs)
}

/// Wrap logical lines of a [`ReaderDoc`] to `content_width` columns with atomic cluster & CJK handling.
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
            ReaderLineKind::Text(runs) => {
                let is_code = runs.iter().any(|(_, s)| *s == ReaderRunStyle::Code);
                let (prefix, avail_w) = if is_code && max_w >= 6 {
                    (Some(("▎ ", ReaderRunStyle::Muted)), max_w - 2)
                } else if is_code && max_w >= 4 {
                    (Some((" ", ReaderRunStyle::Muted)), max_w - 1)
                } else {
                    (None, max_w)
                };
                let mut atoms = Vec::new();
                for (t, s) in runs {
                    push_text_clusters(t, *s, &mut atoms);
                }
                if atoms.is_empty() {
                    rows.push(emit_cluster_slice(&[], prefix));
                    continue;
                }
                let mut pos = 0usize;
                while pos < atoms.len() {
                    let (mut i, mut w, mut last_ws) = (pos, 0usize, None);
                    while i < atoms.len() {
                        if i > pos && w + atoms[i].width > avail_w {
                            break;
                        }
                        if i > pos && atoms[i].is_ascii_ws {
                            last_ws = Some(i);
                        }
                        w += atoms[i].width;
                        i += 1;
                    }
                    if i == atoms.len() {
                        rows.push(emit_cluster_slice(&atoms[pos..], prefix));
                        break;
                    }
                    let break_at =
                        if atoms[i].is_ascii_ws && atoms[pos..i].iter().any(|c| !c.is_ascii_ws) {
                            Some(i)
                        } else {
                            last_ws.filter(|&ws| atoms[pos..ws].iter().any(|c| !c.is_ascii_ws))
                        };
                    if let Some(ws) = break_at {
                        let mut end = ws;
                        while end > pos && atoms[end - 1].is_ascii_ws {
                            end -= 1;
                        }
                        rows.push(emit_cluster_slice(&atoms[pos..end], prefix));
                        pos = ws + 1;
                        if !is_code {
                            while pos < atoms.len() && atoms[pos].is_ascii_ws {
                                pos += 1;
                            }
                        }
                    } else {
                        rows.push(emit_cluster_slice(&atoms[pos..i], prefix));
                        pos = i;
                    }
                }
            }
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

fn is_fence_line(line: &ReaderLineKind) -> bool {
    matches!(line, ReaderLineKind::Text(runs) if runs.iter().map(|(t, _)| t.as_str()).collect::<String>().trim_start().starts_with("```"))
}

fn clip_str_utf8_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let ellipsis = "…";
    let cap = max_bytes.saturating_sub(ellipsis.len());
    let mut end = cap.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    if max_bytes < ellipsis.len() {
        s[..end].to_string()
    } else {
        format!("{}{ellipsis}", &s[..end])
    }
}

fn truncate_shelldeck_runs_to_bytes(
    runs: &[(String, u8)],
    max_raw_bytes: usize,
) -> Vec<(String, u8)> {
    let total_bytes: usize = runs.iter().map(|(t, _)| t.len()).sum();
    if total_bytes <= max_raw_bytes {
        return runs.to_vec();
    }
    let ellipsis = "…";
    let mut budget = max_raw_bytes.saturating_sub(ellipsis.len());
    let mut out: Vec<(String, u8)> = Vec::new();
    for (text, style) in runs {
        if text.len() <= budget {
            out.push((text.clone(), *style));
            budget -= text.len();
        } else {
            let mut end = budget.min(text.len());
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            if end > 0 {
                out.push((format!("{}{ellipsis}", &text[..end]), *style));
            } else if max_raw_bytes >= ellipsis.len() {
                if let Some((last_txt, _)) = out.last_mut() {
                    last_txt.push_str(ellipsis);
                } else {
                    out.push((ellipsis.to_string(), *style));
                }
            }
            break;
        }
    }
    out
}

/// Serialize a [`ReaderDoc`] into ShellDeck v1 JSON strictly bounded to [`MAX_SHELLDECK_JSON_BYTES`]
/// with balanced ``` code fence parity.
pub fn format_reader_doc_json(doc: &ReaderDoc) -> String {
    let title_owned = clip_str_utf8_bytes(&doc.title, 1024);
    let title = title_owned.as_str();
    let converted: Vec<ShellDeckLine> = doc
        .lines
        .iter()
        .map(|l| match l {
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
    let build = |start_idx: usize| -> String {
        let inside = doc.lines[..start_idx]
            .iter()
            .filter(|l| is_fence_line(l))
            .count()
            % 2
            == 1;
        let mut lines_out = Vec::with_capacity(converted.len() - start_idx + usize::from(inside));
        if inside {
            lines_out.push(ShellDeckLine {
                r: vec![("```".to_string(), 0)],
            });
        }
        for l in &converted[start_idx..] {
            lines_out.push(ShellDeckLine { r: l.r.clone() });
        }
        let anchor = if lines_out.is_empty() {
            0
        } else if doc.anchor_line >= start_idx {
            ((doc.anchor_line - start_idx) + usize::from(inside)).min(lines_out.len() - 1)
        } else {
            usize::from(inside).min(lines_out.len() - 1)
        };
        serde_json::to_string(&ShellDeckPayload {
            v: 1,
            title,
            anchor,
            lines: lines_out,
        })
        .unwrap_or_else(|_| "{}".to_string())
    };
    let full = build(0);
    if full.len() <= MAX_SHELLDECK_JSON_BYTES {
        return full;
    }

    let mut best = if doc.lines.len() <= 1 {
        full
    } else {
        let (mut lo, mut hi, mut cur_best) =
            (1usize, doc.lines.len() - 1, build(doc.lines.len() - 1));
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let cand = build(mid);
            if cand.len() <= MAX_SHELLDECK_JSON_BYTES {
                cur_best = cand;
                hi = mid.saturating_sub(1);
                if mid == 0 {
                    break;
                }
            } else {
                lo = mid + 1;
            }
        }
        cur_best
    };

    if best.len() <= MAX_SHELLDECK_JSON_BYTES {
        return best;
    }

    // Even the single surviving line (plus optional synthetic opening fence) exceeded MAX_SHELLDECK_JSON_BYTES:
    // binary-search a char-safe byte prefix on the line runs while preserving fence parity.
    let last_idx = converted.len().saturating_sub(1);
    let inside = doc.lines[..last_idx]
        .iter()
        .filter(|l| is_fence_line(l))
        .count()
        % 2
        == 1;
    let last_runs = converted
        .get(last_idx)
        .map(|l| l.r.as_slice())
        .unwrap_or(&[]);
    let total_run_bytes: usize = last_runs.iter().map(|(t, _)| t.len()).sum();

    let build_truncated = |raw_cap: usize| -> String {
        let mut lines_out = Vec::with_capacity(3);
        if inside {
            lines_out.push(ShellDeckLine {
                r: vec![("```".to_string(), 0)],
            });
        }
        let trunc_runs = truncate_shelldeck_runs_to_bytes(last_runs, raw_cap);
        let trunc_is_fence = trunc_runs
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<String>()
            .trim_start()
            .starts_with("```");
        lines_out.push(ShellDeckLine { r: trunc_runs });
        if (usize::from(inside) + usize::from(trunc_is_fence)) % 2 == 1 {
            lines_out.push(ShellDeckLine {
                r: vec![("```".to_string(), 0)],
            });
        }
        let anchor = usize::from(inside).min(lines_out.len().saturating_sub(1));
        serde_json::to_string(&ShellDeckPayload {
            v: 1,
            title,
            anchor,
            lines: lines_out,
        })
        .unwrap_or_else(|_| "{}".to_string())
    };

    let mut lo = 0usize;
    let mut hi = total_run_bytes;
    best = build_truncated(0);
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let cand = build_truncated(mid);
        if cand.len() <= MAX_SHELLDECK_JSON_BYTES {
            best = cand;
            lo = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    debug_assert!(best.len() <= MAX_SHELLDECK_JSON_BYTES);
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &ReaderLineKind) -> String {
        match line {
            ReaderLineKind::Text(runs) => runs.iter().map(|(t, _)| t.as_str()).collect(),
            ReaderLineKind::Blank => String::new(),
            ReaderLineKind::Divider => "────────".to_string(),
        }
    }

    #[test]
    fn agent_clean_antigravity_sample_reflows_cjk_hides_process_and_anchors_latest() {
        let sample = concat!(
            "> Turn 1 old\n",
            "▸ Thought for 1s\n  step\n",
            "Old answer 1.\n\n",
            "> Turn 2\n",
            "● Used Read(a.rs)\n",
            "Answer 2.\n\n",
            "> Turn 3\n",
            "• Ran cargo test\n",
            "Answer 3.\n\n",
            "> Turn 4 latest\n",
            "▸ Thought for 11s\n",
            "  inspecting layout\n",
            "● Explored 4 files\n",
            "… 31 more actions (12 reads, 18 edits, 1 tool)\n",
            "└ Tip: Use /help for options\n",
            "## 结论\n",
            "这是中文段落第一行因终端宽度折\n",
            "行后的第二行内容。\n\n",
            "Soft wrapped English\n",
            "words join cleanly.\n",
            "- bullet stays separate\n",
            "```rust\n",
            "fn ok() {}\n",
            "```\n",
            "────────────────────\n",
            "> \n",
            "────────────────────\n",
            "  auto · sonnet · esc to cancel\n"
        );
        let doc = build_reader_doc_from_text(
            sample,
            "agy",
            true,
            true,
            Some("live partial (working)".to_string()),
        );
        assert_eq!(doc.source_note.as_deref(), Some("live partial (working)"));
        assert_eq!(doc.hidden_process_lines, 7);
        let texts: Vec<String> = doc.lines.iter().map(line_text).collect();

        assert!(!texts.iter().any(|t| t == "> Turn 1 old"));
        assert!(texts.iter().any(|t| t == "> Turn 2"));
        assert!(texts.iter().any(|t| t == "> Turn 4 latest"));
        assert!(!texts.iter().any(|t| t.contains("Thought for")
            || t.contains("31 more actions")
            || t.contains("Tip:")
            || t.contains("esc to cancel")));
        assert!(texts
            .iter()
            .any(|t| t == "这是中文段落第一行因终端宽度折行后的第二行内容。"));
        assert!(texts
            .iter()
            .any(|t| t == "Soft wrapped English words join cleanly."));
        assert!(texts.iter().any(|t| t == "- bullet stays separate"));
        assert!(doc.lines.contains(&ReaderLineKind::Divider));

        let heading_idx = texts.iter().position(|t| t == "## 结论").unwrap();
        assert_eq!(doc.anchor_line, heading_idx);
        assert_eq!(
            doc.lines[heading_idx],
            ReaderLineKind::Text(vec![("## 结论".to_string(), ReaderRunStyle::Bold)])
        );
    }

    #[test]
    fn agent_full_keeps_process_lines_muted_and_caps_tail() {
        let sample =
            "> Ask\n▸ Thought for 3s\n● Explored 2 files\n… 4 more steps …\nFinal answer.\n";
        let doc = build_reader_doc_from_text(sample, "full", true, false, None);
        assert_eq!(doc.hidden_process_lines, 0);
        let texts: Vec<String> = doc.lines.iter().map(line_text).collect();
        for proc_line in ["▸ Thought for 3s", "● Explored 2 files", "… 4 more steps …"] {
            let idx = texts.iter().position(|t| t == proc_line).unwrap();
            assert_eq!(
                doc.lines[idx],
                ReaderLineKind::Text(vec![(proc_line.to_string(), ReaderRunStyle::Muted)])
            );
        }
        assert_eq!(
            doc.anchor_line,
            texts.iter().position(|t| t == "Final answer.").unwrap()
        );

        // Full mode caps at FULL_MAX_SCAN_ROWS (420) while preserving code fence parity.
        let mut long_full = String::from("```txt\n");
        for i in 0..450 {
            long_full.push_str(&format!("full line {i:03}\n"));
        }
        long_full.push_str("```\n> Final ask\nFinal full body.\n");
        let capped = build_reader_doc_from_text(&long_full, "full-cap", true, false, None);
        assert!(capped.lines.len() <= FULL_MAX_SCAN_ROWS);
        assert_eq!(
            capped
                .lines
                .iter()
                .filter(|l| line_text(l).trim_start().starts_with("```"))
                .count()
                % 2,
            0
        );
    }

    #[test]
    fn shell_mode_caps_200_ignores_percentages_and_adds_dividers() {
        let mut raw = String::from("$ echo start\n```sh\n");
        for i in 0..220 {
            raw.push_str(&format!("line {i:03}\n"));
        }
        raw.push_str("```\n~/repo$ cargo test\n100% passed\nProgress: 50% complete\n● Running\n");
        let doc = build_reader_doc_from_text(&raw, "sh", false, true, None);
        assert!(doc.lines.len() <= SHELL_MAX_CLEAN_LINES);
        let texts: Vec<String> = doc.lines.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t == "● Running"));
        assert_eq!(
            texts
                .iter()
                .filter(|t| t.trim_start().starts_with("```"))
                .count()
                % 2,
            0
        );
        let prompt_idx = texts
            .iter()
            .position(|t| t == "~/repo$ cargo test")
            .unwrap();
        assert_eq!(doc.anchor_line, prompt_idx);
        assert_eq!(
            doc.lines[prompt_idx],
            ReaderLineKind::Text(vec![(
                "~/repo$ cargo test".to_string(),
                ReaderRunStyle::Bold
            )])
        );
        let pct_idx = texts.iter().position(|t| t == "100% passed").unwrap();
        assert_eq!(
            doc.lines[pct_idx],
            ReaderLineKind::Text(vec![("100% passed".to_string(), ReaderRunStyle::Body)])
        );
        assert!(
            doc.lines[prompt_idx - 1] == ReaderLineKind::Divider
                || doc.lines[prompt_idx - 2] == ReaderLineKind::Divider
        );
    }

    #[test]
    fn wrap_reader_doc_handles_zwj_cjk_code_gutter_and_long_prose_chrome_guard() {
        let long_prose = "In this detailed explanation we describe pressing shift+tab or opening ? for shortcuts without stripping real prose lines.";
        assert!(long_prose.chars().count() > 120);
        let doc = build_reader_doc_from_text(
            &format!("> Q\n{long_prose}\n? for shortcuts\n"),
            "g",
            true,
            true,
            None,
        );
        let texts: Vec<String> = doc.lines.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t == long_prose));
        assert!(!texts.iter().any(|t| t == "? for shortcuts"));

        let emoji_doc = ReaderDoc {
            title: "w".to_string(),
            is_agent: true,
            clean_mode: true,
            source_note: None,
            lines: vec![
                ReaderLineKind::Text(vec![("ab👨‍💻cd 👍🏽 🇺🇸".to_string(), ReaderRunStyle::Body)]),
                ReaderLineKind::Text(vec![(
                    "let x = compute(12345);".to_string(),
                    ReaderRunStyle::Code,
                )]),
            ],
            anchor_line: 1,
            hidden_process_lines: 0,
        };
        for width in [2u16, 3, 4] {
            let wrapped = wrap_reader_doc(&emoji_doc, width);
            let rows: Vec<String> = wrapped.rows.iter().map(line_text).collect();
            assert!(rows.iter().any(|r| r.contains("👨‍💻")));
            assert!(rows.iter().any(|r| r.contains("👍🏽")));
            assert!(rows.iter().any(|r| r.contains("🇺🇸")));
            assert!(!rows.iter().any(|r| r.ends_with('\u{200D}')));
        }
        let wide = wrap_reader_doc(&emoji_doc, 12);
        let code_rows: Vec<String> = wide.rows[wide.anchor_visual_row..]
            .iter()
            .map(line_text)
            .collect();
        assert!(code_rows.len() >= 2 && code_rows.iter().all(|r| r.starts_with("▎ ")));
    }

    #[test]
    fn shelldeck_json_v1_caps_48kb_and_screen_fallback_restores_scrollback() {
        let mut huge_lines = vec![ReaderLineKind::Text(vec![(
            "```rust".to_string(),
            ReaderRunStyle::Body,
        )])];
        for i in 0..800 {
            huge_lines.push(ReaderLineKind::Text(vec![(
                format!("// {i:04} 0123456789abcdef0123456789abcdef0123456789abcdef"),
                ReaderRunStyle::Code,
            )]));
        }
        huge_lines.push(ReaderLineKind::Text(vec![(
            "```".to_string(),
            ReaderRunStyle::Body,
        )]));
        let doc = ReaderDoc {
            title: "big".to_string(),
            is_agent: true,
            clean_mode: true,
            source_note: None,
            lines: huge_lines,
            anchor_line: 801,
            hidden_process_lines: 0,
        };
        let json = format_reader_doc_json(&doc);
        assert!(json.len() <= MAX_SHELLDECK_JSON_BYTES);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["v"], 1);
        assert_eq!(
            parsed["lines"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|l| l["r"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r[0].as_str().unwrap())
                    .collect::<String>()
                    .trim_start()
                    .starts_with("```"))
                .count()
                % 2,
            0
        );

        let schema_doc = ReaderDoc {
            title: "schema".to_string(),
            is_agent: true,
            clean_mode: true,
            source_note: None,
            lines: vec![
                ReaderLineKind::Text(vec![("# H".to_string(), ReaderRunStyle::Bold)]),
                ReaderLineKind::Blank,
                ReaderLineKind::Divider,
                ReaderLineKind::Text(vec![
                    ("b".to_string(), ReaderRunStyle::Body),
                    ("m".to_string(), ReaderRunStyle::Muted),
                ]),
                ReaderLineKind::Text(vec![("c".to_string(), ReaderRunStyle::Code)]),
            ],
            anchor_line: 3,
            hidden_process_lines: 0,
        };
        let schema: serde_json::Value =
            serde_json::from_str(&format_reader_doc_json(&schema_doc)).unwrap();
        assert_eq!(schema["lines"][0]["r"][0][1], 1);
        assert_eq!(schema["lines"][1]["r"].as_array().unwrap().len(), 0);
        assert_eq!(schema["lines"][2]["r"][0][0], "────────");
        assert_eq!(schema["lines"][2]["r"][0][1], 2);
        assert_eq!(schema["lines"][3]["r"][0][1], 0);
        assert_eq!(schema["lines"][3]["r"][1][1], 2);
        assert_eq!(schema["lines"][4]["r"][0][1], 0);

        let nz = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let mut parser = vt100::Parser::new(nz(4), nz(10), 40);
        parser.process("中文连续文本测试换行\r\nrow2\r\nrow3\r\nrow4\r\nrow5".as_bytes());
        parser.screen_mut().set_scrollback(2);
        let plain = extract_plain_text_from_screen(parser.screen_mut(), 40);
        assert_eq!(parser.screen().scrollback(), 2);
        assert!(plain.contains("中文连续文本测试换行"));
    }

    #[test]
    fn shelldeck_json_enforces_48kb_cap_on_oversized_single_line_and_huge_title() {
        let single_line = ReaderDoc {
            title: "t".repeat(100_000),
            is_agent: false,
            clean_mode: false,
            source_note: None,
            lines: vec![ReaderLineKind::Text(vec![
                ("中".repeat(40_000), ReaderRunStyle::Body),
                ("\"\\".repeat(5_000), ReaderRunStyle::Code),
            ])],
            anchor_line: 0,
            hidden_process_lines: 0,
        };
        let bounded = format_reader_doc_json(&single_line);
        assert!(
            bounded.len() <= MAX_SHELLDECK_JSON_BYTES,
            "single oversized line + title blew 48KB cap: {} bytes",
            bounded.len()
        );
        let parsed: serde_json::Value = serde_json::from_str(&bounded).unwrap();
        assert_eq!(parsed["v"], 1);
        assert!(parsed["title"].as_str().unwrap().ends_with('…'));
        let rendered_line: String = parsed["lines"][0]["r"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r[0].as_str().unwrap())
            .collect();
        assert!(rendered_line.ends_with('…'));

        // Multi-line doc where the final line alone exceeds 48KB inside an open code fence
        let mut fenced_lines = vec![ReaderLineKind::Text(vec![(
            "```rust".to_string(),
            ReaderRunStyle::Code,
        )])];
        fenced_lines.push(ReaderLineKind::Text(vec![(
            "old line".to_string(),
            ReaderRunStyle::Code,
        )]));
        fenced_lines.push(ReaderLineKind::Text(vec![(
            "x".repeat(100_000),
            ReaderRunStyle::Code,
        )]));
        let fenced_doc = ReaderDoc {
            title: "fenced".to_string(),
            is_agent: true,
            clean_mode: true,
            source_note: None,
            lines: fenced_lines,
            anchor_line: 2,
            hidden_process_lines: 0,
        };
        let bounded_fenced = format_reader_doc_json(&fenced_doc);
        assert!(bounded_fenced.len() <= MAX_SHELLDECK_JSON_BYTES);
        let parsed_fenced: serde_json::Value = serde_json::from_str(&bounded_fenced).unwrap();
        let fence_count = parsed_fenced["lines"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| {
                l["r"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r[0].as_str().unwrap())
                    .collect::<String>()
                    .trim_start()
                    .starts_with("```")
            })
            .count();
        assert_eq!(fence_count % 2, 0);
    }

    #[test]
    fn screen_fallback_unwraps_titled_and_indented_colored_box_borders_with_cjk_join() {
        let nz = |v: u16| std::num::NonZeroU16::new(v).unwrap();
        let mut parser = vt100::Parser::new(nz(4), nz(10), 40);
        parser.process("┌────────┐\r\n".as_bytes());
        parser.process("\x1b[2m思考过程\x1b[0m\r\n".as_bytes());
        parser.process("└────────┘\r\n".as_bytes());
        parser.process("中文连续文本测试换行\r\n\r\n".as_bytes());
        parser.process("正文已完成".as_bytes());
        parser.screen_mut().set_scrollback(2);

        let plain = extract_plain_text_from_screen(parser.screen_mut(), 40);
        assert_eq!(parser.screen().scrollback(), 2);
        let doc = build_reader_doc_from_text(&plain, "agy-1", true, true, None);
        let json_str = format_reader_doc_json(&doc);
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let lines: Vec<String> = parsed["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                l["r"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r[0].as_str().unwrap())
                    .collect()
            })
            .collect();
        assert!(
            lines.iter().any(|s| s == "中文连续文本测试换行"),
            "plain={plain:?}, lines={lines:?}"
        );
        assert!(lines.iter().any(|s| s == "正文已完成"));
        assert!(!lines.iter().any(|s| s.contains('┌') || s.contains('└')));

        let mut boxed = vt100::Parser::new(nz(6), nz(80), 20);
        boxed.process("╭─── Summary ───╮\r\n".as_bytes());
        boxed.process(
            "\x1b[2C\x1b[90m│\x1b[0m \x1b[1m# 结论\x1b[0m \x1b[90m│\x1b[0m\r\n".as_bytes(),
        );
        boxed.process("Body line.".as_bytes());
        let boxed_plain = extract_plain_text_from_screen(boxed.screen_mut(), 40);
        let boxed_doc = build_reader_doc_from_text(&boxed_plain, "small", true, true, None);
        let boxed_lines: Vec<String> = boxed_doc.lines.iter().map(line_text).collect();
        assert!(boxed_lines.iter().any(|s| s == "Summary"));
        assert!(boxed_lines.iter().any(|s| s == "# 结论"));
        let heading = boxed_doc
            .lines
            .iter()
            .find(|l| line_text(l) == "# 结论")
            .unwrap();
        assert!(matches!(
            heading,
            ReaderLineKind::Text(runs) if runs.iter().any(|(_, st)| *st == ReaderRunStyle::Bold)
        ));
    }
}
