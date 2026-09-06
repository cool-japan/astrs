//! Health, statistics and telemetry samples.
//!
//! Blueprint §13: the daemon samples per-node CPU / RSS / queue depth / SHM
//! stats every two seconds and forwards them to the coordinator, which feeds
//! `astrs top` and the TUI. `astrs-telemetry` owns the in-process registry and
//! the OTLP exporter; this crate owns the shapes those samples take on the
//! wire, so the daemon and the coordinator agree on them without either
//! depending on the telemetry crate.
//!
//! Histogram buckets are **fixed** and carried explicitly rather than
//! reconstructed from a configuration both ends must share — that is the
//! difference between a metric that survives a rolling upgrade and one that
//! silently changes meaning.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{DaemonStats, MetricPoint, MetricValue};
//!
//! let stats = DaemonStats::default();
//! assert_eq!(stats.node_count, 0);
//!
//! let point = MetricPoint::counter("shm_fallback_total", 3);
//! assert_eq!(point.value.as_f64(), 3.0);
//! ```

use core::fmt;
use std::collections::BTreeMap;

use astrs_time::HlcTimestamp;
use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::common::duration::DurationMs;
use crate::ids::{DataId, DataflowId, NodeId};

/// The maximum number of metric points accepted in one telemetry batch.
///
/// Batches are fanned out to every telemetry subscriber; an unbounded batch is
/// an amplification vector.
pub const MAX_METRIC_POINTS: usize = 4096;

/// Whole-daemon health, reported on every heartbeat (§12: heartbeat 5 s).
///
/// # Examples
///
/// ```
/// use astrs_wire::DaemonStats;
///
/// let stats = DaemonStats {
///     node_count: 4,
///     shm_fallback_total: 2,
///     ..DaemonStats::default()
/// };
/// assert!(stats.has_shm_fallbacks());
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct DaemonStats {
    /// How long the daemon has been up.
    pub uptime: DurationMs,
    /// How many nodes it currently supervises.
    pub node_count: u32,
    /// How many dataflows it participates in.
    pub dataflow_count: u32,
    /// Daemon process CPU usage, as a percentage of one core.
    pub cpu_percent: f32,
    /// Daemon process resident memory, in bytes.
    pub rss_bytes: u64,
    /// Total bytes currently mapped across all shared-memory segments.
    pub shm_bytes_mapped: u64,
    /// How many times a send fell back from shared memory to the reliable
    /// daemon path because the pool was exhausted.
    ///
    /// Blueprint §6.2 makes this a *visible* metric on purpose: dora's
    /// sleep-retry on pool exhaustion (PR-2366) hid the problem instead of
    /// reporting it.
    pub shm_fallback_total: u64,
    /// Control-plane frames sent since start.
    pub frames_sent: u64,
    /// Control-plane frames received since start.
    pub frames_received: u64,
    /// Control-plane bytes sent since start.
    pub bytes_sent: u64,
    /// Control-plane bytes received since start.
    pub bytes_received: u64,
}

impl DaemonStats {
    /// Whether any shared-memory send has fallen back to the reliable path.
    #[must_use]
    pub const fn has_shm_fallbacks(&self) -> bool {
        self.shm_fallback_total > 0
    }

    /// Bitwise equality, so a `NaN` CPU reading compares equal to itself.
    #[must_use]
    pub fn bitwise_eq(&self, other: &Self) -> bool {
        self.uptime == other.uptime
            && self.node_count == other.node_count
            && self.dataflow_count == other.dataflow_count
            && self.cpu_percent.to_bits() == other.cpu_percent.to_bits()
            && self.rss_bytes == other.rss_bytes
            && self.shm_bytes_mapped == other.shm_bytes_mapped
            && self.shm_fallback_total == other.shm_fallback_total
            && self.frames_sent == other.frames_sent
            && self.frames_received == other.frames_received
            && self.bytes_sent == other.bytes_sent
            && self.bytes_received == other.bytes_received
    }
}

