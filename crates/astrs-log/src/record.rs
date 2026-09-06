//! The structured log record and its supporting sequence counter.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use astrs_time::HlcTimestamp;
use serde::{Deserialize, Serialize};

use crate::level::LogLevel;

/// A single structured log entry.
///
/// This is the JSON payload format carried on `astrs/logs/*` virtual
/// inputs (blueprint §8.4) and written, one object per line, by
/// [`crate::rotate::RotatingWriter`]. `node` deliberately holds the node
/// id as a plain `String` rather than `astrs-wire`'s `NodeId` type — wire
/// types are not a dependency of this crate (blueprint §5.2 budgets
/// `astrs-log` well below what pulling in `astrs-wire` would cost, and
/// nothing here needs anything but the id's string form).
///
/// `seq` is a per-source monotonically increasing counter (see
/// [`SeqCounter`]) that, together with `node`, gives
/// [`LogMerger`](crate::LogMerger) a deterministic tie-break for records
/// that share an identical `hlc` —
/// two records can legitimately be minted at the same physical/logical
/// HLC instant, but never with the same `(node, seq)` pair from the same
/// producer.
///
/// # Examples
///
/// ```
/// use astrs_log::{HlcTimestamp, LogLevel, LogRecord};
///
/// let record = LogRecord::new(HlcTimestamp::new(0, 0), LogLevel::Info, "astrs_daemon", "daemon ready")
///     .with_node("robot-1")
///     .with_field("pid", 4242);
///
/// assert_eq!(record.node.as_deref(), Some("robot-1"));
/// assert_eq!(record.fields["pid"], 4242);
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogRecord {
    /// When the record was minted, in cluster-wide causal order.
    pub hlc: HlcTimestamp,
    /// Severity of the record.
    pub level: LogLevel,
    /// The node that produced the record, if known. `None` for
    /// coordinator/CLI-originated records that are not attributed to a
    /// single node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// The emitting module or crate, matching `tracing`'s notion of a
    /// span/event target (e.g. `"astrs_daemon::spawn"`).
    pub target: String,
    /// The human-readable log message.
    pub message: String,
    /// Monotonically increasing per-`(node, target)`-producer sequence
    /// number; see [`SeqCounter`]. Defaults to `0` for records built
    /// without [`LogRecord::with_seq`], which is fine for single-record
    /// use but should be set explicitly by anything feeding
    /// [`crate::merge::LogMerger`].
    #[serde(default)]
    pub seq: u64,
    /// Structured key-value context, e.g. `{"frame_id": "42"}`. A
    /// `BTreeMap` rather than a `HashMap` so JSON and human-text
    /// rendering are byte-for-byte deterministic (blueprint requirement:
    /// both formatters must be deterministic).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, serde_json::Value>,
}

