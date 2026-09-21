//! The scheduling surface (protocol breadth wave b10): the worker arms for
//! the cron/heartbeat catalog (`cron_list`, `heartbeats_list`,
//! `heartbeat_manage`, `cron_add`, `cron_cancel`, `heartbeat_get`,
//! `heartbeat_set`, `heartbeat_update` — TS daemon-mode cases over
//! `AgentCronJobStore`), the per-session artifact store they read, and the
//! scheduler that fires due jobs into the session queue (TS
//! `AgentCronScheduler` + `runCronJob`).
//!
//! Store: one `AgentCronJobStore::for_session_artifacts()` per worker
//! process, like TS daemon-mode (`options.worker ?
//! AgentCronJobStore.forSessionArtifacts() : ...`); sessions register
//! their artifact partition when they bind (create and every
//! replacement flow - new_session / switch_session / import_jsonl /
//! fork) and jobs rebind with them.
//!
//! Delivery: a due job is claimed by the store and fired through the
//! session's queue lanes — heartbeats on their delivery-mode lane (steer
//! -> steering, follow-up -> follow-up) with the TS queue key
//! `heartbeat:<id>` (a later fire replaces the queued one), plain cron
//! jobs on the follow-up lane (TS queues a busy session's scheduled
//! prompt as a follow-up). The fire settles when its turn settles, so the
//! store's run bookkeeping (`lastRunAt`/`runCount`) matches the TS
//! record-after-run timing.
//!
//! Deviation (deferred fires): TS `promptHeartbeat` steers a running
//! turn mid-stream; this port's lanes deliver at the next turn boundary
//! (the same queue semantics the `steer` command uses).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::{oneshot, Notify};

use pa_core::cron::scheduler::{AgentCronScheduler, AgentCronSchedulerHooks};
use pa_core::cron::store::{
    AgentCronJobStore, CreateAgentCronJobInput, HeartbeatManagementAction, SessionBinding,
};
use pa_core::cron::{
    is_heartbeat_cron_job, normalize_heartbeat_delivery_mode, normalize_heartbeat_schedule,
    should_defer_heartbeat_cron_job, AgentCronJob, DeliveryMode, HeartbeatSessionActivity,
    JobStatus,
};

use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::worker::{QueuedItem, SessionCore, Worker};

/// How long a scheduler fire waits for its turn to settle before answering
/// the scheduler with a skip (a stuck turn must not pin the dispatch lane
/// forever).
const FIRE_SETTLE_TIMEOUT_MS: u64 = 15 * 60 * 1000;

/// The session-artifact directory for one session file (TS
/// `getSessionArtifactPathForFile`): `<sessions>/../session-artifacts/<id>`.
pub(crate) fn session_artifact_dir(session_file: &Path, session_id: &str) -> Option<PathBuf> {
    session_file
        .parent()?
        .parent()
        .map(|root| root.join("session-artifacts").join(session_id))
}

/// The scheduler hooks: how a claimed job reaches this session.
pub(crate) struct QueueHooks {
    core: Arc<Mutex<SessionCore>>,
    work_notify: Arc<Notify>,
    user_bash: Arc<crate::user_bash::UserBash>,
}

impl QueueHooks {
    /// The session's activity snapshot (TS `shouldDeferHeartbeatCronJob`
    /// inputs): busy flags off the core plus the bash slot.
    fn activity(&self) -> HeartbeatSessionActivity {
        let core = self
            .core
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        HeartbeatSessionActivity {
            is_streaming: core.busy,
            is_compacting: core.compacting,
            // The abort flag the retry lane reads: the closest live
            // signal this port keeps for an in-flight retry.
            is_retrying: core.retry_abort_requested,
            is_bash_running: self.user_bash.is_running(),
            has_pending_session_work: !core.pending_next_turn.is_empty(),
            unfinished_action_count: core.steering.len() + core.follow_up.len(),
        }
    }
}

