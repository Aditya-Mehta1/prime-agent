//! AgentSession: the turn admission layer over the pa-agent loop.
//! First slice of core/agent-session.ts: prompt normalization (templates),
//! busy-admission rules (steer/follow-up), and SessionManager persistence.
//!
//! Design note: the TS class runs an internal action-store with admission
//! epochs/tickets. The Rust port keeps the observable contract instead: the
//! pa-agent Agent owns the loop and its steer/follow-up queues; this layer
//! decides admission and persists what the loop produces.

pub mod agent_messaging;
pub mod auto_retry;
pub mod branch_summarization;
pub mod compact_session;
pub mod compaction;
pub mod compaction_exec;
pub mod compaction_utils;
pub mod engine;
pub mod goal_driver;
pub mod harness_digest;
pub mod headless;
pub mod host_requests;
pub mod messages;
pub mod provider_adapter;
pub mod provider_failover;
pub mod provider_retry;
pub mod refine;
pub mod rlm_host;
pub mod rlm_notices;
pub mod runtime;
pub mod runtime_wiring;
pub mod session_commands;
pub mod side_question;
pub mod slash_commands;
pub mod telemetry;
pub mod tool_bridge;
pub mod turn_boundary;

use std::sync::Arc;

use pa_agent::agent::Agent;
use pa_agent::types::{AgentEvent, AgentMessage, ThinkingLevel};
use pa_types::session::AgentMessage as SessionAgentMessage;
use pa_types::session::FileEntry;

use crate::session::manager::SessionManager;
use crate::session_engine::compact_session::CompactOutcome;
use crate::skills::PromptTemplate;
use slash_commands::{parse_session_command, SessionSlashCommand, SlashCommandRegistry};

/// How a prompt submitted while the agent streams is scheduled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamingBehavior {
    /// Interrupt the current turn and inject the message (queue mode "steer").
    Steer,
    /// Queue the message for after the current turn (queue mode "followUp").
    FollowUp,
}

/// What `prompt` did with the input.
#[derive(Debug, PartialEq)]
pub enum PromptOutcome {
    /// Input admitted to the model loop.
    Prompt,
    /// Input recognized as a session command (compact/refine/goal/autonomous).
    /// Execution is the session engine's job; the caller observes it here.
    SessionCommand(SessionSlashCommand),
}

/// Options for `AgentSession::prompt`. Port of PromptOptions (used fields).
#[derive(Debug, Default)]
pub struct PromptOptions {
    pub streaming_behavior: Option<StreamingBehavior>,
    pub expand_prompt_templates: Option<bool>,
    /// Queue instead of erroring when the session is busy (agent messages).
    pub queue_if_busy: bool,
}

/// Which trailing assistant messages [`AgentSession::drop_trailing_assistant`]
/// removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrailingAssistantFilter {
    /// Any trailing assistant message (the TS overflow arm's pre-compaction
    /// drop).
    Any,
    /// Only an error assistant message (the TS will-retry branch's drop
    /// after the compaction rebuild).
    ErrorOnly,
}

/// The standard message inside an agent message, when it is one.
fn standard_message(message: &pa_agent::types::AgentMessage) -> Option<&pa_agent::types::Message> {
    let pa_agent::types::AgentMessage::Standard(message) = message else {
        return None;
    };
    Some(message)
}

/// The session-bound agent: admission rules + persistence over the loop.
pub struct AgentSession {
    agent: Arc<Agent>,
    session: Arc<tokio::sync::Mutex<SessionManager>>,
    prompt_templates: Vec<PromptTemplate>,
    slash_commands: SlashCommandRegistry,
    /// Harness digest inputs; `None` in sessions without harness state
    /// (verification harnesses building the loop directly).
    harness_digest: Option<harness_digest::HarnessDigestContext>,
    /// The first-turn digest rides the turn's admission (fresh sessions defer
    /// delivery so untouched sessions stay empty, TS `_harnessDigestPending`).
    digest_pending: std::sync::atomic::AtomicBool,
    /// Compaction settings from the session's settings.json (TS
    /// `_performCompaction` reads `getCompactionSettings()` on every
    /// compaction path, `/compact` included); defaults until the engine
    /// wiring resolves them.
    compaction: compaction::CompactionSettings,
}

impl AgentSession {
    /// Build a session around a running agent loop.
    pub async fn new(
        agent: Arc<Agent>,
        session: SessionManager,
        prompt_templates: Vec<PromptTemplate>,
    ) -> anyhow::Result<Self> {
        Self::from_session_arc(
            agent,
            Arc::new(tokio::sync::Mutex::new(session)),
            prompt_templates,
            None,
        )
        .await
    }

