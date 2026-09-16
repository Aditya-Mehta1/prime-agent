//! The low-level agent loop, porting `packages/agent/src/agent-loop.ts` line
//! by line where possible.
//!
//! The loop works with [`types::AgentMessage`] throughout and converts to
//! LLM-bound [`types::Message`] values only at the model call boundary.
//! Model streaming goes through the minimal local [`crate::stream::ModelStream`]
//! trait.

use std::future::Future;
use std::sync::Arc;

use crate::abort::{is_abort_error, AbortSignal, ABORT_ERROR_MESSAGE};
use crate::stream::{LlmContext, StreamFn, StreamRequestOptions, ToolDefinition};
use crate::types::{
    AfterToolCallContext, AfterToolCallResult, AgentContext, AgentEvent, AgentMessage, AgentTool,
    AgentToolResult, AgentToolUpdateCallback, AssistantContent, AssistantMessage,
    BeforeToolCallContext, BeforeToolCallResult, GetContinuationMessagesContext, Message, Model,
    ShouldStopAfterTurnContext, StopReason, ThinkingLevel, ToolCall, ToolExecutionMode,
    ToolResultMessage,
};

/// Sink receiving the loop's events. The loop awaits every emission, so
/// listeners see events strictly in order (TS `AgentEventSink`).
pub type AgentEventSink =
    Arc<dyn Fn(AgentEvent) -> crate::BoxFut<'static, anyhow::Result<()>> + Send + Sync>;

// ---------------------------------------------------------------------------
// Hook types (AgentLoopConfig)
// ---------------------------------------------------------------------------

/// `convertToLlm`: converts `AgentMessage`s to LLM-compatible `Message`s before
/// each call. Must not fail (TS contract); a returned `Err` interrupts the
/// loop like a `throw` in the TS reference.
pub type ConvertToLlmFn = Arc<
    dyn Fn(Vec<AgentMessage>) -> crate::BoxFut<'static, anyhow::Result<Vec<Message>>> + Send + Sync,
>;

/// `transformContext`: AgentMessage-level transform applied before
/// `convert_to_llm` (context pruning, external injection).
pub type TransformContextFn = Arc<
    dyn Fn(
            Vec<AgentMessage>,
            AbortSignal,
        ) -> crate::BoxFut<'static, anyhow::Result<Vec<AgentMessage>>>
        + Send
        + Sync,
>;

/// Resolves the system prompt immediately before each LLM call.
pub type GetSystemPromptFn = Arc<dyn Fn() -> String + Send + Sync>;

/// Resolves an API key dynamically for each LLM call.
pub type GetApiKeyFn =
    Arc<dyn Fn(String) -> crate::BoxFut<'static, anyhow::Result<Option<String>>> + Send + Sync>;

/// Called after each turn fully completes and `turn_end` was emitted; return
/// true to stop the run before polling steering/follow-up queues.
pub type ShouldStopAfterTurnFn = Arc<
    dyn Fn(ShouldStopAfterTurnContext) -> crate::BoxFut<'static, anyhow::Result<bool>>
        + Send
        + Sync,
>;

/// Called synchronously after a completed turn and before polling for another
/// turn; never checked before the initial assistant turn.
pub type ShouldStopBeforeTurnFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// Returns steering messages to inject mid-run.
pub type PollMessagesFn =
    Arc<dyn Fn() -> crate::BoxFut<'static, anyhow::Result<Vec<AgentMessage>>> + Send + Sync>;

/// Returns continuation messages when the agent would otherwise stop.
pub type GetContinuationMessagesFn = Arc<
    dyn Fn(
            GetContinuationMessagesContext,
            AbortSignal,
        ) -> crate::BoxFut<'static, anyhow::Result<Vec<AgentMessage>>>
        + Send
        + Sync,
>;

/// `beforeToolCall`: return `{ block: true }` to prevent execution.
pub type BeforeToolCallFn = Arc<
    dyn Fn(
            BeforeToolCallContext,
            AbortSignal,
        ) -> crate::BoxFut<'static, anyhow::Result<Option<BeforeToolCallResult>>>
        + Send
        + Sync,
>;

/// `afterToolCall`: partial override of the executed tool result.
pub type AfterToolCallFn = Arc<
    dyn Fn(
            AfterToolCallContext,
            AbortSignal,
        ) -> crate::BoxFut<'static, anyhow::Result<Option<AfterToolCallResult>>>
        + Send
        + Sync,
>;

/// Configuration for the agent loop (TS `AgentLoopConfig`).
#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Model,
    pub api_key: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub reasoning: ThinkingLevel,
    pub session_id: Option<String>,
    pub convert_to_llm: ConvertToLlmFn,
    pub transform_context: Option<TransformContextFn>,
    pub get_system_prompt: Option<GetSystemPromptFn>,
    pub get_api_key: Option<GetApiKeyFn>,
    pub should_stop_after_turn: Option<ShouldStopAfterTurnFn>,
    pub should_stop_before_turn: Option<ShouldStopBeforeTurnFn>,
    pub get_steering_messages: Option<PollMessagesFn>,
    pub get_follow_up_messages: Option<PollMessagesFn>,
    pub get_continuation_messages: Option<GetContinuationMessagesFn>,
    /// Tool execution mode. Defaults to parallel (TS default).
    pub tool_execution: ToolExecutionMode,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
}

