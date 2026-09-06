//! Fan-out payloads: the bodies of `Data`, `Log` and `Telemetry` frames.
//!
//! Blueprint §7.3: *"Log/topic subscriptions to CLI ride the same framing
//! (`kind = Log|Data`) with a `SubscriptionId` in the payload — no bespoke
//! binary-prefix side channel."* These are those payloads.
//!
//! # Payloads are opaque bytes here
//!
//! [`DataFrame::payload`] is a `Vec<u8>` and stays one. The bytes are an
//! Arrow IPC stream produced by `astrs-data` (§6.1), and this crate does not
//! depend on that crate: the wire's job is to carry the bytes and their
//! metadata intact, and a columnar dependency here would make every consumer
//! of the protocol pay for the columnar layer.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{DataFrame, DataflowId, Metadata, SubscriptionId};
//! use astrs_time::HlcTimestamp;
//!
//! let frame = DataFrame::new(
//!     SubscriptionId::FIRST,
//!     DataflowId::from_u128(1),
//!     "camera/image".parse()?,
//!     Metadata::new(HlcTimestamp::new(1, 0)),
//!     vec![0xAA; 16],
//! );
//! assert_eq!(frame.payload_len(), 16);
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use core::fmt;
use std::collections::BTreeMap;

use astrs_time::HlcTimestamp;
use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::common::log::LogRecord;
use crate::common::metrics::MetricBatch;
use crate::ids::{DataflowId, NodeId, PortRef, SubscriptionId};
use crate::metadata::Metadata;

/// The body of a `kind = Data` frame: one subscribed message.
///
/// Produced by a daemon-side topic tap (`astrs topic echo`, enabled per
/// dataflow with `debug: true`, §13) and by replay feeds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct DataFrame {
    /// Which subscription this belongs to.
    pub subscription: SubscriptionId,
    /// The dataflow the message came from.
    pub dataflow: DataflowId,
    /// The producing port.
    pub source: PortRef,
    /// The message's metadata (§6.1).
    pub metadata: Metadata,
    /// The Arrow IPC stream bytes, exactly as they travelled on the data
    /// plane.
    pub payload: Vec<u8>,
}

impl DataFrame {
    /// Builds a data frame.
    #[must_use]
    pub const fn new(
        subscription: SubscriptionId,
        dataflow: DataflowId,
        source: PortRef,
        metadata: Metadata,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            subscription,
            dataflow,
            source,
            metadata,
            payload,
        }
    }

    /// The payload length in bytes.
    #[must_use]
    pub fn payload_len(&self) -> usize {
        self.payload.len()
    }

    /// Consumes the frame and returns the payload buffer, avoiding a copy.
    #[must_use]
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }

    /// Bitwise equality, so a `NaN` metadata parameter compares equal to
    /// itself.
    #[must_use]
    pub fn bitwise_eq(&self, other: &Self) -> bool {
        self.subscription == other.subscription
            && self.dataflow == other.dataflow
            && self.source == other.source
            && self.metadata.bitwise_eq(&other.metadata)
            && self.payload == other.payload
    }
}

/// The body of a `kind = Log` frame: one subscribed log record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct LogFrame {
    /// Which subscription this belongs to.
    pub subscription: SubscriptionId,
    /// The record.
    pub record: LogRecord,
}

impl LogFrame {
    /// Builds a log frame.
    #[must_use]
    pub const fn new(subscription: SubscriptionId, record: LogRecord) -> Self {
        Self {
            subscription,
            record,
        }
    }
}

/// The body of a `kind = Telemetry` frame: one subscribed metric batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct TelemetryFrame {
    /// Which subscription this belongs to.
    pub subscription: SubscriptionId,
    /// The batch.
    pub batch: MetricBatch,
}

impl TelemetryFrame {
    /// Builds a telemetry frame.
    #[must_use]
    pub const fn new(subscription: SubscriptionId, batch: MetricBatch) -> Self {
        Self {
            subscription,
            batch,
        }
    }
}

/// The outcome recorded on a trace span.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Encode,
    Decode,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SpanStatus {
    /// No outcome was recorded. The default.
    #[default]
    #[oxicode(variant = 0)]
    Unset,
    /// The operation succeeded.
    #[oxicode(variant = 1)]
    Ok,
    /// The operation failed.
    #[oxicode(variant = 2)]
    Error,
}

impl SpanStatus {
    /// Every status, in variant order.
    pub const ALL: &'static [Self] = &[Self::Unset, Self::Ok, Self::Error];

