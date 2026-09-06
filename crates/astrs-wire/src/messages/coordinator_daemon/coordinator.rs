//! `coordinator → daemon`: [`CoordinatorEvent`] (blueprint §7.3, §24.1).
//!
//! The coordinator owns the *plan* — which dataflows exist, which nodes belong
//! to them, where each node runs — and the daemons own the *execution*. This
//! family is the plan being handed down: build these, spawn those, stop that,
//! and here is the state you missed while you were disconnected.
//!
//! Frozen variant indices 0–16, plus three tail appends for dynamic
//! topology (§8, §17): [`CoordinatorEvent::ReplaceNode`],
//! [`CoordinatorEvent::AddEdge`], [`CoordinatorEvent::RemoveEdge`] — see
//! their own docs for why §24.1 froze the *validation* path
//! (`astrs_wire::ControlRequest`'s matching three verbs) without a
//! daemon-facing verb to carry it.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{CoordinatorEvent, DataflowId, FrameKind, StopCause, WireMessage};
//!
//! let stop = CoordinatorEvent::StopDataflow {
//!     dataflow: DataflowId::from_u128(1),
//!     grace: None,
//!     cause: StopCause::Requested,
//! };
//! assert_eq!(CoordinatorEvent::KIND, FrameKind::CoordinatorEvent);
//! assert_eq!(stop.variant_index(), 4);
//! assert_eq!(stop.dataflow(), Some(DataflowId::from_u128(1)));
//! ```

use core::fmt;

use astrs_time::HlcTimestamp;
use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::common::duration::DurationMs;
use crate::common::node::{InputSpec, NodeSpawnSpec};
use crate::common::route::RouteSpec;
use crate::common::status::StopCause;
use crate::frame::FrameKind;
use crate::ids::{
    BuildId, DaemonId, DataId, DataflowId, NodeId, OperatorId, ParamKey, PortRef, SubscriptionId,
};
use crate::messages::control::types::{LogQuery, ParamScope, TopicQuery};
use crate::messages::coordinator_daemon::types::{BuildStep, PeerRouteDirective, StateEntry};
use crate::messages::impl_wire_message;
use crate::metadata::Parameter;

