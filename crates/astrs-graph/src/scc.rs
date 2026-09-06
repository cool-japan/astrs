//! Strongly-connected-component (cycle) detection over the node-level
//! graph (blueprint §5.2's "stable topological metadata").
//!
//! Implements Tarjan's algorithm recursively over a deduplicated
//! node-to-node adjacency (virtual-source edges have no producer node and
//! are excluded — they can never participate in a cycle). Dataflow
//! manifests describe *node counts*, not edge-scale graphs — a robotics
//! pipeline with a few dozen to a few hundred nodes — so recursion depth
//! here is bounded by realistic manifest size; an iterative work-stack
//! rewrite would only add risk of a subtly wrong reimplementation of a
//! well-known algorithm for no practical benefit at this scale.
//!
//! Determinism (required so [`crate::visualize`] and golden-file tests are
//! reproducible) comes from two places: the root-node iteration order and
//! every node's successor list are built from [`std::collections::BTreeMap`]
//! iteration (already key-sorted), and the final SCC list is explicitly
//! sorted by each component's smallest member — not left as an artifact of
//! traversal order.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::diagnostic::{Diagnostic, DiagnosticKind, Severity};
use crate::edge::{Edge, EdgeKey};
use crate::ids::NodeId;
use crate::node::GraphNode;

/// One strongly connected component of the node-level dataflow graph.
///
/// A component with more than one member is, by definition, cyclic (every
/// member can reach every other, which implies a cycle through all of
/// them). A single-member component is only cyclic if that node has an
/// edge from one of its own outputs back into one of its own inputs — see
/// `cycle_diagnostics` (crate-internal), which actually decides
/// legality; [`Scc`] itself is pure topology.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scc {
    /// Every node in this component, sorted by [`NodeId`].
    pub nodes: Vec<NodeId>,
}

/// Build the deduplicated producer→consumer adjacency used for SCC
/// detection: virtual-source edges are excluded (no producer node), and
/// parallel edges between the same two nodes collapse to one adjacency
/// entry (SCC membership only needs reachability, not edge multiplicity).
fn node_adjacency(
    nodes: &BTreeMap<NodeId, GraphNode>,
    edges: &BTreeMap<EdgeKey, Edge>,
) -> BTreeMap<NodeId, Vec<NodeId>> {
    let mut adjacency: BTreeMap<NodeId, BTreeSet<NodeId>> = nodes
        .keys()
        .map(|id| (id.clone(), BTreeSet::new()))
        .collect();
    for (key, edge) in edges {
        if let Some(producer) = edge.from.producer_node()
            && let Some(successors) = adjacency.get_mut(producer)
        {
            successors.insert(key.consumer.clone());
        }
    }
    adjacency
        .into_iter()
        .map(|(id, successors)| (id, successors.into_iter().collect()))
        .collect()
}

/// Tarjan's algorithm state, threaded through recursive `strongconnect`
/// calls via `&mut self` rather than free parameters.
struct Tarjan<'a> {
    adjacency: &'a BTreeMap<NodeId, Vec<NodeId>>,
    next_index: usize,
    index: BTreeMap<NodeId, usize>,
    lowlink: BTreeMap<NodeId, usize>,
    on_stack: BTreeSet<NodeId>,
    stack: Vec<NodeId>,
    sccs: Vec<Scc>,
}

impl<'a> Tarjan<'a> {
    fn new(adjacency: &'a BTreeMap<NodeId, Vec<NodeId>>) -> Self {
        Self {
            adjacency,
            next_index: 0,
            index: BTreeMap::new(),
            lowlink: BTreeMap::new(),
            on_stack: BTreeSet::new(),
            stack: Vec::new(),
            sccs: Vec::new(),
        }
    }

