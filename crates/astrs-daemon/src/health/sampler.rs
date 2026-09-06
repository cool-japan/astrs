//! Per-node CPU, memory and queue sampling (§13).
//!
//! > *Daemon samples per-node CPU/RSS/queue-depth/SHM-stats every 2 s →
//! > coordinator → TUI + `astrs top`.*
//!
//! [`NodeMetricsCollector`] is that sampler. It is deliberately *not* a
//! background task: the queue depths it needs live inside the event loop's
//! mailboxes, and reading them from another thread would mean a lock on the
//! hottest structure the daemon owns. Instead the loop calls
//! [`NodeMetricsCollector::due`] on every tick and, when the interval has
//! elapsed, hands the sampler the pids and queue snapshots it already has in
//! hand.
//!
//! # Sampling fidelity
//!
//! [`astrs_telemetry::sampler::CpuMemSampler`] answers with a
//! [`astrs_telemetry::sampler::SampleFidelity`] describing what the host
//! actually allowed it to measure: a real per-pid reading on Linux, a `ps`
//! fallback or a lifetime-peak RSS on macOS. The collector records the
//! fidelity it saw so an operator reading a suspicious graph can tell a real
//! plateau from a measurement artefact, and so a test asserts *presence*
//! rather than a value the platform cannot promise.
//!
//! # A pid that has already gone
//!
//! Sampling races the reaper by construction: a node can exit between the loop
//! listing its pid and the sampler reading `/proc`. A failed reading is
//! skipped for that round rather than reported as a zero, because a zero is a
//! measurement and "the process is gone" is not.
//!
//! # Examples
//!
//! ```
//! use std::time::{Duration, Instant};
//! use astrs_daemon::health::{NodeMetricsCollector, NodeSampleRequest};
//! use astrs_time::HlcTimestamp;
//! use astrs_wire::{DataflowId, NodeId};
//!
//! let start = Instant::now();
//! let mut collector = NodeMetricsCollector::new(Duration::from_secs(2), start);
//! assert!(!collector.due(start));
//! assert!(collector.due(start + Duration::from_secs(2)));
//!
//! let request = NodeSampleRequest::new(NodeId::new("camera")?, std::process::id());
//! let samples = collector.sample(
//!     DataflowId::from_u128(1),
//!     &[request],
//!     HlcTimestamp::new(1, 0),
//!     start + Duration::from_secs(2),
//! );
//! assert_eq!(samples.len(), 1);
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use astrs_telemetry::sampler::{CpuMemSampler, SampleFidelity};
use astrs_time::HlcTimestamp;
use astrs_wire::{DataId, DataflowId, NodeId, NodeMetricsSample};

/// What the loop knows about one node when a sampling round comes due.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSampleRequest {
    /// The node.
    pub node: NodeId,
    /// Its process id, for the CPU/RSS reading.
    pub pid: u32,
    /// Current depth per input, from the node's mailbox (§11.2).
    pub queue_depths: BTreeMap<DataId, u32>,
    /// Messages delivered per input since the node registered.
    pub received_total: BTreeMap<DataId, u64>,
    /// Messages a full queue discarded per input.
    pub dropped_total: BTreeMap<DataId, u64>,
    /// Messages published per output.
    pub sent_total: BTreeMap<DataId, u64>,
    /// Shared-memory slots the node's rings currently hold in use (§6.2).
    pub shm_slots_in_use: u32,
    /// This node's current 99th-percentile `astrs/timer/*` jitter, in
    /// microseconds, from the shared timer wheel (§11.1). `0` for a node
    /// with no timer subscription, or one whose wheel entry has not yet
    /// delivered a tick to estimate from.
    pub timer_jitter_p99_us: u64,
}

impl NodeSampleRequest {
    /// A request naming only the node and its pid.
    #[must_use]
    pub fn new(node: NodeId, pid: u32) -> Self {
        Self {
            node,
            pid,
            queue_depths: BTreeMap::new(),
            received_total: BTreeMap::new(),
            dropped_total: BTreeMap::new(),
            sent_total: BTreeMap::new(),
            shm_slots_in_use: 0,
            timer_jitter_p99_us: 0,
        }
    }

    /// Records one input's queue figures.
    #[must_use]
    pub fn with_queue(mut self, input: DataId, depth: u32, received: u64, dropped: u64) -> Self {
        self.queue_depths.insert(input.clone(), depth);
        self.received_total.insert(input.clone(), received);
        self.dropped_total.insert(input, dropped);
        self
    }

    /// Records one output's publish count.
    #[must_use]
    pub fn with_sent(mut self, output: DataId, sent: u64) -> Self {
        self.sent_total.insert(output, sent);
        self
    }

    /// Records how many shared-memory slots the node holds.
    #[must_use]
    pub const fn with_shm_slots(mut self, slots: u32) -> Self {
        self.shm_slots_in_use = slots;
        self
    }

    /// Records the node's current timer-jitter p99 reading, in microseconds
    /// (§11.1).
    #[must_use]
    pub const fn with_timer_jitter_p99_us(mut self, micros: u64) -> Self {
        self.timer_jitter_p99_us = micros;
        self
    }

