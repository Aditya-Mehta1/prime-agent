//! Terminal column width measurement, wrapping, and truncation.
//!
//! Mirrors `packages/tui/src/utils.ts`: grapheme-aware widths, emoji counted
//! as 2 columns, tabs expand to 3 spaces when measuring rendered output.

use crate::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// Width of one grapheme cluster approximated by its first char plus zero-width
/// continuation chars. Good enough for the terminal layout we render.
pub fn char_width(c: char) -> usize {
    match c {
        '\t' => 3,
        c if c.is_control() => 0,
        c => c.width().unwrap_or(0),
    }
}

pub fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

pub fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| str_width(&s.content)).sum()
}

pub fn line_width(line: &[Span]) -> usize {
    spans_width(line)
}

pub fn is_whitespace_char(c: char) -> bool {
    c.is_whitespace()
}

const PUNCTUATION: &str = "(){}[]<>.,;:'\"!?+-=*/\\|&%^$#@~`";

pub fn is_punctuation_char(c: char) -> bool {
    PUNCTUATION.contains(c)
}

/// Strip a leading run of zero-width/format chars (approximation of the TS
/// leading-non-printing trim).
fn base_char_width(c: char) -> usize {
    match c {
        '\u{200b}'..='\u{200f}'
        | '\u{feff}'
        | '\u{2060}'..='\u{2064}'
        | '\u{0300}'..='\u{036f}' => 0,
        _ => char_width(c),
    }
}

/// Pad a line with plain spaces to exactly `width` visible columns.
pub fn pad_line(mut line: Line, width: usize) -> Line {
    let w = line_width(&line);
    if w < width {
        line.push(Span::raw(" ".repeat(width - w)));
    }
    line
}

/// Truncate a line to `max_width` visible columns, appending `ellipsis` (also
/// measured) when content was cut.
pub fn truncate_line(line: &Line, max_width: usize, ellipsis: &str) -> Line {
    if line_width(line) <= max_width {
        return line.clone();
    }
    let ellipsis_width = str_width(ellipsis);
    let budget = max_width.saturating_sub(ellipsis_width);
    let mut out: Line = Vec::new();
    let mut used = 0usize;
    'outer: for span in line {
        for c in span.content.chars() {
            let w = char_width(c);
            if used + w > budget {
                break 'outer;
            }
            push_char(&mut out, span.style, c);
            used += w;
        }
    }
    if !ellipsis.is_empty() {
        out.push(Span::styled(
            ellipsis.to_string(),
            ellipsis_span_style(line),
        ));
    }
    out
}

fn ellipsis_span_style(line: &Line) -> ratatui::style::Style {
    line.last().map(|s| s.style).unwrap_or_default()
}

fn push_char(out: &mut Line, style: ratatui::style::Style, c: char) {
    if let Some(last) = out.last_mut() {
        if last.style == style {
            last.content.push(c);
            return;
        }
    }
    out.push(Span::styled(c.to_string(), style));
}

/// Split a line into wrapped lines at word boundaries, mirroring
/// `wrapSingleLine` in utils.ts: break long tokens at char level, trim
/// trailing whitespace on each wrapped line, never start a line with
/// whitespace.
pub fn wrap_line(line: &Line, width: usize) -> Vec<Line> {
    if width == 0 {
        return vec![line.clone()];
    }
    if line_width(line) <= width {
        return vec![line.clone()];
    }

    // Tokenize: whitespace runs and non-whitespace runs (styles split too).
    let tokens = tokenize(line);
    let mut wrapped: Vec<Line> = Vec::new();
    let mut current: Line = Vec::new();
    let mut current_width = 0usize;

    for token in &tokens {
        let token_width = line_width(token);
        let is_ws = token
            .iter()
            .all(|s| s.content.chars().all(is_whitespace_char));
        if token_width > width && !is_ws {
            // Flush current line, then hard-break the token.
            if !current.is_empty() {
                wrapped.push(std::mem::take(&mut current));
            }
            let mut chunk: Line = Vec::new();
            let mut chunk_width = 0usize;
            for span in token.iter() {
                for c in span.content.chars() {
                    let w = char_width(c);
                    if chunk_width + w > width {
                        wrapped.push(std::mem::take(&mut chunk));
                        chunk_width = 0;
                    }
                    push_char(&mut chunk, span.style, c);
                    chunk_width += w;
                }
            }
            current = chunk;
            current_width = chunk_width;
            continue;
        }
        if current_width + token_width > width && current_width > 0 {
            let trimmed = trim_end(&current);
            wrapped.push(trimmed);
            current = Vec::new();
            current_width = 0;
            if is_ws {
                continue;
            }
        }
        current.extend(token.iter().cloned());
        current_width += token_width;
    }
    if !current.is_empty() {
        wrapped.push(current);
    }
    if wrapped.is_empty() {
        wrapped.push(Vec::new());
    }
    wrapped
}

fn trim_end(line: &Line) -> Line {
    let mut out = line.clone();
    while let Some(last) = out.last_mut() {
        let trimmed = last.content.trim_end();
        if trimmed.is_empty() {
            out.pop();
        } else {
            last.content = trimmed.to_string();
            break;
        }
    }
    out
}

fn tokenize(line: &Line) -> Vec<Line> {
    let mut tokens: Vec<Line> = Vec::new();
    for span in line {
        let mut current = String::new();
        let mut current_ws: Option<bool> = None;
        for c in span.content.chars() {
            let ws = is_whitespace_char(c);
            match current_ws {
                Some(prev) if prev == ws => current.push(c),
                Some(_) => {
                    tokens.push(vec![Span::styled(std::mem::take(&mut current), span.style)]);
                    current.push(c);
                    current_ws = Some(ws);
                }
                None => {
                    current.push(c);
                    current_ws = Some(ws);
                }
            }
        }
        if !current.is_empty() {
            tokens.push(vec![Span::styled(current, span.style)]);
        }
    }
    tokens
}

/// Wrap plain text (may contain \n) into lines of styled raw spans.
pub fn wrap_text(text: &str, width: usize) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    for para in text.split('\n') {
        let line: Line = vec![Span::raw(para.to_string())];
        out.extend(wrap_line(&line, width));
    }
    if out.is_empty() {
        out.push(Vec::new());
    }
    out
}

/// Slice a line by visible columns `[start, start+length)`.
pub fn slice_line_by_column(line: &Line, start: usize, length: usize) -> Line {
    let mut out: Line = Vec::new();
    let mut col = 0usize;
    let end = start.saturating_add(length);
    for span in line {
        for c in span.content.chars() {
            let w = char_width(c);
            if col >= start && col + w <= end {
                push_char(&mut out, span.style, c);
            }
            col += w;
            if col >= end {
                return out;
            }
        }
    }
    out
}

/// Drop trailing blank lines from a rendered block (e.g. trailing Spacer output).
pub fn trim_trailing_empty(lines: &mut Vec<Line>) {
    while lines
        .last()
        .is_some_and(|l| l.is_empty() || l.iter().all(|s| s.content.trim().is_empty()))
    {
        lines.pop();
    }
}

/// First char width of `s` for overflow checks.
pub fn base_char_w(c: char) -> usize {
    base_char_width(c)
}
