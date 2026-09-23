//! Supervisor-side roster serving: subscribe/unsubscribe handling, worker
//! roster deltas, the stop-path passivation, and the `roster_update`
//! pushes subscribers receive (the roster arms of TS
//! `daemon-supervisor.ts`; the store itself lives in `agent_roster.rs`,
//! and the seeding/hydration arms live in `supervisor_roster_seed.rs`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pa_types::daemon::agent_roster::AgentRosterEntry;
use pa_types::daemon::DaemonOutbound;
use serde_json::{json, Value};

use crate::lease::canonical_session_path;
use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::registry::ResidentWorker;
use crate::supervisor::{ClientRouting, Supervisor, ROUTE_TIMEOUT_MS};
use crate::supervisor_roster_seed::family_descends_from;

impl Supervisor {
    /// `roster_subscribe` (TS: sets the client flag and answers with the
    /// full roster snapshot; the caller stores the flag). Pure in-memory:
    /// the boot seed and the create path's family seed
    /// (`supervisor_roster_seed.rs`) publish `roster_update` for rows
    /// that land between subscribes, so the answer itself never reads
    /// the ledger or a transcript - a per-switch reseed scaled with the
    /// whole family.
    pub(crate) fn handle_roster_subscribe(
        &self,
        command_id: &str,
        type_name: &str,
    ) -> DaemonResponse {
        let roster = self.roster.lock().unwrap().entries();
        response_success(
            Some(command_id),
            type_name,
            Some(json!({ "roster": roster })),
        )
    }

    /// The seed roots (TS: every worker's `sessionFile` with the durable
    /// create's `sessionPath` as fallback), canonicalized.
    pub(crate) async fn roster_seed_roots(self: &Arc<Self>) -> HashSet<PathBuf> {
        let mut roots = HashSet::new();
        for resident in self.registry.list().await {
            let descriptor = resident.descriptor.lock().await;
            let root = descriptor
                .session_file
                .clone()
                .or_else(|| descriptor.create_command.session_path.clone());
            if let Some(root) = root {
                roots.insert(canonical_session_path(Path::new(&root)));
            }
        }
        roots
    }

    /// `roster_unsubscribe`.
    pub(crate) fn handle_roster_unsubscribe(
        &self,
        command_id: &str,
        type_name: &str,
    ) -> DaemonResponse {
        response_success(Some(command_id), type_name, None)
    }

    /// `worker_roster_delta`: a worker pushes its slim session summary (the
    /// Rust-native form of the TS `roster_delta` worker frame) so the
    /// roster tracks live status without polling. Authenticated by the
    /// worker token, like `worker_register`.
    pub(crate) async fn handle_worker_roster_delta(
        self: &Arc<Self>,
        command_id: &str,
        type_name: &str,
        worker_token: &str,
        summary: Value,
        removed: Vec<String>,
    ) -> DaemonResponse {
        let Some(resident) = self.registry.find_by_token(worker_token).await else {
            return response_failure(
                Some(command_id),
                type_name,
                "Worker authentication failed",
                None,
            );
        };
        let mut changed = Vec::new();
        if let Some(entry) = self.write_roster_summary(&summary, Some(&resident.worker_id)) {
            changed.push(entry);
        }
        let mut removed_ids = Vec::new();
        for agent_id in removed {
            let mut roster = self.roster.lock().unwrap();
            if roster.get(&agent_id).is_some() {
                roster.delete(&agent_id);
                removed_ids.push(agent_id);
            }
        }
        self.push_roster_update(changed, removed_ids);
        response_success(Some(command_id), type_name, None)
    }

    /// Write one summary into the roster and push the change to
    /// subscribers. Returns the classified entry.
    pub(crate) fn write_roster_summary(
        &self,
        summary: &Value,
        worker_id: Option<&str>,
    ) -> Option<AgentRosterEntry> {
        let entry = self
            .roster
            .lock()
            .unwrap()
            .write_summary(summary.clone(), worker_id, None);
        self.push_roster_update(vec![entry.clone()], Vec::new());
        Some(entry)
    }

    /// Refresh one resident worker's entry from its live `get_state`
    /// (registration, adoption, and create flows).
    pub(crate) async fn refresh_roster_entry(self: &Arc<Self>, resident: &Arc<ResidentWorker>) {
        let response = self
            .route_command(resident, "get_state", json!({}), ROUTE_TIMEOUT_MS)
            .await;
        if let Ok(response) = response {
            if response.success {
                if let Some(data) = response.data {
                    self.write_roster_summary(&data, Some(&resident.worker_id));
                }
            }
        }
    }

