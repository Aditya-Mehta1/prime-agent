//! Interactive agent view: fullscreen chat frame composed like the TS
//! interactive mode — a pinned top bar, a scrollable transcript window
//! (splash, chat rows, loader), and a dock (prompt-context line, editor
//! surface, tray). The session loop folds events into the view; this module
//! owns row geometry and scroll behavior only.

use crate::chat::{
    render_assistant, render_loader, render_text_rows, render_tool_card, render_user_block,
    ChatEntry, Detail, WorkingState,
};
use crate::chrome::{
    conversation_detail_status, render_prompt_context, render_splash, render_top_bar, render_tray,
    ChromeState,
};
use crate::editor::Editor;
use crate::session::TranscriptItem;
use crate::theme::{Theme, ThemeBg, ThemeColor};
use crate::width::str_width;
use crate::{Line, Span};
use ratatui::style::{Modifier, Style};

/// Minimum transcript rows when the dock would crowd them out
/// (TS `FULLSCREEN_MIN_TRANSCRIPT_ROWS`).
pub const FULLSCREEN_MIN_TRANSCRIPT_ROWS: usize = 3;

pub struct AgentView {
    pub theme: Theme,
    pub editor: Editor,
    pub chrome: ChromeState,
    pub chat: Vec<ChatEntry>,
    pub detail: Detail,
    pub working: Option<WorkingState>,
    /// Animation frame for spinners and the working icon.
    pub pulse_frame: usize,
    /// When the current working loader started (elapsed label).
    pub working_since: Option<std::time::Instant>,
    /// An active provider auto-retry (replaces the working loader while
    /// the retry loop waits, TS `retryLoader`).
    pub retry: Option<crate::chat::RetryState>,
    /// The first-run onboarding pane (TS `runStartupOnboarding`): while
    /// set, it owns the whole frame.
    pub onboarding: Option<crate::onboarding::OnboardingScreen>,
    scroll_top: usize,
    following: bool,
    /// Rows of the terminal the editor should lay out against.
    terminal_rows: u16,
    /// Cursor cell within the last dock render: (dock row, column).
    dock_cursor: Option<(usize, usize)>,
    /// Window height of the last composed frame (cursor positioning).
    window_rows: usize,
}

impl AgentView {
    pub fn new(theme: Theme) -> Self {
        Self {
            theme,
            editor: Editor::new(),
            chrome: ChromeState::default(),
            chat: Vec::new(),
            detail: Detail::Overview,
            working: None,
            pulse_frame: 0,
            working_since: None,
            retry: None,
            onboarding: None,
            scroll_top: 0,
            following: true,
            terminal_rows: 24,
            dock_cursor: None,
            window_rows: 0,
        }
    }

    pub fn set_terminal_rows(&mut self, rows: u16) {
        self.terminal_rows = rows;
    }

    /// Append one chat component.
    pub fn push_entry(&mut self, entry: ChatEntry) {
        self.chat.push(entry);
    }

    /// Append a replay transcript item (mapped onto chat components).
    pub fn push(&mut self, item: TranscriptItem) {
        self.chat.push(item_to_entry(item));
    }

    /// The conversation-detail label for the prompt-context row.
    fn detail_label(&self) -> String {
        let key = crate::keybindings::KeybindingsManager::new()
            .first_key("app.tools.expand")
            .map(|key| crate::keybindings::format_key_text(&key))
            .unwrap_or_default();
        // The TS label: "Details" keeps the expand hint (only "Expanded"
        // collapses).
        match self.detail {
            Detail::Overview => conversation_detail_status(false, &key),
            Detail::Details => format!("Details mode ({key} to expand)"),
        }
    }

    /// Scroll position of the transcript window: following keeps the tail
    /// pinned; otherwise the offset stays where the user left it.
    pub fn scroll_by(&mut self, delta: isize) {
        self.following = false;
        self.scroll_top = (self.scroll_top as isize + delta).max(0) as usize;
    }

