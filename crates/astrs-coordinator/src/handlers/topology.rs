//! `RestartNode`/`StopNode`/`AddNode`/`RemoveNode`/`ReplaceNode`/`AddEdge`/
//! `RemoveEdge` (blueprint §5.2, §8, §17 dynamic topology; §12 restart).
//!
//! Every one of the five topology-mutating verbs (`AddNode`/`RemoveNode`/
//! `ReplaceNode`/`AddEdge`/`RemoveEdge`) follows the same shape:
//!
//! 1. Validate the request against the coordinator's own tracked
//!    [`astrs_graph::DataflowGraph`] via [`astrs_graph::apply`] — a
//!    *structurally* invalid request (an unknown producer, an id
//!    collision, a dangling edge, a newly-introduced strict-mode type
//!    mismatch) is rejected with [`astrs_wire::ErrorCode::FailedPrecondition`]
//!    before anything else, carrying the offending
//!    [`astrs_graph::Diagnostic`]s in the reply's `context` (see
//!    [`crate::error::CoordinatorError::into_reply`]) rather than a bare
//!    count.
//! 2. Commit the validated candidate graph as `live.graph` — from this
//!    point the change is real for every *other* concurrent request this
//!    coordinator process serves, whether or not the daemon has heard
//!    about it yet.
//! 3. Best-effort record every applied [`astrs_graph::TopologyOp`] to
//!    `astrs-store`'s mutation log ([`persist_topology_ops`]) — durable
//!    enough that a reconnecting daemon's or CLI's `List`/`Info` reflect
//!    the change, and enough for a coordinator that restarts to rebuild
//!    the identical graph (`crate::graph_bridge`'s `rebuild_graph`), but
//!    never a reason to refuse a mutation that has already, correctly,
//!    taken effect on step 2 — a store hiccup is logged, not surfaced to
//!    the caller.
//! 4. Dispatch the matching `CoordinatorEvent` to the daemon that hosts
//!    the affected node: `Spawn`/`StopNode` (the pre-existing, frozen
//!    verbs) for `AddNode`/`RemoveNode`, and three tail-appended verbs —
//!    [`astrs_wire::CoordinatorEvent::ReplaceNode`],
//!    [`astrs_wire::CoordinatorEvent::AddEdge`],
//!    [`astrs_wire::CoordinatorEvent::RemoveEdge`] — added beyond §24.1's
//!    originally frozen set for the other three (append-only protocol
//!    evolution, blueprint principle 4; see those variants' own docs for
//!    the daemon-side application, [`astrs_daemon`]'s
//!    `dataflow::topology` module).

use astrs_graph::TopologyOp;
use astrs_wire::{
    ControlReply, CoordinatorEvent, DaemonId, DataId, DataflowId, DurationMs, InputSpec, NodeId,
    NodeSpawnSpec, StopCause,
};

use crate::coordinator::Coordinator;
use crate::error::CoordinatorError;
use crate::graph_bridge;
use crate::placement;

/// `RestartNode`: bumps the node's generation and asks its current
/// daemon to restart it.
pub async fn restart_node(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    node: NodeId,
) -> ControlReply {
    let (daemon_id, generation) = {
        let mut dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get_mut(dataflow) else {
            return CoordinatorError::NoSuchDataflow(dataflow).into_reply();
        };
        let Some(daemon_id) = live.daemon_for_node(&node).cloned() else {
            return CoordinatorError::NoSuchNode { dataflow, node }.into_reply();
        };
        (daemon_id, live.advance_generation(&node))
    };
    let event = CoordinatorEvent::RestartNode {
        dataflow,
        node,
        generation,
    };
    send_to(coordinator, &daemon_id, event)
}

/// `StopNode`.
pub async fn stop_node(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    node: NodeId,
    grace: Option<DurationMs>,
) -> ControlReply {
    let daemon_id = {
        let dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get(dataflow) else {
            return CoordinatorError::NoSuchDataflow(dataflow).into_reply();
        };
        let Some(daemon_id) = live.daemon_for_node(&node).cloned() else {
            return CoordinatorError::NoSuchNode { dataflow, node }.into_reply();
        };
        daemon_id
    };
    let event = CoordinatorEvent::StopNode {
        dataflow,
        node,
        grace,
        cause: StopCause::Requested,
    };
    send_to(coordinator, &daemon_id, event)
}

