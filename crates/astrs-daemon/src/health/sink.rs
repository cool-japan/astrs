//! [`ReportSink`] — where the daemon's upward traffic goes (§7.3).
//!
//! Everything the daemon tells the coordinator is an
//! [`astrs_wire::DaemonEvent`]: heartbeats with load figures, per-node metric
//! batches, spawn results, node exits, tapped topic frames. In a cluster those
//! frames go out over an `astrs-transport` connection; under `astrs run` there
//! is no coordinator at all; in a test the interesting thing is to *look* at
//! them.
//!
//! Rather than teach the event loop about all three, it writes to one trait
//! object. Wave 4 hands it a connection-backed sink and nothing above changes
//! — which is the point of introducing the seam now, while there is still one
//! implementation to get right.
//!
//! ```text
//!   heartbeat (5 s) ─┐
//!   node metrics (2 s)├─► ReportSink ─┬─► CoordinatorSink   (W4: a connection)
//!   topic taps ───────┤               ├─► RecordingSink     (tests, `astrs run`)
//!   node exits ───────┘               └─► NullSink          (no coordinator)
//! ```
//!
//! # Why `&self` and not `&mut self`
//!
//! A sink is shared: the event loop holds one, and so does whatever background
//! task drains it. Interior mutability inside the implementation is cheaper
//! and simpler than threading a `&mut` through the loop's own borrows, and it
//! keeps the trait object usable behind an [`std::sync::Arc`].
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::health::{RecordingSink, ReportSink};
//! use astrs_wire::{DaemonEvent, DataflowId, NodeExitCause, NodeId, WireMessage};
//!
//! let sink = RecordingSink::new();
//! sink.report(DaemonEvent::NodeStopped {
//!     dataflow: DataflowId::from_u128(1),
//!     node: NodeId::new("camera")?,
//!     generation: 0,
//!     cause: NodeExitCause::Success,
//!     restarting: false,
//! });
//!
//! assert_eq!(sink.len(), 1);
//! assert_eq!(sink.take()[0].variant_name(), "NodeStopped");
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use astrs_wire::{DaemonEvent, WireMessage};
use tokio::sync::mpsc;

/// How many events a [`RecordingSink`] keeps before discarding the oldest.
///
/// A recording sink exists for tests and for `astrs run`'s in-process
/// inspection, neither of which needs an unbounded log; a robot that ran for a
/// week would otherwise accumulate a heartbeat every five seconds forever.
pub const RECORDING_CAPACITY: usize = 4096;

/// Somewhere the daemon's upward events go.
pub trait ReportSink: fmt::Debug + Send + Sync {
    /// Accepts one event.
    ///
    /// Reporting must never block the event loop and must never fail it: a
    /// sink whose far end is gone drops the event and says so through its own
    /// counters, because there is nothing useful the loop could do with an
    /// error on a heartbeat.
    fn report(&self, event: DaemonEvent);

    /// How many events this sink has dropped.
    fn dropped(&self) -> u64 {
        0
    }

    /// Whether the sink still has somewhere to put events.
    ///
    /// A closed sink is not an error — the daemon keeps running in
    /// degraded-autonomous mode (§12) — but the loop can stop paying to build
    /// batches nobody will read.
    fn is_open(&self) -> bool {
        true
    }
}

/// A sink that discards everything, counting as it goes.
///
/// The `astrs run` configuration: there is no coordinator, and the events have
/// no audience.
#[derive(Debug, Default)]
pub struct NullSink {
    /// How many events were discarded.
    discarded: AtomicU64,
}

impl NullSink {
    /// A sink that has discarded nothing yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            discarded: AtomicU64::new(0),
        }
    }
}

impl ReportSink for NullSink {
    fn report(&self, _event: DaemonEvent) {
        self.discarded.fetch_add(1, Ordering::Relaxed);
    }

    fn dropped(&self) -> u64 {
        self.discarded.load(Ordering::Relaxed)
    }

    fn is_open(&self) -> bool {
        false
    }
}

/// A sink that forwards into an unbounded channel.
///
/// The shape wave 4's coordinator link takes: the loop hands events over
/// without awaiting, and a separate task writes them to the socket at whatever
/// pace the socket allows.
#[derive(Debug)]
pub struct ChannelSink {
    /// The channel into the forwarding task.
    sender: mpsc::UnboundedSender<DaemonEvent>,
    /// How many events arrived after the receiver was gone.
    dropped: AtomicU64,
}

impl ChannelSink {
    /// Builds a sink and the receiver a forwarding task drains.
    #[must_use]
    pub fn new() -> (Self, mpsc::UnboundedReceiver<DaemonEvent>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            Self {
                sender,
                dropped: AtomicU64::new(0),
            },
            receiver,
        )
    }
}

impl ReportSink for ChannelSink {
    fn report(&self, event: DaemonEvent) {
        if self.sender.send(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn is_open(&self) -> bool {
        !self.sender.is_closed()
    }
}

/// A sink that keeps the last [`RECORDING_CAPACITY`] events in memory.
///
/// What a test asserts against, and what `astrs status --local` reads.
#[derive(Debug, Default)]
pub struct RecordingSink {
    /// The retained events, oldest first.
    events: Mutex<Vec<DaemonEvent>>,
    /// How many were evicted by the capacity limit.
    evicted: AtomicU64,
}

impl RecordingSink {
    /// An empty recording sink.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            evicted: AtomicU64::new(0),
        }
    }

