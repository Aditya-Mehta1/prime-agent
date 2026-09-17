//! The real agent-session engine for daemon workers: a pa-core session over
//! the shared provider adapter, driven through the daemon's `SessionEngine`
//! contract. Replaces the scripted faux engine when a model is configured.
//!
//! Streaming note: assistant updates are delivered as a batch after the turn
//! settles (the worker persists the final message); live per-chunk streaming to
//! daemon clients is a follow-up wiring on top of the same subscription seam.

use std::sync::Arc;

use serde_json::{json, Value};

use pa_core::kernel::shared::HostRequestHandlers;
use pa_core::session_engine::agent_messaging::{
    register_agent_message_host_handlers, register_agent_observe_host_handlers,
    AgentMessageController, AgentMessageDeliveryStatus, AgentMessageReceipt, AgentMessageSendInput,
    AgentObserveController, AgentObserveMessagePreview, AgentObserveSummary,
};
use pa_core::session_engine::engine::{SessionEngine as CoreSessionEngine, SessionEngineConfig};
use pa_core::session_engine::provider_adapter::{json_round_trip, real_stream_fn};
use pa_types::ai::Model;

use crate::engine::{
    CompactionOutcome, CompactionRequest, CompactionRun, EngineEvent, EngineModelSelection,
    PromptRequest, SessionEngine, SideQuestionOutcome, SideQuestionRequest,
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
    /// Conversation-log path for the system prompt: the daemon worker owns
    /// the session file, so the in-session manager stays in-memory and the
    /// prompt reads the path from here.
    pub session_file: Option<std::path::PathBuf>,
    /// Verification seam: a scripted faux provider (`{"responses": [...]}`).
    /// Never set by the product.
    pub faux_script: Option<String>,
    /// Supervisor socket + own active session id for the worker's supervisor
    /// link. Present only inside a daemon worker; it enables the kernel's
    /// agent_message/agent_observe host requests.
    pub supervisor_link: Option<SupervisorLinkConfig>,
}

/// Supervisor-link coordinates for a daemon worker.
#[derive(Clone, Debug)]
pub struct SupervisorLinkConfig {
    pub socket_path: std::path::PathBuf,
    /// The worker's own active session id, stamped on outgoing messages so
    /// the supervisor can attribute them to this session.
    pub active_session_id: String,
}

/// A [`SessionEngine`] running real agent turns.
pub struct AgentSessionEngine {
    runtime: tokio::runtime::Runtime,
    config: AgentEngineConfig,
    /// The worker-owned session file (conversation-log path), set at create.
    session_file: std::sync::Mutex<Option<std::path::PathBuf>>,
    /// The authoritative model selection. Starts from the process fallback
    /// (create config or worker env) and is re-bound when a session's create
    /// command carries explicit wire flags.
    selection: std::sync::RwLock<EngineModelSelection>,
    /// Built once on the first prompt, reused across prompts.
    session: tokio::sync::Mutex<Option<CoreSessionEngine>>,
}