    /// TS `flipWorkerRosterEntriesInactive` (the Rust form: one pass in
    /// place, no ledger reseed, no transcript read): a stopped worker's
    /// rows settle where they are. An ephemeral (client-owned) worker's
    /// rows and queued children die with the registration; a subagent row
    /// whose ledger edge is tombstoned (or whose transcript is gone) dies
    /// with the deletion; a subagent row still descending from a
    /// surviving resident root passivates - its summary keeps every
    /// durable display field (model, thinking level, cwd) and drops only
    /// the live-only fields (TS `passivatedWorkerRosterEntry`); a
    /// top-level row is removed, exactly like the remove+reseed this
    /// replaces (the reseed never resurrected roots, and a roster that
    /// passivated every stopped top-level row would grow forever).
    pub(crate) async fn passivate_roster_worker(
        self: &Arc<Self>,
        worker_id: &str,
        ephemeral: bool,
    ) {
        let owned: Vec<AgentRosterEntry> = {
            let roster = self.roster.lock().unwrap();
            roster
                .entries_for_worker(worker_id)
                .into_iter()
                .cloned()
                .collect()
        };
        if owned.is_empty() {
            return;
        }
        // The stopping worker's family view - live edges and the
        // surviving resident roots (the caller removed the worker from
        // the registry first) - decides each subagent row's fate. This is
        // the old remove+reseed's reach, without its per-family
        // transcript reads; a ledger failure degrades to an empty view,
        // exactly like the old reseed degraded to no rows.
        let (_, parent_by_child) = self.live_edges_and_parents().await.unwrap_or_default();
        let roots = self.roster_seed_roots().await;
        let mut changed = Vec::new();
        let mut removed = Vec::new();
        {
            let mut roster = self.roster.lock().unwrap();
            for entry in owned {
                let subagent = entry
                    .summary
                    .get("rlmChildId")
                    .and_then(Value::as_str)
                    .is_some();
                // A subagent row survives only while a live edge still
                // carries it and a surviving resident root anchors its
                // family walk; everything else (top-level rows included)
                // is removed.
                let anchored = subagent
                    && entry
                        .summary
                        .get("sessionFile")
                        .and_then(Value::as_str)
                        .map(|file| {
                            parent_by_child
                                .get(&canonical_session_path(Path::new(file)))
                                .is_some_and(|parent| {
                                    family_descends_from(&parent_by_child, parent, &roots)
                                })
                        })
                        .unwrap_or(false);
                if !ephemeral && entry.queued_child != Some(true) && anchored {
                    let passivated =
                        roster.write_summary(passivated_summary(entry.summary), None, None);
                    changed.push(passivated);
                } else {
                    roster.delete(&entry.agent_id);
                    removed.push(entry.agent_id);
                }
            }
        }
        self.push_roster_update(changed, removed);
    }

    /// Push one `roster_update` to subscribed clients. The TS supervisor
    /// batches pending mutations into one push; roster writes are low-rate
    /// here, so each mutation pushes immediately and subscribers apply
    /// entries idempotently by agent id.
    pub(crate) fn push_roster_update(&self, changed: Vec<AgentRosterEntry>, removed: Vec<String>) {
        if changed.is_empty() && removed.is_empty() {
            return;
        }
        let update = DaemonOutbound::RosterUpdate {
            changed: serde_json::to_value(&changed).unwrap_or(Value::Null),
            removed: (!removed.is_empty()).then_some(removed),
            resync: None,
            rest: Default::default(),
        };
        let Ok(payload) = serde_json::to_value(&update) else {
            return;
        };
        let _ = self
            .events
            .send((ClientRouting::RosterSubscribers, payload));
    }
}

