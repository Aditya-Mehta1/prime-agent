//! The side-question pane (TS `SideQuestionComponent`): the `/btw`
//! conversation mounted above the prompt dock. Turns render in the popup
//! surface (tool panel background); the first turn keeps its `/btw`
//! header, follow-ups render as user-message bubbles, and local notices
//! (slash-command and image rejections) render as complete turns that
//! never reach the daemon and never seed follow-ups.

use crate::theme::{Theme, ThemeBg, ThemeColor};
use crate::width::{str_width, wrap_text};

/// One pane turn (TS `AgentConnectionSideQuestionEvent`, plus the local
/// notices the interactive mode adds the same way).
#[derive(Debug, Clone, PartialEq)]
pub struct SideQuestionTurn {
    pub id: String,
    pub question: String,
    pub answer: String,
    /// `running` | `complete` | `cancelled` | `error`.
    pub status: String,
    pub error_message: Option<String>,
    /// A client-local notice (slash-command or image rejection): rendered
    /// like a turn, but never seeds a follow-up's transcript.
    pub local: bool,
}

/// Whether the turn can seed a follow-up side question (TS `sideQuestionTurns`
/// collects answered turns; local notices never join it).
pub fn turn_seeds_follow_up(turn: &SideQuestionTurn) -> bool {
    !turn.local && !turn.answer.is_empty()
}

/// The pane: the turns in order, plus the expansion flag the detail cycle
/// toggles (TS `setExpanded`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SideQuestionPane {
    pub turns: Vec<SideQuestionTurn>,
    pub expanded: bool,
}

/// The pane's horizontal padding (TS `paddingX = max(2, editorPaddingX)`;
/// this surface's editor padding is the default two columns).
const PADDING_X: usize = 2;

impl SideQuestionPane {
    /// The turn the follow-up seeds its context with (TS
    /// `sideQuestionTurns.filter(turn => turn.answer)`).
    pub fn seed_turns(&self) -> Vec<(String, String)> {
        self.turns
            .iter()
            .filter(|turn| turn_seeds_follow_up(turn))
            .map(|turn| (turn.question.clone(), turn.answer.clone()))
            .collect()
    }

    /// Whether any turn or bash run is still running (the hint row's
    /// condition; a completed notice can sit below a running turn).
    pub fn running(&self) -> bool {
        self.turns.iter().any(|turn| turn.status == "running")
    }

    /// Upsert a streamed event into its turn (TS `update`).
    pub fn upsert(&mut self, turn: SideQuestionTurn) {
        match self
            .turns
            .iter_mut()
            .find(|existing| existing.id == turn.id)
        {
            Some(existing) => *existing = turn,
            None => self.turns.push(turn),
        }
    }

    /// The running turn the escape key cancels (TS `sideQuestionEvent` —
    /// the latest turn the pane tracks).
    pub fn active_turn(&self) -> Option<&SideQuestionTurn> {
        self.turns
            .iter()
            .rev()
            .find(|turn| turn.status == "running" && !turn.local)
    }

