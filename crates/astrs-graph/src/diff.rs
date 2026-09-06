//! Topology diffing for dynamic graphs (blueprint §17's `astrs node
//! add/remove/replace`, §24.1's `AddNode/RemoveNode/ReplaceNode/AddEdge/
//! RemoveEdge` control verbs).
//!
//! [`diff`] compares two [`DataflowGraph`]s and returns the
//! [`TopologyOp`] list that turns the first into the second; [`apply`]
//! runs such a list (or any other caller-built one — see below) against a
//! graph and returns the result, rejecting anything that would break one
//! of [`DataflowGraph`]'s documented invariants or introduce a new
//! strict-mode type mismatch. The round trip these two are built to
//! satisfy:
//!
//! ```
//! # use astrs_graph::{DataflowGraph, diff, apply};
//! # use astrs_manifest::Manifest;
//! # fn build(yaml: &str) -> DataflowGraph {
//! #     let manifest = Manifest::from_yaml_str(yaml).unwrap();
//! #     DataflowGraph::from_manifest(&manifest).unwrap().0
//! # }
//! let a = build("nodes:\n  - id: camera\n    path: ./camera\n    outputs: [frames]\n");
//! let b = build(
//!     "nodes:\n  - id: camera\n    path: ./camera\n    outputs: [frames]\n  \
//!      - id: detector\n    path: ./detector\n    inputs: { frames: camera/frames }\n",
//! );
//! let ops = diff(&a, &b);
//! let reconstructed = apply(&a, &ops).unwrap();
//! assert_eq!(reconstructed, b);
//! ```
//!
//! # What `diff` does not diff
//!
//! [`TopologyOp`] describes *nodes and edges only*. [`DataflowGraph::strict_types`]
//! and [`DataflowGraph::type_rules`] are graph-wide configuration, not
//! topology, so [`diff`] does not compare them and [`apply`] always
//! carries the base graph's values forward unchanged into its result —
//! two graphs that differ only in those fields produce an *empty* diff,
//! and applying that diff does **not** reproduce the second graph's
//! configuration. Callers that need to change `strict_types`/`type_rules`
//! do so directly on the returned [`DataflowGraph`]; the round-trip
//! property above only holds between graphs sharing those two fields.
//!
//! # `apply` validates any op list, not only `diff`'s own output
//!
//! `astrs node add/remove/replace` (blueprint §17) builds a [`TopologyOp`]
//! list directly from CLI input, never through [`diff`]. [`apply`]'s
//! checks — referential integrity and strict-mode type safety — hold for
//! *any* well-typed `&[TopologyOp]`, not just one [`diff`] produced.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::diagnostic::{Diagnostic, DiagnosticKind, Severity};
use crate::edge::{Edge, EdgeKey, EdgeSource};
use crate::graph::DataflowGraph;
use crate::ids::NodeId;
use crate::node::GraphNode;
use crate::typecheck;