impl LogRecord {
    /// Builds a record with no node, `seq: 0`, and no extra fields. Chain
    /// [`LogRecord::with_node`], [`LogRecord::with_seq`],
    /// [`LogRecord::with_field`]/[`LogRecord::with_fields`] to fill those
    /// in.
    #[must_use]
    pub fn new(
        hlc: HlcTimestamp,
        level: LogLevel,
        target: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            hlc,
            level,
            node: None,
            target: target.into(),
            message: message.into(),
            seq: 0,
            fields: BTreeMap::new(),
        }
    }

    /// Shaped for a `tracing_subscriber::Layer::on_event` bridge (that
    /// bridge itself is out of scope for this crate — `astrs-telemetry`,
    /// W2, owns it): takes the pieces such a layer already has in hand
    /// after visiting a `tracing::Event` (its level and target) rather
    /// than the `&tracing::Event` itself, so this crate does not need a
    /// `tracing-subscriber` dependency.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_log::{HlcTimestamp, LogRecord};
    ///
    /// let record = LogRecord::from_tracing_event(
    ///     HlcTimestamp::default(),
    ///     tracing::Level::WARN,
    ///     "astrs_daemon::spawn",
    ///     "restart budget exhausted",
    /// );
    /// assert_eq!(record.level, astrs_log::LogLevel::Warn);
    /// ```
    #[must_use]
    pub fn from_tracing_event(
        hlc: HlcTimestamp,
        level: tracing::Level,
        target: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new(hlc, level.into(), target, message)
    }

    /// Wraps one line of a spawned node's captured stdout/stderr as a log
    /// record. Capturing the pipes themselves is the daemon's job
    /// (blueprint §5.2 lists "log capture" under `astrs-daemon`'s own
    /// responsibilities); this crate only provides the shape a captured
    /// line takes once it becomes a `LogRecord`, so it can flow through
    /// the same rotation, filter and merge pipeline as every other
    /// record.
    ///
    /// `target` is the stream name (`"stdout"`/`"stderr"`) so a
    /// [`crate::filter::LogFilter`]/formatter treats captured output like
    /// any other record; `level` defaults to [`LogLevel::Info`] for
    /// stdout and [`LogLevel::Warn`] for stderr — a best-effort heuristic
    /// (unstructured subprocess text carries no real severity), not a
    /// claim about the line's actual content.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_log::{HlcTimestamp, LogLevel, LogRecord, StdioStream};
    ///
    /// let record = LogRecord::from_captured_output(HlcTimestamp::default(), "camera", StdioStream::Stderr, "warning: low disk space");
    /// assert_eq!(record.level, LogLevel::Warn);
    /// assert_eq!(record.target, "stderr");
    /// assert_eq!(record.node.as_deref(), Some("camera"));
    /// ```
    #[must_use]
    pub fn from_captured_output(
        hlc: HlcTimestamp,
        node: impl Into<String>,
        stream: StdioStream,
        line: impl Into<String>,
    ) -> Self {
        let level = match stream {
            StdioStream::Stdout => LogLevel::Info,
            StdioStream::Stderr => LogLevel::Warn,
        };
        Self::new(hlc, level, stream.as_str(), line).with_node(node)
    }

    /// Sets the producing node id.
    #[must_use]
    pub fn with_node(mut self, node: impl Into<String>) -> Self {
        self.node = Some(node.into());
        self
    }

    /// Sets the per-source sequence number (see [`SeqCounter`]).
    #[must_use]
    pub const fn with_seq(mut self, seq: u64) -> Self {
        self.seq = seq;
        self
    }

    /// Inserts one structured field, overwriting any existing value for
    /// the same key.
    #[must_use]
    pub fn with_field(
        mut self,
        key: impl Into<String>,
        value: impl Into<serde_json::Value>,
    ) -> Self {
        self.fields.insert(key.into(), value.into());
        self
    }

    /// Replaces the entire structured-fields map.
    #[must_use]
    pub fn with_fields(mut self, fields: BTreeMap<String, serde_json::Value>) -> Self {
        self.fields = fields;
        self
    }

    /// The `(hlc, node, seq)` triple [`crate::merge::LogMerger`] orders
    /// by. Exposed so callers that maintain their own sorted buffers (or
    /// verify one) do not have to duplicate the tie-break rule.
    #[must_use]
    pub fn merge_key(&self) -> (HlcTimestamp, Option<&str>, u64) {
        (self.hlc, self.node.as_deref(), self.seq)
    }
}

/// Which standard stream a captured subprocess line came from; see
/// [`LogRecord::from_captured_output`].
///
/// # Examples
///
/// ```
/// use astrs_log::StdioStream;
///
/// assert_eq!(StdioStream::Stdout.as_str(), "stdout");
/// assert_eq!(StdioStream::Stderr.to_string(), "stderr");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StdioStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

impl StdioStream {
    /// The canonical lowercase name, also used as [`LogRecord::target`]
    /// by [`LogRecord::from_captured_output`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            StdioStream::Stdout => "stdout",
            StdioStream::Stderr => "stderr",
        }
    }
}

impl std::fmt::Display for StdioStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A monotonically increasing, thread-safe sequence-number generator.
///
/// Intended usage is one counter per log-emitting source (one per node
/// process, or one per `(node, target)` pair for finer-grained
/// interleaving), shared via `Arc<SeqCounter>` if multiple threads in that
/// source emit records concurrently, and consulted once per record via
/// [`SeqCounter::next`] to fill in [`LogRecord::seq`].
///
/// # Examples
///
/// ```
/// use astrs_log::SeqCounter;
///
/// let counter = SeqCounter::new();
/// assert_eq!(counter.next(), 0);
/// assert_eq!(counter.next(), 1);
/// assert_eq!(counter.next(), 2);
/// ```
#[derive(Debug, Default)]
pub struct SeqCounter(AtomicU64);

impl SeqCounter {
    /// Creates a counter starting at `0`.
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Creates a counter starting at `start` (useful when resuming after a
    /// restart that already emitted `start` records).
    #[must_use]
    pub const fn starting_at(start: u64) -> Self {
        Self(AtomicU64::new(start))
    }

    /// Returns the next sequence number and advances the counter.
    ///
    /// Uses [`Ordering::Relaxed`]: callers only need distinct,
    /// monotonically increasing values per counter instance, not a
    /// synchronization point with other memory operations.
    #[must_use]
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }

