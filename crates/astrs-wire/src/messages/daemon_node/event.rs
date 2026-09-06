//! `daemon → node`: [`NodeEvent`] (blueprint §7.3, §24.1).
//!
//! The node-facing event stream: inputs arriving, inputs ending, the dataflow
//! stopping, peers failing and recovering, parameters changing, and the
//! slow-start route handshake of §6.3 moving a route on and off the zero-copy
//! plane.
//!
//! Frozen variant indices 0–12, plus five tail appends documented below.
//!
//! # Beyond §24.1
//!
//! | Index | Variant | Why |
//! |---:|---|---|
//! | 13 | [`NodeEvent::Registered`] | [`crate::NodeRequest::Register`] needs an answer, and a dynamic node needs the wiring it did not have |
//! | 14 | [`NodeEvent::ExtValue`] | [`crate::NodeRequest::ExtLoad`] needs an answer |
//! | 15 | [`NodeEvent::InputRouteUpgrade`] | §6.3's other end: [`NodeEvent::RouteUpgrade`] names an **output**, so nothing could tell a *consumer* that one of its inputs now reads from a ring |
//! | 16 | [`NodeEvent::InputRouteDowngrade`] | the same asymmetry on the way back down |
//! | 17 | [`NodeEvent::DeadlineViolated`] | the daemon's relay of a peer's [`crate::NodeRequest::ReportDeadlineViolation`] (§11.3), fanned out on `astrs/status` exactly like [`NodeEvent::NodeFailed`]/[`NodeEvent::Restarted`] |
//!
//! # Why the consumer side needed its own pair
//!
//! §6.3 has two ends. The producer's is [`NodeEvent::RouteUpgrade`], which
//! names `output: DataId` — a field a consumer cannot act on, because the
//! thing that moves for a consumer is an **input**, and an input's identity
//! (`{node}/{input}`) is not the output's. Consumers were therefore left
//! attaching to segments by hand from facts they had to assemble themselves.
//! [`NodeEvent::InputRouteUpgrade`] carries the three facts an attach needs and
//! no more: the input that moved, the producer port that owns the ring (which
//! a `path: dynamic` consumer has no specification to look up), and the
//! segment itself.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{FrameKind, NodeEvent, WireMessage};
//!
//! let input = NodeEvent::Input {
//!     id: "frames".parse()?,
//!     source: "camera/image".parse()?,
//!     metadata: Default::default(),
//!     payload: vec![1, 2, 3],
//! };
//! assert_eq!(NodeEvent::KIND, FrameKind::NodeEvent);
//! assert_eq!(input.variant_index(), 0);
//! assert_eq!(input.payload_len(), 3);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use core::fmt;

use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::common::duration::DurationMs;
use crate::common::node::NodeSpawnSpec;
use crate::common::route::{RouteCloseReason, RouteDowngradeReason, ShmSegmentSpec};
use crate::common::status::{NodeExitCause, StopCause};
use crate::frame::FrameKind;
use crate::ids::{DataId, NodeId, OperatorId, ParamKey, PortRef, SessionId};
use crate::messages::control::types::ParamScope;
use crate::messages::daemon_node::types::ExtensionKey;
use crate::messages::impl_wire_message;
use crate::metadata::{Metadata, Parameter};

