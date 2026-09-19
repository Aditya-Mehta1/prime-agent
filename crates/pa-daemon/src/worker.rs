//! Session worker runtime: one process, one session.
//!
//! Port of the TS daemon's worker mode (`modes/daemon/daemon-mode.ts` worker
//! branch, `modes/session-worker/*`): the worker owns the session - the
//! append-only store, the queue lanes, event sequencing, and turn execution.
//! Supervisors connect over a private-framed Unix socket and authenticate
//! with the bootstrap token before any command.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use pa_core::session_engine::agent_messaging::{
    AgentFamilyRelationship, AgentMessagePromptPayload, AGENT_MESSAGE_SOURCE,
    DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION,
};
use pa_types::platform::transport::{bind_transport, TransportStream};
use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot, Notify};

use crate::agent_engine::{AgentEngineConfig, AgentSessionEngine, SupervisorLinkConfig};
use crate::engine::{
    EngineEvent, EngineModelSelection, PromptRequest, RlmSessionIdentity, ScriptedEngine,
    SessionEngine,
};
use crate::framing::{write_frame, DEFAULT_PRIVATE_FRAME_LIMITS};
use crate::journal::WorkerRecoveryJournal;
use crate::paths;
use crate::peer::{
    peer_command_allowed, worker_peer_command_allowed, ConnectionRole, PeerGrantStore,
    PEER_COMMAND_NOT_ALLOWED,
};
use crate::protocol::{
    create_daemon_event_meta, create_daemon_replay_info, current_protocol_info,
    default_client_capabilities, default_server_capabilities, normalize_client_capabilities,
    response_failure, response_success, DaemonOutbound, DaemonResponse, DaemonResumeCursor,
    DaemonSessionClosedReason, DAEMON_APP_VERSION, DAEMON_SCHEMA_ID, DAEMON_SCHEMA_REVISION,
};
use crate::registration::RegistrationHandle;
use crate::session_store::{session_file_name, SessionFile};
use crate::types::{AgentConnectionState, SessionActionSnapshot, SessionSummary};

/// TS-parity worker environment variables (`daemon-worker-protocol.ts`).
pub const WORKER_ROLE_ENV: &str = "PRIME_AGENT_INTERNAL_DAEMON_WORKER";
pub const WORKER_TOKEN_ENV: &str = "PRIME_AGENT_INTERNAL_DAEMON_WORKER_TOKEN";
pub const WORKER_INSTANCE_ID_ENV: &str = "PRIME_AGENT_INTERNAL_DAEMON_WORKER_INSTANCE_ID";
pub const WORKER_ACTIVE_SESSION_ID_ENV: &str =
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_ACTIVE_SESSION_ID";
/// Worker process cwd (the create command's `cwd`).
pub const WORKER_CWD_ENV: &str = "PRIME_AGENT_INTERNAL_DAEMON_WORKER_CWD";
pub const WORKER_SUPERVISOR_SOCKET_ENV: &str = "PRIME_AGENT_INTERNAL_DAEMON_SUPERVISOR_SOCKET";
pub const WORKER_RECOVERY_JOURNAL_ENV: &str = "PRIME_AGENT_INTERNAL_DAEMON_WORKER_RECOVERY_JOURNAL";
/// Scripted-engine script file for faux sessions (integration harness).
pub const WORKER_SCRIPT_ENV: &str = "PRIME_AGENT_INTERNAL_DAEMON_WORKER_SCRIPT";
/// Worker socket path (supervisor passes it explicitly).
pub const WORKER_SOCKET_ENV: &str = "PRIME_AGENT_INTERNAL_DAEMON_WORKER_SOCKET";
/// Telemetry opt-out for the worker's sessions (supervisor passes the create
/// command's `telemetryDisabled` through here, TS descriptor parity).
pub const WORKER_TELEMETRY_DISABLED_ENV: &str =
    "PRIME_AGENT_INTERNAL_DAEMON_WORKER_TELEMETRY_DISABLED";
/// Supervisor-lost exit window (ms): a session worker whose supervisor
/// socket stays unreachable for this long exits instead of lingering
/// orphaned (TS `WORKER_SUPERVISOR_LOST_EXIT_MS_ENV` wire parity; the
/// supervisor's environment flows to the workers it spawns).
pub const WORKER_SUPERVISOR_LOST_EXIT_MS_ENV: &str =
    "PRIME_AGENT_INTERNAL_WORKER_SUPERVISOR_LOST_EXIT_MS";

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
    /// Telemetry opt-out inherited from the create command ("1" = disabled;
    /// absent/other = enabled). Sessions created on this worker install no
    /// telemetry subscriber.
    pub telemetry_disabled: Option<bool>,
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
        let agent_dir = paths::agent_dir()?;
        let recovery_journal_path = std::env::var_os(WORKER_RECOVERY_JOURNAL_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                agent_dir
                    .join("daemon-workers")
                    .join(format!("{}.recovery.jsonl", active_session_id))
            });
        let script = std::env::var_os(WORKER_SCRIPT_ENV)
            .map(PathBuf::from)
            .and_then(|path| {
                let content = std::fs::read_to_string(path).ok()?;
                serde_json::from_str::<Value>(&content).ok()
            });
        let telemetry_disabled =
            std::env::var_os(WORKER_TELEMETRY_DISABLED_ENV).map(|value| value == "1");
        Ok(WorkerConfig {
            socket_path,
            supervisor_socket_path,
            token,
            worker_instance_id: std::env::var(WORKER_INSTANCE_ID_ENV).unwrap_or_default(),
            active_session_id,
            agent_dir,
            recovery_journal_path,
            script,
            telemetry_disabled,
        })
    }
}

/// Result of one connection's authentication command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthOutcome {
    Authenticated,
    Failed,
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
pub(crate) struct QueuedItem {
    pub(crate) message: String,
    /// Images attached to the prompt (wire `images`: base64 payload plus
    /// mime type), admitted with the message as multimodal content.
    pub(crate) images: Vec<pa_agent::types::ImageContent>,
    pub(crate) done: Option<oneshot::Sender<Result<(), String>>>,
}

/// Parse the wire `images` array of a prompt-family command (each entry
/// `{type: "image", data, mimeType}`). Entries that do not carry payload
/// data or a mime type are dropped, not failed: the text still admits.
pub(crate) fn parse_prompt_images(payload: &Value) -> Vec<pa_agent::types::ImageContent> {
    let Some(images) = payload.get("images").and_then(Value::as_array) else {
        return Vec::new();
    };
    images
        .iter()
        .filter_map(|image| {
            if image.get("type").and_then(Value::as_str) != Some("image") {
                return None;
            }
            let data = image.get("data").and_then(Value::as_str)?;
            let mime_type = image.get("mimeType").and_then(Value::as_str)?;
            Some(pa_agent::types::ImageContent {
                data: data.to_string(),
                mime_type: mime_type.to_string(),
            })
        })
        .collect()
}

/// The live session: store, queue, sequencing. Shared by the connection tasks,
/// the turn runner, and the compaction manager; every access is through the
/// core mutex.
pub(crate) struct SessionCore {
    pub(crate) active_session_id: String,
    pub(crate) generation: String,
    pub(crate) last_event_sequence: u64,
    pub(crate) store: Option<SessionFile>,
    pub(crate) cwd: String,
    pub(crate) steering: VecDeque<QueuedItem>,
    pub(crate) follow_up: VecDeque<QueuedItem>,
    pub(crate) busy: bool,
    pub(crate) created: bool,
    attached_client_ids: Vec<String>,
    pub(crate) abort_requested: bool,
    pub(crate) shutdown_requested: bool,
    /// True while a compaction run is in flight (TS `isCompacting`).
    pub(crate) compacting: bool,
    /// TS `autoCompactionEnabled` (settings default: on).
    pub(crate) auto_compaction_enabled: bool,
    /// The last broadcast queue snapshot (TS `_lastSessionActionSnapshot`):
    /// `session_action_update` fires only when the projection changed.
    last_action_snapshot: Option<SessionActionSnapshot>,
    /// This session's RLM recursion depth (children run at depth + 1).
    rlm_depth: u32,
    /// `top-level` | `subagent` (summary `runtimeKind`).
    runtime_kind: String,
    /// The subagent runtime identity (create `runtimeMetadata`): the child
    /// id under its parent and the parent's live/persisted ids, carried on
    /// every summary so the roster keys children `parentPath#childId`.
    rlm_child_id: Option<String>,
    parent_active_session_id: Option<String>,
    parent_session_id: Option<String>,
}

impl SessionCore {
    /// Whether a turn, compaction, or queued action is in flight — the TS
    /// `hasOngoingSessionWork` predicate. An active run owns the worker a
    /// little longer; the supervisor-lost exit waits for it to settle.
    pub(crate) fn has_ongoing_work(&self) -> bool {
        self.busy || self.compacting || !self.steering.is_empty() || !self.follow_up.is_empty()
    }
}

impl crate::status_line::StatusSession for SessionCore {
    fn status_messages(&self) -> Vec<Value> {
        self.store
            .as_ref()
            .map(|store| store.messages())
            .unwrap_or_default()
    }

    fn status_busy(&self) -> bool {
        self.busy
    }

    fn status_active_session_id(&self) -> String {
        self.active_session_id.clone()
    }

    fn status_generation(&self) -> String {
        self.generation.clone()
    }

    fn status_next_sequence(&mut self) -> u64 {
        self.last_event_sequence += 1;
        self.last_event_sequence
    }

    fn status_append_agent_status(
        &mut self,
        status: &crate::status_line::PersistedAgentStatus,
    ) -> Result<()> {
        let Some(store) = self.store.as_mut() else {
            return Ok(());
        };
        let persisted = pa_types::session::AgentStatus {
            summary: status.summary.clone(),
            task_state: status
                .task_state
                .map(crate::status_line::AgentTaskState::persisted),
            based_on_message_count: status.based_on_message_count as u64,
        };
        store.persist_entry(
            "agent_status",
            json!({ "status": serde_json::to_value(&persisted)? }),
        )?;
        Ok(())
    }

    fn status_latest_agent_status(&self) -> Option<crate::status_line::PersistedAgentStatus> {
        let store = self.store.as_ref()?;
        let entry = store
            .entries()
            .iter()
            .rev()
            .find(|entry| entry.type_ == "agent_status")?;
        let status: pa_types::session::AgentStatus =
            serde_json::from_value(entry.fields.get("status")?.clone()).ok()?;
        Some(crate::status_line::PersistedAgentStatus {
            summary: status.summary,
            task_state: status
                .task_state
                .map(crate::status_line::AgentTaskState::from_persisted),
            based_on_message_count: status.based_on_message_count as usize,
        })
    }
}

/// One outbound frame: the serialized JSON payload plus its private-frame
/// `outboundType` (`session_event` or `side_question_event`), mirroring the
/// TS worker frame header. The supervisor fans frames out per its own
/// routing (clients attached to the session).
pub(crate) struct OutboundFrame {
    pub(crate) payload: Vec<u8>,
    pub(crate) outbound_type: &'static str,
    /// The pump-assigned broadcast sequence. Connection sinks use it as a
    /// flush position so response frames cannot overtake event frames.
    pub(crate) seq: u64,
}

impl OutboundFrame {
    pub(crate) fn session_event(payload: Vec<u8>) -> Self {
        OutboundFrame {
            payload,
            outbound_type: "session_event",
            seq: 0,
        }
    }

    pub(crate) fn session_status(payload: Vec<u8>) -> Self {
        OutboundFrame {
            payload,
            outbound_type: "session_status",
            seq: 0,
        }
    }

    pub(crate) fn side_question_event(payload: Vec<u8>) -> Self {
        OutboundFrame {
            payload,
            outbound_type: "side_question_event",
            seq: 0,
        }
    }
}

/// The worker's outbound event pump: one sequence-stamped broadcast stream
/// shared by every frame-emitting path (turns, compaction, side questions,
/// status lines). Sequences are assigned under a send guard so channel
/// delivery order matches sequence order, which keeps per-connection flush
/// positions monotonic.
pub(crate) struct EventPump {
    events: broadcast::Sender<Arc<OutboundFrame>>,
    next_seq: AtomicU64,
    send_guard: std::sync::Mutex<()>,
}

