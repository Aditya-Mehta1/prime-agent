//! Headless session-message stream: the same UI renders history JSONL
//! (captured sessions under `~/.prime/agent/sessions`) and live events.
//!
//! `SessionStream` is the seam the interactive mode and the replay binary
//! share; `JsonlSessionStream` implements it over pa-types session entries.

use anyhow::{Context, Result};
use pa_types::session::{AgentMessage, FileEntry};
use std::path::Path;

/// A transcript item rendered by the agent view.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptItem {
    UserMessage {
        text: String,
    },
    Assistant {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        text: String,
        /// The full wire content blocks (text and image), so replayed tool
        /// results render their image rows like live ones.
        content: Vec<serde_json::Value>,
    },
    BashExecution {
        command: String,
        output: String,
        exit_code: Option<i64>,
    },
    AgentStatus {
        summary: String,
        task_state: String,
    },
    ModelChange {
        provider: String,
        model_id: String,
    },
    /// Client-side notice (command output, errors, list rows) rendered muted.
    SystemNote {
        text: String,
    },
    /// One decoded custom-message row (agent messages, injected prompts,
    /// outcomes, and the generic custom box).
    CustomRow {
        entry: crate::chat::ChatEntry,
    },
}

/// Live event surfaced through a [`SessionStream`].
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// A new transcript item appended to the view.
    Item(TranscriptItem),
    /// Stream finished (no more events).
    End,
}

/// Source of session events. Implementations range from a JSONL capture
/// (replay) to a live daemon connection.
pub trait SessionStream: Send {
    fn poll(&mut self) -> Result<SessionEvent>;
}

/// Stream a recorded session JSONL file entry by entry.
pub struct JsonlSessionStream {
    entries: std::vec::IntoIter<FileEntry>,
    pending: Vec<TranscriptItem>,
    finished: bool,
}

impl JsonlSessionStream {
    pub fn from_entries(entries: Vec<FileEntry>) -> Self {
        Self {
            entries: entries.into_iter(),
            pending: Vec::new(),
            finished: false,
        }
    }

    /// Load all entries from a session JSONL file (skip undecodable lines).
    pub fn from_path(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading session {}", path.display()))?;
        let entries = parse_jsonl(&raw)?;
        Ok(Self::from_entries(entries))
    }
}

pub fn parse_jsonl(raw: &str) -> Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<FileEntry>(line) {
            Ok(entry) => entries.push(entry),
            Err(e) => {
                return Err(anyhow::anyhow!("line {}: {}", i + 1, e))
                    .with_context(|| format!("parsing session entry {}", i + 1))
            }
        }
    }
    Ok(entries)
}

