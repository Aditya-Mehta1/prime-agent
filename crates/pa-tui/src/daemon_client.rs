//! JSONL daemon client for the interactive UI.
//!
//! Connects to the supervisor's client socket, performs the `daemon_hello`
//! handshake, sends `type: "command"` envelopes, and matches responses by
//! envelope id. Frames that are not responses (session events, list progress,
//! closing notices) are forwarded to the caller through an event channel, so
//! the UI loop can render live session state while requests are in flight.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use pa_types::daemon::{
    DaemonCommand, DaemonCommandEnvelope, DaemonCommandFrameType, DaemonProtocolInfo,
    DaemonResponse, DAEMON_PROTOCOL_NAME, DAEMON_PROTOCOL_VERSION,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// Default response timeout (TS `DEFAULT_DAEMON_REQUEST_TIMEOUT_MS`).
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;
/// Requests whose completion is bounded by the turn itself
/// (`prompt_and_wait`, `wait_for_idle`) use the supervisor's long route
/// timeout so a long turn cannot expire the request.
pub const LONG_RUNNING_REQUEST_TIMEOUT_MS: u64 = 600_000;
const CONNECT_TIMEOUT_MS: u64 = 3_000;
const HELLO_TIMEOUT_MS: u64 = 3_000;

/// A non-response frame forwarded to the UI event loop. Payloads that are
/// owned by the session engine stay raw JSON (`Value`) so the client keeps
/// working across schema revisions.
#[derive(Debug, Clone)]
pub enum DaemonClientEvent {
    /// `session_event`: one streamed agent/turn event for an attached session.
    SessionEvent {
        active_session_id: String,
        event: Value,
    },
    /// `session_closed`: the attached session stopped existing.
    SessionClosed {
        active_session_id: String,
        reason: String,
    },
    /// `session_list_item` progress frame of `list_saved_sessions`.
    SessionListItem { session: Value },
    /// `session_list_progress` progress frame of `list_saved_sessions`.
    SessionListProgress { loaded: u64, total: u64 },
    /// `daemon_closing`: the supervisor is going down.
    DaemonClosing { reason: String },
}

/// Connection state shared between the request side and the reader task.
struct Shared {
    /// Pending requests keyed by envelope id.
    pending: Mutex<HashMap<String, oneshot::Sender<DaemonResponse>>>,
}

impl Shared {
    fn resolve(&self, id: &str, response: DaemonResponse) -> bool {
        let mut pending = self.pending.lock().unwrap();
        pending
            .remove(id)
            .is_some_and(|tx| tx.send(response).is_ok())
    }
}

/// A live connection to the daemon supervisor socket.
pub struct DaemonClient {
    socket_path: PathBuf,
    client_id: String,
    protocol: DaemonProtocolInfo,
    /// Full `daemon_hello` frame (schema id, app version, capabilities).
    hello: Value,
    next_request_id: Arc<AtomicU64>,
    shared: Arc<Shared>,
    writer: mpsc::UnboundedSender<String>,
}

