//! Daemon-side session slash-command execution: bridge the pa-core
//! executor to the worker engine contract. `run_prompt` parses the four
//! session commands before admission — they never reach the model loop —
//! executes them against the engine's session, and translates the durable
//! rows into engine events (the worker persists and broadcasts them).
//!
//! This is the spin fix for session commands: previously a session command
//! admitted through `run_turn` would wait for an `AgentStart` that never
//! arrives. Parsing before admission keeps the wait loop reachable only
//! for real turns.

use pa_core::session_engine::session_commands::{
    session_command_failure_row, SessionCommandExecution,
};
use pa_core::session_engine::slash_commands::{
    parse_session_command, SessionSlashCommand, SlashCommandRegistry,
};
use pa_types::session::AgentMessage;

use crate::agent_engine::AgentSessionEngine;
use crate::engine::EngineEvent;

/// Run one session command and emit its durable rows. `None` means the
/// emitter asked to stop (abort): the host must not emit a `Done`.
pub(crate) fn run_session_command(
    engine: &AgentSessionEngine,
    command: SessionSlashCommand,
    emit: &mut dyn FnMut(EngineEvent) -> bool,
) -> Option<SessionCommandExecution> {
    let execution = match engine.execute_session_command(&command) {
        Ok(execution) => execution,
        // Pre-execution failures (model resolution, session build) still
        // record the attempted command as a failure result row.
        Err(error) => {
            let error = format!("{error:#}");
            SessionCommandExecution {
                messages: vec![session_command_failure_row(&command, &error)],
                compaction: None,
                continuation_prompt: None,
                error: Some(error),
            }
        }
    };
    for message in &execution.messages {
        if !emit(EngineEvent::CustomMessage(custom_message_value(message))) {
            return None;
        }
    }
    if let Some(compaction) = &execution.compaction {
        let entry = serde_json::to_value(&compaction.entry).unwrap_or(serde_json::Value::Null);
        // The client-facing result is the TS `CompactionResult` wire shape.
        let result = serde_json::json!({
            "summary": compaction.result.summary,
            "firstKeptEntryId": compaction.result.first_kept_entry_id,
            "tokensBefore": compaction.result.tokens_before,
        });
        if !emit(EngineEvent::Compaction { entry, result }) {
            return None;
        }
    }
    Some(execution)
}

/// Parse a session command out of a prompt, if it is one.
pub(crate) fn parse_prompt_session_command(text: &str) -> Option<SessionSlashCommand> {
    let registry = SlashCommandRegistry::builtin();
    parse_session_command(&registry, text)
}

/// One durable row in its wire message form (`role: "custom"`): the shape
/// `message_start`/`message_end` pairs carry and TS sessions keep in
/// `agent.state.messages`.
pub(crate) fn custom_message_value(
    message: &pa_types::session::CustomMessage,
) -> serde_json::Value {
    serde_json::to_value(AgentMessage::Custom(message.clone())).unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_command_prompt_parsing() {
        let command = parse_prompt_session_command("/goal status").unwrap();
        assert_eq!(command.name, "goal");
        assert_eq!(command.args, "status");
        assert!(parse_prompt_session_command("plain prompt").is_none());
        // Client commands are not session commands.
        assert!(parse_prompt_session_command("/model").is_none());
    }

    #[test]
    fn custom_rows_serialize_with_custom_role() {
        let command = parse_prompt_session_command("/compact focus").unwrap();
        let row = session_command_failure_row(&command, "boom");
        let value = custom_message_value(&row);
        assert_eq!(value["role"], "custom");
        assert_eq!(value["customType"], "session_slash_command_result");
        assert_eq!(value["content"], "Command failed: boom");
        assert_eq!(value["details"]["success"], false);
    }
}