impl AgentSessionEngine {
    pub fn new(config: AgentEngineConfig) -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let session_file = std::sync::Mutex::new(config.session_file.clone());
        // Process-level fallback: the create config, else the worker env
        // pair. A create command with explicit wire flags overrides both.
        let selection = if config.provider.is_some() || config.model.is_some() {
            EngineModelSelection {
                provider: config.provider.clone(),
                model: config.model.clone(),
                api_key: config.api_key.clone(),
            }
        } else {
            EngineModelSelection {
                provider: std::env::var("PRIME_AGENT_MODEL_PROVIDER").ok(),
                model: std::env::var("PRIME_AGENT_MODEL").ok(),
                api_key: None,
            }
        };
        Ok(Self {
            runtime,
            config,
            session_file,
            selection: std::sync::RwLock::new(selection),
            session: tokio::sync::Mutex::new(None),
        })
    }

    /// The current explicit selection (create-config flags merged over the
    /// process fallback).
    fn current_selection(&self) -> EngineModelSelection {
        self.selection.read().expect("model selection lock").clone()
    }

    /// Resolve the model through the composed registry.
    fn resolve_registry_model(&self) -> anyhow::Result<Model> {
        let auth = pa_core::auth::AuthStorage::create(&self.config.agent_dir);
        let registry =
            pa_core::models::ModelRegistry::create(auth, self.config.agent_dir.join("models.json"));
        let available: Vec<Model> = registry.get_available().into_iter().cloned().collect();
        let selection = self.current_selection();
        let Some(model_name) = selection.model.as_deref() else {
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
            selection.provider.as_deref(),
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

    /// Resolve the request API key for `model`: the create-config key (the
    /// TS `setRuntimeApiKey` path), else the registry's auth resolution
    /// (auth storage, then the models.json provider `apiKey` — the same
    /// sources `getApiKeyAndHeaders` merges in the TS product).
    fn resolve_request_api_key(&self, model: &Model) -> Option<String> {
        if let Some(api_key) = &self.current_selection().api_key {
            return Some(api_key.clone());
        }
        let auth = pa_core::auth::AuthStorage::create(&self.config.agent_dir);
        let mut registry =
            pa_core::models::ModelRegistry::create(auth, self.config.agent_dir.join("models.json"));
        registry
            .get_api_key_and_headers(model, model.headers.as_ref())
            .api_key
    }

    /// Kernel host-request handlers for agent messaging and observation,
    /// routed through the worker's supervisor link. `None` outside a daemon
    /// worker: without a supervisor there is nobody to reach.
    fn extra_host_handlers(&self) -> Option<HostRequestHandlers> {
        let config = self.config.supervisor_link.as_ref()?;
        let link = Arc::new(crate::supervisor_link::SupervisorLink::new(
            config.socket_path.clone(),
        ));
        let sender = Arc::new(LinkAgentMessageController {
            link: Arc::clone(&link),
            active_session_id: config.active_session_id.clone(),
        });
        let observer = Arc::new(LinkAgentObserveController { link });
        let mut handlers = HostRequestHandlers::default();
        register_agent_message_host_handlers(sender, &mut handlers);
        register_agent_observe_host_handlers(observer, &mut handlers);
        Some(handlers)
    }

    async fn build_session(&self, model: &Model) -> anyhow::Result<CoreSessionEngine> {
        let agent_model =
            json_round_trip(model).ok_or_else(|| anyhow::anyhow!("model conversion failed"))?;
        let stream_fn = real_stream_fn(self.resolve_request_api_key(model), model.clone());
        if let Some(session_dir) = &self.config.session_dir {
            std::fs::create_dir_all(session_dir)?;
        }
        let session_manager =
            pa_core::session::manager::SessionManager::in_memory(&self.config.cwd);
        let session_file = self
            .session_file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        pa_core::session_engine::engine::create_session(SessionEngineConfig {
            cwd: self.config.cwd.clone(),
            agent_dir: self.config.agent_dir.clone(),
            model: Some(agent_model),
            thinking_level: None,
            stream_fn: Some(stream_fn),
            // Model tools: `ipython` only (kernel-resident bash/edit parity);
            // the engine adds the kernel-backed `ipython` tool itself.
            tools: vec![],
            custom_system_prompt: None,
            prompt_guidelines: vec![],
            generic_mcp_servers: vec![],
            allow_recursion: None,
            session_manager: Some(session_manager),
            extra_host_handlers: self.extra_host_handlers(),
            conversation_log_path: session_file,
            additional_skill_paths: vec![],
            additional_prompt_paths: vec![],
            extra_builtin_skill_overrides: vec![],
        })
        .await
    }
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

    fn creation_model(&self) -> Option<(String, String)> {
        let model = self.resolve_registry_model().ok()?;
        Some((model.provider.clone(), model.id.clone()))
    }

    fn set_session_file(&self, path: std::path::PathBuf) {
        *self
            .session_file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
    }

    fn configure_model(&self, selection: EngineModelSelection) {
        // Merge like the TS runtime config: explicit wire flags replace the
        // current selection; absent fields keep it.
        let mut current = self.selection.write().expect("model selection lock");
        if selection.provider.is_some() {
            current.provider = selection.provider;
        }
        if selection.model.is_some() {
            current.model = selection.model;
        }
        if selection.api_key.is_some() {
            current.api_key = selection.api_key;
        }
        // The first prompt after create builds the session against this
        // selection, so no invalidation is needed here: configure runs at
        // create time, before any turn.
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

    /// `compact` over the hosted pa-core session: the session summarizes
    /// its own branch, persists the entry on its in-memory store, and
    /// rebuilds the loop context; the worker persists the durable entry.
    /// The abort races the run: the summarizer call is cancelled by
    /// dropping the future (the entry write happens inside it).
    fn run_compaction(
        &self,
        request: CompactionRequest,
        signal: &pa_agent::abort::AbortSignal,
    ) -> CompactionOutcome {
        let model = match self.resolve_model() {
            Ok(model) => model,
            Err(error) => {
                return CompactionOutcome::Failed {
                    error: error.to_string(),
                }
            }
        };
        if let Err(error) = self.session_agent(&model) {
            return CompactionOutcome::Failed {
                error: error.to_string(),
            };
        }
        let custom_instructions = request.custom_instructions.clone();
        let api_key = self.config.api_key.clone();
        let run = async {
            let guard = self.session.lock().await;
            let Some(engine) = guard.as_ref() else {
                anyhow::bail!("session not built");
            };
            engine
                .session
                .compact(custom_instructions.as_deref(), &model, api_key)
                .await
        };
        let result = self
            .runtime
            .block_on(pa_agent::abort::race_with_abort(run, signal));
        let compaction = match result {
            Ok(Ok(compaction)) => compaction,
            Ok(Err(error)) => {
                // Abort-marked errors and a lost abort race both surface as
                // the TS "Compaction cancelled" outcome.
                if pa_agent::abort::is_abort_error(&error) {
                    return CompactionOutcome::Aborted;
                }
                return CompactionOutcome::Failed {
                    error: format!("{error:#}"),
                };
            }
            Err(_) => return CompactionOutcome::Aborted,
        };
        CompactionOutcome::Compacted {
            run: CompactionRun {
                // pa-core's result carries summary/cut/tokens plus the
                // summarizer usage; file-op details are entry-side in
                // pa-core and not exposed on the compact result yet.
                result: json!({
                    "summary": compaction.summary,
                    "firstKeptEntryId": compaction.first_kept_entry_id,
                    "tokensBefore": compaction.tokens_before,
                }),
                usage: compaction
                    .usage
                    .and_then(|usage| serde_json::to_value(usage).ok()),
            },
        }
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

    /// A models.json custom provider (name has no env-key mapping), with an
    /// apiKey the registry must resolve for request auth (the env-key map
    /// alone cannot find it).
    fn write_custom_provider_models_json(agent_dir: &std::path::Path, base_url: &str) {
        std::fs::create_dir_all(agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("models.json"),
            serde_json::json!({
                "providers": {
                    "battery": {
                        "api": "openai-completions",
                        "baseUrl": base_url,
                        "apiKey": "sk-battery",
                        "models": [
                            {
                                "id": "mock-1",
                                "name": "Mock 1",
                                "api": "openai-completions",
                                "contextWindow": 128000,
                                "maxTokens": 4096,
                            }
                        ]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn create_config_flags_reach_the_engine_model_resolution() {
        let dir = tempfile::TempDir::new().unwrap();
        let agent_dir = dir.path().join("agent");
        write_custom_provider_models_json(&agent_dir, "http://127.0.0.1:9");

        let engine = AgentSessionEngine::new(AgentEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir: agent_dir.clone(),
            // No process-level fallback: the wire flags must be the source.
            provider: None,
            model: None,
            api_key: None,
            session_dir: None,
            session_file: None,
            faux_script: None,
            supervisor_link: None,
        })
        .unwrap();
        // The explicit selection from the session's create config is
        // authoritative over any process-wide fallback model.
        engine.configure_model(EngineModelSelection {
            provider: Some("battery".to_string()),
            model: Some("mock-1".to_string()),
            api_key: None,
        });
        let model = engine.resolve_registry_model().expect("resolved model");
        assert_eq!(model.provider, "battery");
        assert_eq!(model.id, "mock-1");
        // The registry resolves the models.json apiKey (the provider name has
        // no env-key mapping), so the engine can authenticate without env.
        assert_eq!(
            engine.resolve_request_api_key(&model).as_deref(),
            Some("sk-battery")
        );
    }

    #[test]
    fn configure_model_merges_only_present_fields() {
        let dir = tempfile::TempDir::new().unwrap();
        let agent_dir = dir.path().join("agent");
        write_custom_provider_models_json(&agent_dir, "http://127.0.0.1:9");
        let engine = AgentSessionEngine::new(AgentEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir,
            provider: Some("battery".to_string()),
            model: Some("mock-1".to_string()),
            api_key: Some("flag-key".to_string()),
            session_dir: None,
            session_file: None,
            faux_script: None,
            supervisor_link: None,
        })
        .unwrap();
        // A create config with only a model keeps the provider and key.
        engine.configure_model(EngineModelSelection {
            provider: None,
            model: Some("mock-1".to_string()),
            api_key: None,
        });
        let model = engine.resolve_registry_model().expect("resolved model");
        assert_eq!(model.provider, "battery");
        assert_eq!(
            engine.resolve_request_api_key(&model).as_deref(),
            Some("flag-key")
        );
    }

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
            session_file: None,
            faux_script: None,
            supervisor_link: None,
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
        session_file: None,
        faux_script: Some(serde_json::json!({ "responses": ["streamed answer"] }).to_string()),
        supervisor_link: None,
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

// ---------------------------------------------------------------------------
// Supervisor-link controllers (kernel agent_message/agent_observe bridges)
// ---------------------------------------------------------------------------

/// `agent_message.send` controller for daemon workers: one `send_message`
/// command over the supervisor link (the TS worker's
/// `sendRemoteAgentSessionMessage` path). Never retried: daemon commands
/// are not idempotent.
struct LinkAgentMessageController {
    link: Arc<crate::supervisor_link::SupervisorLink>,
    active_session_id: String,
}

impl AgentMessageController for LinkAgentMessageController {
    async fn send_agent_message(
        &self,
        input: AgentMessageSendInput,
    ) -> anyhow::Result<AgentMessageReceipt> {
        let data = self
            .link
            .request_success(
                json!({
                    "type": "send_message",
                    "targetActiveSessionId": input.target,
                    "message": input.message,
                    "fromActiveSessionId": self.active_session_id,
                    "agentOrigin": true,
                }),
                std::time::Duration::from_secs(30),
            )
            .await?;
        Ok(receipt_from_wire(&data, input))
    }
}

/// Map the supervisor's receipt payload onto the kernel receipt shape.
fn receipt_from_wire(data: &Value, input: AgentMessageSendInput) -> AgentMessageReceipt {
    let delivery_status = if data.get("deliveryStatus").and_then(Value::as_str) == Some("delivered")
    {
        AgentMessageDeliveryStatus::Delivered
    } else {
        AgentMessageDeliveryStatus::Queued
    };
    let delivery_mode = match data.get("deliveryMode").and_then(Value::as_str) {
        Some("follow_up") => "follow_up",
        _ => "steer",
    };
    AgentMessageReceipt {
        id: data
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        target: input.target.clone(),
        message: input.message,
        delivery_status,
        delivery_mode: Some(delivery_mode),
        receiver_role: input.receiver_role,
        delivered_at: data
            .get("deliveredAt")
            .and_then(Value::as_str)
            .map(str::to_string),
        queued_at: data
            .get("queuedAt")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// `agent_observe.*` controller for daemon workers: the roster via the
/// supervisor's `list` command, message previews via `get_messages`.
struct LinkAgentObserveController {
    link: Arc<crate::supervisor_link::SupervisorLink>,
}

impl AgentObserveController for LinkAgentObserveController {
    async fn list_agents(&self) -> anyhow::Result<Vec<AgentObserveSummary>> {
        let data = self
            .link
            .request_success(
                json!({ "type": "list" }),
                std::time::Duration::from_secs(30),
            )
            .await?;
        let sessions = data
            .get("sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(summaries_from_roster(sessions))
    }

    async fn get_agent(&self, target: &str) -> anyhow::Result<Option<AgentObserveSummary>> {
        let data = self
            .link
            .request_success(
                json!({ "type": "list" }),
                std::time::Duration::from_secs(30),
            )
            .await?;
        let sessions = data
            .get("sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(summaries_from_roster(sessions).into_iter().find(|summary| {
            summary.active_session_id.as_deref() == Some(target)
                || summary.session_id == target
                || summary.session_name.as_deref() == Some(target)
        }))
    }

    async fn recent_messages(
        &self,
        target: &str,
        limit: usize,
        max_chars: usize,
    ) -> anyhow::Result<Vec<AgentObserveMessagePreview>> {
        let data = self
            .link
            .request_success(
                json!({
                    "type": "get_messages",
                    "activeSessionId": target,
                }),
                std::time::Duration::from_secs(30),
            )
            .await?;
        let messages = data
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let total = messages.len();
        let start = total.saturating_sub(limit);
        let mut previews = Vec::new();
        for (index, message) in messages.iter().enumerate().skip(start) {
            let full_text = message_preview_text(message);
            previews.push(AgentObserveMessagePreview {
                index,
                role: message
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                timestamp: message.get("timestamp").and_then(Value::as_u64),
                text: truncate_chars(&full_text, max_chars),
                truncated: full_text.chars().count() > max_chars,
                tool_calls: Vec::new(),
                custom_type: message
                    .get("customType")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
        Ok(previews)
    }
}

/// Flatten the supervisor's `list` rows (session summaries) into roster
/// summaries for `agent_observe`.
fn summaries_from_roster(sessions: Vec<Value>) -> Vec<AgentObserveSummary> {
    sessions
        .into_iter()
        .map(|session| {
            let runtime_kind = session
                .get("runtimeKind")
                .and_then(Value::as_str)
                .unwrap_or("top-level")
                .to_string();
            let relationship = match runtime_kind.as_str() {
                "subagent" => {
                    Some(pa_core::session_engine::agent_messaging::AgentFamilyRelationship::Child)
                }
                _ => None,
            };
            let queued = session
                .get("sessionActions")
                .and_then(|actions| actions.get("queuedCount"))
                .and_then(Value::as_u64)
                .unwrap_or_default() as usize;
            AgentObserveSummary {
                active_session_id: session
                    .get("activeSessionId")
                    .or_else(|| session.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                session_id: session
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                session_name: session
                    .get("sessionName")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string),
                relationship,
                runtime_kind: Some(runtime_kind),
                status: if session.get("activity").and_then(Value::as_str) == Some("idle") {
                    "inactive".to_string()
                } else {
                    "running".to_string()
                },
                is_current: false,
                is_streaming: session
                    .get("isStreaming")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                is_compacting: session
                    .get("isCompacting")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                attached_clients: session
                    .get("attachedClients")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as usize,
                queued_count: queued,
                is_session_active: session
                    .get("isSessionActive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }
        })
        .collect()
}

/// Concatenate a stored message's content into preview text.
fn message_preview_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}
