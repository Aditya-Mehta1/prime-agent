//! `/compact` execution: resolve the cut over session entries, run the
//! summarizer, persist the compaction entry, and rebuild the agent context.

use pa_types::ai::{AssistantMessage, Message, UserContent};
use pa_types::session::{AgentMessage, FileEntry};

use super::compaction::{
    build_summarization_prompt, estimate_context_tokens, find_cut_point, CutPointResult,
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
    /// The run's abort signal (TS `AbortSignal` threaded through
    /// `_performCompaction` -> `compact`): checked before the summarizer
    /// request and again after it resolves, before the compaction commits —
    /// a late abort never lands a committed compaction. `None` for
    /// surfaces without an abort trigger (headless runs).
    pub abort: Option<&'a pa_agent::abort::AbortSignal>,
    /// Harness digest inputs captured from the live session (TS
    /// `_harnessDigest`): the snapshot rides the durable row as
    /// `harnessDigest`. The merged harness-state disk read happens at the
    /// commit, so state written mid-run is a fresh read. `None` for
    /// sessions without harness state (verification harnesses building
    /// the loop directly; the engine always wires one).
    pub harness_digest: Option<super::harness_digest::HarnessDigestInputs>,
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

/// The pre-compaction context estimate the compaction entry records as
/// `tokensBefore` (TS `prepareCompaction`:
/// `estimateContextTokens(buildSessionContext(pathEntries).messages)`): the
/// last non-error/aborted assistant usage — the probe-measured context of
/// the live provider — plus a chars/4 estimate of the messages that trail
/// it, or a full chars/4 estimate when no valid usage exists yet. Error
/// and aborted turns never anchor the estimate: their usage is not a real
/// measurement, and TS `getLastAssistantUsageInfo` skips them too.
fn context_tokens(entries: &[FileEntry], leaf_id: Option<&str>) -> u64 {
    let context = crate::session::build_session_context(entries, leaf_id);
    estimate_context_tokens(&context.messages).tokens
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
    let tokens_before = context_tokens(&entries, session.get_leaf_id());
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
    // A run aborted before the summarizer request never starts one (TS
    // `throwIfAborted` at the top of the provider call).
    pa_agent::abort::throw_if_aborted_signal(options.abort)?;
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

    // The summarizer resolved while the run was aborted: the compaction is
    // cancelled before it commits (TS `_performCompaction`'s
    // `if (signal.aborted) throw` between the summary and the ledger).
    if options
        .abort
        .is_some_and(pa_agent::abort::AbortSignal::is_aborted)
    {
        return Err(pa_agent::abort::aborted_error());
    }

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
    // TS `_performCompaction` passes `this._harnessDigest()` into
    // `appendCompaction`: the snapshot is attached mechanically at the
    // commit and never flows through the summarizer. The harness-state
    // read happens here, after the summarizer resolved, so harness state
    // written during the run is a fresh read.
    let harness_digest = options
        .harness_digest
        .as_ref()
        .map(super::harness_digest::HarnessDigestInputs::render);
    let entry = compaction_entry_for(
        &result,
        &details,
        options.custom_instructions,
        harness_digest,
    );
    // TS `appendCompaction` persists the full record: `details`,
    // `fromHook`, `customInstructions`, `usage`, and the `harnessDigest`
    // snapshot ride on the durable row alongside the summary, boundary,
    // and token count.
    session.append_compaction(entry.clone());
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
    let tokens = context_tokens(entries, session.get_leaf_id());
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
                abort: None,
                harness_digest: None,
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

    /// The harness digest snapshot rides the durable compaction row (TS
    /// `_performCompaction` -> `appendCompaction(..., this._harnessDigest())`):
    /// the harness-state disk read happens at the commit, so state written
    /// after the inputs were captured (mid-run, the TS test's "written
    /// before compaction" memory) is a fresh read, the digest never flows
    /// through the summarizer, and the rebuilt context leads with the
    /// digest block before the compaction summary.
    #[tokio::test]
    async fn execute_compaction_attaches_harness_digest_snapshot() {
        let registration = faux_registration();
        let model = registration.get_model();
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session_with_turns(tmp.path(), 3);
        let global_dir = tmp.path().join("agent").join("harness");
        let local_dir = tmp
            .path()
            .join("session-artifacts")
            .join("s1")
            .join("harness");
        // Inputs captured before the run (the live-session half); no
        // harness state exists yet.
        let inputs = super::super::harness_digest::HarnessDigestInputs {
            context: super::super::harness_digest::HarnessDigestContext {
                global_dir: global_dir.clone(),
                local_dir: Some(local_dir.clone()),
                include_ipython: true,
                include_shell_examples: true,
                include_refine: true,
            },
            terms: super::super::harness_digest::digest_query_terms(None, &[]),
        };
        // Harness state written after the inputs were captured — the
        // digest must still see it (fresh disk read at the commit).
        let mut state = crate::refinement::empty_harness_state();
        state
            .entries
            .get_mut(&crate::refinement::RefinementKind::Memory)
            .unwrap()
            .insert(
                "compaction_test_memory".to_string(),
                crate::refinement::HarnessEntry {
                    id: "compaction_test_memory".to_string(),
                    kind: crate::refinement::RefinementKind::Memory,
                    title: "Compaction test memory".to_string(),
                    content: "Written before compaction.".to_string(),
                    path: "general".to_string(),
                    scope: Some(crate::refinement::HarnessScope::Local),
                    reference: Default::default(),
                    arguments: Default::default(),
                    metadata: Default::default(),
                    source: "refine".to_string(),
                    created_at: "2026-09-07T00:00:00.000Z".to_string(),
                    updated_at: "2026-09-07T00:00:00.000Z".to_string(),
                    version: 1,
                },
            );
        crate::refinement::save_harness_state(&local_dir, &state).unwrap();
        let result = execute_compaction(
            &mut session,
            CompactOptions {
                model,
                api_key: None,
                custom_instructions: None,
                settings: super::super::compaction::CompactionSettings {
                    keep_recent_tokens: 20,
                    ..Default::default()
                },
                abort: None,
                harness_digest: Some(inputs),
            },
        )
        .await
        .unwrap();
        let CompactOutcome::Ran(run) = result else {
            panic!("expected the compaction to run");
        };
        let digest = run
            .entry
            .harness_digest
            .as_deref()
            .expect("digest snapshot");
        assert!(digest.contains("Compaction test memory"));
        // Mechanical attachment: the digest never flows through the summarizer.
        assert!(!run.entry.summary.contains("# Continual Harness State"));
        // The durable row carries the TS wire shape (`harnessDigest`).
        let serialized = serde_json::to_value(&run.entry).unwrap();
        assert_eq!(
            serialized
                .get("harnessDigest")
                .and_then(|value| value.as_str()),
            Some(digest)
        );
        // The rebuilt context leads with the digest block before the
        // compaction summary (TS `convertToLlm` on the compaction head).
        let rebuilt = rebuilt_context_after_compaction(&session);
        let Message::User(user) = &rebuilt[0] else {
            panic!("expected compaction head user message");
        };
        let text = user.content.text();
        let digest_at = text
            .find("[harness-digest]")
            .expect("digest block leads the compaction head");
        let summary_at = text
            .find("[compaction-summary]")
            .expect("compaction summary follows");
        assert!(digest_at < summary_at);
        assert!(text.contains("Compaction test memory"));
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
                abort: None,
                harness_digest: None,
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

    /// An already-aborted signal cancels the run before any summarizer
    /// request (TS `throwIfAborted` at the top of the provider call):
    /// the abort marker error surfaces and nothing commits.
    #[tokio::test]
    async fn execute_compaction_with_pre_aborted_signal_never_runs_the_summarizer() {
        let registration = faux_registration();
        let model = registration.get_model();
        let controller = pa_agent::abort::AbortController::new();
        controller.abort();
        let signal = controller.signal();
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
                abort: Some(&signal),
                harness_digest: None,
            },
        )
        .await
        .unwrap_err();
        assert!(pa_agent::abort::is_abort_error(&error), "{error:#}");
        assert!(session
            .get_entries()
            .iter()
            .all(|entry| !matches!(entry, FileEntry::Compaction { .. })));
        registration.unregister();
    }

    /// A signal that aborts while the summarizer is in flight cancels the
    /// run before it commits (TS `_performCompaction`'s
    /// `if (signal.aborted) throw` between the summary and the ledger):
    /// the summarizer's resolved summary never lands as a compaction
    /// entry.
    #[tokio::test]
    async fn execute_compaction_with_late_abort_cancels_before_the_commit() {
        let registration = faux_registration();
        let model = registration.get_model();
        // The delayed response holds the summarizer in flight while the
        // abort lands mid-run.
        registration.set_responses(vec![pa_ai::faux::FauxResponseStep::Delayed {
            message: pa_ai::faux::faux_assistant_text_message(
                "## Goal\nsummarized goal",
                pa_ai::faux::FauxAssistantMessageOptions::default(),
            ),
            delay_ms: 200,
        }]);
        let controller = pa_agent::abort::AbortController::new();
        let signal = controller.signal();
        let aborter = {
            let controller = controller.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                controller.abort();
            })
        };
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
                abort: Some(&signal),
                harness_digest: None,
            },
        )
        .await
        .unwrap_err();
        aborter.await.unwrap();
        assert!(pa_agent::abort::is_abort_error(&error), "{error:#}");
        assert!(
            session
                .get_entries()
                .iter()
                .all(|entry| !matches!(entry, FileEntry::Compaction { .. })),
            "the late abort never commits the compaction"
        );
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
                abort: None,
                harness_digest: None,
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
        session.append_compaction(pa_types::session::CompactionEntry {
            summary: "summary".to_string(),
            first_kept_entry_id: "e1".to_string(),
            tokens_before: 100,
            ..Default::default()
        });
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
                abort: None,
                harness_digest: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome, CompactOutcome::Skipped("Already compacted"));
        registration.unregister();
    }

    /// A usage-less error turn never anchors `tokensBefore`: the estimate
    /// uses the last settled (probe-measured) usage plus a chars/4
    /// estimate of everything that trails it — the exact TS overflow-row
    /// scenario (`getLastAssistantUsageInfo` skips error turns).
    #[test]
    fn tokens_before_anchors_on_last_valid_usage_plus_trailing() {
        let reply = |usage: pa_types::ai::Usage, error: bool| {
            // A failed request carries no content: the failure lives in
            // `errorMessage` (the TS and Rust durable error turns both
            // record an empty content list).
            let content = if error {
                Vec::new()
            } else {
                vec![pa_types::ai::AssistantContentBlock::Text(
                    pa_types::ai::TextContent {
                        text: "seed reply".to_string(),
                        text_signature: None,
                        rest: Default::default(),
                    },
                )]
            };
            AgentMessage::Assistant(AssistantMessage {
                content,
                api: "openai-completions".to_string(),
                provider: "test".to_string(),
                model: "m".to_string(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage,
                stop_reason: if error {
                    pa_types::ai::StopReason::Error
                } else {
                    pa_types::ai::StopReason::Stop
                },
                stop_reason_raw: None,
                error_message: None,
                timestamp: 0,
                rest: Default::default(),
            })
        };
        let tmp = tempfile::tempdir().unwrap();
        let mut session = SessionManager::in_memory(tmp.path());
        let settled = pa_types::ai::Usage {
            input: 20,
            output: 10,
            cache_read: 80,
            cache_write: 0,
            total_tokens: 110,
            cost: Default::default(),
        };
        let probe = |text: &str| {
            AgentMessage::User(pa_types::ai::UserMessage {
                content: UserContent::Text(text.to_string()),
                timestamp: 0,
                rest: Default::default(),
            })
        };
        session.append_message(probe("seed turn"));
        session.append_message(reply(settled, false));
        session.append_message(probe(&("overflow probe ".to_string() + &"x".repeat(400))));
        // The overflow error turn: stopReason "error" with zeroed usage
        // (what the provider returns for a failed request).
        session.append_message(reply(Default::default(), true));
        // TS: 110 (last valid usage) + ceil(415/4) (the probe turn) = 214.
        assert_eq!(
            context_tokens(session.get_all_entries(), session.get_leaf_id()),
            214
        );
    }

    /// The durable row carries the full TS `CompactionEntry` record:
    /// `fromHook: false` (the built-in origin), the summarizer usage, and
    /// the file-operation details — not just the summary boundary.
    #[tokio::test]
    async fn durable_compaction_row_carries_the_ts_record() {
        let registration = faux_registration();
        let model = registration.get_model();
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session_with_turns(tmp.path(), 3);
        let before = context_tokens(session.get_all_entries(), session.get_leaf_id());
        let outcome = execute_compaction(
            &mut session,
            CompactOptions {
                model,
                api_key: None,
                custom_instructions: None,
                settings: super::super::compaction::CompactionSettings {
                    keep_recent_tokens: 20,
                    ..Default::default()
                },
                abort: None,
                harness_digest: None,
            },
        )
        .await
        .unwrap();
        let CompactOutcome::Ran(run) = outcome else {
            panic!("expected the compaction to run");
        };
        assert_eq!(run.entry.from_hook, Some(false));
        assert_eq!(run.entry.usage, run.result.usage);
        assert_eq!(run.entry.tokens_before, before);
        // The persisted session record is the full entry, byte-for-byte
        // (TS `appendCompaction` stores the same record it returns).
        let persisted = session
            .get_entries()
            .iter()
            .rev()
            .find_map(|entry| match entry {
                FileEntry::Compaction { payload, .. } => Some(payload.clone()),
                _ => None,
            })
            .expect("compaction entry persisted");
        assert_eq!(persisted, run.entry);
    }
}
