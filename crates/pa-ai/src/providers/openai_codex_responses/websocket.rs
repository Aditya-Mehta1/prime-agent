//! Codex WebSocket transport: connections and the per-request event loop.
//! Section of the port of
//! `packages/ai/src/providers/openai-codex-responses.ts`.
//!
//! Each connection owns a worker task that holds the socket; requests talk to
//! it over a command channel and receive events over a fresh channel per
//! request. The handshake sends the same beta header the codex-rs client
//! sends (`OpenAI-Beta: responses_websockets=2026-02-06`) plus custom
//! headers via an explicit `http::Request` (plain `connect_async` accepts
//! `http::Request`, which carries headers).

use std::sync::atomic::{AtomicU64, Ordering};

use futures::stream::SplitStream;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use tokio_util::sync::CancellationToken;

use crate::providers::openai_codex_responses::errors::CodexStreamError;
use crate::providers::openai_codex_responses::session::{session_state, CachedConnection};

pub const OPENAI_BETA_RESPONSES_WEBSOCKETS: &str = "responses_websockets=2026-02-06";
pub(crate) const SESSION_WEBSOCKET_CACHE_TTL_MS: u64 = 5 * 60 * 1000;
const WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE: u16 = 1009;

#[allow(unused_imports)] // re-exported for consumers of the transport module
pub use crate::providers::openai_codex_responses::session::{
    clear_continuation, close_websocket_sessions, is_websocket_sse_fallback_active,
    record_request_stats, record_websocket_failure, record_websocket_sse_fallback,
    schedule_session_websocket_expiry, take_continuation_for,
};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Command sent to a connection's worker task.
pub(crate) enum WorkerCommand {
    /// Send one `response.create` request; events flow over `events`.
    Send {
        body: String,
        events: mpsc::Sender<WorkerEvent>,
    },
    Close,
}

/// Events a worker forwards to the active request.
pub enum WorkerEvent {
    /// A parsed stream event.
    Event(Value),
    /// Terminal marker after the completion event (Ok) or an error (Err).
    Terminal(Result<(), CodexStreamError>),
}

/// Connection-scoped continuation state
/// (`CachedWebSocketContinuationState` in the TS).
pub struct ContinuationState {
    pub last_request_body: Value,
    pub last_response_id: String,
    pub last_response_items: Vec<Value>,
    /// Id of the connection whose server-side response state this chain is
    /// anchored to.
    pub connection_id: u64,
}

/// Debug counters (`OpenAICodexWebSocketDebugStats` in the TS).
#[derive(Debug, Clone, Default)]
pub struct WebSocketDebugStats {
    pub requests: u64,
    pub connections_created: u64,
    pub connections_reused: u64,
    pub cached_context_requests: u64,
    pub store_true_requests: u64,
    pub full_context_requests: u64,
    pub delta_requests: u64,
    pub last_input_items: u64,
    pub last_delta_input_items: Option<u64>,
    pub last_previous_response_id: Option<String>,
    pub websocket_failures: u64,
    pub sse_fallbacks: u64,
    pub websocket_fallback_active: Option<bool>,
    pub last_websocket_error: Option<String>,
}

fn next_connection_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

/// Port of `connectWebSocket` + worker spawn: handshake with custom headers,
/// then run the reader loop until the channel closes.
/// Port of `connectWebSocket` + worker spawn: handshake with custom headers,
/// then run the reader loop until the channel closes.
async fn spawn_connection_worker(
    url: &str,
    headers: &[(String, String)],
    signal: Option<CancellationToken>,
) -> Result<(mpsc::Sender<WorkerCommand>, u64), CodexStreamError> {
    let mut request = url
        .into_client_request()
        .map_err(|error| CodexStreamError::Transport(format!("Invalid WebSocket URL: {error}")))?;
    for (name, value) in headers {
        let name = http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| CodexStreamError::Transport("Invalid WebSocket header".to_string()))?;
        let value = http::HeaderValue::from_str(value)
            .map_err(|_| CodexStreamError::Transport("Invalid WebSocket header".to_string()))?;
        request.headers_mut().insert(name, value);
    }

    if signal
        .as_ref()
        .map(|signal| signal.is_cancelled())
        .unwrap_or(false)
    {
        return Err(CodexStreamError::Aborted);
    }

    let connect = tokio_tungstenite::connect_async(request);
    let (stream, _response) = match signal.as_ref() {
        Some(signal) => {
            tokio::select! {
                _ = signal.cancelled() => return Err(CodexStreamError::Aborted),
                result = connect => result,
            }
        }
        None => connect.await,
    }
    .map_err(|error| CodexStreamError::Transport(format!("WebSocket connect failed: {error}")))?;

    let connection_id = next_connection_id();
    let (command_tx, command_rx) = mpsc::channel::<WorkerCommand>(4);
    tokio::spawn(connection_worker(stream, command_rx, signal));
    Ok((command_tx, connection_id))
}