    /// How many events are held.
    ///
    /// Zero if the lock is poisoned, which can only happen if a caller
    /// panicked while holding it — a recording sink refuses to turn that into
    /// a second panic.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.lock().map(|events| events.len()).unwrap_or(0)
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Takes every held event, leaving the sink empty.
    #[must_use]
    pub fn take(&self) -> Vec<DaemonEvent> {
        match self.events.lock() {
            Ok(mut events) => std::mem::take(&mut *events),
            Err(_) => Vec::new(),
        }
    }

    /// A copy of every held event.
    #[must_use]
    pub fn snapshot(&self) -> Vec<DaemonEvent> {
        match self.events.lock() {
            Ok(events) => events.clone(),
            Err(_) => Vec::new(),
        }
    }

    /// How many events carry `variant_name`.
    #[must_use]
    pub fn count_of(&self, variant_name: &str) -> usize {
        match self.events.lock() {
            Ok(events) => events
                .iter()
                .filter(|event| event.variant_name() == variant_name)
                .count(),
            Err(_) => 0,
        }
    }

    /// Whether any held event carries `variant_name`.
    #[must_use]
    pub fn contains(&self, variant_name: &str) -> bool {
        self.count_of(variant_name) > 0
    }
}

impl ReportSink for RecordingSink {
    fn report(&self, event: DaemonEvent) {
        let Ok(mut events) = self.events.lock() else {
            self.evicted.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if events.len() >= RECORDING_CAPACITY {
            events.remove(0);
            self.evicted.fetch_add(1, Ordering::Relaxed);
        }
        events.push(event);
    }

    fn dropped(&self) -> u64 {
        self.evicted.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_time::HlcTimestamp;
    use astrs_wire::{DaemonStats, DataflowId, DurationMs, NodeExitCause, NodeId};

    use super::*;

    fn heartbeat(seq: u64) -> DaemonEvent {
        DaemonEvent::Heartbeat {
            seq,
            sent_at: HlcTimestamp::new(seq, 0),
            stats: DaemonStats {
                uptime: DurationMs::from_secs(seq),
                node_count: 1,
                dataflow_count: 1,
                cpu_percent: 0.0,
                rss_bytes: 0,
                shm_bytes_mapped: 0,
                shm_fallback_total: 0,
                frames_sent: 0,
                frames_received: 0,
                bytes_sent: 0,
                bytes_received: 0,
            },
        }
    }

    fn stopped() -> DaemonEvent {
        DaemonEvent::NodeStopped {
            dataflow: DataflowId::from_u128(1),
            node: NodeId::new("camera").unwrap(),
            generation: 0,
            cause: NodeExitCause::Success,
            restarting: false,
        }
    }

    #[test]
    fn the_null_sink_counts_what_it_discards() {
        let sink = NullSink::new();
        assert!(!sink.is_open());
        sink.report(heartbeat(1));
        sink.report(heartbeat(2));
        assert_eq!(sink.dropped(), 2);
    }

    #[tokio::test]
    async fn the_channel_sink_forwards_in_order() {
        let (sink, mut receiver) = ChannelSink::new();
        assert!(sink.is_open());
        sink.report(heartbeat(1));
        sink.report(heartbeat(2));

        for expected in 1..=2u64 {
            match receiver.recv().await.expect("sent") {
                DaemonEvent::Heartbeat { seq, .. } => assert_eq!(seq, expected),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(sink.dropped(), 0);
    }

    #[tokio::test]
    async fn a_channel_sink_with_no_receiver_counts_drops() {
        let (sink, receiver) = ChannelSink::new();
        drop(receiver);
        assert!(!sink.is_open());
        sink.report(heartbeat(1));
        assert_eq!(sink.dropped(), 1);
    }

    #[test]
    fn the_recording_sink_keeps_what_it_is_given() {
        let sink = RecordingSink::new();
        assert!(sink.is_empty());
        sink.report(heartbeat(1));
        sink.report(stopped());

        assert_eq!(sink.len(), 2);
        assert!(sink.contains("Heartbeat"));
        assert_eq!(sink.count_of("NodeStopped"), 1);
        assert_eq!(sink.snapshot().len(), 2);

        let taken = sink.take();
        assert_eq!(taken.len(), 2);
        assert!(sink.is_empty());
    }

    #[test]
    fn the_recording_sink_evicts_the_oldest_at_capacity() {
        let sink = RecordingSink::new();
        for seq in 0..(RECORDING_CAPACITY as u64 + 3) {
            sink.report(heartbeat(seq));
        }
        assert_eq!(sink.len(), RECORDING_CAPACITY);
        assert_eq!(sink.dropped(), 3);

        let held = sink.snapshot();
        match &held[0] {
            DaemonEvent::Heartbeat { seq, .. } => assert_eq!(*seq, 3, "the first three went"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_sink_is_usable_behind_a_trait_object() {
        let sinks: Vec<std::sync::Arc<dyn ReportSink>> = vec![
            std::sync::Arc::new(NullSink::new()),
            std::sync::Arc::new(RecordingSink::new()),
        ];
        for sink in &sinks {
            sink.report(heartbeat(1));
        }
        assert_eq!(sinks[0].dropped(), 1);
        assert_eq!(sinks[1].dropped(), 0);
    }

    #[test]
    fn a_default_recording_sink_is_empty() {
        let sink = RecordingSink::default();
        assert!(sink.is_empty());
        assert!(sink.is_open());
        assert!(!sink.contains("Heartbeat"));
    }
}
