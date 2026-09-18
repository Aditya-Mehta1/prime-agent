//! SessionEngine assembly: build a running agent session from a config.
//! This is the facade pa-cli/pa-daemon call — the Rust equivalent of the
//! `createAgentSession` wiring: resources, prompt, model, tools, loop, and
//! persistence. The session subscribes persistence listeners on the caller's
//! reactor, so `create_session` is async.

use std::path::PathBuf;
use std::sync::Arc;

use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::stream::StreamFn;
use pa_agent::types::{Model, ThinkingLevel};

use crate::resources::{load_resources, ResourceLoaderOptions};
use crate::session::manager::SessionManager;
use crate::skills::PromptTemplate;
use pa_types::session::FileEntry;

use pa_telemetry::base_properties;

use super::{AgentSession, PromptOptions, PromptOutcome};

/// Everything needed to assemble a session.
#[derive(Default)]
pub struct SessionEngineConfig {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    /// Resolved model (registry output).
    pub model: Option<Model>,
    /// Thinking level for the session.
    pub thinking_level: Option<ThinkingLevel>,
    /// Provider seam for the loop (required; wire a real provider here).
    pub stream_fn: Option<StreamFn>,
    /// Pre-bridged loop tools (bash/edit/ipython + extensions).
    pub tools: Vec<Arc<dyn pa_agent::types::AgentTool>>,
    /// Override the default system prompt.
    pub custom_system_prompt: Option<String>,
    /// Prompt guideline bullets.
    pub prompt_guidelines: Vec<String>,
    /// Enabled generic MCP server names.
    pub generic_mcp_servers: Vec<String>,
    /// Suppress the rlm recursion guidance.
    pub allow_recursion: Option<bool>,
    /// Session persistence (in-memory when None).
    pub session_manager: Option<SessionManager>,
    /// Extra kernel host-request handlers (e.g. the daemon's message/observe
    /// bridges), merged over the built-in goal/heartbeat registrations.
    pub extra_host_handlers: Option<crate::kernel::shared::HostRequestHandlers>,
    /// Conversation-log path for the system prompt when the caller owns
    /// persistence outside the session manager (the daemon worker mirrors
    /// entries into its own session file).
    pub conversation_log_path: Option<PathBuf>,
    /// Extra skill paths.
    pub additional_skill_paths: Vec<String>,
    /// Extra prompt-template paths.
    pub additional_prompt_paths: Vec<String>,
    /// Force-exclude patterns for built-in skills (e.g. unauthenticated
    /// integrations); the MCP manager seam.
    pub extra_builtin_skill_overrides: Vec<String>,
    /// Daemon child-session host backing the `rlm.*` recursion surface.
    pub rlm_subagent_host: Option<Arc<dyn super::rlm_host::RlmSubagentHost>>,
    /// The session's depth in the RLM recursion tree (0 for top-level
    /// sessions). Gates the `refine.*` host requests, like the TS
    /// `_autoRefineAllowedForSession` depth check.
    pub rlm_depth: Option<u32>,
    /// The full registry model (input modalities for `model.info`); the
    /// engine derives minimal facts from `model` when absent.
    pub model_info: Option<pa_types::ai::Model>,
    /// CLI `--extension` sources (repeatable): resolved through the
    /// package manager into the session's extension paths (temporary
    /// scope, first-wins against configured/discovered extensions).
    pub cli_extension_sources: Vec<String>,
    /// Optional name allow-list for extension tools (`--tools`, TS
    /// `isAllowedTool`); an absent list allows every registered tool.
    pub extension_tool_allow_list: Option<Vec<String>>,
    /// Session telemetry wiring (PostHog client + execution mode). `None`
    /// (opt-out) installs nothing; non-depth-0 sessions never install.
    pub telemetry: Option<super::telemetry::TelemetryWiring>,
    /// An externally owned MCP manager (the daemon worker's session store):
    /// the engine adopts it instead of building its own, so ACP-admitted
    /// servers reach the prompt's MCP gating through the same store the
    /// `replace_acp_mcp_servers` command writes.
    pub mcp_manager: Option<std::sync::Arc<std::sync::Mutex<crate::mcp::McpManager>>>,
}

