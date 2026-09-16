//! Session worker runtime: one process, one session.
//!
//! Port of the TS daemon's worker mode (`modes/daemon/daemon-mode.ts` worker
//! branch, `modes/session-worker/*`): the worker owns the session - the
//! append-only store, the queue lanes, event sequencing, and turn execution.
//! Supervisors connect over a private-framed Unix socket and authenticate
//! with the bootstrap token before any command.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, oneshot, Notify};

use crate::agent_engine::{AgentEngineConfig, AgentSessionEngine};
use crate::engine::{EngineEvent, PromptRequest, ScriptedEngine, SessionEngine};
use crate::framing::{write_frame, DEFAULT_PRIVATE_FRAME_LIMITS};
use crate::journal::WorkerRecoveryJournal;
use crate::paths;
use crate::protocol::{
    create_daemon_event_meta, create_daemon_replay_info, current_protocol_info,
    default_client_capabilities, default_server_capabilities, normalize_client_capabilities,
    response_failure, response_success, DaemonOutbound, DaemonResponse, DaemonResumeCursor,
    DaemonSessionClosedReason, DAEMON_APP_VERSION, DAEMON_SCHEMA_ID, DAEMON_SCHEMA_REVISION,
};
use crate::session_store::{session_file_name, SessionFile};
use crate::types::{AgentConnectionState, SessionActionSnapshot, SessionSummary};

/// TS-parity worker environment variables (`daemon-worker-protocol.ts`).
pub const WORKER_ROLE_ENV: &str = "PRIME_AGENT_INTERNAL_DAEMON_WORKER";
pub const WORKER_TOKEN_ENV: &str = "PRIME_AGENT_INTERNAL_DAEMON_WORKER_TOKEN";
pub const WORKER_INSTANCE_ID_ENV: &str = "PRIME_AGENT_INTERNAL_DAEMON_WORKER_INSTANCE_ID";
pub const WORKER_ACTIVE_SESSION_ID_ENV: &str =
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_ACTIVE_SESSION_ID";
pub const WORKER_SUPERVISOR_SOCKET_ENV: &str = "PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_SOCKET";
pub const WORKER_RECOVERY_JOURNAL_ENV: &str = "PRIME_AGENT_INTERNAL_DAEMON_WORKER_RECOVERY_JOURNAL";
/// Scripted-engine script file for faux sessions (integration harness).
pub const WORKER_SCRIPT_ENV: &str = "PRIME_AGENT_INTERNAL_DAEMON_WORKER_SCRIPT";
/// Worker socket path (supervisor passes it explicitly).
pub const WORKER_SOCKET_ENV: &str = "PRIME_AGENT_INTERNAL_DAEMON_WORKER_SOCKET";

const QUEUE_SNAPSHOT_CUSTOM_TYPE: &str = "prime-agent-rs.queue_snapshot";

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub socket_path: PathBuf,
    pub supervisor_socket_path: PathBuf,
    pub token: String,
    pub worker_instance_id: String,
    pub active_session_id: String,
    pub agent_dir: PathBuf,
    pub recovery_journal_path: PathBuf,
    pub script: Option<Value>,
}

impl WorkerConfig {
    pub fn from_env() -> Result<Self> {
        let socket_path: PathBuf = std::env::var_os(WORKER_SOCKET_ENV)
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("worker socket path is required ({WORKER_SOCKET_ENV})"))?;
        let token =
            std::env::var(WORKER_TOKEN_ENV).context("worker authentication token is required")?;
        let active_session_id = std::env::var(WORKER_ACTIVE_SESSION_ID_ENV)
            .context("worker root active session id is required")?;
        let supervisor_socket_path = std::env::var_os(WORKER_SUPERVISOR_SOCKET_ENV)
            .map(PathBuf::from)
            .unwrap_or_default();
        let recovery_journal_path = std::env::var_os(WORKER_RECOVERY_JOURNAL_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                paths::agent_dir()
                    .join("daemon-workers")
                    .join(format!("{}.recovery.jsonl", active_session_id))
            });
        let script = std::env::var_os(WORKER_SCRIPT_ENV)
            .map(PathBuf::from)
            .and_then(|path| {
                let content = std::fs::read_to_string(path).ok()?;
                serde_json::from_str::<Value>(&content).ok()
            });
        Ok(WorkerConfig {
            socket_path,
            supervisor_socket_path,
            token,
            worker_instance_id: std::env::var(WORKER_INSTANCE_ID_ENV).unwrap_or_default(),
            active_session_id,
            agent_dir: paths::agent_dir(),
            recovery_journal_path,
            script,
        })
    }
}

/// Queue delivery lanes (port of the session action store's two deliveries).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Steering,
    FollowUp,
}

impl Lane {
    fn as_str(&self) -> &'static str {
        match self {
            Lane::Steering => "steering",
            Lane::FollowUp => "follow_up",
        }
    }
}

#[derive(Debug)]
struct QueuedItem {
    message: String,
    done: Option<oneshot::Sender<Result<(), String>>>,
}

/// The live session: store, queue, sequencing. Shared by the connection tasks
/// and the turn runner; every access is through the core mutex.
struct SessionCore {
    active_session_id: String,
    generation: String,
    last_event_sequence: u64,
    store: Option<SessionFile>,
    cwd: String,
    steering: VecDeque<QueuedItem>,
    follow_up: VecDeque<QueuedItem>,
    busy: bool,
    created: bool,
    attached_client_ids: Vec<String>,
    abort_requested: bool,
    shutdown_requested: bool,
}

/// One outbound session-event frame: the fully sequenced JSON payload.
struct OutboundFrame {
    payload: Vec<u8>,
}

pub struct Worker {
    config: WorkerConfig,
    core: Arc<Mutex<SessionCore>>,
    engine: std::sync::Arc<dyn SessionEngine>,
    work_notify: Arc<Notify>,
    idle_notify: Arc<Notify>,
    events: broadcast::Sender<Arc<OutboundFrame>>,
    recovery: Mutex<Option<WorkerRecoveryJournal>>,
}

