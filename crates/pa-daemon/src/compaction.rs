//! The worker's compaction runs.
//!
//! Port of the TS daemon-mode compaction surface: the `compact` /
//! `abort_compaction` handlers, the `compaction_start`/`compaction_end`
//! `session_event` frames with their exact TS shapes, the `isCompacting`
//! state flag, and the durable compaction entry the worker appends to the
//! session store. The summarizer call itself is one
//! [`SessionEngine::run_compaction`]; this module owns everything around it
//! (abort registry, events, store persistence, state flags).

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::engine::{CompactionOutcome, CompactionRequest, SessionEngine};
use crate::protocol::DaemonOutbound;
use crate::worker::{EventPump, OutboundFrame, SessionCore};
use pa_agent::abort::AbortController;

/// The worker's compaction machinery: the live-run abort slot plus the
/// compaction flow. One slot per session, replaced by each new run, mirroring
/// the TS `_compactionAbortController`.
pub(crate) struct CompactionManager {
    engine: Arc<dyn SessionEngine>,
    events: Arc<EventPump>,
    core: Arc<Mutex<SessionCore>>,
    active_session_id: String,
    agent_dir: std::path::PathBuf,
    abort: Mutex<Option<Arc<AbortController>>>,
}

impl CompactionManager {
    pub(crate) fn new(
        engine: Arc<dyn SessionEngine>,
        events: Arc<EventPump>,
        core: Arc<Mutex<SessionCore>>,
        active_session_id: String,
        agent_dir: std::path::PathBuf,
    ) -> Self {
        CompactionManager {
            engine,
            events,
            core,
            active_session_id,
            agent_dir,
            abort: Mutex::new(None),
        }
    }

    /// `abort_compaction` (TS `abortCompaction`): abort the live compaction.
    /// Succeeds whether or not a run is in flight; the TS handler always
    /// replies success.
    pub(crate) fn abort(&self) {
        let controller = self.abort.lock().unwrap().clone();
        if let Some(controller) = controller {
            controller.abort();
        }
    }

    /// Run one compaction (`compact` command): emits the TS event pair,
    /// keeps `isCompacting` set for the run, and appends the durable
    /// compaction entry on success. The caller translates the outcome into
    /// the command response.
    pub(crate) async fn run(
        &self,
        custom_instructions: Option<String>,
        idle_notify: &tokio::sync::Notify,
    ) -> CompactionOutcome {
        // A compaction interrupts the running turn first (TS `compact()`
        // aborts the agent before summarizing): request the abort and wait
        // for the turn to settle.
        self.wait_for_turn_end(idle_notify).await;

        let controller = Arc::new(AbortController::new());
        let signal = controller.signal();
        {
            // Each run replaces the live slot, mirroring the TS
            // `_compactionAbortController` assignment; aborts hit the newest
            // run, and only its own run clears the slot.
            *self.abort.lock().unwrap() = Some(Arc::clone(&controller));
        }
        {
            let mut core = self.core.lock().unwrap();
            core.compacting = true;
        }
        let start = compaction_start_event(custom_instructions.as_deref());
        let _ = self.emit_session_event(start);

        let engine = Arc::clone(&self.engine);
        let request = CompactionRequest {
            custom_instructions: custom_instructions.clone(),
        };
        let run_signal = signal.clone();
        let outcome = {
            let engine = Arc::clone(&engine);
            tokio::task::spawn_blocking(move || engine.run_compaction(request, &run_signal))
                .await
                .unwrap_or_else(|join_error| CompactionOutcome::Failed {
                    error: format!("compaction run failed: {join_error}"),
                })
        };

        {
            let mut core = self.core.lock().unwrap();
            core.compacting = false;
        }
        if let CompactionOutcome::Compacted { run } = &outcome {
            self.persist_compaction(run, custom_instructions.as_deref());
        }
        let end = compaction_end_event(&outcome, custom_instructions.as_deref());
        let _ = self.emit_session_event(end);
        {
            let mut slot = self.abort.lock().unwrap();
            if slot
                .as_ref()
                .is_some_and(|live| Arc::ptr_eq(live, &controller))
            {
                *slot = None;
            }
        }
        outcome
    }