/// The coordinator → daemon message family (§24.1).
///
/// # Examples
///
/// ```
/// use astrs_wire::{CoordinatorEvent, WireMessage};
///
/// let heartbeat = CoordinatorEvent::Heartbeat {
///     seq: 9,
///     sent_at: Default::default(),
/// };
/// assert_eq!(heartbeat.variant_name(), "Heartbeat");
/// assert!(heartbeat.is_liveness());
/// ```
// No `Eq`: `SetParam` carries a [`Parameter`], which may hold an `f64`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CoordinatorEvent {
    /// Liveness, every `heartbeat_interval` (§24.2: 5 s).
    #[oxicode(variant = 0)]
    Heartbeat {
        /// A counter that increases by one per heartbeat, so a daemon can see
        /// how many it missed.
        seq: u64,
        /// When the coordinator sent it.
        sent_at: HlcTimestamp,
    },
    /// Build the artefacts this daemon is responsible for.
    #[oxicode(variant = 1)]
    Build {
        /// The build these steps belong to.
        build: BuildId,
        /// The dataflow being built.
        dataflow: DataflowId,
        /// The steps to run, in order.
        steps: Vec<BuildStep>,
        /// The directory relative paths resolve against.
        working_dir: Option<String>,
    },
    /// Spawn one node.
    ///
    /// The specification is fully expanded — the daemon does not read the
    /// manifest — and carries the node's `generation`, the incarnation counter
    /// that makes every stale shared-memory mapping detectable (§6.2).
    #[oxicode(variant = 2)]
    Spawn {
        /// The expanded specification, including its generation.
        node: Box<NodeSpawnSpec>,
        /// The routes this node participates in, so the daemon can set up
        /// delivery before the process starts.
        routes: Vec<RouteSpec>,
        /// The name the dataflow was started under, for log lines.
        dataflow_name: Option<String>,
    },
    /// Every node of a dataflow has registered, cluster-wide.
    ///
    /// Until this arrives a daemon holds deliveries: a producer that starts
    /// before its consumers exist would drop the first messages of the run.
    #[oxicode(variant = 3)]
    AllNodesReady {
        /// The dataflow.
        dataflow: DataflowId,
        /// The nodes that failed to come up; empty when every node is ready.
        failed: Vec<NodeId>,
    },
    /// Stop every node of a dataflow on this daemon.
    #[oxicode(variant = 4)]
    StopDataflow {
        /// The dataflow to stop.
        dataflow: DataflowId,
        /// How long nodes get to finish before they are killed.
        grace: Option<DurationMs>,
        /// Why, so the nodes can be told (§24.1 `NodeEvent::Stop{cause}`).
        cause: StopCause,
    },
    /// Reload a node's code without restarting the process (§9.3).
    #[oxicode(variant = 5)]
    ReloadNode {
        /// The dataflow the node belongs to.
        dataflow: DataflowId,
        /// The node to reload.
        node: NodeId,
        /// The operator inside a runtime node, or `None` for the whole node.
        operator: Option<OperatorId>,
    },
    /// Send back a batch of log records.
    #[oxicode(variant = 6)]
    Logs {
        /// The request id to quote in the answer.
        request: u64,
        /// The dataflow whose logs are wanted; `None` means all of them.
        dataflow: Option<DataflowId>,
        /// One node's logs, or all of them.
        node: Option<NodeId>,
        /// The filter to apply before sending.
        query: LogQuery,
    },
    /// Restart one node, incrementing its generation.
    #[oxicode(variant = 7)]
    RestartNode {
        /// The dataflow the node belongs to.
        dataflow: DataflowId,
        /// The node to restart.
        node: NodeId,
        /// The generation the restarted node must use.
        generation: u64,
    },
    /// Stop one node.
    #[oxicode(variant = 8)]
    StopNode {
        /// The dataflow the node belongs to.
        dataflow: DataflowId,
        /// The node to stop.
        node: NodeId,
        /// How long it gets to finish before it is killed.
        grace: Option<DurationMs>,
        /// Why.
        cause: StopCause,
    },
    /// A parameter was written; propagate it to the nodes that read it.
    #[oxicode(variant = 9)]
    SetParam {
        /// The scope it was written in.
        scope: ParamScope,
        /// The key.
        key: ParamKey,
        /// The value.
        value: Parameter,
    },
    /// A parameter was deleted; propagate the deletion.
    #[oxicode(variant = 10)]
    DeleteParam {
        /// The scope it was deleted from.
        scope: ParamScope,
        /// The key.
        key: ParamKey,
    },
    /// Shut this daemon down (`astrs down`).
    #[oxicode(variant = 11)]
    Destroy {
        /// How long running nodes get to finish first.
        grace: Option<DurationMs>,
    },
    /// A peer daemon left the cluster; tear down the routes that crossed to it.
    #[oxicode(variant = 12)]
    PeerDisconnected {
        /// The daemon that went away.
        daemon: DaemonId,
        /// The dataflows that had routes to it.
        dataflows: Vec<DataflowId>,
    },
    /// The state a reconnecting daemon missed, as a numbered batch.
    #[oxicode(variant = 13)]
    StateCatchUp {
        /// The sequence of the **first** entry in this batch.
        seq: u64,
        /// The entries, in ascending sequence order.
        entries: Vec<StateEntry>,
        /// Whether this is the last batch of the catch-up.
        final_batch: bool,
    },
    /// Start tapping a topic for a CLI subscriber (§7.3 fan-out).
    #[oxicode(variant = 14)]
    TopicTapStart {
        /// The dataflow to tap.
        dataflow: DataflowId,
        /// The producer port to tap.
        port: PortRef,
        /// How the tap should be shaped.
        query: TopicQuery,
        /// The subscription to tag delivered frames with.
        subscription: SubscriptionId,
    },
    /// Stop tapping.
    #[oxicode(variant = 15)]
    TopicTapStop {
        /// The subscription to end.
        subscription: SubscriptionId,
    },
    /// The cross-daemon edges this daemon takes part in, and where to reach
    /// the daemon on the other side of each (blueprint §4.2, §6.4).
    ///
    /// Sent immediately **after** the [`Self::Spawn`] events of the same
    /// dispatch, never before: a daemon stamps a route with the producing
    /// node's `generation`, and a node it has not been told to spawn yet has
    /// no generation to stamp with (§12).
    #[oxicode(variant = 16)]
    PeerRoutes {
        /// The dataflow the edges belong to.
        dataflow: DataflowId,
        /// One directive per cross-daemon edge, in a stable order.
        directives: Vec<PeerRouteDirective>,
    },
    /// Swap a live node's declared shape for a new one (blueprint §8, §17
    /// `astrs node replace`): a tail append beyond §24.1, which froze
    /// dynamic topology's *validation* path (`ControlRequest::ReplaceNode`)
    /// without a matching daemon-facing verb to carry it.
    ///
    /// The daemon spawns `node` as a fresh generation while the current
    /// incarnation (if any) keeps running, and only asks the outgoing one
    /// to stop once the new one has registered — a brief, deliberate
    /// dual-run window (see `astrs_daemon::server::core::Daemon`'s own
    /// docs on `superseded` incarnations) that is what lets a reliable-path
    /// consumer see no gap in delivery across the swap.
    #[oxicode(variant = 17)]
    ReplaceNode {
        /// The dataflow the node belongs to.
        dataflow: DataflowId,
        /// The node's new specification. Its declared ports must be a
        /// subset the daemon's tracked routes still support — the
        /// coordinator validates this through `astrs_graph::apply` before
        /// this event is ever sent.
        node: Box<NodeSpawnSpec>,
    },
    /// Add (or rewire) one input edge on a live node (blueprint §8, §17
    /// `astrs node connect`) — a tail append beyond §24.1 for the same
    /// reason as [`Self::ReplaceNode`].
    ///
    /// Also used to *rewire* an existing input to a different producer:
    /// `astrs-graph`'s `diff` emits exactly one `AddEdge` (never a
    /// `RemoveEdge`/`AddEdge` pair) when only an edge's producer changed,
    /// and the daemon applies this the same way either time — its route
    /// table replaces rather than duplicates an entry for the same
    /// `(node, input)` pair.
    #[oxicode(variant = 18)]
    AddEdge {
        /// The dataflow the edge belongs to.
        dataflow: DataflowId,
        /// The consuming node.
        consumer: NodeId,
        /// The input's full specification — id, producer, queue policy.
        input: InputSpec,
    },
    /// Remove one input edge from a live node (blueprint §8, §17 `astrs
    /// node disconnect`) — a tail append beyond §24.1 for the same reason
    /// as [`Self::ReplaceNode`].
    ///
    /// The consumer is told [`crate::NodeEvent::InputClosed`] with
    /// [`crate::RouteCloseReason::ConsumerGone`] is *not* what this uses —
    /// the consumer is very much still there; the daemon's application of
    /// this simply stops delivering on the input rather than reporting a
    /// producer-side fault.
    #[oxicode(variant = 19)]
    RemoveEdge {
        /// The dataflow the edge belongs to.
        dataflow: DataflowId,
        /// The consuming node.
        consumer: NodeId,
        /// The input to remove.
        input: DataId,
    },
}