/// One node's resource and queue sample (§13: every two seconds).
///
/// # Examples
///
/// ```
/// use astrs_wire::{DataId, NodeId, NodeMetricsSample};
/// use astrs_time::HlcTimestamp;
///
/// let mut sample = NodeMetricsSample::new(NodeId::new("camera")?, HlcTimestamp::new(1, 0));
/// sample.queue_depths.insert(DataId::new("frames")?, 7);
/// assert_eq!(sample.deepest_queue(), Some((&DataId::new("frames")?, 7)));
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct NodeMetricsSample {
    /// Which node this describes.
    pub node: NodeId,
    /// When the sample was taken.
    pub timestamp: HlcTimestamp,
    /// Node process CPU usage, as a percentage of one core.
    pub cpu_percent: f32,
    /// Node process resident memory, in bytes.
    pub rss_bytes: u64,
    /// Current depth of each input queue, in messages.
    pub queue_depths: BTreeMap<DataId, u32>,
    /// Messages sent on each output since start.
    pub sent_total: BTreeMap<DataId, u64>,
    /// Messages received on each input since start.
    pub received_total: BTreeMap<DataId, u64>,
    /// Messages dropped by queue policy on each input since start.
    pub dropped_total: BTreeMap<DataId, u64>,
    /// Shared-memory slots the node currently holds.
    pub shm_slots_in_use: u32,
    /// 99th-percentile timer jitter in microseconds (§11.1).
    pub timer_jitter_p99_us: u64,
}

impl NodeMetricsSample {
    /// An empty sample for `node` at `timestamp`.
    #[must_use]
    pub fn new(node: NodeId, timestamp: HlcTimestamp) -> Self {
        Self {
            node,
            timestamp,
            cpu_percent: 0.0,
            rss_bytes: 0,
            queue_depths: BTreeMap::new(),
            sent_total: BTreeMap::new(),
            received_total: BTreeMap::new(),
            dropped_total: BTreeMap::new(),
            shm_slots_in_use: 0,
            timer_jitter_p99_us: 0,
        }
    }

    /// The input with the deepest queue, if any.
    ///
    /// This is what `astrs top` sorts by: the deepest queue is where
    /// backpressure is about to become message loss.
    #[must_use]
    pub fn deepest_queue(&self) -> Option<(&DataId, u32)> {
        self.queue_depths
            .iter()
            .max_by_key(|(_, depth)| **depth)
            .map(|(id, depth)| (id, *depth))
    }

    /// The total number of messages dropped across all inputs.
    #[must_use]
    pub fn total_dropped(&self) -> u64 {
        self.dropped_total.values().copied().sum()
    }
}

/// One node's *bandwidth* sample: bytes rather than messages, plus the
/// shared-memory occupancy behind them (§6.2, §13).
///
/// # Why this is not fields on [`NodeMetricsSample`]
///
/// `DaemonEvent::NodeMetrics` is inside the frozen protocol prefix
/// (`crates/astrs-wire/tests/golden/protocol.frozen.snap`), so
/// [`NodeMetricsSample`]'s encoding cannot change — §7.2's append-only rule
/// applies to a message's *fields* exactly as it does to a family's variants,
/// because both are visible in the bytes. Bandwidth therefore arrives as its
/// own shape on its own tail-appended variant
/// ([`crate::DaemonEvent::NodeIoMetrics`]) rather than by widening a frozen
/// one.
///
/// The split is not only a compatibility artefact. A message count answers
/// "is this edge alive?"; a byte count answers "is this link saturated?", and
/// the two are sampled for different reasons — an `astrs top` that shows
/// queue depth wants the first, one that shows throughput wants the second.
///
/// # Rates are the reader's job
///
/// Every total here is monotonic since the node's current incarnation
/// registered, never a rate. The consumer that wants MiB/s divides the
/// difference between two samples by the difference between their
/// [`NodeIoSample::timestamp`]s — which is the only way to get a rate that
/// stays right when a sample is dropped, delayed, or taken on a cadence the
/// reader did not choose.
///
/// # Examples
///
/// ```
/// use astrs_wire::{DataId, NodeId, NodeIoSample};
/// use astrs_time::HlcTimestamp;
///
/// let mut sample = NodeIoSample::new(NodeId::new("camera")?, HlcTimestamp::new(1, 0));
/// sample.sent_bytes_total.insert(DataId::new("image")?, 4_096);
/// assert_eq!(sample.total_sent_bytes(), 4_096);
/// assert_eq!(sample.shm_slot_occupancy(), None, "no ring, no occupancy");
///
/// sample.shm_slots_in_use = 3;
/// sample.shm_slots_total = 8;
/// assert_eq!(sample.shm_slot_occupancy(), Some(0.375));
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct NodeIoSample {
    /// Which node this describes.
    pub node: NodeId,
    /// When the sample was taken, on the daemon's hybrid logical clock.
    ///
    /// The denominator of every rate derived from two of these.
    pub timestamp: HlcTimestamp,
    /// Payload bytes published on each output since the node registered.
    ///
    /// Counted once per *publish*, not once per consumer: a fan-out to three
    /// consumers is one message of `n` bytes leaving the producer, and
    /// charging it three times would make a graph's egress depend on its
    /// topology rather than on what it produced.
    pub sent_bytes_total: BTreeMap<DataId, u64>,
    /// Payload bytes delivered into each input's queue since the node
    /// registered.
    ///
    /// Counted on delivery, not on publish, so a message the queue policy
    /// discarded (§11.2) is absent here and present in
    /// [`NodeMetricsSample::dropped_total`] — the pair is what makes "the
    /// link is saturated" distinguishable from "the consumer is too slow".
    pub received_bytes_total: BTreeMap<DataId, u64>,
    /// Ring slots this node's shared-memory segments currently hold written
    /// but not yet reclaimed (§6.2).
    pub shm_slots_in_use: u32,
    /// The total slots across those segments — the denominator of
    /// [`NodeIoSample::shm_slot_occupancy`].
    pub shm_slots_total: u32,
    /// How many publishes fell back to the daemon path because a ring could
    /// not take them (§6.2: never sleep-retry).
    pub shm_fallback_total: u64,
}

