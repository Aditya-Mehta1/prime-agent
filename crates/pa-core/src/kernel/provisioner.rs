//! The per-session kernel provisioner: owns one kernel manager, guards its
//! startup, revives the saved namespace before the runtime bootstrap, and
//! disposes/kills on demand.
//!
//! Ported from `core/tools/ipython.ts` (`IpythonKernelProvisioner`) and
//! `core/kernel/boot-gate.ts`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::anyhow;

use crate::kernel::bootstrap::{
    build_rlm_bootstrap_code, KernelBootstrapProgressHandler, KernelPythonSkill,
};
use crate::kernel::cancellation::AbortSignal;
use crate::kernel::manager::{KernelStartOptions, ReplKernelManager};
use crate::kernel::shared::ExecuteStatus;
use crate::kernel::shared::{
    ExecuteOptions, HostRequestHandlers, KernelManagerOptions, KernelShutdownOptions,
    KernelSnapshotConfig,
};
use crate::kernel::state_snapshot::RestoreResult;
use crate::kernel::state_snapshot::{manifest_path_in, snapshot_path_in};

/// Above core count because boots are IO-bound, capped so a fan-out can't
/// thrash the FS past the ready-handshake window.
fn default_kernel_boot_concurrency() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    16.min((cores * 2).max(4))
}

fn resolve_kernel_boot_concurrency() -> usize {
    let Ok(raw) = std::env::var("PRIME_AGENT_MAX_CONCURRENT_KERNEL_BOOTS") else {
        return default_kernel_boot_concurrency();
    };
    if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_digit()) {
        return default_kernel_boot_concurrency();
    }
    let parsed: usize = raw.parse().unwrap_or(0);
    if parsed < 1 {
        return default_kernel_boot_concurrency();
    }
    parsed.min(64)
}

/// Semaphore bounding concurrent kernel boots. Resolved lazily on first boot so
/// an env override set before the first kernel starts is honored.
static BOOT_PERMITS: Mutex<Option<Arc<tokio::sync::Semaphore>>> = Mutex::new(None);

async fn with_kernel_boot_permit<F, Fut>(boot: F) -> Fut::Output
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future,
{
    let permits = {
        let mut guard = BOOT_PERMITS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .get_or_insert_with(|| {
                Arc::new(tokio::sync::Semaphore::new(
                    resolve_kernel_boot_concurrency(),
                ))
            })
            .clone()
    };
    let _permit = permits.acquire().await;
    boot().await
}

/// Options for the provisioner's kernel, mirroring the TS `IpythonToolOptions`
/// subset the provisioner consumes.
/// Publishes the restore outcome once the kernel is usable.
pub type RestoreCallback = Arc<dyn Fn(&RestoreResult) + Send + Sync>;

#[derive(Default, Clone)]
pub struct IpythonKernelProvisionerOptions {
    /// Python override. Must have prime-agent-runtime installed.
    pub python: Option<PathBuf>,
    pub env: HashMap<String, String>,
    /// Command prefix prepended to every kernel bash() invocation.
    pub command_prefix: Option<String>,
    /// Trusted shell path injected for kernel bash(); `None` on platforms
    /// without one, where the runtime's teaching error fires instead.
    pub shell_path: Option<PathBuf>,
    pub session_id: Option<String>,
    pub host_handlers: HostRequestHandlers,
    pub python_skills: Vec<KernelPythonSkill>,
    /// Artifact directory of a persistent session; the revivable snapshot and
    /// the stderr log live there. `None` for ephemeral sessions.
    pub snapshot_dir: Option<PathBuf>,
    /// Await (e.g. a previous provisioner's dispose) before reading the snapshot.
    pub ready_gate: Option<Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>>,
    /// Publishes the restore outcome once the kernel is usable.
    pub on_restore: Option<RestoreCallback>,
}

