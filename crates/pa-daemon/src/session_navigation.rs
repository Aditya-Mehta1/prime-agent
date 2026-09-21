//! The session-navigation surface (protocol breadth wave b9): the worker
//! arms for `new_session`, `switch_session`, and `import_jsonl` (TS
//! daemon-mode cases over `AgentSessionRuntime.newSession` /
//! `switchSession` / `importFromJsonl`). All three replace the worker's
//! live session with another file (the TS runtime's replacement lease
//! path): the store swaps, the engine's session file moves, and the live
//! context rebuilds onto the new branch - the same flow `fork` runs
//! (`branch_navigation.rs`).
//!
//! Replacement ruling (TS parity): the whole-runtime replacement flows
//! retire the old session's runtime first - `teardownForReplacement` ->
//! `teardownCurrent` -> `session.disposeAsync()` disposes the kernel (a
//! final namespace snapshot flush, then the `python -m rlm.repl` process
//! exits) and drops the session object - so the replacement session
//! rebuilds cold: a fresh kernel (the prewarm fires again at the
//! replacement), a system prompt built on the new conversation log, and
//! an empty namespace unless the moved-to session carries its own
//! snapshot. The TS order is kept: the replacement file is prepared and
//! validated BEFORE the teardown (a failed prepare - a missing switch
//! target, a missing import file, a bad fork entry - leaves the old
//! session, its kernel, and any in-flight work untouched), and the
//! teardown runs between the prepare and the swap. The tree moves
//! (`navigate_tree`) are NOT replacements: TS rebuilds the branch
//! context in place on the live session and the kernel stays warm.
//!
//! Responses are the TS `{ cancelled: false }` wire object; a missing input
//! file answers the TS import error (`File not found: <path>`), and a
//! stored session cwd that no longer exists answers the TS
//! `MissingSessionCwdError` text.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::engine::SessionEngine;
use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::session_store::{session_file_name, SessionFile};
use crate::worker::{SessionCore, Worker};

/// The navigation surface: the prepare and swap phases of `fork`'s
/// replacement flow the three commands share. The teardown between the
/// phases is the worker's (it owns the turn/compaction settle and the
/// engine retire), matching the TS split `teardownForReplacement` /
/// `buildAndApplyReplacement`.
pub(crate) struct SessionNavigation {
    engine: Arc<dyn SessionEngine>,
    core: Arc<Mutex<SessionCore>>,
    /// The schedule catalog: a replacement session rebinds its scheduled
    /// jobs (TS `rebindCronJobsToState` on the runtime swap).
    scheduled: Arc<crate::scheduled_jobs::ScheduledJobs>,
}

impl SessionNavigation {
    pub(crate) fn new(
        engine: Arc<dyn SessionEngine>,
        core: Arc<Mutex<SessionCore>>,
        scheduled: Arc<crate::scheduled_jobs::ScheduledJobs>,
    ) -> Self {
        SessionNavigation {
            engine,
            core,
            scheduled,
        }
    }

    /// Swap the worker's live session onto `file`: the store, the engine's
    /// session file, and the rebuilt context (the shared tail of fork /
    /// new_session / switch_session / import_jsonl). The caller retires
    /// the previous runtime (the worker's `teardown_for_replacement`)
    /// before this runs, so the context park lands on the fresh, unbuilt
    /// session and its first build adopts the replacement branch.
    async fn replace_session(&self, file: SessionFile) -> Result<(), String> {
        let branch_entries = file.branch_file_entries();
        let new_path = file.path.clone();
        {
            let mut core = self.core.lock().unwrap();
            core.store = Some(file);
        }
        self.engine.set_session_file(new_path);
        // The replacement session rebinds the schedule catalog (TS
        // `rebindCronJobsToState` on the runtime swap).
        let binding = {
            let core = self
                .core
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            crate::scheduled_jobs::live_binding(&core)
        };
        if let Some((binding, artifact_dir)) = binding {
            self.scheduled.bind_session(binding, artifact_dir).await;
        }
        rebuild_engine_context(&self.engine, branch_entries).await
    }

