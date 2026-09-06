//! [`UplinkSink`] — the bounded outbox between the event loop and the
//! coordinator socket (blueprint §7.3, §12).
//!
//! > *daemon enters degraded-autonomous mode after 20 s silence (keeps local
//! > dataflow running, **buffers events**), reconnects with backoff.*
//!
//! Every [`astrs_wire::DaemonEvent`] the daemon produces — heartbeats, spawn
//! results, node exits, metric batches, log records, tapped frames — is handed
//! to a [`crate::health::ReportSink`] and forgotten. When a coordinator is
//! attached, this sink is that trait object, and the uplink's writer task
//! drains it at whatever pace the socket allows. When the coordinator is
//! *gone*, the same sink keeps accepting: the daemon does not stop running a
//! robot because a control-plane process died.
//!
//! ```text
//!   event loop ──report()──►  [ ring, capacity N ]  ──pop()──► writer task
//!                                    │
//!                            over capacity: shed
//!                            the oldest *shedable*
//!                            event, never a lifecycle fact
//! ```
//!
//! # Why a ring rather than an unbounded channel
//!
//! An unbounded queue turns a coordinator outage into an out-of-memory kill on
//! the machine that is still flying the robot — the exact failure §12's
//! degraded-autonomous mode exists to avoid. A ring bounds it, and the
//! eviction order is what keeps the bound honest:
//!
//! | Class | Events | Shed when full? |
//! |---|---|---|
//! | Liveness | `Heartbeat` | yes — only the newest matters |
//! | Telemetry | `NodeMetrics`, `Log`, `TopicTapData` | yes — samples, not facts |
//! | Lifecycle | `Register`, `BuildResult`, `SpawnResult`, `AllNodesReady`, `AllNodesFinished`, `NodeStopped`, `StateCatchUpAck`, `Exit` | no |
//!
//! A lifecycle event is the only record that a node ever ran, built or died;
//! losing one leaves the coordinator's dataflow FSM permanently wrong. A
//! heartbeat that is one minute stale is worth nothing next to the one behind
//! it. So the ring sheds the second kind first, and only evicts a lifecycle
//! fact when the buffer is *entirely* lifecycle facts — at which point the
//! oldest goes and [`UplinkSink::dropped`] says so.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::coordinator::UplinkSink;
//! use astrs_daemon::health::ReportSink;
//! use astrs_wire::{DaemonEvent, DaemonStats, DurationMs, WireMessage};
//!
//! let sink = UplinkSink::new(2);
//! for seq in 0..4 {
//!     sink.report(DaemonEvent::Heartbeat {
//!         seq,
//!         sent_at: Default::default(),
//!         stats: DaemonStats { uptime: DurationMs::new(seq), ..Default::default() },
//!     });
//! }
//!
//! assert_eq!(sink.len(), 2, "the ring is bounded");
//! assert_eq!(sink.dropped(), 2, "and it says what it shed");
//! assert_eq!(sink.pop().map(|event| event.variant_name()), Some("Heartbeat"));
//! ```

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use astrs_wire::DaemonEvent as WireDaemonEvent;
use tokio::sync::Notify;

use crate::health::ReportSink;

/// The daemon's outbox to its coordinator.
#[derive(Debug)]
pub struct UplinkSink {
    /// The ring itself, oldest first.
    queue: Mutex<VecDeque<WireDaemonEvent>>,
    /// How many events it holds before it starts shedding.
    capacity: usize,
    /// Wakes the writer task when something arrives.
    notify: Notify,
    /// How many events were shed, ever.
    dropped: AtomicU64,
    /// Whether the uplink task is still alive.
    open: AtomicBool,
    /// Whether a connection is up right now (§12: the degraded flag).
    connected: AtomicBool,
    /// How many events have been handed to a socket, ever.
    forwarded: AtomicU64,
}

