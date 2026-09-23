//! Worker-side roster activity feed: the live half of the
//! waiting/executing indicator. Every activity row renders the
//! supervisor's roster (the TUI agents view's Activity column, the session
//! view's subagents box, the daemon CLI lists), so the worker must publish
//! its summary whenever the flags those rows render change mid-turn: busy
//! flips, tool calls starting and ending, compaction, user bash, queue
//! changes, and the post-turn status line. TS observes the worker's
//! outbound event stream (`observeRosterEvent` over
//! `ROSTER_SESSION_EVENT_TRIGGERS` + `scheduleRosterFlush`, daemon-mode.ts);
//! the port watches the worker's event pump — the one stream every
//! session-event frame flows through (turns, compaction, bash, queue
//! updates, worker-level notifications) — and feeds a coalescing push
//! queue whose single consumer composes the summary fresh at flush time
//! and ships one `worker_roster_delta` per burst.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;

use crate::engine::SessionEngine;
use crate::supervisor_link::SupervisorLink;
use crate::user_bash::UserBash;
use crate::worker::{session_summary, EventPump, OutboundFrame, SessionCore};

/// Disables the worker's roster pushes (tests and local harness runs that
/// spin workers without a supervisor).
pub(crate) const ROSTER_PUSH_DISABLE_ENV: &str = "PA_WORKER_DISABLE_ROSTER_PUSH";

/// The session event types that trigger a roster flush (TS
/// `ROSTER_SESSION_EVENT_TRIGGERS`, daemon-mode.ts): each edge moves a
/// summary field the activity rows render. `thinking_level_changed` has
/// no Rust frame yet; it stays listed so the feed wires the moment the
/// frame exists.
pub(crate) const ROSTER_SESSION_EVENT_TRIGGERS: &[&str] = &[
    "turn_start",
    "turn_end",
    "bash_start",
    "bash_end",
    "compaction_start",
    "compaction_end",
    "auto_retry_start",
    "auto_retry_end",
    "tool_execution_start",
    "tool_execution_end",
    "message_end",
    "session_action_update",
    "session_info_changed",
    "thinking_level_changed",
];

/// Whether one broadcast frame triggers a roster flush (TS
/// `observeRosterEvent`: session events by event type, plus the
/// `session_status` frame kind and the `session_closed`/`session_replaced`
/// payload tags, which the worker frames as session events).
pub(crate) fn frame_triggers_roster_flush(frame: &OutboundFrame) -> bool {
    if frame.outbound_type == "session_status" {
        return true;
    }
    if frame.outbound_type != "session_event" {
        return false;
    }
    #[derive(serde::Deserialize)]
    struct Envelope<'a> {
        #[serde(rename = "type", borrow)]
        kind: &'a str,
        #[serde(borrow, default)]
        event: Option<EventType<'a>>,
    }
    #[derive(serde::Deserialize)]
    struct EventType<'a> {
        #[serde(rename = "type", borrow)]
        kind: &'a str,
    }
    // The payload is parsed only for session-event frames, so a
    // non-trigger frame costs one discriminant check.
    match serde_json::from_slice::<Envelope>(&frame.payload) {
        Ok(envelope) => match envelope.kind {
            "session_closed" | "session_replaced" => true,
            _ => envelope
                .event
                .is_some_and(|event| ROSTER_SESSION_EVENT_TRIGGERS.contains(&event.kind)),
        },
        Err(_) => false,
    }
}

/// The coalescing roster push queue (TS `scheduleRosterFlush`): every
/// producer — the turn runner's busy flips and the pump watcher — enqueues
/// a flush request, and the single consumer drains the burst before
/// composing, so one `worker_roster_delta` ships the latest state no
/// matter how the requests interleaved. The summary composes fresh at
/// flush time, never at enqueue time: a late flush reads the worker's
/// current flags instead of replaying a stale snapshot.
#[derive(Clone)]
pub(crate) struct RosterPushQueue {
    tx: Option<UnboundedSender<()>>,
}

impl RosterPushQueue {
    /// The queue with no consumer: pushes are no-ops (no supervisor
    /// endpoint, or [`ROSTER_PUSH_DISABLE_ENV`]).
    pub(crate) fn disabled() -> Self {
        Self { tx: None }
    }

