//! Slash-command chat rows: the durable echo and result rows session commands
//! append (custom types `session_slash_command` /
//! `session_slash_command_result`). Both share the user-message block
//! geometry — `Box(2,1)` on the `userMessageBg` surface — with the echo's
//! `/name` token in `accent` and `@path` / `--flag` argument tokens in
//! `success` / `mdLink` (prompt-highlight token styling).

use crate::theme::{Theme, ThemeBg, ThemeColor};
use crate::width::{str_width, wrap_text};
use crate::{Line, Span};
use ratatui::style::Style;

/// `Box(2,1)` content width: 2 columns of padding on each side, matching
/// the user-message block.
fn content_width(width: usize) -> usize {
    width.saturating_sub(4).max(1)
}

/// One block row: 2-col padding, spans, padded to the full width on the
/// block background.
fn block_row(spans: Line, bg: Style, width: usize) -> Line {
    let mut row: Line = vec![Span::styled("  ".to_string(), bg)];
    row.extend(spans);
    let used: usize = row.iter().map(|s| str_width(&s.content)).sum();
    row.push(Span::styled(" ".repeat(width.saturating_sub(used)), bg));
    row
}

/// The block layout: blank surface row, content rows, blank surface row.
fn wrap_block(
    text: &str,
    theme: &Theme,
    width: usize,
    style_row: impl Fn(&str) -> Line,
) -> Vec<Line> {
    let bg = theme.bg_style(ThemeBg::UserMessageBg);
    let mut rows = vec![vec![Span::styled(" ".repeat(width), bg)]];
    let wrapped = wrap_text(text, content_width(width));
    if wrapped.is_empty() {
        rows.push(block_row(Vec::new(), bg, width));
    }
    for line in wrapped {
        let plain: String = line.iter().map(|s| s.content.as_str()).collect();
        rows.push(block_row(style_row(&plain), bg, width));
    }
    rows.push(vec![Span::styled(" ".repeat(width), bg)]);
    rows
}

/// The command echo row: the full typed command. The `/name` token renders
/// in `accent`; `@path` and `--flag` argument tokens highlight (`success`
/// and `mdLink`); a bare `--` separator highlights only for argument-taking
/// commands.
pub fn render_slash_command(
    text: &str,
    takes_argument: bool,
    theme: &Theme,
    width: usize,
) -> Vec<Line> {
    let accent = theme.fg_style(ThemeColor::Accent);
    let success = theme.fg_style(ThemeColor::Success);
    let md_link = theme.fg_style(ThemeColor::MdLink);
    let style_row = |row: &str| -> Line {
        let mut spans: Line = Vec::new();
        let chars: Vec<char> = row.chars().collect();
        let mut plain_start = 0usize;
        let mut index = 0usize;
        while index < chars.len() {
            let boundary = index == 0 || chars[index - 1].is_whitespace();
            if !boundary {
                index += 1;
                continue;
            }
            let (token_end, style) = if chars[index] == '/' {
                // The command token: through the first whitespace or the
                // row end.
                let end = chars[index..]
                    .iter()
                    .position(|c| c.is_whitespace())
                    .map(|p| index + p)
                    .unwrap_or(chars.len());
                (end, accent)
            } else {
                match argument_token_end(&chars, index, takes_argument) {
                    Some((end, kind)) => {
                        let style = match kind {
                            TokenKind::AtPath => success,
                            TokenKind::Flag => md_link,
                        };
                        (end, style)
                    }
                    None => {
                        index += 1;
                        continue;
                    }
                }
            };
            if token_end == index {
                index += 1;
                continue;
            }
            if plain_start < index {
                spans.push(Span::raw(
                    chars[plain_start..index].iter().collect::<String>(),
                ));
            }
            spans.push(Span::styled(
                chars[index..token_end].iter().collect::<String>(),
                style,
            ));
            plain_start = token_end;
            index = token_end;
        }
        if plain_start < chars.len() {
            spans.push(Span::raw(chars[plain_start..].iter().collect::<String>()));
        }
        spans
    };
    wrap_block(text, theme, width, style_row)
}

/// The result row: plain content, default foreground on the block surface.
pub fn render_slash_command_result(content: &str, theme: &Theme, width: usize) -> Vec<Line> {
    let style_row = |row: &str| -> Line {
        if row.is_empty() {
            Vec::new()
        } else {
            vec![Span::raw(row.to_string())]
        }
    };
    wrap_block(content, theme, width, style_row)
}

/// The argument-token kinds and their colors.
enum TokenKind {
    /// `@path` token (`success`).
    AtPath,
    /// `--flag` token (`mdLink`).
    Flag,
}

/// One argument token starting at `index`: an `@path` (a non-whitespace run)
/// or a `--flag` (letters/digits/dashes). A bare `--` end-of-options
/// separator counts only for argument-taking commands and only before
/// whitespace or the row end.
fn argument_token_end(
    chars: &[char],
    index: usize,
    takes_argument: bool,
) -> Option<(usize, TokenKind)> {
    let first = *chars.get(index)?;
    if first == '@' {
        let mut end = index + 1;
        while end < chars.len() && !chars[end].is_whitespace() {
            end += 1;
        }
        return (end > index + 1).then_some((end, TokenKind::AtPath));
    }
    if first == '-' && chars.get(index + 1) == Some(&'-') {
        let mut end = index + 2;
        if end == chars.len() {
            // `--` at the row end.
            return takes_argument.then_some((end, TokenKind::Flag));
        }
        let head = chars[end];
        if !head.is_ascii_alphanumeric() {
            // Bare `--` before whitespace.
            if takes_argument && head.is_whitespace() {
                return Some((end, TokenKind::Flag));
            }
            return None;
        }
        while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '-') {
            end += 1;
        }
        return Some((end, TokenKind::Flag));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorMode, Theme};

    fn theme() -> Theme {
        Theme::builtin("prime", ColorMode::TrueColor)
    }

    fn plain(rows: &[Line]) -> Vec<String> {
        rows.iter()
            .map(|l| l.iter().map(|s| s.content.as_str()).collect::<String>())
            .collect()
    }

    #[test]
    fn echo_row_uses_block_geometry() {
        let rows = render_slash_command("/goal status", true, &theme(), 40);
        assert_eq!(
            plain(&rows),
            vec![
                " ".repeat(40),
                format!("  {}  ", "/goal status") + &" ".repeat(40 - 2 - 12 - 2),
                " ".repeat(40),
            ]
        );
    }

    #[test]
    fn echo_row_wraps_long_args() {
        let rows = render_slash_command(
            "/goal make the verifier pass everywhere",
            true,
            &theme(),
            30,
        );
        let text = plain(&rows);
        assert_eq!(text.len(), 4);
        assert!(text[1].starts_with("  /goal make the verifier"));
        assert!(text[2].starts_with("  pass everywhere"));
    }

    #[test]
    fn result_row_renders_content() {
        let rows = render_slash_command_result("Goal active: ship it", &theme(), 40);
        let text = plain(&rows);
        assert_eq!(text.len(), 3);
        assert!(text[1].contains("Goal active: ship it"));
    }
}
