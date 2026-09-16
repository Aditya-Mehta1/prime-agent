//! Kernel client for the REPL runtime: the kernel is a JSON-lines subprocess
//! (`python -m rlm.repl`) — requests on stdin, events on stdout, stderr kept
//! as a diagnostics tail. The protocol is documented in
//! prime-agent-runtime/src/rlm/repl.md (protocol version 3).
//!
//! Ported from `core/kernel/repl-manager.ts`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use std::io::Write;

use anyhow::anyhow;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, Notify};

use crate::kernel::cancellation::{merge_signals, AbortSignal};
use crate::kernel::live_kernels;
use crate::kernel::orphan_journal;
use crate::kernel::protocol::{parse_event, Event, Request, REPL_PROTOCOL_VERSION};
use crate::kernel::shared::*;
use crate::kernel::state_snapshot::{
    RestoreResult, SnapshotResult, SnapshotSkip, DEFAULT_SNAPSHOT_MAX_BYTES,
    DEFAULT_SNAPSHOT_MAX_VARIABLE_BYTES,
};

const READY_TIMEOUT_MS: u64 = 30_000;
const REPAIR_STEP_TIMEOUT_MS: u64 = 30_000;
/// Runtime-minted host-request ids never repeat; the bound only guards a
/// misbehaving runtime from growing the dedup set forever.
const MAX_HANDLED_HOST_REQUEST_IDS: usize = 1024;

/// Progress callback for kernel bootstrap (`ensure_kernel_python`).
pub type KernelBootstrapProgressHandler = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
pub struct KernelStartOptions {
    pub signal: Option<AbortSignal>,
    pub on_bootstrap_progress: Option<KernelBootstrapProgressHandler>,
}

/// Lock a mutex, surviving poisoning: the guarded state is plain data, and a
/// panicked reader must not cascade into an unusable kernel.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KernelState {
    Idle,
    Starting,
    Running,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExitInfo {
    code: Option<i32>,
    signal: Option<i32>,
}

/// Fields of a settled execution shared with the stdout reader task.
#[derive(Default)]
struct ExecBuffers {
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
    result: Option<String>,
    diffs: Vec<KernelDiffDisplay>,
    attachments: Vec<KernelAttachment>,
    attachment_oversized: bool,
    sent_agent_messages: Vec<KernelSentAgentMessage>,
    background_output: String,
    background_output_truncated: bool,
    error: Option<KernelError>,
    status: ExecuteStatus,
    done_fields: Option<Value>,
    settled: bool,
}

struct ActiveExecution {
    request_id: String,
    code: String,
    started: Instant,
    max_chars: usize,
    opts: ExecuteOptions,
    buffers: Mutex<ExecBuffers>,
    result_tx: Mutex<Option<oneshot::Sender<anyhow::Result<InternalExecuteResult>>>>,
}

/// ExecuteResult plus the raw fields of the request's `done` event (state ops).
struct InternalExecuteResult {
    result: ExecuteResult,
    done_fields: Option<Value>,
}

impl InternalExecuteResult {
    fn aborted(started: Instant) -> Self {
        Self {
            result: ExecuteResult {
                stdout: String::new(),
                stderr: String::new(),
                result: None,
                diffs: None,
                attachments: None,
                sent_agent_messages: None,
                background_output: None,
                status: ExecuteStatus::Aborted,
                error: None,
                duration_ms: started.elapsed().as_millis() as u64,
            },
            done_fields: None,
        }
    }
}

/// A memoized one-shot operation (start / shutdown / repair / rebootstrap /
/// flush) that concurrent callers join, with an optional abort-aware wait.
struct MemoSlot {
    done: AtomicBool,
    failed: Mutex<Option<String>>,
    notify: Notify,
}

impl MemoSlot {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            done: AtomicBool::new(false),
            failed: Mutex::new(None),
            notify: Notify::new(),
        })
    }

    fn outcome(&self) -> anyhow::Result<()> {
        match &*lock(&self.failed) {
            Some(err) => Err(anyhow!("{err}")),
            None => Ok(()),
        }
    }

    async fn wait(&self) -> anyhow::Result<()> {
        loop {
            if self.done.load(Ordering::SeqCst) {
                return self.outcome();
            }
            self.notify.notified().await;
        }
    }

    /// Wait for completion, returning an abort error as soon as `signal` fires.
    async fn wait_or_abort(
        &self,
        signal: Option<&AbortSignal>,
        message: &str,
    ) -> anyhow::Result<()> {
        match signal {
            None => self.wait().await,
            Some(signal) => {
                if signal.is_aborted() {
                    return Err(anyhow!("{message}"));
                }
                let wait = self.wait();
                tokio::select! {
                    r = wait => r,
                    _ = signal.cancelled() => Err(anyhow!("{message}")),
                }
            }
        }
    }

    fn finish(&self, error: Option<anyhow::Error>) {
        if let Some(err) = error {
            *lock(&self.failed) = Some(format!("{err:#}"));
        }
        self.done.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

struct RepairOwner {
    superseded: AtomicBool,
}

struct RepairHandle {
    owner: Arc<RepairOwner>,
    slot: Arc<MemoSlot>,
}

struct Guarded {
    state: KernelState,
    start_generation: u64,
    /// Generation whose graceful shutdown() owns the teardown, so the exit
    /// handler must not run it.
    graceful_shutdown_generation: Option<u64>,
    teardown_in_flight: u32,
    startup_protocol_error: Option<String>,
    /// A repair discarded its kernel: the next fresh start must re-run the runtime bootstrap.
    pending_rebootstrap: bool,
    /// Restore the saved namespace on that fresh start too (false when the
    /// snapshot itself is the declared culprit).
    pending_restore: bool,
    /// Unattributed stream text that arrived between cells; surfaced on the next execution.
    pending_background_output: String,
    pending_background_output_truncated: bool,
    flushing_snapshot_for_dispose: bool,
    protocol_repair: Option<Arc<RepairHandle>>,
    kernel_stderr: String,
    background_bash_handles: HashMap<String, i32>,
    handled_host_request_ids: (HashSet<String>, VecDeque<String>),
    /// Late agent-message handlers keyed by request id, insertion-ordered with eviction.
    late_handlers: VecDeque<(String, LateSentAgentMessageCallback)>,
    /// Resolvers for done events outside the active execution (the shutdown reply).
    pending_done_waiters: HashMap<String, oneshot::Sender<()>>,
    host_inflight: Vec<tokio::task::JoinHandle<()>>,
    active_execution: Option<Arc<ActiveExecution>>,
    /// Source of the most recently started cell, retained after it finishes so
    /// rlm.run spawns from detached asyncio tasks (cell already idle) can
    /// still attribute their spawning program.
    last_cell_code: Option<String>,
    ready_tx: Option<oneshot::Sender<anyhow::Result<i64>>>,
}

struct ChildHandle {
    pid: i32,
    stdin: Arc<tokio::sync::Mutex<Option<tokio::process::ChildStdin>>>,
    exit_rx: tokio::sync::watch::Receiver<Option<ExitInfo>>,
}

/// The RLM kernel manager: owns one `python -m rlm.repl` subprocess and the
/// JSON-lines protocol v3 conversation with it.
#[derive(Clone)]
pub struct ReplKernelManager {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for ReplKernelManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplKernelManager")
            .field("running", &self.is_running())
            .field("defunct", &self.is_defunct())
            .field("owner_session_id", &self.owner_session_id())
            .finish()
    }
}

pub(crate) struct Inner {
    options: KernelManagerOptions,
    resolved_python: Mutex<Option<std::path::PathBuf>>,
    guarded: Mutex<Guarded>,
    child: Mutex<Option<ChildHandle>>,
    busy_notify: Notify,
    /// Serializes execute() calls — the runtime runs one request at a time.
    execution_queue: tokio::sync::Mutex<()>,
    start_memo: Mutex<Option<Arc<MemoSlot>>>,
    shutdown_memo: Mutex<Option<Arc<MemoSlot>>>,
    rebootstrap_memo: Mutex<Option<Arc<MemoSlot>>>,
    flush_memo: Mutex<Option<Arc<MemoSlot>>>,
    snapshot_timer: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stderr_closed: Notify,
    stderr_closed_flag: AtomicBool,
    /// File receiving pre-ready kernel stderr, with its remaining write budget.
    stderr_log: Mutex<Option<Arc<Mutex<StderrLog>>>>,
}

struct StderrLog {
    file: std::fs::File,
    budget: u64,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Synchronous best-effort cleanup, mirroring disposeSync().
        self.supersede_protocol_repair();
        lock(&self.guarded).state = KernelState::Shutdown;
        live_kernels::remove_inner(self);
        self.cleanup_resources(Signal::Kill);
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

impl ReplKernelManager {
    pub fn new(options: KernelManagerOptions) -> Self {
        let inner = Arc::new(Inner {
            options,
            resolved_python: Mutex::new(None),
            guarded: Mutex::new(Guarded {
                state: KernelState::Idle,
                start_generation: 0,
                graceful_shutdown_generation: None,
                teardown_in_flight: 0,
                startup_protocol_error: None,
                pending_rebootstrap: false,
                pending_restore: false,
                pending_background_output: String::new(),
                pending_background_output_truncated: false,
                flushing_snapshot_for_dispose: false,
                protocol_repair: None,
                kernel_stderr: String::new(),
                background_bash_handles: HashMap::new(),
                handled_host_request_ids: (HashSet::new(), VecDeque::new()),
                late_handlers: VecDeque::new(),
                pending_done_waiters: HashMap::new(),
                host_inflight: Vec::new(),
                active_execution: None,
                last_cell_code: None,
                ready_tx: None,
            }),
            child: Mutex::new(None),
            busy_notify: Notify::new(),
            execution_queue: tokio::sync::Mutex::new(()),
            start_memo: Mutex::new(None),
            shutdown_memo: Mutex::new(None),
            rebootstrap_memo: Mutex::new(None),
            flush_memo: Mutex::new(None),
            snapshot_timer: Mutex::new(None),
            stderr_closed: Notify::new(),
            stderr_closed_flag: AtomicBool::new(false),
            stderr_log: Mutex::new(None),
        });
        Self { inner }
    }

    pub fn owner_session_id(&self) -> Option<&str> {
        self.inner.options.session_id.as_deref()
    }

    pub fn has_background_work(&self) -> bool {
        !lock(&self.inner.guarded).background_bash_handles.is_empty()
    }

    /// Process id of the spawned kernel child, when present. Used by tests and
    /// orphan bookkeeping; not part of the TS surface.
    pub fn process_id(&self) -> Option<i32> {
        lock(&self.inner.child).as_ref().map(|c| c.pid)
    }

    pub fn is_running(&self) -> bool {
        lock(&self.inner.guarded).state == KernelState::Running
    }

    /// Terminal: the kernel died or was torn down; only a fresh manager can serve again.
    pub fn is_defunct(&self) -> bool {
        lock(&self.inner.guarded).state == KernelState::Shutdown
    }

    /// Diagnostics tail (kernel stderr, at most the last 8 KiB).
    pub fn kernel_stderr(&self) -> String {
        lock(&self.inner.guarded).kernel_stderr.clone()
    }

    // ---------------------------------------------------------------- start

