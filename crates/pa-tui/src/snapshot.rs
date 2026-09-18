//! Attach/snapshot reconstruction: wire data from the daemon (slim attach
//! results, streamed session events) folded into UI transcript items.
//!
//! Daemon message payloads are raw JSON (`Value`): the session engine owns
//! their evolution, and the TUI renders what arrives. Message decoding is
//! therefore lenient — it accepts plain-string content and content-block
//! arrays, with or without explicit block `type` tags, covering the shapes
//! the scripted harness and the real engine both emit.

use crate::chat::{AssistantMessage, ChatEntry, MessageBlock, ToolCallCard};
use pa_types::daemon::{DaemonEventCursor, DaemonReplayInfo};
use serde::Deserialize;
use serde_json::Value;

/// The slim attach result: the `data` object of a successful `attach`
/// response (`createAttachResult` wire shape).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachData {
    pub active_session_id: String,
    /// Slim attach carries summary/state/messages inside the snapshot.
    pub snapshot: Value,
    #[serde(default)]
    pub replay: Option<DaemonReplayInfo>,
    #[serde(default)]
    pub last_event_sequence: Option<u64>,
    #[serde(default)]
    pub last_event_cursor: Option<DaemonEventCursor>,
    #[serde(default)]
    pub client: Option<AttachClient>,
}

/// Client block echoed back by attach.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachClient {
    pub id: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// A reconstructed attach: view-ready chat entries plus identity labels.
#[derive(Debug, Clone, Default)]
pub struct Reconstructed {
    pub chat: Vec<ChatEntry>,
    /// Current model id (`state.model.id`), when the session reports one.
    pub model_id: Option<String>,
    /// Session display name.
    pub session_name: Option<String>,
    /// Session id of the persisted session file.
    pub session_id: String,
    pub last_event_sequence: u64,
}

impl Reconstructed {
    /// Fold one raw message into the chat entries. A `toolResult` message
    /// does not add a row: it completes the pending tool card its
    /// `toolCallId` refers to (the TS transcript replay updates the pending
    /// tool component instead of rendering a new row).
    pub fn push_message(&mut self, message: &Value) {
        if let Some(result) = tool_result_message_view(message) {
            apply_tool_result(&mut self.chat, result);
            return;
        }
        self.chat.extend(message_value_to_entries(message));
    }
}

/// A decoded `toolResult` transcript message: the id of the tool call it
/// completes plus the result view rendered on the matching card.
struct ToolResultReplay {
    tool_call_id: String,
    view: crate::chat::ToolResultView,
}

