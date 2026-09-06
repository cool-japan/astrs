//! Resolving `astrs-graph`'s [`PlacementPlan`] against the coordinator's
//! *actually connected* daemons, and deriving the [`RouteSpec`]s a `Spawn`
//! carries (blueprint §5.2, §6.3, §6.4).
//!
//! `astrs-graph::plan_placement` answers "which [`MachineId`] does each
//! node belong on" from the manifest alone — it has never heard of a real
//! [`DaemonId`]. This module is the missing second half: turning a
//! [`MachineId`] into the one connected daemon that claims it (or, for
//! [`MachineId::CoordinatorLocal`], whichever daemon is available — the
//! blueprint §4.2 default of "runs wherever the coordinator itself runs"
//! reduces to "the one daemon" the moment any daemon at all is connected,
//! since this build has no in-process daemon of its own to prefer).

use std::collections::BTreeMap;

use astrs_graph::{DataflowGraph, MachineId};
use astrs_wire::{
    DaemonId, DataflowId, MachineName, NodeId as WireNodeId, Plane, RouteKey, RouteSpec,
};

use crate::error::{CoordinatorError, Result};
use crate::graph_bridge;
use crate::registry::DaemonRegistry;

/// A [`PlacementPlan`] with every [`MachineId`] resolved to a connected
/// [`DaemonId`].
#[derive(Debug, Clone, Default)]
pub struct ResolvedPlacement {
    /// The daemon hosting each manifest-declared node.
    pub node_daemon: BTreeMap<WireNodeId, DaemonId>,
    /// The daemon hosting each node added dynamically after `Start`
    /// (`AddNode`) — kept separate from `node_daemon` because those nodes
    /// have no entry in the manifest-derived [`PlacementPlan`] at all.
    pub dynamic_daemons: BTreeMap<WireNodeId, DaemonId>,
}

/// Resolves every node of `graph` to the daemon that will host it.
///
/// Walks the *graph* rather than [`PlacementPlan::machines`]'s spawn lists,
/// and that difference is the whole point: `plan_placement` lists only the
/// nodes a daemon starts an OS process for, so a `path: dynamic` node
/// (§8.3's external attach) appears in no spawn list at all. It still has to
/// be **placed** — a dynamic node registers with a daemon, and a daemon
/// refuses a registration for a node it was never told about — so a
/// resolution built from spawn lists alone silently drops every dynamic node
/// from the dispatch, and every dynamic graph then starts with nothing
/// running anywhere.
///
/// # Errors
///
/// [`CoordinatorError::NoDaemonsConnected`] if [`MachineId::CoordinatorLocal`]
/// is needed and no daemon at all is connected.
/// [`CoordinatorError::NoDaemonForMachine`] if a named machine has no (or
/// more than one — see [`DaemonRegistry::find_by_machine`]) matching
/// daemon. [`CoordinatorError::Id`] if a node id fails the wire's charset
/// check.
pub fn resolve_placement(
    graph: &DataflowGraph,
    daemons: &DaemonRegistry,
) -> Result<ResolvedPlacement> {
    let mut by_machine: BTreeMap<&MachineId, DaemonId> = BTreeMap::new();
    let mut node_daemon = BTreeMap::new();
    for node in graph.nodes.values() {
        let daemon = match by_machine.get(&node.machine) {
            Some(daemon) => daemon.clone(),
            None => {
                let daemon = resolve_machine(&node.machine, daemons)?;
                by_machine.insert(&node.machine, daemon.clone());
                daemon
            }
        };
        node_daemon.insert(graph_bridge::node_id_to_wire(&node.id)?, daemon);
    }
    Ok(ResolvedPlacement {
        node_daemon,
        dynamic_daemons: BTreeMap::new(),
    })
}