    pub fn follow(&mut self) {
        self.following = true;
    }

    /// Render the scrollable transcript: splash rows, chat component rows,
    /// and the working loader when a turn is active.
    pub fn render_transcript(&mut self, width: usize) -> Vec<Line> {
        if let (Some(working), Some(since)) = (&mut self.working, self.working_since) {
            working.elapsed_secs = since.elapsed().as_secs();
        }
        let mut lines: Vec<Line> = render_splash(&self.chrome, &self.theme, width);
        let mut first = true;
        for entry in &self.chat {
            match entry {
                ChatEntry::Status { text, kind } => {
                    let style = match kind {
                        crate::chat::StatusKind::Info => self.theme.fg_style(ThemeColor::Dim),
                        crate::chat::StatusKind::Warning => {
                            self.theme.fg_style(ThemeColor::Warning)
                        }
                        crate::chat::StatusKind::Error => self.theme.fg_style(ThemeColor::Error),
                    };
                    lines.push(Vec::new());
                    lines.extend(render_text_rows(text, style, width));
                }
                ChatEntry::User { text } => {
                    if !first {
                        lines.push(Vec::new());
                    }
                    lines.extend(render_user_block(text, &self.theme, width));
                }
                ChatEntry::SlashCommand { text } => {
                    // The echo row leads with a spacer when the chat is not
                    // empty (TS adds `Spacer(1)` before the component).
                    if !first {
                        lines.push(Vec::new());
                    }
                    let typed = pa_types::slash_commands::parse_slash_command(text)
                        .map(|(name, _)| name)
                        .unwrap_or_default();
                    let takes_argument = pa_types::slash_commands::SlashCommandRegistry::builtin()
                        .takes_argument(&typed);
                    lines.extend(crate::chat_slash::render_slash_command(
                        text,
                        takes_argument,
                        &self.theme,
                        width,
                    ));
                }
                ChatEntry::SlashCommandResult { content } => {
                    lines.extend(crate::chat_slash::render_slash_command_result(
                        content,
                        &self.theme,
                        width,
                    ));
                }
                ChatEntry::Assistant(message) => {
                    lines.extend(render_assistant(message, self.detail, &self.theme, width));
                }
                ChatEntry::Tool(card) => {
                    lines.extend(render_tool_card(card, self.pulse_frame, &self.theme, width));
                }
            }
            first = false;
        }
        // While the provider retry loop waits, its countdown loader owns
        // the status area (TS `stopWorkingLoader` + `retryLoader`).
        if let Some(retry) = &self.retry {
            lines.extend(crate::chat::render_retry(
                retry,
                self.pulse_frame,
                &self.theme,
                width,
            ));
        } else if let Some(working) = &self.working {
            lines.extend(render_loader(working, self.pulse_frame, &self.theme, width));
        }
        lines
    }

    /// Render the dock: prompt-context row(s), the autocomplete overlay
    /// (when showing), the editor surface, the tray.
    pub fn render_dock(&mut self, width: usize) -> Vec<Line> {
        let mut lines = render_prompt_context(&self.detail_label(), &self.theme, width);
        let context_rows = lines.len();
        let overlay_rows = self.render_autocomplete_overlay(width);
        lines.extend(overlay_rows);
        let (editor_rows, cursor) = self.render_editor_surface(width);
        let overlay_count = lines.len() - context_rows;
        self.dock_cursor = cursor.map(|(row, col)| (context_rows + overlay_count + row, col));
        lines.extend(editor_rows);
        lines.push(render_tray(&self.chrome, &self.theme, width));
        lines
    }

