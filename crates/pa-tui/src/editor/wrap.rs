//! Text segmentation and word wrap: grapheme segments, atomic paste/image
//! markers, and width-aware chunking used by rendering and cursor movement.

use crate::width::{is_whitespace_char, str_width};

/// One layout line produced for rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutLine {
    pub text: String,
    pub has_cursor: bool,
    pub cursor_pos: usize,
    pub source_line: usize,
    pub source_start: usize,
}

/// Visual line mapping entry (logical line + segment bounds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualLine {
    pub logical_line: usize,
    pub start_col: usize,
    pub length: usize,
}

/// A word-wrapping chunk with logical bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    pub text: String,
    pub start_index: usize,
    pub end_index: usize,
}

/// Grapheme segment with byte offset, mirroring Intl.Segmenter data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub segment: String,
    pub index: usize,
}

pub(crate) fn graphemes(text: &str) -> Vec<Segment> {
    use unicode_segmentation::UnicodeSegmentation;
    text.graphemes(true)
        .enumerate()
        .map(|(i, g)| Segment {
            segment: g.to_string(),
            index: i,
        })
        .collect()
}

fn is_paste_marker(seg: &str) -> bool {
    // [paste #N (+L lines | C chars)]
    if !seg.starts_with("[paste #") || !seg.ends_with(']') {
        return false;
    }
    let body = &seg[8..seg.len() - 1];
    let Some((num, rest)) = body.split_once(' ') else {
        return rest_is_empty(body);
    };
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let rest = rest.strip_prefix('+').unwrap_or(rest);
    match rest.split_once(' ') {
        Some((n, "lines")) => !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()),
        Some((n, "chars")) => !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()),
        _ => false,
    }
}

fn rest_is_empty(body: &str) -> bool {
    !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit())
}

fn is_image_marker(seg: &str) -> bool {
    // [image #N]
    if !seg.starts_with("[image #") || !seg.ends_with(']') {
        return false;
    }
    let body = &seg[8..seg.len() - 1];
    !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit())
}

pub fn is_atomic_marker(seg: &str) -> bool {
    seg.len() >= 10 && (is_paste_marker(seg) || is_image_marker(seg))
}

/// Segment text merging valid paste markers and image markers into atomic units.
pub(crate) fn segment_with_markers(
    text: &str,
    valid_paste_ids: &dyn Fn(usize) -> bool,
) -> Vec<Segment> {
    let has_paste = text.contains("[paste #");
    let has_image = text.contains("[image #");
    let base = graphemes(text);
    if !has_paste && !has_image {
        return base;
    }

    let mut markers: Vec<(usize, usize)> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < text.len() {
        if bytes[i] == b'[' {
            let rest = &text[i..];
            let close = rest.find(']').map(|p| i + p + 1);
            if let Some(end) = close {
                let cand = &text[i..end];
                let id = cand
                    .strip_prefix("[paste #")
                    .and_then(|b| b.split([']', ' ']).next().map(|s| s.to_string()))
                    .and_then(|s| s.parse::<usize>().ok());
                let keep = match id {
                    Some(id) => has_paste && valid_paste_ids(id),
                    None => has_image && is_image_marker(cand),
                };
                if keep {
                    markers.push((i, end));
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    if markers.is_empty() {
        return base;
    }

    let mut result: Vec<Segment> = Vec::new();
    let mut marker_idx = 0usize;
    for seg in base {
        while marker_idx < markers.len() && markers[marker_idx].1 <= seg.index {
            marker_idx += 1;
        }
        let in_marker = marker_idx < markers.len()
            && seg.index >= markers[marker_idx].0
            && seg.index < markers[marker_idx].1;
        if in_marker {
            if seg.index == markers[marker_idx].0 {
                let (start, end) = markers[marker_idx];
                result.push(Segment {
                    segment: text[start..end].to_string(),
                    index: start,
                });
            }
        } else {
            result.push(seg);
        }
    }
    result
}

/// Split a line into word-wrapped chunks (port of wordWrapLine).
pub fn word_wrap_line(
    line: &str,
    max_width: usize,
    segments: Option<Vec<Segment>>,
) -> Vec<TextChunk> {
    if line.is_empty() || max_width == 0 {
        return vec![TextChunk {
            text: String::new(),
            start_index: 0,
            end_index: 0,
        }];
    }
    if str_width(line) <= max_width {
        return vec![TextChunk {
            text: line.to_string(),
            start_index: 0,
            end_index: line.len(),
        }];
    }
    let segments = segments.unwrap_or_else(|| graphemes(line));
    let mut chunks: Vec<TextChunk> = Vec::new();
    let mut current_width = 0usize;
    let mut chunk_start = 0usize;
    let mut wrap_opp_index: isize = -1;
    let mut wrap_opp_width = 0usize;

    for i in 0..segments.len() {
        let seg = &segments[i];
        let grapheme = &seg.segment;
        let g_width = str_width(grapheme);
        let char_index = seg.index;
        let is_ws = !is_atomic_marker(grapheme)
            && grapheme.chars().all(is_whitespace_char)
            && !grapheme.is_empty();

        if current_width + g_width > max_width {
            if wrap_opp_index >= 0 && current_width - wrap_opp_width + g_width <= max_width {
                let opp = wrap_opp_index as usize;
                chunks.push(TextChunk {
                    text: line[chunk_start..opp].to_string(),
                    start_index: chunk_start,
                    end_index: opp,
                });
                chunk_start = opp;
                current_width -= wrap_opp_width;
            } else if chunk_start < char_index {
                chunks.push(TextChunk {
                    text: line[chunk_start..char_index].to_string(),
                    start_index: chunk_start,
                    end_index: char_index,
                });
                chunk_start = char_index;
                current_width = 0;
            }
            wrap_opp_index = -1;
        }

        if g_width > max_width {
            // Atomic segment wider than the viewport: visual-only re-wrap.
            let sub_chunks = word_wrap_line(grapheme, max_width, None);
            for sc in &sub_chunks[..sub_chunks.len() - 1] {
                chunks.push(TextChunk {
                    text: sc.text.clone(),
                    start_index: char_index + sc.start_index,
                    end_index: char_index + sc.end_index,
                });
            }
            let last = &sub_chunks[sub_chunks.len() - 1];
            chunk_start = char_index + last.start_index;
            current_width = str_width(&last.text);
            wrap_opp_index = -1;
            continue;
        }

        current_width += g_width;

        let next = segments.get(i + 1);
        let next_starts_word = next.is_some_and(|n| {
            is_atomic_marker(&n.segment) || n.segment.chars().any(|c| !is_whitespace_char(c))
        });
        if is_ws && next_starts_word {
            if let Some(next) = next {
                wrap_opp_index = next.index as isize;
                wrap_opp_width = current_width;
            }
        }
    }

    chunks.push(TextChunk {
        text: line[chunk_start..].to_string(),
        start_index: chunk_start,
        end_index: line.len(),
    });
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_wrap_chunks() {
        let chunks = word_wrap_line("hello world this wraps", 10, None);
        for c in &chunks {
            assert!(str_width(&c.text) <= 10, "chunk too wide: {:?}", c.text);
        }
        let joined: String = chunks
            .iter()
            .map(|c| c.text.clone())
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(joined, "hello world this wraps");
    }
}
