//! Transcript component rendering: status rows, the user-message block,
//! assistant messages (text and thinking), tool-call cards, and the working
//! loader line. Ports the TS chat components' row geometry: `user-message.ts`
//! (Box 2x1 on `userMessageBg`), `assistant-message.ts` block spacers,
//! `ipython-cell.ts` collapsed card line, `tool-panel.ts`, and
//! `loader.ts` (`Loader` + `agent-activity.ts` labels).

use serde_json::Value;

use crate::code_preview::{preview_ipython_code, CodePreviewLanguage};
use crate::theme::{Theme, ThemeBg, ThemeColor};
use crate::width::str_width;
use crate::{Line, Span};
use ratatui::style::Style;

/// How much detail the conversation shows (TS `setChatDetail` cycles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    /// `overview`: thinking hidden, tool output collapsed.
    Overview,
    /// `details`: thinking visible, tool output collapsed.
    Details,
}

/// One rendered chat component.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatEntry {
    /// `showStatus` / `showWarning` rows (startup notices, client notes).
    Status { text: String, warning: bool },
    /// The user's submitted prompt.
    User { text: String },
    /// One assistant message: ordered content blocks.
    Assistant(Box<AssistantMessage>),
    /// One tool call and its execution state.
    Tool(Box<ToolCallCard>),
}

/// An assistant message's visible content (tool calls move to cards).
#[derive(Debug, Clone, PartialEq)]
pub struct AssistantMessage {
    pub blocks: Vec<MessageBlock>,
    /// `toolUse` when the message carried tool calls (drives spacers).
    pub has_tool_calls: bool,
    /// The message is still streaming (an update may replace its blocks).
    pub streaming: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MessageBlock {
    Thinking(String),
    Text(String),
}

/// One tool call rendered as a card (`ipython` gets the cell card; other
/// tools render the generic tool panel).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallCard {
    pub id: String,
    pub name: String,
    pub args: Value,
    /// `tool_execution_start` seen.
    pub started: bool,
    /// Partial (streaming) result; `None` until the first result frame.
    pub result: Option<ToolResultView>,
    pub result_partial: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolResultView {
    pub content: Vec<Value>,
    pub details: Value,
    pub is_error: bool,
}

/// The working loader (TS `Loader`): spinner + activity label.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkingState {
    pub activity: &'static str,
    /// Streaming direction: `true` while tokens flow down.
    pub download: bool,
    pub tokens: u64,
    /// Whole seconds since the loader started (TS `formatWorkingElapsed`).
    pub elapsed_secs: u64,
}

impl WorkingState {
    pub fn label(&self) -> String {
        let mut parts = vec![self.activity.to_string()];
        parts.push(format!("{}s", self.elapsed_secs));
        if self.tokens > 0 {
            parts.push(format!(
                "{} {} tokens",
                if self.download {
                    "\u{2193}"
                } else {
                    "\u{2191}"
                },
                crate::chrome::format_token_count(self.tokens)
            ));
        }
        parts.join(" \u{00b7} ")
    }
}

/// Spinner frames (TS `Loader` DEFAULT_FRAMES).
const LOADER_FRAMES: [&str; 10] = [
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280f}",
];

/// The working pulse icon frames (TS `working-icon.ts`).
pub const WORKING_ICON_FRAMES: [&str; 8] = [
    "\u{25f4}", "\u{25f7}", "\u{25f6}", "\u{25f5}", "\u{25cb}", "\u{25f8}", "\u{25fb}", "\u{25fc}",
];

pub fn working_icon_frame(frame: usize) -> &'static str {
    WORKING_ICON_FRAMES[frame % WORKING_ICON_FRAMES.len()]
}

/// A blank line (`Spacer(1)`).
fn spacer() -> Line {
    Vec::new()
}

/// Pad a rendered line to the full width with a base style.
fn pad_to(line: Line, width: usize, base: Style) -> Line {
    let used: usize = line.iter().map(|s| str_width(&s.content)).sum();
    let mut out = line;
    if used < width {
        out.push(Span::styled(" ".repeat(width - used), base));
    }
    out
}

