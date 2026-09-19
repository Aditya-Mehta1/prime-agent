//! Real-provider stream adapter: pa-ai completion streaming bridged into the
//! pa-agent loop's `StreamFn`/`ModelStream`, crossing the crate boundary by
//! wire-shape (JSON) round-trip. Shared by pa-cli (print/json modes) and
//! pa-daemon (session workers).

use std::sync::Arc;

use pa_agent::stream::{LlmContext, ModelStream, StreamFn, StreamRequestOptions};
use pa_agent::types::{Model as AgentModel, ThinkingLevel};
use pa_types::ai::Model;

/// Wire-shape conversion at the pa-agent/pa-ai boundary: both sides serialize
/// to the same camelCase wire shapes.
pub fn json_round_trip<T, U>(value: &T) -> Option<U>
where
    T: serde::Serialize,
    U: serde::de::DeserializeOwned,
{
    serde_json::to_value(value)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
}

/// Thinking-level mapping across the two crates.
pub fn map_thinking_level(level: pa_types::ai::ModelThinkingLevel) -> ThinkingLevel {
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

/// The mutable provider target a live session's stream reads per call:
/// daemon `set_model` swaps it without rebuilding the session.
#[derive(Debug, Clone)]
pub struct ProviderTarget {
    pub api_key: Option<String>,
    pub model: Model,
}

/// A real pa-ai provider stream adapter for the agent loop, reading its
/// target from a shared slot the host can swap live (`set_model`). The
/// slot is `None` only before the host sets the build-time target; the
/// adapter never runs before that.
pub fn switchable_stream_fn(target: Arc<std::sync::RwLock<Option<ProviderTarget>>>) -> StreamFn {
    Arc::new(
        move |_requested: AgentModel, context: LlmContext, options: StreamRequestOptions| {
            let ProviderTarget { api_key, model } = target
                .read()
                .expect("provider target lock")
                .clone()
                .expect("provider target set before the first stream");
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
                        if let Some(converted) = convert_stream_event(&event) {
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

/// A stream adapter pinned to one target: the headless runtimes (print and
/// json modes) resolve their model once, so the slot never changes.
pub fn real_stream_fn(api_key: Option<String>, model: Model) -> StreamFn {
    switchable_stream_fn(Arc::new(std::sync::RwLock::new(Some(ProviderTarget {
        api_key,
        model,
    }))))
}

/// Convert one pa-ai stream event into the pa-agent loop's event enum.
/// Payloads cross the boundary by wire-shape (JSON) round-trip.
pub fn convert_stream_event(
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

#[cfg(test)]
mod tests {
    //! Regression guard for the pa-agent -> pa-ai message boundary: the wire
    //! round-trip must keep user messages. `UserPart` must stay `type`-tagged
    //! like the TS wire format; an untagged variant serializes parts without
    //! `"type"`, the pa-ai shape rejects them, and `real_stream_fn` silently
    //! dropped every prompt admitted via `AgentPromptInput::Text` (content
    //! parts), leaving the provider with a system prompt only.

    use super::*;

    #[test]
    fn prompt_text_message_round_trips() {
        let message = pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::User(
            pa_agent::types::UserMessage {
                content: pa_agent::types::UserContent::Parts(vec![
                    pa_agent::types::UserPart::Text(pa_agent::types::TextContent {
                        text: "reply with ok".into(),
                        text_signature: None,
                    }),
                ]),
                timestamp: 1,
            },
        ));
        let converted: Option<pa_types::ai::Message> = json_round_trip(&message);
        assert_eq!(
            converted,
            Some(pa_types::ai::Message::User(pa_types::ai::UserMessage {
                content: pa_types::ai::UserContent::Blocks(vec![
                    pa_types::ai::UserContentBlock::Text(pa_types::ai::TextContent {
                        text: "reply with ok".into(),
                        text_signature: None,
                        rest: Default::default(),
                    }),
                ]),
                timestamp: 1,
                rest: Default::default(),
            }))
        );
    }
}
