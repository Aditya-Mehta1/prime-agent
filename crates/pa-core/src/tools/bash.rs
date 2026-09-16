//! The `bash` tool: shell command execution with output truncation and a
//! destructive-git dirty-tree guard.
//!
//! Port of `packages/coding-agent/src/core/tools/bash.ts` (TUI renderers
//! excluded; execution, guard, truncation, and formatting are identical).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use crate::tools::output_accumulator::{OutputAccumulator, OutputAccumulatorOptions};
use crate::tools::shell_utils::{get_shell_config, get_shell_env, kill_process_tree};
use crate::tools::tool_definition::{
    AbortSignal, OnUpdate, ToolContentBlock, ToolDefinition, ToolExecutionResult, ToolUpdate,
};
use crate::tools::truncate::{
    format_size, TruncatedBy, TruncationResult, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
};

/// Bypass env var for the destructive-git dirty-tree guard.
pub const BASH_DESTRUCTIVE_GIT_BYPASS_ENV: &str = "PI_BASH_ALLOW_DESTRUCTIVE_GIT";

const GIT_STATUS_PORCELAIN_COMMAND: &str = "git status --porcelain --untracked-files=all";

/// How many dirty paths the refusal lists before eliding the rest.
const MAX_DIRTY_PATHS_LISTED: usize = 10;

/// Throttle for streamed output updates (TS: `BASH_UPDATE_THROTTLE_MS`).
const BASH_UPDATE_THROTTLE_MS: Duration = Duration::from_millis(100);

type ExecFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<Option<i32>>> + Send + 'a>>;

/// Execution options for [`BashOperations::exec`].
pub struct ExecOptions<'a> {
    /// Receives streamed stdout/stderr chunks.
    pub on_data: &'a (dyn Fn(&[u8]) + Send + Sync),
    /// Cancellation handle; kills the process tree when fired.
    pub signal: Option<AbortSignal>,
    /// Timeout in seconds.
    pub timeout: Option<f64>,
    /// Environment for the child; defaults to the agent shell env.
    pub env: Option<HashMap<String, String>>,
}

/// Pluggable operations for the bash tool (TS: `BashOperations`).
pub trait BashOperations: Send + Sync {
    /// Execute a command and stream output; resolves with the exit code
    /// (`None` when killed by a signal), or an error:
    /// `"aborted"` or `"timeout:<seconds>"`.
    fn exec<'a>(
        &'a self,
        command: &'a str,
        cwd: &'a str,
        options: ExecOptions<'a>,
    ) -> ExecFuture<'a>;
}

/// Local shell execution backend (TS: `createLocalBashOperations`).
#[derive(Default)]
pub struct LocalBashOperations {
    pub shell_path: Option<String>,
}

impl BashOperations for LocalBashOperations {
    fn exec<'a>(
        &'a self,
        command: &'a str,
        cwd: &'a str,
        options: ExecOptions<'a>,
    ) -> ExecFuture<'a> {
        let shell_path = self.shell_path.clone();
        let signal = options.signal;
        let timeout = options.timeout;
        let env = options.env;
        Box::pin(async move {
            let shell = get_shell_config(shell_path.as_deref())?;

            if !std::path::Path::new(cwd).exists() {
                anyhow::bail!(
                    "Working directory does not exist: {cwd}\nCannot execute bash commands."
                );
            }

            let mut process = std::process::Command::new(&shell.shell);
            for arg in &shell.args {
                process.arg(arg);
            }
            process
                .arg(command)
                .current_dir(cwd)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            for (key, value) in env.unwrap_or_else(get_shell_env) {
                process.env(key, value);
            }
            // Detached process group on POSIX so kill_process_tree can reach
            // descendants (node: `detached: process.platform !== "win32"`).
            {
                use std::os::unix::process::CommandExt;
                process.process_group(0);
            }

            let mut child = process.spawn()?;

            let pid = child.id() as i32;
            let stdout = child.stdout.take().expect("piped stdout");
            let stderr = child.stderr.take().expect("piped stderr");

            // Chunks from both pipes flow through one channel preserving arrival
            // order, like node's stream events.
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    use std::io::Read;
                    let mut reader = std::io::BufReader::new(stdout);
                    let mut buf = [0u8; 8192];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if tx.send(buf[..n].to_vec()).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
            std::thread::spawn(move || {
                use std::io::Read;
                let mut reader = std::io::BufReader::new(stderr);
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                }
            });

            let mut waiter = tokio::task::spawn_blocking(move || child.wait());
            let timeout_deadline = timeout
                .filter(|t| *t > 0.0)
                .map(|t| tokio::time::Instant::now() + Duration::from_secs_f64(t));
            let mut abort_watcher = signal
                .as_ref()
                .map(|signal| Box::pin(signal.clone().cancelled_owned()));

            // Abort before spawn kills immediately (node checks signal.aborted
            // synchronously and attaches the abort listener otherwise).
            if let Some(signal) = &signal {
                if signal.is_cancelled() {
                    kill_process_tree(pid);
                }
            }

            let timed_out = Arc::new(AtomicBool::new(false));
            let exit_status: std::io::Result<std::process::ExitStatus> = 'wait: {
                loop {
                    tokio::select! {
                        status = &mut waiter => {
                            break 'wait status?;
                        }
                        _ = async {
                            match timeout_deadline {
                                Some(deadline) => tokio::time::sleep_until(deadline).await,
                                None => std::future::pending::<()>().await,
                            }
                        }, if !timed_out.load(Ordering::SeqCst) && timeout_deadline.is_some() => {
                            timed_out.store(true, Ordering::SeqCst);
                            kill_process_tree(pid);
                        }
                        _ = async {
                            match abort_watcher.as_mut() {
                                Some(cancelled) => cancelled.as_mut().await,
                                None => std::future::pending::<()>().await,
                            }
                        }, if abort_watcher.is_some() && !signal.as_ref().map(|s| s.is_cancelled()).unwrap_or(false) => {
                            kill_process_tree(pid);
                        }
                        Some(chunk) = rx.recv() => {
                            (options.on_data)(&chunk);
                        }
                    }
                }
            };
            // Drain chunks that raced ahead of process exit: the pipe
            // readers close at EOF (process death), so this always finishes.
            while let Some(chunk) = rx.recv().await {
                (options.on_data)(&chunk);
            }

            let status = exit_status.map_err(|err| anyhow::anyhow!("{err}"))?;

            if let Some(signal) = &signal {
                if signal.is_cancelled() {
                    anyhow::bail!("aborted");
                }
            }
            if timed_out.load(Ordering::SeqCst) {
                anyhow::bail!("timeout:{}", timeout.unwrap_or_default());
            }
            Ok(status.code())
        })
    }
}

