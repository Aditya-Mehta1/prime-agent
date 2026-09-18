//! Custom-message row rendering: each component's row geometry and theme
//! colors, ported from the TS interactive components (`agent-message.ts`,
//! `injected-prompt-message.ts`, `shell-completion.ts`, `custom-message.ts`;
//! `expandable-event-message.ts` + `refinement-outcome-message.ts` live in
//! the sibling `refinement` module, `compaction-outcome-message.ts` renders
//! through the chat status rows).

use super::{
    AgentMessageRow, CustomPanelRow, InjectedPromptKind, InjectedPromptRow, ShellCompletionRow,
};
use crate::chat::Detail;
use crate::theme::{Theme, ThemeBg, ThemeColor};
use crate::width::{pad_line, str_width, truncate_line, wrap_line, wrap_text};
use crate::{Line, Span};
use ratatui::style::Style;

/// One blank row (`Spacer(1)`).
pub(crate) fn spacer() -> Line {
    Vec::new()
}

/// A `Text(spans, 1, 0)` row set: content wrapped at `width - 2`, one margin
/// column, padded to the full width with the default style.
pub(crate) fn text_rows(spans: Line, width: usize) -> Vec<Line> {
    let content_width = width.saturating_sub(2).max(1);
    let flat: String = spans.iter().map(|s| s.content.as_str()).collect();
    if flat.trim().is_empty() {
        return Vec::new();
    }
    wrap_line(&spans, content_width)
        .into_iter()
        .map(|wrapped| {
            let mut row: Line = vec![Span::raw(" ")];
            row.extend(wrapped);
            pad_line(row, width)
        })
        .collect()
}

/// Markdown rows at `Markdown(text, 1, 0)` geometry (the assistant-block
/// layout): rendered at `width - 2`, one margin column, padded with the
/// default style.
fn markdown_rows(text: &str, body_color: ThemeColor, theme: &Theme, width: usize) -> Vec<Line> {
    let content_width = width.saturating_sub(2).max(1);
    let mut md = crate::markdown::MarkdownStyle::from_theme(theme);
    md.body = theme.fg_style(body_color);
    crate::markdown::render_markdown(text, content_width, &md)
        .into_iter()
        .map(|line| {
            let mut row: Line = vec![Span::raw(" ")];
            row.extend(line);
            pad_line(row, width)
        })
        .collect()
}

/// The received agent-message rows (TS `AgentMessageComponent`): a leading
/// blank (spacing-driven), the summary header, and the `╰─`-guttered body
/// when expanded.
pub(crate) fn render_agent_message(
    row: &AgentMessageRow,
    detail: Detail,
    theme: &Theme,
    width: usize,
    leading: bool,
) -> Vec<Line> {
    let mut out = Vec::new();
    if leading {
        out.push(spacer());
    }
    let accent = theme.fg_style(ThemeColor::Accent);
    let muted = theme.fg_style(ThemeColor::Muted);
    let dim = theme.fg_style(ThemeColor::Dim);
    let header: Line = vec![
        Span::styled("\u{25c6}".to_string(), accent),
        Span::raw(" "),
        Span::styled(super::AGENT_MESSAGE_LABEL.to_string(), muted),
        Span::styled(" \u{b7} ".to_string(), dim),
        Span::styled(row.participant.clone(), dim),
    ];
    out.extend(text_rows(header, width));
    if detail.tool_output_expanded() {
        out.extend(agent_message_body(&row.message, theme, width));
    }
    out
}

/// TS `agentMessageBodyLines`: each source line wraps at `width - 4`, the
/// first rendered line carries the `╰─ ` gutter, the rest three spaces, all
/// in `customMessageText`, truncated to the width.
fn agent_message_body(message: &str, theme: &Theme, width: usize) -> Vec<Line> {
    let safe_width = width.max(1);
    let text_width = safe_width.saturating_sub(4).max(1);
    let body = theme.fg_style(ThemeColor::CustomMessageText);
    let mut lines: Vec<Line> = Vec::new();
    for source in message.split('\n') {
        let wrapped = wrap_text(source, text_width);
        for line in wrapped {
            lines.push(line);
        }
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    let dim = theme.fg_style(ThemeColor::Dim);
    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            // TS: the first rendered line carries the dim `╰─ ` gutter,
            // continuation lines three unstyled spaces.
            let mut row: Line = vec![Span::raw(" ")];
            if index == 0 {
                row.push(Span::styled("\u{2570}\u{2500} ".to_string(), dim));
            } else {
                row.push(Span::raw("   "));
            }
            for span in line {
                row.push(Span::styled(span.content, body));
            }
            truncate_line(&row, safe_width, "")
        })
        .collect()
}