/// One topology change between two [`DataflowGraph`]s (blueprint §24.1).
///
/// `#[non_exhaustive]`: new topology operations are added as new variants
/// at the tail, matching this crate's append-only-enum convention (see
/// [`crate::DiagnosticKind`]'s docs for the same rule stated once).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TopologyOp {
    /// A node present in the target graph but absent from the base graph.
    AddNode {
        /// The new node's id.
        id: NodeId,
        /// The new node's complete declared shape.
        node: GraphNode,
    },
    /// A node present in the base graph but absent from the target graph.
    ///
    /// Any edge that referenced this node as a producer or consumer has
    /// its own [`TopologyOp::RemoveEdge`]/[`TopologyOp::AddEdge`] entry —
    /// [`diff`] never leaves a removal's dangling references implicit,
    /// and [`apply`] rejects (rather than silently drops) an edge left
    /// referencing a node this op removed without a matching edge op.
    RemoveNode {
        /// The removed node's id.
        id: NodeId,
    },
    /// A node present, by id, in both graphs, but with a different
    /// declared shape (ports, pattern, or placement).
    ///
    /// Diffed and applied as a whole replacement rather than a
    /// field-by-field patch: [`GraphNode`] has no meaningful partial-update
    /// semantics at this layer (a port rename is not distinguishable from
    /// a port removal-plus-addition without out-of-band information this
    /// crate does not have).
    ReplaceNode {
        /// The node's id (unchanged by a replace).
        id: NodeId,
        /// The node's shape before this op — [`apply`] rejects the op if
        /// the node currently stored under `id` does not equal this,
        /// rather than silently overwriting a shape the op was not
        /// actually computed against (an optimistic-concurrency guard
        /// against applying a stale diff).
        old: GraphNode,
        /// The node's shape after this op.
        new: GraphNode,
    },
    /// An edge present in the target graph but absent from the base graph
    /// — either a new [`EdgeKey`], or an existing one whose [`Edge`]
    /// content changed (rewiring an input to a different source, or
    /// changing its queue configuration).
    AddEdge {
        /// The edge's key.
        key: EdgeKey,
        /// The edge's content after this op.
        edge: Edge,
    },
    /// An edge present in the base graph but absent from the target graph.
    RemoveEdge {
        /// The removed edge's key.
        key: EdgeKey,
    },
}

/// An error [`apply`] returns when a [`TopologyOp`] list cannot be applied
/// cleanly to a [`DataflowGraph`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ApplyError {
    /// [`TopologyOp::AddNode`] named an id already present in the graph.
    #[error("AddNode: node `{id}` already exists")]
    NodeAlreadyExists {
        /// The offending id.
        id: NodeId,
    },
    /// [`TopologyOp::RemoveNode`] or [`TopologyOp::ReplaceNode`] named an
    /// id not present in the graph.
    #[error("{op}: no node `{id}` exists in this graph")]
    NodeNotFound {
        /// Which op reported this.
        op: &'static str,
        /// The offending id.
        id: NodeId,
    },
    /// [`TopologyOp::ReplaceNode::old`] did not match the node currently
    /// stored under that id — the op was computed against a shape this
    /// graph no longer has.
    #[error("ReplaceNode: node `{id}`'s current shape does not match this op's `old` field")]
    StaleReplace {
        /// The node id whose current shape disagreed with `old`.
        id: NodeId,
    },
    /// [`TopologyOp::RemoveEdge`] named a key not present in the graph.
    #[error("RemoveEdge: no edge `{key}` exists in this graph")]
    EdgeNotFound {
        /// The offending key.
        key: EdgeKey,
    },
    /// After applying every op, some edge's consumer node no longer
    /// exists — [`DataflowGraph`]'s first documented invariant broken by
    /// this op list (a [`TopologyOp::RemoveNode`] with no matching edge
    /// removal, most commonly).
    #[error("edge `{key}`'s consumer node no longer exists after applying these ops")]
    DanglingConsumer {
        /// The offending edge key.
        key: EdgeKey,
    },
    /// After applying every op, some edge's consumer node exists but does
    /// not declare the input the edge feeds.
    #[error("edge `{key}` feeds input `{}`, which node `{}` does not declare", key.input, key.consumer)]
    UndeclaredInput {
        /// The offending edge key.
        key: EdgeKey,
    },
    /// After applying every op, some edge's producer node no longer
    /// exists.
    #[error("edge `{key}`'s producer node `{node}` does not exist after applying these ops")]
    DanglingProducer {
        /// The offending edge key.
        key: EdgeKey,
        /// The missing producer node id.
        node: NodeId,
    },
    /// After applying every op, some edge's producer node exists but does
    /// not declare the output the edge reads from.
    #[error("edge `{key}`'s producer `{node}` does not declare output `{output}`")]
    UndeclaredOutput {
        /// The offending edge key.
        key: EdgeKey,
        /// The producer node id.
        node: NodeId,
        /// The missing output name.
        output: crate::ids::PortName,
    },
    /// Applying these ops introduced (or would introduce) at least one
    /// edge that fails strict-mode type checking, where it did not before
    /// — see this module's docs on how "before" is computed.
    #[error("{count} edge(s) would newly violate strict type checking after applying these ops")]
    TypeUnsafe {
        /// The count baked into the message above (kept as a named field
        /// so the `#[error(...)]` template can reference it directly).
        count: usize,
        /// One [`Diagnostic`] per newly type-unsafe edge, for callers
        /// that want the detail — reuses [`crate::DiagnosticKind::TypeMismatch`]
        /// rather than duplicating its shape.
        mismatches: Vec<Diagnostic>,
    },
}

