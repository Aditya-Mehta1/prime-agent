//! Prompt-highlight tokens (TS `prompt-highlight.ts`): the accent color on
//! a leading slash-command segment and the `success`/`mdLink` colors on
//! `@path` / `--flag` argument tokens, applied exactly where the TS product
//! applies them:
//!
//! - the queued-message preview strip (`styleQueuedMessagePreview`): dim base, accent on the recognized command's `/name`, arg tokens colored;
//! - the live editor's styled display text (`CustomEditor.styleDisplayText` + `ArgTokenHighlighter`): arg tokens colored on every line, the command token of the first layout line in accent (argument-taking commands only, suppressed while the cursor sits inside it).
//!
//! The TS surface also covers user-message transcript rendering (`PromptTokenMask`); that markdown-layout masking is not ported here.

use crate::theme::{Theme, ThemeColor};
use crate::{Line, Span};
use pa_types::slash_commands::{parse_slash_command, SlashCommandRegistry};
use ratatui::style::{Modifier, Style};
use std::sync::OnceLock;

/// One highlighted argument token (TS `ArgTokenSpan`): a half-open char
/// range over its source text plus the token's theme color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgTokenSpan {
    /// First char of the token.
    pub start: usize,
    /// One past the token's last char.
    pub end: usize,
    /// `success` for `@`-tokens, `mdLink` for flags (TS `tokenColor`).
    pub color: ThemeColor,
}

/// TS `ARG_TOKEN_PATTERN`, or `ARG_TOKEN_PATTERN_WITH_SEPARATOR` when the
/// command line takes arguments (a bare `--` end-of-options separator also
/// highlights).
fn arg_token_pattern(include_bare_separator: bool) -> &'static fancy_regex::Regex {
    static PLAIN: OnceLock<fancy_regex::Regex> = OnceLock::new();
    static WITH_SEPARATOR: OnceLock<fancy_regex::Regex> = OnceLock::new();
    if include_bare_separator {
        WITH_SEPARATOR.get_or_init(|| {
            fancy_regex::Regex::new(
                r#"@"[^"\n]*"|@(?:\\[^\s\x1b]|[^\s\x1b|])+|--[A-Za-z0-9][A-Za-z0-9-]*|--(?=\s|$)"#,
            )
            .expect("arg-token pattern compiles")
        })
    } else {
        PLAIN.get_or_init(|| {
            fancy_regex::Regex::new(
                r#"@"[^"\n]*"|@(?:\\[^\s\x1b]|[^\s\x1b|])+|--[A-Za-z0-9][A-Za-z0-9-]*"#,
            )
            .expect("arg-token pattern compiles")
        })
    }
}

/// TS `tokenColor`: `@`-tokens are `success`, flags are `mdLink`.
fn token_color(token: &str) -> ThemeColor {
    if token.starts_with('@') {
        ThemeColor::Success
    } else {
        ThemeColor::MdLink
    }
}

/// TS `hasTokenBoundary`: a token must start at index 0 or after whitespace.
fn has_token_boundary(text: &str, byte_start: usize) -> bool {
    byte_start == 0
        || text[..byte_start]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
}

fn char_at_index(text: &str, byte_index: usize) -> usize {
    text[..byte_index].chars().count()
}

