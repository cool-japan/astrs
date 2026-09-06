//! Edge type checking against declared type URNs (blueprint §7, §8.2).
//!
//! For every edge, the consumer's declared input type ("expected") is
//! compared against the producer's declared output type ("found"):
//!
//! - Either side **absent** (no `input_types`/`output_types` entry, or a
//!   virtual source, which never has a declared type) — the manifest's
//!   explicit `type: any` opt-out (blueprint §3.7) — flows silently: no
//!   diagnostic at all, not even an [`Severity::Info`] one, matching the
//!   blueprint's "any/absent types flow silently."
//! - Both sides **present and equivalent** (see "Canonical comparison"
//!   below) — flows silently.
//! - Both sides **present and inequivalent**, but a `type_rules` entry
//!   allows the `{from: found, to: expected}` rewrite (compared the same
//!   canonical way) — flows silently.
//! - Otherwise — a [`DiagnosticKind::TypeMismatch`]: [`Severity::Error`]
//!   when the manifest's `strict_types` is set, [`Severity::Warning`]
//!   otherwise (blueprint §8.2: "incompatibility = error when
//!   strict_types else warning").
//!
//! # Canonical comparison
//!
//! Two URNs are equivalent when they name the same type with the same
//! parameter *values*, regardless of the *order* those parameters were
//! written in — `std/media/v1/Image[width=640,pixel=rgb8]` and
//! `std/media/v1/Image[pixel=rgb8,width=640]` are the same type; changing
//! a parameter's value (`pixel=rgb8` vs `pixel=bgr8`) still makes a
//! different type, and only a `type_rules` entry (or a byte-identical
//! re-declaration) bridges that.
//!
//! This crate does not re-derive that grammar: [`astrs_data::TypeUrn`]
//! already parses a URN into its canonical, parameter-sorted text
//! ([`astrs_data::TypeUrn::as_str`]), so [`canonical_form`] delegates to
//! it and compares the canonical strings.
//!
//! [`astrs_manifest::Urn`] and [`astrs_data::TypeUrn`] deliberately accept
//! *different* grammars, though: the manifest crate's syntax check allows
//! any non-empty namespace path and mixed-case identifiers (it does not
//! know what `std` means), while `TypeUrn` only recognizes the closed
//! `std` namespace with lowercase categories/parameter keys (blueprint
//! §24.3). A manifest URN that `TypeUrn::parse` rejects — a vendor/future
//! namespace, an uppercase category, an underscore in the type name — is
//! not a bug in either crate; it is simply a URN outside `TypeUrn`'s
//! closed grammar. [`canonical_form`] falls back to the URN's raw text in
//! that case, which is *not* a rare edge case: it is the normal path for
//! every URN that is not a plain, already-lowercase `std` type. Falling
//! back to raw text recovers this module's original exact-string
//! comparison for exactly the URNs `TypeUrn` cannot canonicalize, so
//! nothing regresses for them — they simply do not get order-insensitive
//! parameter matching.

use std::borrow::Cow;
use std::collections::BTreeMap;

use astrs_data::TypeUrn;
use astrs_manifest::{TypeRule, Urn};
use serde::{Deserialize, Serialize};

use crate::diagnostic::{Diagnostic, DiagnosticKind, Severity};
use crate::edge::{Edge, EdgeKey, EdgeSource};
use crate::ids::NodeId;
use crate::node::GraphNode;

/// The canonical comparison key for one URN — see this module's "Canonical
/// comparison" docs.
///
/// Returns the [`astrs_data::TypeUrn`] canonical text (parameters sorted by
/// key) when `urn` parses under that crate's closed `std`-namespace
/// grammar, and `urn`'s own raw text otherwise. Borrows rather than
/// allocates in the (common, for a plain unparameterized type) case where
/// the raw text already equals the canonical text of a successful parse —
/// [`astrs_data::TypeUrn::as_str`] returns a `&str` this function copies
/// into an owned string only when a rewrite (parameter reordering)
/// actually changes the text.
#[must_use]
fn canonical_form(urn: &Urn) -> Cow<'_, str> {
    match TypeUrn::parse(urn.as_str()) {
        Ok(parsed) if parsed.as_str() == urn.as_str() => Cow::Borrowed(urn.as_str()),
        Ok(parsed) => Cow::Owned(parsed.as_str().to_owned()),
        Err(_) => Cow::Borrowed(urn.as_str()),
    }
}