    /// The autocomplete dropdown, mounted just above the editor surface (TS
    /// anchors the overlay immediately above the cursor row; the editor's
    /// first content row carries the cursor in the common single-line
    /// case). Each row pads to the input width and floats on the popup
    /// background between the editor's left padding and prompt prefix.
    fn render_autocomplete_overlay(&mut self, width: usize) -> Vec<Line> {
        let Some(state) = self.editor.autocomplete_state() else {
            return Vec::new();
        };
        let styles = crate::autocomplete::SelectListStyles {
            selected_prefix: self.theme.fg_style(ThemeColor::Accent),
            selected_text: self.theme.fg_style(ThemeColor::Accent),
            description: self.theme.fg_style(ThemeColor::Muted),
            argument_hint: self.theme.fg_style(ThemeColor::MdCode),
            scroll_info: self.theme.fg_style(ThemeColor::Muted),
            no_match: self.theme.fg_style(ThemeColor::Muted),
        };
        let bg = self.theme.bg_style(ThemeBg::ToolPanelBg);
        let padding_x = 2usize;
        let prompt_width = str_width("> ");
        let content_width = width.saturating_sub(padding_x * 2).max(1);
        let input_width = content_width.saturating_sub(prompt_width).max(1);
        let mut rows: Vec<Line> = Vec::new();
        let mut overlay = Vec::new();
        overlay.push(Vec::new());
        overlay.extend(state.render(input_width, &styles));
        overlay.push(Vec::new());
        for line in overlay {
            let used: usize = line.iter().map(|s| str_width(&s.content)).sum();
            let mut row: Line = vec![Span::styled(" ".repeat(padding_x + prompt_width), bg)];
            row.extend(line);
            row.push(Span::styled(
                " ".repeat(input_width.saturating_sub(used)),
                bg,
            ));
            row.push(Span::styled(" ".repeat(padding_x), bg));
            rows.push(pad_row(row, width));
        }
        rows
    }

    /// The editor surface (TS `Editor.render` with a background): a blank
    /// bg row, content rows with the `> ` prompt and a reverse-video cursor,
    /// and a trailing bg row. Scroll indicators replace the blank rows.
    fn render_editor_surface(&mut self, width: usize) -> (Vec<Line>, Option<(usize, usize)>) {
        let bg = crate::chrome::editor_background(&self.theme);
        let border = self.theme.fg_style(ThemeColor::BorderMuted);
        let padding_x = 2usize;
        let content_width = width.saturating_sub(padding_x * 2).max(1);
        let prompt = "> ";
        let prompt_width = str_width(prompt);
        let input_width = content_width.saturating_sub(prompt_width).max(1);
        let layout_width = input_width;
        let (visible, scroll_offset, _hidden_above, hidden_below) =
            self.editor.visible_window(layout_width, self.terminal_rows);
        let mut rows: Vec<Line> = Vec::new();
        if scroll_offset > 0 {
            let indicator = format!(" \u{2191} {scroll_offset} more");
            rows.push(indicator_row(&indicator, bg, border, width));
        } else {
            rows.push(vec![Span::styled(" ".repeat(width), bg)]);
        }
        let mut cursor: Option<(usize, usize)> = None;
        for (index, line) in visible.iter().enumerate() {
            let mut row: Line = vec![Span::styled(" ".to_string(), bg)];
            // The `> ` prompt prefix renders plain on the surface background
            // (TS `formatPromptPrefix` styles only `!` bash prompts).
            if index == 0 {
                row.push(Span::styled(prompt.to_string(), bg));
            } else {
                row.push(Span::styled(" ".repeat(prompt_width), bg));
            }
            row.push(Span::styled(" ".to_string(), bg));
            let text: &str = &line.text;
            let before = line.cursor_pos.min(text.chars().count()).to_string();
            let _ = before;
            let (head, tail) = split_at_chars(text, line.cursor_pos.min(text.chars().count()));
            let mut used = str_width(text);
            if line.has_cursor {
                if tail.is_empty() {
                    row.push(Span::styled(head.to_string(), bg));
                    row.push(Span::styled(
                        " ".to_string(),
                        bg.add_modifier(Modifier::REVERSED),
                    ));
                    used += 1;
                } else {
                    let first = tail.chars().next().unwrap_or(' ');
                    let rest: String = tail[first.len_utf8()..].to_string();
                    row.push(Span::styled(head.to_string(), bg));
                    row.push(Span::styled(
                        first.to_string(),
                        bg.add_modifier(Modifier::REVERSED),
                    ));
                    row.push(Span::styled(rest, bg));
                }
                cursor = Some((index + 1, str_width(head) + 4));
            } else {
                row.push(Span::styled(text.to_string(), bg));
            }
            row.push(Span::styled(
                " ".repeat(input_width.saturating_sub(used)),
                bg,
            ));
            row.push(Span::styled(" ".repeat(padding_x), bg));
            rows.push(row);
        }
        if hidden_below > 0 {
            rows.push(indicator_row(
                &format!(" \u{2193} {hidden_below} more"),
                bg,
                border,
                width,
            ));
        } else {
            rows.push(vec![Span::styled(" ".repeat(width), bg)]);
        }
        (rows, cursor)
    }