/// Context for the spawn hook: command, cwd, env.
pub struct BashSpawnContext {
    pub command: String,
    pub cwd: String,
    pub env: HashMap<String, String>,
}

pub type BashSpawnHook = Arc<dyn Fn(BashSpawnContext) -> BashSpawnContext + Send + Sync>;

fn resolve_spawn_context(
    command: &str,
    cwd: &str,
    spawn_hook: Option<&BashSpawnHook>,
) -> BashSpawnContext {
    let base = BashSpawnContext {
        command: command.to_string(),
        cwd: cwd.to_string(),
        env: get_shell_env(),
    };
    match spawn_hook {
        Some(hook) => hook(base),
        None => base,
    }
}

/// Options for the bash tool.
#[derive(Default)]
pub struct BashToolOptions {
    /// Custom operations for command execution. Default: local shell.
    pub operations: Option<Arc<dyn BashOperations>>,
    /// Command prefix prepended to every command.
    pub command_prefix: Option<String>,
    /// Optional explicit shell path from settings.
    pub shell_path: Option<String>,
    /// Hook to adjust command, cwd, or env before execution.
    pub spawn_hook: Option<BashSpawnHook>,
}

// ---------------------------------------------------------------------------
// Destructive-git discard detection
// ---------------------------------------------------------------------------

/// JavaScript `\s` character class.
const S: &str = r"[\t\n\x0B\f\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]";

/// JS `str.split(/\s+/)` (leading/trailing empty tokens removed).
fn split_ws(text: &str) -> Vec<&str> {
    let pattern = format!("^{S}+|{S}+");
    let re = fancy_regex::Regex::new(&pattern).expect("split_ws regex");
    let mut parts = Vec::new();
    let mut rest = text;
    // Split on runs of whitespace, discarding empty leading segments.
    while let Some(m) = re.find(rest).ok().flatten() {
        if m.start() > 0 {
            parts.push(&rest[..m.start()]);
        }
        rest = &rest[m.end()..];
    }
    if !rest.is_empty() {
        parts.push(rest);
    }
    parts
}

/// git global options between `git` and the subcommand.
const GIT_GLOBAL_OPTIONS: &str = r#"(?:-{1,2}[^\s;&|]+(?:\s+(?:"[^"]*"|'[^']*'|[^\s;&|]+))?\s+)*"#;

fn discard_checkout_pattern() -> fancy_regex::Regex {
    fancy_regex::Regex::new(&format!(
        r"\bgit\s+{GIT_GLOBAL_OPTIONS}checkout\s+(?:(?:(?:-[fm]|--ours|--theirs|--conflict=\S+)\s+)*(?:--\s+)?(?:\.\/?|:\/)|[^\s;&|()]+\s+(?:--\s+)?(?:\.\/?|:\/)|(?:-f|--force)\s+[^\s;&|()]+)(?=\s|$|[;&|)])"
    ))
    .expect("checkout discard regex")
}

fn discard_restore_pattern() -> fancy_regex::Regex {
    fancy_regex::Regex::new(&format!(
        r"\bgit\s+{GIT_GLOBAL_OPTIONS}restore\s+(?:(?:--source|--worktree)(?:=\S+)?\s+|-s(?:\s+\S+|[^\s]+)\s+|-W\s+|--\s+)?(?:\.\/?|:\/)(?=\s|$|[;&|)])"
    ))
    .expect("restore discard regex")
}