/// Decode a `role: "toolResult"` message into its replay view; `None` for
/// any other message.
fn tool_result_message_view(message: &Value) -> Option<ToolResultReplay> {
    if message.get("role").and_then(Value::as_str) != Some("toolResult") {
        return None;
    }
    Some(ToolResultReplay {
        tool_call_id: message
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        view: crate::chat::ToolResultView {
            content: message
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            details: message.get("details").cloned().unwrap_or(Value::Null),
            is_error: message
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
    })
}

/// Complete the first pending tool card matching `result`'s tool call id
/// (the TS `renderedPendingTools` replay: results land on the card, never
/// as a new transcript row).
fn apply_tool_result(chat: &mut [ChatEntry], result: ToolResultReplay) {
    let ToolResultReplay { tool_call_id, view } = result;
    for entry in chat.iter_mut() {
        if let ChatEntry::Tool(card) = entry {
            if card.id == tool_call_id && card.result.is_none() {
                card.started = true;
                // Replayed cards never saw the live execution: the timing
                // collapses to the rebuild instant, so the bash `Took` row
                // renders the same `0.0s` the TS component does on replay.
                let now = std::time::Instant::now();
                card.started_at = Some(now);
                card.ended_at = Some(now);
                card.result = Some(view);
                card.result_partial = false;
                return;
            }
        }
    }
}

/// Replay a whole transcript: map every message to its rows, then fold
/// `toolResult` messages onto the pending tool cards their ids refer to.
pub fn transcript_to_entries(messages: &[Value]) -> Vec<ChatEntry> {
    let mut chat: Vec<ChatEntry> = Vec::new();
    let mut tool_results: Vec<ToolResultReplay> = Vec::new();
    for message in messages {
        if let Some(result) = tool_result_message_view(message) {
            tool_results.push(result);
            continue;
        }
        chat.extend(message_value_to_entries(message));
    }
    for result in tool_results {
        apply_tool_result(&mut chat, result);
    }
    chat
}

/// Reconstruct the view state from slim attach data.
pub fn reconstruct(attach: &AttachData) -> Reconstructed {
    let snapshot = &attach.snapshot;
    let messages = snapshot
        .get("messages")
        .and_then(Value::as_array)
        .map(|messages| transcript_to_entries(messages))
        .unwrap_or_default();
    let state = snapshot.get("state");
    let model_id = state
        .and_then(|state| state.get("model"))
        .and_then(model_id_value);
    let session_name = state
        .and_then(|state| state.get("sessionName"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let session_id = state
        .and_then(|state| state.get("sessionId"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let last_event_sequence = snapshot
        .get("lastEventSequence")
        .and_then(Value::as_u64)
        .or(attach.last_event_sequence)
        .unwrap_or_default();
    Reconstructed {
        chat: messages,
        model_id,
        session_name,
        session_id,
        last_event_sequence,
    }
}

/// The model id from a `state.model` wire value (`{id, provider}` or a
/// display string).
fn model_id_value(model: &Value) -> Option<String> {
    match model {
        Value::String(label) => Some(label.clone()),
        Value::Object(map) => map.get("id").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// Parse attach data out of a successful attach/create response payload.
pub fn attach_data_from_response(data: &Value) -> anyhow::Result<AttachData> {
    serde_json::from_value(data.clone()).map_err(|error| {
        anyhow::anyhow!("the daemon returned an unrecognizable attach result: {error}")
    })
}

/// One live session event decoded for the transcript (the `event` field of
/// `session_event` frames, matching the worker's event vocabulary).
#[derive(Debug, Clone, PartialEq)]
pub enum TurnUpdate {
    /// `agent_start` / `turn_start`.
    TurnStarted,
    /// `message_start` with a user message.
    UserMessage(String),
    /// `message_start`/`message_update`/`message_end` with an assistant
    /// message (raw wire value); `streaming` distinguishes in-flight from
    /// final.
    AssistantMessage {
        message: Value,
        streaming: bool,
        stream_event: Option<Value>,
    },
    /// `tool_execution_start`: a tool call began executing.
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    /// `tool_execution_update`: a partial tool result.
    ToolExecutionUpdate {
        tool_call_id: String,
        partial: Value,
    },
    /// `tool_execution_end`: the final tool result.
    ToolExecutionEnd {
        tool_call_id: String,
        result: Value,
        is_error: bool,
    },
    /// `turn_end`, with the turn error string when the turn failed.
    TurnEnded { error: Option<String> },
    /// A `custom`-role message the transcript renders (session-command
    /// echo/result rows, or the malformed-notice fallback).
    CustomRow(ChatEntry),
    /// `auto_retry_start`: a provider failure is being retried after
    /// `delay_ms` (TS retry loader countdown).
    AutoRetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
    },
    /// `auto_retry_end`: the retry loop settled; `final_error` is set when
    /// the retries were exhausted.
    AutoRetryEnd {
        success: bool,
        attempt: u32,
        final_error: Option<String>,
    },
    /// `agent_end`: the prompt queue drained.
    Idle,
    /// `session_action_update` and other state churn: the footer status only.
    StatusUpdate,
}

/// Decode the `event` payload of a `session_event` frame.
pub fn event_to_update(event: &Value) -> Option<TurnUpdate> {
    match event.get("type").and_then(Value::as_str)? {
        "agent_start" | "turn_start" => Some(TurnUpdate::TurnStarted),
        "turn_end" => Some(TurnUpdate::TurnEnded {
            error: event
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        "agent_end" => Some(TurnUpdate::Idle),
        "message_start" | "message_update" | "message_end" => {
            let message = event.get("message")?.clone();
            let event_type = event.get("type").and_then(Value::as_str);
            let streaming = event_type != Some("message_end");
            match message.get("role").and_then(Value::as_str) {
                // User messages carry the full payload on start; the
                // message_end twin of the same row must not re-render it
                // (TS interactive ignores user message_end frames), and
                // only a partial user frame would be a protocol anomaly.
                Some("user")
                    if event_type == Some("message_update")
                        || event_type == Some("message_end") =>
                {
                    Some(TurnUpdate::StatusUpdate)
                }
                Some("user") => Some(TurnUpdate::UserMessage(message_text(&message))),
                Some("assistant") => Some(TurnUpdate::AssistantMessage {
                    message,
                    streaming,
                    stream_event: event.get("assistantMessageEvent").cloned(),
                }),
                // Custom rows arrive as a message_start + message_end pair
                // carrying the same payload; only the start adds the row.
                Some("custom") if event_type == Some("message_start") => {
                    custom_row_update(&message)
                }
                Some("custom") => Some(TurnUpdate::StatusUpdate),
                _ => Some(TurnUpdate::StatusUpdate),
            }
        }
        "tool_execution_start" => Some(TurnUpdate::ToolExecutionStart {
            tool_call_id: event
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            tool_name: event
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            args: event.get("args").cloned().unwrap_or(Value::Null),
        }),
        "tool_execution_update" => Some(TurnUpdate::ToolExecutionUpdate {
            tool_call_id: event
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            partial: event.get("partialResult").cloned().unwrap_or(Value::Null),
        }),
        "tool_execution_end" => Some(TurnUpdate::ToolExecutionEnd {
            tool_call_id: event
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            result: event.get("result").cloned().unwrap_or(Value::Null),
            is_error: event
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        "auto_retry_start" => Some(TurnUpdate::AutoRetryStart {
            attempt: event
                .get("attempt")
                .and_then(Value::as_u64)
                .unwrap_or_default() as u32,
            max_attempts: event
                .get("maxAttempts")
                .and_then(Value::as_u64)
                .unwrap_or_default() as u32,
            delay_ms: event
                .get("delayMs")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
        }),
        "auto_retry_end" => Some(TurnUpdate::AutoRetryEnd {
            success: event
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            attempt: event
                .get("attempt")
                .and_then(Value::as_u64)
                .unwrap_or_default() as u32,
            final_error: event
                .get("finalError")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        // Queue churn and unknown events only affect the status line.
        _ => Some(TurnUpdate::StatusUpdate),
    }
}

/// Decode one `custom`-role wire message into its transcript update: the
/// session-command echo and result rows render as slash rows; a custom type
/// matching either shape with an invalid payload renders the malformed
/// notice; everything else (and non-display rows) renders nothing.
fn custom_row_update(message: &Value) -> Option<TurnUpdate> {
    let entries = custom_message_entries(message);
    match entries.first() {
        Some(entry) => Some(TurnUpdate::CustomRow(entry.clone())),
        None => Some(TurnUpdate::StatusUpdate),
    }
}

/// The transcript entries for one `custom`-role message (`display` rows
/// only; non-display and unrelated custom types render nothing).
pub fn custom_message_entries(message: &Value) -> Vec<ChatEntry> {
    use pa_types::slash_commands::{
        SESSION_SLASH_COMMAND_CUSTOM_TYPE, SESSION_SLASH_COMMAND_RESULT_CUSTOM_TYPE,
    };
    let custom_type = message
        .get("customType")
        .and_then(Value::as_str)
        .unwrap_or("");
    let content = message_text(message);
    let display = message
        .get("display")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !display {
        return Vec::new();
    }
    let is_command_row = custom_type == SESSION_SLASH_COMMAND_CUSTOM_TYPE;
    let is_result_row = custom_type == SESSION_SLASH_COMMAND_RESULT_CUSTOM_TYPE;
    if !is_command_row && !is_result_row {
        return Vec::new();
    }
    // The content must be a text payload and (echo rows) the command details
    // must parse (TS `isSessionSlashCommandMessage`); otherwise the row
    // renders the malformed notice.
    let content_is_text = match message.get("content") {
        Some(Value::String(_)) => true,
        Some(Value::Array(blocks)) => {
            blocks.len() == 1
                && matches!(
                    blocks[0].get("type").and_then(Value::as_str),
                    Some("text") | None
                )
        }
        _ => false,
    };
    let command_details_valid = message
        .get("details")
        .and_then(|details| details.get("command"))
        .is_some_and(|command| {
            command.get("name").is_some()
                && command.get("args").is_some()
                && command.get("text").is_some()
        });
    if !content_is_text || (is_command_row && !command_details_valid) {
        return vec![ChatEntry::User {
            text: "[Malformed session command message]".to_string(),
        }];
    }
    if is_command_row {
        vec![ChatEntry::SlashCommand { text: content }]
    } else {
        vec![ChatEntry::SlashCommandResult { content }]
    }
}

/// Concatenated text of a raw daemon message (string or block content).
pub fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(block_text)
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Text of one content block: tagged text blocks and the engine's untagged
/// `{"text": ...}` form. Adjacent fragments of one message concatenate
/// without separators, like the TS message rendering.
fn block_text(block: &Value) -> Option<String> {
    match block {
        Value::Object(_) => block
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string),
        Value::String(text) => Some(text.clone()),
        _ => None,
    }
}

/// Fold one raw message into chat entries. Assistant messages expand into a
/// message component (ordered text/thinking blocks) plus one card per tool
/// call, in content order.
pub fn message_value_to_entries(message: &Value) -> Vec<ChatEntry> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match role {
        "user" => vec![ChatEntry::User {
            text: content_to_text(message.get("content").unwrap_or(&Value::Null)),
        }],
        "assistant" => assistant_value_to_entries(message),
        "custom" => custom_message_entries(message),
        // Other roles (tool results, bookkeeping) have no rendering here:
        // live tool results arrive as tool_execution events instead.
        _ => Vec::new(),
    }
}

/// Decode an assistant wire message into a message component plus tool cards.
pub fn assistant_value_to_entries(message: &Value) -> Vec<ChatEntry> {
    let (blocks, tool_calls) = assistant_message_parts(message);
    if blocks.is_empty() && tool_calls.is_empty() {
        return Vec::new();
    }
    let mut entries = Vec::new();
    // TS `AssistantMessageComponent.rebuild`: an abort renders its error row
    // inside the message; a provider error renders only without tool calls
    // (their cards carry the failure).
    let stop_reason = message.get("stopReason").and_then(Value::as_str);
    let error = match stop_reason {
        Some("aborted") => Some(
            message
                .get("errorMessage")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty() && *text != "Request was aborted")
                .unwrap_or("Operation aborted")
                .to_string(),
        ),
        Some("error") if tool_calls.is_empty() => Some(format!(
            "Error: {}",
            message
                .get("errorMessage")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .unwrap_or("Unknown error")
        )),
        _ => None,
    };
    let aborted = stop_reason == Some("aborted");
    if !blocks.is_empty() || error.is_some() {
        entries.push(ChatEntry::Assistant(Box::new(AssistantMessage {
            blocks,
            has_tool_calls: !tool_calls.is_empty(),
            streaming: false,
            error,
            aborted,
        })));
    }
    for (id, name, args) in tool_calls {
        entries.push(ChatEntry::Tool(Box::new(ToolCallCard {
            id,
            name,
            args,
            started: false,
            ..Default::default()
        })));
    }
    entries
}

/// The ordered visible blocks (thinking, text) and tool calls of one
/// assistant wire message.
pub fn assistant_message_parts(
    message: &Value,
) -> (Vec<MessageBlock>, Vec<(String, String, Value)>) {
    let mut blocks = Vec::new();
    let mut tool_calls = Vec::new();
    match message.get("content") {
        Some(Value::String(text)) => {
            if !text.is_empty() {
                blocks.push(MessageBlock::Text(text.clone()));
            }
        }
        Some(Value::Array(array)) => {
            for block in array {
                let block_type = block.get("type").and_then(Value::as_str);
                match block_type {
                    Some("thinking") => {
                        let thinking = block.get("thinking").and_then(Value::as_str);
                        if let Some(thinking) = thinking.filter(|text| !text.trim().is_empty()) {
                            blocks.push(MessageBlock::Thinking(thinking.to_string()));
                        }
                    }
                    Some("text") => {
                        let text = block.get("text").and_then(Value::as_str);
                        if let Some(text) = text.filter(|text| !text.trim().is_empty()) {
                            blocks.push(MessageBlock::Text(text.to_string()));
                        }
                    }
                    Some("toolCall") => {
                        tool_calls.push((
                            block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            block.get("arguments").cloned().unwrap_or(Value::Null),
                        ));
                    }
                    None => {
                        // Untagged text blocks (the scripted engine's form).
                        let text = block.get("text").and_then(Value::as_str);
                        if let Some(text) = text.filter(|text| !text.is_empty()) {
                            blocks.push(MessageBlock::Text(text.to_string()));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    (blocks, tool_calls)
}

fn content_to_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(block_text)
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn slim_attach() -> Value {
        json!({
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "activeSessionId": "abc123def456",
            "snapshot": {
                "activeSessionId": "abc123def456",
                "summary": { "id": "abc123def456", "cwd": "/tmp" },
                "state": {
                    "activeSessionId": "abc123def456",
                    "cwd": "/tmp",
                    "sessionId": "0199-sess",
                    "sessionName": "my session",
                    "model": null,
                    "thinkingLevel": "default",
                    "serviceTier": "auto",
                    "isStreaming": false,
                    "isCompacting": false,
                    "retryAttempt": 0,
                    "steeringMode": "all",
                    "followUpMode": "all",
                    "autoCompactionEnabled": false,
                    "messageCount": 2,
                    "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
                    "compactionCount": 0,
                    "goal": null,
                    "scopedModels": [],
                    "activeToolNames": [],
                },
                "messages": [
                    { "role": "user", "content": "hello", "timestamp": 1 },
                    { "role": "assistant", "content": "hi there", "provider": "scripted", "model": "faux-1", "usage": { "input": 120, "output": 8 }, "timestamp": 2 },
                ],
                "lastEventSequence": 9,
                "lastEventCursor": { "generation": "g", "sequence": 9 },
                "children": [],
            },
            "replay": { "status": "complete", "toSequence": 9, "toCursor": { "generation": "g", "sequence": 9 } },
            "lastEventSequence": 9,
            "lastEventCursor": { "generation": "g", "sequence": 9 },
            "client": { "id": "c1", "capabilities": ["attach_snapshot", "event_sequence", "slim_attach"] },
        })
    }

    #[test]
    fn reconstructs_slim_attach() {
        let data = attach_data_from_response(&slim_attach()).unwrap();
        assert_eq!(data.active_session_id, "abc123def456");
        let view = reconstruct(&data);
        assert_eq!(view.chat.len(), 2);
        assert!(matches!(&view.chat[0], ChatEntry::User { text } if text == "hello"));
        assert!(matches!(&view.chat[1], ChatEntry::Assistant(m) if m.blocks
            == vec![MessageBlock::Text("hi there".to_string())]));
        assert_eq!(view.session_id, "0199-sess");
        assert_eq!(view.session_name.as_deref(), Some("my session"));
        assert_eq!(view.last_event_sequence, 9);
    }

    #[test]
    fn decodes_auto_retry_events() {
        let start = event_to_update(&json!({
            "type": "auto_retry_start",
            "attempt": 1,
            "maxAttempts": 2,
            "delayMs": 50,
            "errorMessage": "provider down",
        }))
        .expect("retry start maps");
        assert_eq!(
            start,
            TurnUpdate::AutoRetryStart {
                attempt: 1,
                max_attempts: 2,
                delay_ms: 50,
            }
        );
        let end = event_to_update(&json!({
            "type": "auto_retry_end",
            "success": false,
            "attempt": 2,
            "finalError": "provider down",
        }))
        .expect("retry end maps");
        assert_eq!(
            end,
            TurnUpdate::AutoRetryEnd {
                success: false,
                attempt: 2,
                final_error: Some("provider down".to_string()),
            }
        );
        let settled = event_to_update(&json!({
            "type": "auto_retry_end",
            "success": true,
            "attempt": 2,
        }))
        .expect("retry success maps");
        assert_eq!(
            settled,
            TurnUpdate::AutoRetryEnd {
                success: true,
                attempt: 2,
                final_error: None,
            }
        );
    }

    #[test]
    fn failed_assistant_message_end_maps_final() {
        let update = event_to_update(&json!({
            "type": "message_end",
            "message": {
                "role": "assistant",
                "stopReason": "error",
                "errorMessage": "Provider server error",
                "content": [],
            },
        }))
        .expect("failed message_end maps");
        match update {
            TurnUpdate::AssistantMessage {
                streaming, message, ..
            } => {
                assert!(!streaming, "message_end is final");
                assert_eq!(message["stopReason"], "error");
            }
            other => panic!("unexpected update: {other:?}"),
        }
    }

    #[test]
    fn decodes_block_content() {
        let items = message_value_to_entries(&json!({
            "role": "user",
            "content": [{ "text": "hello " }, { "text": "world" }],
        }));
        assert_eq!(
            items,
            vec![ChatEntry::User {
                text: "hello world".to_string()
            }]
        );
        let items = message_value_to_entries(&json!({
            "role": "assistant",
            "content": [
                { "type": "thinking", "thinking": "hmm" },
                { "type": "text", "text": "working" },
                { "type": "toolCall", "id": "t1", "name": "bash", "arguments": { "command": "ls" } },
            ],
        }));
        assert_eq!(items.len(), 2);
        assert!(matches!(
            &items[0],
            ChatEntry::Assistant(m) if m.blocks.len() == 2 && m.has_tool_calls
        ));
        assert!(matches!(&items[1], ChatEntry::Tool(card) if card.name == "bash"));
    }

    #[test]
    fn decodes_streamed_events() {
        let user = event_to_update(&json!({
            "type": "message_start",
            "message": { "role": "user", "content": "go" },
        }))
        .unwrap();
        assert_eq!(user, TurnUpdate::UserMessage("go".to_string()));
        let partial = event_to_update(&json!({
            "type": "message_update",
            "message": { "role": "assistant", "content": "work" },
        }))
        .unwrap();
        assert!(matches!(
            &partial,
            TurnUpdate::AssistantMessage { message, streaming: true, .. } if message["content"] == "work"
        ));
        let final_message = event_to_update(&json!({
            "type": "message_end",
            "message": { "role": "assistant", "content": "done" },
        }))
        .unwrap();
        assert!(matches!(
            &final_message,
            TurnUpdate::AssistantMessage { message, streaming: false, .. } if message["content"] == "done"
        ));
        let ended = event_to_update(&json!({ "type": "turn_end" })).unwrap();
        assert_eq!(ended, TurnUpdate::TurnEnded { error: None });
        let failed = event_to_update(&json!({ "type": "turn_end", "error": "boom" })).unwrap();
        assert_eq!(
            failed,
            TurnUpdate::TurnEnded {
                error: Some("boom".to_string())
            }
        );
        assert_eq!(
            event_to_update(&json!({ "type": "agent_end" })),
            Some(TurnUpdate::Idle)
        );
    }

    #[test]
    fn session_command_rows_decode_once() {
        let echo = json!({
            "type": "message_start",
            "message": {
                "role": "custom",
                "customType": "session_slash_command",
                "content": "/goal ship it",
                "display": true,
                "details": { "command": { "name": "goal", "args": "ship it", "text": "/goal ship it" } },
            },
        });
        assert_eq!(
            event_to_update(&echo),
            Some(TurnUpdate::CustomRow(ChatEntry::SlashCommand {
                text: "/goal ship it".to_string()
            }))
        );
        // The closing frame of the pair must not duplicate the row.
        let end = json!({
            "type": "message_end",
            "message": echo["message"].clone(),
        });
        assert_eq!(event_to_update(&end), Some(TurnUpdate::StatusUpdate));

        let result = json!({
            "type": "message_start",
            "message": {
                "role": "custom",
                "customType": "session_slash_command_result",
                "content": "Goal active: ship it",
                "display": true,
                "details": {
                    "command": { "name": "goal", "args": "ship it", "text": "/goal ship it" },
                    "success": true, "severity": "info",
                },
            },
        });
        assert_eq!(
            event_to_update(&result),
            Some(TurnUpdate::CustomRow(ChatEntry::SlashCommandResult {
                content: "Goal active: ship it".to_string()
            }))
        );
    }

    #[test]
    fn session_command_rows_respect_display_and_shape() {
        // Non-display rows (the refine result) render nothing.
        let hidden = json!({
            "role": "custom",
            "customType": "session_slash_command_result",
            "content": "Refined continual harness state: 1 edit applied.",
            "display": false,
        });
        assert!(custom_message_entries(&hidden).is_empty());
        // Unknown custom types render nothing.
        let other = json!({
            "role": "custom",
            "customType": "harness_digest",
            "content": "digest",
            "display": true,
        });
        assert!(custom_message_entries(&other).is_empty());
        // A command row without command details renders the malformed
        // notice (TS `isSessionSlashCommandMessage` fallback).
        let malformed = json!({
            "role": "custom",
            "customType": "session_slash_command",
            "content": "/goal",
            "display": true,
            "details": {},
        });
        assert_eq!(
            custom_message_entries(&malformed),
            vec![ChatEntry::User {
                text: "[Malformed session command message]".to_string()
            }]
        );
    }

    /// A transcript with tool calls replays the way the TS attach does: the
    /// assistant's tool card stays pending until the matching
    /// `role: "toolResult"` message completes it; orphan results render
    /// nothing.
    #[test]
    fn transcript_replay_completes_tool_cards() {
        let transcript = [
            json!({ "role": "user", "content": "run it", "timestamp": 1 }),
            json!({
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "calling" },
                    { "type": "toolCall", "id": "call-1", "name": "ipython", "arguments": {"code": "1"} },
                ],
                "provider": "faux", "model": "faux-1",
                "usage": { "input": 10, "output": 2 }, "stopReason": "toolUse",
                "timestamp": 2,
            }),
            json!({
                "role": "toolResult",
                "toolCallId": "call-1",
                "toolName": "ipython",
                "content": [{ "type": "text", "text": "42" }],
                "details": { "durationMs": 3, "status": "ok" },
                "isError": false,
                "timestamp": 3,
            }),
            json!({
                "role": "toolResult",
                "toolCallId": "orphan",
                "toolName": "ipython",
                "content": [{ "type": "text", "text": "no card" }],
                "isError": false,
                "timestamp": 4,
            }),
            json!({
                "role": "assistant",
                "content": [{ "type": "text", "text": "done" }],
                "provider": "faux", "model": "faux-1",
                "usage": { "input": 10, "output": 2 }, "stopReason": "stop",
                "timestamp": 5,
            }),
        ];
        let chat = transcript_to_entries(&transcript);
        // user row, assistant text, tool card, final assistant text.
        assert_eq!(chat.len(), 4, "chat: {chat:?}");
        let Some(ChatEntry::Tool(card)) = chat.get(2) else {
            panic!("tool card at index 2: {chat:?}");
        };
        assert!(card.started);
        assert!(!card.result_partial);
        let result = card.result.as_ref().expect("result replayed");
        assert_eq!(
            result.content,
            vec![json!({ "type": "text", "text": "42" })]
        );
        assert_eq!(result.details, json!({ "durationMs": 3, "status": "ok" }));
        assert!(!result.is_error);
    }

    /// A pending card (result absent) replays with no result, like a turn
    /// still in flight when the session was last persisted.
    #[test]
    fn transcript_replay_keeps_pending_cards_without_results() {
        let transcript = [
            json!({ "role": "user", "content": "run it", "timestamp": 1 }),
            json!({
                "role": "assistant",
                "content": [
                    { "type": "toolCall", "id": "call-1", "name": "ipython", "arguments": {} },
                ],
                "provider": "faux", "model": "faux-1",
                "usage": { "input": 10, "output": 2 }, "stopReason": "toolUse",
                "timestamp": 2,
            }),
        ];
        let chat = transcript_to_entries(&transcript);
        let Some(ChatEntry::Tool(card)) = chat.get(1) else {
            panic!("tool card at index 1: {chat:?}");
        };
        assert!(!card.started);
        assert!(card.result.is_none());
    }

    /// `Reconstructed::push_message` folds a late `toolResult` message onto
    /// the card an earlier chunk added (streamed snapshot reassembly).
    #[test]
    fn push_message_completes_pending_tool_card() {
        let mut reconstructed = Reconstructed::default();
        reconstructed.push_message(&json!({
            "role": "assistant",
            "content": [
                { "type": "toolCall", "id": "call-1", "name": "ipython", "arguments": {} },
            ],
            "provider": "faux", "model": "faux-1",
            "usage": { "input": 10, "output": 2 }, "stopReason": "toolUse",
            "timestamp": 2,
        }));
        reconstructed.push_message(&json!({
            "role": "toolResult",
            "toolCallId": "call-1",
            "toolName": "ipython",
            "content": [{ "type": "text", "text": "out" }],
            "isError": true,
            "timestamp": 3,
        }));
        assert_eq!(reconstructed.chat.len(), 1);
        let Some(ChatEntry::Tool(card)) = reconstructed.chat.first() else {
            panic!("single tool card: {:?}", reconstructed.chat);
        };
        let result = card.result.as_ref().expect("result applied");
        assert!(result.is_error);
        assert_eq!(
            result.content,
            vec![json!({ "type": "text", "text": "out" })]
        );
    }
}
