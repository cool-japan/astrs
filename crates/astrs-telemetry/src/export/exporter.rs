//! [`OtlpExporter`] — batches, retries and ships spans and metric
//! snapshots to an OTLP/HTTP+JSON collector.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use astrs_wire::{MetricBatch, TraceSpan};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::export::backoff::RetryConfig;
use crate::export::client::OtlpClient;
use crate::export::error::ExportError;
use crate::export::queue::BoundedQueue;
use crate::otlp::common::Resource;
use crate::otlp::metrics::build_metrics_request;
use crate::otlp::trace::build_trace_request;

/// The default OTLP/HTTP traces endpoint (blueprint §13).
pub const DEFAULT_TRACES_ENDPOINT: &str = "http://localhost:4318/v1/traces";
/// The default OTLP/HTTP metrics endpoint (blueprint §13).
pub const DEFAULT_METRICS_ENDPOINT: &str = "http://localhost:4318/v1/metrics";

/// Configuration for an [`OtlpExporter`].
#[derive(Debug, Clone)]
pub struct ExporterConfig {
    /// Where to `POST` trace batches. Default: [`DEFAULT_TRACES_ENDPOINT`].
    pub traces_endpoint: String,
    /// Where to `POST` metric batches. Default: [`DEFAULT_METRICS_ENDPOINT`].
    pub metrics_endpoint: String,
    /// Resource attributes attached to every export (e.g.
    /// `("service.name", "astrs-daemon")`).
    pub resource_attributes: Vec<(String, String)>,
    /// How many finished spans the pending-export queue holds before it
    /// starts dropping the oldest ones.
    pub max_queued_spans: usize,
    /// How many metric-snapshot batches the pending-export queue holds
    /// before it starts dropping the oldest ones.
    pub max_queued_metric_batches: usize,
    /// The largest number of spans sent in one `POST /v1/traces` request.
    pub span_batch_size: usize,
    /// How often the background export loop wakes up to flush, absent
    /// an explicit [`OtlpExporter::flush_once`] call.
    pub flush_interval: Duration,
    /// The retry-with-backoff policy applied to each `POST`.
    pub retry: RetryConfig,
    /// The underlying HTTP client's connect timeout. Exposed as config
    /// (rather than hardcoded in [`OtlpClient`]) so a test pointed at an
    /// address that silently black-holes packets — a real router
    /// behavior, not just a test convenience — fails in milliseconds
    /// instead of waiting out a 10-second default.
    pub connect_timeout: Duration,
    /// The underlying HTTP client's read timeout.
    pub read_timeout: Duration,
}

impl Default for ExporterConfig {
    fn default() -> Self {
        Self {
            traces_endpoint: DEFAULT_TRACES_ENDPOINT.to_owned(),
            metrics_endpoint: DEFAULT_METRICS_ENDPOINT.to_owned(),
            resource_attributes: Vec::new(),
            max_queued_spans: 2048,
            max_queued_metric_batches: 64,
            span_batch_size: 256,
            flush_interval: Duration::from_secs(5),
            retry: RetryConfig::default(),
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(10),
        }
    }
}

/// Batches, retries and ships spans and metric snapshots to an
/// OTLP/HTTP+JSON collector (blueprint §13).
///
/// Two independent [`BoundedQueue`]s decouple *producing* telemetry
/// (the tracing span layer calling [`OtlpExporter::push_span`]; anything
/// sampling [`crate::metrics::MetricRegistry`] calling
/// [`OtlpExporter::push_metrics`]) from *sending* it — the queue absorbs
/// bursts and outages, dropping and counting the oldest entry rather than
/// growing without bound or blocking the producer.
///
/// This type does not spawn anything itself: call [`OtlpExporter::flush_once`]
/// directly for full manual control (the shape every test in this
/// module and in `tests/exporter_retry_backoff.rs` uses), or
/// [`OtlpExporter::spawn`] for a self-driving background task with
/// graceful shutdown.
#[derive(Debug)]
pub struct OtlpExporter {
    client: OtlpClient,
    config: ExporterConfig,
    span_queue: BoundedQueue<TraceSpan>,
    metric_queue: BoundedQueue<MetricBatch>,
    export_failures_total: AtomicU64,
    flush_calls_total: AtomicU64,
}