    /// Start the kernel, memoizing concurrent callers onto one startup.
    /// An aborted signal abandons the wait without stopping the underlying
    /// startup, mirroring the TS `raceStartupWithAbort`.
    pub async fn start(&self, options: KernelStartOptions) -> anyhow::Result<()> {
        if let Some(signal) = &options.signal {
            if signal.is_aborted() {
                return Err(anyhow!("Kernel startup aborted"));
            }
        }
        // The guard is strictly scoped: a conditionally dropped non-Send
        // MutexGuard would make the whole future non-Send.
        let existing = lock(&self.inner.start_memo).as_ref().cloned();
        if let Some(existing) = existing {
            return existing
                .wait_or_abort(options.signal.as_ref(), "Kernel startup aborted")
                .await;
        }
        let slot = {
            let mut memo = lock(&self.inner.start_memo);
            let slot = MemoSlot::new();
            *memo = Some(slot.clone());
            slot
        };
        // Owner: run the startup to completion in a task so the caller\'s abort
        // signal can abandon the wait without killing the kernel for others.
        let inner = self.inner.clone();
        let run_slot = slot.clone();
        let wait_signal = options.signal.clone();
        let task = tokio::spawn(async move {
            let result = inner.do_start(&options).await;
            run_slot.finish(result.as_ref().err().map(|e| anyhow!("{e:#}")));
            if result.is_err() {
                let mut memo = lock(&inner.start_memo);
                // Only clear our own memoization: a stale start must not evict a newer one.
                if matches!(memo.as_ref(), Some(current) if Arc::ptr_eq(current, &run_slot)) {
                    *memo = None;
                }
            }
            result
        });
        match wait_signal.as_ref() {
            None => match task.await {
                Ok(result) => result,
                Err(join_error) => {
                    slot.finish(Some(anyhow!("{join_error}")));
                    Err(anyhow!("{join_error}"))
                }
            },
            Some(signal) => {
                let wait = slot.wait();
                tokio::select! {
                    r = wait => {
                        // Join the task to keep it observable; its result is already in the slot.
                        let _ = task.await;
                        r
                    }
                    _ = signal.cancelled() => {
                        // The startup keeps running for other callers.
                        Err(anyhow!("Kernel startup aborted"))
                    }
                }
            }
        }
    }

    // -------------------------------------------------------------- execute

    /// Execute one cell. Refreshes the on-disk snapshot after real work so a
    /// later resume (or a crash before graceful shutdown) revives the most
    /// recent namespace.
    pub async fn execute(&self, code: &str, opts: ExecuteOptions) -> anyhow::Result<ExecuteResult> {
        self.wait_for_protocol_repair(opts.signal.as_ref()).await?;
        let result = self.enqueue_execute(code, opts, None).await?;
        if result.result.status == ExecuteStatus::Ok {
            self.schedule_snapshot();
        }
        Ok(result.result)
    }

    /// Queue and run a cell, serializing against all other executions.
    async fn enqueue_execute(
        &self,
        code: &str,
        opts: ExecuteOptions,
        execution_timeout_ms: Option<u64>,
    ) -> anyhow::Result<InternalExecuteResult> {
        self.enqueue_request(
            Request::Execute {
                code: code.to_string(),
            },
            code,
            opts,
            execution_timeout_ms,
        )
        .await
    }

    // -------------------------------------------------------- state ops API

    /// Serialize the user namespace to disk (best-effort, per-variable).
    /// `None` when the kernel isn\'t running or no snapshot target was
    /// configured. Never fails on kernel errors; they land in diagnostics.
    pub async fn snapshot_state(&self) -> Option<SnapshotResult> {
        self.capture_snapshot(None, false).await
    }

    /// Persist the namespace, then remove variables above the per-variable cap.
    pub async fn prune_oversized_variables(&self) -> Option<SnapshotResult> {
        self.capture_snapshot(Some(SNAPSHOT_EXECUTION_TIMEOUT_MS), true)
            .await
    }

    /// Revive a previously snapshotted namespace into the kernel. Call right
    /// after start() and before the runtime bootstrap, which then refreshes
    /// live handles (rlm, skills) over anything restored.
    pub async fn restore_state(&self) -> Option<RestoreResult> {
        self.perform_restore(false).await
    }

    /// Live user-defined top-level names, or `None` if the kernel isn\'t running.
    pub async fn list_namespace_names(&self, signal: Option<AbortSignal>) -> Option<Vec<String>> {
        if !self.is_running() {
            return None;
        }
        let opts = ExecuteOptions {
            internal: true,
            signal,
            ..ExecuteOptions::default()
        };
        match self
            .enqueue_request(Request::ListNames, "", opts, None)
            .await
        {
            Ok(r) if r.result.status == ExecuteStatus::Ok => {
                let names = r
                    .done_fields
                    .as_ref()
                    .and_then(|fields| fields.get("names"))
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                Some(names)
            }
            Ok(r) => {
                self.inner.append_diagnostic(&format!(
                    "namespace listing failed: {}",
                    describe_failure(&r.result)
                ));
                None
            }
            Err(error) => {
                self.inner
                    .append_diagnostic(&format!("namespace listing error: {error:#}"));
                None
            }
        }
    }

    // ----------------------------------------------------- lifecycle (rest)

    /// Resolves `true` when this call performed the cleanup (false: a
    /// concurrent teardown won; a joiner\'s options are ignored — the first
    /// caller\'s policy wins).
    pub async fn shutdown(&self, opts: KernelShutdownOptions) -> anyhow::Result<bool> {
        let existing = lock(&self.inner.shutdown_memo).as_ref().cloned();
        if let Some(existing) = existing {
            let _ = existing.wait().await;
            return Ok(false);
        }
        let slot = {
            let mut memo = lock(&self.inner.shutdown_memo);
            let slot = MemoSlot::new();
            *memo = Some(slot.clone());
            slot
        };
        lock(&self.inner.guarded).teardown_in_flight += 1;
        self.supersede_protocol_repair();
        let inner = self.inner.clone();
        let result = tokio::spawn(async move {
            let performed = inner.perform_shutdown(opts).await;
            slot.finish(None);
            let mut memo = lock(&inner.shutdown_memo);
            if matches!(memo.as_ref(), Some(current) if Arc::ptr_eq(current, &slot)) {
                *memo = None;
            }
            performed
        })
        .await
        .unwrap_or(false);
        lock(&self.inner.guarded).teardown_in_flight -= 1;
        Ok(result)
    }

    pub async fn restart(&self) -> anyhow::Result<()> {
        // A final dispose flush owns the queue tail. Taking a slot now and
        // joining the in-flight shutdown would deadlock: the flush\'s snapshot
        // waits on our slot while we wait on the flush\'s shutdown.
        if lock(&self.inner.guarded).flushing_snapshot_for_dispose {
            return Err(anyhow!("Kernel is shutting down"));
        }
        let _queue_guard = self.inner.execution_queue.lock().await;
        let performed = self.shutdown(KernelShutdownOptions::default()).await?;
        if !performed {
            return Ok(());
        }
        lock(&self.inner.guarded).state = KernelState::Idle;
        lock(&self.inner.guarded).kernel_stderr.clear();
        self.start(KernelStartOptions::default()).await
    }

    pub async fn kill(&self) {
        self.supersede_protocol_repair();
        {
            let mut g = lock(&self.inner.guarded);
            g.state = KernelState::Shutdown;
        }
        live_kernels::remove_inner(&self.inner);
        self.inner.cleanup_resources(Signal::Kill);
    }

    /// Synchronous best-effort cleanup. Safe to call from drop paths.
    pub fn dispose_sync(&self) {
        self.supersede_protocol_repair();
        lock(&self.inner.guarded).state = KernelState::Shutdown;
        live_kernels::remove_inner(&self.inner);
        self.inner.cleanup_resources(Signal::Term);
    }
}

// ---------------------------------------------------------------------------
// Request plumbing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub(crate) enum Signal {
    Term,
    Kill,
}

impl Signal {
    #[cfg(unix)]
    fn as_libc(self) -> i32 {
        match self {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        }
    }
}

/// Append stream text, capping the buffer at `max_chars` and flagging truncation.
fn append_truncated(buffer: &mut String, truncated: &mut bool, text: &str, max_chars: usize) {
    if buffer.chars().count() < max_chars {
        buffer.push_str(text);
        if buffer.chars().count() > max_chars {
            let chars: Vec<char> = buffer.chars().take(max_chars).collect();
            buffer.clear();
            buffer.extend(chars);
            *truncated = true;
        }
    }
}

fn describe_failure(result: &ExecuteResult) -> String {
    if let Some(error) = &result.error {
        if error.evalue.is_empty() {
            return error.ename.clone();
        }
        return format!("{}: {}", error.ename, error.evalue);
    }
    result.stderr.trim_end().to_string()
}

