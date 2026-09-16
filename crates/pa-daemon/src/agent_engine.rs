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

use crate::engine::{EngineEvent, PromptRequest, SessionEngine};

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
    fn run_prompt(
        &self,
        _prompt_index: usize,
        request: PromptRequest,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) {
        // The accepted user message is recorded by the worker.
        if !emit(EngineEvent::UserMessage(json!({
            "role": "user",
            "content": [{ "text": request.message }],
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
    /// Run one turn, streaming assistant updates through `emit` as they
    /// arrive. Returns the final assistant message (Ok), or the turn error.
    fn run_turn(
        &self,
        prompt: &str,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) -> anyhow::Result<Option<Value>> {
        let model = self.resolve_model()?;
        // Build (once) without holding the lock across the await.
        {
            let guard = self.session.blocking_lock();
            if guard.is_none() {
                drop(guard);
                let built = self
                    .runtime
                    .block_on(async { self.build_session(&model).await })?;
                self.session.blocking_lock().replace(built);
            }
        }
        let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
        let agent = {
            let guard = self.session.blocking_lock();
            let engine = guard.as_ref().expect("session built");
            engine.session.agent().clone()
        };
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
                            }
                            | AgentEvent::MessageUpdate {
                                message: agent_message,
                                ..
                            } => {
                                if matches!(
                                    agent_message,
                                    pa_agent::types::AgentMessage::Standard(
                                        pa_agent::types::Message::Assistant(_)
                                    )
                                ) {
                                    if let Some(value) = session_wire_value(agent_message) {
                                        let _ = tx.send(EngineEvent::AssistantUpdate(value));
                                    }
                                }
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

/// Register the faux provider from a `{"responses": [...]}` script and return
/// its model. Verification harness only; never set by the product.
fn faux_model_from_script(script: &str) -> anyhow::Result<Model> {
    let script: serde_json::Value = serde_json::from_str(script)?;
    let responses: Vec<String> = script
        .get("responses")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .map(|entry| match entry {
                    serde_json::Value::String(text) => text.clone(),
                    serde_json::Value::Object(map) => map
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    _ => String::new(),
                })
                .collect()
        })
        .ok_or_else(|| anyhow::anyhow!("responses array required"))?;
    let registration =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            models: Some(vec![pa_ai::faux::FauxModelDefinition {
                id: "faux-1".to_string(),
                name: Some("Faux Model".to_string()),
                reasoning: Some(false),
                input: Some(vec![pa_types::ai::ModelInput::Text]),
                cost: None,
                context_window: Some(100_000),
                max_tokens: Some(4_096),
            }]),
            ..Default::default()
        });
    registration.set_responses(
        responses
            .iter()
            .map(|text| {
                pa_ai::faux::FauxResponseStep::Message(pa_ai::faux::faux_assistant_text_message(
                    text,
                    pa_ai::faux::FauxAssistantMessageOptions::default(),
                ))
            })
            .collect(),
    );
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
        .any(|event| matches!(event, EngineEvent::AssistantUpdate(_))));
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