impl AgentCronSchedulerHooks for QueueHooks {
    async fn run_job(&self, job: &AgentCronJob) -> anyhow::Result<Option<&'static str>> {
        let activity = self.activity();
        if should_defer_heartbeat_cron_job(job, &activity) {
            return Ok(Some("skipped"));
        }
        let (done_tx, done_rx) = oneshot::channel();
        {
            let mut core = self
                .core
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !core.created || core.shutdown_requested || job.status != JobStatus::Active {
                return Ok(Some("skipped"));
            }
            // TS cron fires resume the suspension before admission
            // (`promptHeartbeat`/`promptUntilAccepted` carry
            // `resumeIfIdle: true`): a fire on a post-abort/post-compact
            // session is a resume site.
            core.queued_input_suspended = false;
            let queue_key = is_heartbeat_cron_job(job).then(|| format!("heartbeat:{}", job.id));
            // The TS `heartbeat:<id>` queue key: a later fire replaces the
            // queued one instead of stacking.
            if let Some(key) = &queue_key {
                core.steering
                    .retain(|item| item.queue_key.as_deref() != Some(key.as_str()));
                core.follow_up
                    .retain(|item| item.queue_key.as_deref() != Some(key.as_str()));
            }
            let lane = match (
                is_heartbeat_cron_job(job),
                matches!(job.delivery_mode, Some(DeliveryMode::FollowUp)),
            ) {
                // A heartbeat rides its delivery-mode lane; a plain cron
                // job queues on the follow-up lane.
                (true, false) => &mut core.steering,
                _ => &mut core.follow_up,
            };
            lane.push_back(QueuedItem {
                message: job.prompt.clone(),
                custom_message: None,
                agent_message: None,
                admission_id: None,
                images: Vec::new(),
                queue_key,
                done: Some(done_tx),
            });
        }
        self.work_notify.notify_one();
        match tokio::time::timeout(
            std::time::Duration::from_millis(FIRE_SETTLE_TIMEOUT_MS),
            done_rx,
        )
        .await
        {
            // The turn ran (its result, whatever it was, is a run).
            Ok(_) => Ok(None),
            // The queued item was removed (cancel/clear) or the settle
            // window expired: the fire did not run.
            Err(_) => Ok(Some("skipped")),
        }
    }
}

/// The worker's schedule catalog: the shared artifact store plus the
/// scheduler (started when the first session binds).
pub(crate) struct ScheduledJobs {
    store: Arc<AgentCronJobStore>,
    hooks: Arc<QueueHooks>,
    scheduler: tokio::sync::Mutex<Option<Arc<AgentCronScheduler<QueueHooks>>>>,
}

impl ScheduledJobs {
    pub(crate) fn new(
        core: Arc<Mutex<SessionCore>>,
        work_notify: Arc<Notify>,
        user_bash: Arc<crate::user_bash::UserBash>,
        events: Arc<crate::worker::EventPump>,
    ) -> Self {
        let mut store = AgentCronJobStore::for_session_artifacts();
        // TS daemon-mode's `cronStore.onHeartbeatChange` →
        // `broadcastGlobal({ type: "heartbeats_changed" })`: any heartbeat
        // catalog change (user set/manage, agent `rlm_heartbeat` CRUD, a
        // fire's bookkeeping) broadcasts to the clients and the supervisor
        // re-broadcasts daemon-wide.
        store.on_heartbeat_change(Box::new(move || {
            events.send(crate::worker::OutboundFrame::heartbeats_changed());
        }));
        ScheduledJobs {
            store: Arc::new(store),
            hooks: Arc::new(QueueHooks {
                core,
                work_notify,
                user_bash,
            }),
            scheduler: tokio::sync::Mutex::new(None),
        }
    }

    pub(crate) fn store(&self) -> &Arc<AgentCronJobStore> {
        &self.store
    }

    /// Bind the live session (TS `rebindCronJobsToState`): register the
    /// session's artifact partition, move its stored jobs onto the live
    /// ids, and start (or wake) the scheduler.
    pub(crate) async fn bind_session(
        &self,
        binding: SessionBinding,
        artifact_dir: Option<PathBuf>,
    ) {
        if let Some(dir) = artifact_dir {
            let _ = std::fs::create_dir_all(&dir);
            self.store
                .register_session_artifact(&binding.session_id, &dir);
        }
        if !binding.session_file.is_empty() {
            self.store.rebind_session_jobs(&binding);
        }
        let mut guard = self.scheduler.lock().await;
        if let Some(scheduler) = guard.as_ref() {
            scheduler.wake().await;
            return;
        }
        let scheduler = Arc::new(AgentCronScheduler::new(
            Arc::clone(&self.store),
            Arc::clone(&self.hooks),
        ));
        scheduler.start().await;
        *guard = Some(scheduler);
    }

