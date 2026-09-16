//! `/compact` execution: resolve the cut over session entries, run the
//! summarizer, persist the compaction entry, and rebuild the agent context.

use pa_types::ai::{AssistantMessage, Message, UserContent};
use pa_types::session::{AgentMessage, FileEntry};

use super::compaction::{
    build_summarization_prompt, calculate_context_tokens, estimate_tokens, find_cut_point,
    CutPointResult,
};
use super::compaction_exec::{
    compaction_entry_for, details_for, CompactionDetails, CompactionResult,
};
use super::compaction_utils::serialize_conversation;
use super::messages::convert_to_llm;
use crate::session::manager::SessionManager;

/// Options for `execute_compaction`.
pub struct CompactOptions<'a> {
    /// The model used for summarization.
    pub model: pa_types::ai::Model,
    /// Resolved API key (None falls back to provider env resolution).
    pub api_key: Option<String>,
    /// `/compact <instructions>` guidance.
    pub custom_instructions: Option<&'a str>,
    /// Compaction settings (reserve/keep budgets).
    pub settings: super::compaction::CompactionSettings,
}

/// The model-visible message produced by a session entry (summarizer input).
fn message_from_entry(entry: &FileEntry) -> Option<AgentMessage> {
    match entry {
        FileEntry::Message { message, .. } => match message {
            AgentMessage::ToolResult(_) => None,
            _ => Some(message.clone()),
        },
        FileEntry::CustomMessage { payload, .. } => {
            if payload.custom_type == "harness_digest" {
                return None;
            }
            Some(AgentMessage::Custom(pa_types::session::CustomMessage {
                custom_type: payload.custom_type.clone(),
                content: payload.content.clone(),
                display: payload.display,
                details: payload.details.clone(),
                timestamp: crate::session::timestamp_to_millis(entry.timestamp()),
                rest: Default::default(),
            }))
        }
        FileEntry::BranchSummary { payload, .. } => Some(AgentMessage::BranchSummary(
            pa_types::session::BranchSummaryMessage {
                summary: payload.summary.clone(),
                from_id: payload.from_id.clone(),
                timestamp: crate::session::timestamp_to_millis(entry.timestamp()),
            },
        )),
        // Prior compactions are kept context, not summarizer input; the new
        // compaction covers their retained span.
        FileEntry::Compaction { .. } => None,
        _ => None,
    }
}

fn context_tokens(entries: &[FileEntry]) -> u64 {
    entries
        .iter()
        .rev()
        .find_map(|entry| match entry {
            FileEntry::Message {
                message: AgentMessage::Assistant(assistant),
                ..
            } => Some(calculate_context_tokens(&assistant.usage)),
            _ => None,
        })
        .unwrap_or_else(|| {
            entries
                .iter()
                .filter_map(message_from_entry)
                .map(|message: AgentMessage| estimate_tokens(&message))
                .sum()
        })
}

/// Session AgentMessage -> LLM Message (post convertToLlm).
fn to_llm_messages(messages: &[AgentMessage]) -> Vec<Message> {
    convert_to_llm(messages)
        .into_iter()
        .filter_map(|message| match message {
            AgentMessage::User(user) => Some(Message::User(user)),
            AgentMessage::Assistant(assistant) => Some(Message::Assistant(assistant)),
            AgentMessage::ToolResult(result) => Some(Message::ToolResult(result)),
            _ => None,
        })
        .collect()
}

