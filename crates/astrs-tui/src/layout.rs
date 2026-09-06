//! The Graph tab's compact layered layout: grouping a
//! [`astrs_graph::DataflowGraph`]'s nodes into rows by data-flow depth, so
//! the tab can render "camera, then detector, then planner" top to bottom
//! instead of an unordered node list.
//!
//! # Algorithm
//!
//! Longest-path layering by bounded relaxation: every node starts at layer
//! 0; for up to `node_count` passes, every edge `producer → consumer`
//! (excluding virtual sources, which have no producer node) pulls its
//! consumer to at least `layer(producer) + 1`; the loop stops early once a
//! pass changes nothing. On an acyclic graph this converges to the exact
//! longest-path layering in at most `node_count - 1` passes. On a graph
//! with a cycle (a service/action pattern's paired edges, blueprint §9.4)
//! it does **not** converge to a global optimum — it simply stops after
//! `node_count` passes, because the loop's own bound guarantees
//! termination regardless of whether the relaxation would otherwise keep
//! finding "one more" layer to push a node to around the cycle. The result
//! is still a valid, deterministic row grouping; it is not claimed to be
//! the minimum-height one. [`astrs_graph::Scc`] already computes exactly
//! which nodes participate in a cycle, and collapsing each SCC to one
//! layer before layering across SCCs is the principled fix — left for a
//! later pass since a compact, terminating layout was this tab's bar, not
//! an optimal one.

use std::collections::BTreeMap;

use astrs_graph::{DataflowGraph, NodeId};

/// The Graph tab's node-to-row assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphLayout {
    /// Layer index → the node ids placed there, in [`NodeId`] order.
    pub layers: Vec<Vec<NodeId>>,
}

