//! `tracing` subscriber setup (blueprint §13): env-filter directives,
//! `astrs-log`-backed human/JSON formatting, per-process
//! `service.name`/node attributes, and (via [`init_telemetry_with_spans`])
//! span-context collection for OTLP export.
//!
//! | Module | Contents |
//! |---|---|
//! | [`fmt_layer`] | [`AstrsFmtLayer`] — event formatting through `astrs-log` |
//! | [`span_layer`] | [`AstrsSpanLayer`] — finished-span collection into [`astrs_wire::TraceSpan`] |
//!
//! # Examples
//!
//! A real process calls [`init_telemetry`] (or
//! [`init_telemetry_with_spans`]) exactly once, near the top of `main`;
//! see that function's own docs for a full, running example.
//!
//! ```
//! use astrs_telemetry::subscriber::TelemetryConfig;
//!
//! let config = TelemetryConfig {
//!     filter_directives: "debug".to_owned(),
//!     ..TelemetryConfig::default()
//! };
//! assert_eq!(config.filter_directives, "debug");
//! ```

pub mod fmt_layer;
pub mod span_layer;
mod visitor;

use std::sync::Arc;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use astrs_time::HlcClock;
use astrs_wire::{DataflowId, NodeId, TraceSpan};

pub use fmt_layer::{AstrsFmtLayer, LogFormat};
pub use span_layer::AstrsSpanLayer;

use crate::error::TelemetryError;

/// Configuration for [`init_telemetry`]/[`init_telemetry_with_spans`].
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// The OTLP `service.name` this process reports (not applied to the
    /// human/JSON log lines directly — those are stamped with `node`
    /// instead, matching `astrs-log`'s own record shape; `service_name`
    /// is for [`crate::otlp::common::Resource`] when this config is
    /// combined with [`crate::export::OtlpExporter`]).
    pub service_name: String,
    /// This process's node identity, if it is a node process. Stamped on
    /// every log line and every span this subscriber produces.
    pub node: Option<NodeId>,
    /// The dataflow this node belongs to, if known. Stamped on every
    /// span (not on log lines — `astrs_log::LogRecord` has no dataflow
    /// field).
    pub dataflow: Option<DataflowId>,
    /// A `RUST_LOG`-style directive string parsed by
    /// `tracing_subscriber::EnvFilter`. Default: `"info"`.
    pub filter_directives: String,
    /// Human or JSON log rendering. Default: [`LogFormat::Human`].
    pub format: LogFormat,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            service_name: "astrs".to_owned(),
            node: None,
            dataflow: None,
            filter_directives: "info".to_owned(),
            format: LogFormat::Human,
        }
    }
}

/// What [`init_telemetry`]/[`init_telemetry_with_spans`] hand back after
/// installing the global subscriber.
#[derive(Debug, Clone)]
pub struct TelemetryHandle {
    /// The HLC clock backing every log line's and span's timestamp — kept
    /// available so a caller wiring up
    /// [`crate::sampler::spawn_periodic_sampling`] or anything else that
    /// wants HLC-stamped output shares the exact same clock state rather
    /// than drifting a second, independent clock.
    pub clock: Arc<HlcClock>,
}

/// Installs the global `tracing` subscriber: `EnvFilter` plus
/// [`AstrsFmtLayer`]. Spans are not collected (equivalent to
/// [`init_telemetry_with_spans`] with a no-op sink) — use that function
/// directly to also feed [`crate::export::OtlpExporter::push_span`] or
/// any other sink.
///
/// # Errors
///
/// [`TelemetryError::InvalidFilter`] if [`TelemetryConfig::filter_directives`]
/// is not valid `EnvFilter` syntax; [`TelemetryError::AlreadyInitialized`]
/// if a global subscriber is already installed.
///
/// # Examples
///
/// ```no_run
/// use astrs_telemetry::subscriber::{TelemetryConfig, init_telemetry};
///
/// init_telemetry(TelemetryConfig::default()).expect("install the subscriber exactly once");
/// tracing::info!("astrs starting up");
/// ```
pub fn init_telemetry(config: TelemetryConfig) -> Result<TelemetryHandle, TelemetryError> {
    init_telemetry_with_spans(config, |_span| {})
}

/// [`init_telemetry`], plus an [`AstrsSpanLayer`] that hands every
/// finished span to `sink` — typically `move |span|
/// exporter.push_span(span)` for an [`crate::export::OtlpExporter`], but
/// `sink` is deliberately a plain closure rather than a concrete
/// exporter type, so this module has no compile-time dependency on
/// `oxihttp`/the export pipeline at all.
///
/// # Errors
///
/// See [`init_telemetry`].
///
/// # Examples
///
/// ```
/// use astrs_telemetry::subscriber::{TelemetryConfig, init_telemetry_with_spans};
/// use std::sync::{Arc, Mutex};
///
/// let collected = Arc::new(Mutex::new(Vec::new()));
/// let sink_target = Arc::clone(&collected);
/// // Each doctest is its own process, so installing the real global
/// // subscriber here is safe (no other doctest contends for the slot).
/// init_telemetry_with_spans(TelemetryConfig::default(), move |span| {
///     sink_target.lock().unwrap().push(span);
/// })
/// .expect("install the subscriber exactly once");
///
/// {
///     let span = tracing::info_span!("publish");
///     let _entered = span.enter();
/// }
/// assert_eq!(collected.lock().unwrap().len(), 1);
/// ```
pub fn init_telemetry_with_spans<F>(
    config: TelemetryConfig,
    sink: F,
) -> Result<TelemetryHandle, TelemetryError>
where
    F: Fn(TraceSpan) + Send + Sync + 'static,
{
    let filter = EnvFilter::try_new(&config.filter_directives).map_err(|error| {
        TelemetryError::InvalidFilter {
            directive: config.filter_directives.clone(),
            message: error.to_string(),
        }
    })?;

    let clock = Arc::new(HlcClock::system());
    let node_string = config.node.as_ref().map(|node| node.as_str().to_owned());
    let fmt_layer = AstrsFmtLayer::new(Arc::clone(&clock), config.format, node_string);
    let span_layer = AstrsSpanLayer::new(Arc::clone(&clock), config.node, config.dataflow, sink);

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(span_layer)
        .with(filter)
        .try_init()
        .map_err(|_| TelemetryError::AlreadyInitialized)?;

    Ok(TelemetryHandle { clock })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn default_config_uses_info_and_human_format() {
        let config = TelemetryConfig::default();
        assert_eq!(config.filter_directives, "info");
        assert_eq!(config.format, LogFormat::Human);
        assert!(config.node.is_none());
    }

    #[test]
    fn an_invalid_filter_directive_is_reported_before_installing_anything() {
        let config = TelemetryConfig {
            // "not_a_real_level" is not one of trace/debug/info/warn/
            // error/off, so `EnvFilter::try_new` must reject this.
            filter_directives: "astrs_daemon=not_a_real_level".to_owned(),
            ..TelemetryConfig::default()
        };
        let err = init_telemetry(config).unwrap_err();
        assert!(matches!(err, TelemetryError::InvalidFilter { .. }));
    }

    #[test]
    fn a_second_installation_in_the_same_process_reports_already_initialized() {
        let first = init_telemetry(TelemetryConfig::default());
        // Some other test earlier in this process (nextest runs each
        // `#[test]` in its own process, but plain `cargo test` shares one
        // per binary) may already have installed a subscriber; either
        // way, the *second* attempt from here on must fail cleanly.
        let _ = first;
        let second = init_telemetry(TelemetryConfig::default());
        assert!(matches!(second, Err(TelemetryError::AlreadyInitialized)));
    }
}
