//! `astrs trace` (blueprint §13, §17): a minimal causal span listing.
//!
//! ```text
//!   astrs trace [dataflow] [--node n]  ─► GetTraces  ─► TraceData{spans}
//! ```
//!
//! This is deliberately the whole feature: a summary table (or `--json`)
//! over whatever [`astrs_wire::ControlReply::TraceData`] answers with. A
//! full tracing UI (causal graphs, cross-daemon span joins) is out of
//! scope for this wave — see `astrs-coordinator`'s `handlers::misc::get_traces`
//! for exactly how far span collection reaches today (coordinator-local,
//! honestly empty until something feeds it) and this crate's final report
//! for what wiring a real feed would take.

use std::io::Write;

use astrs_time::HlcTimestamp;
use astrs_wire::{ControlReply, ControlRequest, NodeId, SpanStatus, TraceSpan};

use crate::command::client::{Client, DataflowRef, Endpoint, reply_name, runtime};
use crate::error::CliError;

/// `astrs trace` arguments.
#[derive(Debug, Clone, Default)]
pub struct TraceArgs {
    /// The dataflow to trace; `None` traces every dataflow the coordinator
    /// knows about.
    pub dataflow: Option<String>,
    /// Only spans from this node.
    pub node: Option<String>,
    /// Only spans starting at or after this HLC timestamp.
    pub since: Option<HlcTimestamp>,
    /// Stop after this many spans.
    pub limit: Option<u32>,
    /// Emit JSON rather than a summary table.
    pub json: bool,
}

/// What `astrs trace` printed.
#[derive(Debug, Clone)]
pub struct TraceReport {
    /// The spans the coordinator answered with, oldest first.
    pub spans: Vec<TraceSpan>,
}

/// Runs `astrs trace`.
///
/// # Errors
///
/// - [`CliError::BadArgument`] for an unusable `--node`.
/// - [`CliError::UnknownDataflow`] if a named dataflow resolves to nothing.
/// - As [`Client::connect`]/[`Client::request`] otherwise.
pub fn run(
    out: &mut dyn Write,
    endpoint: &Endpoint,
    args: &TraceArgs,
) -> Result<TraceReport, CliError> {
    let node = match &args.node {
        Some(text) => Some(NodeId::new(text).map_err(|error| CliError::BadArgument {
            flag: "node",
            value: text.clone(),
            reason: error.to_string(),
        })?),
        None => None,
    };
    let runtime = runtime()?;
    let spans = runtime.block_on(async {
        let mut client = Client::connect(endpoint).await?;
        let dataflow = match &args.dataflow {
            Some(text) => Some(client.resolve(&DataflowRef::parse(text)).await?),
            None => None,
        };
        match client
            .request(
                "trace",
                &ControlRequest::GetTraces {
                    dataflow,
                    node,
                    since: args.since,
                    limit: args.limit,
                },
            )
            .await?
        {
            ControlReply::TraceData { traces } => Ok(traces.spans),
            other => Err(CliError::UnexpectedReply {
                request: "trace",
                reply: reply_name(&other),
            }),
        }
    })?;
    print_trace(out, &spans, args.json);
    Ok(TraceReport { spans })
}

/// Prints a summary table, or the spans verbatim as JSON.
fn print_trace(out: &mut dyn Write, spans: &[TraceSpan], json: bool) {
    if json {
        let rendered: Vec<serde_json::Value> = spans.iter().map(span_json).collect();
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "spans": rendered }))
                .unwrap_or_else(|_| "{}".to_owned())
        );
        let _ = out.flush();
        return;
    }
    if spans.is_empty() {
        let _ = writeln!(
            out,
            "no spans (span collection is coordinator-local and empty until a feed is wired)"
        );
        let _ = out.flush();
        return;
    }
    let name_width = spans
        .iter()
        .map(|span| span.name.len())
        .max()
        .unwrap_or(0)
        .max("NAME".len());
    let _ = writeln!(
        out,
        "{:<16} {:<name_width$} {:<8} {:<12} {:<12}",
        "TRACE", "NAME", "STATUS", "NODE", "DURATION"
    );
    for span in spans {
        let _ = writeln!(
            out,
            "{:<16} {:<name_width$} {:<8} {:<12} {:<12}",
            short_id(&span.trace_id),
            span.name,
            status_word(span.status),
            span.node.as_ref().map_or("-", NodeId::as_str),
            duration_of(span).map_or_else(|| "-".to_owned(), |d| format!("{d:.3}s")),
        );
    }
    let _ = out.flush();
}

