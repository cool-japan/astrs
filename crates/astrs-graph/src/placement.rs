//! The placement planner: mapping graph nodes onto machines/daemons, and
//! deciding which edges cross a machine boundary (blueprint §4.2, §5.2,
//! §6.4).
//!
//! [`crate::GraphNode::machine`] already resolves each node's *individual*
//! placement (the manifest's two-level `deploy:` override chain, merged by
//! [`crate::MachineId::resolve`]). What this module adds is the
//! *graph-wide* view a daemon or coordinator actually needs at spawn time:
//!
//! - **Per-machine spawn lists** ([`MachinePlan::spawns`]) — which node ids
//!   the daemon on one machine must launch an OS process for.
//! - **The cross-machine route list** ([`PlacementPlan::cross_machine_routes`])
//!   — every edge whose producer and consumer resolve to *different*
//!   machines, which is exactly the set [`astrs-transport`] needs a route
//!   for (blueprint §6.4); every edge *not* in this list is same-host and
//!   stays on the SHM plane (§6.2).
//!
//! # Why "daemon" and "machine" are the same identifier here
//!
//! Blueprint §5.2 asks for cross-machine routes as `(producer daemon,
//! consumer daemon, edge)` tuples. This module spells that with
//! [`MachineId`] rather than introducing a separate daemon-id type,
//! because blueprint §4.2 fixes one daemon per machine — a [`MachineId`]
//! already *is* the identity of "the daemon that owns this machine." A
//! later revision that lets one machine run more than one daemon would
//! need a new id type; nothing here assumes that can't happen, but nothing
//! here builds it prematurely either.
//!
//! # Why [`PlacementPlan::machines`] is a `Vec`, not a map
//!
//! [`MachineId`] is a two-variant enum
//! ([`MachineId::CoordinatorLocal`]/[`MachineId::Named`]), not a
//! string-newtype — `serde_json` cannot serialize a `BTreeMap` keyed by an
//! arbitrary enum (it requires map keys to serialize as strings, which a
//! transparent newtype like [`crate::NodeId`] satisfies but a data-carrying
//! enum variant does not). Since this plan is exactly the kind of value
//! blueprint §17's `--json` CLI output would print, [`PlacementPlan::machines`]
//! is a plain `Vec<MachinePlan>` (each entry carrying its own
//! [`MachinePlan::machine`] field) instead — JSON-safe by construction,
//! sorted by [`MachineId`] for the same determinism every other collection
//! in this crate keeps by going through a [`BTreeMap`] internally during
//! construction.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::edge::EdgeKey;
use crate::graph::DataflowGraph;
use crate::ids::{MachineId, NodeId};

/// One machine's slice of a [`PlacementPlan`]: the node ids the daemon on
/// that machine must spawn an OS process for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachinePlan {
    /// The machine (daemon) this sub-plan is for.
    pub machine: MachineId,
    /// Node ids to spawn, in [`NodeId`] order. Excludes nodes with
    /// [`crate::GraphNode::spawns_process`] `== false` — a `path: dynamic`
    /// node (blueprint §8.3's external attach) has no process for a
    /// daemon to launch, even though it is still placed on this machine
    /// for every other purpose (its outputs/inputs still wire normally).
    pub spawns: Vec<NodeId>,
}

/// One edge whose producer and consumer nodes resolve to *different*
/// machines — the daemon-to-daemon route list blueprint §5.2 asks for.
///
/// See this module's top-level docs for why "daemon" and "machine" share
/// one identifier here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossMachineRoute {
    /// The machine hosting the edge's producer node.
    pub producer_machine: MachineId,
    /// The machine hosting the edge's consumer node (always different
    /// from `producer_machine` — same-machine edges are not routes, they
    /// stay on the local SHM plane and never appear here).
    pub consumer_machine: MachineId,
    /// The edge this route carries.
    pub edge: EdgeKey,
}

/// The complete placement plan for one [`DataflowGraph`]: one
/// [`MachinePlan`] per machine that hosts at least one node, plus the
/// cross-machine route list.
///
/// Built by [`plan_placement`]; see that function's docs for the exact
/// rules, and this module's top-level docs for the design rationale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlacementPlan {
    /// Every machine that hosts at least one node, sorted by [`MachineId`].
    /// A machine with only dynamic-attach nodes still appears here, with
    /// an empty [`MachinePlan::spawns`] — "no processes to launch" is a
    /// distinct, representable state, not the same as "this machine does
    /// not exist in this plan."
    pub machines: Vec<MachinePlan>,
    /// Every edge that crosses a machine boundary, in the [`EdgeKey`]
    /// order [`DataflowGraph::edges`] already iterates in. Virtual-source
    /// edges never appear here — see [`plan_placement`]'s docs.
    pub cross_machine_routes: Vec<CrossMachineRoute>,
}