    fn strongconnect(&mut self, v: &NodeId) {
        self.index.insert(v.clone(), self.next_index);
        self.lowlink.insert(v.clone(), self.next_index);
        self.next_index += 1;
        self.stack.push(v.clone());
        self.on_stack.insert(v.clone());

        let successors = self.adjacency.get(v).cloned().unwrap_or_default();
        for w in &successors {
            if !self.index.contains_key(w) {
                self.strongconnect(w);
                let w_low = self.lowlink.get(w).copied().unwrap_or(usize::MAX);
                if let Some(v_low) = self.lowlink.get_mut(v) {
                    *v_low = (*v_low).min(w_low);
                }
            } else if self.on_stack.contains(w) {
                let w_index = self.index.get(w).copied().unwrap_or(usize::MAX);
                if let Some(v_low) = self.lowlink.get_mut(v) {
                    *v_low = (*v_low).min(w_index);
                }
            }
        }

        let (Some(&v_index), Some(&v_low)) = (self.index.get(v), self.lowlink.get(v)) else {
            // Both were just inserted above; this branch is unreachable in
            // practice but kept panic-free per policy.
            return;
        };
        if v_low == v_index {
            let mut component = Vec::new();
            while let Some(w) = self.stack.pop() {
                self.on_stack.remove(&w);
                let is_v = w == *v;
                component.push(w);
                if is_v {
                    break;
                }
            }
            component.sort();
            self.sccs.push(Scc { nodes: component });
        }
    }
}

/// Compute every strongly connected component of the node-level dataflow
/// graph, including trivial (single-node) ones — a full topological
/// census, not just the problematic cycles. See [`cycle_diagnostics`] for
/// turning this into `astrs validate` output.
pub(crate) fn compute_sccs(
    nodes: &BTreeMap<NodeId, GraphNode>,
    edges: &BTreeMap<EdgeKey, Edge>,
) -> Vec<Scc> {
    let adjacency = node_adjacency(nodes, edges);
    let mut tarjan = Tarjan::new(&adjacency);
    for id in adjacency.keys() {
        if !tarjan.index.contains_key(id) {
            tarjan.strongconnect(id);
        }
    }
    let mut sccs = tarjan.sccs;
    sccs.sort_by(|a, b| a.nodes.first().cmp(&b.nodes.first()));
    sccs
}

