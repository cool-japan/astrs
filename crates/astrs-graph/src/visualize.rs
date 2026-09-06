//! Mermaid and Graphviz DOT visualization for `astrs graph` (blueprint
//! §5.2, §17: `astrs graph` — "mermaid/DOT/HTML").
//!
//! Both emitters walk the same [`Layout`]: nodes grouped into machine
//! clusters, virtual sources rendered as their own distinctly styled
//! nodes, and edges labeled with the producer's output name plus its
//! declared type URN when one exists. Both are fully deterministic —
//! every collection this module iterates is either a [`std::collections::BTreeMap`]
//! already (via [`DataflowGraph::nodes`]/[`DataflowGraph::edges`]) or is
//! built into one before iteration — so calling either emitter twice on
//! the same graph byte-for-byte repeats, which is what makes golden-file
//! comparison ([`crate` tests under `tests/golden/`]) meaningful at all.
//!
//! # Node identifiers vs. labels
//!
//! Every node/machine/virtual-source gets a synthetic identifier (`n0`,
//! `n1`, ...; `m0`, `m1`, ...; `v0`, `v1`, ...) assigned by sorted
//! iteration order, used as the token both formats key edges and
//! subgraphs off of; the manifest's own id/name is emitted only as a
//! *label*. Manifest-declared identifiers are user text (a
//! [`crate::PortName`] is not charset-restricted at all — see that type's
//! docs) and are never assumed safe to use as a bare mermaid/DOT token.
//! This also sidesteps a real hazard a naive "sanitize the id into a
//! token" approach would have: two distinct, valid node ids that differ
//! only in characters a sanitizer would collapse (e.g. `a.b` and `a-b`,
//! both legal under the manifest's node-id charset) must never collide
//! into the same emitted token.
//!
//! # Virtual sources
//!
//! A virtual-source edge (blueprint §8.4) has no producer node, so it is
//! rendered as its own node — one per *distinct* source string, even if
//! several consumers reference the same `astrs/timer/hz/50` (each daemon
//! materializes it locally per blueprint §11.1, but the diagram shows one
//! shared visual source for readability). It is deliberately **not**
//! placed inside any machine cluster: unlike an ordinary node, a virtual
//! source is not "hosted" on one particular machine — every daemon that
//! has a consumer for it runs its own local timer wheel / log tap.
//! Virtual-source edges are left unlabeled: the source node's own label
//! already names the source in full, and there is no producer output
//! name to show.

use std::collections::{BTreeMap, BTreeSet};

use crate::edge::EdgeSource;
use crate::graph::DataflowGraph;
use crate::ids::{MachineId, NodeId};

/// Escapes `text` for safe embedding inside a mermaid quoted label
/// (`id["<text>"]`) or a pipe-delimited edge label (`-->|"<text>"|`).
///
/// This is not a complete implementation of mermaid's label grammar —
/// that grammar is not itself fully, formally specified. It guarantees
/// the narrower property these two emitters actually need: none of the
/// three characters that would otherwise break the surrounding
/// delimiters (`"`, `|`, a raw newline) reach the output unescaped, so
/// *any* input string always produces syntactically parseable mermaid,
/// even if an exotic label does not render exactly as written.
fn escape_mermaid(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("&quot;"),
            '|' => out.push('/'),
            '\n' | '\r' => out.push(' '),
            other => out.push(other),
        }
    }
    out
}

/// Escapes `text` for safe embedding inside a Graphviz DOT quoted string,
/// per the DOT language spec: backslash and double-quote are the only
/// characters a quoted string must escape. An embedded newline is
/// rewritten to the two-character `\n` escape DOT itself interprets as a
/// label line break, rather than a raw newline splitting the quoted
/// string across source lines.
fn escape_dot(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' | '\r' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// The producer-side edge label — blueprint §5.2's "output name + URN
/// when present" — or `None` for a virtual-source edge (see this
/// module's top-level docs for why those stay unlabeled).
fn edge_label(graph: &DataflowGraph, source: &EdgeSource) -> Option<String> {
    let EdgeSource::NodeOutput { node, output } = source else {
        return None;
    };
    let mut label = output.as_str().to_string();
    if let Some(urn) = graph.node(node).and_then(|n| n.output_type(output)) {
        label.push_str(": ");
        label.push_str(urn.as_str());
    }
    Some(label)
}

/// Deterministic id assignment shared by both emitters — see this
/// module's top-level docs for why identifiers are synthetic rather than
/// sanitized manifest text.
struct Layout<'g> {
    graph: &'g DataflowGraph,
    machine_index: BTreeMap<MachineId, usize>,
    node_index: BTreeMap<&'g NodeId, usize>,
    /// Distinct virtual-source strings, in sorted order, each with its
    /// assigned index.
    virtual_index: BTreeMap<&'g str, usize>,
}

