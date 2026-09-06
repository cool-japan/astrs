//! The daemon → coordinator heartbeat (§12).
//!
//! > *Daemon↔coordinator: heartbeat 5 s; daemon enters degraded-autonomous
//! > mode after 20 s silence (keeps local dataflow running, buffers events),
//! > reconnects with backoff, resyncs via sequence-numbered `StateCatchUp`.*
//!
//! [`HeartbeatProducer`] owns the timing and the sequence number of that leg.
//! It does not own the socket — the daemon writes through a
//! [`crate::health::ReportSink`], and wave 4 puts a connection behind it — and
//! it does not own a task: like every other deadline in this crate it is
//! driven from the merged event loop, so a heartbeat cannot be emitted while
//! the loop is mid-way through a state transition it would misreport.
//!
//! # Degraded-autonomous mode
//!
//! Silence from the coordinator is not a reason to stop a robot. After
//! [`DEGRADED_AFTER`] without contact the producer reports
//! [`LinkHealth::Degraded`]: the daemon keeps its nodes running, keeps
//! stamping events, and keeps *producing* heartbeats — they simply have
//! nowhere to go until the link is back, at which point the coordinator
//! catches up from the sequence number the daemon kept counting.
//!
//! That is why the sequence number lives here rather than in the connection: a
//! reconnect must not restart it, or the coordinator cannot tell a fresh
//! daemon from one it lost contact with.
//!
//! # Examples
//!
//! ```
//! use std::time::{Duration, Instant};
//! use astrs_daemon::health::{HeartbeatProducer, LinkHealth};
//! use astrs_daemon::metrics::FtStats;
//! use astrs_time::HlcTimestamp;
//! use astrs_wire::{DaemonStats, DurationMs, WireMessage};
//!
//! let start = Instant::now();
//! let mut heartbeat = HeartbeatProducer::new(Duration::from_secs(5), start);
//! assert!(!heartbeat.due(start));
//!
//! let at = start + Duration::from_secs(5);
//! assert!(heartbeat.due(at));
//! let event = heartbeat.emit(
//!     DaemonStats { uptime: DurationMs::from_secs(5), ..Default::default() },
//!     FtStats::default(),
//!     HlcTimestamp::new(5, 0),
//!     at,
//! );
//! assert_eq!(event.variant_name(), "Heartbeat");
//! assert_eq!(heartbeat.seq(), 1);
//! assert_eq!(heartbeat.link_health(at), LinkHealth::Live);
//! ```

use std::time::{Duration, Instant};

use astrs_time::HlcTimestamp;
use astrs_wire::{DaemonEvent, DaemonStats, WireMessage};

use crate::metrics::FtStats;

/// How long the daemon may go without hearing from the coordinator before it
/// declares itself degraded-autonomous (§12).
pub const DEGRADED_AFTER: Duration = Duration::from_secs(20);

/// How the coordinator leg is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkHealth {
    /// The coordinator has been heard from recently.
    Live,
    /// Nothing for [`DEGRADED_AFTER`]: running autonomously (§12).
    Degraded,
}

impl LinkHealth {
    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Degraded => "degraded",
        }
    }

    /// Whether the daemon is operating without a coordinator.
    #[must_use]
    pub const fn is_degraded(self) -> bool {
        matches!(self, Self::Degraded)
    }
}

/// Times and numbers the daemon's heartbeats (§12).
#[derive(Debug, Clone)]
pub struct HeartbeatProducer {
    /// How often a heartbeat is due.
    interval: Duration,
    /// When the last one was emitted.
    last_emitted: Instant,
    /// When the coordinator was last heard from.
    last_contact: Instant,
    /// How many have been emitted.
    seq: u64,
    /// When the daemon started, for the uptime figure.
    started_at: Instant,
}

impl HeartbeatProducer {
    /// A producer beating every `interval`, with its clocks starting at `now`.
    #[must_use]
    pub const fn new(interval: Duration, now: Instant) -> Self {
        Self {
            interval,
            last_emitted: now,
            last_contact: now,
            seq: 0,
            started_at: now,
        }
    }

    /// The beat interval.
    #[must_use]
    pub const fn interval(&self) -> Duration {
        self.interval
    }

    /// How many heartbeats have been emitted.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// When the next heartbeat comes due.
    #[must_use]
    pub fn next_deadline(&self) -> Instant {
        self.last_emitted
            .checked_add(self.interval)
            .unwrap_or(self.last_emitted)
    }

    /// Whether a heartbeat is due at `now`.
    #[must_use]
    pub fn due(&self, now: Instant) -> bool {
        !self.interval.is_zero() && now >= self.next_deadline()
    }

