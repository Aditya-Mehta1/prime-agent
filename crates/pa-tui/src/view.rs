//! Interactive agent view: transcript + editor, themed like the TS product.
//!
//! Layout parity: user messages render as padded background blocks
//! (`userMessageBg`), assistant text as markdown, tool calls as panel lines
//! (`toolPanelBg` with a "⏺ toolname" header), a `─` separator above the
//! editor, and the editor with a "> " prompt prefix and "↑/↓ N more" scroll
//! indicators (dynamic-border.ts + editor.ts).

use crate::editor::Editor;
use crate::markdown::render_markdown;
use crate::session::TranscriptItem;
use crate::theme::{Theme, ThemeBg, ThemeColor};
use crate::{Line, Span};
use ratatui::style::Style;

pub struct AgentView {
    pub transcript: Vec<TranscriptItem>,
    pub editor: Editor,
    pub theme: Theme,
    pub model_label: String,
    pub status: String,
    /// Transcript scroll offset in rendered lines (0 = following the tail).
    pub scroll_lines: usize,
}

impl AgentView {
    pub fn new(theme: Theme) -> Self {
        Self {
            transcript: Vec::new(),
            editor: Editor::new(),
            theme,
            model_label: String::new(),
            status: String::new(),
            scroll_lines: 0,
        }
    }

    pub fn push(&mut self, item: TranscriptItem) {
        self.transcript.push(item);
    }

    /// Render the transcript lines for a given width.
    pub fn render_transcript(&self, width: usize) -> Vec<Line> {
        let mut out: Vec<Line> = Vec::new();
        let md = crate::markdown::MarkdownStyle::from_theme(&self.theme);
        for item in &self.transcript {
            match item {
                TranscriptItem::UserMessage { text } => {
                    self.render_user_block(text, width, &mut out);
                }
                TranscriptItem::Assistant { text } => {
                    let lines = render_markdown(text, width, &md);
                    out.extend(lines);
                    out.push(Vec::new());
                }
                TranscriptItem::ToolCall {
                    name, arguments, ..
                } => {
                    self.render_tool_header(name, Some(arguments), width, &mut out);
                }
                TranscriptItem::ToolResult {
                    tool_name, text, ..
                } => {
                    self.render_tool_header(tool_name, None, width, &mut out);
                    for line in text.lines().take(8) {
                        self.panel_line(line, width, &mut out);
                    }
                    if text.lines().count() > 8 {
                        self.panel_line(
                            &format!("… +{} more lines", text.lines().count() - 8),
                            width,
                            &mut out,
                        );
                    }
                    out.push(Vec::new());
                }
                TranscriptItem::BashExecution {
                    command, exit_code, ..
                } => {
                    self.render_tool_header(
                        &format!("bash (exit {})", exit_code.unwrap_or(0)),
                        Some(command),
                        width,
                        &mut out,
                    );
                    out.push(Vec::new());
                }
                TranscriptItem::AgentStatus { summary, .. } => {
                    let style = self.theme.fg_style(ThemeColor::Muted);
                    out.push(vec![Span::styled(format!("⏳ {summary}"), style)]);
                    out.push(Vec::new());
                }
                TranscriptItem::ModelChange { model_id, .. } => {
                    let style = self.theme.fg_style(ThemeColor::Muted);
                    out.push(vec![Span::styled(format!("⚙ {model_id}"), style)]);
                    out.push(Vec::new());
                }
            }
        }
        out
    }

    fn render_user_block(&self, text: &str, width: usize, out: &mut Vec<Line>) {
        let md = crate::markdown::MarkdownStyle::from_theme(&self.theme);
        let content_width = width.saturating_sub(4).max(1);
        let body_style = self.theme.fg_style(ThemeColor::UserMessageText);
        let rendered = render_markdown(text, content_width, &md);
        let blank = self.blank_bg_line(width);
        out.push(blank.clone());
        if rendered.is_empty() {
            out.push(self.bg_line(&format!("  {}  ", ""), width));
        }
        for line in &rendered {
            let mut spans: Vec<Span> = Vec::new();
            for s in line {
                spans.push(Span::styled(s.content.clone(), body_style.patch(s.style)));
            }
            out.push(self.bg_line_spans(&spans, width));
        }
        out.push(blank);
        out.push(Vec::new());
    }