impl ReplKernelManager {
    /// Queue one protocol request (execute or state op) behind every other request.
    #[allow(clippy::too_many_lines)]
    async fn enqueue_request(
        &self,
        request: Request,
        code: &str,
        opts: ExecuteOptions,
        execution_timeout_ms: Option<u64>,
    ) -> anyhow::Result<InternalExecuteResult> {
        let started = Instant::now();
        if let Some(signal) = &opts.signal {
            if signal.is_aborted() {
                return Ok(InternalExecuteResult::aborted(started));
            }
        }
        self.start(KernelStartOptions {
            signal: opts.signal.clone(),
            on_bootstrap_progress: None,
        })
        .await?;
        if self.state() == KernelState::Shutdown {
            return Err(anyhow!("Kernel has been shut down"));
        }
        if lock(&self.inner.guarded).flushing_snapshot_for_dispose && !opts.internal {
            return Err(anyhow!("Kernel is shutting down"));
        }
        if !opts.protocol_repair {
            self.ensure_kernel_rebootstrapped(opts.signal.as_ref())
                .await?;
        }
        // Aborted while waiting on the re-bootstrap: settle now instead of
        // parking on the queue slot behind the still-running bootstrap.
        if let Some(signal) = &opts.signal {
            if signal.is_aborted() {
                return Ok(InternalExecuteResult::aborted(started));
            }
        }
        // Re-check: a final flush may have started while this request awaited
        // the lazy re-bootstrap; admitting it now would splice it between the
        // flush\'s captured queue and the final snapshot, unbounding the teardown.
        if lock(&self.inner.guarded).flushing_snapshot_for_dispose && !opts.internal {
            return Err(anyhow!("Kernel is shutting down"));
        }

        let queue_guard = self.inner.execution_queue.lock().await;

        // A repair started while this request was queued or busy-waiting:
        // release the slot so the repair\'s own restore can run, then requeue
        // behind it.
        if lock(&self.inner.guarded).protocol_repair.is_some() && !opts.protocol_repair {
            drop(queue_guard);
            self.wait_for_protocol_repair(opts.signal.as_ref()).await?;
            return Box::pin(self.enqueue_request(request, code, opts, execution_timeout_ms)).await;
        }

        self.wait_for_active_execution_to_clear_for_reuse(opts.signal.as_ref())
            .await?;
        if let Some(signal) = &opts.signal {
            if signal.is_aborted() {
                return Ok(InternalExecuteResult::aborted(started));
            }
        }
        if self.state() == KernelState::Shutdown {
            return Err(anyhow!("Kernel has been shut down"));
        }

        // Bound the execution with an out-of-band abort (interrupt + grace).
        let timeout_signal = execution_timeout_ms.map(|ms| {
            let signal = AbortSignal::new();
            let timer_signal = signal.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                timer_signal.abort();
            });
            signal
        });
        let merged = merge_signals(opts.signal.as_ref(), timeout_signal.clone());
        let mut opts = opts;
        opts.signal = merged;
        let result = self.execute_inner(request, code, opts, started).await;
        if let Some(signal) = &timeout_signal {
            signal.abort();
        }
        result
    }

    fn state(&self) -> KernelState {
        lock(&self.inner.guarded).state
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_inner(
        &self,
        request: Request,
        code: &str,
        opts: ExecuteOptions,
        started: Instant,
    ) -> anyhow::Result<InternalExecuteResult> {
        let max_chars = opts.max_output_chars.unwrap_or(DEFAULT_MAX_OUTPUT_CHARS);
        let request_id = uuid::Uuid::new_v4().to_string();

        if let Some(signal) = &opts.signal {
            if signal.is_aborted() {
                return Ok(InternalExecuteResult::aborted(started));
            }
        }
        if lock(&self.inner.guarded).active_execution.is_some() {
            return Err(anyhow!("Kernel already has an active execution"));
        }

        let (result_tx, mut result_rx) =
            oneshot::channel::<anyhow::Result<InternalExecuteResult>>();
        let mut buffers = ExecBuffers {
            status: ExecuteStatus::Ok,
            ..ExecBuffers::default()
        };
        {
            let mut g = lock(&self.inner.guarded);
            buffers.background_output = std::mem::take(&mut g.pending_background_output);
            buffers.background_output_truncated =
                std::mem::replace(&mut g.pending_background_output_truncated, false);
            g.active_execution = None; // reset below with the execution in hand
        }
        let execution = Arc::new(ActiveExecution {
            request_id: request_id.clone(),
            code: code.to_string(),
            started,
            max_chars,
            buffers: Mutex::new(buffers),
            result_tx: Mutex::new(Some(result_tx)),
            opts,
        });
        {
            let mut g = lock(&self.inner.guarded);
            g.active_execution = Some(execution.clone());
        }

        // Abort watcher: interrupts the kernel out-of-band, then force-aborts
        // after the grace window if the runtime did not settle the cell.
        if let Some(signal) = execution.opts.signal.clone() {
            let inner = self.inner.clone();
            let weak_exec = Arc::downgrade(&execution);
            tokio::spawn(async move {
                signal.cancelled().await;
                let Some(execution) = weak_exec.upgrade() else {
                    return;
                };
                let _ = inner.interrupt(Some(&execution.request_id)).await;
                tokio::time::sleep(Duration::from_millis(KERNEL_ABORT_GRACE_MS)).await;
                // The execution stays active until its done event arrives;
                // clearing it early would let a new cell race the interrupted
                // one (see busy-after-interrupt).
                inner.force_abort(&execution);
            });
        }

        if !execution.opts.internal {
            lock(&self.inner.guarded).last_cell_code = Some(code.to_string());
        }

        let mut frame = request.to_json();
        frame["id"] = json!(request_id);

        let mut send_task = {
            let writer = lock(&self.inner.child).as_ref().map(|c| c.stdin.clone());
            let Some(stdin) = writer else {
                {
                    let mut g = lock(&self.inner.guarded);
                    g.active_execution = None;
                }
                return Err(anyhow!("Kernel stdin is not connected"));
            };
            let mut line = frame.to_string();
            line.push('\n');
            tokio::spawn(async move {
                let mut guard = stdin.lock().await;
                let Some(stdin) = guard.as_mut() else {
                    return Err(anyhow!("Kernel stdin is not connected"));
                };
                stdin.write_all(line.as_bytes()).await?;
                stdin.flush().await?;
                Ok(())
            })
        };

        let mut settled_result: Option<anyhow::Result<InternalExecuteResult>> = None;
        let send_outcome: anyhow::Result<()> = {
            let send_promise = &mut send_task;
            tokio::select! {
                r = send_promise => r.unwrap_or_else(|e| Err(anyhow!("{e}"))),
                settled = &mut result_rx => {
                    // The cell settled before the write completed (fast runtime).
                    // Only an aborted status may skip waiting for the write; a
                    // failed write on a successful cell must surface.
                    let settled: anyhow::Result<InternalExecuteResult> = match settled {
                        Ok(result) => result,
                        Err(_) => Err(anyhow!("Kernel has been shut down")),
                    };
                    let early_settle = matches!(&settled, Ok(result) if result.result.status == ExecuteStatus::Aborted);
                    if !early_settle {
                        // Surfacing a failed write outranks the settled cell.
                        if let Err(error) = (&mut send_task).await.unwrap_or_else(|e| Err(anyhow!("{e}"))) {
                            settled_result = Some(Err(error));
                        } else {
                            settled_result = Some(settled);
                        }
                    } else {
                        settled_result = Some(settled);
                    }
                    Ok(())
                }
            }
        };

        match settled_result.take() {
            Some(result) => result,
            None => match send_outcome {
                Err(error) => {
                    {
                        let mut g = lock(&self.inner.guarded);
                        if matches!(g.active_execution.as_ref(), Some(active) if Arc::ptr_eq(active, &execution))
                        {
                            g.active_execution = None;
                        }
                    }
                    Err(error)
                }
                Ok(()) => match result_rx.await {
                    Ok(result) => result,
                    Err(_) => Err(anyhow!("Kernel has been shut down")),
                },
            },
        }
    }
}

impl Inner {
    /// Write one JSON-lines request frame; completes when the OS accepted the bytes.
    async fn write_line(&self, frame: &Value) -> anyhow::Result<()> {
        let stdin = {
            let child = lock(&self.child);
            child
                .as_ref()
                .map(|c| c.stdin.clone())
                .ok_or_else(|| anyhow!("Kernel stdin is not connected"))?
        };
        let mut guard = stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(anyhow!("Kernel stdin is not connected"));
        };
        let mut line = frame.to_string();
        line.push('\n');
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn interrupt(&self, id: Option<&str>) -> anyhow::Result<()> {
        let mut frame = json!({ "type": "interrupt" });
        if let Some(id) = id {
            frame["id"] = json!(id);
        }
        self.write_line(&frame).await
    }

    fn force_abort(&self, execution: &Arc<ActiveExecution>) {
        {
            let g = lock(&self.guarded);
            let is_active = matches!(g.active_execution.as_ref(), Some(active) if Arc::ptr_eq(active, execution));
            if !is_active {
                return;
            }
        }
        lock(&execution.buffers).status = ExecuteStatus::Aborted;
        self.resolve_execution(execution, false);
    }

    /// Wait until no execution is active, interrupting a busy cell, bounded by
    /// the busy-reuse window.
    pub(crate) async fn wait_for_active_execution_to_clear_for_reuse(
        self: &Arc<Self>,
        signal: Option<&AbortSignal>,
    ) -> anyhow::Result<()> {
        let started = Instant::now();
        loop {
            let notified = self.busy_notify.notified();
            {
                let g = lock(&self.guarded);
                if g.active_execution.is_none() {
                    return Ok(());
                }
                if g.state == KernelState::Shutdown {
                    return Err(anyhow!("Kernel has been shut down"));
                }
            }
            let elapsed = started.elapsed();
            if elapsed >= Duration::from_millis(KERNEL_BUSY_REUSE_WAIT_MS) {
                break;
            }
            let wait_ms = (KERNEL_BUSY_REUSE_WAIT_MS - elapsed.as_millis() as u64)
                .min(KERNEL_BUSY_INTERRUPT_INTERVAL_MS);
            let wait = tokio::time::sleep(Duration::from_millis(wait_ms.max(1)));
            let active_id = lock(&self.guarded)
                .active_execution
                .as_ref()
                .map(|a| a.request_id.clone());
            let interrupt = {
                let inner = Arc::clone(self);
                tokio::spawn(async move {
                    let _ = inner.interrupt(active_id.as_deref()).await;
                })
            };
            tokio::select! {
                () = notified => {}
                _ = wait => {}
                _ = async {
                    match signal {
                        Some(signal) => signal.cancelled().await,
                        None => std::future::pending().await,
                    }
                } => {
                    interrupt.abort();
                    return Ok(());
                }
            }
        }
        if lock(&self.guarded).active_execution.is_some() {
            return Err(anyhow!("{}", KERNEL_BUSY_AFTER_INTERRUPT_MESSAGE));
        }
        Ok(())
    }

    fn notify_active_execution_idle(&self) {
        self.busy_notify.notify_waiters();
    }

    fn reject_active_execution(&self, message: &str) {
        let execution = {
            let mut g = lock(&self.guarded);
            g.active_execution.take()
        };
        if let Some(execution) = execution {
            if let Some(tx) = lock(&execution.result_tx).take() {
                let _ = tx.send(Err(anyhow!("{message}")));
            }
            self.notify_active_execution_idle();
        }
    }

    fn finish_active_execution(&self, execution: &Arc<ActiveExecution>) {
        let is_active = {
            let g = lock(&self.guarded);
            matches!(g.active_execution.as_ref(), Some(active) if Arc::ptr_eq(active, execution))
        };
        if !is_active {
            return;
        }
        self.resolve_execution(execution, true);
    }

    fn resolve_execution(&self, execution: &Arc<ActiveExecution>, clear_active: bool) {
        let did_clear_active = if clear_active {
            let mut g = lock(&self.guarded);
            matches!(g.active_execution.as_ref(), Some(active) if Arc::ptr_eq(active, execution))
                .then(|| {
                    g.active_execution = None;
                    true
                })
                .unwrap_or(false)
        } else {
            false
        };
        let mut buffers = lock(&execution.buffers);
        if !buffers.settled {
            buffers.settled = true;
            if let Some(callback) = execution.opts.on_late_sent_agent_message.clone() {
                self.register_late_sent_agent_message_handler(&execution.request_id, callback);
            }

            let mut stdout = std::mem::take(&mut buffers.stdout);
            let mut stderr = std::mem::take(&mut buffers.stderr);
            let mut result = buffers.result.take();
            let mut status = buffers.status;
            if buffers.stdout_truncated {
                stdout.push_str(&format!(
                    "\n[... output truncated at {} chars ...]",
                    execution.max_chars
                ));
            }
            if buffers.stderr_truncated {
                stderr.push_str(&format!(
                    "\n[... output truncated at {} chars ...]",
                    execution.max_chars
                ));
            }
            if let Some(text) = &result {
                if text.len() > execution.max_chars {
                    let mut clipped = text[..execution.max_chars.clamp(0, text.len())].to_string();
                    // Trim at a char boundary when max_chars split a multi-byte char.
                    while !clipped.is_char_boundary(clipped.len()) {
                        clipped.pop();
                    }
                    clipped.push_str(&format!(
                        "\n[... output truncated at {} chars ...]",
                        execution.max_chars
                    ));
                    result = Some(clipped);
                }
            }
            if execution
                .opts
                .signal
                .as_ref()
                .map(AbortSignal::is_aborted)
                .unwrap_or(false)
            {
                status = ExecuteStatus::Aborted;
            }

            let mut background_output = std::mem::take(&mut buffers.background_output);
            if buffers.background_output_truncated {
                background_output.push_str(&format!(
                    "\n[... background output truncated at {} chars ...]",
                    MAX_BACKGROUND_OUTPUT_CHARS
                ));
            }
            let done_fields = buffers.done_fields.take();
            let result = ExecuteResult {
                stdout,
                stderr,
                result,
                diffs: (!buffers.diffs.is_empty()).then(|| std::mem::take(&mut buffers.diffs)),
                attachments: (!buffers.attachments.is_empty())
                    .then(|| std::mem::take(&mut buffers.attachments)),
                sent_agent_messages: (!buffers.sent_agent_messages.is_empty())
                    .then(|| std::mem::take(&mut buffers.sent_agent_messages)),
                background_output: (!background_output.is_empty()).then_some(background_output),
                status,
                error: buffers.error.take(),
                duration_ms: execution.started.elapsed().as_millis() as u64,
            };
            drop(buffers);
            if let Some(tx) = lock(&execution.result_tx).take() {
                let _ = tx.send(Ok(InternalExecuteResult {
                    result,
                    done_fields,
                }));
            }
        }
        if did_clear_active {
            self.notify_active_execution_idle();
        }
    }