impl NodeIoSample {
    /// An empty sample for `node` at `timestamp`.
    #[must_use]
    pub fn new(node: NodeId, timestamp: HlcTimestamp) -> Self {
        Self {
            node,
            timestamp,
            sent_bytes_total: BTreeMap::new(),
            received_bytes_total: BTreeMap::new(),
            shm_slots_in_use: 0,
            shm_slots_total: 0,
            shm_fallback_total: 0,
        }
    }

    /// Every output's bytes, added up.
    #[must_use]
    pub fn total_sent_bytes(&self) -> u64 {
        self.sent_bytes_total.values().copied().sum()
    }

    /// Every input's bytes, added up.
    #[must_use]
    pub fn total_received_bytes(&self) -> u64 {
        self.received_bytes_total.values().copied().sum()
    }

    /// The busiest output, by bytes.
    #[must_use]
    pub fn busiest_output(&self) -> Option<(&DataId, u64)> {
        self.sent_bytes_total
            .iter()
            .max_by_key(|(_, bytes)| **bytes)
            .map(|(id, bytes)| (id, *bytes))
    }

    /// The busiest input, by bytes.
    #[must_use]
    pub fn busiest_input(&self) -> Option<(&DataId, u64)> {
        self.received_bytes_total
            .iter()
            .max_by_key(|(_, bytes)| **bytes)
            .map(|(id, bytes)| (id, *bytes))
    }

    /// The fraction of this node's shared-memory slots currently in use, or
    /// `None` when it holds no rings at all.
    ///
    /// `None` rather than `0.0` because "no ring" and "an empty ring" are
    /// different facts about a node, and a display that renders them the same
    /// hides the moment a route was downgraded off the shared-memory plane.
    #[must_use]
    pub fn shm_slot_occupancy(&self) -> Option<f64> {
        (self.shm_slots_total > 0)
            .then(|| f64::from(self.shm_slots_in_use) / f64::from(self.shm_slots_total))
    }

    /// The byte totals that changed between `earlier` and this sample, and how
    /// much time separated them — everything a rate needs, and nothing that
    /// assumes a cadence.
    ///
    /// `None` when the samples are not usable as a pair: a different node, or
    /// a timestamp that did not advance (two readings of one instant have no
    /// rate).
    #[must_use]
    pub fn delta_since(&self, earlier: &Self) -> Option<IoDelta> {
        if earlier.node != self.node {
            return None;
        }
        let nanos = self
            .timestamp
            .physical_ns()
            .checked_sub(earlier.timestamp.physical_ns())?;
        if nanos == 0 {
            return None;
        }
        Some(IoDelta {
            nanos,
            sent_bytes: self
                .total_sent_bytes()
                .saturating_sub(earlier.total_sent_bytes()),
            received_bytes: self
                .total_received_bytes()
                .saturating_sub(earlier.total_received_bytes()),
        })
    }
}

/// The difference between two [`NodeIoSample`]s, from which a rate is one
/// division.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoDelta {
    /// How much physical time the two samples span, in nanoseconds.
    pub nanos: u64,
    /// Payload bytes published in that span.
    pub sent_bytes: u64,
    /// Payload bytes delivered in that span.
    pub received_bytes: u64,
}