    /// Build a session from an already-shared session manager handle, so the
    /// kernel host-request handlers can reach the same persistence.
    #[allow(clippy::too_many_arguments)]
    pub async fn from_session_arc(
        agent: Arc<Agent>,
        session: Arc<tokio::sync::Mutex<SessionManager>>,
        prompt_templates: Vec<PromptTemplate>,
        harness_digest: Option<harness_digest::HarnessDigestContext>,
    ) -> anyhow::Result<Self> {
        let persistence = session.clone();
        agent
            .subscribe(move |event, _signal| {
                let persistence = persistence.clone();
                Box::pin(async move {
                    persist_event(&persistence, event).await;
                    Ok(())
                })
            })
            .await;
        let this = Self {
            agent,
            session,
            prompt_templates,
            slash_commands: SlashCommandRegistry::builtin(),
            harness_digest,
            digest_pending: std::sync::atomic::AtomicBool::new(false),
            compaction: compaction::CompactionSettings::default(),
        };
        this.ensure_harness_digest_context().await?;
        Ok(this)
    }

    /// Override the compaction settings from the session's resolved
    /// settings (TS `getCompactionSettings`); the engine wiring calls this
    /// so `/compact` honors `compaction.keepRecentTokens`/`reserveTokens`
    /// like the TS product instead of the defaults.
    pub fn set_compaction_settings(&mut self, settings: compaction::CompactionSettings) {
        self.compaction = settings;
    }

    /// Whether automatic compaction is enabled for this session (the TS
    /// `getCompactionSettings().enabled` gate the automatic arms check
    /// before any trigger).
    pub fn auto_compaction_enabled(&self) -> bool {
        self.compaction.enabled
    }

    /// The latest compaction boundary in the live loop context, if any
    /// (the TS `getLatestCompactionEntry` guard source): the timestamp of
    /// the newest compaction summary in the agent state.
    pub async fn latest_compaction_timestamp(&self) -> Option<u64> {
        let state = self.agent.state().await;
        state
            .messages
            .iter()
            .filter_map(|message| serde_json::to_value(message).ok())
            .filter_map(|value| serde_json::from_value::<SessionAgentMessage>(value).ok())
            .filter_map(|message| match message {
                SessionAgentMessage::CompactionSummary(summary) => Some(summary.timestamp),
                _ => None,
            })
            .max()
    }

    /// Whether an automatic threshold compaction is due at a turn boundary
    /// (the TS `_checkCompaction` threshold arm, fired at `agent_end` and
    /// before the next admitted prompt): the live loop context over the
    /// model's context window and the compaction reserve headroom. Usage
    /// from before the latest compaction never re-triggers.
    pub async fn auto_compaction_due(&self, context_window: u64) -> bool {
        let state = self.agent.state().await;
        // The live loop context is the agent's message list (the same JSON
        // round-trip `compact` uses for its rebuilt context).
        let messages: Vec<SessionAgentMessage> = state
            .messages
            .iter()
            .filter_map(|message| serde_json::to_value(message).ok())
            .filter_map(|value| serde_json::from_value(value).ok())
            .collect();
        compaction::threshold_compaction_due(&messages, context_window, &self.compaction)
    }

    /// Remove the trailing assistant message from the loop context (TS retry:
    /// `messages.slice(0, -1)`), so a re-issued request does not re-send the
    /// failed turn's error message. The session history keeps it (it already
    /// persisted through the message-end hook).
    ///
    /// [`TrailingAssistantFilter::ErrorOnly`] matches the TS
    /// compact-and-retry will-retry branch: only an error assistant message
    /// drops (a compaction rebuild may leave any other trailing assistant
    /// in place).
    pub async fn drop_trailing_assistant(&self, filter: TrailingAssistantFilter) {
        let state = self.agent.state().await;
        let mut messages = state.messages;
        let matches_filter = |message: &pa_agent::types::AgentMessage| {
            let Some(pa_agent::types::Message::Assistant(assistant)) = standard_message(message)
            else {
                return false;
            };
            match filter {
                TrailingAssistantFilter::Any => true,
                TrailingAssistantFilter::ErrorOnly => {
                    assistant.stop_reason == pa_agent::types::StopReason::Error
                }
            }
        };
        if messages.last().is_some_and(&matches_filter) {
            messages.pop();
            self.agent.set_messages(messages).await;
        }
    }

    /// The last assistant message in the live loop context (TS
    /// `_findLastAssistantMessage`), in the session wire shape: trailing
    /// non-assistant rows (a compaction outcome disclosure, a compaction
    /// summary) are skipped, not matched.
    pub async fn last_assistant_message(&self) -> Option<SessionAgentMessage> {
        let state = self.agent.state().await;
        state.messages.iter().rev().find_map(|message| {
            let value = serde_json::to_value(message).ok()?;
            let message: SessionAgentMessage = serde_json::from_value(value).ok()?;
            matches!(message, SessionAgentMessage::Assistant(_)).then_some(message)
        })
    }