impl AgentLoopConfig {
    /// Config with a pass-through `convert_to_llm` (keeps user/assistant/
    /// toolResult messages, filters everything else) and no hooks, matching
    /// the TS defaults where hooks are optional.
    pub fn new(model: Model, convert_to_llm: ConvertToLlmFn) -> Self {
        AgentLoopConfig {
            model,
            api_key: None,
            temperature: None,
            max_tokens: None,
            reasoning: ThinkingLevel::Off,
            session_id: None,
            convert_to_llm,
            transform_context: None,
            get_system_prompt: None,
            get_api_key: None,
            should_stop_after_turn: None,
            should_stop_before_turn: None,
            get_steering_messages: None,
            get_follow_up_messages: None,
            get_continuation_messages: None,
            tool_execution: ToolExecutionMode::Parallel,
            before_tool_call: None,
            after_tool_call: None,
        }
    }

    pub fn default_convert_to_llm() -> ConvertToLlmFn {
        Arc::new(|messages: Vec<AgentMessage>| {
            Box::pin(async move {
                Ok(messages
                    .into_iter()
                    .filter(|m| {
                        matches!(
                            m,
                            AgentMessage::Standard(Message::User(_))
                                | AgentMessage::Standard(Message::Assistant(_))
                                | AgentMessage::Standard(Message::ToolResult(_))
                        )
                    })
                    .map(|m| match m {
                        AgentMessage::Standard(message) => message,
                        AgentMessage::Custom(_) => unreachable!("filtered above"),
                    })
                    .collect())
            })
        })
    }
}

// ---------------------------------------------------------------------------
// Abort / settlement helpers (ports of raceWithAbort & settlePostTurn)
// ---------------------------------------------------------------------------

async fn race_with_abort<T>(
    operation: impl Future<Output = anyhow::Result<T>>,
    signal: Option<&AbortSignal>,
) -> anyhow::Result<T> {
    match signal {
        None => operation.await,
        Some(signal) => crate::abort::race_with_abort(operation, signal)
            .await
            .and_then(|result| result),
    }
}

enum PostTurnResult<T> {
    Completed(T),
    Aborted,
}

/// Port of `settlePostTurn`: abort rejections collapse into `Aborted`; every
/// other error propagates.
async fn settle_post_turn<T>(
    operation: impl Future<Output = anyhow::Result<T>>,
    signal: Option<&AbortSignal>,
) -> anyhow::Result<PostTurnResult<T>> {
    match operation.await {
        Ok(value) => Ok(PostTurnResult::Completed(value)),
        Err(error) => {
            if signal.map(|s| s.is_aborted()).unwrap_or(false) && is_abort_error(&error) {
                Ok(PostTurnResult::Aborted)
            } else {
                Err(error)
            }
        }
    }
}

/// Port of `pollMessagesUnlessAborted`.
async fn poll_messages_unless_aborted(
    poll: Option<&PollMessagesFn>,
    signal: Option<&AbortSignal>,
) -> anyhow::Result<Vec<AgentMessage>> {
    let Some(poll) = poll else {
        return Ok(Vec::new());
    };
    if signal.map(|s| s.is_aborted()).unwrap_or(false) {
        return Ok(Vec::new());
    }
    race_with_abort(poll(), signal).await
}

// ---------------------------------------------------------------------------
// Aborted assistant message construction
// ---------------------------------------------------------------------------

fn clone_assistant_content(content: &[AssistantContent]) -> Vec<AssistantContent> {
    content
        .iter()
        .map(|part| match part {
            // TS clones the arguments object per toolCall; Value::clone is a
            // full copy, which is at least as safe.
            AssistantContent::ToolCall(tool_call) => AssistantContent::ToolCall(tool_call.clone()),
            AssistantContent::Text(text) => AssistantContent::Text(text.clone()),
            AssistantContent::Thinking(thinking) => AssistantContent::Thinking(thinking.clone()),
        })
        .collect()
}

/// Port of `createAbortedAssistantMessage`.
fn create_aborted_assistant_message(
    config: &AgentLoopConfig,
    partial_message: Option<&AssistantMessage>,
) -> AssistantMessage {
    AssistantMessage {
        content: partial_message
            .map(|partial| clone_assistant_content(&partial.content))
            .unwrap_or_else(|| {
                vec![AssistantContent::Text(crate::types::TextContent {
                    text: String::new(),
                    text_signature: None,
                })]
            }),
        api: partial_message
            .map(|p| p.api.clone())
            .unwrap_or_else(|| config.model.api.clone()),
        provider: partial_message
            .map(|p| p.provider.clone())
            .unwrap_or_else(|| config.model.provider.clone()),
        model: partial_message
            .map(|p| p.model.clone())
            .unwrap_or_else(|| config.model.id.clone()),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: partial_message
            .map(|p| p.usage.clone())
            .unwrap_or_else(crate::types::Usage::zero),
        stop_reason: StopReason::Aborted,
        stop_reason_raw: None,
        error_message: Some(ABORT_ERROR_MESSAGE.to_string()),
        timestamp: crate::now_ms(),
    }
}

