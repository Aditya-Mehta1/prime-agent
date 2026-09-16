//! Daemon wire protocol: thin adapter over `pa_types::daemon`.
//!
//! The shared wire contract lives in `pa-types` (ported from the TS daemon
//! protocol). This module re-exports it and adds the daemon-side mechanics:
//! command-envelope parsing with the TS error strings, capability sets, event
//! meta / replay helpers, and response constructors.

pub use pa_types::daemon::{
    DaemonClosingReason, DaemonCommand, DaemonCommandEnvelope as WireCommandEnvelope,
    DaemonCommandFrameType, DaemonErrorInfo, DaemonEventCursor, DaemonEventId, DaemonEventMeta,
    DaemonEventSequence, DaemonOutbound, DaemonProtocolInfo, DaemonReplayInfo, DaemonReplayStatus,
    DaemonResponse, DaemonResumeCursor, DaemonRuntimeIdentity, DaemonSavedSessionInfo,
    DaemonServerCapability, DaemonSessionClosedReason, DaemonSessionSnapshot,
    DaemonWorkerDescriptor, DaemonWorkerLifecycle, DurableDaemonCreateCommand,
    DAEMON_PROTOCOL_NAME, DAEMON_PROTOCOL_VERSION, DAEMON_SCHEMA_ID, DAEMON_SCHEMA_REVISION,
};
use serde_json::Value;

/// Minimum protocol version accepted in command envelopes (TS parity).
pub const DAEMON_COMMAND_ENVELOPE_MIN_PROTOCOL_VERSION: u64 = DAEMON_PROTOCOL_VERSION;
/// App version reported in `daemon_hello` for stale-daemon detection.
pub const DAEMON_APP_VERSION: &str = concat!("pa-daemon-rs-", env!("CARGO_PKG_VERSION"));

/// Command types the daemon recognizes (TS `DAEMON_COMMAND_TYPES`).
pub const KNOWN_COMMAND_TYPES: &[&str] = &[
    "list",
    "list_saved_sessions",
    "create",
    "attach",
    "reattach",
    "detach",
    "kill",
    "rename",
    "set_session_name",
    "prompt",
    "prompt_and_wait",
    "steer",
    "follow_up",
    "abort",
    "wait_for_idle",
    "get_state",
    "get_session_header",
    "get_session_stats",
    "get_messages",
    "get_queue",
    "clear_queue",
    "abort_and_clear_queue",
    "get_last_assistant_text",
    "retry_worker",
    "restart",
    "shutdown",
    "ack_result",
];

/// Parsed client command envelope.
#[derive(Debug, Clone)]
pub struct DaemonCommandEnvelope {
    pub id: String,
    pub protocol: DaemonProtocolInfo,
    pub client_id: Option<String>,
    pub command: DaemonCommand,
}

/// Envelope parse failure with TS-parity error strings.
#[derive(Debug, Clone, thiserror::Error)]
pub enum EnvelopeParseError {
    #[error("Daemon commands require protocol {0} or newer")]
    ProtocolTooOld(u64),
    #[error("Unknown daemon command: {0}")]
    UnknownCommand(String),
    #[error("Invalid daemon command: {0}")]
    Invalid(String),
}

impl EnvelopeParseError {
    pub fn is_unknown_command(&self) -> bool {
        matches!(self, EnvelopeParseError::UnknownCommand(_))
    }
}

