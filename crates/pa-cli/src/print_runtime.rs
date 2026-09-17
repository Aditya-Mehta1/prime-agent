//! The headless print runtime: single-shot prompt -> answer over the pa-core
//! session engine with a real pa-ai provider. Port of the text-mode half of
//! modes/print-mode.ts wired onto `create_session` (the Rust engine facade).

use std::sync::Arc;

use pa_agent::types::Model as AgentModel;
use pa_core::session::discovery::{
    find_most_recent_session_for_cwd, resolve_session_path, ResolvedSession, SessionSelectorError,
};
use pa_types::ai::Model;

use crate::headless_autonomous::{autonomous_runtime_config, HeadlessAutonomous};
use crate::mode::{AppMode, MissingSubsystem, RunOptions};
use pa_core::session_engine::provider_adapter::{
    json_round_trip, map_thinking_level, real_stream_fn,
};

/// The runtime: implements the print (text) mode against the merged session
/// engine. Modes not wired here still report their typed missing subsystem.
pub struct PrintRuntime;

impl crate::mode::Runtime for PrintRuntime {
    fn run(&self, options: &RunOptions) -> Result<i32, MissingSubsystem> {
        // `model list` takes the full runtime path in every mode and exits
        // (TS main: listModels runs after session assembly, before any mode
        // transport, and exits 0).
        if options.list_models.is_some() {
            return match crate::list_models::run(options) {
                Ok(code) => Ok(code),
                Err(message) => {
                    eprintln!("Error: {message}");
                    Ok(1)
                }
            };
        }
        match options.app_mode {
            // Runtime failures print themselves and exit non-zero; the typed
            // MissingSubsystem channel stays reserved for unwired subsystems.
            AppMode::Print => match run_print_mode(options) {
                Ok(code) => Ok(code),
                Err(message) => {
                    eprintln!("Error: {message}");
                    Ok(1)
                }
            },
            AppMode::Json => match run_print_mode(options) {
                Ok(code) => Ok(code),
                Err(message) => {
                    eprintln!("Error: {message}");
                    Ok(1)
                }
            },
            // The interactive TUI attaches through the daemon (spawning a
            // supervisor when none is running); the daemon mode runs the
            // supervisor in-process. Runtime failures print themselves and
            // exit non-zero, so the typed channel stays for unwired modes.
            AppMode::Interactive => match crate::interactive_mode::run_interactive_mode(options) {
                Ok(code) => Ok(code),
                Err(error) => {
                    eprintln!("Error: {error:#}");
                    Ok(1)
                }
            },
            AppMode::Daemon => {
                match crate::daemon_mode::run_daemon_mode(options.daemon_socket.as_deref()) {
                    Ok(code) => Ok(code),
                    Err(error) => {
                        eprintln!("Error: {error:#}");
                        Ok(1)
                    }
                }
            }
            // ACP mode: a thin JSON-RPC stdio transport over the same
            // in-process session engine the print mode uses.
            AppMode::Acp => match run_acp_mode(options) {
                Ok(code) => Ok(code),
                Err(error) => {
                    eprintln!("Error: {error:#}");
                    Ok(1)
                }
            },
            AppMode::Rpc => Err(MissingSubsystem::SessionEngine),
        }
    }
}

/// The ACP headless mode: build the in-process session engine the same way
/// the print mode does, then serve the ACP JSON-RPC surface over stdio until
/// the client disconnects.
fn run_acp_mode(options: &RunOptions) -> Result<i32, String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    rt.block_on(acp_mode_main(options))
}

async fn acp_mode_main(options: &RunOptions) -> Result<i32, String> {
    let config = &options.config;
    let engine = build_headless_engine_parts(options).await?;
    let exit_code = pa_daemon::acp::run_acp_mode(pa_daemon::acp::AcpOptions {
        engine: std::sync::Arc::new(engine.engine),
        actual_cwd: config.cwd.clone(),
        product_version: crate::config::VERSION.to_string(),
        model: Some(engine.model),
        api_key: engine.api_key,
        agent_dir: config.agent_dir.clone(),
        autonomous_config: options
            .config
            .autonomous
            .as_ref()
            .map(autonomous_runtime_config),
    })
    .await
    .map_err(|error| format!("{error:#}"))?;
    Ok(exit_code)
}

fn run_print_mode(options: &RunOptions) -> Result<i32, String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    rt.block_on(print_mode_main(options))
}

