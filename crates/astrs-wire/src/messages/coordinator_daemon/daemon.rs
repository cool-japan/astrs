//! `daemon → coordinator`: [`DaemonEvent`] (blueprint §7.3, §24.1).
//!
//! What a daemon reports upward: that it exists, that it is alive and how
//! loaded it is, what happened to the builds and spawns it was asked for, what
//! its nodes are doing, and what its logs, metrics and topic taps contain.
//!
//! Frozen variant indices 0–11.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{DaemonEvent, DaemonStats, DurationMs, FrameKind, WireMessage};
//!
//! let heartbeat = DaemonEvent::Heartbeat {
//!     seq: 3,
//!     sent_at: Default::default(),
//!     stats: DaemonStats {
//!         uptime: DurationMs::from_secs(60),
//!         node_count: 4,
//!         dataflow_count: 1,
//!         cpu_percent: 12.5,
//!         rss_bytes: 64 << 20,
//!         shm_bytes_mapped: 8 << 20,
//!         shm_fallback_total: 0,
//!         frames_sent: 1_000,
//!         frames_received: 900,
//!         bytes_sent: 1 << 20,
//!         bytes_received: 1 << 19,
//!     },
//! };
//! assert_eq!(DaemonEvent::KIND, FrameKind::DaemonEvent);
//! assert!(heartbeat.is_liveness());
//! ```

use core::fmt;
use std::collections::BTreeMap;

use astrs_time::HlcTimestamp;
use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::common::log::LogRecord;
use crate::common::metrics::{DaemonStats, NodeIoSample, NodeMetricsSample};
use crate::common::status::NodeExitCause;
use crate::common::stream::DataFrame;
use crate::frame::FrameKind;
use crate::ids::{BuildId, DataflowId, NodeId, SubscriptionId};
use crate::messages::coordinator_daemon::types::{BuildOutcome, DaemonRegistration, SpawnOutcome};
use crate::messages::impl_wire_message;

