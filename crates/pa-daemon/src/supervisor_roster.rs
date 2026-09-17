//! Supervisor-side roster serving: subscribe/unsubscribe handling, worker
//! roster deltas, and the `roster_update` pushes subscribers receive (the
//! roster arms of TS `daemon-supervisor.ts`; the store itself lives in
//! `agent_roster.rs`).

use std::sync::Arc;

use pa_types::daemon::agent_roster::AgentRosterEntry;
use pa_types::daemon::DaemonOutbound;
use serde_json::{json, Value};

use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::registry::ResidentWorker;
use crate::supervisor::{ClientRouting, Supervisor, ROUTE_TIMEOUT_MS};

impl Supervisor {
    /// `roster_subscribe` (TS: sets the client flag and answers with the
    /// full roster snapshot; the caller stores the flag).
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

    /// Remove a stopped worker's entries and push the removals.
    pub(crate) fn remove_roster_worker(&self, worker_id: &str) {
        let removed: Vec<String> = {
            let mut roster = self.roster.lock().unwrap();
            let ids: Vec<String> = roster
                .entries_for_worker(worker_id)
                .iter()
                .map(|entry| entry.agent_id.clone())
                .collect();
            for id in &ids {
                roster.delete(id);
            }
            ids
        };
        self.push_roster_update(Vec::new(), removed);
    }

    /// Push one `roster_update` to subscribed clients. The TS supervisor
    /// batches pending mutations into one push; roster writes are low-rate
    /// here, so each mutation pushes immediately and subscribers apply
    /// entries idempotently by agent id.
    fn push_roster_update(&self, changed: Vec<AgentRosterEntry>, removed: Vec<String>) {
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