fn discard_reset_pattern() -> fancy_regex::Regex {
    fancy_regex::Regex::new(&format!(
        r"\bgit\s+{GIT_GLOBAL_OPTIONS}reset\s+(?:(?:-[^\s;&|]+)\s+)*--hard\b"
    ))
    .expect("reset discard regex")
}

fn discard_clean_pattern() -> fancy_regex::Regex {
    fancy_regex::Regex::new(&format!(r"\bgit\s+{GIT_GLOBAL_OPTIONS}clean\s+([^;&|]*)"))
        .expect("clean discard regex")
}

fn is_forced_clean_segment(args: &str) -> bool {
    let tokens: Vec<&str> = split_ws(args);
    let option_end = tokens.iter().position(|t| *t == "--");
    let option_tokens: &[&str] = match option_end {
        Some(end) => &tokens[..end],
        None => &tokens,
    };
    let forces: Vec<&&str> = option_tokens
        .iter()
        .filter(|arg| {
            if arg.starts_with("--") {
                arg.starts_with("--force")
            } else {
                arg.starts_with('-') && arg.contains('f')
            }
        })
        .collect();
    if forces.is_empty() {
        return false;
    }
    !option_tokens.iter().any(|arg| {
        *arg == "--dry-run" || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('n'))
    })
}

/// Replace characters inside single- or double-quoted spans with spaces so the
/// discard matcher cannot match quoted data. Character positions stay
/// identical to the original string. Command substitution (`$(...)`,
/// backticks) is left live because it executes. Unquoted `#` at a word
/// boundary starts a comment, masked to end of line.
fn mask_quoted_spans(command: &str) -> Vec<char> {
    let mut chars: Vec<char> = command.chars().collect();
    let mut quote: Option<char> = None;
    let len = chars.len();
    let mut i = 0usize;
    while i < len {
        let ch = chars[i];
        if quote.is_none() {
            let prev = if i > 0 { Some(chars[i - 1]) } else { None };
            if ch == '#'
                && (i == 0
                    || prev
                        .map(|p| {
                            p.is_whitespace()
                                || matches!(p, ';' | '&' | '|' | '(' | ')' | '{' | '}')
                        })
                        .unwrap_or(true))
            {
                let mut j = i;
                while j < len && chars[j] != '\n' {
                    chars[j] = ' ';
                    j += 1;
                }
                i = j;
                continue;
            }
            if ch == '"' || ch == '\'' {
                quote = Some(ch);
            }
        } else if quote == Some('\'') {
            if ch == '\'' {
                quote = None;
            } else {
                chars[i] = ' ';
            }
        } else if ch == '"' {
            quote = None;
        } else if ch == '\\' && i + 1 < len {
            chars[i] = ' ';
            chars[i + 1] = ' ';
            i += 1;
        } else if ch == '$' && i + 1 < len && chars[i + 1] == '(' {
            let mut depth = 0i32;
            let mut j = i;
            while j < len {
                if chars[j] == '(' {
                    depth += 1;
                } else if chars[j] == ')' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                j += 1;
            }
            i = j.saturating_sub(1);
        } else if ch == '`' {
            let mut j = i + 1;
            while j < len && chars[j] != '`' {
                j += 1;
            }
            i = j.saturating_sub(1);
        } else {
            chars[i] = ' ';
        }
        i += 1;
    }
    chars
}

/// Find every destructive git discard command in `command`, returning the
/// byte index where each `git` token starts (empty when none match).
pub fn find_destructive_git_discard_commands(command: &str) -> Vec<usize> {
    let masked: String = mask_quoted_spans(command).into_iter().collect();
    let mut indices: Vec<usize> = Vec::new();
    for pattern in [
        discard_checkout_pattern(),
        discard_restore_pattern(),
        discard_reset_pattern(),
    ] {
        for m in pattern.find_iter(&masked).flatten() {
            indices.push(m.start());
        }
    }
    for caps in discard_clean_pattern().captures_iter(&masked).flatten() {
        if let Some(args) = caps.get(1) {
            if is_forced_clean_segment(args.as_str()) {
                indices.push(caps.get(0).map(|m| m.start()).unwrap_or_default());
            }
        }
    }
    indices.sort_unstable();
    indices
}

/// True when `command` contains a git discard command.
pub fn is_destructive_git_discard_command(command: &str) -> bool {
    !find_destructive_git_discard_commands(command).is_empty()
}

/// Where a discard command's probe must run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardProbeTarget {
    /// Shell prefix relocating the probe, for example `cd sub && `.
    pub relocation_prefix: Option<String>,
    /// git status command for this discard.
    pub git_status_command: String,
}

/// The probe cannot safely determine the repository the discard targets.
pub const UNRESOLVABLE_DISCARD_TARGET: () = ();

/// Result of resolving a discard command's probe location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscardProbeResolution {
    /// No relocation is needed; probe in the tool cwd.
    NotRelocated,
    /// The target repository cannot be resolved safely; refuse.
    Unresolvable,
    /// Probe must run with the given relocation.
    Target(DiscardProbeTarget),
}

