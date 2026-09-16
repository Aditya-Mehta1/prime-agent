//! The headless print runtime: single-shot prompt -> answer over the pa-core
//! session engine with a real pa-ai provider. Port of the text-mode half of
//! modes/print-mode.ts wired onto `create_session` (the Rust engine facade).

use std::sync::Arc;

use pa_agent::types::Model as AgentModel;
use pa_types::ai::Model;

use crate::mode::{AppMode, MissingSubsystem, RunOptions};
use pa_core::session_engine::provider_adapter::{
    json_round_trip, map_thinking_level, real_stream_fn,
};

/// The runtime: implements the print (text) mode against the merged session
/// engine. Modes not wired here still report their typed missing subsystem.
pub struct PrintRuntime;

impl crate::mode::Runtime for PrintRuntime {
    fn run(&self, options: &RunOptions) -> Result<i32, MissingSubsystem> {
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
            AppMode::Interactive | AppMode::Rpc | AppMode::Acp | AppMode::Daemon => {
                Err(MissingSubsystem::SessionEngine)
            }
        }
    }
}

fn run_print_mode(options: &RunOptions) -> Result<i32, String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    rt.block_on(print_mode_main(options))
}

async fn print_mode_main(options: &RunOptions) -> Result<i32, String> {
    let config = &options.config;

    // Test seam: a scripted faux provider (`PRIME_AGENT_FAUX_SCRIPT` with
    // `{"responses": ["text", ...]}`) drives the full print path without the
    // network. Verification harness only; never set by the product.
    if let Ok(script) = std::env::var("PRIME_AGENT_FAUX_SCRIPT") {
        return faux_print_mode(options, &script).await;
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

    let stream_fn = real_stream_fn(resolved.api_key, model.clone());
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
            thinking_level: config.thinking.map(map_thinking_level),
            stream_fn: Some(stream_fn),
            tools: builtin_tools(&config.cwd),
            custom_system_prompt: config.system_prompt.clone(),
            prompt_guidelines: config.append_system_prompt.clone(),
            generic_mcp_servers: vec![],
            allow_recursion: None,
            session_manager,
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
        },
    )
    .await
    .map_err(|error| format!("{error:#}"))?;

    run_prompts_and_emit(&engine, options).await
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

fn build_session_manager(
    options: &RunOptions,
) -> Result<pa_core::session::manager::SessionManager, String> {
    let cwd = options.config.cwd.clone();
    if options.session.no_session {
        return Ok(pa_core::session::manager::SessionManager::in_memory(&cwd));
    }
    let session_dir = options
        .session
        .session_dir
        .clone()
        .unwrap_or_else(|| options.config.agent_dir.join("sessions"));
    std::fs::create_dir_all(&session_dir).map_err(|error| error.to_string())?;
    Ok(pa_core::session::manager::SessionManager::in_memory(&cwd))
}

/// Bridge the loop tools (bash/edit/ipython) into the session.
fn builtin_tools(cwd: &std::path::Path) -> Vec<Arc<dyn pa_agent::types::AgentTool>> {
    let cwd = cwd.display().to_string();
    let definitions = vec![
        pa_core::create_bash_tool_definition(&cwd),
        pa_core::create_edit_tool_definition(&cwd),
    ];
    definitions
        .into_iter()
        .map(|definition| {
            Arc::new(pa_core::session_engine::tool_bridge::ToolDefinitionBridge::new(definition))
                as Arc<dyn pa_agent::types::AgentTool>
        })
        .collect()
}

/// Admit prompts, stream json events when requested, and decide the exit code
/// from the headless terminal result. Shared by the real and faux paths.
async fn run_prompts_and_emit(
    engine: &pa_core::session_engine::engine::SessionEngine,
    options: &RunOptions,
) -> Result<i32, String> {
    let mut unsubscribe: Option<pa_agent::agent::Subscription> = None;
    if options.app_mode == AppMode::Json {
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
    }
    if let Some(subscription) = unsubscribe {
        subscription.unsubscribe().await;
    }
    let state = engine.session.agent().state().await;
    let messages: Vec<pa_types::session::AgentMessage> =
        state.messages.iter().filter_map(json_round_trip).collect();
    let result = pa_core::session_engine::headless::select_headless_terminal_result(&messages);
    let mut exit_code = 0;
    if options.app_mode == AppMode::Json {
        if let Some(primary) = &result.primary {
            primary.stderr_text(&mut exit_code);
        }
        for outcome in &result.compaction_outcomes {
            if outcome.outcome == "failed" {
                exit_code = 1;
            }
        }
        return Ok(exit_code);
    }
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
    Ok(exit_code)
}

/// The faux-script print path: identical pipeline, scripted provider.
async fn faux_print_mode(options: &RunOptions, script: &str) -> Result<i32, String> {
    let config = &options.config;
    let script: serde_json::Value = serde_json::from_str(script)
        .map_err(|error| format!("invalid PRIME_AGENT_FAUX_SCRIPT: {error}"))?;
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
        .ok_or_else(|| "PRIME_AGENT_FAUX_SCRIPT requires a responses array".to_string())?;
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
    let model = registration.get_model();
    let agent_model = json_round_trip(&model).ok_or("model conversion failed")?;
    let stream_fn = real_stream_fn(None, model);
    let engine = pa_core::session_engine::engine::create_session(
        pa_core::session_engine::engine::SessionEngineConfig {
            cwd: config.cwd.clone(),
            agent_dir: config.agent_dir.clone(),
            model: Some(agent_model),
            thinking_level: None,
            stream_fn: Some(stream_fn),
            tools: builtin_tools(&config.cwd),
            custom_system_prompt: config.system_prompt.clone(),
            prompt_guidelines: config.append_system_prompt.clone(),
            generic_mcp_servers: vec![],
            allow_recursion: None,
            session_manager: None,
            additional_skill_paths: vec![],
            additional_prompt_paths: vec![],
        },
    )
    .await
    .map_err(|error| format!("{error:#}"))?;

    run_prompts_and_emit(&engine, options).await
}
