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

/// One completed compaction run: the result plus the entry to persist.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactRun {
    pub result: CompactionResult,
    pub entry: pa_types::session::CompactionEntry,
}

/// What `/compact` did. `Skipped` carries the TS `CompactionSkippedError`
/// message; the caller treats a skip as a silent no-op (TS
/// `_executeQueuedSessionCommand` returns without a result row).
#[derive(Debug, Clone, PartialEq)]
pub enum CompactOutcome {
    Ran(Box<CompactRun>),
    Skipped(&'static str),
}

/// Why a compaction cannot prepare (TS `prepareCompaction` returning
/// `undefined`). The two surfaces spell it differently: `/compact` raises
/// the `CompactionSkippedError` message, the kernel `compact.run` host
/// request returns the short reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactSkip {
    AlreadyCompacted,
    TooShort,
}

impl CompactSkip {
    /// The `/compact` skip message (TS `CompactionSkippedError`).
    pub fn user_message(self) -> &'static str {
        match self {
            CompactSkip::AlreadyCompacted => "Already compacted",
            CompactSkip::TooShort => "Session is too short to compact — try again once it grows",
        }
    }

    /// The `compact.run` host-request reason (TS `handleCompactHostRequest`).
    pub fn request_reason(self) -> &'static str {
        match self {
            CompactSkip::AlreadyCompacted => "already compacted",
            CompactSkip::TooShort => "session is too short to compact",
        }
    }
}

/// Resolve the compaction cut and the skip guards without a model call
/// (TS `prepareCompaction`): a branch that already ends in a compaction has
/// nothing new to summarize, and a branch with no summarizable history has
/// no compaction to run.
pub fn prepare_compaction(
    entries: &[FileEntry],
    keep_recent_tokens: u64,
) -> Result<CutPointResult, CompactSkip> {
    // The header is not a compact candidate.
    let start = usize::from(matches!(entries.first(), Some(FileEntry::Header { .. })));
    let cut = find_cut_point(entries, start, entries.len(), keep_recent_tokens);
    // Skip guard (TS prepareCompaction): a branch that already ends in a
    // compaction has nothing new to summarize.
    if matches!(entries.last(), Some(FileEntry::Compaction { .. })) {
        return Err(CompactSkip::AlreadyCompacted);
    }
    // Messages the summarizer would see (TS prepareCompaction): everything
    // before the cut, plus the prefix of a split turn.
    let history_end = if cut.is_split_turn {
        cut.turn_start_index.unwrap_or(cut.first_kept_entry_index)
    } else {
        cut.first_kept_entry_index
    };
    let messages: Vec<AgentMessage> = entries[..history_end]
        .iter()
        .filter_map(message_from_entry)
        .collect();
    let turn_prefix_messages: Vec<AgentMessage> = entries[history_end..cut.first_kept_entry_index]
        .iter()
        .filter_map(message_from_entry)
        .collect();
    let has_previous_summary = entries[..cut.first_kept_entry_index]
        .iter()
        .rev()
        .any(|entry| matches!(entry, FileEntry::Compaction { .. }));
    // Avoid a compaction that would summarize no history (TS prepareCompaction).
    if messages.is_empty() && turn_prefix_messages.is_empty() && !has_previous_summary {
        return Err(CompactSkip::TooShort);
    }
    Ok(cut)
}