    /// How long the daemon has been up at `now`.
    #[must_use]
    pub fn uptime(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started_at)
    }

    /// Records that the coordinator was heard from.
    pub const fn note_contact(&mut self, now: Instant) {
        self.last_contact = now;
    }

    /// How the coordinator leg is doing at `now` (§12).
    #[must_use]
    pub fn link_health(&self, now: Instant) -> LinkHealth {
        if now.saturating_duration_since(self.last_contact) >= DEGRADED_AFTER {
            LinkHealth::Degraded
        } else {
            LinkHealth::Live
        }
    }

    /// How long since the coordinator was last heard from.
    #[must_use]
    pub fn silence(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_contact)
    }

    /// Builds the next heartbeat, advancing the sequence and the cadence.
    ///
    /// `ft` is folded into the event's log-facing summary rather than into
    /// [`DaemonStats`], which has no room for it: the coordinator reads the
    /// counters it needs from the stats, and an operator reads the rest from
    /// [`FtStats::summary`] in the daemon's own log.
    pub fn emit(
        &mut self,
        mut stats: DaemonStats,
        ft: FtStats,
        timestamp: HlcTimestamp,
        now: Instant,
    ) -> DaemonEvent {
        self.last_emitted = now;
        self.seq = self.seq.saturating_add(1);
        stats.shm_fallback_total = ft.shm_fallbacks;
        DaemonEvent::Heartbeat {
            seq: self.seq,
            sent_at: timestamp,
            stats,
        }
    }

    /// The most recent heartbeat's variant name, for a caller asserting shape
    /// without constructing one.
    #[must_use]
    pub fn event_name() -> &'static str {
        DaemonEvent::VARIANT_NAMES[1]
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_wire::DurationMs;

    use super::*;

    fn stats(uptime: Duration) -> DaemonStats {
        DaemonStats {
            uptime: DurationMs::from_duration(uptime),
            node_count: 2,
            dataflow_count: 1,
            cpu_percent: 1.0,
            rss_bytes: 1024,
            shm_bytes_mapped: 0,
            shm_fallback_total: 0,
            frames_sent: 0,
            frames_received: 0,
            bytes_sent: 0,
            bytes_received: 0,
        }
    }

    #[test]
    fn a_heartbeat_is_due_only_after_the_interval() {
        let start = Instant::now();
        let producer = HeartbeatProducer::new(Duration::from_secs(5), start);
        assert!(!producer.due(start));
        assert!(!producer.due(start + Duration::from_millis(4999)));
        assert!(producer.due(start + Duration::from_secs(5)));
        assert_eq!(producer.interval(), Duration::from_secs(5));
    }

    #[test]
    fn a_zero_interval_never_beats() {
        let start = Instant::now();
        let producer = HeartbeatProducer::new(Duration::ZERO, start);
        assert!(!producer.due(start + Duration::from_secs(600)));
    }

    #[test]
    fn the_sequence_increases_by_one_per_beat() {
        let start = Instant::now();
        let mut producer = HeartbeatProducer::new(Duration::from_secs(5), start);
        for expected in 1..=3u64 {
            let at = start + Duration::from_secs(5 * expected);
            assert!(producer.due(at));
            let event = producer.emit(
                stats(Duration::from_secs(5 * expected)),
                FtStats::default(),
                HlcTimestamp::new(expected, 0),
                at,
            );
            match event {
                DaemonEvent::Heartbeat { seq, .. } => assert_eq!(seq, expected),
                other => panic!("unexpected {other:?}"),
            }
            assert_eq!(producer.seq(), expected);
        }
    }

    #[test]
    fn the_fallback_counter_comes_from_the_ft_summary() {
        let start = Instant::now();
        let mut producer = HeartbeatProducer::new(Duration::from_secs(5), start);
        let ft = FtStats {
            shm_fallbacks: 9,
            ..FtStats::default()
        };
        match producer.emit(
            stats(Duration::from_secs(5)),
            ft,
            HlcTimestamp::new(1, 0),
            start + Duration::from_secs(5),
        ) {
            DaemonEvent::Heartbeat { stats, .. } => {
                assert_eq!(stats.shm_fallback_total, 9);
                assert!(stats.has_shm_fallbacks());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn emitting_resets_the_cadence() {
        let start = Instant::now();
        let mut producer = HeartbeatProducer::new(Duration::from_secs(5), start);
        let at = start + Duration::from_secs(7);
        producer.emit(
            stats(Duration::from_secs(7)),
            FtStats::default(),
            HlcTimestamp::new(1, 0),
            at,
        );
        assert!(!producer.due(at + Duration::from_secs(4)));
        assert!(producer.due(at + Duration::from_secs(5)));
    }

    #[test]
    fn silence_degrades_the_link_after_twenty_seconds() {
        let start = Instant::now();
        let mut producer = HeartbeatProducer::new(Duration::from_secs(5), start);
        assert_eq!(producer.link_health(start), LinkHealth::Live);
        assert_eq!(
            producer.link_health(start + Duration::from_secs(19)),
            LinkHealth::Live
        );

        let degraded = start + DEGRADED_AFTER;
        assert_eq!(producer.link_health(degraded), LinkHealth::Degraded);
        assert!(producer.link_health(degraded).is_degraded());
        assert_eq!(producer.silence(degraded), DEGRADED_AFTER);

        producer.note_contact(degraded);
        assert_eq!(producer.link_health(degraded), LinkHealth::Live);
    }

    #[test]
    fn the_sequence_survives_a_reconnect() {
        let start = Instant::now();
        let mut producer = HeartbeatProducer::new(Duration::from_secs(5), start);
        producer.emit(
            stats(Duration::from_secs(5)),
            FtStats::default(),
            HlcTimestamp::new(1, 0),
            start + Duration::from_secs(5),
        );
        // A reconnect is a contact, not a reset.
        producer.note_contact(start + Duration::from_secs(30));
        assert_eq!(producer.seq(), 1);
    }

    #[test]
    fn uptime_grows_with_the_clock() {
        let start = Instant::now();
        let producer = HeartbeatProducer::new(Duration::from_secs(5), start);
        assert_eq!(producer.uptime(start), Duration::ZERO);
        assert_eq!(
            producer.uptime(start + Duration::from_secs(42)),
            Duration::from_secs(42)
        );
    }

    #[test]
    fn link_health_labels_are_distinct() {
        assert_ne!(LinkHealth::Live.as_str(), LinkHealth::Degraded.as_str());
        assert!(!LinkHealth::Live.is_degraded());
    }

    #[test]
    fn the_event_name_matches_the_wire_family() {
        assert_eq!(HeartbeatProducer::event_name(), "Heartbeat");
    }
}