    /// Enqueue one flush request (the TS `scheduleRosterFlush` arm of
    /// every trigger).
    pub(crate) fn push(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(());
        }
    }

    /// Spawn the flush consumer: the one task that composes summaries and
    /// ships them as `worker_roster_delta` commands over the supervisor
    /// link. Requests serialize here, so a wedged supervisor delays pushes
    /// (each request carries its own deadline) but never reorders them.
    pub(crate) fn spawn(
        core: Arc<Mutex<SessionCore>>,
        engine: Arc<dyn SessionEngine>,
        user_bash: Arc<UserBash>,
        roster_link: Arc<SupervisorLink>,
        worker_token: String,
    ) -> Self {
        if std::env::var_os(ROSTER_PUSH_DISABLE_ENV).is_some()
            || worker_token.is_empty()
            || roster_link.socket_path().as_os_str().is_empty()
        {
            return Self::disabled();
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        tokio::spawn(async move {
            while rx.recv().await.is_some() {
                // One flush per burst (TS `setImmediate` coalescing): every
                // request drained here is answered by this one push.
                while rx.try_recv().is_ok() {}
                let summary = {
                    let core = core.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    session_summary(
                        &core,
                        &engine
                            .effective_thinking_level()
                            .unwrap_or_else(|| "default".to_string()),
                        engine.model_metadata(),
                        engine.model_fallback_message(),
                        user_bash.is_running(),
                    )
                };
                let summary = serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null);
                let _ = roster_link
                    .request(
                        json!({
                            "type": "worker_roster_delta",
                            "workerToken": worker_token,
                            "summary": summary,
                        }),
                        Duration::from_secs(10),
                    )
                    .await;
            }
        });
        Self { tx: Some(tx) }
    }
}

/// The pump watcher (TS `observeRosterEvent`): every trigger frame that
/// flows through the worker's event pump enqueues a flush request. The
/// watcher owns a receiver on the pump's broadcast, so it ends with the
/// worker's process (a worker serves one session for its lifetime).
pub(crate) fn spawn_roster_activity_watch(events: Arc<EventPump>, queue: RosterPushQueue) {
    if queue.tx.is_none() {
        return;
    }
    let mut frames = events.subscribe();
    tokio::spawn(async move {
        while let Ok(frame) = frames.recv().await {
            if frame_triggers_roster_flush(&frame) {
                queue.push();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_event_frame(event: serde_json::Value) -> OutboundFrame {
        let payload = json!({
            "type": "session_event",
            "activeSessionId": "session-1",
            "event": event,
        });
        OutboundFrame::session_event(serde_json::to_vec(&payload).unwrap())
    }

    #[test]
    fn every_ts_trigger_event_type_flushes() {
        for event_type in ROSTER_SESSION_EVENT_TRIGGERS {
            let frame = session_event_frame(json!({ "type": event_type }));
            assert!(
                frame_triggers_roster_flush(&frame),
                "{event_type} must trigger a roster flush"
            );
        }
    }

    #[test]
    fn non_trigger_events_and_frame_kinds_stay_silent() {
        for event_type in [
            "message_start",
            "message_update",
            "tool_execution_update",
            "agent_start",
            "agent_end",
            "goal_update",
            "ipython_sent_agent_message",
        ] {
            let frame = session_event_frame(json!({ "type": event_type }));
            assert!(
                !frame_triggers_roster_flush(&frame),
                "{event_type} must not trigger a roster flush"
            );
        }
        // The non-`session_event` frame kinds the pump carries: the status
        // line flushes (TS `session_status`), a side question does not.
        let status_payload =
            serde_json::to_vec(&json!({ "type": "session_status", "activeSessionId": "s" }))
                .unwrap();
        assert!(frame_triggers_roster_flush(&OutboundFrame::session_status(
            status_payload
        )));
        let side_question_payload = serde_json::to_vec(&json!({
            "type": "side_question_event",
            "activeSessionId": "s",
            "event": { "type": "message_end" },
        }))
        .unwrap();
        assert!(!frame_triggers_roster_flush(
            &OutboundFrame::side_question_event(side_question_payload)
        ));
        // The worker frames `session_closed` payloads as session events:
        // the payload tag flushes (TS `observeRosterEvent`'s
        // `message.type === "session_closed"` arm).
        let closed_payload = serde_json::to_vec(&json!({
            "type": "session_closed",
            "activeSessionId": "s",
            "reason": "done",
        }))
        .unwrap();
        assert!(frame_triggers_roster_flush(&OutboundFrame::session_event(
            closed_payload
        )));
        let replaced_payload = serde_json::to_vec(&json!({
            "type": "session_replaced",
            "activeSessionId": "s",
        }))
        .unwrap();
        assert!(frame_triggers_roster_flush(&OutboundFrame::session_event(
            replaced_payload
        )));
    }

    #[test]
    fn the_disabled_queue_never_pushes() {
        let queue = RosterPushQueue::disabled();
        queue.push();
    }
}