    /// The deepest queue this request carries, for a quick backlog check.
    #[must_use]
    pub fn deepest_queue(&self) -> u32 {
        self.queue_depths.values().copied().max().unwrap_or(0)
    }
}

/// Samples node processes on a fixed cadence (§13).
#[derive(Debug)]
pub struct NodeMetricsCollector {
    /// The underlying OS sampler, stateful for CPU deltas.
    sampler: CpuMemSampler,
    /// How often a round is due.
    interval: Duration,
    /// When the last round ran.
    last_round: Instant,
    /// How many rounds have run.
    rounds: u64,
    /// How many pid readings failed.
    failures: u64,
    /// The fidelity of the most recent successful reading.
    fidelity: Option<SampleFidelity>,
}

impl NodeMetricsCollector {
    /// A collector that samples every `interval`, starting the clock at `now`.
    #[must_use]
    pub fn new(interval: Duration, now: Instant) -> Self {
        Self {
            sampler: CpuMemSampler::new(),
            interval,
            last_round: now,
            rounds: 0,
            failures: 0,
            fidelity: None,
        }
    }

    /// The sampling cadence.
    #[must_use]
    pub const fn interval(&self) -> Duration {
        self.interval
    }

    /// When the next round comes due.
    #[must_use]
    pub fn next_deadline(&self) -> Instant {
        self.last_round
            .checked_add(self.interval)
            .unwrap_or(self.last_round)
    }

    /// Whether a round is due at `now`.
    #[must_use]
    pub fn due(&self, now: Instant) -> bool {
        !self.interval.is_zero() && now >= self.next_deadline()
    }

    /// How many rounds have run.
    #[must_use]
    pub const fn rounds(&self) -> u64 {
        self.rounds
    }

    /// How many individual pid readings failed.
    #[must_use]
    pub const fn failures(&self) -> u64 {
        self.failures
    }

    /// The fidelity of the most recent successful reading, if there was one.
    #[must_use]
    pub const fn fidelity(&self) -> Option<SampleFidelity> {
        self.fidelity
    }

    /// Restarts the cadence from `now` without sampling anything.
    ///
    /// The call a caller makes at the end of a due round that found nothing to
    /// sample. Without it the round stays due forever and the deadline the
    /// event loop sleeps on is permanently in the past — the difference
    /// between an idle daemon and a spinning one.
    pub const fn arm_next(&mut self, now: Instant) {
        self.last_round = now;
    }