    /// Re-arm the timer after a catalog mutation (TS `cronScheduler.wake`).
    pub(crate) async fn wake(&self) {
        let guard = self.scheduler.lock().await;
        if let Some(scheduler) = guard.as_ref() {
            scheduler.wake().await;
        }
    }

    /// `removeQueuedHeartbeatFollowUp` (TS daemon-mode): drop the queued
    /// fire of a heartbeat job from the session's queue.
    pub(crate) fn remove_queued_heartbeat_follow_up(&self, job: &AgentCronJob) {
        if !is_heartbeat_cron_job(job) {
            return;
        }
        let key = format!("heartbeat:{}", job.id);
        let mut core = self
            .hooks
            .core
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        core.steering
            .retain(|item| item.queue_key.as_deref() != Some(key.as_str()));
        core.follow_up
            .retain(|item| item.queue_key.as_deref() != Some(key.as_str()));
    }
}

/// The bind inputs of one live session (TS `SessionBinding` plus the
/// session's artifact partition): `None` for in-memory sessions.
pub(crate) fn live_binding(core: &SessionCore) -> Option<(SessionBinding, Option<PathBuf>)> {
    let store = core.store.as_ref()?;
    if store.path.as_os_str().is_empty() {
        return None;
    }
    Some((
        SessionBinding {
            active_session_id: core.active_session_id.clone(),
            session_id: store.session_id().to_string(),
            session_file: store.path.to_string_lossy().to_string(),
            cwd: core.cwd.clone(),
        },
        session_artifact_dir(&store.path, store.session_id()),
    ))
}

impl Worker {
    /// Register the live session's artifact partition on the store
    /// (idempotent) so catalog reads see this session's jobs.
    fn bind_store_artifact(&self, core: &SessionCore) {
        let Some(store) = core.store.as_ref() else {
            return;
        };
        if store.path.as_os_str().is_empty() {
            return;
        }
        if let Some(dir) = session_artifact_dir(&store.path, store.session_id()) {
            self.scheduled
                .store()
                .register_session_artifact(store.session_id(), &dir);
        }
    }

