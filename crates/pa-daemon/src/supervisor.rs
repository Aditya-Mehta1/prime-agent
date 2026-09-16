//! Supervisor runtime: one process spawning one worker per active session.
//!
//! Port of `modes/daemon/daemon-supervisor.ts`: the supervisor hosts no sessions
//! itself. Clients connect over a JSONL Unix socket; the supervisor spawns a
//! dedicated worker process per session, supervises it (restart with
//! exponential backoff, bounded attempts), persists worker descriptors so a
//! restarted supervisor can adopt or relaunch live sessions, and routes
//! commands and events between clients and workers (private-framed channel).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use pa_types::daemon::{
    DaemonCommand, DaemonOutbound, DaemonWorkerDescriptor, DaemonWorkerLifecycle,
    DurableDaemonCreateCommand, SnapshotPurpose,
};
use pa_types::platform::transport::{bind_transport, connect_transport, TransportStream};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::descriptor::{
    create_command_payload, load_descriptors, persist_supervisor_config, persist_worker,
    PersistedSupervisorConfig, SUPERVISOR_CONFIG_FILE_NAME,
};
use crate::framing::{write_frame, PrivateFrameReader, DEFAULT_PRIVATE_FRAME_LIMITS};
use crate::lease::is_process_alive;
use crate::paths;
use crate::protocol::{
    command_active_session_id, command_type_name, current_protocol_info,
    default_server_capabilities, parse_supervisor_command_line, response_failure, response_line,
    response_success, DaemonResponse, DaemonRuntimeIdentity, DAEMON_APP_VERSION, DAEMON_SCHEMA_ID,
    DAEMON_SCHEMA_REVISION,
};
use crate::registry::{ResidentWorker, SessionRegistry, WorkerRegistration, WorkerRequest};
use crate::session_store::{find_most_recent_session_for_cwd, list_sessions};
use crate::snapshot_stream::{attach_client_capabilities, stream_attach, wants_chunked};
use crate::worker::{
    WORKER_ACTIVE_SESSION_ID_ENV, WORKER_INSTANCE_ID_ENV, WORKER_RECOVERY_JOURNAL_ENV,
    WORKER_ROLE_ENV, WORKER_SCRIPT_ENV, WORKER_SOCKET_ENV, WORKER_SUPERVISOR_SOCKET_ENV,
    WORKER_TOKEN_ENV,
};
use crate::{socket, util};

const WORKER_SPAWN_CONNECT_TIMEOUT_MS: u64 = 15_000;
const ROUTE_TIMEOUT_MS: u64 = 30_000;
const LONG_ROUTE_TIMEOUT_MS: u64 = 600_000;
const MAX_CONSECUTIVE_FAILURES: u32 = 5;
const BASE_BACKOFF_MS: u64 = 250;
const MAX_BACKOFF_MS: u64 = 30_000;
const WORKER_CWD_ENV: &str = "PRIME_AGENT_INTERNAL_DAEMON_WORKER_CWD";

#[derive(Debug, Clone)]
pub struct SupervisorOptions {
    pub socket_path: PathBuf,
    pub agent_dir: PathBuf,
}

/// Which clients a worker outbound frame reaches.
#[derive(Debug, Clone)]
enum ClientRouting {
    /// Every connected client (e.g. `daemon_closing`).
    Broadcast,
    /// Clients attached to the session.
    AttachedSession { active_session_id: String },
}

pub struct Supervisor {
    options: SupervisorOptions,
    descriptor_dir: PathBuf,
    registry: SessionRegistry,
    /// Worker outbound frames, with their client routing.
    events: broadcast::Sender<(ClientRouting, Value)>,
    shutting_down: AtomicBool,
    /// Wakes the accept loop when [`Supervisor::begin_shutdown`] sets the
    /// flag: a listening socket blocks in `accept` until a client connects,
    /// so the shutdown must interrupt it for the process to exit.
    shutdown_notify: tokio::sync::Notify,
    log: paths::RotatingLog,
}

impl Supervisor {
    pub fn new(options: SupervisorOptions) -> Result<Self> {
        let descriptor_dir =
            crate::descriptor::descriptor_dir(&options.agent_dir, &options.socket_path);
        paths::ensure_dir(&descriptor_dir)?;
        persist_supervisor_config(
            &descriptor_dir.join(SUPERVISOR_CONFIG_FILE_NAME),
            &PersistedSupervisorConfig {
                version: 1,
                socket_path: options.socket_path.to_string_lossy().to_string(),
                default_session_dir: Some(
                    paths::sessions_dir(&options.agent_dir)
                        .to_string_lossy()
                        .to_string(),
                ),
            },
        )?;
        let (events, _) = broadcast::channel(4096);
        let log = paths::RotatingLog::new(paths::daemon_log_path(
            &options.socket_path,
            &options.agent_dir,
        ));
        Ok(Supervisor {
            options,
            descriptor_dir,
            registry: SessionRegistry::new(),
            events,
            shutting_down: AtomicBool::new(false),
            shutdown_notify: tokio::sync::Notify::new(),
            log,
        })
    }

    /// Bind the client socket, adopt or relaunch persisted workers, serve.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        socket::prepare_socket_path(&self.options.socket_path).await?;
        let listener = bind_transport(&self.options.socket_path)
            .await
            .with_context(|| {
                format!(
                    "bind supervisor socket {}",
                    self.options.socket_path.display()
                )
            })?;
        socket::restrict_socket_path(&self.options.socket_path);
        self.log
            .append(&format!("supervisor started pid {}", std::process::id()));

        // Descriptor adoption runs concurrently with the accept loop: a
        // supervisor restarted over live sessions must accept their
        // self-registrations immediately, not behind the whole descriptor
        // scan.
        {
            let supervisor = Arc::clone(&self);
            tokio::spawn(async move {
                supervisor.adopt_persisted_workers().await;
            });
        }

