//! The headless print runtime: single-shot prompt -> answer over the pa-core
//! session engine with a real pa-ai provider. Port of the text-mode half of
//! modes/print-mode.ts wired onto `create_session` (the Rust engine facade).

use std::sync::Arc;

use pa_agent::stream::{LlmContext, ModelStream, StreamFn, StreamRequestOptions};
use pa_agent::types::{Model as AgentModel, ThinkingLevel};
use pa_types::ai::Model;

use crate::mode::{AppMode, MissingSubsystem, RunOptions};

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

    // JSON mode: emit the session header, then every loop event as JSONL.
    let mut unsubscribe: Option<pa_agent::agent::Subscription> = None;
    if options.app_mode == AppMode::Json {
        let header = session_header_json(&engine, &config.cwd).await;
        println!("{header}");
        let subscription = engine
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
            .await;
        unsubscribe = Some(subscription);
    }

    // Admit the prompts, wait for the loop to settle, then select the
    // terminal result.
    let prompts: Vec<String> = options
        .initial_message
        .iter()
        .cloned()
        .chain(options.messages.iter().cloned())
        .collect();
    if prompts.is_empty() {
        if let Some(subscription) = unsubscribe.take() {
            subscription.unsubscribe().await;
        }
        return Ok(0);
    }
    for prompt in &prompts {
        engine
            .session
            .prompt(prompt, Default::default())
            .await
            .map_err(|error| format!("{error:#}"))?;
        engine.session.agent().wait_for_idle().await;
    }

    let state = engine.session.agent().state().await;
    let messages: Vec<pa_types::session::AgentMessage> =
        state.messages.iter().filter_map(json_round_trip).collect();
    let result = pa_core::session_engine::headless::select_headless_terminal_result(&messages);

    let mut exit_code = 0;
    if options.app_mode == AppMode::Json {
        // JSON mode: events already streamed; the terminal result only
        // decides the exit code.
        if let Some(primary) = &result.primary {
            primary.stderr_text(&mut exit_code);
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
    }
    if let Some(subscription) = unsubscribe {
        subscription.unsubscribe().await;
    }
    for outcome in result.compaction_outcomes {
        eprintln!("{}", outcome.content);
        if outcome.outcome == "failed" {
            exit_code = 1;
        }
    }
    Ok(exit_code)
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

fn map_thinking_level(level: pa_types::ai::ModelThinkingLevel) -> ThinkingLevel {
    match level {
        pa_types::ai::ModelThinkingLevel::Off => ThinkingLevel::Off,
        pa_types::ai::ModelThinkingLevel::Minimal => ThinkingLevel::Minimal,
        pa_types::ai::ModelThinkingLevel::Low => ThinkingLevel::Low,
        pa_types::ai::ModelThinkingLevel::Medium => ThinkingLevel::Medium,
        pa_types::ai::ModelThinkingLevel::High => ThinkingLevel::High,
        pa_types::ai::ModelThinkingLevel::Xhigh => ThinkingLevel::Xhigh,
        pa_types::ai::ModelThinkingLevel::Max => ThinkingLevel::Max,
    }
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

/// Wire-shape conversion at the pa-agent/pa-ai boundary: both sides serialize
/// to the same camelCase wire shapes.
fn json_round_trip<T, U>(value: &T) -> Option<U>
where
    T: serde::Serialize,
    U: serde::de::DeserializeOwned,
{
    serde_json::to_value(value)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
}

/// A real pa-ai provider stream adapter for the agent loop.
pub fn real_stream_fn(api_key: Option<String>, model: Model) -> StreamFn {
    Arc::new(
        move |_requested: AgentModel, context: LlmContext, options: StreamRequestOptions| {
            let api_key = api_key.clone();
            let model = model.clone();
            Box::pin(async move {
                let messages: Vec<pa_types::ai::Message> = context
                    .messages
                    .iter()
                    .filter_map(json_round_trip)
                    .collect();
                let tools: Vec<pa_types::ai::Tool> =
                    context.tools.iter().filter_map(json_round_trip).collect();
                let ai_context = pa_types::ai::Context {
                    system_prompt: context.system_prompt.clone(),
                    messages,
                    tools: Some(tools),
                };
                let stream_options = pa_ai::types::SimpleStreamOptions {
                    base: pa_ai::types::StreamOptions {
                        temperature: options.temperature,
                        max_tokens: options.max_tokens,
                        signal: None,
                        api_key,
                        transport: None,
                        service_tier: None,
                        cache_retention: None,
                        session_id: options.session_id.clone(),
                        on_payload: None,
                        on_response: None,
                        headers: None,
                        metadata: None,
                        timeout_ms: None,
                    },
                    reasoning: Some(match options.reasoning {
                        ThinkingLevel::Off => pa_types::ai::ModelThinkingLevel::Off,
                        ThinkingLevel::Minimal => pa_types::ai::ModelThinkingLevel::Minimal,
                        ThinkingLevel::Low => pa_types::ai::ModelThinkingLevel::Low,
                        ThinkingLevel::Medium => pa_types::ai::ModelThinkingLevel::Medium,
                        ThinkingLevel::High => pa_types::ai::ModelThinkingLevel::High,
                        ThinkingLevel::Xhigh => pa_types::ai::ModelThinkingLevel::Xhigh,
                        ThinkingLevel::Max => pa_types::ai::ModelThinkingLevel::Max,
                    }),
                    thinking_budgets: None,
                };
                let stream = pa_ai::stream_simple(&model, &ai_context, Some(stream_options))
                    .map_err(|error| anyhow::anyhow!("{error:?}"))?;
                // Pump pa-ai events into a pa-agent event stream (the loop's
                // ModelStream): each provider event is forwarded verbatim.
                let (handle, consumer) = pa_agent::stream::event_stream();
                let forwarder = tokio::spawn(async move {
                    let mut stream = stream;
                    while let Some(event) = stream.next_event().await {
                        if let Some(converted) = convert_event(&event) {
                            handle.push(converted);
                        }
                    }
                    let result = stream.result().await;
                    if let Some(converted) =
                        json_round_trip::<_, pa_agent::types::AssistantMessage>(&result)
                    {
                        handle.end(Some(converted));
                    } else {
                        handle.end(None);
                    }
                });
                // Keep the pump task alive as long as the stream lives.
                let (handle2, consumer) = (forwarder, consumer);
                Ok(consumer_pump(handle2, consumer))
            })
        },
    )
}

/// Convert one pa-ai stream event into the pa-agent loop's event enum.
/// Payloads cross the boundary by wire-shape (JSON) round-trip.
fn convert_event(
    event: &pa_types::ai::AssistantMessageEvent,
) -> Option<pa_agent::stream::AssistantMessageEvent> {
    use pa_agent::stream::AssistantMessageEvent as Out;
    use pa_types::ai::AssistantMessageEvent as In;
    fn convert_partial(
        message: &pa_types::ai::AssistantMessage,
    ) -> pa_agent::types::AssistantMessage {
        json_round_trip(message).expect("assistant wire shapes match")
    }
    Some(match event {
        In::Start { partial } => Out::Start {
            partial: convert_partial(partial),
        },
        In::TextStart {
            content_index,
            partial,
        } => Out::TextStart {
            content_index: *content_index as usize,
            partial: convert_partial(partial),
        },
        In::TextDelta {
            content_index,
            delta,
            partial,
        } => Out::TextDelta {
            content_index: *content_index as usize,
            delta: delta.clone(),
            partial: convert_partial(partial),
        },
        In::TextEnd {
            content_index,
            content,
            partial,
        } => Out::TextEnd {
            content_index: *content_index as usize,
            content: content.clone(),
            partial: convert_partial(partial),
        },
        In::ThinkingStart {
            content_index,
            partial,
        } => Out::ThinkingStart {
            content_index: *content_index as usize,
            partial: convert_partial(partial),
        },
        In::ThinkingDelta {
            content_index,
            delta,
            partial,
        } => Out::ThinkingDelta {
            content_index: *content_index as usize,
            delta: delta.clone(),
            partial: convert_partial(partial),
        },
        In::ThinkingEnd {
            content_index,
            partial,
            ..
        } => Out::ThinkingEnd {
            content_index: *content_index as usize,
            partial: convert_partial(partial),
        },
        In::ToolcallStart {
            content_index,
            partial,
        } => Out::ToolCallStart {
            content_index: *content_index as usize,
            partial: convert_partial(partial),
        },
        In::ToolcallDelta {
            content_index,
            delta,
            partial,
        } => Out::ToolCallDelta {
            content_index: *content_index as usize,
            delta: delta.clone(),
            partial: convert_partial(partial),
        },
        In::ToolcallEnd {
            content_index,
            tool_call,
            partial,
        } => Out::ToolCallEnd {
            content_index: *content_index as usize,
            tool_call: json_round_trip(tool_call).expect("tool call wire shapes match"),
            partial: convert_partial(partial),
        },
        In::Done { reason, message } => Out::Done {
            reason: json_round_trip(reason).expect("stop reason wire shapes match"),
            message: convert_partial(message),
        },
        In::Error { reason, error } => Out::Error {
            reason: json_round_trip(reason).expect("stop reason wire shapes match"),
            error: convert_partial(error),
        },
    })
}

/// Wrap the consumer so the pump task is aborted when the stream drops.
fn consumer_pump(
    forwarder: tokio::task::JoinHandle<()>,
    consumer: pa_agent::stream::AssistantMessageEventStream,
) -> Box<dyn ModelStream> {
    Box::new(PumpedStream {
        _forwarder: forwarder,
        stream: consumer,
    })
}

/// A ModelStream whose lifetime keeps the pa-ai pump task alive.
struct PumpedStream {
    _forwarder: tokio::task::JoinHandle<()>,
    stream: pa_agent::stream::AssistantMessageEventStream,
}

impl ModelStream for PumpedStream {
    fn next_event(
        &mut self,
    ) -> pa_agent::BoxFut<'_, Option<pa_agent::stream::AssistantMessageEvent>> {
        self.stream.next_event()
    }

    fn result(
        &mut self,
    ) -> pa_agent::BoxFut<'_, anyhow::Result<pa_agent::types::AssistantMessage>> {
        self.stream.result()
    }
}