/// Shell separators treated by segment splitting.
const SEPARATORS: [&str; 5] = ["&&", "||", ";", "|", "\n"];

/// Split keeping separators (TS: `split(/(&&|\|\||;|\||\n)/)`).
fn split_with_separators(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();
        if let Some(sep) = SEPARATORS.iter().find(|s| rest.starts_with(**s)) {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            parts.push(sep.to_string());
            i += sep.chars().count();
        } else {
            current.push(chars[i]);
            i += 1;
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Split on separators, dropping them (TS: `split(/&&|\|\||;|\||\n/)`).
fn split_on_separators(text: &str) -> Vec<String> {
    split_with_separators(text)
        .into_iter()
        .filter(|part| !SEPARATORS.contains(&part.as_str()))
        .collect()
}

/// Port of `resolveDiscardProbeTarget` from bash.ts.
pub fn resolve_discard_probe_target(
    command: &str,
    discard_index: usize,
    user_command_start: usize,
) -> DiscardProbeResolution {
    let prefix = &command[..discard_index];
    let invocation = &command[discard_index..];
    // A discard inside the configured command prefix would be replayed by the
    // probe itself; refuse instead of executing it during probing.
    if user_command_start > 0 && discard_index < user_command_start {
        return DiscardProbeResolution::Unresolvable;
    }
    let tokens = split_ws(invocation);

    // git -C <dir> (or repository-relocating global options) on the discard
    // invocation itself.
    let mut dash_c_dir: Option<String> = None;
    let mut subcommand_index: Option<usize> = None;
    for (index, token) in tokens.iter().enumerate() {
        if index == 0 {
            continue; // "git"
        }
        if *token == "reset" || *token == "checkout" || *token == "clean" || *token == "restore" {
            subcommand_index = Some(index);
            break;
        }
        if *token == "-C" {
            let dir = tokens.get(index + 1).copied();
            // A quoted, escaped, or substituted path cannot be replayed as a
            // single token; refuse rather than probe a truncated directory.
            let Some(dir) = dir else {
                return DiscardProbeResolution::Unresolvable;
            };
            if dir.contains(['"', '\'', '\\', '$', '`']) {
                return DiscardProbeResolution::Unresolvable;
            }
            // Repeated -C paths are relative to the preceding one.
            dash_c_dir = Some(match dash_c_dir {
                Some(existing) => format!("{existing} -C {dir}"),
                None => dir.to_string(),
            });
        } else if token.starts_with("--git-dir")
            || token.starts_with("--work-tree")
            || token.starts_with("--prefix")
        {
            return DiscardProbeResolution::Unresolvable;
        } else if *token == "-c" {
            let config = tokens.get(index + 1).copied();
            // core.worktree/core.bare relocate the repository.
            if let Some(config) = config {
                if config.starts_with("core.worktree") || config.starts_with("core.bare") {
                    let relocates = config
                        .strip_prefix("core.worktree")
                        .or_else(|| config.strip_prefix("core.bare"))
                        .map(|rest| rest.is_empty() || rest.starts_with('='))
                        .unwrap_or(false);
                    if relocates {
                        return DiscardProbeResolution::Unresolvable;
                    }
                }
            }
        }
        // Other flags do not relocate.
    }

    // git clean -x/-X also deletes ignored files, so its probe includes them.
    let mut clean_removes_ignored = false;
    if let Some(sub_index) = subcommand_index {
        if tokens[sub_index] == "clean" {
            for token in &tokens[sub_index + 1..] {
                if *token == "--" {
                    break; // everything after -- is a pathspec
                }
                if token.starts_with("--") {
                    continue;
                }
                if token.starts_with('-') && token[1..].contains(['x', 'X']) {
                    clean_removes_ignored = true;
                    break;
                }
            }
        }
    }

    // Inline env assignments directly before the git invocation relocate the
    // target repository; replay them in the probe, or refuse when they cannot.
    let last_segment = split_on_separators(prefix).pop().unwrap_or_default();
    let leading_tokens: Vec<&str> = split_ws(last_segment.trim());
    let assignment_re = fancy_regex::Regex::new(r#"^[A-Za-z_][A-Za-z0-9_]*=[^\s$`;&|()<>"!]+$"#)
        .expect("assignment regex");
    for token in &leading_tokens {
        if assignment_re.is_match(token).unwrap_or(false) {
            continue; // replayable assignment
        }
        // Wrappers that cannot change directory or select another repository.
        if *token == "sudo"
            || *token == "env"
            || *token == "command"
            || *token == "builtin"
            || token.ends_with('/')
        {
            continue;
        }
        return DiscardProbeResolution::Unresolvable;
    }
    let assignments: Vec<&str> = leading_tokens
        .iter()
        .copied()
        .filter(|token| token.contains('='))
        .collect();
    let env_prefix = if assignments.is_empty() {
        String::new()
    } else {
        format!("{} ", assignments.join(" "))
    };

    // cd relocations earlier in the command.
    let cd_re = fancy_regex::Regex::new(r"\b(cd|pushd)\b").expect("cd regex");
    let has_cd = cd_re.is_match(prefix).unwrap_or(false) || prefix.contains('(');
    let mut persistent_cd_args: Vec<String> = Vec::new();
    let mut grouped_cd_args: Vec<String> = Vec::new();
    let mut saw_cd = false;
    let mut paren_depth: i64 = 0;
    let mut cd_pending_separator = false;
    if has_cd {
        let mut offset = 0usize;
        for part in split_with_separators(prefix) {
            let start = offset;
            offset += part.len();
            if start < user_command_start {
                continue; // command-prefix region: replayed as-is
            }
            let is_separator = SEPARATORS.contains(&part.as_str());
            if is_separator {
                if cd_pending_separator && (part == ";" || part == "\n") {
                    // The discard's directory depends on the cd succeeding.
                    return DiscardProbeResolution::Unresolvable;
                }
                if part == "||" || part == "|" {
                    if saw_cd {
                        return DiscardProbeResolution::Unresolvable;
                    }
                    continue;
                }
                cd_pending_separator = false;
                continue;
            }
            let trimmed = part.trim();
            let opens = part.matches('(').count() as i64;
            let closes = part.matches(')').count() as i64;
            let inside_group = paren_depth > 0 || opens > 0;
            paren_depth = 0.max(paren_depth + opens - closes);
            if inside_group {
                let stripped: String = trimmed
                    .trim_start_matches(|c: char| c == '(' || c.is_whitespace())
                    .trim_end_matches(|c: char| c == ')' || c.is_whitespace())
                    .to_string();
                if let Some(arg) = stripped.strip_prefix("cd") {
                    let arg = arg.trim();
                    if !arg.is_empty()
                        && arg.contains(['$', '`', ';', '&', '|', '(', ')', '<', '>', '#', '"'])
                    {
                        return DiscardProbeResolution::Unresolvable;
                    }
                    saw_cd = true;
                    cd_pending_separator = true;
                    grouped_cd_args.push(arg.to_string());
                } else if cd_re.is_match(trimmed).unwrap_or(false) {
                    return DiscardProbeResolution::Unresolvable;
                }
                if paren_depth == 0 {
                    grouped_cd_args.clear();
                }
                continue;
            }
            if trimmed == "pushd" || trimmed.starts_with("pushd ") {
                return DiscardProbeResolution::Unresolvable;
            }
            let Some(arg) = trimmed.strip_prefix("cd") else {
                cd_pending_separator = false;
                continue; // not a cd: cannot change cwd
            };
            let arg = arg.trim();
            // An arg we cannot replay safely leaves the target repository
            // unknown; refuse rather than probe blindly.
            let balanced = arg.matches('"').count() % 2 == 0 && arg.matches('\'').count() % 2 == 0;
            if !balanced
                || (!arg.is_empty()
                    && arg.contains(['$', '`', ';', '&', '|', '(', ')', '<', '>', '#']))
            {
                return DiscardProbeResolution::Unresolvable;
            }
            saw_cd = true;
            cd_pending_separator = true;
            persistent_cd_args.push(arg.to_string());
        }
    }
    let cd_args = if paren_depth > 0 {
        let mut args = persistent_cd_args.clone();
        args.extend(grouped_cd_args);
        args
    } else {
        persistent_cd_args
    };

    if cd_args.is_empty() && dash_c_dir.is_none() && !clean_removes_ignored && env_prefix.is_empty()
    {
        return DiscardProbeResolution::NotRelocated;
    }
    let ignored = if clean_removes_ignored {
        " --ignored=matching"
    } else {
        ""
    };
    let cd_prefix = if cd_args.is_empty() {
        String::new()
    } else {
        let joins = cd_args
            .iter()
            .map(|arg| {
                if arg.is_empty() {
                    "cd".to_string()
                } else {
                    format!("cd {arg}")
                }
            })
            .collect::<Vec<_>>()
            .join(" && ");
        format!("{joins} && ")
    };
    let relocation = format!("{cd_prefix}{env_prefix}");
    DiscardProbeResolution::Target(DiscardProbeTarget {
        relocation_prefix: if relocation.is_empty() {
            None
        } else {
            Some(relocation)
        },
        git_status_command: format!(
            "{}status --porcelain --untracked-files=all{ignored}",
            match dash_c_dir.as_deref() {
                Some(dir) => format!("git -C {dir} "),
                None => "git ".to_string(),
            }
        ),
    })
}

fn is_truthy_env_value(value: Option<&String>) -> bool {
    match value {
        Some(v) => !v.is_empty() && v != "0",
        None => false,
    }
}

/// Probe for at-risk files via `git status --porcelain`. Returns `Ok(None)`
/// when dirtiness cannot be determined, so the guard fails open.
async fn probe_uncommitted_changes(
    ops: &dyn BashOperations,
    probe_command: &str,
    cwd: &str,
    env: &HashMap<String, String>,
    signal: Option<AbortSignal>,
    timeout: Option<f64>,
) -> anyhow::Result<Option<Vec<String>>> {
    let output = std::sync::Mutex::new(String::new());
    let result = ops
        .exec(
            probe_command,
            cwd,
            ExecOptions {
                on_data: &|data: &[u8]| {
                    output
                        .lock()
                        .unwrap()
                        .push_str(&String::from_utf8_lossy(data));
                },
                signal,
                timeout,
                env: Some(env.clone()),
            },
        )
        .await;
    match result {
        Ok(Some(0)) => {}
        Ok(_) => return Ok(None),
        Err(err) if err.to_string() == "aborted" => return Err(err),
        Err(_) => return Ok(None),
    }
    Ok(Some(
        output
            .into_inner()
            .expect("probe output unlocked")
            .split('\n')
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.trim_end_matches('\r').to_string())
            .collect(),
    ))
}

fn format_dirty_tree_refusal(dirty_paths: &[String], includes_ignored_files: bool) -> String {
    let listed_count = dirty_paths.len().min(MAX_DIRTY_PATHS_LISTED);
    let listed = &dirty_paths[..listed_count];
    let elided = dirty_paths.len() - listed_count;
    let noun = if includes_ignored_files {
        "uncommitted or ignored file(s)"
    } else {
        "uncommitted change(s)"
    };
    let mut lines = vec![format!(
        "Refusing to run this destructive git command: the working tree has {} {noun}.",
        dirty_paths.len()
    )];
    lines.extend(listed.iter().map(|line| format!("  {line}")));
    if elided > 0 {
        lines.push(format!("  ... and {elided} more"));
    }
    lines.push(String::new());
    lines.push("Commit, stash, or stage your work first.".to_string());
    lines.push(format!(
        "To discard these changes intentionally, retry with allowDestructiveGit: true, or set {BASH_DESTRUCTIVE_GIT_BYPASS_ENV}=1."
    ));
    lines.join("\n")
}

fn format_relocation_refusal() -> String {
    format!(
        "Refusing to run this destructive git command: it changes directory (or repository) first, and the uncommitted changes of the repository it targets cannot be checked safely.\n\nRun the discard as its own command from the target directory, or retry with allowDestructiveGit: true, or set {BASH_DESTRUCTIVE_GIT_BYPASS_ENV}=1."
    )
}

/// Serialize a truncation result with the TS wire shape.
pub fn truncation_to_json(truncation: &TruncationResult) -> serde_json::Value {
    json!({
        "content": truncation.content,
        "truncated": truncation.truncated,
        "truncatedBy": match truncation.truncated_by {
            Some(TruncatedBy::Lines) => json!("lines"),
            Some(TruncatedBy::Bytes) => json!("bytes"),
            None => serde_json::Value::Null,
        },
        "totalLines": truncation.total_lines,
        "totalBytes": truncation.total_bytes,
        "outputLines": truncation.output_lines,
        "outputBytes": truncation.output_bytes,
        "lastLinePartial": truncation.last_line_partial,
        "firstLineExceedsLimit": truncation.first_line_exceeds_limit,
        "maxLines": truncation.max_lines,
        "maxBytes": truncation.max_bytes,
    })
}

struct FormattedOutput {
    text: String,
    details: Option<serde_json::Value>,
}

fn format_output(
    snapshot: &crate::tools::output_accumulator::OutputSnapshot,
    last_line_bytes: usize,
    empty_text: &str,
) -> FormattedOutput {
    let truncation = &snapshot.truncation;
    let mut text = if snapshot.content.is_empty() {
        empty_text.to_string()
    } else {
        snapshot.content.clone()
    };
    let mut details = None;
    if truncation.truncated {
        let mut d = json!({ "truncation": truncation_to_json(truncation) });
        if let Some(path) = &snapshot.full_output_path {
            d["fullOutputPath"] = json!(path);
        }
        details = Some(d);
        let start_line = truncation.total_lines - truncation.output_lines + 1;
        let end_line = truncation.total_lines;
        // A degraded spill has no path; never advertise a missing file.
        let location = snapshot
            .full_output_path
            .as_deref()
            .map(|p| format!(". Full output: {p}"))
            .unwrap_or_default();
        if truncation.last_line_partial {
            let line_size = if last_line_bytes > 0 {
                format!(" (line is {})", format_size(last_line_bytes))
            } else {
                String::new()
            };
            text.push_str(&format!(
                "\n\n[Showing last {} of line {start_line}{line_size}{location}]",
                format_size(truncation.output_bytes)
            ));
        } else if truncation.truncated_by == Some(TruncatedBy::Lines) {
            text.push_str(&format!(
                "\n\n[Showing lines {start_line}-{end_line} of {}{location}]",
                truncation.total_lines
            ));
        } else {
            text.push_str(&format!(
                "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit){location}]",
                truncation.total_lines,
                format_size(DEFAULT_MAX_BYTES)
            ));
        }
    }
    FormattedOutput { text, details }
}

fn append_status(text: &str, status: &str) -> String {
    if text.is_empty() {
        status.to_string()
    } else {
        format!("{text}\n\n{status}")
    }
}

/// Execute one bash tool call. Errors carry the model-facing text.
pub async fn execute_bash(
    cwd: &str,
    options: &BashToolOptions,
    command: &str,
    timeout: Option<f64>,
    allow_destructive_git: Option<bool>,
    signal: Option<AbortSignal>,
    on_update: Option<OnUpdate>,
) -> anyhow::Result<ToolExecutionResult> {
    let default_ops;
    let ops: &dyn BashOperations = match &options.operations {
        Some(ops) => ops.as_ref(),
        None => {
            default_ops = LocalBashOperations {
                shell_path: options.shell_path.clone(),
            };
            &default_ops
        }
    };
    let command_prefix = options.command_prefix.as_deref();
    let spawn_hook = options.spawn_hook.as_ref();

    let resolved_command = match command_prefix {
        Some(prefix) => format!("{prefix}\n{command}"),
        None => command.to_string(),
    };
    let spawn_context = resolve_spawn_context(&resolved_command, cwd, spawn_hook);

    // Refuse destructive git discard commands while the tree is dirty.
    let discard_indices = find_destructive_git_discard_commands(&resolved_command);
    if !discard_indices.is_empty()
        && allow_destructive_git != Some(true)
        && !is_truthy_env_value(spawn_context.env.get(BASH_DESTRUCTIVE_GIT_BYPASS_ENV))
    {
        let mut probes: Vec<(BashSpawnContext, bool)> = Vec::new();
        let mut seen_probes: HashSet<String> = HashSet::new();
        let user_command_start = command_prefix.map(|p| p.len() + 1).unwrap_or(0);
        for index in discard_indices {
            let target = resolve_discard_probe_target(&resolved_command, index, user_command_start);
            match target {
                DiscardProbeResolution::Unresolvable => {
                    anyhow::bail!("{}", format_relocation_refusal());
                }
                DiscardProbeResolution::NotRelocated => {
                    let raw_probe = match command_prefix {
                        Some(prefix) => format!("{prefix}\n{GIT_STATUS_PORCELAIN_COMMAND}"),
                        None => GIT_STATUS_PORCELAIN_COMMAND.to_string(),
                    };
                    let context = resolve_spawn_context(&raw_probe, cwd, spawn_hook);
                    let key = format!("{}\u{0}{}", context.command, context.cwd);
                    if seen_probes.insert(key) {
                        probes.push((context, false));
                    }
                }
                DiscardProbeResolution::Target(target) => {
                    let relocation_prefix = target.relocation_prefix.as_deref().unwrap_or("");
                    let raw_probe = match command_prefix {
                        Some(prefix) => {
                            format!("{prefix}\n{relocation_prefix}{}", target.git_status_command)
                        }
                        None => format!("{relocation_prefix}{}", target.git_status_command),
                    };
                    let context = resolve_spawn_context(&raw_probe, cwd, spawn_hook);
                    let key = format!("{}\u{0}{}", context.command, context.cwd);
                    if seen_probes.insert(key) {
                        probes.push((
                            context,
                            target.git_status_command.contains("--ignored=matching"),
                        ));
                    }
                }
            }
        }
        for (context, includes_ignored) in probes {
            let dirty_paths = probe_uncommitted_changes(
                ops,
                &context.command,
                &context.cwd,
                &context.env,
                signal.clone(),
                timeout,
            )
            .await?;
            if let Some(paths) = dirty_paths.filter(|paths| !paths.is_empty()) {
                anyhow::bail!("{}", format_dirty_tree_refusal(&paths, includes_ignored));
            }
        }
    }

    // Stream output through the accumulator with throttled updates.
    let acc = Arc::new(Mutex::new(OutputAccumulator::new(
        OutputAccumulatorOptions {
            temp_file_prefix: "pi-bash".to_string(),
            ..OutputAccumulatorOptions::default()
        },
    )));
    let notify = Arc::new(tokio::sync::Notify::new());
    let dirty = Arc::new(AtomicBool::new(false));
    let mut last_update_at: Option<std::time::Instant> = None;

    if let Some(on_update) = &on_update {
        on_update(ToolUpdate {
            content: Vec::new(),
            details: None,
        });
    }

    let on_data = {
        let notify = notify.clone();
        let dirty = dirty.clone();
        let acc = acc.clone();
        move |data: &[u8]| {
            acc.lock().unwrap().append(data);
            dirty.store(true, Ordering::SeqCst);
            notify.notify_one();
        }
    };

    let exec_fut = ops.exec(
        &spawn_context.command,
        &spawn_context.cwd,
        ExecOptions {
            on_data: &on_data,
            signal: signal.clone(),
            timeout,
            env: Some(spawn_context.env.clone()),
        },
    );
    tokio::pin!(exec_fut);

    let exec_result: anyhow::Result<Option<i32>> = loop {
        let deadline = if dirty.load(Ordering::SeqCst) {
            last_update_at.map(|last| {
                let elapsed = last.elapsed();
                if elapsed >= BASH_UPDATE_THROTTLE_MS {
                    std::time::Instant::now()
                } else {
                    last + BASH_UPDATE_THROTTLE_MS
                }
            })
        } else {
            None
        };
        tokio::select! {
            _ = notify.notified() => {}
            _ = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
                    None => std::future::pending::<()>().await,
                }
            }, if deadline.is_some() => {}
            result = &mut exec_fut => {
                break result;
            }
        }
        if dirty.swap(false, Ordering::SeqCst) {
            last_update_at = Some(std::time::Instant::now());
            if let Some(on_update) = &on_update {
                let snapshot = acc.lock().unwrap().snapshot();
                on_update(ToolUpdate {
                    content: vec![ToolContentBlock::text(snapshot.content.clone())],
                    details: Some(json!({
                        "truncation": if snapshot.truncation.truncated {
                            truncation_to_json(&snapshot.truncation)
                        } else {
                            serde_json::Value::Null
                        },
                        "fullOutputPath": snapshot.full_output_path,
                    })),
                });
            }
        }
    };

    // Finish the accumulator, settle the spill, then snapshot.
    {
        let mut acc = acc.lock().unwrap();
        acc.finish();
    }
    let (snapshot, last_line_bytes) = {
        let mut acc = acc.lock().unwrap();
        acc.close_temp_file_sync();
        let snapshot = acc.snapshot();
        (snapshot, acc.get_last_line_bytes())
    };

    match exec_result {
        Err(err) => {
            // TS catch path: timeouts and aborts keep the partial output and
            // append a status line; other errors surface raw.
            let message = err.to_string();
            if message == "aborted" {
                let formatted = format_output(&snapshot, last_line_bytes, "");
                anyhow::bail!(append_status(&formatted.text, "Command aborted"));
            }
            if let Some(secs) = message.strip_prefix("timeout:") {
                let formatted = format_output(&snapshot, last_line_bytes, "");
                anyhow::bail!(append_status(
                    &formatted.text,
                    &format!("Command timed out after {secs} seconds")
                ));
            }
            Err(err)
        }
        Ok(exit_code) => {
            let formatted = format_output(&snapshot, last_line_bytes, "(no output)");
            let text = formatted.text;
            if exit_code.is_some_and(|code| code != 0) {
                anyhow::bail!(append_status(
                    &text,
                    &format!("Command exited with code {}", exit_code.unwrap())
                ));
            }
            Ok(ToolExecutionResult {
                content: vec![ToolContentBlock::text(text)],
                details: formatted.details,
                is_error: false,
            })
        }
    }
}

