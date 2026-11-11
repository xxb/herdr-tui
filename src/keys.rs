//! Keybinding configuration (compatible with classic `herdr`) and VT key encoder.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    NextTab,
    PrevTab,
    SelectTab(usize),
    NewTab,
    CloseTab,
    RenameTab,
    NextSpace,
    PrevSpace,
    NewSpace,
    RenameSpace,
    ToggleSidebar,
    FuzzyFinder,
    JumpAlertAgent,
    CyclePaneInTab,
    EnterScrollback,
    ToggleHelp,
    Detach,
    SendPrefix,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct FileConfig {
    #[serde(default)]
    pub ui: UiFileConfig,
    #[serde(default)]
    pub keys: KeysFileConfig,
    #[serde(default)]
    pub quota: QuotaFileConfig,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct QuotaFileConfig {
    pub claude_5h_tokens: Option<u64>,
    pub claude_5h_messages: Option<u64>,
    pub claude_7d_tokens: Option<u64>,
    pub claude_7d_messages: Option<u64>,
    pub opencode_daily_requests: Option<u64>,
    pub opencode_5h_requests: Option<u64>,
    pub agy_5h_steps: Option<u64>,
    pub agy_daily_steps: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct UiFileConfig {
    pub sidebar_width: Option<u16>,
    pub sidebar_start_collapsed: Option<bool>,
    pub mobile_width_threshold: Option<u16>,
    pub touch_mode: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct KeysFileConfig {
    pub prefix: Option<String>,
}

impl FileConfig {
    pub fn load() -> Self {
        let mut paths = Vec::new();
        if let Some(cfg_dir) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
        }) {
            paths.push(cfg_dir.join("herdr-tui/config.toml"));
            paths.push(cfg_dir.join("herdr/config.toml"));
        }
        for p in paths {
            if let Ok(text) = std::fs::read_to_string(&p) {
                if let Ok(cfg) = toml::from_str::<FileConfig>(&text) {
                    return cfg;
                }
            }
        }
        Self::default()
    }
}

fn ctrl_char_byte(c: char) -> Option<u8> {
    match c.to_ascii_lowercase() {
        lower @ 'a'..='z' => Some((lower as u8) - b'a' + 1),
        '@' | ' ' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' | '/' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

fn parse_prefix_ctrl_char(prefix_spec: &str) -> char {
    let spec = prefix_spec.trim().to_ascii_lowercase();
    if let Some(ch_str) = spec.strip_prefix("ctrl+") {
        let mut chars = ch_str.chars();
        if let (Some(ch), None) = (chars.next(), chars.next()) {
            if ctrl_char_byte(ch).is_some() {
                return ch;
            }
        }
    }
    'b'
}

pub fn is_prefix_key(ev: &KeyEvent, prefix_spec: &str) -> bool {
    let target = parse_prefix_ctrl_char(prefix_spec);
    ev.modifiers.contains(KeyModifiers::CONTROL)
        && !ev.modifiers.contains(KeyModifiers::ALT)
        && matches!(ev.code, KeyCode::Char(c) if c.to_ascii_lowercase() == target)
}

pub fn prefix_bytes(prefix_spec: &str) -> Vec<u8> {
    let target = parse_prefix_ctrl_char(prefix_spec);
    vec![ctrl_char_byte(target).unwrap_or(0x02)]
}

/// Direct (non-prefix) global shortcuts (compatible with classic `herdr` & `~/.config/herdr/config.toml`).
pub fn match_direct_action(ev: &KeyEvent) -> Option<Action> {
    if ev.modifiers == KeyModifiers::ALT {
        match ev.code {
            KeyCode::Left => return Some(Action::PrevTab),
            KeyCode::Right => return Some(Action::NextTab),
            KeyCode::Up => return Some(Action::PrevSpace),
            KeyCode::Down => return Some(Action::NextSpace),
            KeyCode::Char(c @ '1'..='9') => {
                return Some(Action::SelectTab((c as usize) - ('1' as usize)));
            }
            _ => {}
        }
    }
    // Also support Ctrl+\ for quick detach (matching `hc attach` habit)
    if ev.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(ev.code, KeyCode::Char('\\') | KeyCode::Char('\x1c'))
    {
        return Some(Action::Detach);
    }
    None
}

/// Match key pressed right after the prefix key (`Ctrl+b`).
pub fn match_prefix_action(ev: &KeyEvent, prefix_spec: &str) -> Option<Action> {
    if is_prefix_key(ev, prefix_spec) {
        return Some(Action::SendPrefix);
    }
    match ev.code {
        KeyCode::Char('n') | KeyCode::Right => Some(Action::NextTab),
        KeyCode::Char('p') | KeyCode::Left => Some(Action::PrevTab),
        KeyCode::Char(c @ '1'..='9') => Some(Action::SelectTab((c as usize) - ('1' as usize))),
        KeyCode::Char('c') => Some(Action::NewTab),
        KeyCode::Char('x' | 'X') => Some(Action::CloseTab),
        KeyCode::Char(',') => Some(Action::RenameTab),
        KeyCode::Up => Some(Action::PrevSpace),
        KeyCode::Down => Some(Action::NextSpace),
        KeyCode::Char('C' | 's') => Some(Action::NewSpace),
        KeyCode::Char('$' | 'R') => Some(Action::RenameSpace),
        KeyCode::Char('b') => Some(Action::ToggleSidebar),
        KeyCode::Char('w' | 'f') => Some(Action::FuzzyFinder),
        KeyCode::Char('a') => Some(Action::JumpAlertAgent),
        KeyCode::Char('o') => Some(Action::CyclePaneInTab),
        KeyCode::Char('[') => Some(Action::EnterScrollback),
        KeyCode::Char('?') => Some(Action::ToggleHelp),
        KeyCode::Char('q' | 'd') => Some(Action::Detach),
        _ => None,
    }
}

/// Encode a `crossterm` `KeyEvent` into standard xterm/VT bytes for the active PTY.
pub fn encode_key_event(ev: &KeyEvent, app_cursor: bool) -> Vec<u8> {
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    let alt = ev.modifiers.contains(KeyModifiers::ALT);
    let shift = ev.modifiers.contains(KeyModifiers::SHIFT);
    let xterm_mod = 1 + u8::from(shift) + (u8::from(alt) * 2) + (u8::from(ctrl) * 4);

    let encode_cursor_key = |final_char: char| -> Vec<u8> {
        if xterm_mod > 1 {
            format!("\x1b[1;{xterm_mod}{final_char}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{final_char}").into_bytes()
        } else {
            format!("\x1b[{final_char}").into_bytes()
        }
    };

    let encode_tilde_key = |code: u8| -> Vec<u8> {
        if xterm_mod > 1 {
            format!("\x1b[{code};{xterm_mod}~").into_bytes()
        } else {
            format!("\x1b[{code}~").into_bytes()
        }
    };

    match ev.code {
        KeyCode::Up => return encode_cursor_key('A'),
        KeyCode::Down => return encode_cursor_key('B'),
        KeyCode::Right => return encode_cursor_key('C'),
        KeyCode::Left => return encode_cursor_key('D'),
        KeyCode::Home => return encode_cursor_key('H'),
        KeyCode::End => return encode_cursor_key('F'),
        KeyCode::Insert => return encode_tilde_key(2),
        KeyCode::Delete => return encode_tilde_key(3),
        KeyCode::PageUp => return encode_tilde_key(5),
        KeyCode::PageDown => return encode_tilde_key(6),
        KeyCode::F(n) => {
            let ss3_char = match n {
                1 => Some('P'),
                2 => Some('Q'),
                3 => Some('R'),
                4 => Some('S'),
                _ => None,
            };
            if let Some(ch) = ss3_char {
                return if xterm_mod > 1 {
                    format!("\x1b[1;{xterm_mod}{ch}").into_bytes()
                } else {
                    format!("\x1bO{ch}").into_bytes()
                };
            }
            let tilde_code = match n {
                5 => Some(15),
                6 => Some(17),
                7 => Some(18),
                8 => Some(19),
                9 => Some(20),
                10 => Some(21),
                11 => Some(23),
                12 => Some(24),
                _ => None,
            };
            return tilde_code.map_or_else(Vec::new, encode_tilde_key);
        }
        _ => {}
    }

    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }

    match ev.code {
        KeyCode::Char(c) => {
            if ctrl {
                if let Some(b) = ctrl_char_byte(c) {
                    out.push(b);
                    return out;
                }
            }
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Backspace => {
            if ctrl {
                out.push(0x17); // Ctrl+W
            } else {
                out.push(0x7f);
            }
        }
        KeyCode::Tab => {
            if shift {
                out.extend_from_slice(b"\x1b[Z");
            } else {
                out.push(b'\t');
            }
        }
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Esc => out.push(0x1b),
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_and_direct_shortcuts_match_classic_herdr() {
        let ctrl_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert!(is_prefix_key(&ctrl_b, "ctrl+b"));

        let alt_right = KeyEvent::new(KeyCode::Right, KeyModifiers::ALT);
        assert_eq!(match_direct_action(&alt_right), Some(Action::NextTab));

        let alt_up = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(match_direct_action(&alt_up), Some(Action::PrevSpace));

        let n_key = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(match_prefix_action(&n_key, "ctrl+b"), Some(Action::NextTab));

        let b_key = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE);
        assert_eq!(
            match_prefix_action(&b_key, "ctrl+b"),
            Some(Action::ToggleSidebar)
        );
    }

    #[test]
    fn encode_key_event_preserves_combined_modifiers() {
        let ctrl_shift_left = KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_eq!(encode_key_event(&ctrl_shift_left, false), b"\x1b[1;6D");
        assert_eq!(encode_key_event(&ctrl_shift_left, true), b"\x1b[1;6D");

        let alt_shift_right = KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        );
        assert_eq!(encode_key_event(&alt_shift_right, false), b"\x1b[1;4C");

        let plain_up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(encode_key_event(&plain_up, false), b"\x1b[A");
        assert_eq!(encode_key_event(&plain_up, true), b"\x1bOA");

        let ctrl_delete = KeyEvent::new(KeyCode::Delete, KeyModifiers::CONTROL);
        assert_eq!(encode_key_event(&ctrl_delete, false), b"\x1b[3;5~");
    }
}
