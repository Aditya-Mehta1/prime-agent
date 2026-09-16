//! Line editor ported from `packages/tui/src/components/editor.ts`.
//!
//! Behavior parity points: multi-line state, grapheme-aware cursor movement,
//! word wrap with atomic paste/image markers, prompt history, kill ring
//! (ctrl+k / ctrl+u / ctrl+w / alt+d, yank ctrl+y / alt+y), undo with
//! fish-style coalescing, jump mode, sticky vertical column, and bracketed
//! paste with large-paste markers.

use crate::keybindings::{KeybindingsManager, TUI_KEYBINDINGS};
use crate::width::{is_punctuation_char, is_whitespace_char, str_width};
use std::collections::HashMap;

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

/// One layout line produced for rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutLine {
    pub text: String,
    pub has_cursor: bool,
    pub cursor_pos: usize,
    pub source_line: usize,
    pub source_start: usize,
}

/// Visual line mapping entry (logical line + segment bounds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualLine {
    pub logical_line: usize,
    pub start_col: usize,
    pub length: usize,
}

/// A word-wrapping chunk with logical bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    pub text: String,
    pub start_index: usize,
    pub end_index: usize,
}

/// Grapheme segment with byte offset, mirroring Intl.Segmenter data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub segment: String,
    pub index: usize,
}

fn graphemes(text: &str) -> Vec<Segment> {
    use unicode_segmentation::UnicodeSegmentation;
    text.graphemes(true)
        .enumerate()
        .map(|(i, g)| Segment {
            segment: g.to_string(),
            index: i,
        })
        .collect()
}

fn is_paste_marker(seg: &str) -> bool {
    // [paste #N (+L lines | C chars)]
    if !seg.starts_with("[paste #") || !seg.ends_with(']') {
        return false;
    }
    let body = &seg[8..seg.len() - 1];
    let Some((num, rest)) = body.split_once(' ') else {
        return rest_is_empty(body);
    };
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let rest = rest.strip_prefix('+').unwrap_or(rest);
    match rest.split_once(' ') {
        Some((n, "lines")) => !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()),
        Some((n, "chars")) => !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()),
        _ => false,
    }
}

fn rest_is_empty(body: &str) -> bool {
    !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit())
}

fn is_image_marker(seg: &str) -> bool {
    // [image #N]
    if !seg.starts_with("[image #") || !seg.ends_with(']') {
        return false;
    }
    let body = &seg[8..seg.len() - 1];
    !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit())
}

pub fn is_atomic_marker(seg: &str) -> bool {
    seg.len() >= 10 && (is_paste_marker(seg) || is_image_marker(seg))
}

