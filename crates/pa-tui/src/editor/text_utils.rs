//! Char-index helpers for editor text manipulation and key-id decoding.

// ---- helpers -------------------------------------------------------------

/// Normalize CRLF/CR to LF and tabs to 4 spaces (TS normalizeText).
pub fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
}

/// Split a string at a char (not byte) index.
pub(crate) fn split_at_char(s: &str, char_idx: usize) -> (String, String) {
    let mut left = String::new();
    let mut count = 0usize;
    for c in s.chars() {
        if count < char_idx {
            left.push(c);
            count += 1;
        } else {
            break;
        }
    }
    let right: String = s.chars().skip(char_idx).collect();
    (left, right)
}

pub(crate) fn char_prefix(s: &str, char_idx: usize) -> String {
    split_at_char(s, char_idx).0
}

pub(crate) fn char_suffix(s: &str, char_idx: usize) -> String {
    split_at_char(s, char_idx).1
}

pub(crate) fn char_at(s: &str, char_idx: usize) -> Option<char> {
    s.chars().nth(char_idx)
}

/// Find `needle` (single char) after a char index.
pub(crate) fn char_find_after(line: &str, from_char: usize, needle: &str) -> Option<usize> {
    let n = needle.chars().next()?;
    if from_char == usize::MAX {
        return line.chars().position(|c| c == n);
    }
    line.chars()
        .enumerate()
        .skip_while(|(i, _)| *i <= from_char)
        .find(|(_, c)| *c == n)
        .map(|(i, _)| i)
}

pub(crate) fn char_find_before(line: &str, from_char: usize, needle: &str) -> Option<usize> {
    let n = needle.chars().next()?;
    let limit = if from_char == usize::MAX {
        line.chars().count()
    } else {
        from_char
    };
    line.chars()
        .take(limit)
        .collect::<Vec<_>>()
        .iter()
        .rposition(|&c| c == n)
}

/// Decode a printable character from a key id ("" for control keys).
pub(crate) fn decode_printable(input: &str) -> Option<String> {
    let (mods, key) = split_key_id(input);
    if !matches!(mods.as_str(), "" | "shift") {
        return None;
    }
    let printable = match key.as_str() {
        "space" => " ".to_string(),
        "enter" | "tab" | "escape" | "backspace" | "delete" | "up" | "down" | "left" | "right"
        | "home" | "end" | "pageUp" | "pageDown" => return None,
        k => k.to_string(),
    };
    if printable.chars().count() == 1 {
        if mods == "shift" {
            return Some(printable.to_uppercase());
        }
        Some(printable)
    } else {
        None
    }
}

fn split_key_id(input: &str) -> (String, String) {
    let parts: Vec<&str> = input.split('+').collect();
    if parts.len() > 1 {
        (
            parts[..parts.len() - 1].join("+"),
            parts[parts.len() - 1].to_string(),
        )
    } else {
        (String::new(), input.to_string())
    }
}

/// Match `(?:^|[ \t])(?:@|...)...` symbol-token suffix used for @/# autocomplete.
pub(crate) fn ends_with_symbol_token(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return false;
    }
    // find last whitespace boundary
    let start = chars
        .iter()
        .rposition(|&c| c == ' ' || c == '\t')
        .map(|p| p + 1)
        .unwrap_or(0);
    let token: String = chars[start..].iter().collect();
    let mut tchars = token.chars();
    matches!(tchars.next(), Some('@' | '#')) && !token.contains(char::is_whitespace)
}