/// Fold a session entry into the transcript (paired tool results attach to
/// nothing here; they render as their own panel lines).
pub fn entry_to_items(entry: &FileEntry) -> Vec<TranscriptItem> {
    match entry {
        FileEntry::Message { message, .. } => message_to_items(message),
        FileEntry::AgentStatus { payload, .. } => vec![TranscriptItem::AgentStatus {
            summary: payload.status.summary.clone(),
            task_state: payload
                .status
                .task_state
                .map(|t| format!("{:?}", t).to_lowercase())
                .unwrap_or_default(),
        }],
        FileEntry::ModelChange { payload, .. } => vec![TranscriptItem::ModelChange {
            provider: payload.provider.clone(),
            model_id: payload.model_id.clone(),
        }],
        // Custom rows rejoin as their wire message form and decode through
        // the same custom-type dispatch the live path uses.
        FileEntry::CustomMessage { payload, .. } => {
            let message = custom_message_wire_value(payload);
            crate::custom_message::custom_message_entries(&message)
                .into_iter()
                .map(|entry| TranscriptItem::CustomRow { entry })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Rebuild the `role: "custom"` wire message shape from a persisted
/// `custom_message` entry (the same rejoin the daemon session store and
/// the TS session manager perform on load).
fn custom_message_wire_value(payload: &pa_types::session::CustomMessageEntry) -> serde_json::Value {
    let content = match serde_json::to_value(&payload.content) {
        Ok(value) => value,
        Err(_) => serde_json::Value::Null,
    };
    serde_json::json!({
        "role": "custom",
        "customType": payload.custom_type,
        "content": content,
        "display": payload.display,
        "details": payload.details.clone().unwrap_or(serde_json::Value::Null),
    })
}

fn message_to_items(message: &AgentMessage) -> Vec<TranscriptItem> {
    match message {
        AgentMessage::User(u) => vec![TranscriptItem::UserMessage {
            // TS `readUserText` + the image-only placeholder: a prompt
            // with content but no text shows `[image]` instead of
            // rendering nothing.
            text: user_display_text(&u.content),
        }],
        AgentMessage::Assistant(a) => {
            let mut items = Vec::new();
            for block in &a.content {
                match block {
                    pa_types::ai::AssistantContentBlock::Text(t) => {
                        items.push(TranscriptItem::Assistant {
                            text: t.text.clone(),
                        })
                    }
                    pa_types::ai::AssistantContentBlock::ToolCall(tc) => {
                        items.push(TranscriptItem::ToolCall {
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            arguments: serde_json::to_string(&tc.arguments).unwrap_or_default(),
                        })
                    }
                    // Thinking blocks render as collapsible UI in TS; replay
                    // keeps them out of the transcript by default.
                    pa_types::ai::AssistantContentBlock::Thinking(_) => {}
                }
            }
            items
        }
        AgentMessage::ToolResult(t) => vec![TranscriptItem::ToolResult {
            tool_call_id: t.tool_call_id.clone(),
            tool_name: t.tool_name.clone(),
            text: tool_result_text(&t.content),
            content: t
                .content
                .iter()
                .map(|block| match serde_json::to_value(block) {
                    Ok(value) => value,
                    Err(_) => serde_json::Value::Null,
                })
                .collect(),
        }],
        AgentMessage::BashExecution(b) => vec![TranscriptItem::BashExecution {
            command: b.command.clone(),
            output: b.output.clone(),
            exit_code: b.exit_code,
        }],
        // Custom/branch/compaction messages carry UI-specific payloads; the
        // standard agent view skips non-displayed ones.
        _ => Vec::new(),
    }
}

/// The concatenated text of a replayed tool result's text blocks
/// (un-modeled blocks have no display text, TS renders only typed text
/// blocks).
fn tool_result_text(content: &[pa_types::ai::UserContentBlock]) -> String {
    content
        .iter()
        .map(|block| match block {
            pa_types::ai::UserContentBlock::Text(text) => text.text.clone(),
            pa_types::ai::UserContentBlock::Image(_) | pa_types::ai::UserContentBlock::Raw(_) => {
                String::new()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The user-message display text (TS `conversation-components`' user
/// branch): the text blocks joined, or the `[image]` placeholder when the
/// message carries content but no text.
fn user_display_text(content: &pa_types::ai::UserContent) -> String {
    let text = content.text();
    if !text.is_empty() {
        return text;
    }
    match content {
        pa_types::ai::UserContent::Text(text) if !text.is_empty() => "[image]".to_string(),
        pa_types::ai::UserContent::Blocks(blocks) if !blocks.is_empty() => "[image]".to_string(),
        _ => String::new(),
    }
}

impl SessionStream for JsonlSessionStream {
    fn poll(&mut self) -> Result<SessionEvent> {
        loop {
            if let Some(item) = self.pending.first().cloned() {
                self.pending.remove(0);
                return Ok(SessionEvent::Item(item));
            }
            if self.finished {
                return Ok(SessionEvent::End);
            }
            match self.entries.next() {
                Some(entry) => {
                    let items = entry_to_items(&entry);
                    if items.is_empty() {
                        continue;
                    }
                    self.pending = items[1..].to_vec();
                    return Ok(SessionEvent::Item(items[0].clone()));
                }
                None => {
                    self.finished = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_message_entries_rejoin_and_decode() {
        // A persisted custom_message entry rejoins as its wire shape and
        // decodes through the same custom-type dispatch the live path
        // uses; a non-display row renders nothing.
        let entry = |display: bool| FileEntry::CustomMessage {
            payload: pa_types::session::CustomMessageEntry {
                custom_type: "agent_message".to_string(),
                content: pa_types::ai::UserContent::Text(
                    "[agent-message from child:lane]\n\nhi".to_string(),
                ),
                details: Some(serde_json::json!({
                    "id": "agentmsg_t1",
                    "message": "hi",
                    "from": { "sessionName": "lane" },
                    "fromRelationship": "child",
                })),
                display,
                rest: serde_json::Map::new(),
            },
            base: pa_types::session::EntryBase {
                id: Some("e1".to_string()),
                parent_id: None,
                timestamp: None,
                rest: serde_json::Map::new(),
            },
        };
        let items = entry_to_items(&entry(true));
        let [TranscriptItem::CustomRow { entry: chat_entry }] = items.as_slice() else {
            panic!("custom row: {items:?}");
        };
        match chat_entry {
            crate::chat::ChatEntry::AgentMessage(row) => {
                assert_eq!(row.participant, "from child lane");
                assert_eq!(row.message, "hi");
            }
            other => panic!("agent row: {other:?}"),
        }
        assert!(entry_to_items(&entry(false)).is_empty());
    }

    #[test]
    fn loads_real_session() {
        let dir = std::path::Path::new("/home/ubuntu/.prime/agent/sessions");
        if !dir.is_dir() {
            return; // sandbox without agent state
        }
        let mut loaded = 0;
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(raw) = std::fs::read_to_string(&path) {
                if parse_jsonl(&raw).is_ok() {
                    loaded += 1;
                }
            }
        }
        assert!(loaded > 0, "no sessions parsed");
    }
}
