//! Transcript component rendering: status rows, the user-message block,
//! assistant messages (text, thinking, and their error rows), and the
//! working loader line. Ports the TS chat components' row geometry:
//! `user-message.ts` (Box 2x1 on `userMessageBg`), `assistant-message.ts`
//! block spacers, and `loader.ts` (`Loader` + `agent-activity.ts` labels).
//! Tool-call cards live in `crate::tool_card`.

use crate::theme::{Theme, ThemeBg, ThemeColor};
use crate::width::str_width;
use crate::{Line, Span};
use ratatui::style::Style;

/// How much detail the conversation shows (TS `setChatDetail` levels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    /// `overview`: thinking hidden, tool output and edit diffs collapsed.
    Overview,
    /// `details`: thinking visible, edit diffs expanded, tool output collapsed.
    Details,
    /// `all`: thinking visible, edit diffs and tool output expanded.
    All,
}

impl Detail {
    /// The Ctrl+O cycle (TS `toggleToolOutputExpansion`): overview adds
    /// details, details adds the expanded output, all wraps to overview.
    pub fn next(self) -> Self {
        match self {
            Detail::Overview => Detail::Details,
            Detail::Details => Detail::All,
            Detail::All => Detail::Overview,
        }
    }

    /// Thinking blocks render (TS `hideThinkingBlock = detail === "overview"`).
    pub fn show_thinking(self) -> bool {
        !matches!(self, Detail::Overview)
    }

    /// Tool output expands (TS `toolOutputExpanded = detail === "all"`).
    pub fn tool_output_expanded(self) -> bool {
        matches!(self, Detail::All)
    }

    /// Edit diffs expand (TS `editDiffsExpanded = detail !== "overview"`).
    pub fn edit_diffs_expanded(self) -> bool {
        !matches!(self, Detail::Overview)
    }
}