// ---------------------------------------------------------------------------
// Entry points (runAgentLoop / runAgentLoopContinue)
// ---------------------------------------------------------------------------

/// Start an agent loop with new prompt messages (TS `runAgentLoop`).
///
/// The prompts are added to the context and message events are emitted for
/// them. Returns every message produced by the run.
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: &AgentLoopConfig,
    emit: AgentEventSink,
    signal: Option<&AbortSignal>,
    stream_fn: Option<&StreamFn>,
) -> anyhow::Result<Vec<AgentMessage>> {
    let mut new_messages: Vec<AgentMessage> = prompts.clone();
    let mut current_context = AgentContext {
        system_prompt: context.system_prompt.clone(),
        tools: context.tools.clone(),
        messages: context.messages.clone(),
    };
    current_context.messages.extend(prompts.iter().cloned());

    emit(AgentEvent::AgentStart).await?;
    emit(AgentEvent::TurnStart).await?;
    for prompt in &prompts {
        emit(AgentEvent::MessageStart {
            message: prompt.clone(),
        })
        .await?;
        emit(AgentEvent::MessageEnd {
            message: prompt.clone(),
        })
        .await?;
    }

    run_loop(
        &mut current_context,
        &mut new_messages,
        config,
        signal,
        &emit,
        stream_fn,
    )
    .await?;
    Ok(new_messages)
}

/// Continue an agent loop from the current context without adding a new
/// message (TS `runAgentLoopContinue`). Used for retries.
///
/// **Important:** the last message in context must convert to a `user` or
/// `toolResult` message via `convert_to_llm`, exactly like the TS reference.
pub async fn run_agent_loop_continue(
    context: AgentContext,
    config: &AgentLoopConfig,
    emit: AgentEventSink,
    signal: Option<&AbortSignal>,
    stream_fn: Option<&StreamFn>,
) -> anyhow::Result<Vec<AgentMessage>> {
    if context.messages.is_empty() {
        anyhow::bail!("Cannot continue: no messages in context");
    }
    if context.messages.last().unwrap().role() == "assistant" {
        anyhow::bail!("Cannot continue from message role: assistant");
    }

    let mut new_messages: Vec<AgentMessage> = Vec::new();
    let mut current_context = context;

    emit(AgentEvent::AgentStart).await?;
    emit(AgentEvent::TurnStart).await?;

    run_loop(
        &mut current_context,
        &mut new_messages,
        config,
        signal,
        &emit,
        stream_fn,
    )
    .await?;
    Ok(new_messages)
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

/// Port of `runLoop`.
async fn run_loop(
    current_context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
    stream_fn: Option<&StreamFn>,
) -> anyhow::Result<()> {
    let mut first_turn = true;
    let mut last_turn: Option<ShouldStopAfterTurnContext> = None;
    let mut pending_messages =
        poll_messages_unless_aborted(config.get_steering_messages.as_ref(), signal).await?;

    macro_rules! should_stop_before_turn {
        () => {
            !first_turn
                && config
                    .should_stop_before_turn
                    .as_ref()
                    .map(|hook| hook())
                    .unwrap_or(false)
        };
    }

    loop {
        crate::abort::throw_if_aborted_signal(signal)?;
        let mut has_more_tool_calls = true;

        while has_more_tool_calls || !pending_messages.is_empty() {
            crate::abort::throw_if_aborted_signal(signal)?;
            if !first_turn {
                emit(AgentEvent::TurnStart).await?;
            } else {
                first_turn = false;
            }

            if !pending_messages.is_empty() {
                for message in pending_messages.drain(..) {
                    emit(AgentEvent::MessageStart {
                        message: message.clone(),
                    })
                    .await?;
                    emit(AgentEvent::MessageEnd {
                        message: message.clone(),
                    })
                    .await?;
                    current_context.messages.push(message.clone());
                    new_messages.push(message);
                }
            }

            let message =
                stream_assistant_response(current_context, config, signal, emit, stream_fn).await?;
            new_messages.push(AgentMessage::from(message.clone()));

            if message.stop_reason == StopReason::Error
                || message.stop_reason == StopReason::Aborted
            {
                emit(AgentEvent::TurnEnd {
                    message: AgentMessage::from(message.clone()),
                    tool_results: Vec::new(),
                })
                .await?;
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await?;
                return Ok(());
            }

            let tool_calls = message
                .tool_calls()
                .into_iter()
                .cloned()
                .collect::<Vec<ToolCall>>();

            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                let executed_tool_batch =
                    execute_tool_calls(current_context, &message, config, signal, emit).await?;
                tool_results.extend(executed_tool_batch.messages);
                has_more_tool_calls = !executed_tool_batch.terminate;

                for result in tool_results.iter() {
                    current_context
                        .messages
                        .push(AgentMessage::from(result.clone()));
                    new_messages.push(AgentMessage::from(result.clone()));
                }
            }

            emit(AgentEvent::TurnEnd {
                message: AgentMessage::from(message.clone()),
                tool_results: tool_results.clone(),
            })
            .await?;
            if signal.map(|s| s.is_aborted()).unwrap_or(false) {
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await?;
                return Ok(());
            }
            last_turn = Some(ShouldStopAfterTurnContext {
                message: message.clone(),
                tool_results: tool_results.clone(),
                context: clone_context(current_context),
                new_messages: new_messages.clone(),
            });

            let should_stop_result = settle_post_turn(
                race_with_abort(
                    async {
                        match config.should_stop_after_turn.as_ref() {
                            Some(hook) => hook(last_turn.clone().unwrap()).await,
                            None => Ok(false),
                        }
                    },
                    signal,
                ),
                signal,
            )
            .await?;
            match should_stop_result {
                PostTurnResult::Aborted => {
                    emit(AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    })
                    .await?;
                    return Ok(());
                }
                PostTurnResult::Completed(true) => {
                    emit(AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    })
                    .await?;
                    return Ok(());
                }
                PostTurnResult::Completed(false) => {}
            }
            if should_stop_before_turn!() {
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await?;
                return Ok(());
            }

            let steering_messages_result = settle_post_turn(
                poll_messages_unless_aborted(config.get_steering_messages.as_ref(), signal),
                signal,
            )
            .await?;
            match steering_messages_result {
                PostTurnResult::Aborted => {
                    emit(AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    })
                    .await?;
                    return Ok(());
                }
                PostTurnResult::Completed(messages) => {
                    pending_messages = messages;
                    // Steering drained by this poll owns the turn boundary;
                    // stop only when it was empty.
                    if pending_messages.is_empty() && should_stop_before_turn!() {
                        emit(AgentEvent::AgentEnd {
                            messages: new_messages.clone(),
                        })
                        .await?;
                        return Ok(());
                    }
                }
            }
        }

        if should_stop_before_turn!() {
            break;
        }
        let follow_up_messages_result = settle_post_turn(
            poll_messages_unless_aborted(config.get_follow_up_messages.as_ref(), signal),
            signal,
        )
        .await?;
        let follow_up_messages = match follow_up_messages_result {
            PostTurnResult::Aborted => {
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await?;
                return Ok(());
            }
            PostTurnResult::Completed(messages) => messages,
        };
        if !follow_up_messages.is_empty() {
            pending_messages = follow_up_messages;
            continue;
        }

        if should_stop_before_turn!() {
            break;
        }
        let continuation_messages_result = match last_turn.clone() {
            Some(context) => {
                let continuation_op: crate::BoxFut<'static, anyhow::Result<Vec<AgentMessage>>> =
                    match config.get_continuation_messages.as_ref() {
                        Some(hook) => hook(context, signal.cloned().unwrap_or_default()),
                        None => Box::pin(async { Ok(Vec::new()) }),
                    };
                settle_post_turn(race_with_abort(continuation_op, signal), signal).await?
            }
            None => PostTurnResult::Completed(Vec::new()),
        };
        let continuation_messages = match continuation_messages_result {
            PostTurnResult::Aborted => {
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await?;
                return Ok(());
            }
            PostTurnResult::Completed(messages) => messages,
        };
        if !continuation_messages.is_empty() {
            pending_messages = continuation_messages;
            continue;
        }

        break;
    }

    emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    })
    .await?;
    Ok(())
}

