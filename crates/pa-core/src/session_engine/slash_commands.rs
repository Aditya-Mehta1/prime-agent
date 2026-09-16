//! Built-in slash-command registry and parsers. Port of core/slash-commands.ts.
//! Descriptions and argument hints are user-facing: keep them byte-identical.

use std::collections::HashMap;

use crate::skills::parse_slash_command;

/// Session-executed commands (their behavior lives in the session engine).
pub const SESSION_SLASH_COMMAND_NAMES: [&str; 4] = ["compact", "refine", "goal", "autonomous"];

pub fn is_session_slash_command_name(value: &str) -> bool {
    SESSION_SLASH_COMMAND_NAMES.contains(&value)
}

/// Where a command executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommandExecution {
    /// Client-side UI action (default).
    Client,
    /// Session-engine behavior.
    Session,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BuiltinSlashCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub execution: SlashCommandExecution,
    pub argument_hint: Option<&'static str>,
    pub aliases: &'static [&'static str],
    pub takes_argument: bool,
}

const CANONICAL_BUILTIN_SLASH_COMMANDS: &[BuiltinSlashCommand] = &[
    BuiltinSlashCommand { name: "settings", description: "Open settings menu", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "model", description: "Select model (opens selector UI)", execution: SlashCommandExecution::Client, argument_hint: Some("[search]"), aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "effort", description: "Select reasoning/thinking level (opens selector UI)", execution: SlashCommandExecution::Client, argument_hint: Some("[level]"), aliases: &["thinking"], takes_argument: false },
    BuiltinSlashCommand { name: "fast", description: "Toggle OpenAI Fast mode", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "scoped-models", description: "Enable/disable models for Alt+M cycling", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "export", description: "Export session (HTML default, or specify path: .html/.jsonl)", execution: SlashCommandExecution::Client, argument_hint: Some("[path]"), aliases: &[], takes_argument: true },
    BuiltinSlashCommand { name: "import", description: "Import and resume a session from a JSONL file", execution: SlashCommandExecution::Client, argument_hint: Some("<path.jsonl>"), aliases: &[], takes_argument: true },
    BuiltinSlashCommand { name: "share", description: "Share session as a secret GitHub gist", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "copy", description: "Copy last agent message to clipboard", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "btw", description: "Ask a side question without adding it to the session; replies follow up, esc returns", execution: SlashCommandExecution::Client, argument_hint: Some("<question>"), aliases: &["side"], takes_argument: true },
    BuiltinSlashCommand { name: "name", description: "Set or show the session display name", execution: SlashCommandExecution::Client, argument_hint: Some("[name]"), aliases: &["rename"], takes_argument: true },
    BuiltinSlashCommand { name: "session", description: "Show session info", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "system-prompt", description: "Show the exact system prompt sent to the model", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "logs", description: "Show where daemon and client logs are saved", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "traces", description: "Preview, upload, or configure Prime Agent traces", execution: SlashCommandExecution::Client, argument_hint: Some("[status|on|off|preview|upload|upload-current|upload-all|login]"), aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "context", description: "Show token, cost, and context usage for agent and sub-agents", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &["usage"], takes_argument: false },
    BuiltinSlashCommand { name: "changelog", description: "Show changelog entries", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "update", description: "Update Prime Agent and installed packages", execution: SlashCommandExecution::Client, argument_hint: Some("[source|--self|--extensions|--nightly|--stable]"), aliases: &[], takes_argument: true },
    BuiltinSlashCommand { name: "nightly", description: "Switch Prime Agent updates to the nightly channel (unreleased builds, may be broken)", execution: SlashCommandExecution::Client, argument_hint: Some("[on|off|status]"), aliases: &[], takes_argument: true },
    BuiltinSlashCommand { name: "hotkeys", description: "Show all keyboard shortcuts", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "fork", description: "Create a new fork from a previous user message", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "clone", description: "Duplicate the current session at the current position", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "tree", description: "Navigate session tree (switch branches)", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "login", description: "Configure provider authentication", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "logout", description: "Remove provider authentication", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "mcp", description: "Open MCP Connections or manage MCP integrations", execution: SlashCommandExecution::Client, argument_hint: Some("[add|list|get|remove|login|logout]"), aliases: &[], takes_argument: true },
    BuiltinSlashCommand { name: "new", description: "Start a new session, optionally named and/or with an initial prompt", execution: SlashCommandExecution::Client, argument_hint: Some("[--name \"session name\" --] [prompt]"), aliases: &["clear"], takes_argument: true },
    BuiltinSlashCommand { name: "compact", description: "Compact the session context; optional instructions focus the summary", execution: SlashCommandExecution::Session, argument_hint: Some("[instructions]"), aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "refine", description: "Refine continual harness prompt notes, skills, subagents, and memory", execution: SlashCommandExecution::Session, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "goal", description: "Set or view a persistent goal; supports pause, resume, and clear", execution: SlashCommandExecution::Session, argument_hint: Some("[objective]"), aliases: &[], takes_argument: true },
    BuiltinSlashCommand { name: "autonomous", description: "Set or view autonomous mode with an optional budget", execution: SlashCommandExecution::Session, argument_hint: Some("[status|off|on [--max-continuations <n>] [--max-turns <n>] [--max-tokens <n>] [--timeout-ms <n>] [--gate <command>]]"), aliases: &[], takes_argument: true },
    BuiltinSlashCommand { name: "rlm-max-depth", description: "Set/view the per-chat persistent RLM max depth immediately; never interrupts or queues the running turn", execution: SlashCommandExecution::Client, argument_hint: Some("[<int> [--global]]"), aliases: &[], takes_argument: true },
    BuiltinSlashCommand { name: "heartbeat", description: "Set or view a persistent heartbeat; delivery defaults to steer, use --follow-up to queue; supports pause, resume, stop, and clear", execution: SlashCommandExecution::Client, argument_hint: Some("[status|pause|resume|stop|[every <duration>] [--steer|--follow-up] <instruction>]"), aliases: &[], takes_argument: true },
    BuiltinSlashCommand { name: "heartbeats", description: "View and manage all user and agent heartbeats", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "resume", description: "Open the agents view, or resume a session by id or path", execution: SlashCommandExecution::Client, argument_hint: Some("[id|path]"), aliases: &[], takes_argument: true },
    BuiltinSlashCommand { name: "reload", description: "Reload keybindings, extensions, skills, prompts, and themes", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
    BuiltinSlashCommand { name: "fullscreen", description: "Toggle fullscreen (alternate screen) rendering with scrollable transcript", execution: SlashCommandExecution::Client, argument_hint: Some("[on|off]"), aliases: &[], takes_argument: true },
    BuiltinSlashCommand { name: "quit", description: "Quit Prime Agent", execution: SlashCommandExecution::Client, argument_hint: None, aliases: &[], takes_argument: false },
];

/// The registry with alias resolution maps prebuilt.
pub struct SlashCommandRegistry {
    commands: &'static [BuiltinSlashCommand],
    by_name: HashMap<&'static str, &'static BuiltinSlashCommand>,
    alias_to_name: HashMap<&'static str, &'static str>,
}

impl SlashCommandRegistry {
    pub fn builtin() -> Self {
        let mut by_name = HashMap::new();
        let mut alias_to_name = HashMap::new();
        for command in CANONICAL_BUILTIN_SLASH_COMMANDS {
            by_name.insert(command.name, command);
            for alias in command.aliases {
                alias_to_name.insert(*alias, command.name);
            }
        }
        Self {
            commands: CANONICAL_BUILTIN_SLASH_COMMANDS,
            by_name,
            alias_to_name,
        }
    }

    pub fn all(&self) -> &'static [BuiltinSlashCommand] {
        self.commands
    }

    /// Resolve an alias to its canonical name.
    pub fn resolve_name(&self, name: &str) -> Option<&'static str> {
        self.alias_to_name
            .get(name)
            .copied()
            .or(match self.by_name.get(name) {
                Some(command) => Some(command.name),
                None => None,
            })
    }

    pub fn is_builtin(&self, name: &str) -> bool {
        self.by_name.contains_key(name) || self.alias_to_name.contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<&'static BuiltinSlashCommand> {
        self.resolve_name(name)
            .and_then(|name| self.by_name.get(name).copied())
    }

    /// Whether a builtin command consumes an argument (aliases included).
    /// `/clear` stays the no-argument compat alias even though `/new` takes one.
    pub fn takes_argument(&self, name: &str) -> bool {
        if name == "clear" {
            return false;
        }
        self.get(name).is_some_and(|command| command.takes_argument)
    }

    /// Parse and resolve a full input line.
    pub fn parse(&self, text: &str) -> Option<ResolvedSlashCommand<'static>> {
        let (name, args) = parse_slash_command(text)?;
        let resolved = self.resolve_name(&name)?;
        let is_alias = resolved != name;
        Some(ResolvedSlashCommand {
            name: resolved,
            original_name: leak_static(name),
            is_alias,
            args,
        })
    }
}

