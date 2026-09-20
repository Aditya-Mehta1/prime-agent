//! The real agent-session engine for daemon workers: a pa-core session over
//! the shared provider adapter, driven through the daemon's `SessionEngine`
//! contract. Replaces the scripted faux engine when a model is configured.
//!
//! Streaming note: assistant updates are forwarded to the worker's emit
//! callback as they arrive (one per provider stream event) while the turn
//! runs — never buffered until the turn settles — matching the TS daemon's
//! `void prompt(...)` live-broadcast behavior. The worker coalesces them
//! for broadcast (see `worker::run_turn`).

use std::sync::Arc;

use serde_json::{json, Value};

use crate::agent_messaging::{LinkAgentMessageController, LinkAgentObserveController};
use crate::overflow_compaction::{OverflowArmRun, OverflowRecovery};
use pa_agent::types::StopReason;
use pa_core::autonomous::AutonomousFollowUp;
use pa_core::kernel::shared::HostRequestHandlers;
use pa_core::session_engine::agent_messaging::{
    register_agent_message_host_handlers, register_agent_observe_host_handlers,
};
use pa_core::session_engine::engine::{SessionEngine as CoreSessionEngine, SessionEngineConfig};
use pa_core::session_engine::provider_adapter::{
    json_round_trip, map_thinking_level, switchable_stream_fn, ProviderTarget,
};
use pa_core::session_engine::session_commands::{
    execute_session_command, SessionCommandExecution, SessionCommandParams,
};
use pa_types::ai::Model;

