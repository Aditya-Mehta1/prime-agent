//! Autocomplete: provider contract, suggestion state, and selection list
//! rendering ported from `packages/tui/src/autocomplete.ts` +
//! `components/select-list.ts` (the subset the interactive agent view uses:
//! slash-command and file/path completion with a select list).

use crate::width::str_width;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionKind {
    SlashCommand,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestions {
    pub prefix: String,
    pub kind: Option<SuggestionKind>,
    pub items: Vec<CompletionItem>,
}

/// Result of applying a completion to the editor buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResult {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,
}

/// Provider contract mirroring TS AutocompleteProvider (synchronous).
pub trait AutocompleteProvider: Send {
    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        force: bool,
    ) -> Option<Suggestions>;
    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &CompletionItem,
        prefix: &str,
    ) -> CompletionResult;
    fn should_trigger_file_completion(
        &self,
        _lines: &[String],
        _cursor_line: usize,
        _cursor_col: usize,
    ) -> bool {
        true
    }
}

/// Slash-command context (port of slash-command-context.ts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashKind {
    Name,
    Argument,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashContext {
    pub kind: SlashKind,
    pub at_prompt_start: bool,
}

/// Detect the active slash command context at the cursor.
pub fn get_slash_command_context(
    lines: &[String],
    cursor_line: usize,
    cursor_col: usize,
) -> Option<SlashContext> {
    let line = lines.get(cursor_line)?;
    if lines.len() != 1 && cursor_line != 0 {
        return None;
    }
    let before: String = line.chars().take(cursor_col).collect();
    let trimmed_start = before.starts_with('/');
    if !trimmed_start {
        return None;
    }
    let has_space = before.contains(' ');
    Some(SlashContext {
        kind: if has_space {
            SlashKind::Argument
        } else {
            SlashKind::Name
        },
        at_prompt_start: true,
    })
}

/// Selection state for the autocomplete dropdown (port of SelectList).
#[derive(Debug, Clone)]
pub struct AutocompleteState {
    pub items: Vec<CompletionItem>,
    pub selected_index: usize,
    pub max_visible: usize,
    pub prefix: String,
    pub kind: Option<SuggestionKind>,
    pub forced: bool,
}

impl AutocompleteState {
    pub fn new(
        items: Vec<CompletionItem>,
        max_visible: usize,
        prefix: String,
        kind: Option<SuggestionKind>,
    ) -> Self {
        Self {
            items,
            selected_index: 0,
            max_visible: max_visible.clamp(3, 20),
            prefix,
            kind,
            forced: false,
        }
    }

    pub fn set_selected_index(&mut self, index: usize) {
        if !self.items.is_empty() {
            self.selected_index = index.min(self.items.len() - 1);
        }
    }

    pub fn move_up(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected_index = if self.selected_index == 0 {
            self.items.len() - 1
        } else {
            self.selected_index - 1
        };
    }

    pub fn move_down(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected_index = if self.selected_index == self.items.len() - 1 {
            0
        } else {
            self.selected_index + 1
        };
    }

    pub fn selected_item(&self) -> Option<CompletionItem> {
        self.items.get(self.selected_index).cloned()
    }

    /// Best match index: exact value match, else first prefix match, else none.
    pub fn best_match_index(&self, prefix: &str) -> Option<usize> {
        if prefix.is_empty() {
            return None;
        }
        let mut first_prefix = None;
        for (i, item) in self.items.iter().enumerate() {
            if item.value == prefix {
                return Some(i);
            }
            if first_prefix.is_none() && item.value.starts_with(prefix) {
                first_prefix = Some(i);
            }
        }
        first_prefix
    }

    /// Render dropdown lines (select list with selected marker, description
    /// column, and directional scroll info).
    pub fn render(&self, width: usize) -> Vec<String> {
        if self.items.is_empty() {
            return vec!["  No matching commands".to_string()];
        }
        let start = self
            .selected_index
            .saturating_sub(self.max_visible / 2)
            .min(self.items.len().saturating_sub(self.max_visible));
        let end = (start + self.max_visible).min(self.items.len());
        let mut lines = Vec::new();
        for (i, item) in self.items[start..end].iter().enumerate() {
            let idx = start + i;
            let selected = idx == self.selected_index;
            let prefix = if selected { "› " } else { "  " };
            let mut line = format!("{}{}", prefix, item.label);
            if let Some(desc) = &item.description {
                let single: String = desc.chars().filter(|&c| c != '\n').collect();
                let used = str_width(&line);
                let remaining = width.saturating_sub(used + 4);
                if remaining > 4 {
                    let truncated: String = truncate(&single, remaining - 1);
                    line = format!("{} {}", line, truncated);
                }
            }
            lines.push(line);
        }
        let hidden_above = start;
        let hidden_below = self.items.len() - end;
        if hidden_above > 0 || hidden_below > 0 {
            let mut indicators = Vec::new();
            if hidden_above > 0 {
                indicators.push(format!("↑ {} more", hidden_above));
            }
            if hidden_below > 0 {
                indicators.push(format!("↓ {} more", hidden_below));
            }
            lines.push(format!("  {}", indicators.join("  ")));
        }
        lines
    }
}