/// The daemon → coordinator message family (§24.1).
///
/// # Examples
///
/// ```
/// use astrs_wire::{DaemonEvent, DataflowId, NodeExitCause, WireMessage};
///
/// let stopped = DaemonEvent::NodeStopped {
///     dataflow: DataflowId::from_u128(1),
///     node: "camera".parse()?,
///     generation: 2,
///     cause: NodeExitCause::Success,
///     restarting: false,
/// };
/// assert_eq!(stopped.variant_name(), "NodeStopped");
/// assert!(stopped.is_terminal());
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
// No `Eq`: `NodeMetrics` carries `f32` gauges and `TopicTapData` carries
// [`crate::Metadata`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DaemonEvent {
    /// The daemon is announcing itself, at connection time or after a
    /// reconnect.
    #[oxicode(variant = 0)]
    Register(DaemonRegistration),
    /// Liveness plus the load figures `astrs top` displays.
    #[oxicode(variant = 1)]
    Heartbeat {
        /// A counter that increases by one per heartbeat.
        seq: u64,
        /// When the daemon sent it.
        sent_at: HlcTimestamp,
        /// The daemon's current load.
        stats: DaemonStats,
    },
    /// A build finished, one way or another.
    #[oxicode(variant = 2)]
    BuildResult {
        /// The build.
        build: BuildId,
        /// The dataflow it was for.
        dataflow: DataflowId,
        /// How it ended.
        outcome: BuildOutcome,
    },
    /// A spawn attempt finished, one way or another.
    #[oxicode(variant = 3)]
    SpawnResult {
        /// The dataflow the node belongs to.
        dataflow: DataflowId,
        /// The node.
        node: NodeId,
        /// The incarnation this result belongs to.
        generation: u64,
        /// How it ended.
        outcome: SpawnOutcome,
    },
    /// Every node this daemon runs for the dataflow has registered.
    #[oxicode(variant = 4)]
    AllNodesReady {
        /// The dataflow.
        dataflow: DataflowId,
        /// The nodes that registered, so the coordinator can tally across
        /// daemons.
        nodes: Vec<NodeId>,
    },
    /// Every node this daemon runs for the dataflow has finished.
    #[oxicode(variant = 5)]
    AllNodesFinished {
        /// The dataflow.
        dataflow: DataflowId,
        /// How each node ended.
        results: BTreeMap<NodeId, NodeExitCause>,
    },
    /// One node stopped.
    #[oxicode(variant = 6)]
    NodeStopped {
        /// The dataflow the node belongs to.
        dataflow: DataflowId,
        /// The node.
        node: NodeId,
        /// The incarnation that stopped.
        generation: u64,
        /// Why it stopped.
        cause: NodeExitCause,
        /// Whether the daemon is about to restart it under its restart policy
        /// (§12), so the coordinator does not declare the dataflow finished.
        restarting: bool,
    },
    /// A batch of per-node metrics (§13, every 2 s by default).
    #[oxicode(variant = 7)]
    NodeMetrics {
        /// The dataflow the samples belong to.
        dataflow: DataflowId,
        /// One sample per node.
        samples: Vec<NodeMetricsSample>,
    },
    /// Log records: either an answer to [`crate::CoordinatorEvent::Logs`] or a
    /// spontaneous push for a live subscriber.
    #[oxicode(variant = 8)]
    Log {
        /// The request being answered, or `None` for a push.
        request: Option<u64>,
        /// The records, oldest first.
        records: Vec<LogRecord>,
        /// Whether a limit cut the batch short.
        truncated: bool,
    },
    /// One tapped message on its way to a CLI subscriber.
    #[oxicode(variant = 9)]
    TopicTapData {
        /// The tapped message, already tagged with its subscription.
        frame: Box<DataFrame>,
        /// How many messages the tap has dropped so far because the subscriber
        /// could not keep up (§17 `topic echo`, `--hz` shaping).
        dropped: u64,
    },
    /// The daemon applied a state catch-up batch (§24.1 `StateCatchUp{seq}`).
    #[oxicode(variant = 10)]
    StateCatchUpAck {
        /// The highest sequence the daemon has applied.
        seq: u64,
        /// How many entries it applied from the batch.
        applied: u32,
    },
    /// The daemon is going away.
    #[oxicode(variant = 11)]
    Exit {
        /// Whether it is shutting down cleanly rather than failing.
        graceful: bool,
        /// Why, for the coordinator's log.
        message: String,
    },
    /// The bandwidth half of [`DaemonEvent::NodeMetrics`]: bytes per port and
    /// shared-memory occupancy (§6.2, §13).
    ///
    /// A tail append beyond §24.1, and necessarily a *separate* variant:
    /// `NodeMetrics` at index 7 is inside the frozen protocol prefix, so its
    /// payload's encoding cannot be widened (§7.2). See
    /// [`crate::NodeIoSample`].
    ///
    /// Emitted on the same sampling round as `NodeMetrics`, immediately after
    /// it and for the same node set, so a coordinator that holds both never
    /// pairs readings from different rounds.
    #[oxicode(variant = 12)]
    NodeIoMetrics {
        /// The dataflow the nodes belong to.
        dataflow: DataflowId,
        /// One sample per node this daemon is running for that dataflow.
        samples: Vec<NodeIoSample>,
    },
}

impl DaemonEvent {
    /// The dataflow this event concerns, when it concerns one.
    #[must_use]
    pub fn dataflow(&self) -> Option<DataflowId> {
        match self {
            Self::BuildResult { dataflow, .. }
            | Self::SpawnResult { dataflow, .. }
            | Self::AllNodesReady { dataflow, .. }
            | Self::AllNodesFinished { dataflow, .. }
            | Self::NodeStopped { dataflow, .. }
            | Self::NodeMetrics { dataflow, .. }
            | Self::NodeIoMetrics { dataflow, .. } => Some(*dataflow),
            Self::TopicTapData { frame, .. } => Some(frame.dataflow),
            Self::Register(_)
            | Self::Heartbeat { .. }
            | Self::Log { .. }
            | Self::StateCatchUpAck { .. }
            | Self::Exit { .. } => None,
        }
    }

    /// The node this event is about, when it is about one.
    #[must_use]
    pub const fn node(&self) -> Option<&NodeId> {
        match self {
            Self::SpawnResult { node, .. } | Self::NodeStopped { node, .. } => Some(node),
            _ => None,
        }
    }