impl CoordinatorEvent {
    /// The dataflow this event concerns, when it concerns exactly one.
    #[must_use]
    pub fn dataflow(&self) -> Option<DataflowId> {
        match self {
            Self::Build { dataflow, .. }
            | Self::AllNodesReady { dataflow, .. }
            | Self::StopDataflow { dataflow, .. }
            | Self::ReloadNode { dataflow, .. }
            | Self::RestartNode { dataflow, .. }
            | Self::StopNode { dataflow, .. }
            | Self::TopicTapStart { dataflow, .. }
            | Self::PeerRoutes { dataflow, .. }
            | Self::ReplaceNode { dataflow, .. }
            | Self::AddEdge { dataflow, .. }
            | Self::RemoveEdge { dataflow, .. } => Some(*dataflow),
            Self::Spawn { node, .. } => Some(node.dataflow),
            Self::Logs { dataflow, .. } => *dataflow,
            Self::SetParam { scope, .. } | Self::DeleteParam { scope, .. } => scope.dataflow(),
            Self::Heartbeat { .. }
            | Self::Destroy { .. }
            | Self::PeerDisconnected { .. }
            | Self::StateCatchUp { .. }
            | Self::TopicTapStop { .. } => None,
        }
    }

    /// The node this event addresses, when it addresses one.
    #[must_use]
    pub const fn node(&self) -> Option<&NodeId> {
        match self {
            Self::ReloadNode { node, .. }
            | Self::RestartNode { node, .. }
            | Self::StopNode { node, .. } => Some(node),
            Self::Spawn { node, .. } => Some(&node.node),
            Self::ReplaceNode { node, .. } => Some(&node.node),
            Self::AddEdge { consumer, .. } | Self::RemoveEdge { consumer, .. } => Some(consumer),
            _ => None,
        }
    }