/// Render a status text (TS `Text` with paddingX=1, paddingY=0): wrapped at
/// `width - 2`, one leading margin column, padded to the full width.
pub fn render_text_rows(text: &str, style: Style, width: usize) -> Vec<Line> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let content_width = width.saturating_sub(2).max(1);
    let wrapped = crate::width::wrap_text(text, content_width);
    let row_count = wrapped.len();
    let mut out = Vec::new();
    for (index, line) in wrapped.into_iter().enumerate() {
        // The TS Text component prepends the margin outside the styled
        // content: the margin itself keeps the default foreground. Wrapped
        // rows keep the ANSI state open through their trailing padding (the
        // closing reset lands on the final wrapped row), so continuation
        // rows pad with the row style.
        let mut row: Line = vec![Span::raw(" ")];
        let styled: Line = line
            .into_iter()
            .map(|span| Span::styled(span.content, style))
            .collect();
        row.extend(styled);
        let padding_style = if index + 1 < row_count {
            style
        } else {
            Style::default()
        };
        out.push(pad_to(row, width, padding_style));
    }
    if out.is_empty() {
        out.push(vec![Span::styled(" ".repeat(width), Style::default())]);
    }
    out
}

/// The user-message block (TS `UserMessageComponent`: Box(2,1) on
/// `userMessageBg`, markdown inside colored `userMessageText`).
pub fn render_user_block(text: &str, theme: &Theme, width: usize) -> Vec<Line> {
    let bg = theme.bg_style(ThemeBg::UserMessageBg);
    let content_width = width.saturating_sub(4).max(1);
    let body = theme.fg_style(ThemeColor::UserMessageText);
    let md = crate::markdown::MarkdownStyle::from_theme(theme);
    let rendered = crate::markdown::render_markdown(text, content_width, &md);
    let mut rows: Vec<Line> = Vec::new();
    let blank = vec![Span::styled(" ".repeat(width), bg)];
    rows.push(blank.clone());
    if rendered.is_empty() {
        let row = vec![
            Span::styled("  ".to_string(), bg),
            Span::styled("".to_string(), body),
        ];
        rows.push(pad_to(row, width, bg));
    }
    for line in rendered {
        let mut row: Line = vec![Span::styled("  ".to_string(), bg)];
        for span in line {
            // The user block colors everything `userMessageText` on the
            // block background; markdown structure (wrapping) is kept, its
            // own colors are not.
            row.push(Span::styled(span.content, bg.patch(body)));
        }
        rows.push(pad_to(row, width, bg));
    }
    rows.push(blank);
    rows
}

/// One assistant message (TS `AssistantMessageComponent`): a leading spacer
/// when a visible body exists, markdown blocks separated by spacers (text in
/// `mdBody`, thinking in `dim`), and a trailing spacer before its tool calls.
pub fn render_assistant(
    message: &AssistantMessage,
    detail: Detail,
    theme: &Theme,
    width: usize,
) -> Vec<Line> {
    let show_thinking = detail != Detail::Overview;
    let visible_blocks: Vec<&MessageBlock> = message
        .blocks
        .iter()
        .filter(|block| match block {
            MessageBlock::Thinking(text) => show_thinking && !text.trim().is_empty(),
            MessageBlock::Text(text) => !text.trim().is_empty(),
        })
        .collect();
    let mut out: Vec<Line> = Vec::new();
    if visible_blocks.is_empty() {
        return out;
    }
    out.push(spacer());
    let md = crate::markdown::MarkdownStyle::from_theme(theme);
    for (index, block) in visible_blocks.iter().enumerate() {
        match block {
            MessageBlock::Text(text) => {
                out.extend(render_markdown_block(text, &md, theme, width));
            }
            MessageBlock::Thinking(text) => {
                out.extend(render_thinking_block(text, theme, width));
                // Thinking adds spacing only when another visible block follows.
                if index + 1 < visible_blocks.len() {
                    out.push(spacer());
                }
            }
        }
    }
    if message.has_tool_calls {
        out.push(spacer());
    }
    out
}