    /// `cron_list` (TS daemon-mode case): the store's jobs filtered by the
    /// selector and the inactive cut.
    pub(crate) async fn handle_cron_list(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("cron_list") {
            return response;
        }
        let include_inactive = payload
            .get("includeInactive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let selector = payload.get("activeSessionId").and_then(Value::as_str);
        {
            let core = self
                .core
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.bind_store_artifact(&core);
        }
        let jobs: Vec<Value> = self
            .scheduled
            .store()
            .list()
            .into_iter()
            .filter(|job| {
                if !include_inactive && !matches!(job.status, JobStatus::Active | JobStatus::Paused)
                {
                    return false;
                }
                match selector {
                    Some(selector) => job.active_session_id == selector,
                    None => true,
                }
            })
            .filter_map(|job| serde_json::to_value(&job).ok())
            .collect();
        response_success(None, "cron_list", Some(json!({ "jobs": jobs })))
    }

    /// `heartbeats_list` (TS daemon-mode `listHeartbeats`): the live or
    /// paused heartbeat jobs as `{ job, sessionName?, firstMessage? }`.
    pub(crate) fn handle_heartbeats_list(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("heartbeats_list") {
            return response;
        }
        let summary = {
            let core = self
                .core
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.bind_store_artifact(&core);
            self.summary_locked(&core)
        };
        let heartbeats: Vec<Value> = self
            .scheduled
            .store()
            .list()
            .into_iter()
            .filter(|job| {
                is_heartbeat_cron_job(job)
                    && matches!(job.status, JobStatus::Active | JobStatus::Paused)
            })
            .map(|job| {
                let mut heartbeat = json!({
                    "job": serde_json::to_value(&job).unwrap_or(Value::Null),
                });
                if let Some(name) = summary.session_name.as_deref() {
                    heartbeat["sessionName"] = json!(name);
                }
                if let Some(first) = summary.first_message.as_deref() {
                    heartbeat["firstMessage"] = json!(first);
                }
                heartbeat
            })
            .collect();
        response_success(
            None,
            "heartbeats_list",
            Some(json!({ "heartbeats": heartbeats })),
        )
    }

    /// `heartbeat_manage` (TS daemon-mode case over `manageHeartbeat`):
    /// pause/resume/stop a heartbeat by job id; an unknown id answers the
    /// TS error.
    pub(crate) async fn handle_heartbeat_manage(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("heartbeat_manage") {
            return response;
        }
        let active_session_id = payload
            .get("activeSessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let job_id = payload
            .get("jobId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // The TS store treats any non-pause/non-stop action as resume.
        let action = match payload.get("action").and_then(Value::as_str) {
            Some("pause") => HeartbeatManagementAction::Pause,
            Some("stop") => HeartbeatManagementAction::Stop,
            _ => HeartbeatManagementAction::Resume,
        };
        {
            let core = self
                .core
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.bind_store_artifact(&core);
        }
        let managed = self.scheduled.store().manage_heartbeat(
            &active_session_id,
            &job_id,
            action,
            crate::util::now_ms(),
        );
        let Ok(Some(job)) = managed else {
            return response_failure(
                None,
                "heartbeat_manage",
                &format!("No active heartbeat found: {job_id}"),
                None,
            );
        };
        if action != HeartbeatManagementAction::Resume {
            self.scheduled.remove_queued_heartbeat_follow_up(&job);
        }
        self.scheduled.wake().await;
        response_success(
            None,
            "heartbeat_manage",
            Some(json!({ "heartbeat": serde_json::to_value(&job).unwrap_or(Value::Null) })),
        )
    }

    /// `cron_add` (TS daemon-mode case over `createCronJobForState`).
    pub(crate) async fn handle_cron_add(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("cron_add") {
            return response;
        }
        let input = {
            let core = self
                .core
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.bind_store_artifact(&core);
            let store = match core.store.as_ref() {
                Some(store) if !store.path.as_os_str().is_empty() => store,
                _ => {
                    return response_failure(
                        None,
                        "cron_add",
                        "Cron jobs require a persisted session file",
                        None,
                    )
                }
            };
            CreateAgentCronJobInput {
                active_session_id: core.active_session_id.clone(),
                session_id: store.session_id().to_string(),
                session_file: store.path.to_string_lossy().to_string(),
                cwd: core.cwd.clone(),
                runtime_kind: Some(core.runtime_kind.clone()),
                prompt: payload
                    .get("prompt")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                schedule_text: payload
                    .get("schedule")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                ..Default::default()
            }
        };
        match self.scheduled.store().create(&input) {
            Ok(job) => {
                self.scheduled.wake().await;
                response_success(
                    None,
                    "cron_add",
                    Some(json!({ "job": serde_json::to_value(&job).unwrap_or(Value::Null) })),
                )
            }
            Err(error) => response_failure(None, "cron_add", &error.to_string(), None),
        }
    }

    /// `cron_cancel` (TS daemon-mode case): cancel by job id, drop any
    /// queued fire, and re-arm the timer.
    pub(crate) async fn handle_cron_cancel(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("cron_cancel") {
            return response;
        }
        let job_id = payload
            .get("jobId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        {
            let core = self
                .core
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.bind_store_artifact(&core);
        }
        match self
            .scheduled
            .store()
            .cancel(&job_id, crate::util::now_ms())
        {
            Some(job) => {
                self.scheduled.remove_queued_heartbeat_follow_up(&job);
                self.scheduled.wake().await;
                response_success(
                    None,
                    "cron_cancel",
                    Some(json!({ "job": serde_json::to_value(&job).unwrap_or(Value::Null) })),
                )
            }
            None => response_failure(
                None,
                "cron_cancel",
                &format!("No cron job found: {job_id}"),
                None,
            ),
        }
    }

    /// `heartbeat_get` (TS daemon-mode case): the session's live or paused
    /// heartbeat, or null.
    pub(crate) fn handle_heartbeat_get(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("heartbeat_get") {
            return response;
        }
        let active_session_id = payload
            .get("activeSessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        {
            let core = self
                .core
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.bind_store_artifact(&core);
        }
        let heartbeat = self
            .scheduled
            .store()
            .get_heartbeat(&active_session_id)
            .and_then(|job| serde_json::to_value(&job).ok());
        response_success(
            None,
            "heartbeat_get",
            Some(json!({ "heartbeat": heartbeat.unwrap_or(Value::Null) })),
        )
    }

    /// `heartbeat_set` (TS daemon-mode case over `createHeartbeatForState`):
    /// replace the session's heartbeat.
    pub(crate) async fn handle_heartbeat_set(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("heartbeat_set") {
            return response;
        }
        let delivery_mode = match normalize_heartbeat_delivery_mode(
            payload.get("deliveryMode").and_then(Value::as_str),
        ) {
            Ok(mode) => mode,
            Err(error) => return response_failure(None, "heartbeat_set", &error.to_string(), None),
        };
        let (previous, input) = {
            let core = self
                .core
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.bind_store_artifact(&core);
            let store = match core.store.as_ref() {
                Some(store) if !store.path.as_os_str().is_empty() => store,
                _ => {
                    return response_failure(
                        None,
                        "heartbeat_set",
                        "Heartbeats require a persisted session file",
                        None,
                    )
                }
            };
            let previous = self
                .scheduled
                .store()
                .get_heartbeat(&core.active_session_id);
            // A replacement keeps the previous delivery mode unless the
            // command carries one (TS `createHeartbeatForState`).
            let delivery_mode =
                delivery_mode.or(previous.as_ref().and_then(|job| job.delivery_mode));
            (
                previous,
                CreateAgentCronJobInput {
                    active_session_id: core.active_session_id.clone(),
                    session_id: store.session_id().to_string(),
                    session_file: store.path.to_string_lossy().to_string(),
                    cwd: core.cwd.clone(),
                    runtime_kind: Some(core.runtime_kind.clone()),
                    delivery_mode,
                    schedule_text: normalize_heartbeat_schedule(Some(
                        payload
                            .get("schedule")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    )),
                    prompt: payload
                        .get("prompt")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    ..Default::default()
                },
            )
        };
        match self.scheduled.store().create_heartbeat(&input) {
            Ok(job) => {
                if let Some(previous) = previous {
                    self.scheduled.remove_queued_heartbeat_follow_up(&previous);
                }
                self.scheduled.wake().await;
                response_success(
                    None,
                    "heartbeat_set",
                    Some(json!({ "heartbeat": serde_json::to_value(&job).unwrap_or(Value::Null) })),
                )
            }
            Err(error) => response_failure(None, "heartbeat_set", &error.to_string(), None),
        }
    }

    /// `heartbeat_update` (TS daemon-mode case over
    /// `updateHeartbeatForState`): pause/resume/clear the session's
    /// heartbeat.
    pub(crate) async fn handle_heartbeat_update(&self, payload: &Value) -> DaemonResponse {
        if let Err(response) = self.require_created("heartbeat_update") {
            return response;
        }
        let active_session_id = payload
            .get("activeSessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let resume = payload.get("action").and_then(Value::as_str) == Some("resume");
        let now = crate::util::now_ms();
        {
            let core = self
                .core
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.bind_store_artifact(&core);
        }
        let outcome = match payload.get("action").and_then(Value::as_str) {
            Some("pause") => Ok(self
                .scheduled
                .store()
                .pause_heartbeat(&active_session_id, now)),
            Some("resume") => self
                .scheduled
                .store()
                .resume_heartbeat(&active_session_id, now),
            // TS `updateHeartbeatForState`: anything but pause/resume
            // clears the heartbeat.
            _ => Ok(self
                .scheduled
                .store()
                .clear_heartbeat(&active_session_id, now)),
        };
        let outcome = match outcome {
            Ok(job) => job,
            Err(error) => {
                return response_failure(None, "heartbeat_update", &error.to_string(), None)
            }
        };
        if let Some(job) = &outcome {
            if !resume {
                self.scheduled.remove_queued_heartbeat_follow_up(job);
            }
        }
        self.scheduled.wake().await;
        let heartbeat = outcome
            .and_then(|job| serde_json::to_value(&job).ok())
            .unwrap_or(Value::Null);
        response_success(
            None,
            "heartbeat_update",
            Some(json!({ "heartbeat": heartbeat })),
        )
    }
}