    /// `set_auto_compaction`: update the connection-state flag.
    pub(crate) fn set_auto_compaction(&self, enabled: bool) {
        let mut core = self.core.lock().unwrap();
        core.auto_compaction_enabled = enabled;
    }

    /// Wait until the running turn (if any) has settled.
    async fn wait_for_turn_end(&self, idle_notify: &tokio::sync::Notify) {
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
            // The turn runner notifies when the queue drains; the timeout is
            // a backstop so a missed notification cannot hang a compact.
            let _ =
                tokio::time::timeout(std::time::Duration::from_millis(50), idle_notify.notified())
                    .await;
        }
    }

    /// Append the durable compaction entry to the worker's session store.
    /// An empty `firstKeptEntryId` (the scripted default) keeps from the
    /// first branch entry, so the compacted read retains the transcript.
    fn persist_compaction(
        &self,
        run: &crate::engine::CompactionRun,
        custom_instructions: Option<&str>,
    ) {
        let result = &run.result;
        let first_kept_entry_id = result
            .get("firstKeptEntryId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut core = self.core.lock().unwrap();
        let cwd = core.cwd.clone();
        let Some(store) = core.store.as_mut() else {
            return;
        };
        let first_kept_entry_id = if first_kept_entry_id.is_empty() {
            store
                .branch()
                .iter()
                .find(|entry| entry.type_ == "message")
                .map(|entry| entry.id.clone())
                .unwrap_or_default()
        } else {
            // The engine's id references its in-memory entry list, a
            // separate id space from the session file: re-pin the boundary
            // to the durable cut so the file read retains the kept tail
            // (TS: one store, ids match by construction). An unreadable
            // durable cut keeps the engine id rather than dropping the
            // boundary entirely.
            let keep_recent = pa_core::settings::SettingsManager::create(&cwd, &self.agent_dir)
                .settings()
                .compaction
                .clone()
                .unwrap_or_default()
                .keep_recent_tokens
                .unwrap_or(pa_core::session_engine::compaction::DEFAULT_KEEP_RECENT_TOKENS);
            store
                .durable_first_kept_entry_id(keep_recent)
                .unwrap_or(first_kept_entry_id)
        };
        let mut fields = json!({
            "summary": result.get("summary").cloned().unwrap_or_default(),
            "firstKeptEntryId": first_kept_entry_id,
            "tokensBefore": result.get("tokensBefore").cloned().unwrap_or(json!(0)),
            "details": result.get("details").cloned().unwrap_or_else(|| json!({
                "readFiles": [], "modifiedFiles": [],
            })),
            "fromHook": false,
        });
        if let Some(custom_instructions) = custom_instructions {
            fields["customInstructions"] = json!(custom_instructions);
        }
        if let Some(usage) = &run.usage {
            fields["usage"] = usage.clone();
        }
        let _ = store.persist_entry("compaction", fields);
    }

    /// Sequence and broadcast one compaction `session_event` frame.
    fn emit_session_event(&self, event: Value) -> serde_json::Result<()> {
        let mut core = self.core.lock().unwrap();
        let sequence = core.last_event_sequence + 1;
        core.last_event_sequence = sequence;
        let meta = crate::protocol::create_daemon_event_meta(
            &self.active_session_id,
            sequence,
            None,
            Some(&core.generation),
        );
        let outbound = DaemonOutbound::SessionEvent {
            active_session_id: self.active_session_id.clone(),
            event,
            meta: Some(meta),
            rest: Default::default(),
        };
        let payload = serde_json::to_vec(&outbound)?;
        drop(core);
        self.events.send(OutboundFrame::session_event(payload));
        Ok(())
    }
}

/// The `compaction_start` event payload (TS `AgentSessionEvent`). Shared by
/// every compaction surface: the `compact` RPC and the `/compact` session
/// command both emit the same shape.
pub(crate) fn compaction_start_event(custom_instructions: Option<&str>) -> Value {
    let mut event = json!({ "type": "compaction_start", "reason": "manual" });
    if let Some(custom_instructions) = custom_instructions {
        event["customInstructions"] = json!(custom_instructions);
    }
    event
}