        while !self.shutting_down.load(Ordering::SeqCst) {
            let stream = tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        if self.shutting_down.load(Ordering::SeqCst) {
                            continue;
                        }
                        return Err(anyhow!("supervisor accept: {error}"));
                    }
                },
                // begin_shutdown fired: loop back and fall out of the loop.
                _ = self.shutdown_notify.notified() => continue,
            };
            let supervisor = Arc::clone(&self);
            tokio::spawn(async move {
                if let Err(error) = supervisor.handle_client(stream).await {
                    eprintln!("pa-daemon client connection error: {error:#}");
                }
            });
        }
        socket::cleanup_socket_path(
            &self.options.socket_path,
            socket::socket_identity(&self.options.socket_path),
        );
        Ok(())
    }

    fn log_line(&self, message: &str) {
        self.log.append(&format!("[{}] {message}", util::now_iso()));
    }

    /// Adopt or relaunch persisted workers, concurrently: one dead worker's
    /// relaunch (create replay) must not delay adopting live sessions.
    async fn adopt_persisted_workers(self: &Arc<Self>) {
        let descriptors = load_descriptors(&self.descriptor_dir, &self.options.socket_path);
        let mut tasks = Vec::new();
        for (path, descriptor) in descriptors {
            let supervisor = Arc::clone(self);
            tasks.push(tokio::spawn(async move {
                supervisor.adopt_persisted_worker(path, descriptor).await;
            }));
        }
        for task in tasks {
            let _ = task.await;
        }
    }

    /// Adopt one persisted worker descriptor. Serialized against worker
    /// self-registration by the per-worker adoption gate: whichever path
    /// arrives first (descriptor scan or live re-registration) builds the
    /// roster entry; the other one finds it present.
    async fn adopt_persisted_worker(
        self: &Arc<Self>,
        path: PathBuf,
        descriptor: crate::descriptor::WorkerDescriptor,
    ) {
        let worker_id = descriptor.worker_id.clone();
        let guard = self.registry.adoption_guard(&worker_id).await;
        if self.registry.get(&worker_id).await.is_some() {
            // The worker re-registered before the descriptor scan reached it.
            self.log_line(&format!(
                "session worker {worker_id} already registered; skipping descriptor adoption"
            ));
            return;
        }
        let socket_path = PathBuf::from(&descriptor.socket_path);
        let alive = socket::can_connect(&socket_path, Duration::from_millis(500)).await;
        let pid = descriptor.pid;
        let resident = ResidentWorker::new(worker_id.clone(), descriptor, path);
        let result = if alive {
            self.connect_worker(&resident).await
        } else {
            // Dead worker: relaunch from the durable create command. The
            // worker rehydrates the session store, restoring history and
            // the persisted queue snapshot.
            self.relaunch_worker(&resident).await.map(|_| ())
        };
        match result {
            Ok(()) => {
                self.registry.insert(Arc::clone(&resident)).await;
                self.spawn_monitor(Arc::clone(&resident), None, pid);
                self.log_line(&format!(
                    "adopted session worker {worker_id} (was alive: {alive})"
                ));
            }
            Err(error) => {
                self.log_line(&format!("could not adopt worker {worker_id}: {error:#}"));
            }
        }
        drop(guard);
    }

    /// Watch a worker process: on unexpected exit, restart with backoff.
    fn spawn_monitor(
        self: &Arc<Self>,
        resident: Arc<ResidentWorker>,
        child: Option<Child>,
        pid: u64,
    ) {
        let supervisor = Arc::clone(self);
        tokio::spawn(async move {
            supervisor.watch_worker(resident, child, pid).await;
        });
    }

    async fn watch_worker(
        self: Arc<Self>,
        resident: Arc<ResidentWorker>,
        mut child: Option<Child>,
        mut adopted_pid: u64,
    ) {
        loop {
            if let Some(mut child) = child.take() {
                let status = child.wait().await;
                if resident.intentional_stop.load(Ordering::SeqCst)
                    || self.shutting_down.load(Ordering::SeqCst)
                {
                    self.log_line(&format!(
                        "session worker {} stopped intentionally (status {status:?})",
                        resident.worker_id
                    ));
                    return;
                }
            } else {
                // Adopted worker: poll liveness (cannot wait on a foreign pid).
                loop {
                    if self.shutting_down.load(Ordering::SeqCst)
                        || resident.intentional_stop.load(Ordering::SeqCst)
                    {
                        return;
                    }
                    if !matches!(is_process_alive(adopted_pid as u32), Ok(true)) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                if resident.intentional_stop.load(Ordering::SeqCst)
                    || self.shutting_down.load(Ordering::SeqCst)
                {
                    return;
                }
            }
            let failures = resident.consecutive_failures.fetch_add(1, Ordering::SeqCst) + 1;
            if failures > MAX_CONSECUTIVE_FAILURES {
                let mut descriptor = resident.descriptor.lock().await;
                descriptor.lifecycle = DaemonWorkerLifecycle::Failed;
                descriptor.last_failure_at = Some(util::now_iso());
                let _ = persist_worker(&resident.descriptor_path, &descriptor);
                drop(descriptor);
                self.registry.remove(&resident.worker_id).await;
                self.log_line(&format!(
                    "session worker {} failed after {failures} consecutive failures",
                    resident.worker_id
                ));
                return;
            }
            let backoff_ms = (BASE_BACKOFF_MS << (failures - 1).min(7)).min(MAX_BACKOFF_MS);
            self.log_line(&format!(
                "session worker {} exited unexpectedly; restarting in {backoff_ms}ms (failure {failures}/{MAX_CONSECUTIVE_FAILURES})",
                resident.worker_id
            ));
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            match self.relaunch_worker(&resident).await {
                Ok(new_child) => {
                    resident.consecutive_failures.store(0, Ordering::SeqCst);
                    child = Some(new_child);
                }
                Err(error) => {
                    self.log_line(&format!(
                        "worker {} relaunch failed: {error:#}",
                        resident.worker_id
                    ));
                    child = None;
                    adopted_pid = 0;
                }
            }
        }
    }

    fn is_stopping(&self, resident: &Arc<ResidentWorker>) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
            || resident.intentional_stop.load(Ordering::SeqCst)
    }

    /// Spawn a fresh worker process, connect, and replay the durable create.
    async fn relaunch_worker(self: &Arc<Self>, resident: &Arc<ResidentWorker>) -> Result<Child> {
        if self.is_stopping(resident) {
            return Err(anyhow!("supervisor is shutting down"));
        }
        let child = self.spawn_worker_process(resident).await?;
        if let Err(error) = self.connect_worker(resident).await {
            // Never leave a spawned-but-unwired worker process behind.
            let mut child = child;
            let _ = child.start_kill();
            return Err(error);
        }
        let payload = {
            let descriptor = resident.descriptor.lock().await;
            create_command_payload(&descriptor.create_command)
        };
        let response = self
            .route_command(resident, "create", payload, LONG_ROUTE_TIMEOUT_MS)
            .await?;
        if self.is_stopping(resident) {
            // A shutdown raced the relaunch: stop the freshly spawned worker
            // instead of leaving it running with nobody supervising it.
            let _ = self
                .route_command(resident, "shutdown", json!({}), ROUTE_TIMEOUT_MS)
                .await;
            let mut child = child;
            let _ = child.start_kill();
            return Err(anyhow!("supervisor is shutting down"));
        }
        if !response.success {
            return Err(anyhow!(
                "worker create failed on relaunch: {}",
                response.error.unwrap_or_default()
            ));
        }
        let mut descriptor = resident.descriptor.lock().await;
        descriptor.lifecycle = DaemonWorkerLifecycle::Ready;
        descriptor.consecutive_failures = 0;
        let _ = persist_worker(&resident.descriptor_path, &descriptor);
        Ok(child)
    }

    async fn spawn_worker_process(
        self: &Arc<Self>,
        resident: &Arc<ResidentWorker>,
    ) -> Result<Child> {
        let descriptor = resident.descriptor.lock().await;
        let worker_socket = PathBuf::from(&descriptor.socket_path);
        let token = descriptor.authentication_token.clone();
        let recovery_journal = PathBuf::from(&descriptor.recovery_journal_path);
        let cwd = descriptor
            .create_command
            .rest
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or("/")
            .to_string();
        let script = descriptor
            .create_command
            .rest
            .get("script")
            .and_then(Value::as_str)
            .map(str::to_string);
        let session_dir = descriptor.session_dir.clone();
        drop(descriptor);

        let executable = std::env::current_exe().context("resolve pa-daemon executable")?;
        let mut command = Command::new(&executable);
        command
            .arg("worker")
            .env(WORKER_ROLE_ENV, "1")
            .env(WORKER_TOKEN_ENV, &token)
            .env(WORKER_INSTANCE_ID_ENV, uuid::Uuid::new_v4().to_string())
            .env(WORKER_ACTIVE_SESSION_ID_ENV, &resident.worker_id)
            .env(WORKER_SUPERVISOR_SOCKET_ENV, &self.options.socket_path)
            .env(WORKER_SOCKET_ENV, &worker_socket)
            .env(WORKER_RECOVERY_JOURNAL_ENV, &recovery_journal)
            .env(WORKER_CWD_ENV, &cwd)
            .env(paths::AGENT_DIR_ENV, &self.options.agent_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit());
        if let Some(script) = script {
            command.env(WORKER_SCRIPT_ENV, script);
        }
        if let Some(dir) = session_dir {
            command.env(paths::SESSION_DIR_ENV, dir);
        }
        if std::path::Path::new(&cwd).is_dir() {
            command.current_dir(&cwd);
        }
        let child = command
            .spawn()
            .with_context(|| format!("spawn session worker {}", resident.worker_id))?;
        if std::env::var("PA_DAEMON_DEBUG").is_ok() {
            eprintln!("[supervisor] spawned worker pid {:?}", child.id());
        }
        {
            let mut descriptor = resident.descriptor.lock().await;
            descriptor.pid = child.id().unwrap_or(0) as u64;
            descriptor.lifecycle = DaemonWorkerLifecycle::Starting;
            let _ = persist_worker(&resident.descriptor_path, &descriptor);
        }

        // Wait for the worker socket to accept connections.
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(WORKER_SPAWN_CONNECT_TIMEOUT_MS);
        loop {
            if socket::can_connect(&worker_socket, Duration::from_millis(250)).await {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "session worker {} did not come up in time",
                    resident.worker_id
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(child)
    }

    /// Connect to the worker socket, authenticate, and wire the request pump.
    async fn connect_worker(self: &Arc<Self>, resident: &Arc<ResidentWorker>) -> Result<()> {
        let (socket_path, token) = {
            let descriptor = resident.descriptor.lock().await;
            (
                PathBuf::from(&descriptor.socket_path),
                descriptor.authentication_token.clone(),
            )
        };
        let stream = connect_transport(&socket_path)
            .await
            .with_context(|| format!("connect worker socket {}", socket_path.display()))?;
        let (reader, mut writer) = stream.split();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<WorkerRequest>();
        resident.pending.lock().await.clear();
        let events = self.events.clone();

        // Writer pump: send command frames.
        tokio::spawn(async move {
            while let Some(request) = cmd_rx.recv().await {
                let header = json!({
                    "kind": "command",
                    "requestId": request.request_id,
                    "commandType": request.command_type,
                });
                let written = write_frame(
                    &mut writer,
                    &header,
                    &serde_json::to_vec(&request.payload).unwrap_or_default(),
                    DEFAULT_PRIVATE_FRAME_LIMITS,
                )
                .await;
                if std::env::var("PA_DAEMON_DEBUG").is_ok() {
                    eprintln!(
                        "[supervisor] wrote worker frame {}: {:?}",
                        request.command_type,
                        written.as_ref().map(|_| "ok").map_err(|e| e.to_string())
                    );
                }
                if written.is_err() {
                    break;
                }
            }
        });
        // Reader: route responses to pending requests, forward session events.
        {
            let reader_resident = Arc::clone(resident);
            let events = events.clone();
            tokio::spawn(async move {
                let mut reader = PrivateFrameReader::new(reader, DEFAULT_PRIVATE_FRAME_LIMITS);
                while let Ok(Some(frame)) = reader.read_frame().await {
                    if std::env::var("PA_DAEMON_DEBUG").is_ok() {
                        eprintln!(
                            "[supervisor] worker frame: {:?}",
                            frame.header.get("outboundType")
                        );
                    }
                    let outbound_type = frame
                        .header
                        .get("outboundType")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let request_id = frame
                        .header
                        .get("requestId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let Ok(payload) = serde_json::from_slice::<Value>(&frame.payload) else {
                        continue;
                    };
                    if outbound_type == "response" {
                        if let Some(reply) =
                            reader_resident.pending.lock().await.remove(&request_id)
                        {
                            let response: DaemonResponse = serde_json::from_value(payload)
                                .unwrap_or_else(|_| {
                                    response_failure(
                                        Some(&request_id),
                                        "parse",
                                        "invalid worker response",
                                        None,
                                    )
                                });
                            let _ = reply.send(response);
                        }
                    } else if outbound_type == "session_event" {
                        let active_session_id = payload
                            .get("activeSessionId")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        let routing = active_session_id
                            .map(|active_session_id| ClientRouting::AttachedSession {
                                active_session_id,
                            })
                            .unwrap_or(ClientRouting::Broadcast);
                        let _ = events.send((routing, payload));
                    } else if outbound_type == "side_question_event" {
                        let active_session_id = payload
                            .get("activeSessionId")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        let routing = active_session_id
                            .map(|active_session_id| ClientRouting::AttachedSession {
                                active_session_id,
                            })
                            .unwrap_or(ClientRouting::Broadcast);
                        let _ = events.send((routing, payload));
                    }
                }
            });
        }
        *resident.cmd_tx.lock().await = Some(cmd_tx);

        // Authenticate against the worker.
        let response = self
            .route_command(
                resident,
                "worker_auth",
                json!({
                    "token": token,
                    "supervisorGeneration": format!("sup:{}", std::process::id()),
                    "supervisorPid": std::process::id(),
                    "supervisorProcessStartId": crate::protocol::process_start_id(std::process::id()),
                    "supervisorSocketPath": self.options.socket_path.to_string_lossy(),
                    "workerInstanceId": None::<String>,
                }),
                ROUTE_TIMEOUT_MS,
            )
            .await?;
        if !response.success {
            return Err(anyhow!(
                "worker authentication failed: {}",
                response.error.unwrap_or_default()
            ));
        }
        Ok(())
    }

    async fn route_command(
        &self,
        resident: &Arc<ResidentWorker>,
        command_type: &str,
        payload: Value,
        timeout_ms: u64,
    ) -> Result<DaemonResponse> {
        let cmd_tx = {
            let guard = resident.cmd_tx.lock().await;
            guard
                .clone()
                .ok_or_else(|| anyhow!("Session worker is not connected"))?
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let request_id = uuid::Uuid::new_v4().to_string();
        resident
            .pending
            .lock()
            .await
            .insert(request_id.clone(), reply_tx);
        cmd_tx
            .send(WorkerRequest {
                request_id,
                command_type: command_type.to_string(),
                payload,
            })
            .map_err(|_| anyhow!("Session worker is not connected"))?;
        match tokio::time::timeout(Duration::from_millis(timeout_ms), reply_rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(anyhow!("Session worker dropped the request")),
            Err(_) => Err(anyhow!("Session worker timed out")),
        }
    }

    /// Launch a brand-new worker for a create command.
    async fn launch_worker(
        self: &Arc<Self>,
        create: &DaemonCommand,
        owner_client_id: Option<String>,
    ) -> Result<Arc<ResidentWorker>> {
        let DaemonCommand::Create {
            session_path,
            continue_recent,
            no_session,
            name,
            config,
            ..
        } = create
        else {
            return Err(anyhow!("launch_worker requires a create command"));
        };
        let config_object = config.as_ref().and_then(Value::as_object);
        let cwd_value = config_object
            .and_then(|config| config.get("cwd"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| "/".to_string());
        let session_dir = config_object
            .and_then(|config| config.get("sessionDir"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let script = config_object
            .and_then(|config| config.get("script"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if *no_session == Some(true) && session_path.is_some() {
            return Err(anyhow!(
                "Session cannot be both no-session and session-pathed"
            ));
        }
        let session_dir_path = session_dir
            .as_deref()
            .map(paths::expand_tilde)
            .unwrap_or_else(|| paths::sessions_dir(&self.options.agent_dir));
        if *continue_recent == Some(true) {
            let recent = find_most_recent_session_for_cwd(&session_dir_path, &cwd_value);
            if recent.is_none() {
                return Err(anyhow!("No recent session found for {}", cwd_value));
            }
        }
        let worker_id = util::new_display_id();
        let worker_socket = socket::worker_socket_path(&self.options.socket_path, &worker_id);
        let now = util::now_iso();
        let mut durable_rest = serde_json::Map::new();
        durable_rest.insert("cwd".to_string(), json!(cwd_value));
        if let Some(session_dir) = &session_dir {
            durable_rest.insert("sessionDir".to_string(), json!(session_dir));
        }
        if let Some(name) = name {
            durable_rest.insert("name".to_string(), json!(name));
        }
        if let Some(script) = &script {
            durable_rest.insert("script".to_string(), json!(script));
        }
        let descriptor = DaemonWorkerDescriptor {
            version: 2,
            worker_id: worker_id.clone(),
            pid: 0,
            process_start_id: None,
            socket_path: worker_socket.to_string_lossy().to_string(),
            recovery_journal_path: self
                .descriptor_dir
                .join(format!("{worker_id}.recovery.jsonl"))
                .to_string_lossy()
                .to_string(),
            orphan_process_journal_path: None,
            supervisor_socket_path: self.options.socket_path.to_string_lossy().to_string(),
            authentication_token: uuid::Uuid::new_v4().to_string(),
            worker_instance_id: Some(uuid::Uuid::new_v4().to_string()),
            root_active_session_id: worker_id.clone(),
            owner_client_id,
            root_session_id: None,
            session_file: session_path.clone(),
            session_dir: session_dir.clone(),
            telemetry_disabled: None,
            created_at: now.clone(),
            updated_at: now,
            lifecycle: DaemonWorkerLifecycle::Starting,
            create_command: DurableDaemonCreateCommand {
                session_path: session_path.clone(),
                no_session: *no_session,
                rest: durable_rest,
            },
            consecutive_failures: 0,
            stop_requested_at: None,
            archive_on_stop: None,
            last_failure_at: None,
            last_error: None,
            rest: Default::default(),
        };
        let descriptor_path = self.descriptor_dir.join(format!("{worker_id}.json"));
        let resident = ResidentWorker::new(worker_id.clone(), descriptor, descriptor_path.clone());
        // Register the resident before spawning the process: the worker
        // self-registers on boot, and the registration handler must find its
        // identity in the registry (registration races the create replay).
        self.registry.insert(Arc::clone(&resident)).await;
        let child = self.spawn_worker_process(&resident).await?;
        self.connect_worker(&resident).await?;
        let create_payload = {
            let descriptor = resident.descriptor.lock().await;
            create_command_payload(&descriptor.create_command)
        };
        let response = self
            .route_command(&resident, "create", create_payload, LONG_ROUTE_TIMEOUT_MS)
            .await?;
        if !response.success {
            let _ = std::fs::remove_file(&descriptor_path);
            return Err(anyhow!(
                "session worker create failed: {}",
                response.error.unwrap_or_default()
            ));
        }
        {
            let mut descriptor = resident.descriptor.lock().await;
            descriptor.lifecycle = DaemonWorkerLifecycle::Ready;
            if let Some(summary) = &response.data {
                descriptor.root_session_id = summary
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(session_file) = summary
                    .get("sessionFile")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                {
                    descriptor.session_file = Some(session_file.clone());
                    // The durable create command must reopen the same session
                    // file on relaunch, or a respawned worker would create a
                    // fresh session and lose history.
                    descriptor.create_command.session_path = Some(session_file.clone());
                }
            }
            persist_worker(&descriptor_path, &descriptor)?;
        }
        let pid = child.id().unwrap_or(0);
        self.spawn_monitor(Arc::clone(&resident), Some(child), pid as u64);
        Ok(resident)
    }

    // ------------------------------------------------------------------
    // Client connections (JSONL transport)
    // ------------------------------------------------------------------

    async fn handle_client(self: Arc<Self>, stream: Box<dyn TransportStream>) -> Result<()> {
        let (reader, mut writer) = stream.split();
        let client_id = util::new_display_id();
        let hello = DaemonOutbound::DaemonHello {
            socket_path: self.options.socket_path.to_string_lossy().to_string(),
            protocol: current_protocol_info(),
            schema_id: Some(DAEMON_SCHEMA_ID.to_string()),
            schema_revision: Some(DAEMON_SCHEMA_REVISION),
            app_version: Some(DAEMON_APP_VERSION.to_string()),
            runtime: Some(DaemonRuntimeIdentity {
                build_id: DAEMON_APP_VERSION.to_string(),
                executable_path: std::env::current_exe()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default(),
                entrypoint_path: None,
                launcher_path: None,
            }),
            supervisor_generation: Some(format!("sup:{}", std::process::id())),
            supervisor_pid: Some(std::process::id() as u64),
            supervisor_owner_token: Some(uuid::Uuid::new_v4().to_string()),
            supervisor_process_start_id: crate::protocol::process_start_id(std::process::id()),
            supervisor_socket_path: Some(self.options.socket_path.to_string_lossy().to_string()),
            client_id: client_id.clone(),
            server_capabilities: default_server_capabilities(),
            rest: Default::default(),
        };
        write_line(&mut writer, &serde_json::to_value(&hello)?).await?;

        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        let mut events = self.events.subscribe();
        let mut attached: Vec<String> = Vec::new();
        let mut effective_client_id = client_id.clone();
        loop {
            line.clear();
            tokio::select! {
                read = reader.read_line(&mut line) => {
                    let Ok(read) = read else { break };
                    if read == 0 {
                        break;
                    }
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let (lines, stop) = self
                        .dispatch_client(trimmed, &mut effective_client_id, &mut attached)
                        .await;
                    for outbound in lines {
                        write_line(&mut writer, &outbound).await?;
                    }
                    if stop {
                        break;
                    }
                }
                event = events.recv() => {
                    match event {
                        Ok((routing, payload)) => {
                            let deliver = match &routing {
                                ClientRouting::Broadcast => true,
                                ClientRouting::AttachedSession { active_session_id } => {
                                    attached.iter().any(|id| id == active_session_id)
                                }
                            };
                            if deliver {
                                write_line(&mut writer, &payload).await?;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
        // Detach from every attached session on disconnect (a TUI exit does
        // not stop the session; the worker keeps running).
        for active_session_id in attached.iter() {
            if let Ok(resident) = self.registry.resolve(active_session_id).await {
                let payload = json!({ "type": "detach", "clientId": effective_client_id });
                let _ = self
                    .route_command(&resident, "detach", payload, ROUTE_TIMEOUT_MS)
                    .await;
            }
        }
        Ok(())
    }

    /// Handle one client command line: returns outbound lines in order and
    /// whether this client connection should stop.
    async fn dispatch_client(
        self: &Arc<Self>,
        line: &str,
        effective_client_id: &mut String,
        attached: &mut Vec<String>,
    ) -> (Vec<Value>, bool) {
        let envelope = match parse_supervisor_command_line(line) {
            Ok(envelope) => envelope,
            Err(error) => {
                let id = salvage_id(line);
                return (
                    vec![response_line(&response_failure(
                        id.as_deref(),
                        "parse",
                        &error.to_string(),
                        None,
                    ))],
                    false,
                );
            }
        };
        let command_id = envelope.id.clone();
        if let Some(client_id) = envelope.client_id.clone() {
            *effective_client_id = client_id;
        }
        let type_name = command_type_name(&envelope.command).to_string();
        match &envelope.command {
            DaemonCommand::AckResult { .. } => (Vec::new(), false),
            DaemonCommand::Restart { .. } | DaemonCommand::Shutdown { .. } => {
                let response = response_success(Some(&command_id), &type_name, None);
                let mut lines = vec![response_line(&response)];
                // daemon_closing goes to every client before the exit.
                let closing = json!({ "type": "daemon_closing", "reason": "shutdown" });
                let _ = self
                    .events
                    .send((ClientRouting::Broadcast, closing.clone()));
                lines.push(closing);
                self.begin_shutdown().await;
                (lines, true)
            }
            DaemonCommand::List {
                all,
                cwd,
                session_dir,
                ..
            } => {
                let response = self
                    .handle_list(
                        command_id,
                        type_name,
                        *all,
                        cwd.clone(),
                        session_dir.clone(),
                    )
                    .await;
                (vec![response_line(&response)], false)
            }
            DaemonCommand::ListSavedSessions { .. } => {
                let lines = self
                    .handle_saved_session_list(&envelope.command, &command_id)
                    .await;
                (lines, false)
            }
            DaemonCommand::Create { .. } => {
                match self
                    .handle_create(&envelope.command, effective_client_id.clone())
                    .await
                {
                    Ok(summary) => (
                        vec![response_line(&response_success(
                            Some(&command_id),
                            &type_name,
                            Some(summary),
                        ))],
                        false,
                    ),
                    Err(error) => (
                        vec![response_line(&response_failure(
                            Some(&command_id),
                            &type_name,
                            &error.to_string(),
                            None,
                        ))],
                        false,
                    ),
                }
            }
            DaemonCommand::WorkerRegister { .. } => {
                // Worker self-registration: rebuilds the roster entry from
                // the worker's own identity instead of routing to a session.
                let response = self
                    .handle_worker_register(&command_id, &type_name, &envelope.command)
                    .await;
                (vec![response_line(&response)], false)
            }
            command => {
                self.route_client_command(
                    command,
                    effective_client_id,
                    attached,
                    command_id,
                    type_name,
                )
                .await
            }
        }
    }

    /// `worker_register`: a session worker presenting its identity (boot
    /// registration or re-registration after this supervisor restarted).
    /// The token was issued when the supervisor spawned or adopted the
    /// worker, so an unknown worker id or a token mismatch is rejected.
    async fn handle_worker_register(
        self: &Arc<Self>,
        command_id: &str,
        type_name: &str,
        command: &DaemonCommand,
    ) -> DaemonResponse {
        let DaemonCommand::WorkerRegister {
            active_session_id,
            session_id,
            socket_path,
            worker_instance_id,
            token,
            pid,
            ..
        } = command
        else {
            return response_failure(Some(command_id), type_name, "not a registration", None);
        };
        let fail = |error: &str| response_failure(Some(command_id), type_name, error, None);
        if self.shutting_down.load(Ordering::SeqCst) {
            return fail("Supervisor is shutting down");
        }
        if active_session_id.is_empty() || socket_path.is_empty() || *pid == 0 {
            return fail("Session worker registration is missing identity fields");
        }
        let worker_instance_id =
            (!worker_instance_id.is_empty()).then(|| worker_instance_id.clone());
        let registration = WorkerRegistration {
            active_session_id: active_session_id.clone(),
            session_id: session_id
                .clone()
                .filter(|value: &String| !value.is_empty()),
            socket_path: socket_path.clone(),
            worker_instance_id: worker_instance_id.clone(),
            pid: *pid,
        };
        // Serialize against descriptor adoption for the same worker.
        let guard = self.registry.adoption_guard(active_session_id).await;
        let resident = match self.registry.get(active_session_id).await {
            Some(resident) => resident,
            None => match self.adopt_registered_worker(&registration, token).await {
                Ok(resident) => resident,
                Err(error) => return fail(&format!("{error:#}")),
            },
        };
        // Refresh the durable identity from the live worker (the token was
        // issued by this supervisor; a mismatch is a rogue registration).
        {
            let mut descriptor = resident.descriptor.lock().await;
            if token.as_str() != descriptor.authentication_token {
                return fail("Session worker authentication failed");
            }
            descriptor.pid = *pid;
            descriptor.socket_path = socket_path.clone();
            descriptor.worker_instance_id = worker_instance_id.clone();
            if let Some(session_id) = &registration.session_id {
                descriptor.root_session_id = Some(session_id.clone());
            }
            descriptor.lifecycle = DaemonWorkerLifecycle::Ready;
            let _ = persist_worker(&resident.descriptor_path, &descriptor);
        }
        let record = self.registry.record_registration(registration).await;
        let verb = if record.epoch > 1 {
            "re-registered"
        } else {
            "registered"
        };
        self.log_line(&format!(
            "session worker {active_session_id} {verb} (epoch {}, pid {pid})",
            record.epoch
        ));
        drop(guard);
        response_success(
            Some(command_id),
            type_name,
            Some(json!({
                "workerId": active_session_id,
                "sessionId": session_id,
                "supervisorGeneration": format!("sup:{}", std::process::id()),
                "supervisorPid": std::process::id(),
                "epoch": record.epoch,
            })),
        )
    }

    /// A registration for a worker with no roster entry: adopt it from its
    /// persisted descriptor (the durable fallback record). The registration
    /// proves the worker process is alive; adoption connects it for routing.
    async fn adopt_registered_worker(
        self: &Arc<Self>,
        registration: &WorkerRegistration,
        token: &str,
    ) -> Result<Arc<ResidentWorker>> {
        let descriptor_path = self
            .descriptor_dir
            .join(format!("{}.json", registration.active_session_id));
        let Ok(content) = std::fs::read_to_string(&descriptor_path) else {
            return Err(anyhow!(
                "Unknown session worker: {}",
                registration.active_session_id
            ));
        };
        let descriptor: crate::descriptor::WorkerDescriptor = serde_json::from_str(&content)
            .with_context(|| format!("invalid descriptor {}", descriptor_path.display()))?;
        crate::descriptor::validate_descriptor(&descriptor, &self.options.socket_path)?;
        if token != descriptor.authentication_token.as_str() {
            return Err(anyhow!("Session worker authentication failed"));
        }
        let worker_id = descriptor.worker_id.clone();
        let resident = ResidentWorker::new(
            registration.active_session_id.clone(),
            descriptor,
            descriptor_path,
        );
        self.connect_worker(&resident).await?;
        self.registry.insert(Arc::clone(&resident)).await;
        self.spawn_monitor(Arc::clone(&resident), None, registration.pid);
        self.log_line(&format!(
            "adopted session worker {worker_id} via self-registration"
        ));
        Ok(resident)
    }

    /// `list_saved_sessions` (port of `handleSavedSessionList`): stream
    /// `session_list_item`/`session_list_progress` events, then a final
    /// response with the full saved-session rows.
    async fn handle_saved_session_list(
        self: &Arc<Self>,
        command: &DaemonCommand,
        command_id: &str,
    ) -> Vec<Value> {
        let DaemonCommand::ListSavedSessions {
            cwd,
            session_dir,
            active_session_id,
            scope,
            ..
        } = command
        else {
            return Vec::new();
        };
        // Session-addressed form: use the live worker's cwd and session dir.
        let (cwd, session_dir) = match active_session_id {
            Some(active_session_id) => {
                let resident = self.registry.get(active_session_id).await;
                match resident {
                    Some(resident) => {
                        let descriptor = resident.descriptor.lock().await;
                        let cwd = descriptor
                            .create_command
                            .rest
                            .get("cwd")
                            .and_then(Value::as_str)
                            .unwrap_or("/")
                            .to_string();
                        let session_dir = descriptor
                            .create_command
                            .rest
                            .get("sessionDir")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        (cwd, session_dir)
                    }
                    None => {
                        return vec![response_line(&response_failure(
                            Some(command_id),
                            "list_saved_sessions",
                            &format!("Unknown active session: {active_session_id}"),
                            None,
                        ))];
                    }
                }
            }
            None => {
                let Some(cwd) = cwd else {
                    // The TS supervisor runs Node's path.resolve on the
                    // missing cwd; reproduce the observable error string.
                    return vec![response_line(&response_failure(
                        Some(command_id),
                        "list_saved_sessions",
                        "The \"paths[0]\" property must be of type string, got undefined",
                        None,
                    ))];
                };
                (cwd.clone(), session_dir.clone())
            }
        };
        let dir = session_dir
            .map(|dir| crate::paths::expand_tilde(&dir))
            .unwrap_or_else(|| crate::paths::sessions_dir(&self.options.agent_dir));
        let scope_current = scope.as_str() == Some("current");
        let mut infos = crate::session_store::list_sessions(&dir);
        if scope_current {
            infos.retain(|info| info.cwd == cwd);
        }
        let total = infos.len();
        let mut lines = Vec::new();
        for (index, info) in infos.iter().enumerate() {
            let row = saved_session_row(info);
            let mut item = json!({
                "id": command_id,
                "type": "session_list_item",
                "command": "list_saved_sessions",
                "session": row,
            });
            if let Some(active_session_id) = active_session_id {
                item["activeSessionId"] = json!(active_session_id);
            }
            lines.push(item);
            let mut progress = json!({
                "id": command_id,
                "type": "session_list_progress",
                "command": "list_saved_sessions",
                "loaded": index + 1,
                "total": total,
            });
            if let Some(active_session_id) = active_session_id {
                progress["activeSessionId"] = json!(active_session_id);
            }
            lines.push(progress);
        }
        let sessions: Vec<Value> = infos.iter().map(saved_session_row).collect();
        lines.push(response_line(&response_success(
            Some(command_id),
            "list_saved_sessions",
            Some(json!({ "sessions": sessions })),
        )));
        lines
    }

    async fn handle_list(
        self: &Arc<Self>,
        command_id: String,
        type_name: String,
        all: Option<bool>,
        cwd: Option<String>,
        session_dir: Option<String>,
    ) -> DaemonResponse {
        let dir = session_dir
            .map(|dir| paths::expand_tilde(&dir))
            .unwrap_or_else(|| paths::sessions_dir(&self.options.agent_dir));
        let summaries: Vec<Value> = match all {
            Some(true) => {
                let mut infos = list_sessions(&dir);
                if let Some(cwd) = cwd {
                    infos.retain(|info| info.cwd == cwd);
                }
                infos
                    .into_iter()
                    .map(|info| saved_session_summary(&info))
                    .collect()
            }
            _ => {
                // Live residents of this supervisor.
                let mut summaries = Vec::new();
                for resident in self.registry.list().await {
                    let response = self
                        .route_command(&resident, "get_state", json!({}), ROUTE_TIMEOUT_MS)
                        .await;
                    match response {
                        Ok(response) if response.success => {
                            if let Some(data) = response.data {
                                summaries.push(data);
                            }
                        }
                        _ => summaries.push(offline_summary(&resident.worker_id)),
                    }
                }
                summaries
            }
        };
        response_success(
            Some(&command_id),
            &type_name,
            Some(json!({ "sessions": summaries })),
        )
    }

    async fn handle_create(
        self: &Arc<Self>,
        command: &DaemonCommand,
        client_id: String,
    ) -> Result<Value> {
        if let DaemonCommand::Create {
            name: Some(name), ..
        } = command
        {
            self.assert_session_name_available(name).await?;
        }
        let resident = self.launch_worker(command, Some(client_id)).await?;
        // Fresh get_state so the response matches attach/list rows exactly.
        let response = self
            .route_command(&resident, "get_state", json!({}), ROUTE_TIMEOUT_MS)
            .await?;
        Ok(response
            .data
            .unwrap_or_else(|| json!({ "id": resident.worker_id })))
    }

    async fn assert_session_name_available(self: &Arc<Self>, name: &str) -> Result<()> {
        if name.trim().is_empty() {
            return Err(anyhow!("Session name cannot be empty"));
        }
        for resident in self.registry.list().await {
            let response = self
                .route_command(&resident, "get_state", json!({}), ROUTE_TIMEOUT_MS)
                .await;
            if let Ok(response) = response {
                if let Some(data) = &response.data {
                    let session_name = data
                        .get("sessionName")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if session_name == name {
                        return Err(anyhow!(
                            "Session name \"{name}\" is unavailable for depth 0"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    async fn route_client_command(
        self: &Arc<Self>,
        command: &DaemonCommand,
        client_id: &str,
        attached: &mut Vec<String>,
        command_id: String,
        type_name: String,
    ) -> (Vec<Value>, bool) {
        let selector = command_active_session_id(command)
            .unwrap_or_default()
            .to_string();
        let Ok(resident) = self.registry.resolve(&selector).await else {
            return (
                vec![response_line(&response_failure(
                    Some(&command_id),
                    &type_name,
                    &format!("Unknown active session: {selector}"),
                    None,
                ))],
                false,
            );
        };
        let timeout = if matches!(
            command,
            DaemonCommand::PromptAndWait { .. } | DaemonCommand::WaitForIdle { .. }
        ) {
            LONG_ROUTE_TIMEOUT_MS
        } else {
            ROUTE_TIMEOUT_MS
        };
        let (worker_command, payload) = match client_command_payload(command, client_id) {
            Ok(payload) => payload,
            Err(error) => {
                return (
                    vec![response_line(&response_failure(
                        Some(&command_id),
                        &type_name,
                        &error.to_string(),
                        None,
                    ))],
                    false,
                )
            }
        };
        let response = self
            .route_command(&resident, worker_command, payload, timeout)
            .await;
        match response {
            Ok(mut response) => {
                // Worker replies carry no client request id; clients match
                // responses by the id they sent, so stamp it back here.
                response.id = Some(command_id.clone());
                if let DaemonCommand::Attach {
                    capabilities,
                    supports_extension_ui,
                    ..
                }
                | DaemonCommand::Reattach {
                    capabilities,
                    supports_extension_ui,
                    ..
                } = command
                {
                    if response.success {
                        if let Some(data) = response.data.as_mut() {
                            let active_id = data
                                .get("activeSessionId")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| resident.worker_id.clone());
                            if !attached.iter().any(|id| id == &active_id) {
                                attached.push(active_id.clone());
                            }
                            // The client's own capability set, not the
                            // supervisor's worker-facing one, is echoed in
                            // the attach result.
                            let client_capabilities = attach_client_capabilities(
                                capabilities.as_deref(),
                                *supports_extension_ui,
                            );
                            if let Some(client) = data.get_mut("client") {
                                client["capabilities"] = json!(client_capabilities.clone());
                            }
                            if wants_chunked(&client_capabilities) {
                                let purpose = if matches!(command, DaemonCommand::Reattach { .. }) {
                                    SnapshotPurpose::Replacement
                                } else {
                                    SnapshotPurpose::Attach
                                };
                                return streamed_attach_lines(response, &active_id, purpose);
                            }
                            return (vec![response_line(&response)], false);
                        }
                    }
                    return (vec![response_line(&response)], false);
                }
                if let DaemonCommand::Detach { .. } = command {
                    if response.success {
                        attached.retain(|id| id != &resident.worker_id);
                    }
                }
                if let DaemonCommand::Kill { .. } = command {
                    if response.success {
                        self.stop_worker(&resident).await;
                    }
                }
                if let DaemonCommand::RetryWorker { .. } = command {
                    if response.success {
                        let _ = self.relaunch_worker(&resident).await;
                    }
                }
                (vec![response_line(&response)], false)
            }
            Err(error) => (
                vec![response_line(&response_failure(
                    Some(&command_id),
                    &type_name,
                    &error.to_string(),
                    None,
                ))],
                false,
            ),
        }
    }

    async fn stop_worker(self: &Arc<Self>, resident: &Arc<ResidentWorker>) {
        resident.intentional_stop.store(true, Ordering::SeqCst);
        let _ = self
            .route_command(resident, "shutdown", json!({}), ROUTE_TIMEOUT_MS)
            .await;
        let _ = std::fs::remove_file(&resident.descriptor_path);
        self.registry.remove(&resident.worker_id).await;
    }

    async fn begin_shutdown(self: &Arc<Self>) {
        self.shutting_down.store(true, Ordering::SeqCst);
        for resident in self.registry.list().await {
            resident.intentional_stop.store(true, Ordering::SeqCst);
            let _ = self
                .route_command(&resident, "shutdown", json!({}), ROUTE_TIMEOUT_MS)
                .await;
            let _ = std::fs::remove_file(&resident.descriptor_path);
        }
        self.registry.clear().await;
        // Wake the accept loop only after the workers stopped, so the process
        // cannot exit mid-stop and orphan a live worker.
        self.shutdown_notify.notify_one();
    }
}

/// Attach outcome for a `chunked_snapshot` client: the response carries the
/// snapshot header with an empty transcript plus a `snapshotStream`
/// descriptor, and the transcript follows as `session_snapshot_begin` /
/// `session_snapshot_chunk` / `session_snapshot_end` records. A snapshot
/// that cannot be transferred after the response surfaces as
/// `session_snapshot_failed` keyed by the same snapshot id.
fn streamed_attach_lines(
    mut response: DaemonResponse,
    active_session_id: &str,
    purpose: SnapshotPurpose,
) -> (Vec<Value>, bool) {
    let Some(data) = response.data.take() else {
        return (vec![response_line(&response)], false);
    };
    match stream_attach(data, active_session_id, purpose) {
        Ok((streamed, events)) => {
            response.data = Some(streamed);
            let mut lines = vec![response_line(&response)];
            lines.extend(events.lines());
            (lines, false)
        }
        // The snapshot could not even be identified: the attach itself
        // fails, before any snapshot record exists on the wire.
        Err(error) => (
            vec![response_line(&response_failure(
                response.id.as_deref(),
                &response.command,
                &error.to_string(),
                None,
            ))],
            false,
        ),
    }
}

fn salvage_id(line: &str) -> Option<String> {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_string))
}

async fn write_line<W: AsyncWriteExt + Unpin>(writer: &mut W, value: &Value) -> Result<()> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// The worker-side command name plus payload for a routed client command.
fn client_command_payload(
    command: &DaemonCommand,
    client_id: &str,
) -> Result<(&'static str, Value)> {
    let type_name = command_type_name(command);
    let mut payload = serde_json::to_value(command)?;
    if let Some(object) = payload.as_object_mut() {
        object.insert("clientId".to_string(), json!(client_id));
        // The supervisor always attaches slim, like the TS supervisor's
        // `attachClient`: summary and messages travel inside the snapshot.
        if matches!(
            command,
            DaemonCommand::Attach { .. } | DaemonCommand::Reattach { .. }
        ) {
            object.insert(
                "capabilities".to_string(),
                json!(["attach_snapshot", "event_sequence", "slim_attach"]),
            );
        }
        // Create carries its fields under `config`; the worker reads them flat.
        if let Some(config) = object.remove("config") {
            if let Some(config) = config.as_object() {
                for (key, value) in config {
                    object.insert(key.clone(), value.clone());
                }
            }
        }
    }
    Ok((type_name, payload))
}

fn saved_session_summary(info: &crate::session_store::SessionInfo) -> Value {
    json!({
        "id": info.id,
        "lifecycle": "resident",
        "activity": "idle",
        "isSessionActive": false,
        "activeSessionId": info.id,
        "sessionId": info.id,
        "sessionFile": info.path.to_string_lossy(),
        "sessionName": info.name,
        "cwd": info.cwd,
        "isStreaming": false,
        "isCompacting": false,
        "attachedClients": 0,
        "messageCount": info.message_count,
        "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
        "created": info.created,
        "modified": info.modified,
        "firstMessage": info.first_message,
    })
}

fn offline_summary(worker_id: &str) -> Value {
    json!({
        "id": worker_id,
        "lifecycle": "recovering",
        "activity": "idle",
        "isSessionActive": false,
        "sessionId": "",
        "cwd": "",
        "isStreaming": false,
        "isCompacting": false,
        "attachedClients": 0,
        "messageCount": 0,
        "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
    })
}

/// Entry point for the supervisor process.
pub async fn run_supervisor(options: SupervisorOptions) -> Result<()> {
    let supervisor = Arc::new(Supervisor::new(options)?);
    supervisor.run().await
}

/// Saved-session row (port of `serializeSavedSessionInfo`).
fn saved_session_row(info: &crate::session_store::SessionInfo) -> Value {
    let mut row = json!({
        "path": info.path.to_string_lossy(),
        "id": info.id,
        "cwd": info.cwd,
        "rlmDepth": info.rlm_depth,
        "created": info.created,
        "modified": info.modified,
        "messageCount": info.message_count,
        "firstMessage": info.first_message,
        // The scan does not concatenate the transcript; consumers use the
        // per-session read paths for full text.
        "allMessagesText": "",
        "state": info.state.as_ref().map(|state| json!({ "status": state })),
    });
    let object = row.as_object_mut().expect("row object");
    if let Some(name) = &info.name {
        object.insert("name".to_string(), json!(name));
    }
    if let Some(parent) = &info.parent_session_path {
        object.insert("parentSessionPath".to_string(), json!(parent));
    }
    if let Some((provider, model_id)) = &info.model {
        object.insert(
            "model".to_string(),
            json!({ "provider": provider, "modelId": model_id }),
        );
    }
    row
}