fn byte_at_char(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

/// Slice by char range (half-open).
fn char_slice(text: &str, start: usize, end: usize) -> &str {
    &text[byte_at_char(text, start)..byte_at_char(text, end)]
}

/// TS `findArgTokens`: the token spans of `text` at or after `from_index`,
/// each needing a whitespace boundary before it. Char offsets.
pub fn find_arg_tokens(
    text: &str,
    from_index: usize,
    include_bare_separator: bool,
) -> Vec<ArgTokenSpan> {
    let regex = arg_token_pattern(include_bare_separator);
    let mut spans = Vec::new();
    for result in regex.find_iter(text) {
        let Ok(m) = result else { continue };
        if char_at_index(text, m.start()) < from_index || !has_token_boundary(text, m.start()) {
            continue;
        }
        spans.push(ArgTokenSpan {
            start: char_at_index(text, m.start()),
            end: char_at_index(text, m.end()),
            color: token_color(&text[m.start()..m.end()]),
        });
    }
    spans
}

/// TS `styleArgumentTokens`: `text` styled `base` color with its argument
/// tokens in their own colors. Char-offset `from_index` skips tokens that
/// start before it.
pub fn style_argument_tokens(
    theme: &Theme,
    text: &str,
    base: ThemeColor,
    from_index: usize,
    include_bare_separator: bool,
) -> Line {
    let mut line = Vec::new();
    let mut offset = 0usize;
    for token in find_arg_tokens(text, from_index, include_bare_separator) {
        if token.start > offset {
            line.push(theme.fg(base, char_slice(text, offset, token.start)));
        }
        line.push(theme.fg(token.color, char_slice(text, token.start, token.end)));
        offset = token.end;
    }
    if offset < text.chars().count() {
        line.push(theme.fg(base, char_slice(text, offset, text.chars().count())));
    }
    line
}

/// TS `styleQueuedMessagePreview`: the strip preview styling. Plain messages
/// render dim with argument tokens colored; a message led by a recognized
/// builtin slash command renders its `/name` segment in accent and the rest
/// dim (argument tokens still colored). `label` is the lane label
/// [`crate::queued::format_queued_message_preview`] prepends.
///
/// The TS recognition check also admits daemon-registered connection
/// commands; the Rust strip recognizes builtin commands (aliases included).
pub fn style_queued_message_preview(theme: &Theme, message: &str, label: &str) -> Line {
    let registry = SlashCommandRegistry::builtin();
    let preview = crate::queued::format_queued_message_preview(message, label);
    // TS `isLeadingSlashCommand`: a leading `/name` naming a known command.
    let leading = parse_slash_command(message).filter(|(name, _)| registry.is_builtin(name));
    let Some((name, _)) = leading else {
        return style_argument_tokens(theme, &preview, ThemeColor::Dim, 0, false);
    };
    let mut line = Vec::new();
    // The lane-label prefix is dim; the styled message follows it.
    let prefix_end = preview.chars().count() - message.chars().count();
    if prefix_end > 0 {
        line.push(theme.fg(ThemeColor::Dim, char_slice(&preview, 0, prefix_end)));
    }
    // TS `styleSlashCommandText`: accent on `/` plus the typed name; a bare
    // `--` separator highlights only for argument-taking commands.
    let command_end = name.chars().count() + 1;
    line.push(theme.fg(ThemeColor::Accent, char_slice(message, 0, command_end)));
    let include_bare_separator = registry.takes_argument(&name);
    line.extend(style_argument_tokens(
        theme,
        char_slice(message, command_end, message.chars().count()),
        ThemeColor::Dim,
        0,
        include_bare_separator,
    ));
    line
}

/// TS `COMMAND_TOKEN_PATTERN` (`/^(\s*)\/(\S+)/`): the leading `/name` run
/// of an editor line. Char offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandToken {
    /// First char of the `/` (after the leading whitespace).
    pub start: usize,
    /// One past the name run's last char.
    pub end: usize,
    /// The typed name (the non-whitespace run after the `/`).
    pub name: String,
}

/// Match a [`CommandToken`] at the start of `line` (TS
/// `COMMAND_TOKEN_PATTERN.exec`); `None` when the line does not open with
/// (optional whitespace and) a `/name` run.
pub fn command_token(line: &str) -> Option<CommandToken> {
    let leading = line.chars().take_while(|c| c.is_whitespace()).count();
    let mut rest = line.chars().skip(leading);
    if rest.next()? != '/' {
        return None;
    }
    let name: String = rest.take_while(|c| !c.is_whitespace()).collect();
    if name.is_empty() {
        return None;
    }
    Some(CommandToken {
        start: leading,
        end: leading + 1 + name.chars().count(),
        name,
    })
}