    fn bg_style(&self) -> Style {
        self.theme.bg_style(ThemeBg::UserMessageBg)
    }

    fn blank_bg_line(&self, width: usize) -> Line {
        self.bg_line("", width)
    }

    fn bg_line(&self, _content: &str, width: usize) -> Line {
        let style = self.bg_style();
        vec![Span::styled(" ".repeat(width), style)]
    }

    fn bg_line_spans(&self, spans: &[Span], width: usize) -> Line {
        let style = self.bg_style();
        let used: usize = spans
            .iter()
            .map(|s| crate::width::str_width(&s.content))
            .sum();
        let mut line: Line = Vec::with_capacity(spans.len() + 1);
        line.push(Span::styled("  ", style));
        for s in spans {
            line.push(s.clone());
        }
        line.push(Span::styled(
            " ".repeat(width.saturating_sub(used + 2)),
            style,
        ));
        line.push(Span::styled("  ", style));
        line
    }

    fn render_tool_header(
        &self,
        name: &str,
        args: Option<&str>,
        width: usize,
        out: &mut Vec<Line>,
    ) {
        let title_style = self.theme.fg_style(ThemeColor::ToolTitle);
        let mut header = format!("⏺ {name}");
        if let Some(args) = args {
            let preview: String = args
                .chars()
                .filter(|&c| c != '\n')
                .take(width.saturating_sub(header.chars().count() + 6))
                .collect();
            if !preview.is_empty() {
                header.push_str(&format!(" {preview}"));
            }
        }
        self.panel_line_raw(vec![Span::styled(header, title_style)], width, out);
    }

    fn panel_line(&self, content: &str, width: usize, out: &mut Vec<Line>) {
        let style = self.theme.fg_style(ThemeColor::ToolOutput);
        self.panel_line_raw(vec![Span::styled(content.to_string(), style)], width, out);
    }

    fn panel_line_raw(&self, spans: Line, width: usize, out: &mut Vec<Line>) {
        let bg = self.theme.bg_style(ThemeBg::ToolPanelBg);
        let used: usize = spans
            .iter()
            .map(|s| crate::width::str_width(&s.content))
            .sum();
        let mut line: Line = Vec::new();
        line.push(Span::styled("  ", bg));
        for s in spans {
            line.push(Span::styled(s.content, bg.patch(s.style)));
        }
        line.push(Span::styled(" ".repeat(width.saturating_sub(used + 4)), bg));
        line.push(Span::styled("  ", bg));
        out.push(line);
    }

    /// Render the editor block for a given width/height, including the
    /// separator line, prompt prefix, scroll indicators, and status footer.
    pub fn render_editor(&mut self, width: usize, height: u16) -> EditorFrame {
        let border_style = self.theme.fg_style(ThemeColor::Border);
        let prompt = "> ";
        let prompt_width = crate::width::str_width(prompt);
        let layout_width = (width.saturating_sub(prompt_width)).max(1);
        let (visible, scroll_offset, hidden_above, hidden_below) =
            self.editor.visible_window(layout_width, height);

        let mut lines: Vec<Line> = Vec::new();
        if hidden_above > 0 {
            let indicator = format!("─── ↑ {hidden_above} more ");
            let rest = width.saturating_sub(crate::width::str_width(&indicator));
            lines.push(vec![Span::styled(
                format!("{}{}", indicator, "─".repeat(rest)),
                border_style,
            )]);
        } else {
            lines.push(vec![Span::styled("─".repeat(width.max(1)), border_style)]);
        }

        for line in &visible {
            let spans: Line = vec![
                Span::styled(prompt.to_string(), border_style),
                Span::styled(line.text.clone(), Style::default()),
            ];
            lines.push(spans);
        }

        if hidden_below > 0 {
            let indicator = format!("─── ↓ {hidden_below} more ");
            let rest = width.saturating_sub(crate::width::str_width(&indicator));
            lines.push(vec![Span::styled(
                format!("{}{}", indicator, "─".repeat(rest)),
                border_style,
            )]);
        } else {
            lines.push(vec![Span::styled("─".repeat(width.max(1)), border_style)]);
        }

        let cursor = self.editor.cursor_visual(&visible);
        EditorFrame {
            lines,
            cursor_row: cursor.map(|(r, _)| r + 1),
            cursor_col: cursor.map(|(_, c)| c + prompt_width),
            _scroll_offset: scroll_offset,
        }
    }

