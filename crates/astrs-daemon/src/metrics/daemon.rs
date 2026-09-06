//! [`DaemonMetrics`] — the resolved metric handles the daemon touches.
//!
//! One struct, built once, holding an `Arc` per series. Everything on a hot
//! path goes through a `&self` method here rather than through the registry,
//! because resolving a family by name takes a read lock and a hash lookup and
//! the publish path can run tens of thousands of times a second.
//!
//! # Ownership
//!
//! The daemon owns its [`DaemonMetrics`], and the [`DaemonMetrics`] owns an
//! `Arc<MetricRegistry>`. An embedder that already has a registry — the CLI's
//! `astrs run`, which also exports its own — hands it in with
//! [`DaemonMetrics::with_registry`] so both sets of series snapshot together.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::metrics::DaemonMetrics;
//!
//! let metrics = DaemonMetrics::new();
//! metrics.record_restart();
//! metrics.record_restart();
//! metrics.record_queue_drops(5);
//!
//! let stats = metrics.ft_stats();
//! assert_eq!(stats.restarts, 2);
//! assert_eq!(stats.queue_drops, 5);
//! ```

use std::sync::Arc;

use astrs_telemetry::metrics::{Counter, Gauge, MetricFamily, MetricRegistry};
use astrs_time::HlcTimestamp;
use astrs_wire::{DaemonStats, DurationMs, MetricBatch};

/// The instrumentation scope every daemon series is attributed to (§13).
pub const SCOPE: &str = "astrs_daemon";

/// How many distinct plane labels the routes gauge may hold.
///
/// Three planes exist ([`crate::state::DeliveryPlane`]), and the family's
/// overflow bucket absorbs anything a future variant adds without unbounded
/// growth — the bounded-cardinality contract `astrs-telemetry` enforces.
pub const PLANE_CARDINALITY: u32 = 8;

/// The label keys of the routes gauge family.
const PLANE_LABELS: &[&str] = &["plane"];

/// The fault-tolerance summary the heartbeat carries (§12).
///
/// A snapshot of the counters an operator watches to answer "is this machine
/// coping?" without scraping the whole registry: it rides in every daemon
/// heartbeat, so the coordinator can render `astrs top` from the control
/// plane alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FtStats {
    /// Supervised respawns since start (§12).
    pub restarts: u64,
    /// Nodes failed for missing a liveness deadline.
    pub health_timeouts: u64,
    /// Messages a full queue discarded (§11.2).
    pub queue_drops: u64,
    /// Times the shared-memory plane fell back to the reliable path (§6.2).
    pub shm_fallbacks: u64,
    /// Routes moved onto the shared-memory plane (§6.3).
    pub route_upgrades: u64,
    /// Routes moved back off it.
    pub route_downgrades: u64,
}

impl FtStats {
    /// Whether anything worth an operator's attention has happened.
    ///
    /// A route upgrade is *not* trouble — it is the system working — so it is
    /// deliberately excluded; a downgrade is, because it means a consumer went
    /// away or a pool ran dry.
    #[must_use]
    pub const fn has_incidents(&self) -> bool {
        self.restarts > 0
            || self.health_timeouts > 0
            || self.queue_drops > 0
            || self.shm_fallbacks > 0
            || self.route_downgrades > 0
    }

    /// A one-line summary for a log record.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "restarts={} health_timeouts={} queue_drops={} shm_fallbacks={} upgrades={} downgrades={}",
            self.restarts,
            self.health_timeouts,
            self.queue_drops,
            self.shm_fallbacks,
            self.route_upgrades,
            self.route_downgrades,
        )
    }
}

