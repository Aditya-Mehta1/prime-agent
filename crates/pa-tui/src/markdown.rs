//! Markdown rendering ported from `packages/tui/src/components/markdown.ts`
//! (the block/inline subset that appears in agent sessions: headings,
//! paragraphs, fenced code, lists, blockquotes, hr, and inline emphasis,
//! code, and links). Emits styled `Line`s for ratatui instead of ANSI strings.

use crate::width::str_width;
use crate::{Line, Span};
use ratatui::style::{Modifier, Style};
use ratatui::text as rt;

/// Styling hooks resolved from a theme.
#[derive(Debug, Clone, Copy)]
pub struct MarkdownStyle {
    pub body: Style,
    pub heading: Style,
    pub link: Style,
    pub link_url: Style,
    pub code: Style,
    pub code_block: Style,
    pub code_block_border: Style,
    pub quote: Style,
    pub quote_border: Style,
    pub hr: Style,
    pub list_bullet: Style,
    pub bold: Modifier,
    pub italic: Modifier,
    pub strikethrough: Modifier,
}

impl Default for MarkdownStyle {
    fn default() -> Self {
        Self::from_theme(&crate::theme::Theme::builtin(
            "prime",
            crate::theme::ColorMode::TrueColor,
        ))
    }
}

impl MarkdownStyle {
    pub fn from_theme(theme: &crate::theme::Theme) -> Self {
        use crate::theme::ThemeColor as C;
        Self {
            body: theme.fg_style(C::MdBody),
            heading: theme.fg_style(C::MdHeading),
            link: theme.fg_style(C::MdLink),
            link_url: theme.fg_style(C::MdLinkUrl),
            code: theme.fg_style(C::MdCode),
            code_block: theme.fg_style(C::MdCodeBlock),
            code_block_border: theme.fg_style(C::MdCodeBlockBorder),
            quote: theme.fg_style(C::MdQuote),
            quote_border: theme.fg_style(C::MdQuoteBorder),
            hr: theme.fg_style(C::MdHr),
            list_bullet: theme.fg_style(C::MdListBullet),
            bold: Modifier::BOLD,
            italic: Modifier::ITALIC,
            strikethrough: Modifier::CROSSED_OUT,
        }
    }
}

/// Rendered markdown document as styled lines.
pub fn render_markdown(text: &str, width: usize, style: &MarkdownStyle) -> Vec<Line> {
    let content_width = width.max(1);
    if text.trim().is_empty() {
        return Vec::new();
    }
    let normalized = text.replace('\t', "   ");
    let mut lines: Vec<Line> = Vec::new();
    let blocks = parse_blocks(&normalized);
    for (i, block) in blocks.iter().enumerate() {
        let next = blocks.get(i + 1);
        render_block(block, next, content_width, style, &mut lines);
    }
    lines
}

#[derive(Debug, Clone, PartialEq)]
enum BlockKind {
    Heading,
    Paragraph,
    Code { lang: Option<String> },
    List { ordered: bool, start: usize },
    Quote,
    Hr,
}

#[derive(Debug, Clone)]
struct Block {
    kind: BlockKind,
    depth: usize,
    /// True when a blank line precedes this block (TS emits a `space` token).
    sep_blank: bool,
    /// Raw lines of the block (for code: literal lines; for others: unwrapped content).
    lines: Vec<String>,
}