/// Resolves one [`MachineId`] to a connected daemon.
///
/// # Errors
///
/// As [`resolve_placement`].
pub fn resolve_machine(machine: &MachineId, daemons: &DaemonRegistry) -> Result<DaemonId> {
    match machine {
        MachineId::CoordinatorLocal => daemons
            .any()
            .map(|handle| handle.id.clone())
            .ok_or(CoordinatorError::NoDaemonsConnected),
        MachineId::Named(name) => {
            let machine_name = MachineName::new(name)?;
            daemons
                .find_by_machine(&machine_name)
                .map(|handle| handle.id.clone())
                .ok_or_else(|| CoordinatorError::NoDaemonForMachine(name.clone()))
        }
    }
}

/// The routes one node participates in, either as consumer or as
/// producer, for its `CoordinatorEvent::Spawn` payload.
///
/// Every route starts on the reliable daemon path (blueprint §6.3: SHM
/// upgrade is negotiated afterward, daemon-side, once every same-host
/// consumer has attached — this coordinator never proposes it directly).
/// The plane chosen here is [`Plane::Uds`] when producer and consumer share
/// a daemon and [`Plane::Tcp`] otherwise: `astrs-transport`'s QUIC backend
/// cannot yet serve as a listener (no `rustls` in this workspace's
/// dependency set — see that crate's own module docs), so TCP is the
/// honest cross-host default until that gap closes, not a fallback taken
/// silently.
///
/// # Errors
///
/// [`CoordinatorError::Id`] if a port name fails the wire's charset check.
pub fn routes_for_node(
    dataflow: DataflowId,
    node: &astrs_graph::NodeId,
    graph: &DataflowGraph,
    resolved: &ResolvedPlacement,
) -> Result<Vec<RouteSpec>> {
    let mut routes = Vec::new();
    let node_daemon = |id: &astrs_graph::NodeId| -> Option<&DaemonId> {
        let wire_id = graph_bridge::node_id_to_wire(id).ok()?;
        resolved
            .dynamic_daemons
            .get(&wire_id)
            .or_else(|| resolved.node_daemon.get(&wire_id))
    };

    for (key, edge) in graph.edges_into(node) {
        let astrs_graph::EdgeSource::NodeOutput {
            node: producer,
            output,
        } = &edge.from
        else {
            continue;
        };
        let producer_port = astrs_wire::PortRef::new(
            graph_bridge::node_id_to_wire(producer)?,
            graph_bridge::port_name_to_data_id(output)?,
        );
        let consumer_port = astrs_wire::PortRef::new(
            graph_bridge::node_id_to_wire(&key.consumer)?,
            graph_bridge::port_name_to_data_id(&key.input)?,
        );
        let plane = plane_for(node_daemon(producer), node_daemon(node));
        routes.push(
            RouteSpec::new(RouteKey::new(dataflow, producer_port, consumer_port))
                .with_plane(plane)
                .with_compression(astrs_wire::Compression::default())
                .with_queue(
                    edge.queue.size,
                    graph_bridge::queue_policy_to_wire(edge.queue.policy),
                ),
        );
    }
    for (key, edge) in graph.edges_from(node) {
        let astrs_graph::EdgeSource::NodeOutput { output, .. } = &edge.from else {
            continue;
        };
        let producer_port = astrs_wire::PortRef::new(
            graph_bridge::node_id_to_wire(node)?,
            graph_bridge::port_name_to_data_id(output)?,
        );
        let consumer_port = astrs_wire::PortRef::new(
            graph_bridge::node_id_to_wire(&key.consumer)?,
            graph_bridge::port_name_to_data_id(&key.input)?,
        );
        let plane = plane_for(node_daemon(node), node_daemon(&key.consumer));
        routes.push(
            RouteSpec::new(RouteKey::new(dataflow, producer_port, consumer_port))
                .with_plane(plane)
                .with_compression(astrs_wire::Compression::default())
                .with_queue(
                    edge.queue.size,
                    graph_bridge::queue_policy_to_wire(edge.queue.policy),
                ),
        );
    }
    Ok(routes)
}