/// Compare two [`DataflowGraph`]s and return the ops that turn `old` into
/// `new` — see this module's top-level docs for what is and is not
/// diffed.
///
/// # Determinism
///
/// Ops are emitted in a fixed order — every `RemoveNode`, then every
/// `AddNode`/`ReplaceNode` (in [`NodeId`] order, following
/// [`DataflowGraph::nodes`]' own `BTreeMap` iteration), then every
/// `RemoveEdge`, then every `AddEdge` (in [`EdgeKey`] order) — but
/// [`apply`] does not rely on that order: it groups any input slice by op
/// kind before applying it (see that function's docs).
#[must_use]
pub fn diff(old: &DataflowGraph, new: &DataflowGraph) -> Vec<TopologyOp> {
    let mut ops = Vec::new();

    for id in old.nodes.keys() {
        if !new.nodes.contains_key(id) {
            ops.push(TopologyOp::RemoveNode { id: id.clone() });
        }
    }
    for (id, node) in &new.nodes {
        match old.nodes.get(id) {
            None => ops.push(TopologyOp::AddNode {
                id: id.clone(),
                node: node.clone(),
            }),
            Some(old_node) if old_node != node => ops.push(TopologyOp::ReplaceNode {
                id: id.clone(),
                old: old_node.clone(),
                new: node.clone(),
            }),
            Some(_) => {}
        }
    }

    for key in old.edges.keys() {
        if !new.edges.contains_key(key) {
            ops.push(TopologyOp::RemoveEdge { key: key.clone() });
        }
    }
    for (key, edge) in &new.edges {
        let changed = old.edges.get(key).is_none_or(|old_edge| old_edge != edge);
        if changed {
            ops.push(TopologyOp::AddEdge {
                key: key.clone(),
                edge: edge.clone(),
            });
        }
    }

    ops
}

