//! Tool-call card rendering, the TUI side of the TS `tool-execution.ts` /
//! `tool-panel.ts` / `ipython-cell.ts` / `bash.ts` renderer stack. The card
//! model mirrors the TS component state (args, execution start, partial
//! results, live timing); each tool renders through its own shell
//! (`ipython` self-renders, `bash` and the generic fallback render the
//! `ToolPanel` on the panel background).

pub mod bash;
pub mod generic;
pub mod highlight;
pub mod ipython;
pub mod ipython_details;

use std::time::Instant;

use serde_json::Value;

use crate::chat::Detail;
use crate::theme::{Theme, ThemeColor};
use crate::{Line, Span};

/// One tool call and its execution state (TS `ToolExecutionComponent`
/// state minus the renderer caches).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolCallCard {
    pub id: String,
    pub name: String,
    pub args: Value,
    /// `tool_execution_start` seen (live only; replayed cards infer it from
    /// the result).
    pub started: bool,
    /// When the execution started, when seen live (drives `Took`/`Elapsed`).
    pub started_at: Option<Instant>,
    /// When the final result landed (replay sets start and end together).
    pub ended_at: Option<Instant>,
    /// The result so far; `None` until the first result frame.
    pub result: Option<ToolResultView>,
    /// `result` is a partial streaming frame.
    pub result_partial: bool,
}

/// One (partial or final) tool result (TS `AgentToolResult` view).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolResultView {
    pub content: Vec<Value>,
    pub details: Value,
    pub is_error: bool,
}

impl ToolResultView {
    /// The joined text of the result's text blocks, ANSI stripped, with
    /// hidden image blocks appended as `[Image: ...]` fallbacks
    /// (TS `render-utils.getTextOutput` without dimensions).
    pub fn text_output(&self, show_images: bool) -> String {
        let mut parts: Vec<String> = Vec::new();
        for block in &self.content {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => parts.push(
                    crate::error_summary::normalize_error_details(
                        block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    )
                    .replace('\r', ""),
                ),
                Some("image") if !show_images => {
                    let mime = block.get("mimeType").and_then(Value::as_str);
                    parts.push(image_fallback(mime));
                }
                _ => {}
            }
        }
        parts.join("\n")
    }
}

/// `imageFallback(mimeType)` (no filename, no dimensions in the transcript).
pub fn image_fallback(mime_type: Option<&str>) -> String {
    format!("[Image: [{}]]", mime_type.unwrap_or("image/unknown"))
}

/// The animated working icon glyph (TS `working-icon.ts`).
pub fn working_icon(frame: usize) -> &'static str {
    crate::chat::working_icon_frame(frame)
}

/// Render one tool-call card through its tool shell.
pub fn render_tool_card(
    card: &ToolCallCard,
    frame: usize,
    detail: Detail,
    theme: &Theme,
    width: usize,
) -> Vec<Line> {
    match card.name.as_str() {
        "ipython" => ipython::render(card, frame, detail, theme, width),
        "bash" => bash::render(card, frame, detail, theme, width),
        _ => generic::render(card, frame, detail, theme, width),
    }
}

/// The generic panel status (TS `ToolExecutionComponent.panelStatus`):
/// the last non-partial result settles the card; error wins even while
/// streaming; `running` animates until then.
pub(crate) enum PanelStatus {
    Queued,
    Running,
    Done,
    Error,
}

pub(crate) fn panel_status(card: &ToolCallCard) -> PanelStatus {
    if let Some(result) = &card.result {
        if !card.result_partial {
            return if result.is_error {
                PanelStatus::Error
            } else {
                PanelStatus::Done
            };
        }
        if result.is_error {
            return PanelStatus::Error;
        }
    }
    if card.started {
        PanelStatus::Running
    } else {
        PanelStatus::Queued
    }
}

/// The panel header row: `label · status` (TS `panelHeader`).
pub(crate) fn panel_header(card: &ToolCallCard, frame: usize, theme: &Theme) -> Line {
    use crate::theme::ThemeColor::*;
    let muted = theme.fg_style(Muted);
    let dim = theme.fg_style(Dim);
    let mut header: Line = vec![Span::styled(card.name.clone(), muted)];
    header.push(Span::styled(" \u{00b7} ".to_string(), dim));
    let status: Line = match panel_status(card) {
        PanelStatus::Error => vec![Span::styled("error".to_string(), theme.fg_style(Error))],
        PanelStatus::Done => vec![Span::styled("done".to_string(), theme.fg_style(Success))],
        PanelStatus::Running => vec![Span::styled(
            format!("{} running", working_icon(frame)),
            theme.fg_style(BashMode),
        )],
        PanelStatus::Queued => vec![Span::styled("queued".to_string(), theme.fg_style(Muted))],
    };
    header.extend(status);
    header
}

/// One tool-panel row: content indented by 2, padded to the full width on
/// the panel background (TS `toolPanelLine`).
pub(crate) fn panel_line(content: Line, bg: ratatui::style::Style, width: usize) -> Line {
    let padding = 2usize;
    let content_width = width.saturating_sub(padding * 2).max(1);
    let mut line: Line = vec![Span::styled(" ".repeat(padding), bg)];
    let used: usize = content
        .iter()
        .map(|s| crate::width::str_width(&s.content))
        .sum();
    for span in content {
        line.push(Span::styled(span.content, bg.patch(span.style)));
    }
    if used > content_width {
        return crate::width::truncate_line(&line, width, "");
    }
    line.push(Span::styled(" ".repeat(content_width - used), bg));
    line.push(Span::styled(" ".repeat(padding), bg));
    line
}

/// `formatDuration` for the bash panel: tenths of a second.
pub(crate) fn format_bash_duration(ms: u128) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

/// The bash tool's output byte budget (TS `DEFAULT_MAX_BYTES`), used in the
/// truncation warning when the spill carries no `maxBytes`.
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// `formatSize` (TS `truncate.ts`): `512B`, `50.0KB`, `1.2MB`.
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Image result blocks render their fallback row below the card (the TS
/// `Image` component with `fallbackOnly` renders the placeholder in plain
/// terminals like tmux).
pub(crate) fn image_fallback_rows(result: &Option<ToolResultView>, theme: &Theme) -> Vec<Line> {
    let Some(result) = result else {
        return Vec::new();
    };
    let mut rows: Vec<Line> = Vec::new();
    for block in &result.content {
        if block.get("type").and_then(Value::as_str) != Some("image") {
            continue;
        }
        if block.get("data").and_then(Value::as_str).is_none()
            || block.get("mimeType").and_then(Value::as_str).is_none()
        {
            continue;
        }
        rows.push(vec![Span::styled(
            format!(
                "    \u{2570}\u{2500} {}",
                image_fallback(block.get("mimeType").and_then(Value::as_str))
            ),
            theme.fg_style(ThemeColor::ToolOutput),
        )]);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_status_settles_on_error_even_while_partial() {
        let mut card = ToolCallCard {
            name: "bash".into(),
            started: true,
            ..Default::default()
        };
        card.result = Some(ToolResultView {
            is_error: true,
            ..Default::default()
        });
        card.result_partial = true;
        assert!(matches!(panel_status(&card), PanelStatus::Error));
        card.result_partial = false;
        assert!(matches!(panel_status(&card), PanelStatus::Error));
        card.result = Some(ToolResultView::default());
        assert!(matches!(panel_status(&card), PanelStatus::Done));
        card.result = None;
        assert!(matches!(panel_status(&card), PanelStatus::Running));
        card.started = false;
        assert!(matches!(panel_status(&card), PanelStatus::Queued));
    }
}
