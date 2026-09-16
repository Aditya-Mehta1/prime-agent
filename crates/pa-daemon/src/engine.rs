//! Session engine contract.
//!
//! The worker drives a [`SessionEngine`]: the worker owns the session store,
//! the queue, event sequencing, and wire framing; the engine owns turn
//! behavior. Today that is the scripted faux session (echo/scripted replies)
//! used by the integration harness and headless checks; the agent-loop crate
//! plugs into the same trait without touching any daemon mechanics.

use anyhow::Result;
use pa_agent::abort::AbortSignal;
use pa_core::session_engine::provider_retry::ProviderRetryPolicy;
use pa_core::session_engine::side_question::{SideQuestionSink, SideQuestionTurn};
use serde_json::{json, Value};

/// One user prompt accepted by the engine.
#[derive(Debug, Clone)]
pub struct PromptRequest {
    pub message: String,
    pub source: String,
    pub agent_message_id: Option<String>,
}

/// Events an engine emits for one prompt, in order. The worker translates these
/// into protocol events and session-store writes. Returning `false` from the
/// emit callback cancels the prompt.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// The user message that was accepted (recorded into the session store).
    UserMessage(Value),
    /// An assistant message update (streaming); the payload is the full
    /// message, plus the provider stream event that produced it (the TS wire
    /// carries `assistantMessageEvent` so clients can track activity).
    AssistantUpdate {
        message: Value,
        stream_event: Option<Value>,
    },
    /// The final assistant message (recorded into the session store).
    AssistantMessage(Value),
    /// A tool call started executing.
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    /// A tool produced a partial result while still executing.
    ToolExecutionUpdate {
        tool_call_id: String,
        partial_result: Value,
    },
    /// A tool call finished; `is_error` mirrors the tool result.
    ToolExecutionEnd {
        tool_call_id: String,
        result: Value,
        is_error: bool,
    },
    /// The prompt completed (successfully or not).
    Done(std::result::Result<(), String>),
}

/// The turn behavior a worker session runs.
pub trait SessionEngine: Send + Sync {
    /// Run one prompt. `prompt_index` counts accepted prompts for this session.
    fn run_prompt(
        &self,
        prompt_index: usize,
        request: PromptRequest,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    );

    /// Run one side question: a second LLM turn over a clone of the
    /// conversation with the serialized previous turns replayed, excluded
    /// from the session history. `signal` aborts the run; `sink` receives
    /// partial answers while the run streams (the worker translates them
    /// into `side_question_event` outbounds).
    fn run_side_question(
        &self,
        request: SideQuestionRequest,
        signal: &AbortSignal,
        sink: &SideQuestionSink,
    ) -> SideQuestionOutcome;

    /// Context window (tokens) of the engine's resolved model, when known.
    /// Drives the `contextUsage` estimate in `get_session_stats`; engines
    /// without model metadata report `None` and the field is omitted.
    fn model_context_window(&self) -> Option<u64> {
        None
    }

    /// The engine's resolved model as connection-state wire data
    /// (`{ id, provider, reasoning }`), when known. Drives the interactive
    /// splash and tray labels.
    fn model_metadata(&self) -> Option<Value> {
        None
    }
}

/// One side-question request (the `start_side_question` command fields).
#[derive(Debug, Clone)]
pub struct SideQuestionRequest {
    /// Caller-generated id; echoed on every event of the run.
    pub side_question_id: String,
    pub question: String,
    /// Earlier `{question, answer}` exchanges replayed before the question.
    pub previous_turns: Vec<SideQuestionTurn>,
}

/// How one side-question run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideQuestionOutcome {
    /// Answered; the string is the final answer text.
    Complete { answer: String },
    /// Aborted mid-run; the string is the partial answer streamed so far.
    Aborted { answer: String },
    /// Failed; the string is the provider/engine error message.
    Failed { answer: String, error: String },
}

/// Wire form of one side-question status (TS `SideQuestionStatus`).
pub const SIDE_QUESTION_STATUS_RUNNING: &str = "running";
pub const SIDE_QUESTION_STATUS_COMPLETE: &str = "complete";
pub const SIDE_QUESTION_STATUS_CANCELLED: &str = "cancelled";
pub const SIDE_QUESTION_STATUS_ERROR: &str = "error";

