//! The payload vocabulary shared by the seven message families.
//!
//! Every type here is a *noun* the control plane needs in more than one place:
//! a spawn specification travels in a `CoordinatorEvent::Spawn` and
//! again in a `ControlReply::NodeInfo`; an exit cause appears in a
//! `DaemonEvent::NodeStopped` and again inside a
//! [`crate::common::DataflowResult`]. Defining them once, here, is what keeps
//! the families themselves thin enough to read.
//!
//! | Module | Contents |
//! |---|---|
//! | [`duration`] | [`DurationMs`], the wire form of every timeout and delay |
//! | [`log`] | [`LogLevel`], [`LogRecord`] |
//! | [`metrics`] | [`DaemonStats`], [`NodeMetricsSample`], [`NodeIoSample`], [`MetricBatch`] |
//! | [`node`] | [`NodeSpawnSpec`] and the port / restart / logging specs it contains |
//! | [`route`] | [`Plane`], [`RouteKey`], [`RouteSpec`], the route-lifecycle reasons |
//! | [`status`] | [`DataflowStatus`], [`NodeExitCause`], [`DataflowResult`], the info rows |
//! | [`stream`] | [`DataFrame`], [`LogFrame`], [`TelemetryFrame`], [`TraceSpan`] |
//! | [`virtual_source`] | [`VIRTUAL_NODE`] and the `astrs/…` ⇄ [`crate::PortRef`] conversion (§8.4) |
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{DataflowStatus, DurationMs, NodeExitCause, Plane};
//!
//! assert_eq!(DurationMs::HEARTBEAT.to_string(), "5s");
//! assert!(DataflowStatus::Running.is_active());
//! assert!(NodeExitCause::Success.is_success());
//! assert!(Plane::Quic.needs_crc());
//! ```

pub mod duration;
pub mod log;
pub mod metrics;
pub mod node;
pub mod route;
pub mod status;
pub mod stream;
pub mod virtual_source;

pub use duration::DurationMs;
pub use log::{LogLevel, LogRecord, MAX_LOG_FIELDS};
pub use metrics::{
    DaemonStats, HistogramBucket, IoDelta, MAX_METRIC_POINTS, MetricBatch, MetricPoint,
    MetricValue, NodeIoSample, NodeMetricsSample,
};
pub use node::{
    DEFAULT_QUEUE_SIZE, DEFAULT_SHM_POOL_SIZE, DeploySpec, InputSpec, LogConfig, NodePattern,
    NodeSource, NodeSpawnSpec, OperatorSpec, OutputSpec, PriorityLane, QueuePolicy, RestartConfig,
    RestartPolicy,
};
pub use route::{
    COMPRESSION_THRESHOLD_BYTES, Plane, RouteAcceptance, RouteCloseReason, RouteDowngradeReason,
    RouteKey, RouteRejection, RouteSpec, ShmSegmentSpec,
};
pub use status::{
    DaemonInfo, DataflowResult, DataflowStatus, DataflowSummary, NodeExitCause, NodeInfo,
    NodeRunState, StopCause,
};
pub use stream::{DataFrame, LogFrame, SpanStatus, TelemetryFrame, TraceData, TraceSpan};
pub use virtual_source::{VIRTUAL_NODE, is_virtual_port, virtual_port_ref, virtual_source_text};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_documented_defaults_match_the_blueprint_table() {
        // Blueprint §24.2 "Defaults & environment variables".
        assert_eq!(DEFAULT_QUEUE_SIZE, 10);
        assert_eq!(DEFAULT_SHM_POOL_SIZE, 8 * 1024 * 1024);
        assert_eq!(DurationMs::HEARTBEAT.as_millis(), 5_000);
        assert_eq!(DurationMs::METRICS.as_millis(), 2_000);
        assert_eq!(DurationMs::HEALTH_CHECK.as_millis(), 5_000);
        assert_eq!(COMPRESSION_THRESHOLD_BYTES, 16 * 1024);
    }

    #[test]
    fn the_re_exports_name_the_types_the_families_use() {
        // A compile-time check that the surface stays complete: if a type is
        // dropped from the re-exports this stops building.
        fn assert_exists<T>() {}
        assert_exists::<DurationMs>();
        assert_exists::<LogLevel>();
        assert_exists::<LogRecord>();
        assert_exists::<DaemonStats>();
        assert_exists::<NodeMetricsSample>();
        assert_exists::<NodeIoSample>();
        assert_exists::<MetricBatch>();
        assert_exists::<MetricPoint>();
        assert_exists::<MetricValue>();
        assert_exists::<HistogramBucket>();
        assert_exists::<NodeSpawnSpec>();
        assert_exists::<NodeSource>();
        assert_exists::<OperatorSpec>();
        assert_exists::<InputSpec>();
        assert_exists::<OutputSpec>();
        assert_exists::<DeploySpec>();
        assert_exists::<LogConfig>();
        assert_exists::<RestartConfig>();
        assert_exists::<RestartPolicy>();
        assert_exists::<QueuePolicy>();
        assert_exists::<PriorityLane>();
        assert_exists::<NodePattern>();
        assert_exists::<Plane>();
        assert_exists::<RouteKey>();
        assert_exists::<RouteSpec>();
        assert_exists::<DataflowStatus>();
        assert_exists::<NodeRunState>();
        assert_exists::<NodeExitCause>();
        assert_exists::<StopCause>();
        assert_exists::<DataflowResult>();
        assert_exists::<DataflowSummary>();
        assert_exists::<NodeInfo>();
        assert_exists::<DaemonInfo>();
        assert_exists::<DataFrame>();
        assert_exists::<LogFrame>();
        assert_exists::<TelemetryFrame>();
        assert_exists::<TraceSpan>();
        assert_exists::<TraceData>();
        assert_exists::<SpanStatus>();
    }
}