/// Port of `parseWebSocket`: per-request event forwarding with completion
/// tracking, error extraction, and close-code reporting. The worker parks
/// between requests so a session can reuse one connection.
/// Port of `parseWebSocket`: per-request event forwarding with completion
/// tracking, error extraction, and close-code reporting. The worker parks
/// between requests so a session can reuse one connection.
async fn connection_worker(
    stream: WsStream,
    mut commands: mpsc::Receiver<WorkerCommand>,
    signal: Option<CancellationToken>,
) {
    let (mut sink, mut stream) = stream.split();
    loop {
        let Some(command) = commands.recv().await else {
            return;
        };
        match command {
            WorkerCommand::Close => {
                let _ = sink.close().await;
                return;
            }
            WorkerCommand::Send { body, events } => {
                if sink.send(Message::Text(body.into())).await.is_err() {
                    let _ = events
                        .send(WorkerEvent::Terminal(Err(CodexStreamError::Transport(
                            "WebSocket send failed".to_string(),
                        ))))
                        .await;
                    let _ = sink.close().await;
                    return;
                }
                let terminal = read_request_events(&mut stream, &events, &signal).await;
                if terminal.is_err() {
                    let _ = events.send(WorkerEvent::Terminal(terminal)).await;
                    let _ = sink.close().await;
                    return;
                }
                let _ = events.send(WorkerEvent::Terminal(terminal)).await;
            }
        }
    }
}

/// Read one request's events until completion, close, or error.
async fn read_request_events(
    stream: &mut SplitStream<WsStream>,
    events: &mpsc::Sender<WorkerEvent>,
    signal: &Option<CancellationToken>,
) -> Result<(), CodexStreamError> {
    let mut saw_completion = false;
    loop {
        if signal
            .as_ref()
            .map(|signal| signal.is_cancelled())
            .unwrap_or(false)
        {
            return Err(CodexStreamError::Aborted);
        }
        let next = stream.next();
        let message = match signal.as_ref() {
            Some(signal) => {
                tokio::select! {
                    _ = signal.cancelled() => return Err(CodexStreamError::Aborted),
                    message = next => message,
                }
            }
            None => next.await,
        };
        match message {
            None => break,
            Some(Ok(Message::Text(text))) => {
                let text = text.as_str().to_string();
                match serde_json::from_str::<Value>(&text) {
                    Ok(event) => {
                        let event_type = event.get("type").and_then(Value::as_str);
                        if matches!(
                            event_type,
                            Some("response.completed")
                                | Some("response.done")
                                | Some("response.incomplete")
                        ) {
                            saw_completion = true;
                        }
                        if events.send(WorkerEvent::Event(event)).await.is_err() {
                            return Err(CodexStreamError::Transport(
                                "WebSocket request cancelled".to_string(),
                            ));
                        }
                        if saw_completion {
                            return Ok(());
                        }
                    }
                    Err(error) => {
                        return Err(CodexStreamError::Transport(format!(
                            "Invalid Codex WebSocket JSON: {error}"
                        )));
                    }
                }
            }
            Some(Ok(Message::Binary(bytes))) => {
                // Codex events are JSON text; binary frames decode as UTF-8.
                let text = String::from_utf8_lossy(&bytes).to_string();
                match serde_json::from_str::<Value>(&text) {
                    Ok(event) => {
                        if events.send(WorkerEvent::Event(event)).await.is_err() {
                            return Err(CodexStreamError::Transport(
                                "WebSocket request cancelled".to_string(),
                            ));
                        }
                    }
                    Err(error) => {
                        return Err(CodexStreamError::Transport(format!(
                            "Invalid Codex WebSocket JSON: {error}"
                        )));
                    }
                }
            }
            Some(Ok(Message::Close(close))) => {
                if saw_completion {
                    return Ok(());
                }
                let code = close.as_ref().map(|frame| frame.code);
                let reason = close
                    .as_ref()
                    .map(|frame| frame.reason.to_string())
                    .unwrap_or_default();
                let code_text = code.map(|code| format!(" {code}")).unwrap_or_default();
                let mut reason_text = if reason.is_empty() {
                    String::new()
                } else {
                    format!(" {reason}")
                };
                if reason_text.is_empty()
                    && code == Some(CloseCode::from(WEBSOCKET_MESSAGE_TOO_BIG_CLOSE_CODE))
                {
                    reason_text = " message too big".to_string();
                }
                return Err(CodexStreamError::Transport(
                    format!("WebSocket closed{code_text}{reason_text}")
                        .trim()
                        .to_string(),
                ));
            }
            Some(Ok(Message::Ping(_)))
            | Some(Ok(Message::Pong(_)))
            | Some(Ok(Message::Frame(_))) => {
                continue;
            }
            Some(Err(error)) => {
                return Err(CodexStreamError::Transport(format!(
                    "WebSocket error: {error}"
                )));
            }
        }
    }
    if !saw_completion {
        return Err(CodexStreamError::Transport(
            "WebSocket stream closed before response.completed".to_string(),
        ));
    }
    Ok(())
}

