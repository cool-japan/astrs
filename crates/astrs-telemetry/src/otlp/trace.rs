//! Maps [`astrs_wire::TraceSpan`] onto the OTLP `ExportTraceServiceRequest`
//! JSON shape.

use serde::{Deserialize, Serialize};

use astrs_wire::{SpanStatus, TraceSpan};

use crate::otlp::common::{KeyValue, Resource, SCOPE_NAME, SCOPE_VERSION, Scope};

/// `SPAN_KIND_INTERNAL`: the only span kind AstRS distinguishes today.
/// The blueprint's span model (publish → deliver → process) does not
/// currently separate client/server/producer/consumer roles the way a
/// full OTel SDK would; every span this crate exports uses this kind
/// rather than guessing at a more specific one.
const SPAN_KIND_INTERNAL: u32 = 1;

/// The top-level OTLP/HTTP trace export request:
/// `POST /v1/traces` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportTraceServiceRequest {
    /// One entry per distinct resource — this crate always emits exactly
    /// one, describing the exporting process.
    #[serde(rename = "resourceSpans")]
    pub resource_spans: Vec<ResourceSpans>,
}

/// Every span produced by one resource (process), grouped by
/// instrumentation scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceSpans {
    /// The resource (process/service) these spans came from.
    pub resource: Resource,
    /// Spans grouped by instrumentation scope; this crate always emits
    /// exactly one scope.
    #[serde(rename = "scopeSpans")]
    pub scope_spans: Vec<ScopeSpans>,
}

/// Every span produced by one instrumentation scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopeSpans {
    /// The instrumentation scope.
    pub scope: Scope,
    /// The spans themselves.
    pub spans: Vec<Span>,
}

/// One OTLP span.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    /// 32 lowercase hex characters.
    #[serde(rename = "traceId")]
    pub trace_id: String,
    /// 16 lowercase hex characters.
    #[serde(rename = "spanId")]
    pub span_id: String,
    /// 16 lowercase hex characters, absent for a root span.
    #[serde(rename = "parentSpanId", skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    /// The operation name.
    pub name: String,
    /// The span kind — always the crate-private `SPAN_KIND_INTERNAL` (see its docs).
    pub kind: u32,
    /// Nanoseconds since the UNIX epoch, as a decimal string (proto3
    /// JSON's mapping for a `fixed64`).
    #[serde(rename = "startTimeUnixNano")]
    pub start_time_unix_nano: String,
    /// Nanoseconds since the UNIX epoch, as a decimal string.
    #[serde(rename = "endTimeUnixNano")]
    pub end_time_unix_nano: String,
    /// Span attributes (dataflow/node identity plus whatever the span
    /// itself carried), omitted entirely when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<KeyValue>,
    /// The recorded outcome.
    pub status: Status,
}

/// An OTLP span status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// `0` unset, `1` ok, `2` error — matching
    /// [`astrs_wire::SpanStatus`]'s own discriminants exactly.
    pub code: u32,
}

/// Converts [`astrs_wire::SpanStatus`] to its OTLP status code.
///
/// # Examples
///
/// ```
/// use astrs_telemetry::otlp::trace::status_code;
/// use astrs_wire::SpanStatus;
///
/// assert_eq!(status_code(SpanStatus::Unset), 0);
/// assert_eq!(status_code(SpanStatus::Ok), 1);
/// assert_eq!(status_code(SpanStatus::Error), 2);
/// ```
#[must_use]
pub const fn status_code(status: SpanStatus) -> u32 {
    match status {
        SpanStatus::Unset => 0,
        SpanStatus::Ok => 1,
        SpanStatus::Error => 2,
        // `SpanStatus` is `#[non_exhaustive]`; a status this build does
        // not know about is reported as unset rather than failing the
        // whole export.
        _ => 0,
    }
}

/// Builds one OTLP `Span` from an `astrs-wire` [`TraceSpan`].
///
/// Returns `None` for a span that has not closed yet (`end.is_none()`):
/// the tracing layer that feeds this crate's exporter only ever buffers
/// finished spans (blueprint §13), so an open span reaching this
/// function is defensive-programming territory, not the expected input.
#[must_use]
pub fn span_to_otlp(span: &TraceSpan) -> Option<Span> {
    let end = span.end?;
    let mut attributes: Vec<KeyValue> = span
        .attributes
        .iter()
        .map(|(k, v)| KeyValue::string(k.clone(), v.clone()))
        .collect();
    if let Some(dataflow) = &span.dataflow {
        attributes.push(KeyValue::string("astrs.dataflow_id", dataflow.to_string()));
    }
    if let Some(node) = &span.node {
        attributes.push(KeyValue::string("astrs.node_id", node.to_string()));
    }
    Some(Span {
        trace_id: span.trace_id.clone(),
        span_id: span.span_id.clone(),
        parent_span_id: span.parent_span_id.clone(),
        name: span.name.clone(),
        kind: SPAN_KIND_INTERNAL,
        start_time_unix_nano: span.start.physical_ns().to_string(),
        end_time_unix_nano: end.physical_ns().to_string(),
        attributes,
        status: Status {
            code: status_code(span.status),
        },
    })
}