fn truncate(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = crate::width::char_width(c);
        if w + cw > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

/// File/path autocomplete provider scanning the filesystem relative to cwd.
pub struct PathCompletionProvider {
    pub base: std::path::PathBuf,
}

impl AutocompleteProvider for PathCompletionProvider {
    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        _cursor_col: usize,
        _force: bool,
    ) -> Option<Suggestions> {
        let line = lines.get(cursor_line)?;
        // find token ending at cursor
        let col = line.len();
        let before = &line[..col];
        let start = before
            .rfind(|c: char| c.is_whitespace())
            .map(|p| p + 1)
            .unwrap_or(0);
        let token = &before[start..];
        if token.is_empty() {
            return None;
        }
        let path = self.base.join(token);
        let parent = if token.ends_with('/') {
            path.clone()
        } else {
            path.parent()?.to_path_buf()
        };
        let partial = if token.ends_with('/') {
            String::new()
        } else {
            path.file_name()?.to_string_lossy().to_string()
        };
        let mut items = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&parent) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with(&partial) || name.starts_with('.') {
                    continue;
                }
                let is_dir = entry.path().is_dir();
                let label = if is_dir {
                    format!("{}/", name)
                } else {
                    name.clone()
                };
                items.push(CompletionItem {
                    value: label.clone(),
                    label,
                    description: None,
                    argument_hint: None,
                });
            }
        }
        items.sort_by(|a, b| a.value.cmp(&b.value));
        if items.is_empty() {
            return None;
        }
        Some(Suggestions {
            prefix: token.to_string(),
            kind: Some(SuggestionKind::File),
            items,
        })
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        _cursor_col: usize,
        item: &CompletionItem,
        prefix: &str,
    ) -> CompletionResult {
        let mut lines = lines.to_vec();
        let line = &mut lines[cursor_line];
        if let Some(pos) = line.rfind(prefix) {
            line.replace_range(pos..pos + prefix.len(), &item.value);
        }
        CompletionResult {
            lines,
            cursor_line,
            cursor_col: cursor_line,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_context() {
        let ctx = get_slash_command_context(&["/he".to_string()], 0, 3);
        assert!(matches!(ctx, Some(c) if c.kind == SlashKind::Name));
        let ctx = get_slash_command_context(&["/help ar".to_string()], 0, 7);
        assert!(matches!(ctx, Some(c) if c.kind == SlashKind::Argument));
        assert!(get_slash_command_context(&["plain".to_string()], 0, 5).is_none());
    }

    #[test]
    fn best_match() {
        let state = AutocompleteState::new(
            vec![
                CompletionItem {
                    value: "help".into(),
                    label: "help".into(),
                    description: None,
                    argument_hint: None,
                },
                CompletionItem {
                    value: "hello".into(),
                    label: "hello".into(),
                    description: None,
                    argument_hint: None,
                },
            ],
            5,
            "hel".into(),
            Some(SuggestionKind::SlashCommand),
        );
        assert_eq!(state.best_match_index("hel"), Some(0));
        assert_eq!(state.best_match_index("hello"), Some(1));
        assert_eq!(state.best_match_index("zzz"), None);
    }

    #[test]
    fn select_render() {
        let items: Vec<CompletionItem> = (0..7)
            .map(|i| CompletionItem {
                value: format!("cmd{i}"),
                label: format!("cmd{i}"),
                description: Some(format!("description {i}\nsecond line")),
                argument_hint: None,
            })
            .collect();
        let state = AutocompleteState::new(items, 5, String::new(), None);
        let lines = state.render(40);
        assert!(lines.len() > 5);
        assert!(lines.iter().any(|l| l.contains("more")));
    }
}