/// Every daemon-level series, resolved once.
#[derive(Debug, Clone)]
pub struct DaemonMetrics {
    /// The registry the series live in.
    registry: Arc<MetricRegistry>,
    /// Routes by plane.
    routes: Arc<MetricFamily<Gauge>>,
    /// §6.2 pool-exhaustion fallbacks.
    shm_fallback_total: Arc<Counter>,
    /// Supervised respawns.
    restarts_total: Arc<Counter>,
    /// Queue evictions.
    queue_drops_total: Arc<Counter>,
    /// Slow-start upgrades.
    route_upgrades_total: Arc<Counter>,
    /// Slow-start downgrades.
    route_downgrades_total: Arc<Counter>,
    /// Liveness failures.
    health_timeouts_total: Arc<Counter>,
    /// Peer frames out.
    peer_frames_sent_total: Arc<Counter>,
    /// Peer frames in.
    peer_frames_received_total: Arc<Counter>,
    /// Peer payload bytes out.
    peer_bytes_sent_total: Arc<Counter>,
    /// Peer payload bytes in.
    peer_bytes_received_total: Arc<Counter>,
    /// Tapped messages.
    tap_messages_total: Arc<Counter>,
    /// Tapped messages lost.
    tap_dropped_total: Arc<Counter>,
    /// Heartbeats emitted.
    heartbeats_total: Arc<Counter>,
    /// Coordinator registrations completed (§4.2).
    coordinator_connects_total: Arc<Counter>,
    /// Coordinator links lost (§12).
    coordinator_losses_total: Arc<Counter>,
    /// Metric batches sampled.
    metric_samples_total: Arc<Counter>,
    /// UDS connections refused for unacceptable peer credentials (§16).
    uds_credential_rejections_total: Arc<Counter>,
    /// Current wheel-wide p99 timer jitter (§11.1).
    timer_jitter_p99_us: Arc<Gauge>,
    /// `cpu_affinity` requests this platform could not honor (§11.3).
    cpu_affinity_unsupported_total: Arc<Counter>,
    /// Per-input deadline (§11.3) budget violations.
    deadline_violations_total: Arc<Counter>,
    /// `rt` reservations successfully applied (§11.3, §22).
    rt_applied_total: Arc<Counter>,
    /// `rt` reservations this platform could not honor (§11.3, §22).
    rt_unsupported_total: Arc<Counter>,
    /// Live nodes.
    nodes_running: Arc<Gauge>,
    /// Connected peers.
    peers_connected: Arc<Gauge>,
    /// Open segments.
    segments_open: Arc<Gauge>,
    /// Hosted dataflows.
    dataflows: Arc<Gauge>,
}

impl DaemonMetrics {
    /// Registers every series into a fresh registry.
    #[must_use]
    pub fn new() -> Self {
        Self::with_registry(Arc::new(MetricRegistry::new()))
    }

    /// Registers every series into `registry`.
    ///
    /// Registration is idempotent by name, so two daemons sharing one registry
    /// (the embedded `astrs run` case, where a second graph is started in the
    /// same process) resolve the same handles rather than shadowing each
    /// other.
    #[must_use]
    pub fn with_registry(registry: Arc<MetricRegistry>) -> Self {
        let routes =
            registry.register_gauge_family(super::names::ROUTES, PLANE_LABELS, PLANE_CARDINALITY);
        Self {
            shm_fallback_total: registry.register_counter(super::names::SHM_FALLBACK_TOTAL),
            restarts_total: registry.register_counter(super::names::RESTARTS_TOTAL),
            queue_drops_total: registry.register_counter(super::names::QUEUE_DROPS_TOTAL),
            route_upgrades_total: registry.register_counter(super::names::ROUTE_UPGRADES_TOTAL),
            route_downgrades_total: registry.register_counter(super::names::ROUTE_DOWNGRADES_TOTAL),
            health_timeouts_total: registry.register_counter(super::names::HEALTH_TIMEOUTS_TOTAL),
            peer_frames_sent_total: registry.register_counter(super::names::PEER_FRAMES_SENT_TOTAL),
            peer_frames_received_total: registry
                .register_counter(super::names::PEER_FRAMES_RECEIVED_TOTAL),
            peer_bytes_sent_total: registry.register_counter(super::names::PEER_BYTES_SENT_TOTAL),
            peer_bytes_received_total: registry
                .register_counter(super::names::PEER_BYTES_RECEIVED_TOTAL),
            tap_messages_total: registry.register_counter(super::names::TAP_MESSAGES_TOTAL),
            tap_dropped_total: registry.register_counter(super::names::TAP_DROPPED_TOTAL),
            heartbeats_total: registry.register_counter(super::names::HEARTBEATS_TOTAL),
            coordinator_connects_total: registry
                .register_counter(super::names::COORDINATOR_CONNECTS_TOTAL),
            coordinator_losses_total: registry
                .register_counter(super::names::COORDINATOR_LOSSES_TOTAL),
            metric_samples_total: registry.register_counter(super::names::METRIC_SAMPLES_TOTAL),
            uds_credential_rejections_total: registry
                .register_counter(super::names::UDS_CREDENTIAL_REJECTIONS_TOTAL),
            timer_jitter_p99_us: registry.register_gauge(super::names::TIMER_JITTER_P99_US),
            cpu_affinity_unsupported_total: registry
                .register_counter(super::names::CPU_AFFINITY_UNSUPPORTED_TOTAL),
            deadline_violations_total: registry
                .register_counter(super::names::DEADLINE_VIOLATIONS_TOTAL),
            rt_applied_total: registry.register_counter(super::names::RT_APPLIED_TOTAL),
            rt_unsupported_total: registry.register_counter(super::names::RT_UNSUPPORTED_TOTAL),
            nodes_running: registry.register_gauge(super::names::NODES_RUNNING),
            peers_connected: registry.register_gauge(super::names::PEERS_CONNECTED),
            segments_open: registry.register_gauge(super::names::SEGMENTS_OPEN),
            dataflows: registry.register_gauge(super::names::DATAFLOWS),
            routes,
            registry,
        }
    }

