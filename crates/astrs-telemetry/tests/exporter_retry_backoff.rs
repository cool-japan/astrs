//! Exporter integration test against a hand-rolled HTTP/1.1 sink over a
//! plain `std::net::TcpListener` (no `oxihttp-server`, no other test
//! dependency): the sink returns `503` for the first request on each
//! connection and `200` after that, capturing every request body it
//! receives so the test can assert on what the exporter actually sent,
//! not just how many times it sent something.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::type_complexity)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use astrs_telemetry::export::{ExporterConfig, OtlpExporter, RetryConfig};
use astrs_time::HlcTimestamp;
use astrs_wire::{MetricBatch, TraceSpan};

/// Reads one HTTP/1.1 request off `stream`: headers, then exactly
/// `Content-Length` body bytes (defaulting to `0` if the header is
/// absent, which is enough for this test's always-JSON, always-sized
/// requests).
fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(n) = stream.read(&mut chunk) {
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        let Some(header_end) = find_double_crlf(&buf) else {
            continue;
        };
        let headers = String::from_utf8_lossy(&buf[..header_end]);
        let content_length: usize = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if buf.len() >= header_end + 4 + content_length {
            break;
        }
    }
    buf
}

/// The byte offset just past the first `\r\n\r\n` in `buf`, if any.
fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// The request body: everything after the header/body separator found by
/// [`read_http_request`].
fn request_body(raw: &[u8]) -> Vec<u8> {
    find_double_crlf(raw).map_or_else(Vec::new, |end| raw[end + 4..].to_vec())
}

/// Spawns a background-thread HTTP/1.1 sink on an OS-assigned loopback
/// port. Serves exactly `total_requests` requests, replying
/// `first_status` to the first one on the listener and `200 OK` to
/// every one after that, then stops (its accept loop is bounded by
/// `.take(total_requests)`, so the returned `JoinHandle` always
/// completes without needing to be aborted).
fn spawn_sink(
    first_status: u16,
    total_requests: usize,
) -> (
    String,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<Vec<u8>>>>,
    std::thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral loopback port");
    let addr = listener.local_addr().expect("read the bound address");
    let request_count = Arc::new(AtomicUsize::new(0));
    let captured_bodies = Arc::new(Mutex::new(Vec::new()));

    let counter = Arc::clone(&request_count);
    let bodies = Arc::clone(&captured_bodies);
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming().take(total_requests) {
            let Ok(mut stream) = stream else { continue };
            let raw_request = read_http_request(&mut stream);
            bodies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request_body(&raw_request));

            let attempt = counter.fetch_add(1, Ordering::SeqCst);
            let status_line = if attempt == 0 {
                format!("HTTP/1.1 {first_status} Service Unavailable\r\n")
            } else {
                "HTTP/1.1 200 OK\r\n".to_owned()
            };
            let body = b"{}";
            let response = format!(
                "{status_line}Content-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len(),
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(body);
            let _ = stream.flush();
        }
    });

    (
        format!("http://{addr}"),
        request_count,
        captured_bodies,
        handle,
    )
}

fn fast_retry_config() -> ExporterConfig {
    ExporterConfig {
        retry: RetryConfig::new(3, Duration::from_millis(5), Duration::from_millis(50)),
        connect_timeout: Duration::from_secs(2),
        read_timeout: Duration::from_secs(2),
        ..ExporterConfig::default()
    }
}

