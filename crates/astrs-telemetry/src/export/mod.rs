//! The OTLP/HTTP+JSON export pipeline (blueprint §13).
//!
//! | Module | Contents |
//! |---|---|
//! | [`queue`] | [`BoundedQueue`] — the drop-oldest, drop-counted queue backing both signals |
//! | [`backoff`] | [`RetryConfig`] — exponential backoff parameters |
//! | [`client`] | [`OtlpClient`] — one `POST` with retry-with-backoff |
//! | [`exporter`] | [`OtlpExporter`]/[`ExporterHandle`] — batching, queueing, background flush, graceful shutdown |
//! | [`error`] | [`ExportError`] |
//!
//! # Examples
//!
//! ```no_run
//! use astrs_telemetry::export::{ExporterConfig, OtlpExporter};
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let handle = OtlpExporter::spawn(ExporterConfig::default())?;
//! // ... push spans/metric batches from the tracing layer and sampler ...
//! handle.shutdown().await; // flushes once more before returning
//! # Ok(())
//! # }
//! ```

pub mod backoff;
pub mod client;
pub mod error;
pub mod exporter;
pub mod queue;

pub use backoff::RetryConfig;
pub use client::OtlpClient;
pub use error::ExportError;
pub use exporter::{
    DEFAULT_METRICS_ENDPOINT, DEFAULT_TRACES_ENDPOINT, ExporterConfig, ExporterHandle, OtlpExporter,
};
pub use queue::BoundedQueue;