impl OtlpExporter {
    /// Builds an exporter. Does not send anything and does not spawn a
    /// background task.
    ///
    /// # Errors
    ///
    /// [`ExportError::ClientBuild`] if the underlying HTTP client could
    /// not be constructed.
    pub fn new(config: ExporterConfig) -> Result<Self, ExportError> {
        let client =
            OtlpClient::with_timeouts(config.retry, config.connect_timeout, config.read_timeout)?;
        Ok(Self {
            client,
            span_queue: BoundedQueue::new(config.max_queued_spans),
            metric_queue: BoundedQueue::new(config.max_queued_metric_batches),
            config,
            export_failures_total: AtomicU64::new(0),
            flush_calls_total: AtomicU64::new(0),
        })
    }

    /// Queues a finished span for export.
    pub fn push_span(&self, span: TraceSpan) {
        self.span_queue.push(span);
    }

    /// Queues a metric snapshot for export.
    pub fn push_metrics(&self, batch: MetricBatch) {
        self.metric_queue.push(batch);
    }

    /// How many spans have been dropped (queue full) rather than queued.
    #[must_use]
    pub fn dropped_spans_total(&self) -> u64 {
        self.span_queue.dropped_total()
    }

    /// How many metric batches have been dropped (queue full) rather
    /// than queued.
    #[must_use]
    pub fn dropped_metric_batches_total(&self) -> u64 {
        self.metric_queue.dropped_total()
    }

    /// How many `POST` attempts exhausted their retries and gave up.
    #[must_use]
    pub fn export_failures_total(&self) -> u64 {
        self.export_failures_total.load(Ordering::Relaxed)
    }

    /// How many times [`OtlpExporter::flush_once`] has run.
    #[must_use]
    pub fn flush_calls_total(&self) -> u64 {
        self.flush_calls_total.load(Ordering::Relaxed)
    }

    /// The number of spans currently queued, awaiting export.
    #[must_use]
    pub fn queued_spans(&self) -> usize {
        self.span_queue.len()
    }

    /// The number of metric batches currently queued, awaiting export.
    #[must_use]
    pub fn queued_metric_batches(&self) -> usize {
        self.metric_queue.len()
    }

    /// Drains and sends every currently-queued span and metric batch,
    /// retrying each `POST` per the configured [`RetryConfig`].
    ///
    /// A failed export (retries exhausted) is logged via `tracing::warn!`
    /// and counted in [`OtlpExporter::export_failures_total`]; the data
    /// that failed to send is not requeued (telemetry is best-effort by
    /// design — see the blueprint's framing of `astrs-telemetry` as
    /// observability, not the recording/replay durability path that
    /// `astrs-recording` owns instead).
    pub async fn flush_once(&self) {
        self.flush_calls_total.fetch_add(1, Ordering::Relaxed);
        self.flush_spans().await;
        self.flush_metrics().await;
    }

    async fn flush_spans(&self) {
        loop {
            let batch = self.span_queue.drain_up_to(self.config.span_batch_size);
            if batch.is_empty() {
                return;
            }
            let exhausted = batch.len() < self.config.span_batch_size;
            let request = build_trace_request(&batch, self.resource());
            if let Err(error) = self
                .client
                .post_json(&self.config.traces_endpoint, &request)
                .await
            {
                tracing::warn!(%error, spans = batch.len(), "OTLP trace export failed");
                self.export_failures_total.fetch_add(1, Ordering::Relaxed);
            }
            if exhausted {
                return;
            }
        }
    }

    async fn flush_metrics(&self) {
        for batch in self.metric_queue.drain_all() {
            let request = build_metrics_request(&batch, self.resource());
            if let Err(error) = self
                .client
                .post_json(&self.config.metrics_endpoint, &request)
                .await
            {
                tracing::warn!(%error, points = batch.points.len(), "OTLP metrics export failed");
                self.export_failures_total.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn resource(&self) -> Resource {
        Resource::from_attributes(
            self.config
                .resource_attributes
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
        )
    }

    /// Builds the exporter and spawns a background task that calls
    /// [`OtlpExporter::flush_once`] on every [`ExporterConfig::flush_interval`]
    /// tick until [`ExporterHandle::shutdown`] is called, at which point
    /// it flushes one last time before stopping.
    ///
    /// # Errors
    ///
    /// See [`OtlpExporter::new`].
    pub fn spawn(config: ExporterConfig) -> Result<ExporterHandle, ExportError> {
        let flush_interval = config.flush_interval;
        let exporter = Arc::new(Self::new(config)?);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let background = Arc::clone(&exporter);
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(flush_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        background.flush_once().await;
                    }
                    _ = &mut shutdown_rx => {
                        background.flush_once().await;
                        break;
                    }
                }
            }
        });
        Ok(ExporterHandle {
            exporter,
            shutdown_tx: Some(shutdown_tx),
            task: Some(task),
        })
    }
}

