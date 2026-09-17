//! The single-session ACP state machine: admission, prompt lifecycle,
//! cancel, and close over the in-process session engine.
//!
//! One ACP connection drives one session. A second `session/new` is refused
//! rather than silently sharing conversation state, cwd, and queues.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use pa_agent::abort::AbortSignal;
use pa_agent::agent::{Agent, Subscription};
use pa_agent::stream::AssistantMessageEvent;
use pa_agent::types::{AgentEvent, AgentMessage, Message, StopReason};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::events::{acp_updates_for_event, AcpEngineEvent, MappingState};
use super::jsonrpc;
use super::meta::{
    PrimeAgentEventPhase, PrimeAgentOutcome, PrimeAgentQuiescenceMeta, PrimeAgentSessionMeta,
};
use super::producer::UpdateProducer;
use super::types::{parse_prompt_blocks, AcpSessionUpdate, ImageBlock, PromptBlockError};

/// One hosted ACP session: the producer, the engine event subscription, and
/// the prompt-lifecycle bookkeeping. The engine itself stays owned by the
/// mode entry; the session handle is shared with the turn tasks.
pub struct AcpSession {
    pub id: String,
    producer: Arc<UpdateProducer>,
    subscription: Mutex<Option<Subscription>>,
    agent: Arc<Agent>,
    cancel_requested: AtomicBool,
}

impl AcpSession {
    /// Admit a session: subscribe the engine event feed before anything can
    /// publish, so no update is lost between admission and the response.
    pub async fn new(id: String, agent: Arc<Agent>, producer: Arc<UpdateProducer>) -> AcpSession {
        let mapping = Arc::new(Mutex::new(MappingState::default()));
        let subscription = subscribe_engine_events(&agent, producer.clone(), mapping).await;
        AcpSession {
            id,
            producer,
            subscription: Mutex::new(Some(subscription)),
            agent,
            cancel_requested: AtomicBool::new(false),
        }
    }

    /// Release the engine event subscription; no further updates flow.
    pub async fn unsubscribe(&self) {
        let subscription = self.subscription.lock().await.take();
        if let Some(subscription) = subscription {
            subscription.unsubscribe().await;
        }
    }

    /// Fence the producer (a closed session publishes nothing until a
    /// replacement `session/new` is admitted).
    pub async fn close_producer(&self) {
        self.producer.close().await;
    }

    pub fn cancel_requested(&self) -> bool {
        self.cancel_requested.load(Ordering::SeqCst)
    }

    pub fn request_cancel(&self) {
        self.cancel_requested.store(true, Ordering::SeqCst);
    }

    pub fn producer(&self) -> &Arc<UpdateProducer> {
        &self.producer
    }

    pub fn agent(&self) -> &Arc<Agent> {
        &self.agent
    }
}

/// Map the engine loop events onto ACP updates for the session lifetime.
async fn subscribe_engine_events(
    agent: &Arc<Agent>,
    producer: Arc<UpdateProducer>,
    mapping: Arc<Mutex<MappingState>>,
) -> Subscription {
    agent
        .subscribe(move |event: AgentEvent, _signal: AbortSignal| {
            let producer = producer.clone();
            let mapping = mapping.clone();
            Box::pin(async move {
                let turn_id = producer.active_prompt_turn().await;
                let events: Vec<AcpEngineEvent> = project_event(&event);
                for engine_event in events {
                    let updates = {
                        let mut mapping = mapping.lock().await;
                        acp_updates_for_event(&engine_event, &mut mapping)
                    };
                    for update in updates {
                        producer
                            .publish(&update, turn_id, PrimeAgentEventPhase::Event, None)
                            .await;
                    }
                }
                Ok(())
            })
        })
        .await
}

/// Project one loop event onto the ACP adapter's input vocabulary. Streaming
/// deltas carry a plain string; the discriminator (reasoning vs visible
/// text) lives in the stream event variant.
fn project_event(event: &AgentEvent) -> Vec<AcpEngineEvent> {
    match event {
        AgentEvent::MessageStart { message } => vec![AcpEngineEvent::MessageStart {
            role: message.role().to_string(),
        }],
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => match assistant_message_event.as_ref() {
            AssistantMessageEvent::TextDelta { delta, .. } if !delta.is_empty() => {
                vec![AcpEngineEvent::AssistantDelta {
                    thinking: false,
                    delta: delta.clone(),
                }]
            }
            AssistantMessageEvent::ThinkingDelta { delta, .. } if !delta.is_empty() => {
                vec![AcpEngineEvent::AssistantDelta {
                    thinking: true,
                    delta: delta.clone(),
                }]
            }
            _ => Vec::new(),
        },
        AgentEvent::MessageEnd { message } => vec![AcpEngineEvent::MessageEnd {
            role: message.role().to_string(),
        }],
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => vec![AcpEngineEvent::ToolExecutionStart {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            args: args.clone(),
        }],
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => vec![AcpEngineEvent::ToolExecutionEnd {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            result: serde_json::to_value(result).unwrap_or(Value::Null),
            is_error: *is_error,
        }],
        _ => Vec::new(),
    }
}