/// Whether `a` and `b` name the same type under [`canonical_form`]'s
/// comparison rule.
#[must_use]
fn urns_equivalent(a: &Urn, b: &Urn) -> bool {
    canonical_form(a) == canonical_form(b)
}

/// The result of type-checking one edge, before it is folded into a
/// [`Diagnostic`] — kept as its own type so
/// [`crate::graph::DataflowGraph::edge_type_status`] can hand back a
/// structured answer for one edge without re-running the whole-graph pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeTypeStatus {
    /// Either side is untyped (or the source is virtual) — flows silently.
    AnyOrAbsent,
    /// Both sides agree, or a `type_rules` entry bridges them.
    Compatible,
    /// Both sides are typed, disagree, and no rule bridges them.
    Mismatch {
        /// The consumer's declared input type.
        expected: Urn,
        /// The producer's declared output type.
        found: Urn,
    },
}

/// Resolve a producer output's declared type URN for one edge, or `None`
/// for a virtual source or a dangling producer reference (the latter
/// cannot occur from [`crate::graph::DataflowGraph::from_manifest`] on a
/// pre-validated manifest, but this function does not assume that — it is
/// also exercised directly by the diff/apply path in [`crate::diff`],
/// which builds graphs from arbitrary caller-supplied node/edge maps).
fn producer_output_type<'a>(
    nodes: &'a BTreeMap<NodeId, GraphNode>,
    source: &EdgeSource,
) -> Option<&'a Urn> {
    match source {
        EdgeSource::NodeOutput { node, output } => nodes.get(node)?.output_type(output),
        EdgeSource::Virtual(_) => None,
    }
}

/// Whether `type_rules` contains an entry allowing `found` to feed an
/// input declared as `expected`, comparing each side with
/// [`urns_equivalent`] rather than raw equality — a rule written as
/// `{from: "std/core/v1/Float32", to: "std/core/v1/Vector[dim=3,unit=m]"}`
/// still applies to a port declared `Vector[unit=m,dim=3]`.
fn rule_allows(type_rules: &[TypeRule], found: &Urn, expected: &Urn) -> bool {
    type_rules
        .iter()
        .any(|rule| urns_equivalent(&rule.from, found) && urns_equivalent(&rule.to, expected))
}

/// Type-check one edge, returning its status without deciding severity —
/// [`check_types`] is what turns a [`EdgeTypeStatus::Mismatch`] into a
/// [`Diagnostic`] at the right severity for the whole graph.
pub(crate) fn edge_status(
    nodes: &BTreeMap<NodeId, GraphNode>,
    consumer: &NodeId,
    input: &crate::ids::PortName,
    edge: &Edge,
    type_rules: &[TypeRule],
) -> EdgeTypeStatus {
    let expected = nodes.get(consumer).and_then(|n| n.input_type(input));
    let found = producer_output_type(nodes, &edge.from);

    match (expected, found) {
        (Some(expected), Some(found)) => {
            if urns_equivalent(expected, found) || rule_allows(type_rules, found, expected) {
                EdgeTypeStatus::Compatible
            } else {
                EdgeTypeStatus::Mismatch {
                    expected: expected.clone(),
                    found: found.clone(),
                }
            }
        }
        _ => EdgeTypeStatus::AnyOrAbsent,
    }
}