    /// Whether this event is a heartbeat and carries no instruction.
    #[must_use]
    pub const fn is_liveness(&self) -> bool {
        matches!(self, Self::Heartbeat { .. })
    }

    /// Whether this event tells the daemon to stop something.
    #[must_use]
    pub const fn is_stop(&self) -> bool {
        matches!(
            self,
            Self::StopDataflow { .. } | Self::StopNode { .. } | Self::Destroy { .. }
        )
    }

    /// The state-catch-up entries this event carries, if it is one.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::CoordinatorEvent;
    ///
    /// let catch_up = CoordinatorEvent::StateCatchUp {
    ///     seq: 41,
    ///     entries: Vec::new(),
    ///     final_batch: true,
    /// };
    /// assert_eq!(catch_up.catch_up_entries().map(<[_]>::len), Some(0));
    /// ```
    #[must_use]
    pub fn catch_up_entries(&self) -> Option<&[StateEntry]> {
        match self {
            Self::StateCatchUp { entries, .. } => Some(entries),
            _ => None,
        }
    }

    /// The highest sequence number in a catch-up batch, for the acknowledgement.
    ///
    /// Returns the batch's own `seq` when the batch is empty, so an empty
    /// final batch still advances the daemon's cursor.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_time::HlcTimestamp;
    /// use astrs_wire::{CoordinatorEvent, DaemonId, StateEntry, StateEntryKind};
    ///
    /// let entries = vec![StateEntry::new(
    ///     41,
    ///     HlcTimestamp::new(1, 0),
    ///     StateEntryKind::DaemonPresence {
    ///         daemon: DaemonId::generate(None),
    ///         connected: true,
    ///     },
    /// )];
    /// let event = CoordinatorEvent::StateCatchUp { seq: 41, entries, final_batch: true };
    /// assert_eq!(event.catch_up_high_water(), Some(41));
    /// ```
    #[must_use]
    pub fn catch_up_high_water(&self) -> Option<u64> {
        match self {
            Self::StateCatchUp { seq, entries, .. } => {
                Some(entries.iter().map(|entry| entry.seq).max().unwrap_or(*seq))
            }
            _ => None,
        }
    }

    /// The subscription this event opens or closes, if any.
    #[must_use]
    pub const fn subscription(&self) -> Option<SubscriptionId> {
        match self {
            Self::TopicTapStart { subscription, .. } | Self::TopicTapStop { subscription } => {
                Some(*subscription)
            }
            _ => None,
        }
    }

    /// Compares two events with `f64` bit patterns rather than IEEE equality —
    /// see [`crate::PeerEvent::bitwise_eq`].
    #[must_use]
    pub fn bitwise_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::SetParam {
                    scope: left_scope,
                    key: left_key,
                    value: left_value,
                },
                Self::SetParam {
                    scope: right_scope,
                    key: right_key,
                    value: right_value,
                },
            ) => {
                left_scope == right_scope
                    && left_key == right_key
                    && left_value.bitwise_eq(right_value)
            }
            (
                Self::StateCatchUp {
                    seq: left_seq,
                    entries: left_entries,
                    final_batch: left_final,
                },
                Self::StateCatchUp {
                    seq: right_seq,
                    entries: right_entries,
                    final_batch: right_final,
                },
            ) => {
                left_seq == right_seq
                    && left_final == right_final
                    && left_entries.len() == right_entries.len()
                    && left_entries
                        .iter()
                        .zip(right_entries)
                        .all(|(left, right)| left.bitwise_eq(right))
            }
            _ => self == other,
        }
    }
}