fn parse_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let src_lines: Vec<&str> = text.lines().collect();
    let mut i = 0usize;
    while i < src_lines.len() {
        let line = src_lines[i];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        let sep_blank = i > 0
            && src_lines[..i]
                .iter()
                .rev()
                .take_while(|l| l.trim().is_empty())
                .count()
                > 0;
        // Fenced code
        if let Some(fence) = trimmed.strip_prefix("```") {
            let lang = if fence.is_empty() {
                None
            } else {
                Some(fence.trim().to_string())
            };
            let mut code = Vec::new();
            i += 1;
            while i < src_lines.len() && !src_lines[i].trim().starts_with("```") {
                code.push(src_lines[i].to_string());
                i += 1;
            }
            i += 1; // skip closing fence
            blocks.push(Block {
                kind: BlockKind::Code { lang },
                depth: 0,
                sep_blank,
                lines: code,
            });
            continue;
        }
        // Heading
        let hashes = trimmed.chars().take_while(|&c| c == '#').count();
        if hashes > 0 && trimmed.len() > hashes && trimmed.as_bytes()[hashes] == b' ' {
            blocks.push(Block {
                kind: BlockKind::Heading,
                depth: hashes,
                sep_blank,
                lines: vec![trimmed[hashes + 1..].to_string()],
            });
            i += 1;
            continue;
        }
        // hr
        if is_hr(trimmed) {
            blocks.push(Block {
                kind: BlockKind::Hr,
                depth: 0,
                sep_blank,
                lines: Vec::new(),
            });
            i += 1;
            continue;
        }
        // Quote
        if let Some(q) = trimmed.strip_prefix('>') {
            let mut qlines = vec![q.trim_start().to_string()];
            i += 1;
            while i < src_lines.len()
                && !src_lines[i].trim().is_empty()
                && src_lines[i].trim().starts_with('>')
            {
                qlines.push(
                    src_lines[i]
                        .trim()
                        .trim_start_matches('>')
                        .trim_start()
                        .to_string(),
                );
                i += 1;
            }
            blocks.push(Block {
                kind: BlockKind::Quote,
                depth: 0,
                sep_blank,
                lines: qlines,
            });
            continue;
        }
        // List
        if let Some(marker) = list_marker(trimmed) {
            let (ordered, start) = marker;
            let mut items: Vec<String> = Vec::new();
            let mut item = trimmed[marker_width(trimmed)..].to_string();
            i += 1;
            while i < src_lines.len() {
                let l = src_lines[i];
                let t = l.trim();
                if t.is_empty() {
                    break;
                }
                if list_marker(t).is_some() {
                    items.push(std::mem::take(&mut item));
                    item = t[marker_width(t)..].to_string();
                    i += 1;
                } else if l.starts_with("  ") || l.starts_with('\t') {
                    item.push(' ');
                    item.push_str(t);
                    i += 1;
                } else {
                    break;
                }
            }
            items.push(item);
            blocks.push(Block {
                kind: BlockKind::List { ordered, start },
                depth: 0,
                sep_blank,
                lines: items,
            });
            continue;
        }
        // Paragraph: consume until blank line or new block marker
        let mut para = trimmed.to_string();
        i += 1;
        while i < src_lines.len() {
            let l = src_lines[i];
            let t = l.trim();
            if t.is_empty()
                || t.starts_with("```")
                || t.starts_with('>')
                || t.starts_with('#')
                || list_marker(t).is_some()
                || is_hr(t)
            {
                break;
            }
            para.push(' ');
            para.push_str(t);
            i += 1;
        }
        blocks.push(Block {
            kind: BlockKind::Paragraph,
            depth: 0,
            sep_blank,
            lines: vec![para],
        });
    }
    blocks
}

fn is_hr(t: &str) -> bool {
    let chars: Vec<char> = t.chars().filter(|&c| c != ' ').collect();
    (chars.len() >= 3)
        && chars.iter().all(|&c| c == '-' || c == '*' || c == '_')
        && (chars[0] == '-' || chars[0] == '*' || chars[0] == '_')
}

fn list_marker(t: &str) -> Option<(bool, usize)> {
    if let Some(rest) = t.strip_prefix("- ") {
        let _ = rest;
        return Some((false, 0));
    }
    if let Some(rest) = t.strip_prefix("* ") {
        let _ = rest;
        return Some((false, 0));
    }
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let after = &t[digits.len()..];
        if let Some(rest) = after.strip_prefix(". ") {
            let _ = rest;
            let n: usize = digits.parse().ok()?;
            return Some((true, n));
        }
    }
    None
}

fn marker_width(t: &str) -> usize {
    if t.starts_with("- ") || t.starts_with("* ") {
        2
    } else {
        t.find(". ").map(|p| p + 2).unwrap_or(t.len())
    }
}

