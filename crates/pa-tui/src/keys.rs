//! Key identifiers and matching, ported from `packages/tui/src/keys.ts`.
//!
//! Input arrives as crossterm events; we map them to the same string key ids
//! the TS product uses ("ctrl+c", "shift+enter", "alt+left", ...) so
//! `KeybindingsManager` matching behaves identically.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::crossterm::event as ct;

pub type KeyId = String;

pub fn key_event_to_id(key: &KeyEvent) -> Option<KeyId> {
    if key.kind == ct::KeyEventKind::Release || key.kind == ct::KeyEventKind::Repeat {
        // Release events are filtered (TS wantsKeyRelease opt-in); repeats behave as presses.
        if key.kind == ct::KeyEventKind::Release {
            return None;
        }
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let base = match key.code {
        KeyCode::Char(c) => {
            let lower = c.to_ascii_lowercase();
            if ctrl {
                let name = if alt {
                    format!("ctrl+alt+{lower}")
                } else if shift && c.is_ascii_uppercase() {
                    // ctrl+shift+letter: TS kitty path reports shifted identity; legacy
                    // terminals cannot send it, so treat as ctrl+letter.
                    format!("ctrl+{lower}")
                } else {
                    format!("ctrl+{lower}")
                };
                return Some(name);
            }
            if alt {
                if c == '\r' || c == '\n' {
                    return Some("alt+enter".into());
                }
                if c == ' ' {
                    return Some("alt+space".into());
                }
                return Some(format!("alt+{}", c.to_ascii_lowercase()));
            }
            if c == '\r' || c == '\n' {
                if shift {
                    return Some("shift+enter".into());
                }
                return Some("enter".into());
            }
            if c == '\t' {
                return Some(if shift {
                    "shift+tab".into()
                } else {
                    "tab".into()
                });
            }
            if c == ' ' && shift {
                return Some("shift+space".into());
            }
            return Some(c.to_string());
        }
        KeyCode::Enter => {
            if alt {
                "alt+enter"
            } else if shift {
                "shift+enter"
            } else {
                "enter"
            }
        }
        KeyCode::Tab => {
            if shift {
                "shift+tab"
            } else {
                "tab"
            }
        }
        KeyCode::Backspace => {
            if alt {
                "alt+backspace"
            } else if ctrl {
                // ctrl+backspace: TS maps raw 0x08 to backspace except Windows Terminal.
                return Some("ctrl+backspace".into());
            } else {
                "backspace"
            }
        }
        KeyCode::Esc => "escape",
        KeyCode::Left => return Some(modified_name("left", ctrl, alt, shift)),
        KeyCode::Right => return Some(modified_name("right", ctrl, alt, shift)),
        KeyCode::Up => return Some(modified_name("up", ctrl, alt, shift)),
        KeyCode::Down => return Some(modified_name("down", ctrl, alt, shift)),
        KeyCode::Home => return Some(modified_name("home", ctrl, alt, shift)),
        KeyCode::End => return Some(modified_name("end", ctrl, alt, shift)),
        KeyCode::PageUp => return Some(modified_name("pageUp", ctrl, alt, shift)),
        KeyCode::PageDown => return Some(modified_name("pageDown", ctrl, alt, shift)),
        KeyCode::Delete => return Some(modified_name("delete", ctrl, alt, shift)),
        KeyCode::Insert => return Some(modified_name("insert", ctrl, alt, shift)),
        KeyCode::F(n) => return Some(modified_name(&format!("f{n}"), ctrl, alt, shift)),
        KeyCode::Null
        | KeyCode::BackTab
        | KeyCode::CapsLock
        | KeyCode::ScrollLock
        | KeyCode::NumLock
        | KeyCode::PrintScreen
        | KeyCode::Pause
        | KeyCode::Menu
        | KeyCode::KeypadBegin
        | KeyCode::Modifier(_)
        | KeyCode::Media(_) => {
            return None;
        }
    };
    Some(base.to_string())
}

fn modified_name(name: &str, ctrl: bool, alt: bool, shift: bool) -> String {
    let mut s = String::new();
    if shift {
        s.push_str("shift+");
    }
    if ctrl {
        s.push_str("ctrl+");
    }
    if alt {
        s.push_str("alt+");
    }
    s.push_str(name);
    s
}

/// Repeated escape presses arrive as separate events; TS splits combined data.
/// Kept for API parity with CustomEditor.splitRepeatedKeybinding.
pub fn split_repeated(data: &[KeyId], keybinding_id: &str) -> Option<Vec<KeyId>> {
    let hits: Vec<KeyId> = data
        .iter()
        .filter(|k| k.as_str() == keybinding_id)
        .cloned()
        .collect();
    if hits.len() > 1 {
        Some(hits)
    } else {
        None
    }
}