/// TS `passivatedWorkerRosterEntry`: the stop keeps every durable display
/// field - the model selector, the thinking level, the cwd, the session
/// identity rows - and strips only the live-runtime fields; the heartbeat
/// and cron registration marks survive when they were true.
fn passivated_summary(summary: Value) -> Value {
    let mut summary = summary;
    let Some(object) = summary.as_object_mut() else {
        return summary;
    };
    let keep_heartbeat = object.get("hasRegisteredHeartbeat").and_then(Value::as_bool)
        == Some(true);
    let keep_cron = object.get("hasRegisteredCronJob").and_then(Value::as_bool) == Some(true);
    for key in [
        "activeSessionId",
        "directAttachedClients",
        "hasActiveHeartbeat",
        "hasRegisteredHeartbeat",
        "hasRegisteredCronJob",
        "hasRunningRlmChildren",
        "isBashRunning",
        "isRunningTools",
        "workerState",
        "workerPid",
    ] {
        object.remove(key);
    }
    object.insert("activity".to_string(), json!("idle"));
    object.insert("isSessionActive".to_string(), json!(false));
    object.insert("isStreaming".to_string(), json!(false));
    object.insert("isCompacting".to_string(), json!(false));
    object.insert("attachedClients".to_string(), json!(0));
    if keep_heartbeat {
        object.insert("hasRegisteredHeartbeat".to_string(), json!(true));
    }
    if keep_cron {
        object.insert("hasRegisteredCronJob".to_string(), json!(true));
    }
    if let Some(session_id) = object.get("sessionId").and_then(Value::as_str) {
        object.insert("id".to_string(), json!(session_id));
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor_roster_seed::tests::{
        append_family_edge, drain_roster_pushes, live_child_summary, register_root_worker,
        roster_fixture, roster_row_for_child, write_display_file,
    };
    use pa_types::daemon::agent_roster::AgentRosterStatus;

    /// `roster_subscribe` is a pure in-memory snapshot: a family the
    /// ledger knows (with readable transcripts, unseeded) never enters
    /// the roster through the subscribe answer - the old per-switch
    /// reseed read the whole family here.
    #[tokio::test]
    async fn subscribe_is_a_pure_in_memory_snapshot() {
        let (dir, supervisor, root_file, child_file) = roster_fixture().await;
        register_root_worker(&supervisor, "w-root", &root_file).await;
        let agent_dir = dir.join("agent");
        append_family_edge(&agent_dir, &agent_dir.join("sessions"), "sub-9", &root_file, &child_file);
        let mut events = supervisor.events.subscribe();
        // The root's live row is the only roster row.
        let mut root_summary = live_child_summary(&root_file, &child_file);
        root_summary["runtimeKind"] = json!("top-level");
        root_summary["sessionId"] = json!("root-persisted");
        root_summary["id"] = json!("root-persisted");
        root_summary["sessionFile"] = json!(root_file.to_string_lossy());
        root_summary.as_object_mut().unwrap().remove("rlmChildId");
        root_summary.as_object_mut().unwrap().remove("parentSessionPath");
        supervisor.write_roster_summary(&root_summary, Some("w-root"));
        let _ = drain_roster_pushes(&mut events);

        let first = supervisor.handle_roster_subscribe("s1", "roster_subscribe");
        assert!(first.success);
        let roster = first.data.expect("roster snapshot")["roster"].clone();
        assert_eq!(
            roster.as_array().map(Vec::len),
            Some(1),
            "only the in-memory row answers: {roster}"
        );
        // A pure snapshot is stable: subscribing again answers the same.
        let second = supervisor.handle_roster_subscribe("s2", "roster_subscribe");
        assert_eq!(second.data.expect("roster snapshot")["roster"], roster);
        assert!(drain_roster_pushes(&mut events).is_empty(), "subscribe never pushes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TS `flipWorkerRosterEntriesInactive`: a stopped subagent under a
    /// surviving resident root passivates in place - the summary keeps
    /// its model, thinking level, and cwd, and drops only the
    /// live-runtime fields. One push carries the settled row.
    #[tokio::test]
    async fn stop_passivates_an_anchored_child_preserving_display_fields() {
        let (dir, supervisor, root_file, child_file) = roster_fixture().await;
        register_root_worker(&supervisor, "w-root", &root_file).await;
        let agent_dir = dir.join("agent");
        append_family_edge(&agent_dir, &agent_dir.join("sessions"), "sub-9", &root_file, &child_file);
        let mut events = supervisor.events.subscribe();
        supervisor.write_roster_summary(&live_child_summary(&root_file, &child_file), Some("w-child"));
        let _ = drain_roster_pushes(&mut events);

        supervisor.passivate_roster_worker("w-child", false).await;
        let pushes = drain_roster_pushes(&mut events);
        assert_eq!(pushes.len(), 1, "one settle push: {pushes:?}");
        assert_eq!(pushes[0]["changed"].as_array().map(Vec::len), Some(1));
        assert!(pushes[0]["removed"].is_null() || pushes[0]["removed"].as_array().is_none());
        let row = roster_row_for_child(&supervisor, "sub-9");
        assert_eq!(row.worker_id, None, "the row is no longer worker-owned");
        assert!(row.summary.get("activeSessionId").is_none());
        assert!(row.summary.get("workerState").is_none());
        assert!(row.summary.get("workerPid").is_none());
        assert_eq!(row.summary["activity"], "idle");
        assert_eq!(row.summary["isStreaming"], false);
        assert_eq!(row.status, AgentRosterStatus::Inactive);
        // The durable display rows survive the stop.
        assert_eq!(row.summary["cwd"], "/the/live/cwd");
        assert_eq!(row.summary["model"]["provider"], "live");
        assert_eq!(row.summary["thinkingLevel"], "low");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Rows without an anchor are removed, never passivated: a
    /// tombstoned ledger edge (the user deleted the subagent), a live
    /// edge with no resident root, a top-level row, a queued child, and
    /// an ephemeral worker's rows all die with the stop.
    #[tokio::test]
    async fn stop_removes_unanchored_rows() {
        let (dir, supervisor, root_file, child_file) = roster_fixture().await;
        let agent_dir = dir.join("agent");
        let sessions_dir = agent_dir.join("sessions");
        let mut events = supervisor.events.subscribe();

        // A tombstoned child: the edge is deleted, so no live edge
        // carries the row even though the files exist.
        append_family_edge(&agent_dir, &sessions_dir, "sub-9", &root_file, &child_file);
        let ledger = crate::rlm_ledger::RlmSpawnLedger::new(&agent_dir, &sessions_dir, |_| {});
        ledger
            .append_delete("sub-9", &child_file.to_string_lossy(), crate::rlm_ledger::RlmLedgerDeleteReason::User)
            .expect("append delete");
        register_root_worker(&supervisor, "w-root", &root_file).await;
        supervisor.write_roster_summary(&live_child_summary(&root_file, &child_file), Some("w-child"));
        let _ = drain_roster_pushes(&mut events);
        supervisor.passivate_roster_worker("w-child", false).await;
        let pushes = drain_roster_pushes(&mut events);
        let removed: Vec<String> = pushes[0]["removed"].as_array().cloned().unwrap_or_default()
            .iter().filter_map(|id| id.as_str().map(str::to_string)).collect();
        assert_eq!(removed.len(), 1, "the tombstoned row dies: {pushes:?}");
        assert!(supervisor.roster.lock().unwrap().get(&removed[0]).is_none());

        // A live edge but no resident root: no surviving root, no row.
        let orphan_file = sessions_dir.join("sub-orphan.jsonl");
        write_display_file(&orphan_file, "/the/orphan/cwd");
        append_family_edge(&agent_dir, &sessions_dir, "sub-orphan", &root_file, &orphan_file);
        let mut orphan_summary = live_child_summary(&root_file, &orphan_file);
        orphan_summary["rlmChildId"] = json!("sub-orphan");
        supervisor.write_roster_summary(&orphan_summary, Some("w-orphan"));
        // Drop every resident worker: nothing anchors the family.
        for worker in supervisor.registry.list().await {
            supervisor.registry.remove(&worker.worker_id).await;
        }
        let _ = drain_roster_pushes(&mut events);
        supervisor.passivate_roster_worker("w-orphan", false).await;
        let pushes = drain_roster_pushes(&mut events);
        assert!(
            pushes[0]["removed"].as_array().is_some_and(|ids| !ids.is_empty()),
            "an unanchored row dies: {pushes:?}"
        );

        // A top-level row is removed, exactly like the remove+reseed it
        // replaces (the reseed never resurrected roots).
        let mut top_summary = live_child_summary(&root_file, &child_file);
        top_summary["runtimeKind"] = json!("top-level");
        top_summary["sessionId"] = json!("root-persisted");
        top_summary["id"] = json!("root-persisted");
        top_summary["sessionFile"] = json!(root_file.to_string_lossy());
        top_summary.as_object_mut().unwrap().remove("rlmChildId");
        top_summary.as_object_mut().unwrap().remove("parentSessionPath");
        supervisor.write_roster_summary(&top_summary, Some("w-top"));
        let _ = drain_roster_pushes(&mut events);
        supervisor.passivate_roster_worker("w-top", false).await;
        let pushes = drain_roster_pushes(&mut events);
        assert!(
            pushes[0]["removed"].as_array().is_some_and(|ids| ids.len() == 1),
            "the top-level row dies: {pushes:?}"
        );

        // A queued child and an ephemeral worker's rows die with the stop.
        let mut queued_summary = live_child_summary(&root_file, &child_file);
        queued_summary["rlmChildId"] = json!("sub-queued");
        queued_summary["queuedChild"] = json!(true);
        let queued_child_file = sessions_dir.join("sub-queued.jsonl");
        write_display_file(&queued_child_file, "/the/queued/cwd");
        append_family_edge(&agent_dir, &sessions_dir, "sub-queued", &root_file, &queued_child_file);
        queued_summary["sessionFile"] = json!(queued_child_file.to_string_lossy());
        supervisor.write_roster_summary(&queued_summary, Some("w-queued"));
        let _ = drain_roster_pushes(&mut events);
        supervisor.passivate_roster_worker("w-queued", true).await;
        let pushes = drain_roster_pushes(&mut events);
        assert!(
            pushes[0]["removed"].as_array().is_some_and(|ids| ids.len() == 1),
            "the queued/ephemeral row dies: {pushes:?}"
        );
        assert!(
            supervisor.roster.lock().unwrap().entries().iter()
                .all(|entry| entry.summary.get("rlmChildId").and_then(Value::as_str) != Some("sub-queued"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TS `passivatedWorkerRosterEntry` keeps the registration marks that
    // were true and strips the live-runtime fields, including the
    // heartbeat's own active flag.
    #[test]
    fn passivation_keeps_registration_marks_and_strips_live_fields() {
        let passivated = passivated_summary(json!({
            "sessionId": "persisted-id",
            "activeSessionId": "a-child",
            "activity": "working",
            "isSessionActive": true,
            "isStreaming": true,
            "isCompacting": true,
            "attachedClients": 2,
            "directAttachedClients": 2,
            "hasActiveHeartbeat": true,
            "hasRegisteredHeartbeat": true,
            "hasRegisteredCronJob": false,
            "hasRunningRlmChildren": true,
            "isBashRunning": true,
            "isRunningTools": true,
            "workerState": "ready",
            "workerPid": 4242,
            "cwd": "/the/live/cwd",
            "model": { "provider": "live", "modelId": "lm" },
            "thinkingLevel": "low",
        }));
        assert_eq!(passivated["id"], "persisted-id");
        assert_eq!(passivated["activity"], "idle");
        assert_eq!(passivated["isSessionActive"], false);
        assert_eq!(passivated["isStreaming"], false);
        assert_eq!(passivated["isCompacting"], false);
        assert_eq!(passivated["attachedClients"], 0);
        for key in [
            "activeSessionId",
            "directAttachedClients",
            "hasActiveHeartbeat",
            "hasRegisteredCronJob",
            "hasRunningRlmChildren",
            "isBashRunning",
            "isRunningTools",
            "workerState",
            "workerPid",
        ] {
            assert!(passivated.get(key).is_none(), "{key} is live-only");
        }
        assert_eq!(passivated["hasRegisteredHeartbeat"], true, "the mark survives");
        assert_eq!(passivated["cwd"], "/the/live/cwd");
        assert_eq!(passivated["model"], json!({ "provider": "live", "modelId": "lm" }));
        assert_eq!(passivated["thinkingLevel"], "low");
    }

    /// Live roster parity: a re-registration over the same session file
    /// replaces the passivated row with the live one, so a resumed
    /// session never renders its stale passive row.
    #[tokio::test]
    async fn a_reregistration_replaces_the_passive_row() {
        let (dir, supervisor, root_file, child_file) = roster_fixture().await;
        register_root_worker(&supervisor, "w-root", &root_file).await;
        let agent_dir = dir.join("agent");
        append_family_edge(&agent_dir, &agent_dir.join("sessions"), "sub-9", &root_file, &child_file);
        let mut events = supervisor.events.subscribe();
        supervisor.write_roster_summary(&live_child_summary(&root_file, &child_file), Some("w-child"));
        let _ = drain_roster_pushes(&mut events);
        supervisor.passivate_roster_worker("w-child", false).await;
        let _ = drain_roster_pushes(&mut events);

        let mut resumed = live_child_summary(&root_file, &child_file);
        resumed["model"] = json!({ "provider": "resumed", "modelId": "rm" });
        resumed["activity"] = json!("idle");
        resumed["isStreaming"] = json!(false);
        resumed["isSessionActive"] = json!(false);
        supervisor.write_roster_summary(&resumed, Some("w-resumed"));
        let row = roster_row_for_child(&supervisor, "sub-9");
        assert_eq!(row.worker_id.as_deref(), Some("w-resumed"));
        assert_eq!(row.summary["model"]["provider"], "resumed");
        assert_eq!(row.status, AgentRosterStatus::Idle);
        assert_eq!(
            supervisor.roster.lock().unwrap().entries().len(),
            1,
            "the passive row was replaced, not duplicated"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
