//! The create-open reuse seam (TS `createOrReuseWorker`'s reuse half): a
//! create that targets a session file a live worker already serves answers
//! THE LIVE WORKER instead of launching a second process over the same
//! file. The second launch is not a duplicate — it is a guaranteed failure:
//! the runtime session lease the live worker holds makes the new worker's
//! create bounce with `Session is already active`, which the client saw as
//! a bare rejection of its open request. TS never launches over a live
//! file (`matchWorkers`/`findWorkerBySessionFile` -> `reuseWorkerForCreate`
//! -> the create arm answers the live worker's root summary; the client
//! attaches, the roster's clients column gains a row).
//!
//! The seam classifies every resident registered for the file:
//! - **route-ready** (connected, create completed): reused immediately.
//! - **waitable** (a replacement mid-replay or crash backoff): the open
//!   waits out the replacement inside the create's own route budget, like
//!   every client-facing route, then reuses; a worker that never returns
//!   (retired, give-up) falls through to a fresh launch, and a fresh
//!   launch over a dead process's file reclaims its lease by construction.
//! - **stopping/retired**: the launch must wait out the teardown — the
//!   dying process still holds the lease — then launch; a stop that
//!   outlives the settle budget answers the TS `worker is stopping` shape
//!   instead of surfacing the lease rejection.
//!
//! A stale binding (the file's previous worker is gone) keeps the launch
//! path: `record_session_binding` supersedes the old ids at create success
//! and the `session_binding` events re-attach the superseded clients
//! (the #2575 rebind seams; unchanged here).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use pa_types::daemon::{DaemonCommand, DaemonSessionLifecycle};
use serde_json::{json, Value};

use crate::registry::ResidentWorker;
use crate::supervisor::{Supervisor, ROUTE_TIMEOUT_MS, WORKER_NOT_CONNECTED};

/// How long an open waits for a stopping worker's teardown before it
/// answers the `worker is stopping` error (the dying process still holds
/// the session lease; a launch inside the window reproduces the bare
/// lease rejection this seam exists to remove).
const STOP_SETTLE_WAIT: Duration = Duration::from_secs(10);
/// The stop-settle poll cadence.
const STOP_SETTLE_POLL: Duration = Duration::from_millis(50);

/// The residents registered for one session file, by reuse class.
#[derive(Default)]
struct ReuseCandidates {
    /// Connected, create completed, and not stopping: reused now.
    ready: Option<Arc<ResidentWorker>>,
    /// Neither stopping nor retired: a replacement may still be coming
    /// (crash backoff, create replay), so an open waits it out.
    waitable: Option<Arc<ResidentWorker>>,
    /// Stopping or retired: the launch must wait out its teardown.
    stopping: Option<Arc<ResidentWorker>>,
}

/// Whether one resident's process is provably gone. A pid the platform
/// cannot answer for counts as alive, like the lease's stale-owner rule:
/// launching under an unverifiable-but-alive holder would surface the
/// lease rejection again.
async fn resident_process_alive(resident: &Arc<ResidentWorker>) -> bool {
    let pid = resident.descriptor.lock().await.pid;
    if pid == 0 {
        return false;
    }
    crate::lease::is_process_alive(pid as u32).unwrap_or(true)
}