/// The daemon → node message family (§24.1).
///
/// # Examples
///
/// ```
/// use astrs_wire::{NodeEvent, StopCause, WireMessage};
///
/// let stop = NodeEvent::Stop { cause: StopCause::Requested, grace: None };
/// assert_eq!(stop.variant_name(), "Stop");
/// assert!(stop.is_terminal());
/// ```
// No `Eq`: `Input` carries [`Metadata`] and `ParamUpdate` a [`Parameter`],
// either of which may hold an `f64`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NodeEvent {
    /// One message for one of the node's inputs.
    ///
    /// The payload is **raw bytes** — an Arrow IPC stream (§6.1) this crate
    /// deliberately cannot interpret.
    ///
    /// # An upgraded route still sends some of these
    ///
    /// This documentation used to say that the daemon *"stops sending these
    /// entirely"* once a route reaches the shared-memory plane (§6.3). That is
    /// not what §6.2 specifies and not what the implementation does: the
    /// threshold rule — *"a heap payload ≥ threshold is copied once into a
    /// slot; below threshold it rides the UDS control channel"* — is per
    /// message, so a producer on the ring still publishes its small messages
    /// as `Input`, and so does one whose ring was momentarily full (§6.2:
    /// *never sleep-retry*).
    ///
    /// What an upgrade does change is the *large* messages: those become a
    /// slot reference the consumer reads in place, which is what makes the
    /// fast path free of copies rather than merely cheap. A consumer must
    /// therefore keep reading this event after
    /// [`NodeEvent::InputRouteUpgrade`], not instead of it.
    #[oxicode(variant = 0)]
    Input {
        /// The input this message arrived on.
        id: DataId,
        /// The producer port it came from.
        source: PortRef,
        /// The metadata riding beside the payload (§6.1).
        metadata: Metadata,
        /// The payload bytes.
        payload: Vec<u8>,
    },
    /// An input will receive nothing further.
    #[oxicode(variant = 1)]
    InputClosed {
        /// The input that closed.
        id: DataId,
        /// The producer port that stopped.
        source: PortRef,
        /// Why it closed.
        reason: RouteCloseReason,
    },
    /// A previously closed input is live again, because its producer restarted.
    ///
    /// The new generation is included so a node can discard state it built
    /// from the previous incarnation.
    #[oxicode(variant = 2)]
    InputRecovered {
        /// The input that recovered.
        id: DataId,
        /// The producer port that came back.
        source: PortRef,
        /// The producer's new incarnation.
        generation: u64,
    },
    /// Finish up and exit.
    #[oxicode(variant = 3)]
    Stop {
        /// Why the node is being stopped.
        cause: StopCause,
        /// How long it has before it is killed.
        grace: Option<DurationMs>,
    },
    /// Reload the node's code, or one operator inside it (§9.3).
    #[oxicode(variant = 4)]
    Reload {
        /// The operator to reload, or `None` for the whole node.
        operator: Option<OperatorId>,
        /// The artefact to load, when it moved.
        path: Option<String>,
    },
    /// Every input of this node has closed.
    ///
    /// A node whose manifest says so exits here; one that also produces on a
    /// timer keeps running.
    #[oxicode(variant = 5)]
    AllInputsClosed,
    /// A peer node failed (§24.1 `NodeFailed{peer}`).
    ///
    /// Delivered even when the peer is not a direct producer, so a node can
    /// abandon a request/response exchange whose other end is gone (§9.4).
    #[oxicode(variant = 6)]
    NodeFailed {
        /// The node that failed.
        peer: NodeId,
        /// How it failed.
        cause: NodeExitCause,
    },
    /// A peer node was restarted under its restart policy (§12).
    #[oxicode(variant = 7)]
    Restarted {
        /// The node that came back.
        peer: NodeId,
        /// Its new incarnation.
        generation: u64,
    },
    /// A parameter this node reads was written (§17 `param set`).
    #[oxicode(variant = 8)]
    ParamUpdate {
        /// The scope it was written in.
        scope: ParamScope,
        /// The key.
        key: ParamKey,
        /// The new value.
        value: Parameter,
    },
    /// A parameter this node reads was deleted.
    #[oxicode(variant = 9)]
    ParamDeleted {
        /// The scope it was deleted from.
        scope: ParamScope,
        /// The key.
        key: ParamKey,
    },
    /// An extension entry this node owned was dropped — its time-to-live
    /// expired, or the daemon reclaimed it.
    #[oxicode(variant = 10)]
    ExtDropped {
        /// The key that went away.
        key: ExtensionKey,
        /// Why.
        reason: String,
    },
    /// Every consumer has attached: publish straight into shared memory
    /// (§6.3).
    ///
    /// Acknowledged with [`crate::NodeRequest::RouteUpgradeAck`]; until that
    /// arrives the daemon keeps brokering the route, so no message can fall
    /// between the two planes.
    #[oxicode(variant = 11)]
    RouteUpgrade {
        /// The output being upgraded.
        output: DataId,
        /// The segment to publish into.
        segment: ShmSegmentSpec,
        /// The consumers that attached, for the node's own accounting.
        consumers: Vec<PortRef>,
    },
    /// Go back to the daemon path for this output (§6.3).
    #[oxicode(variant = 12)]
    RouteDowngrade {
        /// The output being downgraded.
        output: DataId,
        /// Why.
        reason: RouteDowngradeReason,
    },
    /// The registration was accepted — a tail append beyond §24.1.
    ///
    /// Carries the node's effective specification, which a `path: dynamic`
    /// node (§8.3) has no other way of learning: it was never handed an
    /// `ASTRS_NODE_CONFIG` blob because nobody spawned it.
    #[oxicode(variant = 13)]
    Registered {
        /// The effective specification, including the generation the daemon
        /// assigned.
        spec: Box<NodeSpawnSpec>,
        /// The session this node's connection belongs to.
        session: SessionId,
    },
    /// The answer to [`crate::NodeRequest::ExtLoad`] — a tail append beyond
    /// §24.1.
    #[oxicode(variant = 14)]
    ExtValue {
        /// The key that was read.
        key: ExtensionKey,
        /// Its bytes, or `None` when the key is unset.
        value: Option<Vec<u8>>,
    },
    /// One of this node's **inputs** now reads from a shared-memory ring
    /// (§6.2, §6.3) — a tail append beyond §24.1, and the consumer-side twin
    /// of [`NodeEvent::RouteUpgrade`].
    ///
    /// The daemon sends this as soon as a segment exists for the producer's
    /// current incarnation and every consumer of that output is eligible for
    /// the plane — *before* the producer is offered its own upgrade, because
    /// §6.3 only lets the producer switch once the daemon has observed every
    /// consumer in the segment's consumer table, and a consumer cannot be
    /// observed before it attaches.
    ///
    /// Nothing is lost by attaching early: the producer is still on the
    /// reliable path, so the ring is empty and stays empty until it switches.
    #[oxicode(variant = 15)]
    InputRouteUpgrade {
        /// The input that moved onto the ring.
        input: DataId,
        /// The producer port that owns the ring.
        ///
        /// Carried rather than derived because a `path: dynamic` consumer
        /// (§8.3) has no manifest specification to look the producer up in,
        /// and the segment's identity is
        /// `{dataflow}/{producer node}/{output}/{generation}`.
        source: PortRef,
        /// The segment to attach to.
        segment: ShmSegmentSpec,
        /// The consumer port this event is addressed to.
        ///
        /// Redundant with the connection it arrives on, deliberately: it makes
        /// a wire trace of a fan-out readable, and it lets a node refuse an
        /// event that was routed to the wrong session rather than attaching to
        /// a ring that is not its business.
        consumer: PortRef,
    },
    /// One of this node's inputs is back on the reliable daemon path (§6.3) —
    /// a tail append beyond §24.1, and the consumer-side twin of
    /// [`NodeEvent::RouteDowngrade`].
    ///
    /// The consumer detaches from the ring; the daemon resumes brokering the
    /// route. A consumer that has already detached on its own (a stale
    /// generation, an unreadable segment) may receive this anyway, and
    /// ignoring it is correct.
    #[oxicode(variant = 16)]
    InputRouteDowngrade {
        /// The input that went back to the daemon path.
        input: DataId,
        /// Why.
        reason: RouteDowngradeReason,
    },
    /// A peer's per-input deadline (§11.3) was violated — a tail append
    /// beyond §24.1, and the daemon's relay of that peer's own
    /// [`crate::NodeRequest::ReportDeadlineViolation`].
    ///
    /// Delivered on `astrs/status` exactly like [`NodeEvent::NodeFailed`] and
    /// [`NodeEvent::Restarted`] (§8.4): every node that declared `astrs/status`
    /// as an input sees it, not only the node whose deadline was violated.
    #[oxicode(variant = 17)]
    DeadlineViolated {
        /// The node whose deadline was violated.
        peer: NodeId,
        /// The input whose budget was exceeded.
        input: DataId,
        /// The budget it was held to.
        budget: DurationMs,
        /// The measured latency.
        latency: DurationMs,
    },
}