async fn print_mode_main(options: &RunOptions) -> Result<i32, String> {
    let engine = build_headless_engine(options).await?;
    run_prompts_and_emit(&engine, options).await
}

/// Assemble the in-process session engine for a headless run: model
/// resolution, session persistence, and the engine facade. The faux-script
/// seam (`PRIME_AGENT_FAUX_SCRIPT`) drives the same assembly without the
/// network; verification harness only, never set by the product.
/// The assembled headless engine plus the model and request auth it runs
/// on, so host transports can drive session-command executors
/// (compact/refine) with the session's own model.
struct HeadlessEngine {
    engine: pa_core::session_engine::engine::SessionEngine,
    model: Model,
    api_key: Option<String>,
}

async fn build_headless_engine_parts(options: &RunOptions) -> Result<HeadlessEngine, String> {
    let config = &options.config;
    if let Ok(script) = std::env::var("PRIME_AGENT_FAUX_SCRIPT") {
        return build_faux_engine_parts(options, &script).await;
    }

    // Model registry: composed catalog + models.json with real auth.
    let auth = pa_core::auth::AuthStorage::create(&config.agent_dir);
    let mut registry =
        pa_core::models::ModelRegistry::create(auth, config.agent_dir.join("models.json"));
    let model = select_model(
        &mut registry,
        config.provider.as_deref(),
        config.model.as_deref(),
    )?;

    // Resolve request auth once (single-shot mode).
    let resolved = registry.get_api_key_and_headers(&model, model.headers.as_ref());

    let stream_fn = real_stream_fn(resolved.api_key.clone(), model.clone());
    let agent_model: AgentModel = json_round_trip(&model).ok_or("model conversion failed")?;

    let session_manager = if options.session.no_session {
        None
    } else {
        Some(build_session_manager(options)?)
    };

    let engine = pa_core::session_engine::engine::create_session(
        pa_core::session_engine::engine::SessionEngineConfig {
            cwd: config.cwd.clone(),
            agent_dir: config.agent_dir.clone(),
            model: Some(agent_model),
            thinking_level: Some(resolve_thinking_level(config, &model)),
            stream_fn: Some(stream_fn),
            tools: builtin_tools(&config.cwd),
            custom_system_prompt: config.system_prompt.clone(),
            prompt_guidelines: config.append_system_prompt.clone(),
            generic_mcp_servers: vec![],
            allow_recursion: None,
            session_manager,
            extra_host_handlers: None,
            conversation_log_path: None,
            additional_skill_paths: config
                .skills
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            additional_prompt_paths: config
                .prompt_templates
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            extra_builtin_skill_overrides: vec![],
            rlm_subagent_host: None,
        },
    )
    .await
    .map_err(|error| format!("{error:#}"))?;
    Ok(HeadlessEngine {
        engine,
        model,
        api_key: resolved.api_key,
    })
}

/// The engine alone (callers that do not drive session commands).
async fn build_headless_engine(
    options: &RunOptions,
) -> Result<pa_core::session_engine::engine::SessionEngine, String> {
    Ok(build_headless_engine_parts(options).await?.engine)
}

/// The session header line (TS `AgentConnectionSessionHeader` shape).
async fn session_header_json(
    engine: &pa_core::session_engine::engine::SessionEngine,
    cwd: &std::path::Path,
) -> String {
    // The header carries session identity fields; emit what the engine
    // exposes today (id + cwd) with the TS envelope shape.
    let session_id = engine.session.session_id().await;
    serde_json::json!({
        "type": "session",
        "version": 2,
        "id": session_id,
        "cwd": cwd.display().to_string(),
    })
    .to_string()
}