    /// Compose the fullscreen frame: top bar, transcript window (padded),
    /// dock at the bottom — exactly `height` rows.
    pub fn render_frame(&mut self, width: usize, height: usize) -> Vec<Line> {
        // The onboarding splash covers the pane (TS `showOverlay` 100%):
        // no top bar, transcript, or prompt dock behind it.
        if let Some(screen) = &self.onboarding {
            return screen.render(&self.theme, width, height);
        }
        let top = render_top_bar(&self.chrome, &self.theme, width);
        let transcript = self.render_transcript(width);
        let dock = self.render_dock(width);
        let dock_height = dock
            .len()
            .min(height.saturating_sub(FULLSCREEN_MIN_TRANSCRIPT_ROWS));
        let dock: Vec<Line> = if dock.len() > dock_height {
            dock[dock.len() - dock_height..].to_vec()
        } else {
            dock
        };
        let window_height = height
            .saturating_sub(1 + dock.len())
            .max(FULLSCREEN_MIN_TRANSCRIPT_ROWS.min(height.saturating_sub(1 + dock.len())));
        let max_scroll = transcript.len().saturating_sub(window_height);
        if self.following {
            self.scroll_top = max_scroll;
        } else {
            self.scroll_top = self.scroll_top.min(max_scroll);
        }
        let start = self.scroll_top.min(max_scroll);
        self.window_rows = window_height;
        let mut frame: Vec<Line> = Vec::with_capacity(height);
        frame.push(pad_row(top, width));
        for line in &transcript[start..(start + window_height).min(transcript.len())] {
            frame.push(pad_row(line.clone(), width));
        }
        while frame.len() < height.saturating_sub(dock.len()) {
            frame.push(vec![Span::raw(" ".repeat(width))]);
        }
        for line in dock {
            frame.push(pad_row(line, width));
        }
        frame
    }

    /// Hardware cursor position within the last composed frame (0-based row,
    /// 0-based column), when the editor surface drew the cursor.
    pub fn frame_cursor(&self) -> Option<(usize, usize)> {
        if self.onboarding.is_some() {
            return None;
        }
        self.dock_cursor
            .map(|(row, col)| (row + 1 + self.window_rows, col))
    }
}

/// Split a string at a char boundary.
fn split_at_chars(text: &str, at: usize) -> (&str, &str) {
    let mut end = text.len();
    let mut count = 0;
    for (index, _) in text.char_indices() {
        if count == at {
            end = index;
            break;
        }
        count += 1;
    }
    if count < at {
        return (text, "");
    }
    (&text[..end], &text[end..])
}

/// One scroll-indicator surface row (`↑ N more` on the editor background).
fn indicator_row(indicator: &str, bg: Style, border: Style, width: usize) -> Line {
    let mut row: Line = vec![Span::styled(indicator.to_string(), border)];
    let used = str_width(indicator);
    row.push(Span::styled(" ".repeat(width.saturating_sub(used)), bg));
    row
}