    /// Whether this event is a heartbeat and carries no news.
    #[must_use]
    pub const fn is_liveness(&self) -> bool {
        matches!(self, Self::Heartbeat { .. })
    }

    /// Whether this event reports something ending.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::AllNodesFinished { .. } | Self::NodeStopped { .. } | Self::Exit { .. }
        )
    }

    /// Whether this event reports a failure the coordinator should surface.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{DaemonEvent, DataflowId, NodeExitCause};
    ///
    /// let crashed = DaemonEvent::NodeStopped {
    ///     dataflow: DataflowId::from_u128(1),
    ///     node: "camera".parse()?,
    ///     generation: 1,
    ///     cause: NodeExitCause::ExitCode { code: 1 },
    ///     restarting: false,
    /// };
    /// assert!(crashed.is_failure());
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn is_failure(&self) -> bool {
        match self {
            Self::NodeStopped { cause, .. } => cause.is_failure(),
            Self::SpawnResult { outcome, .. } => {
                matches!(outcome, SpawnOutcome::Failed { .. })
            }
            Self::BuildResult { outcome, .. } => matches!(outcome, BuildOutcome::Failed { .. }),
            Self::AllNodesFinished { results, .. } => {
                results.values().any(NodeExitCause::is_failure)
            }
            Self::Exit { graceful, .. } => !*graceful,
            _ => false,
        }
    }

    /// The subscription this event delivers data for, if any.
    #[must_use]
    pub fn subscription(&self) -> Option<SubscriptionId> {
        match self {
            Self::TopicTapData { frame, .. } => Some(frame.subscription),
            _ => None,
        }
    }

    /// The payload bytes this event carries, for bandwidth accounting.
    #[must_use]
    pub fn payload_len(&self) -> usize {
        match self {
            Self::TopicTapData { frame, .. } => frame.payload.len(),
            _ => 0,
        }
    }

    /// Compares two events with `f64`/`f32` bit patterns rather than IEEE
    /// equality — see [`crate::PeerEvent::bitwise_eq`].
    #[must_use]
    pub fn bitwise_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Heartbeat {
                    seq: left_seq,
                    sent_at: left_sent,
                    stats: left_stats,
                },
                Self::Heartbeat {
                    seq: right_seq,
                    sent_at: right_sent,
                    stats: right_stats,
                },
            ) => {
                left_seq == right_seq
                    && left_sent == right_sent
                    && left_stats.bitwise_eq(right_stats)
            }
            (
                Self::TopicTapData {
                    frame: left_frame,
                    dropped: left_dropped,
                },
                Self::TopicTapData {
                    frame: right_frame,
                    dropped: right_dropped,
                },
            ) => left_dropped == right_dropped && left_frame.bitwise_eq(right_frame),
            (
                Self::NodeMetrics {
                    dataflow: left_dataflow,
                    samples: left_samples,
                },
                Self::NodeMetrics {
                    dataflow: right_dataflow,
                    samples: right_samples,
                },
            ) => {
                left_dataflow == right_dataflow
                    && left_samples.len() == right_samples.len()
                    && left_samples.iter().zip(right_samples).all(|(left, right)| {
                        left.node == right.node
                            && left.timestamp == right.timestamp
                            && left.cpu_percent.to_bits() == right.cpu_percent.to_bits()
                            && left.rss_bytes == right.rss_bytes
                            && left.queue_depths == right.queue_depths
                            && left.sent_total == right.sent_total
                            && left.received_total == right.received_total
                            && left.dropped_total == right.dropped_total
                            && left.shm_slots_in_use == right.shm_slots_in_use
                    })
            }
            _ => self == other,
        }
    }
}

