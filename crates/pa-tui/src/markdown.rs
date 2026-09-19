//! Markdown rendering ported from `packages/tui/src/components/markdown.ts`
//! (the block/inline subset that appears in agent sessions: headings,
//! paragraphs, fenced code, lists, blockquotes, hr, and inline emphasis,
//! code, and links). Emits styled `Line`s for ratatui instead of ANSI strings.

use crate::width::str_width;
use crate::{Line, Span};
use ratatui::style::{Modifier, Style};
use ratatui::text as rt;

/// Styling hooks resolved from a theme (plus the settings-driven
/// `code_block_indent`; not `Copy` because of the indent `String`).
#[derive(Debug, Clone)]
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
    /// The fenced-code indent string (`markdown.codeBlockIndent` in
    /// settings, TS `codeBlockIndent` on the markdown theme; default "  ").
    pub code_block_indent: String,
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
            code_block_indent: "  ".to_string(),
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
        // A blank source line separates blocks: TS's lexer emits one `space`
        // token per blank run and `renderToken` pushes one empty row for it
        // (markdown.ts `case "space"`). `parse_blocks` skips the blank
        // source lines, so the row is emitted here, ahead of the block it
        // precedes; adjacent blocks keep their `blank_after` row.
        if block.sep_blank {
            lines.push(Vec::new());
        }
        render_block(block, next, content_width, style, &mut lines);
    }
    lines
}