impl IoDelta {
    /// Bytes published per second.
    #[must_use]
    pub fn sent_bytes_per_second(&self) -> f64 {
        Self::per_second(self.sent_bytes, self.nanos)
    }

    /// Bytes delivered per second.
    #[must_use]
    pub fn received_bytes_per_second(&self) -> f64 {
        Self::per_second(self.received_bytes, self.nanos)
    }

    /// `bytes / (nanos / 1e9)`, guarding the zero span the constructor
    /// already refuses — belt and braces, because this is the one division in
    /// the metrics path a caller could reach with a hand-built value.
    fn per_second(bytes: u64, nanos: u64) -> f64 {
        if nanos == 0 {
            return 0.0;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "a rate is a display figure; f64 holds byte counts exactly to 2^53"
        )]
        let seconds = nanos as f64 / 1e9;
        #[expect(
            clippy::cast_precision_loss,
            reason = "same: the quotient is rendered, never compared for equality"
        )]
        let bytes = bytes as f64;
        bytes / seconds
    }
}

/// One histogram bucket: a cumulative count at or below an upper bound.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct HistogramBucket {
    /// The inclusive upper bound of this bucket.
    pub upper_bound: f64,
    /// How many observations fell at or below `upper_bound`.
    pub cumulative_count: u64,
}

/// The value of one metric point.
///
/// # Examples
///
/// ```
/// use astrs_wire::MetricValue;
///
/// assert_eq!(MetricValue::Counter(7).as_f64(), 7.0);
/// assert_eq!(MetricValue::Gauge(1.5).as_f64(), 1.5);
/// assert_eq!(MetricValue::Counter(3).kind_name(), "counter");
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MetricValue {
    /// A monotonically increasing count.
    #[oxicode(variant = 0)]
    Counter(u64),
    /// An instantaneous reading that may go up or down.
    #[oxicode(variant = 1)]
    Gauge(f64),
    /// A fixed-bucket histogram.
    #[oxicode(variant = 2)]
    Histogram {
        /// Total number of observations.
        count: u64,
        /// Sum of all observed values.
        sum: f64,
        /// The cumulative buckets, in ascending bound order.
        buckets: Vec<HistogramBucket>,
    },
}

impl MetricValue {
    /// A scalar rendering: the count, the gauge, or a histogram's observation
    /// count.
    #[must_use]
    pub fn as_f64(&self) -> f64 {
        match self {
            Self::Counter(value) => *value as f64,
            Self::Gauge(value) => *value,
            Self::Histogram { count, .. } => *count as f64,
        }
    }

    /// A stable, lower-case name for the value's shape.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Counter(_) => "counter",
            Self::Gauge(_) => "gauge",
            Self::Histogram { .. } => "histogram",
        }
    }

    /// Bitwise equality, so `NaN` gauges compare equal to themselves.
    #[must_use]
    pub fn bitwise_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Gauge(left), Self::Gauge(right)) => left.to_bits() == right.to_bits(),
            (
                Self::Histogram {
                    count: lc,
                    sum: ls,
                    buckets: lb,
                },
                Self::Histogram {
                    count: rc,
                    sum: rs,
                    buckets: rb,
                },
            ) => {
                lc == rc
                    && ls.to_bits() == rs.to_bits()
                    && lb.len() == rb.len()
                    && lb.iter().zip(rb.iter()).all(|(a, b)| {
                        a.upper_bound.to_bits() == b.upper_bound.to_bits()
                            && a.cumulative_count == b.cumulative_count
                    })
            }
            _ => self == other,
        }
    }
}

/// One named, labelled metric reading.
///
/// # Examples
///
/// ```
/// use astrs_wire::MetricPoint;
///
/// let point = MetricPoint::gauge("queue_depth", 4.0)
///     .with_label("input", "frames");
/// assert_eq!(point.label("input"), Some("frames"));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct MetricPoint {
    /// The metric name, e.g. `shm_fallback_total`.
    pub name: String,
    /// The reading.
    pub value: MetricValue,
    /// Labels, in key order.
    pub labels: BTreeMap<String, String>,
}

impl MetricPoint {
    /// A counter point with no labels.
    #[must_use]
    pub fn counter(name: impl Into<String>, value: u64) -> Self {
        Self {
            name: name.into(),
            value: MetricValue::Counter(value),
            labels: BTreeMap::new(),
        }
    }

    /// A gauge point with no labels.
    #[must_use]
    pub fn gauge(name: impl Into<String>, value: f64) -> Self {
        Self {
            name: name.into(),
            value: MetricValue::Gauge(value),
            labels: BTreeMap::new(),
        }
    }