impl NodeEvent {
    /// The input this event concerns, when it concerns one.
    #[must_use]
    pub const fn input(&self) -> Option<&DataId> {
        match self {
            Self::Input { id, .. }
            | Self::InputClosed { id, .. }
            | Self::InputRecovered { id, .. } => Some(id),
            Self::InputRouteUpgrade { input, .. } | Self::InputRouteDowngrade { input, .. } => {
                Some(input)
            }
            _ => None,
        }
    }

    /// The output this event concerns, when it concerns one.
    #[must_use]
    pub const fn output(&self) -> Option<&DataId> {
        match self {
            Self::RouteUpgrade { output, .. } | Self::RouteDowngrade { output, .. } => Some(output),
            _ => None,
        }
    }

    /// Whether this event delivers data.
    #[must_use]
    pub const fn is_input(&self) -> bool {
        matches!(self, Self::Input { .. })
    }

    /// The payload bytes this event carries, for bandwidth accounting.
    #[must_use]
    pub fn payload_len(&self) -> usize {
        match self {
            Self::Input { payload, .. } => payload.len(),
            Self::ExtValue {
                value: Some(value), ..
            } => value.len(),
            _ => 0,
        }
    }

    /// Whether this event means the node should wind down.
    ///
    /// [`NodeEvent::AllInputsClosed`] is included because a node with no
    /// remaining inputs and no timer has nothing left to do — whether it
    /// *exits* is the node's decision, but it is a wind-down signal either way.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Stop { .. } | Self::AllInputsClosed)
    }

    /// Whether this event reports a fault rather than normal progress.
    #[must_use]
    pub fn is_fault(&self) -> bool {
        match self {
            Self::NodeFailed { .. } => true,
            Self::InputClosed { reason, .. } => !reason.is_expected(),
            Self::RouteDowngrade { reason, .. } | Self::InputRouteDowngrade { reason, .. } => {
                reason.is_capacity_pressure()
            }
            _ => false,
        }
    }

    /// Whether this event changes which plane a route runs on (§6.3), at
    /// either end.
    #[must_use]
    pub const fn is_route_change(&self) -> bool {
        matches!(
            self,
            Self::RouteUpgrade { .. }
                | Self::RouteDowngrade { .. }
                | Self::InputRouteUpgrade { .. }
                | Self::InputRouteDowngrade { .. }
        )
    }

    /// Whether this event concerns the *consumer* end of a route (§6.3).
    ///
    /// The discriminator a session uses to route an event to its input-side
    /// plane bookkeeping rather than its output-side one, since both ends
    /// arrive on the same connection.
    #[must_use]
    pub const fn is_input_route_change(&self) -> bool {
        matches!(
            self,
            Self::InputRouteUpgrade { .. } | Self::InputRouteDowngrade { .. }
        )
    }

    /// The peer node this event is about, when it is about one.
    #[must_use]
    pub const fn peer(&self) -> Option<&NodeId> {
        match self {
            Self::NodeFailed { peer, .. } | Self::Restarted { peer, .. } => Some(peer),
            _ => None,
        }
    }

    /// Compares two events with `f64` bit patterns rather than IEEE equality —
    /// see [`crate::PeerEvent::bitwise_eq`].
    #[must_use]
    pub fn bitwise_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Input {
                    id: left_id,
                    source: left_source,
                    metadata: left_metadata,
                    payload: left_payload,
                },
                Self::Input {
                    id: right_id,
                    source: right_source,
                    metadata: right_metadata,
                    payload: right_payload,
                },
            ) => {
                left_id == right_id
                    && left_source == right_source
                    && left_metadata.bitwise_eq(right_metadata)
                    && left_payload == right_payload
            }
            (
                Self::ParamUpdate {
                    scope: left_scope,
                    key: left_key,
                    value: left_value,
                },
                Self::ParamUpdate {
                    scope: right_scope,
                    key: right_key,
                    value: right_value,
                },
            ) => {
                left_scope == right_scope
                    && left_key == right_key
                    && left_value.bitwise_eq(right_value)
            }
            _ => self == other,
        }
    }
}