/// Run compaction over the session: summarize the pre-cut prefix, persist the
/// entry, and return the rebuilt post-compaction context messages.
pub async fn execute_compaction(
    session: &mut SessionManager,
    options: CompactOptions<'_>,
) -> anyhow::Result<CompactionResult> {
    let entries = session.get_all_entries().to_vec();
    // The header is not a compact candidate.
    let start = usize::from(matches!(entries.first(), Some(FileEntry::Header { .. })));
    let cut = find_cut_point(
        &entries,
        start,
        entries.len(),
        options.settings.keep_recent_tokens,
    );
    let first_kept_entry = entries
        .get(cut.first_kept_entry_index)
        .and_then(|entry| entry.id())
        .unwrap_or_default()
        .to_string();

    // Messages the summarizer sees: everything before the cut.
    let messages: Vec<AgentMessage> = entries[..cut.first_kept_entry_index]
        .iter()
        .filter_map(message_from_entry)
        .collect();
    let tokens_before = context_tokens(&entries);
    let prev_compaction_index = entries[..cut.first_kept_entry_index]
        .iter()
        .rposition(|entry| matches!(entry, FileEntry::Compaction { .. }));
    let details: CompactionDetails = details_for(&messages, &entries, prev_compaction_index);

    // The summarization request (conversation + prompt).
    let conversation_text = serialize_conversation(&convert_to_llm(&messages));
    let mut prompt_text = format!("<conversation>\n{conversation_text}\n</conversation>\n\n");
    prompt_text.push_str(&build_summarization_prompt(
        options.custom_instructions,
        None,
    ));
    let request_messages = vec![Message::User(pa_types::ai::UserMessage {
        content: UserContent::Text(prompt_text),
        timestamp: 0,
        rest: Default::default(),
    })];
    let max_tokens = options.settings.reserve_tokens / 5 * 4; // floor(0.8 * reserve)
    let context = pa_types::ai::Context {
        system_prompt: Some(
            crate::session_engine::compaction_utils::SUMMARIZATION_SYSTEM_PROMPT.to_string(),
        ),
        messages: request_messages,
        tools: None,
    };
    let stream_options =
        pa_ai::types::SimpleStreamOptions::from_base(pa_ai::types::StreamOptions {
            max_tokens: Some(max_tokens),
            api_key: options.api_key,
            ..Default::default()
        });
    let assistant = pa_ai::complete_simple(&options.model, &context, Some(stream_options)).await?;
    let assistant: AssistantMessage = assistant;

    // Result + persistence.
    let summary = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            pa_types::ai::AssistantContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let result = CompactionResult {
        summary,
        first_kept_entry_id: first_kept_entry.clone(),
        tokens_before,
        usage: Some(assistant.usage),
    };
    let entry = compaction_entry_for(&result, &details, options.custom_instructions);
    session.append_compaction(
        &entry.summary,
        &entry.first_kept_entry_id,
        entry.tokens_before,
    );
    Ok(result)
}

/// Rebuild the agent's message list after compaction (summary-first context).
pub fn rebuilt_context_after_compaction(session: &SessionManager) -> Vec<Message> {
    let entries = session.get_all_entries();
    let context = crate::session::build_session_context(entries, session.get_leaf_id());
    to_llm_messages(&context.messages)
}