fn clone_context(context: &AgentContext) -> AgentContext {
    AgentContext {
        system_prompt: context.system_prompt.clone(),
        messages: context.messages.clone(),
        tools: context.tools.clone(),
    }
}

// ---------------------------------------------------------------------------
// Streaming one assistant response
// ---------------------------------------------------------------------------

/// Port of `streamAssistantResponse`.
async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
    stream_fn: Option<&StreamFn>,
) -> anyhow::Result<AssistantMessage> {
    let mut partial_message: Option<AssistantMessage> = None;
    let mut added_partial = false;

    // The TS closure captures `partialMessage`/`addedPartial` by reference;
    // here the finish helper runs inline in the abort path below.
    macro_rules! finish_aborted_message {
        () => {{
            let final_message = create_aborted_assistant_message(config, partial_message.as_ref());
            if added_partial {
                *context.messages.last_mut().unwrap() = AgentMessage::from(final_message.clone());
            } else {
                context
                    .messages
                    .push(AgentMessage::from(final_message.clone()));
                emit(AgentEvent::MessageStart {
                    message: AgentMessage::from(final_message.clone()),
                })
                .await?;
            }
            emit(AgentEvent::MessageEnd {
                message: AgentMessage::from(final_message.clone()),
            })
            .await?;
            final_message
        }};
    }

    let result = stream_assistant_response_inner(
        context,
        config,
        signal,
        emit,
        stream_fn,
        &mut partial_message,
        &mut added_partial,
    )
    .await;

    match result {
        Ok(message) => Ok(message),
        Err(error) => {
            if signal.map(|s| s.is_aborted()).unwrap_or(false) && is_abort_error(&error) {
                return Ok(finish_aborted_message!());
            }
            Err(error)
        }
    }
}