impl DaemonClient {
    /// Connect to `socket_path`, complete the hello handshake, and return the
    /// client plus the event receiver. The event receiver must be polled or
    /// the reader task stalls once the channel's buffer fills.
    pub async fn connect(
        socket_path: &Path,
    ) -> Result<(Self, mpsc::UnboundedReceiver<DaemonClientEvent>)> {
        let connect = tokio::time::timeout(
            Duration::from_millis(CONNECT_TIMEOUT_MS),
            pa_types::platform::transport::connect_transport(socket_path),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "Timed out after {CONNECT_TIMEOUT_MS}ms connecting to the Prime Agent daemon. Socket: {}.",
                socket_path.display()
            )
        })?
        .with_context(|| {
            format!(
                "Failed to connect to the Prime Agent daemon. Socket: {}.",
                socket_path.display()
            )
        })?;

        let (reader_half, writer_half) = connect.split();
        let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<DaemonClientEvent>();
        let (hello_tx, hello_rx) = oneshot::channel::<Value>();
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
        });
        let reader_shared = Arc::clone(&shared);

        // Writer task: serializes one line per write.
        tokio::spawn(async move {
            let mut writer = writer_half;
            while let Some(line) = line_rx.recv().await {
                let mut payload = line.into_bytes();
                payload.push(b'\n');
                if writer.write_all(&payload).await.is_err() {
                    break;
                }
            }
            let _ = writer.shutdown().await;
        });

        // Reader task: dispatch every inbound line.
        tokio::spawn(async move {
            let mut reader = BufReader::new(reader_half);
            let mut line = String::new();
            let mut hello_tx = Some(hello_tx);
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                let frame_type = value
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match frame_type {
                    "daemon_hello" => {
                        let Some(tx) = hello_tx.take() else { continue };
                        let _ = tx.send(value);
                    }
                    "response" => {
                        if let Ok(response) =
                            serde_json::from_value::<DaemonResponse>(value.clone())
                        {
                            let id = response.id.clone().unwrap_or_default();
                            reader_shared.resolve(&id, response);
                        }
                    }
                    "session_event" => {
                        let _ = event_tx.send(DaemonClientEvent::SessionEvent {
                            active_session_id: value
                                .get("activeSessionId")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            event: value.get("event").cloned().unwrap_or(Value::Null),
                        });
                    }
                    "session_closed" => {
                        let _ = event_tx.send(DaemonClientEvent::SessionClosed {
                            active_session_id: value
                                .get("activeSessionId")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            reason: value
                                .get("reason")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        });
                    }
                    "session_list_item" => {
                        let _ = event_tx.send(DaemonClientEvent::SessionListItem {
                            session: value.get("session").cloned().unwrap_or(Value::Null),
                        });
                    }
                    "session_list_progress" => {
                        let _ = event_tx.send(DaemonClientEvent::SessionListProgress {
                            loaded: value.get("loaded").and_then(Value::as_u64).unwrap_or(0),
                            total: value.get("total").and_then(Value::as_u64).unwrap_or(0),
                        });
                    }
                    "daemon_closing" => {
                        let _ = event_tx.send(DaemonClientEvent::DaemonClosing {
                            reason: value
                                .get("reason")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        });
                    }
                    _ => {}
                }
            }
        });

        // Hello handshake: the supervisor sends daemon_hello immediately on
        // connect (TS `waitForHello`).
        let hello = tokio::time::timeout(Duration::from_millis(HELLO_TIMEOUT_MS), hello_rx)
            .await
            .map_err(|_| {
                anyhow!(
                    "Timed out after {HELLO_TIMEOUT_MS}ms waiting for the Prime Agent daemon handshake. Socket: {}.",
                    socket_path.display()
                )
            })?
            .map_err(|_| anyhow!("the daemon connection closed before the handshake"))?;
        let protocol = hello
            .get("protocol")
            .cloned()
            .and_then(|p| serde_json::from_value::<DaemonProtocolInfo>(p).ok())
            .unwrap_or(DaemonProtocolInfo {
                name: DAEMON_PROTOCOL_NAME.to_string(),
                version: DAEMON_PROTOCOL_VERSION,
            });
        if protocol.name != DAEMON_PROTOCOL_NAME {
            return Err(anyhow!(
                "the daemon on {} speaks an unknown protocol \"{}\"",
                socket_path.display(),
                protocol.name
            ));
        }
        // Envelope protocol version: the shared minimum (TS `request`).
        let version = protocol.version.min(DAEMON_PROTOCOL_VERSION);

        Ok((
            DaemonClient {
                socket_path: socket_path.to_path_buf(),
                client_id: format!("daemon-tui:{}", std::process::id()),
                protocol: DaemonProtocolInfo {
                    name: DAEMON_PROTOCOL_NAME.to_string(),
                    version,
                },
                hello,
                next_request_id: Arc::new(AtomicU64::new(0)),
                shared,
                writer: line_tx,
            },
            event_rx,
        ))
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Protocol identity negotiated in the hello handshake.
    pub fn protocol(&self) -> &DaemonProtocolInfo {
        &self.protocol
    }

    /// The full `daemon_hello` frame the supervisor sent on connect.
    pub fn hello(&self) -> &Value {
        &self.hello
    }

    /// Send one command envelope and wait for the matching response, using
    /// the TS default timeout for the command class.
    pub async fn request(&self, command: DaemonCommand) -> Result<DaemonResponse> {
        let timeout_ms = match &command {
            DaemonCommand::PromptAndWait { .. } | DaemonCommand::WaitForIdle { .. } => {
                LONG_RUNNING_REQUEST_TIMEOUT_MS
            }
            _ => DEFAULT_REQUEST_TIMEOUT_MS,
        };
        self.request_with_timeout(command, timeout_ms).await
    }

    /// Send one command envelope and wait up to `timeout_ms` for the response.
    pub async fn request_with_timeout(
        &self,
        command: DaemonCommand,
        timeout_ms: u64,
    ) -> Result<DaemonResponse> {
        let id = format!(
            "daemon_{}",
            self.next_request_id.fetch_add(1, Ordering::SeqCst) + 1
        );
        let envelope = DaemonCommandEnvelope {
            frame_type: DaemonCommandFrameType::Command,
            id: id.clone(),
            protocol: self.protocol.clone(),
            client_id: Some(self.client_id.clone()),
            command,
        };
        let line = serde_json::to_string(&envelope)?;
        let (tx, rx) = oneshot::channel::<DaemonResponse>();
        self.shared.pending.lock().unwrap().insert(id.clone(), tx);
        self.writer
            .send(line)
            .map_err(|_| anyhow!("the daemon connection is closed"))?;
        tokio::time::timeout(Duration::from_millis(timeout_ms), rx)
            .await
            .map_err(|_| {
                self.shared.pending.lock().unwrap().remove(&id);
                anyhow!(
                    "Timed out after {timeout_ms}ms waiting for the Prime Agent daemon response. Socket: {}.",
                    self.socket_path.display()
                )
            })?
            .map_err(|_| {
                anyhow!(
                    "Connection to the Prime Agent daemon closed. Socket: {}.",
                    self.socket_path.display()
                )
            })
    }

    /// Send a command and require `success: true`, surfacing the daemon error
    /// string otherwise.
    pub async fn request_ok(&self, command: DaemonCommand) -> Result<Value> {
        let name = command_type_debug(&command);
        let response = self.request(command).await?;
        if !response.success {
            return Err(anyhow!(
                "the daemon rejected the {name} request: {}",
                response
                    .error
                    .unwrap_or_else(|| "unknown error".to_string())
            ));
        }
        Ok(response.data.unwrap_or(Value::Null))
    }

    /// Close the connection; pending requests fail with the closed error.
    pub fn close(&self) {
        let _ = self.writer.send(String::new());
    }
}

