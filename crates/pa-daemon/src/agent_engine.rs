//! The real agent-session engine for daemon workers: a pa-core session over
//! the shared provider adapter, driven through the daemon's `SessionEngine`
//! contract. Replaces the scripted faux engine when a model is configured.
//!
//! Streaming note: assistant updates are delivered as a batch after the turn
//! settles (the worker persists the final message); live per-chunk streaming to
//! daemon clients is a follow-up wiring on top of the same subscription seam.

use std::sync::Arc;

use serde_json::{json, Value};

use pa_core::session_engine::engine::{SessionEngine as CoreSessionEngine, SessionEngineConfig};
use pa_core::session_engine::provider_adapter::{json_round_trip, real_stream_fn};
use pa_types::ai::Model;

use crate::engine::{
    EngineEvent, PromptRequest, SessionEngine, SideQuestionOutcome, SideQuestionRequest,
};

/// Configuration for the real engine.
#[derive(Debug, Clone)]
pub struct AgentEngineConfig {
    pub cwd: std::path::PathBuf,
    pub agent_dir: std::path::PathBuf,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    /// Session persistence directory (JSONL sessions live under it).
    pub session_dir: Option<std::path::PathBuf>,
    /// Verification seam: a scripted faux provider (`{"responses": [...]}`).
    /// Never set by the product.
    pub faux_script: Option<String>,
}

/// A [`SessionEngine`] running real agent turns.
pub struct AgentSessionEngine {
    runtime: tokio::runtime::Runtime,
    config: AgentEngineConfig,
    /// Built once on the first prompt, reused across prompts.
    session: tokio::sync::Mutex<Option<CoreSessionEngine>>,
}

impl AgentSessionEngine {
    pub fn new(config: AgentEngineConfig) -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        Ok(Self {
            runtime,
            config,
            session: tokio::sync::Mutex::new(None),
        })
    }

    /// Resolve the model through the composed registry.
    fn resolve_registry_model(&self) -> anyhow::Result<Model> {
        let auth = pa_core::auth::AuthStorage::create(&self.config.agent_dir);
        let registry =
            pa_core::models::ModelRegistry::create(auth, self.config.agent_dir.join("models.json"));
        let available: Vec<Model> = registry.get_available().into_iter().cloned().collect();
        let Some(model_name) = self.config.model.as_deref() else {
            let all: Vec<Model> = registry.get_all().to_vec();
            if let Some(default) = pa_core::models::find_preferred_default_model(&available) {
                return Ok(default.clone());
            }
            return all.first().cloned().ok_or_else(|| {
                anyhow::anyhow!(
                    "No models available. Check your installation or add models to models.json."
                )
            });
        };
        let resolved = pa_core::models::resolve_cli_model(
            self.config.provider.as_deref(),
            model_name,
            &available,
        );
        if let Some(error) = resolved.error {
            anyhow::bail!("{error}");
        }
        resolved
            .model
            .ok_or_else(|| anyhow::anyhow!("No matching model found."))
    }

    /// Test seam: a scripted faux provider (same script contract as pa-cli's
    /// print runtime) drives the engine without the network.
    fn resolve_model(&self) -> anyhow::Result<Model> {
        if let Some(script) = &self.config.faux_script {
            return faux_model_from_script(script);
        }
        self.resolve_registry_model()
    }

    async fn build_session(&self, model: &Model) -> anyhow::Result<CoreSessionEngine> {
        let agent_model =
            json_round_trip(model).ok_or_else(|| anyhow::anyhow!("model conversion failed"))?;
        let stream_fn = real_stream_fn(self.config.api_key.clone(), model.clone());
        if let Some(session_dir) = &self.config.session_dir {
            std::fs::create_dir_all(session_dir)?;
        }
        let session_manager =
            pa_core::session::manager::SessionManager::in_memory(&self.config.cwd);
        pa_core::session_engine::engine::create_session(SessionEngineConfig {
            cwd: self.config.cwd.clone(),
            agent_dir: self.config.agent_dir.clone(),
            model: Some(agent_model),
            thinking_level: None,
            stream_fn: Some(stream_fn),
            tools: builtin_tools(&self.config.cwd),
            custom_system_prompt: None,
            prompt_guidelines: vec![],
            generic_mcp_servers: vec![],
            allow_recursion: None,
            session_manager: Some(session_manager),
            additional_skill_paths: vec![],
            additional_prompt_paths: vec![],
            extra_builtin_skill_overrides: vec![],
        })
        .await
    }
}