/// Wire form of one side-question event (TS `SideQuestionEvent`).
pub fn side_question_event_value(
    request: &SideQuestionRequest,
    answer: &str,
    status: &str,
    error_message: Option<&str>,
) -> Value {
    let mut event = json!({
        "id": request.side_question_id,
        "question": request.question,
        "answer": answer,
        "status": status,
    });
    if let Some(error_message) = error_message {
        event["errorMessage"] = json!(error_message);
    }
    event
}

impl SideQuestionOutcome {
    /// The TS wire status of this outcome.
    pub fn status_str(&self) -> &'static str {
        match self {
            SideQuestionOutcome::Complete { .. } => SIDE_QUESTION_STATUS_COMPLETE,
            SideQuestionOutcome::Aborted { .. } => SIDE_QUESTION_STATUS_CANCELLED,
            SideQuestionOutcome::Failed { .. } => SIDE_QUESTION_STATUS_ERROR,
        }
    }

    /// The answer text carried by the final event (partial on abort/failure).
    pub fn answer(&self) -> &str {
        match self {
            SideQuestionOutcome::Complete { answer }
            | SideQuestionOutcome::Aborted { answer }
            | SideQuestionOutcome::Failed { answer, .. } => answer,
        }
    }

    /// The error message carried by the final event, when the run failed.
    pub fn error_message(&self) -> Option<&str> {
        match self {
            SideQuestionOutcome::Failed { error, .. } => Some(error.as_str()),
            _ => None,
        }
    }
}

/// A scripted faux session: replays a deterministic sequence of assistant
/// messages for the first N prompts, then echoes. Script format (JSON):
/// `{"responses": ["text one", {"text": "two", "delayMs": 250}],
/// "sideQuestion": {"responses": [...], "retry": {...}}}`.
///
/// The `sideQuestion` seam scripts the side-question provider calls, one
/// scripted result per attempt: `{"text": "...", "delayMs": 250}` answers,
/// `{"error": "...", "kind": "server_error", "status": 500,
/// "retryAfterMs": 100}` fails that attempt (retried per `retry`, which is
/// the shared provider policy with test-friendly delays). Verification
/// harness only; never set by the product.
#[derive(Debug, Clone, Default)]
pub struct ScriptedEngine {
    responses: Vec<Value>,
    side_question: SideQuestionScript,
}

/// Scripted side-question provider results, consumed one per attempt.
#[derive(Debug, Clone, Default)]
struct SideQuestionScript {
    responses: Vec<Value>,
    /// Retry policy for scripted provider failures; `None` uses the shared
    /// default policy.
    retry: Option<ProviderRetryPolicy>,
}

impl ScriptedEngine {
    pub fn from_value(script: Value) -> Result<Self> {
        let responses = script
            .get("responses")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let side_question = script
            .get("sideQuestion")
            .map(|side_question| SideQuestionScript {
                responses: side_question
                    .get("responses")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                retry: side_question.get("retry").map(|retry| ProviderRetryPolicy {
                    enabled: retry
                        .get("enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    max_retries: retry.get("maxRetries").and_then(Value::as_u64).unwrap_or(0)
                        as u32,
                    base_delay_ms: retry
                        .get("baseDelayMs")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    max_retry_delay_ms: retry
                        .get("maxRetryDelayMs")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                }),
            })
            .unwrap_or_default();
        Ok(ScriptedEngine {
            responses,
            side_question,
        })
    }

    pub fn from_file(path: &std::path::Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Self::from_value(serde_json::from_str(&content)?)
    }

    fn response_text(response: &Value) -> String {
        match response {
            Value::String(text) => text.clone(),
            Value::Object(_) => response
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            _ => String::new(),
        }
    }

    fn response_delay_ms(response: &Value) -> u64 {
        response
            .get("delayMs")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(60_000)
    }
}

/// Plausible usage block so scripted messages match the real engine's wire
/// shape (and exercise summary aggregation).
fn scripted_usage() -> Value {
    json!({
        "input": 120, "output": 8, "cacheRead": 0, "cacheWrite": 0,
        "totalTokens": 128,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 },
    })
}

impl SessionEngine for ScriptedEngine {
    fn model_metadata(&self) -> Option<Value> {
        Some(json!({
            "id": "faux-1",
            "provider": "scripted",
            "reasoning": false,
        }))
    }

