//! The dataflow graph model: [`DataflowGraph`] and its construction from a
//! validated manifest (blueprint §5.2, §8).

use std::collections::{BTreeMap, BTreeSet};

use astrs_manifest::{Manifest, Node, TypeRule};
use serde::{Deserialize, Serialize};

use crate::diagnostic::{Diagnostic, DiagnosticKind, Severity};
use crate::edge::{Edge, EdgeKey, EdgeSource, QueueConfig};
use crate::ids::{MachineId, NodeId, PortName};
use crate::node::{GraphNode, InputPort, OutputPort};
use crate::pattern::{self, PatternPair};
use crate::scc::{self, Scc};
use crate::typecheck::{self, EdgeTypeStatus};

/// A prefix reserved for record-sugar-synthesized port names (see
/// [`DataflowGraph::from_manifest`]'s `record:` handling). Chosen to start
/// with `_` for the same reason blueprint §8.5's module expansion uses
/// `_mod/<port>` for its own synthesized ids: an underscore-prefixed
/// segment is conventionally "generated, do not hand-write this."
const RECORD_PORT_PREFIX: &str = "_record";

/// An error building a [`DataflowGraph`] from a [`Manifest`] that was not
/// actually valid.
///
/// [`DataflowGraph::from_manifest`] does not re-run
/// [`Manifest::validate`](astrs_manifest::Manifest::validate) — that
/// would mean validating twice on every normal call path, since a caller
/// is expected to validate first (blueprint: "Build a `DataflowGraph`
/// from a validated+expanded Manifest"). What it does instead is stay
/// panic-free on a manifest that skipped validation: every place a
/// validated manifest guarantees an invariant (unique node ids, resolved
/// input references), this constructor checks it explicitly and reports
/// a [`GraphBuildError`] rather than indexing into a `BTreeMap` and
/// trusting the key is there.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GraphBuildError {
    /// Two nodes in the manifest declared the same id.
    ///
    /// [`Manifest::validate`](astrs_manifest::Manifest::validate) already
    /// rejects this; seeing it here means the manifest was not validated
    /// first.
    #[error(
        "duplicate node id `{id}` (this manifest was not validated: call Manifest::validate first)"
    )]
    DuplicateNodeId {
        /// The repeated id.
        id: String,
    },
    /// An input or `record:` entry's source string was neither a
    /// recognized `astrs/...` virtual source nor a `node/output` pair.
    ///
    /// [`Manifest::validate`](astrs_manifest::Manifest::validate) already
    /// rejects this via its own `UnresolvedReference` check; seeing it
    /// here means the manifest was not validated first.
    #[error(
        "node `{node}` input `{input}` has a malformed source `{source_text}` (this manifest was \
         not validated: call Manifest::validate first)"
    )]
    MalformedSource {
        /// The node declaring the malformed input.
        node: String,
        /// The input (or synthesized `_record/N`) name.
        input: String,
        /// The offending source string.
        ///
        /// Named `source_text` rather than `source`: thiserror treats a
        /// field literally named `source` as the `Error::source()`
        /// provider and requires it to implement `std::error::Error`,
        /// which a plain diagnostic string does not.
        source_text: String,
    },
}

