//! Supervisor runtime: one process spawning one worker per active session.
//!
//! Port of `modes/daemon/daemon-supervisor.ts`: the supervisor hosts no sessions
//! itself. Clients connect over a JSONL Unix socket; the supervisor spawns a
//! dedicated worker process per session, supervises it (restart with
//! exponential backoff, bounded attempts), persists worker descriptors so a
//! restarted supervisor can adopt or relaunch live sessions, and routes
//! commands and events between clients and workers (private-framed channel).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use pa_types::daemon::{
    DaemonCommand, DaemonOutbound, DaemonWorkerDescriptor, DaemonWorkerLifecycle,
    DurableDaemonCreateCommand,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};

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
use crate::session_store::{find_most_recent_session_for_cwd, list_sessions};
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

/// One resident session worker (port of `ResidentWorker`).
struct ResidentWorker {
    worker_id: String,
    descriptor: Mutex<DaemonWorkerDescriptor>,
    descriptor_path: PathBuf,
    cmd_tx: Mutex<Option<mpsc::UnboundedSender<WorkerRequest>>>,
    /// Pending replies for in-flight requests on the current connection.
    pending: Mutex<HashMap<String, oneshot::Sender<DaemonResponse>>>,
    intentional_stop: AtomicBool,
    consecutive_failures: AtomicU32,
}

impl ResidentWorker {
    fn new(
        worker_id: String,
        descriptor: DaemonWorkerDescriptor,
        descriptor_path: PathBuf,
    ) -> Arc<Self> {
        Arc::new(ResidentWorker {
            worker_id,
            descriptor: Mutex::new(descriptor),
            descriptor_path,
            cmd_tx: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            intentional_stop: AtomicBool::new(false),
            consecutive_failures: AtomicU32::new(0),
        })
    }

    async fn labels(&self) -> (String, String, String) {
        let descriptor = self.descriptor.lock().await;
        let session_file = descriptor.session_file.as_deref().unwrap_or_default();
        let file_stem = std::path::Path::new(session_file)
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default();
        let name = descriptor
            .create_command
            .rest
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        (descriptor.root_active_session_id.clone(), file_stem, name)
    }
}