impl<'g> Layout<'g> {
    fn build(graph: &'g DataflowGraph) -> Self {
        let machines: BTreeSet<MachineId> = graph
            .nodes
            .values()
            .map(|node| node.machine.clone())
            .collect();
        let machine_index = machines
            .into_iter()
            .enumerate()
            .map(|(i, m)| (m, i))
            .collect();

        let node_index = graph
            .nodes
            .keys()
            .enumerate()
            .map(|(i, id)| (id, i))
            .collect();

        let virtual_sources: BTreeSet<&str> = graph
            .edges
            .values()
            .filter_map(|edge| match &edge.from {
                EdgeSource::Virtual(source) => Some(source.as_str()),
                EdgeSource::NodeOutput { .. } => None,
            })
            .collect();
        let virtual_index = virtual_sources
            .into_iter()
            .enumerate()
            .map(|(i, s)| (s, i))
            .collect();

        Self {
            graph,
            machine_index,
            node_index,
            virtual_index,
        }
    }

    /// Every real node id, grouped by machine, in `(machine, node)` sorted
    /// order — the shape both emitters walk to build machine clusters.
    fn nodes_by_machine(&self) -> BTreeMap<MachineId, Vec<&'g NodeId>> {
        let mut grouped: BTreeMap<MachineId, Vec<&NodeId>> = BTreeMap::new();
        for node in self.graph.nodes.values() {
            grouped
                .entry(node.machine.clone())
                .or_default()
                .push(&node.id);
        }
        grouped
    }

    /// The source token (`n<i>` for a resolvable node output, `v<i>` for a
    /// virtual source) for one edge's producer side, or `None` if the
    /// producer cannot be resolved in this graph (a dangling reference —
    /// see [`crate::placement::plan_placement`]'s docs for why this
    /// function tolerates that instead of assuming it can't happen).
    fn source_token(&self, source: &EdgeSource) -> Option<String> {
        match source {
            EdgeSource::NodeOutput { node, .. } => {
                self.node_index.get(node).map(|i| format!("n{i}"))
            }
            EdgeSource::Virtual(text) => self
                .virtual_index
                .get(text.as_str())
                .map(|i| format!("v{i}")),
        }
    }

    /// The target token (`n<i>`) for one edge's consumer, or `None` if the
    /// consumer id is not in this graph (only possible on a hand-built
    /// graph that skipped [`DataflowGraph::from_manifest`]'s invariants).
    fn target_token(&self, consumer: &NodeId) -> Option<String> {
        self.node_index.get(consumer).map(|i| format!("n{i}"))
    }
}

/// Render `graph` as a mermaid `flowchart` — blueprint §17's `astrs graph`
/// mermaid output.
///
/// ```
/// use astrs_graph::DataflowGraph;
/// use astrs_manifest::Manifest;
///
/// let yaml = "\
/// nodes:
///   - id: camera
///     path: ./camera
///     outputs: [frames]
///   - id: detector
///     path: ./detector
///     inputs:
///       frames: camera/frames
/// ";
/// let manifest = Manifest::from_yaml_str(yaml)?;
/// let (graph, _) = DataflowGraph::from_manifest(&manifest)?;
/// let mermaid = astrs_graph::to_mermaid(&graph);
/// assert!(mermaid.starts_with("flowchart LR"));
/// assert!(mermaid.contains("camera"));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[must_use]
pub fn to_mermaid(graph: &DataflowGraph) -> String {
    let layout = Layout::build(graph);
    let mut lines = Vec::new();

    lines.push("flowchart LR".to_string());
    lines.push("    classDef virtual fill:#eee,stroke:#999,stroke-dasharray: 3 3;".to_string());

    for (machine, node_ids) in layout.nodes_by_machine() {
        let Some(&mi) = layout.machine_index.get(&machine) else {
            continue;
        };
        lines.push(format!(
            "    subgraph m{mi}[\"{}\"]",
            escape_mermaid(&machine.to_string())
        ));
        for id in node_ids {
            let Some(&ni) = layout.node_index.get(id) else {
                continue;
            };
            lines.push(format!(
                "        n{ni}[\"{}\"]",
                escape_mermaid(id.as_str())
            ));
        }
        lines.push("    end".to_string());
    }

    for (source, &vi) in &layout.virtual_index {
        lines.push(format!(
            "    v{vi}([\"{}\"]):::virtual",
            escape_mermaid(source)
        ));
    }

    for (key, edge) in &graph.edges {
        let (Some(source_token), Some(target_token)) = (
            layout.source_token(&edge.from),
            layout.target_token(&key.consumer),
        ) else {
            continue;
        };
        match edge_label(graph, &edge.from) {
            Some(label) => lines.push(format!(
                "    {source_token} -->|\"{}\"| {target_token}",
                escape_mermaid(&label)
            )),
            None => lines.push(format!("    {source_token} --> {target_token}")),
        }
    }

    lines.join("\n") + "\n"
}

