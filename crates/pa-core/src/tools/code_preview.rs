//! Bash/Python command preview extraction for tool-call rendering.
//!
//! Port of `packages/coding-agent/src/core/tools/code-preview.ts`. Regexes keep
//! JavaScript semantics (whitespace/word classes, UTF-16 string indexing).

use crate::tools::ipython_cell_code::parse_ipython_bash_cell;

const DESCRIPTOR_MAX_WIDTH: usize = 64;

/// JavaScript backslash-s character class.
const S: &str = r"[\t\n\x0B\f\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]";
/// JavaScript backslash-w character class.
const W: &str = r"[A-Za-z0-9_]";

struct Rx {
    inner: fancy_regex::Regex,
}

impl Rx {
    /// Compile a preview regex; errors abort (patterns are compile-time constants).
    fn new(pattern: &str) -> Self {
        Rx {
            inner: fancy_regex::Regex::new(pattern).expect("code-preview regex must compile"),
        }
    }

    fn is_match(&self, text: &str) -> bool {
        self.inner.is_match(text).unwrap_or(false)
    }

    fn captures<'t>(&self, text: &'t str) -> Option<fancy_regex::Captures<'t>> {
        self.inner.captures(text).ok().flatten()
    }

    fn captures_iter<'t>(
        &'t self,
        text: &'t str,
    ) -> impl Iterator<Item = fancy_regex::Captures<'t>> + 't {
        self.inner.captures_iter(text).flatten()
    }

    fn find<'t>(&self, text: &'t str) -> Option<fancy_regex::Match<'t>> {
        self.inner.find(text).ok().flatten()
    }

    fn replace(&self, text: &str, rep: &str) -> String {
        self.inner.replace(text, rep).into_owned()
    }

    fn replace_all(&self, text: &str, rep: &str) -> String {
        self.inner.replace_all(text, rep).into_owned()
    }

    fn split<'t>(&'t self, text: &'t str) -> Vec<&'t str> {
        self.inner.split(text).filter_map(Result::ok).collect()
    }
}

