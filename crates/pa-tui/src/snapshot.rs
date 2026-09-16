//! Attach/snapshot reconstruction: wire data from the daemon (slim attach
//! results, streamed session events) folded into UI transcript items.
//!
//! Daemon message payloads are raw JSON (`Value`): the session engine owns
//! their evolution, and the TUI renders what arrives. Message decoding is
//! therefore lenient — it accepts plain-string content and content-block
//! arrays, with or without explicit block `type` tags, covering the shapes
//! the scripted harness and the real engine both emit.

use crate::session::TranscriptItem;
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

/// A reconstructed attach: view-ready transcript plus identity/state labels.
#[derive(Debug, Clone, Default)]
pub struct Reconstructed {
    pub transcript: Vec<TranscriptItem>,
    /// Model label for the footer (`state.model`).
    pub model_label: String,
    /// Session display name for the footer.
    pub session_name: Option<String>,
    /// Session id of the persisted session file.
    pub session_id: String,
    pub last_event_sequence: u64,
}

impl Reconstructed {
    /// Fold one raw message into the transcript.
    pub fn push_message(&mut self, message: &Value) {
        self.transcript.extend(message_value_to_items(message));
    }
}

/// Reconstruct the view state from slim attach data.
pub fn reconstruct(attach: &AttachData) -> Reconstructed {
    let snapshot = &attach.snapshot;
    let messages = snapshot
        .get("messages")
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .flat_map(message_value_to_items)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let state = snapshot.get("state");
    let model_label = state
        .and_then(|state| state.get("model"))
        .and_then(model_label_value)
        .unwrap_or_default();
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
        transcript: messages,
        model_label,
        session_name,
        session_id,
        last_event_sequence,
    }
}

/// Parse attach data out of a successful attach/create response payload.
pub fn attach_data_from_response(data: &Value) -> anyhow::Result<AttachData> {
    serde_json::from_value(data.clone()).map_err(|error| {
        anyhow::anyhow!("the daemon returned an unrecognizable attach result: {error}")
    })
}

fn model_label_value(model: &Value) -> Option<String> {
    match model {
        Value::String(label) => Some(label.clone()),
        Value::Object(map) => {
            let model_id = map.get("modelId").and_then(Value::as_str)?;
            let provider = map.get("provider").and_then(Value::as_str);
            Some(match provider {
                Some(provider) => format!("{provider}/{model_id}"),
                None => model_id.to_string(),
            })
        }
        _ => None,
    }
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
    /// message; `streaming` distinguishes in-flight from final.
    AssistantMessage { text: String, streaming: bool },
    /// `turn_end`, with the turn error string when the turn failed.
    TurnEnded { error: Option<String> },
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
            let message = event.get("message")?;
            let event_type = event.get("type").and_then(Value::as_str);
            let streaming = event_type != Some("message_end");
            match message.get("role").and_then(Value::as_str) {
                // User messages carry the full payload on start; only a
                // partial user frame would be a protocol anomaly.
                Some("user") if event_type == Some("message_update") => {
                    Some(TurnUpdate::StatusUpdate)
                }
                Some("user") => Some(TurnUpdate::UserMessage(message_text(message))),
                Some("assistant") => Some(TurnUpdate::AssistantMessage {
                    text: message_text(message),
                    streaming,
                }),
                _ => Some(TurnUpdate::StatusUpdate),
            }
        }
        // Tool execution and queue churn only affect the status line here.
        _ => Some(TurnUpdate::StatusUpdate),
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

/// Fold one raw message into transcript items. Assistant messages may expand
/// into multiple items (text blocks plus tool calls).
/// Fold one raw message into transcript items. Assistant messages may expand
/// into multiple items (text blocks plus tool calls).
pub fn message_value_to_items(message: &Value) -> Vec<TranscriptItem> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let content = message.get("content");
    match role {
        "user" => content.map(|content| {
            vec![TranscriptItem::UserMessage {
                text: content_to_text(content),
            }]
        }),
        "assistant" => match content {
            // Plain-string assistant content: one text item.
            Some(Value::String(text)) => {
                Some(vec![TranscriptItem::Assistant { text: text.clone() }])
            }
            Some(Value::Array(blocks)) => Some(
                blocks
                    .iter()
                    .filter_map(|block| {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            return Some(TranscriptItem::Assistant {
                                text: text.to_string(),
                            });
                        }
                        if block.get("type").and_then(Value::as_str) == Some("toolCall") {
                            return Some(TranscriptItem::ToolCall {
                                id: block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                name: block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                arguments: block
                                    .get("arguments")
                                    .cloned()
                                    .map(|value| value.to_string())
                                    .unwrap_or_default(),
                            });
                        }
                        None
                    })
                    .collect(),
            ),
            _ => None,
        },
        // Other roles (tool results, bookkeeping) have no rendering here yet.
        _ => None,
    }
    .unwrap_or_default()
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
        assert_eq!(
            view.transcript,
            vec![
                TranscriptItem::UserMessage {
                    text: "hello".to_string()
                },
                TranscriptItem::Assistant {
                    text: "hi there".to_string()
                },
            ]
        );
        assert_eq!(view.session_id, "0199-sess");
        assert_eq!(view.session_name.as_deref(), Some("my session"));
        assert_eq!(view.last_event_sequence, 9);
    }

    #[test]
    fn decodes_block_content() {
        let items = message_value_to_items(&json!({
            "role": "user",
            "content": [{ "text": "hello " }, { "text": "world" }],
        }));
        assert_eq!(
            items,
            vec![TranscriptItem::UserMessage {
                text: "hello world".to_string()
            }]
        );
        let items = message_value_to_items(&json!({
            "role": "assistant",
            "content": [
                { "type": "text", "text": "working" },
                { "type": "toolCall", "id": "t1", "name": "bash", "arguments": { "command": "ls" } },
            ],
        }));
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0], TranscriptItem::Assistant { text } if text == "working"));
        assert!(matches!(
            &items[1],
            TranscriptItem::ToolCall { name, .. } if name == "bash"
        ));
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
        assert_eq!(
            partial,
            TurnUpdate::AssistantMessage {
                text: "work".to_string(),
                streaming: true
            }
        );
        let final_message = event_to_update(&json!({
            "type": "message_end",
            "message": { "role": "assistant", "content": "done" },
        }))
        .unwrap();
        assert_eq!(
            final_message,
            TurnUpdate::AssistantMessage {
                text: "done".to_string(),
                streaming: false
            }
        );
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
}