fn render_block(
    block: &Block,
    next: Option<&Block>,
    width: usize,
    style: &MarkdownStyle,
    out: &mut Vec<Line>,
) {
    // TS pushes a blank line between blocks only when no `space` token sits
    // between them, i.e. when the blocks are adjacent (no blank source line).
    let blank_after = |exclude_lists: bool| -> bool {
        match next {
            Some(nb) => {
                !nb.sep_blank && !(exclude_lists && matches!(nb.kind, BlockKind::List { .. }))
            }
            None => false,
        }
    };
    match &block.kind {
        BlockKind::Heading => {
            let text = block.lines.first().cloned().unwrap_or_default();
            let mut spans = render_inline(&text, style);
            match block.depth {
                1 => {
                    out.push(spans_with(
                        spans,
                        style
                            .heading
                            .add_modifier(style.bold | Modifier::UNDERLINED),
                    ));
                }
                d if d >= 5 => {
                    for s in spans.iter_mut() {
                        s.style = style.heading.add_modifier(style.italic);
                    }
                    out.push(spans);
                }
                4 => {
                    for s in spans.iter_mut() {
                        s.style = style.heading.add_modifier(style.bold | style.italic);
                    }
                    out.push(spans);
                }
                _ => {
                    for s in spans.iter_mut() {
                        s.style = style.heading.add_modifier(style.bold);
                    }
                    out.push(spans);
                }
            }
            if blank_after(false) {
                out.push(Vec::new());
            }
        }
        BlockKind::Paragraph => {
            let text = block.lines.first().cloned().unwrap_or_default();
            let spans = render_inline(&text, style);
            wrap_spans(&spans, width, style.body, out);
            if blank_after(true) {
                out.push(Vec::new());
            }
        }
        BlockKind::Code { .. } => {
            let border = style.code_block_border;
            let top: String = "─".repeat(width.max(1));
            out.push(vec![Span::styled(top, border)]);
            for line in &block.lines {
                let mut spans = vec![Span::styled(" ", style.code_block)];
                spans.push(Span::styled(line.clone(), style.code_block));
                out.push(spans);
            }
            let bottom: String = "─".repeat(width.max(1));
            out.push(vec![Span::styled(bottom, border)]);
            if blank_after(false) {
                out.push(Vec::new());
            }
        }
        BlockKind::List { ordered, start } => {
            for (i, item) in block.lines.iter().enumerate() {
                let bullet = if *ordered {
                    format!("{}. ", start + i)
                } else {
                    "- ".to_string()
                };
                let spans = render_inline(item, style);
                wrap_list_item(&bullet, &spans, width, style, out);
            }
            if blank_after(true) {
                out.push(Vec::new());
            }
        }
        BlockKind::Quote => {
            for line in &block.lines {
                let spans = render_inline(line, style);
                let mut quote_spans: Vec<Span> = Vec::new();
                for mut s in spans {
                    s.style = style.quote.patch(s.style);
                    quote_spans.push(s);
                }
                wrap_quote(&quote_spans, width, style, out);
            }
        }
        BlockKind::Hr => {
            let bar: String = "─".repeat(width.max(1));
            out.push(vec![Span::styled(bar, style.hr)]);
        }
    }
}

fn spans_with(spans: Line, style: Style) -> Line {
    spans
        .into_iter()
        .map(|mut s| {
            s.style = style;
            s
        })
        .collect()
}