impl UplinkSink {
    /// An empty outbox holding at most `capacity` events.
    ///
    /// A `capacity` of zero is raised to one: a ring that cannot hold the
    /// event being written would drop everything, which is a configuration
    /// mistake this refuses to honour rather than a policy.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            capacity: capacity.max(1),
            notify: Notify::new(),
            dropped: AtomicU64::new(0),
            open: AtomicBool::new(true),
            connected: AtomicBool::new(false),
            forwarded: AtomicU64::new(0),
        }
    }

    /// How many events are waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.locked().map_or(0, |queue| queue.len())
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The ring's capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many events have been written to a socket.
    #[must_use]
    pub fn forwarded(&self) -> u64 {
        self.forwarded.load(Ordering::Relaxed)
    }

    /// Whether a coordinator connection is up right now.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Records whether a coordinator connection is up.
    pub fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
        if connected {
            // A reconnect must re-examine a buffer that filled while the
            // link was down, even though nothing new arrived to notify on.
            self.notify.notify_one();
        }
    }

    /// Takes the oldest waiting event, if there is one.
    #[must_use]
    pub fn pop(&self) -> Option<WireDaemonEvent> {
        self.locked()?.pop_front()
    }

    /// Puts an event back at the front — the writer's undo for a frame the
    /// socket refused, so a lifecycle fact survives the reconnect that
    /// follows.
    pub fn requeue(&self, event: WireDaemonEvent) {
        let Some(mut queue) = self.locked() else {
            return;
        };
        if queue.len() >= self.capacity {
            // Making room at the *back* rather than dropping the event being
            // put back: this one is older than everything already queued, so
            // discarding it instead would reorder the daemon's own history.
            queue.pop_back();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        queue.push_front(event);
    }

    /// Records that one event reached the socket.
    pub fn record_forwarded(&self) {
        self.forwarded.fetch_add(1, Ordering::Relaxed);
    }

    /// Waits until something is worth looking at.
    ///
    /// Returns immediately when the ring is already non-empty, so a writer
    /// that pops until empty and then waits can never miss a `report` that
    /// landed in between.
    pub async fn wait(&self) {
        if !self.is_empty() {
            return;
        }
        self.notify.notified().await;
    }

    /// Marks the uplink gone: further reports are counted as dropped.
    pub fn close(&self) {
        self.open.store(false, Ordering::Relaxed);
        self.connected.store(false, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    /// Whether an event of this kind may be shed under pressure.
    ///
    /// See the module table: liveness and telemetry may go, lifecycle facts
    /// may not. Exhaustive over the classes rather than a name list so a
    /// future variant has to be classified rather than silently shed.
    #[must_use]
    pub const fn is_shedable(event: &WireDaemonEvent) -> bool {
        matches!(
            event,
            WireDaemonEvent::Heartbeat { .. }
                | WireDaemonEvent::NodeMetrics { .. }
                | WireDaemonEvent::Log { .. }
                | WireDaemonEvent::TopicTapData { .. }
        )
    }

    /// The ring, or [`None`] when a caller panicked while holding it.
    ///
    /// A poisoned outbox must not turn into a second panic on the path that
    /// reports a *first* failure upward, so every caller degrades: a report is
    /// counted as dropped, a pop returns nothing.
    fn locked(&self) -> Option<std::sync::MutexGuard<'_, VecDeque<WireDaemonEvent>>> {
        self.queue.lock().ok()
    }
}

impl ReportSink for UplinkSink {
    fn report(&self, event: WireDaemonEvent) {
        if !self.open.load(Ordering::Relaxed) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Some(mut queue) = self.locked() else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if queue.len() >= self.capacity {
            let victim = queue.iter().position(Self::is_shedable).unwrap_or_default();
            queue.remove(victim);
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        queue.push_back(event);
        drop(queue);
        self.notify.notify_one();
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::time::Duration;

    use astrs_wire::{
        DaemonStats, DataflowId, DurationMs, NodeExitCause, NodeId, SpawnOutcome, WireMessage,
    };

    use super::*;

    fn heartbeat(seq: u64) -> WireDaemonEvent {
        WireDaemonEvent::Heartbeat {
            seq,
            sent_at: Default::default(),
            stats: DaemonStats {
                uptime: DurationMs::new(seq),
                ..Default::default()
            },
        }
    }

    fn stopped(node: &str) -> WireDaemonEvent {
        WireDaemonEvent::NodeStopped {
            dataflow: DataflowId::from_u128(1),
            node: NodeId::new(node).unwrap(),
            generation: 0,
            cause: NodeExitCause::Success,
            restarting: false,
        }
    }

    fn spawned(node: &str) -> WireDaemonEvent {
        WireDaemonEvent::SpawnResult {
            dataflow: DataflowId::from_u128(1),
            node: NodeId::new(node).unwrap(),
            generation: 1,
            outcome: SpawnOutcome::Spawned {
                pid: Some(7),
                started_at: Default::default(),
            },
        }
    }

    #[test]
    fn an_empty_sink_is_open_and_disconnected() {
        let sink = UplinkSink::new(4);
        assert!(sink.is_empty());
        assert!(sink.is_open());
        assert!(!sink.is_connected());
        assert_eq!(sink.capacity(), 4);
        assert_eq!(sink.dropped(), 0);
        assert!(sink.pop().is_none());
    }

    #[test]
    fn a_zero_capacity_still_holds_one_event() {
        let sink = UplinkSink::new(0);
        assert_eq!(sink.capacity(), 1);
        sink.report(heartbeat(1));
        assert_eq!(sink.len(), 1);
    }

    #[test]
    fn events_leave_in_the_order_they_arrived() {
        let sink = UplinkSink::new(8);
        sink.report(spawned("a"));
        sink.report(heartbeat(1));
        sink.report(stopped("a"));

        assert_eq!(sink.pop().map(|e| e.variant_name()), Some("SpawnResult"));
        assert_eq!(sink.pop().map(|e| e.variant_name()), Some("Heartbeat"));
        assert_eq!(sink.pop().map(|e| e.variant_name()), Some("NodeStopped"));
        assert!(sink.is_empty());
    }

    #[test]
    fn pressure_sheds_liveness_before_lifecycle() {
        let sink = UplinkSink::new(3);
        sink.report(heartbeat(1));
        sink.report(spawned("a"));
        sink.report(heartbeat(2));
        // Over capacity: the *oldest shedable* event goes, which is
        // heartbeat 1 — not the spawn result in front of it.
        sink.report(stopped("a"));

        let held: Vec<&'static str> = std::iter::from_fn(|| sink.pop())
            .map(|event| event.variant_name())
            .collect();
        assert_eq!(held, ["SpawnResult", "Heartbeat", "NodeStopped"]);
        assert_eq!(sink.dropped(), 1);
    }

    #[test]
    fn an_all_lifecycle_buffer_evicts_the_oldest_and_says_so() {
        let sink = UplinkSink::new(2);
        sink.report(spawned("a"));
        sink.report(spawned("b"));
        sink.report(spawned("c"));

        assert_eq!(sink.len(), 2);
        assert_eq!(sink.dropped(), 1);
        match sink.pop().expect("held") {
            WireDaemonEvent::SpawnResult { node, .. } => assert_eq!(node.as_str(), "b"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_closed_sink_counts_reports_instead_of_holding_them() {
        let sink = UplinkSink::new(4);
        sink.close();
        assert!(!sink.is_open());
        sink.report(stopped("a"));
        assert!(sink.is_empty());
        assert_eq!(sink.dropped(), 1);
    }

    #[test]
    fn a_requeued_event_goes_back_to_the_front() {
        let sink = UplinkSink::new(4);
        sink.report(spawned("a"));
        sink.report(heartbeat(1));
        let first = sink.pop().expect("held");
        sink.requeue(first);
        assert_eq!(sink.pop().map(|e| e.variant_name()), Some("SpawnResult"));
    }

    #[test]
    fn requeueing_into_a_full_ring_drops_the_newest_not_the_replay() {
        let sink = UplinkSink::new(2);
        sink.report(spawned("a"));
        sink.report(spawned("b"));
        sink.requeue(spawned("z"));

        assert_eq!(sink.len(), 2);
        match sink.pop().expect("held") {
            WireDaemonEvent::SpawnResult { node, .. } => assert_eq!(node.as_str(), "z"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn the_shedable_classification_covers_the_family() {
        assert!(UplinkSink::is_shedable(&heartbeat(1)));
        assert!(!UplinkSink::is_shedable(&stopped("a")));
        assert!(!UplinkSink::is_shedable(&spawned("a")));
    }

    #[tokio::test]
    async fn a_waiter_returns_at_once_when_something_is_queued() {
        let sink = UplinkSink::new(4);
        sink.report(heartbeat(1));
        tokio::time::timeout(Duration::from_secs(1), sink.wait())
            .await
            .expect("wait must not block on a non-empty ring");
    }

    #[tokio::test]
    async fn a_waiter_is_woken_by_a_report() {
        let sink = std::sync::Arc::new(UplinkSink::new(4));
        let waiter = std::sync::Arc::clone(&sink);
        let task = tokio::spawn(async move { waiter.wait().await });
        // Deadline-polled: give the task a chance to park before reporting,
        // so this exercises the wake path rather than the fast path.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !task.is_finished() && std::time::Instant::now() < deadline {
            sink.report(heartbeat(1));
            tokio::task::yield_now().await;
        }
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("the waiter woke")
            .expect("the task did not panic");
    }

    #[tokio::test]
    async fn closing_wakes_a_parked_waiter() {
        let sink = std::sync::Arc::new(UplinkSink::new(4));
        let waiter = std::sync::Arc::clone(&sink);
        let task = tokio::spawn(async move { waiter.wait().await });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !task.is_finished() && std::time::Instant::now() < deadline {
            sink.close();
            tokio::task::yield_now().await;
        }
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("the waiter woke")
            .expect("the task did not panic");
    }

    #[test]
    fn connection_state_and_counters_are_reported() {
        let sink = UplinkSink::new(4);
        sink.set_connected(true);
        assert!(sink.is_connected());
        sink.record_forwarded();
        assert_eq!(sink.forwarded(), 1);
        sink.set_connected(false);
        assert!(!sink.is_connected());
    }
}