struct WorkerRequest {
    request_id: String,
    command_type: String,
    payload: Value,
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
    workers: Mutex<HashMap<String, Arc<ResidentWorker>>>,
    /// Worker outbound frames, with their client routing.
    events: broadcast::Sender<(ClientRouting, Value)>,
    shutting_down: AtomicBool,
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
            workers: Mutex::new(HashMap::new()),
            events,
            shutting_down: AtomicBool::new(false),
            log,
        })
    }

    /// Bind the client socket, adopt or relaunch persisted workers, serve.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        socket::prepare_socket_path(&self.options.socket_path).await?;
        let listener = UnixListener::bind(&self.options.socket_path).with_context(|| {
            format!(
                "bind supervisor socket {}",
                self.options.socket_path.display()
            )
        })?;
        socket::restrict_socket_path(&self.options.socket_path);
        self.log
            .append(&format!("supervisor started pid {}", std::process::id()));

        self.adopt_persisted_workers().await;

        while !self.shutting_down.load(Ordering::SeqCst) {
            let (stream, _addr) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    if self.shutting_down.load(Ordering::SeqCst) {
                        break;
                    }
                    return Err(anyhow!("supervisor accept: {error}"));
                }
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

    async fn adopt_persisted_workers(self: &Arc<Self>) {
        let descriptors = load_descriptors(&self.descriptor_dir, &self.options.socket_path);
        for (path, descriptor) in descriptors {
            let socket_path = PathBuf::from(&descriptor.socket_path);
            let alive = socket::can_connect(&socket_path, Duration::from_millis(500)).await;
            let worker_id = descriptor.worker_id.clone();
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
                    self.workers
                        .lock()
                        .await
                        .insert(worker_id.clone(), Arc::clone(&resident));
                    self.spawn_monitor(Arc::clone(&resident), None, pid);
                    self.log_line(&format!(
                        "adopted session worker {worker_id} (was alive: {alive})"
                    ));
                }
                Err(error) => {
                    self.log_line(&format!("could not adopt worker {worker_id}: {error:#}"));
                }
            }
        }
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
                    if !is_process_alive(adopted_pid as u32) {
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
                self.workers.lock().await.remove(&resident.worker_id);
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
        let stream = UnixStream::connect(&socket_path)
            .await
            .with_context(|| format!("connect worker socket {}", socket_path.display()))?;
        let (reader, mut writer) = stream.into_split();
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
        self.workers
            .lock()
            .await
            .insert(worker_id.clone(), Arc::clone(&resident));
        self.spawn_monitor(Arc::clone(&resident), Some(child), pid as u64);
        Ok(resident)
    }

    // ------------------------------------------------------------------
    // Client connections (JSONL transport)
    // ------------------------------------------------------------------

    async fn handle_client(self: Arc<Self>, stream: UnixStream) -> Result<()> {
        let (reader, mut writer) = stream.into_split();
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
            if let Ok(resident) = self.resolve_worker(active_session_id).await {
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
                let workers = self.workers.lock().await;
                let resident = workers.get(active_session_id);
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
                let workers = self.workers.lock().await;
                for (worker_id, resident) in workers.iter() {
                    let response = self
                        .route_command(resident, "get_state", json!({}), ROUTE_TIMEOUT_MS)
                        .await;
                    match response {
                        Ok(response) if response.success => {
                            if let Some(data) = response.data {
                                summaries.push(data);
                            }
                        }
                        _ => summaries.push(offline_summary(worker_id)),
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
        let workers = self.workers.lock().await;
        for resident in workers.values() {
            let response = self
                .route_command(resident, "get_state", json!({}), ROUTE_TIMEOUT_MS)
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

    async fn resolve_worker(self: &Arc<Self>, selector: &str) -> Result<Arc<ResidentWorker>> {
        let workers = self.workers.lock().await;
        if let Some(resident) = workers.get(selector) {
            return Ok(Arc::clone(resident));
        }
        let mut matches: Vec<(Arc<ResidentWorker>, String, String)> = Vec::new();
        for resident in workers.values() {
            let (root_id, file_stem, name) = resident.labels().await;
            if selector_matches(&root_id, selector)
                || selector_matches(&file_stem, selector)
                || (!name.is_empty() && name == selector)
            {
                matches.push((Arc::clone(resident), root_id, name));
            }
        }
        if matches.len() == 1 {
            return Ok(matches.pop().map(|(r, ..)| r).expect("one match"));
        }
        if matches.len() > 1 {
            let rendered = matches
                .iter()
                .map(|(_, root, name)| {
                    if name.is_empty() {
                        root.clone()
                    } else {
                        format!("{root} ({name})")
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow!(
                "Ambiguous active session \"{selector}\": matches {rendered}"
            ));
        }
        Err(anyhow!("Unknown active session: {selector}"))
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
        let Ok(resident) = self.resolve_worker(&selector).await else {
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
                if let DaemonCommand::Attach { .. } | DaemonCommand::Reattach { .. } = command {
                    if response.success {
                        if let Some(data) = &response.data {
                            let active_id = data
                                .get("activeSessionId")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| resident.worker_id.clone());
                            if !attached.iter().any(|id| id == &active_id) {
                                attached.push(active_id.clone());
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
        self.workers.lock().await.remove(&resident.worker_id);
    }

    async fn begin_shutdown(self: &Arc<Self>) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let mut workers = self.workers.lock().await;
        for resident in workers.values() {
            resident.intentional_stop.store(true, Ordering::SeqCst);
            let _ = self
                .route_command(resident, "shutdown", json!({}), ROUTE_TIMEOUT_MS)
                .await;
            let _ = std::fs::remove_file(&resident.descriptor_path);
        }
        workers.clear();
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

fn selector_matches(candidate: &str, suffix: &str) -> bool {
    let normalize = |value: &str| -> String { value.replace('-', "").to_lowercase() };
    let candidate = normalize(candidate);
    let suffix = normalize(suffix);
    !candidate.is_empty() && !suffix.is_empty() && candidate.ends_with(&suffix)
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