    fn register_late_sent_agent_message_handler(
        &self,
        request_id: &str,
        callback: LateSentAgentMessageCallback,
    ) {
        let mut g = lock(&self.guarded);
        g.late_handlers
            .retain(|(existing, _)| existing != request_id);
        g.late_handlers
            .push_back((request_id.to_string(), callback));
        while g.late_handlers.len() > MAX_LATE_SENT_AGENT_MESSAGE_HANDLERS {
            g.late_handlers.pop_front();
        }
    }

    fn dispatch_late_sent_agent_message(&self, request_id: Option<&str>, data: &Value) -> bool {
        let Some(request_id) = request_id else {
            return false;
        };
        let Some(payload) = data.get(AGENT_MESSAGE_DISPLAY_MIME) else {
            return false;
        };
        let Some(message) = parse_sent_agent_message(payload) else {
            return false;
        };
        let callback = {
            let mut g = lock(&self.guarded);
            let Some(position) = g.late_handlers.iter().position(|(id, _)| id == request_id) else {
                return false;
            };
            let callback = g
                .late_handlers
                .remove(position)
                .expect("position checked")
                .1;
            // Refresh recency, matching the TS map delete+set.
            g.late_handlers
                .push_back((request_id.to_string(), callback.clone()));
            callback
        };
        callback(message);
        true
    }
}

// ---------------------------------------------------------------------------
// Startup and child wiring
// ---------------------------------------------------------------------------

impl Inner {
    /// True when a teardown (or newer start) superseded the start that
    /// captured `generation`.
    fn start_stale(&self, generation: u64) -> bool {
        lock(&self.guarded).start_generation != generation
    }

    /// Child stderr tail for error messages: at most the last 1024 chars.
    fn stderr_tail(&self, limit: usize) -> String {
        let stderr = lock(&self.guarded).kernel_stderr.clone();
        let chars: Vec<char> = stderr.chars().collect();
        let start = chars.len().saturating_sub(limit);
        chars[start..].iter().collect()
    }

    /// Append raw kernel stderr text to the diagnostics tail (last 8 KiB).
    fn append_kernel_stderr_text(&self, text: &str) {
        let mut g = lock(&self.guarded);
        g.kernel_stderr.push_str(text);
        if g.kernel_stderr.len() > MAX_KERNEL_STDERR_CHARS {
            let chars: Vec<char> = g.kernel_stderr.chars().collect();
            let start = chars.len().saturating_sub(MAX_KERNEL_STDERR_CHARS);
            g.kernel_stderr = chars[start..].iter().collect();
        }
    }

    /// Append one `[kernel]`-prefixed diagnostic line.
    fn append_diagnostic(&self, message: &str) {
        let line = if message.ends_with('\n') {
            format!("[kernel] {message}")
        } else {
            format!("[kernel] {message}\n")
        };
        self.append_kernel_stderr_text(&line);
    }

    /// Open (and rotate when oversized) the kernel stderr log. The write budget
    /// is the file's remaining capacity, so current and `.old` each stay near
    /// the ceiling.
    fn open_stderr_log(&self) -> Option<Arc<Mutex<StderrLog>>> {
        let path = self.options.stderr_log_path.as_ref()?;
        match self.open_stderr_log_at(path) {
            Ok(log) => Some(log),
            Err(error) => {
                self.append_diagnostic(&format!("cannot open kernel stderr log: {error}"));
                None
            }
        }
    }

    fn open_stderr_log_at(&self, path: &std::path::Path) -> anyhow::Result<Arc<Mutex<StderrLog>>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut size = path.metadata().map(|m| m.len()).unwrap_or(0);
        if size > MAX_KERNEL_STDERR_LOG_BYTES {
            let old = path.with_extension("log.old");
            let _ = std::fs::remove_file(&old);
            match std::fs::rename(path, &old) {
                Ok(()) => size = 0,
                Err(error) => {
                    // A failed rotation must not cost the log: keep appending.
                    self.append_diagnostic(&format!("cannot rotate kernel stderr log: {error}"));
                }
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Arc::new(Mutex::new(StderrLog {
            file,
            budget: MAX_KERNEL_STDERR_LOG_BYTES.saturating_sub(size),
        })))
    }

    /// The Python interpreter this kernel runs on: the explicit option when
    /// present, else the auto-bootstrapped kernel venv.
    async fn resolve_python(
        self: &Arc<Self>,
        options: &KernelStartOptions,
    ) -> anyhow::Result<std::path::PathBuf> {
        if let Some(python) = &self.options.python {
            return Ok(python.clone());
        }
        if let Some(cached) = lock(&self.resolved_python).clone() {
            return Ok(cached);
        }
        let progress = options.on_bootstrap_progress.clone();
        let skills = self.options.python_skills.clone();
        let python = crate::kernel::bootstrap::ensure_kernel_python(
            crate::kernel::bootstrap::EnsureKernelPythonOptions {
                python_skills: skills,
                on_progress: progress,
            },
        )
        .await?;
        *lock(&self.resolved_python) = Some(python.clone());
        Ok(python)
    }

    /// Perform the startup: resolve the interpreter, spawn `python -m rlm.repl`,
    /// complete the protocol handshake, and mark the kernel running.
    pub(crate) async fn do_start(
        self: &Arc<Self>,
        options: &KernelStartOptions,
    ) -> anyhow::Result<()> {
        {
            let mut g = lock(&self.guarded);
            if g.state != KernelState::Idle {
                return Ok(());
            }
            g.start_generation += 1;
            g.state = KernelState::Starting;
        }
        // Tracked from the moment startup begins so cleanup can dispose a
        // kernel that is still booting.
        live_kernels::add(self);

        let python = match self.resolve_python(options).await {
            Ok(python) => python,
            Err(error) => {
                if self.start_stale(self.current_generation()) {
                    // Never touch a newer start's state.
                    return Err(error);
                }
                live_kernels::remove(self);
                let mut g = lock(&self.guarded);
                if g.state != KernelState::Shutdown {
                    g.state = KernelState::Idle;
                }
                return Err(error);
            }
        };
        let generation = self.current_generation();
        if self.start_stale(generation) {
            return Err(anyhow!("Kernel start superseded"));
        }
        if lock(&self.guarded).state == KernelState::Shutdown {
            return Err(anyhow!("Kernel was disposed during startup"));
        }

        // bash.py journals its process groups under this pid so the host can
        // reap them if the runtime dies without running its shutdown hook.
        let mut env: HashMap<String, String> = std::env::vars().collect();
        for (key, value) in &self.options.env {
            env.insert(key.clone(), value.clone());
        }
        env.insert(
            "PRIME_AGENT_KERNEL_OWNER_PID".to_string(),
            std::process::id().to_string(),
        );
        let cwd = self.options.cwd.clone();
        let mut command = tokio::process::Command::new(&python);
        command
            .args(["-m", "rlm.repl"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(cwd) = &cwd {
            command.current_dir(cwd);
        }
        command.env_clear().envs(env);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                // Fail a pending start promptly instead of riding out the
                // ready timeout.
                self.append_diagnostic(&format!("spawn error: {error}"));
                {
                    let mut g = lock(&self.guarded);
                    g.state = KernelState::Shutdown;
                }
                live_kernels::remove(self);
                if let Some(tx) = lock(&self.guarded).ready_tx.take() {
                    let _ = tx.send(Err(anyhow!("spawn error: {error}")));
                }
                self.cleanup_resources(Signal::Term);
                return Err(anyhow!(
                    "failed to spawn kernel python {}: {error}",
                    python.display()
                ));
            }
        };
        let pid = child.id().map(|p| p as i32).unwrap_or(-1);
        orphan_journal::record_orphan_process_state(pid, true);
        let (ready_tx, ready_rx) = oneshot::channel::<anyhow::Result<i64>>();
        {
            let mut g = lock(&self.guarded);
            g.startup_protocol_error = None;
            g.ready_tx = Some(ready_tx);
        }
        self.wire_child(child, generation);

        let protocol = match self.wait_for_ready(ready_rx, generation).await {
            Ok(protocol) => protocol,
            Err(error) => {
                if self.start_stale(generation) {
                    // Never tear down a newer start's kernel.
                    return Err(error);
                }
                let can_retry_startup = lock(&self.guarded).state != KernelState::Shutdown;
                // Only the call that performed the cleanup may resurrect to
                // idle; a concurrent kill()/teardown owns the state otherwise.
                let performed = self
                    .perform_shutdown(KernelShutdownOptions::default())
                    .await;
                if performed && can_retry_startup {
                    lock(&self.guarded).state = KernelState::Idle;
                }
                return Err(error);
            }
        };
        if self.start_stale(generation) {
            return Err(anyhow!("Kernel start superseded"));
        }
        if let Some(startup_error) = lock(&self.guarded).startup_protocol_error.clone() {
            return Err(anyhow!("{startup_error}"));
        }
        if protocol != REPL_PROTOCOL_VERSION as i64 {
            return Err(anyhow!(
                "Kernel runtime speaks protocol {protocol}, expected {REPL_PROTOCOL_VERSION}. \
                 Update prime-agent-runtime in the kernel Python (PRIME_AGENT_KERNEL_PYTHON) to match this prime-agent."
            ));
        }
        lock(&self.guarded).state = KernelState::Running;
        Ok(())
    }

    fn current_generation(&self) -> u64 {
        lock(&self.guarded).start_generation
    }