/// The transcript as it stood before a turn started, recorded so the turn's
/// own messages can be told apart from everything older. Compaction can fire
/// during a turn and rebuild the message list, so membership is tracked by
/// a content key the rebuild preserves, not by index.
#[derive(Debug, Default)]
pub struct TurnBoundary {
    keys: Vec<String>,
}

impl TurnBoundary {
    pub async fn capture(agent: &Agent) -> TurnBoundary {
        let state = agent.state().await;
        TurnBoundary {
            keys: state.messages.iter().filter_map(message_key).collect(),
        }
    }

    fn contains(&self, message: &AgentMessage) -> bool {
        message_key(message).is_some_and(|key| self.keys.contains(&key))
    }
}

/// Key for a kept message: (role, timestamp, stopReason, errorMessage) as the
/// wire tuple. Compaction drops messages; it does not rewrite them.
fn message_key(message: &AgentMessage) -> Option<String> {
    let AgentMessage::Standard(Message::Assistant(assistant)) = message else {
        return None;
    };
    let stop_reason = serde_json::to_value(assistant.stop_reason).unwrap_or(Value::Null);
    Some(
        json!([
            "assistant",
            assistant.timestamp,
            stop_reason,
            assistant.error_message,
        ])
        .to_string(),
    )
}

/// Error text from an assistant message this turn produced, when it failed.
///
/// Only messages that were not in the transcript before the turn are
/// considered: scanning the whole transcript would let an earlier failed
/// turn reject a later turn that never called the model, reporting a stale
/// error.
pub async fn turn_failure(agent: &Agent, boundary: &TurnBoundary) -> Option<String> {
    let state = agent.state().await;
    for message in state.messages.iter().rev() {
        let AgentMessage::Standard(Message::Assistant(assistant)) = message else {
            continue;
        };
        // The newest assistant message predates the turn, so the turn
        // appended none.
        if boundary.contains(message) {
            return None;
        }
        if assistant.stop_reason != StopReason::Error {
            return None;
        }
        return Some(
            assistant
                .error_message
                .clone()
                .unwrap_or_else(|| "the model request failed".to_string()),
        );
    }
    None
}

/// The frame pair that brackets every settled turn: the completion event and
/// the terminal quiescence envelope. In-process sessions have no RLM
/// children and no autonomous continuation slots, so quiescence is trivially
/// reached at the moment of settlement.
pub async fn publish_completion_envelope(
    session: &AcpSession,
    turn_id: u64,
    outcome: PrimeAgentOutcome,
) -> anyhow::Result<()> {
    let quiescence = PrimeAgentQuiescenceMeta {
        outstanding_subagents: 0,
        remaining_autonomous_continuations: 0,
    };
    let completion = AcpSessionUpdate::SessionInfoUpdate {
        meta: super::meta::prime_agent_meta(PrimeAgentSessionMeta {
            quiescence: Some(quiescence.clone()),
            ..Default::default()
        }),
    };
    if !session
        .producer()
        .publish(&completion, turn_id, PrimeAgentEventPhase::Event, None)
        .await
    {
        anyhow::bail!("Failed to publish ACP completion update");
    }
    let terminal = AcpSessionUpdate::SessionInfoUpdate {
        meta: super::meta::prime_agent_meta(PrimeAgentSessionMeta {
            quiescence: Some(quiescence),
            ..Default::default()
        }),
    };
    if !session
        .producer()
        .publish(
            &terminal,
            turn_id,
            PrimeAgentEventPhase::TerminalQuiescence,
            Some(outcome),
        )
        .await
    {
        anyhow::bail!("Failed to publish ACP terminal quiescence update");
    }
    Ok(())
}

/// Publish the correlated response boundary in front of a prompt response.
/// `expected` tells the client whether a terminal quiescence envelope
/// follows (an accepted turn) or not (failed admission).
pub async fn publish_response_boundary(
    session: &AcpSession,
    turn_id: u64,
    expected: bool,
    outcome: PrimeAgentOutcome,
) -> anyhow::Result<()> {
    let boundary = AcpSessionUpdate::SessionInfoUpdate {
        meta: super::meta::prime_agent_meta(PrimeAgentSessionMeta {
            terminal_quiescence_expected: Some(expected),
            ..Default::default()
        }),
    };
    if !session
        .producer()
        .publish(
            &boundary,
            turn_id,
            PrimeAgentEventPhase::ResponseBoundary,
            Some(outcome),
        )
        .await
    {
        anyhow::bail!("Failed to publish ACP response boundary");
    }
    Ok(())
}

/// Render a prompt-block failure as the ACP invalid-params error. The TS
/// SDK validates the request schema; the Rust port validates the blocks it
/// actually reads.
pub fn prompt_block_error(id: &Value, error: PromptBlockError) -> Value {
    jsonrpc::error_response(
        id.clone(),
        jsonrpc::INVALID_PARAMS,
        "Invalid params",
        Some(json!({ "reason": error.to_string() })),
    )
}

/// The prompt content admitted into a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmittedPrompt {
    pub text: String,
    pub images: Vec<ImageBlock>,
}

impl AdmittedPrompt {
    pub fn parse(prompt: &[Value]) -> Result<AdmittedPrompt, PromptBlockError> {
        let (text, images) = parse_prompt_blocks(prompt)?;
        Ok(AdmittedPrompt { text, images })
    }
}
