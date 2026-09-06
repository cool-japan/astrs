//! The daemon's metric names, as constants.
//!
//! A metric name is a contract with the operator's dashboard, not an
//! implementation detail: renaming one breaks every alert that reads it. Every
//! name is therefore written exactly once, here, and referenced from both the
//! registration site and the tests that assert its presence — a rename that
//! forgets a call site stops compiling instead of silently emptying a graph.
//!
//! # Naming rules
//!
//! - `astrs_daemon_` prefix on everything, so a collector scraping several
//!   AstRS processes can attribute a series without a label.
//! - `_total` suffix on monotone counters, per OpenTelemetry/Prometheus
//!   convention; gauges carry no suffix.
//! - Units in the name when the value is not dimensionless (`_bytes`,
//!   `_seconds`).
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::metrics::names;
//!
//! assert!(names::ALL.iter().all(|name| name.starts_with("astrs_daemon_")));
//! assert!(names::SHM_FALLBACK_TOTAL.ends_with("_total"));
//! ```

/// Routes currently established, labelled by plane (`local`, `shm`, `remote`).
pub const ROUTES: &str = "astrs_daemon_routes";

/// Times a message fell back from the shared-memory plane to the reliable
/// daemon path (§6.2 pool-exhaustion policy).
pub const SHM_FALLBACK_TOTAL: &str = "astrs_daemon_shm_fallback_total";

/// Supervised node respawns (§12).
pub const RESTARTS_TOTAL: &str = "astrs_daemon_restarts_total";

/// Messages a full input queue discarded (§11.2).
pub const QUEUE_DROPS_TOTAL: &str = "astrs_daemon_queue_drops_total";

/// Routes moved onto the shared-memory plane (§6.3).
pub const ROUTE_UPGRADES_TOTAL: &str = "astrs_daemon_route_upgrades_total";

/// Routes moved back onto the reliable path (§6.3).
pub const ROUTE_DOWNGRADES_TOTAL: &str = "astrs_daemon_route_downgrades_total";

/// Nodes failed for missing their liveness deadline (§12).
pub const HEALTH_TIMEOUTS_TOTAL: &str = "astrs_daemon_health_timeouts_total";

/// Peer frames written to another daemon (§6.4).
pub const PEER_FRAMES_SENT_TOTAL: &str = "astrs_daemon_peer_frames_sent_total";

/// Peer frames read from another daemon (§6.4).
pub const PEER_FRAMES_RECEIVED_TOTAL: &str = "astrs_daemon_peer_frames_received_total";

/// Payload bytes written to peers (§6.4).
pub const PEER_BYTES_SENT_TOTAL: &str = "astrs_daemon_peer_bytes_sent_total";

/// Payload bytes read from peers (§6.4).
pub const PEER_BYTES_RECEIVED_TOTAL: &str = "astrs_daemon_peer_bytes_received_total";

/// Messages copied to a debug tap (§13).
pub const TAP_MESSAGES_TOTAL: &str = "astrs_daemon_tap_messages_total";

/// Tapped messages dropped because a subscriber could not keep up (§13).
pub const TAP_DROPPED_TOTAL: &str = "astrs_daemon_tap_dropped_total";

/// Heartbeats emitted to the coordinator leg (§12).
pub const HEARTBEATS_TOTAL: &str = "astrs_daemon_heartbeats_total";

/// Node metric batches sampled (§13, every 2 s).
pub const METRIC_SAMPLES_TOTAL: &str = "astrs_daemon_metric_samples_total";

/// Nodes in a live run state right now.
pub const NODES_RUNNING: &str = "astrs_daemon_nodes_running";

/// Peer daemons with an established connection right now.
pub const PEERS_CONNECTED: &str = "astrs_daemon_peers_connected";

/// Shared-memory segments this daemon currently brokers.
pub const SEGMENTS_OPEN: &str = "astrs_daemon_segments_open";

/// Dataflows this daemon currently hosts.
pub const DATAFLOWS: &str = "astrs_daemon_dataflows";

/// Times the coordinator uplink completed a registration (§4.2).
///
/// A counter rather than a gauge because the interesting operational question
/// is *"how often did this daemon have to re-join?"* — a link that flaps
/// twenty times an hour and one that has been up since boot both read as one
/// connected daemon on a gauge.
pub const COORDINATOR_CONNECTS_TOTAL: &str = "astrs_daemon_coordinator_connects_total";

/// Times the coordinator uplink dropped, entering degraded-autonomous mode
/// (§12).
pub const COORDINATOR_LOSSES_TOTAL: &str = "astrs_daemon_coordinator_losses_total";