/// Pad a rendered row to the full width (default background).
fn pad_row(line: Line, width: usize) -> Line {
    let used: usize = line.iter().map(|s| str_width(&s.content)).sum();
    let mut out = line;
    if used < width {
        out.push(Span::raw(" ".repeat(width - used)));
    }
    out
}

/// Map a replay transcript item onto a chat component.
fn item_to_entry(item: TranscriptItem) -> ChatEntry {
    match item {
        TranscriptItem::UserMessage { text } => ChatEntry::User { text },
        TranscriptItem::SystemNote { text } => ChatEntry::Status {
            text,
            kind: crate::chat::StatusKind::Info,
        },
        TranscriptItem::Assistant { text } => {
            ChatEntry::Assistant(Box::new(crate::chat::AssistantMessage {
                blocks: vec![crate::chat::MessageBlock::Text(text)],
                has_tool_calls: false,
                streaming: false,
            }))
        }
        TranscriptItem::ToolCall {
            id,
            name,
            arguments,
        } => ChatEntry::Tool(Box::new(crate::chat::ToolCallCard {
            id,
            name,
            args: serde_json::from_str(&arguments).unwrap_or(serde_json::Value::Null),
            started: false,
            result: None,
            result_partial: false,
        })),
        TranscriptItem::ToolResult {
            tool_call_id,
            tool_name,
            text,
        } => ChatEntry::Tool(Box::new(crate::chat::ToolCallCard {
            id: tool_call_id,
            name: tool_name,
            args: serde_json::Value::Null,
            started: true,
            result: Some(crate::chat::ToolResultView {
                content: vec![serde_json::json!({ "type": "text", "text": text })],
                details: serde_json::Value::Null,
                is_error: false,
            }),
            result_partial: false,
        })),
        TranscriptItem::BashExecution {
            command, exit_code, ..
        } => ChatEntry::Tool(Box::new(crate::chat::ToolCallCard {
            id: String::new(),
            name: "bash".to_string(),
            args: serde_json::json!({ "command": command, "exitCode": exit_code }),
            started: true,
            result: None,
            result_partial: false,
        })),
        TranscriptItem::AgentStatus { summary, .. } => ChatEntry::Status {
            text: summary,
            kind: crate::chat::StatusKind::Info,
        },
        TranscriptItem::ModelChange { model_id, .. } => ChatEntry::Status {
            text: format!("\u{2699} {model_id}"),
            kind: crate::chat::StatusKind::Info,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorMode, Theme};

    fn view() -> AgentView {
        AgentView::new(Theme::builtin("prime", ColorMode::TrueColor))
    }

    fn text_of(line: &Line) -> String {
        line.iter().map(|s| s.content.as_str()).collect::<String>()
    }

    #[test]
    fn frame_is_exactly_height_rows() {
        let mut v = view();
        v.chrome.version = "0.0.0".to_string();
        v.chrome.cwd = "/tmp/project".to_string();
        v.chrome.chat_name = "project".to_string();
        let frame = v.render_frame(80, 24);
        assert_eq!(frame.len(), 24);
        assert!(frame.iter().all(|l| str_width(&text_of(l)) <= 80));
        let joined = frame.iter().map(text_of).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("prime agent v0.0.0"));
        assert!(joined.contains("Collapsed mode (Ctrl+O to expand)"));
        assert!(joined.contains(">"));
    }

    #[test]
    fn dock_pads_window_between_splash_and_editor() {
        let mut v = view();
        v.chrome.version = "0.0.0".to_string();
        v.chrome.cwd = "/w".to_string();
        v.chrome.chat_name = "w".to_string();
        let frame = v.render_frame(60, 40);
        assert_eq!(frame.len(), 40);
        // The editor prompt sits above the (empty) tray row.
        let joined = frame.iter().map(text_of).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("Collapsed mode"));
    }
}