fn builtin_tools(cwd: &std::path::Path) -> Vec<Arc<dyn pa_agent::types::AgentTool>> {
    let cwd = cwd.display().to_string();
    vec![
        pa_core::create_bash_tool_definition(&cwd),
        pa_core::create_edit_tool_definition(&cwd),
    ]
    .into_iter()
    .map(|definition| {
        Arc::new(pa_core::session_engine::tool_bridge::ToolDefinitionBridge::new(definition))
            as Arc<dyn pa_agent::types::AgentTool>
    })
    .collect()
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

impl SessionEngine for AgentSessionEngine {
    fn model_context_window(&self) -> Option<u64> {
        self.resolve_model().ok().map(|model| model.context_window)
    }

    fn model_metadata(&self) -> Option<Value> {
        let model = self.resolve_model().ok()?;
        Some(json!({
            "id": model.id,
            "name": model.name,
            "provider": model.provider,
            "reasoning": model.reasoning,
        }))
    }

    fn run_side_question(
        &self,
        request: SideQuestionRequest,
        signal: &pa_agent::abort::AbortSignal,
        sink: &pa_core::session_engine::side_question::SideQuestionSink,
    ) -> SideQuestionOutcome {
        let model = match self.resolve_model() {
            Ok(model) => model,
            Err(error) => {
                return SideQuestionOutcome::Failed {
                    answer: String::new(),
                    error: error.to_string(),
                }
            }
        };
        let agent = match self.session_agent(&model) {
            Ok(agent) => agent,
            Err(error) => {
                return SideQuestionOutcome::Failed {
                    answer: String::new(),
                    error: error.to_string(),
                }
            }
        };
        let question = request.question.clone();
        let previous_turns = request.previous_turns.clone();
        let retry_policy = pa_core::session_engine::provider_retry::DEFAULT_PROVIDER_RETRY_POLICY;
        let result =
            self.runtime
                .block_on(pa_core::session_engine::side_question::run_side_question(
                    &agent,
                    &question,
                    &previous_turns,
                    &retry_policy,
                    signal,
                    sink,
                ));
        match result.status {
            pa_core::session_engine::side_question::SideQuestionStatus::Complete => {
                SideQuestionOutcome::Complete {
                    answer: result.answer,
                }
            }
            pa_core::session_engine::side_question::SideQuestionStatus::Cancelled => {
                SideQuestionOutcome::Aborted {
                    answer: result.answer,
                }
            }
            pa_core::session_engine::side_question::SideQuestionStatus::Error => {
                SideQuestionOutcome::Failed {
                    answer: result.answer,
                    error: result
                        .error_message
                        .unwrap_or_else(|| "Side question failed".to_string()),
                }
            }
        }
    }

    fn run_prompt(
        &self,
        _prompt_index: usize,
        request: PromptRequest,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) {
        // The accepted user message is recorded by the worker.
        if !emit(EngineEvent::UserMessage(json!({
            "role": "user",
            "content": [{ "type": "text", "text": request.message }],
            "timestamp": now_millis(),
        }))) {
            return;
        }
        let prompt = request.message;
        let result: anyhow::Result<Option<Value>> = self.run_turn(&prompt, emit);
        match result {
            Ok(Some(message)) => {
                if !emit(EngineEvent::AssistantMessage(message)) {
                    return;
                }
                emit(EngineEvent::Done(Ok(())));
            }
            Ok(None) => {
                emit(EngineEvent::Done(Err("No response produced.".to_string())));
            }
            Err(error) => {
                emit(EngineEvent::Done(Err(error.to_string())));
            }
        }
    }
}

impl AgentSessionEngine {
    /// The hosted session's agent loop, building the session on first use.
    fn session_agent(
        &self,
        model: &Model,
    ) -> anyhow::Result<std::sync::Arc<pa_agent::agent::Agent>> {
        // Build (once) without holding the lock across the await.
        {
            let guard = self.session.blocking_lock();
            if guard.is_none() {
                drop(guard);
                let built = self
                    .runtime
                    .block_on(async { self.build_session(model).await })?;
                self.session.blocking_lock().replace(built);
            }
        }
        let guard = self.session.blocking_lock();
        let engine = guard.as_ref().expect("session built");
        Ok(std::sync::Arc::clone(engine.session.agent()))
    }

    /// Run one turn, streaming assistant updates through `emit` as they
    /// arrive. Returns the final assistant message (Ok), or the turn error.
    fn run_turn(
        &self,
        prompt: &str,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) -> anyhow::Result<Option<Value>> {
        let model = self.resolve_model()?;
        let agent = self.session_agent(&model)?;
        let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
        // Stream assistant events while the turn runs. The turn starts
        // asynchronously after admission, so the idle watcher must not fire
        // before the run has begun.
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let subscription = self.runtime.block_on(async {
            let tx = std::sync::Arc::new(tx);
            let started_flag = started.clone();
            agent
                .subscribe(move |event, _signal| {
                    let tx = tx.clone();
                    let started_flag = started_flag.clone();
                    Box::pin(async move {
                        use pa_agent::types::AgentEvent;
                        if matches!(event, AgentEvent::AgentStart) {
                            started_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                        match &event {
                            AgentEvent::MessageStart {
                                message: agent_message,
                            } => {
                                if matches!(
                                    agent_message,
                                    pa_agent::types::AgentMessage::Standard(
                                        pa_agent::types::Message::Assistant(_)
                                    )
                                ) {
                                    if let Some(value) = session_wire_value(agent_message) {
                                        let _ = tx.send(EngineEvent::AssistantUpdate {
                                            message: value,
                                            stream_event: Some(json!({ "type": "start" })),
                                        });
                                    }
                                }
                            }
                            AgentEvent::MessageUpdate {
                                message: agent_message,
                                assistant_message_event: stream_event,
                            } => {
                                if matches!(
                                    agent_message,
                                    pa_agent::types::AgentMessage::Standard(
                                        pa_agent::types::Message::Assistant(_)
                                    )
                                ) {
                                    if let Some(value) = session_wire_value(agent_message) {
                                        let _ = tx.send(EngineEvent::AssistantUpdate {
                                            message: value,
                                            stream_event: stream_event_value(stream_event),
                                        });
                                    }
                                }
                            }
                            AgentEvent::ToolExecutionStart {
                                tool_call_id,
                                tool_name,
                                args,
                            } => {
                                let _ = tx.send(EngineEvent::ToolExecutionStart {
                                    tool_call_id: tool_call_id.clone(),
                                    tool_name: tool_name.clone(),
                                    args: args.clone(),
                                });
                            }
                            AgentEvent::ToolExecutionUpdate {
                                tool_call_id,
                                partial_result,
                                ..
                            } => {
                                let _ = tx.send(EngineEvent::ToolExecutionUpdate {
                                    tool_call_id: tool_call_id.clone(),
                                    partial_result: tool_result_wire_value(partial_result),
                                });
                            }
                            AgentEvent::ToolExecutionEnd {
                                tool_call_id,
                                result,
                                is_error,
                                ..
                            } => {
                                let _ = tx.send(EngineEvent::ToolExecutionEnd {
                                    tool_call_id: tool_call_id.clone(),
                                    result: tool_result_wire_value(result),
                                    is_error: *is_error,
                                });
                            }
                            _ => {}
                        }
                        Ok(())
                    })
                })
                .await
        });
        // Admit the prompt.
        {
            let guard = self.session.blocking_lock();
            let engine = guard.as_ref().expect("session built");
            self.runtime
                .block_on(async { engine.session.prompt(prompt, Default::default()).await })
                .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        }
        // Wait for the turn to settle, draining events into `emit` live.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let idle_agent = agent.clone();
        let started_flag = started.clone();
        self.runtime.spawn(async move {
            loop {
                idle_agent.wait_for_idle().await;
                if started_flag.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            let _ = done_tx.send(());
        });
        let mut aborted = false;
        loop {
            while let Ok(event) = rx.recv_timeout(std::time::Duration::from_millis(20)) {
                if !emit(event) {
                    aborted = true;
                    break;
                }
            }
            if aborted {
                break;
            }
            if done_rx.try_recv().is_ok() {
                break;
            }
        }
        self.runtime
            .block_on(async { subscription.unsubscribe().await });
        if aborted {
            return Ok(None);
        }
        // The final assistant message decides the outcome.
        let state = self.runtime.block_on(async { agent.state().await });
        for message in state.messages.iter().rev() {
            if let pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(
                assistant,
            )) = message
            {
                if assistant.stop_reason == pa_agent::types::StopReason::Error {
                    let error = assistant
                        .error_message
                        .clone()
                        .filter(|text| !text.is_empty())
                        .unwrap_or_else(|| "Assistant response failed".to_string());
                    return Err(anyhow::anyhow!(error));
                }
                // Serialize through the session wire shape so `role` is
                // present (the store records session-shaped messages).
                let Some(ai_message) =
                    json_round_trip::<_, pa_types::ai::AssistantMessage>(assistant)
                else {
                    return Ok(None);
                };
                let session_message = pa_types::session::AgentMessage::Assistant(ai_message);
                return serde_json::to_value(&session_message)
                    .map(Some)
                    .map_err(|error| anyhow::anyhow!("{error}"));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_engine_reports_model_resolution_failures() {
        let dir = tempfile::TempDir::new().unwrap();
        let engine = AgentSessionEngine::new(AgentEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir: dir.path().join("agent"),
            provider: Some("no-such-provider".to_string()),
            model: Some("some-model".to_string()),
            api_key: None,
            session_dir: None,
            faux_script: None,
        })
        .unwrap();
        let mut events: Vec<EngineEvent> = Vec::new();
        engine.run_prompt(
            0,
            PromptRequest {
                message: "hi".to_string(),
                source: "user".to_string(),
                agent_message_id: None,
            },
            &mut |event| {
                events.push(event);
                true
            },
        );
        // The engine degrades to a Done error with the resolver message.
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], EngineEvent::UserMessage(_)));
        let EngineEvent::Done(Err(error)) = &events[1] else {
            panic!("expected error done");
        };
        assert!(error.contains("Unknown provider"));
    }
}

/// Register the faux provider from a script and return its model. Scripts
/// carry plain-text responses (strings or `{"text"}` objects) or content-block
/// arrays (thinking, text, tool calls) so harnesses can script full turns.
/// Verification harness only; never set by the product.
fn faux_model_from_script(script: &str) -> anyhow::Result<Model> {
    let script: serde_json::Value = serde_json::from_str(script)?;
    let parsed = pa_ai::faux::script::parse_faux_script(&script).map_err(anyhow::Error::msg)?;
    let registration = pa_ai::faux::script::register_faux_provider_from_script(&parsed);
    Ok(registration.get_model())
}

#[test]
fn agent_engine_streams_updates_and_final_message() {
    let dir = tempfile::TempDir::new().unwrap();
    // Scoped env: the faux seam is process-global; keep the test isolated.
    let engine = AgentSessionEngine::new(AgentEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        provider: None,
        model: None,
        api_key: None,
        session_dir: None,
        faux_script: Some(serde_json::json!({ "responses": ["streamed answer"] }).to_string()),
    })
    .unwrap();
    let mut events: Vec<EngineEvent> = Vec::new();
    engine.run_prompt(
        0,
        PromptRequest {
            message: "hi".to_string(),
            source: "user".to_string(),
            agent_message_id: None,
        },
        &mut |event| {
            events.push(event);
            true
        },
    );
    // User message, streamed updates, final message, done.
    assert!(matches!(&events[0], EngineEvent::UserMessage(_)));
    assert!(events
        .iter()
        .any(|event| matches!(event, EngineEvent::AssistantUpdate { .. })));
    let final_index = events
        .iter()
        .position(|event| matches!(event, EngineEvent::AssistantMessage(_)))
        .expect("final assistant message");
    let EngineEvent::AssistantMessage(message) = &events[final_index] else {
        unreachable!();
    };
    assert_eq!(message["content"][0]["text"], "streamed answer");
    assert_eq!(message["role"], "assistant");
    assert_eq!(message["stopReason"], "stop");
    assert_eq!(events.last(), Some(&EngineEvent::Done(Ok(()))));
}

