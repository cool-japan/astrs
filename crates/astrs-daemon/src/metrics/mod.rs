//! Daemon-level counters and gauges (§13).
//!
//! > *Metrics: `astrs-telemetry` in-process registry (counters/gauges/
//! > histograms, fixed-bucket, allocation-free hot path).*
//!
//! Every number the daemon publishes about itself lives here, registered once
//! into an [`astrs_telemetry::metrics::MetricRegistry`] and thereafter touched
//! through pre-resolved handles: a `Counter::inc()` on the publish path is one
//! relaxed atomic add, with no map lookup, no string formatting, and no
//! allocation. That property is why the handles are resolved eagerly in
//! [`DaemonMetrics::new`] rather than looked up where they are used.
//!
//! | Module | Concern |
//! |---|---|
//! | [`names`] | The metric names, as constants, so a test can assert presence without a string literal |
//! | [`daemon`] | [`DaemonMetrics`]: the resolved handles and the summary views built from them |
//!
//! # What is measured
//!
//! | Metric | Kind | Blueprint |
//! |---|---|---|
//! | [`names::ROUTES`] (`plane` label) | gauge family | §6.3 — how many routes on each plane right now |
//! | [`names::SHM_FALLBACK_TOTAL`] | counter | §6.2 — "never sleep-retry; fall back … and increment a visible `shm_fallback_total`" |
//! | [`names::RESTARTS_TOTAL`] | counter | §12 — supervised respawns |
//! | [`names::QUEUE_DROPS_TOTAL`] | counter | §11.2 — messages evicted by a `drop_oldest` queue |
//! | [`names::ROUTE_UPGRADES_TOTAL`] / [`names::ROUTE_DOWNGRADES_TOTAL`] | counters | §6.3 — slow-start transitions |
//! | [`names::HEALTH_TIMEOUTS_TOTAL`] | counter | §12 — nodes that stopped answering |
//! | [`names::PEER_FRAMES_SENT_TOTAL`] / [`names::PEER_FRAMES_RECEIVED_TOTAL`] | counters | §6.4 — cross-host traffic |
//! | [`names::TAP_MESSAGES_TOTAL`] / [`names::TAP_DROPPED_TOTAL`] | counters | §13 — `astrs topic echo` |
//! | [`names::NODES_RUNNING`], [`names::PEERS_CONNECTED`], [`names::SEGMENTS_OPEN`] | gauges | §13 — `astrs top` |
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::metrics::{DaemonMetrics, names};
//! use astrs_time::HlcTimestamp;
//!
//! let metrics = DaemonMetrics::new();
//! metrics.record_shm_fallback();
//! metrics.set_routes_on_plane("shm", 3);
//!
//! let batch = metrics.snapshot(HlcTimestamp::new(1, 0));
//! assert!(batch.points.iter().any(|point| point.name == names::SHM_FALLBACK_TOTAL));
//! assert!(
//!     batch
//!         .points
//!         .iter()
//!         .any(|point| point.name == names::ROUTES && point.label("plane") == Some("shm"))
//! );
//! ```

pub mod daemon;
pub mod names;

pub use daemon::{DaemonMetrics, FtStats, PLANE_CARDINALITY, SCOPE};