impl fmt::Display for CoordinatorEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Heartbeat { seq, .. } => write!(f, "heartbeat #{seq}"),
            Self::Build {
                build,
                dataflow,
                steps,
                ..
            } => write!(f, "build {build} of {dataflow} ({} step(s))", steps.len()),
            Self::Spawn { node, .. } => write!(
                f,
                "spawn {}/{} generation {}",
                node.dataflow, node.node, node.generation
            ),
            Self::AllNodesReady { dataflow, failed } => {
                if failed.is_empty() {
                    write!(f, "dataflow {dataflow} is ready")
                } else {
                    write!(f, "dataflow {dataflow} ready, {} failed", failed.len())
                }
            }
            Self::StopDataflow {
                dataflow, cause, ..
            } => write!(f, "stop dataflow {dataflow}: {cause}"),
            Self::ReloadNode {
                dataflow,
                node,
                operator,
            } => match operator {
                Some(operator) => write!(f, "reload {dataflow}/{node}/{operator}"),
                None => write!(f, "reload {dataflow}/{node}"),
            },
            Self::Logs { request, .. } => write!(f, "log request {request}"),
            Self::RestartNode {
                dataflow,
                node,
                generation,
            } => write!(f, "restart {dataflow}/{node} as generation {generation}"),
            Self::StopNode {
                dataflow,
                node,
                cause,
                ..
            } => write!(f, "stop {dataflow}/{node}: {cause}"),
            Self::SetParam { scope, key, .. } => write!(f, "set {key} in {scope}"),
            Self::DeleteParam { scope, key } => write!(f, "delete {key} in {scope}"),
            Self::Destroy { .. } => f.write_str("destroy daemon"),
            Self::PeerDisconnected { daemon, dataflows } => write!(
                f,
                "peer {daemon} disconnected ({} dataflow(s) affected)",
                dataflows.len()
            ),
            Self::StateCatchUp {
                seq,
                entries,
                final_batch,
            } => write!(
                f,
                "state catch-up from #{seq} ({} entries{})",
                entries.len(),
                if *final_batch { ", final" } else { "" }
            ),
            Self::TopicTapStart {
                port, subscription, ..
            } => write!(f, "tap {port} as subscription {subscription}"),
            Self::TopicTapStop { subscription } => write!(f, "stop tap {subscription}"),
            Self::PeerRoutes {
                dataflow,
                directives,
            } => write!(f, "{} peer route(s) for {dataflow}", directives.len()),
            Self::ReplaceNode { dataflow, node } => {
                write!(
                    f,
                    "replace {dataflow}/{} generation {}",
                    node.node, node.generation
                )
            }
            Self::AddEdge {
                dataflow,
                consumer,
                input,
            } => write!(
                f,
                "add edge {dataflow}/{consumer}.{} <- {}",
                input.id, input.source
            ),
            Self::RemoveEdge {
                dataflow,
                consumer,
                input,
            } => write!(f, "remove edge {dataflow}/{consumer}.{input}"),
        }
    }
}