    /// Wire the spawned child: protocol reader, stderr tail + log, and the
    /// exit watcher that settles the manager when the process dies.
    fn wire_child(self: &Arc<Self>, mut child: tokio::process::Child, generation: u64) {
        let pid = child.id().map(|p| p as i32).unwrap_or(-1);
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
        let (exit_tx, exit_rx) = tokio::sync::watch::channel::<Option<ExitInfo>>(None);
        *lock(&self.child) = Some(ChildHandle {
            pid,
            stdin: stdin.clone(),
            exit_rx: exit_rx.clone(),
        });

        if let Some(stdout) = stdout {
            let inner = Arc::clone(self);
            let stdin_for_error = stdin.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let trimmed = line.trim_end_matches('\n');
                            if trimmed.trim().is_empty() {
                                continue;
                            }
                            match parse_event(trimmed) {
                                Ok(event) => inner.handle_event(event),
                                Err(reason) => {
                                    let _ = stdin_for_error;
                                    inner.fail_protocol_frame(generation, &reason);
                                }
                            }
                        }
                    }
                }
            });
        }

        if let Some(stderr) = stderr {
            let inner = Arc::clone(self);
            let log = self.open_stderr_log();
            // Keep the host-side handle so teardown drops its reference; the
            // reader task's handle closes the file when the stream ends.
            *lock(&self.stderr_log) = log.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut pending: Vec<u8> = Vec::new();
                let mut buffer = [0u8; 4096];
                // Incremental UTF-8 decoding: a chunk boundary can split a
                // multi-byte character, so only complete sequences surface.
                let decode = |pending: &mut Vec<u8>| {
                    let text = String::from_utf8_lossy(pending);
                    let decoded = text.into_owned();
                    pending.clear();
                    decoded
                };
                loop {
                    match reader.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            pending.extend_from_slice(&buffer[..n]);
                            inner.append_kernel_stderr_text(&decode(&mut pending));
                            if let Some(log) = &log {
                                let mut log = lock(log);
                                if log.budget >= n as u64 {
                                    let _ = log.file.write_all(&buffer[..n]);
                                    let _ = log.file.flush();
                                    log.budget -= n as u64;
                                } else if log.budget > 0 {
                                    let _ = log
                                        .file
                                        .write_all(KERNEL_STDERR_LOG_BUDGET_MARKER.as_bytes());
                                    let _ = log.file.flush();
                                    log.budget = 0;
                                }
                            }
                        }
                    }
                }
                if !pending.is_empty() {
                    inner.append_kernel_stderr_text(&decode(&mut pending));
                }
                // Both the natural EOF and the kill path end here; the ready
                // handshake waits on this notification for the final tail.
                inner.stderr_closed_flag.store(true, Ordering::SeqCst);
                inner.stderr_closed.notify_waiters();
            });
        }

        let inner = Arc::clone(self);
        tokio::spawn(async move {
            let exit = match child.wait().await {
                Ok(status) => ExitInfo {
                    code: status.code(),
                    #[cfg(unix)]
                    signal: unix_signal_of(&status),
                    #[cfg(not(unix))]
                    signal: None,
                },
                Err(_) => ExitInfo {
                    code: None,
                    signal: None,
                },
            };
            let _ = exit_tx.send(Some(exit));
            if inner.start_stale(generation) {
                return;
            }
            // append_diagnostic re-locks `guarded`, so it must run OUTSIDE
            // this lock scope (a std Mutex is not reentrant).
            let was_live = {
                let mut g = lock(&inner.guarded);
                let was_live = g.state != KernelState::Shutdown;
                g.state = KernelState::Shutdown;
                was_live
            };
            if was_live {
                inner.append_diagnostic(&format!(
                    "unexpected exit code={} signal={}",
                    exit.code
                        .map(|c| c.to_string())
                        .unwrap_or("null".to_string()),
                    exit.signal
                            .map(|s| s.to_string())
                            .unwrap_or("null".to_string()),
                ));
            }
            live_kernels::remove(&inner);
            // This exit is part of an in-flight graceful shutdown(): that call
            // owns the teardown and runs cleanup itself.
            // Scoped reads: locking the same std Mutex twice in one
            // expression self-deadlocks (non-reentrant).
            let graceful_in_flight = {
                let g = lock(&inner.guarded);
                g.graceful_shutdown_generation == Some(g.start_generation)
            };
            if graceful_in_flight {
                return;
            }
            inner.cleanup_resources(Signal::Term);
        });
    }

    /// The protocol handshake: the runtime's `ready` frame, the child dying
    /// first, or the 30-second ceiling — whichever comes first.
    async fn wait_for_ready(
        &self,
        ready_rx: oneshot::Receiver<anyhow::Result<i64>>,
        generation: u64,
    ) -> anyhow::Result<i64> {
        let mut ready_rx = ready_rx;
        let mut exit_rx = lock(&self.child)
            .as_ref()
            .map(|c| c.exit_rx.clone())
            .ok_or_else(|| anyhow!("Kernel ready state is missing"))?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(READY_TIMEOUT_MS);
        let mut protocol = None;
        while protocol.is_none() {
            let timeout = tokio::time::sleep_until(deadline);
            tokio::select! {
                ready = &mut ready_rx => {
                    match ready {
                        Ok(Ok(value)) => protocol = Some(value),
                        Ok(Err(error)) => return Err(error),
                        // Dropped only by teardown, which resolves the wait itself.
                        Err(_) => return Err(anyhow!("Kernel ready state is missing")),
                    }
                }
                _ = exit_rx.changed() => {
                    if exit_rx.borrow().is_some() {
                        // Final stderr chunks can still be in flight; wait for
                        // the drained pipe (the ready deadline still bounds it).
                        let stderr_drain = async {
                            loop {
                                if self.stderr_closed_flag.load(Ordering::SeqCst) {
                                    return;
                                }
                                self.stderr_closed.notified().await;
                            }
                        };
                        tokio::select! {
                            () = stderr_drain => {}
                            () = tokio::time::sleep_until(deadline) => {}
                        }
                        let tail = self.stderr_tail(1024);
                        return Err(anyhow!("Kernel exited before ready. stderr:\n{}", if tail.is_empty() { "(empty)".to_string() } else { tail }));
                    }
                }
                () = timeout => {
                    let tail = self.stderr_tail(1024);
                    return Err(anyhow!("Kernel did not become ready within {READY_TIMEOUT_MS}ms. stderr tail:\n{}", if tail.is_empty() { "(empty)".to_string() } else { tail }));
                }
            }
        }
        let _ = generation;
        Ok(protocol.expect("loop only exits with a protocol value"))
    }
}

