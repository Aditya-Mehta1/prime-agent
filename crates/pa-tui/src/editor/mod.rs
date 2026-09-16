//! Line editor ported from `packages/tui/src/components/editor.ts`.
//!
//! Behavior parity points: multi-line state, grapheme-aware cursor movement,
//! word wrap with atomic paste/image markers, prompt history, kill ring
//! (ctrl+k / ctrl+u / ctrl+w / alt+d, yank ctrl+y / alt+y), undo with
//! fish-style coalescing, jump mode, sticky vertical column, and bracketed
//! paste with large-paste markers.
//!
//! The editor is split by concern: `wrap` (segmentation/word wrap), `kill_ring`,
//! `text_ops` (deletion/yank), `motion` (cursor movement), `input` (key
//! dispatch), `autocomplete`, and `layout` (rendering-facing layout).

use crate::keybindings::KeybindingsManager;
use crate::width::is_whitespace_char;
use std::collections::HashMap;

use text_utils::{char_at, split_at_char};
use wrap::segment_with_markers;

mod autocomplete;
mod input;
mod kill_ring;
mod layout;
mod motion;
mod text_ops;
mod text_utils;
mod wrap;

pub use kill_ring::KillRing;
pub use text_utils::normalize_text;
pub use wrap::{is_atomic_marker, word_wrap_line, LayoutLine, Segment, TextChunk, VisualLine};

pub const MAX_HISTORY: usize = 100;
/// Large paste threshold from TS: >10 lines or >1000 chars becomes a marker.
const LARGE_PASTE_LINES: usize = 10;
const LARGE_PASTE_CHARS: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone)]
struct EditorSnapshot {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
    pastes: HashMap<usize, String>,
    paste_counter: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteDisposition {
    /// Paste applied inline (content merged into the buffer).
    Inline,
    /// Large paste stored as an atomic `[paste #N ...]` marker.
    Marker { id: usize },
}

/// Outcome of a submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitOutcome {
    /// Expanded + trimmed text, as delivered to on_submit.
    pub text: String,
}

/// Events produced by the editor for the host app.
#[derive(Debug, Clone)]
pub enum EditorEvent {
    Changed(String),
    Submitted(String),
    /// Autocomplete overlay visibility changed.
    AutocompleteToggled(bool),
}

/// The multi-line editor.
pub struct Editor {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,

    pastes: HashMap<usize, String>,
    paste_counter: usize,
    history: Vec<String>,
    history_index: isize,
    kill_ring: KillRing,
    undo_stack: Vec<EditorSnapshot>,
    last_action: Option<LastAction>,
    jump_mode: Option<JumpDirection>,
    preferred_visual_col: Option<usize>,
    snapped_from_cursor_col: Option<usize>,
    last_width: usize,
    scroll_offset: usize,
    pub disable_submit: bool,
    keybindings: KeybindingsManager,
    terminal_rows: u16,