/// The dataflow graph: nodes, ports and edges built from a manifest
/// (blueprint §5.2), plus every derived analysis this crate performs over
/// that shape.
///
/// # Invariants
///
/// A [`DataflowGraph`] returned by [`DataflowGraph::from_manifest`] on a
/// [validated](astrs_manifest::Manifest::validate) manifest maintains:
///
/// - Every [`EdgeKey::consumer`] names a node present in [`Self::nodes`],
///   and [`EdgeKey::input`] is a key of that node's
///   [`GraphNode::inputs`].
/// - Every [`EdgeSource::NodeOutput`] producer node/output pair either
///   names a node present in [`Self::nodes`] and one of its
///   [`GraphNode::outputs`] keys, or (for a `record:`-sugar edge, or any
///   edge built by [`crate::diff::apply`] without re-validating) may
///   dangle — callers that build or mutate a graph by hand are
///   responsible for this invariant; [`crate::diff::apply`] checks it at
///   apply time (see that module).
///
/// These invariants are *maintained*, not re-checked on every access —
/// see [`crate::diff`] for the one place graphs are mutated after
/// construction, and how it re-validates them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataflowGraph {
    /// Every node, keyed by id.
    pub nodes: BTreeMap<NodeId, GraphNode>,
    /// Every edge, keyed by its consumer `(node, input)` pair — see
    /// [`EdgeKey`]'s docs for why that pair is a valid primary key.
    pub edges: BTreeMap<EdgeKey, Edge>,
    /// Whether a type mismatch with no applicable [`TypeRule`] is an
    /// [`Severity::Error`] (`true`) or a [`Severity::Warning`] (`false`),
    /// taken from [`Manifest::strict_types`](astrs_manifest::Manifest::strict_types).
    pub strict_types: bool,
    /// Implicit type-coercion rules, taken from
    /// [`Manifest::type_rules`](astrs_manifest::Manifest::type_rules).
    pub type_rules: Vec<TypeRule>,
}

impl DataflowGraph {
    /// Build an empty graph with the given graph-wide type-checking
    /// configuration — mainly useful for tests and for
    /// [`crate::diff::apply`]'s starting point.
    #[must_use]
    pub fn new(strict_types: bool, type_rules: Vec<TypeRule>) -> Self {
        Self {
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            strict_types,
            type_rules,
        }
    }

    /// Build a [`DataflowGraph`] from a manifest.
    ///
    /// The caller should call
    /// [`Manifest::validate`](astrs_manifest::Manifest::validate) (and,
    /// once implemented, module expansion — see this crate's top-level
    /// docs) first: this constructor trusts the manifest's cross-references
    /// resolve and its node ids are unique, and returns
    /// [`GraphBuildError`] rather than panicking on a manifest that
    /// skipped that step, but it does not re-run every check `validate`
    /// performs (in particular it does not re-check URN syntax — a
    /// syntactically invalid `Urn` is carried through as-is and will
    /// simply never compare equal to anything in
    /// [`DataflowGraph::diagnostics`]'s type check).
    ///
    /// Returns the graph plus a list of **construction-time** diagnostics
    /// — [`DiagnosticKind::OrphanedOutputType`],
    /// [`DiagnosticKind::OrphanedInputType`] and
    /// [`DiagnosticKind::RecordSugarPortCollision`] — which need the raw
    /// manifest shape to detect and so cannot be recomputed later from
    /// the graph alone. Every other diagnostic (type mismatches, cycles,
    /// pattern pairing, unconsumed outputs) is derivable from the graph
    /// alone and lives behind [`DataflowGraph::diagnostics`] instead —
    /// call both to get the complete `astrs validate` report.
    ///
    /// # Errors
    ///
    /// Returns [`GraphBuildError`] if the manifest has a duplicate node
    /// id or a malformed input/record source string — both conditions
    /// [`Manifest::validate`](astrs_manifest::Manifest::validate) already
    /// rejects, so this only triggers on an unvalidated manifest.
    pub fn from_manifest(manifest: &Manifest) -> Result<(Self, Vec<Diagnostic>), GraphBuildError> {
        let mut nodes = BTreeMap::new();
        let mut diagnostics = Vec::new();

        for node in &manifest.nodes {
            let id = NodeId::new(node.id.clone());
            if nodes.contains_key(&id) {
                return Err(GraphBuildError::DuplicateNodeId {
                    id: node.id.clone(),
                });
            }
            let graph_node = build_graph_node(manifest, node, &id, &mut diagnostics);
            nodes.insert(id, graph_node);
        }

        let mut edges = BTreeMap::new();
        for node in &manifest.nodes {
            let id = NodeId::new(node.id.clone());
            wire_explicit_inputs(node, &id, &mut edges)?;
            wire_record_sugar(node, &id, &mut nodes, &mut edges, &mut diagnostics)?;
        }

        let graph = Self {
            nodes,
            edges,
            strict_types: manifest.strict_types,
            type_rules: manifest.type_rules.clone(),
        };
        Ok((graph, diagnostics))
    }