/// Builds a full `ExportTraceServiceRequest` for a batch of finished
/// spans, all attributed to one `resource` and this crate's own
/// instrumentation scope.
///
/// Unfinished spans (`end.is_none()`) are silently skipped — see
/// [`span_to_otlp`].
///
/// # Examples
///
/// ```
/// use astrs_telemetry::otlp::common::Resource;
/// use astrs_telemetry::otlp::trace::build_trace_request;
/// use astrs_wire::TraceSpan;
/// use astrs_time::HlcTimestamp;
///
/// let span = TraceSpan::new("t", "s", "publish", HlcTimestamp::new(1, 0))
///     .with_end(HlcTimestamp::new(1_000_001, 0));
/// let request = build_trace_request(&[span], Resource::from_attributes([("service.name", "cam")]));
/// assert_eq!(request.resource_spans[0].scope_spans[0].spans.len(), 1);
/// ```
#[must_use]
pub fn build_trace_request(spans: &[TraceSpan], resource: Resource) -> ExportTraceServiceRequest {
    let otlp_spans: Vec<Span> = spans.iter().filter_map(span_to_otlp).collect();
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource,
            scope_spans: vec![ScopeSpans {
                scope: Scope::new(SCOPE_NAME, SCOPE_VERSION),
                spans: otlp_spans,
            }],
        }],
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::{DataflowId, NodeId};

    fn sample_span() -> TraceSpan {
        TraceSpan::new(
            "4bf92f3577b34da6a3ce929d0e0e4736",
            "00f067aa0ba902b7",
            "publish",
            astrs_time::HlcTimestamp::new(1_700_000_000_000_000_000, 0),
        )
        .with_end(astrs_time::HlcTimestamp::new(1_700_000_000_002_000_000, 0))
        .with_status(SpanStatus::Ok)
        .with_attribute("output", "image")
    }

    #[test]
    fn unfinished_spans_are_skipped() {
        let open = TraceSpan::new("t", "s", "n", astrs_time::HlcTimestamp::EPOCH);
        assert!(span_to_otlp(&open).is_none());
        let request = build_trace_request(&[open], Resource::default());
        assert!(request.resource_spans[0].scope_spans[0].spans.is_empty());
    }

    #[test]
    fn finished_span_maps_every_field() {
        let otlp = span_to_otlp(&sample_span()).unwrap();
        assert_eq!(otlp.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(otlp.span_id, "00f067aa0ba902b7");
        assert_eq!(otlp.parent_span_id, None);
        assert_eq!(otlp.name, "publish");
        assert_eq!(otlp.start_time_unix_nano, "1700000000000000000");
        assert_eq!(otlp.end_time_unix_nano, "1700000000002000000");
        assert_eq!(otlp.status.code, 1);
        assert!(
            otlp.attributes
                .iter()
                .any(|kv| kv.key == "output" && kv.value.string_value == "image")
        );
    }

    #[test]
    fn dataflow_and_node_become_namespaced_attributes() {
        let mut span = sample_span();
        span.dataflow = Some(DataflowId::from_u128(1));
        span.node = Some(NodeId::new("camera").unwrap());
        let otlp = span_to_otlp(&span).unwrap();
        assert!(
            otlp.attributes
                .iter()
                .any(|kv| kv.key == "astrs.dataflow_id")
        );
        assert!(
            otlp.attributes
                .iter()
                .any(|kv| kv.key == "astrs.node_id" && kv.value.string_value == "camera")
        );
    }

    #[test]
    fn a_child_span_carries_its_parent_id() {
        let span = sample_span().with_parent("aaaaaaaaaaaaaaaa");
        let otlp = span_to_otlp(&span).unwrap();
        assert_eq!(otlp.parent_span_id.as_deref(), Some("aaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn build_request_wraps_one_resource_and_one_scope() {
        let request = build_trace_request(
            &[sample_span(), sample_span()],
            Resource::from_attributes([("service.name", "astrs-daemon")]),
        );
        assert_eq!(request.resource_spans.len(), 1);
        assert_eq!(request.resource_spans[0].scope_spans.len(), 1);
        assert_eq!(request.resource_spans[0].scope_spans[0].spans.len(), 2);
        assert_eq!(
            request.resource_spans[0].scope_spans[0].scope.name,
            "astrs-telemetry"
        );
    }
}