/// Markdown rows with TS margins: rendered at `width - 2`, one leading margin
/// column, padded to the full width.
fn render_markdown_block(
    text: &str,
    md: &crate::markdown::MarkdownStyle,
    theme: &Theme,
    width: usize,
) -> Vec<Line> {
    let content_width = width.saturating_sub(2).max(1);
    let rendered = crate::markdown::render_markdown(text.trim(), content_width, md);
    let base = theme.fg_style(ThemeColor::MdBody);
    let row_count = rendered.len();
    let mut out = Vec::new();
    for (index, line) in rendered.into_iter().enumerate() {
        let mut row: Line = vec![Span::styled(" ".to_string(), Style::default())];
        row.extend(line);
        // Continuation rows keep the open ANSI state through their padding.
        let padding_style = if index + 1 < row_count {
            base
        } else {
            Style::default()
        };
        out.push(pad_to(row, width, padding_style));
    }
    out
}

/// The thinking block: markdown with every style collapsed to `dim`.
fn render_thinking_block(text: &str, theme: &Theme, width: usize) -> Vec<Line> {
    let mut md = crate::markdown::MarkdownStyle::from_theme(theme);
    let dim = theme.fg_style(ThemeColor::Dim);
    md.body = dim;
    md.heading = dim;
    md.link = dim;
    md.link_url = dim;
    md.code = dim;
    md.code_block = dim;
    md.code_block_border = dim;
    md.quote = dim;
    md.quote_border = dim;
    md.hr = dim;
    md.list_bullet = dim;
    let content_width = width.saturating_sub(2).max(1);
    let rendered = crate::markdown::render_markdown(text.trim(), content_width, &md);
    let row_count = rendered.len();
    let mut out = Vec::new();
    for (index, line) in rendered.into_iter().enumerate() {
        // The markdown margin sits outside the styled content (default fg).
        let mut row: Line = vec![Span::raw(" ")];
        row.extend(line);
        // Continuation rows keep the open ANSI state through their padding.
        let padding_style = if index + 1 < row_count {
            dim
        } else {
            Style::default()
        };
        out.push(pad_to(row, width, padding_style));
    }
    out
}

/// The tool-call card: `ipython` renders the cell card, every other tool the
/// generic tool panel (`tool-panel.ts` + fallback preview).
pub fn render_tool_card(
    card: &ToolCallCard,
    frame: usize,
    theme: &Theme,
    width: usize,
) -> Vec<Line> {
    if card.name == "ipython" {
        render_ipython_card(card, frame, theme, width)
    } else {
        render_generic_tool_panel(card, theme, width)
    }
}

/// Card status (`ipython-cell.ts statusKind`).
enum CardStatus {
    Queued,
    Running,
    Done,
    Error,
}

fn card_status(card: &ToolCallCard) -> CardStatus {
    if let Some(result) = &card.result {
        if !card.result_partial {
            return if result.is_error {
                CardStatus::Error
            } else {
                CardStatus::Done
            };
        }
    }
    if card.started || card.result.is_some() {
        CardStatus::Running
    } else {
        CardStatus::Queued
    }
}

/// The `ipython` collapsed card line: marker, language label, code preview,
/// line counts, and duration, joined by dim separators.
fn render_ipython_card(
    card: &ToolCallCard,
    frame: usize,
    theme: &Theme,
    width: usize,
) -> Vec<Line> {
    let code = card
        .args
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let preview = preview_ipython_code(&code);
    let language = match preview.language {
        CodePreviewLanguage::Bash => "bash".to_string(),
        CodePreviewLanguage::Python => "python".to_string(),
    };
    let muted = theme.fg_style(ThemeColor::Muted);
    let dim = theme.fg_style(ThemeColor::Dim);
    let success = theme.fg_style(ThemeColor::Success);
    let error = theme.fg_style(ThemeColor::Error);
    let bash_mode = theme.fg_style(ThemeColor::BashMode);

    let mut parts: Vec<Line> = Vec::new();
    let marker: Line = match card_status(card) {
        CardStatus::Error => vec![Span::styled("\u{2717}".to_string(), error)],
        CardStatus::Done => vec![Span::styled("\u{2713}".to_string(), success)],
        CardStatus::Running => vec![Span::styled(
            working_icon_frame(frame).to_string(),
            bash_mode,
        )],
        CardStatus::Queued => vec![Span::styled("\u{25c7}".to_string(), muted)],
    };
    let marker = {
        let mut lead: Line = marker;
        lead.push(Span::styled(" ".to_string(), Style::default()));
        lead.push(Span::styled(language, muted));
        lead
    };
    parts.push(marker);
    if !preview.text.is_empty() {
        parts.push(vec![Span::styled(preview.text, dim)]);
    } else if !card.started {
        parts.push(vec![Span::styled("waiting for code".to_string(), dim)]);
    }
    if let Some(counts) = line_counts(card, &code) {
        parts.push(vec![Span::styled(counts, dim)]);
    }
    if let Some(duration) = result_duration_ms(card) {
        parts.push(vec![Span::styled(duration, dim)]);
    }
    if let Some(ename) = result_error_name(card) {
        parts.push(vec![Span::styled(ename, error)]);
    }

    let mut row: Line = vec![Span::styled(" ".to_string(), Style::default())];
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            row.push(Span::styled(" \u{00b7} ".to_string(), dim));
        }
        row.extend(part.iter().cloned());
    }
    let used: usize = row.iter().map(|s| str_width(&s.content)).sum();
    if used > width {
        row = crate::width::truncate_line(&row, width, "");
    }
    vec![row]
}