    /// Render the pane (TS `render`): blank surfaced row, the turns, and
    /// the dim hint row, every row painted with the popup background and
    /// padded to the full width.
    pub fn render(&self, theme: &Theme, width: usize) -> Vec<crate::Line> {
        let bg = theme.bg_style(ThemeBg::ToolPanelBg);
        let user_text = theme.fg_style(ThemeColor::UserMessageText);
        let accent = theme.fg_style(ThemeColor::Accent);
        let dim = theme.fg_style(ThemeColor::Dim);
        let error = theme.fg_style(ThemeColor::Error);
        let blank = || vec![crate::Span::styled(" ".repeat(width.max(1)), bg)];
        let surface = |line: crate::Line| -> crate::Line {
            let used: usize = line.iter().map(|span| str_width(&span.content)).sum();
            let mut line = line;
            for span in &mut line {
                span.style = span.style.patch(bg);
            }
            line.push(crate::Span::styled(
                " ".repeat(width.saturating_sub(used)),
                bg,
            ));
            line
        };
        let mut rows: Vec<crate::Line> = vec![blank()];
        for (index, turn) in self.turns.iter().enumerate() {
            if index > 0 {
                // Follow-ups and notices render as standard user-message
                // bubbles (TS `questionBubble`): the Box(2,1) surface with
                // the question wrapped on it in the user-message text color.
                rows.extend(render_bubble(&turn.question, theme, width));
            } else {
                // The first turn keeps the `/btw` header (TS `Text` with
                // the accent command segment, two spaces, the question).
                let mut line: crate::Line = Vec::new();
                line.push(crate::Span::styled(" ".repeat(PADDING_X), bg));
                line.push(crate::Span::styled("/btw".to_string(), accent));
                line.push(crate::Span::styled("  ".to_string(), bg));
                line.push(crate::Span::styled(turn.question.clone(), user_text));
                for wrapped in wrap_row(&line, width) {
                    rows.push(surface(wrapped));
                }
            }
            rows.push(blank());
            // The answer area: the markdown answer, the error line under
            // partial output, or the placeholder states.
            let style = crate::markdown::MarkdownStyle::from_theme(theme);
            let content_width = width.saturating_sub(PADDING_X).max(1);
            let mut rendered = if turn.answer.is_empty() {
                Vec::new()
            } else {
                crate::markdown::render_markdown(&turn.answer, content_width, &style)
            };
            if let Some(message) = &turn.error_message {
                // TS `renderAnswer`: the error row is a single-paddingX
                // `Text` row; the `padded` prefix below supplies the pad.
                rendered.push(vec![crate::Span::styled(message.clone(), error)]);
            }
            if rendered.is_empty() {
                // The placeholder rows (`Cancelled`/`No response`/
                // `Thinking…`) are single-paddingX `Text` rows too (TS
                // renders each with `new Text(..., this.paddingX, 0)`).
                let text = match turn.status.as_str() {
                    "cancelled" => "Cancelled".to_string(),
                    "complete" => "No response".to_string(),
                    _ => "Thinking…".to_string(),
                };
                rendered.push(vec![crate::Span::styled(text, user_text)]);
            }
            for line in rendered {
                let padded: crate::Line =
                    std::iter::once(crate::Span::styled(" ".repeat(PADDING_X), bg))
                        .chain(line)
                        .collect();
                for wrapped in wrap_row(&padded, width) {
                    rows.push(surface(wrapped));
                }
            }
            rows.push(blank());
        }
        // The hint row (TS `renderHint`): any running turn swaps the
        // affordance to the cancel hint.
        let hint = if self.running() {
            "esc to cancel and return to session"
        } else {
            "reply to follow up · esc to return to session"
        };
        rows.push(surface(vec![
            crate::Span::styled(" ".repeat(PADDING_X), bg),
            crate::Span::styled(hint.to_string(), dim),
        ]));
        rows.push(blank());
        rows
    }
}

/// Wrap one rendered row to the width, keeping the bg style on the tail
/// (the markdown renderer wraps its own lines; this re-wraps the padded
/// row when the terminal is narrower than the rendered content).
fn wrap_row(line: &crate::Line, width: usize) -> Vec<crate::Line> {
    let used: usize = line.iter().map(|span| str_width(&span.content)).sum();
    if used <= width || width == 0 {
        return vec![line.clone()];
    }
    let plain: String = line.iter().map(|span| span.content.as_str()).collect();
    let wrapped = wrap_text(&plain, width);
    let style = line
        .iter()
        .map(|span| span.style)
        .reduce(ratatui::style::Style::patch)
        .unwrap_or_default();
    wrapped
        .into_iter()
        .map(|segments| {
            segments
                .into_iter()
                .map(|span| crate::Span {
                    content: span.content,
                    style,
                })
                .collect()
        })
        .collect()
}