/// One injected-prompt row (TS `InjectedPromptMessageComponent`): a leading
/// blank, then the kind's header when collapsed, or the markdown body when
/// expanded (the kernel-state row stays header-only in both states).
pub(crate) fn render_injected_prompt(
    row: &InjectedPromptRow,
    detail: Detail,
    theme: &Theme,
    width: usize,
) -> Vec<Line> {
    let muted = theme.fg_style(ThemeColor::Muted);
    let dim = theme.fg_style(ThemeColor::Dim);
    let accent = theme.fg_style(ThemeColor::Accent);
    let mut out = vec![spacer()];
    let expanded = detail.tool_output_expanded();
    if expanded && !matches!(row.kind, InjectedPromptKind::KernelRestored { .. }) {
        // TS `InjectedPromptMessageComponent.updateDisplay`: the expanded
        // form shows the markdown body INSTEAD of the header.
        if let Some(body) = &row.body {
            out.extend(markdown_rows(
                body,
                ThemeColor::CustomMessageText,
                theme,
                width,
            ));
        }
        return out;
    }
    let mut header: Line = match &row.kind {
        InjectedPromptKind::Heartbeat { schedule } => vec![
            Span::styled("\u{2665}".to_string(), theme.fg_style(ThemeColor::Error)),
            Span::raw(" "),
            Span::styled("Heartbeat prompt".to_string(), muted),
            Span::styled(" \u{b7} ".to_string(), dim),
            Span::styled(heartbeat_schedule(schedule), muted),
        ],
        InjectedPromptKind::Goal { kind, objective } => {
            let mut spans: Line = vec![Span::styled(goal_label(kind.as_deref()), muted)];
            if let Some(objective) = objective {
                spans.push(Span::styled(goal_meta(objective), muted));
            }
            spans
        }
        InjectedPromptKind::KernelRestored { restored } => vec![
            Span::styled("\u{25c6}".to_string(), accent),
            Span::raw(" "),
            Span::styled(
                if *restored {
                    "Restored Python kernel state"
                } else {
                    "Started fresh Python kernel"
                }
                .to_string(),
                muted,
            ),
        ],
        InjectedPromptKind::RlmChildStatus => {
            vec![Span::styled("RLM child status".to_string(), muted)]
        }
    };
    // TS `headerText`: the collapsed heartbeat/goal/RLM rows append the
    // dim expand hint (`expandCollapseHint("app.tools.expand")` renders
    // empty, leaving the dim-colored space).
    if !expanded && !matches!(row.kind, InjectedPromptKind::KernelRestored { .. }) {
        header.push(Span::styled(" ".to_string(), dim));
    }
    out.extend(text_rows(header, width));
    out
}

/// TS `heartbeatPromptSchedule` over `compactHeartbeatSchedule`: a blank
/// schedule shows as `scheduled` (the `prompt` compact form), every other
/// expression as `every <expression>` (a leading case-insensitive `every`
/// plus whitespace stripped from the stored expression first).
fn heartbeat_schedule(schedule: &Option<String>) -> String {
    let trimmed = schedule.as_deref().map(str::trim).unwrap_or("");
    let compact = if trimmed.is_empty() {
        "prompt"
    } else if trimmed.get(..5).is_some_and(|prefix| {
        prefix.eq_ignore_ascii_case("every")
            && trimmed[5..].chars().next().is_some_and(char::is_whitespace)
    }) {
        trimmed[5..].trim_start()
    } else {
        trimmed
    };
    if compact == "prompt" {
        "scheduled".to_string()
    } else {
        format!("every {compact}")
    }
}

/// TS `goalLabel`.
fn goal_label(kind: Option<&str>) -> String {
    match kind {
        Some("continuation") => "Goal continuation",
        Some("budget_limit") => "Goal budget limit",
        Some("objective_updated") => "Goal updated",
        _ => "Goal context",
    }
    .to_string()
}

/// TS `metaText`: ` · <collapsed objective>` truncated to the TS budget
/// (`max(20, 90 - width("Goal continuation · "))` = 70) with the default
/// `...` ellipsis.
fn goal_meta(objective: &str) -> String {
    let collapsed: String = objective.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(" \u{b7} {}", truncate_text(&collapsed, 70))
}

/// Plain-text truncate with the TS default `...` ellipsis
/// (`truncateToWidth` over unstyled text).
fn truncate_text(text: &str, width: usize) -> String {
    let line: Line = vec![Span::raw(text.to_string())];
    let truncated = truncate_line(&line, width, "...");
    truncated
        .iter()
        .map(|s| s.content.as_str())
        .collect::<String>()
}

