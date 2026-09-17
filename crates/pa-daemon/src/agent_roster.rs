//! The supervisor's agent roster (port of `agent-roster.ts`'s `AgentRoster`
//! store): per-agent entries keyed by roster agent id, with active-session
//! and canonical-session-file indexes so lookups converge across worker
//! restarts and re-registrations. Classification uses the shared formula in
//! `pa_types::daemon::agent_roster`; this store owns persistence of the
//! classification and its indexes, and the supervisor wires mutations to
//! `roster_update` pushes.

use std::collections::HashMap;
use std::path::Path;

use pa_types::daemon::agent_roster::{
    classify_summary_value, roster_agent_id_for_summary, slim_roster_summary, AgentRosterEntry,
};
use serde_json::Value;

/// The supervisor-owned roster. Write() classifies once and its file index
/// converges seed and worker keys.
pub(crate) struct AgentRoster {
    entries: HashMap<String, AgentRosterEntry>,
    agent_id_by_active_session_id: HashMap<String, String>,
    agent_id_by_session_file: HashMap<String, String>,
}

impl AgentRoster {
    pub(crate) fn new() -> Self {
        AgentRoster {
            entries: HashMap::new(),
            agent_id_by_active_session_id: HashMap::new(),
            agent_id_by_session_file: HashMap::new(),
        }
    }

    /// Classify and store one entry from a worker's slim summary. Returns
    /// the stored entry. A session file that changed agent ownership evicts
    /// the previous owner (TS `write` index convergence).
    pub(crate) fn write_summary(
        &mut self,
        summary: Value,
        worker_id: Option<&str>,
        status_label: Option<&str>,
    ) -> AgentRosterEntry {
        let queued_child = summary
            .get("queuedChild")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let agent_id = roster_agent_id_for_summary(&summary);
        let stored = AgentRosterEntry {
            agent_id: agent_id.clone(),
            queued_child: queued_child.then_some(true),
            seeded_cwd: None,
            status: classify_summary_value(&summary, queued_child),
            status_label: if queued_child {
                Some("queued".to_string())
            } else {
                status_label.map(str::to_string)
            },
            last_heard_from_at: None,
            worker_id: worker_id.map(str::to_string),
            summary: slim_roster_summary(summary),
            rest: Default::default(),
        };
        if let Some(previous) = self.entries.get(&agent_id).cloned() {
            self.drop_indexes(&previous);
        }
        if let Some(file) = stored
            .summary
            .get("sessionFile")
            .and_then(Value::as_str)
            .filter(|file| !file.is_empty())
        {
            let file = canonical_roster_path(file);
            if let Some(existing) = self.agent_id_by_session_file.get(&file) {
                if existing != &agent_id {
                    let existing = existing.clone();
                    self.delete(&existing);
                }
            }
            self.agent_id_by_session_file.insert(file, agent_id.clone());
        }
        if let Some(active) = stored
            .summary
            .get("activeSessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            self.agent_id_by_active_session_id
                .insert(active.to_string(), agent_id.clone());
        }
        self.entries.insert(agent_id, stored.clone());
        stored
    }

    pub(crate) fn delete(&mut self, agent_id: &str) {
        let Some(entry) = self.entries.remove(agent_id) else {
            return;
        };
        self.drop_indexes(&entry);
    }

    pub(crate) fn get(&self, agent_id: &str) -> Option<&AgentRosterEntry> {
        self.entries.get(agent_id)
    }

    pub(crate) fn entries_for_worker(&self, worker_id: &str) -> Vec<&AgentRosterEntry> {
        self.entries
            .values()
            .filter(|entry| entry.worker_id.as_deref() == Some(worker_id))
            .collect()
    }

    /// All entries in stable (insertion) order for pushes and snapshots.
    pub(crate) fn entries(&self) -> Vec<AgentRosterEntry> {
        self.entries.values().cloned().collect()
    }