    /// The graph's node count.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// The graph's edge count.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Look up one node by id.
    #[must_use]
    pub fn node(&self, id: &NodeId) -> Option<&GraphNode> {
        self.nodes.get(id)
    }

    /// Look up one edge by its consumer `(node, input)` key.
    #[must_use]
    pub fn edge(&self, key: &EdgeKey) -> Option<&Edge> {
        self.edges.get(key)
    }

    /// Every edge whose consumer is `node`, i.e. `node`'s wired inputs.
    ///
    /// `node` is cloned into the returned iterator's filter closure rather
    /// than borrowed for its lifetime: under edition 2024's return-position
    /// `impl Trait` capture rules, a returned opaque type captures every
    /// lifetime in scope by default, so borrowing `node` for as long as
    /// `Self`'s own `'a` would force every caller to keep a `NodeId` value
    /// alive for as long as the returned iterator — including a bare
    /// `graph.edges_into(&NodeId::new("x"))` call, whose temporary would
    /// otherwise be dropped mid-borrow. `+ use<'a>` makes the (otherwise
    /// implicit) precise-capture set explicit: only `self`'s borrow is
    /// captured.
    pub fn edges_into<'a>(
        &'a self,
        node: &NodeId,
    ) -> impl Iterator<Item = (&'a EdgeKey, &'a Edge)> + use<'a> {
        let node = node.clone();
        self.edges
            .iter()
            .filter(move |(key, _)| key.consumer == node)
    }

    /// Every edge whose producer is `node`, i.e. edges fed by `node`'s
    /// outputs. Excludes virtual-source edges by construction (they have
    /// no producer node). See [`DataflowGraph::edges_into`]'s docs for why
    /// `node` is cloned rather than borrowed into the returned iterator.
    pub fn edges_from<'a>(
        &'a self,
        node: &NodeId,
    ) -> impl Iterator<Item = (&'a EdgeKey, &'a Edge)> + use<'a> {
        let node = node.clone();
        self.edges
            .iter()
            .filter(move |(_, edge)| edge.from.producer_node() == Some(&node))
    }

    /// The type-check status of one edge — see [`EdgeTypeStatus`].
    ///
    /// Returns `None` if `key` does not name an edge in this graph.
    #[must_use]
    pub fn edge_type_status(&self, key: &EdgeKey) -> Option<EdgeTypeStatus> {
        let edge = self.edges.get(key)?;
        Some(typecheck::edge_status(
            &self.nodes,
            &key.consumer,
            &key.input,
            edge,
            &self.type_rules,
        ))
    }

    /// Every strongly connected component of the node-level graph
    /// (blueprint §5.2's "stable topological metadata"), including
    /// trivial single-node components — see [`Scc`].
    #[must_use]
    pub fn sccs(&self) -> Vec<Scc> {
        scc::compute_sccs(&self.nodes, &self.edges)
    }

    /// Every derived service/action request/response correlation — see
    /// [`PatternPair`].
    #[must_use]
    pub fn pattern_pairs(&self) -> Vec<PatternPair> {
        pattern::derive_pattern_pairs(&self.nodes, &self.edges).0
    }

    /// The complete structural diagnostics list — what `astrs validate`
    /// prints, minus the construction-time diagnostics
    /// [`DataflowGraph::from_manifest`] already returned (see that
    /// method's docs for why those cannot be recomputed here).
    ///
    /// Runs a fixed pipeline, in order: edge type checking
    /// ([`EdgeTypeStatus`]), pattern pairing
    /// ([`crate::PatternPair`], surfacing [`DiagnosticKind::UnpairedPatternNode`]
    /// / [`DiagnosticKind::PartialPatternPair`]), cycle detection
    /// ([`crate::Scc`], exempting cycles fully covered by a pattern
    /// correlation), then unconsumed-output detection. Within each stage,
    /// results are ordered by the node id / edge key `BTreeMap` iteration
    /// they were found in — deterministic, but not globally re-sorted by
    /// severity (matching `astrs-manifest`'s own `ValidationErrors`
    /// convention: report order, not severity order).
    #[must_use]
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        let mut out = Vec::new();

        out.extend(typecheck::check_types(
            &self.nodes,
            &self.edges,
            &self.type_rules,
            self.strict_types,
        ));

        let (pairs, pattern_diagnostics) = pattern::derive_pattern_pairs(&self.nodes, &self.edges);
        out.extend(pattern_diagnostics);

        let correlated: BTreeSet<EdgeKey> =
            pairs.iter().flat_map(PatternPair::edges).cloned().collect();
        let sccs = scc::compute_sccs(&self.nodes, &self.edges);
        out.extend(scc::cycle_diagnostics(&sccs, &self.edges, &correlated));

        out.extend(self.unconsumed_output_diagnostics());

        out
    }

    fn unconsumed_output_diagnostics(&self) -> Vec<Diagnostic> {
        let mut consumed: BTreeSet<(&NodeId, &PortName)> = BTreeSet::new();
        for edge in self.edges.values() {
            if let EdgeSource::NodeOutput { node, output } = &edge.from {
                consumed.insert((node, output));
            }
        }

        let mut out = Vec::new();
        for (node_id, node) in &self.nodes {
            for output in node.outputs.keys() {
                if !consumed.contains(&(node_id, output)) {
                    out.push(Diagnostic::new(
                        Severity::Info,
                        DiagnosticKind::UnconsumedOutput {
                            node: node_id.clone(),
                            output: output.clone(),
                        },
                    ));
                }
            }
        }
        out
    }
}