/// An assembled, running session.
pub struct SessionEngine {
    pub session: AgentSession,
    pub skills: Vec<crate::skills::Skill>,
    pub prompt_templates: Vec<PromptTemplate>,
    pub agents_files: Vec<crate::resources::ContextFile>,
    pub system_prompt: String,
    /// The session's goal driver: the same instance the kernel `goal.*`
    /// host handlers reach, so `/goal` and `goal.complete()` in the kernel
    /// observe one state machine.
    pub goal_driver: std::sync::Arc<tokio::sync::Mutex<super::goal_driver::GoalDriver>>,
    /// The session's MCP manager: host-side auth gating and the source the
    /// `mcp.*` kernel host handlers (config/refresh) resolve against. The
    /// daemon's `replace_acp_mcp_servers` wire command reaches it through
    /// this field (shared handle: the daemon worker and the engine gate
    /// prompts through one store).
    pub mcp_manager: std::sync::Arc<std::sync::Mutex<crate::mcp::McpManager>>,
    /// The extension runner when any extension loaded (sidecar host +
    /// registration mirror); `None` keeps the no-extension fast path
    /// byte-identical (cache-prefix stability).
    pub extension_runner: Option<Arc<crate::extensions::ExtensionRunner>>,
    /// Non-fatal startup diagnostics from extension loading (missing
    /// node, per-path load errors, spawn failures). TS surfaces these in
    /// startup notices.
    pub extension_diagnostics: Vec<String>,
    /// The turn-boundary request surface (`compact.*`/`refine.*`/
    /// `model.info` host requests and the pending requests the turn loop
    /// consumes after a settled turn).
    pub turn_boundary: std::sync::Arc<super::turn_boundary::TurnBoundaryRequests>,
    /// Installed session telemetry (agent-event subscriber). `None` when
    /// telemetry is disabled or the session is not depth 0.
    pub telemetry: Option<std::sync::Arc<super::telemetry::SessionTelemetry>>,
}

/// Resolve the MCP gating the resource loader and prompt need: skill
/// overrides for built-in integrations the user is not logged into, plus the
/// enabled persistent generic servers (prompt `mcp` guidance).
async fn mcp_gating(
    settings: &crate::settings::SettingsManager,
    agent_dir: std::path::PathBuf,
) -> anyhow::Result<(Vec<String>, Vec<String>, crate::mcp::McpManager)> {
    let user_servers = settings
        .settings()
        .mcp_servers
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(server, config)| {
            serde_json::from_value(config)
                .ok()
                .map(|parsed| (server, parsed))
        })
        .collect::<std::collections::HashMap<String, crate::mcp::McpServerConfig>>();
    // The MCP manager snapshots auth with a blocking lock; run it off the
    // async runtime (session construction is async). The manager stays
    // alive on the session: it is the source for the `mcp.*` host
    // requests the kernel sends while serving generic MCP servers.
    tokio::task::spawn_blocking(move || mcp_gating_blocking(user_servers, &agent_dir))
        .await
        .map_err(|error| anyhow::anyhow!("MCP gating task failed: {error}"))
}

fn mcp_gating_blocking(
    user_servers: std::collections::HashMap<String, crate::mcp::McpServerConfig>,
    agent_dir: &std::path::Path,
) -> (Vec<String>, Vec<String>, crate::mcp::McpManager) {
    crate::mcp::McpManager::prompt_gating(user_servers, agent_dir)
}