/// One rendered chat component.
#[derive(Debug, Clone, PartialEq)]
/// The style tier of a status row (TS `showStatus`/`showWarning`/`showError`).
pub enum StatusKind {
    /// Muted informational note.
    Info,
    /// Warning highlight.
    Warning,
    /// Error highlight.
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatEntry {
    /// `showStatus` / `showWarning` / `showError` rows (startup notices,
    /// client notes, turn errors).
    Status { text: String, kind: StatusKind },
    /// The user's submitted prompt.
    User { text: String },
    /// A durable session-command echo row (`session_slash_command`):
    /// the command as typed, laid out like a user message.
    SlashCommand { text: String },
    /// A durable session-command outcome row (`session_slash_command_result`).
    SlashCommandResult { content: String },
    /// The compaction summary row (TS `CompactionSummaryMessageComponent`):
    /// `◆ Context compacted` with the summary below.
    CompactionSummary {
        /// The summarizer's summary text.
        summary: String,
        /// The context size before the compaction (the expanded metadata).
        tokens_before: u64,
        /// `/compact <instructions>` focus guidance.
        custom_instructions: Option<String>,
    },
    /// One assistant message: ordered content blocks.
    Assistant(Box<AssistantMessage>),
    /// One tool call and its execution state (rendered by
    /// [`crate::tool_card`]).
    Tool(Box<ToolCallCard>),
    /// One received agent message (TS `AgentMessageComponent`).
    AgentMessage(Box<crate::custom_message::AgentMessageRow>),
    /// One injected prompt row (TS `InjectedPromptMessageComponent`).
    InjectedPrompt(Box<crate::custom_message::InjectedPromptRow>),
    /// One background-shell completion row (TS `ShellCompletionComponent`).
    ShellCompletion(Box<crate::custom_message::ShellCompletionRow>),
    /// One refinement outcome row (TS `RefinementOutcomeMessageComponent`).
    RefinementOutcome(Box<crate::custom_message::RefinementOutcomeRow>),
    /// One generic custom row (TS `CustomMessageComponent` box).
    CustomPanel(Box<crate::custom_message::CustomPanelRow>),
}

// The card types live in `tool_card`; re-exported here because the
// transcript vocabulary (`ChatEntry`) is this module's.
pub use crate::tool_card::{render_tool_card, ToolCallCard, ToolResultView};
// The compaction rows (loader + summary) live in `compaction_row`; same
// re-export rule as the tool cards.
pub use crate::compaction_row::{
    render_compaction_loader, render_compaction_summary, CompactionReason, CompactionState,
};

/// An assistant message's visible content (tool calls move to cards).
#[derive(Debug, Clone, PartialEq)]
pub struct AssistantMessage {
    pub blocks: Vec<MessageBlock>,
    /// `toolUse` when the message carried tool calls (drives spacers).
    pub has_tool_calls: bool,
    /// The message is still streaming (an update may replace its blocks).
    pub streaming: bool,
    /// A failed assistant message's error row (TS renders abort and error
    /// text inside the message component): `aborted` always renders,
    /// `error` only without tool calls (their cards carry the failure).
    pub error: Option<String>,
    /// `stopReason: "aborted"` (drives the tool-call trailing spacer).
    pub aborted: bool,
}

impl AssistantMessage {
    /// TS `AssistantMessageComponent.hasTrailingSpace`: the tool-call
    /// separator renders for visible bodies, aborted messages, and messages
    /// not following tool activity (the same condition `render_assistant`
    /// applies).
    pub fn has_trailing_space(&self, detail: Detail, preceded_by_tool_activity: bool) -> bool {
        let has_visible_content = self.blocks.iter().any(|block| match block {
            MessageBlock::Thinking(text) => detail.show_thinking() && !text.trim().is_empty(),
            MessageBlock::Text(text) => !text.trim().is_empty(),
        });
        self.has_tool_calls && (has_visible_content || self.aborted || !preceded_by_tool_activity)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MessageBlock {
    Thinking(String),
    Text(String),
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
pub(crate) const LOADER_FRAMES: [&str; 10] = [
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
pub(crate) fn pad_to(line: Line, width: usize, base: Style) -> Line {
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
pub fn render_user_block(
    text: &str,
    theme: &Theme,
    code_block_indent: &str,
    width: usize,
) -> Vec<Line> {
    let bg = theme.bg_style(ThemeBg::UserMessageBg);
    let content_width = width.saturating_sub(4).max(1);
    let body = theme.fg_style(ThemeColor::UserMessageText);
    let mut md = crate::markdown::MarkdownStyle::from_theme(theme);
    md.code_block_indent = code_block_indent.to_string();
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
    // Zone markers: `A` on the first block row, `B`/`C` on the last (TS
    // `UserMessageComponent.render`).
    if let Some(first) = rows.first_mut() {
        crate::osc133::mark_start(first);
    }
    if let Some(last) = rows.last_mut() {
        crate::osc133::mark_end(last);
    }
    rows
}

/// One assistant message (TS `AssistantMessageComponent`): a leading spacer
/// when a visible body exists, markdown blocks separated by spacers (text in
/// `mdBody`, thinking in `dim`), and a trailing spacer before its tool calls.
pub fn render_assistant(
    message: &AssistantMessage,
    detail: Detail,
    theme: &Theme,
    code_block_indent: &str,
    width: usize,
    preceded_by_tool_activity: bool,
) -> Vec<Line> {
    let show_thinking = detail.show_thinking();
    let visible_blocks: Vec<&MessageBlock> = message
        .blocks
        .iter()
        .filter(|block| match block {
            MessageBlock::Thinking(text) => show_thinking && !text.trim().is_empty(),
            MessageBlock::Text(text) => !text.trim().is_empty(),
        })
        .collect();
    let has_visible_content = !visible_blocks.is_empty();
    let mut out: Vec<Line> = Vec::new();
    if has_visible_content {
        out.push(spacer());
    }
    let mut md = crate::markdown::MarkdownStyle::from_theme(theme);
    md.code_block_indent = code_block_indent.to_string();
    for (index, block) in visible_blocks.iter().enumerate() {
        match block {
            MessageBlock::Text(text) => {
                out.extend(render_markdown_block(text, &md, width));
            }
            MessageBlock::Thinking(text) => {
                out.extend(render_thinking_block(text, theme, &md, width));
                // Thinking adds spacing only when another visible block follows.
                if index + 1 < visible_blocks.len() {
                    out.push(spacer());
                }
            }
        }
    }
    if let Some(error) = &message.error {
        out.push(spacer());
        out.extend(crate::error_summary::render_collapsible_error(
            error,
            None,
            detail.tool_output_expanded(),
            ThemeColor::Error,
            theme,
            width,
        ));
    }
    // TS `AssistantMessageComponent.hasTrailingSpace`: the tool-call
    // separator renders for visible bodies, aborted messages, and messages
    // not following tool activity.
    if message.has_tool_calls
        && (has_visible_content || message.aborted || !preceded_by_tool_activity)
    {
        out.push(spacer());
    }
    // Zone markers on message bodies without tool calls (TS
    // `AssistantMessageComponent.render`: tool-call messages return
    // unmarked).
    if !message.has_tool_calls {
        if let Some(first) = out.first_mut() {
            crate::osc133::mark_start(first);
        }
        if let Some(last) = out.last_mut() {
            crate::osc133::mark_end(last);
        }
    }
    out
}

/// Markdown rows with TS margins: rendered at `width - 2`, one leading margin
/// column, padded to the full width.
fn render_markdown_block(
    text: &str,
    md: &crate::markdown::MarkdownStyle,
    width: usize,
) -> Vec<Line> {
    let content_width = width.saturating_sub(2).max(1);
    let rendered = crate::markdown::render_markdown(text.trim(), content_width, md);
    let mut out = Vec::new();
    for line in rendered.into_iter() {
        let mut row: Line = vec![Span::styled(" ".to_string(), Style::default())];
        row.extend(line);
        // TS pads every markdown row with unstyled spaces after the row's
        // closing 39m reset (tmux trims them); padding never carries the
        // content style, or a dangling SGR prefix survives the trim on rows
        // whose content style differs from the body color (code rows, blank
        // space rows).
        out.push(pad_to(row, width, Style::default()));
    }
    out
}

/// The thinking block: markdown with every style collapsed to `dim`.
fn render_thinking_block(
    text: &str,
    theme: &Theme,
    md: &crate::markdown::MarkdownStyle,
    width: usize,
) -> Vec<Line> {
    let mut md = md.clone();
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
    let mut out = Vec::new();
    for line in rendered.into_iter() {
        // The markdown margin sits outside the styled content (default fg).
        let mut row: Line = vec![Span::raw(" ")];
        row.extend(line);
        // TS pads with unstyled spaces after the row's closing reset (see
        // render_markdown_block); padding never carries the dim content color.
        out.push(pad_to(row, width, Style::default()));
    }
    out
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

/// An in-flight provider auto-retry (TS `retryLoader` + `CountdownTimer`):
/// replaces the working loader until the retry loop settles.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryState {
    pub attempt: u32,
    pub max_attempts: u32,
    pub ends_at: std::time::Instant,
}

impl RetryState {
    /// Whole seconds left in the countdown (never negative).
    pub fn seconds_left(&self) -> u64 {
        self.ends_at
            .saturating_duration_since(std::time::Instant::now())
            .as_secs()
    }
}

/// The retry loader rows (TS auto_retry_start rendering: muted spinner +
/// `Retrying (attempt/maxAttempts) in <seconds>s...`).
pub fn render_retry(retry: &RetryState, frame: usize, theme: &Theme, width: usize) -> Vec<Line> {
    let accent = theme.fg_style(ThemeColor::Accent);
    let muted = theme.fg_style(ThemeColor::Muted);
    let spinner = LOADER_FRAMES[frame % LOADER_FRAMES.len()];
    let message = format!(
        "Retrying ({}/{}) in {}s...",
        retry.attempt,
        retry.max_attempts,
        retry.seconds_left()
    );
    let mut row: Line = vec![Span::styled(" ".to_string(), Style::default())];
    row.push(Span::styled(spinner.to_string(), accent));
    row.push(Span::styled(" ".to_string(), muted));
    row.push(Span::styled(message, muted));
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
    fn retry_loader_renders_countdown() {
        let retry = RetryState {
            attempt: 1,
            max_attempts: 2,
            ends_at: std::time::Instant::now() + std::time::Duration::from_millis(1500),
        };
        let rows = render_retry(&retry, 0, &theme(), 60);
        let text = rows[1]
            .iter()
            .map(|s| s.content.as_str())
            .collect::<String>();
        assert!(text.contains("Retrying (1/2) in 1s..."), "got: {text}");
    }

    #[test]
    fn detail_cycle_visits_all_three_levels() {
        // TS `toggleToolOutputExpansion`: overview -> details -> all -> overview.
        let mut detail = Detail::Overview;
        assert!(!detail.show_thinking());
        assert!(!detail.tool_output_expanded());
        assert!(!detail.edit_diffs_expanded());
        detail = detail.next();
        assert_eq!(detail, Detail::Details);
        assert!(detail.show_thinking());
        assert!(!detail.tool_output_expanded());
        assert!(detail.edit_diffs_expanded());
        detail = detail.next();
        assert_eq!(detail, Detail::All);
        assert!(detail.show_thinking());
        assert!(detail.tool_output_expanded());
        assert!(detail.edit_diffs_expanded());
        detail = detail.next();
        assert_eq!(detail, Detail::Overview);
    }

    #[test]
    fn user_block_renders_box_rows() {
        let rows = render_user_block("Run a quick check.", &theme(), "  ", 60);
        assert_eq!(rows.len(), 3);
        let text = rows[1]
            .iter()
            .map(|s| s.content.as_str())
            .collect::<String>();
        assert_eq!(text.trim(), "Run a quick check.");
        assert_eq!(text.len(), 60);
    }

    #[test]
    fn user_block_carries_zone_markers() {
        let rows = render_user_block("Run a quick check.", &theme(), "  ", 60);
        // The zone-start sequence leads the first block row; the end and
        // final sequences lead the last block row (TS prepends both).
        assert!(crate::osc133::row_markers(&rows[0]).start);
        assert!(crate::osc133::row_markers(&rows[2]).end);
        let first: String = rows[0].iter().map(|s| s.content.as_str()).collect();
        assert!(first.starts_with(crate::osc133::ZONE_START));
        let last: String = rows[2].iter().map(|s| s.content.as_str()).collect();
        assert!(last.starts_with(crate::osc133::ZONE_END_PREFIX));
        // Markers are zero-width: marked rows still measure full width.
        assert_eq!(crate::width::line_width(&rows[0]), 60);
    }

    #[test]
    fn assistant_markers_skip_tool_call_messages() {
        let plain = AssistantMessage {
            blocks: vec![MessageBlock::Text("Done.".to_string())],
            has_tool_calls: false,
            streaming: false,
            error: None,
            aborted: false,
        };
        let rows = render_assistant(&plain, Detail::Overview, &theme(), "  ", 60, false);
        assert!(crate::osc133::row_markers(&rows[0]).start);
        assert!(crate::osc133::row_markers(rows.last().unwrap()).end);

        let with_tools = AssistantMessage {
            blocks: vec![MessageBlock::Text("Working.".to_string())],
            has_tool_calls: true,
            streaming: false,
            error: None,
            aborted: false,
        };
        let rows = render_assistant(&with_tools, Detail::Overview, &theme(), "  ", 60, false);
        assert_eq!(crate::osc133::row_markers(&rows[0]), Default::default());
    }

    #[test]
    fn code_block_indent_rides_the_render_calls() {
        // `markdown.codeBlockIndent` (TS getCodeBlockIndent ->
        // getMarkdownThemeWithSettings): the settings string flows through
        // render_assistant / render_user_block into every fenced block.
        let message = AssistantMessage {
            blocks: vec![MessageBlock::Text(
                "intro\n\n```\nfn main() {}\n```".to_string(),
            )],
            has_tool_calls: false,
            streaming: false,
            error: None,
            aborted: false,
        };
        let strip_markers = |row: &str| {
            row.replace(crate::osc133::ZONE_END_PREFIX, "")
                .replace(crate::osc133::ZONE_END, "")
                .trim_end()
                .to_string()
        };
        let rows = render_assistant(&message, Detail::Overview, &theme(), "    ", 60, false);
        let flat: Vec<String> = rows
            .iter()
            .map(|line| line.iter().map(|s| s.content.as_str()).collect::<String>())
            .map(|row| strip_markers(&row))
            .collect();
        assert!(
            flat.iter().any(|row| row == "     fn main() {}"),
            "non-default indent applied: {flat:?}"
        );
        // The default (no setting) stays two spaces.
        let rows = render_assistant(&message, Detail::Overview, &theme(), "  ", 60, false);
        let flat: Vec<String> = rows
            .iter()
            .map(|line| line.iter().map(|s| s.content.as_str()).collect::<String>())
            .map(|row| strip_markers(&row))
            .collect();
        assert!(
            flat.iter().any(|row| row == "   fn main() {}"),
            "default indent: {flat:?}"
        );
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
            ..Default::default()
        };
        let rows = render_tool_card(&card, 0, Detail::Overview, &theme(), 100, true);
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
    fn assistant_error_row_and_spacers() {
        let message = AssistantMessage {
            blocks: vec![MessageBlock::Text("Running the checks.".into())],
            has_tool_calls: false,
            streaming: false,
            error: Some("Error: request failed after retries".into()),
            aborted: false,
        };
        let rows = render_assistant(&message, Detail::Overview, &theme(), "  ", 60, false);
        let flat: Vec<String> = rows
            .iter()
            .map(|line| line.iter().map(|s| s.content.as_str()).collect())
            .collect();
        assert!(
            flat[0].starts_with(crate::osc133::ZONE_START),
            "leading spacer carries the OSC-133 start marker: {flat:?}"
        );
        assert!(
            flat.iter().any(|row| row.contains("Error: request failed")),
            "got: {flat:?}"
        );
        // A tool-carrying message keeps its trailing spacer.
        let message = AssistantMessage {
            blocks: vec![MessageBlock::Text("body".into())],
            has_tool_calls: true,
            streaming: false,
            error: None,
            aborted: false,
        };
        let rows = render_assistant(&message, Detail::Overview, &theme(), "  ", 60, true);
        assert_eq!(rows.last().unwrap().len(), 0, "trailing spacer");
        // A tool-only message after tool activity renders no spacers.
        let message = AssistantMessage {
            blocks: Vec::new(),
            has_tool_calls: true,
            streaming: false,
            error: None,
            aborted: false,
        };
        let rows = render_assistant(&message, Detail::Overview, &theme(), "  ", 60, true);
        assert!(rows.is_empty(), "got: {rows:?}");
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