impl fmt::Display for DaemonEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Register(registration) => write!(f, "{registration}"),
            Self::Heartbeat { seq, stats, .. } => {
                write!(f, "heartbeat #{seq} ({} node(s))", stats.node_count)
            }
            Self::BuildResult { build, outcome, .. } => write!(f, "build {build}: {outcome}"),
            Self::SpawnResult {
                dataflow,
                node,
                outcome,
                ..
            } => write!(f, "{dataflow}/{node}: {outcome}"),
            Self::AllNodesReady { dataflow, nodes } => {
                write!(f, "{} node(s) of {dataflow} ready", nodes.len())
            }
            Self::AllNodesFinished { dataflow, results } => {
                write!(f, "{} node(s) of {dataflow} finished", results.len())
            }
            Self::NodeStopped {
                dataflow,
                node,
                cause,
                restarting,
                ..
            } => write!(
                f,
                "{dataflow}/{node} {cause}{}",
                if *restarting { " (restarting)" } else { "" }
            ),
            Self::NodeMetrics { dataflow, samples } => {
                write!(f, "{} metric sample(s) for {dataflow}", samples.len())
            }
            Self::NodeIoMetrics { dataflow, samples } => {
                write!(f, "{} bandwidth sample(s) for {dataflow}", samples.len())
            }
            Self::Log {
                request, records, ..
            } => match request {
                Some(request) => write!(f, "{} log record(s) for request {request}", records.len()),
                None => write!(f, "{} log record(s)", records.len()),
            },
            Self::TopicTapData { frame, dropped } => write!(
                f,
                "tap {} on {} ({} byte(s), {dropped} dropped)",
                frame.subscription,
                frame.source,
                frame.payload.len()
            ),
            Self::StateCatchUpAck { seq, applied } => {
                write!(f, "applied {applied} catch-up entries up to #{seq}")
            }
            Self::Exit { graceful, message } => write!(
                f,
                "daemon exiting {}: {message}",
                if *graceful { "cleanly" } else { "on failure" }
            ),
        }
    }
}