/// Build one [`GraphNode`] from its manifest [`Node`], recording
/// [`DiagnosticKind::OrphanedOutputType`] / [`DiagnosticKind::OrphanedInputType`]
/// for `output_types`/`input_types` entries with no matching declared
/// port.
fn build_graph_node(
    manifest: &Manifest,
    node: &Node,
    id: &NodeId,
    diagnostics: &mut Vec<Diagnostic>,
) -> GraphNode {
    let mut outputs = BTreeMap::new();
    for name in &node.outputs {
        outputs.insert(
            PortName::new(name.clone()),
            OutputPort {
                type_urn: node.output_types.get(name).cloned(),
            },
        );
    }
    for name in node.output_types.keys() {
        if !node.outputs.iter().any(|o| o == name) {
            diagnostics.push(Diagnostic::new(
                Severity::Warning,
                DiagnosticKind::OrphanedOutputType {
                    node: id.clone(),
                    output: PortName::new(name.clone()),
                },
            ));
        }
    }

    let mut inputs = BTreeMap::new();
    for name in node.inputs.keys() {
        inputs.insert(
            PortName::new(name.clone()),
            InputPort {
                type_urn: node.input_types.get(name).cloned(),
            },
        );
    }
    for name in node.input_types.keys() {
        if !node.inputs.contains_key(name) {
            diagnostics.push(Diagnostic::new(
                Severity::Warning,
                DiagnosticKind::OrphanedInputType {
                    node: id.clone(),
                    input: PortName::new(name.clone()),
                },
            ));
        }
    }

    GraphNode {
        id: id.clone(),
        outputs,
        inputs,
        pattern: node.pattern,
        machine: MachineId::resolve(manifest.deploy.as_ref(), node.deploy.as_ref()),
        spawns_process: !node.is_dynamic_path(),
    }
}

/// Parse a manifest source string (an input's `source`, or a `record:`
/// entry) into an [`EdgeSource`], mirroring
/// `astrs_manifest`'s own `NodeIndex::resolve` grammar
/// (`astrs/...` virtual source, else split on the first `/` into
/// `node/output`) so both crates recognize exactly the same strings.
fn parse_source(source: &str) -> Option<EdgeSource> {
    if astrs_manifest::recognize_virtual_source(source).is_some() {
        return Some(EdgeSource::Virtual(source.to_string()));
    }
    let (node_id, output) = source.split_once('/')?;
    if node_id.is_empty() || output.is_empty() {
        return None;
    }
    Some(EdgeSource::NodeOutput {
        node: NodeId::new(node_id),
        output: PortName::new(output),
    })
}