/// Parse one JSONL command line into an envelope. Non-envelope lines are
/// treated as bare commands (TS backward compat). Unknown command types are
/// preserved as an error so callers can reply with the exact TS wire error.
pub fn parse_daemon_command_line(line: &str) -> Result<DaemonCommandEnvelope, EnvelopeParseError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|e| EnvelopeParseError::Invalid(format!("invalid JSON: {e}")))?;
    // Bare commands (no `type: "command"` envelope) are accepted directly.
    let (envelope_id, protocol, client_id, command_value) =
        if value.get("type").and_then(Value::as_str) == Some("command") {
            let obj = value.as_object().ok_or_else(|| {
                EnvelopeParseError::Invalid("command line is not an object".into())
            })?;
            let id = obj
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    EnvelopeParseError::Invalid("command envelope is missing id".into())
                })?
                .to_string();
            let protocol = obj
                .get("protocol")
                .ok_or(EnvelopeParseError::ProtocolTooOld(
                    DAEMON_COMMAND_ENVELOPE_MIN_PROTOCOL_VERSION,
                ))?;
            let name = protocol.get("name").and_then(Value::as_str).unwrap_or("");
            let version = protocol.get("version").and_then(Value::as_u64).unwrap_or(0);
            if name != DAEMON_PROTOCOL_NAME
                || version < DAEMON_COMMAND_ENVELOPE_MIN_PROTOCOL_VERSION
                || version > DAEMON_PROTOCOL_VERSION
            {
                return Err(EnvelopeParseError::ProtocolTooOld(
                    DAEMON_COMMAND_ENVELOPE_MIN_PROTOCOL_VERSION,
                ));
            }
            let client_id = match obj.get("clientId") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(_) => {
                    return Err(EnvelopeParseError::Invalid(
                        "clientId must be a string".into(),
                    ))
                }
            };
            let command_value = obj.get("command").cloned().ok_or_else(|| {
                EnvelopeParseError::Invalid("command envelope is missing command".into())
            })?;
            (
                id,
                DaemonProtocolInfo {
                    name: name.to_string(),
                    version,
                },
                client_id,
                command_value,
            )
        } else {
            (
                value
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                current_protocol_info(),
                None,
                value.clone(),
            )
        };
    let command = match serde_json::from_value::<DaemonCommand>(command_value.clone()) {
        Ok(command) => command,
        Err(_) => {
            let type_name = command_value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            if !KNOWN_COMMAND_TYPES.contains(&type_name.as_str()) {
                return Err(EnvelopeParseError::UnknownCommand(type_name));
            }
            return Err(EnvelopeParseError::Invalid(format!(
                "malformed {type_name} command"
            )));
        }
    };
    Ok(DaemonCommandEnvelope {
        id: envelope_id,
        protocol,
        client_id,
        command,
    })
}

/// Current protocol identity for this build.
pub fn current_protocol_info() -> DaemonProtocolInfo {
    DaemonProtocolInfo {
        name: DAEMON_PROTOCOL_NAME.to_string(),
        version: DAEMON_PROTOCOL_VERSION,
    }
}

/// TS `normalizeClientCapabilities`: filter against the supported set.
pub fn normalize_client_capabilities(capabilities: &[String]) -> Vec<String> {
    capabilities
        .iter()
        .filter(|cap| supported_client_capabilities().contains(&cap.as_str()))
        .cloned()
        .collect()
}

pub fn default_client_capabilities() -> Vec<String> {
    vec!["attach_snapshot".to_string(), "event_sequence".to_string()]
}

pub fn supported_client_capabilities() -> &'static [&'static str] {
    &[
        "attach_snapshot",
        "event_sequence",
        "extension_ui",
        "slim_attach",
        "chunked_snapshot",
        "client_owned_sessions",
    ]
}

pub fn default_server_capabilities() -> Vec<DaemonServerCapability> {
    supported_client_capabilities()
        .iter()
        .map(|cap| cap.to_string())
        .chain(
            [
                "delete_rlm_subagent",
                "heartbeat_catalog",
                "heartbeat_management",
                "model_catalog",
                "side_question_transcript",
                "transient_bash",
                "session_input_admission",
                "prompt_admission_cancellation",
                "owned_prompt_cancellation",
                "queue_message_mutation",
                "authoritative_child_roster",
                "owned_session_recovery_context",
                "rlm_quiescence_barrier",
                "session_input_pause",
                "acp_mcp_servers",
                "agent_roster",
                "direct_peer_transport",
            ]
            .iter()
            .map(|cap| cap.to_string()),
        )
        .map(|cap| cap.to_string())
        .collect()
}

/// Parse a client command line the way the TS supervisor does: only
/// `type: "command"` envelopes are accepted; bare commands fail with the
/// protocol error, because the supervisor has no pre-envelope clients.
pub fn parse_supervisor_command_line(
    line: &str,
) -> Result<DaemonCommandEnvelope, EnvelopeParseError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|e| EnvelopeParseError::Invalid(format!("invalid JSON: {e}")))?;
    if value.get("type").and_then(Value::as_str) != Some("command") {
        return Err(EnvelopeParseError::ProtocolTooOld(
            DAEMON_COMMAND_ENVELOPE_MIN_PROTOCOL_VERSION,
        ));
    }
    parse_daemon_command_line(line)
}

/// `proc:<start_time>` identity of a process, read from `/proc/<pid>/stat`
/// (field 22). `None` when the platform has no procfs.
pub fn process_start_id(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let command_end = stat.rfind(')')?;
    let start_time = stat[command_end + 2..].split(' ').nth(19)?;
    (!start_time.is_empty()).then(|| format!("proc:{start_time}"))
}

