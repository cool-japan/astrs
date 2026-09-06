//! The unified diagnostics list [`crate::DataflowGraph::diagnostics`]
//! produces — what `astrs validate` prints (blueprint §17).
//!
//! Every structural or type-level observation this crate can make about a
//! graph — a type mismatch, an illegal cycle, a half-wired service
//! pattern, an output nobody consumes — is one [`Diagnostic`] with a
//! [`Severity`]. Keeping every check's output in one typed, ordered list
//! (rather than five differently-shaped `Vec`s from five functions) is
//! what lets a single CLI command print one coherent report.

use std::fmt;

use astrs_manifest::{Pattern, Urn};
use serde::{Deserialize, Serialize};

use crate::edge::EdgeKey;
use crate::ids::{NodeId, PortName};
use crate::pattern::PatternKind;

/// How serious a [`Diagnostic`] is.
///
/// Ordered `Info < Warning < Error` (derived `Ord` follows declaration
/// order) so a caller can filter with `>=` — e.g. `astrs validate`
/// (without `--prove`) fails the process on any `Error`, and `--strict`
/// callers may choose to also fail on `Warning`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Worth surfacing, never blocks anything (e.g. an unconsumed output).
    Info,
    /// A likely mistake that does not (by itself) make the graph unsafe to
    /// run (e.g. a non-`strict_types` type mismatch, a legality-ambiguous
    /// cycle).
    Warning,
    /// The graph is unsafe or self-contradictory as declared (e.g. a
    /// `strict_types` type mismatch).
    Error,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        })
    }
}

/// The specific observation a [`Diagnostic`] reports.
///
/// `#[non_exhaustive]`: new checks are added as new variants at the tail,
/// per this workspace's append-only-enum convention (blueprint §3.4) —
/// existing variants never change shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DiagnosticKind {
    /// An edge's producer and consumer declared incompatible type URNs,
    /// with no `type_rules` entry allowing the coercion (blueprint §8.2).
    TypeMismatch {
        /// The mismatched edge.
        edge: EdgeKey,
        /// The consumer's declared input type.
        expected: Urn,
        /// The producer's declared output type.
        found: Urn,
    },
    /// A directed cycle exists among plain data edges with no
    /// service/action correlation covering it (blueprint §5.2: "cycles
    /// are legal for service patterns but flagged for plain data edges as
    /// warnings").
    Cycle {
        /// Every node in the strongly connected component, in
        /// [`NodeId`] order.
        nodes: Vec<NodeId>,
        /// The specific plain-data edges within the cycle that are not
        /// part of any pattern correlation — the edges to look at first
        /// when breaking the cycle.
        uncorrelated_edges: Vec<EdgeKey>,
    },
    /// A node declares a service/action `pattern:` but is not wired
    /// (in either direction) to any counterpart of the matching kind.
    UnpairedPatternNode {
        /// The orphaned node.
        node: NodeId,
        /// Its declared pattern.
        pattern: Pattern,
    },
    /// A client/server pair of the same pattern kind is wired in only one
    /// direction — request without a response path, or vice versa.
    PartialPatternPair {
        /// The client-role node.
        client: NodeId,
        /// The server-role node.
        server: NodeId,
        /// Whether this is a service or action pair.
        kind: PatternKind,
        /// Whether at least one client→server edge exists.
        has_request_edge: bool,
        /// Whether at least one server→client edge exists.
        has_response_edge: bool,
    },
    /// A declared output has no consumer anywhere in the graph.
    UnconsumedOutput {
        /// The node declaring the output.
        node: NodeId,
        /// The unconsumed output's name.
        output: PortName,
    },
    /// `output_types` names a port that is not in this node's declared
    /// `outputs` list, so it can never be referenced (astrs-manifest's
    /// `validate` checks URN syntax but not this cross-reference).
    OrphanedOutputType {
        /// The node declaring the stray entry.
        node: NodeId,
        /// The output name that has a type but no matching declared output.
        output: PortName,
    },
    /// `input_types` names a port that is not in this node's declared
    /// `inputs` map, so it documents a type for an input that does not
    /// exist.
    OrphanedInputType {
        /// The node declaring the stray entry.
        node: NodeId,
        /// The input name that has a type but no matching declared input.
        input: PortName,
    },
    /// A `record:` sugar entry (blueprint §14) synthesized a port name
    /// that collides with one of the node's own explicitly declared
    /// `inputs` — the explicit declaration wins and the sugar entry is
    /// dropped (see [`crate::graph::DataflowGraph::from_manifest`]).
    RecordSugarPortCollision {
        /// The recorder node.
        node: NodeId,
        /// The colliding port name.
        port: PortName,
    },
}