impl Supervisor {
    /// TS `createOrReuseWorker`'s reuse half: resolve the create's
    /// session file, and when a resident already serves it, answer the
    /// LIVE binding (the resident's root summary — the exact create
    /// response shape the client's attach consumes) instead of launching.
    /// `Ok(None)` keeps the launch path (a fresh file, no live resident,
    /// a no-session create, or a resident that went away mid-wait).
    pub(crate) async fn reuse_live_worker_for_create(
        self: &Arc<Self>,
        command: &DaemonCommand,
        client_id: &str,
    ) -> Result<Option<Value>> {
        let DaemonCommand::Create {
            session_path,
            no_session,
            lifecycle,
            ..
        } = command
        else {
            return Ok(None);
        };
        if *no_session == Some(true) {
            return Ok(None);
        }
        let Some(raw_path) = session_path.as_deref() else {
            // A `continueRecent` create resolves its file worker-side (the
            // TS session-manager seam); the supervisor never learns the
            // path, so it cannot resolve a resident for it either.
            return Ok(None);
        };
        let path = crate::paths::expand_tilde(raw_path)?;
        if !path.exists() {
            // The worker create makes the file: nothing has been bound to
            // a path that does not exist yet.
            return Ok(None);
        }
        let path_text = path.to_string_lossy().to_string();
        let residents = self.registry.list_by_session_file(&path_text).await;
        let mut candidates = ReuseCandidates::default();
        for resident in residents {
            let state = resident.route_state();
            if self.is_stopping(&resident) || state.retired {
                candidates.stopping.get_or_insert(resident);
            } else if state.connected && state.session_ready {
                candidates.ready.get_or_insert(resident);
            } else {
                candidates.waitable.get_or_insert(resident);
            }
        }

        // The live binding answers first: a ready resident is the session's
        // current worker, and the open is an attach to it.
        if let Some(resident) = candidates.ready.clone() {
            if let Some(rejection) =
                client_owned_conflict(&resident, *lifecycle, client_id, &path_text).await
            {
                return Err(anyhow!(rejection));
            }
            return self.reuse_ready_summary(&resident, &path_text).await;
        }

        // A resident whose replacement is still coming: wait it out inside
        // the create's route budget, then reuse — the same
        // replacement-aware wait every client-facing route applies.
        if let Some(resident) = candidates.waitable.clone() {
            if let Some(rejection) =
                client_owned_conflict(&resident, *lifecycle, client_id, &path_text).await
            {
                return Err(anyhow!(rejection));
            }
            return self.reuse_ready_summary(&resident, &path_text).await;
        }

        // A stopping or retired resident still owns the lease until its
        // teardown completes: wait out the settle window so the fresh
        // launch lands on a free file instead of the lease rejection. A
        // provably-dead process (a give-up, a crashed child before its
        // monitor reaps it) never blocks the launch.
        if let Some(resident) = candidates.stopping.clone() {
            if !resident_process_alive(&resident).await {
                return Ok(None);
            }
            self.await_stop_settled(&resident, &path_text).await?;
            return Ok(None);
        }

        // No resident serves the file: the launch path (a stale binding
        // rebinds through `record_session_binding` at create success).
        Ok(None)
    }

    /// The summary a reused worker answers the create with: the live
    /// binding's root state. The route is replacement-aware, so an open
    /// landing mid-replay attaches once the replacement's create replay
    /// completed. `Ok(None)` (the worker went away with no successor)
    /// keeps the launch path; the never-ready worker answers the TS
    /// `worker is {state}` shape.
    async fn reuse_ready_summary(
        self: &Arc<Self>,
        resident: &Arc<ResidentWorker>,
        session_path: &str,
    ) -> Result<Option<Value>> {
        match self
            .route_command_ready(resident, "get_state", json!({}), ROUTE_TIMEOUT_MS)
            .await
        {
            Ok(response) => {
                let data = response.data.filter(|data| data.is_object());
                match (response.success, data) {
                    (true, Some(data)) => Ok(Some(data)),
                    _ => Err(anyhow!(
                        "Session \"{session_path}\" worker is unavailable for reuse: \
                         assigned root session is missing"
                    )),
                }
            }
            Err(error) if error.to_string() == WORKER_NOT_CONNECTED => {
                // The worker retired with no successor in flight: the
                // launch path owns the file (its lease holder is gone).
                Ok(None)
            }
            Err(_) => {
                let state = self.effective_reuse_state(resident).await;
                let detail = {
                    let last_error = resident.descriptor.lock().await.last_error.clone();
                    last_error
                        .map(|error| format!(": {error}"))
                        .unwrap_or_default()
                };
                Err(anyhow!(
                    "Session \"{session_path}\" worker is {state}{detail}"
                ))
            }
        }
    }

    /// Wait for a stopping worker's teardown: the resident leaves the
    /// registry, or its process dies (either frees the file for the
    /// launch). Past the settle budget the open answers the TS
    /// `Session "{path}" worker is stopping` shape — never the lease
    /// rejection a racing launch would surface.
    async fn await_stop_settled(
        self: &Arc<Self>,
        resident: &Arc<ResidentWorker>,
        session_path: &str,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + STOP_SETTLE_WAIT;
        loop {
            if self.registry.get(&resident.worker_id).await.is_none()
                || !resident_process_alive(resident).await
            {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("Session \"{session_path}\" worker is stopping");
            }
            tokio::time::sleep(STOP_SETTLE_POLL).await;
        }
    }

    /// TS `effectiveWorkerState` for a reuse answer (the peer-tickets
    /// definition stays the one source).
    async fn effective_reuse_state(&self, resident: &Arc<ResidentWorker>) -> &'static str {
        let connected = resident.cmd_tx.lock().await.is_some();
        let lifecycle = resident.descriptor.lock().await.lifecycle;
        crate::peer_tickets::effective_worker_state(
            connected,
            &lifecycle,
            self.is_stopping(resident),
        )
    }
}