/// Serialize a pa-agent message through the session wire shape (adds `role`).
/// Wire form of one provider stream event (TS `assistantMessageEvent`):
/// the event `type` plus the `delta` when the event carries one.
fn stream_event_value(event: &pa_agent::stream::AssistantMessageEvent) -> Option<Value> {
    use pa_agent::stream::AssistantMessageEvent;
    let (kind, delta) = match event {
        AssistantMessageEvent::Start { .. } => ("start", None),
        AssistantMessageEvent::TextStart { .. } => ("text_start", None),
        AssistantMessageEvent::TextDelta { delta, .. } => ("text_delta", Some(delta.as_str())),
        AssistantMessageEvent::TextEnd { .. } => ("text_end", None),
        AssistantMessageEvent::ThinkingStart { .. } => ("thinking_start", None),
        AssistantMessageEvent::ThinkingDelta { delta, .. } => {
            ("thinking_delta", Some(delta.as_str()))
        }
        AssistantMessageEvent::ThinkingEnd { .. } => ("thinking_end", None),
        AssistantMessageEvent::ToolCallStart { .. } => ("toolcall_start", None),
        AssistantMessageEvent::ToolCallDelta { delta, .. } => {
            ("toolcall_delta", Some(delta.as_str()))
        }
        AssistantMessageEvent::ToolCallEnd { .. } => ("toolcall_end", None),
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => return None,
    };
    match delta {
        Some(delta) => Some(json!({ "type": kind, "delta": delta })),
        None => Some(json!({ "type": kind })),
    }
}

/// Wire form of one tool result (the TS tool-execution event payload).
fn tool_result_wire_value(result: &pa_agent::types::AgentToolResult) -> Value {
    let content: Vec<Value> = result
        .content
        .iter()
        .map(|block| serde_json::to_value(block).unwrap_or(Value::Null))
        .collect();
    json!({ "content": content, "details": result.details })
}

fn session_wire_value(agent_message: &pa_agent::types::AgentMessage) -> Option<Value> {
    use pa_agent::types::Message as LoopMessage;
    let session_message = match agent_message {
        pa_agent::types::AgentMessage::Standard(LoopMessage::User(user)) => {
            pa_types::session::AgentMessage::User(json_round_trip(user)?)
        }
        pa_agent::types::AgentMessage::Standard(LoopMessage::Assistant(assistant)) => {
            pa_types::session::AgentMessage::Assistant(json_round_trip(assistant)?)
        }
        _ => return None,
    };
    serde_json::to_value(&session_message).ok()
}