/// UDS node connections refused because the kernel-reported peer credentials
/// were neither the daemon's own uid nor root (§16).
pub const UDS_CREDENTIAL_REJECTIONS_TOTAL: &str = "astrs_daemon_uds_credential_rejections_total";

/// The 99th-percentile timer jitter the shared wheel is currently measuring
/// for a node's `astrs/timer/*` subscription, in microseconds (§11.1).
///
/// A gauge, not a counter: it reports the P² estimator's *current* reading,
/// not an accumulating count of anything.
pub const TIMER_JITTER_P99_US: &str = "astrs_daemon_timer_jitter_p99_us";

/// Times a node's manifest `cpu_affinity` could not be honored because this
/// platform has no per-process CPU affinity API (§11.3) — the honest-fallback
/// counterpart of the WARN [`crate::spawn::CpuAffinityOutcome::UnsupportedPlatform`]
/// logs once per spawn.
pub const CPU_AFFINITY_UNSUPPORTED_TOTAL: &str = "astrs_daemon_cpu_affinity_unsupported_total";

/// Per-input latency budget violations reported by a node's own
/// [`astrs_scheduler::DeadlineMonitor`] and relayed here (§11.3).
pub const DEADLINE_VIOLATIONS_TOTAL: &str = "astrs_daemon_deadline_violations_total";

/// Times a node's manifest `rt:` reservation was successfully applied — a
/// real-time policy actually put in place before the child's own program
/// ran (§11.3, §22 hard-RT reservations).
pub const RT_APPLIED_TOTAL: &str = "astrs_daemon_rt_applied_total";

/// Times a node's manifest `rt:` reservation could not be honored because
/// this platform (or this Linux architecture) has no `SCHED_FIFO`/`SCHED_RR`
/// application implemented (§11.3, §22) — the honest-fallback counterpart of
/// the WARN [`crate::spawn::RtOutcome::UnsupportedPlatform`] logs once per
/// spawn. Distinct from an `EPERM` refusal, which is a spawn *failure*
/// ([`crate::error::DaemonError::Spawn`]), never counted here.
pub const RT_UNSUPPORTED_TOTAL: &str = "astrs_daemon_rt_unsupported_total";

/// Every name this module defines, for presence assertions and documentation.
pub const ALL: &[&str] = &[
    ROUTES,
    SHM_FALLBACK_TOTAL,
    RESTARTS_TOTAL,
    QUEUE_DROPS_TOTAL,
    ROUTE_UPGRADES_TOTAL,
    ROUTE_DOWNGRADES_TOTAL,
    HEALTH_TIMEOUTS_TOTAL,
    PEER_FRAMES_SENT_TOTAL,
    PEER_FRAMES_RECEIVED_TOTAL,
    PEER_BYTES_SENT_TOTAL,
    PEER_BYTES_RECEIVED_TOTAL,
    TAP_MESSAGES_TOTAL,
    TAP_DROPPED_TOTAL,
    HEARTBEATS_TOTAL,
    METRIC_SAMPLES_TOTAL,
    NODES_RUNNING,
    PEERS_CONNECTED,
    SEGMENTS_OPEN,
    DATAFLOWS,
    COORDINATOR_CONNECTS_TOTAL,
    COORDINATOR_LOSSES_TOTAL,
    UDS_CREDENTIAL_REJECTIONS_TOTAL,
    TIMER_JITTER_P99_US,
    CPU_AFFINITY_UNSUPPORTED_TOTAL,
    DEADLINE_VIOLATIONS_TOTAL,
    RT_APPLIED_TOTAL,
    RT_UNSUPPORTED_TOTAL,
];

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn every_name_is_prefixed() {
        for name in ALL {
            assert!(name.starts_with("astrs_daemon_"), "{name}");
        }
    }

    #[test]
    fn every_name_is_distinct() {
        let mut seen: Vec<&str> = ALL.to_vec();
        let count = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), count, "duplicate metric name");
    }

    #[test]
    fn counters_are_suffixed_and_gauges_are_not() {
        let gauges = [
            ROUTES,
            NODES_RUNNING,
            PEERS_CONNECTED,
            SEGMENTS_OPEN,
            DATAFLOWS,
            TIMER_JITTER_P99_US,
        ];
        for name in ALL {
            let is_gauge = gauges.contains(name);
            assert_eq!(
                !name.ends_with("_total"),
                is_gauge,
                "{name} is on the wrong side of the counter/gauge split"
            );
        }
    }

    #[test]
    fn names_are_lower_snake_case() {
        for name in ALL {
            assert!(
                name.bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_' || byte.is_ascii_digit()),
                "{name}"
            );
        }
    }
}