/// A session-acquired connection handle (`{ socket, entry, reused, release }`).
pub struct AcquiredConnection {
    worker: mpsc::Sender<WorkerCommand>,
    pub session_id: Option<String>,
    pub reused: bool,
    /// A cache entry backs this connection (continuation is possible).
    pub cached: bool,
    pub connection_id: u64,
}

impl AcquiredConnection {
    /// Send the `response.create` request and return the event channel.
    pub async fn send_request(
        &self,
        body: &Value,
        signal: Option<CancellationToken>,
    ) -> Result<mpsc::Receiver<WorkerEvent>, CodexStreamError> {
        if signal
            .as_ref()
            .map(|signal| signal.is_cancelled())
            .unwrap_or(false)
        {
            return Err(CodexStreamError::Aborted);
        }
        let mut request = body.clone();
        request["type"] = Value::String("response.create".to_string());
        let (event_tx, event_rx) = mpsc::channel(64);
        self.worker
            .send(WorkerCommand::Send {
                body: request.to_string(),
                events: event_tx,
            })
            .await
            .map_err(|_| CodexStreamError::Transport("WebSocket connection closed".to_string()))?;
        Ok(event_rx)
    }

    /// Port of `closeWebSocketSilently`: ask the worker to close the socket.
    pub async fn close(&self) {
        let _ = self.worker.send(WorkerCommand::Close).await;
    }
}

/// Port of `isWebSocketSseFallbackActive`.
/// Port of `acquireWebSocket`: reuse the session's idle connection when
/// possible, otherwise open a fresh connection (cached per session).
pub async fn acquire_websocket(
    url: &str,
    headers: &[(String, String)],
    session_id: Option<&str>,
    signal: Option<CancellationToken>,
) -> Result<AcquiredConnection, CodexStreamError> {
    let Some(session_id) = session_id else {
        // No session: uncached one-shot connection, closed after the request.
        let (worker, connection_id) = spawn_connection_worker(url, headers, signal).await?;
        return Ok(AcquiredConnection {
            worker,
            session_id: None,
            reused: false,
            cached: false,
            connection_id,
        });
    };

    // Reuse the session's idle connection.
    let cached_worker = {
        let mut state = session_state().lock().ok();
        match state
            .as_mut()
            .and_then(|state| state.connections.get_mut(session_id))
        {
            Some(entry) if !entry.busy => {
                entry.busy = true;
                entry.expiry_generation += 1;
                Some((entry.worker.clone(), entry.connection_id))
            }
            _ => None,
        }
    };
    if let Some((worker, connection_id)) = cached_worker {
        return Ok(AcquiredConnection {
            worker,
            session_id: Some(session_id.to_string()),
            reused: true,
            cached: true,
            connection_id,
        });
    }

    // Fresh connection: cache it for the session unless an entry is busy.
    let entry_busy = session_state()
        .lock()
        .map(|state| {
            state
                .connections
                .get(session_id)
                .map(|entry| entry.busy)
                .unwrap_or(false)
        })
        .unwrap_or(false);
    let (worker, connection_id) = spawn_connection_worker(url, headers, signal).await?;

    let cached = if entry_busy {
        false
    } else {
        let Ok(mut state) = session_state().lock() else {
            return Ok(AcquiredConnection {
                worker,
                session_id: Some(session_id.to_string()),
                reused: false,
                cached: false,
                connection_id,
            });
        };
        state.connections.insert(
            session_id.to_string(),
            CachedConnection {
                worker: worker.clone(),
                busy: true,
                continuation: None,
                expiry_generation: 0,
                connection_id,
            },
        );
        true
    };

    Ok(AcquiredConnection {
        worker,
        session_id: Some(session_id.to_string()),
        reused: false,
        cached,
        connection_id,
    })
}