    /// Returns the next value without advancing the counter.
    #[must_use]
    pub fn peek(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn builder_chain_sets_every_field() {
        let record = LogRecord::new(HlcTimestamp::new(1, 0), LogLevel::Error, "t", "m")
            .with_node("n1")
            .with_seq(7)
            .with_field("a", 1)
            .with_field("b", "two");
        assert_eq!(record.node.as_deref(), Some("n1"));
        assert_eq!(record.seq, 7);
        assert_eq!(record.fields.len(), 2);
        assert_eq!(record.fields["a"], 1);
        assert_eq!(record.fields["b"], "two");
    }

    #[test]
    fn with_field_overwrites_same_key() {
        let record = LogRecord::new(HlcTimestamp::default(), LogLevel::Info, "t", "m")
            .with_field("a", 1)
            .with_field("a", 2);
        assert_eq!(record.fields.len(), 1);
        assert_eq!(record.fields["a"], 2);
    }

    #[test]
    fn from_tracing_event_converts_level() {
        let record = LogRecord::from_tracing_event(
            HlcTimestamp::default(),
            tracing::Level::ERROR,
            "t",
            "boom",
        );
        assert_eq!(record.level, LogLevel::Error);
        assert_eq!(record.node, None);
        assert_eq!(record.seq, 0);
    }

    #[test]
    fn from_captured_output_maps_stream_to_target_and_level() {
        let out = LogRecord::from_captured_output(
            HlcTimestamp::default(),
            "cam",
            StdioStream::Stdout,
            "ready",
        );
        assert_eq!(out.target, "stdout");
        assert_eq!(out.level, LogLevel::Info);
        assert_eq!(out.node.as_deref(), Some("cam"));
        assert_eq!(out.message, "ready");

        let err = LogRecord::from_captured_output(
            HlcTimestamp::default(),
            "cam",
            StdioStream::Stderr,
            "oops",
        );
        assert_eq!(err.target, "stderr");
        assert_eq!(err.level, LogLevel::Warn);
    }

    #[test]
    fn stdio_stream_serializes_lowercase_and_displays_as_str() {
        assert_eq!(
            serde_json::to_string(&StdioStream::Stdout).unwrap(),
            "\"stdout\""
        );
        assert_eq!(StdioStream::Stderr.to_string(), "stderr");
    }

    #[test]
    fn serde_round_trip_preserves_all_fields() {
        let record = LogRecord::new(HlcTimestamp::new(1, 2), LogLevel::Debug, "t", "m")
            .with_node("n")
            .with_seq(9)
            .with_field("k", true);
        let json = serde_json::to_string(&record).expect("serialize");
        let back: LogRecord = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(record, back);
    }

    #[test]
    fn deserializes_minimal_json_with_missing_optional_fields() {
        let json =
            r#"{"hlc":{"physical_ns":0,"logical":0},"level":"info","target":"t","message":"m"}"#;
        let record: LogRecord = serde_json::from_str(json).expect("deserialize");
        assert_eq!(record.node, None);
        assert_eq!(record.seq, 0);
        assert!(record.fields.is_empty());
    }

    #[test]
    fn omits_absent_node_and_empty_fields_from_json() {
        let record = LogRecord::new(HlcTimestamp::default(), LogLevel::Info, "t", "m");
        let json = serde_json::to_string(&record).expect("serialize");
        assert!(!json.contains("\"node\":"));
        assert!(!json.contains("\"fields\":"));
    }

    #[test]
    fn merge_key_reflects_hlc_node_seq() {
        let record = LogRecord::new(HlcTimestamp::new(5, 0), LogLevel::Info, "t", "m")
            .with_node("n")
            .with_seq(3);
        assert_eq!(record.merge_key(), (HlcTimestamp::new(5, 0), Some("n"), 3));
    }

    #[test]
    fn seq_counter_is_monotonic_and_peek_does_not_advance() {
        let counter = SeqCounter::new();
        assert_eq!(counter.peek(), 0);
        assert_eq!(counter.next(), 0);
        assert_eq!(counter.peek(), 1);
        assert_eq!(counter.next(), 1);
        assert_eq!(counter.next(), 2);
    }

    #[test]
    fn seq_counter_starting_at_resumes_from_given_value() {
        let counter = SeqCounter::starting_at(100);
        assert_eq!(counter.next(), 100);
        assert_eq!(counter.next(), 101);
    }
}