    fn run_prompt(
        &self,
        prompt_index: usize,
        request: PromptRequest,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) {
        let cancelled = || EngineEvent::Done(Err("prompt cancelled".to_string()));
        let scripted = self.responses.get(prompt_index).cloned();
        let text = match scripted {
            Some(response) => {
                let delay = Self::response_delay_ms(&response);
                if delay > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                }
                Self::response_text(&response)
            }
            None => format!("echo: {}", request.message),
        };
        if !emit(EngineEvent::UserMessage(
            json!({"role": "user", "content": request.message, "timestamp": crate::util::now_ms()}),
        )) {
            emit(cancelled());
            return;
        }
        let usage = scripted_usage();
        if !emit(EngineEvent::AssistantUpdate {
            message: json!({"role": "assistant", "content": "", "provider": "scripted", "model": "faux-1", "usage": usage.clone(), "timestamp": crate::util::now_ms()}),
            stream_event: None,
        }) {
            emit(cancelled());
            return;
        }
        if !emit(EngineEvent::AssistantMessage(
            json!({"role": "assistant", "content": text, "provider": "scripted", "model": "faux-1", "usage": usage, "timestamp": crate::util::now_ms()}),
        )) {
            emit(cancelled());
            return;
        }
        emit(EngineEvent::Done(Ok(())));
    }

    fn run_side_question(
        &self,
        request: SideQuestionRequest,
        signal: &AbortSignal,
        sink: &SideQuestionSink,
    ) -> SideQuestionOutcome {
        use pa_core::session_engine::provider_retry::{
            complete_with_provider_retry, DEFAULT_PROVIDER_RETRY_POLICY,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;

        // Unscripted side questions echo, like the prompt fallback.
        if self.side_question.responses.is_empty() {
            let answer = format!("echo: {}", request.question);
            if signal.is_aborted() || !sink(&answer) {
                return SideQuestionOutcome::Aborted { answer };
            }
            return SideQuestionOutcome::Complete { answer };
        }

        let policy = self
            .side_question
            .retry
            .clone()
            .unwrap_or(DEFAULT_PROVIDER_RETRY_POLICY);
        // Every scripted side-question run replays its results from the top,
        // like a fresh side conversation per run.
        let responses = StdArc::new(self.side_question.responses.clone());
        let attempt_index = StdArc::new(AtomicUsize::new(0));
        let sink = StdArc::clone(sink);
        let signal = signal.clone();
        let wait_signal = signal.clone();
        let attempt_signal = signal.clone();
        let result = futures::executor::block_on(complete_with_provider_retry(
            &policy,
            Some(&signal),
            move |delay| {
                let wait_signal = wait_signal.clone();
                async move { abortable_sleep(delay, &wait_signal) }
            },
            move || {
                let responses = StdArc::clone(&responses);
                let attempt_index = StdArc::clone(&attempt_index);
                let sink = StdArc::clone(&sink);
                let signal = attempt_signal.clone();
                async move {
                    let index = attempt_index.fetch_add(1, Ordering::SeqCst);
                    let Some(entry) = responses.get(index) else {
                        anyhow::bail!("No more scripted side-question responses");
                    };
                    Ok(scripted_side_question_turn(entry, &sink, &signal))
                }
            },
        ));
        let failed = |answer: String, error: String| SideQuestionOutcome::Failed { answer, error };
        match result {
            Ok(message) => {
                let text = match &message.content[0] {
                    pa_agent::types::AssistantContent::Text(text) => text.text.clone(),
                    _ => String::new(),
                };
                match message.stop_reason {
                    pa_agent::types::StopReason::Stop => {
                        SideQuestionOutcome::Complete { answer: text }
                    }
                    pa_agent::types::StopReason::Aborted => {
                        SideQuestionOutcome::Aborted { answer: text }
                    }
                    _ => failed(
                        text,
                        message
                            .error_message
                            .unwrap_or_else(|| "Side question failed".to_string()),
                    ),
                }
            }
            Err(error) => failed(String::new(), error.to_string()),
        }
    }
}