/// Type-check every edge in `edges`, producing one [`Diagnostic`] per
/// mismatch. Compatible and any/absent edges produce nothing.
pub(crate) fn check_types(
    nodes: &BTreeMap<NodeId, GraphNode>,
    edges: &BTreeMap<EdgeKey, Edge>,
    type_rules: &[TypeRule],
    strict_types: bool,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for (key, edge) in edges {
        let status = edge_status(nodes, &key.consumer, &key.input, edge, type_rules);
        if let EdgeTypeStatus::Mismatch { expected, found } = status {
            let severity = if strict_types {
                Severity::Error
            } else {
                Severity::Warning
            };
            diagnostics.push(Diagnostic::new(
                severity,
                DiagnosticKind::TypeMismatch {
                    edge: key.clone(),
                    expected,
                    found,
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
    use crate::edge::QueueConfig;
    use crate::ids::{MachineId, PortName};
    use crate::node::{InputPort, OutputPort};

    fn producer(output: &str, urn: Option<&str>) -> GraphNode {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            PortName::new(output),
            OutputPort {
                type_urn: urn.map(Urn::new),
            },
        );
        GraphNode {
            id: NodeId::new("producer"),
            outputs,
            inputs: BTreeMap::new(),
            pattern: None,
            machine: MachineId::CoordinatorLocal,
            spawns_process: true,
        }
    }

    fn consumer(input: &str, urn: Option<&str>) -> GraphNode {
        let mut inputs = BTreeMap::new();
        inputs.insert(
            PortName::new(input),
            InputPort {
                type_urn: urn.map(Urn::new),
            },
        );
        GraphNode {
            id: NodeId::new("consumer"),
            outputs: BTreeMap::new(),
            inputs,
            pattern: None,
            machine: MachineId::CoordinatorLocal,
            spawns_process: true,
        }
    }

    fn wired(
        producer_urn: Option<&str>,
        consumer_urn: Option<&str>,
    ) -> (BTreeMap<NodeId, GraphNode>, BTreeMap<EdgeKey, Edge>) {
        let p = producer("out", producer_urn);
        let c = consumer("in", consumer_urn);
        let mut nodes = BTreeMap::new();
        nodes.insert(NodeId::new("producer"), p);
        nodes.insert(NodeId::new("consumer"), c);

        let mut edges = BTreeMap::new();
        edges.insert(
            EdgeKey::new(NodeId::new("consumer"), PortName::new("in")),
            Edge {
                from: EdgeSource::NodeOutput {
                    node: NodeId::new("producer"),
                    output: PortName::new("out"),
                },
                queue: QueueConfig::default(),
            },
        );
        (nodes, edges)
    }

    #[test]
    fn matching_types_are_compatible() {
        let (nodes, edges) = wired(
            Some("std/vision/v1/Detections"),
            Some("std/vision/v1/Detections"),
        );
        let diags = check_types(&nodes, &edges, &[], false);
        assert!(diags.is_empty());
    }

    #[test]
    fn absent_types_flow_silently() {
        let (nodes, edges) = wired(None, None);
        let diags = check_types(&nodes, &edges, &[], true);
        assert!(diags.is_empty());
    }

    #[test]
    fn one_side_absent_flows_silently() {
        let (nodes, edges) = wired(Some("std/vision/v1/Detections"), None);
        let diags = check_types(&nodes, &edges, &[], true);
        assert!(diags.is_empty());
    }

    #[test]
    fn mismatch_is_warning_when_not_strict() {
        let (nodes, edges) = wired(Some("std/core/v1/Float32"), Some("std/core/v1/Float64"));
        let diags = check_types(&nodes, &edges, &[], false);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Warning);
    }

    #[test]
    fn mismatch_is_error_when_strict() {
        let (nodes, edges) = wired(Some("std/core/v1/Float32"), Some("std/core/v1/Float64"));
        let diags = check_types(&nodes, &edges, &[], true);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Error);
    }

    #[test]
    fn type_rule_bridges_mismatch_even_when_strict() {
        let (nodes, edges) = wired(Some("std/core/v1/Float32"), Some("std/core/v1/Float64"));
        let rules = vec![TypeRule {
            from: Urn::new("std/core/v1/Float32"),
            to: Urn::new("std/core/v1/Float64"),
        }];
        let diags = check_types(&nodes, &edges, &rules, true);
        assert!(diags.is_empty());
    }

    #[test]
    fn type_rule_is_directional() {
        let (nodes, edges) = wired(Some("std/core/v1/Float64"), Some("std/core/v1/Float32"));
        // Rule only allows Float32 -> Float64, not the reverse.
        let rules = vec![TypeRule {
            from: Urn::new("std/core/v1/Float32"),
            to: Urn::new("std/core/v1/Float64"),
        }];
        let diags = check_types(&nodes, &edges, &rules, false);
        assert_eq!(diags.len(), 1);
    }

    #[test]
    fn virtual_source_edge_is_any_or_absent() {
        let c = consumer("tick", Some("std/time/v1/Timestamp"));
        let mut nodes = BTreeMap::new();
        nodes.insert(NodeId::new("consumer"), c);
        let mut edges = BTreeMap::new();
        edges.insert(
            EdgeKey::new(NodeId::new("consumer"), PortName::new("tick")),
            Edge {
                from: EdgeSource::Virtual("astrs/timer/hz/50".to_string()),
                queue: QueueConfig::default(),
            },
        );
        let status = edge_status(
            &nodes,
            &NodeId::new("consumer"),
            &PortName::new("tick"),
            edges.values().next().expect("one edge"),
            &[],
        );
        assert_eq!(status, EdgeTypeStatus::AnyOrAbsent);
    }

    // --- canonical (astrs-data TypeUrn) comparison ------------------------

    #[test]
    fn canonical_form_reorders_std_params() {
        let width_first = Urn::new("std/media/v1/Image[width=640,pixel=rgb8]");
        let pixel_first = Urn::new("std/media/v1/Image[pixel=rgb8,width=640]");
        assert_eq!(canonical_form(&width_first), canonical_form(&pixel_first));
        assert!(urns_equivalent(&width_first, &pixel_first));
    }

    #[test]
    fn canonical_form_falls_back_to_raw_text_for_non_std_namespace() {
        // `vendor/...` is syntactically valid to `astrs_manifest::Urn` but
        // outside `TypeUrn`'s closed `std`-only grammar (the ordinary
        // case this module's docs describe, not a corner case).
        let urn = Urn::new("vendor/thing/v1/Widget");
        assert_eq!(
            canonical_form(&urn),
            Cow::Borrowed("vendor/thing/v1/Widget")
        );
    }

    #[test]
    fn param_reordering_is_compatible_even_when_strict() {
        let (nodes, edges) = wired(
            Some("std/media/v1/Image[width=640,pixel=rgb8]"),
            Some("std/media/v1/Image[pixel=rgb8,width=640]"),
        );
        let diags = check_types(&nodes, &edges, &[], true);
        assert!(diags.is_empty(), "diagnostics: {diags:?}");
    }

    #[test]
    fn different_param_values_still_mismatch() {
        let (nodes, edges) = wired(
            Some("std/media/v1/Image[pixel=rgb8]"),
            Some("std/media/v1/Image[pixel=bgr8]"),
        );
        let diags = check_types(&nodes, &edges, &[], false);
        assert_eq!(diags.len(), 1, "diagnostics: {diags:?}");
    }

    #[test]
    fn non_std_namespace_param_reordering_is_not_bridged() {
        // Outside TypeUrn's grammar, comparison falls back to exact text,
        // so parameter order still matters — a documented limitation of
        // the fallback path, exercised here rather than left implicit.
        let (nodes, edges) = wired(
            Some("vendor/thing/v1/Widget[b=2,a=1]"),
            Some("vendor/thing/v1/Widget[a=1,b=2]"),
        );
        let diags = check_types(&nodes, &edges, &[], false);
        assert_eq!(diags.len(), 1, "diagnostics: {diags:?}");
    }

    #[test]
    fn type_rule_bridges_across_param_order() {
        let (nodes, edges) = wired(
            Some("std/geometry/v1/Vector3[unit=m]"),
            Some("std/geometry/v1/Vector3[frame=base,unit=m]"),
        );
        // The rule's own params are written in a different order than
        // either side of the edge; `rule_allows` must still match it.
        let rules = vec![TypeRule {
            from: Urn::new("std/geometry/v1/Vector3[unit=m]"),
            to: Urn::new("std/geometry/v1/Vector3[unit=m,frame=base]"),
        }];
        let diags = check_types(&nodes, &edges, &rules, true);
        assert!(diags.is_empty(), "diagnostics: {diags:?}");
    }
}