/// `↑in ↓out lines` (TS `lineCounts`): non-empty input lines, output lines
/// from the structured stdout/stderr/result fields.
fn line_counts(card: &ToolCallCard, code: &str) -> Option<String> {
    let body = code;
    let input = body.lines().filter(|line| !line.trim().is_empty()).count();
    let output = match &card.result {
        None => 0,
        Some(result) => {
            let details = &result.details;
            let structured = [
                details.get("stdout"),
                details.get("stderr"),
                details.get("result"),
                details.get("backgroundOutput"),
            ]
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
            let text = if structured.trim().is_empty() {
                result
                    .content
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                structured
            };
            if text.trim().is_empty() {
                0
            } else {
                text.lines().count()
            }
        }
    };
    let mut segments = Vec::new();
    if input > 0 {
        segments.push(format!("\u{2191} {input}"));
    }
    if output > 0 {
        segments.push(format!("\u{2193} {output}"));
    }
    if segments.is_empty() {
        None
    } else {
        Some(format!("{} lines", segments.join(" ")))
    }
}

/// Cell duration (`formatDuration`: `2ms`, `1.2s`).
fn result_duration_ms(card: &ToolCallCard) -> Option<String> {
    let duration = card
        .result
        .as_ref()?
        .details
        .get("durationMs")
        .and_then(Value::as_u64)?;
    if duration < 1_000 {
        Some(format!("{}ms", (duration as f64).round()))
    } else {
        Some(format!("{:.1}s", duration as f64 / 1_000.0))
    }
}

/// The error name shown on a failed cell.
fn result_error_name(card: &ToolCallCard) -> Option<String> {
    let result = card.result.as_ref()?;
    if card.result_partial {
        return None;
    }
    result
        .details
        .get("error")
        .and_then(|error| error.get("ename"))
        .and_then(Value::as_str)
        .or_else(|| result.details.get("errorEname").and_then(Value::as_str))
        .map(str::to_string)
}

/// The generic tool panel (TS `ToolPanel` + fallback preview): a `label ·
/// status` header row and preview body rows on `toolPanelBg`.
fn render_generic_tool_panel(card: &ToolCallCard, theme: &Theme, width: usize) -> Vec<Line> {
    let bg = theme.bg_style(ThemeBg::ToolPanelBg);
    let muted = theme.fg_style(ThemeColor::Muted);
    let dim = theme.fg_style(ThemeColor::Dim);
    let success = theme.fg_style(ThemeColor::Success);
    let error = theme.fg_style(ThemeColor::Error);
    let bash_mode = theme.fg_style(ThemeColor::BashMode);
    let padding = 2usize;
    let content_width = width.saturating_sub(padding * 2).max(1);

    let status: Line = match card_status(card) {
        CardStatus::Error => vec![Span::styled("error".to_string(), error)],
        CardStatus::Done => vec![Span::styled("done".to_string(), success)],
        CardStatus::Running => vec![Span::styled("running".to_string(), bash_mode)],
        CardStatus::Queued => vec![Span::styled("queued".to_string(), muted)],
    };
    let mut header: Line = Vec::new();
    header.push(Span::styled(card.name.clone(), muted));
    header.push(Span::styled(" \u{00b7} ".to_string(), dim));
    header.extend(status);

    let mut lines = vec![panel_line(header, bg, width)];
    // Fallback preview body: the call arguments and any text output.
    let mut body: Vec<Line> = Vec::new();
    let args = serde_json::to_string_pretty(&card.args).unwrap_or_default();
    for row in crate::width::wrap_text(&args, content_width) {
        body.push(row);
    }
    if let Some(result) = &card.result {
        let output = result
            .content
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        for row in crate::width::wrap_text(&output, content_width) {
            body.push(row);
        }
    }
    if !body.is_empty() {
        lines.push(panel_line(Vec::new(), bg, width));
        for row in body {
            let output_style = theme.fg_style(ThemeColor::ToolOutput);
            let styled: Line = row
                .into_iter()
                .map(|span| Span::styled(span.content, output_style))
                .collect();
            lines.push(panel_line(styled, bg, width));
        }
    }
    lines
}

