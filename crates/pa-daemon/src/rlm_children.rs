//! Supervisor-backed RLM child sessions: the daemon's implementation of the
//! pa-core [`RlmSubagentHost`] seam. `rlm.spawn` and `rlm.create_session`
//! create real daemon sessions through the supervisor link (one supervised
//! worker process per child), prompt them, and keep the parent-side roster
//! the kernel reads through `rlm.list_subagents`, `rlm.collect`, and
//! `rlm.delete_subagent`.
//!
//! Mechanism note (PORTING-NOTES): the TS daemon hosts children in-process
//! (`createRlmSubagentRuntime`); this redesign gives every child its own
//! supervised worker process, created through the supervisor like any other
//! session. The kernel-visible surface (handles, roster rows, collect
//! snapshots, selector errors) is TS parity.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use pa_core::kernel::rlm_runtime::create_default_rlm_subagent_session_name;
use pa_core::session_engine::rlm_host::{
    RlmChildResult, RlmCreateSessionHandle, RlmCreateSessionRequest, RlmDeleteSubagentResult,
    RlmHostFuture, RlmSpawnHandle, RlmSpawnRequest, RlmSubagentActivity, RlmSubagentEntry,
    RlmSubagentHost,
};
use pa_types::daemon::{DaemonCommand, DaemonSessionLifecycle, PromptInput};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::rlm_child_model::{
    assert_thinking_supported, compact_rlm_text, resolve_child_model, rlm_child_label,
};
use crate::supervisor_link::SupervisorLink;
use crate::util::now_ms;

/// Depth bound without an explicit override (TS `resolveRlmMaxDepth` default).
pub const DEFAULT_RLM_MAX_DEPTH: u32 = 2;
/// Deadline for one child-session create over the link (TS uses 120s).
const CREATE_TIMEOUT_MS: u64 = 120_000;
const PROMPT_TIMEOUT_MS: u64 = 30_000;
const STATE_TIMEOUT_MS: u64 = 30_000;
const KILL_TIMEOUT_MS: u64 = 30_000;
/// Grace over a collect budget passed to the worker `wait_for_idle`.
const IDLE_WAIT_GRACE_MS: u64 = 5_000;
/// Prompts longer than this are not mirrored into create runtime metadata.
const RUNTIME_METADATA_PROMPT_MAX: usize = 4096;
/// The parent identity children are spawned from: recursion bounds, the
/// inherited model selector and thinking level, and the parent session's
/// persistence identity.
#[derive(Debug, Clone, Default)]
pub struct ParentIdentity {
    pub rlm_depth: u32,
    pub rlm_max_depth: u32,
    /// Parent model selector (`provider/id`); children inherit it.
    pub model: Option<String>,
    /// Parent working directory; children inherit it.
    pub cwd: Option<String>,
    /// Persisted parent session id (keys the session-artifacts tree).
    pub session_id: Option<String>,
    /// Parent session file path.
    pub session_file: Option<String>,
    /// Default thinking level children inherit.
    pub thinking: Option<String>,
    /// Verification seam: create children with a scripted engine file.
    pub child_script: Option<String>,
}

impl ParentIdentity {
    /// Identity with the default depth bound (TS `resolveRlmMaxDepth`).
    pub fn with_default_depth() -> Self {
        Self {
            rlm_max_depth: DEFAULT_RLM_MAX_DEPTH,
            ..Default::default()
        }
    }
}

/// One tracked child session.
#[derive(Debug)]
struct ChildRecord {
    rlm_child_id: String,
    session_name: String,
    active_session_id: String,
    session_id: Option<String>,
    session_dir: String,
    label: String,
    started_at_ms: u64,
    /// Terminal state (`done` | `error`); running while absent.
    settled_status: Option<&'static str>,
    answer_preview: Option<String>,
    answer_captured: bool,
}

