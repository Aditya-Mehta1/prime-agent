//! Session engine contract.
//!
//! The worker drives a [`SessionEngine`]: the worker owns the session store,
//! the queue, event sequencing, and wire framing; the engine owns turn
//! behavior. Today that is the scripted faux session (echo/scripted replies)
//! used by the integration harness and headless checks; the agent-loop crate
//! plugs into the same trait without touching any daemon mechanics.

use anyhow::Result;
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
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// The user message that was accepted (recorded into the session store).
    UserMessage(Value),
    /// An assistant message update (streaming); the payload is the full message.
    AssistantUpdate(Value),
    /// The final assistant message (recorded into the session store).
    AssistantMessage(Value),
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
}

/// A scripted faux session: replays a deterministic sequence of assistant
/// messages for the first N prompts, then echoes. Script format (JSON):
/// `{"responses": ["text one", {"text": "two", "delayMs": 250}]}`.
#[derive(Debug, Clone, Default)]
pub struct ScriptedEngine {
    responses: Vec<Value>,
}

impl ScriptedEngine {
    pub fn from_value(script: Value) -> Result<Self> {
        let responses = script
            .get("responses")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(ScriptedEngine { responses })
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

impl SessionEngine for ScriptedEngine {
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
        if !emit(EngineEvent::AssistantUpdate(
            json!({"role": "assistant", "content": "", "provider": "scripted", "model": "faux-1", "timestamp": crate::util::now_ms()}),
        )) {
            emit(cancelled());
            return;
        }
        if !emit(EngineEvent::AssistantMessage(
            json!({"role": "assistant", "content": text, "provider": "scripted", "model": "faux-1", "timestamp": crate::util::now_ms()}),
        )) {
            emit(cancelled());
            return;
        }
        emit(EngineEvent::Done(Ok(())));
    }
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