/// Serialize one loop event to the TS session_event wire shape.
fn agent_event_json(event: &pa_agent::types::AgentEvent) -> Option<String> {
    use pa_agent::types::AgentEvent;
    fn message_value(value: &pa_agent::types::AgentMessage) -> serde_json::Value {
        json_round_trip(value).unwrap_or(serde_json::Value::Null)
    }
    let value = match event {
        AgentEvent::AgentStart => serde_json::json!({ "type": "agent_start" }),
        AgentEvent::AgentEnd { messages } => serde_json::json!({
            "type": "agent_end",
            "messages": messages.iter().map(message_value).collect::<Vec<_>>(),
        }),
        AgentEvent::TurnStart => serde_json::json!({ "type": "turn_start" }),
        AgentEvent::TurnEnd {
            message,
            tool_results,
        } => serde_json::json!({
            "type": "turn_end",
            "message": message_value(message),
            "toolResults": tool_results.iter().map(|r| json_round_trip(r).unwrap_or(serde_json::Value::Null)).collect::<Vec<_>>(),
        }),
        AgentEvent::MessageStart { message: m } => serde_json::json!({
            "type": "message_start",
            "message": message_value(m),
        }),
        AgentEvent::MessageUpdate { .. } => return None,
        AgentEvent::MessageEnd { message: m } => serde_json::json!({
            "type": "message_end",
            "message": message_value(m),
        }),
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => serde_json::json!({
            "type": "tool_execution_start",
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "args": args,
        }),
        AgentEvent::ToolExecutionUpdate { .. } => return None,
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            ..
        } => serde_json::json!({
            "type": "tool_execution_end",
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "result": json_round_trip(result).unwrap_or(serde_json::Value::Null),
        }),
    };
    Some(value.to_string())
}