impl PlacementPlan {
    /// The sub-plan for one machine, if it hosts at least one node.
    #[must_use]
    pub fn machine(&self, id: &MachineId) -> Option<&MachinePlan> {
        self.machines.iter().find(|plan| &plan.machine == id)
    }

    /// The total number of nodes this plan spawns a process for, across
    /// every machine — a convenience sum, not stored separately.
    #[must_use]
    pub fn total_spawns(&self) -> usize {
        self.machines.iter().map(|plan| plan.spawns.len()).sum()
    }
}

/// Build the placement plan for `graph`: group nodes by their already-
/// resolved [`crate::GraphNode::machine`], and classify every edge as
/// same-host or cross-host.
///
/// # Rules
///
/// - A node contributes to its machine's [`MachinePlan::spawns`] iff
///   [`crate::GraphNode::spawns_process`] is `true`; a dynamic-attach node
///   still causes its machine to appear in [`PlacementPlan::machines`]
///   (with a possibly-empty `spawns` list) but never appears in any
///   `spawns` list itself.
/// - A **virtual-source edge is never a cross-machine route**: it has no
///   producer node (blueprint §11.1 — each daemon runs its own timer
///   wheel, so `astrs/timer/*` and friends are always materialized
///   locally on the *consumer's* machine, never shipped across a route).
/// - An edge whose producer node cannot be resolved in `graph` (a
///   dangling reference — never true of a graph built by
///   [`DataflowGraph::from_manifest`] on a validated manifest, but this
///   function does not assume that; see [`crate::diff`] for the one path
///   that can otherwise introduce one before it is caught) is silently
///   excluded from the route list rather than panicking: there is no
///   sound producer machine to report a route from.
/// - Every other edge is a route iff its producer and consumer resolve to
///   *different* machines; same-machine edges are the SHM-plane default
///   and are not listed.
///
/// # Determinism
///
/// [`PlacementPlan::machines`] is sorted by [`MachineId`] and each
/// [`MachinePlan::spawns`] by [`NodeId`] (both fall out of iterating
/// [`DataflowGraph::nodes`], itself a [`BTreeMap`]).
/// [`PlacementPlan::cross_machine_routes`] is sorted by [`EdgeKey`] for the
/// same reason — [`DataflowGraph::edges`] is a `BTreeMap` too.
#[must_use]
pub fn plan_placement(graph: &DataflowGraph) -> PlacementPlan {
    let mut by_machine: BTreeMap<MachineId, Vec<NodeId>> = BTreeMap::new();
    for node in graph.nodes.values() {
        let spawns = by_machine.entry(node.machine.clone()).or_default();
        if node.spawns_process {
            spawns.push(node.id.clone());
        }
    }
    let machines = by_machine
        .into_iter()
        .map(|(machine, spawns)| MachinePlan { machine, spawns })
        .collect();

    let mut cross_machine_routes = Vec::new();
    for (key, edge) in &graph.edges {
        let Some(producer_id) = edge.from.producer_node() else {
            // Virtual source: always daemon-local to the consumer, never
            // a route.
            continue;
        };
        let Some(producer) = graph.nodes.get(producer_id) else {
            // Dangling producer reference; see the docs above.
            continue;
        };
        let Some(consumer) = graph.nodes.get(&key.consumer) else {
            continue;
        };
        if producer.machine != consumer.machine {
            cross_machine_routes.push(CrossMachineRoute {
                producer_machine: producer.machine.clone(),
                consumer_machine: consumer.machine.clone(),
                edge: key.clone(),
            });
        }
    }

    PlacementPlan {
        machines,
        cross_machine_routes,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use astrs_manifest::{Deploy, Input, Manifest, Node};

    use super::*;
    use crate::edge::{Edge, EdgeSource, QueueConfig};
    use crate::graph::DataflowGraph;
    use crate::ids::PortName;
    use crate::node::{GraphNode, InputPort};

    /// camera (coordinator-local) -> detector (coordinator-local) ->
    /// planner (`robot-1`), plus a virtual timer tick into planner —
    /// mirrors blueprint §8.1's canonical example closely enough to be a
    /// realistic 2-machine fixture.
    fn two_machine_manifest() -> Manifest {
        let mut camera = Node::with_path("camera", "./camera");
        camera.outputs = vec!["frames".to_string()];

        let mut detector = Node::with_path("detector", "./detector");
        detector.outputs = vec!["detections".to_string()];
        detector
            .inputs
            .insert("frames".to_string(), Input::from_source("camera/frames"));

        let mut planner = Node::with_path("planner", "./planner");
        planner.deploy = Some(Deploy {
            machine: Some("robot-1".to_string()),
            ..Deploy::default()
        });
        planner.inputs.insert(
            "detections".to_string(),
            Input::from_source("detector/detections"),
        );
        planner
            .inputs
            .insert("tick".to_string(), Input::from_source("astrs/timer/hz/50"));

        Manifest {
            nodes: vec![camera, detector, planner],
            ..Manifest::default()
        }
    }

    #[test]
    fn single_machine_graph_has_one_machine_plan_and_no_routes() {
        let mut camera = Node::with_path("camera", "./camera");
        camera.outputs = vec!["frames".to_string()];
        let mut detector = Node::with_path("detector", "./detector");
        detector
            .inputs
            .insert("frames".to_string(), Input::from_source("camera/frames"));
        let manifest = Manifest {
            nodes: vec![camera, detector],
            ..Manifest::default()
        };
        let (graph, _) = DataflowGraph::from_manifest(&manifest).unwrap();

        let plan = plan_placement(&graph);
        assert_eq!(plan.machines.len(), 1);
        assert_eq!(plan.machines[0].machine, MachineId::CoordinatorLocal);
        assert_eq!(plan.total_spawns(), 2);
        assert!(plan.cross_machine_routes.is_empty());
    }

    #[test]
    fn two_machine_graph_splits_spawns_and_reports_one_route() {
        let manifest = two_machine_manifest();
        let (graph, diagnostics) = DataflowGraph::from_manifest(&manifest).unwrap();
        assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

        let plan = plan_placement(&graph);
        assert_eq!(plan.machines.len(), 2);

        let coordinator = plan.machine(&MachineId::CoordinatorLocal).unwrap();
        assert_eq!(
            coordinator.spawns,
            vec![NodeId::new("camera"), NodeId::new("detector")]
        );

        let robot1 = plan
            .machine(&MachineId::Named("robot-1".to_string()))
            .unwrap();
        assert_eq!(robot1.spawns, vec![NodeId::new("planner")]);

        // Exactly one route: detector (coordinator) -> planner (robot-1)
        // for `detections`. The virtual timer tick is never a route.
        assert_eq!(plan.cross_machine_routes.len(), 1);
        let route = &plan.cross_machine_routes[0];
        assert_eq!(route.producer_machine, MachineId::CoordinatorLocal);
        assert_eq!(
            route.consumer_machine,
            MachineId::Named("robot-1".to_string())
        );
        assert_eq!(
            route.edge,
            EdgeKey::new(NodeId::new("planner"), PortName::new("detections"))
        );
    }

    #[test]
    fn machine_with_only_dynamic_nodes_still_appears_with_empty_spawns() {
        let manifest = Manifest {
            nodes: vec![Node::with_path(
                "external",
                astrs_manifest::DYNAMIC_PATH_SENTINEL,
            )],
            ..Manifest::default()
        };
        let (graph, _) = DataflowGraph::from_manifest(&manifest).unwrap();
        let plan = plan_placement(&graph);
        assert_eq!(plan.machines.len(), 1);
        assert!(plan.machines[0].spawns.is_empty());
        assert_eq!(plan.total_spawns(), 0);
    }

    #[test]
    fn dangling_producer_reference_is_excluded_not_panicked() {
        // Hand-build a graph (bypassing `from_manifest`) with an edge
        // whose producer node does not exist — the shape `crate::diff`
        // must tolerate before its own integrity check rejects it.
        let mut nodes = BTreeMap::new();
        nodes.insert(
            NodeId::new("consumer"),
            GraphNode {
                id: NodeId::new("consumer"),
                outputs: BTreeMap::new(),
                inputs: [(PortName::new("in"), InputPort { type_urn: None })]
                    .into_iter()
                    .collect(),
                pattern: None,
                machine: MachineId::CoordinatorLocal,
                spawns_process: true,
            },
        );
        let mut edges = BTreeMap::new();
        edges.insert(
            EdgeKey::new(NodeId::new("consumer"), PortName::new("in")),
            Edge {
                from: EdgeSource::NodeOutput {
                    node: NodeId::new("ghost"),
                    output: PortName::new("out"),
                },
                queue: QueueConfig::default(),
            },
        );
        let graph = DataflowGraph {
            nodes,
            edges,
            strict_types: false,
            type_rules: Vec::new(),
        };

        let plan = plan_placement(&graph);
        assert!(plan.cross_machine_routes.is_empty());
    }

    #[test]
    fn placement_plan_default_is_empty() {
        let plan = PlacementPlan::default();
        assert!(plan.machines.is_empty());
        assert!(plan.cross_machine_routes.is_empty());
        assert_eq!(plan.total_spawns(), 0);
        assert!(plan.machine(&MachineId::CoordinatorLocal).is_none());
    }
}