    /// Adds a label.
    #[must_use]
    pub fn with_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    /// Looks up a label.
    #[must_use]
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(String::as_str)
    }
}

impl fmt::Display for MetricPoint {
    /// Renders `name{k=v,…} value`.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::MetricPoint;
    ///
    /// let point = MetricPoint::counter("frames_total", 9).with_label("node", "camera");
    /// assert_eq!(point.to_string(), "frames_total{node=camera} 9");
    /// ```
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if !self.labels.is_empty() {
            f.write_str("{")?;
            for (index, (key, value)) in self.labels.iter().enumerate() {
                if index > 0 {
                    f.write_str(",")?;
                }
                write!(f, "{key}={value}")?;
            }
            f.write_str("}")?;
        }
        match &self.value {
            MetricValue::Counter(value) => write!(f, " {value}"),
            MetricValue::Gauge(value) => write!(f, " {value}"),
            MetricValue::Histogram { count, sum, .. } => write!(f, " count={count} sum={sum}"),
        }
    }
}

/// A batch of metric points sampled at one instant.
///
/// # Examples
///
/// ```
/// use astrs_wire::{MetricBatch, MetricPoint};
/// use astrs_time::HlcTimestamp;
///
/// let batch = MetricBatch::new(HlcTimestamp::new(1, 0), "astrs_daemon")
///     .with_point(MetricPoint::counter("frames_total", 1));
/// assert_eq!(batch.points.len(), 1);
/// assert!(batch.within_limits());
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct MetricBatch {
    /// When the batch was sampled.
    pub timestamp: HlcTimestamp,
    /// The instrumentation scope — usually the crate that registered the
    /// metrics.
    pub scope: String,
    /// The dataflow these metrics belong to, if any.
    pub dataflow: Option<DataflowId>,
    /// The readings.
    pub points: Vec<MetricPoint>,
}

impl MetricBatch {
    /// An empty batch.
    #[must_use]
    pub fn new(timestamp: HlcTimestamp, scope: impl Into<String>) -> Self {
        Self {
            timestamp,
            scope: scope.into(),
            dataflow: None,
            points: Vec::new(),
        }
    }

    /// Adds a point.
    #[must_use]
    pub fn with_point(mut self, point: MetricPoint) -> Self {
        self.points.push(point);
        self
    }

    /// Whether the batch is within [`MAX_METRIC_POINTS`].
    #[must_use]
    pub fn within_limits(&self) -> bool {
        self.points.len() <= MAX_METRIC_POINTS
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireDecode, WireEncode};

    fn node() -> NodeId {
        NodeId::new("camera").unwrap()
    }

    #[test]
    fn daemon_stats_default_to_zero() {
        let stats = DaemonStats::default();
        assert_eq!(stats.node_count, 0);
        assert_eq!(stats.uptime, DurationMs::ZERO);
        assert!(!stats.has_shm_fallbacks());
        assert_eq!(stats.cpu_percent, 0.0);
    }

    #[test]
    fn daemon_stats_round_trip() {
        let stats = DaemonStats {
            uptime: DurationMs::from_secs(3_600),
            node_count: 12,
            dataflow_count: 2,
            cpu_percent: 37.5,
            rss_bytes: 128 * 1024 * 1024,
            shm_bytes_mapped: 64 * 1024 * 1024,
            shm_fallback_total: 3,
            frames_sent: 1_000,
            frames_received: 999,
            bytes_sent: 1 << 30,
            bytes_received: 1 << 29,
        };
        let bytes = stats.encode_to_vec().unwrap();
        assert_eq!(DaemonStats::decode_exact(&bytes).unwrap(), stats);
        assert!(stats.has_shm_fallbacks());
    }

    #[test]
    fn daemon_stats_survive_a_nan_cpu_reading() {
        let stats = DaemonStats {
            cpu_percent: f32::NAN,
            ..DaemonStats::default()
        };
        let bytes = stats.encode_to_vec().unwrap();
        let decoded = DaemonStats::decode_exact(&bytes).unwrap();
        assert_ne!(decoded, stats, "PartialEq cannot see NaN as equal");
        assert!(decoded.bitwise_eq(&stats));
    }