impl fmt::Display for NodeEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input {
                id,
                source,
                payload,
                ..
            } => write!(f, "input {id} from {source} ({} byte(s))", payload.len()),
            Self::InputClosed { id, reason, .. } => write!(f, "input {id} closed: {reason}"),
            Self::InputRecovered { id, generation, .. } => {
                write!(f, "input {id} recovered at generation {generation}")
            }
            Self::Stop { cause, .. } => write!(f, "stop: {cause}"),
            Self::Reload { operator, .. } => match operator {
                Some(operator) => write!(f, "reload operator {operator}"),
                None => f.write_str("reload node"),
            },
            Self::AllInputsClosed => f.write_str("all inputs closed"),
            Self::NodeFailed { peer, cause } => write!(f, "peer {peer} failed: {cause}"),
            Self::Restarted { peer, generation } => {
                write!(f, "peer {peer} restarted as generation {generation}")
            }
            Self::ParamUpdate { scope, key, value } => write!(f, "{scope}: {key} = {value}"),
            Self::ParamDeleted { scope, key } => write!(f, "{scope}: {key} deleted"),
            Self::ExtDropped { key, reason } => write!(f, "extension {key} dropped: {reason}"),
            Self::RouteUpgrade {
                output,
                segment,
                consumers,
            } => write!(
                f,
                "upgrade {output} to {} ({} consumer(s))",
                segment.name,
                consumers.len()
            ),
            Self::RouteDowngrade { output, reason } => {
                write!(f, "downgrade {output}: {reason}")
            }
            Self::Registered { spec, .. } => write!(
                f,
                "registered {}/{} as generation {}",
                spec.dataflow, spec.node, spec.generation
            ),
            Self::ExtValue { key, value } => match value {
                Some(value) => write!(f, "{key} = {} byte(s)", value.len()),
                None => write!(f, "{key} is unset"),
            },
            Self::InputRouteUpgrade {
                input,
                source,
                segment,
                ..
            } => write!(f, "upgrade input {input} from {source} to {}", segment.name),
            Self::InputRouteDowngrade { input, reason } => {
                write!(f, "downgrade input {input}: {reason}")
            }
            Self::DeadlineViolated {
                peer,
                input,
                budget,
                latency,
            } => write!(
                f,
                "peer {peer} violated its deadline on {input}: {}ms over a {}ms budget",
                latency.as_millis().saturating_sub(budget.as_millis()),
                budget.as_millis(),
            ),
        }
    }
}