    /// The registry the series live in, for an exporter.
    #[must_use]
    pub fn registry(&self) -> &Arc<MetricRegistry> {
        &self.registry
    }

    /// Snapshots every series into one batch, stamped at `timestamp`.
    #[must_use]
    pub fn snapshot(&self, timestamp: HlcTimestamp) -> MetricBatch {
        self.registry.snapshot(timestamp, SCOPE)
    }

    /// Records one shared-memory fallback (§6.2).
    pub fn record_shm_fallback(&self) {
        self.shm_fallback_total.inc();
    }

    /// Records `count` shared-memory fallbacks at once.
    pub fn record_shm_fallbacks(&self, count: u64) {
        self.shm_fallback_total.inc_by(count);
    }

    /// Records one supervised respawn (§12).
    pub fn record_restart(&self) {
        self.restarts_total.inc();
    }

    /// Records `count` messages a full queue discarded (§11.2).
    pub fn record_queue_drops(&self, count: u64) {
        if count > 0 {
            self.queue_drops_total.inc_by(count);
        }
    }

    /// Records one route moving onto the shared-memory plane (§6.3).
    pub fn record_route_upgrade(&self) {
        self.route_upgrades_total.inc();
    }

    /// Records one route moving back off it.
    pub fn record_route_downgrade(&self) {
        self.route_downgrades_total.inc();
    }

    /// Records one node failed for a missed liveness deadline (§12).
    pub fn record_health_timeout(&self) {
        self.health_timeouts_total.inc();
    }

    /// Records one frame written to a peer, with its payload size (§6.4).
    pub fn record_peer_sent(&self, payload_bytes: u64) {
        self.peer_frames_sent_total.inc();
        self.peer_bytes_sent_total.inc_by(payload_bytes);
    }

    /// Records one frame read from a peer, with its payload size (§6.4).
    pub fn record_peer_received(&self, payload_bytes: u64) {
        self.peer_frames_received_total.inc();
        self.peer_bytes_received_total.inc_by(payload_bytes);
    }

    /// Records one message copied to a debug tap (§13).
    pub fn record_tap_message(&self) {
        self.tap_messages_total.inc();
    }

    /// Records `count` tapped messages lost to a slow subscriber.
    pub fn record_tap_dropped(&self, count: u64) {
        if count > 0 {
            self.tap_dropped_total.inc_by(count);
        }
    }

    /// Records one heartbeat emitted to the coordinator leg (§12).
    pub fn record_heartbeat(&self) {
        self.heartbeats_total.inc();
    }

    /// Records one completed coordinator registration (§4.2).
    pub fn record_coordinator_connected(&self) {
        self.coordinator_connects_total.inc();
    }

    /// Records one coordinator link loss — the entry into
    /// degraded-autonomous mode (§12).
    pub fn record_coordinator_lost(&self) {
        self.coordinator_losses_total.inc();
    }