impl ChildRecord {
    /// Raw run status: `running` | `done` | `error`.
    fn status(&self) -> &'static str {
        self.settled_status.unwrap_or("running")
    }

    /// Kernel-roster status: `running` | `completed` | `error`.
    fn roster_status(&self) -> &'static str {
        match self.status() {
            "done" => "completed",
            "error" => "error",
            _ => "running",
        }
    }

    fn matches(&self, target: &str) -> bool {
        self.rlm_child_id == target
            || self.active_session_id == target
            || self.session_name == target
            || self.session_id.as_deref() == Some(target)
    }
}

/// RLM children as supervisor-managed daemon sessions. A cheap shared handle:
/// the daemon hands the same children registry to every kernel handler call.
pub struct SupervisorChildSessions {
    inner: Arc<SupervisorChildSessionsInner>,
}

struct SupervisorChildSessionsInner {
    link: Arc<SupervisorLink>,
    agent_dir: PathBuf,
    parent_active_session_id: String,
    // The identity lock is only ever a data swap (never held across an
    // await), so a std mutex keeps the setter callable from sync engine
    // paths (the create command) without a runtime `block_on`.
    identity: std::sync::Mutex<ParentIdentity>,
    children: Mutex<Vec<Arc<Mutex<ChildRecord>>>>,
}