    /// Execute `/compact`: summarize the pre-cut prefix, persist the
    /// compaction entry, and rebuild the loop context summary-first. A skip
    /// (already compacted, or nothing to summarize) leaves the session
    /// untouched, matching the TS `CompactionSkippedError` flow. `abort`
    /// is the run's abort signal (TS `_performCompaction`'s `signal`):
    /// an aborted run returns the abort error and never commits.
    pub async fn compact(
        &self,
        custom_instructions: Option<&str>,
        model: &pa_types::ai::Model,
        api_key: Option<String>,
        abort: Option<&pa_agent::abort::AbortSignal>,
    ) -> anyhow::Result<CompactOutcome> {
        // TS `_performCompaction` captures `this._harnessDigest()` at the
        // commit: relevance terms from the live (pre-compaction) context,
        // harness state read fresh from disk when the snapshot renders.
        let digest_inputs = self.harness_digest_inputs().await;
        let outcome = {
            let mut session = self.session.lock().await;
            crate::session_engine::compact_session::execute_compaction(
                &mut session,
                crate::session_engine::compact_session::CompactOptions {
                    model: model.clone(),
                    api_key,
                    custom_instructions,
                    settings: self.compaction,
                    abort,
                    harness_digest: digest_inputs,
                },
            )
            .await?
        };
        if matches!(outcome, CompactOutcome::Skipped(_)) {
            return Ok(outcome);
        }
        // Rebuild the loop context from the post-compaction session.
        let rebuilt = {
            let session = self.session.lock().await;
            crate::session_engine::compact_session::rebuilt_context_after_compaction(&session)
        };
        let loop_messages: Vec<AgentMessage> = rebuilt
            .into_iter()
            .filter_map(|message| {
                let value = serde_json::to_value(&message).ok()?;
                serde_json::from_value::<AgentMessage>(value).ok()
            })
            .collect();
        self.agent.set_messages(loop_messages).await;
        Ok(outcome)
    }

    /// Record an unsuccessful compaction outcome (TS
    /// `_persistCompactionOutcome`): append the durable `compaction_outcome`
    /// row to the session entries and push it onto the live loop context,
    /// returning it for the caller to broadcast as a `message_start` /
    /// `message_end` pair. The row is a user-facing disclosure, never model
    /// context: `convert_to_llm` drops it, so the KV-cacheable prefix is
    /// unaffected (the TS contract — `agent-session-compaction.test.ts`
    /// asserts the outcome "stays out of model context"). The append is
    /// retained in the in-memory entry chain even when the disk write
    /// fails, so every in-process context rebuild (compaction, tree
    /// navigation) keeps the disclosure — the TS `_unpersistedOutcomes`
    /// guarantee, held structurally.
    pub async fn record_compaction_outcome(
        &self,
        reason: crate::session_engine::messages::CompactionOutcomeReason,
        outcome: crate::session_engine::messages::CompactionOutcomeKind,
        content: &str,
    ) -> pa_types::session::CustomMessage {
        let row = crate::session_engine::messages::create_compaction_outcome_message(
            content, reason, outcome,
        );
        {
            let mut session = self.session.lock().await;
            session.append_custom_message(
                &row.custom_type,
                row.content.clone(),
                row.display,
                row.details.clone(),
            );
        }
        // TS pushes the row onto `agent.state.messages` after the append:
        // the live context owns the disclosure; the loop's converter filters
        // custom rows out of the provider request.
        if let Some(loop_message) =
            session_message_to_loop(&SessionAgentMessage::Custom(row.clone()))
        {
            let state = self.agent.state().await;
            let mut messages = state.messages;
            messages.push(loop_message);
            self.agent.set_messages(messages).await;
        }
        row
    }

    /// Rebuild the live loop context from a durable branch (TS
    /// `navigateTree`'s context rebuild: `sessionManager.branch(newLeafId)`
    /// then `agent.state.messages = buildSessionContext().messages`). The
    /// session adopts the branch entries and the agent's message list is
    /// rebuilt from the post-navigation session state.
    pub async fn rebuild_branch_context(
        &self,
        branch_entries: Vec<FileEntry>,
    ) -> anyhow::Result<()> {
        let rebuilt = {
            let mut session = self.session.lock().await;
            session.adopt_entries(branch_entries);
            crate::session_engine::compact_session::rebuilt_context_after_compaction(&session)
        };
        let loop_messages: Vec<AgentMessage> = rebuilt
            .into_iter()
            .filter_map(|message| {
                let value = serde_json::to_value(&message).ok()?;
                serde_json::from_value::<AgentMessage>(value).ok()
            })
            .collect();
        self.agent.set_messages(loop_messages).await;
        Ok(())
    }

    /// Execute `/refine`: plan, re-read, apply, and persist the continual
    /// harness state for this session. The conversation snapshot comes from
    /// the session entries (what the model would see on a rebuild).
    pub async fn refine(
        &self,
        options: &refine::RefineOptions,
        source: refine::RefinementSource,
        model: &pa_types::ai::Model,
        api_key: Option<String>,
        global_harness_dir: std::path::PathBuf,
    ) -> anyhow::Result<crate::refinement::RefinementResult> {
        let result = {
            let mut session = self.session.lock().await;
            let messages: Vec<SessionAgentMessage> = session
                .get_all_entries()
                .iter()
                .filter_map(|entry| match entry {
                    FileEntry::Message { message, .. } => Some(message.clone()),
                    _ => None,
                })
                .collect();
            refine::execute_refinement(
                &mut session,
                &messages,
                &global_harness_dir,
                model,
                options,
                source,
                refine::default_refiner_call(api_key),
            )
            .await?
        };
        // The notice entry must also enter the live loop context.
        let session = self.session.lock().await;
        let loop_messages: Vec<AgentMessage> =
            crate::session_engine::compact_session::rebuilt_context_after_compaction(&session)
                .into_iter()
                .filter_map(|message| {
                    let value = serde_json::to_value(&message).ok()?;
                    serde_json::from_value::<AgentMessage>(value).ok()
                })
                .collect();
        drop(session);
        self.agent.set_messages(loop_messages).await;
        Ok(result)
    }