fn plane_for(producer_daemon: Option<&DaemonId>, consumer_daemon: Option<&DaemonId>) -> Plane {
    match (producer_daemon, consumer_daemon) {
        (Some(a), Some(b)) if a == b => Plane::Uds,
        _ => Plane::Tcp,
    }
}

/// The [`PeerRouteDirective`]s each daemon needs for one dataflow (blueprint
/// §4.2, §6.4).
///
/// A daemon can derive every same-host edge from the `Spawn` events it is
/// given. A *cross*-daemon edge needs one fact no daemon can derive — which
/// daemon holds the far end, and what address to dial it on — and this
/// coordinator is the process every daemon registered that address with. So
/// for each edge whose two ends resolve to different daemons, **both** get a
/// directive: the producing side, which opens the route and forwards
/// payloads, and the consuming side, which answers the setup and (when its id
/// sorts lower) is the side that dials.
///
/// A daemon that registered no dialable address gets no directive pointing at
/// it, and the edge is reported as a warning by the caller rather than as a
/// route that can never open.
///
/// # Errors
///
/// [`CoordinatorError::Id`] if a node or port name fails the wire's charset
/// check.
pub fn peer_route_directives(
    dataflow: DataflowId,
    graph: &DataflowGraph,
    resolved: &ResolvedPlacement,
    daemons: &DaemonRegistry,
) -> Result<BTreeMap<DaemonId, Vec<astrs_wire::PeerRouteDirective>>> {
    let mut per_daemon: BTreeMap<DaemonId, Vec<astrs_wire::PeerRouteDirective>> = BTreeMap::new();
    for (key, edge) in &graph.edges {
        let astrs_graph::EdgeSource::NodeOutput {
            node: producer,
            output,
        } = &edge.from
        else {
            continue;
        };
        let producer_id = graph_bridge::node_id_to_wire(producer)?;
        let consumer_id = graph_bridge::node_id_to_wire(&key.consumer)?;
        let (Some(producer_daemon), Some(consumer_daemon)) = (
            daemon_of(resolved, &producer_id),
            daemon_of(resolved, &consumer_id),
        ) else {
            continue;
        };
        if producer_daemon == consumer_daemon {
            continue;
        }

        let route = RouteSpec::new(RouteKey::new(
            dataflow,
            astrs_wire::PortRef::new(producer_id, graph_bridge::port_name_to_data_id(output)?),
            astrs_wire::PortRef::new(consumer_id, graph_bridge::port_name_to_data_id(&key.input)?),
        ))
        .with_plane(Plane::Tcp)
        .with_compression(astrs_wire::Compression::default());

        for (side, peer) in [
            (producer_daemon, consumer_daemon),
            (consumer_daemon, producer_daemon),
        ] {
            let Some(handle) = daemons.get(peer) else {
                continue;
            };
            if !handle.is_dialable() {
                tracing::warn!(
                    route = %route.key,
                    daemon = %peer,
                    "a cross-daemon edge points at a daemon that announced no peer address"
                );
                continue;
            }
            per_daemon
                .entry(side.clone())
                .or_default()
                .push(astrs_wire::PeerRouteDirective::new(
                    route.clone(),
                    peer.clone(),
                    handle.peer_address.clone(),
                ));
        }
    }
    Ok(per_daemon)
}

/// The daemon hosting one node, dynamic placements first.
fn daemon_of<'a>(resolved: &'a ResolvedPlacement, node: &WireNodeId) -> Option<&'a DaemonId> {
    resolved
        .dynamic_daemons
        .get(node)
        .or_else(|| resolved.node_daemon.get(node))
}