    /// A stable, lower-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unset => "unset",
            Self::Ok => "ok",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for SpanStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One distributed-tracing span.
///
/// Blueprint §13: span context propagates in message metadata, and per-message
/// causality (publish → deliver → process) is reconstructible from a
/// recording. Timestamps are [`HlcTimestamp`]s rather than wall clocks
/// precisely so that spans from two machines can be ordered against each
/// other.
///
/// # Examples
///
/// ```
/// use astrs_wire::{SpanStatus, TraceSpan};
/// use astrs_time::HlcTimestamp;
///
/// let span = TraceSpan::new("abc123", "def456", "deliver", HlcTimestamp::new(10, 0))
///     .with_end(HlcTimestamp::new(1_000_010, 0))
///     .with_status(SpanStatus::Ok);
///
/// assert_eq!(span.duration_us(), Some(1_000));
/// assert!(span.is_root());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct TraceSpan {
    /// The trace this span belongs to.
    pub trace_id: String,
    /// This span's id.
    pub span_id: String,
    /// The parent span, if this is not a root.
    pub parent_span_id: Option<String>,
    /// The operation name.
    pub name: String,
    /// When the span opened.
    pub start: HlcTimestamp,
    /// When it closed; `None` while still open.
    pub end: Option<HlcTimestamp>,
    /// The dataflow it belongs to, if any.
    pub dataflow: Option<DataflowId>,
    /// The node that produced it, if any.
    pub node: Option<NodeId>,
    /// The recorded outcome.
    pub status: SpanStatus,
    /// Span attributes, in key order.
    pub attributes: BTreeMap<String, String>,
}

impl TraceSpan {
    /// An open root span.
    #[must_use]
    pub fn new(
        trace_id: impl Into<String>,
        span_id: impl Into<String>,
        name: impl Into<String>,
        start: HlcTimestamp,
    ) -> Self {
        Self {
            trace_id: trace_id.into(),
            span_id: span_id.into(),
            parent_span_id: None,
            name: name.into(),
            start,
            end: None,
            dataflow: None,
            node: None,
            status: SpanStatus::Unset,
            attributes: BTreeMap::new(),
        }
    }

    /// Sets the parent span.
    #[must_use]
    pub fn with_parent(mut self, parent_span_id: impl Into<String>) -> Self {
        self.parent_span_id = Some(parent_span_id.into());
        self
    }

    /// Closes the span.
    #[must_use]
    pub const fn with_end(mut self, end: HlcTimestamp) -> Self {
        self.end = Some(end);
        self
    }

    /// Records the outcome.
    #[must_use]
    pub const fn with_status(mut self, status: SpanStatus) -> Self {
        self.status = status;
        self
    }

    /// Adds an attribute.
    #[must_use]
    pub fn with_attribute(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes.insert(key.into(), value.into());
        self
    }

    /// Whether this span has no parent.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.parent_span_id.is_none()
    }

    /// The span's duration in microseconds, if it has closed.
    ///
    /// Returns `None` for an open span, and also for a span whose end
    /// physically precedes its start — which happens when a clock was
    /// corrected mid-span and is better surfaced than reported as zero.
    #[must_use]
    pub fn duration_us(&self) -> Option<u64> {
        let end = self.end?;
        let elapsed = end.physical_duration_since(&self.start)?;
        u64::try_from(elapsed.as_micros()).ok()
    }
}

/// A batch of spans answering a `GetTraces` request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct TraceData {
    /// The spans, in the order the coordinator collected them.
    pub spans: Vec<TraceSpan>,
    /// Whether more spans were available but not returned.
    pub truncated: bool,
}