/// Assemble a session: load resources, build the system prompt, and start the
/// loop with persistence wiring.
pub async fn create_session(mut config: SessionEngineConfig) -> anyhow::Result<SessionEngine> {
    let cwd = config.cwd.clone();
    // Session persistence first: the conversation-log path and the resume
    // context both come from the session manager (TS `_rebuildSystemPrompt`
    // reads `sessionManager.getSessionFile()`).
    let session_manager = config
        .session_manager
        .unwrap_or_else(|| SessionManager::in_memory(&cwd));
    let conversation_log = {
        let session = &session_manager;
        session
            .get_session_file()
            .map(|path| path.display().to_string())
            .or_else(|| {
                config
                    .conversation_log_path
                    .as_ref()
                    .map(|path| path.display().to_string())
            })
    };
    let wiring = super::runtime_wiring::wire_session_runtime(
        session_manager,
        &config.agent_dir,
        super::runtime_wiring::RlmWiring {
            model_registry: None,
            subagent_host: config.rlm_subagent_host.clone(),
        },
    );

    let settings = crate::settings::SettingsManager::create(&cwd, &config.agent_dir);
    let service_tier_preference = settings.get_default_service_tier();
    // Captured before `settings` moves into the resource loader: the
    // compaction scheduling budget (`compact.run` prepare check).
    let compaction_settings = settings.settings().compaction.clone().unwrap_or_default();
    let (mcp_skill_overrides, mcp_generic_servers, built_manager) =
        mcp_gating(&settings, config.agent_dir.clone()).await?;
    let mcp_manager = config
        .mcp_manager
        .take()
        .unwrap_or_else(|| std::sync::Arc::new(std::sync::Mutex::new(built_manager)));
    let mut extra_builtin_skill_overrides = config.extra_builtin_skill_overrides.clone();
    extra_builtin_skill_overrides.extend(mcp_skill_overrides);
    let mut generic_mcp_servers = config.generic_mcp_servers.clone();
    for server in mcp_generic_servers {
        if !generic_mcp_servers.contains(&server) {
            generic_mcp_servers.push(server);
        }
    }
    let resources = load_resources(ResourceLoaderOptions {
        cwd: cwd.clone(),
        agent_dir: config.agent_dir.clone(),
        settings: Some(settings),
        additional_extension_sources: config.cli_extension_sources.clone(),
        extra_builtin_skill_overrides,
        additional_skill_paths: config.additional_skill_paths.clone(),
        additional_prompt_paths: config.additional_prompt_paths.clone(),
        no_skills: false,
        no_prompt_templates: false,
        no_context_files: false,
        system_prompt: config.custom_system_prompt.clone(),
        append_system_prompt: Vec::new(),
        ..Default::default()
    })?;

    let model = config
        .model
        .ok_or_else(|| anyhow::anyhow!("a resolved model is required"))?;
    let stream_fn = config
        .stream_fn
        .ok_or_else(|| anyhow::anyhow!("a provider stream_fn is required"))?;
    // `model.info` facts, captured before the model moves into the loop.
    let model_info = match config.model_info.clone() {
        Some(full) => super::turn_boundary::ModelInfo {
            id: full.id,
            provider: full.provider,
            input: full.input,
        },
        None => super::turn_boundary::ModelInfo {
            id: model.id.clone(),
            provider: model.provider.clone(),
            input: Vec::new(),
        },
    };
    // The context window the usage estimate and status rows read.
    let model_context_window = model.context_window;

    // The runtime wiring: goal/rlm-heartbeat host handlers ride the kernel
    // provisioner, and the agent gains the `ipython` tool backed by that
    // kernel (unless the caller supplied one).
    let python_skills = super::runtime_wiring::kernel_python_skills(&resources.skills);
    let session_id = wiring.session.lock().await.get_session_id().to_string();
    let mut handlers = wiring.handlers.clone();
    if let Some(extra) = config.extra_host_handlers.clone() {
        handlers.merge(extra);
    }
    // The `mcp.*` host requests (config/refresh/begin_login) the kernel's
    // generic MCP registry sends while listing or calling generic servers.
    // Telemetry reports connector usage (server name + action only) when the
    // session is telemetry-enabled; set before the handlers register so
    // their closures capture the reporter.
    {
        let mut manager = mcp_manager.lock().unwrap();
        if let Some(wiring) = &config.telemetry {
            let client = wiring.client.clone();
            let execution_mode = wiring
                .execution_mode
                .clone()
                .unwrap_or_else(|| super::telemetry::EXECUTION_MODE_UNKNOWN.to_string());
            manager.set_usage_report(Some(std::sync::Arc::new(move |action, server| {
                let mut properties = base_properties(&execution_mode);
                properties.set("action", serde_json::Value::from(action));
                properties.set("server_name", serde_json::Value::from(server));
                client.track("mcp connector used", properties);
            })));
        }
        manager.register_host_handlers(&mut handlers);
    }
    // The turn-boundary surface: `model.info` always; `compact.*` behind
    // the compaction `agentCallable` setting; `refine.*` behind the TS
    // `_autoRefineAllowedForSession` gate (depth 0 with a local harness
    // state dir, i.e. exactly the sessions the refine skill targets).
    let turn_boundary = Arc::new(super::turn_boundary::TurnBoundaryRequests::new());
    turn_boundary.register_model_info_handler(&mut handlers, model_info.clone());
    let keep_recent_tokens = compaction_settings
        .keep_recent_tokens
        .unwrap_or(super::compaction::DEFAULT_KEEP_RECENT_TOKENS);
    if compaction_settings.agent_callable.unwrap_or(true) {
        turn_boundary.register_compact_handlers(&mut handlers, keep_recent_tokens);
    }
    let local_harness_dir = wiring
        .session
        .lock()
        .await
        .get_session_artifact_dir()
        .or_else(|| {
            config
                .conversation_log_path
                .as_deref()
                .and_then(super::harness_digest::local_harness_dir_for_log)
        });
    if config.rlm_depth.unwrap_or(0) == 0 && local_harness_dir.is_some() {
        turn_boundary.register_refine_handlers(&mut handlers);
    }
    // `kernel bootstrap` telemetry: the provisioner reports every actual
    // boot (duration, cold/revived, outcome) through the session's client.
    let on_bootstrap_result = config.telemetry.as_ref().map(|wiring| {
        let client = wiring.client.clone();
        let execution_mode = wiring
            .execution_mode
            .clone()
            .unwrap_or_else(|| super::telemetry::EXECUTION_MODE_UNKNOWN.to_string());
        std::sync::Arc::new(
            move |stats: crate::kernel::provisioner::KernelBootstrapStats| {
                let mut properties = base_properties(&execution_mode);
                properties.set("cold", serde_json::Value::from(stats.cold));
                properties.set(
                    "outcome",
                    serde_json::Value::from(match stats.outcome {
                        crate::kernel::provisioner::KernelBootstrapOutcome::Ready => "success",
                        crate::kernel::provisioner::KernelBootstrapOutcome::Error => "error",
                    }),
                );
                properties.set("duration_ms", serde_json::Value::from(stats.duration_ms));
                client.track("kernel bootstrap", properties);
            },
        ) as crate::kernel::provisioner::KernelBootstrapResultHandler
    });
    let provisioner = super::runtime_wiring::kernel_provisioner(
        session_id,
        handlers,
        python_skills,
        &config.agent_dir,
        on_bootstrap_result,
    );
    let mut tools = config.tools.clone();
    // Extension loading (design doc §3.2, stage 2): discovery already
    // resolved the paths; the sidecar loads modules and lands the
    // registrations. A session with zero extension paths never spawns the
    // sidecar (fast-path parity) and nothing below changes.
    let mut extension_diagnostics = Vec::new();
    let extension_runner = if resources.extension_paths.is_empty() {
        None
    } else {
        let mut spec =
            crate::extensions::ExtensionHostSpec::new(cwd.clone(), config.agent_dir.clone());
        spec.extension_paths = resources.extension_paths.clone();
        match crate::extensions::ExtensionRunner::start(spec).await {
            Ok(runner) => {
                for error in runner.load_errors() {
                    extension_diagnostics.push(format!(
                        "Failed to load extension {}: {}",
                        error.path, error.error
                    ));
                }
                let tools_to_bridge = runner
                    .bridge_tools(config.extension_tool_allow_list.as_deref())
                    .await;
                // TS `_refreshToolRegistry`: extension tools replace
                // same-named tools (an extension may override a built-in).
                for tool in tools_to_bridge {
                    if let Some(existing) = tools.iter().position(|t| t.name() == tool.name()) {
                        tools[existing] = tool;
                    } else {
                        tools.push(tool);
                    }
                }
                Some(std::sync::Arc::new(runner))
            }
            Err(error) => {
                // A spawn/handshake failure degrades to no extensions
                // (design doc §2.4 crash isolation); it is never fatal.
                extension_diagnostics.push(format!("Extensions unavailable: {error:#}"));
                None
            }
        }
    };
    if !tools.iter().any(|tool| tool.name() == "ipython") {
        let definition = crate::tools::ipython::create_ipython_tool_definition(
            &cwd.to_string_lossy(),
            super::runtime_wiring::ipython_tool_options(provisioner),
        );
        tools.push(Arc::new(
            crate::session_engine::tool_bridge::ToolDefinitionBridge::new(definition),
        ));
    }
    let active_tool_names: Vec<String> = tools.iter().map(|tool| tool.name().to_string()).collect();

    // Extension tool prompt guidelines flow into the prompt exactly like
    // TS `_rebuildSystemPrompt` (agent-session.ts L5091+): normalized
    // guidelines of the active tools append to the configured ones.
    let mut prompt_guidelines = config.prompt_guidelines.clone();
    if let Some(runner) = &extension_runner {
        let guidelines = runner.registry().await.prompt_guidelines();
        for guideline in guidelines {
            if !prompt_guidelines.contains(&guideline) {
                prompt_guidelines.push(guideline);
            }
        }
    }

    // The per-model prompt layer keys on the resolved `provider/id`
    // selector; vision capability gates the image-input line. Both are
    // captured before `model_info` moves into the turn-boundary handler.
    let prompt_model_selector = Some(format!("{}/{}", model_info.provider, model_info.id));
    let prompt_vision_capable = Some(model_info.input.contains(&pa_types::ai::ModelInput::Image));
    let system_prompt = crate::prompts::system_prompt::build_system_prompt(
        &crate::prompts::system_prompt::BuildSystemPromptOptions {
            custom_prompt: resources.system_prompt.clone(),
            model: prompt_model_selector.as_deref(),
            vision_capable: prompt_vision_capable,
            cwd: cwd.display().to_string(),
            messages_path: conversation_log.clone(),
            context_files: resources
                .agents_files
                .iter()
                .map(|file| (file.path.display().to_string(), file.content.clone()))
                .collect(),
            skills: resources.skills.clone(),
            selected_tools: Some(
                active_tool_names
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            ),
            allow_recursion: config.allow_recursion,
            generic_mcp_servers,
            prompt_guidelines: Some(prompt_guidelines),
            ..Default::default()
        },
    );

    // Harness digest inputs: global state from the agent dir, local state
    // from the session artifacts (or the daemon-owned conversation log), and
    // the interfaces the digest may reference.
    let digest_context = super::harness_digest::HarnessDigestContext {
        global_dir: crate::refinement::get_global_harness_state_dir(&config.agent_dir),
        local_dir: wiring
            .session
            .lock()
            .await
            .get_session_artifact_dir()
            .or_else(|| {
                config
                    .conversation_log_path
                    .as_deref()
                    .and_then(super::harness_digest::local_harness_dir_for_log)
            }),
        include_ipython: active_tool_names.iter().any(|name| name == "ipython"),
        include_shell_examples: active_tool_names.iter().any(|name| name == "bash"),
        include_refine: resources.skills.iter().any(|skill| {
            !skill.disable_model_invocation
                && skill.name == crate::prompts::system_prompt::REFINE_SKILL_NAME
        }),
    };
    // sdk.ts `createAgentSession` parity: a session manager that already
    // holds messages is a resume — the loop starts from the persisted
    // context. Fresh sessions record the creation prefix (model_change +
    // thinking_level_change + service_tier_change); resumed sessions record
    // the thinking level and service tier only when no earlier entry set
    // them.
    let (existing_messages, has_thinking_entry, has_service_tier_entry) = {
        let session = wiring.session.lock().await;
        let messages = super::compact_session::rebuilt_context_after_compaction(&session);
        let has_thinking_entry = session
            .get_all_entries()
            .iter()
            .any(|entry| matches!(entry, FileEntry::ThinkingLevelChange { .. }));
        let has_service_tier_entry = session
            .get_all_entries()
            .iter()
            .any(|entry| matches!(entry, FileEntry::ServiceTierChange { .. }));
        (messages, has_thinking_entry, has_service_tier_entry)
    };
    let thinking_level = config.thinking_level.unwrap_or(ThinkingLevel::Off);
    {
        let mut session = wiring.session.lock().await;
        if existing_messages.is_empty() {
            session.append_model_change(&model.provider, &model.id);
            session.append_thinking_level_change(&format!("{thinking_level:?}").to_lowercase());
        } else if !has_thinking_entry {
            session.append_thinking_level_change(&format!("{thinking_level:?}").to_lowercase());
        }
        if existing_messages.is_empty() || !has_service_tier_entry {
            session.append_service_tier_change(Some(service_tier_preference));
        }
    }
    // The loop consumes agent-side messages; session entries cross through
    // the shared wire shape (same conversion the compaction rebuild uses).
    let initial_messages = if existing_messages.is_empty() {
        None
    } else {
        Some(
            existing_messages
                .into_iter()
                .filter_map(|message| {
                    let value = serde_json::to_value(&message).ok()?;
                    serde_json::from_value(value).ok()
                })
                .collect(),
        )
    };

    let agent = Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some(system_prompt.clone()),
            model: Some(model),
            thinking_level: Some(thinking_level),
            tools: Some(tools),
            messages: initial_messages,
        },
        stream_fn: Some(stream_fn),
        ..Default::default()
    });

    let agent = Arc::new(agent);
    let telemetry_agent = std::sync::Arc::clone(&agent);
    let session = AgentSession::from_session_arc(
        agent.clone(),
        wiring.session.clone(),
        resources.prompts.clone(),
        Some(digest_context),
    )
    .await?;

    // Bind the turn-boundary runtime the `compact.*`/`refine.*` handlers
    // probe (turn-active state, usage estimate, compaction preparation).
    turn_boundary.bind(super::turn_boundary::TurnBoundaryRuntime {
        agent,
        session: wiring.session.clone(),
        context_window: (model_context_window > 0).then_some(model_context_window),
        model_info,
    });

    // Session telemetry: installed only for depth-0 sessions (TS parity —
    // subagents never double-report). The composition root supplies the
    // resolved client; `None` wires the opt-out fast path.
    let telemetry = match (config.telemetry.take(), config.rlm_depth.unwrap_or(0)) {
        (Some(wiring), 0) => {
            let skill_counts = super::telemetry::SkillCounts {
                skill_count: resources.skills.len(),
                python_skill_count: super::runtime_wiring::kernel_python_skills(&resources.skills)
                    .len(),
            };
            let installed = super::telemetry::install_session_telemetry(
                &telemetry_agent,
                &wiring,
                Some(skill_counts),
            )
            .await?;
            Some(std::sync::Arc::new(installed))
        }
        _ => None,
    };

    let goal_driver = wiring.runtime.goal_driver().clone();
    Ok(SessionEngine {
        session,
        skills: resources.skills,
        prompt_templates: resources.prompts,
        agents_files: resources.agents_files,
        system_prompt,
        goal_driver,
        mcp_manager,
        extension_runner,
        extension_diagnostics,
        turn_boundary,
        telemetry,
    })
}