/// [`RouteSpec::with_queue`] does not exist upstream — this crate needs the
/// queue depth/policy on the route (so a cross-host producer can size its
/// buffering without asking the consumer again), which
/// [`astrs_wire::RouteSpec::new`] already defaults from
/// [`astrs_wire::DEFAULT_QUEUE_SIZE`]/[`astrs_wire::QueuePolicy::default`].
/// A small extension trait keeps the call sites above reading as a single
/// builder chain instead of a `RouteSpec { queue_size, queue_policy, ..}`
/// literal that would have to repeat every other field by hand.
trait RouteSpecQueueExt {
    /// Sets the queue depth/policy fields.
    #[must_use]
    fn with_queue(self, size: u32, policy: astrs_wire::QueuePolicy) -> Self;
}

impl RouteSpecQueueExt for RouteSpec {
    fn with_queue(mut self, size: u32, policy: astrs_wire::QueuePolicy) -> Self {
        self.queue_size = size;
        self.queue_policy = policy;
        self
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::registry::daemon::DaemonHandle;
    use astrs_time::HlcTimestamp;
    use astrs_wire::SessionId;
    use tokio::sync::mpsc;

    fn manifest_and_graph(yaml: &str) -> (astrs_manifest::Manifest, DataflowGraph) {
        let manifest = astrs_manifest::Manifest::from_yaml_str(yaml).unwrap();
        manifest.validate().unwrap();
        let (graph, diagnostics) = DataflowGraph::from_manifest(&manifest).unwrap();
        assert!(diagnostics.is_empty());
        (manifest, graph)
    }

    fn insert_daemon(registry: &mut DaemonRegistry, machine: Option<&str>) -> DaemonId {
        insert_daemon_at(registry, machine, Some("tcp:127.0.0.1:7409"))
    }

    fn insert_daemon_at(
        registry: &mut DaemonRegistry,
        machine: Option<&str>,
        address: Option<&str>,
    ) -> DaemonId {
        let id = DaemonId::generate(None);
        let (tx, _rx) = mpsc::channel(8);
        let mut handle = DaemonHandle::new(
            id.clone(),
            machine.map(|m| MachineName::new(m).unwrap()),
            SessionId::generate(),
            tx,
            HlcTimestamp::EPOCH,
        );
        if let Some(address) = address {
            handle = handle.with_peer_address(address);
        }
        registry.insert(handle);
        id
    }

    /// The two-machine graph every directive test uses.
    fn split_graph() -> DataflowGraph {
        manifest_and_graph(
            "\
nodes:
  - id: camera
    path: ./camera
    outputs: [frames]
  - id: detector
    deploy: {machine: robot-1}
    path: ./detector
    inputs:
      frames: camera/frames
",
        )
        .1
    }

    fn resolved_pair(local: &DaemonId, remote: &DaemonId) -> ResolvedPlacement {
        let mut resolved = ResolvedPlacement::default();
        resolved
            .node_daemon
            .insert(WireNodeId::new("camera").unwrap(), local.clone());
        resolved
            .node_daemon
            .insert(WireNodeId::new("detector").unwrap(), remote.clone());
        resolved
    }

    #[test]
    fn a_cross_daemon_edge_produces_one_directive_for_each_side() {
        let mut daemons = DaemonRegistry::new();
        let local = insert_daemon(&mut daemons, None);
        let robot1 = insert_daemon(&mut daemons, Some("robot-1"));
        let graph = split_graph();
        let resolved = resolved_pair(&local, &robot1);

        let dataflow = DataflowId::generate();
        let per_daemon = peer_route_directives(dataflow, &graph, &resolved, &daemons).unwrap();

        assert_eq!(per_daemon.len(), 2, "both ends are told about the edge");
        let producing = &per_daemon[&local];
        assert_eq!(producing.len(), 1);
        assert_eq!(producing[0].peer, robot1, "it names the *other* daemon");
        assert_eq!(producing[0].producer_node().as_str(), "camera");
        assert_eq!(producing[0].consumer_node().as_str(), "detector");
        assert_eq!(producing[0].dataflow(), dataflow);
        assert_eq!(producing[0].address, "tcp:127.0.0.1:7409");
        assert_eq!(producing[0].route.plane, Plane::Tcp);

        let consuming = &per_daemon[&robot1];
        assert_eq!(consuming.len(), 1);
        assert_eq!(consuming[0].peer, local);
        assert_eq!(
            consuming[0].route.key, producing[0].route.key,
            "one edge, one key, described identically to both sides"
        );
    }

    #[test]
    fn a_same_daemon_edge_produces_no_directive() {
        let mut daemons = DaemonRegistry::new();
        let local = insert_daemon(&mut daemons, None);
        insert_daemon(&mut daemons, Some("robot-1"));
        let graph = split_graph();
        let resolved = resolved_pair(&local, &local);

        let per_daemon =
            peer_route_directives(DataflowId::generate(), &graph, &resolved, &daemons).unwrap();
        assert!(
            per_daemon.is_empty(),
            "a same-host edge needs no peer at all"
        );
    }

    #[test]
    fn a_daemon_with_no_announced_peer_address_gets_no_directive_pointing_at_it() {
        let mut daemons = DaemonRegistry::new();
        let local = insert_daemon(&mut daemons, None);
        let robot1 = insert_daemon_at(&mut daemons, Some("robot-1"), None);
        let graph = split_graph();
        let resolved = resolved_pair(&local, &robot1);

        let per_daemon =
            peer_route_directives(DataflowId::generate(), &graph, &resolved, &daemons).unwrap();
        assert!(
            !per_daemon.contains_key(&local),
            "nothing points at a daemon nobody can dial"
        );
        assert_eq!(
            per_daemon.get(&robot1).map(Vec::len),
            Some(1),
            "the reachable side is still told, so it can be dialled instead"
        );
    }

    #[test]
    fn an_unplaced_node_produces_no_directive() {
        let mut daemons = DaemonRegistry::new();
        let local = insert_daemon(&mut daemons, None);
        insert_daemon(&mut daemons, Some("robot-1"));
        let graph = split_graph();
        let mut resolved = ResolvedPlacement::default();
        resolved
            .node_daemon
            .insert(WireNodeId::new("camera").unwrap(), local);

        let per_daemon =
            peer_route_directives(DataflowId::generate(), &graph, &resolved, &daemons).unwrap();
        assert!(per_daemon.is_empty(), "half a placement is no placement");
    }

    #[test]
    fn a_dynamic_placement_wins_over_the_manifest_one() {
        let mut daemons = DaemonRegistry::new();
        let local = insert_daemon(&mut daemons, None);
        let robot1 = insert_daemon(&mut daemons, Some("robot-1"));
        let graph = split_graph();
        let mut resolved = resolved_pair(&local, &local);
        resolved
            .dynamic_daemons
            .insert(WireNodeId::new("detector").unwrap(), robot1.clone());

        let per_daemon =
            peer_route_directives(DataflowId::generate(), &graph, &resolved, &daemons).unwrap();
        assert_eq!(
            per_daemon.len(),
            2,
            "an `AddNode` that moved the consumer makes the edge cross machines"
        );
        assert_eq!(per_daemon[&local][0].peer, robot1);
    }

    #[test]
    fn coordinator_local_resolves_to_the_only_daemon() {
        let mut daemons = DaemonRegistry::new();
        let id = insert_daemon(&mut daemons, None);
        assert_eq!(
            resolve_machine(&MachineId::CoordinatorLocal, &daemons).unwrap(),
            id
        );
    }

    #[test]
    fn coordinator_local_with_no_daemons_is_a_typed_error() {
        let daemons = DaemonRegistry::new();
        let err = resolve_machine(&MachineId::CoordinatorLocal, &daemons).unwrap_err();
        assert!(matches!(err, CoordinatorError::NoDaemonsConnected));
    }

    #[test]
    fn a_named_machine_resolves_to_its_daemon() {
        let mut daemons = DaemonRegistry::new();
        let id = insert_daemon(&mut daemons, Some("robot-1"));
        assert_eq!(
            resolve_machine(&MachineId::Named("robot-1".to_owned()), &daemons).unwrap(),
            id
        );
    }

    #[test]
    fn an_unregistered_machine_name_is_a_typed_error() {
        let daemons = DaemonRegistry::new();
        let err = resolve_machine(&MachineId::Named("ghost".to_owned()), &daemons).unwrap_err();
        assert!(matches!(err, CoordinatorError::NoDaemonForMachine(name) if name == "ghost"));
    }

    #[test]
    fn resolve_placement_maps_every_spawned_node() {
        let (_manifest, graph) = manifest_and_graph(
            "nodes:\n  - id: a\n    path: ./a\n  - id: b\n    deploy: {machine: robot-1}\n    path: ./b\n",
        );
        let mut daemons = DaemonRegistry::new();
        let local = insert_daemon(&mut daemons, None);
        let robot1 = insert_daemon(&mut daemons, Some("robot-1"));

        let resolved = resolve_placement(&graph, &daemons).unwrap();
        assert_eq!(
            resolved.node_daemon.get(&WireNodeId::new("a").unwrap()),
            Some(&local)
        );
        assert_eq!(
            resolved.node_daemon.get(&WireNodeId::new("b").unwrap()),
            Some(&robot1)
        );
    }

    #[test]
    fn routes_are_uds_on_shared_daemons_and_tcp_across_daemons() {
        let (_manifest, graph) = manifest_and_graph(
            "\
nodes:
  - id: camera
    path: ./camera
    outputs: [frames]
  - id: detector
    deploy: {machine: robot-1}
    path: ./detector
    inputs:
      frames: camera/frames
",
        );
        let mut daemons = DaemonRegistry::new();
        let local = insert_daemon(&mut daemons, None);
        let robot1 = insert_daemon(&mut daemons, Some("robot-1"));
        let mut resolved = ResolvedPlacement::default();
        resolved
            .node_daemon
            .insert(WireNodeId::new("camera").unwrap(), local.clone());
        resolved
            .node_daemon
            .insert(WireNodeId::new("detector").unwrap(), robot1.clone());

        let dataflow = DataflowId::generate();
        let camera_routes = routes_for_node(
            dataflow,
            &astrs_graph::NodeId::new("camera"),
            &graph,
            &resolved,
        )
        .unwrap();
        assert_eq!(camera_routes.len(), 1);
        assert_eq!(camera_routes[0].plane, Plane::Tcp, "different daemons");

        // Now co-locate them and check the plane flips to Uds.
        resolved
            .node_daemon
            .insert(WireNodeId::new("detector").unwrap(), local.clone());
        let camera_routes = routes_for_node(
            dataflow,
            &astrs_graph::NodeId::new("camera"),
            &graph,
            &resolved,
        )
        .unwrap();
        assert_eq!(camera_routes[0].plane, Plane::Uds, "same daemon now");

        let detector_routes = routes_for_node(
            dataflow,
            &astrs_graph::NodeId::new("detector"),
            &graph,
            &resolved,
        )
        .unwrap();
        assert_eq!(detector_routes.len(), 1);
        assert_eq!(
            detector_routes[0].key.consumer.to_string(),
            "detector/frames"
        );
        assert_eq!(detector_routes[0].key.producer.to_string(), "camera/frames");
    }

    #[test]
    fn a_node_with_no_edges_gets_no_routes() {
        let (_manifest, graph) = manifest_and_graph("nodes:\n  - id: solo\n    path: ./solo\n");
        let resolved = ResolvedPlacement::default();
        let routes = routes_for_node(
            DataflowId::generate(),
            &astrs_graph::NodeId::new("solo"),
            &graph,
            &resolved,
        )
        .unwrap();
        assert!(routes.is_empty());
    }
}