    #[test]
    fn node_metrics_round_trip_and_summarise() {
        let mut sample = NodeMetricsSample::new(node(), HlcTimestamp::new(5, 1));
        sample.cpu_percent = 12.5;
        sample.rss_bytes = 1 << 20;
        sample
            .queue_depths
            .insert(DataId::new("frames").unwrap(), 3);
        sample
            .queue_depths
            .insert(DataId::new("control").unwrap(), 9);
        sample
            .dropped_total
            .insert(DataId::new("frames").unwrap(), 4);
        sample
            .dropped_total
            .insert(DataId::new("control").unwrap(), 1);
        sample.timer_jitter_p99_us = 250;

        let deepest = sample.deepest_queue().unwrap();
        assert_eq!(deepest.0.as_str(), "control");
        assert_eq!(deepest.1, 9);
        assert_eq!(sample.total_dropped(), 5);

        let bytes = sample.encode_to_vec().unwrap();
        assert_eq!(NodeMetricsSample::decode_exact(&bytes).unwrap(), sample);
    }

    #[test]
    fn an_empty_sample_has_no_deepest_queue() {
        let sample = NodeMetricsSample::new(node(), HlcTimestamp::EPOCH);
        assert_eq!(sample.deepest_queue(), None);
        assert_eq!(sample.total_dropped(), 0);
    }

    #[test]
    fn metric_values_round_trip_and_classify() {
        let values = [
            MetricValue::Counter(0),
            MetricValue::Counter(u64::MAX),
            MetricValue::Gauge(-1.5),
            MetricValue::Histogram {
                count: 10,
                sum: 55.0,
                buckets: vec![
                    HistogramBucket {
                        upper_bound: 1.0,
                        cumulative_count: 2,
                    },
                    HistogramBucket {
                        upper_bound: f64::INFINITY,
                        cumulative_count: 10,
                    },
                ],
            },
        ];
        let mut names = std::collections::BTreeSet::new();
        for value in values {
            let bytes = value.encode_to_vec().unwrap();
            let decoded = MetricValue::decode_exact(&bytes).unwrap();
            assert!(value.bitwise_eq(&decoded));
            names.insert(value.kind_name());
        }
        assert_eq!(names.len(), 3);

        assert_eq!(MetricValue::Counter(7).as_f64(), 7.0);
        assert_eq!(MetricValue::Gauge(0.25).as_f64(), 0.25);
        assert_eq!(
            MetricValue::Histogram {
                count: 3,
                sum: 0.0,
                buckets: vec![]
            }
            .as_f64(),
            3.0
        );
    }

    #[test]
    fn metric_values_survive_nan() {
        let value = MetricValue::Gauge(f64::NAN);
        let bytes = value.encode_to_vec().unwrap();
        let decoded = MetricValue::decode_exact(&bytes).unwrap();
        assert!(value.bitwise_eq(&decoded));
        assert!(!value.bitwise_eq(&MetricValue::Gauge(0.0)));
        assert!(!MetricValue::Counter(1).bitwise_eq(&MetricValue::Gauge(1.0)));
    }

    #[test]
    fn metric_points_carry_labels() {
        let point = MetricPoint::gauge("queue_depth", 4.0)
            .with_label("node", "camera")
            .with_label("input", "frames");
        assert_eq!(point.label("node"), Some("camera"));
        assert_eq!(point.label("absent"), None);
        assert_eq!(point.to_string(), "queue_depth{input=frames,node=camera} 4");

        let bare = MetricPoint::counter("total", 1);
        assert_eq!(bare.to_string(), "total 1");

        let histogram = MetricPoint {
            name: "latency".to_owned(),
            value: MetricValue::Histogram {
                count: 2,
                sum: 3.0,
                buckets: vec![],
            },
            labels: BTreeMap::new(),
        };
        assert_eq!(histogram.to_string(), "latency count=2 sum=3");
    }

    #[test]
    fn metric_batches_round_trip_and_bound_themselves() {
        let batch = MetricBatch::new(HlcTimestamp::new(1, 0), "astrs_daemon")
            .with_point(MetricPoint::counter("a", 1))
            .with_point(MetricPoint::gauge("b", 2.0));
        assert!(batch.within_limits());
        let bytes = batch.encode_to_vec().unwrap();
        assert_eq!(MetricBatch::decode_exact(&bytes).unwrap(), batch);

        let mut oversized = MetricBatch::new(HlcTimestamp::EPOCH, "s");
        oversized.points = (0..=MAX_METRIC_POINTS)
            .map(|index| MetricPoint::counter(format!("m{index}"), 0))
            .collect();
        assert!(!oversized.within_limits());
    }
}
