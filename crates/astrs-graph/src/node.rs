//! Graph-level node representation.
//!
//! [`GraphNode`] reduces a [`Node`](astrs_manifest::Node) to the shape the
//! dataflow graph cares about: its declared ports (names and types),
//! service/action pattern, and resolved placement. It deliberately does
//! **not** carry wiring — an input's `source` and queue configuration live
//! on [`crate::Edge`], keyed by `(this node, this input)`
//! ([`crate::EdgeKey`]) — because the manifest itself declares a port's
//! *existence and type* (`inputs`/`outputs`, `input_types`/`output_types`)
//! independently of how it is currently wired, and [`crate::diff::diff`]
//! relies on that split to tell "the node's interface changed"
//! ([`crate::diff::TopologyOp::ReplaceNode`]) apart from "the wiring
//! changed" ([`crate::diff::TopologyOp::AddEdge`] /
//! [`crate::diff::TopologyOp::RemoveEdge`]).

use std::collections::BTreeMap;

use astrs_manifest::{Pattern, Urn};
use serde::{Deserialize, Serialize};

use crate::ids::{MachineId, NodeId, PortName};

/// One declared output port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputPort {
    /// This output's declared type URN, if the manifest's `output_types`
    /// gave it one. `None` means untyped — an explicit,
    /// silently-compatible-with-anything port (blueprint §3.7's "Raw/
    /// untyped ports remain available").
    pub type_urn: Option<Urn>,
}

/// One declared input port's *declared type*.
///
/// Deliberately narrower than [`OutputPort`] would suggest by symmetry: an
/// input's queueing behavior lives on [`crate::Edge`] instead (see this
/// module's top-level docs), so this struct exists mainly so
/// [`GraphNode::inputs`] and [`GraphNode::outputs`] share a uniform
/// `BTreeMap<PortName, _>` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputPort {
    /// This input's declared type URN, if the manifest's `input_types`
    /// gave it one. `None` means untyped (see [`OutputPort::type_urn`]).
    pub type_urn: Option<Urn>,
}

/// A dataflow node reduced to its graph-relevant shape: ports, pattern,
/// and placement. See this module's top-level docs for what is
/// deliberately excluded (wiring) and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNode {
    /// This node's id.
    pub id: NodeId,
    /// This node's declared output ports, keyed by name.
    pub outputs: BTreeMap<PortName, OutputPort>,
    /// This node's declared input ports, keyed by name. Every key here has
    /// exactly one corresponding [`crate::Edge`] in the owning
    /// [`crate::DataflowGraph`] — the manifest's `Input` struct requires a
    /// `source`, so a declared input is never edge-less.
    pub inputs: BTreeMap<PortName, InputPort>,
    /// The service/action wiring pattern this node participates in, if
    /// any (blueprint §9.4). See [`crate::PatternPair`] for how pattern-typed
    /// nodes are paired into request/response correlations.
    pub pattern: Option<Pattern>,
    /// This node's resolved placement (blueprint §5.2's placement
    /// planner), merged from the manifest-wide default and this node's
    /// own `deploy:` override — see [`MachineId::resolve`].
    pub machine: MachineId,
    /// Whether the daemon spawns an OS process for this node. `false` for
    /// a `path: dynamic` node (blueprint §8.3's "external attach" —
    /// [`Node::is_dynamic_path`](astrs_manifest::Node::is_dynamic_path)):
    /// there is no process for [`crate::placement::plan_placement`] to
    /// list in a machine's spawn list.
    pub spawns_process: bool,
}

impl GraphNode {
    /// This node's declared output type, if `name` is a declared output
    /// with a type in `output_types`.
    #[must_use]
    pub fn output_type(&self, name: &PortName) -> Option<&Urn> {
        self.outputs.get(name).and_then(|p| p.type_urn.as_ref())
    }

    /// This node's declared input type, if `name` is a declared input
    /// with a type in `input_types`.
    #[must_use]
    pub fn input_type(&self, name: &PortName) -> Option<&Urn> {
        self.inputs.get(name).and_then(|p| p.type_urn.as_ref())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn sample_node() -> GraphNode {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            PortName::new("frames"),
            OutputPort {
                type_urn: Some(Urn::new("std/media/v1/Image[pixel=rgb8]")),
            },
        );
        let mut inputs = BTreeMap::new();
        inputs.insert(PortName::new("tick"), InputPort { type_urn: None });
        GraphNode {
            id: NodeId::new("camera"),
            outputs,
            inputs,
            pattern: None,
            machine: MachineId::CoordinatorLocal,
            spawns_process: true,
        }
    }

    #[test]
    fn output_type_returns_declared_type() {
        let node = sample_node();
        assert_eq!(
            node.output_type(&PortName::new("frames")).map(Urn::as_str),
            Some("std/media/v1/Image[pixel=rgb8]")
        );
    }

    #[test]
    fn output_type_is_none_for_unknown_port() {
        let node = sample_node();
        assert_eq!(node.output_type(&PortName::new("missing")), None);
    }

    #[test]
    fn input_type_is_none_when_untyped() {
        let node = sample_node();
        assert_eq!(node.input_type(&PortName::new("tick")), None);
    }

    #[test]
    fn graph_node_equality_is_structural() {
        assert_eq!(sample_node(), sample_node());
        let mut other = sample_node();
        other.pattern = Some(Pattern::ServiceServer);
        assert_ne!(sample_node(), other);
    }
}