#[derive(Debug, Clone, PartialEq)]
enum BlockKind {
    Heading,
    Paragraph,
    Code {
        lang: Option<String>,
    },
    List {
        ordered: bool,
        start: usize,
    },
    Quote,
    Hr,
    Table {
        header: Vec<String>,
        rows: Vec<Vec<String>>,
    },
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
        // Table (marked's table rule: header row + delimiter row +
        // body rows; tried after the other block starts).
        if crate::markdown_table::is_table_start(trimmed, src_lines.get(i + 1)) {
            let table = crate::markdown_table::parse_table_block(&src_lines, &mut i);
            blocks.push(Block {
                kind: BlockKind::Table {
                    header: table.header,
                    rows: table.rows,
                },
                depth: 0,
                sep_blank,
                lines: table.raw,
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
                || crate::markdown_table::is_table_start(t, src_lines.get(i + 1))
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

pub(crate) fn is_hr(t: &str) -> bool {
    let chars: Vec<char> = t.chars().filter(|&c| c != ' ').collect();
    (chars.len() >= 3)
        && chars.iter().all(|&c| c == '-' || c == '*' || c == '_')
        && (chars[0] == '-' || chars[0] == '*' || chars[0] == '_')
}

pub(crate) fn list_marker(t: &str) -> Option<(bool, usize)> {
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
            // TS `renderCodeBlock`: no borders in the chat markdown - the
            // block is `codeBlockIndent` (settings-driven, default "  ")
            // outside the styled code line, each source line rendered with
            // the codeBlock style. The theme's `codeBlockBorder` hook exists
            // in the TS MarkdownTheme too and is unused by the renderer on
            // both sides.
            let indent = style.code_block_indent.as_str();
            for line in &block.lines {
                out.push(vec![
                    Span::raw(indent),
                    Span::styled(line.clone(), style.code_block),
                ]);
            }
            if block.lines.is_empty() {
                // An empty block still renders one indented empty line
                // (TS maps a lone codeBlock("")).
                out.push(vec![Span::raw(indent)]);
            }
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
        BlockKind::Table { header, rows } => {
            crate::markdown_table::render_table(header, rows, &block.lines, width, style, out);
            if blank_after(false) {
                out.push(Vec::new());
            }
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
                    // The observed TS binary output (0.9.5, the parity ground
                    // truth) renders the link label with the body color only:
                    // the link color is shadowed by the body color applied
                    // inside the label, and the underline wrapper never
                    // reaches the wire. `m` carries the emphasis context.
                    let href = crate::hyperlinks::rewrite_drive_path(&url);
                    let mut label_spans = render_inline(&label, style);
                    for s in label_spans.iter_mut() {
                        s.style = s.style.add_modifier(m);
                    }
                    if crate::hyperlinks::hyperlinks_enabled() {
                        // OSC 8: the label is clickable, the URL never
                        // printed inline (TS `hyperlink()`).
                        let open = crate::hyperlinks::osc8_open(&href);
                        if let Some(first) = label_spans.first_mut() {
                            first.content.insert_str(0, &open);
                        }
                        if let Some(last) = label_spans.last_mut() {
                            last.content.push_str(crate::hyperlinks::OSC8_CLOSE);
                        }
                        spans.extend(label_spans);
                    } else {
                        spans.extend(label_spans);
                        // Legacy form: the URL shows after the text unless
                        // the label is the URL (mailto stripped for the
                        // comparison, like autolinked emails).
                        let comparison = url.strip_prefix("mailto:").unwrap_or(url.as_str());
                        if label != url && label != comparison {
                            spans.push(Span::styled(format!(" ({url})"), style.link_url));
                        }
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
    // tokens: (text, style); alternating words and single-space gaps. A gap
    // at a span boundary must survive (bold text followed by " plain"), so
    // whitespace runs collapse to one gap token across the whole line.
    let mut tokens: Vec<(String, Style)> = Vec::new();
    for span in spans {
        let mut word = String::new();
        for ch in span.content.chars() {
            if ch == ' ' {
                if !word.is_empty() {
                    tokens.push((std::mem::take(&mut word), span.style));
                }
                let gap_already_emitted = tokens.last().is_some_and(|(text, _)| text == " ");
                if !gap_already_emitted {
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
            // A wrapped row never carries its trailing gap: TS
            // wrapTextWithAnsi drops the boundary space, so the styled
            // content ends at the last word and the plain padding follows.
            while current
                .last()
                .is_some_and(|span| span.content.trim().is_empty())
            {
                current.pop();
            }
            out.push(std::mem::take(&mut current));
            col = 0;
            // drop leading whitespace at the new line start
            if text.trim().is_empty() {
                i += 1;
                continue;
            }
        }
        // break overlong words; escape sequences copy through atomically
        // at zero width (OSC 8 sequences must never split mid-sequence)
        let mut rest = text.clone();
        let style = *style;
        while str_width(&rest) + col > width {
            let mut take = String::new();
            let mut tw = 0usize;
            let mut taken = 0usize;
            while taken < rest.len() {
                if let Some(len) = crate::width::escape_len(&rest[taken..]) {
                    take.push_str(&rest[taken..taken + len]);
                    taken += len;
                    continue;
                }
                let c = rest[taken..].chars().next().expect("char at boundary");
                let cw = crate::width::char_width(c);
                if tw + cw + col > width {
                    break;
                }
                take.push(c);
                tw += cw;
                taken += c.len_utf8();
            }
            if take.is_empty() {
                break;
            }
            current.push(Span::styled(take.clone(), style));
            out.push(std::mem::take(&mut current));
            col = 0;
            rest = rest[taken..].to_string();
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

/// Convert our Line type to ratatui text for rendering. OSC zone markers and
/// OSC 8 hyperlink sequences are stripped: ratatui has no escape-sequence
/// support and would count their bytes as visible cells (the paint path
/// re-emits them: zone markers per row, links via `HyperlinkWriter`).
pub fn to_ratatui_line(line: &Line) -> rt::Line<'static> {
    let mut stripped = line.clone();
    crate::osc133::strip(&mut stripped);
    crate::hyperlinks::strip_osc8(&mut stripped);
    let spans: Vec<rt::Span<'static>> = stripped
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
        // Blank line between blocks: the TS `space` token renders one empty
        // row between them (markdown.ts `case "space"`).
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0][0].content, "Title");
        assert!(lines[1].is_empty(), "the space row is empty");
        let joined: String = lines[2].iter().map(|s| s.content.as_str()).collect();
        assert_eq!(joined, "Body text here");
        // Adjacent heading + paragraph: heading pushes a blank line.
        let adjacent = render_markdown("# Title\nBody text here", 40, &style);
        assert_eq!(adjacent.len(), 3);
    }

    #[test]
    fn paragraph_blank_lines_render_space_rows() {
        let style = MarkdownStyle::default();
        let lines = render_markdown("a\n\nb", 40, &style);
        let flat: Vec<String> = lines
            .iter()
            .map(|line| line.iter().map(|s| s.content.as_str()).collect())
            .collect();
        assert_eq!(flat, vec!["a".to_string(), String::new(), "b".to_string()]);
    }

    #[test]
    fn consecutive_blank_lines_render_one_space_row() {
        let style = MarkdownStyle::default();
        // marked collapses a blank-line run into one `space` token.
        let lines = render_markdown("a\n\n\n\nb", 40, &style);
        assert_eq!(lines.len(), 3);
        assert!(lines[1].is_empty());
    }

    #[test]
    fn single_newline_stays_one_paragraph() {
        let style = MarkdownStyle::default();
        let lines = render_markdown("one\ntwo", 40, &style);
        assert_eq!(lines.len(), 1);
        let joined: String = lines[0].iter().map(|s| s.content.as_str()).collect();
        assert_eq!(joined, "one two");
    }

    #[test]
    fn code_block_keeps_space_rows_around_it() {
        let style = MarkdownStyle::default();
        let lines = render_markdown("para\n\n```rust\nfn a() {}\n```\n\nafter", 40, &style);
        let flat: Vec<String> = lines
            .iter()
            .map(|line| line.iter().map(|s| s.content.as_str()).collect())
            .collect();
        assert_eq!(
            flat,
            vec![
                "para".to_string(),
                String::new(),
                "  fn a() {}".to_string(),
                String::new(),
                "after".to_string(),
            ]
        );
    }

    #[test]
    fn code_block_indented_no_borders() {
        let style = MarkdownStyle::default();
        // TS `renderCodeBlock`: `codeBlockIndent` (default "  ") outside the
        // styled code line, no border rows in the chat markdown.
        let lines = render_markdown("```rust\nfn main() {}\n```", 40, &style);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0][0].content, "  ");
        assert_eq!(lines[0][1].content, "fn main() {}");
        // An empty block still renders one indented empty line.
        let empty = render_markdown("```\n```", 40, &style);
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0][0].content, "  ");
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
        // Pin the terminal-capability gate: a link renders the legacy
        // `label (url)` form when OSC 8 hyperlinks are unavailable.
        crate::hyperlinks::set_hyperlinks_override(Some(false));
        let style = MarkdownStyle::default();
        let spans = render_inline("a **b** `c` [d](http://e)", &style);
        let texts: Vec<&str> = spans.iter().map(|s| s.content.as_str()).collect();
        assert_eq!(texts, vec!["a ", "b", " ", "c", " ", "d", " (http://e)"]);
        crate::hyperlinks::set_hyperlinks_override(None);
    }

    #[test]
    fn legacy_link_row_is_underlined_and_shows_the_url() {
        crate::hyperlinks::set_hyperlinks_override(Some(false));
        let style = MarkdownStyle::default();
        let spans = render_inline("see [docs](https://x.dev/a)", &style);
        let texts: Vec<String> = spans.iter().map(|s| s.content.clone()).collect();
        assert_eq!(
            texts,
            vec![
                "see ".to_string(),
                "docs".to_string(),
                " (https://x.dev/a)".to_string()
            ]
        );
        // The observed TS binary output styles the label with the body
        // color only (the underline wrapper never reaches the wire).
        assert!(!spans[1].style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(spans[1].style.fg, style.body.fg);
        assert_eq!(spans[2].style.fg, style.link_url.fg);
        // The URL is not repeated when the label is the URL, and mailto
        // labels compare with the prefix stripped (autolinked emails).
        let bare = render_inline("[https://x.dev](https://x.dev)", &style);
        let joined: String = bare.iter().map(|s| s.content.as_str()).collect();
        assert_eq!(joined, "https://x.dev");
        let mail = render_inline("[a@b.dev](mailto:a@b.dev)", &style);
        let joined: String = mail.iter().map(|s| s.content.as_str()).collect();
        assert_eq!(joined, "a@b.dev");
        crate::hyperlinks::set_hyperlinks_override(None);
    }

    #[test]
    fn osc8_gated_link_row_wraps_the_label_in_a_hyperlink() {
        crate::hyperlinks::set_hyperlinks_override(Some(true));
        let style = MarkdownStyle::default();
        let spans = render_inline("see [docs](https://x.dev/a)", &style);
        let joined: String = spans.iter().map(|s| s.content.as_str()).collect();
        assert_eq!(
            joined,
            format!(
                "see {}docs{}",
                crate::hyperlinks::osc8_open("https://x.dev/a"),
                crate::hyperlinks::OSC8_CLOSE
            )
        );
        // The sequences are zero-width: the row measures like the plain text
        // and never prints the URL inline.
        assert_eq!(
            joined.chars().filter(|&c| c == '(').count(),
            0,
            "osc8 rows must not inline the url: {joined}"
        );
        assert_eq!(str_width(&joined), str_width("see docs"));
        // Windows drive-letter targets classify as file paths.
        let drive = render_inline("[c:\\src](c:\\src)", &style);
        let joined: String = drive.iter().map(|s| s.content.as_str()).collect();
        assert!(joined.contains("file:///c:/src"), "drive path: {joined}");
        crate::hyperlinks::set_hyperlinks_override(None);
    }

    #[test]
    fn table_block_renders_boxed_rows() {
        let style = MarkdownStyle::default();
        let lines = render_markdown("| a | b |\n| --- | --- |\n| 1 | 2 |\n\nafter", 40, &style);
        let flat: Vec<String> = lines
            .iter()
            .map(|line| line.iter().map(|s| s.content.as_str()).collect())
            .collect();
        assert_eq!(
            flat,
            vec![
                "┌───┬───┐".to_string(),
                "│ a │ b │".to_string(),
                "├───┼───┤".to_string(),
                "│ 1 │ 2 │".to_string(),
                "└───┴───┘".to_string(),
                String::new(),
                "after".to_string(),
            ]
        );
    }

    #[test]
    fn styled_span_boundaries_keep_their_spaces() {
        // A gap starting a new span must not be swallowed by the wrap pass.
        let style = MarkdownStyle::default();
        let lines = render_markdown("**Hello.** I can render", 80, &style);
        let joined: String = lines[0].iter().map(|s| s.content.as_str()).collect();
        assert_eq!(joined, "Hello. I can render");
        // Whitespace runs still collapse to a single gap across spans.
        let spans = render_inline("a **b**   c", &style);
        let wrapped = wrap_spans_to_text(&spans, 40);
        assert_eq!(wrapped, "a b c");
    }

    fn wrap_spans_to_text(spans: &[Span], width: usize) -> String {
        let mut lines: Vec<Line> = Vec::new();
        wrap_spans(spans, width, Style::default(), &mut lines);
        lines
            .iter()
            .flat_map(|l| l.iter().map(|s| s.content.as_str()))
            .collect()
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