/// `AddNode`: places a fully-specified node onto a running dataflow.
///
/// Unlike a manifest-declared node, the caller supplies the complete
/// [`NodeSpawnSpec`] directly — there is no manifest text for this crate
/// to expand — so this only needs to resolve placement, track the
/// addition and, if `start`, dispatch the spawn.
pub async fn add_node(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    mut node: Box<NodeSpawnSpec>,
    start: bool,
) -> ControlReply {
    let (daemon_id, ops) = {
        let daemons = coordinator.daemons();
        let mut dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get_mut(dataflow) else {
            return CoordinatorError::NoSuchDataflow(dataflow).into_reply();
        };
        if live.daemon_for_node(&node.node).is_some() {
            return CoordinatorError::AlreadyExists {
                kind: "node",
                name: node.node.to_string(),
            }
            .into_reply();
        }

        // Validate against the tracked graph *before* touching placement
        // or dispatching anything: a request that would corrupt the
        // graph (an id the graph already has even though the placement
        // map above did not, an input wired to a producer/output that
        // does not exist) must fail before any daemon hears about it —
        // see [`astrs_graph::apply`]'s docs for exactly what this checks.
        let ops = graph_bridge::topology_ops_for_add_node(&node);
        let candidate = match astrs_graph::apply(&live.graph, &ops) {
            Ok(candidate) => candidate,
            Err(err) => return CoordinatorError::Apply(err).into_reply(),
        };

        let machine = node
            .deploy
            .machine
            .as_ref()
            .map(|name| astrs_graph::MachineId::Named(name.to_string()))
            .unwrap_or(astrs_graph::MachineId::CoordinatorLocal);
        let daemon_id = match placement::resolve_machine(&machine, &daemons) {
            Ok(id) => id,
            Err(err) => return err.into_reply(),
        };
        // Only committed once every fallible step above has succeeded —
        // `astrs_graph::apply` never mutates its input, so a rejected
        // candidate leaves `live.graph` exactly as it was.
        live.graph = candidate;
        live.placement
            .dynamic_daemons
            .insert(node.node.clone(), daemon_id.clone());
        live.generations
            .insert(node.node.clone(), node.generation.max(1));
        (daemon_id, ops)
    };
    persist_topology_ops(coordinator, dataflow, &ops).await;
    node.dataflow = dataflow;
    if node.generation == 0 {
        node.generation = 1;
    }

    let name = coordinator
        .store
        .get_dataflow_meta(dataflow)
        .await
        .ok()
        .flatten()
        .and_then(|meta| meta.name);

    if !start {
        return ControlReply::Ok;
    }
    let event = CoordinatorEvent::Spawn {
        node,
        routes: Vec::new(),
        dataflow_name: name,
    };
    send_to(coordinator, &daemon_id, event)
}

/// `RemoveNode`: removes the node from the tracked graph — along with
/// every edge that referenced it, so the graph never carries a dangling
/// reference (see [`crate::graph_bridge::topology_ops_for_remove_node`])
/// — then asks the hosting daemon to stop it; the coordinator forgets the
/// placement once `NodeStopped` confirms it (see `crate::session::daemon`).
pub async fn remove_node(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    node: NodeId,
    grace: Option<DurationMs>,
) -> ControlReply {
    let ops = {
        let mut dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get_mut(dataflow) else {
            return CoordinatorError::NoSuchDataflow(dataflow).into_reply();
        };
        if live.daemon_for_node(&node).is_none() {
            return CoordinatorError::NoSuchNode { dataflow, node }.into_reply();
        }
        let graph_id = graph_bridge::wire_node_id_to_graph(&node);
        let ops = graph_bridge::topology_ops_for_remove_node(&live.graph, &graph_id);
        match astrs_graph::apply(&live.graph, &ops) {
            Ok(candidate) => {
                live.graph = candidate;
                ops
            }
            Err(err) => return CoordinatorError::Apply(err).into_reply(),
        }
    };
    persist_topology_ops(coordinator, dataflow, &ops).await;
    stop_node(coordinator, dataflow, node, grace).await
}

/// `ReplaceNode`.
///
/// Validates the swap against the tracked graph — the node's *new*
/// declared shape, its *existing* edges otherwise untouched — commits and
/// persists it exactly like [`add_node`]/[`remove_node`], then dispatches
/// [`CoordinatorEvent::ReplaceNode`] to whichever daemon already hosts
/// this node (a replace never moves a node to a different daemon; see
/// [`astrs_daemon`]'s `dataflow::topology` module for the dual-run cutover
/// this triggers there). `drain` is accepted (blueprint §17's `astrs node
/// replace` surface) but not yet acted on: the daemon-side cutover always
/// runs its own brief dual-run window regardless, so a caller asking to
/// drain first sees the same behavior as one who did not — tracked as a
/// deviation in this crate's final report, not silently ignored.
pub async fn replace_node(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    node: Box<NodeSpawnSpec>,
    _drain: bool,
) -> ControlReply {
    let outgoing = node.node.clone();
    let (daemon_id, op) = {
        let mut dataflows = coordinator.dataflows();
        let Some(live) = dataflows.get_mut(dataflow) else {
            return CoordinatorError::NoSuchDataflow(dataflow).into_reply();
        };
        let graph_id = graph_bridge::wire_node_id_to_graph(&outgoing);
        let Some(old) = live.graph.node(&graph_id).cloned() else {
            return CoordinatorError::NoSuchNode {
                dataflow,
                node: outgoing,
            }
            .into_reply();
        };
        let Some(daemon_id) = live.daemon_for_node(&outgoing).cloned() else {
            return CoordinatorError::NoSuchNode {
                dataflow,
                node: outgoing,
            }
            .into_reply();
        };
        let new = graph_bridge::graph_node_for_spawn_spec(&node);
        let op = TopologyOp::ReplaceNode {
            id: graph_id,
            old,
            new,
        };
        match astrs_graph::apply(&live.graph, std::slice::from_ref(&op)) {
            Ok(candidate) => {
                live.graph = candidate;
                (daemon_id, op)
            }
            Err(err) => return CoordinatorError::Apply(err).into_reply(),
        }
    };
    persist_topology_ops(coordinator, dataflow, std::slice::from_ref(&op)).await;
    let event = CoordinatorEvent::ReplaceNode { dataflow, node };
    send_to(coordinator, &daemon_id, event)
}