/// Scripted side-question turn statuses ride the assistant message's stop
/// reason; text blocks carry the (partial) answer.
fn scripted_side_question_turn(
    entry: &Value,
    sink: &SideQuestionSink,
    signal: &AbortSignal,
) -> pa_agent::types::AssistantMessage {
    use pa_agent::types::{
        AssistantContent, AssistantMessage, AssistantMessageDiagnostic, StopReason, TextContent,
    };
    let base = || AssistantMessage {
        content: vec![AssistantContent::Text(TextContent {
            text: String::new(),
            text_signature: None,
        })],
        api: String::new(),
        provider: "scripted".to_string(),
        model: "faux-1".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: pa_agent::types::Usage::zero(),
        stop_reason: StopReason::Stop,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
    };
    let mut message = base();
    let set_text = |message: &mut AssistantMessage, text: &str| {
        let AssistantContent::Text(block) = &mut message.content[0] else {
            return;
        };
        block.text = text.to_string();
    };
    let text = ScriptedEngine::response_text(entry);
    if let Some(error) = entry.get("error").and_then(Value::as_str) {
        // Scripted provider failure with structured classification, so the
        // shared retry policy sees the same details a real provider records.
        let kind = entry.get("kind").and_then(Value::as_str);
        let status = entry.get("status").and_then(Value::as_u64);
        let retry_after_ms = entry.get("retryAfterMs").and_then(Value::as_u64);
        message.stop_reason = StopReason::Error;
        message.error_message = Some(error.to_string());
        message.diagnostics = Some(vec![AssistantMessageDiagnostic {
            kind: "provider_stream_failure".to_string(),
            timestamp: 0,
            error: None,
            details: Some(json!({
                "kind": kind,
                "status": status,
                "retryAfterMs": retry_after_ms,
            })),
        }]);
        return message;
    }
    // Stream the partial answer, then wait the scripted delay abortably.
    if signal.is_aborted() || !sink(&text) {
        set_text(&mut message, &text);
        message.stop_reason = StopReason::Aborted;
        return message;
    }
    let delay_ms = ScriptedEngine::response_delay_ms(entry);
    if delay_ms > 0 && !abortable_sleep(std::time::Duration::from_millis(delay_ms), signal) {
        set_text(&mut message, &text);
        message.stop_reason = StopReason::Aborted;
        return message;
    }
    if signal.is_aborted() {
        set_text(&mut message, &text);
        message.stop_reason = StopReason::Aborted;
        return message;
    }
    set_text(&mut message, &text);
    message
}

/// Sleep `delay` in slices, stopping early when `signal` aborts.
/// Returns `false` when the wait ended aborted.
fn abortable_sleep(delay: std::time::Duration, signal: &AbortSignal) -> bool {
    let mut remaining = delay;
    while !remaining.is_zero() {
        if signal.is_aborted() {
            return false;
        }
        let slice = remaining.min(std::time::Duration::from_millis(25));
        std::thread::sleep(slice);
        remaining = remaining.saturating_sub(slice);
    }
    !signal.is_aborted()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn scripted_engine_replays_then_echoes() {
        let engine = ScriptedEngine::from_value(
            json!({"responses": ["first", {"text": "second", "delayMs": 0}]}),
        )
        .unwrap();
        let request_for = |message: &str| PromptRequest {
            message: message.to_string(),
            source: "test".to_string(),
            agent_message_id: None,
        };
        let collect = |engine: &ScriptedEngine, index: usize, message: &str| {
            let mut final_message = None;
            engine.run_prompt(index, request_for(message), &mut |event| {
                if let EngineEvent::AssistantMessage(value) = event {
                    final_message = Some(value);
                }
                true
            });
            final_message
        };
        assert_eq!(collect(&engine, 0, "hi").unwrap()["content"], "first");
        assert_eq!(collect(&engine, 1, "go").unwrap()["content"], "second");
        assert_eq!(
            collect(&engine, 2, "more").unwrap()["content"],
            "echo: more"
        );
    }

    #[test]
    fn cancellation_stops_the_prompt() {
        let engine = ScriptedEngine::default();
        let seen = Arc::new(AtomicUsize::new(0));
        let seen_clone = seen.clone();
        engine.run_prompt(
            0,
            PromptRequest {
                message: "x".into(),
                source: "test".into(),
                agent_message_id: None,
            },
            &mut |event| {
                seen_clone.fetch_add(1, Ordering::SeqCst);
                match event {
                    EngineEvent::UserMessage(_) => false, // cancel right away
                    EngineEvent::Done(_) => true,
                    _ => true,
                }
            },
        );
        assert_eq!(seen.load(Ordering::SeqCst), 2); // user message + done
    }
}