    /// Status footer: model label + task status (right-aligned in TS footer).
    pub fn render_footer(&self, width: usize) -> Line {
        let muted = self.theme.fg_style(ThemeColor::Muted);
        let accent = self.theme.fg_style(ThemeColor::Accent);
        let mut left = String::new();
        if !self.model_label.is_empty() {
            left.push_str(&self.model_label);
        }
        if !self.status.is_empty() {
            if !left.is_empty() {
                left.push_str(" · ");
            }
            left.push_str(&self.status);
        }
        let mut line = Vec::new();
        if left.is_empty() {
            line.push(Span::styled(" ".repeat(width), muted));
        } else {
            line.push(Span::styled(left.clone(), accent));
            let pad = width.saturating_sub(crate::width::str_width(&left));
            line.push(Span::styled(" ".repeat(pad), muted));
        }
        line
    }
}

/// One rendered editor frame with cursor coordinates (row within frame lines,
/// column within the frame width).
pub struct EditorFrame {
    pub lines: Vec<Line>,
    pub cursor_row: Option<usize>,
    pub cursor_col: Option<usize>,
    pub _scroll_offset: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorMode;

    fn view() -> AgentView {
        AgentView::new(Theme::builtin("prime", ColorMode::TrueColor))
    }

    #[test]
    fn user_block_has_bg() {
        let mut v = view();
        v.push(TranscriptItem::UserMessage {
            text: "hello there".to_string(),
        });
        let lines = v.render_transcript(40);
        assert!(lines.len() >= 3);
        let bg = v.theme.bg_style(ThemeBg::UserMessageBg).bg;
        assert!(lines[0][0].style.bg == bg);
    }

    #[test]
    fn assistant_markdown() {
        let mut v = view();
        v.push(TranscriptItem::Assistant {
            text: "# Title\n\nbody".to_string(),
        });
        let lines = v.render_transcript(40);
        assert!(lines
            .iter()
            .any(|l| { !l.is_empty() && l[0].content == "Title" }));
    }

    #[test]
    fn tool_panel() {
        let mut v = view();
        v.push(TranscriptItem::ToolCall {
            id: "1".to_string(),
            name: "ipython".to_string(),
            arguments: "{\"code\": \"print(1)\"}".to_string(),
        });
        let lines = v.render_transcript(40);
        assert!(lines
            .iter()
            .any(|l| l.iter().any(|s| s.content.contains("⏺ ipython"))));
    }

    #[test]
    fn editor_frame_separator_and_prompt() {
        let mut v = view();
        v.editor.handle_input("h");
        v.editor.handle_input("i");
        let frame = v.render_editor(40, 24);
        assert_eq!(frame.lines[0][0].content, "─".repeat(40));
        assert_eq!(frame.lines[1][0].content, "> ");
        assert_eq!(frame.lines[1][1].content, "hi");
        assert_eq!(frame.cursor_row, Some(1));
        assert_eq!(frame.cursor_col, Some(4));
    }
}