impl EventPump {
    pub(crate) fn new() -> Self {
        let (events, _) = broadcast::channel(4096);
        EventPump {
            events,
            next_seq: AtomicU64::new(0),
            send_guard: std::sync::Mutex::new(()),
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Arc<OutboundFrame>> {
        self.events.subscribe()
    }

    /// Stamp the frame with the next sequence and broadcast it.
    pub(crate) fn send(&self, mut frame: OutboundFrame) {
        let _guard = self.send_guard.lock().unwrap();
        frame.seq = self.next_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.events.send(Arc::new(frame));
    }

    /// The current broadcast sequence: a response written now must wait for
    /// every frame with a sequence up to this value to be flushed.
    pub(crate) fn current_seq(&self) -> u64 {
        self.next_seq.load(Ordering::SeqCst)
    }
}

/// One connection's outbound state: the framed writer plus the fan-out's
/// flush position. The TS worker writes session events synchronously while
/// a command runs, so its command response always follows them; the Rust
/// fan-out is a separate task, so response writes wait for the fan-out to
/// catch up to the sequence they observed (`wait_flushed`), restoring the
/// same ordering contract: events emitted during a command are written
/// before the command's response, never after it.
pub(crate) struct ConnectionSink {
    pub(crate) writer:
        Arc<tokio::sync::Mutex<Box<dyn pa_types::platform::transport::AsyncWriteHalf>>>,
    /// The fan-out's flush position; `FLUSH_CLOSED` once the fan-out ended.
    /// Watch semantics: a send with zero live receivers is dropped, so
    /// the sink keeps a permanent receiver and every position update is
    /// stored even while no response is waiting.
    flushed: tokio::sync::watch::Sender<u64>,
    _flushed_anchor: tokio::sync::watch::Receiver<u64>,
    /// The first broadcast sequence this connection's fan-out can receive:
    /// frames older than this were broadcast before the connection
    /// subscribed and are never delivered to it, so a gate below `entry_seq`
    /// is already satisfied.
    entry_seq: u64,
}

/// The fan-out either wrote every frame or the connection ended; a waiting
/// response proceeds on both paths.
const FLUSH_CLOSED: u64 = u64::MAX;

impl ConnectionSink {
    pub(crate) fn new(
        writer: Arc<tokio::sync::Mutex<Box<dyn pa_types::platform::transport::AsyncWriteHalf>>>,
        entry_seq: u64,
    ) -> Self {
        let (flushed, _flushed_anchor) = tokio::sync::watch::channel(0);
        ConnectionSink {
            writer,
            flushed,
            _flushed_anchor,
            entry_seq,
        }
    }

    /// Record the fan-out's position after one processed frame (written or
    /// skipped for role reasons: a skipped frame cannot arrive later).
    pub(crate) fn mark_flushed(&self, seq: u64) {
        let _ = self.flushed.send(seq);
    }

    /// The fan-out ended (write failure or closed stream); waiting
    /// responses stop waiting.
    pub(crate) fn mark_closed(&self) {
        let _ = self.flushed.send(FLUSH_CLOSED);
    }

    /// Block until the fan-out flushed `gate` (or ended).
    pub(crate) async fn wait_flushed(&self, gate: u64) {
        // Frames older than `entry_seq` are never delivered to this
        // connection, so a gate below them needs no wait.
        if gate < self.entry_seq {
            return;
        }
        let mut rx = self.flushed.subscribe();
        loop {
            if *rx.borrow_and_update() >= gate {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Releases a connection's supervisor claim when the connection ends:
/// the supervisor-role connection on the worker's socket is the supervisor's
/// presence proof for the orphan-exit monitor, so its end must decrement
/// the claim count on every return path. Inspects the role at drop time —
/// only a connection that authenticated as the supervisor ever claimed.
struct SupervisorClaimRelease {
    role: Arc<std::sync::Mutex<crate::peer::ConnectionRole>>,
    claims: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for SupervisorClaimRelease {
    fn drop(&mut self) {
        let supervisor = matches!(
            *self.role.lock().unwrap(),
            crate::peer::ConnectionRole::Supervisor { .. }
        );
        if supervisor {
            self.claims
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

pub struct Worker {
    pub(crate) config: WorkerConfig,
    /// Supervisor self-registration handle; `None` for standalone workers.
    registration: Option<RegistrationHandle>,
    /// Live connections authenticated as the supervisor role. A non-zero
    /// count disarms the supervisor-lost exit monitor (TS
    /// `hasAuthenticatedSupervisorConnection`): while the supervisor is
    /// connected on this socket, it is by definition reachable.
    pub(crate) supervisor_claims: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) core: Arc<Mutex<SessionCore>>,
    pub(crate) engine: std::sync::Arc<dyn SessionEngine>,
    pub(crate) work_notify: Arc<Notify>,
    idle_notify: Arc<Notify>,
    events: Arc<EventPump>,
    recovery: Arc<Mutex<Option<WorkerRecoveryJournal>>>,
    /// Post-turn status-line runner (seeded from persisted verdicts at
    /// session create).
    status_runner: std::sync::Arc<crate::status_line::StatusLineRunner<SessionCore>>,
    /// Live side-question runs (registry, guards, event frames).
    side_questions: crate::side_question::SideQuestionManager,
    /// Single-use peer-transport grants (worker memory only).
    pub(crate) peer_grants: PeerGrantStore,
    /// Compaction runs: abort slot, events, durable entry persistence.
    compaction: crate::compaction::CompactionManager,
    /// Session-scoped ACP MCP servers for engines without their own store
    /// (the scripted harness); the real engine's manager serves the
    /// product path.
    acp_mcp: std::sync::Arc<std::sync::Mutex<pa_core::mcp::McpManager>>,
}

/// Supervisor-link coordinates for a worker's agent engine: where the
/// supervisor listens and who this worker is on it.
fn supervisor_link_config(config: &WorkerConfig) -> SupervisorLinkConfig {
    SupervisorLinkConfig {
        socket_path: config.supervisor_socket_path.clone(),
        active_session_id: config.active_session_id.clone(),
        worker_token: config.token.clone(),
    }
}

impl Worker {
    pub fn new(config: WorkerConfig, registration: Option<RegistrationHandle>) -> Self {
        let events = Arc::new(EventPump::new());
        let supervisor_claims = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
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
            compacting: false,
            auto_compaction_enabled: true,
            // TS seeds `_lastSessionActionSnapshot` with the empty
            // projection, so a fresh session's first empty snapshot is not
            // an update.
            last_action_snapshot: Some(SessionActionSnapshot::default()),
            rlm_depth: 0,
            runtime_kind: "top-level".to_string(),
            rlm_child_id: None,
            parent_active_session_id: None,
            parent_session_id: None,
        };
        let active_session_id = config.active_session_id.clone();
        let script = config.script.clone();
        let core = Arc::new(Mutex::new(core));
        // Shared worker recovery journal: the turn runner persists queue
        // snapshots into it, `serve` opens the file, and command handlers
        // record busy/operation state.
        let recovery = Arc::new(Mutex::new(None));
        let work_notify = Arc::new(Notify::new());
        let idle_notify = Arc::new(Notify::new());
        // The post-turn status line: turn-end notifications (debounced) and
        // periodic sweeps ask the small dashboard model for a recap.
        let status_runner = std::sync::Arc::new(crate::status_line::StatusLineRunner::new(
            std::sync::Arc::clone(&core),
            config.agent_dir.clone(),
            events.clone(),
        ));
        let (status_notify, status_rx) = tokio::sync::mpsc::unbounded_channel();
        let status_runner_handle = std::sync::Arc::clone(&status_runner);
        tokio::spawn(async move {
            status_runner.run(status_rx).await;
        });
        // The turn runner runs for the whole process lifetime. The command
        // dispatcher keeps the engine handle too (model metadata for the
        // stats commands).
        let engine: std::sync::Arc<dyn SessionEngine> = {
            // Scripted sessions serve the integration harness; sessions
            // without a script run the real agent engine.
            let engine: std::sync::Arc<dyn SessionEngine> = match &script {
                // A `{"engine": "faux", ...}` script drives the real agent
                // engine over the scripted faux provider (full turns with
                // tools, thinking, and token-paced streaming). Verification
                // harness only; the product never sets a script.
                Some(script) if script.get("engine") == Some(&serde_json::json!("faux")) => {
                    let cwd =
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                    match AgentSessionEngine::new(AgentEngineConfig {
                        cwd,
                        agent_dir: config.agent_dir.clone(),
                        provider: None,
                        model: None,
                        api_key: None,
                        thinking: None,
                        session_dir: None,
                        session_file: None,
                        faux_script: Some(script.to_string()),
                        supervisor_link: Some(supervisor_link_config(&config)),
                        telemetry_disabled: config.telemetry_disabled,
                    }) {
                        Ok(engine) => std::sync::Arc::new(engine),
                        // Runtime construction failed: degrade to the echo engine.
                        Err(_) => std::sync::Arc::new(ScriptedEngine::default()),
                    }
                }
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
                        thinking: None,
                        session_dir: None,
                        session_file: None,
                        faux_script: None,
                        supervisor_link: Some(supervisor_link_config(&config)),
                        telemetry_disabled: config.telemetry_disabled,
                    }) {
                        Ok(engine) => std::sync::Arc::new(engine),
                        // Runtime construction failed: degrade to the echo engine.
                        Err(_) => std::sync::Arc::new(ScriptedEngine::default()),
                    }
                }
            };
            let runner = TurnRunner {
                recovery: Arc::clone(&recovery),
                core: Arc::clone(&core),
                work_notify: Arc::clone(&work_notify),
                idle_notify: Arc::clone(&idle_notify),
                events: events.clone(),
                engine: std::sync::Arc::clone(&engine),
                active_session_id,
                status_notify: status_notify.clone(),
                roster_link: std::sync::Arc::new(crate::supervisor_link::SupervisorLink::new(
                    std::env::var_os(WORKER_SUPERVISOR_SOCKET_ENV)
                        .map(std::path::PathBuf::from)
                        .unwrap_or_default(),
                )),
                worker_token: std::env::var(WORKER_TOKEN_ENV).unwrap_or_default(),
            };
            tokio::spawn(async move {
                runner.run().await;
            });
            engine
        };
        let side_questions = crate::side_question::SideQuestionManager::new(
            std::sync::Arc::clone(&engine),
            events.clone(),
            config.active_session_id.clone(),
        );
        let compaction = crate::compaction::CompactionManager::new(
            std::sync::Arc::clone(&engine),
            events.clone(),
            Arc::clone(&core),
            config.active_session_id.clone(),
            config.agent_dir.clone(),
        );
        // The session-scoped ACP MCP manager: auth storage construction is
        // blocking, so the builder runs off the async runtime (the same
        // pattern as the session engine's MCP gating).
        let agent_dir = config.agent_dir.clone();
        let acp_mcp = pa_core::mcp::McpManager::new(pa_core::mcp::McpManagerOptions {
            auth_storage: pa_core::auth::AuthStorage::create(&agent_dir),
            get_user_servers: Box::new(|| None),
            begin_login: None,
        });
        Worker {
            config,
            registration,
            supervisor_claims,
            core,
            engine,
            work_notify,
            idle_notify,
            events,
            recovery,
            status_runner: status_runner_handle,
            side_questions,
            peer_grants: PeerGrantStore::new(),
            compaction,
            acp_mcp: std::sync::Arc::new(std::sync::Mutex::new(acp_mcp)),
        }
    }

    /// Serve worker connections until the process is asked to shut down.
    pub async fn serve(self: Arc<Self>) -> Result<()> {
        *self.recovery.lock().unwrap() = Some(WorkerRecoveryJournal::open(
            &self.config.recovery_journal_path,
        )?);
        // A worker spawned under a supervisor arms the orphan-exit monitor
        // (TS `startSupervisorMonitor`): nobody else reaps it if the
        // supervisor dies without a graceful stop.
        if !self.config.supervisor_socket_path.as_os_str().is_empty() {
            crate::supervisor_lost::start(self.clone());
        }
        crate::socket::prepare_socket_path(&self.config.socket_path).await?;
        let listener = bind_transport(&self.config.socket_path)
            .await
            .with_context(|| format!("bind worker socket {}", self.config.socket_path.display()))?;
        crate::socket::restrict_socket_path(&self.config.socket_path);
        loop {
            let stream = match listener.accept().await {
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

    async fn handle_connection(self: Arc<Self>, stream: Box<dyn TransportStream>) -> Result<()> {
        let (reader, writer) = stream.split();
        let writer = Arc::new(tokio::sync::Mutex::new(writer));
        // The connection's event subscription and its entry sequence are
        // captured together (before any awaited write): every frame the
        // receiver can see has a sequence at or above `entry_seq`, which is
        // what the sink's flush barrier gates on.
        let subscription = self.events.subscribe();
        let entry_seq = self.events.current_seq() + 1;
        // The connection's outbound sink: the framed writer plus the
        // fan-out flush position (response writes wait on it; see
        // `ConnectionSink`).
        let sink = Arc::new(ConnectionSink::new(Arc::clone(&writer), entry_seq));
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
            // The worker's hello carries no resume contract (the
            // supervisor owns the boot restore pass).
            update_resume: None,
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

        // The connection's authenticated role, shared with the event
        // fan-out task (streaming is gated on it).
        let role = Arc::new(std::sync::Mutex::new(ConnectionRole::Unauthenticated));

        // Releases the supervisor claim this connection may take (see
        // `SupervisorClaimRelease`): the claim's lifetime is the
        // connection's, so every return path (EOF, auth failure, frame
        // error) goes through the same decrement.
        let _claim_release = SupervisorClaimRelease {
            role: Arc::clone(&role),
            claims: Arc::clone(&self.supervisor_claims),
        };

        // Connection-closed signal: the read loop fires it when the peer is
        // gone (EOF, auth failure) or drops it on return. The fan-out task
        // must not outlive the connection - the shared-socket write half it
        // holds keeps the socket fd open, and a per-connection fd leak here
        // (probes, direct clients, peer deliveries) ends in EMFILE for a
        // long-lived worker.
        let (closed_tx, closed_rx) = tokio::sync::watch::channel(false);

        // Event fan-out: this connection's subscription to the shared pump.
        // Only authenticated roles stream: the supervisor always, a session
        // client only while it holds an attach on the session.
        {
            let worker = Arc::clone(&self);
            let sink = Arc::clone(&sink);
            let role = Arc::clone(&role);
            let mut closed = closed_rx;
            tokio::spawn(async move {
                let mut events = subscription;
                loop {
                    tokio::select! {
                        // The read loop ended (or dropped its sender):
                        // release the subscription and the write half so
                        // the socket fd closes.
                        changed = closed.changed() => {
                            let _ = changed;
                            sink.mark_closed();
                            break;
                        }
                        received = events.recv() => {
                            match received {
                                Ok(frame) => {
                                    // A frame this role does not stream still
                                    // advances the flush position: it cannot be
                                    // delivered later, so a gated response must not
                                    // wait for it.
                                    if role.lock().unwrap().streams_events() {
                                        let active_session_id = active_session_id_of(&frame.payload);
                                        let header = json!({
                                            "kind": "outbound",
                                            "outboundType": frame.outbound_type,
                                            "activeSessionId": active_session_id,
                                        });
                                        if worker
                                            .write_frame(&sink.writer, &header, &frame.payload)
                                            .await
                                            .is_err()
                                        {
                                            sink.mark_closed();
                                            break;
                                        }
                                    }
                                    sink.mark_flushed(frame.seq);
                                }
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(broadcast::error::RecvError::Closed) => {
                                    sink.mark_closed();
                                    break;
                                }
                            }
                        }
                    }
                }
            });
        }

        let mut reader =
            crate::framing::PrivateFrameReader::new(reader, DEFAULT_PRIVATE_FRAME_LIMITS);
        loop {
            let frame: Option<crate::framing::PrivateFrame> = reader.read_frame().await?;
            let Some(frame) = frame else {
                // Peer closed: wake the fan-out so it drops the write half.
                let _ = closed_tx.send(true);
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

            let current_role = role.lock().unwrap().clone();
            match current_role {
                ConnectionRole::Unauthenticated => {
                    // The first command authenticates the connection; a
                    // failed authentication ends it (TS worker branch).
                    let outcome = self
                        .authenticate_connection(&command_type, &payload, &request_id, &role, &sink)
                        .await;
                    if outcome == AuthOutcome::Failed {
                        // Failed auth ends the connection: wake the fan-out
                        // so it releases the write half (and the fd).
                        let _ = closed_tx.send(true);
                        break;
                    }
                }
                ConnectionRole::Supervisor { ref generation } => {
                    if command_type == "worker_register_peer_transport" {
                        let response =
                            self.handle_worker_register_peer_transport(&payload, generation);
                        self.write_response_frame(&sink, &request_id, &response)
                            .await;
                        continue;
                    }
                    // Shutdown stays sequential: the reply must precede the
                    // exit. Every other command runs concurrently, like the
                    // TS daemon's async handlers: a long-running command (a
                    // turn, a compaction) must not block aborts or state
                    // reads from other clients.
                    if command_type == "shutdown" {
                        let response = self.dispatch(&command_type, &payload).await;
                        self.write_response_frame(&sink, &request_id, &response)
                            .await;
                        if response.success {
                            // Shutdown keeps the resume entry and exits the
                            // process, like the TS close path
                            // (`closeKeepsResumeEntry("shutdown")`).
                            let _ = self.record_recovery(false, "shutdown");
                            // A graceful exit owns its socket file: remove
                            // it now so a respawn does not wait out the
                            // stale-socket path (a killed worker cannot
                            // clean up, but its killer relaunches through
                            // `prepare_socket_path`).
                            crate::socket::cleanup_socket_path(
                                &self.config.socket_path,
                                crate::socket::socket_identity(&self.config.socket_path),
                            );
                            std::process::exit(0);
                        }
                        continue;
                    }
                    let worker = Arc::clone(&self);
                    let sink = Arc::clone(&sink);
                    let request_id = request_id.clone();
                    let command_type = command_type.clone();
                    tokio::spawn(async move {
                        let response = worker.dispatch(&command_type, &payload).await;
                        worker
                            .write_response_frame(&sink, &request_id, &response)
                            .await;
                    });
                }
                ConnectionRole::SessionClient { ref session } => {
                    // A direct peer may only run session-plane commands for
                    // the grant's session (TS `peerClaims` gate).
                    if !peer_command_allowed(&command_type, &payload, &session.grant) {
                        let failure = response_failure(
                            Some(&request_id),
                            &command_type,
                            PEER_COMMAND_NOT_ALLOWED,
                            None,
                        );
                        self.write_response_frame(&sink, &request_id, &failure)
                            .await;
                        continue;
                    }
                    // Session-plane commands run concurrently for the same
                    // reason as the supervisor arm above.
                    let worker = Arc::clone(&self);
                    let sink = Arc::clone(&sink);
                    let request_id = request_id.clone();
                    let command_type = command_type.clone();
                    let session = Arc::clone(session);
                    tokio::spawn(async move {
                        let response = worker.dispatch(&command_type, &payload).await;
                        if response.success {
                            match command_type.as_str() {
                                "attach" => session.mark_attached(),
                                "detach" => session.mark_detached(),
                                _ => {}
                            }
                        }
                        worker
                            .write_response_frame(&sink, &request_id, &response)
                            .await;
                    });
                }
                ConnectionRole::PeerWorker { ref session } => {
                    // A peer worker delivers agent messages only, for the
                    // grant's session; everything else bounces with the TS
                    // gate string.
                    if !worker_peer_command_allowed(&command_type, &payload, &session.grant) {
                        let failure = response_failure(
                            Some(&request_id),
                            &command_type,
                            PEER_COMMAND_NOT_ALLOWED,
                            None,
                        );
                        self.write_response_frame(&sink, &request_id, &failure)
                            .await;
                        continue;
                    }
                    // Delivery runs concurrently, like the other planes.
                    let worker = Arc::clone(&self);
                    let sink = Arc::clone(&sink);
                    let request_id = request_id.clone();
                    let command_type = command_type.clone();
                    tokio::spawn(async move {
                        let response = worker.dispatch(&command_type, &payload).await;
                        worker
                            .write_response_frame(&sink, &request_id, &response)
                            .await;
                    });
                }
            }
        }
        Ok(())
    }

    /// Authenticate one connection's first command: `worker_auth` promotes
    /// the connection to the supervisor role, `peer_auth` to a session
    /// client role holding a burned single-use grant. Writes the response.
    async fn authenticate_connection(
        self: &Arc<Self>,
        command_type: &str,
        payload: &Value,
        request_id: &str,
        role: &Arc<std::sync::Mutex<ConnectionRole>>,
        sink: &ConnectionSink,
    ) -> AuthOutcome {
        if command_type == "peer_auth" {
            return self.handle_peer_auth(payload, request_id, role, sink).await;
        }
        if command_type != "worker_auth" {
            let failure = response_failure(
                Some(request_id),
                "worker_auth",
                "Worker authentication failed",
                None,
            );
            self.write_response_frame(sink, request_id, &failure).await;
            return AuthOutcome::Failed;
        }
        match self.authenticate(payload) {
            Ok(()) => {
                if std::env::var("PA_DAEMON_DEBUG").is_ok() {
                    eprintln!("[worker {}] auth ok", std::process::id());
                }
                let generation = payload
                    .get("supervisorGeneration")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                // The roster capability is always granted; the peer
                // transport capability rides on the worker instance
                // id, like the TS worker.
                let mut capabilities = vec!["agent_roster".to_string()];
                if !self.config.worker_instance_id.is_empty() {
                    capabilities.push("direct_peer_transport".to_string());
                }
                let success = response_success(
                    Some(request_id),
                    "worker_auth",
                    Some(json!({ "capabilities": capabilities })),
                );
                *role.lock().unwrap() = ConnectionRole::Supervisor { generation };
                self.supervisor_claims
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.write_response_frame(sink, request_id, &success).await;
                AuthOutcome::Authenticated
            }
            Err(error) => {
                let failure =
                    response_failure(Some(request_id), "worker_auth", &error.to_string(), None);
                self.write_response_frame(sink, request_id, &failure).await;
                AuthOutcome::Failed
            }
        }
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

    pub(crate) async fn write_frame(
        &self,
        writer: &Arc<tokio::sync::Mutex<Box<dyn pa_types::platform::transport::AsyncWriteHalf>>>,
        header: &Value,
        payload: &[u8],
    ) -> Result<()> {
        let mut guard = writer.lock().await;
        write_frame(&mut *guard, header, payload, DEFAULT_PRIVATE_FRAME_LIMITS)
            .await
            .context("write private frame")
    }

    pub(crate) async fn write_response_frame(
        &self,
        sink: &ConnectionSink,
        request_id: &str,
        response: &DaemonResponse,
    ) {
        // Flush barrier: every event frame broadcast before this response
        // reaches the connection's writer first, so a command response
        // never overtakes the events its command emitted (the TS worker
        // gets this ordering for free from synchronous writes).
        sink.wait_flushed(self.events.current_seq()).await;
        let payload =
            serde_json::to_vec(&crate::protocol::response_line(response)).unwrap_or_default();
        let header = json!({
            "kind": "outbound",
            "requestId": request_id,
            "outboundType": "response",
        });
        if let Err(error) = self.write_frame(&sink.writer, &header, &payload).await {
            eprintln!("pa-daemon worker response write failed: {error:#}");
        }
    }

    pub(crate) async fn dispatch(&self, command_type: &str, payload: &Value) -> DaemonResponse {
        match command_type {
            "create" => self.handle_create(payload),
            "attach" => self.handle_attach(payload),
            "detach" => self.handle_detach(payload),
            "prompt" => self.handle_prompt(payload, false).await,
            "prompt_and_wait" => self.handle_prompt(payload, true).await,
            "steer" => self.handle_queue(payload, Lane::Steering),
            "follow_up" => self.handle_queue(payload, Lane::FollowUp),
            "abort" => self.handle_abort(),
            "start_side_question" => {
                if let Err(response) = self.require_created("start_side_question") {
                    return response;
                }
                self.side_questions.start(payload)
            }
            "abort_side_question" => {
                if let Err(response) = self.require_created("abort_side_question") {
                    return response;
                }
                self.side_questions.abort(payload)
            }
            "compact" => self.handle_compaction(payload).await,
            "abort_compaction" => {
                self.compaction.abort();
                response_success(None, "abort_compaction", None)
            }
            "set_auto_compaction" => self.handle_set_auto_compaction(payload),
            "wait_for_idle" => self.handle_wait_for_idle().await,
            "wait_for_headless_completion" => self.handle_wait_for_headless_completion().await,
            "get_state" => self.handle_get_state(),
            "get_messages" => self.handle_get_messages(),
            "get_session_header" => self.handle_get_session_header(),
            "get_session_stats" => self.handle_get_session_stats(),
            "get_queue" => self.handle_get_queue(),
            "clear_queue" => self.handle_clear_queue(),
            "abort_and_clear_queue" => self.handle_abort_and_clear_queue(),
            "get_last_assistant_text" => self.handle_get_last_assistant_text(),
            "worker_deliver_message" => self.handle_worker_deliver_message(payload),
            "update_snapshot" => self.handle_update_snapshot(),
            "kill" => self.handle_kill().await,
            "shutdown" => self.handle_shutdown().await,
            "rename" => self.handle_rename("rename", payload),
            "set_session_name" => self.handle_rename("set_session_name", payload),
            "replace_acp_mcp_servers" => self.handle_replace_acp_mcp_servers(payload),
            "set_model" => self.handle_set_model(payload).await,
            "set_thinking_level" => self.handle_set_thinking_level(payload).await,
            "mutate_queued_message" => self.handle_mutate_queued_message(payload),
            "resume_queue" => self.handle_resume_queue(),
            other => response_failure(
                None,
                command_type,
                &format!("Unknown worker command: {other}"),
                None,
            ),
        }
    }

    #[allow(clippy::result_large_err)]
    pub(crate) fn require_created(&self, command_type: &str) -> Result<(), DaemonResponse> {
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

    /// `replace_acp_mcp_servers` (TS daemon-mode.ts case): the session's
    /// owner-fenced ACP MCP store. The ACP transport resolves and validates
    /// the servers before sending them; the worker only fences ownership,
    /// guards the busy turn, and rolls back a failed replacement.
    fn handle_replace_acp_mcp_servers(&self, payload: &Value) -> DaemonResponse {
        let owner_id = payload
            .get("ownerId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if owner_id.is_empty() {
            return response_failure(
                None,
                "replace_acp_mcp_servers",
                "ACP MCP owner id is required",
                None,
            );
        }
        let servers: Vec<pa_core::mcp::AcpMcpServerConfig> = payload
            .get("servers")
            .cloned()
            .map(|servers| serde_json::from_value(servers).unwrap_or_default())
            .unwrap_or_default();
        // The agent cannot adopt a different MCP tool list mid-turn (TS
        // `session.isStreaming` guard).
        if !servers.is_empty() && self.core.lock().unwrap().busy {
            return response_failure(
                None,
                "replace_acp_mcp_servers",
                "Cannot replace ACP MCP servers while the agent is running",
                None,
            );
        }
        // The real agent engine owns the session's MCP store (one store
        // for admission and prompt gating); scripted harness engines fall
        // back to the worker-level store.
        let manager = self
            .engine
            .acp_mcp_manager()
            .unwrap_or_else(|| std::sync::Arc::clone(&self.acp_mcp));
        let manager = manager.lock().unwrap();
        match manager.replace_acp_servers(&servers, owner_id) {
            // An unchanged list (same owner, identical servers) is a no-op
            // success, like the TS manager's unchanged short-circuit.
            Ok(_) => response_success(None, "replace_acp_mcp_servers", None),
            Err(error) => {
                // Roll back any partially applied configuration with the
                // owner-scoped clear, exactly like the TS rollback, before
                // surfacing the failure.
                if manager.can_release_acp_servers(owner_id) {
                    let _ = manager.replace_acp_servers(&[], owner_id);
                }
                response_failure(None, "replace_acp_mcp_servers", &error.to_string(), None)
            }
        }
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
        let session_path = match payload.get("sessionPath").and_then(Value::as_str) {
            Some(path) => match paths::expand_tilde(path) {
                Ok(expanded) => Some(expanded),
                Err(error) => return response_failure(None, "create", &error.to_string(), None),
            },
            None => None,
        };
        let no_session = payload
            .get("noSession")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let name = payload.get("name").and_then(Value::as_str);
        // Explicit model flags from the create config are authoritative for
        // this session (TS runtime-config propagation): the engine rebinds
        // its selection instead of falling back to a process-wide model.
        let requested_thinking = match payload.get("thinking") {
            None => None,
            Some(Value::String(level)) => {
                match pa_ai::models::thinking_level_from_str(level) {
                    Some(level) => Some(level),
                    // The wire contract takes validated levels only: reject
                    // the create loudly instead of silently dropping it.
                    None => {
                        return response_failure(
                            None,
                            "create",
                            &format!("Invalid thinking level \"{level}\". Valid values: off, minimal, low, medium, high, xhigh, max"),
                            None,
                        );
                    }
                }
            }
            Some(_) => {
                return response_failure(
                    None,
                    "create",
                    "Invalid thinking level: expected a string",
                    None,
                );
            }
        };
        self.engine.configure_model(EngineModelSelection {
            provider: payload
                .get("provider")
                .and_then(Value::as_str)
                .map(str::to_string),
            model: payload
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
            api_key: payload
                .get("apiKey")
                .and_then(Value::as_str)
                .map(str::to_string),
            thinking: requested_thinking,
        });
        let cwd = payload
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or("/")
            .to_string();
        let session_dir = match payload.get("sessionDir").and_then(Value::as_str) {
            Some(dir) => match paths::expand_tilde(dir) {
                Ok(expanded) => expanded,
                Err(error) => return response_failure(None, "create", &error.to_string(), None),
            },
            None => match paths::sessions_dir(&self.config.agent_dir) {
                Ok(dir) => dir,
                Err(error) => return response_failure(None, "create", &error.to_string(), None),
            },
        };
        // RLM recursion identity (children of an RLM parent run at depth+1):
        // the durable create replays these so a respawned child keeps them.
        let (rlm_depth, rlm_max_depth) = match create_payload_rlm_depth(payload) {
            Ok(identity) => identity,
            Err(error) => return response_failure(None, "create", &error, None),
        };
        let parent_session_path = payload
            .get("parentSessionPath")
            .and_then(Value::as_str)
            .map(str::to_string);
        // The subagent runtime identity (TS `runtimeMetadata` on the create
        // command): the child id and the parent's live/persisted ids ride
        // the session summaries so the roster can key children
        // `parentPath#childId` like TS `rosterAgentIdForSummary`.
        let (rlm_child_id, parent_active_session_id, parent_session_id) = match payload
            .get("runtimeMetadata")
        {
            Some(metadata) if metadata.get("kind").and_then(Value::as_str) == Some("subagent") => (
                metadata
                    .get("rlmChildId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                metadata
                    .get("parentActiveSessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                metadata
                    .get("parentSessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            ),
            _ => (None, None, None),
        };
        let thinking = payload
            .get("thinking")
            .and_then(Value::as_str)
            .map(str::to_string);

        let mut store = match (&session_path, no_session) {
            (Some(path), false) if path.exists() => match SessionFile::open(path) {
                Ok(mut opened) => {
                    append_creation_prefix(
                        &mut opened,
                        self.engine.as_ref(),
                        &self.config.agent_dir,
                        &cwd,
                        false,
                    );
                    let _ = opened.append_session_state("active");
                    if let Err(error) = opened.rewrite() {
                        return response_failure(None, "create", &error.to_string(), None);
                    }
                    opened
                }
                Err(error) => return response_failure(None, "create", &error.to_string(), None),
            },
            (Some(path), false) => {
                let mut created =
                    SessionFile::create(&cwd, parent_session_path.as_deref(), rlm_depth);
                created.set_path(path.clone());
                if let Err(error) = created.rewrite() {
                    return response_failure(None, "create", &error.to_string(), None);
                }
                append_creation_prefix(
                    &mut created,
                    self.engine.as_ref(),
                    &self.config.agent_dir,
                    &cwd,
                    true,
                );
                let _ = created.append_session_state("active");
                if let Err(error) = created.rewrite() {
                    return response_failure(None, "create", &error.to_string(), None);
                }
                created
            }
            // In-memory session: no file, like the TS `noSession` create.
            (None, true) => {
                let mut created =
                    SessionFile::create(&cwd, parent_session_path.as_deref(), rlm_depth);
                append_creation_prefix(
                    &mut created,
                    self.engine.as_ref(),
                    &self.config.agent_dir,
                    &cwd,
                    true,
                );
                created
            }
            (None, false) => {
                let mut created =
                    SessionFile::create(&cwd, parent_session_path.as_deref(), rlm_depth);
                let path = session_dir.join(session_file_name(created.session_id()));
                created.set_path(path);
                if let Err(error) = created.rewrite() {
                    return response_failure(None, "create", &error.to_string(), None);
                }
                append_creation_prefix(
                    &mut created,
                    self.engine.as_ref(),
                    &self.config.agent_dir,
                    &cwd,
                    true,
                );
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
        // Restore the persisted queue snapshot (crash/respawn recovery) from
        // the worker recovery journal.
        let (steering, follow_up) = {
            let guard = self.recovery.lock().unwrap();
            match guard.as_ref() {
                Some(journal) => restore_queue_snapshot(journal, &self.config.active_session_id),
                None => (VecDeque::new(), VecDeque::new()),
            }
        };
        // The worker owns the session file; the engine reads it for the
        // system prompt's conversation-log path and the local harness dir.
        if !store.path.as_os_str().is_empty() {
            self.engine.set_session_file(store.path.clone());
        }
        let mut core = self.core.lock().unwrap();
        core.cwd = cwd;
        core.steering = steering;
        core.follow_up = follow_up;
        core.store = Some(store);
        core.created = true;
        core.abort_requested = false;
        core.rlm_depth = rlm_depth;
        core.runtime_kind = if rlm_depth > 0 || rlm_child_id.is_some() {
            "subagent".to_string()
        } else {
            "top-level".to_string()
        };
        core.rlm_child_id = rlm_child_id;
        core.parent_active_session_id = parent_active_session_id;
        core.parent_session_id = parent_session_id;
        let summary = self.summary_locked(&core);
        drop(core);
        // Seed the engine's RLM identity: recursion depth and bound, this
        // session's persistence ids, and the default thinking level its
        // children inherit.
        if let Err(error) = self.engine.configure_rlm_identity(RlmSessionIdentity {
            rlm_depth,
            rlm_max_depth,
            cwd: Some(summary.cwd.clone()),
            session_id: Some(summary.session_id.clone()),
            session_file: summary.session_file.clone(),
            thinking,
        }) {
            return response_failure(None, "create", &error.to_string(), None);
        }
        // The engine renders this summary into the sender identity block
        // of worker-to-worker agent messages.
        if let Ok(summary_value) = serde_json::to_value(&summary) {
            self.engine.set_session_summary(summary_value);
        }
        // Seed the status line from the latest persisted verdict (a respawned
        // worker resumes with the pre-crash verdict).
        self.status_runner.seed_from_session();
        // Recovery journal writes must not happen while holding the core
        // lock: record_recovery locks the core to read the store.
        let _ = self.record_recovery(true, "create");
        let session_id = summary.session_id.clone();
        if let Some(registration) = &self.registration {
            registration.notify_session_created(session_id);
        }
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
        let compacting = core.compacting;
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
            activity: if streaming || compacting {
                "working"
            } else {
                "idle"
            }
            .to_string(),
            is_session_active: streaming || compacting || queued > 0,
            has_registered_cron_job: Some(false),
            last_activity_at,
            rlm_depth: Some(core.rlm_depth),
            active_session_id: Some(core.active_session_id.clone()),
            session_id: store
                .map(|s| s.session_id().to_string())
                .unwrap_or_default(),
            session_file: store.map(|s| s.path.to_string_lossy().to_string()),
            session_name: store.and_then(|s| s.session_name().map(str::to_string)),
            cwd: core.cwd.clone(),
            thinking_level: Some(
                self.engine
                    .effective_thinking_level()
                    .unwrap_or_else(|| "default".to_string()),
            ),
            is_streaming: streaming,
            is_compacting: compacting,
            is_bash_running: Some(false),
            attached_clients: core.attached_client_ids.len() as u32,
            message_count: store.map(|s| s.message_count()).unwrap_or(0) as u32,
            session_actions: self.snapshot_locked(core),
            streaming_message: None,
            created: store.map(|s| s.header.timestamp.clone()),
            modified,
            first_message: store.and_then(|s| s.first_message()),
            parent_session_path: store.and_then(|store| store.header.parent_session.clone()),
            parent_active_session_id: core.parent_active_session_id.clone(),
            parent_session_id: core.parent_session_id.clone(),
            rlm_child_id: core.rlm_child_id.clone(),
            usage,
            worker_state: Some("ready".to_string()),
            worker_pid: Some(std::process::id()),
            status_label: None,
            summary: None,
            task_state: None,
            model: None,
            runtime_kind: Some(core.runtime_kind.clone()),
            unfinished_action_count: Some(0),
        }
    }

    pub(crate) fn snapshot_locked(&self, core: &SessionCore) -> SessionActionSnapshot {
        session_snapshot(core)
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
        self.side_questions.abort_for_client(&client_id);
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
        let images = parse_prompt_images(payload);
        let (done_tx, done_rx) = oneshot::channel();
        let done = if wait { Some(done_tx) } else { None };
        let (snapshot, queued_behind_work) = {
            let mut core = self.core.lock().unwrap();
            // An idle session runs the prompt immediately: the lane is the
            // work hand-off, not a queue, so the projection did not change
            // (TS prompt admission with queueIfBusy=false never queues).
            let queued_behind_work = core.busy;
            match streaming_behavior {
                Some("steer") => core.steering.push_back(QueuedItem {
                    message: message.to_string(),
                    images: images.clone(),
                    done,
                }),
                // Plain prompts admitted while busy drain when the run goes
                // idle, like `queueIfBusy` prompt admission.
                _ => core.follow_up.push_back(QueuedItem {
                    message: message.to_string(),
                    images: images.clone(),
                    done,
                }),
            }
            let snapshot = self.snapshot_locked(&core);
            let lanes = queue_lanes(&core);
            let active_session_id = core.active_session_id.clone();
            drop(core);
            self.persist_queue_snapshot(&active_session_id, &lanes);
            (snapshot, queued_behind_work)
        };
        if queued_behind_work {
            let _ = self.emit_action_update(&snapshot);
        }
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
        let images = parse_prompt_images(payload);
        match lane {
            Lane::Steering => &mut core.steering,
            Lane::FollowUp => &mut core.follow_up,
        }
        .push_back(QueuedItem {
            message: message.to_string(),
            images,
            done: None,
        });
        let snapshot = self.snapshot_locked(&core);
        let lanes = queue_lanes(&core);
        let active_session_id = core.active_session_id.clone();
        drop(core);
        self.persist_queue_snapshot(&active_session_id, &lanes);
        let _ = self.emit_action_update(&snapshot);
        self.work_notify.notify_one();
        let command = if lane == Lane::Steering {
            "steer"
        } else {
            "follow_up"
        };
        response_success(None, command, Some(json!({ "queued": true })))
    }

    /// Agent-to-agent message delivery, routed by the supervisor's
    /// `send_message` arm: render the `[agent-message from ...]` prompt and
    /// queue it on the requested lane. Answers with the delivery receipt
    /// (`createAgentSessionMessageReceipt` shape): `queued` when a turn is
    /// running (`queueIfBusy` semantics), `delivered` when the prompt
    /// becomes the next run.
    fn handle_worker_deliver_message(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("worker_deliver_message") {
            return response;
        }
        let message = payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Err(error) =
            pa_core::session_engine::agent_messaging::normalize_agent_session_message(message)
        {
            return response_failure(None, "worker_deliver_message", &error.to_string(), None);
        }
        let sender = payload.get("sender").cloned().unwrap_or(Value::Null);
        // Sender label precedence (TS `createAgentSessionMessagePrompt`):
        // session name, session id, active session id, client id.
        let sender_name = ["sessionName", "sessionId", "activeSessionId", "clientId"]
            .iter()
            .find_map(|key| sender.get(*key).and_then(Value::as_str))
            .unwrap_or("unknown")
            .to_string();
        let from_relationship = match sender.get("runtimeKind").and_then(Value::as_str) {
            Some("subagent") => Some(AgentFamilyRelationship::Child),
            _ => None,
        };
        let prompt = pa_core::session_engine::agent_messaging::create_agent_session_message_prompt(
            &AgentMessagePromptPayload {
                message: message.to_string(),
                sender_name,
                from_relationship,
            },
        );
        let lane = if payload.get("deliveryMode").and_then(Value::as_str) == Some("follow_up") {
            Lane::FollowUp
        } else {
            Lane::Steering
        };
        let (id, summary, queued, snapshot, lanes, active_session_id) = {
            let mut core = self.core.lock().unwrap();
            let pending = core.steering.len() + core.follow_up.len();
            if let Err(error) =
                pa_core::session_engine::agent_messaging::assert_agent_message_queue_capacity(
                    pending,
                    DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION,
                )
            {
                drop(core);
                return response_failure(None, "worker_deliver_message", &error.to_string(), None);
            }
            let id = pa_core::session_engine::agent_messaging::create_agent_session_message_id();
            match lane {
                Lane::Steering => &mut core.steering,
                Lane::FollowUp => &mut core.follow_up,
            }
            .push_back(QueuedItem {
                message: prompt,
                images: Vec::new(),
                done: None,
            });
            let queued = core.busy;
            let summary = self.summary_locked(&core);
            let snapshot = self.snapshot_locked(&core);
            let lanes = queue_lanes(&core);
            let active_session_id = core.active_session_id.clone();
            (id, summary, queued, snapshot, lanes, active_session_id)
        };
        self.persist_queue_snapshot(&active_session_id, &lanes);
        let _ = self.emit_action_update(&snapshot);
        self.work_notify.notify_one();
        let mut target = json!({
            "activeSessionId": summary.active_session_id.clone().unwrap_or_default(),
            "sessionId": summary.session_id,
            "runtimeKind": summary
                .runtime_kind
                .clone()
                .unwrap_or_else(|| "top-level".to_string()),
        });
        if let Some(name) = summary.session_name.clone().filter(|name| !name.is_empty()) {
            target["sessionName"] = json!(name);
        }
        let timestamp = crate::util::now_iso();
        let mut receipt = json!({
            "id": id,
            "source": AGENT_MESSAGE_SOURCE,
            "target": target,
            "message": message,
            // TS receipts always report `steer`; the follow-up lane is the
            // Rust extension for queue-behind-current-work delivery.
            "deliveryMode": if lane == Lane::FollowUp { "follow_up" } else { "steer" },
        });
        if queued {
            receipt["deliveryStatus"] = json!("queued");
            receipt["queuedAt"] = json!(timestamp);
        } else {
            receipt["deliveryStatus"] = json!("delivered");
            receipt["deliveredAt"] = json!(timestamp);
        }
        if !sender.is_null() {
            receipt["from"] = json!(sender);
        }
        response_success(None, "worker_deliver_message", Some(receipt))
    }

    /// `update_snapshot` (supervisor plane, update flow spec §8): a
    /// read-only capture of this session for the update roster. The worker
    /// persists its queue lanes to the recovery journal BEFORE replying, so
    /// the reported queue and the durable respawn state agree; the snapshot
    /// itself freezes nothing — a busy session keeps running (the supervisor
    /// gate already fences new mutations, and the graceful-stop budget owns
    /// the exit).
    ///
    /// In-flight granularity: the Rust engine exposes `busy` (a turn in
    /// flight) and `compacting` only; provider streaming, tool/bash work,
    /// and retries all live inside a busy turn and are reported through it
    /// (the roster's `bash_running`/`retrying`/`prompt_in_flight` flags are
    /// false on this build for that reason — restore treats `busy` as the
    /// continuation signal).
    fn handle_update_snapshot(&self) -> DaemonResponse {
        let (core_data, lanes) = {
            let core = self.core.lock().unwrap();
            let store = core.store.as_ref();
            let data = json!({
                "activeSessionId": core.active_session_id,
                "sessionId": store.map(|s| s.session_id()).unwrap_or_default(),
                "sessionFile": core
                    .store
                    .as_ref()
                    .map(|s| s.path.to_string_lossy().to_string()),
                "cwd": core.cwd,
                "generation": core.generation,
                "runtimeMetadata": {
                    "kind": core.runtime_kind,
                    "rlmChildId": core.rlm_child_id,
                    "parentSessionId": core.parent_session_id,
                    "rlmDepth": core.rlm_depth,
                },
                "queue": {
                    "actions": serde_json::to_value(session_snapshot(&core)).ok(),
                    "steering": core.steering.iter().map(|item| item.message.clone()).collect::<Vec<_>>(),
                    "followUps": core.follow_up.iter().map(|item| item.message.clone()).collect::<Vec<_>>(),
                },
                "busy": core.busy,
                "compacting": core.compacting,
            });
            (data, queue_lanes(&core))
        };
        // Journal the lanes after releasing the core lock (record paths take
        // the locks in the opposite order).
        self.persist_queue_snapshot(
            core_data["activeSessionId"].as_str().unwrap_or_default(),
            &lanes,
        );
        response_success(None, "update_snapshot", Some(core_data))
    }

    /// Graceful stop: the connection loop exits the process after replying.
    /// The session's telemetry finalizes first (TS dispose callback:
    /// `agent session ended` + one flush), bounded by the sink timeouts.
    async fn handle_shutdown(&self) -> DaemonResponse {
        {
            let mut core = self.core.lock().unwrap();
            core.shutdown_requested = true;
            core.abort_requested = true;
        }
        self.work_notify.notify_one();
        self.engine.end_telemetry().await;
        response_success(None, "shutdown", None)
    }

    fn handle_abort(&self) -> DaemonResponse {
        let mut core = self.core.lock().unwrap();
        core.abort_requested = true;
        response_success(None, "abort", None)
    }

    /// `compact` (TS handler): run one compaction and answer with the TS
    /// `CompactionResult` wire shape; skips, aborts, and failures answer
    /// with the session's error message exactly like the TS daemon catch.
    async fn handle_compaction(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("compact") {
            return response;
        }
        let custom_instructions = payload
            .get("customInstructions")
            .and_then(Value::as_str)
            .map(str::to_string);
        let outcome = self
            .compaction
            .run(custom_instructions, &self.idle_notify)
            .await;
        match outcome {
            crate::engine::CompactionOutcome::Compacted { run } => {
                response_success(None, "compact", Some(run.result))
            }
            crate::engine::CompactionOutcome::Skipped { message } => {
                response_failure(None, "compact", &message, None)
            }
            crate::engine::CompactionOutcome::Aborted => {
                response_failure(None, "compact", "Compaction cancelled", None)
            }
            crate::engine::CompactionOutcome::Failed { error } => {
                response_failure(None, "compact", &error, None)
            }
        }
    }

    /// `set_auto_compaction` (TS handler): update the connection state and
    /// answer success without data.
    fn handle_set_auto_compaction(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("set_auto_compaction") {
            return response;
        }
        let enabled = payload
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.compaction.set_auto_compaction(enabled);
        response_success(None, "set_auto_compaction", None)
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

    /// `wait_for_headless_completion` (TS daemon command): settle the
    /// headless run first (same idle wait as `wait_for_idle`), then answer
    /// the autonomous-run accounting snapshot (`DaemonAutonomousStatus`).
    async fn handle_wait_for_headless_completion(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("wait_for_headless_completion") {
            return response;
        }
        loop {
            {
                let core = self.core.lock().unwrap();
                if !core.busy && core.steering.is_empty() && core.follow_up.is_empty() {
                    break;
                }
            }
            self.idle_notify.notified().await;
        }
        // The idle wait finished, so no turn holds the accounting state;
        // the snapshot read cannot interleave with a running turn.
        let status = self
            .engine
            .autonomous_status()
            .await
            .unwrap_or_else(pa_core::autonomous::disabled_autonomous_status);
        response_success(
            None,
            "wait_for_headless_completion",
            Some(serde_json::to_value(&status).unwrap_or(Value::Null)),
        )
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
        let lanes = queue_lanes(&core);
        let active_session_id = core.active_session_id.clone();
        drop(core);
        self.persist_queue_snapshot(&active_session_id, &lanes);
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

    async fn handle_kill(&self) -> DaemonResponse {
        self.side_questions.abort_all();
        // `session archived` (schema v1) + the session-ended finalization:
        // kill disposes the session like the TS dispose callback does.
        self.engine.archive_session_telemetry().await;
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
        drop(core);
        // The sender identity follows the live name.
        if let Ok(summary_value) = serde_json::to_value(&summary) {
            self.engine.set_session_summary(summary_value);
        }
        response_success(
            None,
            command,
            Some(serde_json::to_value(&summary).unwrap_or(Value::Null)),
        )
    }

    fn connection_state_locked(&self, core: &SessionCore) -> AgentConnectionState {
        let store = core.store.as_ref();
        AgentConnectionState {
            is_streaming: core.busy,
            is_compacting: core.compacting,
            active_session_id: Some(core.active_session_id.clone()),
            cwd: core.cwd.clone(),
            model: self.engine.model_metadata(),
            thinking_level: self
                .engine
                .effective_thinking_level()
                .unwrap_or_else(|| "default".to_string()),
            service_tier: "auto".to_string(),
            // The resolved model's supported levels (TS `getSupportedThinkingLevels`
            // in `getState`): a non-reasoning model reports ["off"], which the
            // client treats as no thinking surface.
            available_thinking_levels: self
                .engine
                .supported_thinking_levels()
                .unwrap_or_else(|| vec!["off".to_string()]),
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
            auto_compaction_enabled: core.auto_compaction_enabled,
            message_count: store.map(|s| s.message_count()).unwrap_or(0) as u32,
            session_actions: session_snapshot(core),
            compaction_count: store
                .map(|s| {
                    s.entries()
                        .iter()
                        .filter(|entry| entry.type_ == "compaction")
                        .count() as u32
                })
                .unwrap_or(0),
            goal: self.engine.goal_state_value(),
            scoped_models: Vec::new(),
            active_tool_names: Vec::new(),
            context_usage: None,
            recap: None,
        }
    }

    /// Persist the queue lanes to the worker recovery journal (crash-safe
    /// queue recovery; TS keeps session files free of daemon bookkeeping).
    /// Call after releasing the core lock: `record_recovery` takes the locks
    /// in the opposite order.
    pub(crate) fn persist_queue_snapshot(&self, active_session_id: &str, lanes: &QueueLanes) {
        let mut guard = self.recovery.lock().unwrap();
        let Some(journal) = guard.as_mut() else {
            return;
        };
        let _ = journal.record_queue_snapshot(active_session_id, &lanes.steering, &lanes.follow_up);
    }

    pub(crate) fn record_recovery(&self, busy: bool, operation: &str) -> Result<()> {
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
    pub(crate) fn emit_action_update(&self, snapshot: &SessionActionSnapshot) -> Result<()> {
        let mut core = self.core.lock().unwrap();
        // TS `_emitQueueUpdate`: an unchanged projection stays silent (an
        // empty queue before and after a turn is not an update).
        if core.last_action_snapshot.as_ref() == Some(snapshot) {
            return Ok(());
        }
        core.last_action_snapshot = Some(snapshot.clone());
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
        self.events.send(OutboundFrame::session_event(payload));
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
        self.events.send(OutboundFrame::session_event(payload));
        Ok(())
    }
}

/// The compaction cut budget the engine ran with
/// (`compaction.keepRecentTokens` from settings, TS default 20k): the
/// durable boundary re-cut in the turn callback must walk with the same
/// budget to pin the same cut.
fn keep_recent_tokens(cwd: &str, agent_dir: &std::path::Path) -> u64 {
    pa_core::settings::SettingsManager::create(cwd, agent_dir)
        .settings()
        .compaction
        .clone()
        .unwrap_or_default()
        .keep_recent_tokens
        .unwrap_or(pa_core::session_engine::compaction::DEFAULT_KEEP_RECENT_TOKENS)
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

/// RLM depth fields of a create payload: `(depth, max_depth)`. Values must
/// be non-negative integers that fit a u32; anything else fails the create
/// instead of silently truncating.
fn create_payload_rlm_depth(payload: &Value) -> Result<(u32, Option<u32>), String> {
    fn parse(payload: &Value, key: &str) -> Result<Option<u32>, String> {
        match payload.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .map(Some)
                .ok_or_else(|| format!("create {key} must be a non-negative integer")),
        }
    }
    let depth = parse(payload, "rlmDepth")?.unwrap_or(0);
    let max_depth = parse(payload, "rlmMaxDepth")?;
    Ok((depth, max_depth))
}

/// Creation prefix for a daemon-hosted session file (TS `createAgentSession`
/// in the worker process): fresh files record `model_change` (when the engine
/// resolves a model), `thinking_level_change`, and `service_tier_change`; a
/// reopened session records the thinking level and service tier only when no
/// earlier entry set them. The recorded thinking level is the engine's
/// effective one — the create-config flag (else settings default/medium)
/// clamped to the model's supported levels; engines without a model
/// resolution (the scripted harness) record "off".
fn append_creation_prefix(
    store: &mut SessionFile,
    engine: &dyn SessionEngine,
    agent_dir: &std::path::Path,
    cwd: &str,
    fresh: bool,
) {
    let has_thinking_entry = store
        .entries()
        .iter()
        .any(|entry| entry.type_ == "thinking_level_change");
    let has_service_tier_entry = store
        .entries()
        .iter()
        .any(|entry| entry.type_ == "service_tier_change");
    let thinking_level = engine
        .effective_thinking_level()
        .unwrap_or_else(|| "off".to_string());
    if fresh {
        if let Some((provider, model_id)) = engine.creation_model() {
            store.append_model_change(&provider, &model_id);
        }
        store.append_thinking_level_change(&thinking_level);
    } else if !has_thinking_entry {
        store.append_thinking_level_change(&thinking_level);
    }
    if fresh || !has_service_tier_entry {
        let settings = pa_core::settings::SettingsManager::create(cwd, agent_dir);
        let service_tier = settings.get_default_service_tier();
        store.append_entry(
            "service_tier_change",
            json!({ "serviceTier": service_tier }),
        );
    }
}

/// The pending queue lanes of a session (journal persistence payload).
pub(crate) struct QueueLanes {
    pub(crate) steering: Vec<String>,
    pub(crate) follow_up: Vec<String>,
}

/// Read the pending lanes off a locked core.
pub(crate) fn queue_lanes(core: &SessionCore) -> QueueLanes {
    QueueLanes {
        steering: core
            .steering
            .iter()
            .map(|item| item.message.clone())
            .collect(),
        follow_up: core
            .follow_up
            .iter()
            .map(|item| item.message.clone())
            .collect(),
    }
}

/// Queue snapshot restore from the worker recovery journal (crash/respawn
/// recovery): the latest persisted lanes for this session.
fn restore_queue_snapshot(
    journal: &WorkerRecoveryJournal,
    active_session_id: &str,
) -> (VecDeque<QueuedItem>, VecDeque<QueuedItem>) {
    let mut steering = VecDeque::new();
    let mut follow_up = VecDeque::new();
    fn pending(lanes: Vec<String>) -> VecDeque<QueuedItem> {
        // Images on a queued prompt do not survive the worker restart:
        // the recovery journal stores the message lanes as text (the TS
        // command-recovery journal keeps the same text-only shape).
        lanes
            .into_iter()
            .map(|message| QueuedItem {
                message,
                images: Vec::new(),
                done: None,
            })
            .collect()
    }
    if let Some((steering_lanes, follow_up_lanes)) =
        journal.latest_queue_snapshot(active_session_id)
    {
        steering = pending(steering_lanes);
        follow_up = pending(follow_up_lanes);
    }
    (steering, follow_up)
}

/// The turn runner: drains the queue one turn at a time, running the session
/// engine and emitting the agent-loop event lifecycle.
struct TurnRunner {
    pub(crate) core: Arc<Mutex<SessionCore>>,
    work_notify: Arc<Notify>,
    idle_notify: Arc<Notify>,
    events: Arc<EventPump>,
    engine: std::sync::Arc<dyn SessionEngine>,
    /// Shared worker recovery journal (queue snapshot persistence).
    recovery: Arc<Mutex<Option<WorkerRecoveryJournal>>>,
    active_session_id: String,
    status_notify: tokio::sync::mpsc::UnboundedSender<()>,
    /// The supervisor link for roster pushes (lazy reconnect like the
    /// agent-messaging link).
    roster_link: std::sync::Arc<crate::supervisor_link::SupervisorLink>,
    worker_token: String,
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
                // The busy flip reaches the supervisor's roster before the
                // turn runs (TS pushes the same transition).
                self.push_roster_delta();
                self.run_turn(engine, item).await;
            } else {
                self.idle_notify.notify_waiters();
                self.work_notify.notified().await;
            }
        }
    }

    /// Push one roster delta to the supervisor (the Rust-native form of
    /// the TS `roster_delta` worker frame): the worker's summary after a
    /// busy flip, so subscribed clients see live status without polling.
    /// Fire-and-forget: a dead link reconnects on the next flip, and a
    /// supervisor restart re-seeds the entry from registration.
    fn push_roster_delta(&self) {
        if std::env::var_os("PA_WORKER_DISABLE_ROSTER_PUSH").is_some() {
            return;
        }
        if self.worker_token.is_empty() || self.roster_link.socket_path().as_os_str().is_empty() {
            return;
        }
        let summary = {
            let core = self.core.lock().unwrap();
            session_summary(
                &core,
                &self
                    .engine
                    .effective_thinking_level()
                    .unwrap_or_else(|| "default".to_string()),
            )
        };
        let summary = serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null);
        let link = std::sync::Arc::clone(&self.roster_link);
        let worker_token = self.worker_token.clone();
        tokio::spawn(async move {
            let command = serde_json::json!({
                "type": "worker_roster_delta",
                "workerToken": worker_token,
                "summary": summary,
            });
            let _ = link
                .request(command, std::time::Duration::from_secs(10))
                .await;
        });
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
            images: item.images.clone(),
            source: "user".to_string(),
            agent_message_id: None,
        };
        // Live token-stream coalescing for this turn: the emit path parks
        // `message_update` frames in a single slot and a flusher task
        // broadcasts at most one parked snapshot per interval, while every
        // other frame goes out directly (flushing the parked update first,
        // so wire order matches event-sequence order exactly).
        let coalescer = {
            let core = self.core.lock().unwrap();
            Arc::new(crate::streaming::TurnStreamCoalescer::new(
                core.active_session_id.clone(),
                core.generation.clone(),
            ))
        };
        let flusher = {
            let coalescer = Arc::clone(&coalescer);
            let events = self.events.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(crate::streaming::UPDATE_FLUSH_INTERVAL).await;
                    if !coalescer.flush_pending(&events) {
                        break;
                    }
                }
            })
        };
        let engine = engine.clone();
        let core = Arc::clone(&self.core);
        let events = self.events.clone();
        let turn_coalescer = Arc::clone(&coalescer);
        let agent_dir = crate::paths::agent_dir().unwrap_or_default();
        let done = item.done;
        let turn = tokio::task::spawn_blocking(move || {
            let mut done = done;
            let mut emit = |mut event: EngineEvent| -> bool {
                // Sequence + persist under the core lock, then broadcast.
                // The abort flag lives on the session core (`abort` command):
                // a cancelled turn stops consuming its own events.
                let mut core = core.lock().unwrap();
                if core.abort_requested {
                    return false;
                }
                // The engine cuts its in-memory entries; its
                // `firstKeptEntryId` never matches this store's file ids,
                // so a verbatim copy retains nothing on the durable read.
                // Re-pin the boundary to the durable cut (TS: one store,
                // ids match by construction) before persist + broadcast.
                if let EngineEvent::Compaction {
                    ref mut entry,
                    event: ref mut payload,
                } = event
                {
                    if !entry.is_null() {
                        if let Some(id) = core.store.as_ref().and_then(|store| {
                            store.durable_first_kept_entry_id(keep_recent_tokens(
                                &core.cwd, &agent_dir,
                            ))
                        }) {
                            entry["firstKeptEntryId"] = json!(id);
                            if let Some(result) =
                                payload.get_mut("result").and_then(Value::as_object_mut)
                            {
                                result.insert("firstKeptEntryId".to_string(), json!(id));
                            }
                        }
                    }
                }
                match &event {
                    EngineEvent::UserMessage(message) | EngineEvent::AssistantMessage(message) => {
                        if let Some(store) = core.store.as_mut() {
                            let _ = store.persist_entry("message", json!({ "message": message }));
                        }
                    }
                    // The session-file form of a tool result: a `message`
                    // entry with the `role: "toolResult"` payload (TS
                    // `_processAgentEvent` appendMessage path).
                    EngineEvent::ToolResultMessage(message) => {
                        if let Some(store) = core.store.as_mut() {
                            let _ = store.persist_entry("message", json!({ "message": message }));
                        }
                    }
                    // The session-file form of a custom row (TS
                    // `appendCustomMessageEntry`: customType/content/display/
                    // details fields on a `custom_message` entry).
                    EngineEvent::CustomMessage(message) => {
                        if let Some(store) = core.store.as_mut() {
                            let _ = store.persist_entry(
                                "custom_message",
                                json!({
                                    "customType": message.get("customType").cloned().unwrap_or(Value::Null),
                                    "content": message.get("content").cloned().unwrap_or(Value::Null),
                                    "display": message.get("display").cloned().unwrap_or(Value::Bool(true)),
                                    "details": message.get("details").cloned().unwrap_or(Value::Null),
                                }),
                            );
                        }
                    }
                    EngineEvent::Compaction { entry, .. } => {
                        // A skipped compaction carries a null entry (the
                        // skip shape): publish the event, never persist it.
                        if let Some(store) = core.store.as_mut().filter(|_| !entry.is_null()) {
                            let _ = store.persist_entry("compaction", entry.clone());
                        }
                    }
                    _ => {}
                }
                let done_result = if let EngineEvent::Done(result) = &event {
                    Some(result.clone())
                } else {
                    None
                };
                // One event may map to several wire frames (a custom row
                // is a message_start + message_end pair).
                let frames: Vec<Value> = match event {
                    EngineEvent::UserMessage(message) => {
                        // TS emits the accepted user message as a
                        // message_start + message_end pair (the row is
                        // complete the moment it is accepted).
                        vec![
                            json!({ "type": "message_start", "message": message }),
                            json!({ "type": "message_end", "message": message }),
                        ]
                    }
                    EngineEvent::AssistantUpdate {
                        message,
                        stream_event,
                    } => {
                        // A provider `start` begins a new assistant message;
                        // later stream events update it (TS message_start vs
                        // message_update).
                        let starts_message = stream_event
                            .as_ref()
                            .and_then(|event| event.get("type"))
                            .and_then(Value::as_str)
                            == Some("start");
                        let mut event = json!({
                            "type": if starts_message { "message_start" } else { "message_update" },
                            "message": message,
                        });
                        if let Some(stream_event) = stream_event {
                            event["assistantMessageEvent"] = stream_event;
                        }
                        vec![event]
                    }
                    EngineEvent::AssistantMessage(message) => {
                        vec![json!({ "type": "message_end", "message": message })]
                    }
                    EngineEvent::ToolExecutionStart {
                        tool_call_id,
                        tool_name,
                        args,
                    } => vec![json!({
                        "type": "tool_execution_start",
                        "toolCallId": tool_call_id,
                        "toolName": tool_name,
                        "args": args,
                    })],
                    EngineEvent::ToolExecutionUpdate {
                        tool_call_id,
                        partial_result,
                    } => vec![json!({
                        "type": "tool_execution_update",
                        "toolCallId": tool_call_id,
                        "partialResult": partial_result,
                    })],
                    EngineEvent::ToolExecutionEnd {
                        tool_call_id,
                        result,
                        is_error,
                    } => vec![json!({
                        "type": "tool_execution_end",
                        "toolCallId": tool_call_id,
                        "result": result,
                        "isError": is_error,
                    })],
                    EngineEvent::ToolResultMessage(message) => vec![
                        json!({ "type": "message_start", "message": message }),
                        json!({ "type": "message_end", "message": message }),
                    ],
                    EngineEvent::CustomMessage(message) => vec![
                        json!({ "type": "message_start", "message": message }),
                        json!({ "type": "message_end", "message": message }),
                    ],
                    EngineEvent::CompactionStart { event } => vec![event.clone()],
                    EngineEvent::Compaction { event, .. } => vec![event.clone()],
                    EngineEvent::GoalUpdate { goal } => vec![json!({
                        "type": "goal_update",
                        "goal": goal,
                    })],
                    EngineEvent::Done(Ok(())) => vec![json!({ "type": "turn_end" })],
                    EngineEvent::Done(Err(error)) => {
                        vec![json!({ "type": "turn_end", "error": error })]
                    }
                    EngineEvent::AutoRetryStart {
                        attempt,
                        max_attempts,
                        delay_ms,
                        error_message,
                        reason,
                    } => {
                        let mut event = json!({
                            "type": "auto_retry_start",
                            "attempt": attempt,
                            "maxAttempts": max_attempts,
                            "delayMs": delay_ms,
                            "errorMessage": error_message,
                        });
                        match reason {
                            pa_core::session_engine::auto_retry::RetryStartReason::Quick => {}
                            pa_core::session_engine::auto_retry::RetryStartReason::Backup {
                                backup_model,
                            } => {
                                event["reason"] = json!("backup");
                                event["backupModel"] = json!(backup_model);
                            }
                        }
                        vec![event]
                    }
                    EngineEvent::AutoRetryEnd {
                        success,
                        attempt,
                        final_error,
                        restored_model,
                    } => {
                        let mut event = json!({
                            "type": "auto_retry_end",
                            "success": success,
                            "attempt": attempt,
                        });
                        if let Some(final_error) = final_error {
                            event["finalError"] = json!(final_error);
                        }
                        if let Some(restored_model) = restored_model {
                            event["restoredModel"] = json!(restored_model);
                        }
                        vec![event]
                    }
                };
                // Verification seam: dump the emitted session events for
                // harness debugging (PA_DAEMON_EVENT_LOG=<path>).
                if let Ok(path) = std::env::var("PA_DAEMON_EVENT_LOG") {
                    use std::io::Write;
                    if let Ok(mut file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                    {
                        for frame in &frames {
                            let _ = writeln!(file, "{}", frame);
                        }
                    }
                }
                let mut direct_payloads: Vec<Vec<u8>> = Vec::new();
                for event_json in frames {
                    let is_stream_update =
                        event_json.get("type").and_then(Value::as_str) == Some("message_update");
                    // A block-end stream event (`text_end` and friends)
                    // settles the parked delta run: it must supersede
                    // nothing, so it travels direct (flushing the parked
                    // update first, in order).
                    let stream_kind = event_json
                        .get("assistantMessageEvent")
                        .and_then(|event| event.get("type"))
                        .and_then(Value::as_str);
                    let flushes_pending = matches!(
                        stream_kind,
                        Some("text_end") | Some("thinking_end") | Some("toolcall_end")
                    );
                    if is_stream_update && !flushes_pending {
                        let sequence = core.last_event_sequence + 1;
                        core.last_event_sequence = sequence;
                        // Streaming updates park in the coalescer (the
                        // newest full-partial snapshot wins, the delta run
                        // merges); `park_update` only returns false after
                        // the turn joined, which cannot race this closure.
                        let delta = event_json
                            .get("assistantMessageEvent")
                            .and_then(|event| event.get("delta"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let parked = turn_coalescer.park_update(
                            event_json.get("message").cloned().unwrap_or(Value::Null),
                            stream_kind.unwrap_or_default(),
                            delta,
                            sequence,
                        );
                        if !parked {
                            return false;
                        }
                        continue;
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
                        active_session_id: core.active_session_id.clone(),
                        event: event_json,
                        meta: Some(meta),
                        rest: Default::default(),
                    };
                    let payload = serde_json::to_vec(&outbound).unwrap_or_default();
                    direct_payloads.push(payload);
                }
                drop(core);
                // A batch that carries direct frames goes out immediately
                // (flushing the parked update first, preserving
                // event-sequence order); a pure-update batch leaves its
                // frame parked for the flusher.
                if !direct_payloads.is_empty() {
                    turn_coalescer.send_direct(&direct_payloads, &events);
                }
                // Resolve `done` only after the turn's final frames are on
                // the pump: the waiting response must observe their
                // sequences (see `ConnectionSink`), so the response cannot
                // be written before the turn's own events.
                if let Some(result) = done_result {
                    if let Some(done) = done.take() {
                        let _ = done.send(result);
                    }
                }
                true
            };
            let aborted_probe = {
                let core = Arc::clone(&core);
                move || core.lock().unwrap().abort_requested
            };
            engine.run_prompt(prompt_index, request, &aborted_probe, &mut emit);
        });
        let _ = turn.await;
        // The turn's emit path is joined: nothing parks from here on, a
        // stale parked partial must not surface after the settle events,
        // and the flusher task stops on its next tick.
        coalescer.close();
        flusher.abort();

        {
            let mut core = self.core.lock().unwrap();
            core.busy = false;
        }
        self.push_roster_delta();
        self.emit_turn_event(json!({ "type": "agent_end" }));
        let snapshot = {
            let core = self.core.lock().unwrap();
            self.snapshot_from(&core)
        };
        let (lanes, lane_session_id) = {
            let core = self.core.lock().unwrap();
            (queue_lanes(&core), core.active_session_id.clone())
        };
        {
            let mut guard = self.recovery.lock().unwrap();
            if let Some(journal) = guard.as_mut() {
                let _ = journal.record_queue_snapshot(
                    &lane_session_id,
                    &lanes.steering,
                    &lanes.follow_up,
                );
            }
        }
        let _ = self.emit_action_update(&snapshot);
        // A finished turn is the cue to refresh the session's status line
        // (the runner debounces a burst into one request).
        let _ = self.status_notify.send(());
        self.idle_notify.notify_waiters();
    }

    /// The post-turn queue projection (TS `_emitQueueUpdate`): an unchanged
    /// snapshot stays silent.
    fn emit_action_update(&self, snapshot: &SessionActionSnapshot) -> Result<()> {
        let mut core = self.core.lock().unwrap();
        if core.last_action_snapshot.as_ref() == Some(snapshot) {
            return Ok(());
        }
        core.last_action_snapshot = Some(snapshot.clone());
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
        self.events.send(OutboundFrame::session_event(payload));
        Ok(())
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
        self.events.send(OutboundFrame::session_event(payload));
    }
}

/// Entry point for the worker process.
pub async fn run_worker() -> Result<()> {
    if std::env::var(WORKER_ROLE_ENV).unwrap_or_default() != "1" {
        return Err(anyhow!("worker mode requires {WORKER_ROLE_ENV}=1"));
    }
    let config = WorkerConfig::from_env()?;
    // Self-registration: the supervisor's roster survives its own restarts
    // because workers re-present their identity (liveness watch + backoff).
    let registration = crate::registration::start(&config);
    let worker = Arc::new(Worker::new(config, registration));
    worker.serve().await
}

/// The session summary for one core (TS `summaryForActiveSession`): the
/// shared shape `get_state`, the roster, and list rows all serve. Free so
/// the turn runner can push roster deltas without the worker handle; the
/// thinking level rides in from the engine (the core has no engine access).
fn session_summary(core: &SessionCore, thinking_level: &str) -> SessionSummary {
    let store = core.store.as_ref();
    let streaming = core.busy;
    let compacting = core.compacting;
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
        activity: if streaming || compacting {
            "working"
        } else {
            "idle"
        }
        .to_string(),
        is_session_active: streaming || compacting || queued > 0,
        has_registered_cron_job: Some(false),
        last_activity_at,
        rlm_depth: Some(core.rlm_depth),
        active_session_id: Some(core.active_session_id.clone()),
        session_id: store
            .map(|s| s.session_id().to_string())
            .unwrap_or_default(),
        session_file: store.map(|s| s.path.to_string_lossy().to_string()),
        session_name: store.and_then(|s| s.session_name().map(str::to_string)),
        cwd: core.cwd.clone(),
        thinking_level: Some(thinking_level.to_string()),
        is_streaming: streaming,
        is_compacting: compacting,
        is_bash_running: Some(false),
        attached_clients: core.attached_client_ids.len() as u32,
        message_count: store.map(|s| s.message_count()).unwrap_or(0) as u32,
        session_actions: session_snapshot(core),
        streaming_message: None,
        created: store.map(|s| s.header.timestamp.clone()),
        modified,
        first_message: store.and_then(|s| s.first_message()),
        parent_session_path: store.and_then(|store| store.header.parent_session.clone()),
        parent_active_session_id: core.parent_active_session_id.clone(),
        parent_session_id: core.parent_session_id.clone(),
        rlm_child_id: core.rlm_child_id.clone(),
        usage,
        worker_state: Some("ready".to_string()),
        worker_pid: Some(std::process::id()),
        status_label: None,
        summary: None,
        task_state: None,
        model: None,
        runtime_kind: Some(core.runtime_kind.clone()),
        unfinished_action_count: Some(0),
    }
}

/// The queue snapshot for one core (TS `sessionActions`).
fn session_snapshot(core: &SessionCore) -> SessionActionSnapshot {
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

#[cfg(test)]
mod update_snapshot_tests {
    use super::*;

    async fn snapshot_after_create() -> (Arc<Worker>, DaemonResponse) {
        let dir = std::env::temp_dir().join(format!("pa-worker-us-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = WorkerConfig {
            socket_path: dir.join("worker.sock"),
            supervisor_socket_path: PathBuf::new(),
            token: "token".to_string(),
            worker_instance_id: String::new(),
            active_session_id: "target-session".to_string(),
            agent_dir: dir.join("agent"),
            recovery_journal_path: dir.join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({ "responses": ["ack"] })),
        };
        let worker = Arc::new(Worker::new(config, None));
        // The journal is opened in `serve()`; tests open it directly so the
        // snapshot flush has the same durable sink as production.
        *worker.recovery.lock().unwrap() =
            Some(WorkerRecoveryJournal::open(&worker.config.recovery_journal_path).unwrap());
        let created = worker
            .dispatch(
                "create",
                &json!({ "noSession": true, "cwd": "/tmp", "name": "target" }),
            )
            .await;
        assert!(created.success, "create must succeed: {created:?}");
        let response = worker.dispatch("update_snapshot", &json!({})).await;
        (worker, response)
    }

    #[tokio::test]
    async fn update_snapshot_reports_the_session_and_flushes_the_journal() {
        let (worker, response) = snapshot_after_create().await;
        assert!(response.success, "snapshot must succeed: {response:?}");
        let data = response.data.expect("snapshot data");
        // The no-session worker has no durable session file: the active id
        // still identifies the worker's session.
        assert_eq!(data["activeSessionId"], "target-session");
        assert_eq!(data["cwd"], "/tmp");
        assert_eq!(data["busy"], false);
        assert_eq!(data["compacting"], false);
        assert_eq!(data["runtimeMetadata"]["kind"], "top-level");
        assert!(data["queue"]["actions"].is_object());
        // The flush happened before the reply: the recovery journal has a
        // queue snapshot record for this session.
        let snapshot = WorkerRecoveryJournal::read_queue_snapshot(
            &worker.config.recovery_journal_path,
            "target-session",
        )
        .expect("journal is readable");
        assert!(snapshot.is_some(), "the queue lanes were flushed");
    }

    #[tokio::test]
    async fn update_snapshot_reflects_queued_work() {
        let (worker, _) = snapshot_after_create().await;
        worker
            .dispatch("steer", &json!({ "message": "finish the build" }))
            .await;
        let response = worker.dispatch("update_snapshot", &json!({})).await;
        let data = response.data.expect("snapshot data");
        assert_eq!(data["queue"]["steering"][0], "finish the build");
        assert_eq!(
            data["queue"]["actions"]["steering"][0], "finish the build",
            "the lane snapshot and the actions projection agree"
        );
    }
}

#[cfg(test)]
mod agent_message_tests {
    use super::*;

    fn test_worker() -> Arc<Worker> {
        let dir = std::env::temp_dir().join(format!("pa-worker-am-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = WorkerConfig {
            socket_path: dir.join("worker.sock"),
            supervisor_socket_path: PathBuf::new(),
            token: "token".to_string(),
            worker_instance_id: String::new(),
            active_session_id: "target-session".to_string(),
            agent_dir: dir.join("agent"),
            recovery_journal_path: dir.join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({ "responses": ["ack"] })),
        };
        Arc::new(Worker::new(config, None))
    }

    async fn created_worker() -> Arc<Worker> {
        let worker = test_worker();
        let created = worker
            .dispatch(
                "create",
                &json!({ "noSession": true, "cwd": "/tmp", "name": "target" }),
            )
            .await;
        assert!(created.success, "create failed: {created:?}");
        worker
    }

    fn queue_texts(core: &Mutex<SessionCore>, lane: Lane) -> Vec<String> {
        let core = core.lock().unwrap();
        match lane {
            Lane::Steering => &core.steering,
            Lane::FollowUp => &core.follow_up,
        }
        .iter()
        .map(|item| item.message.clone())
        .collect()
    }

    /// Receipt shape (`createAgentSessionMessageReceipt`): id, source,
    /// target endpoint, sender echo, delivered status and timestamp while
    /// the session is idle, and the rendered prompt on the steering lane.
    #[tokio::test]
    async fn deliver_message_answers_the_ts_receipt_shape() {
        let worker = created_worker().await;
        let response = worker
            .dispatch(
                "worker_deliver_message",
                &json!({
                    "targetActiveSessionId": "target-session",
                    "message": "ping from the first session",
                    "sender": {
                        "activeSessionId": "source-session",
                        "sessionId": "source-file",
                        "sessionName": "source-agent",
                        "runtimeKind": "top-level",
                        "clientId": "cli-1",
                    },
                }),
            )
            .await;
        assert!(response.success, "deliver failed: {response:?}");
        assert_eq!(response.command, "worker_deliver_message");
        let data = response.data.expect("receipt data");
        assert!(
            data["id"]
                .as_str()
                .unwrap_or_default()
                .starts_with("agentmsg_"),
            "receipt id: {data}"
        );
        assert_eq!(data["source"], "agent_message");
        assert_eq!(data["message"], "ping from the first session");
        assert_eq!(data["deliveryStatus"], "delivered");
        assert_eq!(data["deliveryMode"], "steer");
        assert!(
            data["deliveredAt"].as_str().is_some(),
            "deliveredAt: {data}"
        );
        assert!(
            data.get("queuedAt").is_none(),
            "queuedAt on delivery: {data}"
        );
        assert_eq!(data["target"]["activeSessionId"], "target-session");
        assert_eq!(data["target"]["sessionName"], "target");
        assert!(!data["target"]["sessionId"]
            .as_str()
            .unwrap_or_default()
            .is_empty());
        assert_eq!(data["from"]["sessionName"], "source-agent");
        assert_eq!(
            queue_texts(&worker.core, Lane::Steering),
            vec!["[agent-message from source-agent]\n\nping from the first session"],
            "steering lane"
        );
        assert!(queue_texts(&worker.core, Lane::FollowUp).is_empty());
    }

    /// An explicit `follow_up` delivery mode queues behind current work
    /// instead of steering, and a subagent sender renders the relationship.
    #[tokio::test]
    async fn deliver_message_follow_up_lane_and_subagent_sender() {
        let worker = created_worker().await;
        let response = worker
            .dispatch(
                "worker_deliver_message",
                &json!({
                    "targetActiveSessionId": "target-session",
                    "message": "queue me",
                    "sender": {
                        "activeSessionId": "source-session",
                        "sessionName": "source-agent",
                        "runtimeKind": "subagent",
                    },
                    "deliveryMode": "follow_up",
                }),
            )
            .await;
        assert!(response.success, "deliver failed: {response:?}");
        let data = response.data.expect("receipt data");
        assert_eq!(data["deliveryMode"], "follow_up");
        assert_eq!(
            queue_texts(&worker.core, Lane::FollowUp),
            vec!["[agent-message from child:source-agent]\n\nqueue me"],
            "follow-up lane"
        );
        assert!(queue_texts(&worker.core, Lane::Steering).is_empty());
    }

    /// A busy session reports `queued` with `queuedAt` (`queueIfBusy`).
    #[tokio::test]
    async fn deliver_message_while_busy_queues() {
        let worker = created_worker().await;
        worker.core.lock().unwrap().busy = true;
        let response = worker
            .dispatch(
                "worker_deliver_message",
                &json!({
                    "targetActiveSessionId": "target-session",
                    "message": "while busy",
                    "sender": { "activeSessionId": "source-session" },
                }),
            )
            .await;
        assert!(response.success, "deliver failed: {response:?}");
        let data = response.data.expect("receipt data");
        assert_eq!(data["deliveryStatus"], "queued");
        assert!(data["queuedAt"].as_str().is_some(), "queuedAt: {data}");
        assert!(
            data.get("deliveredAt").is_none(),
            "deliveredAt while queued: {data}"
        );
    }

    /// The pending-capacity guard fails with the TS error string.
    #[tokio::test]
    async fn deliver_message_respects_the_pending_capacity() {
        let worker = created_worker().await;
        {
            let mut core = worker.core.lock().unwrap();
            for _ in 0..DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION {
                core.follow_up.push_back(QueuedItem {
                    message: "occupied".to_string(),
                    images: Vec::new(),
                    done: None,
                });
            }
        }
        let response = worker
            .dispatch(
                "worker_deliver_message",
                &json!({
                    "targetActiveSessionId": "target-session",
                    "message": "over the limit",
                    "sender": { "activeSessionId": "source-session" },
                }),
            )
            .await;
        assert!(!response.success, "deliver should fail: {response:?}");
        assert_eq!(
            response.error.as_deref(),
            Some("Target session has too many pending messages: 20 unfinished, limit is 20")
        );
    }
}

#[cfg(test)]
mod prompt_image_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_wire_images_and_drops_incomplete_entries() {
        let payload = json!({
            "message": "look",
            "images": [
                { "type": "image", "data": "QUJD", "mimeType": "image/png" },
                { "type": "image", "mimeType": "image/png" },
                { "type": "image", "data": "QQ==" },
                { "type": "text", "text": "not an image" }
            ]
        });
        let images = parse_prompt_images(&payload);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].data, "QUJD");
        assert_eq!(images[0].mime_type, "image/png");
    }

    #[test]
    fn missing_or_empty_images_admit_text_only() {
        assert!(parse_prompt_images(&json!({ "message": "plain" })).is_empty());
        assert!(parse_prompt_images(&json!({ "images": [] })).is_empty());
        assert!(parse_prompt_images(&json!({ "images": null })).is_empty());
    }

    fn test_worker() -> Arc<Worker> {
        let dir = std::env::temp_dir().join(format!("pa-worker-img-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = WorkerConfig {
            socket_path: dir.join("worker.sock"),
            supervisor_socket_path: PathBuf::new(),
            token: "token".to_string(),
            worker_instance_id: String::new(),
            active_session_id: "target-session".to_string(),
            agent_dir: dir.join("agent"),
            recovery_journal_path: dir.join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({ "responses": ["ack"] })),
        };
        Arc::new(Worker::new(config, None))
    }

    /// A `prompt` command with wire images queues the attachments with the
    /// message (they ride the queue item into the engine as multimodal
    /// user content).
    #[tokio::test]
    async fn prompt_with_images_queues_the_images_with_the_message() {
        let worker = test_worker();
        let created = worker
            .dispatch(
                "create",
                &json!({ "noSession": true, "cwd": "/tmp", "name": "target" }),
            )
            .await;
        assert!(created.success, "create failed: {created:?}");
        // Busy session: the prompt lands on the follow-up lane.
        worker.core.lock().unwrap().busy = true;
        let response = worker
            .dispatch(
                "prompt",
                &json!({
                    "message": "look at this",
                    "images": [
                        { "type": "image", "data": "QUJD", "mimeType": "image/png" }
                    ],
                }),
            )
            .await;
        assert!(response.success, "prompt failed: {response:?}");
        let images = {
            let core = worker.core.lock().unwrap();
            core.follow_up
                .iter()
                .map(|item| item.images.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(images.len(), 1, "one queued item");
        assert_eq!(
            images[0],
            vec![pa_agent::types::ImageContent {
                data: "QUJD".to_string(),
                mime_type: "image/png".to_string(),
            }],
            "the attachment rides the queue item"
        );
    }
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
    fn queue_snapshot_round_trips_through_the_recovery_journal() {
        let dir = std::env::temp_dir().join(format!("pa-worker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let journal_path = dir.join("recovery.jsonl");
        let mut journal = WorkerRecoveryJournal::open(&journal_path).unwrap();
        journal
            .record_queue_snapshot(
                "session-a",
                &["steer-me".to_string()],
                &["follow-me".to_string()],
            )
            .unwrap();
        // A reopen (respawned worker) reads the latest snapshot per session.
        let reloaded = WorkerRecoveryJournal::open(&journal_path).unwrap();
        let (steering, follow_up) = restore_queue_snapshot(&reloaded, "session-a");
        assert_eq!(steering.len(), 1);
        assert_eq!(steering[0].message, "steer-me");
        assert_eq!(follow_up.len(), 1);
        assert_eq!(follow_up[0].message, "follow-me");
        // Compaction (triggered by an all-idle record) keeps the snapshot.
        let mut compacting = WorkerRecoveryJournal::open(&journal_path).unwrap();
        compacting
            .record("session-a", "s1", None, false, "idle")
            .unwrap();
        let compacted = WorkerRecoveryJournal::open(&journal_path).unwrap();
        let (steering, _) = restore_queue_snapshot(&compacted, "session-a");
        assert_eq!(steering.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod turn_stream_tests {
    use super::*;
    use crate::engine::{
        CompactionOutcome, CompactionRequest, PromptRequest, SessionEngine, SideQuestionOutcome,
        SideQuestionRequest,
    };

    /// One scripted turn that streams `deltas` partial-message updates
    /// (one full-snapshot `message_update` frame per provider delta, the
    /// wire shape a fast provider produces on a big turn) and settles
    /// with one final assistant message. `spacing_ms` paces the deltas so
    /// the flusher tick can interleave (the realistic case: a provider
    /// that outruns 20 updates/second).
    struct BurstStreamEngine {
        deltas: usize,
        spacing_ms: u64,
    }

    impl BurstStreamEngine {
        fn message_with(text: &str) -> Value {
            json!({
                "role": "assistant",
                "provider": "faux",
                "model": "faux-1",
                "content": [{ "type": "text", "text": text }],
            })
        }

        fn delta_text(&self, index: usize) -> String {
            "x".repeat((index + 1) * 4)
        }

        fn full_text(&self) -> String {
            self.delta_text(self.deltas)
        }
    }

    impl SessionEngine for BurstStreamEngine {
        fn run_prompt(
            &self,
            _prompt_index: usize,
            _request: PromptRequest,
            _aborted: &dyn Fn() -> bool,
            emit: &mut dyn FnMut(EngineEvent) -> bool,
        ) {
            for index in 0..=self.deltas {
                let message = Self::message_with(&self.delta_text(index));
                let stream_event = if index == 0 {
                    json!({ "type": "start" })
                } else {
                    json!({ "type": "text_delta", "delta": "xxxx" })
                };
                if !emit(EngineEvent::AssistantUpdate {
                    message,
                    stream_event: Some(stream_event),
                }) {
                    return;
                }
                if self.spacing_ms > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(self.spacing_ms));
                }
            }
            if !emit(EngineEvent::AssistantMessage(Self::message_with(
                &self.full_text(),
            ))) {
                return;
            }
            emit(EngineEvent::Done(Ok(())));
        }

        fn run_side_question(
            &self,
            _request: SideQuestionRequest,
            _signal: &pa_agent::abort::AbortSignal,
            _sink: &pa_core::session_engine::side_question::SideQuestionSink,
        ) -> SideQuestionOutcome {
            SideQuestionOutcome::Failed {
                answer: String::new(),
                error: "unsupported".to_string(),
            }
        }

        fn run_compaction(
            &self,
            _request: CompactionRequest,
            _signal: &pa_agent::abort::AbortSignal,
        ) -> CompactionOutcome {
            CompactionOutcome::Skipped {
                message: "nothing to compact".to_string(),
            }
        }
    }

    /// A minimal turn runner over a fresh session core: exactly what
    /// `run_turn` touches (the store stays `None`, the roster push is a
    /// no-op link, no supervisor socket).
    fn burst_runner(engine: Arc<dyn SessionEngine>) -> TurnRunner {
        let core = Arc::new(Mutex::new(SessionCore {
            active_session_id: "burst-session".to_string(),
            generation: "gen".to_string(),
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
            compacting: false,
            auto_compaction_enabled: true,
            last_action_snapshot: Some(SessionActionSnapshot::default()),
            rlm_depth: 0,
            runtime_kind: "top-level".to_string(),
            rlm_child_id: None,
            parent_active_session_id: None,
            parent_session_id: None,
        }));
        let (status_notify, _status_rx) = tokio::sync::mpsc::unbounded_channel();
        TurnRunner {
            core,
            work_notify: Arc::new(Notify::new()),
            idle_notify: Arc::new(Notify::new()),
            events: Arc::new(EventPump::new()),
            engine,
            recovery: Arc::new(Mutex::new(None)),
            active_session_id: "burst-session".to_string(),
            status_notify,
            roster_link: Arc::new(crate::supervisor_link::SupervisorLink::new(PathBuf::new())),
            worker_token: String::new(),
        }
    }

    /// Run one scripted turn and return its session-event frames in wire
    /// order.
    async fn turn_session_events(engine: Arc<dyn SessionEngine>) -> Vec<Value> {
        let runner = burst_runner(Arc::clone(&engine));
        let mut subscription = runner.events.subscribe();
        runner
            .run_turn(
                engine,
                QueuedItem {
                    message: "burst".to_string(),
                    images: Vec::new(),
                    done: None,
                },
            )
            .await;
        let mut events = Vec::new();
        while let Ok(frame) = subscription.try_recv() {
            if frame.outbound_type == "session_event" {
                if let Ok(outbound) = serde_json::from_slice::<Value>(&frame.payload) {
                    events.push(outbound["event"].clone());
                }
            }
        }
        events
    }

    fn positions_of(events: &[Value], frame_type: &str) -> Vec<usize> {
        events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.get("type").and_then(Value::as_str) == Some(frame_type))
            .map(|(index, _)| index)
            .collect()
    }

    fn texts_at(events: &[Value], positions: &[usize]) -> Vec<String> {
        positions
            .iter()
            .filter_map(|index| {
                events[*index]["message"]["content"][0]["text"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect()
    }

    /// A provider that outruns the flush tick still broadcasts at most one
    /// parked update per tick — never one wire frame per delta (the
    /// pre-fix path flooded the wire with every delta and the client
    /// starved at the tick rate; a 12k-token turn took minutes to render).
    #[tokio::test]
    async fn a_provider_burst_broadcasts_one_coalesced_update_per_tick_not_per_delta() {
        const DELTAS: usize = 120;
        // 1ms spacing: the burst spans ~120ms, so the 50ms flusher tick
        // flushes at most a handful of mid-burst snapshots.
        let engine = Arc::new(BurstStreamEngine {
            deltas: DELTAS,
            spacing_ms: 1,
        });
        let events = turn_session_events(engine).await;

        assert_eq!(
            positions_of(&events, "message_start").len(),
            1,
            "one message_start frame opens the stream"
        );
        let updates = positions_of(&events, "message_update");
        let end = positions_of(&events, "message_end");
        assert_eq!(end.len(), 1, "the turn settles with one message_end");
        assert!(
            !updates.is_empty(),
            "the parked snapshots must reach the wire"
        );
        assert!(
            updates.len() * 10 < DELTAS,
            "{DELTAS} spaced deltas must coalesce to a handful of wire updates, saw {}",
            updates.len()
        );
        // The latest snapshot wins: the flushed update carries the full
        // message so far, and superseded snapshots are dropped.
        assert_eq!(
            texts_at(&events, &updates).last().map(String::len),
            Some((DELTAS + 1) * 4),
            "the last flushed update must carry the full text"
        );
        // Event-sequence order: every update precedes the settle frame.
        assert!(
            updates.iter().all(|index| *index < end[0]),
            "a superseded snapshot must never follow message_end"
        );
    }

    /// An instant burst (the provider outruns the tick entirely) parks one
    /// snapshot at a time; the settle frame flushes the final snapshot
    /// before message_end, so the client sees the full message without a
    /// tick waiting period and nothing lands out of order.
    #[tokio::test]
    async fn an_instant_burst_flushes_the_final_snapshot_with_its_settle_frame() {
        const DELTAS: usize = 200;
        let engine = Arc::new(BurstStreamEngine {
            deltas: DELTAS,
            spacing_ms: 0,
        });
        let events = turn_session_events(engine).await;

        let updates = positions_of(&events, "message_update");
        let end = positions_of(&events, "message_end");
        assert_eq!(end.len(), 1, "the turn settles with one message_end");
        assert!(
            updates.len() <= 3,
            "an instant burst broadcasts at most the settle-flushed snapshot (a mid-burst tick race adds one per 50ms stall), saw {}",
            updates.len()
        );
        assert!(
            texts_at(&events, &updates)
                .iter()
                .any(|text| text.len() == (DELTAS + 1) * 4),
            "the flushed snapshot must carry the full message"
        );
        assert!(
            updates.iter().all(|index| *index < end[0]),
            "the flushed snapshot precedes message_end"
        );
    }
}