/// One span as a JSON object.
fn span_json(span: &TraceSpan) -> serde_json::Value {
    serde_json::json!({
        "trace_id": span.trace_id,
        "span_id": span.span_id,
        "parent_span_id": span.parent_span_id,
        "name": span.name,
        "status": status_word(span.status),
        "dataflow": span.dataflow.map(|id| id.to_string()),
        "node": span.node.as_ref().map(NodeId::as_str),
        "start": span.start.to_string(),
        "end": span.end.map(|end| end.to_string()),
        "duration_secs": duration_of(span),
        "attributes": span.attributes,
    })
}

/// A short, still-distinguishing prefix of a hex trace/span id, so the
/// table stays readable.
fn short_id(id: &str) -> String {
    id.chars().take(12).collect()
}

/// A stable lower-case word for a span's status.
const fn status_word(status: SpanStatus) -> &'static str {
    match status {
        SpanStatus::Ok => "ok",
        SpanStatus::Error => "error",
        SpanStatus::Unset => "unset",
        // `SpanStatus` is `#[non_exhaustive]`; an unknown status is shown
        // rather than hidden.
        _ => "unknown",
    }
}

/// The span's wall-clock duration, in seconds, when it has closed.
///
/// [`HlcTimestamp`]'s ordering is causal, not purely physical, so this is a
/// display convenience, not a latency measurement to alarm on.
fn duration_of(span: &TraceSpan) -> Option<f64> {
    let end = span.end?;
    Some(
        end.physical_duration_since(&span.start)
            .unwrap_or_default()
            .as_secs_f64(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn span(name: &str, status: SpanStatus, start: u64, end: Option<u64>) -> TraceSpan {
        let span = TraceSpan::new(
            "0123456789abcdef0123456789abcdef",
            "abcdef0123456789",
            name,
            HlcTimestamp::new(start, 0),
        )
        .with_status(status);
        match end {
            Some(end) => span.with_end(HlcTimestamp::new(end, 0)),
            None => span,
        }
    }

    #[test]
    fn trace_reports_a_dead_cluster_rather_than_hanging() {
        let endpoint = Endpoint::new(
            std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            astrs_wire::AuthToken::ZERO,
        );
        let error = run(&mut Vec::new(), &endpoint, &TraceArgs::default()).unwrap_err();
        assert!(matches!(error, CliError::NoCluster { .. }), "{error}");
    }

    #[test]
    fn an_unusable_node_flag_is_refused_before_the_dial() {
        let endpoint = Endpoint::new(
            std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            astrs_wire::AuthToken::ZERO,
        );
        let args = TraceArgs {
            node: Some("not a node".to_owned()),
            ..TraceArgs::default()
        };
        let error = run(&mut Vec::new(), &endpoint, &args).unwrap_err();
        assert!(
            matches!(error, CliError::BadArgument { flag: "node", .. }),
            "{error}"
        );
    }

    #[test]
    fn an_empty_span_list_says_so_rather_than_printing_a_bare_header() {
        let mut out = Vec::new();
        print_trace(&mut out, &[], false);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("no spans"), "{text}");
    }

    #[test]
    fn the_table_lists_every_span_with_its_status_and_duration() {
        let spans = vec![
            span("publish", SpanStatus::Ok, 0, Some(2_000_000_000)),
            span("deliver", SpanStatus::Error, 5, None),
        ];
        let mut out = Vec::new();
        print_trace(&mut out, &spans, false);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("publish"), "{text}");
        assert!(text.contains("deliver"), "{text}");
        assert!(text.contains("ok"), "{text}");
        assert!(text.contains("error"), "{text}");
        assert!(text.contains("2.000s"), "{text}");
    }

    #[test]
    fn json_output_carries_every_span_field() {
        let spans = vec![span("publish", SpanStatus::Ok, 0, Some(1_000_000_000))];
        let mut out = Vec::new();
        print_trace(&mut out, &spans, true);
        let text = String::from_utf8(out).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["spans"][0]["name"], "publish");
        assert_eq!(value["spans"][0]["status"], "ok");
    }

    #[test]
    fn a_span_never_closed_shows_no_duration() {
        let s = span("hung", SpanStatus::Unset, 0, None);
        assert_eq!(duration_of(&s), None);
    }
}