/// One shell-completion row (TS `ShellCompletionComponent`, standalone form).
pub(crate) fn render_shell_completion(
    row: &ShellCompletionRow,
    detail: Detail,
    theme: &Theme,
    width: usize,
    leading: bool,
) -> Vec<Line> {
    let failed = matches!(row.exit_code, Some(code) if code != 0);
    let color = if failed {
        theme.fg_style(ThemeColor::Error)
    } else {
        theme.fg_style(ThemeColor::Muted)
    };
    let label = if let (Some(code), true) = (row.exit_code, failed) {
        format!("Background shell command failed \u{b7} exit {code}")
    } else {
        "Background shell command finished".to_string()
    };
    let mark = if failed { "\u{2717}" } else { "\u{2713}" };
    let header = truncate_line(
        &vec![Span::styled(format!(" {mark} {label}"), color)],
        width,
        "",
    );
    let mut out = Vec::new();
    if leading {
        out.push(spacer());
    }
    out.push(header);
    if detail.tool_output_expanded() {
        // TS `Text(raw, 1, 0)`: one row per input line at the content width;
        // an empty line keeps its margins-only row.
        for line in row.content.split('\n') {
            if line.trim().is_empty() {
                out.push(vec![Span::raw(" ".repeat(width))]);
            } else {
                out.extend(text_rows(vec![Span::raw(line.to_string())], width));
            }
        }
    }
    out
}

/// Pad a rendered line to the full width with a base style (TS
/// `theme.bg` over `padToWidth`).
pub(crate) fn pad_with(mut line: Line, width: usize, base: Style) -> Line {
    let used: usize = line.iter().map(|s| str_width(&s.content)).sum();
    if used < width {
        line.push(Span::styled(" ".repeat(width - used), base));
    }
    line
}

/// One generic custom row (TS `CustomMessageComponent`): a leading blank,
/// then a `Box(1,1)` on `customMessageBg` holding the bold `[<customType>]`
/// label in `customMessageLabel` and the markdown body in
/// `customMessageText`.
pub(crate) fn render_custom_panel(row: &CustomPanelRow, theme: &Theme, width: usize) -> Vec<Line> {
    let bg = theme.bg_style(ThemeBg::CustomMessageBg);
    let blank = vec![Span::styled(" ".repeat(width), bg)];
    let mut out = vec![spacer(), blank.clone()];
    let content_width = width.saturating_sub(2).max(1);
    let label = Span::styled(
        format!("[{}]", row.custom_type),
        theme
            .fg_style(ThemeColor::CustomMessageLabel)
            .add_modifier(ratatui::style::Modifier::BOLD),
    );
    out.push(box_row(vec![label], bg, width));
    // The box's internal `Spacer(1)`: one blank surface row.
    out.push(blank.clone());
    if !row.content.trim().is_empty() {
        let mut md = crate::markdown::MarkdownStyle::from_theme(theme);
        md.body = theme.fg_style(ThemeColor::CustomMessageText);
        for line in crate::markdown::render_markdown(&row.content, content_width, &md) {
            out.push(box_row(line, bg, width));
        }
    }
    out.push(blank);
    out
}