/// One tool-panel row: content indented by 2, padded to the full width on
/// the panel background (TS `toolPanelLine`).
fn panel_line(content: Line, bg: Style, width: usize) -> Line {
    let padding = 2usize;
    let content_width = width.saturating_sub(padding * 2).max(1);
    let mut line: Line = vec![Span::styled(" ".repeat(padding), bg)];
    let used: usize = content.iter().map(|s| str_width(&s.content)).sum();
    for span in content {
        line.push(Span::styled(span.content, bg.patch(span.style)));
    }
    if used > content_width {
        line = crate::width::truncate_line(&line, width, "");
        return line;
    }
    line.push(Span::styled(" ".repeat(content_width - used), bg));
    line.push(Span::styled(" ".repeat(padding), bg));
    line
}

/// The working loader rows (TS `Loader.render`: `["", spinner + message]`).
pub fn render_loader(
    working: &WorkingState,
    frame: usize,
    theme: &Theme,
    width: usize,
) -> Vec<Line> {
    let accent = theme.fg_style(ThemeColor::Accent);
    let muted = theme.fg_style(ThemeColor::Muted);
    let spinner = LOADER_FRAMES[frame % LOADER_FRAMES.len()];
    let message = working.label();
    let mut row: Line = vec![Span::styled(" ".to_string(), Style::default())];
    row.push(Span::styled(spinner.to_string(), accent));
    if !message.is_empty() {
        row.push(Span::styled(" ".to_string(), muted));
        row.push(Span::styled(message, muted));
    }
    vec![spacer(), pad_to(row, width, Style::default())]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorMode, Theme};

    fn theme() -> Theme {
        Theme::builtin("prime", ColorMode::TrueColor)
    }

    #[test]
    fn user_block_renders_box_rows() {
        let rows = render_user_block("Run a quick check.", &theme(), 60);
        assert_eq!(rows.len(), 3);
        let text = rows[1]
            .iter()
            .map(|s| s.content.as_str())
            .collect::<String>();
        assert_eq!(text.trim(), "Run a quick check.");
        assert_eq!(text.len(), 60);
    }

    #[test]
    fn ipython_card_done_line() {
        let card = ToolCallCard {
            id: "toolu_1".into(),
            name: "ipython".into(),
            args: serde_json::json!({ "code": "print('visual parity ok')" }),
            started: true,
            result: Some(ToolResultView {
                content: vec![serde_json::json!({ "type": "text", "text": "visual parity ok" })],
                details: serde_json::json!({ "status": "ok", "durationMs": 2, "stdout": "visual parity ok\n" }),
                is_error: false,
            }),
            result_partial: false,
        };
        let rows = render_tool_card(&card, 0, &theme(), 100);
        let text = rows[0]
            .iter()
            .map(|s| s.content.as_str())
            .collect::<String>();
        assert!(
            text.contains("\u{2713} python \u{00b7} print('visual parity ok') \u{00b7} \u{2191} 1 \u{2193} 1 lines"),
            "got: {text}"
        );
    }

    #[test]
    fn loader_line_shape() {
        let working = WorkingState {
            activity: "Writing",
            download: true,
            tokens: 72,
            elapsed_secs: 1,
        };
        let rows = render_loader(&working, 4, &theme(), 100);
        assert_eq!(rows.len(), 2);
        let text = rows[1]
            .iter()
            .map(|s| s.content.as_str())
            .collect::<String>();
        assert!(text.contains("\u{283c} Writing \u{00b7} 1s \u{00b7} \u{2193} 72 tokens"));
    }
}
