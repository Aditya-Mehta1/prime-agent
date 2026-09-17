//! The real agent-session engine for daemon workers: a pa-core session over
//! the shared provider adapter, driven through the daemon's `SessionEngine`
//! contract. Replaces the scripted faux engine when a model is configured.
//!
//! Streaming note: assistant updates are delivered as a batch after the turn
//! settles (the worker persists the final message); live per-chunk streaming to
//! daemon clients is a follow-up wiring on top of the same subscription seam.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::agent_messaging::{LinkAgentMessageController, LinkAgentObserveController};
use pa_agent::types::StopReason;
use pa_core::kernel::shared::HostRequestHandlers;
use pa_core::session_engine::agent_messaging::{
    register_agent_message_host_handlers, register_agent_observe_host_handlers,
};
use pa_core::session_engine::engine::{SessionEngine as CoreSessionEngine, SessionEngineConfig};
use pa_core::session_engine::provider_adapter::{json_round_trip, real_stream_fn};
use pa_core::session_engine::session_commands::{
    execute_session_command, SessionCommandExecution, SessionCommandParams,
};
use pa_types::ai::Model;

use crate::engine::{
    CompactionOutcome, CompactionRequest, CompactionRun, EngineEvent, EngineModelSelection,
    PromptRequest, SessionEngine, SideQuestionOutcome, SideQuestionRequest,
};
use crate::rlm_children::{ParentIdentity, SupervisorChildSessions, DEFAULT_RLM_MAX_DEPTH};

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
    /// The worker's authentication token, presented on supervisor requests
    /// that act on this worker's behalf (worker-to-worker peer tickets).
    pub worker_token: String,
}