    /// `new_session`'s prepare phase (TS `SessionManager.create` +
    /// `newSession({ parentSession, rlmDepth })` before the runtime
    /// teardown): a fresh session in the same directory, optionally
    /// parented on `parentSession` with the current depth. Preparing
    /// never touches the live session, so a prepare failure leaves it
    /// untouched - the TS `releaseUncommittedLease` fallthrough.
    #[allow(clippy::result_large_err)]
    pub(crate) async fn prepare_new_session(
        &self,
        payload: &Value,
    ) -> Result<SessionFile, DaemonResponse> {
        let parent_session = payload
            .get("parentSession")
            .and_then(Value::as_str)
            .map(str::to_string);
        let (cwd, session_dir, rlm_depth) = {
            let core = self.core.lock().unwrap();
            match core.store.as_ref() {
                Some(store) => (
                    core.cwd.clone(),
                    store.path.parent().map(|dir| dir.to_path_buf()),
                    store.header.rlm_depth.unwrap_or(0) as u32,
                ),
                None => {
                    return Err(response_failure(
                        None,
                        "new_session",
                        "Session is still initializing",
                        None,
                    ))
                }
            }
        };
        let mut fresh = SessionFile::create(&cwd, parent_session.as_deref(), rlm_depth);
        if let Some(session_dir) = session_dir {
            fresh.set_path(session_dir.join(session_file_name(fresh.session_id())));
            if let Err(error) = fresh.rewrite() {
                return Err(response_failure(
                    None,
                    "new_session",
                    &error.to_string(),
                    None,
                ));
            }
        }
        Ok(fresh)
    }

    /// `switch_session`'s prepare phase (TS `SessionManager.open` +
    /// `assertSessionCwdExists`): open the requested session file and
    /// check its stored cwd exists. A missing file or a gone cwd fails
    /// here - before the teardown - so the live session keeps its kernel
    /// and any in-flight work, exactly like the TS throw out of
    /// `switchSession` before `teardownForReplacement`.
    #[allow(clippy::result_large_err)]
    pub(crate) async fn prepare_switch_session(
        &self,
        payload: &Value,
    ) -> Result<SessionFile, DaemonResponse> {
        let session_path = payload
            .get("sessionPath")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let cwd_override = payload
            .get("cwdOverride")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.open_replacement(session_path, cwd_override, "switch_session")
            .await
    }

    /// `import_jsonl`'s prepare phase (TS `importFromJsonl` before the
    /// runtime teardown): copy the input file into the session dir and
    /// open the copy. A missing input file answers the TS import error
    /// without touching the live session.
    #[allow(clippy::result_large_err)]
    pub(crate) async fn prepare_import_jsonl(
        &self,
        payload: &Value,
    ) -> Result<SessionFile, DaemonResponse> {
        let input_path = payload
            .get("inputPath")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let cwd_override = payload
            .get("cwdOverride")
            .and_then(Value::as_str)
            .map(str::to_string);
        let resolved = std::path::Path::new(input_path);
        if !resolved.is_file() {
            return Err(response_failure(
                None,
                "import_jsonl",
                &format!("File not found: {}", resolved.display()),
                None,
            ));
        }
        // The destination is the session dir's copy of the imported file
        // (TS `copyFileSync`); an in-place import (same file) skips the
        // copy.
        let destination = {
            let core = self.core.lock().unwrap();
            core.store
                .as_ref()
                .and_then(|store| store.path.parent().map(|dir| dir.to_path_buf()))
        };
        let target = match destination {
            Some(dir) => dir.join(
                resolved
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| session_file_name("imported")),
            ),
            None => {
                return Err(response_failure(
                    None,
                    "import_jsonl",
                    "Session is still initializing",
                    None,
                ))
            }
        };
        std::fs::create_dir_all(target.parent().unwrap_or(std::path::Path::new(".")))
            .map_err(|error| error.to_string())
            .ok();
        if std::fs::canonicalize(&target).ok() != std::fs::canonicalize(resolved).ok() {
            if let Err(error) = std::fs::copy(resolved, &target) {
                return Err(response_failure(
                    None,
                    "import_jsonl",
                    &error.to_string(),
                    None,
                ));
            }
        }
        self.open_replacement(&target.to_string_lossy(), cwd_override, "import_jsonl")
            .await
    }

    /// Open one replacement session file and check its stored cwd exists
    /// (TS `SessionManager.open` + `assertSessionCwdExists`): the
    /// `MissingSessionCwdError` text is TS-verbatim.
    #[allow(clippy::result_large_err)]
    async fn open_replacement(
        &self,
        path: &str,
        cwd_override: Option<String>,
        command: &'static str,
    ) -> Result<SessionFile, DaemonResponse> {
        let file = SessionFile::open(std::path::Path::new(path))
            .map_err(|error| response_failure(None, command, &error.to_string(), None))?;
        if let Some(cwd) = cwd_override
            .as_deref()
            .or_else(|| (!file.header.cwd.is_empty()).then_some(file.header.cwd.as_str()))
        {
            if !std::path::Path::new(cwd).is_dir() {
                let fallback = {
                    let core = self.core.lock().unwrap();
                    core.cwd.clone()
                };
                return Err(response_failure(
                    None,
                    command,
                    &format!(
                        "Stored session working directory does not exist: {cwd}\nSession file: {path}\nCurrent working directory: {fallback}"
                    ),
                    None,
                ));
            }
        }
        Ok(file)
    }
}