impl GraphLayout {
    /// Computes the layering for `graph` — see this module's docs for the
    /// algorithm and its known simplification on cyclic graphs.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_graph::DataflowGraph;
    /// use astrs_manifest::{Input, Manifest, Node};
    /// use astrs_tui::GraphLayout;
    ///
    /// let mut camera = Node::with_path("camera", "./camera");
    /// camera.outputs = vec!["frames".to_owned()];
    /// let mut detector = Node::with_path("detector", "./detector");
    /// detector
    ///     .inputs
    ///     .insert("frames".to_owned(), Input::from_source("camera/frames"));
    /// let manifest = Manifest {
    ///     nodes: vec![camera, detector],
    ///     ..Manifest::default()
    /// };
    /// let (graph, _diagnostics) = DataflowGraph::from_manifest(&manifest).unwrap();
    ///
    /// let layout = GraphLayout::compute(&graph);
    /// assert_eq!(layout.layers.len(), 2, "camera, then detector");
    /// assert_eq!(layout.node_count(), 2);
    /// ```
    #[must_use]
    pub fn compute(graph: &DataflowGraph) -> Self {
        let node_count = graph.nodes.len();
        let mut layer_of: BTreeMap<NodeId, usize> =
            graph.nodes.keys().map(|id| (id.clone(), 0)).collect();

        for _ in 0..node_count {
            let mut changed = false;
            for (key, edge) in &graph.edges {
                let Some(producer) = edge.from.producer_node() else {
                    continue;
                };
                let producer_layer = layer_of.get(producer).copied().unwrap_or(0);
                let consumer_layer = layer_of.entry(key.consumer.clone()).or_insert(0);
                if *consumer_layer < producer_layer + 1 {
                    *consumer_layer = producer_layer + 1;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        let max_layer = layer_of.values().copied().max().unwrap_or(0);
        let mut layers: Vec<Vec<NodeId>> = vec![Vec::new(); max_layer + 1];
        // `layer_of` is a `BTreeMap`, so this iterates in `NodeId` order —
        // each layer's row is alphabetical, which is what keeps the
        // rendered tab deterministic and golden-testable.
        for (id, layer) in layer_of {
            layers[layer].push(id);
        }
        Self { layers }
    }

    /// The layer index `id` was placed in, if it names a node this layout
    /// covers.
    #[must_use]
    pub fn layer_of(&self, id: &NodeId) -> Option<usize> {
        self.layers.iter().position(|layer| layer.contains(id))
    }

    /// The total node count across every layer.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.layers.iter().map(Vec::len).sum()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_manifest::{Input, Manifest, Node};

    use super::*;

    fn graph_of(manifest: Manifest) -> DataflowGraph {
        DataflowGraph::from_manifest(&manifest).unwrap().0
    }

    #[test]
    fn a_linear_chain_layers_one_node_per_row_in_order() {
        let mut camera = Node::with_path("camera", "./camera");
        camera.outputs = vec!["frames".to_owned()];
        let mut detector = Node::with_path("detector", "./detector");
        detector.outputs = vec!["detections".to_owned()];
        detector
            .inputs
            .insert("frames".to_owned(), Input::from_source("camera/frames"));
        let mut planner = Node::with_path("planner", "./planner");
        planner.inputs.insert(
            "detections".to_owned(),
            Input::from_source("detector/detections"),
        );
        let graph = graph_of(Manifest {
            nodes: vec![camera, detector, planner],
            ..Manifest::default()
        });

        let layout = GraphLayout::compute(&graph);
        assert_eq!(layout.layers.len(), 3);
        assert_eq!(layout.layers[0], vec![NodeId::new("camera")]);
        assert_eq!(layout.layers[1], vec![NodeId::new("detector")]);
        assert_eq!(layout.layers[2], vec![NodeId::new("planner")]);
        assert_eq!(layout.node_count(), 3);
    }

    #[test]
    fn a_node_fed_only_by_a_virtual_source_stays_at_layer_zero() {
        let mut node = Node::with_path("planner", "./planner");
        node.inputs
            .insert("tick".to_owned(), Input::from_source("astrs/timer/hz/50"));
        let graph = graph_of(Manifest {
            nodes: vec![node],
            ..Manifest::default()
        });

        let layout = GraphLayout::compute(&graph);
        assert_eq!(layout.layers.len(), 1);
        assert_eq!(layout.layer_of(&NodeId::new("planner")), Some(0));
    }

    #[test]
    fn nodes_sharing_a_layer_are_ordered_alphabetically() {
        let mut camera = Node::with_path("camera", "./camera");
        camera.outputs = vec!["frames".to_owned()];
        let mut zebra = Node::with_path("zebra", "./zebra");
        zebra
            .inputs
            .insert("frames".to_owned(), Input::from_source("camera/frames"));
        let mut alpha = Node::with_path("alpha", "./alpha");
        alpha
            .inputs
            .insert("frames".to_owned(), Input::from_source("camera/frames"));
        let graph = graph_of(Manifest {
            nodes: vec![camera, zebra, alpha],
            ..Manifest::default()
        });

        let layout = GraphLayout::compute(&graph);
        assert_eq!(
            layout.layers[1],
            vec![NodeId::new("alpha"), NodeId::new("zebra")]
        );
    }

    /// A fully-wired service pattern (caller ⇄ adder) is a 2-node cycle at
    /// the node-topology level. The layering must still terminate — the
    /// whole point of the bounded relaxation — and place both nodes
    /// deterministically, even though (per this module's docs) the result
    /// is not claimed to be an optimal layering of the cycle.
    #[test]
    fn a_cyclic_service_pattern_terminates_and_layers_deterministically() {
        let mut caller = Node::with_path("caller", "./caller");
        caller.pattern = Some(astrs_manifest::Pattern::ServiceClient);
        caller.outputs = vec!["request".to_owned()];
        let mut adder = Node::with_path("adder", "./adder");
        adder.pattern = Some(astrs_manifest::Pattern::ServiceServer);
        adder.outputs = vec!["response".to_owned()];
        adder
            .inputs
            .insert("request".to_owned(), Input::from_source("caller/request"));
        caller
            .inputs
            .insert("response".to_owned(), Input::from_source("adder/response"));
        let graph = graph_of(Manifest {
            nodes: vec![caller, adder],
            ..Manifest::default()
        });

        // Must terminate (the test itself is the termination proof: it
        // would hang forever on an unbounded relaxation over a cycle).
        let layout = GraphLayout::compute(&graph);
        assert_eq!(layout.node_count(), 2);
        assert!(layout.layer_of(&NodeId::new("caller")).is_some());
        assert!(layout.layer_of(&NodeId::new("adder")).is_some());

        // Deterministic: computing it again from the same graph gives the
        // same layering.
        let again = GraphLayout::compute(&graph);
        assert_eq!(layout, again);
    }

    #[test]
    fn an_empty_graph_layers_to_nothing() {
        let graph = graph_of(Manifest::default());
        let layout = GraphLayout::compute(&graph);
        assert_eq!(layout.node_count(), 0);
        assert_eq!(layout.layer_of(&NodeId::new("nope")), None);
    }

    #[test]
    fn a_diamond_places_the_join_node_at_the_deeper_branchs_layer() {
        let mut source = Node::with_path("source", "./source");
        source.outputs = vec!["out".to_owned()];
        let mut short = Node::with_path("short", "./short");
        short.outputs = vec!["out".to_owned()];
        short
            .inputs
            .insert("in".to_owned(), Input::from_source("source/out"));
        let mut long_a = Node::with_path("long_a", "./long_a");
        long_a.outputs = vec!["out".to_owned()];
        long_a
            .inputs
            .insert("in".to_owned(), Input::from_source("source/out"));
        let mut long_b = Node::with_path("long_b", "./long_b");
        long_b.outputs = vec!["out".to_owned()];
        long_b
            .inputs
            .insert("in".to_owned(), Input::from_source("long_a/out"));
        let mut join = Node::with_path("join", "./join");
        join.inputs
            .insert("a".to_owned(), Input::from_source("short/out"));
        join.inputs
            .insert("b".to_owned(), Input::from_source("long_b/out"));
        let graph = graph_of(Manifest {
            nodes: vec![source, short, long_a, long_b, join],
            ..Manifest::default()
        });

        let layout = GraphLayout::compute(&graph);
        // source=0, short=1, long_a=1, long_b=2, join must be after its
        // deepest input (long_b at 2), so join=3.
        assert_eq!(layout.layer_of(&NodeId::new("source")), Some(0));
        assert_eq!(layout.layer_of(&NodeId::new("long_b")), Some(2));
        assert_eq!(layout.layer_of(&NodeId::new("join")), Some(3));
    }
}