impl TraceData {
    /// An empty, untruncated batch.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            spans: Vec::new(),
            truncated: false,
        }
    }

    /// The distinct trace ids present in the batch.
    #[must_use]
    pub fn trace_ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self
            .spans
            .iter()
            .map(|span| span.trace_id.as_str())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireDecode, WireEncode, round_trip};
    use crate::common::log::LogLevel;
    use crate::common::metrics::MetricPoint;
    use crate::metadata::Parameter;

    fn port() -> PortRef {
        "camera/image".parse().unwrap()
    }

    #[test]
    fn data_frames_round_trip_with_their_payload_intact() {
        let mut metadata = Metadata::new(HlcTimestamp::new(9, 1));
        metadata.set_seq(3);
        let frame = DataFrame::new(
            SubscriptionId::new(7),
            DataflowId::from_u128(2),
            port(),
            metadata,
            (0u8..=255).collect(),
        );

        let bytes = frame.encode_to_vec().unwrap();
        let decoded = DataFrame::decode_exact(&bytes).unwrap();
        assert_eq!(decoded, frame);
        assert_eq!(decoded.payload_len(), 256);
        assert_eq!(decoded.payload, (0u8..=255).collect::<Vec<_>>());
    }

    #[test]
    fn an_empty_payload_round_trips() {
        let frame = DataFrame::new(
            SubscriptionId::NONE,
            DataflowId::NIL,
            port(),
            Metadata::default(),
            Vec::new(),
        );
        assert_eq!(round_trip(&frame).unwrap(), frame);
        assert_eq!(frame.payload_len(), 0);
        assert!(frame.clone().into_payload().is_empty());
    }

    #[test]
    fn data_frames_compare_bitwise_through_nan_metadata() {
        let mut metadata = Metadata::new(HlcTimestamp::new(1, 0));
        metadata.insert("f", Parameter::Float(f64::NAN)).unwrap();
        let frame = DataFrame::new(
            SubscriptionId::FIRST,
            DataflowId::NIL,
            port(),
            metadata,
            vec![1],
        );
        let decoded = DataFrame::decode_exact(&frame.encode_to_vec().unwrap()).unwrap();
        assert_ne!(decoded, frame);
        assert!(decoded.bitwise_eq(&frame));
    }

    #[test]
    fn log_frames_round_trip() {
        let frame = LogFrame::new(
            SubscriptionId::new(4),
            LogRecord::new(HlcTimestamp::new(1, 0), LogLevel::Warn, "careful"),
        );
        assert_eq!(round_trip(&frame).unwrap(), frame);
        assert_eq!(frame.record.message, "careful");
    }

    #[test]
    fn telemetry_frames_round_trip() {
        let frame = TelemetryFrame::new(
            SubscriptionId::new(5),
            MetricBatch::new(HlcTimestamp::new(2, 0), "astrs_daemon")
                .with_point(MetricPoint::counter("frames_total", 12)),
        );
        assert_eq!(round_trip(&frame).unwrap(), frame);
        assert_eq!(frame.batch.points.len(), 1);
    }

    #[test]
    fn span_statuses_round_trip_and_name_themselves() {
        let mut names = std::collections::BTreeSet::new();
        for status in SpanStatus::ALL.iter().copied() {
            assert_eq!(round_trip(&status).unwrap(), status);
            assert!(names.insert(status.as_str()));
            assert_eq!(status.to_string(), status.as_str());
        }
        assert_eq!(names.len(), 3);
        assert_eq!(SpanStatus::default(), SpanStatus::Unset);
    }

    #[test]
    fn spans_measure_their_own_duration() {
        let span = TraceSpan::new("t", "s", "deliver", HlcTimestamp::new(1_000, 0))
            .with_end(HlcTimestamp::new(1_000 + 2_500_000, 0));
        assert_eq!(span.duration_us(), Some(2_500));
        assert!(span.is_root());
    }

    #[test]
    fn an_open_span_has_no_duration() {
        let span = TraceSpan::new("t", "s", "process", HlcTimestamp::new(1, 0));
        assert_eq!(span.duration_us(), None);
    }

    #[test]
    fn a_backwards_span_reports_none_rather_than_zero() {
        let span = TraceSpan::new("t", "s", "process", HlcTimestamp::new(1_000, 0))
            .with_end(HlcTimestamp::new(500, 0));
        assert_eq!(span.duration_us(), None);
    }

    #[test]
    fn spans_round_trip_with_every_field_set() {
        let span = TraceSpan::new("trace-1", "span-2", "publish", HlcTimestamp::new(5, 0))
            .with_parent("span-1")
            .with_end(HlcTimestamp::new(6, 0))
            .with_status(SpanStatus::Error)
            .with_attribute("output", "image");
        assert!(!span.is_root());
        assert_eq!(round_trip(&span).unwrap(), span);
    }

    #[test]
    fn trace_data_collects_distinct_trace_ids() {
        let data = TraceData {
            spans: vec![
                TraceSpan::new("b", "1", "n", HlcTimestamp::EPOCH),
                TraceSpan::new("a", "2", "n", HlcTimestamp::EPOCH),
                TraceSpan::new("a", "3", "n", HlcTimestamp::EPOCH),
            ],
            truncated: true,
        };
        assert_eq!(data.trace_ids(), vec!["a", "b"]);
        assert_eq!(round_trip(&data).unwrap(), data);

        assert!(TraceData::empty().trace_ids().is_empty());
        assert_eq!(TraceData::default(), TraceData::empty());
    }
}