fn leak_static(value: String) -> &'static str {
    // Original names come from user input; keep the common canonical case
    // static and box the rest. Canonical names are covered by the table; for
    // alias reporting the input itself is echoed back to the caller via args.
    Box::leak(value.into_boxed_str())
}

/// A parsed and resolved slash command.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSlashCommand<'a> {
    pub name: &'static str,
    pub original_name: &'a str,
    pub is_alias: bool,
    pub args: String,
}

/// Parsed `/refine` options.
#[derive(Debug, Default, PartialEq)]
pub struct RefineCommandOptions {
    pub instructions: Option<String>,
    pub rollback_id: Option<String>,
    pub global: bool,
}

/// Parse `/refine [--global] [instructions]` and `/refine rollback <id>`.
pub fn parse_refine_command_options(args: &str) -> Result<RefineCommandOptions, String> {
    let mut rest = args.trim();
    let mut global = false;
    if rest.starts_with("--global")
        && matches!(rest.as_bytes().get(8), None | Some(b' ') | Some(b'\t'))
    {
        global = true;
        rest = rest["--global".len()..].trim();
    }
    if rest == "rollback" {
        return Err("Usage: /refine rollback <refinement-id>".to_string());
    }
    if let Some(tail) = rest.strip_prefix("rollback") {
        if tail.starts_with('\t') || tail.starts_with(' ') {
            let mut rollback_id = rest["rollback".len()..].trim().to_string();
            if rollback_id == "--global" {
                return Err("Usage: /refine rollback <refinement-id>".to_string());
            }
            if rollback_id.ends_with(" --global") {
                global = true;
                rollback_id = rollback_id.trim_end_matches(" --global").trim().to_string();
            }
            if rollback_id.is_empty() {
                return Err("Usage: /refine rollback <refinement-id>".to_string());
            }
            return Ok(RefineCommandOptions {
                instructions: None,
                rollback_id: Some(rollback_id),
                global,
            });
        }
    }
    Ok(RefineCommandOptions {
        instructions: (!rest.is_empty()).then(|| rest.to_string()),
        rollback_id: None,
        global,
    })
}