    /// The underlying agent loop (steering, state, subscriptions).
    pub fn agent(&self) -> &Arc<Agent> {
        &self.agent
    }

    /// The shared persistence handle: the kernel host handlers and the
    /// session-command executor reach the same session state as the loop.
    pub(crate) fn session_handle(&self) -> &Arc<tokio::sync::Mutex<SessionManager>> {
        &self.session
    }

    /// The shared persistence handle for host runtimes in other crates (the
    /// daemon's ACP transport records goal usage into the same session
    /// state as the loop).
    pub fn shared_persistence(&self) -> Arc<tokio::sync::Mutex<SessionManager>> {
        self.session.clone()
    }

    /// Submit a prompt. Session commands (compact/refine/goal/autonomous)
    /// are recognized before admission and never reach the model.
    pub async fn prompt(
        &self,
        text: &str,
        options: PromptOptions,
    ) -> anyhow::Result<PromptOutcome> {
        self.prompt_with_images(text, Vec::new(), options).await
    }

    /// Prompt with images attached (the ACP prompt-capability path). Busy
    /// sessions queue the text and images together as one follow-up batch,
    /// so an admitted prompt never loses its images to a queue race.
    pub async fn prompt_with_images(
        &self,
        text: &str,
        images: Vec<pa_agent::types::ImageContent>,
        options: PromptOptions,
    ) -> anyhow::Result<PromptOutcome> {
        let expand = options.expand_prompt_templates.unwrap_or(true);
        let normalized = if expand {
            crate::skills::expand_prompt_template(text, &self.prompt_templates)
        } else {
            text.to_string()
        };

        if let Some(command) = parse_session_command(&self.slash_commands, &normalized) {
            return Ok(PromptOutcome::SessionCommand(command));
        }

        let state = self.agent.state().await;
        let busy = state.is_streaming;
        if busy && options.streaming_behavior.is_none() {
            anyhow::bail!(
                "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message."
            );
        }
        // The deferred first-turn harness digest rides this admission, so the
        // model sees it before the prompt (TS commit-time injection).
        self.deliver_pending_harness_digest().await?;
        // User messages persist through the loop's `message_end` event (the
        // persistence subscription in `from_session_arc`), matching the TS
        // reference: `_processAgentEvent` is the only appendMessage path for
        // user prompts. Appending here as well would double-persist.

        if busy {
            // Identical shape to the loop's own prompt normalization
            // (text part first, image parts after), so the queued message
            // matches a directly admitted one token for token.
            let mut parts = vec![pa_agent::types::UserPart::Text(
                pa_agent::types::TextContent {
                    text: normalized,
                    text_signature: None,
                },
            )];
            for image in images {
                parts.push(pa_agent::types::UserPart::Image(image));
            }
            let message = AgentMessage::Standard(pa_agent::types::Message::User(
                pa_agent::types::UserMessage {
                    content: pa_agent::types::UserContent::Parts(parts),
                    timestamp: now_millis() as i64,
                },
            ));
            match options.streaming_behavior {
                Some(StreamingBehavior::Steer) => self.agent.steer(message),
                Some(StreamingBehavior::FollowUp) => self.agent.follow_up(message),
                None => unreachable!("busy without a streaming behavior errors above"),
            }
        } else {
            self.agent
                .prompt(pa_agent::agent::AgentPromptInput::Text {
                    text: normalized,
                    images,
                })
                .await?;
        }
        Ok(PromptOutcome::Prompt)
    }

    /// Session id (persistence identity).
    pub async fn session_id(&self) -> String {
        self.session.lock().await.get_session_id().to_string()
    }

    /// Persisted entries (for UI resume and inspection).
    pub async fn entries(&self) -> Vec<FileEntry> {
        self.session.lock().await.get_entries().to_vec()
    }