/// Port of `release`: return the connection to the cache with its new
/// continuation state, or close it. `keep` mirrors the TS `{ keep }` option.
/// Port of `release`: return the connection to the cache with its new
/// continuation state, or close it. `keep` mirrors the TS `{ keep }` option.
pub async fn release_connection(
    connection: AcquiredConnection,
    keep: bool,
    continuation: Option<ContinuationState>,
) {
    let session_id = connection.session_id.clone();
    let Some(session_id) = session_id else {
        // Uncached connections always close after the request.
        connection.close().await;
        return;
    };
    if !keep {
        close_websocket_sessions(Some(&session_id));
        return;
    }
    let matched = match session_state().lock() {
        Ok(mut state) => match state.connections.get_mut(&session_id) {
            Some(entry) if entry.connection_id == connection.connection_id => {
                entry.busy = false;
                entry.continuation = continuation;
                true
            }
            _ => false,
        },
        Err(_) => false,
    };
    if matched {
        schedule_session_websocket_expiry(&session_id);
    } else {
        connection.close().await;
    }
}

/// Port of `scheduleSessionWebSocketExpiry`: close the cached connection
/// after the TTL if it stayed idle.
/// Port of `requestBodiesMatchExceptInput` + `getCachedWebSocketInputDelta`:
/// compute the continuation delta (input items beyond the cached baseline)
/// for an otherwise-identical request body.
pub fn get_cached_websocket_input_delta(
    body: &Value,
    continuation: &ContinuationState,
) -> Option<Vec<Value>> {
    let strip = |value: &Value| -> Value {
        let mut stripped = value.clone();
        if let Some(map) = stripped.as_object_mut() {
            map.remove("input");
            map.remove("previous_response_id");
        }
        stripped
    };
    if strip(body) != strip(&continuation.last_request_body) {
        return None;
    }
    let current_input = body.get("input").and_then(Value::as_array).cloned()?;
    let last_input = continuation
        .last_request_body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut baseline = last_input;
    baseline.extend(continuation.last_response_items.iter().cloned());
    if current_input.len() < baseline.len() {
        return None;
    }
    let prefix = &current_input[..baseline.len()];
    if prefix != baseline.as_slice() {
        return None;
    }
    Some(current_input[baseline.len()..].to_vec())
}

/// Port of `buildCachedWebSocketRequestBody`.
pub fn build_cached_websocket_request_body(
    continuation: Option<&ContinuationState>,
    body: &Value,
    connection_id: u64,
) -> Value {
    let Some(continuation) = continuation else {
        return body.clone();
    };
    // Continuations are anchored to the connection that produced the
    // response; a different socket cannot resolve their previous_response_id.
    if continuation.connection_id != connection_id {
        return body.clone();
    }
    let delta = get_cached_websocket_input_delta(body, continuation);
    match delta {
        Some(delta) if !continuation.last_response_id.is_empty() => {
            let mut request = body.clone();
            request["previous_response_id"] = json!(continuation.last_response_id);
            request["input"] = Value::Array(delta);
            request
        }
        _ => body.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn continuation(last_body: Value, items: Vec<Value>) -> ContinuationState {
        ContinuationState {
            last_request_body: last_body,
            last_response_id: "resp_1".to_string(),
            last_response_items: items,
            connection_id: 42,
        }
    }

    #[test]
    fn computes_input_delta_for_matching_bodies() {
        let base = json!({ "model": "gpt", "input": [ { "type": "a" } ] });
        let items = vec![json!({ "type": "assistant_item" })];
        let cont = continuation(base.clone(), items);
        let next = json!({
            "model": "gpt",
            "input": [ { "type": "a" }, { "type": "assistant_item" }, { "type": "new" } ],
        });
        let request = build_cached_websocket_request_body(Some(&cont), &next, 42);
        assert_eq!(request["previous_response_id"], "resp_1");
        assert_eq!(request["input"], json!([{ "type": "new" }]));
    }

    #[test]
    fn rejects_delta_when_body_differs() {
        let base = json!({ "model": "gpt", "input": [ { "type": "a" } ] });
        let cont = continuation(base.clone(), vec![]);
        let next = json!({ "model": "other", "input": [ { "type": "a" }, { "type": "b" } ] });
        let request = build_cached_websocket_request_body(Some(&cont), &next, 42);
        assert!(request.get("previous_response_id").is_none());
        assert_eq!(request["input"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn rejects_delta_when_prefix_differs() {
        let base = json!({ "model": "gpt", "input": [ { "type": "a" } ] });
        let cont = continuation(base.clone(), vec![json!({ "type": "x" })]);
        let next = json!({ "model": "gpt", "input": [ { "type": "a" }, { "type": "y" } ] });
        let request = build_cached_websocket_request_body(Some(&cont), &next, 42);
        assert!(request.get("previous_response_id").is_none());
    }

    #[test]
    fn rejects_continuation_from_other_connection() {
        let base = json!({ "model": "gpt", "input": [] });
        let cont = continuation(base.clone(), vec![]);
        let request = build_cached_websocket_request_body(Some(&cont), &base, 7);
        assert!(request.get("previous_response_id").is_none());
    }
}