/// TS `assertWorkerCreateOwner`: only an explicit client-owned create is
/// exclusive. A client-owned open of a worker another client owns keeps
/// the TS `SessionAlreadyActiveError` rejection — naming the LIVE
/// binding's active id, never the stale lease id the bug surfaced. Every
/// other create reuses the live worker (multi-client attach).
async fn client_owned_conflict(
    resident: &Arc<ResidentWorker>,
    lifecycle: Option<DaemonSessionLifecycle>,
    client_id: &str,
    session_path: &str,
) -> Option<String> {
    if lifecycle != Some(DaemonSessionLifecycle::ClientOwned) {
        return None;
    }
    let (owner, live_id) = {
        let descriptor = resident.descriptor.lock().await;
        (
            descriptor.owner_client_id.clone(),
            descriptor.root_active_session_id.clone(),
        )
    };
    match owner.as_deref() {
        Some(owner) if owner != client_id => Some(format!(
            "Session is already active in {live_id}: {session_path}"
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resident_with(
        owner_client_id: Option<&str>,
        root_active_session_id: &str,
    ) -> Arc<ResidentWorker> {
        let descriptor: pa_types::daemon::DaemonWorkerDescriptor =
            serde_json::from_value(serde_json::json!({
                "version": 2,
                "workerId": "w-1",
                "pid": 0,
                "socketPath": "/tmp/none.sock",
                "recoveryJournalPath": "/tmp/none.jsonl",
                "supervisorSocketPath": "/tmp/none.sock",
                "authenticationToken": "test",
                "rootActiveSessionId": root_active_session_id,
                "ownerClientId": owner_client_id,
                "createdAt": "2026-09-23T00:00:00Z",
                "updatedAt": "2026-09-23T00:00:00Z",
                "lifecycle": "ready",
                "createCommand": {},
                "consecutiveFailures": 0,
            }))
            .expect("descriptor");
        ResidentWorker::new(
            "w-1".to_string(),
            descriptor,
            std::path::PathBuf::from("/tmp/none"),
        )
    }

    /// TS `assertWorkerCreateOwner`: a client-owned create over a worker
    /// another client owns rejects with the LIVE binding's active id —
    /// never a stale lease id.
    #[tokio::test]
    async fn a_client_owned_create_names_the_live_binding_on_owner_conflicts() {
        let resident = resident_with(Some("daemon-tui:1"), "live-id");
        let conflict = client_owned_conflict(
            &resident,
            Some(DaemonSessionLifecycle::ClientOwned),
            "daemon-tui:2",
            "/sessions/s.jsonl",
        )
        .await
        .expect("the owner conflict rejects");
        assert_eq!(
            conflict,
            "Session is already active in live-id: /sessions/s.jsonl"
        );
    }

    /// The plain open a pane sends (no `lifecycle`) is never exclusive:
    /// the live worker answers it whatever client created it.
    #[tokio::test]
    async fn a_plain_create_never_conflicts_on_ownership() {
        let resident = resident_with(Some("daemon-tui:1"), "live-id");
        assert!(
            client_owned_conflict(&resident, None, "daemon-tui:2", "/s.jsonl")
                .await
                .is_none()
        );
    }

    /// The owning client's own re-open (TS: a client-owned create whose
    /// owner matches) reuses the worker.
    #[tokio::test]
    async fn the_owning_clients_reopen_reuses() {
        let resident = resident_with(Some("daemon-tui:1"), "live-id");
        assert!(client_owned_conflict(
            &resident,
            Some(DaemonSessionLifecycle::ClientOwned),
            "daemon-tui:1",
            "/s.jsonl"
        )
        .await
        .is_none());
    }

    /// A worker with no owner stamp (an adopted worker) is reusable by a
    /// client-owned create too.
    #[tokio::test]
    async fn an_unowned_worker_is_reusable() {
        let resident = resident_with(None, "live-id");
        assert!(client_owned_conflict(
            &resident,
            Some(DaemonSessionLifecycle::ClientOwned),
            "daemon-tui:2",
            "/s.jsonl"
        )
        .await
        .is_none());
    }

    /// An adopted worker (pid 0) is never "process alive": its file is
    /// free for a launch even while its registration lingers.
    #[tokio::test]
    async fn an_unlaunched_registration_counts_as_dead() {
        let resident = resident_with(None, "live-id");
        assert!(!resident_process_alive(&resident).await);
    }
}