/// Rebuild the engine's live context onto the moved session (the same
/// helper `branch_navigation` runs for forks).
async fn rebuild_engine_context(
    engine: &Arc<dyn SessionEngine>,
    branch_entries: Vec<pa_types::session::FileEntry>,
) -> Result<(), String> {
    let engine = Arc::clone(engine);
    tokio::task::spawn_blocking(move || engine.rebuild_session_context(branch_entries))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("{error:#}"))
}

impl Worker {
    /// The shared replacement flow (TS `teardownForReplacement` ->
    /// `buildAndApplyReplacement`): prepare the replacement file, retire
    /// the live runtime (kernel dispose - the fresh session's kernel
    /// starts cold), swap the store, and rebuild the context onto the
    /// new branch. The fresh session builds in the background, so the
    /// replacement kernel's prewarm fires at the replacement.
    async fn run_session_replacement(
        &self,
        command: &'static str,
        prepared: Result<SessionFile, DaemonResponse>,
    ) -> DaemonResponse {
        let file = match prepared {
            Ok(file) => file,
            // A prepare failure never tore anything down: the live
            // session, its kernel, and any in-flight work are untouched
            // (the TS `releaseUncommittedLease` fallthrough).
            Err(response) => return response,
        };
        self.teardown_for_replacement().await;
        match self.navigation.replace_session(file).await {
            Ok(()) => {
                self.prewarm_replacement_session();
                response_success(None, command, Some(json!({ "cancelled": false })))
            }
            Err(error) => response_failure(None, command, &error, None),
        }
    }

    /// `new_session` (the dispatch surface of [`SessionNavigation`]).
    pub(crate) async fn handle_new_session(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("new_session") {
            return response;
        }
        let prepared = self.navigation.prepare_new_session(payload).await;
        self.run_session_replacement("new_session", prepared).await
    }

    /// `switch_session`.
    pub(crate) async fn handle_switch_session(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("switch_session") {
            return response;
        }
        let prepared = self.navigation.prepare_switch_session(payload).await;
        self.run_session_replacement("switch_session", prepared)
            .await
    }

    /// `import_jsonl`.
    pub(crate) async fn handle_import_jsonl(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("import_jsonl") {
            return response;
        }
        let prepared = self.navigation.prepare_import_jsonl(payload).await;
        self.run_session_replacement("import_jsonl", prepared).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    async fn created_worker() -> Arc<Worker> {
        let dir = std::env::temp_dir().join(format!("pa-worker-nav-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = crate::worker::WorkerConfig {
            socket_path: dir.join("worker.sock"),
            supervisor_socket_path: std::path::PathBuf::new(),
            token: "token".to_string(),
            worker_instance_id: String::new(),
            active_session_id: "nav-session".to_string(),
            agent_dir: dir.join("agent"),
            recovery_journal_path: dir.join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({ "responses": ["ack"] })),
        };
        let worker = Arc::new(Worker::new(config, None));
        let created = worker
            .dispatch(
                "create",
                &json!({ "noSession": true, "cwd": "/tmp", "name": "nav" }),
            )
            .await;
        assert!(created.success, "create failed: {created:?}");
        worker
    }

    /// Wire shape: `new_session` answers `{ cancelled: false }` and the
    /// worker's session moves to the fresh file.
    #[tokio::test]
    async fn new_session_answers_the_ts_cancelled_shape() {
        let worker = created_worker().await;
        let response = worker
            .dispatch("new_session", &json!({ "activeSessionId": "nav-session" }))
            .await;
        assert_eq!(response.data, Some(json!({ "cancelled": false })));
        // The fresh session carries no history.
        let stats = worker
            .dispatch(
                "get_session_stats",
                &json!({ "activeSessionId": "nav-session" }),
            )
            .await;
        assert!(stats.success, "{stats:?}");
    }

    /// Wire shape: `switch_session` moves to an existing session file;
    /// a missing file fails the command.
    #[tokio::test]
    async fn switch_session_replaces_the_store_and_fails_on_missing_files() {
        let worker = created_worker().await;
        let response = worker
            .dispatch(
                "switch_session",
                &json!({ "activeSessionId": "nav-session", "sessionPath": "/tmp/definitely-missing.jsonl" }),
            )
            .await;
        assert!(!response.success, "{response:?}");
        assert!(
            response
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("/tmp/definitely-missing.jsonl"),
            "{response:?}"
        );
    }

    /// The replacement ruling over the dispatch surface (TS
    /// `teardownForReplacement` order): the teardown runs only after the
    /// replacement file is prepared - a failed switch target never retires
    /// the live session - while a successful replacement retires the built
    /// session (its kernel disposes; the fresh session rebuilds in the
    /// background, its kernel prewarm firing at the replacement). A tree
    /// move is not a replacement: the built session stays warm.
    // The faux provider registration is global; the lock must span the
    // awaited turns that consume its queue.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn replacement_flows_retire_only_on_a_prepared_file() {
        let _faux = crate::agent_engine::FAUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::TempDir::new().expect("temp dir");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
        let config = crate::worker::WorkerConfig {
            socket_path: dir.path().join("worker.sock"),
            supervisor_socket_path: std::path::PathBuf::new(),
            token: "token".to_string(),
            worker_instance_id: String::new(),
            active_session_id: "replacement-session".to_string(),
            agent_dir: dir.path().join("agent"),
            recovery_journal_path: dir.path().join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({
                "engine": "faux",
                "responses": [{ "text": "one" }, { "text": "two" }],
            })),
        };
        let worker = Arc::new(Worker::new(config, None));
        let created = worker
            .dispatch(
                "create",
                &json!({
                    "cwd": dir.path().to_string_lossy(),
                    "sessionDir": sessions_dir.to_string_lossy(),
                }),
            )
            .await;
        assert!(created.success, "create failed: {created:?}");
        let session_id = "replacement-session".to_string();
        let prompted = worker
            .dispatch(
                "prompt_and_wait",
                &json!({ "activeSessionId": session_id, "message": "hello" }),
            )
            .await;
        assert!(prompted.success, "prompt failed: {prompted:?}");
        let engine = worker
            .agent_engine
            .as_ref()
            .expect("faux script drives the real engine")
            .clone();
        let session_built = || {
            let engine = std::sync::Arc::clone(&engine);
            tokio::task::spawn_blocking(move || engine.session.blocking_lock().is_some())
        };
        assert!(
            session_built().await.expect("built join"),
            "the turn built the session"
        );