    fn drop_indexes(&mut self, entry: &AgentRosterEntry) {
        if let Some(active) = entry
            .summary
            .get("activeSessionId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            if self
                .agent_id_by_active_session_id
                .get(active)
                .is_some_and(|agent_id| agent_id == &entry.agent_id)
            {
                self.agent_id_by_active_session_id.remove(active);
            }
        }
        if let Some(file) = entry
            .summary
            .get("sessionFile")
            .and_then(Value::as_str)
            .filter(|file| !file.is_empty())
        {
            let file = canonical_roster_path(file);
            if self
                .agent_id_by_session_file
                .get(&file)
                .is_some_and(|agent_id| agent_id == &entry.agent_id)
            {
                self.agent_id_by_session_file.remove(&file);
            }
        }
    }
}

/// The canonical key for a session file (TS canonicalizes paths before
/// indexing): lexically normalized, falling back to the raw path when the
/// file does not exist yet.
fn canonical_roster_path(path: &str) -> String {
    Path::new(path)
        .canonicalize()
        .map(|canonical| canonical.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_types::daemon::agent_roster::AgentRosterStatus;
    use serde_json::json;
    use std::sync::Mutex;

    /// The store is not Sync-friendly in tests through `&mut`; the
    /// supervisor holds it behind a lock, so tests do too.
    fn locked() -> Mutex<AgentRoster> {
        Mutex::new(AgentRoster::new())
    }

    fn summary(agent: &str, active: Option<&str>, file: Option<&str>) -> Value {
        json!({
            "sessionId": agent,
            "activeSessionId": active,
            "sessionFile": file,
            "activity": "idle",
        })
    }

    #[test]
    fn write_classifies_and_indexes() {
        let roster = locked();
        let mut roster = roster.lock().unwrap();
        let entry = roster.write_summary(
            summary("s1", Some("a1"), Some("/tmp/s1.jsonl")),
            Some("w1"),
            None,
        );
        assert_eq!(entry.status, AgentRosterStatus::Idle);
        assert_eq!(entry.agent_id, "s1");
        // A working activity reclassifies to running.
        let mut working = summary("s1", Some("a1"), Some("/tmp/s1.jsonl"));
        working["activity"] = json!("working");
        let entry = roster.write_summary(working, Some("w1"), None);
        assert_eq!(entry.status, AgentRosterStatus::Running);
    }

    #[test]
    fn session_file_ownership_converges() {
        let roster = locked();
        let mut roster = roster.lock().unwrap();
        roster.write_summary(
            summary("s1", Some("a1"), Some("/tmp/shared.jsonl")),
            Some("w1"),
            None,
        );
        // A new agent claiming the same session file evicts the old owner.
        roster.write_summary(
            summary("s2", Some("a2"), Some("/tmp/shared.jsonl")),
            Some("w2"),
            None,
        );
        assert!(roster.get("s1").is_none());
        assert!(roster.get("s2").is_some());
        assert_eq!(roster.entries().len(), 1);
    }

    #[test]
    fn delete_is_idempotent() {
        let roster = locked();
        let mut roster = roster.lock().unwrap();
        roster.write_summary(summary("s1", Some("a1"), None), Some("w1"), None);
        roster.delete("s1");
        assert!(roster.get("s1").is_none());
        // Deleting an absent id is a no-op.
        roster.delete("s1");
    }

    #[test]
    fn queued_children_classify_running_with_label() {
        let roster = locked();
        let mut roster = roster.lock().unwrap();
        let mut queued = summary("child1", None, None);
        queued["queuedChild"] = json!(true);
        let entry = roster.write_summary(queued, Some("w1"), None);
        assert_eq!(entry.status, AgentRosterStatus::Running);
        assert_eq!(entry.status_label.as_deref(), Some("queued"));
    }

    #[test]
    fn worker_entries_filter_by_worker() {
        let roster = locked();
        let mut roster = roster.lock().unwrap();
        roster.write_summary(summary("s1", Some("a1"), None), Some("w1"), None);
        roster.write_summary(summary("s2", Some("a2"), None), Some("w2"), None);
        let w1 = roster.entries_for_worker("w1");
        assert_eq!(w1.len(), 1);
        assert_eq!(w1[0].agent_id, "s1");
    }
}