/// Inner body of `streamAssistantResponse` (the TS `try` block).
async fn stream_assistant_response_inner(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
    stream_fn: Option<&StreamFn>,
    partial_message: &mut Option<AssistantMessage>,
    added_partial: &mut bool,
) -> anyhow::Result<AssistantMessage> {
    crate::abort::throw_if_aborted_signal(signal)?;

    let mut messages: Vec<AgentMessage> = context.messages.clone();
    if let Some(transform) = config.transform_context.as_ref() {
        messages = race_with_abort(
            transform(messages, signal.cloned().unwrap_or_default()),
            signal,
        )
        .await?;
    }

    let llm_messages = race_with_abort((config.convert_to_llm)(messages), signal).await?;

    let stream_fn = stream_fn.ok_or_else(|| {
        anyhow::anyhow!(
            "No stream function provided; the agent loop requires a model stream function (pa-ai integration supplies the default)"
        )
    })?;

    let resolved_api_key = match config.get_api_key.as_ref() {
        Some(get_api_key) => {
            match race_with_abort(get_api_key(config.model.provider.clone()), signal).await? {
                Some(key) => Some(key),
                None => config.api_key.clone(),
            }
        }
        None => config.api_key.clone(),
    };

    let llm_context = LlmContext {
        system_prompt: Some(
            config
                .get_system_prompt
                .as_ref()
                .map(|hook| hook())
                .unwrap_or_else(|| context.system_prompt.clone()),
        ),
        messages: llm_messages,
        tools: context
            .tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters().clone(),
            })
            .collect(),
    };

    let options = StreamRequestOptions {
        temperature: config.temperature,
        max_tokens: config.max_tokens,
        reasoning: config.reasoning,
        session_id: config.session_id.clone(),
        api_key: resolved_api_key,
        signal: signal.cloned().unwrap_or_default(),
    };

    let mut response = race_with_abort(
        stream_fn(config.model.clone(), llm_context, options),
        signal,
    )
    .await?;

    loop {
        let next = match signal {
            Some(signal) => {
                // TS races the iterator with `closeIterator` as the abort
                // callback: cancel the stream when the user aborts mid-read.
                let result = crate::abort::race_with_abort(response.next_event(), signal).await;
                if result.is_err() {
                    response.close();
                }
                result?
            }
            None => response.next_event().await,
        };
        let Some(event) = next else {
            break;
        };

        match &event {
            crate::stream::AssistantMessageEvent::Start { partial } => {
                *partial_message = Some(partial.clone());
                context.messages.push(AgentMessage::from(partial.clone()));
                *added_partial = true;
                emit(AgentEvent::MessageStart {
                    message: AgentMessage::from(partial.clone()),
                })
                .await?;
            }
            event if event.is_delta() => {
                if let Some(partial) = event_partial(event) {
                    *partial_message = Some(partial.clone());
                    *context.messages.last_mut().unwrap() = AgentMessage::from(partial.clone());
                    emit(AgentEvent::MessageUpdate {
                        message: AgentMessage::from(partial.clone()),
                        assistant_message_event: Box::new(event.clone()),
                    })
                    .await?;
                }
            }
            event if event.terminal_message().is_some() => {
                let mut final_message = event.terminal_message().unwrap().clone();
                match race_with_abort(response.result(), signal).await {
                    Ok(result_message) => final_message = result_message,
                    Err(error) => {
                        let aborted = signal.map(|s| s.is_aborted()).unwrap_or(false);
                        if !(aborted && is_abort_error(&error)) {
                            return Err(error);
                        }
                    }
                }
                if *added_partial {
                    *context.messages.last_mut().unwrap() =
                        AgentMessage::from(final_message.clone());
                } else {
                    context
                        .messages
                        .push(AgentMessage::from(final_message.clone()));
                }
                if !*added_partial {
                    emit(AgentEvent::MessageStart {
                        message: AgentMessage::from(final_message.clone()),
                    })
                    .await?;
                }
                emit(AgentEvent::MessageEnd {
                    message: AgentMessage::from(final_message.clone()),
                })
                .await?;
                return Ok(final_message);
            }
            _ => {}
        }
    }

    // Stream ended without a terminal event: resolve the final message (TS
    // awaits `response.result()` here too; a stream that ends cleanly always
    // pushed done/error first).
    let final_message = race_with_abort(response.result(), signal).await?;
    if *added_partial {
        *context.messages.last_mut().unwrap() = AgentMessage::from(final_message.clone());
    } else {
        context
            .messages
            .push(AgentMessage::from(final_message.clone()));
        emit(AgentEvent::MessageStart {
            message: AgentMessage::from(final_message.clone()),
        })
        .await?;
    }
    emit(AgentEvent::MessageEnd {
        message: AgentMessage::from(final_message.clone()),
    })
    .await?;
    Ok(final_message)
}