/// Wire `type` tag of a command, for error messages.
fn command_type_debug(command: &DaemonCommand) -> String {
    serde_json::to_value(command)
        .ok()
        .and_then(|value| value.get("type").cloned())
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "command".to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::net::UnixListener;

    /// Minimal scripted supervisor used by client tests: hello on connect,
    /// canned responses keyed by command type.
    async fn spawn_mock_daemon(listener: UnixListener) {
        let (stream, _) = listener.accept().await.expect("accept");
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let hello = json!({
            "type": "daemon_hello",
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "clientId": "srv",
            "serverCapabilities": [],
        });
        let mut line = serde_json::to_string(&hello).unwrap();
        line.push('\n');
        writer.write_all(line.as_bytes()).await.unwrap();
        let mut seen = String::new();
        loop {
            seen.clear();
            match reader.read_line(&mut seen).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let envelope: Value = serde_json::from_str(seen.trim()).unwrap();
            let id = envelope["id"].as_str().unwrap().to_string();
            let command_type = envelope["command"]["type"].as_str().unwrap();
            if command_type == "prompt" {
                // Stream an event before the response, like the real worker.
                let event = json!({
                    "type": "session_event",
                    "activeSessionId": "s1",
                    "event": { "type": "turn_end" },
                });
                let mut payload = serde_json::to_string(&event).unwrap();
                payload.push('\n');
                writer.write_all(payload.as_bytes()).await.unwrap();
            }
            let response = json!({
                "type": "response",
                "id": id,
                "command": command_type,
                "success": true,
                "data": { "ok": true },
            });
            let mut payload = serde_json::to_string(&response).unwrap();
            payload.push('\n');
            writer.write_all(payload.as_bytes()).await.unwrap();
        }
    }

    fn empty_prompt_input() -> pa_types::daemon::PromptInput {
        pa_types::daemon::PromptInput {
            content: None,
            images: None,
            streaming_behavior: None,
            queue_if_busy: None,
            expand_prompt_templates: None,
            source: None,
            agent_message_id: None,
            custom_message: None,
            queue_key: None,
            prefix_messages: None,
            admission_id: None,
        }
    }

    #[tokio::test]
    async fn handshake_and_request_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("d.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move { spawn_mock_daemon(listener).await });

        let (client, mut events) = DaemonClient::connect(&socket).await.unwrap();
        assert_eq!(client.protocol().version, 7);
        let data = client
            .request_ok(DaemonCommand::List {
                id: None,
                all: None,
                cwd: None,
                session_dir: None,
                include_client_owned: None,
                rest: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(data["ok"], true);

        // A session event frames arrives out of band, ahead of its response.
        client
            .request_ok(DaemonCommand::Prompt {
                id: None,
                active_session_id: "s1".to_string(),
                message: "hi".to_string(),
                input: empty_prompt_input(),
                rest: Default::default(),
            })
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                event,
                DaemonClientEvent::SessionEvent { ref event, .. } if event["type"] == "turn_end"
            ),
            "unexpected event: {event:?}"
        );
        client.close();
    }

    #[tokio::test]
    async fn request_timeout_reports_socket() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("d.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        // A daemon that sends hello but never answers commands.
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut writer = stream;
            let mut hello = json!({
                "type": "daemon_hello",
                "protocol": { "name": "prime-agent.daemon", "version": 7 },
                "clientId": "srv",
                "serverCapabilities": [],
            })
            .to_string();
            hello.push('\n');
            writer.write_all(hello.as_bytes()).await.unwrap();
            std::future::pending::<()>().await;
        });
        let (client, _events) = DaemonClient::connect(&socket).await.unwrap();
        let error = client
            .request_with_timeout(
                DaemonCommand::List {
                    id: None,
                    all: None,
                    cwd: None,
                    session_dir: None,
                    include_client_owned: None,
                    rest: Default::default(),
                },
                100,
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("Timed out after"),
            "unexpected error: {error}"
        );
        assert!(error
            .to_string()
            .contains(socket.display().to_string().as_str()));
    }
}