impl Worker {
    pub fn new(config: WorkerConfig) -> Self {
        let (events, _) = broadcast::channel(4096);
        let core = SessionCore {
            active_session_id: config.active_session_id.clone(),
            generation: crate::util::new_display_id(),
            last_event_sequence: 0,
            store: None,
            cwd: String::new(),
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            busy: false,
            created: false,
            attached_client_ids: Vec::new(),
            abort_requested: false,
            shutdown_requested: false,
        };
        let active_session_id = config.active_session_id.clone();
        let script = config.script.clone();
        let core = Arc::new(Mutex::new(core));
        let work_notify = Arc::new(Notify::new());
        let idle_notify = Arc::new(Notify::new());
        // The turn runner runs for the whole process lifetime. The command
        // dispatcher keeps the engine handle too (model metadata for the
        // stats commands).
        let engine: std::sync::Arc<dyn SessionEngine> = {
            // Scripted sessions serve the integration harness; sessions
            // without a script run the real agent engine.
            let engine: std::sync::Arc<dyn SessionEngine> = match &script {
                Some(script) => std::sync::Arc::new(
                    ScriptedEngine::from_value(script.clone()).unwrap_or_default(),
                ),
                None => {
                    let cwd =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                    match AgentSessionEngine::new(AgentEngineConfig {
                        cwd,
                        agent_dir: config.agent_dir.clone(),
                        provider: std::env::var("PRIME_AGENT_MODEL_PROVIDER").ok(),
                        model: std::env::var("PRIME_AGENT_MODEL").ok(),
                        api_key: None,
                        session_dir: None,
                        faux_script: None,
                    }) {
                        Ok(engine) => std::sync::Arc::new(engine),
                        // Runtime construction failed: degrade to the echo engine.
                        Err(_) => std::sync::Arc::new(ScriptedEngine::default()),
                    }
                }
            };
            let runner = TurnRunner {
                core: Arc::clone(&core),
                work_notify: Arc::clone(&work_notify),
                idle_notify: Arc::clone(&idle_notify),
                events: events.clone(),
                engine: std::sync::Arc::clone(&engine),
                active_session_id,
            };
            tokio::spawn(async move {
                runner.run().await;
            });
            engine
        };
        Worker {
            config,
            core,
            engine,
            work_notify,
            idle_notify,
            events,
            recovery: Mutex::new(None),
        }
    }

    /// Serve worker connections until the process is asked to shut down.
    pub async fn serve(self: Arc<Self>) -> Result<()> {
        *self.recovery.lock().unwrap() = Some(WorkerRecoveryJournal::open(
            &self.config.recovery_journal_path,
        )?);
        crate::socket::prepare_socket_path(&self.config.socket_path).await?;
        let listener = UnixListener::bind(&self.config.socket_path)
            .with_context(|| format!("bind worker socket {}", self.config.socket_path.display()))?;
        crate::socket::restrict_socket_path(&self.config.socket_path);
        loop {
            let (stream, _addr) = match listener.accept().await {
                Ok(accepted) => {
                    if std::env::var("PA_DAEMON_DEBUG").is_ok() {
                        eprintln!("[worker {}] accepted connection", std::process::id());
                    }
                    accepted
                }
                Err(error) => return Err(anyhow!("worker accept: {error}")),
            };
            let worker = Arc::clone(&self);
            tokio::spawn(async move {
                if let Err(error) = worker.handle_connection(stream).await {
                    eprintln!("pa-daemon worker connection error: {error:#}");
                }
            });
        }
    }