fn re(pattern: &str) -> Rx {
    Rx::new(pattern)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodePreviewLanguage {
    Bash,
    Python,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodePreview {
    pub language: CodePreviewLanguage,
    pub text: String,
}

impl CodePreview {
    fn bash(text: impl Into<String>) -> Self {
        CodePreview {
            language: CodePreviewLanguage::Bash,
            text: text.into(),
        }
    }

    fn python(text: impl Into<String>) -> Self {
        CodePreview {
            language: CodePreviewLanguage::Python,
            text: text.into(),
        }
    }
}

/// JS String.prototype.trim whitespace set.
fn is_js_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\u{09}'
            ..='\u{0D}'
                | ' '
                | '\u{00A0}'
                | '\u{1680}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    ) || ('\u{2000}'..='\u{200A}').contains(&ch)
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

fn js_trim_end(s: &str) -> &str {
    s.trim_end_matches(is_js_whitespace)
}

/// Number of UTF-16 code units (JS String.length).
fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// JS s.slice(0, n): first n UTF-16 code units.
fn utf16_slice_prefix(s: &str, n: usize) -> String {
    let mut units = 0usize;
    for (idx, ch) in s.char_indices() {
        if units + ch.len_utf16() > n {
            return s[..idx].to_string();
        }
        units += ch.len_utf16();
    }
    s.to_string()
}

fn collapse_whitespace(text: &str) -> String {
    re(&format!(r"{S}+")).replace_all(text, " ")
}

fn truncate_descriptor(text: &str) -> String {
    if utf16_len(text) <= DESCRIPTOR_MAX_WIDTH {
        return text.to_string();
    }
    let cut = utf16_slice_prefix(text, DESCRIPTOR_MAX_WIDTH - 1);
    format!("{}\u{2026}", js_trim_end(&cut))
}

/// Mask secrets, blobs, and oversized strings before display.
fn redact_noise(text: &str) -> String {
    let step1 = re(r"[A-Za-z0-9+/]{80,}={0,2}").replace_all(text, "<blob>");
    let step2 = re(&format!(
        r#"\b((?={W}*(?:token|key|secret|password))[A-Za-z_]{W}*){S}*={S}*(["'])[^"']*\2"#
    ))
    .replace_all(&step1, "$1=<redacted>");
    let step3 = re(&format!(
        r#"(?i)\b((?={W}*(?:token|key|secret|password))[A-Za-z_]{W}*){S}*={S}*(?!<redacted>)(?!["'])\S+"#
    ))
    .replace_all(&step2, "$1=<redacted>");
    let step4 = re(r#"(?i)\b(authorization:\s*(?:bearer\s+)?)[^\s"']+"#)
        .replace_all(&step3, "$1<redacted>");
    let step5 = re(r#"(["'])sk-[^"']+\1"#).replace_all(&step4, "$1<redacted>$1");
    re(r#"(["']).{160,}\1"#).replace_all(&step5, "$1\u{2026}$1")
}

fn descriptor(text: &str) -> String {
    truncate_descriptor(js_trim(&collapse_whitespace(&redact_noise(text))))
}

/// Strip a leading ! magic and a leading cd-prefix chain segment.
fn strip_bash_prefix(line: &str) -> String {
    let no_magic = re(&format!(r"^{S}*!")).replace(line, "");
    let trimmed = js_trim(&no_magic);
    let no_cd = re(&format!(r"^{S}*cd{S}+([^&;|]+)(?:&&|;){S}*")).replace(trimmed, "");
    js_trim(&no_cd).to_string()
}

fn is_comment_line(line: &str) -> bool {
    re(&format!(r"^{S}*#")).is_match(line)
}

fn is_skippable_bash_line(line: &str) -> bool {
    let trimmed = js_trim(line);
    trimmed.is_empty()
        || is_comment_line(trimmed)
        || re(&format!(
            r"^{S}*set{S}+[-+][A-Za-z]*(?:{S}+[-+]?{W}+)*(?:{S}+pipefail)?{S}*$"
        ))
        .is_match(trimmed)
        || re(&format!(r"^(?:export{S}+{W}+=|source{S}+\S+|\.{S}+\S+)")).is_match(trimmed)
}

/// Split a command line into shell-quoted words.
fn shell_words(line: &str) -> Vec<String> {
    let re_words = re(r#""([^"]*)"|'([^']*)'|(\S+)"#);
    re_words
        .captures_iter(line)
        .filter_map(|c| {
            c.get(1)
                .or_else(|| c.get(2))
                .or_else(|| c.get(3))
                .map(|g| g.as_str().to_string())
        })
        .collect()
}

/// Drop a leading ./ from a path.
fn path_tail(path: &str) -> String {
    re(r"^\./").replace(path, "")
}

/// Shorten well-known runner invocations for display.
fn simplify_runner_command(line: &str) -> Option<String> {
    let words = shell_words(line);
    let joined = words.join(" ");
    let vitest_index = words
        .iter()
        .position(|w| re(r"(?:^|/)vitest/dist/cli\.js$").is_match(w));
    if words.first().map(String::as_str) == Some("npx")
        && words.get(1).map(String::as_str) == Some("tsx")
    {
        if let Some(vi) = vitest_index.filter(|&i| i >= 2) {
            return Some(
                format!("vitest {}", words[vi + 1..].join(" "))
                    .trim()
                    .to_string(),
            );
        }
    }
    if words.first().map(String::as_str) == Some("npm") {
        let prefix_index = words.iter().position(|w| w == "--prefix");
        let cwd = prefix_index.and_then(|i| words.get(i + 1)).cloned();
        if let Some(ri) = words.iter().position(|w| w == "run") {
            if let Some(next) = words.get(ri + 1) {
                let command = format!("npm {} {}", next, words[ri + 2..].join(" "))
                    .trim()
                    .to_string();
                return cwd
                    .map(|cwd| format!("{command} ({})", path_tail(&cwd)))
                    .or(Some(command));
            }
        }
    }
    if words.first().map(String::as_str) == Some("pnpm") {
        let cwd_index = words.iter().position(|w| w == "-C" || w == "--dir");
        let cwd = cwd_index.and_then(|i| words.get(i + 1)).cloned();
        if let Some(ci) = cwd_index {
            let rest: Vec<String> = words
                .into_iter()
                .enumerate()
                .filter(|(i, _)| *i != ci && *i != ci + 1)
                .map(|(_, w)| w)
                .collect();
            return cwd.map(|cwd| format!("{} ({})", rest.join(" "), path_tail(&cwd)));
        }
        return None;
    }
    // TS findIndex: word === "pytest" (the -m clause is unreachable there).
    if words.first().map(String::as_str) == Some("uv")
        && words.get(1).map(String::as_str) == Some("run")
    {
        if let Some(pi) = words.iter().position(|w| w == "pytest") {
            return Some(
                format!("pytest {}", words[pi + 1..].join(" "))
                    .trim()
                    .to_string(),
            );
        }
    }
    if matches!(
        words.first().map(String::as_str),
        Some("python") | Some("python3")
    ) && words.get(1).map(String::as_str) == Some("-m")
        && words.get(2).map(String::as_str) == Some("pytest")
    {
        return Some(
            format!("pytest {}", words[3..].join(" "))
                .trim()
                .to_string(),
        );
    }
    if joined.contains("node_modules/.bin/") {
        return Some(re(r"\S*node_modules/\.bin/").replace_all(&joined, ""));
    }
    None
}

/// Shorten file-mutation commands (cat >, tee, apply_patch) for display.
fn simplify_mutation_command(line: &str) -> Option<String> {
    let words = shell_words(line);
    if words.is_empty() {
        return None;
    }
    let first = words[0].as_str();
    if first == "cat" && words.get(1).map(String::as_str) == Some(">") && words.len() > 2 {
        return Some(format!("write {}", path_tail(&words[2])));
    }
    if first == "tee" {
        if let Some(last) = words.last() {
            let action = if words.iter().any(|w| w == "-a") {
                "append"
            } else {
                "write"
            };
            return Some(format!("{action} {}", path_tail(last)));
        }
    }
    if first == "apply_patch" {
        return Some("apply patch".to_string());
    }
    if matches!(first, "rm" | "mv" | "cp" | "git" | "npm") {
        return Some(line.to_string());
    }
    if (first == "sed" && words.iter().any(|w| w.starts_with("-i")))
        || (first == "perl" && words.iter().any(|w| w == "-pi"))
    {
        return Some(line.to_string());
    }
    None
}

fn simplify_bash_command_line(line: &str) -> String {
    simplify_runner_command(line)
        .or_else(|| simplify_mutation_command(line))
        .unwrap_or_else(|| line.to_string())
}

/// Split a && b; c into its command segments.
fn split_command_chain(line: &str) -> Vec<String> {
    re(r"\s*(?:&&|;)\s*")
        .split(line)
        .into_iter()
        .map(|p| js_trim(p).to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn heredoc_body(lines: &[String], start_index: usize, delimiter: &str) -> Option<String> {
    // While args stream, preview the partial heredoc body rather than the
    // low-signal heredoc opener.
    let mut body: Vec<&str> = Vec::new();
    for line in lines.iter().skip(start_index + 1) {
        if js_trim(line) == delimiter {
            return Some(body.join("\n"));
        }
        body.push(line);
    }
    if body.is_empty() {
        None
    } else {
        Some(body.join("\n"))
    }
}

fn preview_heredoc(lines: &[String]) -> Option<CodePreview> {
    // A generic heredoc body is low-signal; keep it as a fallback and prefer a
    // later, more specific heredoc (python/bash/node/write) if one follows.
    let mut fallback: Option<CodePreview> = None;
    for (i, raw) in lines.iter().enumerate() {
        let line = strip_bash_prefix(raw);
        if is_skippable_bash_line(&line) {
            continue;
        }
        let captures = re(r#"<<-?\s*['"]?([A-Za-z_][A-Za-z0-9_]*)['"]?"#).captures(&line);
        let delimiter = captures
            .as_ref()
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string());
        let delimiter = match delimiter {
            Some(d) => d,
            None => continue,
        };
        let body = match heredoc_body(lines, i, &delimiter) {
            Some(b) => b,
            None => continue,
        };
        if re(&format!(r"\b(?:uv{S}+run{S}+)?python3?\b")).is_match(&line) {
            let preview = preview_python_code(&body);
            if !preview.text.is_empty() {
                return Some(preview);
            }
            continue;
        }
        // Match bash/sh as an interpreter word (incl. /bin/sh), not a path
        // suffix like script.sh.
        if re(r"(?<![\w.])(?:bash|sh)\b").is_match(&line) {
            let preview = preview_bash_command(&body);
            if !preview.text.is_empty() {
                return Some(preview);
            }
            return Some(CodePreview::bash(descriptor(&body)));
        }
        if re(r"\bnode\b").is_match(&line) {
            return Some(CodePreview::bash(format!("node: {}", descriptor(&body))));
        }
        let cat_write = re(r"\b(?:cat|tee)\b.*(?:>|\s)(\S+)\s*<<-?")
            .captures(&line)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string());
        if let Some(target) = cat_write {
            let action = if line.contains("tee -a") {
                "append"
            } else {
                "write"
            };
            return Some(CodePreview::bash(format!(
                "{action} {}",
                path_tail(&target)
            )));
        }
        if re(r"\bapply_patch\b").is_match(&line) {
            return Some(CodePreview::bash("apply patch".to_string()));
        }
        fallback = Some(CodePreview::bash(descriptor(&body)));
    }
    fallback
}

fn bash_line_score(line: &str, index: usize) -> usize {
    let simplified = simplify_bash_command_line(line);
    let words = shell_words(line);
    let mut score = 30usize;
    if simplified != line {
        score += 40;
    }
    if words.first().is_some_and(|w| {
        matches!(
            w.as_str(),
            "rm" | "mv" | "cp" | "git" | "npm" | "pnpm" | "pytest" | "vitest"
        )
    }) {
        score += 20;
    }
    if re(&format!(r"\b(?:rm|mv|cp|git{S}+(?:add|commit)|npm{S}+install|sed{S}+-i|perl{S}+-pi|tee|cat{S}*>|apply_patch)\b"))
        .is_match(line)
    {
        score += 40;
    }
    score + index
}

/// Pick the highest-signal line of a bash command as its preview.
pub fn preview_bash_command(command: &str) -> CodePreview {
    let lines: Vec<String> = command.split('\n').map(String::from).collect();
    let heredoc = preview_heredoc(&lines);
    if let Some(h) = heredoc.filter(|h| !h.text.is_empty()) {
        return CodePreview {
            language: h.language,
            text: descriptor(&h.text),
        };
    }

    let mut best: Option<(String, usize)> = None;
    let mut index = 0usize;
    for raw_line in &lines {
        for raw_part in split_command_chain(raw_line) {
            let command_line = strip_bash_prefix(js_trim(&raw_part));
            if command_line.is_empty() || is_skippable_bash_line(&command_line) {
                continue;
            }
            let text = simplify_bash_command_line(&command_line);
            let score = bash_line_score(&command_line, index);
            if best.as_ref().is_none_or(|(_, s)| score > *s) {
                best = Some((text, score));
            }
            index += 1;
        }
    }
    CodePreview::bash(best.map_or(String::new(), |(text, _)| descriptor(&text)))
}

fn is_skippable_python_line(line: &str) -> bool {
    let trimmed = js_trim(line);
    trimmed.is_empty()
        || is_comment_line(trimmed)
        || re(&format!(r"^{S}*(?:import{S}+\S|from{S}+\S+{S}+import{S}+)")).is_match(trimmed)
}

fn python_indent(line: &str) -> usize {
    re(&format!(r"^{S}*"))
        .find(line)
        .map(|m| m.as_str().chars().count())
        .unwrap_or(0)
}

fn python_call_pattern(inner: &str) -> bool {
    re(&format!(
        r"^{S}*(?:await{S}+)?[A-Za-z_][A-Za-z0-9_.]*{S}*\("
    ))
    .is_match(inner)
}

fn python_low_signal_call_pattern(inner: &str) -> bool {
    re(&format!(
        r"^{S}*(?:await{S}+)?(?:print|len|str|repr|int|float|list|dict|set|tuple){S}*\("
    ))
    .is_match(inner)
}

fn python_print_inner_call(line: &str) -> Option<String> {
    let trimmed = js_trim(line);
    let inner = re(r"^print\((.*)\)$")
        .captures(trimmed)
        .and_then(|c| c.get(1))
        .map(|m| js_trim(m.as_str()).to_string());
    inner.filter(|inner| python_call_pattern(inner))
}

fn python_path_vars(lines: &[String]) -> std::collections::HashMap<String, String> {
    let mut vars = std::collections::HashMap::new();
    let path_assign = re(&format!(
        r#"^{S}*([A-Za-z_][A-Za-z0-9_]*){S}*={S}*(?:Path|pathlib\.Path)\((["'])([^"']+)\2\)"#
    ));
    let string_assign = re(&format!(
        r#"^{S}*([A-Za-z_][A-Za-z0-9_]*){S}*={S}*(["'])([^"']+)\2"#
    ));
    for line in lines {
        let m = path_assign
            .captures(line)
            .or_else(|| string_assign.captures(line));
        if let Some(c) = m {
            let name = c.get(1).map(|g| g.as_str().to_string());
            let value = c.get(3).map(|g| g.as_str().to_string());
            if let (Some(name), Some(value)) = (name, value) {
                if value.contains('/') {
                    vars.insert(name, value);
                }
            }
        }
    }
    vars
}

fn python_file_operation(
    line: &str,
    paths: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let m = re(
        r"^(?:await\s+)?([A-Za-z_][A-Za-z0-9_]*)\.(write_text|write_bytes|read_text|read_bytes|mkdir|unlink|rename|replace|touch)\s*\(",
    )
    .captures(js_trim(line))?;
    let name = m.get(1)?.as_str();
    let method = m.get(2)?.as_str();
    let path = paths.get(name)?;
    let action = match method {
        "write_text" | "write_bytes" => "write",
        "read_text" | "read_bytes" => "read",
        "mkdir" => "mkdir",
        "unlink" => "delete",
        "rename" => "rename",
        "replace" => "replace",
        "touch" => "touch",
        other => other,
    };
    Some(format!("{action} {}", path_tail(path)))
}

fn python_subprocess_command(line: &str) -> Option<String> {
    let trimmed = js_trim(line);
    if let Some(c) = re(r#"subprocess\.(?:run|check_call|check_output|Popen)\(\s*(["`])([^"`]+)\1"#)
        .captures(trimmed)
    {
        return c.get(2).map(|g| simplify_bash_command_line(g.as_str()));
    }
    if let Some(c) =
        re(r"subprocess\.(?:run|check_call|check_output|Popen)\(\s*\[([^\]]+)\]").captures(trimmed)
    {
        let inner = c.get(1)?.as_str();
        let words_re = re(r#"["']([^"']+)["']"#);
        let words: Vec<&str> = words_re
            .captures_iter(inner)
            .filter_map(|c| c.get(1).map(|g| g.as_str()))
            .collect();
        return Some(simplify_bash_command_line(&words.join(" ")));
    }
    None
}

fn simplify_python_preview_line(
    line: &str,
    paths: &std::collections::HashMap<String, String>,
) -> String {
    python_file_operation(line, paths)
        .or_else(|| python_subprocess_command(line))
        .or_else(|| python_print_inner_call(line))
        .unwrap_or_else(|| js_trim(line).to_string())
}

fn python_preview_line(
    lines: &[String],
    index: usize,
    paths: &std::collections::HashMap<String, String>,
) -> String {
    let line = lines.get(index).map(String::as_str).unwrap_or("");
    if index > 0 && re(&format!(r"^{S}*(?:async{S}+def|def|class){S}+")).is_match(line) {
        let previous = lines.get(index - 1).map(String::as_str).unwrap_or("");
        if re(&format!(r"^{S}*@")).is_match(js_trim(previous)) {
            return format!("{} {}", js_trim(previous), js_trim(line));
        }
    }
    if re(&format!(
        r"^{S}*(?:if|elif|else|for|while|with|try|except|finally)\b.*:\s*$"
    ))
    .is_match(line)
    {
        if let Some(child_index) = first_python_child_line(lines, index) {
            let head = re(r":\s*$").replace(js_trim(line), ":");
            let child = lines.get(child_index).cloned().unwrap_or_default();
            return format!("{head} {}", simplify_python_preview_line(&child, paths));
        }
    }
    simplify_python_preview_line(line, paths)
}

fn first_python_child_line(lines: &[String], parent_index: usize) -> Option<usize> {
    let parent_line = lines.get(parent_index).cloned().unwrap_or_default();
    let parent_indent = python_indent(&parent_line);
    for (i, line) in lines.iter().enumerate().skip(parent_index + 1) {
        if is_skippable_python_line(line) || re(&format!(r"^{S}*@")).is_match(js_trim(line)) {
            continue;
        }
        if python_indent(line) <= parent_indent {
            return None;
        }
        return Some(i);
    }
    None
}

fn python_line_score(
    lines: &[String],
    index: usize,
    paths: &std::collections::HashMap<String, String>,
) -> i64 {
    let line = lines.get(index).cloned().unwrap_or_default();
    let trimmed = js_trim(&line).to_string();
    if is_skippable_python_line(&line)
        || re(&format!(r"^{S}*@")).is_match(&trimmed)
        || re(r"^[)\]},;\s]+(?:#.*)?$").is_match(&trimmed)
    {
        return -1;
    }
    if python_file_operation(&line, paths).is_some() {
        return 95;
    }
    if python_subprocess_command(&line).is_some() {
        return 90;
    }
    if re(&format!(
        r#"^{S}*if{S}+__name__{S}*=={S}*['"]__main__['"]{S}*:"#
    ))
    .is_match(&line)
    {
        return 70;
    }
    if re(&format!(
        r"^{S}*(?:await{S}+)?[A-Za-z_][A-Za-z0-9_.]*\.(?:write_text|write_bytes|mkdir|unlink|rename|replace|touch|append|extend|update|add|remove|discard|close|commit|execute|run){S}*\("
    ))
    .is_match(&line)
    {
        return 80;
    }
    if re(&format!(
        r"^{S}*(?:if|elif|else|for|while|with|try|except|finally)\b.*:\s*$"
    ))
    .is_match(&line)
    {
        return match first_python_child_line(lines, index) {
            None => 20,
            Some(child_index) => (python_line_score(lines, child_index, paths) - 5).max(20),
        };
    }
    if re(&format!(r"^{S}*(?:async{S}+def|def|class){S}+")).is_match(&line) {
        return 50;
    }
    if re(&format!(
        r#"^{S}*[A-Za-z_][A-Za-z0-9_]*(?:{S}*:\s*[^=]+)?{S}*={S}*(?:await{S}+)?(?:Path|pathlib\.Path|json\.loads|json\.dumps|str|int|float|list|dict|set|tuple){S}*\("#
    ))
    .is_match(&line)
    {
        return 25;
    }
    let print_inner_call = python_print_inner_call(&line);
    let is_low_signal_call = print_inner_call
        .as_deref()
        .is_some_and(python_low_signal_call_pattern);
    if print_inner_call.is_some() && !is_low_signal_call {
        return 55;
    }
    if re(&format!(
        r"^{S}*[A-Za-z_][A-Za-z0-9_]*(?:{S}*:\s*[^=]+)?{S}*={S}*(?:await{S}+)?[A-Za-z_][A-Za-z0-9_.]*{S}*\("
    ))
    .is_match(&line)
    {
        return 60;
    }
    let matches_call = python_call_pattern(&line);
    let matches_low_signal = python_low_signal_call_pattern(&line);
    if matches_call && !matches_low_signal {
        return 65;
    }
    if matches_call {
        return 15;
    }
    30
}

fn python_preview_index(lines: &[String], index: usize) -> usize {
    let line = lines.get(index).cloned().unwrap_or_default();
    if !re(&format!(
        r"^{S}*(?:if|elif|else|for|while|with|try|except|finally)\b.*:\s*$"
    ))
    .is_match(&line)
    {
        return index;
    }
    match first_python_child_line(lines, index) {
        None => index,
        Some(child_index) => python_preview_index(lines, child_index),
    }
}

struct PythonStringScan {
    value: String,
    end: usize,
    closed: bool,
    /// Saw a cooked escape whose value is not computed here.
    unsupported_escape: bool,
}

fn is_unsupported_escape_char(ch: char) -> bool {
    matches!(
        ch,
        'x' | 'u' | 'U' | 'N' | 'a' | 'b' | 'f' | 'v' | '0'..='7'
    )
}

/// Walk a python string-literal body from just after the opening delimiter,
/// following python's escape rules (in raw strings backslash-quote never
/// closes). Offsets index bytes into `code`.
fn scan_python_string_literal(
    code: &str,
    start: usize,
    quote: &str,
    raw: bool,
) -> PythonStringScan {
    let mut value = String::new();
    let mut i = start;
    let mut unsupported_escape = false;
    let quote_bytes = quote.as_bytes();
    while i < code.len() {
        let ch = code[i..].chars().next().expect("char boundary");
        if ch == '\\' && i + 1 < code.len() {
            let next = code[i + 1..].chars().next().expect("char boundary");
            if !raw {
                if is_unsupported_escape_char(next) {
                    unsupported_escape = true;
                }
                match next {
                    '\n' => {} // backslash-newline is a line continuation
                    '"' => value.push('"'),
                    '\'' => value.push('\''),
                    '\\' => value.push('\\'),
                    'n' => value.push('\n'),
                    'r' => value.push('\r'),
                    't' => value.push('\t'),
                    other => {
                        value.push('\\');
                        value.push(other);
                    }
                }
            } else {
                value.push('\\');
                value.push(next);
            }
            i += 1 + next.len_utf8();
            continue;
        }
        if code.as_bytes()[i..].starts_with(quote_bytes) {
            return PythonStringScan {
                value,
                end: i + quote.len(),
                closed: true,
                unsupported_escape,
            };
        }
        if quote.len() == 1 && ch == '\n' {
            break; // single-quoted literals cannot span lines
        }
        value.push(ch);
        i += ch.len_utf8();
    }
    PythonStringScan {
        value,
        end: i,
        closed: false,
        unsupported_escape,
    }
}

/// Keep source-line positions while masking multiline-string continuations.
pub fn python_statement_lines(code: &str) -> Vec<String> {
    let mut lines: Vec<String> = code.split('\n').map(String::from).collect();
    let mut line = 0usize;
    let mut i = 0usize;
    while i < code.len() {
        let ch = code[i..].chars().next().expect("char boundary");
        if ch == '#' {
            let Some(nl) = code[i..].find('\n') else {
                break;
            };
            i += nl;
            continue;
        }
        if ch == '"' || ch == '\'' {
            let quote = if code[i..].starts_with(&ch.to_string().repeat(3)) {
                ch.to_string().repeat(3)
            } else {
                ch.to_string()
            };
            let scan = scan_python_string_literal(code, i + quote.len(), &quote, true);
            let start_line = line;
            for end in i..scan.end {
                if code.as_bytes().get(end) == Some(&b'\n') {
                    line += 1;
                    while lines.len() <= line {
                        lines.push(String::new());
                    }
                    lines[line] = String::new();
                }
            }
            if scan.closed && line > start_line {
                let column = scan.end - (code[..scan.end].rfind('\n').map(|p| p + 1).unwrap_or(0));
                let rest_start = scan.end;
                let rest_end = code[rest_start..]
                    .find('\n')
                    .map(|p| rest_start + p)
                    .unwrap_or(code.len());
                while lines.len() <= line {
                    lines.push(String::new());
                }
                lines[line] = format!("{}{}", " ".repeat(column), &code[rest_start..rest_end]);
            }
            i = scan.end;
            continue;
        }
        if ch == '\n' {
            line += 1;
        }
        i += ch.len_utf8();
    }
    lines
}

fn extract_bash_skill_command(code: &str) -> Option<String> {
    let triple_double = "\"\"\"";
    let triple_single = concat!("''", "'");
    let m = re(&format!(
        r#"{S}*(?:[A-Za-z_][A-Za-z0-9_]*{S}*={S}*)?(?:await{S}+)?bash{S}*\({S}*[rR]?({}|{}|"|')"#,
        triple_double, triple_single
    ))
    .captures(code)?;
    let quote = m.get(1)?.as_str();
    let start = m.get(0)?.end();
    // The character before the quote (r/R) marks a raw literal.
    let head: Vec<char> = code[..start].chars().collect();
    let quote_chars = quote.chars().count();
    let prefix_char = if head.len() > quote_chars {
        head[head.len() - quote_chars - 1]
    } else {
        ' '
    };
    let raw = matches!(prefix_char, 'r' | 'R');
    let scan = scan_python_string_literal(code, start, quote, raw);
    if !scan.closed || scan.unsupported_escape {
        return None;
    }
    let rest = code[scan.end..].trim_start();
    // Require a plain literal first argument; concatenation or other
    // expressions fall back.
    if !rest.starts_with(',') && !rest.starts_with(')') {
        return None;
    }
    Some(scan.value)
}

/// Pick the highest-signal line of a python cell as its preview.
pub fn preview_python_code(code: &str) -> CodePreview {
    let raw_lines: Vec<String> = code.split('\n').map(String::from).collect();
    let lines: Vec<String> = python_statement_lines(code)
        .into_iter()
        .map(|line| re(&format!(r"^({S}*);{S}*")).replace(&line, "$1"))
        .collect();
    let paths = python_path_vars(&lines);
    let mut best_index: Option<usize> = None;
    let mut best_score: i64 = -1;

    for (i, _) in lines.iter().enumerate() {
        let score = python_line_score(&lines, i, &paths);
        if score > best_score {
            best_index = Some(i);
            best_score = score;
        }
    }

    if let Some(best_index) = best_index.filter(|_| best_score >= 0) {
        let preview_index = python_preview_index(&lines, best_index);
        // Keep the original tail for multiline commands, excluding any
        // preceding string continuation.
        let mut joined = String::new();
        if let Some(first) = lines.get(preview_index) {
            joined.push_str(first);
        }
        for raw in raw_lines.iter().skip(preview_index + 1) {
            joined.push('\n');
            joined.push_str(raw);
        }
        if let Some(bash_command) = extract_bash_skill_command(&joined) {
            return preview_bash_command(&bash_command);
        }
        let text = python_preview_line(&lines, preview_index, &paths);
        return CodePreview::python(descriptor(&text));
    }
    CodePreview::python(String::new())
}

/// Preview an ipython cell: %%bash cells preview as bash, the rest as python.
pub fn preview_ipython_code(code: &str) -> CodePreview {
    let trimmed = js_trim_end(code);
    if let Some(cell) = parse_ipython_bash_cell(trimmed) {
        return preview_bash_command(&cell.body);
    }
    preview_python_code(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_truncates_at_64_units() {
        let long = "hello world ".repeat(12);
        let d = descriptor(&long);
        assert_eq!(utf16_len(&d), 64);
        assert!(d.ends_with('\u{2026}'));
    }

    #[test]
    fn redact_hides_tokens_and_blobs() {
        let text = "api_token = \"abc123\" and 01234567890123456789012345678901234567890123456789012345678901234567890123456789";
        let red = redact_noise(text);
        assert!(red.contains("api_token=<redacted>"));
        assert!(red.contains("<blob>"));
    }

    #[test]
    fn bash_preview_prefers_mutation_line() {
        let p = preview_bash_command("echo start && git commit -m x && ls");
        assert_eq!(p.text, "git commit -m x");
        assert_eq!(p.language, CodePreviewLanguage::Bash);
    }

    #[test]
    fn bash_preview_simplifies_npm_run() {
        let p = preview_bash_command("npm run test -- --watch");
        assert_eq!(p.text, "npm test -- --watch");
    }

    #[test]
    fn bash_preview_heredoc_python() {
        let cmd = "python3 <<'EOF'\nprint('hi')\nEOF";
        let p = preview_bash_command(cmd);
        assert_eq!(p.text, "print('hi')");
    }

    #[test]
    fn python_preview_file_operation() {
        let code = "p = Path('src/a.ts')\nprint('x')\np.write_text('data')";
        let p = preview_python_code(code);
        assert_eq!(p.text, "write src/a.ts");
        assert_eq!(p.language, CodePreviewLanguage::Python);
    }

    #[test]
    fn python_preview_bash_skill_call() {
        let code = "print('x')\nawait bash(\"git status --porcelain\")";
        let p = preview_python_code(code);
        assert_eq!(p.language, CodePreviewLanguage::Bash);
        assert_eq!(p.text, "git status --porcelain");
    }

    #[test]
    fn ipython_cell_magic_preview() {
        let p = preview_ipython_code("%%bash\necho hi");
        assert_eq!(p.language, CodePreviewLanguage::Bash);
        assert_eq!(p.text, "echo hi");
    }

    #[test]
    fn statement_lines_mask_multiline_strings() {
        let code = "x = \"\"\"\na\nb\n\"\"\"\nrun(1)";
        let lines = python_statement_lines(code);
        assert_eq!(lines[0], "x = \"\"\"");
        assert_eq!(lines[3].trim(), "");
        assert_eq!(lines[4], "run(1)");
    }
}