    // Autocomplete
    autocomplete_provider: Option<Box<dyn crate::autocomplete::AutocompleteProvider + Send>>,
    autocomplete: Option<crate::autocomplete::AutocompleteState>,
    events: Vec<EditorEvent>,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            pastes: HashMap::new(),
            paste_counter: 0,
            history: Vec::new(),
            history_index: -1,
            kill_ring: KillRing::default(),
            undo_stack: Vec::new(),
            last_action: None,
            jump_mode: None,
            preferred_visual_col: None,
            snapped_from_cursor_col: None,
            last_width: 80,
            scroll_offset: 0,
            disable_submit: false,
            keybindings: KeybindingsManager::new(),
            terminal_rows: 24,
            autocomplete_provider: None,
            autocomplete: None,
            events: Vec::new(),
        }
    }

    pub fn set_keybindings(&mut self, kb: KeybindingsManager) {
        self.keybindings = kb;
    }

    pub fn keybindings(&self) -> &KeybindingsManager {
        &self.keybindings
    }

    /// Terminal height, used for editor scroll window sizing.
    pub fn set_terminal_rows(&mut self, rows: u16) {
        self.terminal_rows = rows;
    }

    pub fn set_autocomplete_provider(
        &mut self,
        provider: Box<dyn crate::autocomplete::AutocompleteProvider + Send>,
    ) {
        self.cancel_autocomplete();
        self.autocomplete_provider = Some(provider);
    }

    pub fn autocomplete_state(&self) -> Option<&crate::autocomplete::AutocompleteState> {
        self.autocomplete.as_ref()
    }

    pub fn is_showing_autocomplete(&self) -> bool {
        self.autocomplete.is_some()
    }

    /// Drain pending editor events (change/submit) for the host loop.
    pub fn take_events(&mut self) -> Vec<EditorEvent> {
        std::mem::take(&mut self.events)
    }

    fn emit(&mut self, ev: EditorEvent) {
        self.events.push(ev);
    }

    // ---- state helpers -------------------------------------------------

    #[allow(dead_code)]
    fn valid_paste_id(&self, id: usize) -> bool {
        self.pastes.contains_key(&id)
    }

    fn segment(&self, text: &str) -> Vec<Segment> {
        let pastes = self.pastes.clone();
        segment_with_markers(text, &move |id| pastes.contains_key(&id))
    }

    pub fn get_text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn get_expanded_text(&self) -> String {
        self.expand_paste_markers(&self.lines.join("\n"))
    }

    fn expand_paste_markers(&self, text: &str) -> String {
        let mut result = text.to_string();
        for (id, content) in self.pastes.clone() {
            while let Some(idx) = result.find(&format!("[paste #{id}")) {
                let end = match result[idx..].find(']') {
                    Some(rel) => idx + rel + 1,
                    None => break,
                };
                let marker = result[idx..end].to_string();
                let body = marker[8..marker.len() - 1].to_string();
                let valid = body.strip_prefix(&format!("{id}")).is_some_and(|rest| {
                    rest.is_empty() || rest.starts_with(' ') || rest.starts_with('+')
                });
                if !valid {
                    break;
                }
                result.replace_range(idx..end, &content);
            }
        }
        result
    }

    pub fn get_lines(&self) -> Vec<String> {
        self.lines.clone()
    }

    pub fn get_cursor(&self) -> (usize, usize) {
        (self.cursor_line, self.cursor_col)
    }

    pub fn get_paste_snapshot(&self) -> (Vec<(usize, String)>, usize) {
        let mut pastes: Vec<(usize, String)> =
            self.pastes.iter().map(|(k, v)| (*k, v.clone())).collect();
        pastes.sort();
        (pastes, self.paste_counter)
    }

    pub fn restore_paste_snapshot(&mut self, pastes: Vec<(usize, String)>, counter: usize) {
        self.pastes = pastes.into_iter().collect();
        self.paste_counter = counter;
    }

    pub fn set_text(&mut self, text: &str) {
        self.cancel_autocomplete();
        self.last_action = None;
        self.history_index = -1;
        let normalized = normalize_text(text);
        if self.get_text() != normalized {
            self.push_undo_snapshot();
        }
        self.set_text_internal(&normalized);
    }

    fn set_text_internal(&mut self, text: &str) {
        let lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
        self.lines = if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        };
        self.cursor_line = self.lines.len() - 1;
        let col = self.lines[self.cursor_line].chars().count();
        self.set_cursor_col(col);
        self.scroll_offset = 0;
        self.emit(EditorEvent::Changed(self.get_text()));
    }

    pub fn insert_text_at_cursor(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.cancel_autocomplete();
        self.push_undo_snapshot();
        self.last_action = None;
        self.history_index = -1;
        self.insert_text_at_cursor_internal(text);
    }

    pub fn add_to_history(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.history.first() == Some(&trimmed.to_string()) {
            return;
        }
        self.history.insert(0, trimmed.to_string());
        if self.history.len() > MAX_HISTORY {
            self.history.pop();
        }
    }

    pub fn get_history(&self) -> &[String] {
        &self.history
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
        self.history_index = -1;
    }

    fn is_editor_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    fn navigate_history(&mut self, direction: isize) {
        self.last_action = None;
        if self.history.is_empty() {
            return;
        }
        let new_index = self.history_index - direction;
        if new_index < -1 || new_index >= self.history.len() as isize {
            return;
        }
        if self.history_index == -1 && new_index >= 0 {
            self.push_undo_snapshot();
        }
        self.history_index = new_index;
        let idx = self.history_index;
        if idx == -1 {
            self.set_text_internal("");
        } else {
            let text = self.history[idx as usize].clone();
            self.set_text_internal(&text);
        }
    }

    fn is_history_navigation_active(&self) -> bool {
        self.history_index > -1
    }

    // ---- undo / kill ring -----------------------------------------------

    fn push_undo_snapshot(&mut self) {
        self.undo_stack.push(EditorSnapshot {
            lines: self.lines.clone(),
            cursor_line: self.cursor_line,
            cursor_col: self.cursor_col,
            pastes: self.pastes.clone(),
            paste_counter: self.paste_counter,
        });
    }

    fn undo(&mut self) {
        self.history_index = -1;
        let Some(snapshot) = self.undo_stack.pop() else {
            return;
        };
        self.lines = snapshot.lines;
        self.cursor_line = snapshot.cursor_line;
        self.cursor_col = snapshot.cursor_col;
        self.pastes = snapshot.pastes;
        self.paste_counter = snapshot.paste_counter;
        self.last_action = None;
        self.preferred_visual_col = None;
        self.emit(EditorEvent::Changed(self.get_text()));
        self.refresh_autocomplete_after_edit(true);
    }

    // ---- text mutation ---------------------------------------------------

    fn insert_character(&mut self, ch: &str) {
        self.insert_character_opts(ch, false);
    }

    fn insert_character_opts(&mut self, ch: &str, skip_undo_coalescing: bool) {
        self.history_index = -1;
        if !skip_undo_coalescing {
            let is_ws = ch.chars().any(is_whitespace_char);
            if is_ws || self.last_action.as_ref() != Some(&LastAction::TypeWord) {
                self.push_undo_snapshot();
            }
            self.last_action = Some(LastAction::TypeWord);
        }
        let line = self.lines[self.cursor_line].clone();
        let (before, after) = split_at_char(&line, self.cursor_col);
        self.lines[self.cursor_line] = format!("{}{}{}", before, ch, after);
        self.set_cursor_col(self.cursor_col + ch.chars().count());
        self.emit(EditorEvent::Changed(self.get_text()));
        self.maybe_autocomplete_after_insert(ch);
    }

    fn insert_text_at_cursor_internal(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let normalized = normalize_text(text);
        let inserted: Vec<String> = normalized.split('\n').map(|s| s.to_string()).collect();
        let current_line = self.lines[self.cursor_line].clone();
        let (before, after) = split_at_char(&current_line, self.cursor_col);
        if inserted.len() == 1 {
            self.lines[self.cursor_line] = format!("{}{}{}", before, normalized, after);
            self.set_cursor_col(self.cursor_col + normalized.chars().count());
        } else {
            let mut new_lines: Vec<String> = Vec::with_capacity(self.lines.len() + inserted.len());
            new_lines.extend_from_slice(&self.lines[..self.cursor_line]);
            new_lines.push(format!("{}{}", before, inserted[0]));
            for mid in &inserted[1..inserted.len() - 1] {
                new_lines.push(mid.clone());
            }
            new_lines.push(format!("{}{}", inserted[inserted.len() - 1], after));
            new_lines.extend_from_slice(&self.lines[self.cursor_line + 1..]);
            self.lines = new_lines;
            self.cursor_line += inserted.len() - 1;
            let last_len = inserted[inserted.len() - 1].chars().count();
            self.set_cursor_col(last_len);
        }
        self.emit(EditorEvent::Changed(self.get_text()));
    }

    fn add_new_line(&mut self) {
        self.cancel_autocomplete();
        self.history_index = -1;
        self.last_action = None;
        self.push_undo_snapshot();
        let current_line = self.lines[self.cursor_line].clone();
        let (before, after) = split_at_char(&current_line, self.cursor_col);
        self.lines[self.cursor_line] = before;
        self.lines.insert(self.cursor_line + 1, after);
        self.cursor_line += 1;
        self.set_cursor_col(0);
        self.emit(EditorEvent::Changed(self.get_text()));
    }

    fn submit_value(&mut self) {
        self.cancel_autocomplete();
        let result = self
            .expand_paste_markers(&self.lines.join("\n"))
            .trim()
            .to_string();
        self.lines = vec![String::new()];
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.pastes.clear();
        self.paste_counter = 0;
        self.history_index = -1;
        self.scroll_offset = 0;
        self.undo_stack.clear();
        self.last_action = None;
        self.emit(EditorEvent::Changed(String::new()));
        self.emit(EditorEvent::Submitted(result));
    }

    // ---- paste -----------------------------------------------------------

    /// Handle a bracketed-paste payload (port of handlePaste, including the
    /// large-paste marker logic).
    pub fn handle_paste(&mut self, pasted_text: &str) -> PasteDisposition {
        self.cancel_autocomplete();
        self.history_index = -1;
        self.last_action = None;
        self.push_undo_snapshot();

        let clean = normalize_text(pasted_text);
        let filtered_raw: String = clean
            .chars()
            .filter(|&c| c == '\n' || (c as u32) >= 32)
            .collect();
        let mut filtered = filtered_raw;
        // File paths get a leading space when following a word char.
        if filtered.starts_with(['/', '~', '.']) {
            let line = &self.lines[self.cursor_line];
            let char_before = char_at(line, self.cursor_col.saturating_sub(1));
            if let Some(c) = char_before {
                if c.is_alphanumeric() || c == '_' {
                    filtered = format!(" {}", filtered);
                }
            }
        }
        let line_count = filtered.split('\n').count();
        if line_count > LARGE_PASTE_LINES || filtered.chars().count() > LARGE_PASTE_CHARS {
            self.paste_counter += 1;
            let id = self.paste_counter;
            self.pastes.insert(id, filtered.clone());
            let marker = if line_count > LARGE_PASTE_LINES {
                format!("[paste #{} +{} lines]", id, line_count)
            } else {
                format!("[paste #{} {} chars]", id, filtered.chars().count())
            };
            self.insert_text_at_cursor_internal(&marker);
            return PasteDisposition::Marker { id };
        }
        self.insert_text_at_cursor_internal(&filtered);
        PasteDisposition::Inline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ed() -> Editor {
        Editor::new()
    }

    #[test]
    fn typing_and_backspace() {
        let mut e = ed();
        e.handle_input("h");
        e.handle_input("i");
        assert_eq!(e.get_text(), "hi");
        e.handle_input("backspace");
        assert_eq!(e.get_text(), "h");
        e.handle_input("left");
        e.handle_input("backspace");
        // At column 0 of the first line backspace is a no-op (TS parity).
        assert_eq!(e.get_text(), "h");
    }

    #[test]
    fn newline_and_submit() {
        let mut e = ed();
        e.handle_input("a");
        e.handle_input("shift+enter");
        assert_eq!(e.get_lines(), vec!["a", ""]);
        e.handle_input("b");
        e.handle_input("enter");
        let events = e.take_events();
        let submitted = events
            .iter()
            .find_map(|ev| match ev {
                EditorEvent::Submitted(t) => Some(t.clone()),
                _ => None,
            })
            .expect("submit event");
        assert_eq!(submitted, "a\nb");
        assert_eq!(e.get_text(), "");
    }

    #[test]
    fn undo_coalescing() {
        let mut e = ed();
        for c in ["h", "e", "l", "l", "o"] {
            e.handle_input(c);
        }
        e.handle_input("ctrl+-");
        assert_eq!(e.get_text(), "");
    }

    #[test]
    fn history_navigation() {
        let mut e = ed();
        e.add_to_history("first prompt");
        e.add_to_history("second prompt");
        e.handle_input("up");
        assert_eq!(e.get_text(), "second prompt");
        e.handle_input("up");
        assert_eq!(e.get_text(), "first prompt");
        e.handle_input("down");
        assert_eq!(e.get_text(), "second prompt");
        e.handle_input("down");
        assert_eq!(e.get_text(), "");
    }

    #[test]
    fn large_paste_marker() {
        let mut e = ed();
        let big = (0..15)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = e.handle_paste(&big);
        assert!(matches!(d, PasteDisposition::Marker { id: 1 }));
        assert_eq!(e.get_text(), "[paste #1 +15 lines]");
        assert_eq!(e.get_expanded_text(), big);
    }

    #[test]
    fn small_paste_inline() {
        let mut e = ed();
        e.handle_paste("one\ntwo");
        assert_eq!(e.get_text(), "one\ntwo");
    }

    #[test]
    fn backslash_enter_newline() {
        let mut e = ed();
        e.handle_input("a");
        e.handle_input("\\");
        e.handle_input("enter");
        assert_eq!(e.get_lines(), vec!["a", ""]);
    }
}