/// Segment text merging valid paste markers and image markers into atomic units.
fn segment_with_markers(text: &str, valid_paste_ids: &dyn Fn(usize) -> bool) -> Vec<Segment> {
    let has_paste = text.contains("[paste #");
    let has_image = text.contains("[image #");
    let base = graphemes(text);
    if !has_paste && !has_image {
        return base;
    }

    let mut markers: Vec<(usize, usize)> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < text.len() {
        if bytes[i] == b'[' {
            let rest = &text[i..];
            let close = rest.find(']').map(|p| i + p + 1);
            if let Some(end) = close {
                let cand = &text[i..end];
                let id = cand
                    .strip_prefix("[paste #")
                    .and_then(|b| b.split([']', ' ']).next().map(|s| s.to_string()))
                    .and_then(|s| s.parse::<usize>().ok());
                let keep = match id {
                    Some(id) => has_paste && valid_paste_ids(id),
                    None => has_image && is_image_marker(cand),
                };
                if keep {
                    markers.push((i, end));
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    if markers.is_empty() {
        return base;
    }

    let mut result: Vec<Segment> = Vec::new();
    let mut marker_idx = 0usize;
    for seg in base {
        while marker_idx < markers.len() && markers[marker_idx].1 <= seg.index {
            marker_idx += 1;
        }
        let in_marker = marker_idx < markers.len()
            && seg.index >= markers[marker_idx].0
            && seg.index < markers[marker_idx].1;
        if in_marker {
            if seg.index == markers[marker_idx].0 {
                let (start, end) = markers[marker_idx];
                result.push(Segment {
                    segment: text[start..end].to_string(),
                    index: start,
                });
            }
        } else {
            result.push(seg);
        }
    }
    result
}

/// Split a line into word-wrapped chunks (port of wordWrapLine).
pub fn word_wrap_line(
    line: &str,
    max_width: usize,
    segments: Option<Vec<Segment>>,
) -> Vec<TextChunk> {
    if line.is_empty() || max_width == 0 {
        return vec![TextChunk {
            text: String::new(),
            start_index: 0,
            end_index: 0,
        }];
    }
    if str_width(line) <= max_width {
        return vec![TextChunk {
            text: line.to_string(),
            start_index: 0,
            end_index: line.len(),
        }];
    }
    let segments = segments.unwrap_or_else(|| graphemes(line));
    let mut chunks: Vec<TextChunk> = Vec::new();
    let mut current_width = 0usize;
    let mut chunk_start = 0usize;
    let mut wrap_opp_index: isize = -1;
    let mut wrap_opp_width = 0usize;

    for i in 0..segments.len() {
        let seg = &segments[i];
        let grapheme = &seg.segment;
        let g_width = str_width(grapheme);
        let char_index = seg.index;
        let is_ws = !is_atomic_marker(grapheme)
            && grapheme.chars().all(is_whitespace_char)
            && !grapheme.is_empty();

        if current_width + g_width > max_width {
            if wrap_opp_index >= 0 && current_width - wrap_opp_width + g_width <= max_width {
                let opp = wrap_opp_index as usize;
                chunks.push(TextChunk {
                    text: line[chunk_start..opp].to_string(),
                    start_index: chunk_start,
                    end_index: opp,
                });
                chunk_start = opp;
                current_width -= wrap_opp_width;
            } else if chunk_start < char_index {
                chunks.push(TextChunk {
                    text: line[chunk_start..char_index].to_string(),
                    start_index: chunk_start,
                    end_index: char_index,
                });
                chunk_start = char_index;
                current_width = 0;
            }
            wrap_opp_index = -1;
        }

        if g_width > max_width {
            // Atomic segment wider than the viewport: visual-only re-wrap.
            let sub_chunks = word_wrap_line(grapheme, max_width, None);
            for sc in &sub_chunks[..sub_chunks.len() - 1] {
                chunks.push(TextChunk {
                    text: sc.text.clone(),
                    start_index: char_index + sc.start_index,
                    end_index: char_index + sc.end_index,
                });
            }
            let last = &sub_chunks[sub_chunks.len() - 1];
            chunk_start = char_index + last.start_index;
            current_width = str_width(&last.text);
            wrap_opp_index = -1;
            continue;
        }

        current_width += g_width;

        let next = segments.get(i + 1);
        let next_starts_word = next.is_some_and(|n| {
            is_atomic_marker(&n.segment) || n.segment.chars().any(|c| !is_whitespace_char(c))
        });
        if is_ws && next_starts_word {
            if let Some(next) = next {
                wrap_opp_index = next.index as isize;
                wrap_opp_width = current_width;
            }
        }
    }

    chunks.push(TextChunk {
        text: line[chunk_start..].to_string(),
        start_index: chunk_start,
        end_index: line.len(),
    });
    chunks
}

/// Ring buffer for Emacs-style kill/yank (port of kill-ring.ts).
#[derive(Debug, Clone, Default)]
pub struct KillRing {
    ring: Vec<String>,
}

impl KillRing {
    pub fn push(&mut self, text: &str, prepend: bool, accumulate: bool) {
        if text.is_empty() {
            return;
        }
        if accumulate && !self.ring.is_empty() {
            let last = self.ring.pop().unwrap_or_default();
            let merged = if prepend {
                format!("{}{}", text, last)
            } else {
                format!("{}{}", last, text)
            };
            self.ring.push(merged);
        } else {
            self.ring.push(text.to_string());
        }
    }
    pub fn peek(&self) -> Option<&str> {
        self.ring.last().map(|s| s.as_str())
    }
    pub fn rotate(&mut self) {
        if self.ring.len() > 1 {
            let last = self.ring.pop().unwrap_or_default();
            self.ring.insert(0, last);
        }
    }
    pub fn len(&self) -> usize {
        self.ring.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
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

    fn handle_backspace(&mut self) {
        self.history_index = -1;
        self.last_action = None;
        let line = self.lines[self.cursor_line].clone();
        if self.cursor_col > 0 {
            self.push_undo_snapshot();
            let before_cursor = char_prefix(&line, self.cursor_col);
            let graphemes = self.segment(&before_cursor);
            let last_len = graphemes
                .last()
                .map(|g| g.segment.chars().count())
                .unwrap_or(1);
            let (before, after) = split_at_char(&line, self.cursor_col);
            let before = char_prefix(&before, before.chars().count() - last_len);
            self.lines[self.cursor_line] = format!("{}{}", before, after);
            self.set_cursor_col(self.cursor_col.saturating_sub(last_len));
        } else if self.cursor_line > 0 {
            self.push_undo_snapshot();
            let current_line = self.lines[self.cursor_line].clone();
            let previous_line = self.lines[self.cursor_line - 1].clone();
            self.lines[self.cursor_line - 1] = format!("{}{}", previous_line, current_line);
            self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.set_cursor_col(previous_line.chars().count());
        }
        self.emit(EditorEvent::Changed(self.get_text()));
        self.refresh_autocomplete_after_edit(true);
    }

    fn handle_forward_delete(&mut self) {
        self.history_index = -1;
        self.last_action = None;
        let current_line = self.lines[self.cursor_line].clone();
        if self.cursor_col < current_line.chars().count() {
            self.push_undo_snapshot();
            let after_cursor = char_suffix(&current_line, self.cursor_col);
            let first_len = self
                .segment(&after_cursor)
                .first()
                .map(|g| g.segment.chars().count())
                .unwrap_or(1);
            let (before, after) = split_at_char(&current_line, self.cursor_col + first_len);
            self.lines[self.cursor_line] = format!("{}{}", before, after);
        } else if self.cursor_line < self.lines.len() - 1 {
            self.push_undo_snapshot();
            let next_line = self.lines[self.cursor_line + 1].clone();
            self.lines[self.cursor_line] = format!("{}{}", current_line, next_line);
            self.lines.remove(self.cursor_line + 1);
        }
        self.emit(EditorEvent::Changed(self.get_text()));
        self.refresh_autocomplete_after_edit(true);
    }

    fn delete_to_start_of_line(&mut self) {
        self.history_index = -1;
        let current_line = self.lines[self.cursor_line].clone();
        if self.cursor_col > 0 {
            self.push_undo_snapshot();
            let (deleted, rest) = split_at_char(&current_line, self.cursor_col);
            self.kill_ring.push(
                &deleted,
                true,
                self.last_action.as_ref() == Some(&LastAction::Kill),
            );
            self.last_action = Some(LastAction::Kill);
            self.lines[self.cursor_line] = rest;
            self.set_cursor_col(0);
        } else if self.cursor_line > 0 {
            self.push_undo_snapshot();
            self.kill_ring.push(
                "\n",
                true,
                self.last_action.as_ref() == Some(&LastAction::Kill),
            );
            self.last_action = Some(LastAction::Kill);
            let previous_line = self.lines[self.cursor_line - 1].clone();
            self.lines[self.cursor_line - 1] = format!("{}{}", previous_line, current_line);
            self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.set_cursor_col(previous_line.chars().count());
        }
        self.emit(EditorEvent::Changed(self.get_text()));
        self.refresh_autocomplete_after_edit(false);
    }

    fn delete_to_end_of_line(&mut self) {
        self.history_index = -1;
        let current_line = self.lines[self.cursor_line].clone();
        let line_len = current_line.chars().count();
        if self.cursor_col < line_len {
            self.push_undo_snapshot();
            let (before, deleted) = split_at_char(&current_line, self.cursor_col);
            self.kill_ring.push(
                &deleted,
                false,
                self.last_action.as_ref() == Some(&LastAction::Kill),
            );
            self.last_action = Some(LastAction::Kill);
            self.lines[self.cursor_line] = before;
        } else if self.cursor_line < self.lines.len() - 1 {
            self.push_undo_snapshot();
            self.kill_ring.push(
                "\n",
                false,
                self.last_action.as_ref() == Some(&LastAction::Kill),
            );
            self.last_action = Some(LastAction::Kill);
            let next_line = self.lines[self.cursor_line + 1].clone();
            self.lines[self.cursor_line] = format!("{}{}", current_line, next_line);
            self.lines.remove(self.cursor_line + 1);
        }
        self.emit(EditorEvent::Changed(self.get_text()));
        self.refresh_autocomplete_after_edit(false);
    }

    fn delete_word_backwards(&mut self) {
        self.history_index = -1;
        let current_line = self.lines[self.cursor_line].clone();
        if self.cursor_col == 0 {
            if self.cursor_line > 0 {
                self.push_undo_snapshot();
                self.kill_ring.push(
                    "\n",
                    true,
                    self.last_action.as_ref() == Some(&LastAction::Kill),
                );
                self.last_action = Some(LastAction::Kill);
                let previous_line = self.lines[self.cursor_line - 1].clone();
                self.lines[self.cursor_line - 1] = format!("{}{}", previous_line, current_line);
                self.lines.remove(self.cursor_line);
                self.cursor_line -= 1;
                self.set_cursor_col(previous_line.chars().count());
            }
        } else {
            self.push_undo_snapshot();
            let was_kill = self.last_action.as_ref() == Some(&LastAction::Kill);
            let old_cursor_col = self.cursor_col;
            self.move_word_backwards();
            let delete_from = self.cursor_col;
            self.set_cursor_col(old_cursor_col);
            let head = char_prefix(&current_line, old_cursor_col);
            let deleted = char_suffix(&head, delete_from);
            self.kill_ring.push(&deleted, true, was_kill);
            self.last_action = Some(LastAction::Kill);
            let before = char_prefix(&head, delete_from);
            let rest = char_suffix(&current_line, old_cursor_col);
            self.lines[self.cursor_line] = format!("{}{}", before, rest);
            self.set_cursor_col(delete_from);
        }
        self.emit(EditorEvent::Changed(self.get_text()));
        self.refresh_autocomplete_after_edit(false);
    }

    fn delete_word_forward(&mut self) {
        self.history_index = -1;
        let current_line = self.lines[self.cursor_line].clone();
        let line_len = current_line.chars().count();
        if self.cursor_col >= line_len {
            if self.cursor_line < self.lines.len() - 1 {
                self.push_undo_snapshot();
                self.kill_ring.push(
                    "\n",
                    false,
                    self.last_action.as_ref() == Some(&LastAction::Kill),
                );
                self.last_action = Some(LastAction::Kill);
                let next_line = self.lines[self.cursor_line + 1].clone();
                self.lines[self.cursor_line] = format!("{}{}", current_line, next_line);
                self.lines.remove(self.cursor_line + 1);
            }
        } else {
            self.push_undo_snapshot();
            let was_kill = self.last_action.as_ref() == Some(&LastAction::Kill);
            let old_cursor_col = self.cursor_col;
            self.move_word_forwards();
            let delete_to = self.cursor_col;
            self.set_cursor_col(old_cursor_col);
            let (before, deleted) = split_at_char(&current_line, self.cursor_col);
            let deleted = char_suffix(&deleted, delete_to - self.cursor_col);
            self.kill_ring.push(&deleted, false, was_kill);
            self.last_action = Some(LastAction::Kill);
            let (_, after) = split_at_char(&current_line, delete_to);
            self.lines[self.cursor_line] = format!("{}{}", before, after);
        }
        self.emit(EditorEvent::Changed(self.get_text()));
        self.refresh_autocomplete_after_edit(false);
    }

    fn yank(&mut self) {
        if self.kill_ring.is_empty() {
            return;
        }
        self.push_undo_snapshot();
        let text = self.kill_ring.peek().unwrap_or_default().to_string();
        self.insert_yanked_text(&text);
        self.last_action = Some(LastAction::Yank);
        self.refresh_autocomplete_after_edit(false);
    }

    fn yank_pop(&mut self) {
        if self.last_action.as_ref() != Some(&LastAction::Yank) || self.kill_ring.len() <= 1 {
            return;
        }
        self.push_undo_snapshot();
        self.delete_yanked_text();
        self.kill_ring.rotate();
        let text = self.kill_ring.peek().unwrap_or_default().to_string();
        self.insert_yanked_text(&text);
        self.last_action = Some(LastAction::Yank);
        self.refresh_autocomplete_after_edit(false);
    }

    fn insert_yanked_text(&mut self, text: &str) {
        self.history_index = -1;
        let parts: Vec<&str> = text.split('\n').collect();
        if parts.len() == 1 {
            let current_line = self.lines[self.cursor_line].clone();
            let (before, after) = split_at_char(&current_line, self.cursor_col);
            self.lines[self.cursor_line] = format!("{}{}{}", before, text, after);
            self.set_cursor_col(self.cursor_col + text.chars().count());
        } else {
            let current_line = self.lines[self.cursor_line].clone();
            let (before, after) = split_at_char(&current_line, self.cursor_col);
            self.lines[self.cursor_line] = format!("{}{}", before, parts[0]);
            for (i, mid) in parts[1..parts.len() - 1].iter().enumerate() {
                self.lines.insert(self.cursor_line + 1 + i, mid.to_string());
            }
            let last_index = self.cursor_line + parts.len() - 1;
            self.lines
                .insert(last_index, format!("{}{}", parts[parts.len() - 1], after));
            self.cursor_line = last_index;
            let last_len = parts[parts.len() - 1].chars().count();
            self.set_cursor_col(last_len);
        }
        self.emit(EditorEvent::Changed(self.get_text()));
    }

    fn delete_yanked_text(&mut self) {
        let Some(yanked) = self.kill_ring.peek().map(|s| s.to_string()) else {
            return;
        };
        let parts: Vec<&str> = yanked.split('\n').collect();
        if parts.len() == 1 {
            let current_line = self.lines[self.cursor_line].clone();
            let delete_len = yanked.chars().count();
            let (before, after) = split_at_char(&current_line, self.cursor_col);
            let before = char_prefix(&before, before.chars().count() - delete_len);
            self.lines[self.cursor_line] = format!("{}{}", before, after);
            self.set_cursor_col(self.cursor_col.saturating_sub(delete_len));
        } else {
            let start_line = self.cursor_line - (parts.len() - 1);
            let start_col = self.lines[start_line].chars().count() - parts[0].chars().count();
            let after_cursor = char_suffix(&self.lines[self.cursor_line], self.cursor_col);
            let before_yank = char_prefix(&self.lines[start_line], start_col);
            let replacement = format!("{}{}", before_yank, after_cursor);
            self.lines.drain(start_line..=self.cursor_line);
            self.lines.insert(start_line, replacement);
            self.cursor_line = start_line;
            self.set_cursor_col(start_col);
        }
        self.emit(EditorEvent::Changed(self.get_text()));
    }

    // ---- cursor movement --------------------------------------------------

    fn set_cursor_col(&mut self, col: usize) {
        self.cursor_col = col;
        self.preferred_visual_col = None;
        self.snapped_from_cursor_col = None;
    }

    fn move_to_line_start(&mut self) {
        self.last_action = None;
        self.set_cursor_col(0);
    }

    fn move_to_line_end(&mut self) {
        self.last_action = None;
        let len = self.lines[self.cursor_line].chars().count();
        self.set_cursor_col(len);
    }

    fn move_word_backwards(&mut self) {
        self.last_action = None;
        let current_line = self.lines[self.cursor_line].clone();
        if self.cursor_col == 0 {
            if self.cursor_line > 0 {
                self.cursor_line -= 1;
                let prev_len = self.lines[self.cursor_line].chars().count();
                self.set_cursor_col(prev_len);
            }
            return;
        }
        let before_cursor = char_prefix(&current_line, self.cursor_col);
        let mut graphemes = self.segment(&before_cursor);
        let mut new_col = self.cursor_col;
        while let Some(last) = graphemes.last() {
            if is_atomic_marker(&last.segment) || !last.segment.chars().any(is_whitespace_char) {
                break;
            }
            new_col -= last.segment.chars().count();
            graphemes.pop();
        }
        if let Some(last) = graphemes.last() {
            let seg = &last.segment;
            if is_atomic_marker(seg) {
                new_col -= seg.chars().count();
                graphemes.pop();
            } else if seg.chars().any(is_punctuation_char) {
                while let Some(last) = graphemes.last() {
                    if !last.segment.chars().any(is_punctuation_char)
                        || is_atomic_marker(&last.segment)
                    {
                        break;
                    }
                    new_col -= last.segment.chars().count();
                    graphemes.pop();
                }
            } else {
                while let Some(last) = graphemes.last() {
                    let g = &last.segment;
                    if g.chars().any(is_whitespace_char)
                        || g.chars().any(is_punctuation_char)
                        || is_atomic_marker(g)
                    {
                        break;
                    }
                    new_col -= g.chars().count();
                    graphemes.pop();
                }
            }
        }
        self.set_cursor_col(new_col);
    }

    fn move_word_forwards(&mut self) {
        self.last_action = None;
        let current_line = self.lines[self.cursor_line].clone();
        let line_len = current_line.chars().count();
        if self.cursor_col >= line_len {
            if self.cursor_line < self.lines.len() - 1 {
                self.cursor_line += 1;
                self.set_cursor_col(0);
            }
            return;
        }
        let after_cursor = char_suffix(&current_line, self.cursor_col);
        let mut iter = self.segment(&after_cursor).into_iter().peekable();
        let mut new_col = self.cursor_col;
        while let Some(seg) = iter.peek() {
            if is_atomic_marker(&seg.segment) || !seg.segment.chars().any(is_whitespace_char) {
                break;
            }
            new_col += seg.segment.chars().count();
            iter.next();
        }
        if let Some(first) = iter.peek() {
            let g = &first.segment;
            if is_atomic_marker(g) {
                new_col += g.chars().count();
            } else if g.chars().any(is_punctuation_char) {
                while let Some(seg) = iter.peek() {
                    if !seg.segment.chars().any(is_punctuation_char)
                        || is_atomic_marker(&seg.segment)
                    {
                        break;
                    }
                    new_col += seg.segment.chars().count();
                    iter.next();
                }
            } else {
                while let Some(seg) = iter.peek() {
                    let g = &seg.segment;
                    if g.chars().any(is_whitespace_char)
                        || g.chars().any(is_punctuation_char)
                        || is_atomic_marker(g)
                    {
                        break;
                    }
                    new_col += g.chars().count();
                    iter.next();
                }
            }
        }
        self.set_cursor_col(new_col);
    }

    fn jump_to_char(&mut self, ch: &str, forward: bool) {
        self.last_action = None;
        let (end, step) = if forward {
            (self.lines.len() as isize, 1isize)
        } else {
            (-1isize, -1isize)
        };
        let mut line_idx = self.cursor_line as isize;
        while line_idx != end {
            let idx = line_idx as usize;
            let line = &self.lines[idx];
            let found = if idx == self.cursor_line {
                if forward {
                    char_find_after(line, self.cursor_col, ch)
                } else {
                    char_find_before(line, self.cursor_col, ch)
                }
            } else if forward {
                char_find_after(line, usize::MAX, ch)
            } else {
                char_find_before(line, usize::MAX, ch)
            };
            if let Some(pos) = found {
                self.cursor_line = idx;
                self.set_cursor_col(pos);
                return;
            }
            line_idx += step;
        }
    }

    pub fn build_visual_line_map(&self, width: usize) -> Vec<VisualLine> {
        let mut visual_lines = Vec::new();
        for (i, line) in self.lines.iter().enumerate() {
            let line_vis_width = str_width(line);
            if line.is_empty() {
                visual_lines.push(VisualLine {
                    logical_line: i,
                    start_col: 0,
                    length: 0,
                });
            } else if line_vis_width <= width {
                visual_lines.push(VisualLine {
                    logical_line: i,
                    start_col: 0,
                    length: line.chars().count(),
                });
            } else {
                for chunk in word_wrap_line(line, width, Some(self.segment(line))) {
                    visual_lines.push(VisualLine {
                        logical_line: i,
                        start_col: chunk.start_index,
                        length: chunk.end_index - chunk.start_index,
                    });
                }
            }
        }
        visual_lines
    }

    fn find_visual_line_at(&self, visual_lines: &[VisualLine], line: usize, col: usize) -> usize {
        for (i, vl) in visual_lines.iter().enumerate() {
            if vl.logical_line != line {
                continue;
            }
            let offset = col as isize - vl.start_col as isize;
            let is_last_segment =
                i == visual_lines.len() - 1 || visual_lines[i + 1].logical_line != vl.logical_line;
            if offset >= 0
                && (offset < vl.length as isize
                    || (is_last_segment && offset == vl.length as isize))
            {
                return i;
            }
        }
        visual_lines.len().saturating_sub(1)
    }

    fn find_current_visual_line(&self, visual_lines: &[VisualLine]) -> usize {
        self.find_visual_line_at(visual_lines, self.cursor_line, self.cursor_col)
    }

    fn compute_vertical_move_column(
        &mut self,
        current_visual_col: usize,
        source_max: usize,
        target_max: usize,
    ) -> usize {
        let has_preferred = self.preferred_visual_col.is_some();
        let cursor_in_middle = current_visual_col < source_max;
        let target_too_short = target_max < current_visual_col;
        if !has_preferred || cursor_in_middle {
            if target_too_short {
                self.preferred_visual_col = Some(current_visual_col);
                return target_max;
            }
            self.preferred_visual_col = None;
            return current_visual_col;
        }
        let preferred = self.preferred_visual_col.unwrap_or(0);
        if target_too_short || target_max < preferred {
            return target_max;
        }
        self.preferred_visual_col = None;
        preferred
    }

    fn move_to_visual_line(
        &mut self,
        visual_lines: &[VisualLine],
        current_visual_line: usize,
        target_visual_line: usize,
    ) {
        let Some(current_vl) = visual_lines.get(current_visual_line) else {
            return;
        };
        let Some(target_vl) = visual_lines.get(target_visual_line) else {
            return;
        };
        let current_visual_col = if let Some(snapped) = self.snapped_from_cursor_col {
            let vl_idx = self.find_visual_line_at(visual_lines, current_vl.logical_line, snapped);
            snapped.saturating_sub(visual_lines[vl_idx].start_col)
        } else {
            self.cursor_col.saturating_sub(current_vl.start_col)
        };
        let is_last_source = current_visual_line == visual_lines.len() - 1
            || visual_lines[current_visual_line + 1].logical_line != current_vl.logical_line;
        let source_max = if is_last_source {
            current_vl.length
        } else {
            current_vl.length.saturating_sub(1)
        };
        let is_last_target = target_visual_line == visual_lines.len() - 1
            || visual_lines[target_visual_line + 1].logical_line != target_vl.logical_line;
        let target_max = if is_last_target {
            target_vl.length
        } else {
            target_vl.length.saturating_sub(1)
        };
        let move_to_visual_col =
            self.compute_vertical_move_column(current_visual_col, source_max, target_max);

        self.cursor_line = target_vl.logical_line;
        let target_col = target_vl.start_col + move_to_visual_col;
        let logical_len = self.lines[target_vl.logical_line].chars().count();
        self.cursor_col = target_col.min(logical_len);

        // Snap to atomic segment boundaries so the cursor never lands mid-marker.
        let logical_line = self.lines[self.cursor_line].clone();
        for seg in self.segment(&logical_line) {
            if seg.index > self.cursor_col {
                break;
            }
            if seg.segment.chars().count() <= 1 {
                continue;
            }
            let seg_len = seg.segment.chars().count();
            if self.cursor_col < seg.index + seg_len {
                let is_continuation = seg.index < target_vl.start_col;
                let is_moving_down = target_visual_line > current_visual_line;
                if is_continuation && is_moving_down {
                    let seg_end = seg.index + seg_len;
                    let mut next = target_visual_line + 1;
                    while next < visual_lines.len()
                        && visual_lines[next].logical_line == target_vl.logical_line
                        && visual_lines[next].start_col < seg_end
                    {
                        next += 1;
                    }
                    if next < visual_lines.len() {
                        self.move_to_visual_line(visual_lines, current_visual_line, next);
                        return;
                    }
                }
                self.snapped_from_cursor_col = Some(self.cursor_col);
                self.cursor_col = seg.index;
                return;
            }
        }
        self.snapped_from_cursor_col = None;
    }

    fn move_cursor(&mut self, delta_line: isize, delta_col: isize) {
        self.last_action = None;
        let visual_lines = self.build_visual_line_map(self.last_width);
        let current_visual_line = self.find_current_visual_line(&visual_lines);
        if delta_line != 0 {
            let target = current_visual_line as isize + delta_line;
            if target >= 0 && (target as usize) < visual_lines.len() {
                self.move_to_visual_line(&visual_lines, current_visual_line, target as usize);
            }
        }
        if delta_col != 0 {
            let current_line = self.lines[self.cursor_line].clone();
            if delta_col > 0 {
                if self.cursor_col < current_line.chars().count() {
                    let after = char_suffix(&current_line, self.cursor_col);
                    let advance = self
                        .segment(&after)
                        .first()
                        .map(|g| g.segment.chars().count())
                        .unwrap_or(1);
                    self.set_cursor_col(self.cursor_col + advance);
                } else if self.cursor_line < self.lines.len() - 1 {
                    self.cursor_line += 1;
                    self.set_cursor_col(0);
                } else if let Some(current_vl) = visual_lines.get(current_visual_line) {
                    self.preferred_visual_col =
                        Some(self.cursor_col.saturating_sub(current_vl.start_col));
                }
            } else if self.cursor_col > 0 {
                let before = char_prefix(&current_line, self.cursor_col);
                let back = self
                    .segment(&before)
                    .last()
                    .map(|g| g.segment.chars().count())
                    .unwrap_or(1);
                self.set_cursor_col(self.cursor_col.saturating_sub(back));
            } else if self.cursor_line > 0 {
                self.cursor_line -= 1;
                let prev_len = self.lines[self.cursor_line].chars().count();
                self.set_cursor_col(prev_len);
            }
        }
    }

    fn page_scroll(&mut self, direction: isize) {
        self.last_action = None;
        let page_size = (self.terminal_rows as f32 * 0.3).floor().max(5.0) as usize;
        let visual_lines = self.build_visual_line_map(self.last_width);
        if visual_lines.is_empty() {
            return;
        }
        let current = self.find_current_visual_line(&visual_lines);
        let target = (current as isize + direction * page_size as isize)
            .clamp(0, visual_lines.len() as isize - 1) as usize;
        self.move_to_visual_line(&visual_lines, current, target);
    }

    fn is_on_first_visual_line(&self) -> bool {
        let vl = self.build_visual_line_map(self.last_width);
        self.find_current_visual_line(&vl) == 0
    }

    fn is_on_last_visual_line(&self) -> bool {
        let vl = self.build_visual_line_map(self.last_width);
        !vl.is_empty() && self.find_current_visual_line(&vl) == vl.len() - 1
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

    // ---- input dispatch ----------------------------------------------------

    /// Handle one key event (already decoded to a TS-style key id, e.g.
    /// "ctrl+k", or a literal character for printable input).
    pub fn handle_input(&mut self, input: &str) {
        let kb_ids: Vec<String> = TUI_KEYBINDINGS
            .iter()
            .map(|(id, _)| id.to_string())
            .collect();
        let _ = kb_ids;
        self.handle_input_inner(input);
    }

    fn kb_matches(&self, input: &str, binding: &str) -> bool {
        self.keybindings.matches(input, binding)
    }

    fn handle_input_inner(&mut self, input: &str) {
        if let Some(direction) = self.jump_mode {
            if self.kb_matches(input, "tui.editor.jumpForward")
                || self.kb_matches(input, "tui.editor.jumpBackward")
            {
                self.jump_mode = None;
                return;
            }
            if let Some(printable) = decode_printable(input) {
                self.jump_mode = None;
                self.jump_to_char(&printable, direction == JumpDirection::Forward);
                return;
            }
            self.jump_mode = None;
        }

        if self.kb_matches(input, "tui.input.copy") {
            return;
        }
        if self.kb_matches(input, "tui.editor.undo") {
            self.undo();
            return;
        }

        if self.autocomplete.is_some() {
            if self.kb_matches(input, "tui.select.cancel") {
                self.cancel_autocomplete();
                return;
            }
            if self.kb_matches(input, "tui.select.up") || self.kb_matches(input, "tui.select.down")
            {
                let up = self.kb_matches(input, "tui.select.up");
                if let Some(state) = self.autocomplete.as_mut() {
                    if up {
                        state.move_up();
                    } else {
                        state.move_down();
                    }
                }
                return;
            }
            if self.kb_matches(input, "tui.input.tab")
                || self.kb_matches(input, "tui.select.confirm")
            {
                let selected = self.autocomplete.as_ref().and_then(|s| s.selected_item());
                if let Some(item) = selected {
                    let is_typed_exact = self.is_slash_name_completion_at_prompt_start();
                    self.push_undo_snapshot();
                    self.last_action = None;
                    let (cl, cc) = (self.cursor_line, self.cursor_col);
                    let prefix = self
                        .autocomplete
                        .as_ref()
                        .map(|s| s.prefix.clone())
                        .unwrap_or_default();
                    let result = self.apply_completion(&item, &prefix);
                    let completed_noop = result.lines == self.lines
                        && result.cursor_line == cl
                        && result.cursor_col == cc;
                    self.lines = result.lines;
                    self.cursor_line = result.cursor_line;
                    self.set_cursor_col(result.cursor_col);
                    self.cancel_autocomplete();
                    if !is_typed_exact || !completed_noop {
                        self.emit(EditorEvent::Changed(self.get_text()));
                        return;
                    }
                    // Exact slash command typed: fall through so Enter submits.
                }
            }
        }

        if self.kb_matches(input, "tui.input.tab") && self.autocomplete.is_none() {
            self.handle_tab_completion();
            return;
        }

        if self.kb_matches(input, "tui.editor.deleteToLineEnd") {
            self.delete_to_end_of_line();
            return;
        }
        if self.kb_matches(input, "tui.editor.deleteToLineStart") {
            self.delete_to_start_of_line();
            return;
        }
        if self.kb_matches(input, "tui.editor.deleteWordBackward") {
            self.delete_word_backwards();
            return;
        }
        if self.kb_matches(input, "tui.editor.deleteWordForward") {
            self.delete_word_forward();
            return;
        }
        if self.kb_matches(input, "tui.editor.deleteCharBackward") || input == "shift+backspace" {
            self.handle_backspace();
            return;
        }
        if self.kb_matches(input, "tui.editor.deleteCharForward") || input == "shift+delete" {
            self.handle_forward_delete();
            return;
        }
        if self.kb_matches(input, "tui.editor.yank") {
            self.yank();
            return;
        }
        if self.kb_matches(input, "tui.editor.yankPop") {
            self.yank_pop();
            return;
        }
        if self.kb_matches(input, "tui.editor.cursorLineStart") {
            self.move_to_line_start();
            return;
        }
        if self.kb_matches(input, "tui.editor.cursorLineEnd") {
            self.move_to_line_end();
            return;
        }
        if self.kb_matches(input, "tui.editor.cursorWordLeft") {
            self.move_word_backwards();
            return;
        }
        if self.kb_matches(input, "tui.editor.cursorWordRight") {
            self.move_word_forwards();
            return;
        }
        if self.kb_matches(input, "tui.input.newLine") {
            self.add_new_line();
            return;
        }
        if self.kb_matches(input, "tui.input.submit") {
            if self.disable_submit {
                return;
            }
            let current_line = self.lines[self.cursor_line].clone();
            if self.cursor_col > 0 && char_at(&current_line, self.cursor_col - 1) == Some('\\') {
                self.handle_backspace();
                self.add_new_line();
                return;
            }
            self.submit_value();
            return;
        }
        if self.kb_matches(input, "tui.editor.cursorUp") {
            if self.is_editor_empty()
                || (self.is_history_navigation_active() && self.is_on_first_visual_line())
            {
                self.navigate_history(-1);
            } else if self.is_on_first_visual_line() {
                self.move_to_line_start();
            } else {
                self.move_cursor(-1, 0);
            }
            return;
        }
        if self.kb_matches(input, "tui.editor.cursorDown") {
            if self.is_history_navigation_active() && self.is_on_last_visual_line() {
                self.navigate_history(1);
            } else if self.is_on_last_visual_line() {
                self.move_to_line_end();
            } else {
                self.move_cursor(1, 0);
            }
            return;
        }
        if self.kb_matches(input, "tui.editor.cursorRight") {
            self.move_cursor(0, 1);
            return;
        }
        if self.kb_matches(input, "tui.editor.cursorLeft") {
            self.move_cursor(0, -1);
            return;
        }
        if self.kb_matches(input, "tui.editor.pageUp") {
            self.page_scroll(-1);
            return;
        }
        if self.kb_matches(input, "tui.editor.pageDown") {
            self.page_scroll(1);
            return;
        }
        if self.kb_matches(input, "tui.editor.jumpForward") {
            self.jump_mode = Some(JumpDirection::Forward);
            return;
        }
        if self.kb_matches(input, "tui.editor.jumpBackward") {
            self.jump_mode = Some(JumpDirection::Backward);
            return;
        }
        if input == "shift+space" {
            self.insert_character(" ");
            return;
        }
        if let Some(printable) = decode_printable(input) {
            self.insert_character(&printable);
        }
    }

    // ---- autocomplete ------------------------------------------------------

    fn current_slash_command_context(&self) -> Option<crate::autocomplete::SlashContext> {
        crate::autocomplete::get_slash_command_context(
            &self.lines,
            self.cursor_line,
            self.cursor_col,
        )
    }

    fn is_slash_name_completion_at_prompt_start(&self) -> bool {
        let ctx = self.current_slash_command_context();
        let kind_slash = self
            .autocomplete
            .as_ref()
            .map(|s| s.kind == Some(crate::autocomplete::SuggestionKind::SlashCommand))
            .unwrap_or(false);
        let default_slash = self.autocomplete.is_some()
            && self.autocomplete.as_ref().unwrap().prefix.starts_with('/');
        (kind_slash || default_slash)
            && matches!(ctx, Some(c) if c.kind == crate::autocomplete::SlashKind::Name && c.at_prompt_start)
    }

    fn apply_completion(
        &self,
        item: &crate::autocomplete::CompletionItem,
        prefix: &str,
    ) -> crate::autocomplete::CompletionResult {
        match self.autocomplete_provider.as_ref() {
            Some(provider) => provider.apply_completion(
                &self.lines,
                self.cursor_line,
                self.cursor_col,
                item,
                prefix,
            ),
            None => crate::autocomplete::CompletionResult {
                lines: self.lines.clone(),
                cursor_line: self.cursor_line,
                cursor_col: self.cursor_col,
            },
        }
    }

    fn handle_tab_completion(&mut self) {
        if self.autocomplete_provider.is_none() {
            return;
        }
        if matches!(self.current_slash_command_context(), Some(c) if c.kind == crate::autocomplete::SlashKind::Name)
        {
            self.request_autocomplete(false, true);
        } else {
            self.request_autocomplete(true, true);
        }
    }

    fn maybe_autocomplete_after_insert(&mut self, ch: &str) {
        if self.autocomplete.is_none() {
            let slash_ctx = self.current_slash_command_context();
            let c = ch.chars().next().unwrap_or(' ');
            if c == '/'
                && matches!(&slash_ctx, Some(ctx) if ctx.kind == crate::autocomplete::SlashKind::Name)
            {
                self.request_autocomplete(false, false);
            } else if c == '@' || c == '#' {
                let current_line = &self.lines[self.cursor_line];
                let before = char_prefix(current_line, self.cursor_col);
                let prev = char_at(&before, before.chars().count().saturating_sub(2));
                if before.chars().count() <= 1 || prev == Some(' ') || prev == Some('\t') {
                    self.request_autocomplete(false, false);
                }
            } else if c.is_ascii_alphanumeric() || ".-_".contains(c) {
                let current_line = &self.lines[self.cursor_line];
                let before = char_prefix(current_line, self.cursor_col);
                if slash_ctx.is_some() || ends_with_symbol_token(&before) {
                    self.request_autocomplete(false, false);
                }
            }
        } else {
            self.refresh_autocomplete_after_edit(false);
        }
    }

    fn request_autocomplete(&mut self, force: bool, explicit_tab: bool) {
        let Some(provider) = self.autocomplete_provider.as_ref() else {
            return;
        };
        if force {
            let should = provider.should_trigger_file_completion(
                &self.lines,
                self.cursor_line,
                self.cursor_col,
            );
            if !should {
                return;
            }
        }
        let Some(suggestions) =
            provider.get_suggestions(&self.lines, self.cursor_line, self.cursor_col, force)
        else {
            self.cancel_autocomplete();
            return;
        };
        if suggestions.items.is_empty() {
            self.cancel_autocomplete();
            return;
        }
        if force && explicit_tab && suggestions.items.len() == 1 {
            let item = suggestions.items[0].clone();
            self.push_undo_snapshot();
            self.last_action = None;
            let prefix = suggestions.prefix.clone();
            let result = self.apply_completion(&item, &prefix);
            self.lines = result.lines;
            self.cursor_line = result.cursor_line;
            self.set_cursor_col(result.cursor_col);
            self.emit(EditorEvent::Changed(self.get_text()));
            return;
        }
        let matching_prefix =
            if suggestions.kind == Some(crate::autocomplete::SuggestionKind::SlashCommand) {
                suggestions
                    .prefix
                    .strip_prefix('/')
                    .unwrap_or(&suggestions.prefix)
                    .to_string()
            } else {
                suggestions.prefix.clone()
            };
        let mut state = crate::autocomplete::AutocompleteState::new(
            suggestions.items,
            5,
            suggestions.prefix.clone(),
            suggestions.kind,
        );
        if let Some(idx) = state.best_match_index(&matching_prefix) {
            state.set_selected_index(idx);
        }
        let was_showing = self.autocomplete.is_some();
        self.autocomplete = Some(state);
        if was_showing != self.autocomplete.is_some() {
            self.emit(EditorEvent::AutocompleteToggled(
                self.autocomplete.is_some(),
            ));
        }
    }

    fn refresh_autocomplete_after_edit(&mut self, retrigger: bool) {
        let current_line = &self.lines[self.cursor_line];
        let before = char_prefix(current_line, self.cursor_col);
        let has_ctx =
            self.current_slash_command_context().is_some() || ends_with_symbol_token(&before);

        if self.autocomplete.is_some() {
            if self.get_text().trim().is_empty() {
                self.cancel_autocomplete();
                return;
            }
            let force = self
                .autocomplete
                .as_ref()
                .map(|s| s.forced)
                .unwrap_or(false);
            self.request_autocomplete(force, false);
            return;
        }
        if retrigger && has_ctx {
            self.request_autocomplete(false, false);
        }
    }

    pub fn cancel_autocomplete(&mut self) {
        let was = self.autocomplete.is_some();
        self.autocomplete = None;
        if was {
            self.emit(EditorEvent::AutocompleteToggled(false));
        }
    }

    // ---- layout / rendering ------------------------------------------------

    /// Build layout lines for a given content width (port of layoutText).
    pub fn layout_text(&self, content_width: usize) -> Vec<LayoutLine> {
        let mut layout_lines = Vec::new();
        if self.lines.is_empty() || (self.lines.len() == 1 && self.lines[0].is_empty()) {
            layout_lines.push(LayoutLine {
                text: String::new(),
                has_cursor: true,
                cursor_pos: 0,
                source_line: 0,
                source_start: 0,
            });
            return layout_lines;
        }
        for (i, line) in self.lines.iter().enumerate() {
            let is_current = i == self.cursor_line;
            let line_vis_width = str_width(line);
            if line.is_empty() {
                layout_lines.push(LayoutLine {
                    text: String::new(),
                    has_cursor: is_current,
                    cursor_pos: 0,
                    source_line: i,
                    source_start: 0,
                });
                continue;
            }
            if line_vis_width <= content_width {
                if is_current {
                    layout_lines.push(LayoutLine {
                        text: line.clone(),
                        has_cursor: true,
                        cursor_pos: self.cursor_col.min(line.chars().count()),
                        source_line: i,
                        source_start: 0,
                    });
                } else {
                    layout_lines.push(LayoutLine {
                        text: line.clone(),
                        has_cursor: false,
                        cursor_pos: 0,
                        source_line: i,
                        source_start: 0,
                    });
                }
            } else {
                let chunks = word_wrap_line(line, content_width, Some(self.segment(line)));
                for (chunk_index, chunk) in chunks.iter().enumerate() {
                    let cursor_pos = self.cursor_col;
                    let is_last = chunk_index == chunks.len() - 1;
                    let (has_cursor, adjusted) = if is_current {
                        if is_last {
                            (
                                cursor_pos >= chunk.start_index,
                                cursor_pos.saturating_sub(chunk.start_index),
                            )
                        } else if cursor_pos >= chunk.start_index && cursor_pos < chunk.end_index {
                            let adj = cursor_pos - chunk.start_index;
                            (true, adj.min(chunk.text.chars().count()))
                        } else {
                            (false, 0)
                        }
                    } else {
                        (false, 0)
                    };
                    layout_lines.push(LayoutLine {
                        text: chunk.text.clone(),
                        has_cursor,
                        cursor_pos: adjusted,
                        source_line: i,
                        source_start: chunk.start_index,
                    });
                }
            }
        }
        layout_lines
    }

    /// Compute the scroll window and the visible layout lines. Returns
    /// (visible lines, scroll offset, lines hidden above, lines hidden below).
    pub fn visible_window(
        &mut self,
        width: usize,
        terminal_rows: u16,
    ) -> (Vec<LayoutLine>, usize, usize, usize) {
        self.last_width = width.max(1);
        self.terminal_rows = terminal_rows;
        let layout_lines = self.layout_text(self.last_width);
        let max_visible = (terminal_rows as f32 * 0.3).floor().max(5.0) as usize;
        let cursor_line_index = layout_lines.iter().position(|l| l.has_cursor).unwrap_or(0);
        if cursor_line_index < self.scroll_offset {
            self.scroll_offset = cursor_line_index;
        } else if cursor_line_index >= self.scroll_offset + max_visible {
            self.scroll_offset = cursor_line_index + 1 - max_visible;
        }
        let max_scroll = layout_lines.len().saturating_sub(max_visible);
        self.scroll_offset = self.scroll_offset.min(max_scroll);
        let end = (self.scroll_offset + max_visible).min(layout_lines.len());
        let visible: Vec<LayoutLine> = layout_lines[self.scroll_offset..end].to_vec();
        let below = layout_lines.len().saturating_sub(end);
        (visible, self.scroll_offset, self.scroll_offset, below)
    }

    pub fn cursor_visual(&self, visible: &[LayoutLine]) -> Option<(usize, usize)> {
        visible
            .iter()
            .position(|l| l.has_cursor)
            .map(|row| (row, visible[row].cursor_pos))
    }
}

// ---- helpers -------------------------------------------------------------

/// Normalize CRLF/CR to LF and tabs to 4 spaces (TS normalizeText).
pub fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
}

/// Split a string at a char (not byte) index.
fn split_at_char(s: &str, char_idx: usize) -> (String, String) {
    let mut left = String::new();
    let mut count = 0usize;
    for c in s.chars() {
        if count < char_idx {
            left.push(c);
            count += 1;
        } else {
            break;
        }
    }
    let right: String = s.chars().skip(char_idx).collect();
    (left, right)
}

fn char_prefix(s: &str, char_idx: usize) -> String {
    split_at_char(s, char_idx).0
}

fn char_suffix(s: &str, char_idx: usize) -> String {
    split_at_char(s, char_idx).1
}

fn char_at(s: &str, char_idx: usize) -> Option<char> {
    s.chars().nth(char_idx)
}

/// Find `needle` (single char) after a char index.
fn char_find_after(line: &str, from_char: usize, needle: &str) -> Option<usize> {
    let n = needle.chars().next()?;
    if from_char == usize::MAX {
        return line.chars().position(|c| c == n);
    }
    line.chars()
        .enumerate()
        .skip_while(|(i, _)| *i <= from_char)
        .find(|(_, c)| *c == n)
        .map(|(i, _)| i)
}

fn char_find_before(line: &str, from_char: usize, needle: &str) -> Option<usize> {
    let n = needle.chars().next()?;
    let limit = if from_char == usize::MAX {
        line.chars().count()
    } else {
        from_char
    };
    line.chars()
        .take(limit)
        .collect::<Vec<_>>()
        .iter()
        .rposition(|&c| c == n)
}

/// Decode a printable character from a key id ("" for control keys).
fn decode_printable(input: &str) -> Option<String> {
    let (mods, key) = split_key_id(input);
    if !matches!(mods.as_str(), "" | "shift") {
        return None;
    }
    let printable = match key.as_str() {
        "space" => " ".to_string(),
        "enter" | "tab" | "escape" | "backspace" | "delete" | "up" | "down" | "left" | "right"
        | "home" | "end" | "pageUp" | "pageDown" => return None,
        k => k.to_string(),
    };
    if printable.chars().count() == 1 {
        if mods == "shift" {
            return Some(printable.to_uppercase());
        }
        Some(printable)
    } else {
        None
    }
}

fn split_key_id(input: &str) -> (String, String) {
    let parts: Vec<&str> = input.split('+').collect();
    if parts.len() > 1 {
        (
            parts[..parts.len() - 1].join("+"),
            parts[parts.len() - 1].to_string(),
        )
    } else {
        (String::new(), input.to_string())
    }
}

/// Match `(?:^|[ \t])(?:@|...)...` symbol-token suffix used for @/# autocomplete.
fn ends_with_symbol_token(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return false;
    }
    // find last whitespace boundary
    let start = chars
        .iter()
        .rposition(|&c| c == ' ' || c == '\t')
        .map(|p| p + 1)
        .unwrap_or(0);
    let token: String = chars[start..].iter().collect();
    let mut tchars = token.chars();
    matches!(tchars.next(), Some('@' | '#')) && !token.contains(char::is_whitespace)
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
    fn word_ops_and_kill_ring() {
        let mut e = ed();
        e.handle_input("h");
        e.handle_input("e");
        e.handle_input("l");
        e.handle_input("l");
        e.handle_input("o");
        e.handle_input(" ");
        e.handle_input("w");
        e.handle_input("o");
        e.handle_input("r");
        e.handle_input("l");
        e.handle_input("d");
        assert_eq!(e.get_text(), "hello world");
        e.handle_input("ctrl+w");
        assert_eq!(e.get_text(), "hello ");
        // ctrl+k at end of line is a no-op (TS parity: only kills forward text).
        e.handle_input("ctrl+k");
        assert_eq!(e.get_text(), "hello ");
        e.handle_input("ctrl+y");
        assert_eq!(e.get_text(), "hello world");
        // Ring holds one entry: yank-pop is a no-op.
        e.handle_input("alt+y");
        assert_eq!(e.get_text(), "hello world");
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
    fn jump_mode() {
        let mut e = ed();
        e.set_text("hello world");
        e.move_to_line_start();
        e.handle_input("ctrl+]");
        e.handle_input("w");
        assert_eq!(e.get_cursor(), (0, 6));
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

    #[test]
    fn word_wrap_chunks() {
        let chunks = word_wrap_line("hello world this wraps", 10, None);
        for c in &chunks {
            assert!(str_width(&c.text) <= 10, "chunk too wide: {:?}", c.text);
        }
        let joined: String = chunks
            .iter()
            .map(|c| c.text.clone())
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(joined, "hello world this wraps");
    }

    #[test]
    fn visual_map_wrap() {
        let mut e = ed();
        e.set_text("aaaaaaaaaa bbbbbbbbbb");
        let vl = e.build_visual_line_map(10);
        // The trailing space of chunk 1 wraps to its own visual line (TS parity).
        assert_eq!(vl.len(), 3);
        assert_eq!(vl[0].length, 10);
        assert_eq!(vl[1].length, 1);
        assert_eq!(vl[0].logical_line, 0);
        assert_eq!(vl[1].logical_line, 0);
    }
}