/// Port of `createDaemonEventMeta`.
pub fn create_daemon_event_meta(
    active_session_id: &str,
    sequence: DaemonEventSequence,
    emitted_at: Option<String>,
    generation: Option<&str>,
) -> DaemonEventMeta {
    DaemonEventMeta {
        id: format!("{active_session_id}:{sequence}"),
        protocol: current_protocol_info(),
        active_session_id: Some(active_session_id.to_string()),
        sequence: Some(sequence),
        cursor: Some(DaemonEventCursor {
            generation: generation.unwrap_or(active_session_id).to_string(),
            sequence,
        }),
        emitted_at: emitted_at.unwrap_or_else(crate::util::now_iso),
        replayed: None,
    }
}

/// Port of `createDaemonReplayInfo`.
pub fn create_daemon_replay_info(
    resume_cursor: Option<&DaemonResumeCursor>,
    last_event_sequence: DaemonEventSequence,
    generation: &str,
) -> DaemonReplayInfo {
    let to_cursor = DaemonEventCursor {
        generation: generation.to_string(),
        sequence: last_event_sequence,
    };
    let Some(resume) = resume_cursor else {
        return DaemonReplayInfo {
            status: DaemonReplayStatus::Complete,
            from_sequence: None,
            to_sequence: last_event_sequence,
            from_cursor: None,
            to_cursor: Some(to_cursor),
            reason: None,
        };
    };
    let resume_sequence = resume
        .sequence
        .or(resume.event_sequence)
        .unwrap_or_default();
    let from_cursor = resume
        .generation
        .as_deref()
        .map(|generation| DaemonEventCursor {
            generation: generation.to_string(),
            sequence: resume_sequence,
        });
    let unavailable = |reason: &str| DaemonReplayInfo {
        status: DaemonReplayStatus::Unavailable,
        from_sequence: Some(resume_sequence),
        to_sequence: last_event_sequence,
        from_cursor: from_cursor.clone(),
        to_cursor: Some(to_cursor.clone()),
        reason: Some(reason.to_string()),
    };
    if let Some(from) = &from_cursor {
        if from.generation != generation {
            return unavailable("event_generation_changed");
        }
    }
    if resume_sequence > last_event_sequence {
        return unavailable("resume_cursor_ahead_of_session");
    }
    if resume_sequence == last_event_sequence {
        return DaemonReplayInfo {
            status: DaemonReplayStatus::Complete,
            from_sequence: Some(resume_sequence),
            to_sequence: last_event_sequence,
            from_cursor,
            to_cursor: Some(to_cursor),
            reason: None,
        };
    }
    unavailable("event_replay_not_available")
}

/// Response constructors with the TS shape (`type: "response"` included on
/// serialize by the `DaemonOutbound::Response` variant; standalone responses
/// add the tag here).
pub fn response_success(id: Option<&str>, command: &str, data: Option<Value>) -> DaemonResponse {
    DaemonResponse {
        id: id.map(str::to_string),
        command: command.to_string(),
        success: true,
        data,
        error: None,
        error_info: None,
    }
}

pub fn response_failure(
    id: Option<&str>,
    command: &str,
    error: &str,
    error_info: Option<DaemonErrorInfo>,
) -> DaemonResponse {
    DaemonResponse {
        id: id.map(str::to_string),
        command: command.to_string(),
        success: false,
        data: None,
        error: Some(error.to_string()),
        error_info,
    }
}

/// Serialize a standalone response line (`type: "response"`).
pub fn response_line(response: &DaemonResponse) -> Value {
    let mut value = serde_json::to_value(response).unwrap_or(Value::Null);
    if let Some(obj) = value.as_object_mut() {
        obj.insert("type".to_string(), Value::String("response".to_string()));
    }
    value
}

/// Session selector carried by a command, when it has one.
pub fn command_active_session_id(command: &DaemonCommand) -> Option<&str> {
    match command {
        DaemonCommand::Attach {
            active_session_id, ..
        }
        | DaemonCommand::Reattach {
            active_session_id, ..
        }
        | DaemonCommand::Kill {
            active_session_id, ..
        }
        | DaemonCommand::Rename {
            active_session_id, ..
        }
        | DaemonCommand::SetSessionName {
            active_session_id, ..
        }
        | DaemonCommand::Prompt {
            active_session_id, ..
        }
        | DaemonCommand::PromptAndWait {
            active_session_id, ..
        }
        | DaemonCommand::Steer {
            active_session_id, ..
        }
        | DaemonCommand::FollowUp {
            active_session_id, ..
        }
        | DaemonCommand::Abort {
            active_session_id, ..
        }
        | DaemonCommand::StartSideQuestion {
            active_session_id, ..
        }
        | DaemonCommand::AbortSideQuestion {
            active_session_id, ..
        }
        | DaemonCommand::WaitForIdle {
            active_session_id, ..
        }
        | DaemonCommand::GetState {
            active_session_id, ..
        }
        | DaemonCommand::GetSessionHeader {
            active_session_id, ..
        }
        | DaemonCommand::GetSessionStats {
            active_session_id, ..
        }
        | DaemonCommand::GetMessages {
            active_session_id, ..
        }
        | DaemonCommand::GetQueue {
            active_session_id, ..
        }
        | DaemonCommand::ClearQueue {
            active_session_id, ..
        }
        | DaemonCommand::AbortAndClearQueue {
            active_session_id, ..
        }
        | DaemonCommand::GetLastAssistantText {
            active_session_id, ..
        }
        | DaemonCommand::RetryWorker {
            active_session_id, ..
        } => Some(active_session_id),
        DaemonCommand::Detach {
            active_session_id, ..
        } => active_session_id.as_deref(),
        _ => None,
    }
}