fn event_partial(event: &crate::stream::AssistantMessageEvent) -> Option<&AssistantMessage> {
    match event {
        crate::stream::AssistantMessageEvent::Start { partial }
        | crate::stream::AssistantMessageEvent::TextStart { partial, .. }
        | crate::stream::AssistantMessageEvent::TextDelta { partial, .. }
        | crate::stream::AssistantMessageEvent::TextEnd { partial, .. }
        | crate::stream::AssistantMessageEvent::ThinkingStart { partial, .. }
        | crate::stream::AssistantMessageEvent::ThinkingDelta { partial, .. }
        | crate::stream::AssistantMessageEvent::ThinkingEnd { partial, .. }
        | crate::stream::AssistantMessageEvent::ToolCallStart { partial, .. }
        | crate::stream::AssistantMessageEvent::ToolCallDelta { partial, .. }
        | crate::stream::AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(partial),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

struct ExecutedToolCallBatch {
    messages: Vec<ToolResultMessage>,
    terminate: bool,
}

struct FinalizedToolCallOutcome {
    tool_call: ToolCall,
    result: AgentToolResult,
    is_error: bool,
}

/// Port of `executeToolCalls`.
async fn execute_tool_calls(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
) -> anyhow::Result<ExecutedToolCallBatch> {
    let tool_calls = assistant_message
        .tool_calls()
        .into_iter()
        .cloned()
        .collect::<Vec<ToolCall>>();
    let has_sequential_tool_call = tool_calls.iter().any(|tc| {
        current_context
            .tools
            .iter()
            .find(|t| t.name() == tc.name)
            .and_then(|tool| tool.execution_mode())
            == Some(ToolExecutionMode::Sequential)
    });
    if config.tool_execution == ToolExecutionMode::Sequential || has_sequential_tool_call {
        execute_tool_calls_sequential(
            current_context,
            assistant_message,
            &tool_calls,
            config,
            signal,
            emit,
        )
        .await
    } else {
        execute_tool_calls_parallel(
            current_context,
            assistant_message,
            &tool_calls,
            config,
            signal,
            emit,
        )
        .await
    }
}

/// Port of `executeToolCallsSequential`.
async fn execute_tool_calls_sequential(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
) -> anyhow::Result<ExecutedToolCallBatch> {
    let mut finalized_calls: Vec<FinalizedToolCallOutcome> = Vec::new();
    let mut messages: Vec<ToolResultMessage> = Vec::new();

    for tool_call in tool_calls {
        if signal.map(|s| s.is_aborted()).unwrap_or(false) {
            break;
        }

        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
        })
        .await?;

        let preparation = prepare_tool_call(
            current_context,
            assistant_message,
            tool_call,
            config,
            signal,
        )
        .await;
        let finalized = match preparation {
            Preparation::Immediate { result, is_error } => FinalizedToolCallOutcome {
                tool_call: tool_call.clone(),
                result,
                is_error,
            },
            Preparation::Prepared(prepared) => {
                let executed = execute_prepared_tool_call(&prepared, signal, emit).await;
                finalize_executed_tool_call(
                    current_context,
                    assistant_message,
                    &prepared,
                    executed,
                    config,
                    signal,
                )
                .await?
            }
        };

        emit_tool_execution_end(&finalized, emit).await?;
        let tool_result_message = create_tool_result_message(&finalized);
        emit_tool_result_message(&tool_result_message, emit).await?;
        messages.push(tool_result_message);
        finalized_calls.push(finalized);

        if signal.map(|s| s.is_aborted()).unwrap_or(false) {
            break;
        }
    }

    Ok(ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized_calls),
    })
}

/// Port of `executeToolCallsParallel`.
///
/// `tool_execution_end` is emitted in completion order (from inside the
/// concurrent tasks), while tool-result message events are emitted afterwards
/// in assistant source order, matching the TS reference.
async fn execute_tool_calls_parallel(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
) -> anyhow::Result<ExecutedToolCallBatch> {
    enum TaskOrOutcome {
        Task(tokio::task::JoinHandle<anyhow::Result<FinalizedToolCallOutcome>>),
        Outcome(FinalizedToolCallOutcome),
    }

    let mut entries: Vec<TaskOrOutcome> = Vec::new();

    for tool_call in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
        })
        .await?;

        let preparation = prepare_tool_call(
            current_context,
            assistant_message,
            tool_call,
            config,
            signal,
        )
        .await;
        match preparation {
            Preparation::Immediate { result, is_error } => {
                let finalized = FinalizedToolCallOutcome {
                    tool_call: tool_call.clone(),
                    result,
                    is_error,
                };
                emit_tool_execution_end(&finalized, emit).await?;
                entries.push(TaskOrOutcome::Outcome(finalized));
            }
            Preparation::Prepared(prepared) => {
                let sink = Arc::clone(emit);
                let assistant_message = assistant_message.clone();
                let context = clone_context(current_context);
                let config = config.clone();
                let signal = signal.cloned().unwrap_or_default();
                let prepared = PreparedToolCall {
                    tool_call: prepared.tool_call.clone(),
                    tool: Arc::clone(&prepared.tool),
                    args: prepared.args.clone(),
                };
                let handle = tokio::spawn(async move {
                    let executed =
                        execute_prepared_tool_call(&prepared, Some(&signal), &sink).await;
                    let finalized = finalize_executed_tool_call(
                        &context,
                        &assistant_message,
                        &prepared,
                        executed,
                        &config,
                        Some(&signal),
                    )
                    .await?;
                    emit_tool_execution_end(&finalized, &sink).await?;
                    Ok(finalized)
                });
                entries.push(TaskOrOutcome::Task(handle));
            }
        }
    }

    // Promise.all semantics: await every entry; results stay in source order.
    let mut ordered_finalized_calls: Vec<FinalizedToolCallOutcome> = Vec::new();
    for entry in entries {
        match entry {
            TaskOrOutcome::Outcome(finalized) => ordered_finalized_calls.push(finalized),
            TaskOrOutcome::Task(handle) => {
                let finalized = handle.await.map_err(|error| {
                    anyhow::anyhow!("Parallel tool execution task failed: {error}")
                })??;
                ordered_finalized_calls.push(finalized);
            }
        }
    }

    let mut messages: Vec<ToolResultMessage> = Vec::new();
    for finalized in &ordered_finalized_calls {
        let tool_result_message = create_tool_result_message(finalized);
        emit_tool_result_message(&tool_result_message, emit).await?;
        messages.push(tool_result_message);
    }

    Ok(ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&ordered_finalized_calls),
    })
}