    /// Model change bookkeeping (mirrors appendModelChange). The resolved
    /// model is forwarded to the loop; pa-agent and pa-types serialize to the
    /// same camelCase wire shape, so the boundary converts through JSON.
    pub async fn set_model(
        &self,
        model: &pa_types::ai::Model,
        provider: &str,
        model_id: &str,
    ) -> anyhow::Result<()> {
        let wire: pa_agent::types::Model = serde_json::from_value(
            serde_json::to_value(model).map_err(|error| anyhow::anyhow!(error.to_string()))?,
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        self.agent.set_model(wire).await;
        let mut session = self.session.lock().await;
        session.append_model_change(provider, model_id);
        Ok(())
    }

    /// Thinking level bookkeeping (mirrors appendThinkingLevelChange).
    pub async fn set_thinking_level(&self, level: ThinkingLevel) -> anyhow::Result<()> {
        self.agent.set_thinking_level(level).await;
        let mut session = self.session.lock().await;
        session.append_thinking_level_change(&format!("{level:?}").to_lowercase());
        Ok(())
    }
}

async fn persist_event(session: &Arc<tokio::sync::Mutex<SessionManager>>, event: AgentEvent) {
    match event {
        AgentEvent::MessageEnd { message, .. } => {
            let Some(session_message) = loop_message_to_session(&message) else {
                return;
            };
            let mut session = session.lock().await;
            session.append_message(session_message);
        }
        // Git state is captured at both run boundaries, exactly like the TS
        // extension-event path: a commit or branch switch made during the run
        // (e.g. via the bash tool) lands in the session file at `agent_end`.
        // The persist check lives inside `record_git_state_if_changed`.
        AgentEvent::AgentStart | AgentEvent::AgentEnd { .. } => {
            let mut session = session.lock().await;
            session.record_git_state_if_changed();
        }
        _ => {}
    }
}

/// Convert a loop message to its persisted form via the shared wire shape.
fn loop_message_to_session(message: &AgentMessage) -> Option<SessionAgentMessage> {
    let AgentMessage::Standard(inner) = message else {
        return None;
    };
    serde_json::from_value(serde_json::to_value(inner).ok()?).ok()
}

/// Convert a session message to its loop form via the shared wire shape
/// (custom rows ride the loop's custom variant; its converter filters them
/// out of the provider request).
fn session_message_to_loop(message: &SessionAgentMessage) -> Option<AgentMessage> {
    serde_json::from_value(serde_json::to_value(message).ok()?).ok()
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_agent::agent::{AgentInitialState, AgentOptions};
    use pa_agent::scripted::ScriptedProvider;

    fn test_model() -> pa_agent::types::Model {
        serde_json::from_value(serde_json::json!({
            "id": "m", "name": "m", "api": "openai-completions", "provider": "test",
            "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap()
    }

    async fn scripted_session() -> AgentSession {
        let provider = Arc::new(ScriptedProvider::new(test_model()));
        provider.push_text_turn("hello from the model");
        let options = AgentOptions {
            initial_state: AgentInitialState {
                model: Some(test_model()),
                ..Default::default()
            },
            stream_fn: Some(provider.stream_fn()),
            ..Default::default()
        };
        let agent = Agent::new(options);
        let tmp = tempfile::tempdir().unwrap();
        let session = SessionManager::in_memory(tmp.path());
        AgentSession::new(Arc::new(agent), session, vec![])
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn prompt_persists_user_and_assistant() {
        let session = scripted_session().await;
        session
            .prompt("hi there", PromptOptions::default())
            .await
            .unwrap();
        session.agent().wait_for_idle().await;
        let entries = session.entries().await;
        let roles: Vec<String> = entries
            .iter()
            .filter_map(|entry| match entry {
                FileEntry::Message {
                    message: SessionAgentMessage::User(user),
                    ..
                } => Some(format!("user:{}", user.content.text())),
                FileEntry::Message {
                    message: SessionAgentMessage::Assistant(assistant),
                    ..
                } => Some(format!("assistant:{}", assistant.model)),
                _ => None,
            })
            .collect();
        assert_eq!(
            roles,
            vec!["user:hi there".to_string(), "assistant:m".to_string()]
        );
    }

    #[tokio::test]
    async fn prompt_persists_tool_results() {
        struct EchoTool;
        impl pa_agent::types::AgentTool for EchoTool {
            fn name(&self) -> &str {
                "echo"
            }
            fn description(&self) -> &str {
                "echo the call"
            }
            fn parameters(&self) -> &serde_json::Value {
                static PARAMETERS: std::sync::OnceLock<serde_json::Value> =
                    std::sync::OnceLock::new();
                PARAMETERS.get_or_init(|| serde_json::json!({ "type": "object" }))
            }
            fn execute(
                self: Arc<Self>,
                _tool_call_id: String,
                _params: serde_json::Value,
                _signal: pa_agent::abort::AbortSignal,
                _on_update: pa_agent::types::AgentToolUpdateCallback,
            ) -> pa_agent::BoxFut<'static, anyhow::Result<pa_agent::types::AgentToolResult>>
            {
                Box::pin(async { Ok(pa_agent::types::AgentToolResult::text("tool output")) })
            }
        }
        let provider = Arc::new(ScriptedProvider::new(test_model()));
        provider.push_tool_call_turn(
            Some("calling"),
            vec![("call-1", "echo", serde_json::json!({}))],
        );
        provider.push_text_turn("done");
        let options = AgentOptions {
            initial_state: AgentInitialState {
                model: Some(test_model()),
                ..Default::default()
            },
            stream_fn: Some(provider.stream_fn()),
            ..Default::default()
        };
        let agent = Agent::new(options);
        agent.set_tools(vec![Arc::new(EchoTool)]).await;
        let tmp = tempfile::tempdir().unwrap();
        let session = SessionManager::persisted(std::path::Path::new("/w"), tmp.path());
        let engine = AgentSession::new(Arc::new(agent), session, vec![])
            .await
            .unwrap();
        engine.prompt("hi", PromptOptions::default()).await.unwrap();
        engine.agent().wait_for_idle().await;
        let entries = engine.entries().await;
        let roles: Vec<&str> = entries
            .iter()
            .filter_map(|entry| match entry {
                FileEntry::Message { message, .. } => Some(match message {
                    SessionAgentMessage::User(_) => "user",
                    SessionAgentMessage::Assistant(_) => "assistant",
                    SessionAgentMessage::ToolResult(_) => "toolResult",
                    _ => "other",
                }),
                _ => None,
            })
            .collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "toolResult", "assistant"],
            "entries: {entries:?}"
        );
        let tool_result = entries
            .iter()
            .find_map(|entry| match entry {
                FileEntry::Message {
                    message: SessionAgentMessage::ToolResult(result),
                    ..
                } => Some(result.clone()),
                _ => None,
            })
            .expect("toolResult entry persisted");
        // Whole-object compare through the TS wire shape (timestamp is
        // turn-dependent and asserted only by type).
        let value = serde_json::to_value(SessionAgentMessage::ToolResult(tool_result)).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "role": "toolResult",
                "toolCallId": "call-1",
                "toolName": "echo",
                "content": [{ "type": "text", "text": "tool output" }],
                "isError": false,
                "timestamp": value["timestamp"],
            })
        );
        // The persisted file line carries the live-TS entry envelope: the
        // message under `message`, chained to its assistant parent.
        let file = tmp
            .path()
            .join(format!("{}.jsonl", engine.session_id().await))
            .to_string_lossy()
            .to_string();
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(file)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let entry = lines
            .iter()
            .find(|entry| {
                entry.get("message").and_then(|m| m.get("role"))
                    == Some(&serde_json::json!("toolResult"))
            })
            .expect("toolResult entry on disk");
        assert_eq!(entry["type"], "message");
        assert_eq!(entry["message"]["toolCallId"], "call-1");
        assert_eq!(entry["message"]["content"][0]["text"], "tool output");
        assert_eq!(entry["message"]["isError"], false);
        assert!(entry["id"].as_str().is_some_and(|id| id.len() == 8));
        assert!(entry["parentId"].as_str().is_some());
        assert!(entry["timestamp"].as_str().is_some());
    }

