//! Hand-rolled OTLP/HTTP+JSON schema types (blueprint §13: "own ~1.5k-line
//! implementation over oxihttp" — no `opentelemetry-*`/`prost`, both
//! banned outright, §18.1).
//!
//! | Module | Contents |
//! |---|---|
//! | [`common`] | `Resource`, `Scope`, `KeyValue`/`AnyValue` — shared by both payloads |
//! | [`trace`] | [`astrs_wire::TraceSpan`] → `ExportTraceServiceRequest` |
//! | [`metrics`] | [`astrs_wire::MetricBatch`] → `ExportMetricsServiceRequest` |
//!
//! These are plain `serde`-derived structs matching the
//! `opentelemetry.proto.*.v1` messages' JSON mapping closely enough for
//! any standard OTLP/HTTP collector to accept — golden-file tests in
//! `tests/otlp_golden_*.rs` pin the exact byte-for-byte shape for a
//! synthetic trace and a metrics snapshot, so a schema change is a
//! visible diff, not a silent drift.

pub mod common;
pub mod metrics;
pub mod trace;

pub use common::{AnyValue, KeyValue, Resource, SCOPE_NAME, SCOPE_VERSION, Scope};
pub use metrics::{ExportMetricsServiceRequest, build_metrics_request};
pub use trace::{ExportTraceServiceRequest, build_trace_request, status_code};