#[tokio::test]
async fn a_503_is_retried_and_the_retry_succeeds() {
    let (base_url, request_count, captured_bodies, sink) = spawn_sink(503, 2);
    let exporter = OtlpExporter::new(ExporterConfig {
        traces_endpoint: format!("{base_url}/v1/traces"),
        metrics_endpoint: format!("{base_url}/v1/metrics"),
        ..fast_retry_config()
    })
    .expect("build the exporter against the local sink");

    exporter.push_span(
        TraceSpan::new(
            "4bf92f3577b34da6a3ce929d0e0e4736",
            "00f067aa0ba902b7",
            "captured-span",
            HlcTimestamp::EPOCH,
        )
        .with_end(HlcTimestamp::new(1, 0)),
    );
    exporter.flush_once().await;

    sink.join().expect("sink thread must not panic");

    assert_eq!(
        request_count.load(Ordering::SeqCst),
        2,
        "one 503, then one retry that succeeds"
    );
    assert_eq!(
        exporter.export_failures_total(),
        0,
        "the retry succeeded, so this is not a failure"
    );
    assert_eq!(
        exporter.queued_spans(),
        0,
        "the batch was sent (successfully, on the retry)"
    );

    let bodies = captured_bodies.lock().expect("no poisoning");
    assert_eq!(bodies.len(), 2, "the sink actually received two requests");
    for body in bodies.iter() {
        let text = String::from_utf8_lossy(body);
        assert!(
            text.contains("captured-span"),
            "both attempts carried the same span payload"
        );
        assert!(
            text.contains("resourceSpans"),
            "the body is genuinely OTLP-shaped JSON"
        );
    }
}

#[tokio::test]
async fn retries_are_exhausted_and_reported_as_a_failure() {
    // `max_retries: 0`, so the *first* 503 already exhausts retries.
    let (base_url, request_count, _bodies, sink) = spawn_sink(503, 1);
    let exporter = OtlpExporter::new(ExporterConfig {
        traces_endpoint: format!("{base_url}/v1/traces"),
        metrics_endpoint: format!("{base_url}/v1/metrics"),
        retry: RetryConfig::none(),
        connect_timeout: Duration::from_secs(2),
        read_timeout: Duration::from_secs(2),
        ..ExporterConfig::default()
    })
    .expect("build the exporter");

    exporter.push_span(
        TraceSpan::new("t", "s", "n", HlcTimestamp::EPOCH).with_end(HlcTimestamp::new(1, 0)),
    );
    exporter.flush_once().await;

    sink.join().expect("sink thread must not panic");
    assert_eq!(request_count.load(Ordering::SeqCst), 1);
    assert_eq!(exporter.export_failures_total(), 1);
    assert_eq!(
        exporter.queued_spans(),
        0,
        "the failed batch is not requeued -- telemetry is best-effort"
    );
}

#[tokio::test]
async fn metrics_batches_also_flow_through_the_sink() {
    let (base_url, request_count, captured_bodies, sink) = spawn_sink(200, 1);
    let exporter = OtlpExporter::new(ExporterConfig {
        traces_endpoint: format!("{base_url}/v1/traces"),
        metrics_endpoint: format!("{base_url}/v1/metrics"),
        ..fast_retry_config()
    })
    .expect("build the exporter");

    exporter.push_metrics(MetricBatch::new(HlcTimestamp::EPOCH, "test_scope"));
    exporter.flush_once().await;

    sink.join().expect("sink thread must not panic");
    assert_eq!(request_count.load(Ordering::SeqCst), 1);
    assert_eq!(exporter.export_failures_total(), 0);

    let bodies = captured_bodies.lock().expect("no poisoning");
    let text = String::from_utf8_lossy(&bodies[0]);
    assert!(text.contains("resourceMetrics"));
}

#[tokio::test]
async fn flushing_a_larger_queue_than_one_batch_sends_multiple_requests() {
    let (base_url, request_count, _bodies, sink) = spawn_sink(200, 3);
    let exporter = OtlpExporter::new(ExporterConfig {
        traces_endpoint: format!("{base_url}/v1/traces"),
        metrics_endpoint: format!("{base_url}/v1/metrics"),
        span_batch_size: 1, // force one span per request
        ..fast_retry_config()
    })
    .expect("build the exporter");

    for i in 0..3u64 {
        exporter.push_span(
            TraceSpan::new("t", "s", "n", HlcTimestamp::new(i, 0))
                .with_end(HlcTimestamp::new(i + 1, 0)),
        );
    }
    exporter.flush_once().await;

    sink.join().expect("sink thread must not panic");
    assert_eq!(
        request_count.load(Ordering::SeqCst),
        3,
        "one request per span, batch size 1"
    );
    assert_eq!(exporter.export_failures_total(), 0);
    assert_eq!(exporter.queued_spans(), 0);
}