/// The follow-up bubble (TS `Box(paddingX, 1)` with the user-message
/// background): blank surface row, wrapped question rows, blank surface
/// row, every row padded to the full width on the block background.
fn render_bubble(text: &str, theme: &Theme, width: usize) -> Vec<crate::Line> {
    let bg = theme.bg_style(ThemeBg::UserMessageBg);
    let text_style = theme.fg_style(ThemeColor::UserMessageText);
    let content_width = width.saturating_sub(PADDING_X * 2).max(1);
    let mut rows: Vec<crate::Line> = vec![vec![crate::Span::styled(" ".repeat(width.max(1)), bg)]];
    let wrapped = wrap_text(text, content_width);
    if wrapped.is_empty() {
        rows.push(vec![crate::Span::styled(" ".repeat(width.max(1)), bg)]);
    }
    for line in wrapped {
        let mut row: crate::Line = vec![crate::Span::styled(" ".repeat(PADDING_X), bg)];
        let mut segments = line;
        for span in &mut segments {
            span.style = span.style.patch(text_style);
        }
        row.extend(segments);
        let used: usize = row.iter().map(|span| str_width(&span.content)).sum();
        row.push(crate::Span::styled(
            " ".repeat(width.saturating_sub(used)),
            bg,
        ));
        rows.push(row);
    }
    rows.push(vec![crate::Span::styled(" ".repeat(width.max(1)), bg)]);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(id: &str, status: &str, answer: &str) -> SideQuestionTurn {
        SideQuestionTurn {
            id: id.to_string(),
            question: format!("question {id}"),
            answer: answer.to_string(),
            status: status.to_string(),
            error_message: None,
            local: false,
        }
    }

    #[test]
    fn local_notices_never_seed_follow_ups() {
        let mut pane = SideQuestionPane::default();
        pane.upsert(turn("a", "complete", "answered"));
        pane.turns.push(SideQuestionTurn {
            id: "side-notice-1".to_string(),
            question: "/tree".to_string(),
            answer: "Slash commands are not available...".to_string(),
            status: "complete".to_string(),
            error_message: None,
            local: true,
        });
        assert_eq!(
            pane.seed_turns(),
            vec![("question a".into(), "answered".into())]
        );
    }

    #[test]
    fn running_hint_follows_any_running_turn() {
        let mut pane = SideQuestionPane::default();
        pane.upsert(turn("a", "complete", "done"));
        assert!(!pane.running());
        pane.upsert(turn("b", "running", ""));
        assert!(pane.running());
        assert_eq!(pane.active_turn().unwrap().id, "b");
    }

    #[test]
    fn render_places_the_btw_header_then_answer_then_hint() {
        let theme = crate::theme::Theme::builtin("prime", crate::theme::ColorMode::Color256);
        let mut pane = SideQuestionPane::default();
        pane.upsert(turn("a", "complete", "the answer"));
        let rows = pane.render(&theme, 80);
        let text =
            |line: &crate::Line| -> String { line.iter().map(|s| s.content.as_str()).collect() };
        let joined: Vec<String> = rows.iter().map(&text).collect();
        let first = joined
            .iter()
            .find(|row| row.contains("/btw"))
            .expect("the /btw header row");
        assert!(first.contains("question a"));
        assert!(joined.iter().any(|row| row.contains("the answer")));
        assert!(joined
            .iter()
            .any(|row| row.contains("reply to follow up · esc to return to session")));
        // A running turn swaps the hint.
        pane.upsert(turn("b", "running", ""));
        let rows = pane.render(&theme, 80);
        let joined: Vec<String> = rows.iter().map(&text).collect();
        assert!(joined
            .iter()
            .any(|row| row.contains("esc to cancel and return to session")));
        // The cancelled placeholder shows when no answer streamed.
        pane.upsert(SideQuestionTurn {
            status: "cancelled".to_string(),
            answer: String::new(),
            ..turn("b", "cancelled", "")
        });
        let rows = pane.render(&theme, 80);
        let joined: Vec<String> = rows.iter().map(&text).collect();
        assert!(joined.iter().any(|row| row.contains("Cancelled")));
    }
}