pub fn bash_tool_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "required": ["command"],
        "properties": {
            "command": {
                "type": "string",
                "description": "Bash command to execute"
            },
            "timeout": {
                "type": "number",
                "description": "Timeout in seconds (optional, no default timeout)"
            },
            "allowDestructiveGit": {
                "type": "boolean",
                "description": "Skip the dirty-tree guard for destructive git discard commands. Only set when discarding uncommitted work is intentional."
            }
        }
    })
}

pub fn bash_tool_description() -> String {
    format!(
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds. Destructive git discard commands (git checkout -- ., git checkout ., git clean -f..., git reset --hard, git restore .) are refused while uncommitted changes exist; retry with allowDestructiveGit: true only when the discard is intentional.",
        DEFAULT_MAX_BYTES / 1024
    )
}

/// The `bash` tool definition: exact name, schema, and description.
pub fn create_bash_tool_definition(cwd: &str) -> ToolDefinition {
    create_bash_tool_definition_with_options(cwd, BashToolOptions::default())
}

pub fn create_bash_tool_definition_with_options(
    cwd: &str,
    options: BashToolOptions,
) -> ToolDefinition {
    let cwd = cwd.to_string();
    let operations = options.operations.clone();
    let command_prefix = options.command_prefix.clone();
    let shell_path = options.shell_path.clone();
    let spawn_hook = options.spawn_hook.clone();
    let execute: crate::tools::tool_definition::ExecuteFn = {
        let cwd = cwd.clone();
        Arc::new(move |_tool_call_id, params, signal, on_update| {
            let cwd = cwd.clone();
            let options = BashToolOptions {
                operations: operations.clone(),
                command_prefix: command_prefix.clone(),
                shell_path: shell_path.clone(),
                spawn_hook: spawn_hook.clone(),
            };
            Box::pin(async move {
                let command = params
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("bash tool requires a command string"))?
                    .to_string();
                let timeout = params.get("timeout").and_then(serde_json::Value::as_f64);
                let allow_destructive_git = params
                    .get("allowDestructiveGit")
                    .and_then(serde_json::Value::as_bool);
                execute_bash(
                    &cwd,
                    &options,
                    &command,
                    timeout,
                    allow_destructive_git,
                    signal,
                    on_update,
                )
                .await
            })
        })
    };
    ToolDefinition {
        name: "bash".to_string(),
        label: "bash".to_string(),
        description: bash_tool_description(),
        prompt_snippet: "Execute bash commands (ls, grep, find, etc.)".to_string(),
        parameters: bash_tool_schema(),
        execution_mode: None,
        prepare_arguments: None,
        execute,
    }
}