    async fn handle_connection(self: Arc<Self>, stream: UnixStream) -> Result<()> {
        let (reader, writer) = stream.into_split();
        let writer = Arc::new(tokio::sync::Mutex::new(writer));
        // daemon_hello goes out immediately on every connection.
        let hello = DaemonOutbound::DaemonHello {
            socket_path: self.config.socket_path.to_string_lossy().to_string(),
            protocol: current_protocol_info(),
            schema_id: Some(DAEMON_SCHEMA_ID.to_string()),
            schema_revision: Some(DAEMON_SCHEMA_REVISION),
            app_version: Some(DAEMON_APP_VERSION.to_string()),
            runtime: None,
            supervisor_generation: None,
            supervisor_pid: Some(std::process::id() as u64),
            supervisor_owner_token: None,
            supervisor_process_start_id: None,
            supervisor_socket_path: None,
            client_id: crate::util::new_display_id(),
            server_capabilities: worker_server_capabilities(),
            rest: Default::default(),
        };
        let hello_bytes = serde_json::to_vec(&hello)?;
        // A supervisor liveness probe may connect and drop immediately; that
        // is not an error worth reporting (the peer simply went away first).
        if let Err(error) = self
            .write_frame(
                &writer,
                &json!({ "kind": "outbound", "outboundType": "daemon_hello" }),
                &hello_bytes,
            )
            .await
        {
            if std::env::var("PA_DAEMON_DEBUG").is_ok() {
                eprintln!(
                    "[worker {}] hello write failed: {error:#}",
                    std::process::id()
                );
            }
            return Ok(());
        }

        // Event fan-out: this connection's subscription to the shared pump.
        let mut events = self.events.subscribe();
        {
            let worker = Arc::clone(&self);
            let writer = Arc::clone(&writer);
            tokio::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(frame) => {
                            let active_session_id = active_session_id_of(&frame.payload);
                            let header = json!({
                                "kind": "outbound",
                                "outboundType": "session_event",
                                "activeSessionId": active_session_id,
                            });
                            if worker
                                .write_frame(&writer, &header, &frame.payload)
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        let mut reader =
            crate::framing::PrivateFrameReader::new(reader, DEFAULT_PRIVATE_FRAME_LIMITS);
        let mut authenticated = false;
        loop {
            let frame: Option<crate::framing::PrivateFrame> = reader.read_frame().await?;
            let Some(frame) = frame else {
                break;
            };
            let command_type = frame
                .header
                .get("commandType")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let request_id = frame
                .header
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let payload: Value = serde_json::from_slice(&frame.payload)
                .with_context(|| format!("invalid worker command JSON for {command_type}"))?;
            if std::env::var("PA_DAEMON_DEBUG").is_ok() {
                eprintln!("[worker {}] got command {command_type}", std::process::id());
            }

            if !authenticated {
                if command_type != "worker_auth" {
                    let failure = response_failure(
                        Some(&request_id),
                        "worker_auth",
                        "Worker authentication failed",
                        None,
                    );
                    let _ = self
                        .write_frame(
                            &writer,
                            &json!({ "kind": "outbound", "requestId": request_id, "outboundType": "response" }),
                            &serde_json::to_vec(&failure).unwrap_or_default(),
                        )
                        .await;
                    break;
                }
                match self.authenticate(&payload) {
                    Ok(()) => {
                        if std::env::var("PA_DAEMON_DEBUG").is_ok() {
                            eprintln!("[worker {}] auth ok", std::process::id());
                        }
                        authenticated = true;
                        // The roster capability is always granted; the peer
                        // transport capability rides on the worker instance
                        // id, like the TS worker.
                        let mut capabilities = vec!["agent_roster".to_string()];
                        if !self.config.worker_instance_id.is_empty() {
                            capabilities.push("direct_peer_transport".to_string());
                        }
                        let success = response_success(
                            Some(&request_id),
                            "worker_auth",
                            Some(json!({ "capabilities": capabilities })),
                        );
                        self.write_response_frame(&writer, &request_id, &success)
                            .await;
                    }
                    Err(error) => {
                        let failure = response_failure(
                            Some(&request_id),
                            "worker_auth",
                            &error.to_string(),
                            None,
                        );
                        self.write_response_frame(&writer, &request_id, &failure)
                            .await;
                        break;
                    }
                }
                continue;
            }

            let response = self.dispatch(&command_type, &payload).await;
            self.write_response_frame(&writer, &request_id, &response)
                .await;
            if command_type == "shutdown" && response.success {
                // Shutdown keeps the resume entry and exits the process, like
                // the TS close path (`closeKeepsResumeEntry("shutdown")`).
                let _ = self.record_recovery(false, "shutdown");
                std::process::exit(0);
            }
        }
        Ok(())
    }

    fn authenticate(&self, payload: &Value) -> Result<()> {
        let token = payload
            .get("token")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // TS `worker_auth` validation: token, generation, pid, socket path are
        // mandatory; instance and process-start ids only checked when present.
        if token.is_empty() || token != self.config.token {
            return Err(anyhow!("Worker authentication failed"));
        }
        if payload
            .get("supervisorGeneration")
            .and_then(Value::as_str)
            .is_none()
        {
            return Err(anyhow!("Worker authentication failed"));
        }
        let pid = payload
            .get("supervisorPid")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if pid == 0 {
            return Err(anyhow!("Worker authentication failed"));
        }
        if payload
            .get("supervisorSocketPath")
            .and_then(Value::as_str)
            .is_none()
        {
            return Err(anyhow!("Worker authentication failed"));
        }
        if let Some(instance) = payload.get("workerInstanceId") {
            if !instance.is_null()
                && instance.as_str() != Some("")
                && instance.as_str().map(str::to_string)
                    != Some(self.config.worker_instance_id.clone())
            {
                return Err(anyhow!("Worker authentication failed"));
            }
        }
        Ok(())
    }

    async fn write_frame(
        &self,
        writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
        header: &Value,
        payload: &[u8],
    ) -> Result<()> {
        let mut guard = writer.lock().await;
        write_frame(&mut *guard, header, payload, DEFAULT_PRIVATE_FRAME_LIMITS)
            .await
            .context("write private frame")
    }

    async fn write_response_frame(
        &self,
        writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
        request_id: &str,
        response: &DaemonResponse,
    ) {
        let payload =
            serde_json::to_vec(&crate::protocol::response_line(response)).unwrap_or_default();
        let header = json!({
            "kind": "outbound",
            "requestId": request_id,
            "outboundType": "response",
        });
        if let Err(error) = self.write_frame(writer, &header, &payload).await {
            eprintln!("pa-daemon worker response write failed: {error:#}");
        }
    }

    async fn dispatch(&self, command_type: &str, payload: &Value) -> DaemonResponse {
        match command_type {
            "create" => self.handle_create(payload),
            "attach" => self.handle_attach(payload),
            "detach" => self.handle_detach(payload),
            "prompt" => self.handle_prompt(payload, false).await,
            "prompt_and_wait" => self.handle_prompt(payload, true).await,
            "steer" => self.handle_queue(payload, Lane::Steering),
            "follow_up" => self.handle_queue(payload, Lane::FollowUp),
            "abort" => self.handle_abort(),
            "wait_for_idle" => self.handle_wait_for_idle().await,
            "get_state" => self.handle_get_state(),
            "get_messages" => self.handle_get_messages(),
            "get_session_header" => self.handle_get_session_header(),
            "get_session_stats" => self.handle_get_session_stats(),
            "get_queue" => self.handle_get_queue(),
            "clear_queue" => self.handle_clear_queue(),
            "abort_and_clear_queue" => self.handle_abort_and_clear_queue(),
            "get_last_assistant_text" => self.handle_get_last_assistant_text(),
            "kill" => self.handle_kill(),
            "shutdown" => self.handle_shutdown(),
            "rename" => self.handle_rename("rename", payload),
            "set_session_name" => self.handle_rename("set_session_name", payload),
            other => response_failure(
                None,
                command_type,
                &format!("Unknown worker command: {other}"),
                None,
            ),
        }
    }

    #[allow(clippy::result_large_err)]
    fn require_created(&self, command_type: &str) -> Result<(), DaemonResponse> {
        let core = self.core.lock().unwrap();
        if !core.created {
            return Err(response_failure(
                None,
                command_type,
                "Session is still initializing",
                None,
            ));
        }
        Ok(())
    }

    fn handle_create(&self, payload: &Value) -> DaemonResponse {
        {
            let core = self.core.lock().unwrap();
            if core.created {
                // Idempotent re-create after a supervisor restart or respawn.
                let summary = self.summary_locked(&core);
                return response_success(
                    None,
                    "create",
                    Some(serde_json::to_value(&summary).unwrap_or(Value::Null)),
                );
            }
        }
        let session_path = payload
            .get("sessionPath")
            .and_then(Value::as_str)
            .map(paths::expand_tilde);
        let no_session = payload
            .get("noSession")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let name = payload.get("name").and_then(Value::as_str);
        let cwd = payload
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or("/")
            .to_string();
        let session_dir = payload
            .get("sessionDir")
            .and_then(Value::as_str)
            .map(paths::expand_tilde)
            .unwrap_or_else(|| paths::sessions_dir(&self.config.agent_dir));

        let mut store = match (&session_path, no_session) {
            (Some(path), false) if path.exists() => match SessionFile::open(path) {
                Ok(mut opened) => {
                    let _ = opened.append_session_state("active");
                    if let Err(error) = opened.rewrite() {
                        return response_failure(None, "create", &error.to_string(), None);
                    }
                    opened
                }
                Err(error) => return response_failure(None, "create", &error.to_string(), None),
            },
            (Some(path), false) => {
                let mut created = SessionFile::create(&cwd, None, 0);
                created.set_path(path.clone());
                if let Err(error) = created.rewrite() {
                    return response_failure(None, "create", &error.to_string(), None);
                }
                let _ = created.append_session_state("active");
                if let Err(error) = created.rewrite() {
                    return response_failure(None, "create", &error.to_string(), None);
                }
                created
            }
            // In-memory session: no file, like the TS `noSession` create.
            (None, true) => SessionFile::create(&cwd, None, 0),
            (None, false) => {
                let mut created = SessionFile::create(&cwd, None, 0);
                let path = session_dir.join(session_file_name(created.session_id()));
                created.set_path(path);
                if let Err(error) = created.rewrite() {
                    return response_failure(None, "create", &error.to_string(), None);
                }
                let _ = created.append_session_state("active");
                if let Err(error) = created.rewrite() {
                    return response_failure(None, "create", &error.to_string(), None);
                }
                created
            }
            (Some(_), true) => {
                return response_failure(
                    None,
                    "create",
                    "Session cannot be both no-session and session-pathed",
                    None,
                )
            }
        };

        if let Some(name) = name.filter(|n| !n.trim().is_empty()) {
            let _ = store.append_session_info(name);
            let _ = store.rewrite();
        }
        // Restore the persisted queue snapshot (crash/respawn recovery).
        let (steering, follow_up) = restore_queue_snapshot(&store);
        let mut core = self.core.lock().unwrap();
        core.cwd = cwd;
        core.steering = steering;
        core.follow_up = follow_up;
        core.store = Some(store);
        core.created = true;
        core.abort_requested = false;
        let summary = self.summary_locked(&core);
        drop(core);
        // Recovery journal writes must not happen while holding the core
        // lock: record_recovery locks the core to read the store.
        let _ = self.record_recovery(true, "create");
        self.work_notify.notify_one();
        response_success(
            None,
            "create",
            Some(serde_json::to_value(&summary).unwrap_or(Value::Null)),
        )
    }

    fn summary_locked(&self, core: &SessionCore) -> SessionSummary {
        let store = core.store.as_ref();
        let streaming = core.busy;
        let queued = core.steering.len() + core.follow_up.len();
        // `modified` is the session file mtime; `lastActivityAt` prefers the
        // newest message timestamp (port of `summaryForActiveSession`).
        let modified = store
            .and_then(|store| std::fs::metadata(&store.path).ok())
            .and_then(|metadata| metadata.modified().ok())
            .map(|time| {
                crate::util::iso_from_unix_ms(
                    time.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or_default(),
                )
            });
        let messages = store.map(|store| store.messages()).unwrap_or_default();
        let last_activity_at = messages
            .iter()
            .rev()
            .find_map(crate::types::message_timestamp_ms)
            .map(crate::util::iso_from_unix_ms)
            .or_else(|| modified.clone())
            .or_else(|| store.map(|store| store.header.timestamp.clone()));
        // Usage: summed assistant usage (`sessionUsageSummaryFrom`), absent
        // when everything is zero.
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut cost = 0.0f64;
        for message in &messages {
            if crate::types::message_role(message) != Some("assistant") {
                continue;
            }
            let Some(usage) = message.get("usage") else {
                continue;
            };
            input_tokens += usage
                .get("input")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            input_tokens += usage
                .get("cacheRead")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            input_tokens += usage
                .get("cacheWrite")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            output_tokens += usage
                .get("output")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            cost += usage
                .get("cost")
                .and_then(|cost| cost.get("total"))
                .and_then(Value::as_f64)
                .unwrap_or_default();
        }
        let usage = (input_tokens > 0 || output_tokens > 0 || cost > 0.0).then(
            || json!({ "inputTokens": input_tokens, "outputTokens": output_tokens, "cost": cost }),
        );
        SessionSummary {
            id: core.active_session_id.clone(),
            lifecycle: "resident".to_string(),
            activity: if streaming { "working" } else { "idle" }.to_string(),
            is_session_active: streaming || queued > 0,
            has_registered_cron_job: Some(false),
            last_activity_at,
            rlm_depth: Some(0),
            active_session_id: Some(core.active_session_id.clone()),
            session_id: store
                .map(|s| s.session_id().to_string())
                .unwrap_or_default(),
            session_file: store.map(|s| s.path.to_string_lossy().to_string()),
            session_name: store.and_then(|s| s.session_name().map(str::to_string)),
            cwd: core.cwd.clone(),
            thinking_level: Some("default".to_string()),
            is_streaming: streaming,
            is_compacting: false,
            is_bash_running: Some(false),
            attached_clients: core.attached_client_ids.len() as u32,
            message_count: store.map(|s| s.message_count()).unwrap_or(0) as u32,
            session_actions: self.snapshot_locked(core),
            streaming_message: None,
            created: store.map(|s| s.header.timestamp.clone()),
            modified,
            first_message: store.and_then(|s| s.first_message()),
            parent_session_path: None,
            usage,
            worker_state: Some("ready".to_string()),
            worker_pid: Some(std::process::id()),
            status_label: None,
            summary: None,
            task_state: None,
            model: None,
            runtime_kind: Some("top-level".to_string()),
            unfinished_action_count: Some(0),
        }
    }

    fn snapshot_locked(&self, core: &SessionCore) -> SessionActionSnapshot {
        SessionActionSnapshot {
            queued_count: (core.steering.len() + core.follow_up.len()) as u32,
            steering: core
                .steering
                .iter()
                .map(|item| item.message.clone())
                .collect(),
            follow_ups: core
                .follow_up
                .iter()
                .map(|item| item.message.clone())
                .collect(),
            active: if core.busy {
                Some(crate::types::SessionActionActive {
                    kind: "turn".to_string(),
                    phase: "running".to_string(),
                    label: None,
                })
            } else {
                None
            },
        }
    }

    fn handle_attach(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("attach") {
            return response;
        }
        let client_id = payload
            .get("clientId")
            .and_then(Value::as_str)
            .unwrap_or("anonymous")
            .to_string();
        let capabilities = payload
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|array| {
                normalize_client_capabilities(
                    &array
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>(),
                )
            })
            .unwrap_or_else(default_client_capabilities);
        let resume_cursor = payload
            .get("resumeCursor")
            .cloned()
            .filter(|value| !value.is_null())
            .and_then(|value| serde_json::from_value::<DaemonResumeCursor>(value).ok());

        let mut core = self.core.lock().unwrap();
        if !core.attached_client_ids.iter().any(|id| id == &client_id) {
            core.attached_client_ids.push(client_id.clone());
        }
        let summary = self.summary_locked(&core);
        let messages: Vec<Value> = core
            .store
            .as_ref()
            .map(|s| s.messages())
            .unwrap_or_default();
        let state = self.connection_state_locked(&core);
        let last_event_sequence = core.last_event_sequence;
        let generation = core.generation.clone();
        let active_session_id = core.active_session_id.clone();
        drop(core);
        let replay =
            create_daemon_replay_info(resume_cursor.as_ref(), last_event_sequence, &generation);
        let cursor = json!({ "generation": generation, "sequence": last_event_sequence });
        let summary_value = serde_json::to_value(&summary).unwrap_or(Value::Null);
        let state_value = serde_json::to_value(&state).unwrap_or(Value::Null);
        let snapshot = json!({
            "activeSessionId": active_session_id,
            "summary": summary_value,
            "state": state_value,
            "messages": messages,
            "lastEventSequence": last_event_sequence,
            "lastEventCursor": cursor,
            // RLM child roster; empty for top-level daemon sessions.
            "children": [],
        });
        // Slim clients read summary/messages from the snapshot; duplicating
        // them at the top level would serialize the history twice per attach
        // (port of `createAttachResult`).
        let slim = capabilities.iter().any(|cap| cap == "slim_attach");
        let mut result = json!({
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "activeSessionId": active_session_id,
            "snapshot": snapshot,
            "replay": replay,
            "lastEventSequence": last_event_sequence,
            "lastEventCursor": cursor,
            "client": { "id": client_id, "capabilities": capabilities },
        });
        if !slim {
            result["state"] = summary_value;
            result["messages"] = Value::Array(messages);
        }

        response_success(None, "attach", Some(result))
    }

    fn handle_detach(&self, payload: &Value) -> DaemonResponse {
        let client_id = payload
            .get("clientId")
            .and_then(Value::as_str)
            .unwrap_or("anonymous")
            .to_string();
        let mut core = self.core.lock().unwrap();
        core.attached_client_ids.retain(|id| id != &client_id);
        response_success(None, "detach", None)
    }

    async fn handle_prompt(&self, payload: &Value, wait: bool) -> DaemonResponse {
        if let Err(response) = self.require_created("prompt") {
            return response;
        }
        let message = payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if message.is_empty() {
            return response_failure(None, "prompt", "Prompt cannot be empty", None);
        }
        let streaming_behavior = payload.get("streamingBehavior").and_then(Value::as_str);
        let (done_tx, done_rx) = oneshot::channel();
        let done = if wait { Some(done_tx) } else { None };
        let snapshot = {
            let mut core = self.core.lock().unwrap();
            match streaming_behavior {
                Some("steer") => core.steering.push_back(QueuedItem {
                    message: message.to_string(),
                    done,
                }),
                // Plain prompts admitted while busy drain when the run goes
                // idle, like `queueIfBusy` prompt admission.
                _ => core.follow_up.push_back(QueuedItem {
                    message: message.to_string(),
                    done,
                }),
            }
            let snapshot = self.snapshot_locked(&core);
            self.persist_queue_snapshot_locked(&mut core);
            snapshot
        };
        let _ = self.emit_action_update(&snapshot);
        self.work_notify.notify_one();
        if !wait {
            return response_success(None, "prompt", None);
        }
        match done_rx.await {
            Ok(Ok(())) => response_success(None, "prompt_and_wait", None),
            Ok(Err(error)) => response_failure(None, "prompt_and_wait", &error, None),
            Err(_) => response_failure(None, "prompt_and_wait", "Prompt did not complete", None),
        }
    }

    fn handle_queue(&self, payload: &Value, lane: Lane) -> DaemonResponse {
        if let Err(response) = self.require_created(lane.as_str()) {
            return response;
        }
        let message = payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut core = self.core.lock().unwrap();
        match lane {
            Lane::Steering => &mut core.steering,
            Lane::FollowUp => &mut core.follow_up,
        }
        .push_back(QueuedItem {
            message: message.to_string(),
            done: None,
        });
        let snapshot = self.snapshot_locked(&core);
        self.persist_queue_snapshot_locked(&mut core);
        drop(core);
        let _ = self.emit_action_update(&snapshot);
        self.work_notify.notify_one();
        let command = if lane == Lane::Steering {
            "steer"
        } else {
            "follow_up"
        };
        response_success(None, command, Some(json!({ "queued": true })))
    }

    /// Graceful stop: the connection loop exits the process after replying.
    fn handle_shutdown(&self) -> DaemonResponse {
        {
            let mut core = self.core.lock().unwrap();
            core.shutdown_requested = true;
            core.abort_requested = true;
        }
        self.work_notify.notify_one();
        response_success(None, "shutdown", None)
    }

    fn handle_abort(&self) -> DaemonResponse {
        let mut core = self.core.lock().unwrap();
        core.abort_requested = true;
        response_success(None, "abort", None)
    }

    async fn handle_wait_for_idle(&self) -> DaemonResponse {
        loop {
            {
                let core = self.core.lock().unwrap();
                if !core.busy && core.steering.is_empty() && core.follow_up.is_empty() {
                    return response_success(None, "wait_for_idle", None);
                }
            }
            self.idle_notify.notified().await;
        }
    }

    fn handle_get_state(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("get_state") {
            return response;
        }
        let core = self.core.lock().unwrap();
        let summary = self.summary_locked(&core);
        response_success(
            None,
            "get_state",
            Some(serde_json::to_value(&summary).unwrap_or(Value::Null)),
        )
    }

    /// `get_session_header`: the persisted session header line (TS wraps it
    /// in `{ header: ... }`).
    fn handle_get_session_header(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("get_session_header") {
            return response;
        }
        let core = self.core.lock().unwrap();
        let Some(store) = core.store.as_ref() else {
            return response_failure(
                None,
                "get_session_header",
                "Session is still initializing",
                None,
            );
        };
        response_success(
            None,
            "get_session_header",
            Some(json!({ "header": crate::session_store::session_header_line(&store.header) })),
        )
    }

    /// `get_session_stats`: counts, token totals, and the context-usage
    /// estimate over the persisted branch (TS `getSessionStats`).
    fn handle_get_session_stats(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("get_session_stats") {
            return response;
        }
        let core = self.core.lock().unwrap();
        let Some(store) = core.store.as_ref() else {
            return response_failure(
                None,
                "get_session_stats",
                "Session is still initializing",
                None,
            );
        };
        let stats = crate::session_stats::session_stats(store, self.engine.model_context_window());
        response_success(None, "get_session_stats", Some(stats))
    }

    fn handle_get_messages(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("get_messages") {
            return response;
        }
        let core = self.core.lock().unwrap();
        let messages: Vec<Value> = core
            .store
            .as_ref()
            .map(|s| s.messages())
            .unwrap_or_default();
        response_success(None, "get_messages", Some(json!({ "messages": messages })))
    }

    fn handle_get_queue(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("get_queue") {
            return response;
        }
        let core = self.core.lock().unwrap();
        response_success(
            None,
            "get_queue",
            Some(json!({
                "steering": core.steering.iter().map(|item| item.message.clone()).collect::<Vec<_>>(),
                "followUp": core.follow_up.iter().map(|item| item.message.clone()).collect::<Vec<_>>(),
            })),
        )
    }

    fn handle_clear_queue(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("clear_queue") {
            return response;
        }
        let mut core = self.core.lock().unwrap();
        let steering: Vec<String> = core.steering.drain(..).map(|item| item.message).collect();
        let follow_up: Vec<String> = core.follow_up.drain(..).map(|item| item.message).collect();
        let snapshot = self.snapshot_locked(&core);
        self.persist_queue_snapshot_locked(&mut core);
        drop(core);
        let _ = self.emit_action_update(&snapshot);
        response_success(
            None,
            "clear_queue",
            Some(json!({ "steering": steering, "followUp": follow_up })),
        )
    }

    fn handle_abort_and_clear_queue(&self) -> DaemonResponse {
        let cleared = self.handle_clear_queue();
        if !cleared.success {
            return cleared;
        }
        let mut core = self.core.lock().unwrap();
        core.abort_requested = true;
        drop(core);
        response_success(None, "abort_and_clear_queue", cleared.data)
    }

    fn handle_get_last_assistant_text(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("get_last_assistant_text") {
            return response;
        }
        let core = self.core.lock().unwrap();
        let text = core.store.as_ref().and_then(|store| {
            store
                .messages()
                .into_iter()
                .rev()
                .find(|message| crate::types::message_role(message) == Some("assistant"))
                .map(|message| crate::types::message_text(&message))
        });
        response_success(
            None,
            "get_last_assistant_text",
            Some(json!({ "text": text })),
        )
    }

    fn handle_kill(&self) -> DaemonResponse {
        let mut core = self.core.lock().unwrap();
        if let Some(store) = core.store.as_mut() {
            let _ = store.append_session_state("archived");
            let _ = store.rewrite();
        }
        core.created = false;
        let active_session_id = core.active_session_id.clone();
        drop(core);
        let _ = self.emit_session_closed(&active_session_id, DaemonSessionClosedReason::Killed);
        let _ = self.record_recovery(false, "killed");
        response_success(None, "kill", None)
    }

    fn handle_rename(&self, command: &str, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created(command) {
            return response;
        }
        let name = payload.get("name").and_then(Value::as_str).unwrap_or("");
        if name.trim().is_empty() {
            return response_failure(None, command, "Session name cannot be empty", None);
        }
        let mut core = self.core.lock().unwrap();
        if let Some(store) = core.store.as_mut() {
            let _ = store.append_session_info(name);
            let _ = store.rewrite();
        }
        let summary = self.summary_locked(&core);
        response_success(
            None,
            command,
            Some(serde_json::to_value(&summary).unwrap_or(Value::Null)),
        )
    }

    fn connection_state_locked(&self, core: &SessionCore) -> AgentConnectionState {
        let store = core.store.as_ref();
        AgentConnectionState {
            active_session_id: Some(core.active_session_id.clone()),
            cwd: core.cwd.clone(),
            model: None,
            thinking_level: "default".to_string(),
            service_tier: "auto".to_string(),
            available_thinking_levels: vec!["default".to_string()],
            is_streaming: core.busy,
            is_compacting: false,
            is_bash_running: false,
            retry_attempt: 0,
            steering_mode: "all".to_string(),
            follow_up_mode: "all".to_string(),
            session_file: store.map(|s| s.path.to_string_lossy().to_string()),
            session_id: store
                .map(|s| s.session_id().to_string())
                .unwrap_or_default(),
            session_name: store.and_then(|s| s.session_name().map(str::to_string)),
            session_dir: store
                .and_then(|s| s.path.parent())
                .map(|p| p.to_string_lossy().to_string()),
            leaf_id: store.and_then(|s| s.leaf_id().map(str::to_string)),
            auto_compaction_enabled: false,
            message_count: store.map(|s| s.message_count()).unwrap_or(0) as u32,
            session_actions: self.snapshot_locked(core),
            compaction_count: 0,
            goal: Value::Null,
            scoped_models: Vec::new(),
            active_tool_names: Vec::new(),
            context_usage: None,
            recap: None,
        }
    }

    /// Append the queue snapshot to the store (crash-safe queue persistence).
    fn persist_queue_snapshot_locked(&self, core: &mut SessionCore) {
        let steering: Vec<String> = core
            .steering
            .iter()
            .map(|item| item.message.clone())
            .collect();
        let follow_up: Vec<String> = core
            .follow_up
            .iter()
            .map(|item| item.message.clone())
            .collect();
        if let Some(store) = core.store.as_mut() {
            let _ = store.persist_entry(
                "custom",
                json!({
                    "customType": QUEUE_SNAPSHOT_CUSTOM_TYPE,
                    "steering": steering,
                    "followUp": follow_up,
                }),
            );
        }
    }

    fn record_recovery(&self, busy: bool, operation: &str) -> Result<()> {
        let mut guard = self.recovery.lock().unwrap();
        let Some(journal) = guard.as_mut() else {
            return Ok(());
        };
        let core = self.core.lock().unwrap();
        let store = core.store.as_ref();
        journal.record(
            &core.active_session_id,
            store.map(|s| s.session_id()).unwrap_or(""),
            store
                .map(|s| s.path.to_string_lossy().to_string())
                .as_deref(),
            busy,
            operation,
        )
    }

    /// Sequence and broadcast one session_event for the queue projection.
    fn emit_action_update(&self, snapshot: &SessionActionSnapshot) -> Result<()> {
        let mut core = self.core.lock().unwrap();
        let sequence = core.last_event_sequence + 1;
        core.last_event_sequence = sequence;
        let meta = create_daemon_event_meta(
            &core.active_session_id,
            sequence,
            None,
            Some(&core.generation),
        );
        let outbound = DaemonOutbound::SessionEvent {
            active_session_id: core.active_session_id.clone(),
            event: json!({ "type": "session_action_update", "actions": snapshot }),
            meta: Some(meta),
            rest: Default::default(),
        };
        let payload = serde_json::to_vec(&outbound)?;
        drop(core);
        let _ = self.events.send(Arc::new(OutboundFrame { payload }));
        Ok(())
    }

    fn emit_session_closed(
        &self,
        active_session_id: &str,
        reason: DaemonSessionClosedReason,
    ) -> Result<()> {
        let mut core = self.core.lock().unwrap();
        let sequence = core.last_event_sequence + 1;
        core.last_event_sequence = sequence;
        let meta = create_daemon_event_meta(
            &core.active_session_id,
            sequence,
            None,
            Some(&core.generation),
        );
        let outbound = DaemonOutbound::SessionClosed {
            active_session_id: active_session_id.to_string(),
            reason,
            meta: Some(meta),
            rest: Default::default(),
        };
        let payload = serde_json::to_vec(&outbound)?;
        drop(core);
        let _ = self.events.send(Arc::new(OutboundFrame { payload }));
        Ok(())
    }
}

fn active_session_id_of(payload: &[u8]) -> String {
    serde_json::from_slice::<Value>(payload)
        .ok()
        .and_then(|value| {
            value
                .get("activeSessionId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn worker_server_capabilities() -> Vec<String> {
    default_server_capabilities()
}

/// Queue snapshot restore from the latest persisted entry.
fn restore_queue_snapshot(store: &SessionFile) -> (VecDeque<QueuedItem>, VecDeque<QueuedItem>) {
    let mut steering = VecDeque::new();
    let mut follow_up = VecDeque::new();
    let snapshot = store.entries().iter().rev().find(|entry| {
        entry.type_ == "custom"
            && entry.fields.get("customType").and_then(Value::as_str)
                == Some(QUEUE_SNAPSHOT_CUSTOM_TYPE)
    });
    if let Some(entry) = snapshot {
        let fields = &entry.fields;
        if let Some(list) = fields.get("steering").and_then(Value::as_array) {
            for message in list.iter().filter_map(Value::as_str) {
                steering.push_back(QueuedItem {
                    message: message.to_string(),
                    done: None,
                });
            }
        }
        if let Some(list) = fields.get("followUp").and_then(Value::as_array) {
            for message in list.iter().filter_map(Value::as_str) {
                follow_up.push_back(QueuedItem {
                    message: message.to_string(),
                    done: None,
                });
            }
        }
    }
    (steering, follow_up)
}

/// The turn runner: drains the queue one turn at a time, running the session
/// engine and emitting the agent-loop event lifecycle.
struct TurnRunner {
    core: Arc<Mutex<SessionCore>>,
    work_notify: Arc<Notify>,
    idle_notify: Arc<Notify>,
    events: broadcast::Sender<Arc<OutboundFrame>>,
    engine: std::sync::Arc<dyn SessionEngine>,
    active_session_id: String,
}

impl TurnRunner {
    async fn run(self) {
        loop {
            let engine = self.engine.clone();
            let item: Option<QueuedItem> = {
                let mut core = self.core.lock().unwrap();
                if core.shutdown_requested {
                    return;
                }
                if let Some(item) = core.steering.pop_front() {
                    core.busy = true;
                    core.abort_requested = false;
                    Some(item)
                } else if let Some(item) = core.follow_up.pop_front() {
                    core.busy = true;
                    core.abort_requested = false;
                    Some(item)
                } else {
                    core.busy = false;
                    None
                }
            };
            if let Some(item) = item {
                self.run_turn(engine, item).await;
            } else {
                self.idle_notify.notify_waiters();
                self.work_notify.notified().await;
            }
        }
    }

    async fn run_turn(&self, engine: std::sync::Arc<dyn SessionEngine>, item: QueuedItem) {
        self.emit_turn_event(json!({ "type": "agent_start" }));
        self.emit_turn_event(json!({ "type": "turn_start" }));

        let prompt_index = {
            let core = self.core.lock().unwrap();
            core.store.as_ref().map(|s| s.message_count()).unwrap_or(0) / 2
        };
        let request = PromptRequest {
            message: item.message.clone(),
            source: "user".to_string(),
            agent_message_id: None,
        };
        let abort_flag = Arc::new(AtomicBool::new(false));
        let engine = engine.clone();
        let core = Arc::clone(&self.core);
        let events = self.events.clone();
        let done = item.done;
        let turn = tokio::task::spawn_blocking(move || {
            let mut done = done;
            let mut emit = |event: EngineEvent| -> bool {
                if abort_flag.load(Ordering::SeqCst) {
                    return false;
                }
                // Sequence + persist under the core lock, then broadcast.
                let mut core = core.lock().unwrap();
                match &event {
                    EngineEvent::UserMessage(message) | EngineEvent::AssistantMessage(message) => {
                        if let Some(store) = core.store.as_mut() {
                            let _ = store.persist_entry("message", json!({ "message": message }));
                        }
                    }
                    _ => {}
                }
                let sequence = core.last_event_sequence + 1;
                core.last_event_sequence = sequence;
                let meta = create_daemon_event_meta(
                    &core.active_session_id,
                    sequence,
                    None,
                    Some(&core.generation),
                );
                let done_result = if let EngineEvent::Done(result) = &event {
                    Some(result.clone())
                } else {
                    None
                };
                let event_json = match event {
                    EngineEvent::UserMessage(message) => {
                        json!({ "type": "message_start", "message": message })
                    }
                    EngineEvent::AssistantUpdate(message) => {
                        json!({ "type": "message_update", "message": message })
                    }
                    EngineEvent::AssistantMessage(message) => {
                        json!({ "type": "message_end", "message": message })
                    }
                    EngineEvent::Done(Ok(())) => json!({ "type": "turn_end" }),
                    EngineEvent::Done(Err(error)) => {
                        json!({ "type": "turn_end", "error": error })
                    }
                };
                // Take the sender only when the event is `Done`: the
                // `if let` scrutinee runs before matching, so a combined
                // pattern would consume `done` on every event.
                if let Some(result) = done_result {
                    if let Some(done) = done.take() {
                        let _ = done.send(result);
                    }
                }
                let outbound = DaemonOutbound::SessionEvent {
                    active_session_id: core.active_session_id.clone(),
                    event: event_json,
                    meta: Some(meta),
                    rest: Default::default(),
                };
                drop(core);
                let payload = serde_json::to_vec(&outbound).unwrap_or_default();
                let _ = events.send(Arc::new(OutboundFrame { payload }));
                true
            };
            engine.run_prompt(prompt_index, request, &mut emit);
        });
        let _ = turn.await;

        {
            let mut core = self.core.lock().unwrap();
            core.busy = false;
        }
        self.emit_turn_event(json!({ "type": "agent_end" }));
        let snapshot = {
            let core = self.core.lock().unwrap();
            self.snapshot_from(&core)
        };
        {
            let mut core = self.core.lock().unwrap();
            let snapshot_entry = json!({
                "customType": QUEUE_SNAPSHOT_CUSTOM_TYPE,
                "steering": core.steering.iter().map(|item| item.message.clone()).collect::<Vec<_>>(),
                "followUp": core.follow_up.iter().map(|item| item.message.clone()).collect::<Vec<_>>(),
            });
            if let Some(store) = core.store.as_mut() {
                let _ = store.persist_entry("custom", snapshot_entry);
            }
            let sequence = core.last_event_sequence + 1;
            core.last_event_sequence = sequence;
            let meta = create_daemon_event_meta(
                &core.active_session_id,
                sequence,
                None,
                Some(&core.generation),
            );
            let outbound = DaemonOutbound::SessionEvent {
                active_session_id: self.active_session_id.clone(),
                event: json!({ "type": "session_action_update", "actions": snapshot }),
                meta: Some(meta),
                rest: Default::default(),
            };
            let payload = serde_json::to_vec(&outbound).unwrap_or_default();
            drop(core);
            let _ = self.events.send(Arc::new(OutboundFrame { payload }));
        }
        self.idle_notify.notify_waiters();
    }

    fn snapshot_from(&self, core: &SessionCore) -> SessionActionSnapshot {
        SessionActionSnapshot {
            queued_count: (core.steering.len() + core.follow_up.len()) as u32,
            steering: core
                .steering
                .iter()
                .map(|item| item.message.clone())
                .collect(),
            follow_ups: core
                .follow_up
                .iter()
                .map(|item| item.message.clone())
                .collect(),
            active: None,
        }
    }

    fn emit_turn_event(&self, event: Value) {
        let mut core = self.core.lock().unwrap();
        let sequence = core.last_event_sequence + 1;
        core.last_event_sequence = sequence;
        let meta = create_daemon_event_meta(
            &core.active_session_id,
            sequence,
            None,
            Some(&core.generation),
        );
        let outbound = DaemonOutbound::SessionEvent {
            active_session_id: self.active_session_id.clone(),
            event,
            meta: Some(meta),
            rest: Default::default(),
        };
        let payload = serde_json::to_vec(&outbound).unwrap_or_default();
        drop(core);
        let _ = self.events.send(Arc::new(OutboundFrame { payload }));
    }
}

/// Entry point for the worker process.
pub async fn run_worker() -> Result<()> {
    if std::env::var(WORKER_ROLE_ENV).unwrap_or_default() != "1" {
        return Err(anyhow!("worker mode requires {WORKER_ROLE_ENV}=1"));
    }
    let config = WorkerConfig::from_env()?;
    let worker = Arc::new(Worker::new(config));
    worker.serve().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_ids_are_twelve_hex() {
        let id = crate::util::new_display_id();
        assert_eq!(id.len(), 12);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn queue_snapshot_round_trips_through_store() {
        let dir = std::env::temp_dir().join(format!("pa-worker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = SessionFile::create("/tmp", None, 0);
        store.set_path(dir.join("s.jsonl"));
        store
            .persist_entry(
                "custom",
                json!({
                    "customType": QUEUE_SNAPSHOT_CUSTOM_TYPE,
                    "steering": ["steer-me"],
                    "followUp": ["follow-me"],
                }),
            )
            .unwrap();
        let reloaded = SessionFile::open(&dir.join("s.jsonl")).unwrap();
        let (steering, follow_up) = restore_queue_snapshot(&reloaded);
        assert_eq!(steering.len(), 1);
        assert_eq!(steering[0].message, "steer-me");
        assert_eq!(follow_up.len(), 1);
        assert_eq!(follow_up[0].message, "follow-me");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