/// The `compaction_end` event payload (TS `AgentSessionEvent`) from the
/// outcome fields: success carries `result`; a skip or failure carries its
/// `errorMessage` with the matching severity; an abort carries `aborted`.
/// `reason` is the TS `CompactionOutcomeReason` (`manual` for user-initiated
/// runs, `requested` for model-requested boundary compactions).
pub(crate) fn compaction_end_payload(
    reason: &str,
    result: Option<&Value>,
    aborted: bool,
    error_message: Option<&str>,
    error_severity: Option<&str>,
    custom_instructions: Option<&str>,
) -> Value {
    let mut event = json!({ "type": "compaction_end", "reason": reason });
    if let Some(result) = result {
        event["result"] = result.clone();
    }
    event["aborted"] = json!(aborted);
    if let Some(error_message) = error_message {
        event["errorMessage"] = json!(error_message);
    }
    if let Some(error_severity) = error_severity {
        event["errorSeverity"] = json!(error_severity);
    }
    event["willRetry"] = json!(false);
    if let Some(custom_instructions) = custom_instructions {
        event["customInstructions"] = json!(custom_instructions);
    }
    event
}

/// The `compaction_end` event payload (TS `AgentSessionEvent`), per outcome:
/// success carries `result`; a skip carries `errorMessage` with warning
/// severity; a failure carries `Compaction failed: <message>` with error
/// severity; an abort carries `aborted` with error severity and no message.
fn compaction_end_event(outcome: &CompactionOutcome, custom_instructions: Option<&str>) -> Value {
    match outcome {
        CompactionOutcome::Compacted { run } => compaction_end_payload(
            "manual",
            Some(&run.result),
            false,
            None,
            None,
            custom_instructions,
        ),
        CompactionOutcome::Skipped { message } => compaction_end_payload(
            "manual",
            None,
            false,
            Some(message),
            Some("warning"),
            custom_instructions,
        ),
        CompactionOutcome::Failed { error } => compaction_end_payload(
            "manual",
            None,
            false,
            Some(&format!("Compaction failed: {error}")),
            Some("error"),
            custom_instructions,
        ),
        CompactionOutcome::Aborted => compaction_end_payload(
            "manual",
            None,
            true,
            None,
            Some("error"),
            custom_instructions,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_shapes_match_ts() {
        let run = crate::engine::CompactionRun {
            result: json!({
                "summary": "the story so far",
                "firstKeptEntryId": "abcd1234",
                "tokensBefore": 1234,
                "details": { "readFiles": ["a.rs"], "modifiedFiles": [] },
            }),
            usage: None,
        };
        assert_eq!(
            compaction_start_event(Some("focus on the goal")),
            json!({
                "type": "compaction_start",
                "reason": "manual",
                "customInstructions": "focus on the goal",
            })
        );
        assert_eq!(
            compaction_start_event(None),
            json!({ "type": "compaction_start", "reason": "manual" })
        );
        assert_eq!(
            compaction_end_event(&CompactionOutcome::Compacted { run }, Some("focus")),
            json!({
                "type": "compaction_end",
                "reason": "manual",
                "result": {
                    "summary": "the story so far",
                    "firstKeptEntryId": "abcd1234",
                    "tokensBefore": 1234,
                    "details": { "readFiles": ["a.rs"], "modifiedFiles": [] },
                },
                "aborted": false,
                "willRetry": false,
                "customInstructions": "focus",
            })
        );
        assert_eq!(
            compaction_end_event(
                &CompactionOutcome::Skipped {
                    message: "Session is too short to compact — try again once it grows"
                        .to_string(),
                },
                None,
            ),
            json!({
                "type": "compaction_end",
                "reason": "manual",
                "aborted": false,
                "willRetry": false,
                "errorMessage": "Session is too short to compact — try again once it grows",
                "errorSeverity": "warning",
            })
        );
        assert_eq!(
            compaction_end_event(&CompactionOutcome::Aborted, None),
            json!({
                "type": "compaction_end",
                "reason": "manual",
                "aborted": true,
                "willRetry": false,
                "errorSeverity": "error",
            })
        );
        assert_eq!(
            compaction_end_event(
                &CompactionOutcome::Failed {
                    error: "Summarization failed".to_string(),
                },
                None,
            ),
            json!({
                "type": "compaction_end",
                "reason": "manual",
                "aborted": false,
                "willRetry": false,
                "errorMessage": "Compaction failed: Summarization failed",
                "errorSeverity": "error",
            })
        );
    }
}
