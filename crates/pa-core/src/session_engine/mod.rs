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
pub mod provider_retry;
pub mod refine;
pub mod rlm_host;
pub mod runtime;
pub mod runtime_wiring;
pub mod session_commands;
pub mod side_question;
pub mod slash_commands;
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
        };
        this.ensure_harness_digest_context().await?;
        Ok(this)
    }

    /// Execute `/compact`: summarize the pre-cut prefix, persist the
    /// compaction entry, and rebuild the loop context summary-first. A skip
    /// (already compacted, or nothing to summarize) leaves the session
    /// untouched, matching the TS `CompactionSkippedError` flow.
    pub async fn compact(
        &self,
        custom_instructions: Option<&str>,
        model: &pa_types::ai::Model,
        api_key: Option<String>,
    ) -> anyhow::Result<CompactOutcome> {
        let outcome = {
            let mut session = self.session.lock().await;
            crate::session_engine::compact_session::execute_compaction(
                &mut session,
                crate::session_engine::compact_session::CompactOptions {
                    model: model.clone(),
                    api_key,
                    custom_instructions,
                    settings: crate::session_engine::compaction::CompactionSettings::default(),
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
    if let AgentEvent::MessageEnd { message, .. } = event {
        let Some(session_message) = loop_message_to_session(&message) else {
            return;
        };
        let mut session = session.lock().await;
        session.append_message(session_message);
    }
}

/// Convert a loop message to its persisted form via the shared wire shape.
fn loop_message_to_session(message: &AgentMessage) -> Option<SessionAgentMessage> {
    let AgentMessage::Standard(inner) = message else {
        return None;
    };
    serde_json::from_value(serde_json::to_value(inner).ok()?).ok()
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