fn select_model(
    registry: &mut pa_core::models::ModelRegistry,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<Model, String> {
    let available: Vec<Model> = registry.get_available().into_iter().cloned().collect();
    let Some(model_name) = model else {
        // No model selection: prefer the registry's featured default.
        let all: Vec<Model> = registry.get_all().to_vec();
        if let Some(default) = pa_core::models::find_preferred_default_model(&available) {
            return Ok(default.clone());
        }
        return all.first().cloned().ok_or_else(|| {
            "No models available. Check your installation or add models to models.json.".to_string()
        });
    };
    let resolved = pa_core::models::resolve_cli_model(provider, model_name, &available);
    if let Some(error) = resolved.error {
        return Err(error);
    }
    resolved
        .model
        .ok_or_else(|| "No matching model found.".to_string())
}

/// Resolve the session thinking level with the sdk.ts `createAgentSession`
/// order: the CLI flag, then the settings default, then "medium" — always
/// clamped to what the model supports.
fn resolve_thinking_level(
    config: &crate::mode::RuntimeConfig,
    model: &Model,
) -> pa_agent::types::ThinkingLevel {
    use pa_types::ai::ModelThinkingLevel;
    let settings = pa_core::settings::SettingsManager::create(&config.cwd, &config.agent_dir);
    let requested = config
        .thinking
        .or_else(|| {
            settings
                .get_default_thinking_level()
                .map(pa_core::settings::ThinkingLevelSetting::model_level)
        })
        // TS `DEFAULT_THINKING_LEVEL`.
        .unwrap_or(ModelThinkingLevel::Medium);
    let clamped = pa_ai::models::clamp_thinking_level(model, requested);
    map_thinking_level(clamped)
}

/// Build the session manager for a headless run, mirroring the flag order of
/// TS `createSessionManager` (noSession -> fork -> resume -> continue ->
/// create). `--no-session` never reaches here: the caller passes `None` to
/// the engine, which builds the in-memory manager itself.
fn build_session_manager(
    options: &RunOptions,
) -> Result<pa_core::session::manager::SessionManager, String> {
    use pa_core::session::manager::SessionManager;
    let cwd = options.config.cwd.clone();
    if let Some(selector) = &options.session.fork {
        // TS print mode forks through SessionManager.forkFrom; the Rust port
        // does not implement fork yet, so fail loudly instead of silently
        // starting an unrelated fresh session.
        let _ = selector;
        return Err("--fork is not supported in print mode yet".to_string());
    }
    let session_dir = options
        .session
        .session_dir
        .clone()
        .unwrap_or_else(|| options.config.agent_dir.join("sessions"));
    // main.ts `explicitCwdOverride`: with --cwd, the flag's directory wins
    // over the stored session cwd on resume.
    let explicit_cwd_override = options.session.cwd_from_flag.then_some(cwd.as_path());
    if let Some(selector) = &options.session.resume {
        let resolved =
            resolve_session_path(selector, &cwd, &session_dir).map_err(render_selector_error)?;
        return match resolved {
            ResolvedSession::Path(path) | ResolvedSession::Local(path) => {
                assert_session_not_active_in_daemon(options.daemon_socket.as_deref(), &path)?;
                open_session_file(&path, &session_dir, &cwd, explicit_cwd_override)
            }
            ResolvedSession::Global {
                path: _,
                cwd: session_cwd,
            } => {
                // Print mode has no fork prompt; mirror the TS non-TTY path.
                Err(format!(
                    "session {selector} belongs to a different project ({}). Pass --fork {selector} to use it here, or run from that project's directory.",
                    session_cwd.display()
                ))
            }
        };
    }
    if options.session.continue_recent {
        let most_recent = find_most_recent_session_for_cwd(&session_dir, &cwd);
        return match most_recent {
            Some(path) => {
                assert_session_not_active_in_daemon(options.daemon_socket.as_deref(), &path)?;
                open_session_file(&path, &session_dir, &cwd, explicit_cwd_override)
            }
            None => Ok(SessionManager::persisted(&cwd, &session_dir)),
        };
    }
    Ok(SessionManager::persisted(&cwd, &session_dir))
}

/// Open a session file with the TS `SessionManager.open` cwd semantics: an
/// explicit `--cwd` override wins, else the header's cwd, falling back to the
/// process cwd for unreadable or new files. Resumed sessions keep the
/// missing-cwd guard from main.ts.
/// Guard the TS print path: `-c`/`-r` refuse to open a session file that a
/// live daemon worker already hosts (`SessionAlreadyActiveError`, raised by
/// the TS supervisor's create ownership check). The Rust print path runs
/// in-process, so the guard probes the daemon's live roster first; when no
/// daemon answers, the open proceeds like a TS run without a daemon.
fn assert_session_not_active_in_daemon(
    socket_path: Option<&str>,
    session_path: &std::path::Path,
) -> Result<(), String> {
    let socket = crate::interactive_mode::resolve_socket_path(socket_path);
    let Ok(mut client) = crate::daemon_client::DaemonClient::connect(&socket) else {
        return Ok(());
    };
    let list = client
        .request(pa_types::daemon::DaemonCommand::List {
            id: None,
            all: None,
            cwd: None,
            session_dir: None,
            include_client_owned: None,
            rest: Default::default(),
        })
        .map_err(|error| format!("Could not check active sessions: {error:#}"))?;
    if !list.success {
        return Ok(());
    }
    let target = pa_daemon::lease::canonical_session_path(session_path);
    for row in list
        .data
        .and_then(|data| data.get("sessions").cloned())
        .and_then(|sessions| sessions.as_array().cloned())
        .unwrap_or_default()
    {
        let Some(file) = row.get("sessionFile").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if pa_daemon::lease::canonical_session_path(std::path::Path::new(file)) != target {
            continue;
        }
        let active_session_id = row
            .get("activeSessionId")
            .or_else(|| row.get("id"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        return Err(format!(
            "Session is already active in {active_session_id}: {}",
            target.display()
        ));
    }
    Ok(())
}

fn open_session_file(
    path: &std::path::Path,
    session_dir: &std::path::Path,
    fallback_cwd: &std::path::Path,
    explicit_cwd_override: Option<&std::path::Path>,
) -> Result<pa_core::session::manager::SessionManager, String> {
    let session_cwd = explicit_cwd_override
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| {
            let header = pa_core::session::manager::read_session_header(path);
            header
                .filter(|header| !header.cwd.is_empty())
                .map(|header| std::path::PathBuf::from(&header.cwd))
                .unwrap_or_else(|| fallback_cwd.to_path_buf())
        });
    let manager = pa_core::session::manager::SessionManager::open(&session_cwd, session_dir, path);
    // main.ts getMissingSessionCwdIssue: a session stored against a deleted
    // directory must not silently continue somewhere else.
    if !manager.get_cwd().exists() {
        let session_file = manager
            .get_session_file()
            .map(|path| format!("\nSession file: {}", path.display()))
            .unwrap_or_default();
        return Err(format!(
            "Stored session working directory does not exist: {}{session_file}\nCurrent working directory: {}",
            manager.get_cwd().display(),
            fallback_cwd.display()
        ));
    }
    Ok(manager)
}

/// Render a selector failure with the main.ts formatting: the error message
/// plus the browse hint.
fn render_selector_error(error: SessionSelectorError) -> String {
    format!(
        "{}.{}\nOpen prime-agent and press left-arrow to browse sessions.",
        error.message(),
        error.suggestion().unwrap_or_default()
    )
}

/// Model tools for the print runtime: `ipython` only (the TS product exposes
/// only the REPL tool to the model; `bash` and `edit` live in the kernel).
/// The engine adds the kernel-backed `ipython` tool itself.
fn builtin_tools(_cwd: &std::path::Path) -> Vec<Arc<dyn pa_agent::types::AgentTool>> {
    Vec::new()
}

/// Admit prompts, stream json events when requested, and decide the exit code
/// from the headless terminal result plus the autonomous gate contract.
/// Shared by the real and faux paths. When autonomous flags are present the
/// gate loop runs after every settled prompt: continuations stream like any
/// other turn, and a stop surfaces the durable `autonomous_status` row as a
/// `message_end` event before the process exits.
async fn run_prompts_and_emit(
    engine: &pa_core::session_engine::engine::SessionEngine,
    options: &RunOptions,
) -> Result<i32, String> {
    let json_mode = options.app_mode == AppMode::Json;
    let mut unsubscribe: Option<pa_agent::agent::Subscription> = None;
    if json_mode {
        let header = session_header_json(engine, &options.config.cwd).await;
        println!("{header}");
        unsubscribe = Some(
            engine
                .session
                .agent()
                .subscribe(move |event, _signal| {
                    Box::pin(async move {
                        if let Some(json) = agent_event_json(&event) {
                            println!("{json}");
                        }
                        Ok(())
                    })
                })
                .await,
        );
    }
    // The autonomous run from the CLI flags (the verifier/eval composition
    // seam): per-message accounting plus the gate continuation loop.
    let autonomous = options
        .config
        .autonomous
        .as_ref()
        .map(|config| HeadlessAutonomous::from_cli(config, &options.config.cwd));
    let mut accounting: Option<pa_agent::agent::Subscription> = None;
    if let Some(run) = &autonomous {
        accounting = Some(run.wire_accounting(engine.session.agent()).await);
    }
    for prompt in options
        .initial_message
        .iter()
        .chain(options.messages.iter())
    {
        engine
            .session
            .prompt(prompt, Default::default())
            .await
            .map_err(|error| format!("{error:#}"))?;
        engine.session.agent().wait_for_idle().await;
        if let Some(run) = &autonomous {
            if let Some(row) = run
                .drive(engine)
                .await
                .map_err(|error| format!("{error:#}"))?
            {
                emit_stop_row_events(json_mode, &row);
            }
        }
    }
    if let Some(subscription) = accounting {
        subscription.unsubscribe().await;
    }
    if let Some(subscription) = unsubscribe {
        subscription.unsubscribe().await;
    }
    let state = engine.session.agent().state().await;
    let messages: Vec<pa_types::session::AgentMessage> =
        state.messages.iter().filter_map(json_round_trip).collect();
    let result = pa_core::session_engine::headless::select_headless_terminal_result(&messages);
    let mut exit_code = 0;
    if json_mode {
        if let Some(primary) = &result.primary {
            primary.stderr_text(&mut exit_code);
        }
        for outcome in &result.compaction_outcomes {
            if outcome.outcome == "failed" {
                exit_code = 1;
            }
        }
    } else {
        match result.primary {
            Some(primary) => {
                if let Some(stderr) = primary.stderr_text(&mut exit_code) {
                    eprintln!("{stderr}");
                }
                if exit_code == 0 {
                    if let Some(text) = primary.stdout_text() {
                        println!("{text}");
                    }
                }
            }
            None => {
                eprintln!("No response produced.");
                exit_code = 1;
            }
        }
        for outcome in result.compaction_outcomes {
            eprintln!("{}", outcome.content);
            if outcome.outcome == "failed" {
                exit_code = 1;
            }
        }
    }
    // The TS print-mode autonomous contract applies to both output modes.
    if let Some(run) = &autonomous {
        if let Some(stderr) = run.exit_stderr().await {
            eprintln!("{stderr}");
            exit_code = 1;
        }
    }
    Ok(exit_code)
}

/// The durable stop row as `message_start` + `message_end` events (the daemon
/// worker's wire shape for custom rows). Text mode stays quiet.
fn emit_stop_row_events(json_mode: bool, row: &pa_types::session::CustomMessage) {
    if !json_mode {
        return;
    }
    let message = crate::headless_autonomous::stop_row_wire_value(row);
    for event_type in ["message_start", "message_end"] {
        let event = serde_json::json!({ "type": event_type, "message": message });
        println!("{event}");
    }
}

/// The faux-script engine: identical session assembly, scripted provider.
async fn build_faux_engine_parts(
    options: &RunOptions,
    script: &str,
) -> Result<HeadlessEngine, String> {
    let config = &options.config;
    let script: serde_json::Value = serde_json::from_str(script)
        .map_err(|error| format!("invalid PRIME_AGENT_FAUX_SCRIPT: {error}"))?;
    // Response entries: a plain string (or `{"text": ...}`) answers with
    // fixed text; `{"systemPrompt": true}` answers with the request's system
    // prompt (binary-level verification of session assembly; never used by
    // the product).
    let response_steps: Vec<pa_ai::faux::FauxResponseStep> = script
        .get("responses")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .map(|entry| match entry {
                    serde_json::Value::String(text) => pa_ai::faux::FauxResponseStep::Message(
                        pa_ai::faux::faux_assistant_text_message(
                            text,
                            pa_ai::faux::FauxAssistantMessageOptions::default(),
                        ),
                    ),
                    serde_json::Value::Object(map) => {
                        if map.get("systemPrompt").and_then(serde_json::Value::as_bool)
                            == Some(true)
                        {
                            pa_ai::faux::FauxResponseStep::Factory(std::sync::Arc::new(
                                |context, _options, _call, _model| {
                                    Ok(pa_ai::faux::faux_assistant_text_message(
                                        context.system_prompt.as_deref().unwrap_or_default(),
                                        pa_ai::faux::FauxAssistantMessageOptions::default(),
                                    ))
                                },
                            ))
                        } else {
                            pa_ai::faux::FauxResponseStep::Message(
                                pa_ai::faux::faux_assistant_text_message(
                                    map.get("text")
                                        .and_then(serde_json::Value::as_str)
                                        .unwrap_or_default(),
                                    pa_ai::faux::FauxAssistantMessageOptions::default(),
                                ),
                            )
                        }
                    }
                    _ => pa_ai::faux::FauxResponseStep::Message(
                        pa_ai::faux::faux_assistant_text_message(
                            "",
                            pa_ai::faux::FauxAssistantMessageOptions::default(),
                        ),
                    ),
                })
                .collect()
        })
        .ok_or_else(|| "PRIME_AGENT_FAUX_SCRIPT requires a responses array".to_string())?;
    // The same faux-script model contract as the daemon worker seam: a
    // `reasoning` model makes the harness script thinking-capable turns so
    // thinking-level resolution can be verified without the network.
    let reasoning = script
        .get("reasoning")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let registration =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            models: Some(vec![pa_ai::faux::FauxModelDefinition {
                id: "faux-1".to_string(),
                name: Some("Faux Model".to_string()),
                reasoning: Some(reasoning),
                input: Some(vec![pa_types::ai::ModelInput::Text]),
                cost: None,
                context_window: Some(100_000),
                max_tokens: Some(4_096),
            }]),
            ..Default::default()
        });
    registration.set_responses(response_steps);
    let model = registration.get_model();
    let agent_model = json_round_trip(&model).ok_or("model conversion failed")?;
    let stream_fn = real_stream_fn(None, model.clone());
    // The faux path shares the session-manager wiring (persist / --no-session
    // / --resume / --continue) with the real provider path so binary-level
    // tests can verify persistence without the network.
    let session_manager = if options.session.no_session {
        None
    } else {
        Some(build_session_manager(options)?)
    };
    let engine = pa_core::session_engine::engine::create_session(
        pa_core::session_engine::engine::SessionEngineConfig {
            cwd: config.cwd.clone(),
            agent_dir: config.agent_dir.clone(),
            model: Some(agent_model),
            thinking_level: Some(resolve_thinking_level(config, &model)),
            stream_fn: Some(stream_fn),
            tools: builtin_tools(&config.cwd),
            custom_system_prompt: config.system_prompt.clone(),
            prompt_guidelines: config.append_system_prompt.clone(),
            generic_mcp_servers: vec![],
            allow_recursion: None,
            session_manager,
            extra_host_handlers: None,
            conversation_log_path: None,
            additional_skill_paths: vec![],
            additional_prompt_paths: vec![],
            extra_builtin_skill_overrides: vec![],
            rlm_subagent_host: None,
        },
    )
    .await
    .map_err(|error| format!("{error:#}"))?;
    Ok(HeadlessEngine {
        engine,
        model,
        api_key: None,
    })
}