#[cfg(unix)]
fn unix_signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn unix_signal_of(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

// ---------------------------------------------------------------------------
// Event dispatch
// ---------------------------------------------------------------------------

impl Inner {
    fn handle_event(self: &Arc<Self>, event: Event) {
        match event {
            Event::Display { id, ref data } => {
                if let Some(activity) = data.get(BASH_ACTIVITY_DISPLAY_MIME) {
                    let obj = match activity.as_object() {
                        Some(obj) => obj,
                        None => return,
                    };
                    let activity_id = obj.get("id").and_then(Value::as_str).unwrap_or_default();
                    let pid = obj.get("pid").and_then(Value::as_i64).unwrap_or_default();
                    let active = obj.get("active").and_then(Value::as_bool);
                    if activity_id.len() == 32
                        && activity_id.chars().all(|c| c.is_ascii_hexdigit())
                        && pid > 0
                        && matches!(active, Some(true) | Some(false))
                    {
                        let mut g = lock(&self.guarded);
                        if active == Some(true) {
                            g.background_bash_handles
                                .entry(activity_id.to_string())
                                .or_insert(pid as i32);
                        } else if g.background_bash_handles.get(activity_id) == Some(&(pid as i32))
                        {
                            g.background_bash_handles.remove(activity_id);
                        }
                    }
                    return;
                }
                self.dispatch_display(id.as_deref(), data);
            }
            Event::Ready { protocol } => {
                if let Some(tx) = lock(&self.guarded).ready_tx.take() {
                    let _ = tx.send(Ok(protocol));
                }
            }
            Event::HostRequest { id, data } => self.start_host_request(&id, data),
            Event::Stdout { id, text } => {
                self.route_stream(id.as_deref(), StreamName::Stdout, &text)
            }
            Event::Stderr { id, text } => {
                self.route_stream(id.as_deref(), StreamName::Stderr, &text)
            }
            Event::Result { id, text } => {
                let execution = lock(&self.guarded).active_execution.clone();
                if let Some(execution) = execution.filter(|e| e.request_id == id) {
                    lock(&execution.buffers).result = Some(text);
                }
            }
            Event::Error {
                id,
                ename,
                evalue,
                traceback,
            } => {
                let execution = lock(&self.guarded).active_execution.clone();
                match (execution.filter(|e| Some(&e.request_id) == id.as_ref()), id) {
                    (Some(execution), _) => {
                        let mut buffers = lock(&execution.buffers);
                        buffers.error = Some(KernelError {
                            ename,
                            evalue,
                            traceback,
                        });
                        buffers.status = ExecuteStatus::Error;
                    }
                    (None, None) => {
                        // A protocol-level error without a cell id is runtime noise.
                        self.append_diagnostic(&format!("protocol error: {evalue}"));
                    }
                    (None, Some(_)) => {}
                }
            }
            Event::Done { id, fields } => {
                let status = fields
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("error");
                let execution = lock(&self.guarded).active_execution.clone();
                if let Some(execution) = execution.filter(|e| e.request_id == id) {
                    {
                        let mut buffers = lock(&execution.buffers);
                        buffers.done_fields = Some(fields.clone());
                        if status != "ok" && buffers.status == ExecuteStatus::Ok {
                            buffers.status = ExecuteStatus::Error;
                            // State requests report failures as a done reason
                            // without an error event.
                            if buffers.error.is_none() {
                                if let Some(reason) = fields.get("reason").and_then(Value::as_str) {
                                    buffers.error = Some(KernelError {
                                        ename: "KernelError".to_string(),
                                        evalue: reason.to_string(),
                                        traceback: Vec::new(),
                                    });
                                }
                            }
                        }
                    }
                    self.finish_active_execution(&execution);
                    return;
                }
                // A done outside the active execution settles its waiter.
                let waiter = lock(&self.guarded).pending_done_waiters.remove(&id);
                if let Some(tx) = waiter {
                    let _ = tx.send(());
                }
            }
        }
    }

    /// stdout/stderr events: attributed to the active execution when the id
    /// matches, otherwise buffered as background output.
    fn route_stream(&self, id: Option<&str>, stream: StreamName, text: &str) {
        let execution = lock(&self.guarded).active_execution.clone();
        let Some(execution) = execution.filter(|e| Some(e.request_id.as_str()) == id) else {
            // Unowned output (null id, or another cell's id): never merge it
            // into the active cell's streams; buffer it as background output.
            self.append_background_output(text);
            return;
        };
        let mut buffers = lock(&execution.buffers);
        let ExecBuffers {
            stdout,
            stdout_truncated,
            stderr,
            stderr_truncated,
            ..
        } = &mut *buffers;
        match stream {
            StreamName::Stdout => {
                append_truncated(stdout, stdout_truncated, text, execution.max_chars)
            }
            StreamName::Stderr => {
                append_truncated(stderr, stderr_truncated, text, execution.max_chars)
            }
        }
        drop(buffers);
        if let Some(on_stream) = &execution.opts.on_stream {
            on_stream(text, stream);
        }
    }

    /// display events: diffs, attachments, sent agent messages, late or live.
    fn dispatch_display(&self, id: Option<&str>, data: &Value) {
        let execution = lock(&self.guarded).active_execution.clone();
        let matching = execution
            .as_ref()
            .filter(|e| Some(e.request_id.as_str()) == id);
        // A settled cell keeps receiving late agent messages via its handler.
        if matching.is_none() {
            if self.dispatch_late_sent_agent_message(id, data) {
                return;
            }
            return;
        }
        let execution = matching.expect("filter guarantees presence").clone();
        let settled = lock(&execution.buffers).settled;
        if settled && self.dispatch_late_sent_agent_message(id, data) {
            return;
        }
        let mut buffers = lock(&execution.buffers);
        if let Some(payload) = data.get(DIFF_DISPLAY_MIME) {
            if let Some(diff) = parse_diff_display(payload) {
                buffers.diffs.push(diff);
            }
        }
        match data
            .get(ATTACHMENT_DISPLAY_MIME)
            .and_then(parse_attachment_display)
        {
            Some(Err(_)) => {
                buffers.attachment_oversized = true;
                if !buffers.stderr.is_empty() {
                    buffers.stderr.push('\n');
                }
                buffers.stderr.push_str(&format!(
                    "attachment dropped: exceeds {MAX_ATTACHMENT_DATA_CHARS} base64 chars"
                ));
                buffers.status = ExecuteStatus::Error;
            }
            Some(Ok(attachment)) => buffers.attachments.push(attachment),
            None => {}
        }
        if let Some(payload) = data.get(AGENT_MESSAGE_DISPLAY_MIME) {
            if let Some(message) = parse_sent_agent_message(payload) {
                buffers.sent_agent_messages.push(message);
            }
        }
    }

    /// Unattributed stream text: attached to the active cell's background
    /// buffer, or held for the next cell when the kernel is idle.
    fn append_background_output(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let execution = lock(&self.guarded).active_execution.clone();
        if let Some(execution) = execution {
            let mut buffers = lock(&execution.buffers);
            if buffers.background_output.chars().count() >= MAX_BACKGROUND_OUTPUT_CHARS {
                buffers.background_output_truncated = true;
                return;
            }
            buffers.background_output.push_str(text);
            if buffers.background_output.chars().count() > MAX_BACKGROUND_OUTPUT_CHARS {
                let chars: Vec<char> = buffers
                    .background_output
                    .chars()
                    .take(MAX_BACKGROUND_OUTPUT_CHARS)
                    .collect();
                buffers.background_output.clear();
                buffers.background_output.extend(chars);
                buffers.background_output_truncated = true;
            }
            return;
        }
        let mut g = lock(&self.guarded);
        if g.pending_background_output.chars().count() >= MAX_BACKGROUND_OUTPUT_CHARS {
            g.pending_background_output_truncated = true;
            return;
        }
        g.pending_background_output.push_str(text);
        if g.pending_background_output.chars().count() > MAX_BACKGROUND_OUTPUT_CHARS {
            let chars: Vec<char> = g
                .pending_background_output
                .chars()
                .take(MAX_BACKGROUND_OUTPUT_CHARS)
                .collect();
            g.pending_background_output.clear();
            g.pending_background_output.extend(chars);
            g.pending_background_output_truncated = true;
        }
    }
}

// ---------------------------------------------------------------------------
// Protocol repair
// ---------------------------------------------------------------------------

impl Inner {
    /// A JSON object that was not a valid protocol frame: fail the in-flight
    /// request and replace the child, since its stream framing is corrupted.
    fn fail_protocol_frame(self: &Arc<Self>, generation: u64, diagnostic: &str) {
        if self.start_stale(generation) {
            return;
        }
        self.append_diagnostic(diagnostic);
        let error = format!("Kernel protocol error: {diagnostic}");
        {
            let mut g = lock(&self.guarded);
            if g.state == KernelState::Starting {
                g.startup_protocol_error = Some(error.clone());
            }
        }
        if let Some(tx) = lock(&self.guarded).ready_tx.take() {
            let _ = tx.send(Err(anyhow!("{error}")));
        }
        self.reject_active_execution(&error);
        {
            let g = lock(&self.guarded);
            if g.teardown_in_flight > 0 || g.state != KernelState::Running {
                return;
            }
        }
        let existing = lock(&self.guarded).protocol_repair.clone();
        if let Some(existing) = existing {
            // A repair's own replacement child corrupted: discard it instead
            // of respawn-looping.
            self.append_diagnostic(
                "replacement kernel corrupted during protocol repair; giving up",
            );
            existing.owner.superseded.store(true, Ordering::SeqCst);
            // performRestore clears pendingRestore, so it still being set
            // means the corruption struck at or before the restore phase: the
            // snapshot stays the prime suspect (ambiguous attribution,
            // loop-safe). Corruption strictly after a successful restore never
            // implicates the snapshot.
            let snapshot_suspect = lock(&self.guarded).pending_restore;
            self.kill_child_to_idle();
            if snapshot_suspect {
                lock(&self.guarded).pending_restore = false;
            }
            return;
        }
        let owner = Arc::new(RepairOwner {
            superseded: AtomicBool::new(false),
        });
        let handle = Arc::new(RepairHandle {
            owner: owner.clone(),
            slot: MemoSlot::new(),
        });
        lock(&self.guarded).protocol_repair = Some(handle.clone());
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            inner.repair_protocol_child(generation, owner.clone()).await;
            let superseded = handle.owner.superseded.load(Ordering::SeqCst);
            let current = lock(&inner.guarded).protocol_repair.clone();
            if matches!(&current, Some(current) if Arc::ptr_eq(&current.owner, &handle.owner))
                && !superseded
            {
                handle.slot.finish(None);
            } else {
                handle
                    .slot
                    .finish(Some(anyhow!("protocol repair superseded")));
            }
            let mut g = lock(&inner.guarded);
            if matches!(&g.protocol_repair, Some(current) if Arc::ptr_eq(&current.owner, &handle.owner))
            {
                g.protocol_repair = None;
            }
        });
    }

    /// Replace the corrupted child: fresh spawn, restore the saved namespace,
    /// re-run the runtime bootstrap. Never lets a failure wedge the kernel.
    async fn repair_protocol_child(self: &Arc<Self>, generation: u64, owner: Arc<RepairOwner>) {
        if self.start_stale(generation) || lock(&self.guarded).state == KernelState::Shutdown {
            return;
        }
        self.kill_child_to_idle();

        if let Err(error) = self.do_start(&KernelStartOptions::default()).await {
            self.finish_failed_protocol_repair(&owner, Some(format!("{error:#}")));
            return;
        }
        let generation = self.current_generation();
        if self.start_stale(generation) || lock(&self.guarded).state != KernelState::Running {
            self.finish_failed_protocol_repair(&owner, None);
            return;
        }

        let restored = self.perform_restore(true).await;
        if self.start_stale(generation) || lock(&self.guarded).state != KernelState::Running {
            self.finish_failed_protocol_repair(&owner, None);
            return;
        }
        if self.options.snapshot.is_some() && restored.is_none() {
            if self.repair_superseded(&owner) {
                return;
            }
            self.append_diagnostic("protocol repair restore failed; discarding replacement kernel");
            self.kill_child_to_idle();
            // The snapshot is the declared culprit; the lazy path must not retry it.
            lock(&self.guarded).pending_restore = false;
            return;
        }

        // Restore revives only the user namespace; live handles (rlm, bash,
        // skills) come from the runtime bootstrap, so a repaired kernel must
        // re-run it.
        let Some(code) = self.options.bootstrap_code.clone() else {
            return;
        };
        let bootstrapped = self.bootstrap_repaired_kernel(&code).await;
        if self.start_stale(generation) || lock(&self.guarded).state != KernelState::Running {
            self.finish_failed_protocol_repair(&owner, None);
            return;
        }
        if !bootstrapped {
            if self.repair_superseded(&owner) {
                return;
            }
            self.append_diagnostic(
                "protocol repair bootstrap failed; discarding replacement kernel",
            );
            self.kill_child_to_idle();
        }
    }

    fn repair_superseded(&self, owner: &Arc<RepairOwner>) -> bool {
        if owner.superseded.load(Ordering::SeqCst) {
            return true;
        }
        let current = lock(&self.guarded).protocol_repair.clone();
        !matches!(&current, Some(handle) if Arc::ptr_eq(&handle.owner, owner))
    }

    /// Bounded bootstrap of a repaired kernel; `false` when it failed. Never throws.
    async fn bootstrap_repaired_kernel(self: &Arc<Self>, code: &str) -> bool {
        // Boxed: enqueue -> rebootstrap -> reprovision -> this call is a
        // recursive cycle, and recursive async fns need one boxed link.
        let result = Box::pin(self.enqueue_request(
            Request::Execute {
                code: code.to_string(),
            },
            code,
            ExecuteOptions {
                internal: true,
                protocol_repair: true,
                ..ExecuteOptions::default()
            },
            Some(REPAIR_STEP_TIMEOUT_MS),
        ))
        .await;
        match result {
            Ok(r) if r.result.status == ExecuteStatus::Ok => {
                lock(&self.guarded).pending_rebootstrap = false;
                true
            }
            Ok(r) => {
                let detail = r
                    .result
                    .error
                    .as_ref()
                    .map(|e| e.evalue.clone())
                    .unwrap_or_else(|| r.result.stderr.trim_end().to_string());
                self.append_diagnostic(&format!("protocol repair bootstrap failed: {detail}"));
                false
            }
            Err(error) => {
                self.append_diagnostic(&format!("protocol repair bootstrap error: {error:#}"));
                false
            }
        }
    }

    /// A fresh kernel started after a discarded repair has none of the runtime
    /// bootstrap's live handles (rlm, bash, skills) and an empty namespace:
    /// reprovision (restore, then bootstrap) before any user request.
    async fn ensure_kernel_rebootstrapped(
        self: &Arc<Self>,
        signal: Option<&AbortSignal>,
    ) -> anyhow::Result<()> {
        let code = self.options.bootstrap_code.clone();
        let needs_restore = self.options.snapshot.is_some() && lock(&self.guarded).pending_restore;
        let needs_bootstrap = code.is_some() && lock(&self.guarded).pending_rebootstrap;
        // An in-flight repair owns its kernel's restore/bootstrap sequence,
        // and a teardown's final snapshot must never trigger reprovisioning.
        if (!needs_restore && !needs_bootstrap)
            || lock(&self.guarded).protocol_repair.is_some()
            || lock(&self.guarded).teardown_in_flight > 0
            || lock(&self.guarded).state != KernelState::Running
        {
            return Ok(());
        }
        let task = {
            let mut memo = lock(&self.rebootstrap_memo);
            match memo.as_ref() {
                Some(existing) => existing.clone(),
                None => {
                    let inner = Arc::clone(self);
                    let slot = MemoSlot::new();
                    let run_slot = slot.clone();
                    tokio::spawn(async move {
                        let ok = inner.reprovision_fresh_kernel().await;
                        run_slot.finish(
                            (!ok).then(|| anyhow!("Kernel bootstrap failed after protocol repair")),
                        );
                    });
                    *memo = Some(slot.clone());
                    slot
                }
            }
        };
        // An aborted request never executes, so it may skip the wait; race
        // the signal instead of riding out the bootstrap bound after an abort.
        match signal {
            None => task.wait().await,
            Some(signal) => {
                if signal.is_aborted() {
                    return Ok(());
                }
                tokio::select! {
                    result = task.wait() => result,
                    _ = signal.cancelled() => Ok(()),
                }
            }
        }
    }

    /// Restore (one-shot, best-effort) then bootstrap the lazily started fresh kernel.
    async fn reprovision_fresh_kernel(self: &Arc<Self>) -> bool {
        if self.options.snapshot.is_some() && lock(&self.guarded).pending_restore {
            self.perform_restore(true).await; // clears pendingRestore on success
                                              // Corrupted during the restore: the spawned repair owns the kernel now.
            if lock(&self.guarded).protocol_repair.is_some()
                || lock(&self.guarded).state != KernelState::Running
            {
                return false;
            }
            // One attempt per discard: a clean restore failure falls back to an
            // empty namespace (ordinary startup semantics), never a retry loop.
            lock(&self.guarded).pending_restore = false;
        }
        let Some(code) = self.options.bootstrap_code.clone() else {
            return true;
        };
        if !lock(&self.guarded).pending_rebootstrap {
            return true;
        }
        let ok = self.bootstrap_repaired_kernel(&code).await;
        if !ok && lock(&self.guarded).state == KernelState::Running {
            self.kill_child_to_idle();
        }
        ok
    }

    /// Kill the current child and settle at clean idle, so the next start spawns fresh.
    fn kill_child_to_idle(self: &Arc<Self>) {
        // The discarded kernel carried the runtime bootstrap and (possibly) the
        // restored namespace; a lazily started replacement must reprovision both.
        {
            let mut g = lock(&self.guarded);
            g.pending_rebootstrap = true;
            g.pending_restore = true;
            g.state = KernelState::Shutdown;
        }
        live_kernels::remove(self);
        self.cleanup_resources(Signal::Kill);
        lock(&self.guarded).state = KernelState::Idle;
    }

    fn finish_failed_protocol_repair(&self, owner: &Arc<RepairOwner>, error: Option<String>) {
        if let Some(error) = error {
            self.append_diagnostic(&format!("protocol repair start failed: {error}"));
        }
        if owner.superseded.load(Ordering::SeqCst) || !self.repair_owner_is(owner) {
            return;
        }
        if lock(&self.guarded).state == KernelState::Shutdown {
            lock(&self.guarded).state = KernelState::Idle;
        }
    }

    fn repair_owner_is(&self, owner: &Arc<RepairOwner>) -> bool {
        let current = lock(&self.guarded).protocol_repair.clone();
        matches!(&current, Some(handle) if Arc::ptr_eq(&handle.owner, owner))
    }

    pub(crate) fn supersede_protocol_repair(&self) {
        if let Some(handle) = &lock(&self.guarded).protocol_repair {
            handle.owner.superseded.store(true, Ordering::SeqCst);
        }
    }

    /// Wait until no protocol repair is pending; resolves early when the signal aborts.
    async fn wait_for_protocol_repair(&self, signal: Option<&AbortSignal>) -> anyhow::Result<()> {
        loop {
            let Some(repair) = lock(&self.guarded).protocol_repair.clone() else {
                return Ok(());
            };
            match signal {
                None => repair.slot.wait().await?,
                Some(signal) => {
                    if signal.is_aborted() {
                        return Ok(());
                    }
                    tokio::select! {
                        result = repair.slot.wait() => result?,
                        _ = signal.cancelled() => return Ok(()),
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Host requests
// ---------------------------------------------------------------------------

impl Inner {
    /// Dispatch one typed request from kernel code to the registered handler
    /// and reply over the protocol. Unhandled requests answer with an error.
    fn start_host_request(self: &Arc<Self>, request_id: &str, data: Value) {
        {
            let mut g = lock(&self.guarded);
            let (seen, order) = &mut g.handled_host_request_ids;
            if seen.contains(request_id) {
                return;
            }
            seen.insert(request_id.to_string());
            order.push_back(request_id.to_string());
            while seen.len() > MAX_HANDLED_HOST_REQUEST_IDS {
                if let Some(oldest) = order.pop_front() {
                    seen.remove(&oldest);
                } else {
                    break;
                }
            }
        }
        let inner = Arc::clone(self);
        let request_id = request_id.to_string();
        let task = tokio::spawn(async move {
            let result = inner.handle_host_request(&data).await;
            let reply = match result {
                Ok(result) => json!({ "status": "ok", "result": result }),
                Err(error) => {
                    inner.append_diagnostic(&format!(
                        "host request failed for {request_id}: {error:#}"
                    ));
                    json!({ "status": "error", "error": format!("{error:#}") })
                }
            };
            let frame = json!({ "type": "host_reply", "id": request_id, "data": reply });
            if let Err(error) = inner.write_line(&frame).await {
                inner.append_diagnostic(&format!(
                    "failed to send host request reply for {request_id}: {error:#}"
                ));
            }
        });
        let mut g = lock(&self.guarded);
        // Completed task handles are dropped so the inflight set stays bounded.
        g.host_inflight.retain(|handle| !handle.is_finished());
        g.host_inflight.push(task);
    }

    async fn handle_host_request(&self, data: &Value) -> anyhow::Result<Value> {
        let Some(obj) = data.as_object() else {
            return Err(anyhow!("host request payload must be an object"));
        };
        let request_type = obj
            .get("type")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow!("host request payload must have a string type"))?;
        let handler = self
            .options
            .host_handlers
            .get(request_type)
            .ok_or_else(|| {
                anyhow!("host request type \"{request_type}\" is not available in this session")
            })?
            .clone();
        // Tag the request with the cell that triggered it. A blocking call is
        // still the in-flight execution; detached spawns fire after the
        // scheduling cell goes idle, so fall back to that last cell's source.
        let cell_source_code = {
            let g = lock(&self.guarded);
            g.active_execution
                .as_ref()
                .map(|e| e.code.clone())
                .or_else(|| g.last_cell_code.clone())
        };
        let mut payload = obj.clone();
        if let Some(code) = cell_source_code {
            payload.insert("cellSourceCode".to_string(), Value::String(code));
        }
        handler(HostRequestPayload {
            data: Value::Object(payload),
            cell_source_code: None,
        })
        .await
    }

    /// Wait (bounded) for the in-flight host request tasks to settle.
    async fn wait_for_host_requests_to_settle(
        &self,
        tasks: Vec<tokio::task::JoinHandle<()>>,
        timeout_ms: u64,
    ) {
        let all = async {
            for task in tasks {
                let _ = task.await;
            }
        };
        if tokio::time::timeout(Duration::from_millis(timeout_ms), all)
            .await
            .is_err()
        {
            self.append_diagnostic(&format!(
                "timed out waiting {timeout_ms}ms for host request task(s) during shutdown"
            ));
        }
    }

    async fn wait_for_kernel_exit(&self) {
        let exit_rx = match lock(&self.child).as_ref() {
            Some(child) => child.exit_rx.clone(),
            None => return,
        };
        let mut exit_rx = exit_rx;
        if exit_rx.borrow().is_some() {
            return;
        }
        loop {
            if exit_rx.borrow().is_some() {
                return;
            }
            if exit_rx.changed().await.is_err() {
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Teardown
// ---------------------------------------------------------------------------

impl Inner {
    /// Resolves `true` when this call performed the cleanup (false: a
    /// concurrent teardown won; a joiner's options are ignored — the first
    /// caller's policy wins). The memoization joins concurrent callers onto
    /// one in-flight shutdown.
    pub(crate) async fn shutdown_for_cleanup(
        self: &Arc<Self>,
        opts: KernelShutdownOptions,
    ) -> anyhow::Result<bool> {
        let existing = lock(&self.shutdown_memo).as_ref().cloned();
        if let Some(existing) = existing {
            let _ = existing.wait().await;
            return Ok(false);
        }
        let slot = {
            let mut memo = lock(&self.shutdown_memo);
            let slot = MemoSlot::new();
            *memo = Some(slot.clone());
            slot
        };
        lock(&self.guarded).teardown_in_flight += 1;
        self.supersede_protocol_repair();
        let performed = self.perform_shutdown(opts).await;
        lock(&self.guarded).teardown_in_flight -= 1;
        slot.finish(None);
        {
            let mut memo = lock(&self.shutdown_memo);
            if matches!(memo.as_ref(), Some(current) if Arc::ptr_eq(current, &slot)) {
                *memo = None;
            }
        }
        Ok(performed)
    }

    async fn perform_shutdown(self: &Arc<Self>, opts: KernelShutdownOptions) -> bool {
        if lock(&self.guarded).state == KernelState::Shutdown {
            live_kernels::remove(self);
            // Scoped read: re-locking inside the comparison self-deadlocks.
            let graceful_in_flight = {
                let g = lock(&self.guarded);
                g.graceful_shutdown_generation == Some(g.start_generation)
            };
            if graceful_in_flight {
                return false;
            }
            self.cleanup_resources(Signal::Term);
            return true;
        }
        // Captured before any await: teardowns and newer starts bump the counter.
        let generation = lock(&self.guarded).start_generation;
        if opts.snapshot {
            self.flush_snapshot_for_dispose().await;
            if self.start_stale(generation) {
                return false;
            }
        }
        // Protocol shutdown first: the runtime closes MCP servers and kills
        // live bash() process groups a bare hard-kill would leak.
        let protocol_shutdown_available = lock(&self.guarded).state == KernelState::Running;
        {
            let mut g = lock(&self.guarded);
            g.state = KernelState::Shutdown;
            g.graceful_shutdown_generation = Some(generation);
        }
        live_kernels::remove(self);

        let mut performed_cleanup = false;
        let mut request_id: Option<String> = None;
        if opts.drain_host_requests {
            let in_flight = {
                let mut g = lock(&self.guarded);
                std::mem::take(&mut g.host_inflight)
            };
            if !in_flight.is_empty() {
                self.wait_for_host_requests_to_settle(in_flight, HOST_REQUEST_SHUTDOWN_TIMEOUT_MS)
                    .await;
            }
        }
        if protocol_shutdown_available
            && !self.start_stale(generation)
            && lock(&self.child).is_some()
        {
            let id = uuid::Uuid::new_v4().to_string();
            let (done_tx, done_rx) = oneshot::channel::<()>();
            lock(&self.guarded)
                .pending_done_waiters
                .insert(id.clone(), done_tx);
            request_id = Some(id.clone());
            let frame = json!({ "type": "shutdown", "id": id });
            let send_result = self.write_line(&frame).await;
            if let Err(error) = send_result {
                self.append_diagnostic(&format!("failed to send shutdown request: {error:#}"));
            }
            let graceful_reply = async {
                let _ = done_rx.await;
            };
            let deadline = tokio::time::sleep(Duration::from_millis(KERNEL_SHUTDOWN_TIMEOUT_MS));
            let mut failed = false;
            tokio::select! {
                () = graceful_reply => {}
                () = self.wait_for_kernel_exit() => {}
                () = deadline => { eprintln!("DBG: select1 -> deadline");
                    failed = true;
                    self.append_diagnostic(&format!(
                        "graceful shutdown failed (killing instead): Kernel did not shut down within {KERNEL_SHUTDOWN_TIMEOUT_MS}ms"
                    ));
                }
            }
            if !failed {
                let deadline =
                    tokio::time::sleep(Duration::from_millis(KERNEL_SHUTDOWN_TIMEOUT_MS));
                tokio::select! {
                    () = self.wait_for_kernel_exit() => {}
                    () = deadline => {}
                }
            }
        }
        if let Some(id) = request_id {
            lock(&self.guarded).pending_done_waiters.remove(&id);
        }
        {
            let mut g = lock(&self.guarded);
            if g.graceful_shutdown_generation == Some(generation) {
                g.graceful_shutdown_generation = None;
            }
        }
        if !self.start_stale(generation) {
            self.cleanup_resources(Signal::Term);
            performed_cleanup = true;
        }
        performed_cleanup
    }

    /// Tear the child down: stop timers, fail pending work, close pipes, kill
    /// the process, and reap any bash() process groups it journaled.
    pub(crate) fn cleanup_resources(&self, kill_signal: Signal) {
        {
            let mut g = lock(&self.guarded);
            // Any teardown invalidates in-flight starts.
            g.start_generation += 1;
            if let Some(timer) = lock(&self.snapshot_timer).take() {
                timer.abort();
            }
            g.late_handlers.clear();
            g.pending_done_waiters.clear();
            g.background_bash_handles.clear();
            // Stale pre-teardown background output must not surface after a restart.
            g.pending_background_output.clear();
            g.pending_background_output_truncated = false;
        }
        self.reject_active_execution("Kernel has been shut down");
        *lock(&self.stderr_log) = None;
        let child = lock(&self.child).take();
        lock(&self.guarded).ready_tx.take();
        if let Some(child) = child {
            // Dropping the write pipe signals EOF to the child's stdin reader;
            // closing stdin is equivalent to a shutdown request.
            if let Ok(mut stdin) = child.stdin.try_lock() {
                *stdin = None;
            }
            let pid = child.pid;
            let signaled = kill_process(pid, kill_signal);
            // Inactive only when the signal proved the pid still named our child.
            if pid > 0 && signaled {
                orphan_journal::record_orphan_process_state(pid, false);
            }
            // A killed/crashed kernel cannot run its own shutdown hook, so the
            // host reaps the bash() process groups it journaled under this pid.
            if pid > 0 {
                orphan_journal::reap_kernel_orphan_processes(pid);
            }
        }
        *lock(&self.start_memo) = None;
    }
}

#[cfg(unix)]
fn kill_process(pid: i32, signal: Signal) -> bool {
    if pid <= 0 {
        return false;
    }
    let sig = signal.as_libc();
    unsafe { libc::kill(pid, sig) == 0 }
}

#[cfg(not(unix))]
fn kill_process(pid: i32, signal: Signal) -> bool {
    let _ = (pid, signal);
    false
}

// ---------------------------------------------------------------------------
// Snapshot / restore
// ---------------------------------------------------------------------------

impl Inner {
    /// Serialize the user namespace to disk (best-effort, per-variable).
    /// `None` when the kernel isn't running or no snapshot target was
    /// configured. Never fails on kernel errors; they land in diagnostics.
    pub(crate) async fn capture_snapshot(
        self: &Arc<Self>,
        execution_timeout_ms: Option<u64>,
        prune_oversized: bool,
    ) -> Option<SnapshotResult> {
        let cfg = self.options.snapshot.clone()?;
        if !self.is_running_state() {
            return None;
        }
        let request = Request::Snapshot {
            path: cfg.path.to_string_lossy().to_string(),
            manifest_path: cfg.manifest_path.to_string_lossy().to_string(),
            max_bytes: cfg.max_bytes.unwrap_or(DEFAULT_SNAPSHOT_MAX_BYTES),
            max_variable_bytes: cfg
                .max_variable_bytes
                .unwrap_or(DEFAULT_SNAPSHOT_MAX_VARIABLE_BYTES),
            prune_oversized,
        };
        let result = self
            .enqueue_request(
                request,
                "",
                ExecuteOptions {
                    internal: true,
                    ..ExecuteOptions::default()
                },
                execution_timeout_ms,
            )
            .await;
        match result {
            Ok(r) if r.result.status == ExecuteStatus::Ok => {
                let Some(fields) = &r.done_fields else {
                    self.append_diagnostic("state snapshot failed: no done fields");
                    return None;
                };
                Some(SnapshotResult {
                    saved: as_string_array(fields, "saved"),
                    skipped: as_reason_array(fields, "skipped"),
                    pruned: {
                        let pruned = as_string_array(fields, "pruned");
                        (!pruned.is_empty()).then_some(pruned)
                    },
                    bytes: fields.get("bytes").and_then(Value::as_u64).unwrap_or(0),
                    path: cfg.path,
                })
            }
            Ok(r) => {
                self.append_diagnostic(&format!(
                    "state snapshot {}: {}",
                    if r.result.status == ExecuteStatus::Aborted {
                        "timed out"
                    } else {
                        "failed"
                    },
                    describe_failure(&r.result),
                ));
                None
            }
            Err(error) => {
                self.append_diagnostic(&format!("state snapshot error: {error:#}"));
                None
            }
        }
    }

    fn is_running_state(&self) -> bool {
        lock(&self.guarded).state == KernelState::Running
    }

    /// Revive a previously snapshotted namespace into the kernel.
    /// `None` when no snapshot is configured or the restore failed.
    /// Repair restores bypass the repair gate and are bounded so a stalled
    /// kernel cannot wedge it.
    pub(crate) async fn perform_restore(
        self: &Arc<Self>,
        protocol_repair: bool,
    ) -> Option<RestoreResult> {
        let cfg = self.options.snapshot.clone()?;
        let request = Request::Restore {
            path: cfg.path.to_string_lossy().to_string(),
        };
        let result = self
            .enqueue_request(
                request,
                "",
                ExecuteOptions {
                    internal: true,
                    protocol_repair,
                    ..ExecuteOptions::default()
                },
                protocol_repair.then_some(REPAIR_STEP_TIMEOUT_MS),
            )
            .await;
        match result {
            Ok(r) if r.result.status == ExecuteStatus::Ok => {
                lock(&self.guarded).pending_restore = false;
                let Some(fields) = &r.done_fields else {
                    self.append_diagnostic("state restore failed: no done fields");
                    return None;
                };
                Some(RestoreResult {
                    restored: as_string_array(fields, "restored"),
                    failed: as_reason_array(fields, "failed"),
                    path: cfg.path,
                })
            }
            Ok(r) => {
                self.append_diagnostic(&format!(
                    "state restore {}: {}",
                    if r.result.status == ExecuteStatus::Aborted {
                        "timed out"
                    } else {
                        "failed"
                    },
                    describe_failure(&r.result),
                ));
                None
            }
            Err(error) => {
                self.append_diagnostic(&format!("state restore error: {error:#}"));
                None
            }
        }
    }

    /// Debounced auto-snapshot after a successful execution: a later resume
    /// (or a crash before graceful shutdown) revives the most recent namespace.
    pub(crate) fn schedule_snapshot(self: &Arc<Self>) {
        if self.options.snapshot.is_none() {
            return;
        }
        let debounce = self
            .options
            .snapshot
            .as_ref()
            .and_then(|cfg| cfg.debounce_ms)
            .unwrap_or(DEFAULT_SNAPSHOT_DEBOUNCE_MS);
        let mut timer = lock(&self.snapshot_timer);
        if let Some(existing) = timer.take() {
            existing.abort();
        }
        let inner = Arc::clone(self);
        *timer = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(debounce)).await;
            inner
                .capture_snapshot(Some(SNAPSHOT_EXECUTION_TIMEOUT_MS), false)
                .await;
        }));
    }

    /// Concurrent teardowns (dispose vs a signal-handler shutdown) join one
    /// flush: a second flusher would clear the execution guard while the first
    /// is still snapshotting and enqueue a duplicate final snapshot behind it.
    async fn flush_snapshot_for_dispose(self: &Arc<Self>) {
        let slot = {
            let mut memo = lock(&self.flush_memo);
            match memo.as_ref() {
                Some(existing) => existing.clone(),
                None => {
                    let slot = MemoSlot::new();
                    *memo = Some(slot.clone());
                    slot
                }
            }
        };
        let owns = {
            let memo = lock(&self.flush_memo);
            matches!(memo.as_ref(), Some(current) if Arc::ptr_eq(current, &slot))
        };
        if owns {
            self.run_snapshot_flush_for_dispose().await;
            slot.finish(None);
            let mut memo = lock(&self.flush_memo);
            if matches!(memo.as_ref(), Some(current) if Arc::ptr_eq(current, &slot)) {
                *memo = None;
            }
        } else {
            let _ = slot.wait().await;
        }
    }

    async fn run_snapshot_flush_for_dispose(self: &Arc<Self>) {
        if self.options.snapshot.is_none() || !self.is_running_state() {
            return;
        }
        // A kernel that never restored the saved namespace must not overwrite
        // it: the on-disk snapshot is strictly fresher than this namespace.
        if lock(&self.guarded).pending_restore {
            return;
        }
        // Block new external executions so none can splice ahead of the final
        // snapshot and stall dispose.
        lock(&self.guarded).flushing_snapshot_for_dispose = true;
        async {
            if lock(&self.guarded).active_execution.is_some() {
                let _ = self.interrupt(None).await;
            }
            // Wait for the execution queue to drain, bounded by the snapshot
            // execution timeout.
            let deadline = Instant::now() + Duration::from_millis(SNAPSHOT_EXECUTION_TIMEOUT_MS);
            let drained = loop {
                if let Ok(_guard) = self.execution_queue.try_lock() {
                    // Release immediately: the snapshot's own request takes the slot next.
                    drop(_guard);
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            if !drained {
                return;
            }
            self.capture_snapshot(Some(SNAPSHOT_EXECUTION_TIMEOUT_MS), false)
                .await;
        }
        .await;
        // Reset: a superseding start() can revive this kernel for new work.
        lock(&self.guarded).flushing_snapshot_for_dispose = false;
    }
}

fn as_string_array(fields: &Value, key: &str) -> Vec<String> {
    fields
        .get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn as_reason_array(fields: &Value, key: &str) -> Vec<SnapshotSkip> {
    fields
        .get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|entry| {
                    let obj = entry.as_object()?;
                    Some(SnapshotSkip {
                        name: obj.get("name")?.as_str()?.to_string(),
                        reason: obj
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// ReplKernelManager delegations onto Inner
// ---------------------------------------------------------------------------

impl ReplKernelManager {
    async fn wait_for_protocol_repair(&self, signal: Option<&AbortSignal>) -> anyhow::Result<()> {
        self.inner.wait_for_protocol_repair(signal).await
    }

    fn schedule_snapshot(&self) {
        // Spawn on the shared runtime: the debounced snapshot must outlive the
        // cell that scheduled it.
        let inner = Arc::clone(&self.inner);
        if tokio::runtime::Handle::try_current()
            .map(|handle| {
                handle.spawn(async move {
                    inner.schedule_snapshot();
                })
            })
            .is_err()
        {
            // No runtime (e.g. sync drop path): the snapshot stays pending.
        }
    }

    async fn capture_snapshot(&self, timeout: Option<u64>, prune: bool) -> Option<SnapshotResult> {
        self.inner.capture_snapshot(timeout, prune).await
    }

    async fn perform_restore(&self, protocol_repair: bool) -> Option<RestoreResult> {
        self.inner.perform_restore(protocol_repair).await
    }

    pub(crate) async fn wait_for_active_execution_to_clear_for_reuse(
        &self,
        signal: Option<&AbortSignal>,
    ) -> anyhow::Result<()> {
        self.inner
            .wait_for_active_execution_to_clear_for_reuse(signal)
            .await
    }

    async fn ensure_kernel_rebootstrapped(
        &self,
        signal: Option<&AbortSignal>,
    ) -> anyhow::Result<()> {
        self.inner.ensure_kernel_rebootstrapped(signal).await
    }

    pub(crate) fn supersede_protocol_repair(&self) {
        self.inner.supersede_protocol_repair();
    }
}

impl Inner {
    /// Bridge the Inner-only call sites onto the shared request plumbing:
    /// the manager struct is a thin Arc wrapper, so this is just a view.
    fn as_manager(self: &Arc<Self>) -> ReplKernelManager {
        ReplKernelManager {
            inner: Arc::clone(self),
        }
    }

    /// Type-erased entry onto the shared request plumbing: the state-op /
    /// repair paths recurse back through the queue (rebootstrap -> enqueue),
    /// so the cycle is broken with `dyn` here, not just `Box::pin`.
    fn enqueue_request(
        self: &Arc<Self>,
        request: Request,
        code: &str,
        opts: ExecuteOptions,
        execution_timeout_ms: Option<u64>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<InternalExecuteResult>> + Send>,
    > {
        let manager = self.as_manager();
        let code = code.to_string();
        Box::pin(async move {
            manager
                .enqueue_request(request, &code, opts, execution_timeout_ms)
                .await
        })
    }
}