/// Inline rendering: bold, italic, strikethrough, code, links.
pub fn render_inline(text: &str, style: &MarkdownStyle) -> Line {
    let mut spans: Vec<Span> = Vec::new();
    let bytes: Vec<char> = text.chars().collect();
    let mut buf = String::new();
    let mut i = 0usize;
    let base = style.body;
    let mut bold = false;
    let mut italic = false;
    let strike = false;

    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                let mut m = Modifier::empty();
                if bold {
                    m |= style.bold;
                }
                if italic {
                    m |= style.italic;
                }
                if strike {
                    m |= style.strikethrough;
                }
                spans.push(Span::styled(std::mem::take(&mut buf), base.add_modifier(m)));
            }
        };
    }

    while i < bytes.len() {
        let c = bytes[i];
        // inline code
        if c == '`' {
            let mut j = i + 1;
            let mut code = String::new();
            while j < bytes.len() && bytes[j] != '`' {
                code.push(bytes[j]);
                j += 1;
            }
            if j < bytes.len() {
                flush!();
                spans.push(Span::styled(code, style.code));
                i = j + 1;
                continue;
            }
        }
        // links [text](url)
        if c == '[' {
            let mut j = i + 1;
            let mut label = String::new();
            while j < bytes.len() && bytes[j] != ']' {
                label.push(bytes[j]);
                j += 1;
            }
            if j + 1 < bytes.len() && bytes[j] == ']' && bytes[j + 1] == '(' {
                let mut k = j + 2;
                let mut url = String::new();
                while k < bytes.len() && bytes[k] != ')' {
                    url.push(bytes[k]);
                    k += 1;
                }
                if k < bytes.len() {
                    flush!();
                    let mut m = Modifier::empty();
                    if bold {
                        m |= style.bold;
                    }
                    if italic {
                        m |= style.italic;
                    }
                    spans.push(Span::styled(label, style.link.add_modifier(m)));
                    if !url.is_empty() {
                        spans.push(Span::styled(format!(" ({url})"), style.link_url));
                    }
                    i = k + 1;
                    continue;
                }
            }
        }
        // emphasis
        if (c == '*' || c == '_') && i + 1 < bytes.len() {
            let is_triple = i + 2 < bytes.len() && bytes[i + 1] == c && bytes[i + 2] == c;
            if is_triple {
                if let Some(close) = find_closing(&bytes, i + 3, c, 3) {
                    flush!();
                    bold = !bold;
                    italic = !italic;
                    let inner: String = bytes[i + 3..close].iter().collect();
                    spans.push(Span::styled(
                        inner,
                        base.add_modifier(style.bold | style.italic),
                    ));
                    bold = !bold;
                    italic = !italic;
                    i = close + 3;
                    continue;
                }
            }
            let doubled = i + 1 < bytes.len() && bytes[i + 1] == c;
            let (len, close_search) = if doubled { (2, i + 2) } else { (1, i + 1) };
            if let Some(close) = find_closing(&bytes, close_search, c, len) {
                let inner: String = bytes[close_search..close].iter().collect();
                if inner.trim().is_empty() {
                    buf.push(c);
                    i += 1;
                    continue;
                }
                flush!();
                if doubled {
                    bold = !bold;
                    let mut inner_spans = render_inline(&inner, style);
                    for s in inner_spans.iter_mut() {
                        s.style = s.style.add_modifier(style.bold);
                    }
                    spans.extend(inner_spans);
                    bold = !bold;
                } else {
                    italic = !italic;
                    let mut inner_spans = render_inline(&inner, style);
                    for s in inner_spans.iter_mut() {
                        s.style = s.style.add_modifier(style.italic);
                    }
                    spans.extend(inner_spans);
                    italic = !italic;
                }
                i = close + len;
                continue;
            }
        }
        if c == '~' && i + 1 < bytes.len() && bytes[i + 1] == '~' {
            if let Some(close) = find_closing(&bytes, i + 2, '~', 2) {
                let inner: String = bytes[i + 2..close].iter().collect();
                if !inner.trim().is_empty() {
                    flush!();
                    let mut inner_spans = render_inline(&inner, style);
                    for s in inner_spans.iter_mut() {
                        s.style = s.style.add_modifier(style.strikethrough);
                    }
                    spans.extend(inner_spans);
                    i = close + 2;
                    continue;
                }
            }
        }
        buf.push(c);
        i += 1;
    }
    flush!();
    if spans.is_empty() {
        spans.push(Span::raw(""));
    }
    spans
}

