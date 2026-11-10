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
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct UiFileConfig {
    pub sidebar_width: Option<u16>,
    pub sidebar_start_collapsed: Option<bool>,
    pub mobile_width_threshold: Option<u16>,
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

pub fn is_prefix_key(ev: &KeyEvent, prefix_spec: &str) -> bool {
    let spec = prefix_spec.trim().to_ascii_lowercase();
    if let Some(ch_str) = spec.strip_prefix("ctrl+") {
        if let Some(ch) = ch_str.chars().next() {
            return ev.modifiers.contains(KeyModifiers::CONTROL)
                && !ev.modifiers.contains(KeyModifiers::ALT)
                && matches!(ev.code, KeyCode::Char(c) if c.to_ascii_lowercase() == ch);
        }
    }
    // Default Ctrl+b
    ev.modifiers == KeyModifiers::CONTROL && matches!(ev.code, KeyCode::Char('b' | 'B'))
}

pub fn prefix_bytes(prefix_spec: &str) -> Vec<u8> {
    let spec = prefix_spec.trim().to_ascii_lowercase();
    if let Some(ch_str) = spec.strip_prefix("ctrl+") {
        if let Some(ch) = ch_str.chars().next() {
            if ch.is_ascii_alphabetic() {
                return vec![(ch as u8) - b'a' + 1];
            }
        }
    }
    vec![0x02] // Ctrl+B
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

    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }

    match ev.code {
        KeyCode::Char(c) => {
            if ctrl {
                let lower = c.to_ascii_lowercase();
                let code = match lower {
                    'a'..='z' => Some((lower as u8) - b'a' + 1),
                    '@' | ' ' => Some(0x00),
                    '[' => Some(0x1b),
                    '\\' => Some(0x1c),
                    ']' => Some(0x1d),
                    '^' => Some(0x1e),
                    '_' | '/' => Some(0x1f),
                    '?' => Some(0x7f),
                    _ => None,
                };
                if let Some(b) = code {
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
        KeyCode::Up => {
            if ctrl {
                out.extend_from_slice(b"\x1b[1;5A");
            } else if shift {
                out.extend_from_slice(b"\x1b[1;2A");
            } else if app_cursor {
                out.extend_from_slice(b"\x1bOA");
            } else {
                out.extend_from_slice(b"\x1b[A");
            }
        }
        KeyCode::Down => {
            if ctrl {
                out.extend_from_slice(b"\x1b[1;5B");
            } else if shift {
                out.extend_from_slice(b"\x1b[1;2B");
            } else if app_cursor {
                out.extend_from_slice(b"\x1bOB");
            } else {
                out.extend_from_slice(b"\x1b[B");
            }
        }
        KeyCode::Right => {
            if ctrl {
                out.extend_from_slice(b"\x1b[1;5C");
            } else if shift {
                out.extend_from_slice(b"\x1b[1;2C");
            } else if app_cursor {
                out.extend_from_slice(b"\x1bOC");
            } else {
                out.extend_from_slice(b"\x1b[C");
            }
        }
        KeyCode::Left => {
            if ctrl {
                out.extend_from_slice(b"\x1b[1;5D");
            } else if shift {
                out.extend_from_slice(b"\x1b[1;2D");
            } else if app_cursor {
                out.extend_from_slice(b"\x1bOD");
            } else {
                out.extend_from_slice(b"\x1b[D");
            }
        }
        KeyCode::Home => {
            if app_cursor {
                out.extend_from_slice(b"\x1bOH");
            } else {
                out.extend_from_slice(b"\x1b[H");
            }
        }
        KeyCode::End => {
            if app_cursor {
                out.extend_from_slice(b"\x1bOF");
            } else {
                out.extend_from_slice(b"\x1b[F");
            }
        }
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::F(n) => {
            let seq: &[u8] = match n {
                1 => b"\x1bOP",
                2 => b"\x1bOQ",
                3 => b"\x1bOR",
                4 => b"\x1bOS",
                5 => b"\x1b[15~",
                6 => b"\x1b[17~",
                7 => b"\x1b[18~",
                8 => b"\x1b[19~",
                9 => b"\x1b[20~",
                10 => b"\x1b[21~",
                11 => b"\x1b[23~",
                12 => b"\x1b[24~",
                _ => b"",
            };
            out.extend_from_slice(seq);
        }
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
}