pub fn command_type_name(command: &DaemonCommand) -> &'static str {
    match command {
        DaemonCommand::List { .. } => "list",
        DaemonCommand::ListSavedSessions { .. } => "list_saved_sessions",
        DaemonCommand::Create { .. } => "create",
        DaemonCommand::Attach { .. } => "attach",
        DaemonCommand::Reattach { .. } => "reattach",
        DaemonCommand::Detach { .. } => "detach",
        DaemonCommand::Kill { .. } => "kill",
        DaemonCommand::Rename { .. } => "rename",
        DaemonCommand::SetSessionName { .. } => "set_session_name",
        DaemonCommand::Prompt { .. } => "prompt",
        DaemonCommand::PromptAndWait { .. } => "prompt_and_wait",
        DaemonCommand::Steer { .. } => "steer",
        DaemonCommand::FollowUp { .. } => "follow_up",
        DaemonCommand::Abort { .. } => "abort",
        DaemonCommand::StartSideQuestion { .. } => "start_side_question",
        DaemonCommand::AbortSideQuestion { .. } => "abort_side_question",
        DaemonCommand::WaitForIdle { .. } => "wait_for_idle",
        DaemonCommand::GetState { .. } => "get_state",
        DaemonCommand::GetSessionHeader { .. } => "get_session_header",
        DaemonCommand::GetSessionStats { .. } => "get_session_stats",
        DaemonCommand::GetMessages { .. } => "get_messages",
        DaemonCommand::GetQueue { .. } => "get_queue",
        DaemonCommand::ClearQueue { .. } => "clear_queue",
        DaemonCommand::AbortAndClearQueue { .. } => "abort_and_clear_queue",
        DaemonCommand::GetLastAssistantText { .. } => "get_last_assistant_text",
        DaemonCommand::RetryWorker { .. } => "retry_worker",
        DaemonCommand::Restart { .. } => "restart",
        DaemonCommand::Shutdown { .. } => "shutdown",
        DaemonCommand::AckResult { .. } => "ack_result",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trips() {
        let line = r#"{"type":"command","id":"c1","protocol":{"name":"prime-agent.daemon","version":7},"command":{"type":"list","all":true}}"#;
        let envelope = parse_daemon_command_line(line).expect("envelope parses");
        assert_eq!(envelope.id, "c1");
        assert!(matches!(envelope.command, DaemonCommand::List { .. }));
    }

    #[test]
    fn unknown_command_is_preserved() {
        let line = r#"{"type":"command","id":"c2","protocol":{"name":"prime-agent.daemon","version":7},"command":{"type":"cron_add","activeSessionId":"x"}}"#;
        let err = parse_daemon_command_line(line).unwrap_err();
        assert_eq!(err.to_string(), "Unknown daemon command: cron_add");
    }

    #[test]
    fn old_protocol_is_rejected() {
        let line = r#"{"type":"command","id":"c3","protocol":{"name":"prime-agent.daemon","version":6},"command":{"type":"list"}}"#;
        let err = parse_daemon_command_line(line).unwrap_err();
        assert!(err.to_string().contains("protocol 7 or newer"));
    }

    #[test]
    fn replay_info_matches_ts() {
        let info = create_daemon_replay_info(None, 5, "legacy");
        assert_eq!(info.status, DaemonReplayStatus::Complete);
        let info = create_daemon_replay_info(
            Some(&DaemonResumeCursor {
                active_session_id: None,
                generation: Some("other".to_string()),
                sequence: Some(2),
                event_sequence: None,
            }),
            5,
            "legacy",
        );
        assert_eq!(info.reason.as_deref(), Some("event_generation_changed"));
        let info = create_daemon_replay_info(
            Some(&DaemonResumeCursor {
                active_session_id: None,
                generation: None,
                sequence: None,
                event_sequence: Some(9),
            }),
            5,
            "legacy",
        );
        assert_eq!(
            info.reason.as_deref(),
            Some("resume_cursor_ahead_of_session")
        );
    }
}
