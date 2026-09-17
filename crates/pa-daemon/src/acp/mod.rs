//! ACP stdio mode: a thin JSON-RPC transport over the session engine.
//!
//! One connection hosts at most one session. `session/new` admits the
//! session (reporting a cwd mismatch instead of adopting one), `session/
//! prompt` drives one engine turn with follow-up queueing semantics, and
//! `session/cancel` / `session/close` stop work. Every frame leaves through
//! one ordered write queue, so responses and `session/update` notifications
//! interleave exactly in publication order. The process exits when stdin
//! closes.

mod events;
mod jsonrpc;
mod meta;
mod producer;
mod session;
mod types;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use pa_core::session_engine::engine::SessionEngine;
use pa_core::session_engine::{PromptOptions, PromptOutcome, StreamingBehavior};

use jsonrpc::Incoming;
use meta::{PrimeAgentOutcome, PrimeAgentSessionMeta};
use producer::UpdateProducer;
use session::{AcpSession, TurnBoundary};
use types::{
    initialize_result, session_id_params, AcpStopReason, AcpStopReasonResponse, NewSessionParams,
    PromptParams,
};

/// Everything the mode needs from the composition root.
pub struct AcpOptions {
    /// The running session engine (`create_session` output).
    pub engine: Arc<SessionEngine>,
    /// The cwd the session actually runs in, fixed at startup.
    pub actual_cwd: PathBuf,
    /// The product version reported in `initialize`.
    pub product_version: String,
}

/// The hosted-session slot plus the in-flight admission bookkeeping.
#[derive(Default)]
struct ConnectionState {
    session: Option<SessionEntry>,
    session_new_in_flight: bool,
    session_close_in_flight: bool,
}

/// One hosted session and its in-flight prompt turn, if any.
struct SessionEntry {
    session: Arc<AcpSession>,
    prompt_task: Option<tokio::task::JoinHandle<()>>,
}

/// Run the ACP stdio mode until stdin closes. Returns the process exit code.
pub async fn run_acp_mode(options: AcpOptions) -> Result<i32> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(frame) = rx.recv().await {
            let Ok(mut line) = serde_json::to_string(&frame) else {
                continue;
            };
            line.push('\n');
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if stdout.flush().await.is_err() {
                break;
            }
        }
    });

    let state = Arc::new(Mutex::new(ConnectionState::default()));
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        if line.trim().is_empty() {
            continue;
        }
        let request = match jsonrpc::parse_line(&line) {
            Ok(incoming) => incoming,
            Err(error_response) => {
                let _ = tx.send(error_response);
                continue;
            }
        };
        let handler = spawn_handler(
            request,
            state.clone(),
            options.engine.clone(),
            options.actual_cwd.clone(),
            options.product_version.clone(),
            tx.clone(),
        );
        handler.await.ok();
    }

    // Exit when the client disconnects: stop the resident work, release the
    // subscription, fence the producer, and let the writer drain.
    teardown(&state).await;
    drop(tx);
    let _ = writer.await;
    Ok(0)
}

/// Stop the hosted session after stdin closes: abort work, settle the prompt
/// task, release the subscription, and fence the producer.
async fn teardown(state: &Arc<Mutex<ConnectionState>>) {
    let entry = {
        let mut state = state.lock().await;
        state.session.take()
    };
    let Some(mut entry) = entry else {
        return;
    };
    entry.session.agent().abort();
    entry.session.agent().clear_all_queues();
    entry.session.agent().wait_for_idle().await;
    if let Some(task) = entry.prompt_task.take() {
        let _ = task.await;
    }
    entry.session.unsubscribe().await;
    entry.session.close_producer().await;
}

