//! Metrics, tracing and export for AstRS.
//!
//! Observability without the opentelemetry/tonic dependency tree
//! (blueprint §13):
//!
//! - [`subscriber::init_telemetry`] / [`init_telemetry`] — `tracing`
//!   subscriber setup shared by every AstRS process: `RUST_LOG`-style
//!   env-filter directives, human/JSON rendering that reuses
//!   `astrs-log`'s deterministic formatters, and per-process
//!   `service.name`/node attributes.
//! - [`metrics::MetricRegistry`] — an in-process registry of
//!   [`metrics::Counter`], [`metrics::Gauge`] and [`metrics::Histogram`]
//!   handles with an allocation-free record path once a handle is
//!   resolved, and bounded-cardinality label sets
//!   ([`metrics::MetricFamily`]) that cannot be driven to unbounded
//!   memory use by a hostile or buggy label value.
//! - [`sampler::CpuMemSampler`] — per-process CPU%/RSS sampling for self
//!   and child pids, honestly reporting when a platform can only offer
//!   degraded fidelity (macOS has no `/proc`).
//! - [`propagation`] — W3C `traceparent` injection/extraction over
//!   [`astrs_wire::Metadata`], so publish → deliver → process causality
//!   survives the wire (and [`propagation::follow_with_context`] for the
//!   one hop, `Metadata::follow`, that would otherwise drop it).
//! - [`otlp`] — hand-rolled OTLP/HTTP+JSON schema types
//!   ([`astrs_wire::TraceSpan`]/[`astrs_wire::MetricBatch`] → the
//!   `ExportTraceServiceRequest`/`ExportMetricsServiceRequest` JSON
//!   shapes), independent of any HTTP client.
//! - [`export::OtlpExporter`] (feature `telemetry-export`, default on) —
//!   batches, retries with backoff, and ships both signals over
//!   `oxihttp` to any OTLP/HTTP collector, with a bounded queue and drop
//!   counter absorbing an unreachable collector without unbounded memory
//!   growth.
//!
//! # Feature flags
//!
//! | Feature | Default | Enables |
//! |---|---|---|
//! | `telemetry-export` | on | [`export`] (the OTLP/HTTP client, over `oxihttp`) |

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod error;
mod ids;
pub mod metrics;
pub mod otlp;
pub mod propagation;
pub mod sampler;
pub mod subscriber;

#[cfg(feature = "telemetry-export")]
pub mod export;

#[cfg(test)]
mod alloc_guard;

pub use error::{Result, TelemetryError};
pub use subscriber::{
    AstrsFmtLayer, AstrsSpanLayer, LogFormat, TelemetryConfig, TelemetryHandle, init_telemetry,
    init_telemetry_with_spans,
};

#[cfg(test)]
mod crate_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    /// A compile-time check that the documented top-level surface stays
    /// exported: if any of these is dropped from `lib.rs`'s re-exports,
    /// this stops building.
    #[test]
    fn top_level_re_exports_exist() {
        fn assert_exists<T>() {}
        assert_exists::<crate::TelemetryError>();
        assert_exists::<crate::TelemetryConfig>();
        assert_exists::<crate::TelemetryHandle>();
        assert_exists::<crate::LogFormat>();
    }
}