enum Preparation {
    Prepared(PreparedToolCall),
    Immediate {
        result: AgentToolResult,
        is_error: bool,
    },
}

struct PreparedToolCall {
    tool_call: ToolCall,
    tool: Arc<dyn AgentTool>,
    args: serde_json::Value,
}

/// Port of `shouldTerminateToolBatch`.
fn should_terminate_tool_batch(finalized_calls: &[FinalizedToolCallOutcome]) -> bool {
    !finalized_calls.is_empty()
        && finalized_calls
            .iter()
            .all(|finalized| finalized.result.terminate == Some(true))
}

/// Port of `prepareToolCall`: tool lookup, `prepareArguments`, schema
/// validation, and the `beforeToolCall` hook. Never fails; errors become
/// immediate error tool results exactly like the TS catch-all.
async fn prepare_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
) -> Preparation {
    let Some(tool) = current_context
        .tools
        .iter()
        .find(|t| t.name() == tool_call.name)
    else {
        return Preparation::Immediate {
            result: AgentToolResult::error(format!("Tool {} not found", tool_call.name)),
            is_error: true,
        };
    };

    let result = prepare_tool_call_inner(
        tool,
        assistant_message,
        tool_call,
        current_context,
        config,
        signal,
    )
    .await;
    match result {
        Ok(preparation) => preparation,
        Err(error) => Preparation::Immediate {
            result: AgentToolResult::error(format!("{error:#}")),
            is_error: true,
        },
    }
}

async fn prepare_tool_call_inner(
    tool: &Arc<dyn AgentTool>,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    current_context: &AgentContext,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
) -> anyhow::Result<Preparation> {
    let prepared_tool_call = match tool.prepare_arguments(&tool_call.arguments) {
        Some(prepared) => ToolCall {
            arguments: prepared,
            ..tool_call.clone()
        },
        None => tool_call.clone(),
    };
    let validated_args = crate::validation::validate_tool_arguments(
        tool_call.name.as_str(),
        tool.parameters(),
        &prepared_tool_call.arguments,
    )
    .map_err(anyhow::Error::msg)?;

    if let Some(before_tool_call) = config.before_tool_call.as_ref() {
        let before_result = race_with_abort(
            before_tool_call(
                BeforeToolCallContext {
                    assistant_message: assistant_message.clone(),
                    tool_call: tool_call.clone(),
                    args: validated_args.clone(),
                    context: clone_context(current_context),
                },
                signal.cloned().unwrap_or_default(),
            ),
            signal,
        )
        .await?;
        if before_result.as_ref().map(|r| r.block).unwrap_or(false) {
            let reason = before_result
                .and_then(|r| r.reason)
                .unwrap_or_else(|| "Tool execution was blocked".to_string());
            return Ok(Preparation::Immediate {
                result: AgentToolResult::error(reason),
                is_error: true,
            });
        }
    }

    Ok(Preparation::Prepared(PreparedToolCall {
        tool_call: tool_call.clone(),
        tool: Arc::clone(tool),
        args: validated_args,
    }))
}

struct ExecutedToolCallOutcome {
    result: AgentToolResult,
    is_error: bool,
}

