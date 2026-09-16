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
    fn resolve_model(&self) -> anyhow::Result<Model> {
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
        // Resolve/build once; reuse the session across prompts.
        let turn: anyhow::Result<Option<Value>> = self.runtime.block_on(async {
            let model = self.resolve_model()?;
            {
                let mut guard = self.session.lock().await;
                if guard.is_none() {
                    let built = self.build_session(&model).await?;
                    guard.replace(built);
                }
                let engine: &_ = guard.as_ref().expect("just built");
                engine
                    .session
                    .prompt(&prompt, Default::default())
                    .await
                    .map_err(|error| anyhow::anyhow!("{error:#}"))?;
                engine.session.agent().wait_for_idle().await;
                // The final assistant message of the turn, if any.
                let state = engine.session.agent().state().await;
                let final_message = state.messages.iter().rev().find_map(|message| {
                    match message {
                        pa_agent::types::AgentMessage::Standard(
                            pa_agent::types::Message::Assistant(assistant),
                        ) => {
                            let mut value = json_round_trip::<_, Value>(assistant)?;
                            if assistant.stop_reason == pa_agent::types::StopReason::Error {
                                return Some(value); // caller turns this into an error
                            }
                            // The store records the successful final message.
                            Some(value.take())
                        }
                        _ => None,
                    }
                });
                // Error outcome: the turn failed.
                let mut errored: Option<Value> = None;
                for message in state.messages.iter().rev() {
                    if let pa_agent::types::AgentMessage::Standard(
                        pa_agent::types::Message::Assistant(assistant),
                    ) = message
                    {
                        if assistant.stop_reason == pa_agent::types::StopReason::Error {
                            errored = assistant
                                .error_message
                                .clone()
                                .filter(|text| !text.is_empty())
                                .map(Value::String)
                                .or_else(|| {
                                    Some(Value::String("Assistant response failed".into()))
                                });
                        }
                        break;
                    }
                }
                if let Some(error) = errored {
                    return Err(anyhow::anyhow!(error
                        .as_str()
                        .unwrap_or_default()
                        .to_string()));
                }
                Ok(final_message)
            }
        });
        match turn {
            Ok(Some(message)) => {
                emit(EngineEvent::AssistantMessage(message));
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
