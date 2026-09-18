//! Live token-stream coalescing on the worker broadcast path.
//!
//! With live turn-event forwarding (see `agent_engine`), one assistant
//! message streams as one `message_update` wire event per provider delta,
//! each carrying the full partial message. Providers emit tens to hundreds
//! of deltas per second, so the turn's emit path parks those frames in a
//! single-slot coalescer instead of broadcasting every one: a flusher task
//! emits at most one parked update per interval (the latest snapshot wins —
//! superseded frames are equivalent, never additive), while every other
//! frame (message_start, message_end, tool events, turn_end) flushes the
//! parked update first and then goes out immediately, so wire order and
//! event-sequence order stay identical to uncoalesced streaming.
//!
//! The supervisor stays payload-free: coalescing happens in the worker, on
//! the worker -> client session-event stream (direct-attach or
//! supervisor-routed), before any broadcast.

use std::sync::Mutex;
use std::time::Duration;

/// One parked update flushes per interval; anything parked longer is a
/// stream stall, so this bounds both broadcast rate and update staleness.
pub(crate) const UPDATE_FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// Single-slot coalescer for one turn's `message_update` frames. Shared
/// between the turn's emit path (park/direct-send) and the flusher task;
/// every broadcast happens under `inner`, so frame order is total.
pub(crate) struct TurnStreamCoalescer {
    inner: Mutex<CoalescerInner>,
}

struct CoalescerInner {
    /// The latest parked `message_update` payload (serialized session
    /// event, ready to broadcast). Replaced by newer snapshots.
    pending: Option<Vec<u8>>,
    /// The turn ended: park no more, flush nothing, stop the flusher.
    closed: bool,
}

impl TurnStreamCoalescer {
    pub(crate) fn new() -> Self {
        TurnStreamCoalescer {
            inner: Mutex::new(CoalescerInner {
                pending: None,
                closed: false,
            }),
        }
    }

    /// Park one `message_update` payload, replacing any parked snapshot.
    /// Returns `false` when the turn already ended (the caller drops the
    /// frame instead of broadcasting a stale streaming event).
    pub(crate) fn park_update(&self, payload: Vec<u8>) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return false;
        }
        inner.pending = Some(payload);
        true
    }

    /// Broadcast every `payloads` frame directly, after flushing any parked
    /// update first (the parked snapshot is ordered before the frames that
    /// supersede it). All sends happen under the coalescer lock, so the
    /// flusher can never interleave between the parked update and its
    /// settling frame.
    pub(crate) fn send_direct(&self, payloads: &[Vec<u8>], events: &crate::worker::EventPump) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(pending) = inner.pending.take() {
            events.send(crate::worker::OutboundFrame::session_event(pending));
        }
        for payload in payloads {
            events.send(crate::worker::OutboundFrame::session_event(payload.clone()));
        }
    }

    /// Flusher tick: broadcast the parked update when one is waiting.
    /// Returns `false` once the turn closed and the flusher should stop.
    pub(crate) fn flush_pending(&self, events: &crate::worker::EventPump) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return false;
        }
        if let Some(pending) = inner.pending.take() {
            events.send(crate::worker::OutboundFrame::session_event(pending));
        }
        true
    }

    /// End of turn: nothing parked after this point is broadcast (an aborted
    /// turn's stale partial must not appear after its settle events), and
    /// anything still parked is dropped.
    pub(crate) fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        inner.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_sends_flush_the_parked_update_first() {
        let pump = crate::worker::EventPump::new();
        let mut rx = pump.subscribe();
        let coalescer = TurnStreamCoalescer::new();
        assert!(coalescer.park_update(b"update-1".to_vec()));
        // The parked update waits for the flusher until a direct frame
        // arrives: the direct send must broadcast the parked snapshot first.
        coalescer.send_direct(&[b"message-end".to_vec()], &pump);
        assert_eq!(rx.try_recv().unwrap().payload, b"update-1".to_vec());
        assert_eq!(rx.try_recv().unwrap().payload, b"message-end".to_vec());
        assert!(rx.try_recv().is_err(), "no further frames");
    }

    #[test]
    fn a_newer_snapshot_replaces_the_parked_one() {
        let pump = crate::worker::EventPump::new();
        let mut rx = pump.subscribe();
        let coalescer = TurnStreamCoalescer::new();
        assert!(coalescer.park_update(b"update-1".to_vec()));
        assert!(coalescer.park_update(b"update-2".to_vec()));
        assert!(coalescer.flush_pending(&pump));
        assert_eq!(rx.try_recv().unwrap().payload, b"update-2".to_vec());
        assert!(rx.try_recv().is_err(), "the superseded snapshot is dropped");
    }

    #[test]
    fn close_stops_parking_and_flushing() {
        let pump = crate::worker::EventPump::new();
        let mut rx = pump.subscribe();
        let coalescer = TurnStreamCoalescer::new();
        assert!(coalescer.park_update(b"update-1".to_vec()));
        coalescer.close();
        assert!(!coalescer.park_update(b"update-2".to_vec()));
        assert!(!coalescer.flush_pending(&pump), "the flusher stops");
        assert!(rx.try_recv().is_err(), "a closed turn parks nothing");
    }

    #[test]
    fn flush_without_parked_frames_is_a_kept_alive_noop() {
        let pump = crate::worker::EventPump::new();
        let mut rx = pump.subscribe();
        let coalescer = TurnStreamCoalescer::new();
        assert!(coalescer.flush_pending(&pump));
        assert!(rx.try_recv().is_err());
    }
}