/// Wire every explicitly declared `inputs:` entry of `node` into `edges`.
fn wire_explicit_inputs(
    node: &Node,
    id: &NodeId,
    edges: &mut BTreeMap<EdgeKey, Edge>,
) -> Result<(), GraphBuildError> {
    for (name, input) in &node.inputs {
        let from = parse_source(&input.source).ok_or_else(|| GraphBuildError::MalformedSource {
            node: node.id.clone(),
            input: name.clone(),
            source_text: input.source.clone(),
        })?;
        let key = EdgeKey::new(id.clone(), PortName::new(name.clone()));
        edges.insert(
            key,
            Edge {
                from,
                queue: QueueConfig {
                    size: input.queue_size,
                    policy: input.queue_policy,
                    timeout: input.timeout,
                },
            },
        );
    }
    Ok(())
}

/// Expand `node`'s `record:` sugar (blueprint §14) into synthesized
/// `_record/<index>` input ports and edges, skipping (with a
/// [`DiagnosticKind::RecordSugarPortCollision`]) any index whose
/// synthesized name collides with an explicitly declared input — the
/// explicit declaration wins.
fn wire_record_sugar(
    node: &Node,
    id: &NodeId,
    nodes: &mut BTreeMap<NodeId, GraphNode>,
    edges: &mut BTreeMap<EdgeKey, Edge>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<(), GraphBuildError> {
    let Some(record) = &node.record else {
        return Ok(());
    };
    for (index, value) in record.iter().enumerate() {
        let port = PortName::new(format!("{RECORD_PORT_PREFIX}/{index}"));
        if node.inputs.contains_key(port.as_str()) {
            diagnostics.push(Diagnostic::new(
                Severity::Warning,
                DiagnosticKind::RecordSugarPortCollision {
                    node: id.clone(),
                    port,
                },
            ));
            continue;
        }
        let from = parse_source(value).ok_or_else(|| GraphBuildError::MalformedSource {
            node: node.id.clone(),
            input: port.as_str().to_string(),
            source_text: value.clone(),
        })?;
        if let Some(graph_node) = nodes.get_mut(id) {
            graph_node
                .inputs
                .insert(port.clone(), InputPort { type_urn: None });
        }
        edges.insert(
            EdgeKey::new(id.clone(), port),
            Edge {
                from,
                queue: QueueConfig::default(),
            },
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use astrs_manifest::{Deploy, Manifest};

    use super::*;

    fn two_node_manifest() -> Manifest {
        let mut camera = Node::with_path("camera", "./camera");
        camera.outputs = vec!["frames".to_string()];
        camera.output_types.insert(
            "frames".to_string(),
            astrs_manifest::Urn::new("std/media/v1/Image"),
        );

        let mut detector = Node::with_path("detector", "./detector");
        detector.inputs.insert(
            "frames".to_string(),
            astrs_manifest::Input::from_source("camera/frames"),
        );

        // `..Manifest::default()` rather than a full struct literal:
        // `Manifest` gains fields as `astrs-manifest` grows (e.g. the
        // `module:` header), and this crate's tests should not need to
        // track every one of them just to build a two-node fixture.
        Manifest {
            nodes: vec![camera, detector],
            ..Manifest::default()
        }
    }

    #[test]
    fn builds_nodes_and_edges_from_manifest() {
        let manifest = two_node_manifest();
        let (graph, diagnostics) = DataflowGraph::from_manifest(&manifest).unwrap();
        assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
        assert_eq!(graph.node_count(), 2);
        assert_eq!(graph.edge_count(), 1);

        let camera = graph.node(&NodeId::new("camera")).unwrap();
        assert_eq!(
            camera
                .output_type(&PortName::new("frames"))
                .map(|u| u.as_str()),
            Some("std/media/v1/Image")
        );

        let edge_key = EdgeKey::new(NodeId::new("detector"), PortName::new("frames"));
        let edge = graph.edge(&edge_key).unwrap();
        assert_eq!(
            edge.from,
            EdgeSource::NodeOutput {
                node: NodeId::new("camera"),
                output: PortName::new("frames"),
            }
        );
    }

    #[test]
    fn duplicate_node_id_is_reported_not_panicked() {
        let mut manifest = two_node_manifest();
        manifest.nodes[1].id = "camera".to_string();
        let err = DataflowGraph::from_manifest(&manifest).unwrap_err();
        assert!(matches!(err, GraphBuildError::DuplicateNodeId { .. }));
    }

    #[test]
    fn malformed_source_is_reported_not_panicked() {
        let mut manifest = two_node_manifest();
        manifest.nodes[1].inputs.insert(
            "bad".to_string(),
            astrs_manifest::Input::from_source("not-a-valid-source"),
        );
        let err = DataflowGraph::from_manifest(&manifest).unwrap_err();
        assert!(matches!(err, GraphBuildError::MalformedSource { .. }));
    }

    #[test]
    fn record_sugar_expands_to_synthesized_ports() {
        let mut manifest = two_node_manifest();
        let mut recorder = Node::with_path("recorder", "dummy");
        recorder.path = None;
        recorder.record = Some(vec![
            "camera/frames".to_string(),
            "detector/detections".to_string(),
        ]);
        manifest.nodes[1].outputs.push("detections".to_string());
        manifest.nodes.push(recorder);

        let (graph, diagnostics) = DataflowGraph::from_manifest(&manifest).unwrap();
        assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
        let recorder_node = graph.node(&NodeId::new("recorder")).unwrap();
        assert_eq!(recorder_node.inputs.len(), 2);
        assert!(
            recorder_node
                .inputs
                .contains_key(&PortName::new("_record/0"))
        );
        assert!(
            recorder_node
                .inputs
                .contains_key(&PortName::new("_record/1"))
        );

        let edge0 = graph
            .edge(&EdgeKey::new(
                NodeId::new("recorder"),
                PortName::new("_record/0"),
            ))
            .unwrap();
        assert_eq!(
            edge0.from,
            EdgeSource::NodeOutput {
                node: NodeId::new("camera"),
                output: PortName::new("frames"),
            }
        );
    }

    #[test]
    fn record_sugar_collision_with_explicit_input_is_flagged_and_explicit_wins() {
        let mut manifest = two_node_manifest();
        let mut recorder = Node::with_path("recorder", "dummy");
        recorder.path = None;
        recorder.record = Some(vec!["camera/frames".to_string()]);
        recorder.inputs.insert(
            "_record/0".to_string(),
            astrs_manifest::Input::from_source("detector/detections"),
        );
        manifest.nodes[1].outputs.push("detections".to_string());
        manifest.nodes.push(recorder);

        let (graph, diagnostics) = DataflowGraph::from_manifest(&manifest).unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert!(matches!(
            diagnostics[0].kind,
            DiagnosticKind::RecordSugarPortCollision { .. }
        ));
        let edge0 = graph
            .edge(&EdgeKey::new(
                NodeId::new("recorder"),
                PortName::new("_record/0"),
            ))
            .unwrap();
        // The explicit declaration (detector/detections) wins over the
        // record-sugar entry (camera/frames).
        assert_eq!(
            edge0.from,
            EdgeSource::NodeOutput {
                node: NodeId::new("detector"),
                output: PortName::new("detections"),
            }
        );
    }

    #[test]
    fn orphaned_output_type_is_flagged() {
        let mut manifest = two_node_manifest();
        manifest.nodes[0].output_types.insert(
            "nonexistent".to_string(),
            astrs_manifest::Urn::new("std/core/v1/Int8"),
        );
        let (_graph, diagnostics) = DataflowGraph::from_manifest(&manifest).unwrap();
        assert!(
            diagnostics
                .iter()
                .any(|d| matches!(d.kind, DiagnosticKind::OrphanedOutputType { .. }))
        );
    }

    #[test]
    fn orphaned_input_type_is_flagged() {
        let mut manifest = two_node_manifest();
        manifest.nodes[1].input_types.insert(
            "nonexistent".to_string(),
            astrs_manifest::Urn::new("std/core/v1/Int8"),
        );
        let (_graph, diagnostics) = DataflowGraph::from_manifest(&manifest).unwrap();
        assert!(
            diagnostics
                .iter()
                .any(|d| matches!(d.kind, DiagnosticKind::OrphanedInputType { .. }))
        );
    }

    #[test]
    fn unconsumed_output_is_reported_by_diagnostics() {
        let manifest = two_node_manifest();
        // camera/frames IS consumed by detector; add a second, unconsumed
        // output.
        let mut manifest = manifest;
        manifest.nodes[0].outputs.push("debug_overlay".to_string());
        let (graph, _) = DataflowGraph::from_manifest(&manifest).unwrap();
        let diagnostics = graph.diagnostics();
        assert!(diagnostics.iter().any(|d| matches!(
            &d.kind,
            DiagnosticKind::UnconsumedOutput { output, .. } if output.as_str() == "debug_overlay"
        )));
    }

    #[test]
    fn virtual_source_input_builds_an_edge_with_no_producer() {
        let mut manifest = two_node_manifest();
        manifest.nodes[1].inputs.insert(
            "tick".to_string(),
            astrs_manifest::Input::from_source("astrs/timer/hz/50"),
        );
        let (graph, diagnostics) = DataflowGraph::from_manifest(&manifest).unwrap();
        assert!(diagnostics.is_empty());
        let edge = graph
            .edge(&EdgeKey::new(
                NodeId::new("detector"),
                PortName::new("tick"),
            ))
            .unwrap();
        assert!(edge.from.is_virtual());
    }

    #[test]
    fn dynamic_path_node_does_not_spawn() {
        let mut manifest = two_node_manifest();
        manifest.nodes.push(Node::with_path(
            "external",
            astrs_manifest::DYNAMIC_PATH_SENTINEL,
        ));
        let (graph, _) = DataflowGraph::from_manifest(&manifest).unwrap();
        let external = graph.node(&NodeId::new("external")).unwrap();
        assert!(!external.spawns_process);
        let camera = graph.node(&NodeId::new("camera")).unwrap();
        assert!(camera.spawns_process);
    }

    #[test]
    fn node_deploy_overrides_graph_deploy_in_resolved_machine() {
        let mut manifest = two_node_manifest();
        manifest.deploy = Some(Deploy {
            machine: Some("default-machine".to_string()),
            ..Deploy::default()
        });
        manifest.nodes[1].deploy = Some(Deploy {
            machine: Some("robot-1".to_string()),
            ..Deploy::default()
        });
        let (graph, _) = DataflowGraph::from_manifest(&manifest).unwrap();
        assert_eq!(
            graph.node(&NodeId::new("camera")).unwrap().machine,
            MachineId::Named("default-machine".to_string())
        );
        assert_eq!(
            graph.node(&NodeId::new("detector")).unwrap().machine,
            MachineId::Named("robot-1".to_string())
        );
    }

    #[test]
    fn edges_into_and_edges_from_filter_correctly() {
        let manifest = two_node_manifest();
        let (graph, _) = DataflowGraph::from_manifest(&manifest).unwrap();
        let camera_id = NodeId::new("camera");
        let detector_id = NodeId::new("detector");

        assert_eq!(graph.edges_from(&camera_id).count(), 1);
        assert_eq!(graph.edges_into(&camera_id).count(), 0);
        assert_eq!(graph.edges_into(&detector_id).count(), 1);
        assert_eq!(graph.edges_from(&detector_id).count(), 0);
    }

    #[test]
    fn edge_type_status_reports_compatible_for_matching_types() {
        let mut manifest = two_node_manifest();
        manifest.nodes[1].input_types.insert(
            "frames".to_string(),
            astrs_manifest::Urn::new("std/media/v1/Image"),
        );
        let (graph, _) = DataflowGraph::from_manifest(&manifest).unwrap();
        let key = EdgeKey::new(NodeId::new("detector"), PortName::new("frames"));
        assert_eq!(
            graph.edge_type_status(&key),
            Some(EdgeTypeStatus::Compatible)
        );
    }

    #[test]
    fn edge_type_status_is_none_for_unknown_edge() {
        let manifest = two_node_manifest();
        let (graph, _) = DataflowGraph::from_manifest(&manifest).unwrap();
        let key = EdgeKey::new(NodeId::new("nope"), PortName::new("nope"));
        assert_eq!(graph.edge_type_status(&key), None);
    }

    /// End-to-end integration check: a manifest-declared service pattern,
    /// wired in both directions, produces no pattern or cycle diagnostics
    /// once it goes through the full `from_manifest` -> `diagnostics`
    /// pipeline — [`crate::pattern`] and [`crate::scc`] are unit-tested
    /// directly against hand-built maps, but this is the one place that
    /// proves `astrs_manifest::Pattern` on a real [`Node`] actually reaches
    /// them unchanged.
    #[test]
    fn fully_wired_service_pattern_produces_no_diagnostics_end_to_end() {
        let mut caller = Node::with_path("caller", "./caller");
        caller.pattern = Some(astrs_manifest::Pattern::ServiceClient);
        caller.outputs = vec!["request".to_string()];

        let mut adder = Node::with_path("adder", "./adder");
        adder.pattern = Some(astrs_manifest::Pattern::ServiceServer);
        adder.outputs = vec!["response".to_string()];
        adder.inputs.insert(
            "request".to_string(),
            astrs_manifest::Input::from_source("caller/request"),
        );
        caller.inputs.insert(
            "response".to_string(),
            astrs_manifest::Input::from_source("adder/response"),
        );

        let manifest = Manifest {
            nodes: vec![caller, adder],
            ..Manifest::default()
        };
        let (graph, construction_diagnostics) = DataflowGraph::from_manifest(&manifest).unwrap();
        assert!(construction_diagnostics.is_empty());

        // The two edges form a 2-node cycle at the node-topology level
        // (caller -> adder -> caller); `diagnostics()` must recognize it
        // as a fully-covered pattern correlation and stay silent about it.
        let diagnostics = graph.diagnostics();
        assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

        let pairs = graph.pattern_pairs();
        assert_eq!(pairs.len(), 1);
        assert!(pairs[0].is_fully_wired());

        let cyclic: Vec<_> = graph
            .sccs()
            .into_iter()
            .filter(|s| s.nodes.len() > 1)
            .collect();
        assert_eq!(cyclic.len(), 1);
    }

    /// The same shape, but missing the response edge: the pattern is only
    /// partially wired, so `diagnostics()` must report it (there is no
    /// cycle to also worry about here, since one direction is unwired).
    #[test]
    fn partially_wired_service_pattern_is_flagged_end_to_end() {
        let mut caller = Node::with_path("caller", "./caller");
        caller.pattern = Some(astrs_manifest::Pattern::ServiceClient);
        caller.outputs = vec!["request".to_string()];

        let mut adder = Node::with_path("adder", "./adder");
        adder.pattern = Some(astrs_manifest::Pattern::ServiceServer);
        adder.inputs.insert(
            "request".to_string(),
            astrs_manifest::Input::from_source("caller/request"),
        );

        let manifest = Manifest {
            nodes: vec![caller, adder],
            ..Manifest::default()
        };
        let (graph, _) = DataflowGraph::from_manifest(&manifest).unwrap();

        let diagnostics = graph.diagnostics();
        assert!(
            diagnostics
                .iter()
                .any(|d| matches!(d.kind, DiagnosticKind::PartialPatternPair { .. }))
        );
    }

    /// A plain (unpatterned) 2-node cycle is flagged as a warning-level
    /// [`DiagnosticKind::Cycle`] end to end — the counterpart to the
    /// pattern-covered cycle above, proving the two paths (legal vs.
    /// flagged) both reach `diagnostics()` correctly from real manifest
    /// input.
    #[test]
    fn plain_data_cycle_is_flagged_end_to_end() {
        let mut a = Node::with_path("a", "./a");
        a.outputs = vec!["out".to_string()];
        let mut b = Node::with_path("b", "./b");
        b.outputs = vec!["out".to_string()];
        a.inputs.insert(
            "in".to_string(),
            astrs_manifest::Input::from_source("b/out"),
        );
        b.inputs.insert(
            "in".to_string(),
            astrs_manifest::Input::from_source("a/out"),
        );

        let manifest = Manifest {
            nodes: vec![a, b],
            ..Manifest::default()
        };
        let (graph, _) = DataflowGraph::from_manifest(&manifest).unwrap();

        let diagnostics = graph.diagnostics();
        assert!(
            diagnostics
                .iter()
                .any(|d| matches!(d.kind, DiagnosticKind::Cycle { .. })
                    && d.severity == Severity::Warning)
        );
    }
}
