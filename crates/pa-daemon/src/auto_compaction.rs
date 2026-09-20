//! The automatic threshold compaction at the daemon engine's turn
//! boundaries: the TS `_checkCompaction` threshold arm wired into the
//! turn loop (TS `agent-session.ts`).
//!
//! TS fires the check at two boundaries: after every settled turn
//! (`agent_end`) and before the next admitted prompt
//! (`_runPreTurnCompaction`, `beforeModelSelection` for queued prompts).
//! The check itself is the pa-core decision ([`AgentSession::
//! auto_compaction_due`]: the live context over the reserve headroom);
//! this module owns the daemon flow around it — the `compaction_start` /
//! `compaction_end` event pair with the `threshold` reason (TS
//! `_runAutoCompaction`), the worker's persist-and-broadcast contract
//! (the `Compaction` event carries the durable entry like `/compact`), and
//! the outcome shapes: the client-facing result on success, the TS skip /
//! failure messages with their severities otherwise.

use serde_json::{json, Value};

use crate::agent_engine::AgentSessionEngine;
use crate::engine::EngineEvent;
use pa_core::session_engine::compact_session::CompactOutcome;

/// The outcome of one turn-boundary threshold check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoCompactionRun {
    /// No threshold crossing (the common turn): nothing ran, nothing fired.
    NotDue,
    /// The check fired and a compaction ran (success, skip, or failure —
    /// the `compaction_start`/`compaction_end` pair went out either way).
    Ran,
    /// The emit callback cancelled the run (an aborted turn): the caller
    /// stops the turn loop like any other cancelled emit.
    Cancelled,
}

impl AgentSessionEngine {
    /// The TS `_checkCompaction` threshold arm at a turn boundary: check
    /// the live context against the reserve headroom and, when it crossed,
    /// run one compaction with the `threshold` event pair. The worker
    /// persists the durable entry and broadcasts both events exactly like
    /// the `/compact` flow (the TUI swaps in the `Auto-compacting...`
    /// loader for the start event and the durable `◆ Context compacted`
    /// row for the end).
    pub(crate) fn run_auto_compaction(
        &self,
        emit: &mut dyn FnMut(EngineEvent) -> bool,
    ) -> AutoCompactionRun {
        // TS reads `this.model?.contextWindow ?? 0`: a session without a
        // resolvable model never crosses a threshold.
        let model = match self.resolve_model() {
            Ok(model) => model,
            Err(_) => return AutoCompactionRun::NotDue,
        };
        let due = {
            let guard = self.session.blocking_lock();
            match guard.as_ref() {
                Some(engine) => self.runtime.block_on(async {
                    engine
                        .session
                        .auto_compaction_due(model.context_window)
                        .await
                }),
                // No built session: the live context is empty (nothing to
                // compact), matching the TS pre-turn check on a fresh
                // session whose first turn has not run yet.
                None => false,
            }
        };
        if !due {
            return AutoCompactionRun::NotDue;
        }
        // TS `_runAutoCompaction` emits the start event before the
        // summarizer runs, so attached surfaces see the loader.
        if !emit(EngineEvent::CompactionStart {
            event: crate::compaction::compaction_start_event("threshold", None),
        }) {
            return AutoCompactionRun::Cancelled;
        }
        let api_key = self.resolve_request_api_key(&model);
        let outcome = {
            let guard = self.session.blocking_lock();
            let Some(engine) = guard.as_ref() else {
                return AutoCompactionRun::NotDue;
            };
            self.runtime
                .block_on(async { engine.session.compact(None, &model, api_key).await })
        };
        let event = match &outcome {
            Ok(CompactOutcome::Ran(run)) => {
                // The wire result is the TS `CompactionResult` shape.
                let result = json!({
                    "summary": run.result.summary,
                    "firstKeptEntryId": run.result.first_kept_entry_id,
                    "tokensBefore": run.result.tokens_before,
                });
                crate::compaction::compaction_end_payload(
                    "threshold",
                    Some(&result),
                    false,
                    None,
                    None,
                    None,
                )
            }
            // A skip consumed the check (TS `CompactionSkippedError`): the
            // wire carries the warning so attached surfaces can show it.
            Ok(CompactOutcome::Skipped(message)) => crate::compaction::compaction_end_payload(
                "threshold",
                None,
                false,
                Some(&format!("Auto-compaction skipped: {message}")),
                Some("warning"),
                None,
            ),
            Err(error) => crate::compaction::compaction_end_payload(
                "threshold",
                None,
                false,
                Some(&format!("Auto-compaction failed: {error:#}")),
                Some("error"),
                None,
            ),
        };
        let entry = match &outcome {
            Ok(CompactOutcome::Ran(run)) => serde_json::to_value(&run.entry).unwrap_or(Value::Null),
            _ => Value::Null,
        };
        if !emit(EngineEvent::Compaction { entry, event }) {
            return AutoCompactionRun::Cancelled;
        }
        AutoCompactionRun::Ran
    }
}
