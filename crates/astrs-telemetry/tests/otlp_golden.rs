//! Golden-file tests: a synthetic trace and a synthetic metrics snapshot,
//! mapped through [`astrs_telemetry::otlp`] and compared byte-for-byte
//! against a committed expected JSON payload. A schema change shows up
//! as a diff against `tests/golden/*.json` here, rather than as a silent
//! drift some downstream OTLP collector discovers first.
//!
//! To regenerate both golden files after a deliberate schema change, run
//! `ASTRS_WRITE_GOLDEN=1 cargo test -p astrs-telemetry --test otlp_golden`
//! and review the resulting diff.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use astrs_telemetry::otlp::common::Resource;
use astrs_telemetry::otlp::{build_metrics_request, build_trace_request};
use astrs_time::HlcTimestamp;
use astrs_wire::{
    DataflowId, HistogramBucket, MetricBatch, MetricPoint, MetricValue, NodeId, SpanStatus,
    TraceSpan,
};

fn golden_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

fn assert_matches_golden(actual_value: &impl serde::Serialize, golden_file: &str) {
    let actual = serde_json::to_string_pretty(actual_value).expect("serialize") + "\n";
    let path = golden_path(golden_file);
    if std::env::var_os("ASTRS_WRITE_GOLDEN").is_some() {
        std::fs::write(&path, &actual).expect("write golden file");
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("reading golden file {}: {error}", path.display()));
    assert_eq!(
        actual, expected,
        "OTLP JSON shape changed -- if this is intentional, overwrite {golden_file} with the printed `actual` above",
    );
}

/// A trace with a root and a child span, spanning two dataflow
/// participants' worth of attributes, statuses and hex ids -- enough
/// surface to catch a schema regression in field naming, enum mapping,
/// or attribute handling.
#[test]
fn otlp_trace_export_matches_the_golden_file() {
    let mut root = TraceSpan::new(
        "4bf92f3577b34da6a3ce929d0e0e4736",
        "00f067aa0ba902b7",
        "publish",
        HlcTimestamp::new(1_700_000_000_000_000_000, 0),
    )
    .with_end(HlcTimestamp::new(1_700_000_000_002_000_000, 0))
    .with_status(SpanStatus::Ok)
    .with_attribute("output", "image");
    root.dataflow = Some(DataflowId::from_u128(
        0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10,
    ));
    root.node = Some(NodeId::new("camera").expect("valid node id"));

    let child = TraceSpan::new(
        "4bf92f3577b34da6a3ce929d0e0e4736",
        "b7ad6b7169203331",
        "deliver",
        HlcTimestamp::new(1_700_000_000_001_000_000, 0),
    )
    .with_parent("00f067aa0ba902b7")
    .with_end(HlcTimestamp::new(1_700_000_000_003_000_000, 0))
    .with_status(SpanStatus::Error);

    // An unfinished span must never appear in the export -- included
    // here so the golden file also pins that it is silently dropped.
    let unfinished = TraceSpan::new(
        "4bf92f3577b34da6a3ce929d0e0e4736",
        "1111111111111111",
        "still-open",
        HlcTimestamp::new(1_700_000_000_004_000_000, 0),
    );

    let resource = Resource::from_attributes([
        ("service.name", "astrs-daemon"),
        ("service.instance.id", "host-1"),
    ]);
    let request = build_trace_request(&[root, child, unfinished], resource);

    assert_matches_golden(&request, "otlp_trace.json");
}

/// A metrics snapshot with one counter, one gauge, and one histogram --
/// including a labelled counter with two data points, to also pin the
/// "points sharing a name become multiple data points on one metric"
/// grouping behavior in the golden output.
#[test]
fn otlp_metrics_export_matches_the_golden_file() {
    let batch = MetricBatch {
        timestamp: HlcTimestamp::new(1_700_000_000_000_000_000, 0),
        scope: "astrs_daemon".to_owned(),
        dataflow: Some(DataflowId::from_u128(1)),
        points: vec![
            MetricPoint::counter("frames_total", 42),
            MetricPoint::counter("io_bytes_total", 100).with_label("direction", "rx"),
            MetricPoint::counter("io_bytes_total", 200).with_label("direction", "tx"),
            MetricPoint::gauge("queue_depth", 3.5),
            MetricPoint {
                name: "latency_seconds".to_owned(),
                value: MetricValue::Histogram {
                    count: 10,
                    sum: 4.25,
                    buckets: vec![
                        HistogramBucket {
                            upper_bound: 0.1,
                            cumulative_count: 3,
                        },
                        HistogramBucket {
                            upper_bound: 1.0,
                            cumulative_count: 9,
                        },
                        HistogramBucket {
                            upper_bound: f64::INFINITY,
                            cumulative_count: 10,
                        },
                    ],
                },
                labels: std::collections::BTreeMap::new(),
            },
        ],
    };

    let resource = Resource::from_attributes([("service.name", "astrs-daemon")]);
    let request = build_metrics_request(&batch, resource);

    assert_matches_golden(&request, "otlp_metrics.json");
}