/// Apply `ops` to `graph`, returning the resulting graph.
///
/// Ops are grouped by kind and applied in five fixed phases regardless of
/// their order in `ops` — every `RemoveNode`, then every `AddNode`, then
/// every `ReplaceNode`, then every `RemoveEdge`, then every `AddEdge` —
/// so that (for example) a `RemoveEdge` for an edge a `RemoveNode` in the
/// same batch orphans is always applied before the node disappears,
/// whatever order a caller listed the ops in.
///
/// After all five phases, the candidate result is checked against
/// [`DataflowGraph`]'s two documented structural invariants (every edge's
/// consumer declares the input it feeds; every non-virtual edge's
/// producer declares the output it reads) and, when
/// [`DataflowGraph::strict_types`] is `true`, against a type-safety
/// check: an edge that was not a [`Severity::Error`] mismatch in `graph`
/// but is one in the candidate is rejected. An edge that was *already* a
/// mismatch before these ops ran is not re-flagged — `apply` validates
/// what these ops changed, not the graph's pre-existing state.
///
/// # Errors
///
/// See [`ApplyError`]'s variants. Returns the *first* violation found,
/// checked in the phase order above (node ops, then edge ops, then the
/// two post-application passes) — `graph` is never partially mutated on
/// error, since this function only ever returns a new value.
pub fn apply(graph: &DataflowGraph, ops: &[TopologyOp]) -> Result<DataflowGraph, ApplyError> {
    let mut nodes = graph.nodes.clone();
    let mut edges = graph.edges.clone();

    for op in ops {
        if let TopologyOp::RemoveNode { id } = op
            && nodes.remove(id).is_none()
        {
            return Err(ApplyError::NodeNotFound {
                op: "RemoveNode",
                id: id.clone(),
            });
        }
    }
    for op in ops {
        if let TopologyOp::AddNode { id, node } = op {
            if nodes.contains_key(id) {
                return Err(ApplyError::NodeAlreadyExists { id: id.clone() });
            }
            nodes.insert(id.clone(), node.clone());
        }
    }
    for op in ops {
        if let TopologyOp::ReplaceNode { id, old, new } = op {
            match nodes.get(id) {
                None => {
                    return Err(ApplyError::NodeNotFound {
                        op: "ReplaceNode",
                        id: id.clone(),
                    });
                }
                Some(current) if current != old => {
                    return Err(ApplyError::StaleReplace { id: id.clone() });
                }
                Some(_) => {}
            }
            nodes.insert(id.clone(), new.clone());
        }
    }
    for op in ops {
        if let TopologyOp::RemoveEdge { key } = op
            && edges.remove(key).is_none()
        {
            return Err(ApplyError::EdgeNotFound { key: key.clone() });
        }
    }
    for op in ops {
        if let TopologyOp::AddEdge { key, edge } = op {
            edges.insert(key.clone(), edge.clone());
        }
    }

    let candidate = DataflowGraph {
        nodes,
        edges,
        strict_types: graph.strict_types,
        type_rules: graph.type_rules.clone(),
    };

    check_referential_integrity(&candidate)?;
    check_type_safety(graph, &candidate)?;

    Ok(candidate)
}