/// Port of `executePreparedToolCall`: race the tool against abort, stream
/// `tool_execution_update` events through a background emitter task, and
/// return an error tool result when the tool fails (or aborts, with the TS
/// message "Tool execution aborted").
async fn execute_prepared_tool_call(
    prepared: &PreparedToolCall,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
) -> ExecutedToolCallOutcome {
    let accepting_updates = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (update_tx, mut update_rx) = tokio::sync::mpsc::unbounded_channel::<AgentToolResult>();
    let update_tx_for_callback = update_tx.clone();

    let tool_call_id = prepared.tool_call.id.clone();
    let tool_name = prepared.tool.name().to_string();
    let args = prepared.tool_call.arguments.clone();
    let sink = Arc::clone(emit);
    let signal_for_updates = signal.cloned().unwrap_or_default();
    let emit_updates = tokio::spawn(async move {
        while let Some(partial_result) = update_rx.recv().await {
            sink(AgentEvent::ToolExecutionUpdate {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                args: args.clone(),
                partial_result,
            })
            .await?;
        }
        Ok(())
    });

    let on_update: AgentToolUpdateCallback = {
        let accepting_updates = Arc::clone(&accepting_updates);
        Arc::new(move |partial_result: AgentToolResult| {
            if !accepting_updates.load(std::sync::atomic::Ordering::SeqCst)
                || signal_for_updates.is_aborted()
            {
                return;
            }
            let _ = update_tx_for_callback.send(partial_result);
        })
    };

    let execute_result = race_with_abort(
        (Arc::clone(&prepared.tool)).execute(
            prepared.tool_call.id.clone(),
            prepared.args.clone(),
            signal.cloned().unwrap_or_default(),
            on_update,
        ),
        signal,
    )
    .await;

    accepting_updates.store(false, std::sync::atomic::Ordering::SeqCst);
    drop(update_tx);

    match execute_result {
        Ok(result) => {
            match race_with_abort(
                async {
                    emit_updates.await.map_err(|error| {
                        anyhow::anyhow!("Tool update emitter task failed: {error}")
                    })
                },
                signal,
            )
            .await
            {
                Ok(update_result) => {
                    if let Err(error) = update_result {
                        // Success path: a failing update emitter mirrors a
                        // rejecting updateEvents promise in TS.
                        return ExecutedToolCallOutcome {
                            result: error_tool_result(signal, &error),
                            is_error: true,
                        };
                    }
                }
                Err(error) => {
                    if !(signal.map(|s| s.is_aborted()).unwrap_or(false) && is_abort_error(&error))
                    {
                        return ExecutedToolCallOutcome {
                            result: error_tool_result(signal, &error),
                            is_error: true,
                        };
                    }
                }
            }
            ExecutedToolCallOutcome {
                result,
                is_error: false,
            }
        }
        Err(error) => {
            // TS drains the pending update emissions on the error path,
            // swallowing every failure (`Promise.all(...).catch(() => undefined)`).
            let _ = race_with_abort(
                async {
                    emit_updates
                        .await
                        .map_err(anyhow::Error::from)
                        .and_then(|drained| drained)
                },
                signal,
            )
            .await;
            ExecutedToolCallOutcome {
                result: error_tool_result(signal, &error),
                is_error: true,
            }
        }
    }
}

fn error_tool_result(signal: Option<&AbortSignal>, error: &anyhow::Error) -> AgentToolResult {
    if signal.map(|s| s.is_aborted()).unwrap_or(false) {
        AgentToolResult::error("Tool execution aborted")
    } else {
        AgentToolResult::error(format!("{error:#}"))
    }
}

/// Port of `finalizeExecutedToolCall`: apply the `afterToolCall` overrides.
async fn finalize_executed_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    prepared: &PreparedToolCall,
    executed: ExecutedToolCallOutcome,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
) -> anyhow::Result<FinalizedToolCallOutcome> {
    let mut result = executed.result;
    let mut is_error = executed.is_error;

    if let Some(after_tool_call) = config.after_tool_call.as_ref() {
        let after_result = race_with_abort(
            after_tool_call(
                AfterToolCallContext {
                    assistant_message: assistant_message.clone(),
                    tool_call: prepared.tool_call.clone(),
                    args: prepared.args.clone(),
                    result: result.clone(),
                    is_error,
                    context: clone_context(current_context),
                },
                signal.cloned().unwrap_or_default(),
            ),
            signal,
        )
        .await;
        match after_result {
            Ok(Some(overrides)) => {
                // Field-by-field merge, no deep merge (TS semantics).
                let mut merged = result;
                if let Some(content) = overrides.content {
                    merged.content = content;
                }
                if let Some(details) = overrides.details {
                    merged.details = details;
                }
                if let Some(terminate) = overrides.terminate {
                    merged.terminate = Some(terminate);
                }
                if let Some(error) = overrides.is_error {
                    is_error = error;
                }
                result = merged;
            }
            Ok(None) => {}
            Err(error) => {
                result = AgentToolResult::error(format!("{error:#}"));
                is_error = true;
            }
        }
    }

    Ok(FinalizedToolCallOutcome {
        tool_call: prepared.tool_call.clone(),
        result,
        is_error,
    })
}

async fn emit_tool_execution_end(
    finalized: &FinalizedToolCallOutcome,
    emit: &AgentEventSink,
) -> anyhow::Result<()> {
    emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        result: finalized.result.clone(),
        is_error: finalized.is_error,
    })
    .await
}

/// Port of `createToolResultMessage`.
fn create_tool_result_message(finalized: &FinalizedToolCallOutcome) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        content: finalized.result.content.clone(),
        details: if finalized.result.details.is_null() {
            None
        } else {
            Some(finalized.result.details.clone())
        },
        is_error: finalized.is_error,
        timestamp: crate::now_ms(),
    }
}

async fn emit_tool_result_message(
    tool_result_message: &ToolResultMessage,
    emit: &AgentEventSink,
) -> anyhow::Result<()> {
    emit(AgentEvent::MessageStart {
        message: AgentMessage::from(tool_result_message.clone()),
    })
    .await?;
    emit(AgentEvent::MessageEnd {
        message: AgentMessage::from(tool_result_message.clone()),
    })
    .await
}