/// `AddEdge`.
///
/// Adds — or, for an already-wired input, rewires — one input edge on a
/// live node: validated, committed and persisted like every other verb in
/// this module, then dispatched as [`CoordinatorEvent::AddEdge`] to the
/// consumer's own daemon. [`astrs_graph::diff`] emits exactly this one op
/// (never a `RemoveEdge`/`AddEdge` pair) when only an edge's producer
/// changed, and [`astrs_daemon`]'s `apply_add_edge` applies a rewire the
/// same way a fresh wire, so this handler needs no separate "is this new
/// or a rewire" branch either.
pub async fn add_edge(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    consumer: NodeId,
    input: InputSpec,
) -> ControlReply {
    let (daemon_id, op) = match commit_edge_op(coordinator, dataflow, &consumer, |graph_consumer| {
        TopologyOp::AddEdge {
            key: astrs_graph::EdgeKey::new(
                graph_consumer,
                astrs_graph::PortName::new(input.id.as_str().to_owned()),
            ),
            edge: graph_bridge::edge_for_input_spec(&input),
        }
    }) {
        Ok(committed) => committed,
        Err(reply) => return reply,
    };
    persist_topology_ops(coordinator, dataflow, std::slice::from_ref(&op)).await;
    let event = CoordinatorEvent::AddEdge {
        dataflow,
        consumer,
        input,
    };
    send_to(coordinator, &daemon_id, event)
}

/// `RemoveEdge`.
///
/// Disconnects one input edge on a live node — validated, committed and
/// persisted like [`add_edge`], then dispatched as
/// [`CoordinatorEvent::RemoveEdge`]. Unlike a producer-side failure
/// ([`astrs_wire::RouteCloseReason::ConsumerGone`]), the consumer is very
/// much still there; [`astrs_daemon`]'s `apply_remove_edge` tells it
/// [`astrs_wire::NodeEvent::InputClosed`] with
/// [`astrs_wire::RouteCloseReason::Disconnected`] instead.
pub async fn remove_edge(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    consumer: NodeId,
    input: DataId,
) -> ControlReply {
    let (daemon_id, op) = match commit_edge_op(coordinator, dataflow, &consumer, |graph_consumer| {
        TopologyOp::RemoveEdge {
            key: astrs_graph::EdgeKey::new(
                graph_consumer,
                astrs_graph::PortName::new(input.as_str().to_owned()),
            ),
        }
    }) {
        Ok(committed) => committed,
        Err(reply) => return reply,
    };
    persist_topology_ops(coordinator, dataflow, std::slice::from_ref(&op)).await;
    let event = CoordinatorEvent::RemoveEdge {
        dataflow,
        consumer,
        input,
    };
    send_to(coordinator, &daemon_id, event)
}

/// Shared core of [`add_edge`]/[`remove_edge`]: resolves `consumer`'s
/// hosting daemon, builds the one-op edge change `build_op` describes
/// (given the consumer's id already translated to
/// [`astrs_graph::NodeId`], so neither caller repeats that translation),
/// validates it against the tracked graph and, only once every fallible
/// step has succeeded, commits the candidate as `live.graph` — returning
/// the daemon to dispatch to and the op to persist, for the caller to do
/// both once this function's lock has been released (persistence is
/// `async`; see this module's top-level docs on why it never runs with
/// `coordinator.dataflows()`'s guard held).
///
/// # Errors
///
/// The already-typed [`ControlReply`] a caller returns verbatim:
/// [`CoordinatorError::NoSuchDataflow`]/[`CoordinatorError::NoSuchNode`]
/// if `consumer` is not one this dataflow currently hosts on a connected
/// daemon, or [`CoordinatorError::Apply`] if `build_op`'s change fails
/// [`astrs_graph::apply`]'s validation.
fn commit_edge_op(
    coordinator: &Coordinator,
    dataflow: DataflowId,
    consumer: &NodeId,
    build_op: impl FnOnce(astrs_graph::NodeId) -> TopologyOp,
) -> std::result::Result<(DaemonId, TopologyOp), ControlReply> {
    let mut dataflows = coordinator.dataflows();
    let Some(live) = dataflows.get_mut(dataflow) else {
        return Err(CoordinatorError::NoSuchDataflow(dataflow).into_reply());
    };
    let Some(daemon_id) = live.daemon_for_node(consumer).cloned() else {
        return Err(CoordinatorError::NoSuchNode {
            dataflow,
            node: consumer.clone(),
        }
        .into_reply());
    };
    let op = build_op(graph_bridge::wire_node_id_to_graph(consumer));
    match astrs_graph::apply(&live.graph, std::slice::from_ref(&op)) {
        Ok(candidate) => {
            live.graph = candidate;
            Ok((daemon_id, op))
        }
        Err(err) => Err(CoordinatorError::Apply(err).into_reply()),
    }
}