/// The cut computed for a session (test seam for decision verification).
pub fn compute_cut(session: &SessionManager, keep_recent_tokens: u64) -> (CutPointResult, u64) {
    let entries = session.get_all_entries();
    let start = usize::from(matches!(entries.first(), Some(FileEntry::Header { .. })));
    let cut = find_cut_point(entries, start, entries.len(), keep_recent_tokens);
    let tokens = context_tokens(entries);
    (cut, tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_types::session::EntryBase;

    fn session_with_turns(cwd: &std::path::Path, turns: usize) -> SessionManager {
        let mut session = SessionManager::in_memory(cwd);
        for i in 0..turns {
            session.append_message(AgentMessage::User(pa_types::ai::UserMessage {
                content: UserContent::Text(format!("turn {i} message with some words")),
                timestamp: 0,
                rest: Default::default(),
            }));
            session.append_message(AgentMessage::Assistant(AssistantMessage {
                content: vec![pa_types::ai::AssistantContentBlock::Text(
                    pa_types::ai::TextContent {
                        text: format!("reply {i}"),
                        text_signature: None,
                        rest: Default::default(),
                    },
                )],
                api: "openai-completions".to_string(),
                provider: "test".to_string(),
                model: "m".to_string(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: pa_types::ai::Usage {
                    input: 100,
                    output: 20,
                    cache_read: 0,
                    cache_write: 0,
                    total_tokens: 120,
                    cost: Default::default(),
                },
                stop_reason: pa_types::ai::StopReason::Stop,
                stop_reason_raw: None,
                error_message: None,
                timestamp: 0,
                rest: Default::default(),
            }));
        }
        session
    }

    #[test]
    fn cut_and_tokens_computed_from_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let session = session_with_turns(tmp.path(), 3);
        let (cut, tokens) = compute_cut(&session, 10_000);
        // A large keep budget keeps from the start.
        assert_eq!(cut.first_kept_entry_index, 1); // after the header
        assert_eq!(tokens, 120);
    }

    #[test]
    fn message_extraction_skips_compaction_and_tool_results() {
        let mut compaction = FileEntry::Compaction {
            payload: pa_types::session::CompactionEntry {
                summary: "s".to_string(),
                first_kept_entry_id: "x".to_string(),
                tokens_before: 1,
                details: None,
                from_hook: None,
                custom_instructions: None,
                usage: None,
                harness_digest: None,
            },
            base: EntryBase {
                id: Some("c".to_string()),
                parent_id: None,
                timestamp: Some("2024-01-01T00:00:00.000Z".to_string()),
                rest: Default::default(),
            },
        };
        let _ = &mut compaction;
        assert!(message_from_entry(&compaction).is_none());
        let tool_result = FileEntry::Message {
            message: AgentMessage::ToolResult(pa_types::ai::ToolResultMessage {
                tool_call_id: "c".to_string(),
                tool_name: "bash".to_string(),
                content: vec![],
                details: None,
                is_error: false,
                timestamp: 0,
                rest: Default::default(),
            }),
            base: EntryBase {
                id: Some("t".to_string()),
                parent_id: None,
                timestamp: Some("2024-01-01T00:00:00.000Z".to_string()),
                rest: Default::default(),
            },
        };
        assert!(message_from_entry(&tool_result).is_none());
    }

    #[tokio::test]
    async fn execute_compaction_persists_and_rebuilds() {
        let registration =
            pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
                models: Some(vec![pa_ai::faux::FauxModelDefinition {
                    id: "compact-m".to_string(),
                    name: Some("Compact Model".to_string()),
                    reasoning: Some(false),
                    input: Some(vec![pa_types::ai::ModelInput::Text]),
                    cost: None,
                    context_window: Some(1_000),
                    max_tokens: Some(256),
                }]),
                ..Default::default()
            });
        registration.set_responses(vec![pa_ai::faux::FauxResponseStep::Message(
            pa_ai::faux::faux_assistant_text_message(
                "## Goal\nsummarized goal",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ),
        )]);
        let model = registration.get_model();
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session_with_turns(tmp.path(), 3);
        let result = execute_compaction(
            &mut session,
            CompactOptions {
                model,
                api_key: None,
                custom_instructions: Some("focus on the goal"),
                settings: super::super::compaction::CompactionSettings::default(),
            },
        )
        .await
        .unwrap();
        assert!(result.summary.contains("summarized goal"));
        assert!(result.usage.is_some());
        // The compaction entry persisted on the session.
        assert!(session
            .get_entries()
            .iter()
            .any(|entry| matches!(entry, FileEntry::Compaction { .. })));
        // The rebuilt context starts with the summary message.
        let rebuilt = rebuilt_context_after_compaction(&session);
        assert!(!rebuilt.is_empty());
        match &rebuilt[0] {
            Message::User(user) => assert!(user.content.text().contains("[compaction-summary]")),
            other => panic!("expected summary user message, got {other:?}"),
        }
        registration.unregister();
    }
}