/// Render `graph` as a Graphviz DOT `digraph` — blueprint §17's `astrs
/// graph` DOT output.
///
/// ```
/// use astrs_graph::DataflowGraph;
/// use astrs_manifest::Manifest;
///
/// let yaml = "\
/// nodes:
///   - id: camera
///     path: ./camera
///     outputs: [frames]
///   - id: detector
///     path: ./detector
///     inputs:
///       frames: camera/frames
/// ";
/// let manifest = Manifest::from_yaml_str(yaml)?;
/// let (graph, _) = DataflowGraph::from_manifest(&manifest)?;
/// let dot = astrs_graph::to_dot(&graph);
/// assert!(dot.starts_with("digraph astrs {"));
/// assert!(dot.contains("camera"));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[must_use]
pub fn to_dot(graph: &DataflowGraph) -> String {
    let layout = Layout::build(graph);
    let mut lines = Vec::new();

    lines.push("digraph astrs {".to_string());
    lines.push("    rankdir=LR;".to_string());
    lines.push("    node [shape=box];".to_string());

    for (machine, node_ids) in layout.nodes_by_machine() {
        let Some(&mi) = layout.machine_index.get(&machine) else {
            continue;
        };
        lines.push(format!("    subgraph cluster_m{mi} {{"));
        lines.push(format!(
            "        label=\"{}\";",
            escape_dot(&machine.to_string())
        ));
        for id in node_ids {
            let Some(&ni) = layout.node_index.get(id) else {
                continue;
            };
            lines.push(format!(
                "        \"n{ni}\" [label=\"{}\"];",
                escape_dot(id.as_str())
            ));
        }
        lines.push("    }".to_string());
    }

    for (source, &vi) in &layout.virtual_index {
        lines.push(format!(
            "    \"v{vi}\" [label=\"{}\", shape=ellipse, style=dashed];",
            escape_dot(source)
        ));
    }

    for (key, edge) in &graph.edges {
        let (Some(source_token), Some(target_token)) = (
            layout.source_token(&edge.from),
            layout.target_token(&key.consumer),
        ) else {
            continue;
        };
        match edge_label(graph, &edge.from) {
            Some(label) => lines.push(format!(
                "    \"{source_token}\" -> \"{target_token}\" [label=\"{}\"];",
                escape_dot(&label)
            )),
            None => lines.push(format!("    \"{source_token}\" -> \"{target_token}\";")),
        }
    }

    lines.push("}".to_string());
    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use astrs_manifest::{Deploy, Input, Manifest, Node, Urn};

    use super::*;
    use crate::graph::DataflowGraph;

    fn two_machine_graph() -> DataflowGraph {
        let mut camera = Node::with_path("camera", "./camera");
        camera.outputs = vec!["frames".to_string()];
        camera.output_types.insert(
            "frames".to_string(),
            Urn::new("std/media/v1/Image[pixel=rgb8]"),
        );

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

        let manifest = Manifest {
            nodes: vec![camera, detector, planner],
            ..Manifest::default()
        };
        DataflowGraph::from_manifest(&manifest).unwrap().0
    }

    #[test]
    fn mermaid_output_is_deterministic() {
        let graph = two_machine_graph();
        assert_eq!(to_mermaid(&graph), to_mermaid(&graph));
    }

    #[test]
    fn dot_output_is_deterministic() {
        let graph = two_machine_graph();
        assert_eq!(to_dot(&graph), to_dot(&graph));
    }

    #[test]
    fn mermaid_includes_machine_clusters() {
        let mermaid = to_mermaid(&two_machine_graph());
        assert!(mermaid.contains("subgraph m0[\"coordinator\"]"));
        assert!(mermaid.contains("subgraph m1[\"robot-1\"]"));
    }

    #[test]
    fn dot_includes_machine_clusters() {
        let dot = to_dot(&two_machine_graph());
        assert!(dot.contains("subgraph cluster_m0"));
        assert!(dot.contains("label=\"coordinator\""));
        assert!(dot.contains("subgraph cluster_m1"));
        assert!(dot.contains("label=\"robot-1\""));
    }

    #[test]
    fn mermaid_styles_virtual_sources_distinctly() {
        let mermaid = to_mermaid(&two_machine_graph());
        assert!(mermaid.contains("v0([\"astrs/timer/hz/50\"]):::virtual"));
        assert!(mermaid.contains("classDef virtual"));
    }

    #[test]
    fn dot_styles_virtual_sources_distinctly() {
        let dot = to_dot(&two_machine_graph());
        assert!(dot.contains("\"v0\" [label=\"astrs/timer/hz/50\", shape=ellipse, style=dashed];"));
    }

    #[test]
    fn mermaid_edge_label_includes_output_name_and_urn() {
        let mermaid = to_mermaid(&two_machine_graph());
        assert!(mermaid.contains("|\"frames: std/media/v1/Image[pixel=rgb8]\"|"));
        // detector's `detections` output has no declared type: name only.
        assert!(mermaid.contains("|\"detections\"|"));
    }

    #[test]
    fn dot_edge_label_includes_output_name_and_urn() {
        let dot = to_dot(&two_machine_graph());
        assert!(dot.contains("[label=\"frames: std/media/v1/Image[pixel=rgb8]\"];"));
        assert!(dot.contains("[label=\"detections\"];"));
    }

    #[test]
    fn virtual_source_edges_are_unlabeled() {
        let mermaid = to_mermaid(&two_machine_graph());
        assert!(mermaid.contains("v0 --> n2"));
        let dot = to_dot(&two_machine_graph());
        assert!(dot.contains("\"v0\" -> \"n2\";"));
    }

    #[test]
    fn escape_mermaid_neutralizes_delimiter_characters() {
        let escaped = escape_mermaid("a\"b|c\nd");
        assert!(!escaped.contains('"'));
        assert!(!escaped.contains('|'));
        assert!(!escaped.contains('\n'));
    }

    #[test]
    fn escape_dot_follows_the_dot_spec() {
        assert_eq!(escape_dot("a\"b"), "a\\\"b");
        assert_eq!(escape_dot("a\\b"), "a\\\\b");
        assert_eq!(escape_dot("a\nb"), "a\\nb");
    }

    #[test]
    fn distinct_node_ids_that_sanitize_the_same_do_not_collide() {
        // `a.b` and `a-b` are both legal manifest node ids and would
        // collapse to the same token under a naive "replace non-word
        // chars with _" sanitizer. The index-based scheme must not.
        //
        // `NodeId`'s `Ord` is the plain byte-string order, so `"a-b"`
        // (`-` = 0x2D) sorts before `"a.b"` (`.` = 0x2E) regardless of
        // which is pushed into `manifest.nodes` first: `graph.nodes` is a
        // `BTreeMap`, and `node_index` follows its key order, not
        // insertion order. That makes `a-b` -> n0 and `a.b` -> n1 here.
        let mut a = Node::with_path("a.b", "./a");
        a.outputs = vec!["out".to_string()];
        let mut b = Node::with_path("a-b", "./b");
        b.inputs
            .insert("in".to_string(), Input::from_source("a.b/out"));
        let manifest = Manifest {
            nodes: vec![a, b],
            ..Manifest::default()
        };
        let (graph, _) = DataflowGraph::from_manifest(&manifest).unwrap();

        let mermaid = to_mermaid(&graph);
        assert!(mermaid.contains("n1[\"a.b\"]"));
        assert!(mermaid.contains("n0[\"a-b\"]"));
        assert!(mermaid.contains("n1 -->|\"out\"| n0"));
    }

    #[test]
    fn single_node_graph_renders_without_edges() {
        let manifest = Manifest {
            nodes: vec![Node::with_path("solo", "./solo")],
            ..Manifest::default()
        };
        let (graph, _) = DataflowGraph::from_manifest(&manifest).unwrap();
        let mermaid = to_mermaid(&graph);
        assert!(mermaid.contains("n0[\"solo\"]"));
        let dot = to_dot(&graph);
        assert!(dot.contains("\"n0\" [label=\"solo\"];"));
    }
}