    /// Records one node-metrics sampling round (§13).
    pub fn record_metric_sample(&self) {
        self.metric_samples_total.inc();
    }

    /// Records one UDS connection refused because its kernel-reported peer
    /// credentials were neither the daemon's own uid nor root (§16).
    pub fn record_uds_credential_rejection(&self) {
        self.uds_credential_rejections_total.inc();
    }

    /// Records one `cpu_affinity` request this platform had no API to honor
    /// (§11.3) — the counter half of [`crate::spawn::CpuAffinityOutcome::UnsupportedPlatform`]'s
    /// WARN log.
    pub fn record_cpu_affinity_unsupported(&self) {
        self.cpu_affinity_unsupported_total.inc();
    }

    /// Records one per-input deadline (§11.3) budget violation, reported by a
    /// node's own [`astrs_scheduler::DeadlineMonitor`].
    pub fn record_deadline_violation(&self) {
        self.deadline_violations_total.inc();
    }

    /// Records one `rt` reservation successfully applied (§11.3, §22).
    pub fn record_rt_applied(&self) {
        self.rt_applied_total.inc();
    }

    /// Records one `rt` reservation this platform had no API to honor
    /// (§11.3, §22) — the counter half of
    /// [`crate::spawn::RtOutcome::UnsupportedPlatform`]'s WARN log.
    pub fn record_rt_unsupported(&self) {
        self.rt_unsupported_total.inc();
    }

    /// Publishes the shared timer wheel's current p99 jitter reading, in
    /// microseconds (§11.1).
    pub fn set_timer_jitter_p99_us(&self, micros: u64) {
        self.timer_jitter_p99_us.set(micros as f64);
    }

    /// Publishes how many routes are on `plane` right now.
    pub fn set_routes_on_plane(&self, plane: &str, count: u64) {
        self.routes.get_or_create(&[plane]).set(count as f64);
    }

    /// Publishes how many nodes are live.
    pub fn set_nodes_running(&self, count: u64) {
        self.nodes_running.set(count as f64);
    }

    /// Publishes how many peers are connected.
    pub fn set_peers_connected(&self, count: u64) {
        self.peers_connected.set(count as f64);
    }

    /// Publishes how many segments the daemon brokers.
    pub fn set_segments_open(&self, count: u64) {
        self.segments_open.set(count as f64);
    }

    /// Publishes how many dataflows the daemon hosts.
    pub fn set_dataflows(&self, count: u64) {
        self.dataflows.set(count as f64);
    }

    /// The fault-tolerance summary the heartbeat carries (§12).
    #[must_use]
    pub fn ft_stats(&self) -> FtStats {
        FtStats {
            restarts: self.restarts_total.get(),
            health_timeouts: self.health_timeouts_total.get(),
            queue_drops: self.queue_drops_total.get(),
            shm_fallbacks: self.shm_fallback_total.get(),
            route_upgrades: self.route_upgrades_total.get(),
            route_downgrades: self.route_downgrades_total.get(),
        }
    }

    /// The load figures `astrs top` displays (§13).
    ///
    /// `cpu_percent` and `rss_bytes` are the daemon's *own* process readings,
    /// supplied by the caller because sampling them is a syscall the metrics
    /// struct has no business making on a `&self` accessor.
    #[must_use]
    pub fn daemon_stats(
        &self,
        uptime: DurationMs,
        node_count: u32,
        dataflow_count: u32,
        cpu_percent: f32,
        rss_bytes: u64,
        shm_bytes_mapped: u64,
    ) -> DaemonStats {
        DaemonStats {
            uptime,
            node_count,
            dataflow_count,
            cpu_percent,
            rss_bytes,
            shm_bytes_mapped,
            shm_fallback_total: self.shm_fallback_total.get(),
            frames_sent: self.peer_frames_sent_total.get(),
            frames_received: self.peer_frames_received_total.get(),
            bytes_sent: self.peer_bytes_sent_total.get(),
            bytes_received: self.peer_bytes_received_total.get(),
        }
    }
}