    /// Runs one sampling round.
    ///
    /// Advances the cadence clock whether or not any pid could be read, so a
    /// dataflow whose nodes have all exited does not spin trying.
    pub fn sample(
        &mut self,
        _dataflow: DataflowId,
        requests: &[NodeSampleRequest],
        timestamp: HlcTimestamp,
        now: Instant,
    ) -> Vec<NodeMetricsSample> {
        self.last_round = now;
        self.rounds = self.rounds.saturating_add(1);

        let pids: Vec<u32> = requests.iter().map(|request| request.pid).collect();
        let readings = self.sampler.sample_many(&pids);

        let mut samples = Vec::with_capacity(requests.len());
        for request in requests {
            let mut sample = NodeMetricsSample::new(request.node.clone(), timestamp);
            match readings.get(&request.pid) {
                Some(Ok(reading)) => {
                    sample.cpu_percent = reading.cpu_percent;
                    sample.rss_bytes = reading.rss_bytes;
                    self.fidelity = Some(reading.fidelity);
                }
                _ => {
                    // The pid is gone, or this platform cannot read it. Report
                    // the queue figures — which the daemon measured itself and
                    // therefore knows — and leave the process gauges at zero
                    // rather than inventing them.
                    self.failures = self.failures.saturating_add(1);
                }
            }
            sample.queue_depths = request.queue_depths.clone();
            sample.received_total = request.received_total.clone();
            sample.dropped_total = request.dropped_total.clone();
            sample.sent_total = request.sent_total.clone();
            sample.shm_slots_in_use = request.shm_slots_in_use;
            sample.timer_jitter_p99_us = request.timer_jitter_p99_us;
            samples.push(sample);
        }
        samples
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn node(name: &str) -> NodeId {
        NodeId::new(name).unwrap()
    }

    fn data(name: &str) -> DataId {
        DataId::new(name).unwrap()
    }

    fn dataflow() -> DataflowId {
        DataflowId::from_u128(3)
    }

    #[test]
    fn a_round_is_due_only_after_the_interval() {
        let start = Instant::now();
        let collector = NodeMetricsCollector::new(Duration::from_secs(2), start);
        assert!(!collector.due(start));
        assert!(!collector.due(start + Duration::from_millis(1999)));
        assert!(collector.due(start + Duration::from_secs(2)));
        assert_eq!(collector.next_deadline(), start + Duration::from_secs(2));
        assert_eq!(collector.interval(), Duration::from_secs(2));
    }

    #[test]
    fn a_zero_interval_never_comes_due() {
        let start = Instant::now();
        let collector = NodeMetricsCollector::new(Duration::ZERO, start);
        assert!(!collector.due(start + Duration::from_secs(60)));
    }

    #[test]
    fn sampling_advances_the_cadence() {
        let start = Instant::now();
        let mut collector = NodeMetricsCollector::new(Duration::from_secs(2), start);
        let at = start + Duration::from_secs(2);
        collector.sample(dataflow(), &[], HlcTimestamp::new(1, 0), at);

        assert_eq!(collector.rounds(), 1);
        assert!(!collector.due(at));
        assert!(collector.due(at + Duration::from_secs(2)));
    }

    #[test]
    fn queue_figures_come_from_the_request_not_the_os() {
        let start = Instant::now();
        let mut collector = NodeMetricsCollector::new(Duration::from_secs(2), start);
        let request = NodeSampleRequest::new(node("detect"), std::process::id())
            .with_queue(data("frames"), 3, 42, 1)
            .with_sent(data("boxes"), 40)
            .with_shm_slots(2)
            .with_timer_jitter_p99_us(150);
        assert_eq!(request.deepest_queue(), 3);

        let samples = collector.sample(dataflow(), &[request], HlcTimestamp::new(9, 0), start);
        assert_eq!(samples.len(), 1);
        let sample = &samples[0];
        assert_eq!(sample.node, node("detect"));
        assert_eq!(sample.timestamp, HlcTimestamp::new(9, 0));
        assert_eq!(sample.queue_depths.get(&data("frames")), Some(&3));
        assert_eq!(sample.received_total.get(&data("frames")), Some(&42));
        assert_eq!(sample.dropped_total.get(&data("frames")), Some(&1));
        assert_eq!(sample.sent_total.get(&data("boxes")), Some(&40));
        assert_eq!(sample.shm_slots_in_use, 2);
        assert_eq!(sample.total_dropped(), 1);
        assert_eq!(sample.deepest_queue(), Some((&data("frames"), 3)));
        assert_eq!(
            sample.timer_jitter_p99_us, 150,
            "a running timer's jitter reaches the exported sample"
        );
    }

    #[test]
    fn a_request_with_no_jitter_reading_samples_as_zero() {
        let start = Instant::now();
        let mut collector = NodeMetricsCollector::new(Duration::from_secs(2), start);
        let request = NodeSampleRequest::new(node("idle"), std::process::id());
        let samples = collector.sample(dataflow(), &[request], HlcTimestamp::new(1, 0), start);
        assert_eq!(samples[0].timer_jitter_p99_us, 0);
    }

    #[test]
    fn a_dead_pid_is_counted_rather_than_invented() {
        let start = Instant::now();
        let mut collector = NodeMetricsCollector::new(Duration::from_secs(2), start);
        let request = NodeSampleRequest::new(node("ghost"), u32::MAX);
        let samples = collector.sample(dataflow(), &[request], HlcTimestamp::new(1, 0), start);

        assert_eq!(samples.len(), 1, "the node is still reported");
        assert_eq!(samples[0].cpu_percent, 0.0);
        assert_eq!(samples[0].rss_bytes, 0);
        assert_eq!(collector.failures(), 1);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn this_process_reads_with_a_known_fidelity() {
        let start = Instant::now();
        let mut collector = NodeMetricsCollector::new(Duration::from_secs(2), start);
        let request = NodeSampleRequest::new(node("self"), std::process::id());
        let samples = collector.sample(dataflow(), &[request], HlcTimestamp::new(1, 0), start);

        assert_eq!(samples.len(), 1);
        assert_eq!(collector.failures(), 0);
        assert!(
            collector.fidelity().is_some(),
            "a successful reading records how it was obtained"
        );
    }

    #[test]
    fn arming_restarts_the_cadence_without_a_round() {
        let start = Instant::now();
        let mut collector = NodeMetricsCollector::new(Duration::from_secs(2), start);
        let at = start + Duration::from_secs(2);
        assert!(collector.due(at));

        collector.arm_next(at);
        assert!(!collector.due(at), "the round is no longer due");
        assert_eq!(collector.rounds(), 0, "arming is not a round");
        assert!(collector.due(at + Duration::from_secs(2)));
    }

    #[test]
    fn a_fresh_collector_has_no_fidelity_yet() {
        let collector = NodeMetricsCollector::new(Duration::from_secs(2), Instant::now());
        assert!(collector.fidelity().is_none());
        assert_eq!(collector.rounds(), 0);
        assert_eq!(collector.failures(), 0);
    }

    #[test]
    fn several_nodes_are_sampled_in_one_round() {
        let start = Instant::now();
        let mut collector = NodeMetricsCollector::new(Duration::from_secs(2), start);
        let requests = [
            NodeSampleRequest::new(node("a"), std::process::id()),
            NodeSampleRequest::new(node("b"), std::process::id()),
        ];
        let samples = collector.sample(dataflow(), &requests, HlcTimestamp::new(1, 0), start);
        assert_eq!(samples.len(), 2);
        assert_eq!(collector.rounds(), 1, "one round, not one per node");
    }
}