use crate::auto_compaction::AutoCompactionRun;
use crate::engine::{
    BranchSummaryOutcome, BranchSummaryRequest, BranchSummaryRun, CompactionOutcome,
    CompactionRequest, CompactionRun, EngineEvent, EngineModelSelection, PromptRequest,
    SessionEngine, SideQuestionOutcome, SideQuestionRequest,
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
    /// Requested thinking level from the process-level fallback. The
    /// session's create command (`--thinking`) overrides it via
    /// [`SessionEngine::configure_model`].
    pub thinking: Option<pa_types::ai::ModelThinkingLevel>,
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
    /// Telemetry opt-out from the create command (Some(true) installs no
    /// telemetry; None/Some(false) resolve the configured sinks).
    pub telemetry_disabled: Option<bool>,
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

/// The goal driver and session-manager handles mirrored from the core
/// session (see `AgentSessionEngine::goal_runtime`).
#[derive(Clone)]
struct GoalRuntimeHandles {
    driver: std::sync::Arc<tokio::sync::Mutex<pa_core::session_engine::goal_driver::GoalDriver>>,
    session: std::sync::Arc<tokio::sync::Mutex<pa_core::session::manager::SessionManager>>,
}

/// A [`SessionEngine`] running real agent turns.
pub struct AgentSessionEngine {
    pub(crate) runtime: tokio::runtime::Runtime,
    pub(crate) config: AgentEngineConfig,
    /// The session-scoped ACP MCP store (TS `session._mcpManager`): shared
    /// with the core engine's prompt gating, so admitted servers are one
    /// store for admission and execution.
    pub(crate) mcp: std::sync::Arc<std::sync::Mutex<pa_core::mcp::McpManager>>,
    /// The last goal state emitted as a `goal_update` event: the TS session
    /// emits on state change, so unchanged states (e.g. `/goal status`)
    /// stay silent.
    published_goal: std::sync::Mutex<Option<pa_core::goals::GoalState>>,
    /// The session's goal driver and session-manager handles, mirrored from
    /// the core session at build time: the core session's own mutex is held
    /// across a turn's admission, so goal checks inside emit callbacks
    /// (which may run in async context) must not lock it.
    goal_runtime: std::sync::Mutex<Option<GoalRuntimeHandles>>,
    /// The worker-owned session file (conversation-log path), set at create.
    session_file: std::sync::Mutex<Option<std::path::PathBuf>>,
    /// The authoritative model selection. Starts from the process fallback
    /// (create config or worker env) and is re-bound when a session's create
    /// command carries explicit wire flags.
    selection: std::sync::RwLock<EngineModelSelection>,
    /// The session's resolved effective thinking level, computed once when
    /// the create command adopts the selection and reused afterwards.
    /// Resolved at create time (before any turn) so summary/state polls
    /// during a live turn stay side-effect-free.
    effective_thinking: std::sync::RwLock<Option<pa_types::ai::ModelThinkingLevel>>,
    /// Built once on the first prompt, reused across prompts.
    pub(crate) session: tokio::sync::Mutex<Option<CoreSessionEngine>>,
    /// A branch move (tree navigation or fork) that landed before the first
    /// turn built the session: consumed at build so the session starts on
    /// the moved branch (TS rebuilds context from the durable branch).
    pending_branch: std::sync::Mutex<Option<Vec<pa_types::session::FileEntry>>>,
    /// The provider target the built session's stream reads per call
    /// (api key + model), set when the session builds: `set_model` swaps
    /// the slot so the live session follows the new model without a
    /// rebuild.
    provider_target: std::sync::Arc<
        std::sync::RwLock<Option<pa_core::session_engine::provider_adapter::ProviderTarget>>,
    >,
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
    /// Shared with the agent-loop subscription so per-message accounting can
    /// run on every settled assistant message.
    pub(crate) autonomous:
        std::sync::Arc<tokio::sync::Mutex<pa_core::autonomous::AutonomousRuntimeState>>,
    /// The autonomous continuation policy the turn loop consults after
    /// every settled turn. Product default: the shell-gate driver in the
    /// session cwd; deterministic harnesses replace it through
    /// [`AgentSessionEngine::set_autonomous_driver`].
    autonomous_driver: std::sync::RwLock<std::sync::Arc<dyn pa_core::autonomous::AutonomousDriver>>,
    /// This session's RLM recursion depth (0 for top-level sessions),
    /// stamped by `configure_rlm_identity`. Gates the kernel `refine.*`
    /// host requests (TS `_autoRefineAllowedForSession` depth check).
    rlm_depth: std::sync::atomic::AtomicU32,
    /// The RLM depth bound's TS source stamp (`default` | `env` | `global`
    /// | `inherited` | `chat`), seeded by `configure_rlm_identity` (the
    /// TS `_resolveRlmMaxDepth` precedence) and flipped to `chat` by a
    /// `set_rlm_max_depth` override.
    rlm_max_depth_source: std::sync::Mutex<&'static str>,
    /// A `set_rlm_max_depth` that landed before the first turn built the
    /// session: the durable `rlm_max_depth_state` custom entry parks here
    /// and flushes at build, exactly the `pending_branch` pattern.
    pending_max_depth: std::sync::Mutex<Option<u64>>,
    /// The resolved faux model, registered once per engine so scripted
    /// responses queue across turns instead of replaying per resolution.
    /// Verification harness only; never set by the product.
    faux_model: std::sync::OnceLock<Model>,
    /// One compact-and-retry attempt per context overflow (TS
    /// `_overflowRecovery`): the state machine the overflow arm walks.
    pub(crate) overflow_recovery: std::sync::Mutex<OverflowRecovery>,
}

impl AgentSessionEngine {
    pub fn new(config: AgentEngineConfig) -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let session_file = std::sync::Mutex::new(config.session_file.clone());
        // Process-level fallback: the create config, else the worker env
        // pair. A create command with explicit wire flags overrides both.
        let thinking = config.thinking;
        let selection = if config.provider.is_some() || config.model.is_some() {
            EngineModelSelection {
                provider: config.provider.clone(),
                model: config.model.clone(),
                api_key: config.api_key.clone(),
                thinking,
            }
        } else {
            EngineModelSelection {
                provider: std::env::var("PRIME_AGENT_MODEL_PROVIDER").ok(),
                model: std::env::var("PRIME_AGENT_MODEL").ok(),
                api_key: None,
                thinking,
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
        let autonomous_driver = std::sync::RwLock::new(std::sync::Arc::new(
            pa_core::autonomous::ShellAutonomousDriver::new(config.cwd.clone()),
        )
            as std::sync::Arc<dyn pa_core::autonomous::AutonomousDriver>);
        // The ACP MCP store (auth storage construction is blocking; the
        // engine construction paths are already off the hot async paths).
        let agent_dir = config.agent_dir.clone();
        // Settings-declared user servers feed the store this worker owns
        // (TS `session._mcpManager` resolves user settings; the
        // `mcp.config` host request answers from them). Read per resolve
        // so `mcp.refresh` - which re-resolves integrations - sees
        // settings changes, mirroring the in-process engine's
        // `mcp_gating` extraction (agentDir + project settings.json).
        let mcp_cwd = config.cwd.clone();
        let mcp_agent_dir = agent_dir.clone();
        let mcp = pa_core::mcp::McpManager::new(pa_core::mcp::McpManagerOptions {
            auth_storage: pa_core::auth::AuthStorage::create_with_oauth(
                &agent_dir,
                std::sync::Arc::new(pa_core::mcp::McpOAuth::new()),
            ),
            get_user_servers: Box::new(move || {
                let settings = pa_core::settings::SettingsManager::create(&mcp_cwd, &mcp_agent_dir);
                Some(
                    settings
                        .settings()
                        .mcp_servers
                        .clone()
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|(server, server_config)| {
                            serde_json::from_value(server_config)
                                .ok()
                                .map(|parsed| (server, parsed))
                        })
                        .collect::<std::collections::HashMap<
                            String,
                            pa_core::mcp::McpServerConfig,
                        >>(),
                )
            }),
            begin_login: None,
        });
        // The kernel's `mcp.begin_login` host request: the worker runs the
        // OAuth login (browser + local callback) and persists the
        // endpoint-bound credential the shared auth store gates on. Wired
        // before any session registers host handlers, so every session the
        // worker builds exposes it.
        let mcp = std::sync::Arc::new(std::sync::Mutex::new(mcp));
        crate::mcp_login::wire_worker_mcp_login(
            &mcp,
            std::sync::Arc::new(crate::mcp_login::WorkerMcpLoginUi::from_env()),
            std::sync::Arc::new(pa_core::mcp::ReqwestOAuthHttp::new()),
        );
        Ok(Self {
            runtime,
            config,
            mcp,
            published_goal: std::sync::Mutex::new(None),
            goal_runtime: std::sync::Mutex::new(None),
            session_file,
            selection: std::sync::RwLock::new(selection),
            effective_thinking: std::sync::RwLock::new(None),
            session: tokio::sync::Mutex::new(None),
            pending_branch: std::sync::Mutex::new(None),
            provider_target: std::sync::Arc::new(std::sync::RwLock::new(None)),
            own_summary: std::sync::Arc::new(std::sync::Mutex::new(None)),
            autonomous: std::sync::Arc::new(tokio::sync::Mutex::new(
                pa_core::autonomous::create_autonomous_runtime_state(None, None),
            )),
            link,
            children,
            autonomous_driver,
            rlm_depth: std::sync::atomic::AtomicU32::new(0),
            rlm_max_depth_source: std::sync::Mutex::new("default"),
            pending_max_depth: std::sync::Mutex::new(None),
            faux_model: std::sync::OnceLock::new(),
            overflow_recovery: std::sync::Mutex::new(OverflowRecovery::default()),
        })
    }

    /// Replace the autonomous continuation policy. Deterministic eval
    /// harnesses inject a scripted driver here; the product keeps the
    /// default shell-gate driver in the session cwd. Call before the
    /// first admitted turn.
    pub fn set_autonomous_driver(
        &self,
        driver: std::sync::Arc<dyn pa_core::autonomous::AutonomousDriver>,
    ) {
        *self
            .autonomous_driver
            .write()
            .expect("autonomous driver lock") = driver;
    }

    /// The async build of the core session (the same funnel as
    /// `ensure_core_session`, awaited on the caller's runtime instead of
    /// parked on the engine's own): read seams (`get_system_prompt`)
    /// reaching an unbuilt session build it here.
    pub(crate) async fn ensure_core_session_async(&self, model: &Model) -> anyhow::Result<()> {
        {
            let guard = self.session.lock().await;
            if guard.is_some() {
                return Ok(());
            }
        }
        let built = self.build_session(model).await?;
        self.mirror_goal_runtime(&built);
        self.session.lock().await.replace(built);
        Ok(())
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
        self.mirror_goal_runtime(&built);
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
        let mut autonomous = self.autonomous.blocking_lock();
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
        let mut registry =
            pa_core::models::ModelRegistry::create(auth, self.config.agent_dir.join("models.json"));
        // A fresh registry gates private Prime Inference models out until the
        // async authorization refresh runs; adopt the on-disk authorization
        // cache so create-time resolution can pick the session's private
        // model (e.g. internal/glm-5.3-fast).
        registry.load_private_authorization_from_cache();
        let available: Vec<Model> = registry.get_available().into_iter().cloned().collect();
        let selection = self.current_selection();
        let Some(model_name) = selection.model.as_deref() else {
            // No flagged model: the TS `createAgentSession` startup chain —
            // the saved settings default, then the featured default, then
            // the first available model.
            let all: Vec<Model> = registry.get_all().to_vec();
            let settings = pa_core::settings::SettingsManager::create(
                &self.config.cwd,
                &self.config.agent_dir,
            );
            let startup =
                pa_core::models::find_initial_model(&pa_core::models::InitialModelOptions {
                    cli_provider: None,
                    cli_model: None,
                    scoped_models: &[],
                    is_continuing: false,
                    default_provider: settings.get_default_provider(),
                    default_model_id: settings.get_default_model(),
                    all_models: &all,
                    available_models: &available,
                });
            if let Some(model) = startup {
                return Ok(model);
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
    /// print runtime) drives the engine without the network. The provider
    /// registers once per engine: its queued responses then span the whole
    /// session (multi-turn scripts), instead of replaying from the top on
    /// every model resolution.
    pub(crate) fn resolve_model(&self) -> anyhow::Result<Model> {
        if let Some(script) = &self.config.faux_script {
            if let Some(model) = self.faux_model.get() {
                return Ok(model.clone());
            }
            let model = faux_model_from_script(script)?;
            let _ = self.faux_model.set(model.clone());
            return Ok(model);
        }
        self.resolve_registry_model()
    }

    /// The effective session thinking level (the sdk.ts `createAgentSession`
    /// order): the create-config flag, then the settings default, then
    /// "medium" — always clamped to what the model supports; a model that
    /// cannot be resolved degrades to "off". Resolved once at create time
    /// and cached so summary/state calls stay side-effect-free while turns
    /// run.
    fn effective_thinking(&self) -> pa_types::ai::ModelThinkingLevel {
        if let Some(level) = *self
            .effective_thinking
            .read()
            .expect("effective thinking lock")
        {
            return level;
        }
        let requested = self
            .current_selection()
            .thinking
            .or_else(|| {
                let settings = pa_core::settings::SettingsManager::create(
                    &self.config.cwd,
                    &self.config.agent_dir,
                );
                settings
                    .get_default_thinking_level()
                    .map(pa_core::settings::ThinkingLevelSetting::model_level)
            })
            // TS `DEFAULT_THINKING_LEVEL`.
            .unwrap_or(pa_types::ai::ModelThinkingLevel::Medium);
        let resolved = match self.resolve_model() {
            Ok(model) => pa_ai::models::clamp_thinking_level(&model, requested),
            Err(_) => pa_types::ai::ModelThinkingLevel::Off,
        };
        *self
            .effective_thinking
            .write()
            .expect("effective thinking lock") = Some(resolved);
        resolved
    }

    /// Resolve the request API key for `model`: the create-config key (the
    /// TS `setRuntimeApiKey` path), else the registry's auth resolution
    /// (auth storage, then the models.json provider `apiKey` — the same
    /// sources `getApiKeyAndHeaders` merges in the TS product).
    pub(crate) fn resolve_request_api_key(&self, model: &Model) -> Option<String> {
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
            self.children.clone(),
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

        // The session's stream reads its target from the engine's live slot:
        // `set_model` swaps the slot so the built session follows without a
        // rebuild.
        let stream_fn = switchable_stream_fn(std::sync::Arc::clone(&self.provider_target));
        {
            let mut target = self.provider_target.write().expect("provider target lock");
            *target = Some(ProviderTarget {
                api_key: self.resolve_request_api_key(model),
                model: model.clone(),
            });
        }
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
        // Session telemetry: the composition root is this worker process;
        // the create command's opt-out rides the engine config (TS main.ts
        // `telemetryDisabled` on the runtime config). Sinks resolve from
        // settings + env inside `build_client`.
        let telemetry = (self.config.telemetry_disabled != Some(true)).then(|| {
            let settings = pa_core::settings::SettingsManager::create(
                &self.config.cwd,
                &self.config.agent_dir,
            );
            pa_core::session_engine::telemetry::TelemetryWiring {
                client: pa_core::session_engine::telemetry::build_client(
                    &settings,
                    &self.config.agent_dir,
                ),
                execution_mode: Some("daemon".to_string()),
                now: None,
            }
        });
        pa_core::session_engine::engine::create_session(SessionEngineConfig {
            telemetry,
            cwd: self.config.cwd.clone(),
            agent_dir: self.config.agent_dir.clone(),
            mcp_manager: Some(std::sync::Arc::clone(&self.mcp)),
            model: Some(agent_model),
            thinking_level: Some(map_thinking_level(self.effective_thinking())),
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
            rlm_depth: Some(self.rlm_depth.load(std::sync::atomic::Ordering::Relaxed)),
            model_info: Some(model.clone()),
            // The daemon worker has no CLI extension sources: sessions
            // load configured/discovered extensions only (the attached
            // TUI/ACP surfaces do not carry `-e` flags today).
            cli_extension_sources: vec![],
            extension_tool_allow_list: None,
        })
        .await
    }
}

/// The last persisted `rlm_max_depth_state` custom entry in a session
/// file (TS `_loadPersistedRlmMaxDepthState`): the chat override a
/// resumed session re-seeds its depth bound from. `None` when the file
/// carries no override (or cannot be read - an unreadable file keeps the
/// create-carried bound, exactly the TS fallthrough).
pub(crate) fn persisted_rlm_max_depth(path: Option<&str>) -> Option<u64> {
    let path = std::path::Path::new(path?);
    let content = std::fs::read_to_string(path).ok()?;
    crate::session_store::parse_session_entries(&content)
        .iter()
        .rev()
        .find_map(|entry| {
            (entry.get("type").and_then(Value::as_str) == Some("custom")
                && entry.get("customType").and_then(Value::as_str) == Some("rlm_max_depth_state"))
            .then(|| {
                entry
                    .get("data")
                    .and_then(|data| data.get("maxDepth"))
                    .and_then(Value::as_u64)
            })
            .flatten()
        })
}

/// One artifact reference (TS `createArtifactReference` in
/// modes/agent-connection/snapshot.ts): the sha256-derived id, the owning
/// session, the artifact type, and the logical path (cwd-relative when the
/// file lives under the cwd, else the basename).
fn artifact_reference(
    session_id: &str,
    cwd: &str,
    artifact_type: &str,
    file_path: &str,
) -> Option<Value> {
    if file_path.is_empty() {
        return None;
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::new()
        .chain_update(format!("{session_id}\0{artifact_type}\0{file_path}"))
        .finalize();
    let id = format!("artifact_{}", hex_prefix(&digest, 16));
    let mut reference = json!({
        "id": id,
        "sessionId": session_id,
        "type": artifact_type,
        "logicalPath": logical_artifact_path(cwd, file_path),
    });
    let logical = reference["logicalPath"].as_str().unwrap_or_default();
    let resolved_cwd = std::path::Path::new(cwd);
    let resolved_path = std::path::Path::new(file_path);
    if let (Ok(relative), true) = (
        resolved_path.strip_prefix(resolved_cwd),
        logical.chars().next().is_some_and(|c| c != '.' && c != '/'),
    ) {
        reference["relativePath"] = json!(relative.to_string_lossy().replace('\\', "/"));
    }
    Some(reference)
}

/// The first `len` hex characters of a digest.
fn hex_prefix(digest: &[u8], len: usize) -> String {
    digest
        .iter()
        .flat_map(|byte| [format!("{:02x}", byte >> 4), format!("{:02x}", byte & 0x0f)])
        .collect::<String>()
        .chars()
        .take(len)
        .collect()
}

/// TS `createArtifactPathInfo`: synthetic paths (`<...>`) stay as-is; a
/// path under the cwd keeps its cwd-relative form; anything else degrades
/// to the basename.
fn logical_artifact_path(cwd: &str, file_path: &str) -> String {
    if file_path.starts_with('<') && file_path.ends_with('>') {
        return file_path.to_string();
    }
    let resolved_cwd = std::path::Path::new(cwd);
    let resolved_path = std::path::Path::new(file_path);
    if let Ok(relative) = resolved_path.strip_prefix(resolved_cwd) {
        let relative = relative.to_string_lossy().replace('\\', "/");
        if !relative.is_empty() && !relative.starts_with("..") && !relative.starts_with('/') {
            return relative;
        }
    }
    std::path::Path::new(file_path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "artifact".to_string())
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

impl AgentSessionEngine {
    /// Write the durable `rlm_max_depth_state` custom entry (TS
    /// `RLM_MAX_DEPTH_STATE_CUSTOM_TYPE`): straight into the built
    /// session's persistence handle, or parked for the build when the
    /// first turn has not built the session yet.
    fn persist_max_depth_state(&self, max_depth: u64) {
        let handles = self.goal_runtime.lock().expect("goal runtime lock").clone();
        match handles {
            Some(handles) => {
                let mut manager = self
                    .runtime
                    .block_on(async { handles.session.lock().await });
                manager.append_custom_entry(
                    "rlm_max_depth_state",
                    Some(json!({ "maxDepth": max_depth })),
                );
            }
            None => {
                *self
                    .pending_max_depth
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(max_depth);
            }
        }
    }

    /// The global settings write behind `set_rlm_max_depth { global: true }`
    /// (TS `settingsManager.setRlmMaxDepth` + flush + `drainErrors`):
    /// `Some(message)` when the write failed, mirroring the TS
    /// `globalError` field.
    fn write_global_rlm_max_depth(&self, max_depth: u64) -> Option<String> {
        let mut settings =
            pa_core::settings::SettingsManager::create(&self.config.cwd, &self.config.agent_dir);
        match settings.set_rlm_max_depth(max_depth) {
            Ok(()) => None,
            Err(error) => Some(error.to_string()),
        }
    }

    /// Flush a parked `rlm_max_depth_state` entry once the session built
    /// (the `pending_branch` pattern's build-site twin).
    fn flush_pending_max_depth(&self, manager: &mut pa_core::session::manager::SessionManager) {
        let pending = self
            .pending_max_depth
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(max_depth) = pending {
            manager.append_custom_entry(
                "rlm_max_depth_state",
                Some(json!({ "maxDepth": max_depth })),
            );
        }
    }

    /// Mirror the built session's goal handles: the core session's own
    /// mutex stays held across a turn's admission, so goal checks in emit
    /// callbacks read the mirror instead of the session.
    fn mirror_goal_runtime(&self, core: &CoreSessionEngine) {
        *self.goal_runtime.lock().expect("goal runtime lock") = Some(GoalRuntimeHandles {
            driver: core.goal_driver.clone(),
            session: core.session.shared_persistence(),
        });
    }

    /// The current goal state for a wire emission, when the driver is free
    /// to read (an in-flight host request holds it only for its own
    /// critical section; the next emitted event re-checks).
    fn current_goal_state(&self) -> Option<pa_core::goals::GoalState> {
        let handles = self
            .goal_runtime
            .lock()
            .expect("goal runtime lock")
            .clone()?;
        let driver = handles.driver.try_lock().ok()?;
        Some(driver.state().clone())
    }

    /// Emit the `goal_update` engine event when the session's goal state
    /// changed since the last emission (per-session dedupe: the TS session
    /// listener fires on state change). Returns the emit callback's verdict.
    /// A session without a goal seeds the baseline silently instead of
    /// emitting an idle-state event TS never sends.
    pub(crate) fn goal_update_if_changed(&self, emit: &mut dyn FnMut(EngineEvent) -> bool) -> bool {
        let Some(goal) = self.current_goal_state() else {
            // No session yet, or the driver is mid-mutation: a later event
            // re-checks before the turn settles.
            return true;
        };
        {
            let mut published = self.published_goal.lock().expect("published goal lock");
            if published.as_ref() == Some(&goal) {
                return true;
            }
            let baseline_only =
                published.is_none() && goal.status == pa_core::goals::GoalStatus::Idle;
            *published = Some(goal.clone());
            if baseline_only {
                return true;
            }
        }
        emit(EngineEvent::GoalUpdate {
            goal: serde_json::to_value(&goal).unwrap_or(Value::Null),
        })
    }

    /// Wrap one prompt's emit callback so every forwarded event is followed
    /// by a goal-change check: kernel `goal.complete`/`goal.create` host
    /// requests and session-command mutations surface as `goal_update` at
    /// the moment they happen (TS emits from `_setGoalState`), so the
    /// announcement row lands between the surrounding rows — after the
    /// echo/tool card, before the result/reply — not after the turn.
    pub(crate) fn goal_tracking_emit<'a>(
        &'a self,
        emit: &'a mut dyn FnMut(EngineEvent) -> bool,
    ) -> impl FnMut(EngineEvent) -> bool + 'a {
        move |event: EngineEvent| {
            if !emit(event) {
                return false;
            }
            self.goal_update_if_changed(emit)
        }
    }
}

impl SessionEngine for AgentSessionEngine {
    fn goal_state_value(&self) -> Value {
        if let Some(goal) = self.current_goal_state() {
            return serde_json::to_value(&goal).unwrap_or(Value::Null);
        }
        // The driver is mid-mutation or the session is not built yet (goal
        // rehydration surfaces with the first prompt/command): fall back to
        // the last published state, then the empty state.
        let published = self.published_goal.lock().expect("published goal lock");
        published
            .as_ref()
            .and_then(|goal| serde_json::to_value(goal).ok())
            .or_else(|| serde_json::to_value(pa_core::goals::empty_goal_state()).ok())
            .unwrap_or(Value::Null)
    }

    fn autonomous_status(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Option<pa_core::autonomous::AgentAutonomousStatus>>
                + Send
                + '_,
        >,
    > {
        // The turn loop's accounting holds the state lock across awaits
        // (gate evaluation), so the snapshot takes the async lock; the
        // caller waits for the session to settle first
        // (wait_for_headless_completion waits for idle).
        let autonomous = std::sync::Arc::clone(&self.autonomous);
        Box::pin(async move {
            let state = autonomous.lock().await;
            Some(pa_core::autonomous::autonomous_status(&state))
        })
    }

    /// Finalize telemetry on the live core session: `agent session ended`
    /// plus one flush (TS dispose callback). Best-effort by contract: a
    /// failed end never blocks or fails shutdown.
    fn end_telemetry(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let session = self.session.lock().await;
            let Some(engine) = session.as_ref() else {
                return;
            };
            let Some(telemetry) = &engine.telemetry else {
                return;
            };
            let _ = telemetry.end().await;
        })
    }

    /// The daemon `kill` path: report `session archived` (lifetime in ms),
    /// then finalize with `agent session ended` + flush. Best-effort like
    /// all telemetry; `SessionTelemetry::end` is idempotent, so a later
    /// worker shutdown stays a no-op for a killed session.
    fn archive_session_telemetry(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let session = self.session.lock().await;
            let Some(engine) = session.as_ref() else {
                return;
            };
            let Some(telemetry) = &engine.telemetry else {
                return;
            };
            telemetry.note_archived();
            let _ = telemetry.end().await;
        })
    }

    fn acp_mcp_manager(
        &self,
    ) -> Option<std::sync::Arc<std::sync::Mutex<pa_core::mcp::McpManager>>> {
        Some(std::sync::Arc::clone(&self.mcp))
    }

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
        {
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
            if selection.thinking.is_some() {
                current.thinking = selection.thinking;
            }
        }
        // Resolve the effective thinking level now (create time, before any
        // turn): the merge above may have changed the selection, so drop the
        // cached value and recompute. `effective_thinking` caches it, so
        // later summary/state calls stay side-effect-free while turns run.
        *self
            .effective_thinking
            .write()
            .expect("effective thinking lock") = None;
        let _ = self.effective_thinking();
        // The first prompt after create builds the session against this
        // selection, so no invalidation is needed here: configure runs at
        // create time, before any turn.
    }

    fn switch_model(&self, selection: EngineModelSelection) -> bool {
        self.configure_model(selection);
        let Ok(model) = self.resolve_model() else {
            return false;
        };
        // The built session follows the new model without a rebuild: the
        // agent's model (loop context) and the provider stream's target
        // swap in place (TS `agent.state.model = model`).
        {
            let mut target = self.provider_target.write().expect("provider target lock");
            *target = Some(ProviderTarget {
                api_key: self.resolve_request_api_key(&model),
                model: model.clone(),
            });
        }
        let session = self.session.blocking_lock();
        if let Some(core) = session.as_ref() {
            let provider = model.provider.clone();
            let model_id = model.id.clone();
            let _ = self
                .runtime
                .block_on(core.session.set_model(&model, &provider, &model_id));
        }
        true
    }

    fn supported_thinking_levels(&self) -> Option<Vec<String>> {
        let model = self.resolve_model().ok()?;
        Some(
            pa_ai::models::get_supported_thinking_levels(&model)
                .into_iter()
                .map(|level| level.wire_name().to_string())
                .collect(),
        )
    }

    fn switch_thinking_level(&self, level: pa_types::ai::ModelThinkingLevel) -> bool {
        self.configure_model(EngineModelSelection {
            thinking: Some(level),
            ..Default::default()
        });
        // The effective level is the request clamped to the model's
        // supported levels (TS `setThinkingLevel`); a built session's
        // agent follows it on the next turn.
        let effective = self.effective_thinking();
        let session = self.session.blocking_lock();
        if let Some(core) = session.as_ref() {
            let _ = self.runtime.block_on(
                core.session
                    .set_thinking_level(map_thinking_level(effective)),
            );
        }
        true
    }

    fn effective_thinking_level(&self) -> Option<String> {
        Some(self.effective_thinking().wire_name().to_string())
    }

    /// The built core session's assembled prompt (the export embeds it).
    /// Best-effort: the caller's `export_tools` read (which builds an
    /// absent session, the TS create-time state) runs first; a still
    /// unbuilt or busy session omits the section.
    fn export_system_prompt(&self) -> Option<String> {
        let session = self.session.try_lock().ok()?;
        session.as_ref().map(|core| core.system_prompt.clone())
    }

    /// The built session's live tool registry mapped to the export's tools
    /// section (TS `state.tools`). An export that precedes the first turn
    /// builds the session now (the TS state exists from create); a
    /// mid-turn engine reports `None` and the export omits the section.
    fn export_tools(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<Value>>> + Send + '_>> {
        Box::pin(async move {
            let model = self.resolve_model().ok()?;
            self.ensure_core_session_async(&model).await.ok()?;
            let session = self.session.try_lock().ok()?;
            let state = session.as_ref()?.session.agent().state().await;
            Some(pa_core::export_html::tools_section(&state.tools))
        })
    }

    /// The export's custom-tool pre-render: walk the entries through the
    /// registry-backed renderer (TS `preRenderCustomTools`), against the
    /// same built-session registry as [`Self::export_tools`].
    fn export_rendered_tools(
        &self,
        entries: &[Value],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Value>> + Send + '_>> {
        let entries = entries.to_vec();
        Box::pin(async move {
            let model = self.resolve_model().ok()?;
            self.ensure_core_session_async(&model).await.ok()?;
            let session = self.session.try_lock().ok()?;
            let state = session.as_ref()?.session.agent().state().await;
            let renderer = crate::session_export::ExportToolRenderer {
                tools: &state.tools,
            };
            pa_core::export_html::pre_render_custom_tools(&entries, &renderer)
        })
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

    fn run_branch_summary(
        &self,
        request: BranchSummaryRequest,
        signal: &pa_agent::abort::AbortSignal,
    ) -> BranchSummaryOutcome {
        let model = match self.resolve_model() {
            Ok(model) => model,
            Err(error) => {
                return BranchSummaryOutcome::Failed {
                    error: error.to_string(),
                }
            }
        };
        let api_key = self.resolve_request_api_key(&model);
        let settings =
            pa_core::settings::SettingsManager::create(&self.config.cwd, &self.config.agent_dir);
        let reserve_tokens = settings
            .settings()
            .branch_summary
            .as_ref()
            .and_then(|branch_summary| branch_summary.reserve_tokens)
            .unwrap_or(
                pa_core::session_engine::branch_summarization::DEFAULT_BRANCH_RESERVE_TOKENS,
            );
        let entries = request.entries;
        let custom_instructions = request.custom_instructions;
        let replace_instructions = request.replace_instructions;
        let run = async {
            pa_core::session_engine::branch_summarization::generate_branch_summary(
                &entries,
                pa_core::session_engine::branch_summarization::GenerateBranchSummaryOptions {
                    model: &model,
                    api_key,
                    custom_instructions: custom_instructions.as_deref(),
                    replace_instructions,
                    reserve_tokens,
                },
            )
            .await
        };
        match self
            .runtime
            .block_on(pa_agent::abort::race_with_abort(run, signal))
        {
            Ok(result) => {
                if result.aborted {
                    return BranchSummaryOutcome::Aborted;
                }
                if let Some(error) = result.error {
                    return BranchSummaryOutcome::Failed { error };
                }
                let summary = result
                    .summary
                    .unwrap_or_else(|| "No summary generated".to_string());
                BranchSummaryOutcome::Complete {
                    run: BranchSummaryRun {
                        summary,
                        usage: result
                            .usage
                            .and_then(|usage| serde_json::to_value(usage).ok()),
                        details: Some(json!({
                            "readFiles": result.read_files,
                            "modifiedFiles": result.modified_files,
                        })),
                    },
                }
            }
            Err(_) => BranchSummaryOutcome::Aborted,
        }
    }

    fn rebuild_session_context(
        &self,
        branch_entries: Vec<pa_types::session::FileEntry>,
    ) -> anyhow::Result<()> {
        // The caller parks this synchronous engine call on a blocking
        // thread (see `branch_navigation`), so `blocking_lock` is legal
        // here; the async session move below then rides the engine
        // runtime, the same pattern as `run_compaction`.
        let built = self.session.blocking_lock().is_some();
        if !built {
            // The session builds lazily on the first turn; park the branch
            // so the build consumes it (see `session_agent`).
            *self
                .pending_branch
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(branch_entries);
            return Ok(());
        }
        self.runtime.block_on(async move {
            let guard = self.session.lock().await;
            let Some(engine) = guard.as_ref() else {
                return Ok(());
            };
            engine.session.rebuild_branch_context(branch_entries).await
        })
    }

    fn configure_rlm_identity(
        &self,
        identity: crate::engine::RlmSessionIdentity,
    ) -> anyhow::Result<()> {
        // This session's own depth gates the kernel `refine.*` host requests
        // (TS `_autoRefineAllowedForSession`: depth-0 sessions only).
        self.rlm_depth
            .store(identity.rlm_depth, std::sync::atomic::Ordering::Relaxed);
        // The inherited default the children registry seeds from (validated;
        // the children create command carries it onward). This session's own
        // effective level resolves through the shared path instead: the
        // worker routes the same create-config `thinking` flag through
        // `configure_model`, so it lands in `effective_thinking` already
        // validated and clamped to the model (the TS `resolveRuntimeSessionOptions`
        // -> sdk.ts `createAgentSession` order).
        if let Some(thinking) = &identity.thinking {
            pa_ai::models::thinking_level_from_str(thinking)
                .ok_or_else(|| anyhow::anyhow!("unknown thinking level \"{thinking}\""))?;
        }
        // The depth bound's TS precedence (agent-session
        // `_resolveRlmMaxDepth`): a persisted chat override wins, then the
        // create-carried bound (inherited), the global setting, the
        // `RLM_MAX_DEPTH` env, and finally the shared default.
        let (max_depth, source) = persisted_rlm_max_depth(identity.session_file.as_deref())
            .map(|depth| (depth, "chat"))
            .or_else(|| {
                identity
                    .rlm_max_depth
                    .map(|depth| (u64::from(depth), "inherited"))
            })
            .or_else(|| {
                let settings = pa_core::settings::SettingsManager::create(
                    &self.config.cwd,
                    &self.config.agent_dir,
                );
                settings.get_rlm_max_depth().map(|depth| (depth, "global"))
            })
            .or_else(|| {
                std::env::var("RLM_MAX_DEPTH")
                    .ok()
                    .filter(|value| !value.is_empty())
                    .and_then(|value| value.parse::<u64>().ok())
                    .filter(|value| *value >= 1)
                    .map(|depth| (depth, "env"))
            })
            .unwrap_or((u64::from(DEFAULT_RLM_MAX_DEPTH), "default"));
        *self.rlm_max_depth_source.lock().expect("depth source lock") = source;
        if let Some(children) = &self.children {
            let parent = ParentIdentity {
                rlm_depth: identity.rlm_depth,
                rlm_max_depth: max_depth.min(u64::from(u32::MAX)) as u32,
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

    /// An agent message from one of this session's children arrived: the
    /// children registry records it so the child's no-reply terminal
    /// notice is withheld (TS `_parentReplyCount` on the child run).
    fn mark_child_reply(&self, child_active_session_id: &str) {
        if let Some(children) = &self.children {
            let children = Arc::clone(children);
            let child = child_active_session_id.to_string();
            // The delivery handler is sync; the registry lock is async, so
            // the mark parks on this engine's own runtime.
            self.runtime.spawn(async move {
                children.mark_replied(&child).await;
            });
        }
    }

    /// The worker's turn completed: release child prompt tasks waiting on
    /// the turn boundary (see `SupervisorChildSessions::wait_turn_done`).
    fn on_turn_done(&self) {
        if let Some(children) = &self.children {
            children.notify_turn_done();
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

    fn rlm_child_snapshots(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Value>> + Send + '_>> {
        let children = self.children.clone();
        Box::pin(async move {
            let Some(children) = children else {
                return Vec::new();
            };
            children.child_snapshots().await
        })
    }

    fn connection_commands(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<Value>> + Send + '_>> {
        Box::pin(async move {
            let guard = self.session.lock().await;
            let Some(engine) = guard.as_ref() else {
                return Vec::new();
            };
            // TS `createAgentConnectionCommands` order: extension
            // commands, then prompt templates, then skills. The Rust
            // extension registry does not track per-command source info,
            // so extension entries carry the TS fields minus
            // `sourceInfo`.
            let mut commands = Vec::new();
            if let Some(runner) = &engine.extension_runner {
                let registry = runner.registry().await;
                for command in registry.commands() {
                    let mut entry = json!({
                        "name": command.invocation_name,
                        "registeredName": command.name,
                        "source": "extension",
                    });
                    if let Some(description) = &command.description {
                        entry["description"] = json!(description);
                    }
                    commands.push(entry);
                }
            }
            for template in &engine.prompt_templates {
                let mut entry = json!({
                    "name": template.name,
                    "source": "prompt",
                    "sourceInfo": template.source_info,
                });
                if let Some(hint) = &template.argument_hint {
                    entry["argumentHint"] = json!(hint);
                }
                if !template.description.is_empty() {
                    entry["description"] = json!(template.description);
                }
                commands.push(entry);
            }
            for skill in &engine.skills {
                let mut entry = json!({
                    "name": format!("skill:{}", skill.name),
                    "source": "skill",
                    "sourceInfo": skill.source_info,
                });
                if !skill.description.is_empty() {
                    entry["description"] = json!(skill.description);
                }
                commands.push(entry);
            }
            commands
        })
    }

    fn resource_snapshot(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send + '_>> {
        Box::pin(async move {
            let session_id = {
                let guard = self.session.lock().await;
                match guard.as_ref() {
                    Some(engine) => engine.session.session_id().await,
                    None => {
                        // The session builds lazily (first prompt); the
                        // resource surface reads the session's own loader
                        // results, so an unbuilt session answers the
                        // empty snapshot.
                        return crate::engine::empty_resource_snapshot();
                    }
                }
            };
            let guard = self.session.lock().await;
            let Some(engine) = guard.as_ref() else {
                return crate::engine::empty_resource_snapshot();
            };
            let cwd = self.config.cwd.display().to_string();
            let mut skills = Vec::new();
            for skill in &engine.skills {
                let mut entry = json!({
                    "name": skill.name,
                    "filePath": skill.file_path.display().to_string(),
                    "sourceInfo": skill.source_info,
                });
                if !skill.description.is_empty() {
                    entry["description"] = json!(skill.description);
                }
                if let Some(artifact) = artifact_reference(
                    &session_id,
                    &cwd,
                    "skill",
                    &skill.file_path.display().to_string(),
                ) {
                    entry["artifact"] = artifact;
                }
                skills.push(entry);
            }
            let mut prompts = Vec::new();
            for template in &engine.prompt_templates {
                let mut entry = json!({
                    "name": template.name,
                    "filePath": template.file_path,
                    "sourceInfo": template.source_info,
                });
                if !template.description.is_empty() {
                    entry["description"] = json!(template.description);
                }
                if let Some(hint) = &template.argument_hint {
                    entry["argumentHint"] = json!(hint);
                }
                if let Some(artifact) =
                    artifact_reference(&session_id, &cwd, "prompt", &template.file_path)
                {
                    entry["artifact"] = artifact;
                }
                prompts.push(entry);
            }
            let mut context_files = Vec::new();
            for file in &engine.agents_files {
                let mut entry = json!({ "path": file.path.display().to_string() });
                if let Some(artifact) = artifact_reference(
                    &session_id,
                    &cwd,
                    "context_file",
                    &file.path.display().to_string(),
                ) {
                    entry["artifact"] = artifact;
                }
                context_files.push(entry);
            }
            json!({
                "contextFiles": context_files,
                "skills": skills,
                "prompts": prompts,
                "extensions": [],
                "themes": [],
                "diagnostics": {
                    "skills": engine.skill_diagnostics,
                    "prompts": [],
                    "extensions": engine
                        .extension_diagnostics
                        .iter()
                        .map(|error| json!({ "type": "error", "message": error }))
                        .collect::<Vec<_>>(),
                    "themes": [],
                },
            })
        })
    }

    fn system_prompt(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send + '_>>
    {
        Box::pin(async move {
            // The TS session exists from create; this port builds the
            // core session lazily on the first turn, so a prompt read
            // before any turn builds it now (the async build path, never
            // the blocking `ensure_core_session`: this future runs on the
            // caller's runtime).
            let model = self.resolve_model()?;
            self.ensure_core_session_async(&model).await?;
            let guard = self.session.lock().await;
            let engine = guard.as_ref().expect("session built above");
            Ok(engine.system_prompt.clone())
        })
    }

    fn tool_definition(
        &self,
        name: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Value>> + Send + '_>> {
        let name = name.to_string();
        Box::pin(async move {
            let guard = self.session.lock().await;
            let engine = guard.as_ref()?;
            let state = engine.session.agent().state().await;
            let tool = state.tools.iter().find(|tool| tool.name() == name)?;
            Some(json!({
                "name": tool.name(),
                "label": tool.label(),
                "description": tool.description(),
                "parameters": tool.parameters(),
            }))
        })
    }

    fn run_refinement(
        &self,
        options: pa_core::session_engine::refine::RefineOptions,
    ) -> anyhow::Result<Value> {
        let model = self.resolve_model()?;
        self.ensure_core_session(&model)?;
        let api_key = self.resolve_request_api_key(&model);
        let global_harness_dir = self.config.agent_dir.clone();
        let guard = self.session.blocking_lock();
        let core = guard
            .as_ref()
            .expect("session built by ensure_core_session");
        let result = self.runtime.block_on(async {
            core.session
                .refine(
                    &options,
                    pa_core::session_engine::refine::RefinementSource::User,
                    &model,
                    api_key,
                    global_harness_dir,
                )
                .await
        })?;
        serde_json::to_value(&result)
            .map_err(|error| anyhow::anyhow!("refinement result conversion failed: {error}"))
    }

    fn rlm_max_depth_status(&self) -> Value {
        let source = *self.rlm_max_depth_source.lock().expect("depth source lock");
        let max_depth = match &self.children {
            // The live bound the registry enforces (the chat override and
            // the inherited/seeded bound both land there).
            Some(children) => children.rlm_max_depth(),
            None => DEFAULT_RLM_MAX_DEPTH,
        };
        json!({ "maxDepth": max_depth, "source": source })
    }

    fn cancel_rlm_child<'a>(
        &'a self,
        child_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            match &self.children {
                Some(children) => children.cancel_child_run(child_id).await,
                None => false,
            }
        })
    }

    fn delete_rlm_subagent<'a>(
        &'a self,
        child_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<&'static str>> + Send + 'a>,
    > {
        Box::pin(async move {
            match &self.children {
                Some(children) => children.delete_inactive_subagent(child_id).await,
                None => Ok("not_found"),
            }
        })
    }

    fn set_rlm_max_depth(&self, max_depth: u64, global: bool) -> anyhow::Result<Value> {
        // The live bound every spawn checks (TS updates `_rlmMaxDepth`
        // and rebuilds the system prompt; the bound itself lives in the
        // registry here - see PORTING-NOTES for the prompt-text note).
        if let Some(children) = &self.children {
            children.set_rlm_max_depth(max_depth.min(u64::from(u32::MAX)) as u32);
        }
        *self.rlm_max_depth_source.lock().expect("depth source lock") = "chat";
        // The durable `rlm_max_depth_state` custom entry (TS
        // `appendCustomEntryWithRollback`): a resumed session re-seeds
        // its bound from it. The session that is not built yet parks the
        // entry for its build (the `pending_branch` pattern).
        self.persist_max_depth_state(max_depth);
        // The global settings write (TS `settingsManager.setRlmMaxDepth`
        // + flush + drain): errors join the TS `globalError` field, they
        // do not fail the command.
        let mut result = json!({
            "maxDepth": max_depth,
            "source": "chat",
            "globalSaved": false,
        });
        if global {
            if let Some(error) = self.write_global_rlm_max_depth(max_depth) {
                result["globalError"] = json!(error);
            } else {
                result["globalSaved"] = json!(true);
            }
        }
        Ok(result)
    }

    fn run_prompt(
        &self,
        _prompt_index: usize,
        request: PromptRequest,
        aborted: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) {
        // Goal-state changes surface as `goal_update` at the moment they
        // happen (kernel host requests and session-command mutations), so
        // every emit of this prompt runs through the tracking wrapper.
        let mut emit = self.goal_tracking_emit(emit);
        // Session commands (compact/refine/goal/autonomous) never admit a
        // model turn and never record a user-message row: the durable echo
        // row replaces it. Execute before admission so the idle-wait loop
        // below stays reachable only for real turns.
        if let Some(command) =
            crate::session_commands::parse_prompt_session_command(&request.message)
        {
            let Some(execution) =
                crate::session_commands::run_session_command(self, command, &mut emit)
            else {
                return;
            };
            if let Some(error) = &execution.error {
                emit(EngineEvent::Done(Err(error.clone())));
                return;
            }
            // A goal start/resume schedules its continuation context as
            // the turn; the durable goal-context row is already emitted.
            // An unchanged `/goal` state stays silent (TS emits
            // goal_update only on state change; the interactive surface
            // dedupes announcements).
            if let Some(continuation) = execution.continuation_prompt {
                self.run_turns(&continuation, &[], aborted, &mut emit);
            } else {
                emit(EngineEvent::Done(Ok(())));
            }
            return;
        }
        // The accepted turn row: an injected custom row (wire
        // `role: "custom"`) replaces the user message — the row persists
        // and renders as itself while the model turn still runs on the
        // message text (TS injected-prompt turns: RLM child terminal
        // notices). The plain turn records the accepted user message;
        // images ride as multimodal content blocks after the text (TS
        // prompt admission: the text part first, then the image parts).
        let accepted = match &request.custom_message {
            Some(custom) => EngineEvent::CustomMessage(custom.clone()),
            None => {
                let mut content = vec![json!({ "type": "text", "text": request.message })];
                for image in &request.images {
                    let mut block = match serde_json::to_value(image) {
                        Ok(Value::Object(block)) => Value::Object(block),
                        _ => continue,
                    };
                    if let Some(object) = block.as_object_mut() {
                        object.insert("type".to_string(), json!("image"));
                    }
                    content.push(block);
                }
                EngineEvent::UserMessage(json!({
                    "role": "user",
                    "content": content,
                    "timestamp": now_millis(),
                }))
            }
        };
        if !emit(accepted) {
            return;
        }
        self.run_turns(&request.message, &request.images, aborted, &mut emit);
    }
}

impl AgentSessionEngine {
    /// Drive one admitted prompt through the retry-driver model loop and
    /// emit the turn outcome (provider-failure retries + final-row
    /// surfacing). The user row — or a goal continuation's durable context
    /// row — precedes this, so this starts at the model turn. The trailing
    /// `Done` is owned by the caller (`run_turns`).
    fn run_model_turn(
        &self,
        admission: TurnAdmission,
        prompt: &str,
        images: &[pa_agent::types::ImageContent],
        aborted: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) -> TurnResult {
        let prompt = prompt.to_string();
        // Model resolution and session construction are hard failures: they
        // never reach the provider, so the retry loop does not apply (the
        // TS loop only classifies provider stream failures).
        let model = match self.resolve_model() {
            Ok(model) => model,
            Err(error) => {
                return TurnResult::Error {
                    error: error.to_string(),
                    assistant: None,
                }
            }
        };
        let agent = match self.session_agent(&model) {
            Ok(agent) => agent,
            Err(error) => {
                return TurnResult::Error {
                    error: format!("{error:#}"),
                    assistant: None,
                }
            }
        };
        let policy = self.retry_policy();
        let failover_policy = self.failover_policy();
        let candidates = self.failover_candidates(&model);
        // The pa-core retry driver owns the attempt loop; this engine owns
        // one turn. The driver awaits each attempt to completion before
        // emitting retry events, so the single `emit` reference is handed
        // through a RefCell slot to whichever closure is currently running.
        let emit_cell = std::cell::RefCell::new(emit);
        // The overflow compact-and-retry re-issues the loop without a new
        // user message, so its turn starts as a continuation (TS
        // `agent.continue()`); an ordinary turn starts fresh and only the
        // retry driver's re-issues continue.
        let first_attempt = std::cell::Cell::new(matches!(admission, TurnAdmission::FreshPrompt));
        // Failover switch/restore re-bind the live agent's model and append
        // the model-change row the TS backup-model retry logs. The primary
        // (model + thinking level) is captured at the first switch and
        // restored on every settled outcome.
        let persistence = {
            let guard = self.session.blocking_lock();
            guard
                .as_ref()
                .map(|engine| engine.session.shared_persistence())
        };
        // Retry/failover adoption telemetry (TS `auto_retry_start` counting):
        // retries increment `retry_count`, provider switches `failover_count`.
        let telemetry = {
            let guard = self.session.blocking_lock();
            guard.as_ref().and_then(|engine| engine.telemetry.clone())
        };
        let primary_state: std::cell::RefCell<
            Option<(
                pa_types::ai::Model,
                pa_agent::types::ThinkingLevel,
                Option<String>,
            )>,
        > = std::cell::RefCell::new(None);
        let result = self.runtime.block_on(
            pa_core::session_engine::provider_failover::run_turn_with_provider_failover(
                &policy,
                &failover_policy,
                &candidates,
                model.context_window,
                None,
                || {
                    let mut emit = emit_cell.borrow_mut();
                    let first = first_attempt.get();
                    first_attempt.set(false);
                    let agent = agent.clone();
                    let prompt = prompt.clone();
                    let images = images.to_vec();
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
                            .run_turn_once(&agent, &prompt, &images, first, &mut **emit)
                            .await
                        {
                            Ok(TurnOnce::Message { assistant }) => {
                                // The settled messages already reached the
                                // transcript through their message_end
                                // events (the failure included: TS persists
                                // and renders it like any outcome); this
                                // arm only carries the final message to the
                                // retry classifier.
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
                    let telemetry = telemetry.clone();
                    async move {
                        if let Some(telemetry) = &telemetry {
                            telemetry.note_auto_retry();
                            if matches!(
                                &event,
                                pa_core::session_engine::auto_retry::AutoRetryEvent::Start {
                                    reason:
                                        pa_core::session_engine::auto_retry::RetryStartReason::Backup { .. },
                                    ..
                                }
                            ) {
                                telemetry.note_provider_failover();
                            }
                        }
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
                |next: &pa_types::ai::Model| {
                    let agent = agent.clone();
                    let persistence = persistence.clone();
                    {
                        let mut primary = primary_state.borrow_mut();
                        // Capture the primary model + thinking level + key
                        // once (TS `_backupModel` state): the level the
                        // session was built with, restored when the turn
                        // settles.
                        if primary.is_none() {
                            *primary = Some((
                                model.clone(),
                                map_thinking_level(self.effective_thinking()),
                                self.resolve_request_api_key(&model),
                            ));
                        }
                    }
                    let next = next.clone();
                    async move {
                        let agent_model = json_round_trip(&next)
                            .ok_or_else(|| anyhow::anyhow!("model conversion failed"))?;
                        // Clamp the requested level to what the switched-to
                        // model supports (TS `clampThinkingLevel` on the
                        // backup switch); the primary's level is restored
                        // with the primary.
                        let clamped =
                            pa_ai::models::clamp_thinking_level(&next, self.effective_thinking());
                        // The stream's provider target follows the switch
                        // (the same slot `set_model` swaps): the retried
                        // request hits the switched-to provider with its
                        // resolved key.
                        {
                            let mut target =
                                self.provider_target.write().expect("provider target lock");
                            *target = Some(ProviderTarget {
                                api_key: self.resolve_request_api_key(&next),
                                model: next.clone(),
                            });
                        }
                        agent.set_model(agent_model).await;
                        agent
                            .set_thinking_level(map_thinking_level(clamped))
                            .await;
                        if let Some(persistence) = persistence {
                            let mut session = persistence.lock().await;
                            session.append_model_change(&next.provider, &next.id);
                        }
                        Ok(())
                    }
                },
                || {
                    let agent = agent.clone();
                    let persistence = persistence.clone();
                    let primary = primary_state.borrow().clone();
                    async move {
                        let Some((primary_model, thinking_level, primary_api_key)) = primary
                        else {
                            return Ok(None);
                        };
                        let agent_model = json_round_trip(&primary_model)
                            .ok_or_else(|| anyhow::anyhow!("model conversion failed"))?;
                        // Restore the stream's provider target with the
                        // primary (the slot the build-time target set).
                        {
                            let mut target =
                                self.provider_target.write().expect("provider target lock");
                            *target = Some(ProviderTarget {
                                api_key: primary_api_key,
                                model: primary_model.clone(),
                            });
                        }
                        agent.set_model(agent_model).await;
                        agent.set_thinking_level(thinking_level).await;
                        if let Some(persistence) = persistence {
                            let mut session = persistence.lock().await;
                            session.append_model_change(
                                &primary_model.provider,
                                &primary_model.id,
                            );
                        }
                        Ok(Some(format!(
                            "{}/{}",
                            primary_model.provider, primary_model.id
                        )))
                    }
                },
            ),
        );
        match result {
            Ok(message) => match message.stop_reason {
                // The failure already reached the transcript as the final
                // assistant message; the turn error still travels to
                // headless callers through the turn result.
                StopReason::Error => TurnResult::Error {
                    error: message
                        .error_message
                        .clone()
                        .filter(|error| !error.is_empty())
                        .unwrap_or_else(|| "Assistant response failed".to_string()),
                    assistant: Some(Box::new(message)),
                },
                StopReason::Aborted => TurnResult::Aborted,
                _ => TurnResult::Message(Box::new(message)),
            },
            Err(error) => TurnResult::Error {
                error: error.to_string(),
                assistant: None,
            },
        }
    }

    /// Drop pending turn-boundary requests (aborted turns; TS `_checkCompaction`
    /// abort arm clears both the compaction and the refine request).
    fn drop_turn_boundary_requests(&self) {
        let guard = self.session.blocking_lock();
        if let Some(engine) = guard.as_ref() {
            self.runtime.block_on(engine.turn_boundary.clear_pending());
        }
    }

    /// Consume pending `compact.run`/`refine.run` requests at the settled
    /// turn boundary, in TS order (compaction, then refinement). The
    /// compaction outcome reaches the transcript like `/compact` (the
    /// worker persists the entry and broadcasts `compaction_end`); the
    /// model-facing refinement notice reaches it like the `/refine` notice
    /// row. A consumed compaction stops the run (TS: requested compaction
    /// stops the loop on purpose; the model resumes on the next prompt or
    /// queued continuation).
    fn run_turn_boundary(&self, emit: &mut dyn FnMut(EngineEvent) -> bool) -> BoundaryRun {
        // Fast path: nothing scheduled (the common turn).
        let has_pending = {
            let guard = self.session.blocking_lock();
            match guard.as_ref() {
                Some(engine) => self.runtime.block_on(async {
                    engine.turn_boundary.compaction_scheduled().await
                        || engine.turn_boundary.refine_pending().await
                }),
                None => false,
            }
        };
        if !has_pending {
            return BoundaryRun::Proceed;
        }
        let model = match self.resolve_model() {
            Ok(model) => model,
            Err(error) => {
                eprintln!("pa-daemon: boundary request could not resolve a model: {error:#}");
                return BoundaryRun::Proceed;
            }
        };
        let api_key = self.resolve_request_api_key(&model);
        let global_harness_dir = self.config.agent_dir.clone();
        let consumption = {
            let guard = self.session.blocking_lock();
            let Some(engine) = guard.as_ref() else {
                return BoundaryRun::Proceed;
            };
            self.runtime.block_on(async {
                engine
                    .consume_turn_boundary_requests(&model, api_key, global_harness_dir)
                    .await
            })
        };
        let mut stopped_for_compaction = false;
        match consumption.compaction {
            Some(Ok(pa_core::session_engine::compact_session::CompactOutcome::Ran(run))) => {
                let entry = serde_json::to_value(&run.entry).unwrap_or(Value::Null);
                // The wire result is the TS `CompactionResult` shape; the
                // event reason is `requested` (TS `_runAutoCompaction`).
                let result = serde_json::json!({
                    "summary": run.result.summary,
                    "firstKeptEntryId": run.result.first_kept_entry_id,
                    "tokensBefore": run.result.tokens_before,
                });
                let event =
                    crate::compaction::compaction_end_success("requested", &result, false, None);
                if !emit(EngineEvent::Compaction { entry, event }) {
                    return BoundaryRun::Cancelled;
                }
                stopped_for_compaction = true;
            }
            // A skip consumed the request (the Rust `/compact` contract):
            // the durable disclosure row goes out with its message pair,
            // then the end event carries the TS warning.
            Some(Ok(pa_core::session_engine::compact_session::CompactOutcome::Skipped(
                message,
            ))) => {
                eprintln!("pa-daemon: requested compaction skipped: {message}");
                if !self.emit_unsuccessful_compaction(
                    pa_core::session_engine::messages::CompactionOutcomeReason::Requested,
                    pa_core::session_engine::messages::CompactionOutcomeKind::Skipped,
                    &format!("Requested compaction skipped: {message}"),
                    Some("warning"),
                    None,
                    emit,
                ) {
                    return BoundaryRun::Cancelled;
                }
                stopped_for_compaction = true;
            }
            Some(Err(error)) => {
                eprintln!("pa-daemon: requested compaction failed: {error:#}");
                // TS `_endCompactionUnsuccessfully` passes no
                // `errorSeverity` for automatic failures.
                if !self.emit_unsuccessful_compaction(
                    pa_core::session_engine::messages::CompactionOutcomeReason::Requested,
                    pa_core::session_engine::messages::CompactionOutcomeKind::Failed,
                    &format!("Requested compaction failed: {error:#}"),
                    None,
                    None,
                    emit,
                ) {
                    return BoundaryRun::Cancelled;
                }
                stopped_for_compaction = true;
            }
            None => {}
        }
        match consumption.refinement {
            Some(Ok(refinement)) => {
                // The model-facing notice row (durable, like the session
                // persistence of TS `refine()`).
                if refinement.applied_edits.iter().any(|edit| edit.applied) {
                    let notice = pa_core::session_engine::refine::create_refinement_notice_message(
                        &refinement,
                        pa_core::session_engine::refine::RefinementSource::SelfRefine,
                    );
                    if !emit(EngineEvent::CustomMessage(
                        crate::session_commands::custom_message_value(&notice),
                    )) {
                        return BoundaryRun::Cancelled;
                    }
                }
            }
            // TS emits `refine_failed` on the wire; the Rust daemon wire
            // has no refine event yet — the worker log keeps the failure.
            Some(Err(error)) => {
                eprintln!("pa-daemon: requested refinement failed: {error:#}");
            }
            None => {}
        }
        if stopped_for_compaction {
            BoundaryRun::StoppedForCompaction
        } else {
            BoundaryRun::Proceed
        }
    }

    /// The turn loop: run one model turn, consume turn-boundary requests,
    /// then ask the autonomous driver what follows. A continuation is
    /// injected as a durable user row and drives the next turn; a stop
    /// surfaces its reason as a durable `autonomous_status` row. The single
    /// trailing `Done` ends the run.
    fn run_turns(
        &self,
        first_prompt: &str,
        first_images: &[pa_agent::types::ImageContent],
        aborted: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) {
        // The first turn admits the prompt with its images; every
        // autonomous follow-up turn runs text-only (the TS driver
        // regenerates from the loop state, never re-sending attachments).
        let mut prompt = first_prompt.to_string();
        let mut first = true;
        let mut overflow_retry = false;
        // TS resets `_overflowRecovery` when a message that starts an agent
        // run enters the loop: the admitted prompt here.
        self.reset_overflow_recovery();
        loop {
            let images: &[pa_agent::types::ImageContent] = if first { first_images } else { &[] };
            first = false;
            // TS `_runPreTurnCompaction` (`beforeModelSelection` for queued
            // prompts): a stale overflow error from the previous run gets
            // its compact-and-retry attempt on the newly admitted prompt
            // (Case 1 runs before the threshold arm), then a threshold
            // crossing that predates this admission compacts before the
            // turn runs; the turn then proceeds either way.
            if !self.run_pre_turn_overflow_compaction(emit) {
                return;
            }
            if self.run_auto_compaction(emit) == AutoCompactionRun::Cancelled {
                return;
            }
            // The overflow compact-and-retry re-issues the loop without a
            // new user message; every other iteration runs a fresh prompt
            // (autonomous continuations are real user rows).
            let admission = if overflow_retry {
                overflow_retry = false;
                TurnAdmission::Continue
            } else {
                TurnAdmission::FreshPrompt
            };
            let turn = self.run_model_turn(admission, &prompt, images, aborted, emit);
            let assistant = match turn {
                TurnResult::Message(assistant) => {
                    // A settled non-error turn resets the overflow
                    // recovery state (TS resets at every non-error
                    // assistant message end).
                    self.reset_overflow_recovery();
                    assistant
                }
                // An aborted turn never services boundary requests (TS
                // `_checkCompaction` abort arm): drop any pending ones so
                // a stale request cannot leak into the next turn.
                TurnResult::Aborted => {
                    self.reset_overflow_recovery();
                    self.drop_turn_boundary_requests();
                    emit(EngineEvent::Done(Err("No response produced.".to_string())));
                    return;
                }
                TurnResult::Error { error, assistant } => {
                    // TS `_checkCompaction` Case 1 at `agent_end`: a
                    // context-overflow error triggers one compact-and-retry
                    // attempt before the run ends.
                    let arm = assistant
                        .map(|assistant| self.run_overflow_compaction(&assistant, emit))
                        .unwrap_or(OverflowArmRun::NotApplicable);
                    match arm {
                        OverflowArmRun::RetryTurn => {
                            overflow_retry = true;
                            continue;
                        }
                        OverflowArmRun::NotApplicable | OverflowArmRun::Finished => {}
                        OverflowArmRun::Cancelled => return,
                    }
                    emit(EngineEvent::Done(Err(error)));
                    return;
                }
            };
            // Turn-boundary consumption (TS `_checkCompaction` requested
            // arm, then `_consumePendingRequestedRefine`): requests the
            // kernel `compact.run`/`refine.run` host handlers scheduled
            // during this turn run now, between turns.
            match self.run_turn_boundary(emit) {
                BoundaryRun::Cancelled => return,
                BoundaryRun::StoppedForCompaction => {
                    emit(EngineEvent::Done(Ok(())));
                    return;
                }
                BoundaryRun::Proceed => {}
            }
            // TS agent_end `_checkCompaction` threshold arm (after the
            // requested arm, which never falls through to it): the settled
            // turn's usage crossing the reserve headroom auto-compacts;
            // the autonomous continuation decision below still runs, so a
            // continuation the driver queues continues after the
            // compaction like the TS queued continuation.
            if self.run_auto_compaction(emit) == AutoCompactionRun::Cancelled {
                return;
            }
            match self.autonomous_follow_up(&assistant) {
                AutonomousFollowUp::Inactive => {
                    emit(EngineEvent::Done(Ok(())));
                    return;
                }
                AutonomousFollowUp::Continue { text } => {
                    if aborted()
                        || !emit(EngineEvent::UserMessage(json!({
                            "role": "user",
                            "content": [{ "type": "text", "text": text }],
                            "timestamp": now_millis(),
                        })))
                    {
                        emit(EngineEvent::Done(Err("No response produced.".to_string())));
                        return;
                    }
                    prompt = text;
                }
                AutonomousFollowUp::Stop { reason, status } => {
                    let row = pa_core::autonomous::autonomous_stop_row(&reason, &status);
                    emit(EngineEvent::CustomMessage(
                        crate::session_commands::custom_message_value(&row),
                    ));
                    emit(EngineEvent::Done(Ok(())));
                    return;
                }
            }
        }
    }

    /// Consult the autonomous driver for one settled turn: gate evaluation
    /// (a shell command per configured gate) runs on the engine runtime.
    fn autonomous_follow_up(
        &self,
        assistant: &pa_agent::types::AssistantMessage,
    ) -> pa_core::autonomous::AutonomousFollowUp {
        let Some(message) = json_round_trip::<_, pa_types::ai::AssistantMessage>(assistant) else {
            return pa_core::autonomous::AutonomousFollowUp::Inactive;
        };
        let driver = std::sync::Arc::clone(
            &*self
                .autonomous_driver
                .read()
                .expect("autonomous driver lock"),
        );
        self.runtime.block_on(async {
            let mut state = self.autonomous.lock().await;
            driver.after_turn(&mut state, &message).await
        })
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
                self.mirror_goal_runtime(&built);
                // A `set_rlm_max_depth` that landed before the build parks
                // its durable entry; the built session owns the store now.
                {
                    let handles = self.goal_runtime.lock().expect("goal runtime lock").clone();
                    if let Some(handles) = handles {
                        let mut manager = self
                            .runtime
                            .block_on(async { handles.session.lock().await });
                        self.flush_pending_max_depth(&mut manager);
                    }
                }
                // A branch move that landed before the first turn (tree
                // navigation/fork with no turn yet) re-seeds the session
                // onto the moved branch.
                let pending_branch = self
                    .pending_branch
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                if let Some(entries) = pending_branch {
                    self.runtime
                        .block_on(async { built.session.rebuild_branch_context(entries).await })?;
                }
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

    /// The provider-failover policy from settings (`retry.failover`).
    fn failover_policy(
        &self,
    ) -> pa_core::session_engine::provider_failover::ProviderFailoverPolicy {
        pa_core::settings::SettingsManager::create(&self.config.cwd, &self.config.agent_dir)
            .get_provider_failover_policy()
    }

    /// The failover chain for `model`: the other auth-configured providers
    /// serving the same model id, in catalog order after the current one.
    /// Faux-script sessions never fail over (their failures are
    /// deterministic test fixtures, and a second provider would only
    /// reroute the scripted queue).
    fn failover_candidates(&self, model: &pa_types::ai::Model) -> Vec<pa_types::ai::Model> {
        if self.config.faux_script.is_some() {
            return Vec::new();
        }
        let auth = pa_core::auth::AuthStorage::create(&self.config.agent_dir);
        let mut registry =
            pa_core::models::ModelRegistry::create(auth, self.config.agent_dir.join("models.json"));
        registry.load_private_authorization_from_cache();
        let available: Vec<pa_types::ai::Model> =
            registry.get_available().into_iter().cloned().collect();
        pa_core::models::failover_candidates(model, &available)
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
        images: &[pa_agent::types::ImageContent],
        first_attempt: bool,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) -> anyhow::Result<TurnOnce> {
        // Stream assistant events while the turn runs.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<EngineEvent>();
        // Goal usage accounting (TS `_accountGoalUsageForAssistantMessage`
        // at the message_end hook) shares the same per-message hook: while a
        // goal is active, each settled non-error assistant message records
        // its token delta; a budget crossing moves the goal to
        // `budget_limited` and the next emitted event publishes the
        // `goal_update`. The handles come from the engine mirror: the core
        // session's own mutex is held across the turn's admission.
        let goal_runtime = self.goal_runtime.lock().expect("goal runtime lock").clone();
        let subscription = {
            let tx = tx.clone();
            // Per-message usage accounting runs on every settled assistant
            // message (whatever the stop reason except errors), matching the
            // TS message_end hook. The driver owns the policy; this loop
            // only forwards the message to it.
            let autonomous_state = std::sync::Arc::clone(&self.autonomous);
            let autonomous_driver = std::sync::Arc::clone(
                &*self
                    .autonomous_driver
                    .read()
                    .expect("autonomous driver lock"),
            );
            agent
                .subscribe(move |event, _signal| {
                    let tx = tx.clone();
                    let autonomous_state = std::sync::Arc::clone(&autonomous_state);
                    let autonomous_driver = std::sync::Arc::clone(&autonomous_driver);
                    let goal_runtime = goal_runtime.clone();
                    Box::pin(async move {
                        use pa_agent::types::AgentEvent;
                        if let AgentEvent::MessageEnd {
                            message:
                                pa_agent::types::AgentMessage::Standard(
                                    pa_agent::types::Message::Assistant(assistant),
                                ),
                        } = &event
                        {
                            if let Some(message) =
                                json_round_trip::<_, pa_types::ai::AssistantMessage>(assistant)
                            {
                                let mut state = autonomous_state.lock().await;
                                autonomous_driver.account_message(&mut state, &message);
                                // Goal accounting mirrors the TS guard: only
                                // turns that were neither errors nor aborted
                                // spend the goal's budget, and only while
                                // the goal is active.
                                if let Some(handles) = goal_runtime.as_ref() {
                                    if !matches!(
                                        message.stop_reason,
                                        pa_types::ai::StopReason::Error
                                            | pa_types::ai::StopReason::Aborted
                                    ) {
                                        let mut driver = handles.driver.lock().await;
                                        let mut session = handles.session.lock().await;
                                        // The loop does not assign message
                                        // ids in-process; the timestamp is
                                        // the double-counting guard identity.
                                        let message_id = format!("a-{}", message.timestamp);
                                        driver.record_assistant_usage(
                                            &mut session,
                                            &message_id,
                                            &message.usage,
                                        );
                                    }
                                }
                            }
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
                            AgentEvent::MessageEnd {
                                message: agent_message,
                            } => {
                                // Settled messages persist as session entries
                                // and reach clients: every assistant message
                                // (the TS `message_end` hook appends each
                                // one, mid-run tool-call turns included) and
                                // every tool-result message (framed as a
                                // message pair).
                                match agent_message {
                                    pa_agent::types::AgentMessage::Standard(
                                        pa_agent::types::Message::Assistant(_),
                                    )
                                    | pa_agent::types::AgentMessage::Standard(
                                        pa_agent::types::Message::ToolResult(_),
                                    ) => {
                                        if let Some(value) = session_wire_value(agent_message) {
                                            let event = if matches!(
                                                agent_message,
                                                pa_agent::types::AgentMessage::Standard(
                                                    pa_agent::types::Message::ToolResult(_)
                                                )
                                            ) {
                                                EngineEvent::ToolResultMessage(value)
                                            } else {
                                                EngineEvent::AssistantMessage(value)
                                            };
                                            let _ = tx.send(event);
                                        }
                                    }
                                    _ => {}
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
        // Admit the turn on the engine runtime without blocking the
        // forwarding loop below: the admission future settles only when the
        // whole turn settles (the TS daemon fires `prompt` with `void` and
        // streams events from the session listeners while it runs), while
        // the loop hands each streamed event to `emit` the moment it
        // arrives. Buffering events until the future resolves is what made
        // clients render a turn as one final batch.
        let prompt_text = prompt.to_string();
        let prompt_images = images.to_vec();
        let mut admitted = std::pin::pin!(async {
            if first_attempt {
                let guard = self.session.lock().await;
                let engine = guard.as_ref().expect("session built");
                engine
                    .session
                    .prompt_with_images(&prompt_text, prompt_images, Default::default())
                    .await
                    .map(|_| ())
            } else {
                agent.continue_run().await.map(|_| ())
            }
        });
        let mut aborted = false;
        let mut admission_error: Option<anyhow::Error> = None;
        let mut settled = false;
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
                outcome = &mut admitted => {
                    settled = true;
                    match outcome {
                        Ok(()) => {}
                        Err(error) => admission_error = Some(error),
                    }
                    // The turn settled: drain the events that raced the
                    // resolution, then stop the loop.
                    while let Ok(event) = rx.try_recv() {
                        if !emit(event) {
                            aborted = true;
                            break;
                        }
                    }
                }
            }
            if aborted || settled {
                break;
            }
        }
        if aborted {
            // The emit callback cancelled the turn: stop the still-running
            // admission and wait out its abort path before returning, so no
            // run outlives this attempt.
            agent.abort();
            let _ = (&mut admitted).await;
        }
        let _ = subscription.unsubscribe().await;
        if aborted {
            return Ok(TurnOnce::Aborted);
        }
        if let Some(error) = admission_error {
            return Err(anyhow::anyhow!("{error:#}"));
        }
        // The final assistant message decides the outcome (provider
        // failures included: the retry driver classifies them).
        let state = agent.state().await;
        for message in state.messages.iter().rev() {
            if let pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(
                assistant,
            )) = message
            {
                // The message must carry the session wire shape (the
                // transcript already received it through message_end); a
                // round-trip failure means no usable turn outcome.
                if json_round_trip::<_, pa_types::ai::AssistantMessage>(assistant).is_none() {
                    return Ok(TurnOnce::None);
                }
                return Ok(TurnOnce::Message {
                    assistant: Box::new(assistant.clone()),
                });
            }
        }
        Ok(TurnOnce::None)
    }
}

/// The outcome of one admitted turn.
enum TurnResult {
    /// The turn settled; the final assistant message (typed, boxed to
    /// keep the enum small).
    Message(Box<pa_agent::types::AssistantMessage>),
    /// The turn was aborted before a settled message.
    Aborted,
    /// The turn failed before or during the model call. `assistant` is the
    /// failed turn's settled message when one exists (provider failures:
    /// the overflow arm inspects it); model-resolution and session-build
    /// failures never reached the provider and carry none.
    Error {
        error: String,
        assistant: Option<Box<pa_agent::types::AssistantMessage>>,
    },
}

/// How one turn is admitted to the agent loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnAdmission {
    /// A fresh user prompt: the loop context gains the user message.
    FreshPrompt,
    /// Re-issue the loop without a new user message (TS `agent.continue()`):
    /// the overflow compact-and-retry path after the failed turn's error
    /// message left the loop context.
    Continue,
}

/// What the turn-boundary consumption did to the run.
enum BoundaryRun {
    /// Nothing pending, or requests consumed without stopping the run.
    Proceed,
    /// A consumed compaction stops the loop (TS: requested compaction
    /// stops the run on purpose).
    StoppedForCompaction,
    /// The emitter asked to stop.
    Cancelled,
}

/// The outcome of one turn attempt.
enum TurnOnce {
    /// The emit callback cancelled the run.
    Aborted,
    /// The turn produced no assistant message.
    None,
    /// The turn's final assistant message (retry classification).
    Message {
        assistant: Box<pa_agent::types::AssistantMessage>,
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
            reason,
        } => EngineEvent::AutoRetryStart {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
            reason,
        },
        AutoRetryEvent::End {
            success,
            attempt,
            final_error,
            restored_model,
        } => EngineEvent::AutoRetryEnd {
            success,
            attempt,
            final_error,
            restored_model,
        },
    }
}

/// The faux provider registry is process-global; faux-driven tests must
/// not register concurrently (each registration replaces the queue).
#[cfg(test)]
pub(crate) static FAUX_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) mod tests {
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
            thinking: None,
            session_dir: None,
            session_file: None,
            faux_script: None,
            supervisor_link: None,
            telemetry_disabled: None,
        })
        .unwrap();
        // The explicit selection from the session's create config is
        // authoritative over any process-wide fallback model.
        engine.configure_model(EngineModelSelection {
            provider: Some("battery".to_string()),
            model: Some("mock-1".to_string()),
            api_key: None,
            thinking: None,
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

    fn bare_engine(dir: &std::path::Path) -> AgentSessionEngine {
        let agent_dir = dir.join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        AgentSessionEngine::new(AgentEngineConfig {
            cwd: dir.to_path_buf(),
            agent_dir,
            provider: None,
            model: None,
            api_key: None,
            thinking: None,
            session_dir: None,
            session_file: None,
            faux_script: None,
            supervisor_link: None,
            telemetry_disabled: None,
        })
        .unwrap()
    }

    /// A settings.json with an explicit compaction reserve (the f14 battery
    /// shape: `reserveTokens` set so a seeded usage crosses the headroom).
    fn write_compaction_settings(dir: &std::path::Path, reserve_tokens: u64) {
        std::fs::create_dir_all(dir.join("agent")).unwrap();
        std::fs::write(
            dir.join("agent").join("settings.json"),
            serde_json::json!({ "compaction": { "enabled": true, "reserveTokens": reserve_tokens, "keepRecentTokens": 10 } })
                .to_string(),
        )
        .unwrap();
    }

    /// One faux-driven engine over its own tempdir (settings written before
    /// the first prompt so the session build resolves them).
    pub(crate) fn faux_engine_with_settings(
        script: serde_json::Value,
        reserve_tokens: u64,
    ) -> (AgentSessionEngine, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        write_compaction_settings(dir.path(), reserve_tokens);
        let engine = AgentSessionEngine::new(AgentEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir: dir.path().join("agent"),
            provider: None,
            model: None,
            api_key: None,
            thinking: None,
            session_dir: None,
            session_file: None,
            faux_script: Some(script.to_string()),
            supervisor_link: None,
            telemetry_disabled: None,
        })
        .unwrap();
        (engine, dir)
    }

    /// Admit one prompt through the engine, collecting its events.
    pub(crate) fn admit(
        engine: &AgentSessionEngine,
        message: String,
        events: &mut Vec<EngineEvent>,
    ) {
        engine.run_prompt(
            0,
            PromptRequest {
                images: Vec::new(),
                message,
                source: "user".to_string(),
                agent_message_id: None,
                custom_message: None,
            },
            &|| false,
            &mut |event| {
                events.push(event);
                true
            },
        );
    }

    /// The automatic threshold compaction at the turn boundary (TS
    /// `_checkCompaction` threshold arm): a settled turn whose usage
    /// crosses the reserve headroom emits the `compaction_start` /
    /// `compaction_end` pair with the `threshold` reason, runs the
    /// summarizer, and rewrites the loop context.
    ///
    /// The faux provider estimates usage from the serialized context (the
    /// f14 battery's mock-provider shape is not part of the faux script),
    /// so the probe engine first measures one baseline turn's usage and the
    /// threshold engine places the headroom halfway between that baseline
    /// and the baseline plus the big prompt (~12k tokens of `x`s) —
    /// environment-independent margins on both sides.
    #[test]
    fn threshold_crossing_auto_compacts_with_the_event_pair() {
        let _faux = FAUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Probe: the baseline turn's total usage (system prompt included).
        let (probe, _probe_dir) = faux_engine_with_settings(
            serde_json::json!({ "responses": [{"text": "seed reply"}] }),
            1,
        );
        let mut probe_events: Vec<EngineEvent> = Vec::new();
        admit(&probe, "seed turn".to_string(), &mut probe_events);
        let baseline = probe_events
            .iter()
            .find_map(|event| match event {
                EngineEvent::AssistantMessage(message) => message["usage"]["totalTokens"].as_u64(),
                _ => None,
            })
            .expect("probe turn produced usage");
        assert!(
            baseline < 100_000,
            "the probe baseline is implausibly large: {baseline}"
        );
        drop(probe);

        // ~12k tokens of deterministic extra context on the crossing turn.
        let big_prompt = format!("seed turn {} crossing", "x".repeat(48_000));
        let big_tokens = (48_000 + "seed turn  crossing".len() as u64).div_ceil(4);
        // The headroom sits between the two turns' usage (the f14 battery
        // shape: reserveTokens so exactly the seeded crossing fires).
        let headroom = baseline + big_tokens / 2;
        let (engine, _engine_dir) = faux_engine_with_settings(
            serde_json::json!({
                "responses": [
                    {"text": "seed reply"},
                    {"text": "crossing reply"},
                    {"text": "the summary"},
                ],
            }),
            128_000u64.saturating_sub(headroom).max(1),
        );

        let mut events: Vec<EngineEvent> = Vec::new();
        // The seed turn stays below the headroom: no compaction events.
        admit(&engine, "seed turn".to_string(), &mut events);
        assert_eq!(
            assistant_texts(&events),
            vec!["seed reply".to_string()],
            "the seed turn answered"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                EngineEvent::CompactionStart { .. } | EngineEvent::Compaction { .. }
            )),
            "no compaction below the headroom"
        );
        // The threshold-crossing turn: the settled usage fires the
        // `compaction_start`/`compaction_end` pair with the `threshold`
        // reason, after the assistant message (TS agent_end order).
        admit(&engine, big_prompt, &mut events);
        let assistant_index = events
            .iter()
            .rposition(|event| matches!(event, EngineEvent::AssistantMessage(_)))
            .expect("assistant message emitted");
        let start_index = events
            .iter()
            .position(|event| {
                matches!(event, EngineEvent::CompactionStart { event } if event["reason"] == "threshold")
            })
            .expect("threshold compaction_start emitted");
        assert!(
            start_index > assistant_index,
            "the check fires at the settled turn boundary"
        );
        let EngineEvent::CompactionStart { event } = &events[start_index] else {
            unreachable!();
        };
        assert_eq!(
            event,
            &serde_json::json!({ "type": "compaction_start", "reason": "threshold" })
        );
        // The durable end event carries the entry and the client-facing
        // result with the summarizer's text (the summarizer consumed the
        // third scripted response).
        let compaction_index = events
            .iter()
            .position(|event| matches!(event, EngineEvent::Compaction { .. }))
            .expect("compaction_end emitted");
        let EngineEvent::Compaction { entry, event } = &events[compaction_index] else {
            unreachable!();
        };
        assert!(compaction_index > start_index);
        assert_eq!(event["reason"], "threshold");
        assert_eq!(event["result"]["summary"], "the summary");
        assert!(entry["firstKeptEntryId"].is_string());
        // Exactly one pair for the admission: the pre-turn check on the
        // first iteration sees no built session (nothing to compact), and
        // the post-turn check fires once — no double compaction.
        let start_count = events
            .iter()
            .filter(|event| matches!(event, EngineEvent::CompactionStart { .. }))
            .count();
        let end_count = events
            .iter()
            .filter(|event| matches!(event, EngineEvent::Compaction { .. }))
            .count();
        assert_eq!((start_count, end_count), (1, 1));
    }

    /// The `compaction_outcome` rows an unsuccessful auto-compaction
    /// records, with the indices of the disclosure pair and the end event
    /// within the event list (the disclosure goes out first, the end event
    /// second — TS `_endCompactionUnsuccessfully`).
    fn outcome_row_and_end_event(
        events: &[EngineEvent],
        expected_reason: &str,
        expected_outcome: &str,
        expected_message: &str,
        expected_severity: &str,
    ) -> (usize, Value) {
        let row_index = events
            .iter()
            .position(|event| {
                matches!(event, EngineEvent::CustomMessage(row) if row["customType"] == "compaction_outcome")
            })
            .expect("the outcome row was broadcast as a custom message");
        let row = match &events[row_index] {
            EngineEvent::CustomMessage(row) => row.clone(),
            _ => unreachable!("matched above"),
        };
        assert_eq!(row["role"], "custom", "the row is a custom message");
        assert_eq!(row["customType"], "compaction_outcome");
        assert_eq!(row["content"], serde_json::json!(expected_message));
        assert_eq!(row["display"], serde_json::json!(true));
        assert_eq!(
            row["details"],
            serde_json::json!({
                "reason": expected_reason,
                "outcome": expected_outcome,
            })
        );
        let end_index = events[row_index + 1..]
            .iter()
            .position(|event| {
                matches!(event, EngineEvent::Compaction { event, .. } if event["type"] == "compaction_end")
            })
            .map(|offset| offset + row_index + 1)
            .expect("the settled compaction_end follows the row");
        let event = match &events[end_index] {
            EngineEvent::Compaction { event, .. } => event.clone(),
            _ => unreachable!("matched above"),
        };
        assert_eq!(event["reason"], serde_json::json!(expected_reason));
        assert_eq!(event["errorMessage"], serde_json::json!(expected_message));
        assert_eq!(event["errorSeverity"], serde_json::json!(expected_severity));
        assert_eq!(event["aborted"], serde_json::json!(false));
        assert_eq!(event["willRetry"], serde_json::json!(false));
        assert!(
            event.get("result").is_none(),
            "no result on an unsuccessful compaction"
        );
        (row_index, event)
    }

    /// The engine session's durable entry chain carries the outcome row.
    fn outcome_row_in_entries(engine: &AgentSessionEngine) -> bool {
        let guard = engine.session.blocking_lock();
        let Some(core) = guard.as_ref() else {
            return false;
        };
        let persistence = core.session.shared_persistence();
        let entries = engine
            .runtime
            .block_on(async { persistence.lock().await.get_entries() });
        entries.iter().any(|entry| {
            matches!(entry, pa_types::session::FileEntry::CustomMessage { payload, .. }
                if payload.custom_type == "compaction_outcome")
        })
    }

    /// The live loop context carries the outcome row (TS
    /// `agent.state.messages.push`); the loop's converter keeps it out of
    /// the provider request.
    fn outcome_row_in_live_context(engine: &AgentSessionEngine) -> bool {
        let guard = engine.session.blocking_lock();
        let Some(core) = guard.as_ref() else {
            return false;
        };
        engine.runtime.block_on(async {
            let state = core.session.agent().state().await;
            state
                .messages
                .last()
                .is_some_and(|message| message.role() == "custom")
        })
    }

    /// The threshold call site (TS `_runAutoCompaction` -> the
    /// `CompactionSkippedError` arm): a threshold compaction that skips
    /// records the durable `compaction_outcome` row, broadcasts its
    /// message pair before the settled `compaction_end` warning, keeps it
    /// in the live context, and never persists a compaction entry.
    #[test]
    fn threshold_skip_records_the_durable_outcome_row() {
        let _faux = FAUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Probe: the baseline turn's total usage (system prompt included).
        let (probe, _probe_dir) = faux_engine_with_settings(
            serde_json::json!({ "responses": [{"text": "seed reply"}] }),
            1,
        );
        let mut probe_events: Vec<EngineEvent> = Vec::new();
        admit(&probe, "seed turn".to_string(), &mut probe_events);
        let baseline = probe_events
            .iter()
            .find_map(|event| match event {
                EngineEvent::AssistantMessage(message) => message["usage"]["totalTokens"].as_u64(),
                _ => None,
            })
            .expect("probe turn produced usage");
        drop(probe);

        // One big crossing turn whose only summarizable history is itself:
        // the threshold fires, and the compaction skips (too short).
        let big_prompt = format!("seed turn {} crossing", "x".repeat(48_000));
        let big_tokens = (48_000 + "seed turn  crossing".len() as u64).div_ceil(4);
        let headroom = baseline + big_tokens / 2;
        let (engine, _engine_dir) = faux_engine_with_settings(
            serde_json::json!({ "responses": [{"text": "crossing reply"}] }),
            128_000u64.saturating_sub(headroom).max(1),
        );
        let mut events: Vec<EngineEvent> = Vec::new();
        admit(&engine, big_prompt, &mut events);
        assert_eq!(
            assistant_texts(&events),
            vec!["crossing reply".to_string()],
            "the crossing turn answered"
        );
        let skip_message =
            "Auto-compaction skipped: Session is too short to compact — try again once it grows";
        let (row_index, _) =
            outcome_row_and_end_event(&events, "threshold", "skipped", skip_message, "warning");
        let start_index = events
            .iter()
            .position(|event| {
                matches!(event, EngineEvent::CompactionStart { event } if event["reason"] == "threshold")
            })
            .expect("threshold compaction_start emitted");
        assert!(
            row_index > start_index,
            "the disclosure pair goes out after the start event"
        );
        // The engine's durable entry chain and the live context both carry
        // the row; no compaction entry was written for the skip.
        assert!(outcome_row_in_entries(&engine));
        assert!(outcome_row_in_live_context(&engine));
        let guard = engine.session.blocking_lock();
        let core = guard.as_ref().expect("session built");
        let persistence = core.session.shared_persistence();
        let has_compaction_entry = engine.runtime.block_on(async {
            persistence
                .lock()
                .await
                .get_entries()
                .iter()
                .any(|entry| matches!(entry, pa_types::session::FileEntry::Compaction { .. }))
        });
        assert!(
            !has_compaction_entry,
            "a skipped compaction persists no compaction entry"
        );
    }

    /// The requested call site (the turn-boundary consumption): a scheduled
    /// `compact.run` request that skips at consumption records the same
    /// durable disclosure with the `requested` reason.
    #[test]
    fn requested_compaction_skip_records_the_durable_outcome_row() {
        let _faux = FAUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::TempDir::new().unwrap();
        let engine = AgentSessionEngine::new(AgentEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir: dir.path().join("agent"),
            provider: None,
            model: None,
            api_key: None,
            thinking: None,
            session_dir: None,
            session_file: None,
            faux_script: Some(
                serde_json::json!({ "responses": [{"text": "seed reply"}, {"text": "second reply"}] })
                    .to_string(),
            ),
            supervisor_link: None,
            telemetry_disabled: None,
        })
        .unwrap();
        let mut events: Vec<EngineEvent> = Vec::new();
        admit(&engine, "turn one".to_string(), &mut events);
        // Schedule a requested compaction (the `compact.run` write path):
        // the boundary consumes it after the next turn settles.
        {
            let guard = engine.session.blocking_lock();
            let core = guard.as_ref().expect("session built");
            engine
                .runtime
                .block_on(async { core.turn_boundary.schedule_compaction(None).await });
        }
        admit(&engine, "turn two".to_string(), &mut events);
        assert_eq!(
            assistant_texts(&events),
            vec!["seed reply".to_string(), "second reply".to_string()],
            "both turns answered"
        );
        outcome_row_and_end_event(
            &events,
            "requested",
            "skipped",
            "Requested compaction skipped: Session is too short to compact — try again once it grows",
            "warning",
        );
        assert!(outcome_row_in_entries(&engine));
        assert!(outcome_row_in_live_context(&engine));
    }

    /// Below the headroom nothing fires: the threshold check stays silent
    /// for turns whose usage fits the default 16k reserve (a 111k headroom
    /// on the 128k window).
    #[test]
    fn threshold_below_the_headroom_stays_silent() {
        let _faux = FAUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::TempDir::new().unwrap();
        let engine = AgentSessionEngine::new(AgentEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir: dir.path().join("agent"),
            provider: None,
            model: None,
            api_key: None,
            thinking: None,
            session_dir: None,
            session_file: None,
            faux_script: Some(
                serde_json::json!({ "responses": [{"text": "plain reply"}] }).to_string(),
            ),
            supervisor_link: None,
            telemetry_disabled: None,
        })
        .unwrap();
        let mut events: Vec<EngineEvent> = Vec::new();
        engine.run_prompt(
            0,
            PromptRequest {
                images: Vec::new(),
                message: "a small turn".to_string(),
                source: "user".to_string(),
                agent_message_id: None,
                custom_message: None,
            },
            &|| false,
            &mut |event| {
                events.push(event);
                true
            },
        );
        assert_eq!(assistant_texts(&events), vec!["plain reply".to_string()]);
        assert!(
            !events.iter().any(|event| matches!(
                event,
                EngineEvent::CompactionStart { .. } | EngineEvent::Compaction { .. }
            )),
            "no compaction events below the headroom"
        );
    }

    /// A prompt with images records the attachments as multimodal content
    /// blocks after the text (TS prompt admission), even when the model
    /// turn itself cannot run.
    #[test]
    fn prompt_images_ride_the_user_message_content() {
        let dir = tempfile::TempDir::new().unwrap();
        let engine = bare_engine(dir.path());
        let mut events: Vec<EngineEvent> = Vec::new();
        engine.run_prompt(
            0,
            PromptRequest {
                images: vec![pa_agent::types::ImageContent {
                    data: "QUJD".to_string(),
                    mime_type: "image/png".to_string(),
                }],
                message: "look at this".to_string(),
                source: "user".to_string(),
                agent_message_id: None,
                custom_message: None,
            },
            &|| false,
            &mut |event| {
                events.push(event);
                true
            },
        );
        let user = events.iter().find_map(|event| match event {
            EngineEvent::UserMessage(message) => Some(message.clone()),
            _ => None,
        });
        let user = user.expect("user message emitted");
        assert_eq!(
            user["content"][0],
            json!({ "type": "text", "text": "look at this" })
        );
        assert_eq!(
            user["content"][1],
            json!({ "type": "image", "data": "QUJD", "mimeType": "image/png" })
        );
    }

    #[test]
    fn settings_default_drives_unflagged_resolution() {
        let dir = tempfile::TempDir::new().unwrap();
        let agent_dir = dir.path().join("agent");
        write_custom_provider_models_json(&agent_dir, "http://127.0.0.1:9");
        let mut settings = pa_core::settings::SettingsManager::create(dir.path(), &agent_dir);
        settings
            .set_default_model_and_provider("battery".into(), "mock-1".into())
            .unwrap();
        let engine = AgentSessionEngine::new(AgentEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir,
            provider: None,
            model: None,
            api_key: None,
            thinking: None,
            session_dir: None,
            session_file: None,
            faux_script: None,
            supervisor_link: None,
            telemetry_disabled: None,
        })
        .unwrap();
        let model = engine.resolve_registry_model().expect("resolved model");
        assert_eq!(model.provider, "battery");
        assert_eq!(model.id, "mock-1");
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
            thinking: None,
            session_dir: None,
            session_file: None,
            faux_script: None,
            supervisor_link: None,
            telemetry_disabled: None,
        })
        .unwrap();
        // A create config with only a model keeps the provider and key.
        engine.configure_model(EngineModelSelection {
            provider: None,
            model: Some("mock-1".to_string()),
            api_key: None,
            thinking: None,
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
        // One auth-configured model keeps the available list non-empty in
        // every environment (a clean env with no credentials resolves to
        // "No models available" before the flagged-provider error, while a
        // machine with ambient env credentials reaches this test's branch).
        std::fs::create_dir_all(dir.path().join("agent")).unwrap();
        std::fs::write(
            dir.path().join("agent").join("models.json"),
            serde_json::json!({
                "providers": {
                    "battery": {
                        "api": "openai-completions",
                        "baseUrl": "http://127.0.0.1:9",
                        "apiKey": "sk-battery",
                        "models": [
                            { "id": "mock-1", "contextWindow": 128000, "maxTokens": 4096 }
                        ]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let engine = AgentSessionEngine::new(AgentEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir: dir.path().join("agent"),
            provider: Some("no-such-provider".to_string()),
            model: Some("some-model".to_string()),
            api_key: None,
            thinking: None,
            session_dir: None,
            session_file: None,
            faux_script: None,
            supervisor_link: None,
            telemetry_disabled: None,
        })
        .unwrap();
        let mut events: Vec<EngineEvent> = Vec::new();
        engine.run_prompt(
            0,
            PromptRequest {
                images: Vec::new(),
                message: "hi".to_string(),
                source: "user".to_string(),
                agent_message_id: None,
                custom_message: None,
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

    /// A reasoning models.json model (no thinkingLevelMap): supported
    /// levels are off..high, so a requested max clamps to high.
    #[test]
    fn configure_model_thinking_clamps_to_the_models_supported_levels() {
        let dir = tempfile::TempDir::new().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("models.json"),
            serde_json::json!({
                "providers": {
                    "battery": {
                        "api": "openai-completions",
                        "baseUrl": "http://127.0.0.1:9",
                        "apiKey": "sk-battery",
                        "models": [
                            {
                                "id": "mock-1",
                                "reasoning": true,
                                "contextWindow": 128000,
                                "maxTokens": 4096
                            }
                        ]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let engine = AgentSessionEngine::new(AgentEngineConfig {
            cwd: dir.path().to_path_buf(),
            agent_dir,
            provider: Some("battery".to_string()),
            model: Some("mock-1".to_string()),
            api_key: None,
            thinking: None,
            session_dir: None,
            session_file: None,
            faux_script: None,
            supervisor_link: None,
            telemetry_disabled: None,
        })
        .unwrap();
        // Without an explicit flag the TS default applies (medium, clamped).
        assert_eq!(engine.effective_thinking_level().as_deref(), Some("medium"));
        // The create-config flag is authoritative, clamped to model support.
        engine.configure_model(EngineModelSelection {
            provider: None,
            model: None,
            api_key: None,
            thinking: Some(pa_types::ai::ModelThinkingLevel::Max),
        });
        assert_eq!(engine.effective_thinking_level().as_deref(), Some("high"));
        engine.configure_model(EngineModelSelection {
            provider: None,
            model: None,
            api_key: None,
            thinking: Some(pa_types::ai::ModelThinkingLevel::Low),
        });
        assert_eq!(engine.effective_thinking_level().as_deref(), Some("low"));
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

/// A driver loop test harness: faux script + collected events. Holds the
/// faux lock while the engine runs.
#[cfg(test)]
fn run_prompts(
    script: serde_json::Value,
    prompts: &[&str],
) -> (AgentSessionEngine, Vec<EngineEvent>) {
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::TempDir::new().unwrap();
    let engine = AgentSessionEngine::new(AgentEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        provider: None,
        model: None,
        api_key: None,
        thinking: None,
        session_dir: None,
        session_file: None,
        faux_script: Some(script.to_string()),
        supervisor_link: None,
        telemetry_disabled: None,
    })
    .unwrap();
    let mut events: Vec<EngineEvent> = Vec::new();
    for prompt in prompts {
        engine.run_prompt(
            0,
            PromptRequest {
                images: Vec::new(),
                message: prompt.to_string(),
                source: "user".to_string(),
                agent_message_id: None,
                custom_message: None,
            },
            &|| false,
            &mut |event| {
                events.push(event);
                true
            },
        );
    }
    (engine, events)
}

/// The user rows emitted by one run (message texts in order).
#[cfg(test)]
fn user_texts(events: &[EngineEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::UserMessage(value) => Some(
                value["content"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            ),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
fn assistant_texts(events: &[EngineEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::AssistantMessage(value) => Some(
                value["content"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            ),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
fn custom_rows(events: &[EngineEvent]) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::CustomMessage(value) => Some(value.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn assistant_updates_stream_live_while_the_turn_runs() {
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::TempDir::new().unwrap();
    // A paced script: 40 short words at 100 tokens/second streams for
    // roughly 0.4s wall time. If the engine buffered events until the turn
    // settled, every update would share one emit timestamp; live
    // forwarding spreads them across the stream.
    let words = (0..40).map(|i| format!("w{i} ")).collect::<String>();
    let script = serde_json::json!({
        "engine": "faux",
        "tokensPerSecond": 100.0,
        "responses": [
            {"content": [{"type": "text", "text": words}]}
        ],
    });
    let engine = AgentSessionEngine::new(AgentEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        provider: None,
        model: None,
        api_key: None,
        thinking: None,
        session_dir: None,
        session_file: None,
        faux_script: Some(script.to_string()),
        supervisor_link: None,
        telemetry_disabled: None,
    })
    .unwrap();
    let start = std::time::Instant::now();
    let mut updates: Vec<(std::time::Duration, usize)> = Vec::new();
    engine.run_prompt(
        0,
        PromptRequest {
            images: Vec::new(),
            message: "hi".to_string(),
            source: "user".to_string(),
            agent_message_id: None,
            custom_message: None,
        },
        &|| false,
        &mut |event| {
            if let EngineEvent::AssistantUpdate { message, .. } = &event {
                let text_len = message["content"]
                    .as_array()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .map(|block| {
                                block
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .map_or(0, str::len)
                            })
                            .sum()
                    })
                    .unwrap_or(0);
                updates.push((start.elapsed(), text_len));
            }
            true
        },
    );
    assert!(
        updates.len() >= 10,
        "the paced stream must produce many updates, got {}",
        updates.len()
    );
    let first = updates.first().unwrap().0;
    let last = updates.last().unwrap().0;
    assert!(
        (last - first) >= std::time::Duration::from_millis(200),
        "updates must spread across the stream, got {first:?}..{last:?}"
    );
    // Content grows monotonically: every update carries the full partial
    // message, so lengths never regress.
    let lengths: Vec<usize> = updates.iter().map(|(_, len)| *len).collect();
    let mut monotonic = lengths.clone();
    monotonic.sort_unstable();
    assert_eq!(lengths, monotonic, "partial message lengths regress");
    // The settled final message arrives too (message_end, not just updates).
    let final_len = lengths.last().copied().unwrap_or(0);
    assert!(final_len >= 40 * 3, "final partial is the full text");
}

/// The wire events one `/compact` produced, in order: the compaction
/// event pair around the durable rows.
#[cfg(test)]
fn compaction_events(events: &[EngineEvent]) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::CompactionStart { event } | EngineEvent::Compaction { event, .. } => {
                Some(event.clone())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn compact_session_command_emits_the_ts_event_pair_on_a_skip() {
    let (_engine, events) = run_prompts(
        serde_json::json!({ "responses": ["unused"] }),
        &["/compact"],
    );
    // The echo row precedes the events (TS `_executeSelectedSessionCommand`
    // records it before the queue runs the command); a skip records no
    // result row.
    let rows = custom_rows(&events);
    assert_eq!(rows.len(), 1, "echo only, no result row: {rows:?}");
    assert_eq!(rows[0]["customType"], "session_slash_command");
    assert_eq!(rows[0]["content"], "/compact");
    // The event pair: start, then the settled skip warning.
    let compaction = compaction_events(&events);
    assert_eq!(compaction.len(), 2, "start + end: {compaction:?}");
    assert_eq!(
        compaction[0],
        serde_json::json!({ "type": "compaction_start", "reason": "manual" })
    );
    assert_eq!(
        compaction[1],
        serde_json::json!({
            "type": "compaction_end",
            "reason": "manual",
            "aborted": false,
            "willRetry": false,
            "errorMessage": "Session is too short to compact \u{2014} try again once it grows",
            "errorSeverity": "warning",
        })
    );
    assert_eq!(events.last(), Some(&EngineEvent::Done(Ok(()))));
}

#[test]
fn compact_session_command_emits_the_result_on_success() {
    // Two big turns (each ~12k tokens by the chars/4 estimate) push the
    // history past the keep-recent budget: the cut keeps the last turn,
    // the summarizer (the third queued faux response) covers the first.
    let filler = "history ".repeat(6_000); // ~48k chars = ~12k tokens each
    let (_engine, events) = run_prompts(
        serde_json::json!({
            "responses": [
                { "text": filler.clone() },
                { "text": filler.clone() },
                { "text": "## Summary\nthe session story" },
            ]
        }),
        &["first", "second", "/compact focus on the goal"],
    );
    let compaction = compaction_events(&events);
    assert_eq!(compaction.len(), 2, "{compaction:?}");
    assert_eq!(
        compaction[0],
        serde_json::json!({
            "type": "compaction_start",
            "reason": "manual",
            "customInstructions": "focus on the goal",
        })
    );
    let end = &compaction[1];
    assert_eq!(end["type"], "compaction_end");
    assert_eq!(end["reason"], "manual");
    assert_eq!(end["aborted"], false);
    assert_eq!(end["customInstructions"], "focus on the goal");
    let result = end["result"].as_object().expect("the result payload");
    assert_eq!(result["summary"], "## Summary\nthe session story");
    assert!(result["tokensBefore"].as_u64().unwrap_or_default() > 0);
    // The durable rows stay minimal (TS's queued `/compact` catch arm
    // records no result row): the echo row is the only custom row.
    let rows = custom_rows(&events);
    assert_eq!(rows.len(), 1, "the /compact echo only: {rows:?}");
    assert_eq!(rows[0]["customType"], "session_slash_command");
}

#[test]
fn autonomous_on_enables_the_driver_loop() {
    let (engine, events) = run_prompts(
        serde_json::json!({ "responses": ["unused"] }),
        &["/autonomous on --max-continuations 1 --max-turns 5"],
    );
    // The enable prompt runs the session command (echo + status rows) and
    // never admits a model turn.
    let status = custom_rows(&events);
    assert!(status
        .iter()
        .any(|row| row["customType"] == "autonomous_status"
            && row["content"]
                .as_str()
                .unwrap_or_default()
                .starts_with("[autonomous-status: on]")));
    assert_eq!(assistant_texts(&events), Vec::<String>::new());
    let state = engine.autonomous.blocking_lock();
    assert!(state.enabled);
    assert_eq!(state.limits.max_continuations, 1);
    assert_eq!(state.limits.max_turns, 5);
}

#[test]
fn autonomous_limit_stops_the_run_with_durable_stop_row() {
    let (engine, events) = run_prompts(
        serde_json::json!({ "responses": ["first", "second"] }),
        &["/autonomous on --max-continuations 1 --max-turns 5", "go"],
    );
    // Turn 1 continues (missing terminal evidence), turn 2 hits the
    // continuation cap: one injected continuation, then the stop row.
    assert_eq!(assistant_texts(&events), vec!["first", "second"]);
    let texts = user_texts(&events);
    assert_eq!(
        texts,
        vec![
            "go".to_string(),
            "[autonomous-continuation]\n\nNo human input is available in autonomous mode. Continue working until the host evaluator, verifier, or configured autonomous limits stop the run. If you were asking the user a question, make a reasonable assumption and verify it. If you believe you are blocked, prove it with host-observable evidence, preserve that evidence, and keep looking for safe progress while budget remains. Do not end the session yourself; the verifier/evaluator decides completion when configured gates pass.".to_string()
        ]
    );
    let stop = custom_rows(&events)
        .into_iter()
        .find(|row| {
            row["content"]
                .as_str()
                .unwrap_or_default()
                .starts_with("[autonomous-stop:")
        })
        .expect("durable stop row");
    assert!(stop["content"]
        .as_str()
        .unwrap()
        .starts_with("[autonomous-stop: limit-reached] maxContinuations reached (1/1)"));
    assert_eq!(stop["details"]["stopReason"], "maxContinuations");
    assert_eq!(stop["details"]["enabled"], true);
    assert_eq!(events.last(), Some(&EngineEvent::Done(Ok(()))));
    // Per-turn usage accounting: two settled turns.
    let state = engine.autonomous.blocking_lock();
    assert_eq!(state.turns_used, 2);
    assert_eq!(state.continuations_used, 1);
}

#[test]
fn autonomous_gate_pass_and_failure_drive_the_loop() {
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // The gate passes only on its second run (a counter file in the cwd).
    let dir = tempfile::TempDir::new().unwrap();
    let gate = format!(
        "n=$(cat {0}/cnt 2>/dev/null || echo 0); echo $((n+1)) > {0}/cnt; [ $n -ge 1 ]",
        dir.path().display()
    );
    let engine = AgentSessionEngine::new(AgentEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        provider: None,
        model: None,
        api_key: None,
        thinking: None,
        session_dir: None,
        session_file: None,
        faux_script: Some(
            serde_json::json!({ "responses": ["first attempt", "fixed it"] }).to_string(),
        ),
        supervisor_link: None,
        telemetry_disabled: None,
    })
    .unwrap();
    let on = format!("/autonomous on --gate {gate:?}");
    let mut events: Vec<EngineEvent> = Vec::new();
    for prompt in [on.as_str(), "go"] {
        engine.run_prompt(
            0,
            PromptRequest {
                images: Vec::new(),
                message: prompt.to_string(),
                source: "user".to_string(),
                agent_message_id: None,
                custom_message: None,
            },
            &|| false,
            &mut |event| {
                events.push(event);
                true
            },
        );
    }
    // Turn 1 fails the gate -> gate-failure continuation; turn 2 passes ->
    // gate-passed stop row.
    assert_eq!(assistant_texts(&events), vec!["first attempt", "fixed it"]);
    let texts = user_texts(&events);
    assert_eq!(texts.len(), 2);
    assert!(texts[1].starts_with("[autonomous-continuation: gate-failed]"));
    assert!(texts[1].contains("exited with code 1"));
    let stop = custom_rows(&events)
        .into_iter()
        .find(|row| {
            row["content"]
                .as_str()
                .unwrap_or_default()
                .starts_with("[autonomous-stop: gate-passed]")
        })
        .expect("gate-passed stop row");
    assert_eq!(stop["details"]["stopReason"], "gate_passed");
    assert_eq!(
        stop["details"]["gates"]["commands"][0],
        serde_json::json!(gate)
    );
    assert_eq!(events.last(), Some(&EngineEvent::Done(Ok(()))));
}

/// A scripted policy driver: the engine must inject exactly what the trait
/// returns, consult it after every turn, and account every settled message.
#[cfg(test)]
struct ScriptedDriver {
    /// Pops from the end, so reverse the desired order when building.
    follow_ups: std::sync::Mutex<Vec<pa_core::autonomous::AutonomousFollowUp>>,
    accounted: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl pa_core::autonomous::AutonomousDriver for ScriptedDriver {
    fn account_message(
        &self,
        _state: &mut pa_core::autonomous::AutonomousRuntimeState,
        message: &pa_types::ai::AssistantMessage,
    ) {
        assert_ne!(message.stop_reason, pa_types::ai::StopReason::Error);
        self.accounted
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn after_turn<'a>(
        &'a self,
        _state: &'a mut pa_core::autonomous::AutonomousRuntimeState,
        _message: &'a pa_types::ai::AssistantMessage,
    ) -> pa_core::autonomous::AutonomousFollowUpFuture<'a> {
        let next = self
            .follow_ups
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(pa_core::autonomous::AutonomousFollowUp::Inactive);
        Box::pin(async move { next })
    }
}

#[test]
fn the_turn_loop_is_driven_by_the_driver_trait() {
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::TempDir::new().unwrap();
    let engine = AgentSessionEngine::new(AgentEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        provider: None,
        model: None,
        api_key: None,
        thinking: None,
        session_dir: None,
        session_file: None,
        faux_script: Some(serde_json::json!({ "responses": ["one", "two"] }).to_string()),
        supervisor_link: None,
        telemetry_disabled: None,
    })
    .unwrap();
    let status = pa_core::autonomous::autonomous_status(&engine.autonomous.blocking_lock());
    // The queue pops from the end: the continuation is consulted first,
    // the stop on the second settled turn.
    let driver = std::sync::Arc::new(ScriptedDriver {
        follow_ups: std::sync::Mutex::new(vec![
            pa_core::autonomous::AutonomousFollowUp::Stop {
                reason: pa_core::autonomous::AutonomousStopReason::Limit(
                    pa_core::autonomous::AutonomousLimitReason::MaxTurns,
                ),
                status: Box::new(status),
            },
            pa_core::autonomous::AutonomousFollowUp::Continue {
                text: "scripted continuation".to_string(),
            },
        ]),
        accounted: std::sync::atomic::AtomicUsize::new(0),
    });
    engine
        .set_autonomous_driver(std::sync::Arc::clone(&driver)
            as std::sync::Arc<dyn pa_core::autonomous::AutonomousDriver>);
    let mut events: Vec<EngineEvent> = Vec::new();
    engine.run_prompt(
        0,
        PromptRequest {
            images: Vec::new(),
            message: "go".to_string(),
            source: "user".to_string(),
            agent_message_id: None,
            custom_message: None,
        },
        &|| false,
        &mut |event| {
            events.push(event);
            true
        },
    );
    // The engine holds no autonomous logic of its own: the injected text,
    // the stop row, and the turn count come straight from the trait.
    assert_eq!(
        user_texts(&events),
        vec!["go".to_string(), "scripted continuation".to_string()]
    );
    assert_eq!(
        assistant_texts(&events),
        vec!["one".to_string(), "two".to_string()]
    );
    let stop = custom_rows(&events)
        .into_iter()
        .find(|row| {
            row["content"]
                .as_str()
                .unwrap_or_default()
                .starts_with("[autonomous-stop:")
        })
        .expect("durable stop row");
    assert_eq!(stop["details"]["stopReason"], "maxTurns");
    assert_eq!(events.last(), Some(&EngineEvent::Done(Ok(()))));
    // Per-message accounting ran through the trait for both settled turns.
    assert_eq!(
        driver.accounted.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
}

#[test]
fn agent_engine_streams_updates_and_final_message() {
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::TempDir::new().unwrap();
    // Scoped env: the faux seam is process-global; keep the test isolated.
    let engine = AgentSessionEngine::new(AgentEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        provider: None,
        model: None,
        api_key: None,
        thinking: None,
        session_dir: None,
        session_file: None,
        faux_script: Some(serde_json::json!({ "responses": ["streamed answer"] }).to_string()),
        supervisor_link: None,
        telemetry_disabled: None,
    })
    .unwrap();
    let mut events: Vec<EngineEvent> = Vec::new();
    engine.run_prompt(
        0,
        PromptRequest {
            images: Vec::new(),
            message: "hi".to_string(),
            source: "user".to_string(),
            agent_message_id: None,
            custom_message: None,
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
        pa_agent::types::AgentMessage::Standard(LoopMessage::ToolResult(tool_result)) => {
            pa_types::session::AgentMessage::ToolResult(json_round_trip(tool_result)?)
        }
        _ => return None,
    };
    serde_json::to_value(&session_message).ok()
}