impl_wire_message!(
    DaemonEvent,
    FrameKind::DaemonEvent,
    [
        "Register",
        "Heartbeat",
        "BuildResult",
        "SpawnResult",
        "AllNodesReady",
        "AllNodesFinished",
        "NodeStopped",
        "NodeMetrics",
        "Log",
        "TopicTapData",
        "StateCatchUpAck",
        "Exit",
        "NodeIoMetrics",
    ],
    fn variant_index(&self) -> u16 {
        match self {
            Self::Register(_) => 0,
            Self::Heartbeat { .. } => 1,
            Self::BuildResult { .. } => 2,
            Self::SpawnResult { .. } => 3,
            Self::AllNodesReady { .. } => 4,
            Self::AllNodesFinished { .. } => 5,
            Self::NodeStopped { .. } => 6,
            Self::NodeMetrics { .. } => 7,
            Self::Log { .. } => 8,
            Self::TopicTapData { .. } => 9,
            Self::StateCatchUpAck { .. } => 10,
            Self::Exit { .. } => 11,
            Self::NodeIoMetrics { .. } => 12,
        }
    }
);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireDecode, WireEncode};
    use crate::frame::{FrameFlags, FrameLimits};
    use crate::messages::WireMessage;
    use crate::messages::samples::daemon_events;

    #[test]
    fn the_family_keeps_its_twelve_frozen_variants_and_its_tail_append() {
        // §24.1 froze 0..=11. `NodeIoMetrics` is appended at 12 because
        // `NodeMetrics` at 7 is inside the frozen protocol prefix and its
        // payload therefore cannot be widened (§7.2) — see
        // `crate::NodeIoSample`.
        assert_eq!(DaemonEvent::VARIANT_NAMES.len(), 13);
        assert_eq!(DaemonEvent::VARIANT_NAMES[0], "Register");
        assert_eq!(DaemonEvent::VARIANT_NAMES[11], "Exit");
        assert_eq!(DaemonEvent::VARIANT_NAMES[12], "NodeIoMetrics");
    }

    #[test]
    fn every_variant_reports_and_encodes_its_frozen_index() {
        let samples = daemon_events().unwrap();
        assert_eq!(samples.len(), DaemonEvent::VARIANT_NAMES.len());
        for (index, sample) in samples.iter().enumerate() {
            let index = u16::try_from(index).unwrap();
            assert_eq!(sample.variant_index(), index, "{sample:?}");
            assert_eq!(u16::from(sample.encode_to_vec().unwrap()[0]), index);
        }
    }

    #[test]
    fn every_variant_round_trips_through_a_frame() {
        let limits = FrameLimits::network();
        for sample in daemon_events().unwrap() {
            let bytes = sample.to_frame(FrameFlags::CRC, &limits).unwrap();
            let decoded = DaemonEvent::from_bytes(&bytes, &limits).unwrap();
            assert!(decoded.bitwise_eq(&sample), "{sample:?}");
        }
    }

    #[test]
    fn trailing_bytes_after_an_event_are_refused() {
        for sample in daemon_events().unwrap() {
            let mut bytes = sample.encode_to_vec().unwrap();
            bytes.push(2);
            assert!(matches!(
                DaemonEvent::decode_exact(&bytes),
                Err(crate::error::WireError::TrailingBytes { .. })
            ));
        }
    }

    #[test]
    fn failure_reporting_covers_every_way_a_daemon_reports_bad_news() {
        let dataflow = DataflowId::from_u128(1);
        let node = NodeId::new("camera").unwrap();

        assert!(
            DaemonEvent::NodeStopped {
                dataflow,
                node: node.clone(),
                generation: 1,
                cause: NodeExitCause::ExitCode { code: 1 },
                restarting: false,
            }
            .is_failure()
        );
        assert!(
            !DaemonEvent::NodeStopped {
                dataflow,
                node: node.clone(),
                generation: 1,
                cause: NodeExitCause::Success,
                restarting: false,
            }
            .is_failure()
        );
        assert!(
            DaemonEvent::SpawnResult {
                dataflow,
                node: node.clone(),
                generation: 1,
                outcome: SpawnOutcome::Failed {
                    message: "no such file".to_owned(),
                    errno: Some(2),
                },
            }
            .is_failure()
        );
        assert!(
            DaemonEvent::BuildResult {
                build: BuildId::from_u128(1),
                dataflow,
                outcome: BuildOutcome::Failed {
                    node: None,
                    exit_code: Some(101),
                    message: "failed".to_owned(),
                    output: String::new(),
                },
            }
            .is_failure()
        );
        assert!(
            DaemonEvent::AllNodesFinished {
                dataflow,
                results: BTreeMap::from([(
                    node,
                    NodeExitCause::Panic {
                        message: "boom".to_owned()
                    }
                )]),
            }
            .is_failure()
        );
        assert!(
            DaemonEvent::Exit {
                graceful: false,
                message: "socket closed".to_owned(),
            }
            .is_failure()
        );
        assert!(
            !DaemonEvent::Exit {
                graceful: true,
                message: "asked to".to_owned(),
            }
            .is_failure()
        );
    }

    #[test]
    fn a_tap_reports_its_subscription_and_payload_size() {
        let sample = daemon_events()
            .unwrap()
            .into_iter()
            .find(|event| matches!(event, DaemonEvent::TopicTapData { .. }))
            .expect("the sample table covers TopicTapData");
        assert!(sample.subscription().is_some());
        assert!(sample.payload_len() > 0);
        assert!(sample.dataflow().is_some());
        assert_eq!(
            DaemonEvent::StateCatchUpAck { seq: 1, applied: 0 }.payload_len(),
            0
        );
    }

    #[test]
    fn float_gauges_survive_the_wire_bit_for_bit() {
        let mut sample =
            NodeMetricsSample::new(NodeId::new("camera").unwrap(), HlcTimestamp::new(1_000, 0));
        sample.cpu_percent = f32::NAN;
        let event = DaemonEvent::NodeMetrics {
            dataflow: DataflowId::from_u128(1),
            samples: vec![sample],
        };
        let bytes = event.encode_to_vec().unwrap();
        let decoded = DaemonEvent::decode_exact(&bytes).unwrap();
        assert_ne!(decoded, event);
        assert!(decoded.bitwise_eq(&event));
    }

    #[test]
    fn terminal_and_liveness_predicates_classify_the_family() {
        for sample in daemon_events().unwrap() {
            if sample.is_liveness() {
                assert!(!sample.is_terminal());
            }
        }
        assert!(
            DaemonEvent::Exit {
                graceful: true,
                message: String::new()
            }
            .is_terminal()
        );
    }

    #[test]
    fn display_names_every_variant_without_panicking() {
        for sample in daemon_events().unwrap() {
            assert!(!sample.to_string().is_empty());
        }
    }
}
