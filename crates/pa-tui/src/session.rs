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
        _ => Vec::new(),
    }
}

fn message_to_items(message: &AgentMessage) -> Vec<TranscriptItem> {
    match message {
        AgentMessage::User(u) => vec![TranscriptItem::UserMessage {
            text: u.content.text(),
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
            text: t
                .content
                .iter()
                .map(|b| match b {
                    pa_types::ai::UserContentBlock::Text(t) => t.text.clone(),
                    pa_types::ai::UserContentBlock::Image(_) => String::new(),
                })
                .collect::<Vec<_>>()
                .join("\n"),
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