impl_wire_message!(
    NodeEvent,
    FrameKind::NodeEvent,
    [
        "Input",
        "InputClosed",
        "InputRecovered",
        "Stop",
        "Reload",
        "AllInputsClosed",
        "NodeFailed",
        "Restarted",
        "ParamUpdate",
        "ParamDeleted",
        "ExtDropped",
        "RouteUpgrade",
        "RouteDowngrade",
        "Registered",
        "ExtValue",
        "InputRouteUpgrade",
        "InputRouteDowngrade",
        "DeadlineViolated",
    ],
    fn variant_index(&self) -> u16 {
        match self {
            Self::Input { .. } => 0,
            Self::InputClosed { .. } => 1,
            Self::InputRecovered { .. } => 2,
            Self::Stop { .. } => 3,
            Self::Reload { .. } => 4,
            Self::AllInputsClosed => 5,
            Self::NodeFailed { .. } => 6,
            Self::Restarted { .. } => 7,
            Self::ParamUpdate { .. } => 8,
            Self::ParamDeleted { .. } => 9,
            Self::ExtDropped { .. } => 10,
            Self::RouteUpgrade { .. } => 11,
            Self::RouteDowngrade { .. } => 12,
            Self::Registered { .. } => 13,
            Self::ExtValue { .. } => 14,
            Self::InputRouteUpgrade { .. } => 15,
            Self::InputRouteDowngrade { .. } => 16,
            Self::DeadlineViolated { .. } => 17,
        }
    }
);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_time::HlcTimestamp;

    use super::*;
    use crate::codec::{WireDecode, WireEncode, round_trip};
    use crate::common::route::Plane;
    use crate::frame::{FrameFlags, FrameLimits};
    use crate::messages::WireMessage;
    use crate::messages::samples::node_events;

    #[test]
    fn the_family_freezes_the_thirteen_normative_variants_first() {
        assert_eq!(
            &NodeEvent::VARIANT_NAMES[..13],
            &[
                "Input",
                "InputClosed",
                "InputRecovered",
                "Stop",
                "Reload",
                "AllInputsClosed",
                "NodeFailed",
                "Restarted",
                "ParamUpdate",
                "ParamDeleted",
                "ExtDropped",
                "RouteUpgrade",
                "RouteDowngrade",
            ]
        );
        assert_eq!(NodeEvent::VARIANT_NAMES.len(), 18);
        assert_eq!(
            &NodeEvent::VARIANT_NAMES[13..],
            &[
                "Registered",
                "ExtValue",
                "InputRouteUpgrade",
                "InputRouteDowngrade",
                "DeadlineViolated",
            ],
            "tail appends stay at the tail, in the order they were added"
        );
    }

    #[test]
    fn every_variant_reports_and_encodes_its_frozen_index() {
        let samples = node_events().unwrap();
        assert_eq!(samples.len(), NodeEvent::VARIANT_NAMES.len());
        for (index, sample) in samples.iter().enumerate() {
            let index = u16::try_from(index).unwrap();
            assert_eq!(sample.variant_index(), index, "{sample:?}");
            assert_eq!(u16::from(sample.encode_to_vec().unwrap()[0]), index);
        }
    }

    #[test]
    fn every_variant_round_trips_through_a_frame() {
        let limits = FrameLimits::uds();
        for sample in node_events().unwrap() {
            let bytes = sample.to_frame(FrameFlags::EMPTY, &limits).unwrap();
            let decoded = NodeEvent::from_bytes(&bytes, &limits).unwrap();
            assert!(decoded.bitwise_eq(&sample), "{sample:?}");
        }
    }

    #[test]
    fn trailing_bytes_after_an_event_are_refused() {
        for sample in node_events().unwrap() {
            let mut bytes = sample.encode_to_vec().unwrap();
            bytes.push(3);
            assert!(matches!(
                NodeEvent::decode_exact(&bytes),
                Err(crate::error::WireError::TrailingBytes { .. })
            ));
        }
    }

    #[test]
    fn an_input_carries_its_id_metadata_and_bytes() {
        let event = NodeEvent::Input {
            id: DataId::new("frames").unwrap(),
            source: "camera/image".parse().unwrap(),
            metadata: Metadata::new(HlcTimestamp::new(1_000, 1)),
            payload: vec![9; 64],
        };
        assert!(event.is_input());
        assert_eq!(event.input().map(DataId::as_str), Some("frames"));
        assert_eq!(event.payload_len(), 64);
        assert!(!event.is_fault());
        assert_eq!(round_trip(&event).unwrap(), event);
    }

    #[test]
    fn faults_are_distinguished_from_orderly_progress() {
        assert!(
            NodeEvent::NodeFailed {
                peer: NodeId::new("camera").unwrap(),
                cause: NodeExitCause::Panic {
                    message: "boom".to_owned()
                },
            }
            .is_fault()
        );
        assert!(
            !NodeEvent::InputClosed {
                id: DataId::new("frames").unwrap(),
                source: "camera/image".parse().unwrap(),
                reason: RouteCloseReason::ProducerFinished,
            }
            .is_fault(),
            "a producer finishing is not a fault"
        );
        assert!(
            NodeEvent::InputClosed {
                id: DataId::new("frames").unwrap(),
                source: "camera/image".parse().unwrap(),
                reason: RouteCloseReason::PlaneFailed {
                    plane: Plane::Quic,
                    message: "reset".to_owned(),
                },
            }
            .is_fault()
        );
        assert!(
            NodeEvent::RouteDowngrade {
                output: DataId::new("image").unwrap(),
                reason: RouteDowngradeReason::PoolExhausted,
            }
            .is_fault(),
            "pool exhaustion is the metric §6.2 insists on surfacing"
        );
        assert!(
            !NodeEvent::RouteDowngrade {
                output: DataId::new("image").unwrap(),
                reason: RouteDowngradeReason::ConsumerDetached {
                    consumer: "detector/frames".parse().unwrap(),
                },
            }
            .is_fault()
        );
    }

    #[test]
    fn route_changes_name_their_output() {
        let upgrade = NodeEvent::RouteUpgrade {
            output: DataId::new("image").unwrap(),
            segment: ShmSegmentSpec::new("astrs/df/camera/3/image", 3, 32, 1 << 20),
            consumers: vec!["detector/frames".parse().unwrap()],
        };
        assert!(upgrade.is_route_change());
        assert_eq!(upgrade.output().map(DataId::as_str), Some("image"));

        let downgrade = NodeEvent::RouteDowngrade {
            output: DataId::new("image").unwrap(),
            reason: RouteDowngradeReason::PoolExhausted,
        };
        assert!(downgrade.is_route_change());
        assert!(!NodeEvent::AllInputsClosed.is_route_change());
    }

    #[test]
    fn input_route_changes_name_their_input() {
        let upgrade = NodeEvent::InputRouteUpgrade {
            input: DataId::new("frames").unwrap(),
            source: "camera/image".parse().unwrap(),
            segment: ShmSegmentSpec::new("astrs/df/camera/3/image", 3, 32, 1 << 20),
            consumer: "detector/frames".parse().unwrap(),
        };
        assert!(upgrade.is_route_change());
        assert!(upgrade.is_input_route_change());
        assert_eq!(upgrade.input().map(DataId::as_str), Some("frames"));
        assert_eq!(
            upgrade.output(),
            None,
            "an input-side change names no output"
        );
        assert_eq!(round_trip(&upgrade).unwrap(), upgrade);

        let downgrade = NodeEvent::InputRouteDowngrade {
            input: DataId::new("frames").unwrap(),
            reason: RouteDowngradeReason::SegmentClosed { generation: 3 },
        };
        assert!(downgrade.is_input_route_change());
        assert!(!downgrade.is_fault(), "a closed segment is not congestion");
        assert!(
            NodeEvent::InputRouteDowngrade {
                input: DataId::new("frames").unwrap(),
                reason: RouteDowngradeReason::PoolExhausted,
            }
            .is_fault(),
            "pool exhaustion is the metric §6.2 insists on surfacing, both ends"
        );
        assert_eq!(round_trip(&downgrade).unwrap(), downgrade);

        // The producer-side pair is not an input-side change, and vice versa.
        assert!(
            !NodeEvent::RouteUpgrade {
                output: DataId::new("image").unwrap(),
                segment: ShmSegmentSpec::new("astrs/df/camera/3/image", 3, 32, 1 << 20),
                consumers: Vec::new(),
            }
            .is_input_route_change()
        );
    }

    #[test]
    fn terminal_events_wind_the_node_down() {
        assert!(NodeEvent::AllInputsClosed.is_terminal());
        assert!(
            NodeEvent::Stop {
                cause: StopCause::Requested,
                grace: None
            }
            .is_terminal()
        );
        assert!(
            !NodeEvent::Reload {
                operator: None,
                path: None
            }
            .is_terminal()
        );
    }

    #[test]
    fn peer_events_name_their_peer() {
        let peer = NodeId::new("camera").unwrap();
        assert_eq!(
            NodeEvent::Restarted {
                peer: peer.clone(),
                generation: 2
            }
            .peer(),
            Some(&peer)
        );
        assert_eq!(NodeEvent::AllInputsClosed.peer(), None);
    }

    #[test]
    fn nan_values_survive_the_wire() {
        let mut metadata = Metadata::new(HlcTimestamp::new(1, 0));
        metadata.insert("nan", f64::NAN).unwrap();
        let input = NodeEvent::Input {
            id: DataId::new("frames").unwrap(),
            source: "camera/image".parse().unwrap(),
            metadata,
            payload: Vec::new(),
        };
        assert!(round_trip(&input).unwrap().bitwise_eq(&input));

        let param = NodeEvent::ParamUpdate {
            scope: ParamScope::Global,
            key: ParamKey::new("gain").unwrap(),
            value: Parameter::Float(f64::NAN),
        };
        assert!(round_trip(&param).unwrap().bitwise_eq(&param));
    }

    #[test]
    fn display_names_every_variant_without_panicking() {
        for sample in node_events().unwrap() {
            assert!(!sample.to_string().is_empty());
        }
    }
}