/// One box content row: left padding column, content spans (bg-patched),
/// padded to the full width on the box background (TS `Box.applyBg` covers
/// the whole row, trailing padding included).
fn box_row(spans: Line, bg: Style, width: usize) -> Line {
    let mut row: Line = vec![Span::styled(" ".to_string(), bg)];
    for span in spans {
        row.push(Span::styled(span.content, span.style.patch(bg)));
    }
    pad_with(row, width, bg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::Detail;
    use crate::theme::{ColorMode, Theme};
    use crate::Span;

    fn theme() -> Theme {
        Theme::builtin("prime", ColorMode::TrueColor)
    }

    fn flat(row: &Line) -> String {
        row.iter().map(|s| s.content.as_str()).collect()
    }

    #[test]
    fn agent_message_header_shape() {
        let row = AgentMessageRow {
            participant: "from child model-probe".to_string(),
            message: "ready".to_string(),
        };
        let rows = render_agent_message(&row, Detail::Overview, &theme(), 60, true);
        // Leading blank + the diamond summary line.
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].is_empty());
        let header = flat(&rows[1]);
        assert_eq!(
            header.trim_end(),
            " \u{25c6} Agent message received \u{b7} from child model-probe"
        );
        // Colors: accent diamond, muted label, dim participant and dot.
        let accent = theme().fg_style(ThemeColor::Accent);
        let muted = theme().fg_style(ThemeColor::Muted);
        let dim = theme().fg_style(ThemeColor::Dim);
        assert_eq!(rows[1][0], Span::styled(" ".to_string(), Style::default()));
        assert_eq!(rows[1][1], Span::styled("\u{25c6}".to_string(), accent));
        assert_eq!(
            rows[1][3],
            Span::styled("Agent message received".to_string(), muted)
        );
        assert_eq!(rows[1][4], Span::styled(" \u{b7} ".to_string(), dim));
        assert_eq!(
            rows[1][5],
            Span::styled("from child model-probe".to_string(), dim)
        );
    }

    #[test]
    fn agent_message_body_gutter_when_expanded() {
        let row = AgentMessageRow {
            participant: "from parent root".to_string(),
            message: "line one\nline two".to_string(),
        };
        let rows = render_agent_message(&row, Detail::All, &theme(), 60, false);
        // No leading blank (spacing decided otherwise), header, two body rows.
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(flat(&rows[1]), " \u{2570}\u{2500} line one");
        assert_eq!(flat(&rows[2]), "    line two");
        let dim = theme().fg_style(ThemeColor::Dim);
        let body = theme().fg_style(ThemeColor::CustomMessageText);
        // The first rendered line carries the dim gutter, continuation rows
        // three unstyled spaces, both bodies in `customMessageText`.
        assert_eq!(
            rows[1][1],
            Span::styled("\u{2570}\u{2500} ".to_string(), dim)
        );
        assert_eq!(rows[2][1], Span::raw("   "));
        assert!(rows[1]
            .iter()
            .any(|span| span.content == "line one" && span.style == body));
    }

    #[test]
    fn heartbeat_header_and_schedule_forms() {
        assert_eq!(
            heartbeat_schedule(&Some("every 10m".to_string())),
            "every 10m"
        );
        assert_eq!(heartbeat_schedule(&Some("10m".to_string())), "every 10m");
        // TS `/^every\s+/i`: case-insensitive with any whitespace run;
        // `every` without whitespace stays part of the expression.
        assert_eq!(
            heartbeat_schedule(&Some("EVERY  10m".to_string())),
            "every 10m"
        );
        assert_eq!(
            heartbeat_schedule(&Some("every10m".to_string())),
            "every every10m"
        );
        assert_eq!(heartbeat_schedule(&Some("prompt".to_string())), "scheduled");
        assert_eq!(heartbeat_schedule(&Some("  ".to_string())), "scheduled");
        assert_eq!(heartbeat_schedule(&None), "scheduled");
        let row = InjectedPromptRow {
            kind: InjectedPromptKind::Heartbeat {
                schedule: Some("every 10m".to_string()),
            },
            body: Some("nudge".to_string()),
        };
        let rows = render_injected_prompt(&row, Detail::Overview, &theme(), 60);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].is_empty());
        assert_eq!(
            flat(&rows[1]).trim_end(),
            " \u{2665} Heartbeat prompt \u{b7} every 10m"
        );
        assert_eq!(
            rows[1][1],
            Span::styled("\u{2665}".to_string(), theme().fg_style(ThemeColor::Error))
        );
    }

    #[test]
    fn goal_header_label_and_meta() {
        let row = InjectedPromptRow {
            kind: InjectedPromptKind::Goal {
                kind: Some("continuation".to_string()),
                objective: Some("ship it today".to_string()),
            },
            body: Some("continue".to_string()),
        };
        let rows = render_injected_prompt(&row, Detail::Overview, &theme(), 60);
        assert_eq!(
            flat(&rows[1]).trim_end(),
            " Goal continuation \u{b7} ship it today"
        );
        // Budget-limit and objective-update kinds carry their own labels.
        for (kind, label) in [
            ("budget_limit", "Goal budget limit"),
            ("objective_updated", "Goal updated"),
            ("other", "Goal context"),
        ] {
            let row = InjectedPromptRow {
                kind: InjectedPromptKind::Goal {
                    kind: Some(kind.to_string()),
                    objective: None,
                },
                body: None,
            };
            let rows = render_injected_prompt(&row, Detail::Overview, &theme(), 60);
            assert_eq!(flat(&rows[1]).trim_end(), format!(" {label}"));
        }
        // A long objective truncates to the TS budget with the default
        // `...` ellipsis, after whitespace collapsing. TS `metaText`
        // truncates only the objective (70 columns); the rendered row
        // adds the 1-column inset plus the 20-column
        // `Goal continuation \u{b7} ` prefix for a 91-wide line.
        let objective = format!("{} tail", "word ".repeat(15));
        let row = InjectedPromptRow {
            kind: InjectedPromptKind::Goal {
                kind: Some("continuation".to_string()),
                objective: Some(objective),
            },
            body: None,
        };
        let rows = render_injected_prompt(&row, Detail::Overview, &theme(), 120);
        let rendered = flat(&rows[1]);
        let meta = rendered.trim_end();
        assert!(meta.ends_with("..."), "ellipsized meta: {meta:?}");
        let visible: String = meta.trim_end_matches('.').to_string();
        let preview = visible.trim_end();
        assert_eq!(str_width(preview) + 3, 91, "meta {meta:?}");
        // The truncated objective alone stays within the TS budget.
        let prefix = " Goal continuation \u{b7} ";
        assert_eq!(
            str_width(preview.trim_start_matches(prefix)) + 3,
            70,
            "objective {meta:?}"
        );
    }

    #[test]
    fn kernel_state_labels() {
        for (restored, label) in [
            (true, "Restored Python kernel state"),
            (false, "Started fresh Python kernel"),
        ] {
            let row = InjectedPromptRow {
                kind: InjectedPromptKind::KernelRestored { restored },
                body: None,
            };
            let rows = render_injected_prompt(&row, Detail::All, &theme(), 60);
            // Header only, no body even expanded.
            assert_eq!(rows.len(), 2, "{rows:?}");
            assert_eq!(flat(&rows[1]).trim_end(), format!(" \u{25c6} {label}"));
        }
    }

    #[test]
    fn rlm_child_status_header() {
        let row = InjectedPromptRow {
            kind: InjectedPromptKind::RlmChildStatus,
            body: Some("[child-failed child:lane]\n\nboom".to_string()),
        };
        let rows = render_injected_prompt(&row, Detail::Overview, &theme(), 60);
        assert_eq!(flat(&rows[1]).trim_end(), " RLM child status");
        // No diamond on this row.
        assert!(!flat(&rows[1]).contains('\u{25c6}'));
        let expanded = render_injected_prompt(&row, Detail::All, &theme(), 60);
        assert!(expanded.len() > 2, "body renders expanded: {expanded:?}");
    }

    #[test]
    fn shell_completion_rows() {
        let ok = ShellCompletionRow {
            pid: Some(4371),
            exit_code: Some(0),
            content: "[bash-done pid:4371 exit:0]".to_string(),
        };
        let rows = render_shell_completion(&ok, Detail::Overview, &theme(), 60, true);
        assert!(rows[0].is_empty());
        assert_eq!(
            flat(&rows[1]),
            " \u{2713} Background shell command finished"
        );
        assert_eq!(
            rows[1][0].style,
            theme().fg_style(ThemeColor::Muted),
            "muted when exit 0"
        );
        let failed = ShellCompletionRow {
            pid: Some(11),
            exit_code: Some(2),
            content: "[bash-done pid:11 exit:2]".to_string(),
        };
        let rows = render_shell_completion(&failed, Detail::Overview, &theme(), 60, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            flat(&rows[0]),
            " \u{2717} Background shell command failed \u{b7} exit 2"
        );
        assert_eq!(
            rows[0][0].style,
            theme().fg_style(ThemeColor::Error),
            "error when failed"
        );
    }

    #[test]
    fn custom_panel_box_shape() {
        let row = CustomPanelRow {
            custom_type: "autonomous_status".to_string(),
            content: "[autonomous-status: on]".to_string(),
        };
        let rows = render_custom_panel(&row, &theme(), 40);
        // Blank, bg row, label row, blank, content row, bg row.
        assert_eq!(rows.len(), 6, "{rows:?}");
        assert!(rows[0].is_empty());
        let bg = theme().bg_style(ThemeBg::CustomMessageBg);
        assert_eq!(rows[1][0].style, bg);
        let label_row = flat(&rows[2]);
        assert_eq!(label_row.trim_end(), " [autonomous_status]");
        assert!(
            rows[2][0].style.bg.is_some(),
            "label row carries the box bg"
        );
        assert!(rows[2][1]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
        assert_eq!(
            rows[2][1].style.fg,
            theme().fg_style(ThemeColor::CustomMessageLabel).fg
        );
        assert_eq!(flat(&rows[4]).trim_end(), " [autonomous-status: on]");
        assert_eq!(
            rows[4][1].style.fg,
            theme().fg_style(ThemeColor::CustomMessageText).fg
        );
    }
}