/// A running [`OtlpExporter`] plus its background flush task.
///
/// Dropping a handle without calling [`ExporterHandle::shutdown`] still
/// signals the background task to stop (best-effort, via `Drop`), but
/// does not wait for its final flush to finish — call `shutdown` when
/// the caller can afford to await one more flush cycle, which is the
/// normal case for a clean process exit.
#[derive(Debug)]
pub struct ExporterHandle {
    exporter: Arc<OtlpExporter>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl ExporterHandle {
    /// The running exporter, for calling [`OtlpExporter::push_span`],
    /// [`OtlpExporter::push_metrics`], or reading its counters.
    #[must_use]
    pub fn exporter(&self) -> &Arc<OtlpExporter> {
        &self.exporter
    }

    /// Signals the background task to stop, waits for its final flush to
    /// complete, and returns the exporter (whose counters remain
    /// readable afterward).
    pub async fn shutdown(mut self) -> Arc<OtlpExporter> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        Arc::clone(&self.exporter)
    }
}

impl Drop for ExporterHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_time::HlcTimestamp;

    fn config_to(endpoint: &str) -> ExporterConfig {
        ExporterConfig {
            traces_endpoint: format!("{endpoint}/v1/traces"),
            metrics_endpoint: format!("{endpoint}/v1/metrics"),
            retry: RetryConfig::none(),
            connect_timeout: Duration::from_millis(200),
            read_timeout: Duration::from_millis(200),
            ..ExporterConfig::default()
        }
    }

    #[test]
    fn default_endpoints_match_the_blueprint() {
        let config = ExporterConfig::default();
        assert_eq!(config.traces_endpoint, "http://localhost:4318/v1/traces");
        assert_eq!(config.metrics_endpoint, "http://localhost:4318/v1/metrics");
    }

    #[test]
    fn pushing_spans_and_metrics_is_visible_before_any_flush() {
        let exporter = OtlpExporter::new(config_to("http://127.0.0.1:1")).unwrap();
        exporter.push_span(
            TraceSpan::new("t", "s", "n", HlcTimestamp::EPOCH).with_end(HlcTimestamp::new(1, 0)),
        );
        exporter.push_metrics(MetricBatch::new(HlcTimestamp::EPOCH, "s"));
        assert_eq!(exporter.queued_spans(), 1);
        assert_eq!(exporter.queued_metric_batches(), 1);
        assert_eq!(exporter.flush_calls_total(), 0);
    }

    #[tokio::test]
    async fn flush_against_an_unreachable_collector_counts_a_failure_and_drains_the_queue() {
        // TEST-NET-1 (RFC 5737): reserved, never routes.
        let exporter = OtlpExporter::new(ExporterConfig {
            retry: RetryConfig::none(),
            ..config_to("http://192.0.2.1:4318")
        })
        .unwrap();
        exporter.push_span(
            TraceSpan::new("t", "s", "n", HlcTimestamp::EPOCH).with_end(HlcTimestamp::new(1, 0)),
        );
        exporter.flush_once().await;
        assert_eq!(
            exporter.queued_spans(),
            0,
            "the batch was drained even though it failed"
        );
        assert_eq!(exporter.export_failures_total(), 1);
        assert_eq!(exporter.flush_calls_total(), 1);
    }

    #[tokio::test]
    async fn flushing_an_empty_exporter_does_nothing_and_still_counts_the_call() {
        let exporter = OtlpExporter::new(config_to("http://127.0.0.1:1")).unwrap();
        exporter.flush_once().await;
        assert_eq!(exporter.flush_calls_total(), 1);
        assert_eq!(exporter.export_failures_total(), 0);
    }

    #[tokio::test]
    async fn spawn_and_shutdown_flush_and_stop_cleanly() {
        let mut config = config_to("http://192.0.2.1:4318");
        config.flush_interval = Duration::from_secs(3_600); // never ticks during the test
        let handle = OtlpExporter::spawn(config).unwrap();
        handle.exporter().push_span(
            TraceSpan::new("t", "s", "n", HlcTimestamp::EPOCH).with_end(HlcTimestamp::new(1, 0)),
        );
        let exporter = handle.shutdown().await;
        // `shutdown` awaited the background task's own final flush.
        assert_eq!(exporter.flush_calls_total(), 1);
        assert_eq!(exporter.queued_spans(), 0);
    }
}