/// The highlight ranges of one laid-out editor chunk (TS
/// `ArgTokenHighlighter.highlightLine` + `CustomEditor.styleCommandToken`):
/// the source line's argument tokens clipped to the chunk, plus — when the
/// chunk is the first layout line and opens with an argument-taking command
/// the cursor does not sit inside — the command token in accent. Char
/// offsets over the chunk, in order.
pub fn editor_chunk_highlights(
    chunk: &str,
    line_spans: &[ArgTokenSpan],
    source_start: usize,
    command: Option<&CommandToken>,
    command_takes_argument: bool,
    cursor_col: Option<usize>,
) -> Vec<(usize, usize, ThemeColor)> {
    let chunk_chars = chunk.chars().count();
    let range_end = source_start + chunk_chars;
    let mut out = Vec::new();
    for span in line_spans {
        if span.end <= source_start {
            continue;
        }
        if span.start >= range_end {
            break;
        }
        out.push((
            span.start.max(source_start) - source_start,
            span.end.min(range_end) - source_start,
            span.color,
        ));
    }
    if let Some(command) = command {
        if command_takes_argument && !cursor_col.is_some_and(|cursor| cursor < command.end) {
            out.push((command.start, command.end, ThemeColor::Accent));
        }
    }
    out
}

/// The visible text spans of one editor chunk: highlight-colored runs with
/// the cursor cell reverse-video. The reversed cell carries the highlight
/// color under it (TS `highlightLine` re-wraps the cursor splice inside the
/// token color); the appended end-of-line cursor cell stays default, like
/// the TS `\x1b[7m \x1b[27m` splice beyond the last token.
pub fn editor_text_spans(
    theme: &Theme,
    chunk: &str,
    highlights: &[(usize, usize, ThemeColor)],
    cursor_col: Option<usize>,
    bg: Style,
) -> Vec<Span> {
    let chars: Vec<char> = chunk.chars().collect();
    let len = chars.len();
    let mut boundaries = vec![0usize, len];
    for (start, end, _) in highlights {
        boundaries.push(*start);
        boundaries.push(*end);
    }
    let cursor_on_chunk = cursor_col.filter(|cursor| *cursor <= len);
    if let Some(cursor) = cursor_on_chunk {
        boundaries.push(cursor);
        if cursor < len {
            boundaries.push(cursor + 1);
        }
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut out = Vec::new();
    for pair in boundaries.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        if start >= end {
            continue;
        }
        let mut style = bg;
        if let Some((_, _, color)) = highlights
            .iter()
            .find(|(s, e, _)| *s <= start && start < *e)
        {
            style = bg.patch(theme.fg_style(*color));
        }
        // The cursor covers exactly the one char under it.
        if cursor_on_chunk == Some(start) && end == start + 1 {
            style = style.add_modifier(Modifier::REVERSED);
        }
        out.push(Span::styled(
            chars[start..end].iter().collect::<String>(),
            style,
        ));
    }
    if cursor_on_chunk == Some(len) {
        out.push(Span::styled(
            " ".to_string(),
            bg.add_modifier(Modifier::REVERSED),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorMode;

    fn theme() -> Theme {
        Theme::builtin("prime", ColorMode::TrueColor)
    }

    fn dim() -> Style {
        theme().fg_style(ThemeColor::Dim)
    }

    fn accent() -> Style {
        theme().fg_style(ThemeColor::Accent)
    }

    fn success() -> Style {
        theme().fg_style(ThemeColor::Success)
    }

    fn md_link() -> Style {
        theme().fg_style(ThemeColor::MdLink)
    }

    fn spans(line: &Line) -> Vec<(String, Style)> {
        line.iter()
            .map(|span| (span.content.to_string(), span.style))
            .collect()
    }

    #[test]
    fn arg_tokens_cover_paths_flags_and_separators() {
        let tokens = find_arg_tokens("fix @Cargo.toml --quiet now", 0, false);
        assert_eq!(
            tokens,
            vec![
                ArgTokenSpan {
                    start: 4,
                    end: 15,
                    color: ThemeColor::Success
                },
                ArgTokenSpan {
                    start: 16,
                    end: 23,
                    color: ThemeColor::MdLink
                },
            ]
        );
        // A bare `--` only highlights with the separator pattern.
        assert!(find_arg_tokens("x -- y", 0, false).is_empty());
        assert_eq!(
            find_arg_tokens("x -- y", 0, true),
            vec![ArgTokenSpan {
                start: 2,
                end: 4,
                color: ThemeColor::MdLink
            }]
        );
        // Quoted @-paths keep their spaces.
        assert_eq!(
            find_arg_tokens(r#"a @"my file" b"#, 0, false),
            vec![ArgTokenSpan {
                start: 2,
                end: 12,
                color: ThemeColor::Success
            }]
        );
        // An unterminated quote falls back to the bare-path form.
        assert_eq!(
            find_arg_tokens(r#"a @"open b"#, 0, false),
            vec![ArgTokenSpan {
                start: 2,
                end: 8,
                color: ThemeColor::Success
            }]
        );
        // A backslash escape can carry a non-space char; the token stops at
        // the first whitespace either way.
        assert_eq!(
            find_arg_tokens(r"a @pa\th b", 0, false),
            vec![ArgTokenSpan {
                start: 2,
                end: 8,
                color: ThemeColor::Success
            }]
        );
    }

    #[test]
    fn arg_tokens_need_a_whitespace_boundary() {
        assert!(find_arg_tokens("a@b", 0, false).is_empty());
        assert_eq!(
            find_arg_tokens("a @b", 0, false),
            vec![ArgTokenSpan {
                start: 2,
                end: 4,
                color: ThemeColor::Success
            }]
        );
        // A token starting before `from_index` is skipped even with a
        // boundary (TS filters matches that start early).
        assert_eq!(find_arg_tokens("@a --b", 2, false).len(), 1);
        assert_eq!(
            find_arg_tokens("@a --b", 2, false)[0].start,
            3,
            "the early @-token is skipped, the flag still highlights"
        );
    }

    #[test]
    fn plain_previews_render_dim_with_colored_tokens() {
        let line = style_queued_message_preview(&theme(), "fix @Cargo.toml --quiet", "Follow-up");
        assert_eq!(
            spans(&line),
            vec![
                ("Follow-up: fix ".to_string(), dim()),
                ("@Cargo.toml".to_string(), success()),
                (" ".to_string(), dim()),
                ("--quiet".to_string(), md_link()),
            ]
        );
    }

    #[test]
    fn slash_previews_render_the_command_segment_in_accent() {
        let line = style_queued_message_preview(&theme(), "/new @docs/plan.md --draft", "Steering");
        assert_eq!(
            spans(&line),
            vec![
                ("Steering: ".to_string(), dim()),
                ("/new".to_string(), accent()),
                (" ".to_string(), dim()),
                ("@docs/plan.md".to_string(), success()),
                (" ".to_string(), dim()),
                ("--draft".to_string(), md_link()),
            ]
        );
        // Aliases highlight with their typed name.
        let line = style_queued_message_preview(&theme(), "/clear now", "Follow-up");
        assert_eq!(
            spans(&line),
            vec![
                ("Follow-up: ".to_string(), dim()),
                ("/clear".to_string(), accent()),
                (" now".to_string(), dim()),
            ]
        );
        // Argument-taking commands highlight a bare separator; others do not.
        let line = style_queued_message_preview(&theme(), "/new x -- y", "Steering");
        assert!(spans(&line)
            .iter()
            .any(|(text, style)| { text == "--" && *style == md_link() }));
        let line = style_queued_message_preview(&theme(), "/hotkeys x -- y", "Steering");
        assert!(
            !spans(&line).iter().any(|(text, _)| text == "--"),
            "a no-argument command does not get the separator pattern"
        );
    }

    #[test]
    fn unrecognized_commands_render_uniformly_dim() {
        let line = style_queued_message_preview(&theme(), "/definitely-not-builtin x", "Steering");
        assert_eq!(
            spans(&line),
            vec![("Steering: /definitely-not-builtin x".to_string(), dim())]
        );
        // A labeled internal prompt keeps its own label and stays dim.
        let line =
            style_queued_message_preview(&theme(), "Heartbeat prompt: run @check", "Steering");
        assert_eq!(
            spans(&line),
            vec![
                ("Heartbeat prompt: run ".to_string(), dim()),
                ("@check".to_string(), success()),
            ]
        );
    }

    #[test]
    fn command_token_matches_leading_slash_runs() {
        assert_eq!(
            command_token("  /new foo"),
            Some(CommandToken {
                start: 2,
                end: 6,
                name: "new".to_string()
            })
        );
        assert_eq!(
            command_token("/x"),
            Some(CommandToken {
                start: 0,
                end: 2,
                name: "x".to_string()
            })
        );
        assert_eq!(command_token("a /new"), None);
        assert_eq!(command_token("/"), None);
        assert_eq!(command_token("  / foo"), None);
    }

    #[test]
    fn editor_highlights_clip_to_the_chunk() {
        let source = "/new @docs/plan.md --draft";
        let spans = find_arg_tokens(source, 0, true);
        // A chunk starting after the @-token: only the flag token
        // intersects, at its clipped offset.
        let chunk = " --draft";
        let highlights = editor_chunk_highlights(chunk, &spans, 18, None, false, None);
        assert_eq!(
            highlights,
            vec![(1, 8, ThemeColor::MdLink)],
            "the @-token ends at the chunk start and is skipped; the flag clips in"
        );
        // The first layout line's command token highlights in accent when
        // the command takes an argument and the cursor is past it.
        let command = command_token("/new x").unwrap();
        let highlights = editor_chunk_highlights("/new x", &[], 0, Some(&command), true, Some(5));
        assert_eq!(
            highlights,
            vec![(0, 4, ThemeColor::Accent)],
            "cursor past the token keeps the accent"
        );
        // The cursor inside the token suppresses it entirely.
        let highlights = editor_chunk_highlights("/new x", &[], 0, Some(&command), true, Some(3));
        assert!(highlights.is_empty());
        // A no-argument command never highlights.
        let command = command_token("/hotkeys").unwrap();
        let highlights =
            editor_chunk_highlights("/hotkeys", &[], 0, Some(&command), false, Some(8));
        assert!(highlights.is_empty());
    }

    #[test]
    fn editor_text_spans_carry_the_cursor_reverse() {
        let theme = theme();
        let bg = Style::default();
        // Cursor inside an accent token: the reversed cell carries accent.
        let styled = editor_text_spans(&theme, "/new", &[(0, 4, ThemeColor::Accent)], Some(2), bg);
        assert_eq!(
            styled
                .iter()
                .map(|span| (span.content.to_string(), span.style))
                .collect::<Vec<_>>(),
            vec![
                ("/n".to_string(), accent()),
                ("e".to_string(), accent().add_modifier(Modifier::REVERSED)),
                ("w".to_string(), accent()),
            ]
        );
        // Cursor at the end: the appended reversed space stays default.
        let styled = editor_text_spans(&theme, "/new", &[(0, 4, ThemeColor::Accent)], Some(4), bg);
        assert_eq!(
            styled
                .iter()
                .map(|span| (span.content.to_string(), span.style))
                .collect::<Vec<_>>(),
            vec![
                ("/new".to_string(), accent()),
                (" ".to_string(), bg.add_modifier(Modifier::REVERSED)),
            ]
        );
        // No highlights: a single plain run, cursor reversed over its char.
        let styled = editor_text_spans(&theme, "hello", &[], Some(2), bg);
        assert_eq!(
            styled
                .iter()
                .map(|span| (span.content.to_string(), span.style))
                .collect::<Vec<_>>(),
            vec![
                ("he".to_string(), bg),
                ("l".to_string(), bg.add_modifier(Modifier::REVERSED)),
                ("lo".to_string(), bg),
            ]
        );
    }
}