/// A [`SessionEngine`] running real agent turns.
pub struct AgentSessionEngine {
    pub(crate) runtime: tokio::runtime::Runtime,
    pub(crate) config: AgentEngineConfig,
    /// The worker-owned session file (conversation-log path), set at create.
    session_file: std::sync::Mutex<Option<std::path::PathBuf>>,
    /// The authoritative model selection. Starts from the process fallback
    /// (create config or worker env) and is re-bound when a session's create
    /// command carries explicit wire flags.
    selection: std::sync::RwLock<EngineModelSelection>,
    /// The default thinking level from the session's create command.
    thinking: std::sync::RwLock<Option<String>>,
    /// Built once on the first prompt, reused across prompts.
    pub(crate) session: tokio::sync::Mutex<Option<CoreSessionEngine>>,
    /// One shared supervisor-link client for the worker: agent messaging
    /// and supervisor-backed RLM children multiplex the same connection
    /// (the TS worker's single `SupervisorLink` socket). Unconnected until
    /// the first request; standalone workers never use it.
    link: Arc<crate::supervisor_link::SupervisorLink>,
    /// Supervisor-backed RLM children; `None` for standalone workers.
    children: Option<Arc<SupervisorChildSessions>>,
    /// This worker's own session summary (worker-pushed at create/rename),
    /// read by the kernel messaging controller to render sender identity.
    own_summary: std::sync::Arc<std::sync::Mutex<Option<Value>>>,
    /// The session's autonomous runtime state (limits, usage accounting).
    /// The continuation driver that consults it is future work; `/autonomous`
    /// status, on, and off operate on this state today.
    pub(crate) autonomous: std::sync::Mutex<pa_core::autonomous::AutonomousRuntimeState>,
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
        // One shared supervisor-link client for the worker: agent messaging
        // and supervisor-backed RLM children multiplex the same connection
        // (the TS worker's single `SupervisorLink` socket).
        let link = Arc::new(crate::supervisor_link::SupervisorLink::new(
            config
                .supervisor_link
                .as_ref()
                .map(|link_config| link_config.socket_path.clone())
                .unwrap_or_default(),
        ));
        let children = config.supervisor_link.as_ref().map(|link_config| {
            Arc::new(SupervisorChildSessions::new(
                Arc::clone(&link),
                config.agent_dir.clone(),
                link_config.active_session_id.clone(),
            ))
        });
        Ok(Self {
            runtime,
            config,
            session_file,
            selection: std::sync::RwLock::new(selection),
            thinking: std::sync::RwLock::new(None),
            session: tokio::sync::Mutex::new(None),
            own_summary: std::sync::Arc::new(std::sync::Mutex::new(None)),
            autonomous: std::sync::Mutex::new(
                pa_core::autonomous::create_autonomous_runtime_state(None, None),
            ),
            link,
            children,
        })
    }

    /// Build the core session once (same once-only rule as `session_agent`).
    pub(crate) fn ensure_core_session(&self, model: &Model) -> anyhow::Result<()> {
        {
            let guard = self.session.blocking_lock();
            if guard.is_some() {
                return Ok(());
            }
        }
        let built = self
            .runtime
            .block_on(async { self.build_session(model).await })?;
        self.session.blocking_lock().replace(built);
        Ok(())
    }

    /// Execute one session slash command against the built session: resolve
    /// the model, build the core session on first use, then run the pa-core
    /// executor (durable rows, compaction, goal continuation).
    pub(crate) fn execute_session_command(
        &self,
        command: &pa_core::session_engine::slash_commands::SessionSlashCommand,
    ) -> anyhow::Result<SessionCommandExecution> {
        let model = self.resolve_model()?;
        self.ensure_core_session(&model)?;
        let api_key = self.resolve_request_api_key(&model);
        let mut autonomous = self.autonomous.lock().expect("autonomous state lock");
        let mut params = SessionCommandParams {
            model: &model,
            api_key,
            global_harness_dir: self.config.agent_dir.clone(),
            autonomous: &mut autonomous,
        };
        let guard = self.session.blocking_lock();
        let core = guard
            .as_ref()
            .expect("session built by ensure_core_session");
        Ok(self
            .runtime
            .block_on(async { execute_session_command(core, &mut params, command).await }))
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
        let sender = Arc::new(LinkAgentMessageController::new(
            Arc::clone(&self.link),
            config.active_session_id.clone(),
            config.worker_token.clone(),
            Arc::clone(&self.own_summary),
        ));
        let observer = Arc::new(LinkAgentObserveController::new(Arc::clone(&self.link)));
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
        // Children inherit the parent model selector; the engine resolves
        // the model here, after the create command set the rest of the
        // parent identity.
        if let Some(children) = &self.children {
            children.set_model(format!("{}/{}", model.provider, model.id));
        }
        pa_core::session_engine::engine::create_session(SessionEngineConfig {
            cwd: self.config.cwd.clone(),
            agent_dir: self.config.agent_dir.clone(),
            model: Some(agent_model),
            thinking_level: self.thinking_level(),
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
            rlm_subagent_host: self.children.clone().map(|children| {
                children as Arc<dyn pa_core::session_engine::rlm_host::RlmSubagentHost>
            }),
        })
        .await
    }

    /// The session's thinking level from the create command (validated at
    /// `configure_rlm_identity` time), when set.
    fn thinking_level(&self) -> Option<pa_agent::types::ThinkingLevel> {
        let name = self.thinking.read().expect("thinking lock").clone()?;
        match name.as_str() {
            "off" => Some(pa_agent::types::ThinkingLevel::Off),
            "minimal" => Some(pa_agent::types::ThinkingLevel::Minimal),
            "low" => Some(pa_agent::types::ThinkingLevel::Low),
            "medium" => Some(pa_agent::types::ThinkingLevel::Medium),
            "high" => Some(pa_agent::types::ThinkingLevel::High),
            "xhigh" => Some(pa_agent::types::ThinkingLevel::Xhigh),
            "max" => Some(pa_agent::types::ThinkingLevel::Max),
            _ => None,
        }
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

    /// The worker's live session summary (the TS
    /// `createAgentSessionMessageSender` source): rendered into the
    /// sender identity block of direct worker-to-worker deliveries.
    fn set_session_summary(&self, summary: Value) {
        *self
            .own_summary
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(summary);
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
            Ok(Ok(outcome)) => outcome,
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
        match compaction {
            pa_core::session_engine::compact_session::CompactOutcome::Skipped(message) => {
                CompactionOutcome::Skipped {
                    message: message.to_string(),
                }
            }
            pa_core::session_engine::compact_session::CompactOutcome::Ran(run) => {
                CompactionOutcome::Compacted {
                    run: CompactionRun {
                        // The wire result is the TS `CompactionResult` shape:
                        // summary, firstKeptEntryId, tokensBefore. Usage and
                        // file-op details live on the persisted entry.
                        result: json!({
                            "summary": run.result.summary,
                            "firstKeptEntryId": run.result.first_kept_entry_id,
                            "tokensBefore": run.result.tokens_before,
                        }),
                        usage: run
                            .result
                            .usage
                            .and_then(|usage| serde_json::to_value(usage).ok()),
                    },
                }
            }
        }
    }

    fn configure_rlm_identity(
        &self,
        identity: crate::engine::RlmSessionIdentity,
    ) -> anyhow::Result<()> {
        if let Some(thinking) = &identity.thinking {
            pa_ai::models::thinking_level_from_str(thinking)
                .ok_or_else(|| anyhow::anyhow!("unknown thinking level \"{thinking}\""))?;
        }
        *self.thinking.write().expect("thinking lock") = identity.thinking.clone();
        if let Some(children) = &self.children {
            let parent = ParentIdentity {
                rlm_depth: identity.rlm_depth,
                rlm_max_depth: identity.rlm_max_depth.unwrap_or(DEFAULT_RLM_MAX_DEPTH),
                model: None,
                cwd: identity.cwd.clone(),
                session_id: identity.session_id.clone(),
                session_file: identity.session_file.clone(),
                thinking: identity.thinking.clone(),
                child_script: None,
            };
            children.set_identity(parent);
        }
        Ok(())
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
        aborted: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) {
        // Session commands (compact/refine/goal/autonomous) never admit a
        // model turn and never record a user-message row: the durable echo
        // row replaces it. Execute before admission so the idle-wait loop
        // below stays reachable only for real turns.
        if let Some(command) =
            crate::session_commands::parse_prompt_session_command(&request.message)
        {
            let Some(execution) = crate::session_commands::run_session_command(self, command, emit)
            else {
                return;
            };
            if let Some(error) = &execution.error {
                emit(EngineEvent::Done(Err(error.clone())));
                return;
            }
            // A goal start/resume schedules its continuation context as
            // the turn; the durable goal-context row is already emitted.
            if let Some(continuation) = execution.continuation_prompt {
                self.run_model_turn(&continuation, aborted, emit);
            } else {
                emit(EngineEvent::Done(Ok(())));
            }
            return;
        }
        // The accepted user message is recorded by the worker.
        if !emit(EngineEvent::UserMessage(json!({
            "role": "user",
            "content": [{ "type": "text", "text": request.message }],
            "timestamp": now_millis(),
        }))) {
            return;
        }
        self.run_model_turn(&request.message, aborted, emit);
    }
}