/// One diagnostic observation about a [`crate::DataflowGraph`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// How serious this observation is.
    pub severity: Severity,
    /// What was observed.
    pub kind: DiagnosticKind,
}

impl Diagnostic {
    /// Build a diagnostic from a severity and kind.
    #[must_use]
    pub fn new(severity: Severity, kind: DiagnosticKind) -> Self {
        Self { severity, kind }
    }

    /// The edge this diagnostic is scoped to, if any.
    ///
    /// Only [`DiagnosticKind::TypeMismatch`] is inherently edge-scoped;
    /// every other kind returns `None` (they are node- or pair-scoped —
    /// see their own fields for the specific ids involved).
    #[must_use]
    pub fn edge(&self) -> Option<&EdgeKey> {
        match &self.kind {
            DiagnosticKind::TypeMismatch { edge, .. } => Some(edge),
            _ => None,
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] ", self.severity)?;
        match &self.kind {
            DiagnosticKind::TypeMismatch {
                edge,
                expected,
                found,
            } => write!(
                f,
                "type mismatch on {edge}: expected `{expected}`, found `{found}`"
            ),
            DiagnosticKind::Cycle {
                nodes,
                uncorrelated_edges,
            } => {
                let node_list = nodes
                    .iter()
                    .map(NodeId::as_str)
                    .collect::<Vec<_>>()
                    .join(" -> ");
                write!(
                    f,
                    "cycle among plain data edges: {node_list} (uncorrelated edges: {})",
                    uncorrelated_edges.len()
                )
            }
            DiagnosticKind::UnpairedPatternNode { node, pattern } => {
                write!(
                    f,
                    "node `{node}` declares pattern {pattern:?} but is not wired to a matching counterpart"
                )
            }
            DiagnosticKind::PartialPatternPair {
                client,
                server,
                kind,
                has_request_edge,
                has_response_edge,
            } => write!(
                f,
                "{kind} pair `{client}` <-> `{server}` is only partially wired (request: {has_request_edge}, response: {has_response_edge})"
            ),
            DiagnosticKind::UnconsumedOutput { node, output } => {
                write!(f, "output `{output}` on node `{node}` has no consumer")
            }
            DiagnosticKind::OrphanedOutputType { node, output } => write!(
                f,
                "node `{node}` declares an output_types entry for `{output}`, which is not in its outputs list"
            ),
            DiagnosticKind::OrphanedInputType { node, input } => write!(
                f,
                "node `{node}` declares an input_types entry for `{input}`, which is not in its inputs map"
            ),
            DiagnosticKind::RecordSugarPortCollision { node, port } => write!(
                f,
                "node `{node}`'s record: sugar wants a synthetic port `{port}` that collides with an explicitly declared input; the declared input wins"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn severity_orders_info_below_warning_below_error() {
        assert!(Severity::Info < Severity::Warning);
        assert!(Severity::Warning < Severity::Error);
    }

    #[test]
    fn diagnostic_edge_is_some_only_for_type_mismatch() {
        let d = Diagnostic::new(
            Severity::Error,
            DiagnosticKind::TypeMismatch {
                edge: EdgeKey::new(NodeId::new("detector"), PortName::new("frames")),
                expected: Urn::new("std/media/v1/Image"),
                found: Urn::new("std/vision/v1/Detections"),
            },
        );
        assert!(d.edge().is_some());

        let d2 = Diagnostic::new(
            Severity::Info,
            DiagnosticKind::UnconsumedOutput {
                node: NodeId::new("camera"),
                output: PortName::new("frames"),
            },
        );
        assert!(d2.edge().is_none());
    }

    #[test]
    fn diagnostic_display_includes_severity_tag() {
        let d = Diagnostic::new(
            Severity::Warning,
            DiagnosticKind::UnconsumedOutput {
                node: NodeId::new("camera"),
                output: PortName::new("frames"),
            },
        );
        let rendered = d.to_string();
        assert!(rendered.starts_with("[warning]"), "was: {rendered}");
        assert!(rendered.contains("camera"), "was: {rendered}");
    }

    #[test]
    fn severity_display_is_lowercase() {
        assert_eq!(Severity::Error.to_string(), "error");
        assert_eq!(Severity::Warning.to_string(), "warning");
        assert_eq!(Severity::Info.to_string(), "info");
    }
}