impl Clone for SupervisorChildSessions {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl SupervisorChildSessions {
    /// Children registry bound to one parent session worker.
    pub fn new(
        link: Arc<SupervisorLink>,
        agent_dir: PathBuf,
        parent_active_session_id: String,
    ) -> Self {
        Self {
            inner: Arc::new(SupervisorChildSessionsInner {
                link,
                agent_dir,
                parent_active_session_id,
                identity: std::sync::Mutex::new(ParentIdentity::with_default_depth()),
                children: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Replace the parent identity (the worker session sets it once its own
    /// session exists).
    pub fn set_identity(&self, identity: ParentIdentity) {
        *self.inner.identity.lock().expect("identity lock") = identity;
    }

    /// Set only the inherited model selector (the engine resolves its model
    /// when it builds the session, after the create command arrived).
    pub fn set_model(&self, model: String) {
        self.inner.identity.lock().expect("identity lock").model = Some(model);
    }

    fn entry(record: &ChildRecord) -> RlmSubagentEntry {
        let running = record.settled_status.is_none();
        RlmSubagentEntry {
            rlm_child_id: record.rlm_child_id.clone(),
            active_session_id: Some(record.active_session_id.clone()),
            session_id: record.session_id.clone(),
            session_name: record.session_name.clone(),
            session_dir: record.session_dir.clone(),
            status: record.roster_status(),
            // Live tool introspection across worker processes is a follow-up
            // (PORTING-NOTES); a running child reports `executing`.
            activity: running.then_some(RlmSubagentActivity {
                kind: "executing",
                tool_name: None,
            }),
            tool_use_count: None,
            duration_ms: Some(now_ms().saturating_sub(record.started_at_ms)),
            answer_preview: record.answer_preview.clone(),
            replied_since_task: None,
            progress_note: None,
            label: (!record.label.is_empty()).then(|| record.label.clone()),
            last_activity_at: Some(record.started_at_ms),
            activity_stale_ms: None,
        }
    }

    fn collect_result(record: &ChildRecord) -> RlmChildResult {
        RlmChildResult {
            rlm_child_id: record.rlm_child_id.clone(),
            session_name: Some(record.session_name.clone()),
            session_dir: Some(record.session_dir.clone()),
            status: record.status(),
            settled: record.settled_status.is_some(),
            answer_preview: record.answer_preview.clone(),
            error: None,
            duration_ms: Some(now_ms().saturating_sub(record.started_at_ms)),
            tool_use_count: None,
            replied_since_task: None,
        }
    }
}

impl SupervisorChildSessionsInner {
    /// Send one daemon command over the supervisor link and return its
    /// response data. The link owns timeouts/reconnects; this only maps the
    /// command to its wire value.
    async fn command(&self, command: &DaemonCommand, timeout_ms: u64) -> Result<Value> {
        let wire = serde_json::to_value(command).context("serialize supervisor link command")?;
        self.link
            .request_success(wire, Duration::from_millis(timeout_ms))
            .await
    }

    /// A child session name conflicts when any retained or live child of
    /// this parent already holds it (the parent-side half of the TS
    /// `_assertRlmSubagentSessionNameAvailable` check; the supervisor's
    /// create assertion is the daemon-wide half).
    async fn assert_name_available(&self, name: &str, depth: u32) -> Result<()> {
        let children = self.children.lock().await;
        for record in children.iter() {
            if record.lock().await.session_name == name {
                bail!(
                    "Agent name \"{name}\" is unavailable: an agent of that name already exists at depth {depth} under this parent"
                );
            }
        }
        Ok(())
    }

    /// The per-child session directory under the parent's artifacts tree
    /// (TS `_createChildRlmSessionDir`); the child session persists inside it.
    fn child_session_dir(&self, child_id: &str, identity: &ParentIdentity) -> Result<PathBuf> {
        let base = match &identity.session_id {
            Some(session_id) => self
                .agent_dir
                .join("session-artifacts")
                .join(session_id)
                .join(child_id),
            // No persistent parent artifacts dir: an ephemeral temp dir, the
            // TS `_createEphemeralRlmSessionDir` fallback.
            None => std::env::temp_dir().join(format!("prime-agent-rlm-{child_id}")),
        };
        std::fs::create_dir_all(&base)
            .with_context(|| format!("create RLM child session dir {}", base.display()))?;
        Ok(base)
    }

    /// Launch one child session over the supervisor link and prompt it.
    /// `depth` is the child's recursion depth; `session_dir` holds its
    /// persisted session; `model` is the resolved `provider/id` selector.
    #[allow(clippy::too_many_arguments)]
    async fn launch_child(
        &self,
        child_id: &str,
        name: Option<&str>,
        prompt: &str,
        depth: u32,
        model: &str,
        thinking: Option<&str>,
        cwd: &str,
        session_dir: &Path,
        runtime_metadata: Option<Value>,
        identity: &ParentIdentity,
    ) -> Result<CreatedChild> {
        let mut config = json!({
            "cwd": cwd,
            "sessionDir": session_dir.to_string_lossy(),
            "rlmDepth": depth,
            "rlmMaxDepth": identity.rlm_max_depth,
        });
        if let Some((provider, id)) = model.split_once('/') {
            config["provider"] = json!(provider);
            config["model"] = json!(id);
        } else {
            config["model"] = json!(model);
        }
        if let Some(thinking) = thinking {
            config["thinking"] = json!(thinking);
        }
        if let Some(parent_file) = &identity.session_file {
            config["parentSessionPath"] = json!(parent_file);
        }
        if let Some(script) = &identity.child_script {
            config["script"] = json!(script);
        }
        // Runtime metadata mirrors the TS subagent runtime identity; a
        // depth-0 resident session carries none (it is a plain root session).
        let runtime_metadata = runtime_metadata.map(|mut metadata| {
            if let Some(session_id) = &identity.session_id {
                metadata["parentSessionId"] = json!(session_id);
            }
            if let Some(parent_file) = &identity.session_file {
                metadata["parentSessionFile"] = json!(parent_file);
            }
            if prompt.len() <= RUNTIME_METADATA_PROMPT_MAX {
                metadata["prompt"] = json!(prompt);
            }
            // The resolved model rides the metadata so the supervisor's
            // display entry carries it for passive hydration.
            if let Some((provider, model_id)) = model.split_once('/') {
                metadata["model"] = json!({ "provider": provider, "modelId": model_id });
            }
            metadata
        });
        let create = DaemonCommand::Create {
            id: None,
            session_path: None,
            continue_recent: None,
            no_session: None,
            name: name.map(str::to_string),
            config: Some(config),
            // RLM children never report telemetry (the depth-0 gate in the
            // session engine installs nothing); the worker's own opt-out
            // stays process-level.
            telemetry_disabled: None,
            runtime_metadata,
            lifecycle: Some(DaemonSessionLifecycle::Resident),
            env: None,
            launch_env: None,
            rest: Default::default(),
        };
        let summary = self
            .command(&create, CREATE_TIMEOUT_MS)
            .await
            .with_context(|| format!("spawn RLM child session {child_id}"))?;
        let created = CreatedChild::from_summary(&summary, session_dir)?;
        // A failed prompt tears the just-created session down (TS kills the
        // created session in the create-path catch block).
        if let Err(error) = self.prompt_child(&created.active_session_id, prompt).await {
            let _ = self.kill_child(&created.active_session_id).await;
            return Err(error);
        }
        Ok(created)
    }

    /// Parse a created-session summary into its ids (TS `createRlmRootSession`
    /// reads `activeSessionId`/`sessionId`/`sessionFile`/`sessionName`).
    fn created_summary_ids(summary: &Value) -> Result<CreatedSessionIds> {
        let active_session_id = summary
            .get("activeSessionId")
            .or_else(|| summary.get("id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("supervisor returned a session summary without an id"))?;
        Ok(CreatedSessionIds {
            active_session_id: active_session_id.to_string(),
            session_id: summary
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string),
            session_file: summary
                .get("sessionFile")
                .and_then(Value::as_str)
                .map(str::to_string),
            session_name: summary
                .get("sessionName")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    async fn prompt_child(&self, active_session_id: &str, prompt: &str) -> Result<()> {
        let command = DaemonCommand::Prompt {
            id: None,
            active_session_id: active_session_id.to_string(),
            message: prompt.to_string(),
            input: PromptInput {
                content: None,
                images: None,
                streaming_behavior: None,
                queue_if_busy: None,
                expand_prompt_templates: None,
                source: Some(json!("rpc")),
                agent_message_id: None,
                custom_message: None,
                queue_key: None,
                prefix_messages: None,
                admission_id: None,
            },
            rest: Default::default(),
        };
        self.command(&command, PROMPT_TIMEOUT_MS)
            .await
            .with_context(|| format!("prompt RLM child session {active_session_id}"))?;
        Ok(())
    }

    async fn kill_child(&self, active_session_id: &str) -> Result<()> {
        let command = DaemonCommand::Kill {
            id: None,
            active_session_id: active_session_id.to_string(),
            rest: Default::default(),
        };
        self.command(&command, KILL_TIMEOUT_MS)
            .await
            .with_context(|| format!("kill RLM child session {active_session_id}"))?;
        Ok(())
    }

    /// Whether the child worker still has work in flight (streaming or
    /// queued). `Err` means the child cannot be reached right now.
    async fn child_busy(&self, active_session_id: &str) -> Result<bool> {
        let command = DaemonCommand::GetState {
            id: None,
            active_session_id: active_session_id.to_string(),
            rest: Default::default(),
        };
        let state = self.command(&command, STATE_TIMEOUT_MS).await?;
        Ok(state
            .get("isStreaming")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || state
                .get("sessionActions")
                .and_then(|actions| actions.get("queuedCount"))
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0)
    }

    /// The child's final answer text, compacted for the roster preview.
    async fn child_answer(&self, active_session_id: &str) -> Result<Option<String>> {
        let command = DaemonCommand::GetLastAssistantText {
            id: None,
            active_session_id: active_session_id.to_string(),
            rest: Default::default(),
        };
        let answer = self.command(&command, STATE_TIMEOUT_MS).await?;
        Ok(answer
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(compact_rlm_text))
    }

    /// Best-effort bounded wait for one child to go idle. The wait is a
    /// snapshot helper, not a gate: its timeout is not an error, and the
    /// caller re-reads the child's state afterwards (TS collect: "a
    /// timeout returns current snapshots, never an error").
    async fn wait_for_child(&self, active_session_id: &str, budget: Duration) {
        if budget.is_zero() {
            return;
        }
        let command = DaemonCommand::WaitForIdle {
            id: None,
            active_session_id: active_session_id.to_string(),
            rest: Default::default(),
        };
        let _ = self
            .command(&command, budget.as_millis() as u64 + IDLE_WAIT_GRACE_MS)
            .await;
    }

    /// Refresh one record against its worker: settle a child whose worker
    /// ran out of work and capture its answer once. An unreachable child
    /// keeps its last known state (the supervisor may be restarting).
    async fn refresh_record(&self, record: &Arc<Mutex<ChildRecord>>) {
        let active_session_id = record.lock().await.active_session_id.clone();
        let busy = self.child_busy(&active_session_id).await;
        if !matches!(busy, Ok(false)) {
            return;
        }
        // Capture the answer before taking the record lock (the capture is
        // a link round trip).
        let answer = self.child_answer(&active_session_id).await.ok().flatten();
        let mut record = record.lock().await;
        if record.settled_status.is_none() {
            record.settled_status = Some("done");
            if !record.answer_captured {
                record.answer_preview = answer;
                record.answer_captured = true;
            }
        }
    }

    /// The one record matching a selector, or the TS selector errors
    /// (`No direct RLM {kind} matches ...` / `... is ambiguous ...`).
    async fn resolve_record(
        &self,
        target: &str,
        miss_kind: &str,
    ) -> Result<Arc<Mutex<ChildRecord>>> {
        let children = self.children.lock().await;
        let mut matches: Vec<Arc<Mutex<ChildRecord>>> = Vec::new();
        for record in children.iter() {
            if record.lock().await.matches(target) {
                matches.push(Arc::clone(record));
            }
        }
        match matches.len() {
            0 => bail!(
                "No direct RLM {miss_kind} matches \"{target}\" in the current parent session"
            ),
            1 => Ok(Arc::clone(matches.first().expect("one match"))),
            _ => bail!(
                "RLM {miss_kind} selector \"{target}\" is ambiguous in the current parent session"
            ),
        }
    }
}

/// Parsed ids of one created child session.
struct CreatedSessionIds {
    active_session_id: String,
    session_id: Option<String>,
    session_file: Option<String>,
    session_name: Option<String>,
}

struct CreatedChild {
    active_session_id: String,
    session_id: Option<String>,
    session_file: Option<String>,
    session_name: Option<String>,
    session_dir: String,
    summary_rlm_depth: Option<u64>,
}

impl CreatedChild {
    fn from_summary(summary: &Value, session_dir: &Path) -> Result<Self> {
        let ids = SupervisorChildSessionsInner::created_summary_ids(summary)?;
        Ok(Self {
            active_session_id: ids.active_session_id,
            session_id: ids.session_id,
            session_file: ids.session_file,
            session_name: ids.session_name,
            session_dir: session_dir.to_string_lossy().to_string(),
            summary_rlm_depth: summary.get("rlmDepth").and_then(Value::as_u64),
        })
    }
}

impl RlmSubagentHost for SupervisorChildSessions {
    fn spawn(&self, request: RlmSpawnRequest) -> RlmHostFuture<RlmSpawnHandle> {
        let this = Arc::clone(&self.inner);
        Box::pin(async move {
            let identity = this.identity.lock().expect("identity lock").clone();
            if identity.rlm_depth >= identity.rlm_max_depth {
                bail!(
                    "RLM recursion depth limit reached (RLM_DEPTH={}, RLM_MAX_DEPTH={})",
                    identity.rlm_depth,
                    identity.rlm_max_depth
                );
            }
            let child_id = format!("sub-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
            let name = request.name.clone().unwrap_or_else(|| {
                create_default_rlm_subagent_session_name(&request.prompt, &child_id)
            });
            this.assert_name_available(&name, identity.rlm_depth + 1)
                .await?;
            let model = resolve_child_model(
                &this.agent_dir,
                request.model.as_deref(),
                identity.model.as_deref(),
                "subagent",
            )?;
            assert_thinking_supported(&this.agent_dir, request.thinking.as_deref(), &model)?;
            let thinking = request.thinking.as_deref().or(identity.thinking.as_deref());
            let child_dir = this.child_session_dir(&child_id, &identity)?;
            let cwd = identity.cwd.clone().unwrap_or_else(|| "/".to_string());
            let runtime_metadata = json!({
                "kind": "subagent",
                "rlmChildId": child_id,
                "parentActiveSessionId": this.parent_active_session_id,
                "rlmDepth": identity.rlm_depth + 1,
                "createdAt": now_ms(),
            });
            let created = this
                .launch_child(
                    &child_id,
                    Some(&name),
                    &request.prompt,
                    identity.rlm_depth + 1,
                    &model,
                    thinking,
                    &cwd,
                    &child_dir,
                    Some(runtime_metadata),
                    &identity,
                )
                .await?;
            let record = ChildRecord {
                rlm_child_id: child_id.clone(),
                session_name: created.session_name.clone().unwrap_or_else(|| name.clone()),
                active_session_id: created.active_session_id.clone(),
                session_id: created.session_id,
                session_dir: created.session_dir.clone(),
                label: rlm_child_label(&request.prompt),
                started_at_ms: now_ms(),
                settled_status: None,
                answer_preview: None,
                answer_captured: false,
            };
            this.children
                .lock()
                .await
                .push(Arc::new(Mutex::new(record)));
            Ok(RlmSpawnHandle {
                rlm_child_id: child_id,
                name,
                session_dir: created.session_dir,
                model,
            })
        })
    }

    fn create_session(
        &self,
        request: RlmCreateSessionRequest,
    ) -> RlmHostFuture<RlmCreateSessionHandle> {
        let this = Arc::clone(&self.inner);
        Box::pin(async move {
            let identity = this.identity.lock().expect("identity lock").clone();
            if identity.rlm_depth != 0 {
                bail!("rlm.create_session is available only from a depth-0 session");
            }
            let model = resolve_child_model(
                &this.agent_dir,
                request.model.as_deref(),
                identity.model.as_deref(),
                "top-level session",
            )?;
            assert_thinking_supported(&this.agent_dir, request.thinking.as_deref(), &model)?;
            // A depth-0 resident session is created exactly like a client
            // `create`: the shared sessions dir and the requested cwd
            // (TS `resolve(this._cwd, rawCwd)`), no per-child artifacts dir.
            let cwd = match &request.cwd {
                Some(cwd) if Path::new(cwd).is_absolute() => PathBuf::from(cwd),
                Some(cwd) => Path::new(identity.cwd.as_deref().unwrap_or("/")).join(cwd),
                None => PathBuf::from(identity.cwd.clone().unwrap_or_else(|| "/".to_string())),
            };
            let sessions_dir = crate::paths::sessions_dir(&this.agent_dir)?;
            std::fs::create_dir_all(&sessions_dir)
                .with_context(|| format!("create sessions dir {}", sessions_dir.display()))?;
            let thinking = request.thinking.as_deref().or(identity.thinking.as_deref());
            let created = this
                .launch_child(
                    "root",
                    request.name.as_deref(),
                    &request.prompt,
                    0,
                    &model,
                    thinking,
                    &cwd.to_string_lossy(),
                    &sessions_dir,
                    None,
                    &identity,
                )
                .await?;
            // The TS create-path summary validation: a resident depth-0
            // session must never report another depth.
            if created.summary_rlm_depth.is_some_and(|depth| depth != 0) {
                bail!("Daemon supervisor returned an invalid depth-0 session summary");
            }
            Ok(RlmCreateSessionHandle {
                active_session_id: created.active_session_id.clone(),
                session_id: created
                    .session_id
                    .clone()
                    .unwrap_or_else(|| created.active_session_id.clone()),
                name: created
                    .session_name
                    .clone()
                    .unwrap_or_else(|| created.active_session_id.clone()),
                session_file: created.session_file.unwrap_or_default(),
                model,
            })
        })
    }

    fn list_subagents(&self) -> RlmHostFuture<Vec<RlmSubagentEntry>> {
        let this = Arc::clone(&self.inner);
        Box::pin(async move {
            let records = this.children.lock().await.clone();
            let mut entries = Vec::with_capacity(records.len());
            for record in &records {
                this.refresh_record(record).await;
                let entry = {
                    let record = record.lock().await;
                    SupervisorChildSessions::entry(&record)
                };
                entries.push(entry);
            }
            Ok(entries)
        })
    }

    fn delete_subagent(&self, target: String) -> RlmHostFuture<RlmDeleteSubagentResult> {
        let this = Arc::clone(&self.inner);
        Box::pin(async move {
            // Selector errors surface unwrapped (TS parity: the
            // `No direct RLM subagent matches ...` message is the product
            // surface); only the kill below gets a delete context.
            let record = this.resolve_record(&target, "subagent").await?;
            let active_session_id = record.lock().await.active_session_id.clone();
            // Kill first: a failed kill keeps the child tracked so the caller
            // can retry; a successful kill removes it from the registry.
            // The `rlmLedgerDelete` marker tells the supervisor this kill
            // is a delete (a plain stop must not tombstone the child).
            let record_guard = record.lock().await;
            let command = DaemonCommand::Kill {
                id: None,
                active_session_id: active_session_id.clone(),
                rest: serde_json::Map::from_iter([
                    ("rlmLedgerDelete".to_string(), json!("user")),
                    ("rlmChildId".to_string(), json!(record_guard.rlm_child_id)),
                ]),
            };
            drop(record_guard);
            this.command(&command, KILL_TIMEOUT_MS)
                .await
                .with_context(|| format!("kill RLM child \"{target}\""))?;
            let entry = {
                let record = record.lock().await;
                SupervisorChildSessions::entry(&record)
            };
            this.children
                .lock()
                .await
                .retain(|candidate| !Arc::ptr_eq(candidate, &record));
            Ok(RlmDeleteSubagentResult {
                subagent: entry,
                outcome: Some("deleted"),
            })
        })
    }

    fn collect(&self, targets: Vec<String>, timeout_ms: u64) -> RlmHostFuture<Vec<RlmChildResult>> {
        let this = Arc::clone(&self.inner);
        Box::pin(async move {
            // Resolve targets outside the registry lock: `resolve_record`
            // takes it too, and the tokio mutex is not re-entrant.
            let records = if targets.is_empty() {
                this.children.lock().await.clone()
            } else {
                let mut records = Vec::with_capacity(targets.len());
                for target in &targets {
                    records.push(this.resolve_record(target, "child").await?);
                }
                records
            };
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            let mut results = Vec::with_capacity(records.len());
            for record in &records {
                this.refresh_record(record).await;
                let still_running = record.lock().await.settled_status.is_none();
                if still_running {
                    // Wait inside the shared budget, then re-read the child:
                    // a timeout yields the current snapshot, never an error.
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let active_session_id = record.lock().await.active_session_id.clone();
                    this.wait_for_child(&active_session_id, remaining).await;
                    this.refresh_record(record).await;
                }
                let result = {
                    let record = record.lock().await;
                    SupervisorChildSessions::collect_result(&record)
                };
                results.push(result);
            }
            Ok(results)
        })
    }
}