impl AgentSessionEngine {
    /// Drive one admitted prompt through the retry-driver model loop and
    /// emit the turn outcome (provider-failure retries + final-row
    /// surfacing). The user row — or a goal continuation's durable context
    /// row — precedes this, so this starts at the model turn.
    fn run_model_turn(
        &self,
        prompt: &str,
        aborted: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) {
        let prompt = prompt.to_string();
        // Model resolution and session construction are hard failures: they
        // never reach the provider, so the retry loop does not apply (the
        // TS loop only classifies provider stream failures).
        let model = match self.resolve_model() {
            Ok(model) => model,
            Err(error) => {
                emit(EngineEvent::Done(Err(error.to_string())));
                return;
            }
        };
        let agent = match self.session_agent(&model) {
            Ok(agent) => agent,
            Err(error) => {
                emit(EngineEvent::Done(Err(format!("{error:#}"))));
                return;
            }
        };
        let policy = self.retry_policy();
        // The pa-core retry driver owns the attempt loop; this engine owns
        // one turn. The driver awaits each attempt to completion before
        // emitting retry events, so the single `emit` reference is handed
        // through a RefCell slot to whichever closure is currently running.
        let emit_cell = std::cell::RefCell::new(emit);
        let first_attempt = std::cell::Cell::new(true);
        let result = self.runtime.block_on(
            pa_core::session_engine::auto_retry::run_turn_with_auto_retry(
                &policy,
                None,
                || {
                    let mut emit = emit_cell.borrow_mut();
                    let first = first_attempt.get();
                    first_attempt.set(false);
                    let agent = agent.clone();
                    let prompt = prompt.clone();
                    let model = model.clone();
                    async move {
                        // A retry re-issues the failed turn: the failed
                        // assistant message leaves the loop context first
                        // (TS `messages.slice(0, -1)` keeps the retried
                        // request free of the error turn), then `continue`.
                        if !first {
                            drop_trailing_assistant(&agent).await;
                        }
                        match self
                            .run_turn_once(&agent, &prompt, first, &mut **emit)
                            .await
                        {
                            Ok(TurnOnce::Message { assistant, value }) => {
                                // The final message always reaches the
                                // transcript — the failure included: TS
                                // persists and renders it like any outcome.
                                if !emit(EngineEvent::AssistantMessage(value)) {
                                    return Ok(aborted_message(&model));
                                }
                                Ok(*assistant)
                            }
                            Ok(TurnOnce::None) => Err(anyhow::anyhow!("No response produced.")),
                            Ok(TurnOnce::Aborted) => Ok(aborted_message(&model)),
                            Err(error) => Err(error),
                        }
                    }
                },
                |event| {
                    let mut emit = emit_cell.borrow_mut();
                    async move {
                        let engine_event = retry_event_to_engine_event(event);
                        if !emit(engine_event) {
                            anyhow::bail!("emit cancelled");
                        }
                        Ok(())
                    }
                },
                |delay| {
                    async move {
                        // Abort-aware wait: the worker's cancel flag stops
                        // the retry sleep early (TS `_retryAbortController`).
                        let deadline = tokio::time::Instant::now() + delay;
                        loop {
                            if aborted() {
                                return false;
                            }
                            if tokio::time::Instant::now() >= deadline {
                                return true;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                    }
                },
            ),
        );
        let emit = emit_cell.into_inner();
        let done = match result {
            Ok(message) => match message.stop_reason {
                // The failure already reached the transcript as the final
                // assistant message; the turn error still travels to
                // headless callers through the turn result.
                StopReason::Error => Err(message
                    .error_message
                    .clone()
                    .filter(|error| !error.is_empty())
                    .unwrap_or_else(|| "Assistant response failed".to_string())),
                StopReason::Aborted => Err("No response produced.".to_string()),
                _ => Ok(()),
            },
            Err(error) => Err(error.to_string()),
        };
        emit(EngineEvent::Done(done));
    }

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

    /// The provider retry policy from settings (TS `providerRetryPolicy`).
    fn retry_policy(&self) -> pa_core::session_engine::provider_retry::ProviderRetryPolicy {
        pa_core::settings::SettingsManager::create(&self.config.cwd, &self.config.agent_dir)
            .get_provider_retry_policy()
    }

    /// Run one turn, streaming assistant updates through `emit` as they
    /// arrive. The first attempt prompts the session; retries continue the
    /// parked turn. Returns the turn outcome: the final assistant message
    /// (provider failures included), `None` when no assistant message was
    /// produced, or `Aborted` when the emit callback cancelled the run.
    async fn run_turn_once(
        &self,
        agent: &std::sync::Arc<pa_agent::agent::Agent>,
        prompt: &str,
        first_attempt: bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) -> anyhow::Result<TurnOnce> {
        // Stream assistant events while the turn runs. The turn starts
        // asynchronously after admission, so the idle watcher must not fire
        // before the run has begun.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EngineEvent>();
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let subscription = {
            let tx = tx.clone();
            let started_flag = started.clone();
            agent
                .subscribe(move |event, _signal| {
                    let tx = tx.clone();
                    let started_flag = started_flag.clone();
                    Box::pin(async move {
                        use pa_agent::types::AgentEvent;
                        if matches!(event, AgentEvent::AgentStart) {
                            started_flag.store(true, Ordering::SeqCst);
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
        };
        // Admit the turn: the first attempt prompts the session; a retry
        // continues the parked conversation.
        let admitted: anyhow::Result<()> = if first_attempt {
            let guard = self.session.lock().await;
            let engine = guard.as_ref().expect("session built");
            engine
                .session
                .prompt(prompt, Default::default())
                .await
                .map(|_| ())
        } else {
            agent.continue_run().await.map(|_| ())
        };
        if let Err(error) = admitted {
            let _ = subscription.unsubscribe().await;
            return Err(anyhow::anyhow!("{error:#}"));
        }
        // Wait for the turn to settle, forwarding events into `emit` live.
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();
        let idle_agent = agent.clone();
        let started_flag = started.clone();
        self.runtime.spawn(async move {
            loop {
                idle_agent.wait_for_idle().await;
                if started_flag.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            let _ = done_tx.send(());
        });
        let mut aborted = false;
        loop {
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Some(event) => {
                            if !emit(event) {
                                aborted = true;
                            }
                        }
                        None => break,
                    }
                }
                _ = &mut done_rx => {
                    // Idle: drain any events that raced the signal, then settle.
                    while let Ok(event) = rx.try_recv() {
                        if !emit(event) {
                            aborted = true;
                            break;
                        }
                    }
                    break;
                }
            }
            if aborted {
                break;
            }
        }
        let _ = subscription.unsubscribe().await;
        if aborted {
            return Ok(TurnOnce::Aborted);
        }
        // The final assistant message decides the outcome (provider
        // failures included: the retry driver classifies them).
        let state = agent.state().await;
        for message in state.messages.iter().rev() {
            if let pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(
                assistant,
            )) = message
            {
                let Some(ai_message) =
                    json_round_trip::<_, pa_types::ai::AssistantMessage>(assistant)
                else {
                    return Ok(TurnOnce::None);
                };
                let session_message = pa_types::session::AgentMessage::Assistant(ai_message);
                let Ok(value) = serde_json::to_value(&session_message) else {
                    return Ok(TurnOnce::None);
                };
                return Ok(TurnOnce::Message {
                    assistant: Box::new(assistant.clone()),
                    value,
                });
            }
        }
        Ok(TurnOnce::None)
    }
}

/// The outcome of one turn attempt.
enum TurnOnce {
    /// The emit callback cancelled the run.
    Aborted,
    /// The turn produced no assistant message.
    None,
    /// The turn's final assistant message: the typed message (retry
    /// classification) plus its session wire value (transcript + store).
    Message {
        assistant: Box<pa_agent::types::AssistantMessage>,
        value: Value,
    },
}

/// Remove the trailing assistant message from the loop context (TS retry:
/// `messages.slice(0, -1)`), so a retried request does not re-send the
/// failed turn's error message.
async fn drop_trailing_assistant(agent: &std::sync::Arc<pa_agent::agent::Agent>) {
    let state = agent.state().await;
    let mut messages = state.messages;
    if matches!(
        messages.last(),
        Some(pa_agent::types::AgentMessage::Standard(
            pa_agent::types::Message::Assistant(_)
        ))
    ) {
        messages.pop();
        agent.set_messages(messages).await;
    }
}

/// The synthesized aborted assistant message (an abort racing the turn ends
/// the loop without a provider failure).
fn aborted_message(model: &Model) -> pa_agent::types::AssistantMessage {
    pa_agent::types::AssistantMessage {
        content: vec![pa_agent::types::AssistantContent::Text(
            pa_agent::types::TextContent {
                text: String::new(),
                text_signature: None,
            },
        )],
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: pa_agent::types::Usage::zero(),
        stop_reason: StopReason::Aborted,
        stop_reason_raw: None,
        error_message: None,
        timestamp: pa_agent::now_ms(),
    }
}

/// Translate one retry-loop event to the engine event vocabulary.
fn retry_event_to_engine_event(
    event: pa_core::session_engine::auto_retry::AutoRetryEvent,
) -> EngineEvent {
    use pa_core::session_engine::auto_retry::AutoRetryEvent;
    match event {
        AutoRetryEvent::Start {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
        } => EngineEvent::AutoRetryStart {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
        },
        AutoRetryEvent::End {
            success,
            attempt,
            final_error,
        } => EngineEvent::AutoRetryEnd {
            success,
            attempt,
            final_error,
        },
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
            &|| false,
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
        &|| false,
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