impl_wire_message!(
    CoordinatorEvent,
    FrameKind::CoordinatorEvent,
    [
        "Heartbeat",
        "Build",
        "Spawn",
        "AllNodesReady",
        "StopDataflow",
        "ReloadNode",
        "Logs",
        "RestartNode",
        "StopNode",
        "SetParam",
        "DeleteParam",
        "Destroy",
        "PeerDisconnected",
        "StateCatchUp",
        "TopicTapStart",
        "TopicTapStop",
        "PeerRoutes",
        "ReplaceNode",
        "AddEdge",
        "RemoveEdge",
    ],
    fn variant_index(&self) -> u16 {
        match self {
            Self::Heartbeat { .. } => 0,
            Self::Build { .. } => 1,
            Self::Spawn { .. } => 2,
            Self::AllNodesReady { .. } => 3,
            Self::StopDataflow { .. } => 4,
            Self::ReloadNode { .. } => 5,
            Self::Logs { .. } => 6,
            Self::RestartNode { .. } => 7,
            Self::StopNode { .. } => 8,
            Self::SetParam { .. } => 9,
            Self::DeleteParam { .. } => 10,
            Self::Destroy { .. } => 11,
            Self::PeerDisconnected { .. } => 12,
            Self::StateCatchUp { .. } => 13,
            Self::TopicTapStart { .. } => 14,
            Self::TopicTapStop { .. } => 15,
            Self::PeerRoutes { .. } => 16,
            Self::ReplaceNode { .. } => 17,
            Self::AddEdge { .. } => 18,
            Self::RemoveEdge { .. } => 19,
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
    use crate::messages::coordinator_daemon::types::StateEntryKind;
    use crate::messages::samples::coordinator_events;

    #[test]
    fn the_family_has_the_seventeen_frozen_variants_plus_three_topology_tail_appends() {
        assert_eq!(CoordinatorEvent::VARIANT_NAMES.len(), 20);
        assert_eq!(CoordinatorEvent::VARIANT_NAMES[0], "Heartbeat");
        assert_eq!(CoordinatorEvent::VARIANT_NAMES[13], "StateCatchUp");
        assert_eq!(CoordinatorEvent::VARIANT_NAMES[15], "TopicTapStop");
        assert_eq!(CoordinatorEvent::VARIANT_NAMES[16], "PeerRoutes");
        assert_eq!(CoordinatorEvent::VARIANT_NAMES[17], "ReplaceNode");
        assert_eq!(CoordinatorEvent::VARIANT_NAMES[18], "AddEdge");
        assert_eq!(CoordinatorEvent::VARIANT_NAMES[19], "RemoveEdge");
    }

    #[test]
    fn the_three_topology_tail_appends_name_their_dataflow_and_node() {
        use crate::common::node::{InputSpec, NodeSource, NodeSpawnSpec};

        let dataflow = DataflowId::from_u128(9);
        let node = NodeId::new("camera").unwrap();
        let replace = CoordinatorEvent::ReplaceNode {
            dataflow,
            node: Box::new(NodeSpawnSpec::new(
                dataflow,
                node.clone(),
                2,
                NodeSource::Executable {
                    path: "./camera".into(),
                },
            )),
        };
        assert_eq!(replace.dataflow(), Some(dataflow));
        assert_eq!(replace.node(), Some(&node));

        let add_edge = CoordinatorEvent::AddEdge {
            dataflow,
            consumer: node.clone(),
            input: InputSpec::new(DataId::new("frames").unwrap(), "cam2/out".parse().unwrap()),
        };
        assert_eq!(add_edge.dataflow(), Some(dataflow));
        assert_eq!(add_edge.node(), Some(&node));

        let remove_edge = CoordinatorEvent::RemoveEdge {
            dataflow,
            consumer: node.clone(),
            input: DataId::new("frames").unwrap(),
        };
        assert_eq!(remove_edge.dataflow(), Some(dataflow));
        assert_eq!(remove_edge.node(), Some(&node));
    }

    #[test]
    fn every_variant_reports_and_encodes_its_frozen_index() {
        let samples = coordinator_events().unwrap();
        assert_eq!(samples.len(), CoordinatorEvent::VARIANT_NAMES.len());
        for (index, sample) in samples.iter().enumerate() {
            let index = u16::try_from(index).unwrap();
            assert_eq!(sample.variant_index(), index, "{sample:?}");
            assert_eq!(u16::from(sample.encode_to_vec().unwrap()[0]), index);
        }
    }

    #[test]
    fn every_variant_round_trips_through_a_frame() {
        let limits = FrameLimits::network();
        for sample in coordinator_events().unwrap() {
            let bytes = sample.to_frame(FrameFlags::CRC, &limits).unwrap();
            let decoded = CoordinatorEvent::from_bytes(&bytes, &limits).unwrap();
            assert!(decoded.bitwise_eq(&sample), "{sample:?}");
        }
    }

    #[test]
    fn trailing_bytes_after_an_event_are_refused() {
        for sample in coordinator_events().unwrap() {
            let mut bytes = sample.encode_to_vec().unwrap();
            bytes.push(9);
            assert!(matches!(
                CoordinatorEvent::decode_exact(&bytes),
                Err(crate::error::WireError::TrailingBytes { .. })
            ));
        }
    }

    #[test]
    fn a_spawn_carries_the_expanded_spec_and_its_generation() {
        let sample = coordinator_events()
            .unwrap()
            .into_iter()
            .find(|event| matches!(event, CoordinatorEvent::Spawn { .. }))
            .expect("the sample table covers Spawn");
        match &sample {
            CoordinatorEvent::Spawn { node, routes, .. } => {
                assert!(node.generation > 0, "a spawn always names a generation");
                assert!(!node.inputs.is_empty() || !node.outputs.is_empty());
                assert!(!routes.is_empty());
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
        assert_eq!(sample.node().map(NodeId::as_str), Some("camera"));
        assert!(sample.dataflow().is_some());
    }

    #[test]
    fn catch_up_reports_its_high_water_mark() {
        let entries = vec![
            StateEntry::new(
                41,
                HlcTimestamp::new(1, 0),
                StateEntryKind::DaemonPresence {
                    daemon: DaemonId::generate(None),
                    connected: true,
                },
            ),
            StateEntry::new(
                43,
                HlcTimestamp::new(2, 0),
                StateEntryKind::DaemonPresence {
                    daemon: DaemonId::generate(None),
                    connected: false,
                },
            ),
        ];
        let event = CoordinatorEvent::StateCatchUp {
            seq: 41,
            entries,
            final_batch: false,
        };
        assert_eq!(event.catch_up_high_water(), Some(43));
        assert_eq!(event.catch_up_entries().map(<[_]>::len), Some(2));

        let empty = CoordinatorEvent::StateCatchUp {
            seq: 50,
            entries: Vec::new(),
            final_batch: true,
        };
        assert_eq!(
            empty.catch_up_high_water(),
            Some(50),
            "an empty final batch still advances the cursor"
        );
        assert_eq!(
            CoordinatorEvent::Destroy { grace: None }.catch_up_high_water(),
            None
        );
    }

    #[test]
    fn stop_and_liveness_predicates_classify_the_family() {
        for sample in coordinator_events().unwrap() {
            if sample.is_liveness() {
                assert!(!sample.is_stop());
                assert_eq!(sample.dataflow(), None);
            }
        }
        assert!(
            CoordinatorEvent::StopDataflow {
                dataflow: DataflowId::from_u128(1),
                grace: None,
                cause: StopCause::Requested,
            }
            .is_stop()
        );
        assert!(CoordinatorEvent::Destroy { grace: None }.is_stop());
        assert!(
            !CoordinatorEvent::Heartbeat {
                seq: 1,
                sent_at: HlcTimestamp::EPOCH,
            }
            .is_stop()
        );
    }

    #[test]
    fn subscription_accessors_cover_the_tap_verbs() {
        let subscription = SubscriptionId::new(5);
        assert_eq!(
            CoordinatorEvent::TopicTapStop { subscription }.subscription(),
            Some(subscription)
        );
        assert_eq!(
            CoordinatorEvent::Destroy { grace: None }.subscription(),
            None
        );
    }

    #[test]
    fn nan_parameters_survive_the_wire() {
        let event = CoordinatorEvent::SetParam {
            scope: ParamScope::Global,
            key: ParamKey::new("gain").unwrap(),
            value: Parameter::Float(f64::NAN),
        };
        let bytes = event.encode_to_vec().unwrap();
        let decoded = CoordinatorEvent::decode_exact(&bytes).unwrap();
        assert_ne!(decoded, event);
        assert!(decoded.bitwise_eq(&event));
    }

    #[test]
    fn display_names_every_variant_without_panicking() {
        for sample in coordinator_events().unwrap() {
            assert!(!sample.to_string().is_empty());
        }
    }
}