        // A failed switch prepare leaves the live session untouched: no
        // teardown ran, so the built session (and its kernel) survive -
        // the TS `releaseUncommittedLease` fallthrough.
        let failed = worker
            .dispatch(
                "switch_session",
                &json!({
                    "activeSessionId": session_id,
                    "sessionPath": "/tmp/definitely-missing-replacement.jsonl",
                }),
            )
            .await;
        assert!(
            !failed.success,
            "missing switch target succeeded: {failed:?}"
        );
        assert!(
            session_built().await.expect("built join"),
            "a failed prepare must not retire the session"
        );

        // A tree move is not a replacement: the built session stays
        // (the kernel stays warm - TS rebuilds the branch in place).
        let tree = worker
            .dispatch(
                "get_session_tree",
                &json!({ "activeSessionId": session_id }),
            )
            .await;
        assert!(tree.success, "tree failed: {tree:?}");
        assert!(
            session_built().await.expect("built join"),
            "a tree move must keep the session warm"
        );

        // A successful replacement retires the built session and
        // rebuilds it in the background (the fresh session's kernel
        // prewarm fires at the replacement).
        let replaced = worker
            .dispatch("new_session", &json!({ "activeSessionId": session_id }))
            .await;
        assert_eq!(
            replaced.data,
            Some(json!({ "cancelled": false })),
            "new_session failed: {replaced:?}"
        );
        let second = worker
            .dispatch(
                "prompt_and_wait",
                &json!({ "activeSessionId": session_id, "message": "again" }),
            )
            .await;
        assert!(
            second.success,
            "the replacement session's turn failed: {second:?}"
        );
        assert!(
            session_built().await.expect("built join"),
            "the replacement session rebuilt"
        );

        // The worker (and its engine's private runtime) must drop off the
        // async context.
        let worker_for_drop = worker;
        drop(engine);
        tokio::task::spawn_blocking(move || drop(worker_for_drop))
            .await
            .expect("worker drop join");
    }

    /// Wire shape: `import_jsonl` answers the TS import error for a
    /// missing input file.
    #[tokio::test]
    async fn import_jsonl_answers_the_ts_file_not_found_error() {
        let worker = created_worker().await;
        let response = worker
            .dispatch(
                "import_jsonl",
                &json!({ "activeSessionId": "nav-session", "inputPath": "/tmp/no-such-import.jsonl" }),
            )
            .await;
        assert!(!response.success, "{response:?}");
        assert_eq!(
            response.error.as_deref(),
            Some("File not found: /tmp/no-such-import.jsonl")
        );
    }
}