/// Turn the non-trivial components of `sccs` into [`Diagnostic`]s,
/// skipping any cycle whose every edge belongs to `correlated` (blueprint
/// §5.2: "cycles are legal for service patterns but flagged for plain
/// data edges as warnings").
///
/// A single-node component is only a cycle if that node has a
/// self-loop — an edge whose producer and consumer are the same node —
/// so those are checked against `edges` directly rather than assumed.
pub(crate) fn cycle_diagnostics(
    sccs: &[Scc],
    edges: &BTreeMap<EdgeKey, Edge>,
    correlated: &BTreeSet<EdgeKey>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for scc in sccs {
        let members: BTreeSet<&NodeId> = scc.nodes.iter().collect();
        if members.len() < 2 {
            // Only a cycle if the lone member has an edge back to itself.
            let Some(only) = scc.nodes.first() else {
                continue;
            };
            let has_self_loop = edges.iter().any(|(key, edge)| {
                &key.consumer == only && edge.from.producer_node() == Some(only)
            });
            if !has_self_loop {
                continue;
            }
        }

        let edges_in_scc: Vec<EdgeKey> = edges
            .iter()
            .filter(|(key, edge)| {
                members.contains(&key.consumer)
                    && edge
                        .from
                        .producer_node()
                        .is_some_and(|p| members.contains(p))
            })
            .map(|(key, _)| key.clone())
            .collect();

        let uncorrelated: Vec<EdgeKey> = edges_in_scc
            .into_iter()
            .filter(|key| !correlated.contains(key))
            .collect();

        if !uncorrelated.is_empty() {
            diagnostics.push(Diagnostic::new(
                Severity::Warning,
                DiagnosticKind::Cycle {
                    nodes: scc.nodes.clone(),
                    uncorrelated_edges: uncorrelated,
                },
            ));
        }
    }

    diagnostics
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::collections::BTreeMap;

    use super::*;
    use crate::edge::{EdgeSource, QueueConfig};
    use crate::ids::{MachineId, PortName};

    fn node(id: &str) -> GraphNode {
        GraphNode {
            id: NodeId::new(id),
            outputs: BTreeMap::new(),
            inputs: BTreeMap::new(),
            pattern: None,
            machine: MachineId::CoordinatorLocal,
            spawns_process: true,
        }
    }

    fn edge_from(producer: &str) -> Edge {
        Edge {
            from: EdgeSource::NodeOutput {
                node: NodeId::new(producer),
                output: PortName::new("out"),
            },
            queue: QueueConfig::default(),
        }
    }

    fn nodes_map(ids: &[&str]) -> BTreeMap<NodeId, GraphNode> {
        ids.iter().map(|id| (NodeId::new(*id), node(id))).collect()
    }

    #[test]
    fn linear_chain_has_only_trivial_sccs() {
        let nodes = nodes_map(&["a", "b", "c"]);
        let mut edges = BTreeMap::new();
        edges.insert(
            EdgeKey::new(NodeId::new("b"), PortName::new("in")),
            edge_from("a"),
        );
        edges.insert(
            EdgeKey::new(NodeId::new("c"), PortName::new("in")),
            edge_from("b"),
        );

        let sccs = compute_sccs(&nodes, &edges);
        assert_eq!(sccs.len(), 3);
        assert!(sccs.iter().all(|s| s.nodes.len() == 1));

        let diagnostics = cycle_diagnostics(&sccs, &edges, &BTreeSet::new());
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn two_node_cycle_is_detected() {
        let nodes = nodes_map(&["a", "b"]);
        let mut edges = BTreeMap::new();
        edges.insert(
            EdgeKey::new(NodeId::new("b"), PortName::new("in")),
            edge_from("a"),
        );
        edges.insert(
            EdgeKey::new(NodeId::new("a"), PortName::new("in")),
            edge_from("b"),
        );

        let sccs = compute_sccs(&nodes, &edges);
        let cyclic: Vec<_> = sccs.iter().filter(|s| s.nodes.len() > 1).collect();
        assert_eq!(cyclic.len(), 1);
        assert_eq!(cyclic[0].nodes, vec![NodeId::new("a"), NodeId::new("b")]);

        let diagnostics = cycle_diagnostics(&sccs, &edges, &BTreeSet::new());
        assert_eq!(diagnostics.len(), 1);
        assert!(matches!(diagnostics[0].kind, DiagnosticKind::Cycle { .. }));
    }

    #[test]
    fn cycle_fully_covered_by_pattern_correlation_is_not_flagged() {
        let nodes = nodes_map(&["client", "server"]);
        let req_key = EdgeKey::new(NodeId::new("server"), PortName::new("request"));
        let resp_key = EdgeKey::new(NodeId::new("client"), PortName::new("response"));
        let mut edges = BTreeMap::new();
        edges.insert(req_key.clone(), edge_from("client"));
        edges.insert(resp_key.clone(), edge_from("server"));

        let sccs = compute_sccs(&nodes, &edges);
        let correlated: BTreeSet<EdgeKey> = [req_key, resp_key].into_iter().collect();
        let diagnostics = cycle_diagnostics(&sccs, &edges, &correlated);
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn self_loop_is_detected_as_a_cycle() {
        let nodes = nodes_map(&["a"]);
        let mut edges = BTreeMap::new();
        edges.insert(
            EdgeKey::new(NodeId::new("a"), PortName::new("in")),
            edge_from("a"),
        );

        let sccs = compute_sccs(&nodes, &edges);
        let diagnostics = cycle_diagnostics(&sccs, &edges, &BTreeSet::new());
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn virtual_source_edges_never_create_a_cycle() {
        let nodes = nodes_map(&["a"]);
        let mut edges = BTreeMap::new();
        edges.insert(
            EdgeKey::new(NodeId::new("a"), PortName::new("tick")),
            Edge {
                from: EdgeSource::Virtual("astrs/timer/hz/50".to_string()),
                queue: QueueConfig::default(),
            },
        );
        let sccs = compute_sccs(&nodes, &edges);
        assert!(sccs.iter().all(|s| s.nodes.len() == 1));
        assert!(cycle_diagnostics(&sccs, &edges, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn scc_output_is_deterministic_across_repeated_runs() {
        let nodes = nodes_map(&["a", "b", "c", "d"]);
        let mut edges = BTreeMap::new();
        edges.insert(
            EdgeKey::new(NodeId::new("b"), PortName::new("in")),
            edge_from("a"),
        );
        edges.insert(
            EdgeKey::new(NodeId::new("a"), PortName::new("in")),
            edge_from("b"),
        );
        edges.insert(
            EdgeKey::new(NodeId::new("d"), PortName::new("in")),
            edge_from("c"),
        );

        let first = compute_sccs(&nodes, &edges);
        let second = compute_sccs(&nodes, &edges);
        assert_eq!(first, second);
    }
}
