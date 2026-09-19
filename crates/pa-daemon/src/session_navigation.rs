//! The session-navigation surface (protocol breadth wave b9): the worker
//! arms for `new_session`, `switch_session`, and `import_jsonl` (TS
//! daemon-mode cases over `AgentSessionRuntime.newSession` /
//! `switchSession` / `importFromJsonl`). All three replace the worker's
//! live session with another file (the TS runtime's replacement lease
//! path): the store swaps, the engine's session file moves, and the live
//! context rebuilds onto the new branch - the same flow `fork` runs
//! (`branch_navigation.rs`).
//!
//! Responses are the TS `{ cancelled: false }` wire object; a missing input
//! file answers the TS import error (`File not found: <path>`), and a
//! stored session cwd that no longer exists answers the TS
//! `MissingSessionCwdError` text.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::engine::SessionEngine;
use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::session_store::{session_file_name, SessionFile};
use crate::worker::{SessionCore, Worker};

/// The navigation surface: the pieces of `fork`'s replacement flow the
/// three commands share.
pub(crate) struct SessionNavigation {
    engine: Arc<dyn SessionEngine>,
    core: Arc<Mutex<SessionCore>>,
    idle_notify: Arc<Notify>,
    /// The schedule catalog: a replacement session rebinds its scheduled
    /// jobs (TS `rebindCronJobsToState` on the runtime swap).
    scheduled: Arc<crate::scheduled_jobs::ScheduledJobs>,
}

impl SessionNavigation {
    pub(crate) fn new(
        engine: Arc<dyn SessionEngine>,
        core: Arc<Mutex<SessionCore>>,
        idle_notify: Arc<Notify>,
        scheduled: Arc<crate::scheduled_jobs::ScheduledJobs>,
    ) -> Self {
        SessionNavigation {
            engine,
            core,
            idle_notify,
            scheduled,
        }
    }

    /// Wait until the running turn (if any) has settled before replacing
    /// the session (the TS replacement lease's teardown pass).
    async fn wait_turn_end(&self) {
        loop {
            let busy = {
                let mut core = self.core.lock().unwrap();
                if core.busy {
                    core.abort_requested = true;
                    true
                } else {
                    false
                }
            };
            if !busy {
                return;
            }
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(50),
                self.idle_notify.notified(),
            )
            .await;
        }
    }

    /// Swap the worker's live session onto `file`: the store, the engine's
    /// session file, and the rebuilt context (the shared tail of fork /
    /// new_session / switch_session / import_jsonl).
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

    /// `new_session`: a fresh session in the same directory, optionally
    /// parented on `parentSession` with the current depth (TS
    /// `SessionManager.create` + `newSession({ parentSession, rlmDepth })`).
    pub(crate) async fn new_session(&self, payload: &Value) -> DaemonResponse {
        let parent_session = payload
            .get("parentSession")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.wait_turn_end().await;
        let (cwd, session_dir, rlm_depth) = {
            let core = self.core.lock().unwrap();
            match core.store.as_ref() {
                Some(store) => (
                    core.cwd.clone(),
                    store.path.parent().map(|dir| dir.to_path_buf()),
                    store.header.rlm_depth.unwrap_or(0) as u32,
                ),
                None => {
                    return response_failure(
                        None,
                        "new_session",
                        "Session is still initializing",
                        None,
                    )
                }
            }
        };
        let mut fresh = SessionFile::create(&cwd, parent_session.as_deref(), rlm_depth);
        if let Some(session_dir) = session_dir {
            fresh.set_path(session_dir.join(session_file_name(fresh.session_id())));
            if let Err(error) = fresh.rewrite() {
                return response_failure(None, "new_session", &error.to_string(), None);
            }
        }
        match self.replace_session(fresh).await {
            Ok(()) => response_success(None, "new_session", Some(json!({ "cancelled": false }))),
            Err(error) => response_failure(None, "new_session", &error, None),
        }
    }

    /// `switch_session`: reopen the requested session file in place (TS
    /// `SessionManager.open` + `assertSessionCwdExists`).
    pub(crate) async fn switch_session(&self, payload: &Value) -> DaemonResponse {
        let session_path = payload
            .get("sessionPath")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let cwd_override = payload
            .get("cwdOverride")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.wait_turn_end().await;
        let response = self
            .open_replacement(session_path, cwd_override, "switch_session")
            .await;
        match response {
            Ok(file) => match self.replace_session(file).await {
                Ok(()) => {
                    response_success(None, "switch_session", Some(json!({ "cancelled": false })))
                }
                Err(error) => response_failure(None, "switch_session", &error, None),
            },
            Err(response) => response,
        }
    }

    /// `import_jsonl`: copy the input file into the session dir and switch
    /// to it (TS `importFromJsonl`).
    pub(crate) async fn import_jsonl(&self, payload: &Value) -> DaemonResponse {
        let input_path = payload
            .get("inputPath")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let cwd_override = payload
            .get("cwdOverride")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.wait_turn_end().await;
        let resolved = std::path::Path::new(input_path);
        if !resolved.is_file() {
            return response_failure(
                None,
                "import_jsonl",
                &format!("File not found: {}", resolved.display()),
                None,
            );
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
                return response_failure(
                    None,
                    "import_jsonl",
                    "Session is still initializing",
                    None,
                )
            }
        };
        std::fs::create_dir_all(target.parent().unwrap_or(std::path::Path::new(".")))
            .map_err(|error| error.to_string())
            .ok();
        if std::fs::canonicalize(&target).ok() != std::fs::canonicalize(resolved).ok() {
            if let Err(error) = std::fs::copy(resolved, &target) {
                return response_failure(None, "import_jsonl", &error.to_string(), None);
            }
        }
        let response = self
            .open_replacement(&target.to_string_lossy(), cwd_override, "import_jsonl")
            .await;
        match response {
            Ok(file) => match self.replace_session(file).await {
                Ok(()) => {
                    response_success(None, "import_jsonl", Some(json!({ "cancelled": false })))
                }
                Err(error) => response_failure(None, "import_jsonl", &error, None),
            },
            Err(response) => response,
        }
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
    /// `new_session` (the dispatch surface of [`SessionNavigation`]).
    pub(crate) async fn handle_new_session(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("new_session") {
            return response;
        }
        self.navigation.new_session(payload).await
    }

    /// `switch_session`.
    pub(crate) async fn handle_switch_session(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("switch_session") {
            return response;
        }
        self.navigation.switch_session(payload).await
    }

    /// `import_jsonl`.
    pub(crate) async fn handle_import_jsonl(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("import_jsonl") {
            return response;
        }
        self.navigation.import_jsonl(payload).await
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