/// Run compaction over the session: summarize the pre-cut prefix, persist the
/// entry, and return the rebuilt post-compaction context messages.
pub async fn execute_compaction(
    session: &mut SessionManager,
    options: CompactOptions<'_>,
) -> anyhow::Result<CompactOutcome> {
    let entries = session.get_all_entries().to_vec();
    let cut = match prepare_compaction(&entries, options.settings.keep_recent_tokens) {
        Ok(cut) => cut,
        Err(skip) => return Ok(CompactOutcome::Skipped(skip.user_message())),
    };
    let first_kept_entry = entries
        .get(cut.first_kept_entry_index)
        .and_then(|entry| entry.id())
        .unwrap_or_default()
        .to_string();

    // Messages the summarizer sees (TS prepareCompaction): everything before
    // the cut, plus the prefix of a split turn (turnPrefixMessages).
    let history_end = if cut.is_split_turn {
        cut.turn_start_index.unwrap_or(cut.first_kept_entry_index)
    } else {
        cut.first_kept_entry_index
    };
    let mut messages: Vec<AgentMessage> = entries[..history_end]
        .iter()
        .filter_map(message_from_entry)
        .collect();
    let turn_prefix_messages: Vec<AgentMessage> = entries[history_end..cut.first_kept_entry_index]
        .iter()
        .filter_map(message_from_entry)
        .collect();
    let tokens_before = context_tokens(&entries);
    let prev_compaction_index = entries[..cut.first_kept_entry_index]
        .iter()
        .rposition(|entry| matches!(entry, FileEntry::Compaction { .. }));
    messages.extend(turn_prefix_messages);
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

    // An error-stop summarizer response is a failed compaction, never an
    // empty-summary success (TS throws `Summarization failed: ...`).
    if assistant.stop_reason == pa_types::ai::StopReason::Error {
        anyhow::bail!(
            "Summarization failed: {}",
            assistant
                .error_message
                .as_deref()
                .unwrap_or("Unknown error")
        );
    }

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
    Ok(CompactOutcome::Ran(Box::new(CompactRun { result, entry })))
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

    /// The faux provider with one scripted summarizer response. The faux
    /// seam is process-global, so every registration unregisters on drop.
    fn faux_registration() -> pa_ai::faux::FauxProviderRegistration {
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
        registration
    }

    #[tokio::test]
    async fn execute_compaction_persists_and_rebuilds() {
        let registration = faux_registration();
        let model = registration.get_model();
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session_with_turns(tmp.path(), 3);
        let result = execute_compaction(
            &mut session,
            CompactOptions {
                model,
                api_key: None,
                custom_instructions: Some("focus on the goal"),
                settings: super::super::compaction::CompactionSettings {
                    keep_recent_tokens: 20,
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();
        let CompactOutcome::Ran(run) = result else {
            panic!("expected the compaction to run");
        };
        assert!(run.result.summary.contains("summarized goal"));
        assert!(run.result.usage.is_some());
        assert_eq!(run.entry.summary, run.result.summary);
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

    /// An error-stop summarizer response fails the compaction (TS throws
    /// `Summarization failed: ...`), never an empty-summary success.
    #[tokio::test]
    async fn execute_compaction_fails_on_an_error_summarizer_response() {
        let registration = faux_registration();
        let model = registration.get_model();
        registration.set_responses(vec![pa_ai::faux::FauxResponseStep::Message(
            pa_ai::faux::faux_assistant_text_message(
                "",
                pa_ai::faux::FauxAssistantMessageOptions {
                    stop_reason: Some(pa_types::ai::StopReason::Error),
                    error_message: Some("summarizer exploded".to_string()),
                    ..Default::default()
                },
            ),
        )]);
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session_with_turns(tmp.path(), 3);
        let error = execute_compaction(
            &mut session,
            CompactOptions {
                model,
                api_key: None,
                custom_instructions: None,
                settings: super::super::compaction::CompactionSettings {
                    keep_recent_tokens: 20,
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Summarization failed: summarizer exploded"
        );
        // No compaction entry persisted for the failed run.
        assert!(session
            .get_entries()
            .iter()
            .all(|entry| !matches!(entry, FileEntry::Compaction { .. })));
        registration.unregister();
    }

    #[tokio::test]
    async fn execute_compaction_skips_short_sessions() {
        let registration = faux_registration();
        let model = registration.get_model();
        let tmp = tempfile::tempdir().unwrap();
        // Three small turns fit inside the keep-recent budget: nothing to
        // summarize, so compaction skips (TS prepareCompaction).
        let mut session = session_with_turns(tmp.path(), 3);
        let outcome = execute_compaction(
            &mut session,
            CompactOptions {
                model,
                api_key: None,
                custom_instructions: None,
                settings: super::super::compaction::CompactionSettings::default(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            CompactOutcome::Skipped("Session is too short to compact — try again once it grows")
        );
        assert!(session
            .get_entries()
            .iter()
            .all(|entry| !matches!(entry, FileEntry::Compaction { .. })));
        registration.unregister();
    }

    #[tokio::test]
    async fn execute_compaction_skips_when_already_compacted() {
        let registration = faux_registration();
        let model = registration.get_model();
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session_with_turns(tmp.path(), 3);
        session.append_compaction("summary", "e1", 100);
        let outcome = execute_compaction(
            &mut session,
            CompactOptions {
                model,
                api_key: None,
                custom_instructions: None,
                settings: super::super::compaction::CompactionSettings {
                    keep_recent_tokens: 200,
                    ..Default::default()
                },
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome, CompactOutcome::Skipped("Already compacted"));
        registration.unregister();
    }
}