impl Default for DaemonMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::super::names;
    use super::*;

    fn value(batch: &MetricBatch, name: &str) -> Option<f64> {
        batch
            .points
            .iter()
            .find(|point| point.name == name)
            .map(|point| point.value.as_f64())
    }

    #[test]
    fn a_fresh_registry_reports_every_series_at_zero() {
        let metrics = DaemonMetrics::new();
        let batch = metrics.snapshot(HlcTimestamp::new(1, 0));
        for name in names::ALL {
            if *name == names::ROUTES {
                // A labelled family has no series until a label value is used.
                continue;
            }
            assert_eq!(value(&batch, name), Some(0.0), "{name}");
        }
    }

    #[test]
    fn counters_accumulate() {
        let metrics = DaemonMetrics::new();
        metrics.record_shm_fallback();
        metrics.record_shm_fallbacks(4);
        metrics.record_restart();
        metrics.record_queue_drops(3);
        metrics.record_route_upgrade();
        metrics.record_route_downgrade();
        metrics.record_health_timeout();
        metrics.record_heartbeat();
        metrics.record_metric_sample();
        metrics.record_tap_message();
        metrics.record_tap_dropped(2);
        metrics.record_peer_sent(128);
        metrics.record_peer_received(64);
        metrics.record_uds_credential_rejection();
        metrics.record_uds_credential_rejection();

        let batch = metrics.snapshot(HlcTimestamp::new(2, 0));
        assert_eq!(value(&batch, names::SHM_FALLBACK_TOTAL), Some(5.0));
        assert_eq!(
            value(&batch, names::UDS_CREDENTIAL_REJECTIONS_TOTAL),
            Some(2.0)
        );
        assert_eq!(value(&batch, names::RESTARTS_TOTAL), Some(1.0));
        assert_eq!(value(&batch, names::QUEUE_DROPS_TOTAL), Some(3.0));
        assert_eq!(value(&batch, names::ROUTE_UPGRADES_TOTAL), Some(1.0));
        assert_eq!(value(&batch, names::ROUTE_DOWNGRADES_TOTAL), Some(1.0));
        assert_eq!(value(&batch, names::HEALTH_TIMEOUTS_TOTAL), Some(1.0));
        assert_eq!(value(&batch, names::HEARTBEATS_TOTAL), Some(1.0));
        assert_eq!(value(&batch, names::METRIC_SAMPLES_TOTAL), Some(1.0));
        assert_eq!(value(&batch, names::TAP_MESSAGES_TOTAL), Some(1.0));
        assert_eq!(value(&batch, names::TAP_DROPPED_TOTAL), Some(2.0));
        assert_eq!(value(&batch, names::PEER_BYTES_SENT_TOTAL), Some(128.0));
        assert_eq!(value(&batch, names::PEER_BYTES_RECEIVED_TOTAL), Some(64.0));
    }

    #[test]
    fn cpu_affinity_and_deadline_counters_accumulate() {
        let metrics = DaemonMetrics::new();
        metrics.record_cpu_affinity_unsupported();
        metrics.record_cpu_affinity_unsupported();
        metrics.record_deadline_violation();

        let batch = metrics.snapshot(HlcTimestamp::new(5, 0));
        assert_eq!(
            value(&batch, names::CPU_AFFINITY_UNSUPPORTED_TOTAL),
            Some(2.0)
        );
        assert_eq!(value(&batch, names::DEADLINE_VIOLATIONS_TOTAL), Some(1.0));
    }

    #[test]
    fn rt_counters_accumulate_independently() {
        let metrics = DaemonMetrics::new();
        metrics.record_rt_applied();
        metrics.record_rt_applied();
        metrics.record_rt_applied();
        metrics.record_rt_unsupported();

        let batch = metrics.snapshot(HlcTimestamp::new(6, 0));
        assert_eq!(value(&batch, names::RT_APPLIED_TOTAL), Some(3.0));
        assert_eq!(value(&batch, names::RT_UNSUPPORTED_TOTAL), Some(1.0));
    }

    #[test]
    fn zero_sized_batches_do_not_touch_their_counter() {
        let metrics = DaemonMetrics::new();
        metrics.record_queue_drops(0);
        metrics.record_tap_dropped(0);
        assert_eq!(metrics.ft_stats().queue_drops, 0);
    }

    #[test]
    fn the_routes_gauge_is_labelled_by_plane() {
        let metrics = DaemonMetrics::new();
        metrics.set_routes_on_plane("local", 2);
        metrics.set_routes_on_plane("shm", 5);
        metrics.set_routes_on_plane("remote", 1);

        let batch = metrics.snapshot(HlcTimestamp::new(3, 0));
        let planes: Vec<(&str, f64)> = batch
            .points
            .iter()
            .filter(|point| point.name == names::ROUTES)
            .filter_map(|point| {
                point
                    .label("plane")
                    .map(|plane| (plane, point.value.as_f64()))
            })
            .collect();
        assert_eq!(planes.len(), 3);
        assert!(planes.contains(&("shm", 5.0)));
        assert!(planes.contains(&("local", 2.0)));
    }

    #[test]
    fn gauges_replace_rather_than_accumulate() {
        let metrics = DaemonMetrics::new();
        metrics.set_nodes_running(4);
        metrics.set_nodes_running(2);
        metrics.set_peers_connected(1);
        metrics.set_segments_open(3);
        metrics.set_dataflows(1);
        metrics.set_timer_jitter_p99_us(500);
        metrics.set_timer_jitter_p99_us(120);

        let batch = metrics.snapshot(HlcTimestamp::new(4, 0));
        assert_eq!(value(&batch, names::NODES_RUNNING), Some(2.0));
        assert_eq!(value(&batch, names::PEERS_CONNECTED), Some(1.0));
        assert_eq!(value(&batch, names::SEGMENTS_OPEN), Some(3.0));
        assert_eq!(value(&batch, names::DATAFLOWS), Some(1.0));
        assert_eq!(value(&batch, names::TIMER_JITTER_P99_US), Some(120.0));
    }

    #[test]
    fn the_ft_summary_reads_the_same_counters() {
        let metrics = DaemonMetrics::new();
        assert!(!metrics.ft_stats().has_incidents());
        metrics.record_restart();
        metrics.record_route_upgrade();

        let stats = metrics.ft_stats();
        assert_eq!(stats.restarts, 1);
        assert_eq!(stats.route_upgrades, 1);
        assert!(stats.has_incidents());
        assert!(
            stats.summary().contains("restarts=1"),
            "{}",
            stats.summary()
        );
    }

    #[test]
    fn an_upgrade_alone_is_not_an_incident() {
        let metrics = DaemonMetrics::new();
        metrics.record_route_upgrade();
        assert!(!metrics.ft_stats().has_incidents());
        metrics.record_route_downgrade();
        assert!(metrics.ft_stats().has_incidents());
    }

    #[test]
    fn daemon_stats_report_the_shared_fallback_counter() {
        let metrics = DaemonMetrics::new();
        metrics.record_shm_fallbacks(7);
        metrics.record_peer_sent(10);
        let stats = metrics.daemon_stats(DurationMs::from_secs(3), 2, 1, 1.5, 4096, 8192);

        assert_eq!(stats.shm_fallback_total, 7);
        assert!(stats.has_shm_fallbacks());
        assert_eq!(stats.frames_sent, 1);
        assert_eq!(stats.bytes_sent, 10);
        assert_eq!(stats.node_count, 2);
        assert_eq!(stats.shm_bytes_mapped, 8192);
    }

    #[test]
    fn two_views_of_one_registry_share_their_counters() {
        let registry = Arc::new(MetricRegistry::new());
        let first = DaemonMetrics::with_registry(Arc::clone(&registry));
        let second = DaemonMetrics::with_registry(registry);
        first.record_restart();
        second.record_restart();
        assert_eq!(first.ft_stats().restarts, 2);
    }

    #[test]
    fn a_cloned_handle_shares_state() {
        let metrics = DaemonMetrics::new();
        let clone = metrics.clone();
        clone.record_health_timeout();
        assert_eq!(metrics.ft_stats().health_timeouts, 1);
    }

    #[test]
    fn the_default_is_a_fresh_registry() {
        let metrics = DaemonMetrics::default();
        assert_eq!(metrics.ft_stats(), FtStats::default());
    }
}