/// Check [`DataflowGraph`]'s two documented structural invariants against
/// `graph` as a whole, returning the first violation found in
/// [`EdgeKey`] order.
fn check_referential_integrity(graph: &DataflowGraph) -> Result<(), ApplyError> {
    for (key, edge) in &graph.edges {
        let Some(consumer) = graph.nodes.get(&key.consumer) else {
            return Err(ApplyError::DanglingConsumer { key: key.clone() });
        };
        if !consumer.inputs.contains_key(&key.input) {
            return Err(ApplyError::UndeclaredInput { key: key.clone() });
        }
        if let EdgeSource::NodeOutput { node, output } = &edge.from {
            let Some(producer) = graph.nodes.get(node) else {
                return Err(ApplyError::DanglingProducer {
                    key: key.clone(),
                    node: node.clone(),
                });
            };
            if !producer.outputs.contains_key(output) {
                return Err(ApplyError::UndeclaredOutput {
                    key: key.clone(),
                    node: node.clone(),
                    output: output.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Check that `candidate` introduces no *new* strict-mode type mismatch
/// relative to `base` — see [`apply`]'s docs for the exact rule.
fn check_type_safety(base: &DataflowGraph, candidate: &DataflowGraph) -> Result<(), ApplyError> {
    if !candidate.strict_types {
        // Non-strict mismatches are warnings, never "type-unsafe" —
        // nothing to enforce.
        return Ok(());
    }

    let mismatched_before: BTreeSet<EdgeKey> = typecheck::check_types(
        &base.nodes,
        &base.edges,
        &base.type_rules,
        base.strict_types,
    )
    .into_iter()
    .filter_map(|d| match d.kind {
        DiagnosticKind::TypeMismatch { edge, .. } => Some(edge),
        _ => None,
    })
    .collect();

    let new_violations: Vec<Diagnostic> = typecheck::check_types(
        &candidate.nodes,
        &candidate.edges,
        &candidate.type_rules,
        candidate.strict_types,
    )
    .into_iter()
    .filter(|d| {
        d.severity == Severity::Error
            && matches!(&d.kind, DiagnosticKind::TypeMismatch { edge, .. } if !mismatched_before.contains(edge))
    })
    .collect();

    if new_violations.is_empty() {
        Ok(())
    } else {
        Err(ApplyError::TypeUnsafe {
            count: new_violations.len(),
            mismatches: new_violations,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::collections::BTreeMap;

    use astrs_manifest::{Input, Manifest, Node, Urn};

    use super::*;
    use crate::edge::QueueConfig;
    use crate::ids::PortName;
    use crate::node::{InputPort, OutputPort};

    fn manifest_graph(yaml: &str) -> DataflowGraph {
        let manifest = Manifest::from_yaml_str(yaml).unwrap();
        DataflowGraph::from_manifest(&manifest).unwrap().0
    }

    fn one_node() -> DataflowGraph {
        manifest_graph("nodes:\n  - id: camera\n    path: ./camera\n    outputs: [frames]\n")
    }

    fn two_nodes_wired() -> DataflowGraph {
        manifest_graph(
            "nodes:\n  - id: camera\n    path: ./camera\n    outputs: [frames]\n  \
             - id: detector\n    path: ./detector\n    inputs: { frames: camera/frames }\n",
        )
    }

    #[test]
    fn diff_of_identical_graphs_is_empty() {
        let g = two_nodes_wired();
        assert!(diff(&g, &g).is_empty());
    }

    #[test]
    fn diff_detects_added_node_and_edge() {
        let a = one_node();
        let b = two_nodes_wired();
        let ops = diff(&a, &b);
        assert!(
            ops.iter().any(
                |op| matches!(op, TopologyOp::AddNode { id, .. } if id.as_str() == "detector")
            )
        );
        assert!(
            ops.iter()
                .any(|op| matches!(op, TopologyOp::AddEdge { .. }))
        );
    }

    #[test]
    fn diff_detects_removed_node_and_edge() {
        let a = two_nodes_wired();
        let b = one_node();
        let ops = diff(&a, &b);
        assert!(
            ops.iter()
                .any(|op| matches!(op, TopologyOp::RemoveNode { id } if id.as_str() == "detector"))
        );
        assert!(
            ops.iter()
                .any(|op| matches!(op, TopologyOp::RemoveEdge { .. }))
        );
    }

    #[test]
    fn round_trip_add_node_and_edge() {
        let a = one_node();
        let b = two_nodes_wired();
        let ops = diff(&a, &b);
        let reconstructed = apply(&a, &ops).unwrap();
        assert_eq!(reconstructed, b);
    }

    #[test]
    fn round_trip_remove_node_and_edge() {
        let a = two_nodes_wired();
        let b = one_node();
        let ops = diff(&a, &b);
        let reconstructed = apply(&a, &ops).unwrap();
        assert_eq!(reconstructed, b);
    }

    #[test]
    fn round_trip_replace_node_shape() {
        let a = two_nodes_wired();
        let b = manifest_graph(
            "nodes:\n  - id: camera\n    path: ./camera\n    outputs: [frames, debug]\n  \
             - id: detector\n    path: ./detector\n    inputs: { frames: camera/frames }\n",
        );
        let ops = diff(&a, &b);
        assert!(
            ops.iter().any(
                |op| matches!(op, TopologyOp::ReplaceNode { id, .. } if id.as_str() == "camera")
            )
        );
        let reconstructed = apply(&a, &ops).unwrap();
        assert_eq!(reconstructed, b);
    }

    #[test]
    fn round_trip_rewire_edge_to_a_new_source() {
        let a = manifest_graph(
            "nodes:\n  - id: camera\n    path: ./camera\n    outputs: [frames]\n  \
             - id: camera2\n    path: ./camera2\n    outputs: [frames]\n  \
             - id: detector\n    path: ./detector\n    inputs: { frames: camera/frames }\n",
        );
        let b = manifest_graph(
            "nodes:\n  - id: camera\n    path: ./camera\n    outputs: [frames]\n  \
             - id: camera2\n    path: ./camera2\n    outputs: [frames]\n  \
             - id: detector\n    path: ./detector\n    inputs: { frames: camera2/frames }\n",
        );
        let ops = diff(&a, &b);
        // The edge key is unchanged; only its content (producer) changed,
        // so this must be exactly one AddEdge, no Remove/Add pair.
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0], TopologyOp::AddEdge { .. }));
        let reconstructed = apply(&a, &ops).unwrap();
        assert_eq!(reconstructed, b);
    }

    #[test]
    fn diff_does_not_cover_strict_types_or_type_rules() {
        let mut a = one_node();
        a.strict_types = false;
        let mut b = one_node();
        b.strict_types = true;
        assert!(diff(&a, &b).is_empty());
    }

    #[test]
    fn add_node_rejects_an_id_that_already_exists() {
        let g = one_node();
        let ops = vec![TopologyOp::AddNode {
            id: NodeId::new("camera"),
            node: g.node(&NodeId::new("camera")).unwrap().clone(),
        }];
        let err = apply(&g, &ops).unwrap_err();
        assert!(matches!(err, ApplyError::NodeAlreadyExists { .. }));
    }

    #[test]
    fn remove_node_rejects_an_absent_id() {
        let g = one_node();
        let ops = vec![TopologyOp::RemoveNode {
            id: NodeId::new("ghost"),
        }];
        let err = apply(&g, &ops).unwrap_err();
        assert!(matches!(err, ApplyError::NodeNotFound { .. }));
    }

    #[test]
    fn remove_edge_rejects_an_absent_key() {
        let g = one_node();
        let ops = vec![TopologyOp::RemoveEdge {
            key: EdgeKey::new(NodeId::new("camera"), PortName::new("nope")),
        }];
        let err = apply(&g, &ops).unwrap_err();
        assert!(matches!(err, ApplyError::EdgeNotFound { .. }));
    }

    #[test]
    fn replace_node_rejects_a_stale_old_value() {
        let g = two_nodes_wired();
        let current = g.node(&NodeId::new("camera")).unwrap().clone();
        let mut stale = current.clone();
        stale.outputs.insert(
            PortName::new("not-actually-there"),
            OutputPort { type_urn: None },
        );
        let ops = vec![TopologyOp::ReplaceNode {
            id: NodeId::new("camera"),
            old: stale,
            new: current,
        }];
        let err = apply(&g, &ops).unwrap_err();
        assert!(matches!(err, ApplyError::StaleReplace { .. }));
    }

    #[test]
    fn remove_node_without_removing_its_edge_is_rejected() {
        let g = two_nodes_wired();
        // Removing `camera` alone, with no matching `RemoveEdge` for
        // `detector.frames`, would leave a dangling producer reference.
        let ops = vec![TopologyOp::RemoveNode {
            id: NodeId::new("camera"),
        }];
        let err = apply(&g, &ops).unwrap_err();
        assert!(matches!(err, ApplyError::DanglingProducer { .. }));
    }

    #[test]
    fn add_edge_to_an_undeclared_input_is_rejected() {
        let g = one_node();
        let ops = vec![TopologyOp::AddEdge {
            key: EdgeKey::new(NodeId::new("camera"), PortName::new("not-declared")),
            edge: Edge {
                from: EdgeSource::Virtual("astrs/status".to_string()),
                queue: QueueConfig::default(),
            },
        }];
        let err = apply(&g, &ops).unwrap_err();
        assert!(matches!(err, ApplyError::UndeclaredInput { .. }));
    }

    #[test]
    fn add_edge_from_an_undeclared_output_is_rejected() {
        let a = two_nodes_wired();
        let ops = vec![TopologyOp::AddEdge {
            key: EdgeKey::new(NodeId::new("detector"), PortName::new("frames")),
            edge: Edge {
                from: EdgeSource::NodeOutput {
                    node: NodeId::new("camera"),
                    output: PortName::new("not-an-output"),
                },
                queue: QueueConfig::default(),
            },
        }];
        let err = apply(&a, &ops).unwrap_err();
        assert!(matches!(err, ApplyError::UndeclaredOutput { .. }));
    }

    #[test]
    fn strict_mode_rejects_a_newly_introduced_mismatch() {
        let mut camera = Node::with_path("camera", "./camera");
        camera.outputs = vec!["frames".to_string()];
        camera
            .output_types
            .insert("frames".to_string(), Urn::new("std/core/v1/Float32"));
        let mut detector = Node::with_path("detector", "./detector");
        detector
            .inputs
            .insert("frames".to_string(), Input::from_source("camera/frames"));
        detector
            .input_types
            .insert("frames".to_string(), Urn::new("std/core/v1/Float64"));
        let manifest = Manifest {
            nodes: vec![camera, detector],
            strict_types: true,
            ..Manifest::default()
        };
        let (full, _) = DataflowGraph::from_manifest(&manifest).unwrap();

        // `base` has only `camera` and no edges at all — the mismatch
        // cannot exist yet. The ops below add `detector` and wire the
        // mismatched edge in one batch, so the violation they introduce
        // is genuinely new relative to `base`.
        let mut base_nodes = BTreeMap::new();
        base_nodes.insert(
            NodeId::new("camera"),
            full.node(&NodeId::new("camera")).unwrap().clone(),
        );
        let base = DataflowGraph {
            nodes: base_nodes,
            edges: BTreeMap::new(),
            strict_types: true,
            type_rules: Vec::new(),
        };

        let edge_key = EdgeKey::new(NodeId::new("detector"), PortName::new("frames"));
        let ops = vec![
            TopologyOp::AddNode {
                id: NodeId::new("detector"),
                node: full.node(&NodeId::new("detector")).unwrap().clone(),
            },
            TopologyOp::AddEdge {
                key: edge_key.clone(),
                edge: full.edge(&edge_key).unwrap().clone(),
            },
        ];

        let err = apply(&base, &ops).unwrap_err();
        assert!(matches!(err, ApplyError::TypeUnsafe { count: 1, .. }));
    }

    #[test]
    fn pre_existing_mismatch_is_not_re_flagged_by_an_unrelated_op() {
        let mut camera = Node::with_path("camera", "./camera");
        camera.outputs = vec!["frames".to_string(), "extra".to_string()];
        camera
            .output_types
            .insert("frames".to_string(), Urn::new("std/core/v1/Float32"));
        let mut detector = Node::with_path("detector", "./detector");
        detector
            .inputs
            .insert("frames".to_string(), Input::from_source("camera/frames"));
        detector
            .input_types
            .insert("frames".to_string(), Urn::new("std/core/v1/Float64"));
        let manifest = Manifest {
            nodes: vec![camera, detector],
            strict_types: true,
            ..Manifest::default()
        };
        let (graph, _) = DataflowGraph::from_manifest(&manifest).unwrap();
        // This graph already has one strict-mode mismatch (camera/frames
        // -> detector.frames). An unrelated op (adding a brand-new,
        // untyped edge on camera's second output) must not be rejected
        // because of it.
        let mut extra_node = graph.node(&NodeId::new("detector")).unwrap().clone();
        extra_node
            .inputs
            .insert(PortName::new("extra"), InputPort { type_urn: None });
        let ops = vec![
            TopologyOp::ReplaceNode {
                id: NodeId::new("detector"),
                old: graph.node(&NodeId::new("detector")).unwrap().clone(),
                new: extra_node,
            },
            TopologyOp::AddEdge {
                key: EdgeKey::new(NodeId::new("detector"), PortName::new("extra")),
                edge: Edge {
                    from: EdgeSource::NodeOutput {
                        node: NodeId::new("camera"),
                        output: PortName::new("extra"),
                    },
                    queue: QueueConfig::default(),
                },
            },
        ];

        let result = apply(&graph, &ops).unwrap();
        assert_eq!(result.node_count(), 2);
    }
}