/// Sends `event` to `daemon_id`, converting a missing/closed connection
/// into the matching typed reply.
fn send_to(
    coordinator: &Coordinator,
    daemon_id: &astrs_wire::DaemonId,
    event: CoordinatorEvent,
) -> ControlReply {
    let daemons = coordinator.daemons();
    match daemons.get(daemon_id) {
        Some(handle) => match handle.send(event) {
            Ok(()) => ControlReply::Ok,
            Err(err) => err.into_reply(),
        },
        None => CoordinatorError::DaemonNotConnected(daemon_id.clone()).into_reply(),
    }
}

/// Best-effort mutation-log record for every op in `ops`, in order
/// (blueprint §8, §17: "applied topology ops recorded in the store
/// mutation log so reconnecting daemons/CLI see the current graph").
///
/// Called only *after* the caller has already committed the matching
/// candidate to `live.graph` — by the time this runs, the change is
/// already real for this coordinator process, so a failure here (a
/// `serde_json` encode error, which never happens for a well-formed
/// [`TopologyOp`]; or [`astrs_store::Error`], which realistically means the
/// durable backend itself is in trouble) is logged and swallowed rather
/// than retroactively refusing a mutation that has already, correctly,
/// taken effect. What it costs when it does fail: a coordinator that
/// later restarts (`crate::graph_bridge::rebuild_graph`) rebuilds a graph
/// missing this one op — a gap in the *recovery* journal, not in the
/// running cluster, which is why every caller in this module awaits this
/// without inspecting a return value.
async fn persist_topology_ops(coordinator: &Coordinator, dataflow: DataflowId, ops: &[TopologyOp]) {
    for op in ops {
        let op_json = match serde_json::to_string(op) {
            Ok(json) => json,
            Err(err) => {
                tracing::warn!(
                    %dataflow,
                    %err,
                    "a topology op could not be serialized for the mutation log"
                );
                continue;
            }
        };
        if let Err(err) = coordinator
            .store
            .record_topology_op(dataflow, op_json)
            .await
        {
            tracing::warn!(
                %dataflow,
                %err,
                "a topology op was applied but could not be durably recorded"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::registry::{DaemonHandle, LiveDataflow};
    use astrs_time::HlcTimestamp;
    use astrs_wire::{AuthToken, NodeSource, SessionId};
    use tokio::sync::mpsc;

    fn coordinator() -> Coordinator {
        Coordinator::open_in_memory(
            CoordinatorConfig::new(AuthToken::from_bytes([7; 32])).with_port(0),
        )
        .unwrap()
    }

    fn live_dataflow(id: DataflowId) -> LiveDataflow {
        let manifest =
            astrs_manifest::Manifest::from_yaml_str("nodes:\n  - id: a\n    path: ./a\n").unwrap();
        let (graph, _) = astrs_graph::DataflowGraph::from_manifest(&manifest).unwrap();
        LiveDataflow::new(id, None, manifest, None, graph)
    }

    fn connect_daemon(
        coordinator: &Coordinator,
    ) -> (astrs_wire::DaemonId, mpsc::Receiver<CoordinatorEvent>) {
        let id = astrs_wire::DaemonId::generate(None);
        let (tx, rx) = mpsc::channel(8);
        coordinator.daemons().insert(DaemonHandle::new(
            id.clone(),
            None,
            SessionId::generate(),
            tx,
            HlcTimestamp::EPOCH,
        ));
        (id, rx)
    }

    #[tokio::test]
    async fn restart_node_advances_the_generation_and_dispatches() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let (daemon_id, mut rx) = connect_daemon(&coordinator);
        let mut df = live_dataflow(dataflow);
        df.placement
            .node_daemon
            .insert(NodeId::new("a").unwrap(), daemon_id);
        coordinator.dataflows().insert(df);

        let reply = restart_node(&coordinator, dataflow, NodeId::new("a").unwrap()).await;
        assert_eq!(reply, ControlReply::Ok);
        match rx.try_recv().unwrap() {
            CoordinatorEvent::RestartNode { generation, .. } => assert_eq!(generation, 1),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn restart_node_on_a_missing_node_is_not_found() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        coordinator.dataflows().insert(live_dataflow(dataflow));
        let reply = restart_node(&coordinator, dataflow, NodeId::new("ghost").unwrap()).await;
        assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::NotFound));
    }

    #[tokio::test]
    async fn stop_node_dispatches_to_the_hosting_daemon() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let (daemon_id, mut rx) = connect_daemon(&coordinator);
        let mut df = live_dataflow(dataflow);
        df.placement
            .node_daemon
            .insert(NodeId::new("a").unwrap(), daemon_id);
        coordinator.dataflows().insert(df);

        let reply = stop_node(&coordinator, dataflow, NodeId::new("a").unwrap(), None).await;
        assert_eq!(reply, ControlReply::Ok);
        assert!(matches!(
            rx.try_recv().unwrap(),
            CoordinatorEvent::StopNode { .. }
        ));
    }

    #[tokio::test]
    async fn add_node_resolves_coordinator_local_placement_and_spawns() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let (_daemon_id, mut rx) = connect_daemon(&coordinator);
        coordinator.dataflows().insert(live_dataflow(dataflow));

        let spec = NodeSpawnSpec::new(
            dataflow,
            NodeId::new("extra").unwrap(),
            0,
            NodeSource::Executable {
                path: "./extra".into(),
            },
        );
        let reply = add_node(&coordinator, dataflow, Box::new(spec), true).await;
        assert_eq!(reply, ControlReply::Ok);
        match rx.try_recv().unwrap() {
            CoordinatorEvent::Spawn { node, .. } => assert_eq!(node.generation, 1),
            other => panic!("unexpected {other:?}"),
        }
        assert!(
            coordinator
                .dataflows()
                .get(dataflow)
                .unwrap()
                .daemon_for_node(&NodeId::new("extra").unwrap())
                .is_some()
        );
    }

    #[tokio::test]
    async fn add_node_without_start_does_not_dispatch() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let (_daemon_id, mut rx) = connect_daemon(&coordinator);
        coordinator.dataflows().insert(live_dataflow(dataflow));

        let spec = NodeSpawnSpec::new(
            dataflow,
            NodeId::new("extra").unwrap(),
            0,
            NodeSource::Executable {
                path: "./extra".into(),
            },
        );
        let reply = add_node(&coordinator, dataflow, Box::new(spec), false).await;
        assert_eq!(reply, ControlReply::Ok);
        assert!(rx.try_recv().is_err(), "nothing dispatched without `start`");
    }

    #[tokio::test]
    async fn add_node_rejects_a_colliding_id() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let (daemon_id, _rx) = connect_daemon(&coordinator);
        let mut df = live_dataflow(dataflow);
        df.placement
            .node_daemon
            .insert(NodeId::new("a").unwrap(), daemon_id);
        coordinator.dataflows().insert(df);

        let spec = NodeSpawnSpec::new(
            dataflow,
            NodeId::new("a").unwrap(),
            0,
            NodeSource::Executable { path: "./a".into() },
        );
        let reply = add_node(&coordinator, dataflow, Box::new(spec), true).await;
        assert_eq!(
            reply.error_code(),
            Some(astrs_wire::ErrorCode::AlreadyExists)
        );
    }

    #[tokio::test]
    async fn add_node_naming_a_producer_that_does_not_exist_is_rejected_before_any_placement_work()
    {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let (daemon_id, mut rx) = connect_daemon(&coordinator);
        let mut df = live_dataflow(dataflow);
        df.placement
            .node_daemon
            .insert(NodeId::new("a").unwrap(), daemon_id);
        coordinator.dataflows().insert(df);

        let mut spec = NodeSpawnSpec::new(
            dataflow,
            NodeId::new("extra").unwrap(),
            0,
            NodeSource::Executable {
                path: "./extra".into(),
            },
        );
        spec.inputs.push(InputSpec::new(
            DataId::new("in").unwrap(),
            "ghost/out".parse().unwrap(),
        ));
        let reply = add_node(&coordinator, dataflow, Box::new(spec), true).await;
        assert_eq!(
            reply.error_code(),
            Some(astrs_wire::ErrorCode::FailedPrecondition)
        );
        assert!(
            rx.try_recv().is_err(),
            "an invalid node must never reach a daemon"
        );
        assert!(
            coordinator
                .dataflows()
                .get(dataflow)
                .unwrap()
                .graph
                .node(&astrs_graph::NodeId::new("extra"))
                .is_none(),
            "a rejected candidate must never be committed to the tracked graph"
        );
    }

    #[tokio::test]
    async fn add_node_with_no_daemons_connected_is_unavailable() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        coordinator.dataflows().insert(live_dataflow(dataflow));
        let spec = NodeSpawnSpec::new(
            dataflow,
            NodeId::new("extra").unwrap(),
            0,
            NodeSource::Executable {
                path: "./extra".into(),
            },
        );
        let reply = add_node(&coordinator, dataflow, Box::new(spec), true).await;
        assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::Unavailable));
    }

    /// A dataflow with a real, wired edge to validate `ReplaceNode`/
    /// `AddEdge`/`RemoveEdge` against — `live_dataflow`'s single unwired
    /// node cannot exercise any of those three's graph validation at all.
    fn wired_live_dataflow(id: DataflowId) -> LiveDataflow {
        let manifest = astrs_manifest::Manifest::from_yaml_str(
            "nodes:\n  \
             - id: producer\n    path: ./producer\n    outputs: [out]\n  \
             - id: producer2\n    path: ./producer2\n    outputs: [out2]\n  \
             - id: a\n    path: ./a\n    inputs: { in: producer/out }\n",
        )
        .unwrap();
        let (graph, diagnostics) = astrs_graph::DataflowGraph::from_manifest(&manifest).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        LiveDataflow::new(id, None, manifest, None, graph)
    }

    #[tokio::test]
    async fn replace_node_dispatches_to_the_hosting_daemon_and_updates_the_tracked_graph() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let (daemon_id, mut rx) = connect_daemon(&coordinator);
        let mut df = wired_live_dataflow(dataflow);
        df.placement
            .node_daemon
            .insert(NodeId::new("a").unwrap(), daemon_id);
        coordinator.dataflows().insert(df);

        let mut spec = NodeSpawnSpec::new(
            dataflow,
            NodeId::new("a").unwrap(),
            1,
            NodeSource::Executable { path: "./a".into() },
        );
        spec.inputs.push(InputSpec::new(
            DataId::new("in").unwrap(),
            "producer/out".parse().unwrap(),
        ));
        spec.outputs.push(astrs_wire::OutputSpec::new(
            DataId::new("extra-out").unwrap(),
        ));
        let reply = replace_node(&coordinator, dataflow, Box::new(spec), false).await;
        assert_eq!(reply, ControlReply::Ok);

        match rx.try_recv().unwrap() {
            CoordinatorEvent::ReplaceNode {
                dataflow: got,
                node,
            } => {
                assert_eq!(got, dataflow);
                assert_eq!(node.node.as_str(), "a");
                assert_eq!(node.outputs.len(), 1, "the new shape was carried as-is");
            }
            other => panic!("unexpected {other:?}"),
        }

        let dataflows = coordinator.dataflows();
        let live = dataflows.get(dataflow).unwrap();
        let node = live.graph.node(&astrs_graph::NodeId::new("a")).unwrap();
        assert_eq!(
            node.outputs.len(),
            1,
            "the tracked graph reflects the replacement's new shape"
        );
    }

    #[tokio::test]
    async fn replace_node_on_a_node_with_no_daemon_is_not_found() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        coordinator
            .dataflows()
            .insert(wired_live_dataflow(dataflow));
        // "a" is in the graph (`wired_live_dataflow` puts it there) but no
        // `node_daemon`/`dynamic_daemons` entry was ever inserted for it in
        // this test — the placement-not-found path, distinct from the
        // graph-not-found path `replace_node_on_a_missing_node_is_not_found_not_unsupported`
        // already covers.
        let spec = NodeSpawnSpec::new(
            dataflow,
            NodeId::new("a").unwrap(),
            1,
            NodeSource::Executable { path: "./a".into() },
        );
        let reply = replace_node(&coordinator, dataflow, Box::new(spec), false).await;
        assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::NotFound));
    }

    #[tokio::test]
    async fn add_edge_dispatches_and_rewires_the_tracked_graph() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let (daemon_id, mut rx) = connect_daemon(&coordinator);
        let mut df = wired_live_dataflow(dataflow);
        df.placement
            .node_daemon
            .insert(NodeId::new("a").unwrap(), daemon_id);
        coordinator.dataflows().insert(df);

        // Re-wiring the *existing* declared input "in" to a different,
        // equally real producer — exactly the "rewire" case
        // `astrs_graph::diff` itself documents as one `AddEdge`, no
        // Remove/Add pair.
        let new_input = InputSpec::new(
            DataId::new("in").unwrap(),
            "producer2/out2".parse().unwrap(),
        );
        let reply = add_edge(
            &coordinator,
            dataflow,
            NodeId::new("a").unwrap(),
            new_input.clone(),
        )
        .await;
        assert_eq!(reply, ControlReply::Ok);

        match rx.try_recv().unwrap() {
            CoordinatorEvent::AddEdge {
                dataflow: got,
                consumer,
                input,
            } => {
                assert_eq!(got, dataflow);
                assert_eq!(consumer.as_str(), "a");
                assert_eq!(input.source.to_string(), "producer2/out2");
            }
            other => panic!("unexpected {other:?}"),
        }

        let dataflows = coordinator.dataflows();
        let live = dataflows.get(dataflow).unwrap();
        let edge = live
            .graph
            .edge(&astrs_graph::EdgeKey::new(
                astrs_graph::NodeId::new("a"),
                astrs_graph::PortName::new("in".to_owned()),
            ))
            .unwrap();
        assert!(
            matches!(&edge.from, astrs_graph::EdgeSource::NodeOutput { node, .. } if node.as_str() == "producer2"),
            "the tracked graph now reads from the new producer"
        );

        // The mutation log durably has this op, in `TopologyOp` form —
        // reconnecting daemons/CLI (`List`/`Info`) and a restarted
        // coordinator's graph rebuild both read from exactly this.
        let batch = coordinator
            .store
            .sync()
            .mutations_since(astrs_store::record::MutationSeq::ZERO, 10)
            .unwrap();
        assert!(
            batch.entries.iter().any(|entry| matches!(
                &entry.op,
                astrs_store::record::MutationOp::TopologyOpApplied { dataflow: d, op_json }
                    if *d == dataflow && op_json.contains("AddEdge")
            )),
            "the applied AddEdge was recorded to the mutation log"
        );
    }

    #[tokio::test]
    async fn remove_edge_dispatches_and_drops_the_tracked_edge() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let (daemon_id, mut rx) = connect_daemon(&coordinator);
        let mut df = wired_live_dataflow(dataflow);
        df.placement
            .node_daemon
            .insert(NodeId::new("a").unwrap(), daemon_id);
        coordinator.dataflows().insert(df);

        let reply = remove_edge(
            &coordinator,
            dataflow,
            NodeId::new("a").unwrap(),
            DataId::new("in").unwrap(),
        )
        .await;
        assert_eq!(reply, ControlReply::Ok);

        match rx.try_recv().unwrap() {
            CoordinatorEvent::RemoveEdge {
                dataflow: got,
                consumer,
                input,
            } => {
                assert_eq!(got, dataflow);
                assert_eq!(consumer.as_str(), "a");
                assert_eq!(input.as_str(), "in");
            }
            other => panic!("unexpected {other:?}"),
        }

        let dataflows = coordinator.dataflows();
        let live = dataflows.get(dataflow).unwrap();
        assert!(
            live.graph
                .edge(&astrs_graph::EdgeKey::new(
                    astrs_graph::NodeId::new("a"),
                    astrs_graph::PortName::new("in".to_owned())
                ))
                .is_none(),
            "the tracked graph no longer has the removed edge"
        );
    }

    #[tokio::test]
    async fn add_edge_and_remove_edge_on_a_node_with_no_daemon_are_not_found() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        coordinator
            .dataflows()
            .insert(wired_live_dataflow(dataflow));

        let reply = add_edge(
            &coordinator,
            dataflow,
            NodeId::new("a").unwrap(),
            InputSpec::new(
                DataId::new("in").unwrap(),
                "producer2/out2".parse().unwrap(),
            ),
        )
        .await;
        assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::NotFound));

        let reply = remove_edge(
            &coordinator,
            dataflow,
            NodeId::new("a").unwrap(),
            DataId::new("in").unwrap(),
        )
        .await;
        assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::NotFound));
    }

    #[tokio::test]
    async fn add_edge_naming_a_producer_that_does_not_exist_is_a_failed_precondition_not_unsupported()
     {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let (daemon_id, _rx) = connect_daemon(&coordinator);
        let mut df = wired_live_dataflow(dataflow);
        df.placement
            .node_daemon
            .insert(NodeId::new("a").unwrap(), daemon_id);
        coordinator.dataflows().insert(df);

        let reply = add_edge(
            &coordinator,
            dataflow,
            NodeId::new("a").unwrap(),
            InputSpec::new(DataId::new("in").unwrap(), "ghost/out".parse().unwrap()),
        )
        .await;
        assert_eq!(
            reply.error_code(),
            Some(astrs_wire::ErrorCode::FailedPrecondition),
            "a caller must not read this as 'would work once a verb exists'"
        );
    }

    #[tokio::test]
    async fn add_edge_introducing_a_strict_type_mismatch_is_rejected_with_diagnostics() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let (daemon_id, mut rx) = connect_daemon(&coordinator);
        let manifest = astrs_manifest::Manifest::from_yaml_str(
            "strict_types: true\nnodes:\n  \
             - id: camera\n    path: ./camera\n    outputs: [frames]\n    \
               output_types: { frames: std/core/v1/Float32 }\n  \
             - id: bad_camera\n    path: ./bad_camera\n    outputs: [frames]\n    \
               output_types: { frames: std/core/v1/Float64 }\n  \
             - id: detector\n    path: ./detector\n    inputs: { frames: camera/frames }\n    \
               input_types: { frames: std/core/v1/Float32 }\n",
        )
        .unwrap();
        let (graph, diagnostics) = astrs_graph::DataflowGraph::from_manifest(&manifest).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let mut df = LiveDataflow::new(dataflow, None, manifest, None, graph);
        df.placement
            .node_daemon
            .insert(NodeId::new("detector").unwrap(), daemon_id);
        coordinator.dataflows().insert(df);

        // Rewiring `detector.frames` away from the type-compatible
        // `camera` onto `bad_camera` (declared `Float64`, where
        // `detector.frames` declares `Float32`) introduces a mismatch
        // that did not exist before this op.
        let reply = add_edge(
            &coordinator,
            dataflow,
            NodeId::new("detector").unwrap(),
            InputSpec::new(
                DataId::new("frames").unwrap(),
                "bad_camera/frames".parse().unwrap(),
            ),
        )
        .await;
        assert_eq!(
            reply.error_code(),
            Some(astrs_wire::ErrorCode::FailedPrecondition)
        );
        match reply {
            ControlReply::Error { context, .. } => {
                assert_eq!(context.len(), 1, "exactly one edge newly violates the rule");
                assert!(
                    context[0].contains("Float32") && context[0].contains("Float64"),
                    "the diagnostic names both sides of the mismatch: {context:?}"
                );
            }
            other => panic!("expected an Error reply, got {other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "a type-unsafe edge must never reach a daemon"
        );

        let dataflows = coordinator.dataflows();
        let live = dataflows.get(dataflow).unwrap();
        assert!(
            matches!(
                &live
                    .graph
                    .edge(&astrs_graph::EdgeKey::new(
                        astrs_graph::NodeId::new("detector"),
                        astrs_graph::PortName::new("frames".to_owned())
                    ))
                    .unwrap()
                    .from,
                astrs_graph::EdgeSource::NodeOutput { node, .. } if node.as_str() == "camera"
            ),
            "a rejected candidate must never be committed to the tracked graph"
        );
    }

    #[tokio::test]
    async fn remove_edge_naming_an_edge_that_does_not_exist_is_a_failed_precondition() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let (daemon_id, _rx) = connect_daemon(&coordinator);
        let mut df = wired_live_dataflow(dataflow);
        df.placement
            .node_daemon
            .insert(NodeId::new("a").unwrap(), daemon_id);
        coordinator.dataflows().insert(df);

        let reply = remove_edge(
            &coordinator,
            dataflow,
            NodeId::new("a").unwrap(),
            DataId::new("never-wired").unwrap(),
        )
        .await;
        assert_eq!(
            reply.error_code(),
            Some(astrs_wire::ErrorCode::FailedPrecondition)
        );
    }

    #[tokio::test]
    async fn remove_node_drops_it_and_its_edges_from_the_tracked_graph() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        let (daemon_id, mut rx) = connect_daemon(&coordinator);
        let mut df = wired_live_dataflow(dataflow);
        df.placement
            .node_daemon
            .insert(NodeId::new("producer").unwrap(), daemon_id);
        coordinator.dataflows().insert(df);

        let reply = remove_node(
            &coordinator,
            dataflow,
            NodeId::new("producer").unwrap(),
            None,
        )
        .await;
        assert_eq!(reply, ControlReply::Ok);
        assert!(matches!(
            rx.try_recv().unwrap(),
            CoordinatorEvent::StopNode { .. }
        ));

        let dataflows = coordinator.dataflows();
        let live = dataflows.get(dataflow).unwrap();
        assert!(
            live.graph
                .node(&astrs_graph::NodeId::new("producer"))
                .is_none()
        );
        assert!(
            live.graph
                .edge(&astrs_graph::EdgeKey::new(
                    astrs_graph::NodeId::new("a"),
                    astrs_graph::PortName::new("in".to_owned())
                ))
                .is_none(),
            "the edge that referenced the removed node must go with it"
        );
    }

    #[tokio::test]
    async fn remove_node_on_a_missing_node_is_not_found() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        coordinator.dataflows().insert(live_dataflow(dataflow));
        let reply = remove_node(&coordinator, dataflow, NodeId::new("ghost").unwrap(), None).await;
        assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::NotFound));
    }

    #[tokio::test]
    async fn replace_node_on_a_missing_node_is_not_found_not_unsupported() {
        let coordinator = coordinator();
        let dataflow = DataflowId::generate();
        coordinator.dataflows().insert(live_dataflow(dataflow));
        let spec = NodeSpawnSpec::new(
            dataflow,
            NodeId::new("ghost").unwrap(),
            1,
            NodeSource::Executable { path: "./g".into() },
        );
        let reply = replace_node(&coordinator, dataflow, Box::new(spec), false).await;
        assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::NotFound));
    }
}