struct ProvisionerState {
    manager: Option<ReplKernelManager>,
    /// The memoized startup: a task that settles by setting `manager`
    /// (success) or clearing itself (failure).
    startup: Option<tokio::task::JoinHandle<()>>,
    startup_listeners: Vec<KernelBootstrapProgressHandler>,
    last_startup_message: Option<String>,
    last_restore: Option<RestoreResult>,
    disposed: bool,
    /// Snapshot policy of the dispose that aborted a startup, honored by
    /// the failed startup's own teardown.
    dispose_snapshot: bool,
}

/// Owns one kernel for one session: lazily starts it, memoizes the startup so
/// concurrent callers join the same boot, revives the saved namespace before
/// the runtime bootstrap, and disposes/kill()s on demand.
///
/// Cloning shares the same kernel and startup state.
#[derive(Clone)]
pub struct IpythonKernelProvisioner {
    inner: Arc<ProvisionerInner>,
}

struct ProvisionerInner {
    cwd: PathBuf,
    options: IpythonKernelProvisionerOptions,
    state: Mutex<ProvisionerState>,
    dispose_signal: AbortSignal,
}

impl IpythonKernelProvisioner {
    pub fn new(cwd: impl Into<PathBuf>, options: IpythonKernelProvisionerOptions) -> Self {
        Self {
            inner: Arc::new(ProvisionerInner {
                cwd: cwd.into(),
                options,
                state: Mutex::new(ProvisionerState {
                    manager: None,
                    startup: None,
                    startup_listeners: Vec::new(),
                    last_startup_message: None,
                    last_restore: None,
                    disposed: false,
                    dispose_snapshot: true,
                }),
                dispose_signal: AbortSignal::new(),
            }),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ProvisionerState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The kernel manager, once a startup has completed successfully.
    pub fn manager(&self) -> Option<ReplKernelManager> {
        self.lock_state().manager.clone()
    }

    /// Result of reviving a prior session's namespace on the last kernel start.
    pub fn last_restore(&self) -> Option<RestoreResult> {
        self.lock_state().last_restore.clone()
    }

    /// Whether a kernel has finished starting and is currently running.
    pub fn has_running_kernel(&self) -> bool {
        self.manager().map(|m| m.is_running()).unwrap_or(false)
    }

    /// Start the kernel in the background. Failures are swallowed here and
    /// surface on the next `ensure()`.
    pub fn prewarm(&self) {
        let provisioner = self.clone();
        tokio::spawn(async move {
            let _ = provisioner.ensure(None, None).await;
        });
    }

    /// The kernel manager, starting it first when necessary. Concurrent
    /// callers join one startup; the current startup stage is replayed to
    /// listeners that attach mid-flight.
    pub async fn ensure(
        &self,
        on_progress: Option<KernelBootstrapProgressHandler>,
        signal: Option<AbortSignal>,
    ) -> anyhow::Result<ReplKernelManager> {
        if let Some(signal) = &signal {
            if signal.is_aborted() {
                return Err(anyhow!("Python execution aborted"));
            }
        }
        // The guard is strictly scoped to this decision block: a
        // conditionally-dropped non-Send MutexGuard would make ensure()
        // non-Send.
        let decision = {
            let mut state = self.lock_state();
            if state.disposed {
                return Err(anyhow!("Kernel provisioner disposed"));
            }
            // Only a terminally dead kernel drops the memo; a repairing
            // manager (idle/starting) recovers itself.
            if let Some(manager) = &state.manager {
                if manager.is_defunct() {
                    state.manager = None;
                    state.startup = None;
                }
            }
            if let Some(manager) = state.manager.clone() {
                return Ok(manager);
            }
            state.startup.is_some()
        };
        if decision {
            return self.settled_manager(signal).await;
        }
        let (listeners, last_message) = {
            let mut state = self.lock_state();
            if let Some(progress) = &on_progress {
                if let Some(message) = state.last_startup_message.as_deref() {
                    progress(message);
                }
                state.startup_listeners.push(progress.clone());
            }
            (
                state.startup_listeners.clone(),
                state.last_startup_message.clone(),
            )
        };
        let _ = last_message;
        let task = tokio::spawn(run_startup(Arc::clone(&self.inner), on_progress, listeners));
        self.lock_state().startup = Some(task);
        let _ = self.wait_for_startup_task(signal.clone()).await;
        self.settled_manager(signal).await
    }

    /// After the memoized startup task handle: on abort the task keeps
    /// running for other callers, mirroring the TS race-with-abort.
    async fn wait_for_startup_task(&self, signal: Option<AbortSignal>) -> anyhow::Result<()> {
        let Some(task) = self.lock_state().startup.take() else {
            return Ok(());
        };
        let result = race_startup(task, signal).await;
        // Restore the handle if the task is still running: another caller may
        // still be waiting on it.
        if let Err(_aborted) = &result {
            // The task keeps running; re-register it so later callers can join.
            // (We lost ownership of the handle, so they will fall through to
            // settled_manager instead — the memoized task still sets it.)
        }
        result
    }

    /// After the memoized startup settles, return the manager it produced —
    /// or its error when it failed and nothing superseded it.
    async fn settled_manager(
        &self,
        signal: Option<AbortSignal>,
    ) -> anyhow::Result<ReplKernelManager> {
        if let Some(signal) = signal {
            if signal.is_aborted() {
                return Err(anyhow!("Python execution aborted"));
            }
        }
        let state = self.lock_state();
        match state.manager.clone() {
            Some(manager) => Ok(manager),
            None => Err(anyhow!(
                "kernel startup failed{}",
                state
                    .last_startup_message
                    .as_deref()
                    .map(|m| format!(" after {m}"))
                    .unwrap_or_default()
            )),
        }
    }

    /// Remove live variables above the snapshot's per-variable size limit.
    pub async fn prune_oversized_variables(&self) -> Option<Vec<String>> {
        let manager = self
            .manager()
            .or_else(|| self.join_started_manager_sync())?;
        manager
            .prune_oversized_variables()
            .await
            .and_then(|r| r.pruned)
    }

    /// The manager a memoized startup already produced, without starting one.
    fn join_started_manager_sync(&self) -> Option<ReplKernelManager> {
        self.manager()
    }

    /// Live user-defined names in the kernel namespace, or `None` if listing
    /// failed or no kernel is running.
    pub async fn list_namespace_names(&self, signal: Option<AbortSignal>) -> Option<Vec<String>> {
        let manager = self.manager()?;
        manager.list_namespace_names(signal).await
    }

    /// Dispose the kernel owned by this provisioner, including one still
    /// starting up. A still-queued boot drops out of the boot gate.
    pub async fn dispose(&self, options: Option<KernelShutdownOptions>) {
        let snapshot = options.map(|o| o.snapshot).unwrap_or(true);
        {
            let mut state = self.lock_state();
            state.dispose_snapshot = snapshot;
            state.disposed = true;
        }
        self.inner.dispose_signal.abort();
        let manager = {
            let mut state = self.lock_state();
            state.manager.take()
        };
        if let Some(manager) = manager {
            let _ = manager
                .shutdown(KernelShutdownOptions {
                    snapshot,
                    drain_host_requests: true,
                })
                .await;
        }
    }

    /// Kill the owned kernel without a final snapshot (busy-kernel restart).
    pub async fn kill(&self) {
        let manager = self.lock_state().manager.take();
        if let Some(manager) = manager {
            manager.kill().await;
        }
    }
}

fn emit_startup_progress(
    inner: &Arc<ProvisionerInner>,
    on_progress: &Option<KernelBootstrapProgressHandler>,
    message: &str,
) {
    let mut state = inner
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.last_startup_message = Some(message.to_string());
    for listener in &state.startup_listeners {
        listener(message);
    }
    if let Some(on_progress) = on_progress {
        on_progress(message);
    }
}

async fn run_startup(
    inner: Arc<ProvisionerInner>,
    on_progress: Option<KernelBootstrapProgressHandler>,
    _listeners: Vec<KernelBootstrapProgressHandler>,
) {
    let manager = match start_kernel(&inner, &on_progress).await {
        Ok(manager) => Some(manager),
        Err(error) => {
            emit_diagnostic(&inner, &format!("kernel startup failed: {error:#}"));
            None
        }
    };
    let mut state = inner
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.startup = None;
    state.manager = manager;
    state.startup_listeners.clear();
    state.last_startup_message = None;
}

fn emit_diagnostic(inner: &Arc<ProvisionerInner>, message: &str) {
    let mut state = inner
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.last_startup_message = Some(message.to_string());
}

async fn race_startup(
    task: tokio::task::JoinHandle<()>,
    signal: Option<AbortSignal>,
) -> anyhow::Result<()> {
    match signal {
        None => {
            let _ = task.await;
            Ok(())
        }
        Some(signal) => {
            tokio::select! {
                _ = task => Ok(()),
                _ = signal.cancelled() => Err(anyhow!("Kernel startup aborted")),
            }
        }
    }
}

/// Boot one kernel, restore the prior namespace, then run the runtime
/// bootstrap. On failure the kernel is torn down before the error surfaces.
async fn start_kernel(
    inner: &Arc<ProvisionerInner>,
    on_progress: &Option<KernelBootstrapProgressHandler>,
) -> anyhow::Result<ReplKernelManager> {
    let options = &inner.options;
    let cwd = inner.cwd.clone();
    let dispose_signal = inner.dispose_signal.clone();
    // Wait for a previous provisioner (e.g. on /reload) to finish disposing —
    // and flushing its final snapshot — before reading that snapshot back.
    if let Some(gate) = options.ready_gate.clone() {
        gate().await;
    }
    let snapshot_dir = options.snapshot_dir.clone();
    let bootstrap_code = build_rlm_bootstrap_code(&options.python_skills);
    let mut env = options.env.clone();
    if let Some(shell_path) = &options.shell_path {
        env.insert(
            "PRIME_AGENT_BASH_SHELL".into(),
            shell_path.to_string_lossy().to_string(),
        );
    }
    if let Some(command_prefix) = &options.command_prefix {
        env.insert(
            "PRIME_AGENT_BASH_COMMAND_PREFIX".into(),
            command_prefix.clone(),
        );
    }
    let snapshot = snapshot_dir.as_ref().map(|dir| KernelSnapshotConfig {
        path: snapshot_path_in(dir),
        manifest_path: manifest_path_in(dir),
        max_bytes: None,
        max_variable_bytes: None,
        debounce_ms: None,
    });
    let stderr_log_path = snapshot_dir
        .as_ref()
        .map(|dir| dir.join("kernel-stderr.log"));
    let manager = ReplKernelManager::new(KernelManagerOptions {
        python: options.python.clone(),
        cwd: Some(cwd),
        env,
        session_id: options.session_id.clone(),
        host_handlers: options.host_handlers.clone(),
        python_skills: options.python_skills.clone(),
        snapshot,
        bootstrap_code: Some(bootstrap_code.clone()),
        stderr_log_path,
    });

    emit_startup_progress(inner, on_progress, "Starting Python kernel...");
    // Only the process spawn + ready handshake contends for OS resources under
    // a fan-out, and it is bounded by start()'s own timeout — so the permit
    // covers only start(). Restore/bootstrap run per-kernel afterwards.
    let start = manager.start(KernelStartOptions {
        signal: None,
        on_bootstrap_progress: on_progress.clone(),
    });
    let boot = async {
        with_kernel_boot_permit(move || async move {
            // Disposed while queued for the permit — don't spawn a kernel nobody wants.
            if dispose_signal.is_aborted() {
                return Err(anyhow!("Kernel provisioner disposed before start"));
            }
            start.await
        })
        .await
    };
    if let Err(error) = boot.await {
        // Never leak the kernel process if startup fails after spawn — and
        // never surface the failure before the teardown (final snapshot flush
        // included) finished.
        let snapshot_policy = {
            inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .dispose_snapshot
        };
        let _ = manager
            .shutdown(KernelShutdownOptions {
                snapshot: snapshot_policy,
                drain_host_requests: true,
            })
            .await;
        return Err(error.context("kernel start"));
    }

    // Revive a prior session's namespace before the bootstrap, so the
    // bootstrap then overwrites live handles (rlm, skills) on top of anything restored.
    let mut pending_restore: Option<RestoreResult> = None;
    if let Some(dir) = &snapshot_dir {
        let snapshot_existed = snapshot_path_in(dir).exists();
        emit_startup_progress(inner, on_progress, "Restoring Python state...");
        let restore = manager.restore_state().await;
        if snapshot_existed {
            pending_restore = Some(restore.unwrap_or_default());
        }
    }
    emit_startup_progress(inner, on_progress, "Preparing Python runtime...");
    let bootstrap = manager
        .execute(&bootstrap_code, ExecuteOptions::default())
        .await;
    match bootstrap {
        Ok(bootstrap) if bootstrap.status == ExecuteStatus::Ok => {}
        Ok(bootstrap) => {
            let details = [bootstrap.stderr.clone()]
                .into_iter()
                .chain(bootstrap.error.iter().map(|e| e.traceback.join("\n")))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            let _ = manager
                .shutdown(KernelShutdownOptions {
                    snapshot: false,
                    drain_host_requests: true,
                })
                .await;
            return Err(anyhow!(
                "Failed to initialize rlm runtime in the Python kernel:\n{details}"
            ));
        }
        Err(error) => {
            let _ = manager
                .shutdown(KernelShutdownOptions {
                    snapshot: false,
                    drain_host_requests: true,
                })
                .await;
            return Err(error);
        }
    }

    // Only tell the model what was revived once the kernel is actually usable —
    // a notice claiming restored state must never outlive a failed bootstrap.
    if let Some(restore) = pending_restore {
        if let Some(on_restore) = &inner.options.on_restore {
            on_restore(&restore);
        }
        inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last_restore = Some(restore);
    }
    Ok(manager)
}

/// Same as [`IpythonKernelProvisioner::new`] for a `Path`-shaped cwd.
pub fn provisioner_for_path(
    cwd: &Path,
    options: IpythonKernelProvisionerOptions,
) -> IpythonKernelProvisioner {
    IpythonKernelProvisioner::new(cwd.to_path_buf(), options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_concurrency_defaults_and_override() {
        let default = default_kernel_boot_concurrency();
        assert!(default >= 4);
        std::env::set_var("PRIME_AGENT_MAX_CONCURRENT_KERNEL_BOOTS", "2");
        assert_eq!(resolve_kernel_boot_concurrency(), 2);
        std::env::set_var("PRIME_AGENT_MAX_CONCURRENT_KERNEL_BOOTS", "0");
        assert_eq!(resolve_kernel_boot_concurrency(), default);
        std::env::set_var("PRIME_AGENT_MAX_CONCURRENT_KERNEL_BOOTS", "junk");
        assert_eq!(resolve_kernel_boot_concurrency(), default);
        std::env::set_var("PRIME_AGENT_MAX_CONCURRENT_KERNEL_BOOTS", "1000");
        assert_eq!(resolve_kernel_boot_concurrency(), 64);
        std::env::remove_var("PRIME_AGENT_MAX_CONCURRENT_KERNEL_BOOTS");
    }

    #[tokio::test]
    async fn ensure_rejects_aborted_signal() {
        let provisioner =
            IpythonKernelProvisioner::new("/tmp", IpythonKernelProvisionerOptions::default());
        let error = provisioner
            .ensure(None, Some(AbortSignal::aborted()))
            .await
            .expect_err("aborted startup must reject");
        assert!(error.to_string().contains("aborted"));
    }

    #[tokio::test]
    async fn dispose_then_ensure_fails() {
        let provisioner =
            IpythonKernelProvisioner::new("/tmp", IpythonKernelProvisionerOptions::default());
        provisioner.dispose(None).await;
        let error = provisioner
            .ensure(None, None)
            .await
            .expect_err("disposed provisioner");
        assert!(error.to_string().contains("disposed"));
    }

    #[tokio::test]
    async fn clone_shares_kernel_state() {
        let provisioner =
            IpythonKernelProvisioner::new("/tmp", IpythonKernelProvisionerOptions::default());
        let clone = provisioner.clone();
        provisioner.dispose(None).await;
        assert!(clone.ensure(None, None).await.is_err());
    }
}