#[allow(clippy::too_many_arguments)]
fn spawn_handler(
    request: Incoming,
    state: Arc<Mutex<ConnectionState>>,
    engine: Arc<SessionEngine>,
    actual_cwd: PathBuf,
    product_version: String,
    tx: producer::FrameSink,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        match request {
            Incoming::Request { id, method, params } => {
                handle_request(
                    id,
                    method,
                    params,
                    state,
                    engine,
                    actual_cwd,
                    product_version,
                    tx,
                )
                .await;
            }
            Incoming::Notification { method, params } => {
                handle_notification(method, params, state).await;
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn handle_request(
    id: Value,
    method: String,
    params: Value,
    state: Arc<Mutex<ConnectionState>>,
    engine: Arc<SessionEngine>,
    actual_cwd: PathBuf,
    product_version: String,
    tx: producer::FrameSink,
) {
    match method.as_str() {
        "initialize" => handle_initialize(id, params, product_version, tx),
        "session/new" => {
            handle_session_new(id, params, state, engine, actual_cwd, tx).await;
        }
        "session/prompt" => {
            handle_session_prompt(id, params, state, engine, tx).await;
        }
        "session/close" => {
            handle_session_close(id, params, state, engine, tx).await;
        }
        other => {
            // The observed TS response: the message quotes the constant, and
            // the method name rides under `data.method`.
            let _ = tx.send(jsonrpc::error_response(
                id,
                jsonrpc::METHOD_NOT_FOUND,
                &format!("\"Method not found\": {other}"),
                Some(json!({ "method": other })),
            ));
        }
    }
}

fn handle_initialize(id: Value, params: Value, product_version: String, tx: producer::FrameSink) {
    if let Err(error_response) = validate_initialize(&id, &params) {
        let _ = tx.send(error_response);
        return;
    }
    let result = serde_json::to_value(initialize_result(&product_version)).expect("serializes");
    let _ = tx.send(jsonrpc::response(id, result));
}

/// The `initialize` schema check the TS SDK performs: the protocol version
/// must be a number. The error body mirrors the observed TS response.
fn validate_initialize(id: &Value, params: &Value) -> std::result::Result<(), Value> {
    let field_error = |received: &str| {
        jsonrpc::error_response(
            id.clone(),
            jsonrpc::INVALID_PARAMS,
            "Invalid params",
            Some(json!({
                "_errors": [],
                "protocolVersion": {
                    "_errors": [format!("Invalid input: expected number, received {received}")]
                },
            })),
        )
    };
    match params.get("protocolVersion") {
        None => Err(field_error("undefined")),
        Some(value) if value.is_number() => Ok(()),
        Some(Value::String(_)) => Err(field_error("string")),
        Some(Value::Bool(_)) => Err(field_error("boolean")),
        Some(Value::Null) => Err(field_error("null")),
        Some(_) => Err(field_error("object")),
    }
}

async fn handle_notification(method: String, params: Value, state: Arc<Mutex<ConnectionState>>) {
    if method != "session/cancel" {
        return;
    }
    let session_id = session_id_params(&params);
    // Only cancel the addressed session: aborting unconditionally would kill
    // whichever turn happens to be running, and leave the real turn's stop
    // reason wrong.
    let session = {
        let state = state.lock().await;
        let Some(entry) = state
            .session
            .as_ref()
            .filter(|entry| entry.session.id == session_id && entry.prompt_task.is_some())
        else {
            return;
        };
        if entry.session.cancel_requested() {
            return;
        }
        entry.session.clone()
    };
    session.request_cancel();
    session.agent().abort();
    session.agent().clear_all_queues();
}

#[allow(clippy::too_many_arguments)]
async fn handle_session_new(
    id: Value,
    params: Value,
    state: Arc<Mutex<ConnectionState>>,
    engine: Arc<SessionEngine>,
    actual_cwd: PathBuf,
    tx: producer::FrameSink,
) {
    // Reserve the single-session slot before the first await: two
    // concurrent requests must not both pass the empty-slot check while
    // cwd reads are in flight.
    {
        let mut state = state.lock().await;
        if state.session.is_some() || state.session_new_in_flight || state.session_close_in_flight {
            let _ = tx.send(internal_error(
                &id,
                "prime-agent ACP mode hosts one session per connection; start another prime-agent process for a second session",
            ));
            return;
        }
        state.session_new_in_flight = true;
    }

    let result = session_new(&id, params, &engine, &actual_cwd, tx.clone()).await;

    let mut state = state.lock().await;
    state.session_new_in_flight = false;
    if let Ok(entry) = result {
        state.session = Some(entry);
    }
}

/// Admit one session. On failure the error response has already been queued.
async fn session_new(
    id: &Value,
    params: Value,
    engine: &Arc<SessionEngine>,
    actual_cwd: &Path,
    tx: producer::FrameSink,
) -> std::result::Result<SessionEntry, ()> {
    let params = NewSessionParams::parse(&params);
    if !params.mcp_servers.is_empty() {
        let _ = tx.send(jsonrpc::error_response(
            id.clone(),
            jsonrpc::INVALID_PARAMS,
            "Invalid params",
            Some(json!({ "reason": "MCP servers are unavailable in this ACP host" })),
        ));
        return Err(());
    }
    // The agent's cwd is fixed at startup; a client-supplied cwd is reported
    // back in `_meta` when it differs, never adopted.
    let mut cwd_mismatch = None;
    if let Some(requested) = params.cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
        if !same_cwd(Path::new(requested), actual_cwd) {
            cwd_mismatch = Some(meta::PrimeAgentCwdMeta {
                requested: requested.to_string(),
                actual: actual_cwd.display().to_string(),
            });
        }
    }

    let session_id = uuid::Uuid::new_v4().to_string();
    let producer = UpdateProducer::new(session_id.clone(), tx.clone());
    let session = Arc::new(
        AcpSession::new(session_id.clone(), engine.session.agent().clone(), producer).await,
    );

    let mut result = json!({ "sessionId": session_id });
    if let Some(cwd_mismatch) = cwd_mismatch {
        result["_meta"] = meta::prime_agent_meta(PrimeAgentSessionMeta {
            cwd: Some(cwd_mismatch),
            ..Default::default()
        });
    }
    // Queue the admission response before opening the producer gate, so no
    // held update can precede it.
    let _ = tx.send(jsonrpc::response(id.clone(), result));
    session.producer().commit_session_new_response().await;
    Ok(SessionEntry {
        session,
        prompt_task: None,
    })
}

async fn handle_session_prompt(
    id: Value,
    params: Value,
    state: Arc<Mutex<ConnectionState>>,
    engine: Arc<SessionEngine>,
    tx: producer::FrameSink,
) {
    let params = PromptParams::parse(&params);
    // Admission: one prompt turn at a time, behind any started cancellation.
    let (session, turn_id) = {
        let mut state = state.lock().await;
        let closing = state.session_close_in_flight;
        let Some(entry) = state.session.as_mut() else {
            let _ = tx.send(internal_error(
                &id,
                &format!("Unknown ACP session: {}", params.session_id),
            ));
            return;
        };
        if closing {
            let _ = tx.send(internal_error(
                &id,
                &format!("ACP session is closing: {}", params.session_id),
            ));
            return;
        }
        if entry.prompt_task.is_some() {
            let _ = tx.send(internal_error(
                &id,
                "A prompt turn is already running for this ACP session",
            ));
            return;
        }
        let turn_id = entry.session.producer().begin_prompt().await;
        (entry.session.clone(), turn_id)
    };
    if session.cancel_requested() {
        // This prompt was admitted after a cancellation started; it is
        // dropped by the cancel, so report the protocol stop reason instead
        // of a request error.
        session.producer().finish_prompt(turn_id).await;
        let _ = tx.send(jsonrpc::response(
            id,
            stop_reason_response(AcpStopReason::Cancelled),
        ));
        return;
    }

    let admitted_prompt = match session::AdmittedPrompt::parse(&params.prompt) {
        Ok(prompt) => prompt,
        Err(error) => {
            session.producer().finish_prompt(turn_id).await;
            let _ = tx.send(session::prompt_block_error(&id, error));
            return;
        }
    };

    // The turn runs as its own task so the reader loop can keep serving
    // session/cancel and session/close while it settles.
    let task = tokio::spawn(run_prompt_turn(
        id,
        params.session_id.clone(),
        turn_id,
        admitted_prompt,
        session,
        state.clone(),
        engine.clone(),
        tx.clone(),
    ));
    let mut state = state.lock().await;
    if let Some(entry) = state.session.as_mut() {
        if entry.session.id == params.session_id {
            entry.prompt_task = Some(task);
        }
    }
}

/// One prompt turn: admission into the engine, streaming, and the
/// correlated boundary / completion envelope in front of the response.
#[allow(clippy::too_many_arguments)]
async fn run_prompt_turn(
    id: Value,
    session_id: String,
    turn_id: u64,
    admitted_prompt: session::AdmittedPrompt,
    session: Arc<AcpSession>,
    state: Arc<Mutex<ConnectionState>>,
    engine: Arc<SessionEngine>,
    tx: producer::FrameSink,
) {
    let boundary = TurnBoundary::capture(engine.session.agent()).await;
    let prompt_result = engine
        .session
        .prompt_with_images(
            &admitted_prompt.text,
            admitted_prompt
                .images
                .into_iter()
                .map(|image| pa_agent::types::ImageContent {
                    data: image.data,
                    mime_type: image.mime_type,
                })
                .collect(),
            PromptOptions {
                streaming_behavior: Some(StreamingBehavior::FollowUp),
                queue_if_busy: true,
                ..Default::default()
            },
        )
        .await;

    let admitted = match prompt_result {
        // Session commands are admitted as text in this slice; the engine
        // command path is not driven in-process, and the turn settles with
        // no new assistant message.
        Ok(PromptOutcome::Prompt) | Ok(PromptOutcome::SessionCommand(_)) => true,
        Err(error) => {
            // Failed prompt admission gets one correlated error boundary; it
            // never gets an invented terminal-quiescence update.
            let _ = session::publish_response_boundary(
                &session,
                turn_id,
                false,
                PrimeAgentOutcome::Error,
            )
            .await;
            session.producer().finish_prompt(turn_id).await;
            let _ = tx.send(internal_error(&id, &format!("{error:#}")));
            return;
        }
    };
    let _ = admitted;

    engine.session.agent().wait_for_idle().await;
    let cancelled = session.cancel_requested();
    let failure = session::turn_failure(engine.session.agent(), &boundary).await;

    if cancelled {
        // A cancellation before the response boundary resolves the request
        // with the protocol stop reason and no boundary frames.
        session.producer().finish_prompt(turn_id).await;
        let _ = tx.send(jsonrpc::response(
            id,
            stop_reason_response(AcpStopReason::Cancelled),
        ));
        clear_prompt_slot(&state, &session_id).await;
        return;
    }

    let outcome = if failure.is_some() {
        PrimeAgentOutcome::Error
    } else {
        PrimeAgentOutcome::Result
    };
    // The response boundary precedes the correlated response; the completion
    // event and the terminal quiescence envelope follow it in publication
    // order.
    if session::publish_response_boundary(&session, turn_id, true, outcome)
        .await
        .is_err()
    {
        session.producer().finish_prompt(turn_id).await;
        let _ = tx.send(internal_error(
            &id,
            "Failed to publish ACP response boundary",
        ));
        clear_prompt_slot(&state, &session_id).await;
        return;
    }
    if session::publish_completion_envelope(&session, turn_id, outcome)
        .await
        .is_err()
    {
        session.producer().finish_prompt(turn_id).await;
        let _ = tx.send(internal_error(
            &id,
            "Failed to publish ACP completion update",
        ));
        clear_prompt_slot(&state, &session_id).await;
        return;
    }

    session.producer().finish_prompt(turn_id).await;
    let response = match failure {
        Some(failure) => internal_error(&id, &format!("prime-agent turn failed: {failure}")),
        None => jsonrpc::response(id, stop_reason_response(AcpStopReason::EndTurn)),
    };
    let _ = tx.send(response);
    clear_prompt_slot(&state, &session_id).await;
}

/// The running turn released the prompt slot; close/EOF no longer awaits it.
async fn clear_prompt_slot(state: &Arc<Mutex<ConnectionState>>, session_id: &str) {
    let mut state = state.lock().await;
    if let Some(entry) = state.session.as_mut() {
        if entry.session.id == session_id {
            entry.prompt_task = None;
        }
    }
}

async fn handle_session_close(
    id: Value,
    params: Value,
    state: Arc<Mutex<ConnectionState>>,
    engine: Arc<SessionEngine>,
    tx: producer::FrameSink,
) {
    let session_id = session_id_params(&params);
    // Stop real work, not just local bookkeeping: closing aborts the
    // connection the same way session/cancel does.
    let taken = {
        let mut state = state.lock().await;
        if state.session_close_in_flight {
            None
        } else {
            match state.session.take() {
                Some(entry) if entry.session.id == session_id => {
                    state.session_close_in_flight = true;
                    Some(entry)
                }
                _ => {
                    let _ = tx.send(internal_error(
                        &id,
                        &format!("Unknown ACP session: {session_id}"),
                    ));
                    return;
                }
            }
        }
    };
    let Some(mut entry) = taken else {
        let _ = tx.send(internal_error(
            &id,
            &format!("ACP session is already closing: {session_id}"),
        ));
        return;
    };
    engine.session.agent().abort();
    engine.session.agent().clear_all_queues();
    engine.session.agent().wait_for_idle().await;
    // The cancelled prompt resolves before the close response: the turn task
    // is awaited first and its frames already sit in the write queue.
    if let Some(task) = entry.prompt_task.take() {
        let _ = task.await;
    }
    entry.session.unsubscribe().await;
    // Keep the backing session fenced until a replacement ACP session is
    // admitted.
    entry.session.close_producer().await;
    let _ = tx.send(jsonrpc::response(id, json!({})));
    let mut state = state.lock().await;
    state.session_close_in_flight = false;
}

/// The `session/prompt` success response: the terminal stop reason.
fn stop_reason_response(stop_reason: AcpStopReason) -> Value {
    serde_json::to_value(AcpStopReasonResponse { stop_reason }).expect("serializes")
}

fn internal_error(id: &Value, details: &str) -> Value {
    jsonrpc::error_response(
        id.clone(),
        jsonrpc::INTERNAL_ERROR,
        "Internal error",
        Some(json!({ "details": details })),
    )
}

/// Two paths are the same cwd when their canonical forms match, or when they
/// are the same directory on disk (dev/inode) — the bind-mount and
/// case-normalized-FS cases a lexical comparison misses.
fn same_cwd(requested: &Path, actual: &Path) -> bool {
    let canonical = |path: &Path| -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    };
    let requested = canonical(requested);
    let actual = canonical(actual);
    if requested == actual {
        return true;
    }
    #[cfg(unix)]
    {
        let identity = |path: &Path| -> Option<(u64, u64)> {
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(path).ok()?;
            if metadata.dev() == 0 || metadata.ino() == 0 {
                return None;
            }
            Some((metadata.dev(), metadata.ino()))
        };
        if let (Some(left), Some(right)) = (identity(&requested), identity(&actual)) {
            return left == right;
        }
    }
    false
}