fn find_closing(chars: &[char], from: usize, delim: char, len: usize) -> Option<usize> {
    let mut i = from;
    while i + len <= chars.len() {
        if (0..len).all(|k| chars[i + k] == delim) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Wrap styled spans to `width`. Words break at whitespace; leading spaces are
/// dropped after a wrap break. Adjacent same-style output pieces merge.
pub fn wrap_spans(spans: &[Span], width: usize, base: Style, out: &mut Vec<Line>) {
    let _ = base;
    if width == 0 {
        out.push(spans.to_vec());
        return;
    }
    // tokens: (text, style); even entries words, odd entries single-space gaps
    let mut tokens: Vec<(String, Style)> = Vec::new();
    for span in spans {
        let mut word = String::new();
        for ch in span.content.chars() {
            if ch == ' ' {
                if !word.is_empty() {
                    tokens.push((std::mem::take(&mut word), span.style));
                    tokens.push((" ".to_string(), span.style));
                }
            } else {
                word.push(ch);
            }
        }
        if !word.is_empty() {
            tokens.push((word, span.style));
        }
    }

    let mut current: Line = Vec::new();
    let mut col = 0usize;
    let mut i = 0usize;
    while i < tokens.len() {
        let (text, style) = &tokens[i];
        let w = str_width(text);
        if col + w > width && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            col = 0;
            // drop leading whitespace at the new line start
            if text.trim().is_empty() {
                i += 1;
                continue;
            }
        }
        // break overlong words
        let mut rest = text.clone();
        let style = *style;
        while str_width(&rest) + col > width {
            let mut take = String::new();
            let mut tw = 0usize;
            for c in rest.chars() {
                let cw = crate::width::char_width(c);
                if tw + cw + col > width {
                    break;
                }
                take.push(c);
                tw += cw;
            }
            if take.is_empty() {
                break;
            }
            current.push(Span::styled(take.clone(), style));
            out.push(std::mem::take(&mut current));
            col = 0;
            rest = rest[take.len()..].to_string();
        }
        col += str_width(&rest);
        current.push(Span::styled(rest, style));
        i += 1;
    }
    out.push(current);
}

fn wrap_list_item(
    bullet: &str,
    spans: &[Span],
    width: usize,
    style: &MarkdownStyle,
    out: &mut Vec<Line>,
) {
    let bullet_width = str_width(bullet);
    let content_width = width.saturating_sub(bullet_width).max(1);
    let mut wrapped: Vec<Line> = Vec::new();
    wrap_spans(spans, content_width, style.body, &mut wrapped);
    for (i, line) in wrapped.into_iter().enumerate() {
        if i == 0 {
            let mut l = vec![Span::styled(bullet.to_string(), style.list_bullet)];
            l.extend(line);
            out.push(l);
        } else {
            let mut l = vec![Span::styled(" ".repeat(bullet_width), style.body)];
            l.extend(line);
            out.push(l);
        }
    }
}

fn wrap_quote(spans: &[Span], width: usize, style: &MarkdownStyle, out: &mut Vec<Line>) {
    let quote_width = width.saturating_sub(2).max(1);
    let mut wrapped: Vec<Line> = Vec::new();
    wrap_spans(spans, quote_width, style.quote, &mut wrapped);
    for line in wrapped {
        let mut l = vec![Span::styled("▐ ", style.quote_border)];
        l.extend(line);
        out.push(l);
    }
}

/// Convert our Line type to ratatui text for rendering.
pub fn to_ratatui_line(line: &Line) -> rt::Line<'static> {
    let spans: Vec<rt::Span<'static>> = line
        .iter()
        .map(|s| rt::Span::styled(s.content.clone(), s.style))
        .collect();
    rt::Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_and_paragraph() {
        let style = MarkdownStyle::default();
        let lines = render_markdown("# Title\n\nBody text here", 40, &style);
        // Blank line between blocks: no empty spacer line (TS `space` token).
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0][0].content, "Title");
        let joined: String = lines[1].iter().map(|s| s.content.as_str()).collect();
        assert_eq!(joined, "Body text here");
        // Adjacent heading + paragraph: heading pushes a blank line.
        let adjacent = render_markdown("# Title\nBody text here", 40, &style);
        assert_eq!(adjacent.len(), 3);
    }

    #[test]
    fn code_block_borders() {
        let style = MarkdownStyle::default();
        let lines = render_markdown("```rust\nfn main() {}\n```", 40, &style);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0][0].content, "─".repeat(40));
        assert_eq!(lines[1][1].content, "fn main() {}");
    }

    #[test]
    fn list_render() {
        let style = MarkdownStyle::default();
        let lines = render_markdown("- one\n- two", 40, &style);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0][0].content, "- ");
        assert_eq!(lines[0][1].content, "one");
    }

    #[test]
    fn inline_bold_code_link() {
        let style = MarkdownStyle::default();
        let spans = render_inline("a **b** `c` [d](http://e)", &style);
        let texts: Vec<&str> = spans.iter().map(|s| s.content.as_str()).collect();
        assert_eq!(texts, vec!["a ", "b", " ", "c", " ", "d", " (http://e)"]);
    }

    #[test]
    fn wrapping() {
        let style = MarkdownStyle::default();
        let lines = render_markdown("word ".repeat(10).trim(), 20, &style);
        assert!(lines.len() >= 3);
        for l in &lines {
            let w: usize = l.iter().map(|s| str_width(&s.content)).sum();
            assert!(w <= 20, "line too wide: {w}");
        }
    }

    #[test]
    fn quote_block() {
        let style = MarkdownStyle::default();
        let lines = render_markdown("> wisdom", 40, &style);
        assert_eq!(lines[0][0].content, "▐ ");
        assert_eq!(lines[0][1].content, "wisdom");
    }
}
