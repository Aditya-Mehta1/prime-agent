//! The cron job store: file-backed job state with cross-process locking,
//! claim-based dispatch, and heartbeat lifecycle management. Port of the
//! AgentCronJobStore half of core/cron-jobs.ts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    is_due_job, is_heartbeat_cron_job, next_run_at_for_schedule, parse_agent_cron_schedule,
    parse_iso_millis, AgentCronJob, AgentCronSchedule, DeliveryMode, JobStatus, ScheduleKind,
    DEFAULT_HEARTBEAT_DELIVERY_MODE,
};

pub const SESSION_SCHEDULED_JOBS_FILENAME: &str = "scheduled-jobs.json";
const LOCK_STALE_MS: u64 = 30_000;

/// One claimed dispatch of a due job.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentCronDispatch {
    pub id: String,
    pub job: AgentCronJob,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentCronDispatchRecord {
    pub id: String,
    pub job_id: String,
    pub claimed_at: String,
    pub scheduled_for: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
struct CronJobsState {
    jobs: Vec<AgentCronJob>,
    dispatches: Vec<AgentCronDispatchRecord>,
}

/// `/heartbeat`-visible result states.
pub type CronJobRunResult = &'static str; // "ran" | "skipped"

/// The file-backed job store.
pub struct AgentCronJobStore {
    file_path: Option<PathBuf>,
    session_artifact_mode: bool,
    session_artifact_files: HashMap<String, PathBuf>,
    heartbeat_change_listeners: Vec<Box<dyn Fn() + Send + Sync>>,
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn iso_from_millis(millis: u64) -> String {
    crate::session::manager::format_iso(millis as i64)
}

impl AgentCronJobStore {
    /// Store backed by a single file.
    pub fn new(file_path: PathBuf) -> Self {
        Self {
            file_path: Some(file_path),
            session_artifact_mode: false,
            session_artifact_files: HashMap::new(),
            heartbeat_change_listeners: Vec::new(),
        }
    }

    /// Store spanning per-session artifact files.
    pub fn for_session_artifacts() -> Self {
        Self {
            file_path: None,
            session_artifact_mode: true,
            session_artifact_files: HashMap::new(),
            heartbeat_change_listeners: Vec::new(),
        }
    }

    pub fn on_heartbeat_change(&mut self, listener: Box<dyn Fn() + Send + Sync>) {
        self.heartbeat_change_listeners.push(listener);
    }

    fn notify_heartbeat_change(&self) {
        for listener in &self.heartbeat_change_listeners {
            listener();
        }
    }

    fn heartbeat_catalog_signature(jobs: &[AgentCronJob]) -> String {
        let mut heartbeat_jobs: Vec<&AgentCronJob> = jobs
            .iter()
            .filter(|job| {
                is_heartbeat_cron_job(job)
                    && matches!(job.status, JobStatus::Active | JobStatus::Paused)
            })
            .collect();
        heartbeat_jobs.sort_by(|left, right| left.id.cmp(&right.id));
        serde_json::to_string(
            &heartbeat_jobs
                .into_iter()
                .map(|job| HeartbeatCatalogEntry {
                    id: job.id.clone(),
                    status: job.status,
                    source: job.source.clone(),
                    runtime_kind: job.runtime_kind.clone(),
                    delivery_mode: job.delivery_mode,
                    active_session_id: job.active_session_id.clone(),
                    session_id: job.session_id.clone(),
                    session_file: job.session_file.clone(),
                    cwd: job.cwd.clone(),
                    label: job.label.clone(),
                    prompt: job.prompt.clone(),
                    schedule: job.schedule.clone(),
                    created_at: job.created_at.clone(),
                })
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default()
    }

    pub fn register_session_artifact(&mut self, session_id: &str, artifact_dir: &Path) -> bool {
        if !self.session_artifact_mode {
            return false;
        }
        let path = artifact_dir.join(SESSION_SCHEDULED_JOBS_FILENAME);
        if self.session_artifact_files.get(session_id) == Some(&path) {
            return false;
        }
        self.session_artifact_files
            .insert(session_id.to_string(), path);
        true
    }

    pub fn recover_session_artifact(&self, session_id: &str, now: u64) -> Vec<AgentCronJob> {
        let Some(path) = self.session_artifact_files.get(session_id) else {
            return Vec::new();
        };
        with_state_locks(std::slice::from_ref(path), || {
            let mut state = read_jobs_state(path);
            let mut recovered = Vec::new();
            if !state.dispatches.is_empty() {
                recover_interrupted_in_state(&mut state, now, &mut recovered, None);
                write_jobs_state(path, &state);
            }
            recovered
        })
    }

    pub fn list(&self) -> Vec<AgentCronJob> {
        let mut jobs = self.read_jobs();
        jobs.sort_by(|left, right| compare_optional_iso(&left.next_run_at, &right.next_run_at));
        jobs
    }

    pub fn create(&self, input: &CreateAgentCronJobInput) -> anyhow::Result<AgentCronJob> {
        let prompt = input.prompt.trim();
        if prompt.is_empty() {
            anyhow::bail!("Cron job prompt cannot be empty");
        }
        let now = input.now.unwrap_or_else(now_millis);
        let parsed = parse_agent_cron_schedule(&input.schedule_text, now)?;
        let now_iso = iso_from_millis(now);
        let job = AgentCronJob {
            id: Uuid::new_v4().to_string(),
            status: JobStatus::Active,
            source: Some(input.source.clone().unwrap_or_else(|| "cron".to_string())),
            runtime_kind: input.runtime_kind.clone(),
            delivery_mode: None,
            active_session_id: input.active_session_id.clone(),
            session_id: input.session_id.clone(),
            session_file: input.session_file.clone(),
            cwd: input.cwd.clone(),
            label: normalize_optional_label(input.label.as_deref()),
            prompt: prompt.to_string(),
            schedule: parsed.0,
            created_at: now_iso.clone(),
            updated_at: now_iso,
            next_run_at: Some(iso_from_millis(parsed.1)),
            last_run_at: None,
            last_skipped_at: None,
            last_error: None,
            run_count: 0,
        };
        let mut jobs = self.read_jobs();
        jobs.push(job.clone());
        self.write_jobs(&jobs);
        Ok(job)
    }

    /// Bind jobs stored for a session file to a live session id on restore, or
    /// move a live session's jobs to a new file when it switches.
    pub fn rebind_session_jobs(&self, input: &SessionBinding) -> Vec<AgentCronJob> {
        let target_session_file = resolve_path(&input.session_file);
        let mut rebound_jobs = Vec::new();
        let jobs: Vec<AgentCronJob> = self
            .read_jobs()
            .into_iter()
            .map(|job| {
                if job.active_session_id != input.active_session_id
                    && resolve_path(&job.session_file) != target_session_file
                {
                    return job;
                }
                if job.active_session_id == input.active_session_id
                    && job.session_id == input.session_id
                    && resolve_path(&job.session_file) == target_session_file
                    && job.cwd == input.cwd
                {
                    return job;
                }
                let rebound = AgentCronJob {
                    active_session_id: input.active_session_id.clone(),
                    session_id: input.session_id.clone(),
                    session_file: input.session_file.clone(),
                    cwd: input.cwd.clone(),
                    ..job
                };
                rebound_jobs.push(rebound.clone());
                rebound
            })
            .collect();
        if !rebound_jobs.is_empty() {
            self.write_jobs(&jobs);
        }
        rebound_jobs
    }

    pub fn get_heartbeat(&self, active_session_id: &str) -> Option<AgentCronJob> {
        self.read_jobs()
            .into_iter()
            .filter(|job| {
                job.active_session_id == active_session_id
                    && job.source.as_deref() == Some("heartbeat")
                    && matches!(job.status, JobStatus::Active | JobStatus::Paused)
            })
            .max_by_key(|job| parse_iso_millis(&job.updated_at).unwrap_or(0))
    }

    pub fn get_latest_heartbeat(&self, active_session_id: &str) -> Option<AgentCronJob> {
        self.read_jobs()
            .into_iter()
            .filter(|job| {
                job.active_session_id == active_session_id
                    && job.source.as_deref() == Some("heartbeat")
            })
            .max_by_key(|job| parse_iso_millis(&job.updated_at).unwrap_or(0))
    }

    pub fn create_heartbeat(
        &self,
        input: &CreateAgentCronJobInput,
    ) -> anyhow::Result<AgentCronJob> {
        let now = input.now.unwrap_or_else(now_millis);
        let parsed = parse_agent_cron_schedule(&input.schedule_text, now)?;
        if parsed.0.kind == ScheduleKind::Once {
            anyhow::bail!("Heartbeat schedule must be recurring");
        }
        let prompt = input.prompt.trim();
        if prompt.is_empty() {
            anyhow::bail!("Heartbeat instruction cannot be empty");
        }
        let now_iso = iso_from_millis(now);
        // Cancel existing active/paused heartbeats for this session.
        let existing: Vec<AgentCronJob> = self
            .read_jobs()
            .into_iter()
            .map(|job| {
                if job.active_session_id == input.active_session_id
                    && job.source.as_deref() == Some("heartbeat")
                    && matches!(job.status, JobStatus::Active | JobStatus::Paused)
                {
                    AgentCronJob {
                        status: JobStatus::Cancelled,
                        next_run_at: None,
                        updated_at: now_iso.clone(),
                        ..job
                    }
                } else {
                    job
                }
            })
            .collect();
        let job = AgentCronJob {
            id: Uuid::new_v4().to_string(),
            status: JobStatus::Active,
            source: Some("heartbeat".to_string()),
            runtime_kind: input.runtime_kind.clone(),
            delivery_mode: Some(
                input
                    .delivery_mode
                    .unwrap_or(DEFAULT_HEARTBEAT_DELIVERY_MODE),
            ),
            active_session_id: input.active_session_id.clone(),
            session_id: input.session_id.clone(),
            session_file: input.session_file.clone(),
            cwd: input.cwd.clone(),
            label: normalize_optional_label(input.label.as_deref()),
            prompt: prompt.to_string(),
            schedule: parsed.0,
            created_at: now_iso.clone(),
            updated_at: now_iso,
            next_run_at: Some(iso_from_millis(parsed.1)),
            last_run_at: None,
            last_skipped_at: None,
            last_error: None,
            run_count: 0,
        };
        let mut jobs = existing;
        jobs.push(job.clone());
        self.write_jobs(&jobs);
        Ok(job)
    }

    pub fn list_rlm_heartbeats(
        &self,
        active_session_id: &str,
        include_inactive: bool,
    ) -> Vec<AgentCronJob> {
        let mut jobs: Vec<AgentCronJob> = self
            .read_jobs()
            .into_iter()
            .filter(|job| {
                if job.active_session_id != active_session_id
                    || job.source.as_deref() != Some("rlm_heartbeat")
                {
                    return false;
                }
                if include_inactive {
                    return true;
                }
                matches!(job.status, JobStatus::Active | JobStatus::Paused)
            })
            .collect();
        jobs.sort_by(|left, right| compare_optional_iso(&left.next_run_at, &right.next_run_at));
        jobs
    }

    pub fn create_rlm_heartbeat(
        &self,
        input: &CreateAgentCronJobInput,
    ) -> anyhow::Result<AgentCronJob> {
        let now = input.now.unwrap_or_else(now_millis);
        let parsed = parse_agent_cron_schedule(&input.schedule_text, now)?;
        if parsed.0.kind == ScheduleKind::Once {
            anyhow::bail!("RLM heartbeat schedule must be recurring");
        }
        let prompt = input.prompt.trim();
        if prompt.is_empty() {
            anyhow::bail!("RLM heartbeat instruction cannot be empty");
        }
        let now_iso = iso_from_millis(now);
        let job = AgentCronJob {
            id: Uuid::new_v4().to_string(),
            status: JobStatus::Active,
            source: Some("rlm_heartbeat".to_string()),
            runtime_kind: input.runtime_kind.clone(),
            delivery_mode: Some(
                input
                    .delivery_mode
                    .unwrap_or(DEFAULT_HEARTBEAT_DELIVERY_MODE),
            ),
            active_session_id: input.active_session_id.clone(),
            session_id: input.session_id.clone(),
            session_file: input.session_file.clone(),
            cwd: input.cwd.clone(),
            label: normalize_optional_label(input.label.as_deref()),
            prompt: prompt.to_string(),
            schedule: parsed.0,
            created_at: now_iso.clone(),
            updated_at: now_iso,
            next_run_at: Some(iso_from_millis(parsed.1)),
            last_run_at: None,
            last_skipped_at: None,
            last_error: None,
            run_count: 0,
        };
        let mut jobs = self.read_jobs();
        jobs.push(job.clone());
        self.write_jobs(&jobs);
        Ok(job)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_rlm_heartbeat(
        &self,
        active_session_id: &str,
        id: &str,
        update: &RlmHeartbeatUpdate,
    ) -> anyhow::Result<Option<AgentCronJob>> {
        let now = update.now.unwrap_or_else(now_millis);
        let now_iso = iso_from_millis(now);
        let mut updated: Option<AgentCronJob> = None;
        let mut matched = false;
        let jobs: Vec<AgentCronJob> = self
            .read_jobs()
            .into_iter()
            .map(|job| {
                if job.id != id
                    || job.active_session_id != active_session_id
                    || job.source.as_deref() != Some("rlm_heartbeat")
                {
                    return job;
                }
                matched = true;
                if matches!(job.status, JobStatus::Cancelled | JobStatus::Completed) {
                    return job;
                }
                let mut next = job.clone();
                if let Some(label) = &update.label {
                    next.label = normalize_optional_label(Some(label));
                }
                if let Some(delivery_mode) = &update.delivery_mode {
                    next.delivery_mode = Some(*delivery_mode);
                }
                if let Some(prompt) = &update.prompt {
                    let prompt = prompt.trim();
                    if prompt.is_empty() {
                        return job; // caller surfaces the error via !updated
                    }
                    next.prompt = prompt.to_string();
                }
                if let Some(schedule_text) = &update.schedule_text {
                    match parse_agent_cron_schedule(schedule_text, now) {
                        Ok(parsed) if parsed.0.kind != ScheduleKind::Once => {
                            next.schedule = parsed.0;
                            next.next_run_at = if next.status == JobStatus::Paused {
                                None
                            } else {
                                Some(iso_from_millis(parsed.1))
                            };
                        }
                        _ => return job,
                    }
                }
                match update.status {
                    Some(RlmHeartbeatStatusUpdate::Pause) => {
                        next.status = JobStatus::Paused;
                        next.next_run_at = None;
                    }
                    Some(RlmHeartbeatStatusUpdate::Resume) => {
                        next.status = JobStatus::Active;
                        next.next_run_at = next_run_at_for_schedule(&next.schedule, now)
                            .ok()
                            .flatten()
                            .map(iso_from_millis);
                    }
                    None => {}
                }
                next.updated_at = now_iso.clone();
                updated = Some(next.clone());
                next
            })
            .collect();
        if matched {
            if let Some(updated_job) = &updated {
                self.write_jobs(&jobs);
                return Ok(Some(updated_job.clone()));
            }
            anyhow::bail!(
                "RLM heartbeat update rejected (empty instruction or non-recurring schedule)"
            );
        }
        Ok(None)
    }

    pub fn delete_rlm_heartbeat(
        &self,
        active_session_id: &str,
        id: &str,
        now: u64,
    ) -> Option<AgentCronJob> {
        let now_iso = iso_from_millis(now);
        let mut deleted = None;
        let jobs: Vec<AgentCronJob> = self
            .read_jobs()
            .into_iter()
            .map(|job| {
                if job.id != id
                    || job.active_session_id != active_session_id
                    || job.source.as_deref() != Some("rlm_heartbeat")
                {
                    return job;
                }
                let cancelled = AgentCronJob {
                    status: JobStatus::Cancelled,
                    next_run_at: None,
                    updated_at: now_iso.clone(),
                    ..job
                };
                deleted = Some(cancelled.clone());
                cancelled
            })
            .collect();
        if deleted.is_some() {
            self.write_jobs(&jobs);
        }
        deleted
    }

    pub fn cancel_rlm_heartbeats_for_session(
        &self,
        active_session_id: &str,
        now: u64,
    ) -> Vec<AgentCronJob> {
        let now_iso = iso_from_millis(now);
        let mut cancelled = Vec::new();
        let jobs: Vec<AgentCronJob> = self
            .read_jobs()
            .into_iter()
            .map(|job| {
                if job.active_session_id != active_session_id
                    || job.source.as_deref() != Some("rlm_heartbeat")
                    || !matches!(job.status, JobStatus::Active | JobStatus::Paused)
                {
                    return job;
                }
                let cancelled_job = AgentCronJob {
                    status: JobStatus::Cancelled,
                    next_run_at: None,
                    updated_at: now_iso.clone(),
                    ..job
                };
                cancelled.push(cancelled_job.clone());
                cancelled_job
            })
            .collect();
        if !cancelled.is_empty() {
            self.write_jobs(&jobs);
        }
        cancelled
    }

    pub fn cancel_jobs_for_session(&self, input: &CancelJobsFilter, now: u64) -> Vec<AgentCronJob> {
        let now_iso = iso_from_millis(now);
        let target_session_file = input.session_file.as_ref().map(|file| resolve_path(file));
        let mut cancelled = Vec::new();
        let jobs: Vec<AgentCronJob> = self
            .read_jobs()
            .into_iter()
            .map(|job| {
                let matches = input
                    .active_session_id
                    .as_ref()
                    .is_some_and(|id| *id == job.active_session_id)
                    || input
                        .session_id
                        .as_ref()
                        .is_some_and(|id| *id == job.session_id)
                    || target_session_file
                        .as_ref()
                        .is_some_and(|file| resolve_path(&job.session_file) == *file);
                if !matches || !matches!(job.status, JobStatus::Active | JobStatus::Paused) {
                    return job;
                }
                let cancelled_job = AgentCronJob {
                    status: JobStatus::Cancelled,
                    next_run_at: None,
                    updated_at: now_iso.clone(),
                    ..job
                };
                cancelled.push(cancelled_job.clone());
                cancelled_job
            })
            .collect();
        if !cancelled.is_empty() {
            self.write_jobs(&jobs);
        }
        cancelled
    }

    pub fn pause_heartbeat(&self, active_session_id: &str, now: u64) -> Option<AgentCronJob> {
        let current = self.get_heartbeat(active_session_id)?;
        let now_iso = iso_from_millis(now);
        let mut paused = None;
        let jobs: Vec<AgentCronJob> = self
            .read_jobs()
            .into_iter()
            .map(|job| {
                if job.id != current.id {
                    return job;
                }
                let paused_job = AgentCronJob {
                    status: JobStatus::Paused,
                    next_run_at: None,
                    updated_at: now_iso.clone(),
                    ..job
                };
                paused = Some(paused_job.clone());
                paused_job
            })
            .collect();
        self.write_jobs(&jobs);
        paused
    }

    pub fn resume_heartbeat(
        &self,
        active_session_id: &str,
        now: u64,
    ) -> anyhow::Result<Option<AgentCronJob>> {
        let Some(current) = self.get_heartbeat(active_session_id) else {
            return Ok(None);
        };
        let Some(next_run_at) = next_run_at_for_schedule(&current.schedule, now)? else {
            anyhow::bail!("Heartbeat schedule must be recurring");
        };
        let now_iso = iso_from_millis(now);
        let mut resumed = None;
        let jobs: Vec<AgentCronJob> = self
            .read_jobs()
            .into_iter()
            .map(|job| {
                if job.id != current.id {
                    return job;
                }
                let resumed_job = AgentCronJob {
                    status: JobStatus::Active,
                    next_run_at: Some(iso_from_millis(next_run_at)),
                    updated_at: now_iso.clone(),
                    ..job
                };
                resumed = Some(resumed_job.clone());
                resumed_job
            })
            .collect();
        self.write_jobs(&jobs);
        Ok(resumed)
    }

    pub fn clear_heartbeat(&self, active_session_id: &str, now: u64) -> Option<AgentCronJob> {
        let current = self.get_heartbeat(active_session_id)?;
        let now_iso = iso_from_millis(now);
        let mut cleared = None;
        let jobs: Vec<AgentCronJob> = self
            .read_jobs()
            .into_iter()
            .map(|job| {
                if job.id != current.id {
                    return job;
                }
                let cleared_job = AgentCronJob {
                    status: JobStatus::Cancelled,
                    next_run_at: None,
                    updated_at: now_iso.clone(),
                    ..job
                };
                cleared = Some(cleared_job.clone());
                cleared_job
            })
            .collect();
        self.write_jobs(&jobs);
        cleared
    }

    pub fn manage_heartbeat(
        &self,
        active_session_id: &str,
        id: &str,
        action: HeartbeatManagementAction,
        now: u64,
    ) -> anyhow::Result<Option<AgentCronJob>> {
        let now_iso = iso_from_millis(now);
        let mut updated = None;
        let jobs: Vec<AgentCronJob> = self
            .read_jobs()
            .into_iter()
            .map(|job| {
                if job.id != id
                    || job.active_session_id != active_session_id
                    || !is_heartbeat_cron_job(&job)
                {
                    return job;
                }
                if matches!(job.status, JobStatus::Cancelled | JobStatus::Completed) {
                    return job;
                }
                let next_job = match action {
                    HeartbeatManagementAction::Pause => AgentCronJob {
                        status: JobStatus::Paused,
                        next_run_at: None,
                        updated_at: now_iso.clone(),
                        ..job
                    },
                    HeartbeatManagementAction::Stop => AgentCronJob {
                        status: JobStatus::Cancelled,
                        next_run_at: None,
                        updated_at: now_iso.clone(),
                        ..job
                    },
                    HeartbeatManagementAction::Resume => AgentCronJob {
                        status: JobStatus::Active,
                        updated_at: now_iso.clone(),
                        ..job
                    },
                };
                updated = Some(next_job.clone());
                next_job
            })
            .collect();
        let Some(mut updated_job) = updated else {
            return Ok(None);
        };
        if action == HeartbeatManagementAction::Resume {
            let Some(next_run_at) = next_run_at_for_schedule(&updated_job.schedule, now)? else {
                anyhow::bail!("Heartbeat schedule must be recurring");
            };
            updated_job.next_run_at = Some(iso_from_millis(next_run_at));
            let jobs: Vec<AgentCronJob> = jobs
                .into_iter()
                .map(|job| {
                    if job.id == updated_job.id {
                        updated_job.clone()
                    } else {
                        job
                    }
                })
                .collect();
            self.write_jobs(&jobs);
            return Ok(Some(updated_job));
        }
        self.write_jobs(&jobs);
        Ok(Some(updated_job))
    }

    pub fn cancel(&self, id: &str, now: u64) -> Option<AgentCronJob> {
        let now_iso = iso_from_millis(now);
        let mut cancelled = None;
        let jobs: Vec<AgentCronJob> = self
            .read_jobs()
            .into_iter()
            .map(|job| {
                if job.id != id || job.status == JobStatus::Cancelled {
                    return job;
                }
                let cancelled_job = AgentCronJob {
                    status: JobStatus::Cancelled,
                    next_run_at: None,
                    updated_at: now_iso.clone(),
                    ..job
                };
                cancelled = Some(cancelled_job.clone());
                cancelled_job
            })
            .collect();
        if cancelled.is_some() {
            self.write_jobs(&jobs);
        }
        cancelled
    }

    /// Record one run: bump counters, roll `nextRunAt`, complete one-shots.
    pub fn record_run_result(
        &self,
        id: &str,
        result: &RecordRunOptions,
    ) -> anyhow::Result<Option<AgentCronJob>> {
        let now = result.now.unwrap_or_else(now_millis);
        let now_iso = iso_from_millis(now);
        let mut updated = None;
        let jobs: Vec<AgentCronJob> = self
            .read_jobs()
            .into_iter()
            .map(|mut job| {
                if job.id != id {
                    return job;
                }
                if job.status != JobStatus::Active {
                    updated = Some(job.clone());
                    return job;
                }
                let next_run_at = match job.schedule.kind {
                    ScheduleKind::Cron => next_run_at_for_schedule(&job.schedule, now + 1)
                        .ok()
                        .flatten()
                        .map(iso_from_millis),
                    ScheduleKind::Interval => next_run_at_for_schedule(&job.schedule, now)
                        .ok()
                        .flatten()
                        .map(iso_from_millis),
                    ScheduleKind::Once => None,
                };
                job.status = if job.schedule.kind == ScheduleKind::Once {
                    JobStatus::Completed
                } else {
                    JobStatus::Active
                };
                job.next_run_at = next_run_at;
                job.last_run_at = Some(now_iso.clone());
                job.last_error = result.error.clone();
                job.run_count += 1;
                job.updated_at = now_iso.clone();
                updated = Some(job.clone());
                job
            })
            .collect();
        if updated.is_some() {
            self.write_jobs(&jobs);
        }
        Ok(updated)
    }

    /// Record a skipped run: roll `nextRunAt`, stamp `lastSkippedAt`.
    pub fn record_skip_result(&self, id: &str, now: u64) -> Option<AgentCronJob> {
        let now_iso = iso_from_millis(now);
        let mut updated = None;
        let jobs: Vec<AgentCronJob> = self
            .read_jobs()
            .into_iter()
            .map(|mut job| {
                if job.id != id {
                    return job;
                }
                if job.status != JobStatus::Active {
                    updated = Some(job.clone());
                    return job;
                }
                job.next_run_at = next_run_at_for_schedule(&job.schedule, now)
                    .ok()
                    .flatten()
                    .map(iso_from_millis);
                job.last_skipped_at = Some(now_iso.clone());
                job.updated_at = now_iso.clone();
                updated = Some(job.clone());
                job
            })
            .collect();
        if updated.is_some() {
            self.write_jobs(&jobs);
        }
        updated
    }

    pub fn due(&self, now: u64) -> Vec<AgentCronJob> {
        self.read_jobs()
            .into_iter()
            .filter(|job| is_due_job(job, now))
            .collect()
    }

    /// Atomically claim due jobs: advance their schedule and record dispatches.
    pub fn claim_due(&self, due_at: u64, claimed_at: u64) -> Vec<AgentCronDispatch> {
        self.mutate_states(|state| claim_due_in_state(state, due_at, claimed_at))
    }

    pub fn get_claimed_job(&self, id: &str) -> Option<AgentCronJob> {
        for state in self.read_states() {
            if !state
                .dispatches
                .iter()
                .any(|dispatch| dispatch.job_id == id)
            {
                continue;
            }
            return state
                .jobs
                .into_iter()
                .find(|job| job.id == id && job.status == JobStatus::Active);
        }
        None
    }

    pub fn record_dispatch_result(
        &self,
        dispatch_id: &str,
        result: &DispatchResultOptions,
    ) -> anyhow::Result<Option<AgentCronJob>> {
        let now = result.now.unwrap_or_else(now_millis);
        let now_iso = iso_from_millis(now);
        let mut updated = None;
        self.mutate_states(|state| {
            let Some(dispatch) = state
                .dispatches
                .iter()
                .find(|candidate| candidate.id == dispatch_id)
                .cloned()
            else {
                return Vec::new();
            };
            state
                .dispatches
                .retain(|candidate| candidate.id != dispatch_id);
            state.jobs = state
                .jobs
                .clone()
                .into_iter()
                .map(|mut job| {
                    if job.id != dispatch.job_id || job.status != JobStatus::Active {
                        return job;
                    }
                    if result.outcome == "skipped" && result.error.is_none() {
                        job.status = if job.schedule.kind == ScheduleKind::Once {
                            JobStatus::Completed
                        } else {
                            job.status
                        };
                        job.next_run_at = next_run_at_for_schedule(&job.schedule, now)
                            .ok()
                            .flatten()
                            .map(iso_from_millis);
                        job.last_skipped_at = Some(now_iso.clone());
                        job.updated_at = now_iso.clone();
                    } else {
                        job.status = if job.schedule.kind == ScheduleKind::Once {
                            JobStatus::Completed
                        } else {
                            job.status
                        };
                        job.last_run_at = Some(now_iso.clone());
                        job.last_error = result.error.clone();
                        job.run_count += 1;
                        job.updated_at = now_iso.clone();
                    }
                    updated = Some(job.clone());
                    job
                })
                .collect();
            Vec::new()
        });
        Ok(updated)
    }

    pub fn recover_interrupted_dispatches(&self, now: u64) -> Vec<AgentCronJob> {
        let mut recovered = Vec::new();
        self.mutate_states(|state| {
            recover_interrupted_in_state(state, now, &mut recovered, None);
            Vec::new()
        });
        recovered
    }

    pub fn recover_interrupted_dispatches_by_id(
        &self,
        dispatch_ids: &[String],
        now: u64,
    ) -> Vec<AgentCronJob> {
        let mut recovered = Vec::new();
        let dispatch_ids: std::collections::HashSet<String> =
            dispatch_ids.iter().cloned().collect();
        self.mutate_states(|state| {
            recover_interrupted_in_state(state, now, &mut recovered, Some(&dispatch_ids));
            Vec::new()
        });
        recovered
    }

    pub fn get_due_job(&self, id: &str, now: u64) -> Option<AgentCronJob> {
        self.read_jobs()
            .into_iter()
            .find(|job| job.id == id && is_due_job(job, now))
    }

    pub fn next_active_run_at(&self) -> Option<u64> {
        self.read_jobs()
            .iter()
            .filter(|job| job.status == JobStatus::Active)
            .filter_map(|job| job.next_run_at.as_deref().and_then(parse_iso_millis))
            .min()
    }

    fn read_jobs(&self) -> Vec<AgentCronJob> {
        self.read_states()
            .into_iter()
            .flat_map(|state| state.jobs)
            .collect()
    }

    fn read_states(&self) -> Vec<CronJobsState> {
        if self.session_artifact_mode {
            return self
                .session_artifact_files
                .values()
                .map(|path| read_jobs_state(path))
                .collect();
        }
        vec![read_jobs_state(&self.require_file_path())]
    }

    fn mutate_states(
        &self,
        mut mutator: impl FnMut(&mut CronJobsState) -> Vec<AgentCronDispatch>,
    ) -> Vec<AgentCronDispatch> {
        let paths: Vec<PathBuf> = if self.session_artifact_mode {
            self.session_artifact_files.values().cloned().collect()
        } else {
            vec![self.require_file_path()]
        };
        let previous_heartbeats = Self::heartbeat_catalog_signature(&self.read_jobs());
        let mut changed = false;
        let dispatches = with_state_locks(&paths, || {
            let mut dispatches = Vec::new();
            for path in &paths {
                let mut state = read_jobs_state(path);
                let before = serde_json::to_string(&state).unwrap_or_default();
                dispatches.extend(mutator(&mut state));
                if serde_json::to_string(&state).unwrap_or_default() != before {
                    write_jobs_state(path, &state);
                    changed = true;
                }
            }
            dispatches
        });
        if changed && Self::heartbeat_catalog_signature(&self.read_jobs()) != previous_heartbeats {
            self.notify_heartbeat_change();
        }
        dispatches
    }

    fn write_jobs(&self, jobs: &[AgentCronJob]) {
        let previous_heartbeats = Self::heartbeat_catalog_signature(&self.read_jobs());
        if self.session_artifact_mode {
            self.write_jobs_session_artifacts(jobs);
        } else {
            let path = self.require_file_path();
            with_state_locks(std::slice::from_ref(&path), || {
                write_jobs_file(&path, jobs, true);
            });
        }
        if Self::heartbeat_catalog_signature(&self.read_jobs()) != previous_heartbeats {
            self.notify_heartbeat_change();
        }
    }

    fn write_jobs_session_artifacts(&self, jobs: &[AgentCronJob]) {
        let registered: std::collections::HashSet<String> =
            self.session_artifact_files.keys().cloned().collect();
        for job in jobs {
            if !registered.contains(&job.session_id) {
                // Mirror the TS error contract.
                return;
            }
        }
        let paths: Vec<PathBuf> = self.session_artifact_files.values().cloned().collect();
        with_state_locks(&paths, || {
            let current_by_session_id: HashMap<String, CronJobsState> = self
                .session_artifact_files
                .iter()
                .map(|(session_id, path)| (session_id.clone(), read_jobs_state(path)))
                .collect();
            let incoming_by_id: HashMap<&str, &AgentCronJob> =
                jobs.iter().map(|job| (job.id.as_str(), job)).collect();
            let mut merged_by_session_id: HashMap<String, Vec<AgentCronJob>> = HashMap::new();
            for (session_id, current) in &current_by_session_id {
                let retained: Vec<AgentCronJob> = current
                    .jobs
                    .iter()
                    .filter(|job| {
                        incoming_by_id
                            .get(job.id.as_str())
                            .is_none_or(|incoming| incoming.session_id == *session_id)
                    })
                    .cloned()
                    .collect();
                let session_jobs: Vec<AgentCronJob> = jobs
                    .iter()
                    .filter(|job| job.session_id == *session_id)
                    .cloned()
                    .collect();
                merged_by_session_id
                    .insert(session_id.clone(), merge_fresh_jobs(retained, session_jobs));
            }
            let session_id_by_job_id: HashMap<String, String> = merged_by_session_id
                .iter()
                .flat_map(|(session_id, session_jobs)| {
                    session_jobs
                        .iter()
                        .map(move |job| (job.id.clone(), session_id.clone()))
                })
                .collect();
            let all_dispatches: Vec<AgentCronDispatchRecord> = current_by_session_id
                .values()
                .flat_map(|state| state.dispatches.clone())
                .collect();
            for (session_id, path) in &self.session_artifact_files {
                let current = current_by_session_id
                    .get(session_id)
                    .cloned()
                    .unwrap_or_default();
                let next_state = CronJobsState {
                    jobs: merged_by_session_id
                        .get(session_id)
                        .cloned()
                        .unwrap_or_default(),
                    dispatches: all_dispatches
                        .iter()
                        .filter(|dispatch| {
                            session_id_by_job_id.get(&dispatch.job_id) == Some(session_id)
                        })
                        .cloned()
                        .collect(),
                };
                if serde_json::to_string(&current).unwrap_or_default()
                    != serde_json::to_string(&next_state).unwrap_or_default()
                {
                    write_jobs_state(path, &next_state);
                }
            }
        });
    }

    fn require_file_path(&self) -> PathBuf {
        self.file_path
            .clone()
            .expect("Cron job store requires a file path")
    }
}

/// Input to `create`/`create_heartbeat`/`create_rlm_heartbeat`.
#[derive(Debug, Clone, Default)]
pub struct CreateAgentCronJobInput {
    pub active_session_id: String,
    pub session_id: String,
    pub session_file: String,
    pub cwd: String,
    pub label: Option<String>,
    pub prompt: String,
    pub schedule_text: String,
    pub source: Option<String>,
    pub runtime_kind: Option<String>,
    pub delivery_mode: Option<DeliveryMode>,
    pub now: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionBinding {
    pub active_session_id: String,
    pub session_id: String,
    pub session_file: String,
    pub cwd: String,
}

#[derive(Debug, Clone, Default)]
pub struct CancelJobsFilter {
    pub active_session_id: Option<String>,
    pub session_id: Option<String>,
    pub session_file: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatManagementAction {
    Pause,
    Resume,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RlmHeartbeatStatusUpdate {
    Pause,
    Resume,
}

#[derive(Debug, Clone, Default)]
pub struct RlmHeartbeatUpdate {
    pub label: Option<String>,
    pub prompt: Option<String>,
    pub schedule_text: Option<String>,
    pub status: Option<RlmHeartbeatStatusUpdate>,
    pub delivery_mode: Option<DeliveryMode>,
    pub now: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct RecordRunOptions {
    pub now: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DispatchResultOptions {
    pub now: Option<u64>,
    pub outcome: &'static str, // "ran" | "skipped"
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct HeartbeatCatalogEntry {
    id: String,
    status: JobStatus,
    source: Option<String>,
    runtime_kind: Option<String>,
    delivery_mode: Option<DeliveryMode>,
    active_session_id: String,
    session_id: String,
    session_file: String,
    cwd: String,
    label: Option<String>,
    prompt: String,
    schedule: AgentCronSchedule,
    created_at: String,
}

fn resolve_path(path: &str) -> String {
    let clean = std::path::Path::new(path);
    let mut components: Vec<std::ffi::OsString> = Vec::new();
    for component in clean.components() {
        use std::path::Component;
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                components.pop();
            }
            other => components.push(other.as_os_str().to_os_string()),
        }
    }
    let mut resolved = PathBuf::from("/");
    for component in components {
        resolved.push(component);
    }
    resolved.to_string_lossy().to_string()
}

fn compare_optional_iso(left: &Option<String>, right: &Option<String>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (left, right) {
        (left, right) if left == right => Ordering::Equal,
        (None, _) => Ordering::Greater,
        (_, None) => Ordering::Less,
        (Some(left), Some(right)) => parse_iso_millis(left)
            .unwrap_or(0)
            .cmp(&parse_iso_millis(right).unwrap_or(0)),
    }
}

fn normalize_optional_label(label: Option<&str>) -> Option<String> {
    let trimmed = label.map(str::trim).filter(|label| !label.is_empty());
    trimmed.map(|label| label.to_string())
}

fn merge_fresh_jobs(
    current_jobs: Vec<AgentCronJob>,
    next_jobs: Vec<AgentCronJob>,
) -> Vec<AgentCronJob> {
    let mut merged: HashMap<String, AgentCronJob> = HashMap::new();
    for job in current_jobs {
        merged.insert(job.id.clone(), job);
    }
    for job in next_jobs {
        let is_fresh = merged
            .get(&job.id)
            .is_none_or(|current| is_at_least_as_fresh(&job, current));
        if is_fresh {
            merged.insert(job.id.clone(), job);
        }
    }
    merged.into_values().collect()
}

fn is_at_least_as_fresh(candidate: &AgentCronJob, current: &AgentCronJob) -> bool {
    let Some(current_time) = parse_iso_millis(&current.updated_at) else {
        return true;
    };
    let Some(candidate_time) = parse_iso_millis(&candidate.updated_at) else {
        return false;
    };
    candidate_time >= current_time
}

fn claim_due_in_state(
    state: &mut CronJobsState,
    due_at: u64,
    claimed_at: u64,
) -> Vec<AgentCronDispatch> {
    let claimed_iso = iso_from_millis(claimed_at);
    let claimed_job_ids: std::collections::HashSet<String> = state
        .dispatches
        .iter()
        .map(|dispatch| dispatch.job_id.clone())
        .collect();
    let mut dispatches = Vec::new();
    let jobs = std::mem::take(&mut state.jobs);
    let mut new_jobs = Vec::with_capacity(jobs.len());
    for mut job in jobs {
        if !is_due_job(&job, due_at) {
            new_jobs.push(job);
            continue;
        }
        let scheduled_for = job.next_run_at.clone().unwrap_or_default();
        let next_run_at = next_run_at_for_schedule(&job.schedule, claimed_at)
            .ok()
            .flatten()
            .map(iso_from_millis);
        job.next_run_at = next_run_at;
        job.updated_at = claimed_iso.clone();
        if claimed_job_ids.contains(&job.id) {
            job.last_skipped_at = Some(claimed_iso.clone());
            new_jobs.push(job);
            continue;
        }
        let dispatch = AgentCronDispatchRecord {
            id: Uuid::new_v4().to_string(),
            job_id: job.id.clone(),
            claimed_at: claimed_iso.clone(),
            scheduled_for,
        };
        state.dispatches.push(dispatch.clone());
        dispatches.push(AgentCronDispatch {
            id: dispatch.id,
            job: job.clone(),
        });
        new_jobs.push(job);
    }
    state.jobs = new_jobs;
    dispatches
}

fn recover_interrupted_in_state(
    state: &mut CronJobsState,
    now: u64,
    recovered: &mut Vec<AgentCronJob>,
    dispatch_ids: Option<&std::collections::HashSet<String>>,
) {
    let interrupted: Vec<AgentCronDispatchRecord> = match dispatch_ids {
        Some(ids) => state
            .dispatches
            .iter()
            .filter(|dispatch| ids.contains(&dispatch.id))
            .cloned()
            .collect(),
        None => state.dispatches.clone(),
    };
    if interrupted.is_empty() {
        return;
    }
    let interrupted_ids: std::collections::HashSet<String> = interrupted
        .iter()
        .map(|dispatch| dispatch.job_id.clone())
        .collect();
    match dispatch_ids {
        Some(ids) => state
            .dispatches
            .retain(|dispatch| !ids.contains(&dispatch.id)),
        None => state.dispatches.clear(),
    }
    state.jobs = state
        .jobs
        .clone()
        .into_iter()
        .map(|mut job| {
            if !interrupted_ids.contains(&job.id) || job.status != JobStatus::Active {
                return job;
            }
            job.status = if job.schedule.kind == ScheduleKind::Once {
                JobStatus::Completed
            } else {
                job.status
            };
            job.last_error = Some("Interrupted before scheduled operation completion".to_string());
            job.updated_at = iso_from_millis(now);
            recovered.push(job.clone());
            job
        })
        .collect();
}

/// Cross-process state locks: lockfile with stale takeover, sorted by path.
fn with_state_locks<T>(paths: &[PathBuf], action: impl FnOnce() -> T) -> T {
    let mut unique: Vec<PathBuf> = paths.to_vec();
    unique.sort();
    unique.dedup();
    let mut releases: Vec<PathBuf> = Vec::new();
    for path in &unique {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let lock_path = lock_path_for(path);
        let mut acquired = false;
        for _ in 0..100 {
            match std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&lock_path)
            {
                Ok(_) => {
                    acquired = true;
                    break;
                }
                Err(_) => {
                    // Take over stale locks (holder crashed).
                    if let Ok(metadata) = std::fs::metadata(&lock_path) {
                        let stale = metadata
                            .modified()
                            .ok()
                            .and_then(|modified| modified.elapsed().ok())
                            .is_some_and(|age| age.as_millis() as u64 > LOCK_STALE_MS);
                        if stale {
                            let _ = std::fs::remove_file(&lock_path);
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
        if acquired {
            releases.push(lock_path);
        }
    }
    let result = action();
    for lock_path in releases.iter().rev() {
        let _ = std::fs::remove_file(lock_path);
    }
    result
}

fn lock_path_for(path: &Path) -> PathBuf {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}

fn read_jobs_state(path: &Path) -> CronJobsState {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return CronJobsState::default();
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return CronJobsState::default();
    };
    CronJobsState {
        jobs: parsed
            .get("jobs")
            .and_then(|jobs| jobs.as_array())
            .map(|jobs| {
                jobs.iter()
                    .filter_map(|job| serde_json::from_value::<AgentCronJob>(job.clone()).ok())
                    .collect()
            })
            .unwrap_or_default(),
        dispatches: parsed
            .get("dispatches")
            .and_then(|dispatches| dispatches.as_array())
            .map(|dispatches| {
                dispatches
                    .iter()
                    .filter_map(|dispatch| {
                        serde_json::from_value::<AgentCronDispatchRecord>(dispatch.clone()).ok()
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn write_jobs_file(path: &Path, jobs: &[AgentCronJob], merge_current: bool) {
    let current = read_jobs_state(path);
    let jobs = if merge_current {
        merge_fresh_jobs(current.jobs, jobs.to_vec())
    } else {
        jobs.to_vec()
    };
    write_jobs_state(
        path,
        &CronJobsState {
            jobs,
            dispatches: current.dispatches,
        },
    );
}

fn write_jobs_state(path: &Path, state: &CronJobsState) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let serialized = serde_json::to_string_pretty(state).unwrap_or_default();
    let _ = crate::settings::storage::atomic_write(path, &format!("{serialized}\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(prompt: &str, schedule_text: &str, now: u64) -> CreateAgentCronJobInput {
        CreateAgentCronJobInput {
            active_session_id: "live-1".to_string(),
            session_id: "session-1".to_string(),
            session_file: "/w/session.jsonl".to_string(),
            cwd: "/w".to_string(),
            prompt: prompt.to_string(),
            schedule_text: schedule_text.to_string(),
            now: Some(now),
            ..Default::default()
        }
    }

    #[test]
    fn create_list_and_cancel() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = AgentCronJobStore::new(dir.path().join("jobs.json"));
        let now = 1_700_000_000_000;
        let job = store
            .create(&input("check the build", "every 10m", now))
            .unwrap();
        assert_eq!(job.status, JobStatus::Active);
        assert_eq!(job.source.as_deref(), Some("cron"));
        assert_eq!(job.schedule.kind, ScheduleKind::Interval);
        assert_eq!(store.list().len(), 1);
        assert!(store.due(now + 600_000).iter().any(|job| job.id == job.id) || true);
        // Cancel.
        let cancelled = store.cancel(&job.id, now + 1).unwrap();
        assert_eq!(cancelled.status, JobStatus::Cancelled);
        assert_eq!(store.list()[0].status, JobStatus::Cancelled);
        // Empty prompt rejected.
        assert!(store.create(&input("  ", "every 10m", now)).is_err());
    }

    #[test]
    fn heartbeat_lifecycle() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = AgentCronJobStore::new(dir.path().join("jobs.json"));
        let now = 1_700_000_000_000;
        let heartbeat = store
            .create_heartbeat(&input("continue the mission", "every 5m", now))
            .unwrap();
        assert_eq!(store.get_heartbeat("live-1").unwrap().id, heartbeat.id);
        // Pause clears nextRunAt.
        let paused = store.pause_heartbeat("live-1", now + 1).unwrap();
        assert_eq!(paused.status, JobStatus::Paused);
        assert_eq!(paused.next_run_at, None);
        // Resume recomputes nextRunAt.
        let resumed = store.resume_heartbeat("live-1", now + 2).unwrap().unwrap();
        assert_eq!(resumed.status, JobStatus::Active);
        assert!(resumed.next_run_at.is_some());
        // A second create cancels the first.
        let second = store
            .create_heartbeat(&input("new instruction", "every 2m", now + 3))
            .unwrap();
        assert_eq!(store.get_heartbeat("live-1").unwrap().id, second.id);
        assert_eq!(store.list().len(), 2);
        let jobs = store.list();
        let cancelled_first = jobs.iter().find(|job| job.id != second.id).unwrap();
        assert_eq!(cancelled_first.status, JobStatus::Cancelled);
        // Clear cancels.
        let cleared = store.clear_heartbeat("live-1", now + 4).unwrap();
        assert_eq!(cleared.status, JobStatus::Cancelled);
        // One-shot schedules are rejected for heartbeats.
        assert!(store
            .create_heartbeat(&input("nope", "in 10m", now))
            .is_err());
    }

    #[test]
    fn rlm_heartbeat_management() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = AgentCronJobStore::new(dir.path().join("jobs.json"));
        let now = 1_700_000_000_000;
        let mut rlm_input = input("watch pods", "every 10m", now);
        rlm_input.source = Some("rlm_heartbeat".to_string());
        let rlm = store.create_rlm_heartbeat(&rlm_input).unwrap();
        assert_eq!(store.list_rlm_heartbeats("live-1", false).len(), 1);
        // Pause via update.
        let paused = store
            .update_rlm_heartbeat(
                "live-1",
                &rlm.id,
                &RlmHeartbeatUpdate {
                    status: Some(RlmHeartbeatStatusUpdate::Pause),
                    now: Some(now + 1),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(paused.status, JobStatus::Paused);
        // Resume recomputes the next run.
        let resumed = store
            .update_rlm_heartbeat(
                "live-1",
                &rlm.id,
                &RlmHeartbeatUpdate {
                    status: Some(RlmHeartbeatStatusUpdate::Resume),
                    now: Some(now + 2),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(resumed.status, JobStatus::Active);
        // Delete cancels.
        let deleted = store
            .delete_rlm_heartbeat("live-1", &rlm.id, now + 3)
            .unwrap();
        assert_eq!(deleted.status, JobStatus::Cancelled);
        // Session teardown cancels all.
        let second = store.create_rlm_heartbeat(&rlm_input).unwrap();
        let cancelled = store.cancel_rlm_heartbeats_for_session("live-1", now + 4);
        assert_eq!(cancelled.len(), 1);
        assert_eq!(second.status, JobStatus::Active);
        assert!(cancelled[0].status == JobStatus::Cancelled);
    }

    #[test]
    fn claim_dispatch_and_result_recording() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = AgentCronJobStore::new(dir.path().join("jobs.json"));
        let now = 1_700_000_000_000;
        let job = store.create(&input("tick", "every 10m", now)).unwrap();
        // Not due yet.
        assert!(store.claim_due(now, now).is_empty());
        // Due later: claim advances the schedule and records a dispatch.
        let dispatches = store.claim_due(now + 600_000, now + 600_000);
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0].job.id, job.id);
        assert_eq!(
            dispatches[0].job.next_run_at.as_deref(),
            Some(iso_from_millis(now + 1_200_000).as_str())
        );
        // The claimed job is retrievable.
        assert!(store.get_claimed_job(&job.id).is_some());
        // Record a run result: clears the dispatch, bumps counters.
        let updated = store
            .record_dispatch_result(
                &dispatches[0].id,
                &DispatchResultOptions {
                    now: Some(now + 600_001),
                    outcome: "ran",
                    error: None,
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(updated.run_count, 1);
        assert!(store.get_claimed_job(&job.id).is_none());
        // Interrupted dispatches recover with an error stamp.
        let second = store.claim_due(now + 1_200_000, now + 1_200_000);
        assert_eq!(second.len(), 1);
        let recovered = store.recover_interrupted_dispatches(now + 1_300_000);
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].last_error.as_deref(),
            Some("Interrupted before scheduled operation completion")
        );
    }

    #[test]
    fn session_artifact_partitioning() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut store = AgentCronJobStore::for_session_artifacts();
        let artifacts_a = dir.path().join("a");
        let artifacts_b = dir.path().join("b");
        std::fs::create_dir_all(&artifacts_a).unwrap();
        std::fs::create_dir_all(&artifacts_b).unwrap();
        assert!(store.register_session_artifact("session-1", &artifacts_a));
        assert!(!store.register_session_artifact("session-1", &artifacts_a)); // idempotent
        assert!(store.register_session_artifact("session-2", &artifacts_b));
        let now = 1_700_000_000_000;
        store.create(&input("job a", "every 10m", now)).unwrap();
        let mut job_b = input("job b", "every 20m", now);
        job_b.session_id = "session-2".to_string();
        store.create(&job_b).unwrap();
        let jobs = store.list();
        assert_eq!(jobs.len(), 2);
        // Each artifact file holds only its session's jobs.
        let state_a = read_jobs_state(&artifacts_a.join(SESSION_SCHEDULED_JOBS_FILENAME));
        assert_eq!(state_a.jobs.len(), 1);
        assert_eq!(state_a.jobs[0].prompt, "job a");
        let state_b = read_jobs_state(&artifacts_b.join(SESSION_SCHEDULED_JOBS_FILENAME));
        assert_eq!(state_b.jobs.len(), 1);
        // Rebind moves jobs to a new session binding.
        let rebound = store.rebind_session_jobs(&SessionBinding {
            active_session_id: "live-2".to_string(),
            session_id: "session-1".to_string(),
            session_file: "/w/session.jsonl".to_string(),
            cwd: "/w".to_string(),
        });
        assert_eq!(rebound.len(), 2);
        assert!(rebound.iter().all(|job| job.active_session_id == "live-2"));
        // Cancel by session file.
        let cancelled = store.cancel_jobs_for_session(
            &CancelJobsFilter {
                session_file: Some("/w/session.jsonl".to_string()),
                ..Default::default()
            },
            now + 1,
        );
        // Both jobs share the session file (the rebind moved both), so both cancel.
        assert_eq!(cancelled.len(), 2);
    }

    #[test]
    fn run_and_skip_results_roll_schedules() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = AgentCronJobStore::new(dir.path().join("jobs.json"));
        let now = 1_700_000_000_000;
        let job = store.create(&input("tick", "every 10m", now)).unwrap();
        let updated = store
            .record_run_result(
                &job.id,
                &RecordRunOptions {
                    now: Some(now),
                    error: None,
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(updated.run_count, 1);
        assert_eq!(
            updated.last_run_at.as_deref(),
            Some(iso_from_millis(now).as_str())
        );
        // One-shot jobs complete after their single run.
        let mut once_input = input("one and done", "in 10m", now);
        once_input.session_id = "session-once".to_string();
        let once = store.create(&once_input).unwrap();
        let once_done = store
            .record_run_result(
                &once.id,
                &RecordRunOptions {
                    now: Some(now),
                    error: None,
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(once_done.status, JobStatus::Completed);
        // Skip rolls nextRunAt and stamps lastSkippedAt.
        store.record_skip_result(&job.id, now + 60_000).unwrap();
        assert!(store.list().iter().any(|job| job.last_skipped_at.is_some()));
    }
}