impl SessionEngine {
    /// Prompt the session (delegates to AgentSession::prompt).
    pub async fn prompt(
        &self,
        text: &str,
        options: PromptOptions,
    ) -> anyhow::Result<PromptOutcome> {
        self.session.prompt(text, options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_engine::tool_bridge::{bridge_tool, ToolDefinitionBridge};
    use crate::tools::tool_definition::{ExecutionMode, ToolDefinition, ToolExecutionResult};
    use pa_agent::scripted::ScriptedProvider;

    fn echo_definition() -> ToolDefinition {
        ToolDefinition {
            name: "echo".to_string(),
            label: "Echo".to_string(),
            description: "Echoes its input".to_string(),
            prompt_snippet: String::new(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }),
            execution_mode: Some(ExecutionMode::Sequential),
            prepare_arguments: None,
            execute: Arc::new(|_id, params, _signal, _on_update| {
                let text = params
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string();
                Box::pin(async move { Ok(ToolExecutionResult::text(format!("echo: {text}"))) })
            }),
        }
    }

    #[tokio::test]
    async fn engine_runs_tool_loop_and_persists() {
        let model = pa_agent::types::Model {
            id: "m".into(),
            name: "m".into(),
            api: "test".into(),
            provider: "test".into(),
            base_url: "http://localhost".into(),
            reasoning: false,
            cost: Default::default(),
            context_window: 1_000,
            max_tokens: 100,
        };
        let provider = Arc::new(ScriptedProvider::new(model.clone()));
        // First turn: call the tool. Second turn: final text.
        provider.push_tool_call_turn(
            Some("checking"),
            vec![("call-1", "echo", serde_json::json!({ "text": "hi" }))],
        );
        provider.push_text_turn("all done");

        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let engine = create_session(SessionEngineConfig {
            cwd: cwd.clone(),
            agent_dir: tmp.path().join("agent"),
            mcp_manager: None,
            model: Some(model),
            thinking_level: None,
            stream_fn: Some(provider.stream_fn()),
            tools: vec![bridge_tool(echo_definition())],
            custom_system_prompt: None,
            prompt_guidelines: vec![],
            generic_mcp_servers: vec![],
            allow_recursion: None,
            session_manager: None,
            extra_host_handlers: None,
            conversation_log_path: None,
            additional_skill_paths: vec![],
            additional_prompt_paths: vec![],
            extra_builtin_skill_overrides: vec![],
            rlm_subagent_host: None,
            rlm_depth: None,
            telemetry: None,
            model_info: None,
            cli_extension_sources: vec![],
            extension_tool_allow_list: None,
        })
        .await
        .unwrap();

        // The system prompt is the layered assembly: static core layer
        // first, dynamic tail after.
        assert!(engine.system_prompt.starts_with("# prime-agent harness"));
        assert!(engine
            .system_prompt
            .contains("Recursive agent depth: 0 (root)"));

        let outcome = engine
            .prompt("run the echo tool", PromptOptions::default())
            .await
            .unwrap();
        assert_eq!(outcome, PromptOutcome::Prompt);
        engine.session.agent().wait_for_idle().await;

        // The loop executed the tool and produced the final message.
        let state = engine.session.agent().state().await;
        assert!(state.messages.iter().any(|message| match message {
            pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::ToolResult(
                result,
            )) => {
                result.tool_name == "echo"
            }
            _ => false,
        }));
        assert!(state.messages.iter().any(|message| match message {
            pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(
                assistant,
            )) => {
                assistant.content.iter().any(|block| {
                    matches!(
                        block,
                        pa_agent::types::AssistantContent::Text(text) if text.text == "all done"
                    )
                })
            }
            _ => false,
        }));
        // The session persisted user + assistant turns.
        let entries = engine.session.entries().await;
        assert!(entries.iter().any(|entry| matches!(
            entry,
            pa_types::session::FileEntry::Message {
                message: pa_types::session::AgentMessage::User(user),
                ..
            } if user.content.text() == "run the echo tool"
        )));
        let _ = ToolDefinitionBridge::new;
    }

    /// The login chain's prompt-gating end to end at the engine level: a
    /// settings-declared OAuth server stays gated, an endpoint-bound
    /// credential (exactly what `mcp.begin_login` persists) unlocks it in
    /// the NEXT session the engine builds, and a credential bound to
    /// another endpoint does not.
    #[tokio::test]
    async fn oauth_creds_unlock_generic_mcp_gating_in_new_sessions() {
        fn model() -> pa_agent::types::Model {
            pa_agent::types::Model {
                id: "m".into(),
                name: "m".into(),
                api: "test".into(),
                provider: "test".into(),
                base_url: "http://localhost".into(),
                reasoning: false,
                cost: Default::default(),
                context_window: 1_000,
                max_tokens: 100,
            }
        }

        fn config(
            cwd: &std::path::Path,
            agent_dir: &std::path::Path,
            stream_fn: pa_agent::stream::StreamFn,
        ) -> SessionEngineConfig {
            SessionEngineConfig {
                cwd: cwd.to_path_buf(),
                agent_dir: agent_dir.to_path_buf(),
                mcp_manager: None,
                model: Some(model()),
                thinking_level: None,
                stream_fn: Some(stream_fn),
                tools: vec![],
                custom_system_prompt: None,
                prompt_guidelines: vec![],
                generic_mcp_servers: vec![],
                allow_recursion: None,
                session_manager: None,
                extra_host_handlers: None,
                conversation_log_path: None,
                additional_skill_paths: vec![],
                additional_prompt_paths: vec![],
                extra_builtin_skill_overrides: vec![],
                rlm_subagent_host: None,
                rlm_depth: None,
                telemetry: None,
                model_info: None,
                cli_extension_sources: vec![],
                extension_tool_allow_list: None,
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("project");
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        // The settings declaration the daemon worker's MCP manager also
        // resolves (an OAuth HTTP server, like `mcp add ... --oauth`).
        std::fs::write(
            agent_dir.join("settings.json"),
            serde_json::json!({
                "mcpServers": {
                    "fixture-oauth": {
                        "type": "http",
                        "url": "https://fixture.example/mcp",
                        "oauth": true,
                    },
                },
            })
            .to_string(),
        )
        .unwrap();
        let provider = Arc::new(ScriptedProvider::new(model()));

        // Gated: no credentials, no generic MCP guidance in the prompt.
        let engine = create_session(config(&cwd, &agent_dir, provider.stream_fn()))
            .await
            .unwrap();
        assert!(!engine.system_prompt.contains("# Generic MCP Connections"));

        // The persisted credential begin_login leaves behind (the TS
        // McpCredentials shape, endpoint-bound).
        let write_credential = |endpoint: &str| {
            std::fs::write(
                agent_dir.join("auth.json"),
                serde_json::json!({
                    "mcp:fixture-oauth": {
                        "type": "oauth",
                        "access": "fixture-access",
                        "refresh": "fixture-refresh",
                        "expires": 999999999999999i64,
                        "endpoint": endpoint,
                        "tokenEndpoint": "https://fixture.example/token",
                        "clientId": "fixture-client",
                    },
                })
                .to_string(),
            )
            .unwrap();
        };

        // A credential bound to another endpoint stays gated: the token
        // must prove where it belongs (a retargeted entry forces a
        // re-login).
        write_credential("https://other.example/mcp");
        let engine = create_session(config(&cwd, &agent_dir, provider.stream_fn()))
            .await
            .unwrap();
        assert!(!engine.system_prompt.contains("# Generic MCP Connections"));

        // The endpoint-bound credential unlocks the prompt guidance in the
        // next session the engine builds.
        write_credential("https://fixture.example/mcp");
        let engine = create_session(config(&cwd, &agent_dir, provider.stream_fn()))
            .await
            .unwrap();
        assert!(engine.system_prompt.contains("# Generic MCP Connections"));
        assert!(engine.system_prompt.contains("`fixture-oauth`"));
    }
}

#[tokio::test]
async fn create_session_registers_goal_and_heartbeat_handlers() {
    let dir = tempfile::TempDir::new().unwrap();
    let registration =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            models: Some(vec![pa_ai::faux::FauxModelDefinition {
                id: "faux-1".to_string(),
                name: Some("Faux".to_string()),
                reasoning: Some(false),
                input: Some(vec![pa_types::ai::ModelInput::Text]),
                cost: None,
                context_window: Some(100_000),
                max_tokens: Some(4_096),
            }]),
            ..Default::default()
        });
    registration.set_responses(vec![pa_ai::faux::FauxResponseStep::Message(
        pa_ai::faux::faux_assistant_text_message(
            "ok",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        ),
    )]);
    let model = registration.get_model();
    let agent_model = crate::session_engine::provider_adapter::json_round_trip(&model).unwrap();
    let stream_fn = crate::session_engine::provider_adapter::real_stream_fn(None, model.clone());
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().to_path_buf(),
        model: Some(agent_model),
        stream_fn: Some(stream_fn),
        tools: Vec::new(),
        ..Default::default()
    })
    .await
    .unwrap();
    // The agent loop gained the ipython tool backed by the kernel.
    let names: Vec<String> = engine
        .session
        .agent()
        .state()
        .await
        .tools
        .iter()
        .map(|tool| tool.name().to_string())
        .collect();
    assert!(
        names.iter().any(|name| name == "ipython"),
        "tools: {names:?}"
    );
}