    /// A `toolResult` entry captured from a live TS session (read-only, from
    /// the installed product's own session store) parses into the Rust
    /// session types and re-serializes to the identical wire shape.
    #[test]
    fn ts_toolresult_entry_round_trips() {
        let golden: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/golden/corpus/toolresult-entry-live-ts.json"
        ))
        .unwrap();
        let entry: FileEntry = serde_json::from_value(golden.clone()).unwrap();
        let FileEntry::Message {
            message: SessionAgentMessage::ToolResult(tool_result),
            base,
        } = &entry
        else {
            panic!("golden entry is not a toolResult message: {entry:?}");
        };
        assert_eq!(tool_result.tool_name, "ipython");
        assert_eq!(
            tool_result.tool_call_id,
            "c2425715-419e-4d06-a101-a78da1969b96"
        );
        assert!(!tool_result.is_error);
        assert_eq!(base.id.clone().unwrap_or_default().len(), 8);
        assert_eq!(base.parent_id.as_deref(), Some("8902561b"));
        // The ipython `details` block survives the round trip intact.
        assert_eq!(
            tool_result.details,
            Some(serde_json::json!({
                "durationMs": 10,
                "status": "ok",
                "stdout": "/root/prime-agent-rs\n['.git', 'MISSION.md', 'README.md', 'WATCHDOG.md']\nTrue\n",
                "stderr": "",
                "kernelRestarted": false
            }))
        );
        // Re-serialization is byte-identical (stable wire shape).
        let serialized = serde_json::to_value(&entry).unwrap();
        assert_eq!(serialized, golden);
    }

    #[tokio::test]
    async fn template_expansion_applies() {
        let provider = Arc::new(ScriptedProvider::new(test_model()));
        provider.push_text_turn("ok");
        let options = AgentOptions {
            initial_state: AgentInitialState {
                model: Some(test_model()),
                ..Default::default()
            },
            stream_fn: Some(provider.stream_fn()),
            ..Default::default()
        };
        let agent = Agent::new(options);
        let tmp = tempfile::tempdir().unwrap();
        let session = SessionManager::in_memory(tmp.path());
        let template = PromptTemplate {
            name: "fix".to_string(),
            description: "fix".to_string(),
            argument_hint: None,
            content: "Fix $1 please".to_string(),
            source_info: crate::skills::create_synthetic_source_info(
                "/p",
                "local",
                crate::skills::SourceScope::User,
                None,
            ),
            file_path: "/p/fix.md".to_string(),
        };
        let engine = AgentSession::new(Arc::new(agent), session, vec![template])
            .await
            .unwrap();
        engine
            .prompt("/fix lint", PromptOptions::default())
            .await
            .unwrap();
        engine.agent().wait_for_idle().await;
        let entries = engine.entries().await;
        let user_text = entries
            .iter()
            .find_map(|entry| match entry {
                FileEntry::Message {
                    message: SessionAgentMessage::User(user),
                    ..
                } => Some(user.content.text()),
                _ => None,
            })
            .unwrap();
        assert_eq!(user_text, "Fix lint please");
    }

    /// Git state is captured at both run boundaries (TS `_emitExtensionEvent`
    /// calls `recordGitStateIfChanged` on `agent_start`/`agent_end`): a commit
    /// made between session creation and the run lands as a `git_state`
    /// entry, and an unchanged context at `agent_end` adds nothing.
    #[tokio::test]
    async fn run_boundaries_record_git_state() {
        fn git(cwd: &std::path::Path, args: &[&str]) {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("git is available in the test environment");
            assert!(output.status.success(), "git {args:?} failed");
        }
        fn commit(dir: &std::path::Path, message: &str) -> String {
            std::fs::write(dir.join("file.txt"), format!("{message}\n")).unwrap();
            git(dir, &["add", "-A"]);
            git(dir, &["commit", "-q", "-m", message]);
            let output = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(dir)
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        let repo = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "t@t.co"]);
        git(repo.path(), &["config", "user.name", "t"]);
        commit(repo.path(), "init");

        let provider = Arc::new(ScriptedProvider::new(test_model()));
        provider.push_text_turn("ok");
        let options = AgentOptions {
            initial_state: AgentInitialState {
                model: Some(test_model()),
                ..Default::default()
            },
            stream_fn: Some(provider.stream_fn()),
            ..Default::default()
        };
        let agent = Agent::new(options);
        let session = SessionManager::persisted(repo.path(), sessions.path());
        let engine = AgentSession::new(Arc::new(agent), session, vec![])
            .await
            .unwrap();

        // The run starts on a newer commit than the header captured.
        let second_sha = commit(repo.path(), "second");
        engine.prompt("hi", PromptOptions::default()).await.unwrap();
        engine.agent().wait_for_idle().await;

        let entries = engine.entries().await;
        let git_states: Vec<_> = entries
            .iter()
            .filter_map(|entry| match entry {
                FileEntry::GitState { payload, .. } => Some(payload.git.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(git_states.len(), 1, "one git_state per changed context");
        assert_eq!(git_states[0].commit.as_deref(), Some(second_sha.as_str()));
        assert_eq!(git_states[0].branch.as_deref(), Some("main"));
    }
}

#[cfg(test)]
mod slash_session_tests {
    use super::*;
    use pa_agent::agent::{AgentInitialState, AgentOptions};
    use pa_agent::scripted::ScriptedProvider;

    fn test_model() -> pa_agent::types::Model {
        serde_json::from_value(serde_json::json!({
            "id": "m", "name": "m", "api": "openai-completions", "provider": "test",
            "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn session_commands_never_reach_the_model() {
        let provider = Arc::new(ScriptedProvider::new(test_model()));
        provider.push_text_turn("unused");
        let options = AgentOptions {
            initial_state: AgentInitialState {
                model: Some(test_model()),
                ..Default::default()
            },
            stream_fn: Some(provider.stream_fn()),
            ..Default::default()
        };
        let agent = Agent::new(options);
        let tmp = tempfile::tempdir().unwrap();
        let session = SessionManager::in_memory(tmp.path());
        let engine = AgentSession::new(Arc::new(agent), session, vec![])
            .await
            .unwrap();
        let outcome = engine
            .prompt("/compact focus on tests", PromptOptions::default())
            .await
            .unwrap();
        match &outcome {
            PromptOutcome::SessionCommand(command) => {
                assert_eq!(command.name, "compact");
                assert_eq!(command.args, "focus on tests");
            }
            _ => panic!("expected a session command"),
        }
        // No model call and no persisted user message.
        assert!(provider.calls().is_empty());
        assert!(engine.entries().await.is_empty());
    }
}

#[cfg(test)]
mod compaction_outcome_tests {
    use super::*;
    use crate::session_engine::messages::{
        convert_to_llm, create_compaction_outcome_message, CompactionOutcomeKind,
        CompactionOutcomeReason,
    };
    use pa_agent::agent::{AgentInitialState, AgentOptions};
    use pa_agent::scripted::ScriptedProvider;
    use pa_types::ai::AssistantMessage;

    fn test_model() -> pa_agent::types::Model {
        serde_json::from_value(serde_json::json!({
            "id": "m", "name": "m", "api": "openai-completions", "provider": "test",
            "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap()
    }

    async fn scripted_session_over(session: SessionManager) -> AgentSession {
        let provider = Arc::new(ScriptedProvider::new(test_model()));
        let options = AgentOptions {
            initial_state: AgentInitialState {
                model: Some(test_model()),
                ..Default::default()
            },
            stream_fn: Some(provider.stream_fn()),
            ..Default::default()
        };
        let agent = Agent::new(options);
        AgentSession::new(Arc::new(agent), session, vec![])
            .await
            .unwrap()
    }

    fn seeded_assistant() -> SessionAgentMessage {
        SessionAgentMessage::Assistant(AssistantMessage {
            content: vec![],
            api: "openai-completions".to_string(),
            provider: "test".to_string(),
            model: "m".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Default::default(),
            stop_reason: pa_types::ai::StopReason::Stop,
            stop_reason_raw: None,
            error_message: None,
            timestamp: 0,
            rest: Default::default(),
        })
    }

    /// The disclosure row's shape (TS `createCompactionOutcomeMessage`):
    /// customType `compaction_outcome`, the outcome message as text content,
    /// displayed, `{reason, outcome}` details.
    #[test]
    fn outcome_row_shape_matches_ts() {
        let row = create_compaction_outcome_message(
            "Auto-compaction skipped: Session is too short to compact — try again once it grows",
            CompactionOutcomeReason::Threshold,
            CompactionOutcomeKind::Skipped,
        );
        assert_eq!(row.custom_type, "compaction_outcome");
        assert_eq!(
            row.content.text(),
            "Auto-compaction skipped: Session is too short to compact — try again once it grows"
        );
        assert!(row.display);
        assert_eq!(
            row.details,
            Some(serde_json::json!({ "reason": "threshold", "outcome": "skipped" }))
        );
        assert!(row.timestamp > 0);
        let wire = serde_json::to_value(SessionAgentMessage::Custom(row)).unwrap();
        assert_eq!(wire["role"], "custom");
        assert_eq!(wire["customType"], "compaction_outcome");
    }

    /// The seam (TS `_persistCompactionOutcome`): the row lands in the
    /// session entry chain and on the live loop context, a context rebuild
    /// over the entries keeps it, and the LLM conversion drops it — the
    /// model never sees the disclosure, so the KV-cacheable prefix is
    /// unaffected (TS `agent-session-compaction.test.ts` pins the same
    /// exclusion).
    #[tokio::test]
    async fn record_appends_row_to_entries_and_live_context_but_not_llm_input() {
        let tmp = tempfile::tempdir().unwrap();
        let session = scripted_session_over(SessionManager::in_memory(tmp.path())).await;
        let row = session
            .record_compaction_outcome(
                CompactionOutcomeReason::Requested,
                CompactionOutcomeKind::Failed,
                "Requested compaction failed: Summarization failed",
            )
            .await;
        // The entry chain owns the row (context rebuilds read it).
        let entries = session.entries().await;
        let outcome_entries: Vec<_> = entries
            .iter()
            .filter_map(|entry| match entry {
                FileEntry::CustomMessage { payload, .. }
                    if payload.custom_type == "compaction_outcome" =>
                {
                    Some(payload.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(outcome_entries.len(), 1, "one durable outcome row");
        assert_eq!(
            outcome_entries[0].content.text(),
            "Requested compaction failed: Summarization failed"
        );
        assert_eq!(
            outcome_entries[0].details,
            Some(serde_json::json!({ "reason": "requested", "outcome": "failed" }))
        );
        assert!(outcome_entries[0].display);
        // The live loop context owns the disclosure (TS
        // `agent.state.messages.push`).
        let state = session.agent().state().await;
        assert!(
            matches!(
                state.messages.last(),
                Some(AgentMessage::Custom(custom)) if custom.role == "custom"
            ),
            "the live context carries the outcome row"
        );
        // A rebuild over the session entries keeps the disclosure (the TS
        // `_unpersistedOutcomes` invariant: a rebuild cannot drop it).
        let guard = session.session.lock().await;
        let context =
            crate::session::build_session_context(guard.get_all_entries(), guard.get_leaf_id());
        drop(guard);
        assert!(
            context
                .messages
                .iter()
                .any(|message| matches!(message, SessionAgentMessage::Custom(custom) if custom.custom_type == "compaction_outcome")),
            "the rebuilt context keeps the outcome row"
        );
        // Model context exclusion: the LLM conversion drops the row.
        assert!(convert_to_llm(std::slice::from_ref(&SessionAgentMessage::Custom(row))).is_empty());
    }

    /// The disclosure survives a failed disk write (the TS
    /// `_unpersistedOutcomes` fallback's guarantee): the entry chain keeps
    /// the row in memory, so a context rebuild never drops it even when the
    /// session file could not be written.
    #[tokio::test]
    async fn record_survives_a_failed_disk_write() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let mut manager = SessionManager::persisted(tmp.path(), &sessions);
        manager.append_message(seeded_assistant());
        let file = manager.get_session_file().unwrap().to_path_buf();
        assert!(file.exists(), "the session file materialized");
        // Replace the session file with a directory at the same path: every
        // disk write path fails (the append line and the atomic rename),
        // even for root (permission bits would not stop root).
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        let session = scripted_session_over(manager).await;
        session
            .record_compaction_outcome(
                CompactionOutcomeReason::Threshold,
                CompactionOutcomeKind::Skipped,
                "Auto-compaction skipped: Already compacted",
            )
            .await;
        // The write failed (the file path is a directory) — but the entry
        // chain and a context rebuild keep the disclosure.
        let entries = session.entries().await;
        assert!(
            entries.iter().any(
                |entry| matches!(entry, FileEntry::CustomMessage { payload, .. }
                if payload.custom_type == "compaction_outcome")
            ),
            "the outcome row stays in the entry chain after the failed write"
        );
        let guard = session.session.lock().await;
        let context =
            crate::session::build_session_context(guard.get_all_entries(), guard.get_leaf_id());
        drop(guard);
        assert!(
            context
                .messages
                .iter()
                .any(|message| matches!(message, SessionAgentMessage::Custom(custom) if custom.custom_type == "compaction_outcome")),
            "a rebuild cannot drop the disclosure"
        );
    }
}