/// A parsed session slash command (compact/refine/goal/autonomous).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSlashCommand {
    pub name: &'static str,
    pub args: String,
    pub text: String,
}

/// Parse a session command from input; None for non-session commands.
pub fn parse_session_command(
    registry: &SlashCommandRegistry,
    text: &str,
) -> Option<SessionSlashCommand> {
    let resolved = registry.parse(text)?;
    if !is_session_slash_command_name(resolved.name) {
        return None;
    }
    Some(SessionSlashCommand {
        name: resolved.name,
        args: resolved.args.clone(),
        text: text.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_resolves_aliases() {
        let registry = SlashCommandRegistry::builtin();
        assert_eq!(registry.resolve_name("clear"), Some("new"));
        assert_eq!(registry.resolve_name("thinking"), Some("effort"));
        assert_eq!(registry.resolve_name("usage"), Some("context"));
        assert_eq!(registry.resolve_name("rename"), Some("name"));
        assert_eq!(registry.resolve_name("side"), Some("btw"));
        assert!(registry.is_builtin("model"));
        assert!(!registry.is_builtin("nope"));
        // /clear remains the no-argument alias.
        assert!(!registry.takes_argument("clear"));
        assert!(registry.takes_argument("new"));
    }

    #[test]
    fn parse_resolves_and_keeps_args() {
        let registry = SlashCommandRegistry::builtin();
        let resolved = registry.parse("/thinking high").unwrap();
        assert_eq!(resolved.name, "effort");
        assert!(resolved.is_alias);
        assert_eq!(resolved.args, "high");
        let plain = registry.parse("/model").unwrap();
        assert_eq!(plain.name, "model");
        assert_eq!(plain.args, "");
        assert!(registry.parse("not a command").is_none());
    }

    #[test]
    fn refine_options_parsing() {
        assert_eq!(
            parse_refine_command_options("do the thing").unwrap(),
            RefineCommandOptions {
                instructions: Some("do the thing".to_string()),
                rollback_id: None,
                global: false,
            }
        );
        assert_eq!(
            parse_refine_command_options("").unwrap(),
            RefineCommandOptions::default()
        );
        assert_eq!(
            parse_refine_command_options("--global tweak").unwrap(),
            RefineCommandOptions {
                instructions: Some("tweak".to_string()),
                rollback_id: None,
                global: true,
            }
        );
        assert_eq!(
            parse_refine_command_options("rollback ref_123").unwrap(),
            RefineCommandOptions {
                instructions: None,
                rollback_id: Some("ref_123".to_string()),
                global: false,
            }
        );
        assert!(
            parse_refine_command_options("--global rollback ref_123")
                .unwrap()
                .global
        );
        assert_eq!(
            parse_refine_command_options("rollback").unwrap_err(),
            "Usage: /refine rollback <refinement-id>"
        );
        assert_eq!(
            parse_refine_command_options("rollback ").unwrap_err(),
            "Usage: /refine rollback <refinement-id>"
        );
    }

    #[test]
    fn session_command_extraction() {
        let registry = SlashCommandRegistry::builtin();
        let command = parse_session_command(&registry, "/compact focus on tests").unwrap();
        assert_eq!(command.name, "compact");
        assert_eq!(command.args, "focus on tests");
        // Non-session commands are not session commands.
        assert!(parse_session_command(&registry, "/model").is_none());
        assert!(parse_session_command(&registry, "/unknown x").is_none());
    }

    #[test]
    fn session_commands_execute_in_session() {
        let registry = SlashCommandRegistry::builtin();
        for name in SESSION_SLASH_COMMAND_NAMES {
            assert_eq!(
                registry.get(name).unwrap().execution,
                SlashCommandExecution::Session
            );
        }
    }
}
