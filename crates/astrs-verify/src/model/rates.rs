//! Deriving exact firing rates from the manifest's declared ones.
//!
//! # The rule
//!
//! An AstRS node runs a merged event loop (blueprint §9.1): it fires once
//! per delivered event, whichever input that event arrived on. So a node's
//! firing rate is the **sum** of its inputs' arrival rates, and an output's
//! arrival rate downstream is its producer's firing rate — one message per
//! firing, the model this crate documents on
//! [`Model::messages_per_firing`](super::Model::messages_per_firing).
//!
//! Rates enter the graph in exactly one place: a periodic virtual timer
//! (`astrs/timer/…`, blueprint §8.4) or a rate a verification profile
//! declared for a source node. Everything else is derived, exactly, in
//! rational arithmetic ([`Rate`]).
//!
//! # Where derivation stops
//!
//! Three situations leave a node's rate genuinely unknown, and the
//! propagation reports each rather than inventing a number:
//!
//! - **Undeclared sources.** A node with no wired inputs runs on its own
//!   schedule; nothing in the manifest says how fast.
//! - **Aperiodic virtual sources.** `astrs/logs` and `astrs/status` deliver
//!   events when something happens, at no declared rate.
//! - **Feedback cycles.** A node whose own output reaches its own input
//!   adds its rate to itself, so the sum has no finite solution — which is
//!   the honest reading of an event-driven feedback loop, and is
//!   independently reported as a cycle by
//!   [`astrs_graph::DataflowGraph::diagnostics`].
//!
//! Indeterminacy is contagious: a node with an indeterminate input is
//! itself indeterminate.

use std::collections::{BTreeMap, BTreeSet};

use astrs_graph::{DataflowGraph, EdgeKey, NodeId};

use super::channel::{Channel, Producer};
use crate::scale::Rate;

/// Every node's derived firing rate, plus why a rate is missing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DerivedRates {
    /// Firing rates for the nodes whose rate the model determines.
    pub rates: BTreeMap<NodeId, Rate>,
    /// Nodes whose rate is not determined, with the reason.
    pub indeterminate: BTreeMap<NodeId, Indeterminate>,
}

impl DerivedRates {
    /// One node's derived firing rate.
    #[must_use]
    pub fn rate_of(&self, node: &NodeId) -> Option<Rate> {
        self.rates.get(node).copied()
    }

    /// Why a node has no derived rate.
    #[must_use]
    pub fn reason(&self, node: &NodeId) -> Option<Indeterminate> {
        self.indeterminate.get(node).copied()
    }

    /// The arrival rate on one channel: its timer's rate, or its
    /// producer's firing rate.
    #[must_use]
    pub fn arrival_rate(&self, channel: &Channel) -> Option<Rate> {
        match &channel.producer {
            Producer::Timer { rate, .. } => Some(*rate),
            Producer::Aperiodic { .. } => None,
            Producer::Node { node, .. } => self.rate_of(node),
        }
    }
}

/// Why a node's firing rate could not be derived.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Indeterminate {
    /// The node has no wired inputs and no profile-declared rate.
    UndeclaredSource,
    /// The node reads an aperiodic virtual source (`astrs/logs`,
    /// `astrs/status`), which has no declared rate.
    AperiodicSource,
    /// The node takes part in a feedback cycle.
    FeedbackCycle,
    /// An upstream node's rate is itself indeterminate.
    UpstreamIndeterminate,
    /// Summing the input rates overflowed the exact rational
    /// representation — unreachable for any realistic manifest, reported
    /// rather than saturated so a wrong number never reaches a proof.
    RateOverflow,
}

impl Indeterminate {
    /// A one-line explanation for a report.
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::UndeclaredSource => {
                "the node has no wired inputs and no declared rate, so it may emit at any rate"
            }
            Self::AperiodicSource => {
                "the node reads an aperiodic virtual source, which fires when events happen rather than on a schedule"
            }
            Self::FeedbackCycle => {
                "the node takes part in a feedback cycle, so its firing rate has no finite solution"
            }
            Self::UpstreamIndeterminate => "an upstream node's rate is itself undetermined",
            Self::RateOverflow => "the summed input rate is not representable exactly",
        }
    }
}

/// Derive every node's firing rate.
///
/// `declared` supplies rates for source nodes (from a verification
/// profile); nodes absent from it and lacking wired inputs are
/// [`Indeterminate::UndeclaredSource`].
///
/// The result is a pure function of the graph and the declarations: nodes
/// are visited in [`NodeId`] order, and the fixed point is reached in at
/// most one pass per node, so two runs on the same input produce the same
/// map.
#[must_use]
pub fn derive(
    graph: &DataflowGraph,
    channels: &BTreeMap<EdgeKey, Channel>,
    declared: &BTreeMap<NodeId, Rate>,
) -> DerivedRates {
    let mut derived = DerivedRates::default();
    let cyclic = cyclic_nodes(graph);

    for id in cyclic.iter() {
        derived
            .indeterminate
            .insert(id.clone(), Indeterminate::FeedbackCycle);
    }

    // Seed sources, then relax until nothing changes. The bound is the
    // node count: each pass settles at least one more node of any
    // acyclic chain, and cyclic nodes are already settled above.
    for (id, node) in &graph.nodes {
        if derived.indeterminate.contains_key(id) {
            continue;
        }
        if node.inputs.is_empty() {
            match declared.get(id) {
                Some(rate) => {
                    derived.rates.insert(id.clone(), *rate);
                }
                None => {
                    derived
                        .indeterminate
                        .insert(id.clone(), Indeterminate::UndeclaredSource);
                }
            }
        }
    }

    let passes = graph.nodes.len().saturating_add(1);
    for _ in 0..passes {
        let mut changed = false;
        for id in graph.nodes.keys() {
            if derived.rates.contains_key(id) || derived.indeterminate.contains_key(id) {
                continue;
            }
            match settle(id, graph, channels, &derived) {
                Settled::Rate(rate) => {
                    derived.rates.insert(id.clone(), rate);
                    changed = true;
                }
                Settled::Indeterminate(reason) => {
                    derived.indeterminate.insert(id.clone(), reason);
                    changed = true;
                }
                Settled::Pending => {}
            }
        }
        if !changed {
            break;
        }
    }

    // Anything still unsettled sits behind a node that never settled;
    // with cycles already excluded this is unreachable, but leaving a
    // node absent from *both* maps would let a caller read "no rate, no
    // reason" as "rate zero".
    for id in graph.nodes.keys() {
        if !derived.rates.contains_key(id) && !derived.indeterminate.contains_key(id) {
            derived
                .indeterminate
                .insert(id.clone(), Indeterminate::UpstreamIndeterminate);
        }
    }

    derived
}

/// What one relaxation step concluded about a node.
enum Settled {
    /// The node's rate is now known.
    Rate(Rate),
    /// The node's rate will never be known, for this reason.
    Indeterminate(Indeterminate),
    /// An input is not settled yet; try again next pass.
    Pending,
}

fn settle(
    id: &NodeId,
    graph: &DataflowGraph,
    channels: &BTreeMap<EdgeKey, Channel>,
    derived: &DerivedRates,
) -> Settled {
    let mut total = Rate::ZERO;
    let mut pending = false;
    for (key, _) in graph.edges_into(id) {
        let Some(channel) = channels.get(key) else {
            continue;
        };
        match &channel.producer {
            Producer::Aperiodic { .. } => {
                return Settled::Indeterminate(Indeterminate::AperiodicSource);
            }
            Producer::Timer { rate, .. } => match total.checked_add(*rate) {
                Some(sum) => total = sum,
                None => return Settled::Indeterminate(Indeterminate::RateOverflow),
            },
            Producer::Node { node, .. } => {
                if derived.indeterminate.contains_key(node) {
                    return Settled::Indeterminate(Indeterminate::UpstreamIndeterminate);
                }
                match derived.rate_of(node) {
                    Some(rate) => match total.checked_add(rate) {
                        Some(sum) => total = sum,
                        None => return Settled::Indeterminate(Indeterminate::RateOverflow),
                    },
                    None => pending = true,
                }
            }
        }
    }
    if pending {
        Settled::Pending
    } else {
        Settled::Rate(total)
    }
}

/// Every node that takes part in a feedback cycle.
///
/// A strongly connected component with more than one member is cyclic by
/// definition; a single-member component is cyclic only when the node
/// feeds one of its own inputs.
fn cyclic_nodes(graph: &DataflowGraph) -> BTreeSet<NodeId> {
    let mut cyclic = BTreeSet::new();
    for scc in graph.sccs() {
        if scc.nodes.len() > 1 {
            cyclic.extend(scc.nodes.iter().cloned());
        }
    }
    for (key, edge) in &graph.edges {
        if edge.from.producer_node() == Some(&key.consumer) {
            cyclic.insert(key.consumer.clone());
        }
    }
    cyclic
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::model::Model;
    use astrs_manifest::Manifest;

    fn model_of(yaml: &str) -> Model {
        let manifest = Manifest::from_yaml_str(yaml).expect("valid yaml");
        manifest.validate().expect("valid manifest");
        let (graph, _) = astrs_graph::DataflowGraph::from_manifest(&manifest).expect("graph");
        Model::from_graph(&graph).expect("model")
    }

    #[test]
    fn timer_rate_propagates_down_a_chain() {
        let model = model_of(
            "
nodes:
  - id: camera
    path: ./camera
    inputs: { tick: astrs/timer/hz/50 }
    outputs: [frames]
  - id: detector
    path: ./detector
    inputs: { frames: camera/frames }
    outputs: [dets]
  - id: planner
    path: ./planner
    inputs: { dets: detector/dets }
",
        );
        assert_eq!(model.rate_of(&NodeId::new("camera")), Rate::new(50, 1));
        assert_eq!(model.rate_of(&NodeId::new("detector")), Rate::new(50, 1));
        assert_eq!(model.rate_of(&NodeId::new("planner")), Rate::new(50, 1));
    }

    #[test]
    fn merged_inputs_sum_their_rates() {
        let model = model_of(
            "
nodes:
  - id: fast
    path: ./fast
    inputs: { tick: astrs/timer/hz/50 }
    outputs: [out]
  - id: slow
    path: ./slow
    inputs: { tick: astrs/timer/secs/2 }
    outputs: [out]
  - id: merge
    path: ./merge
    inputs:
      a: fast/out
      b: slow/out
",
        );
        // 50 Hz + 1/2 Hz = 101/2 Hz, exactly.
        assert_eq!(model.rate_of(&NodeId::new("merge")), Rate::new(101, 2));
    }

    #[test]
    fn undeclared_sources_stay_indeterminate_and_infect_downstream() {
        let model = model_of(
            "
nodes:
  - id: sensor
    path: ./sensor
    outputs: [raw]
  - id: filter
    path: ./filter
    inputs: { raw: sensor/raw }
",
        );
        assert_eq!(model.rate_of(&NodeId::new("sensor")), None);
        assert_eq!(
            model.rates().reason(&NodeId::new("sensor")),
            Some(Indeterminate::UndeclaredSource)
        );
        assert_eq!(
            model.rates().reason(&NodeId::new("filter")),
            Some(Indeterminate::UpstreamIndeterminate)
        );
    }

    #[test]
    fn aperiodic_virtual_sources_are_indeterminate() {
        let model = model_of(
            "
nodes:
  - id: supervisor
    path: ./supervisor
    inputs: { status: astrs/status }
",
        );
        assert_eq!(
            model.rates().reason(&NodeId::new("supervisor")),
            Some(Indeterminate::AperiodicSource)
        );
    }

    #[test]
    fn feedback_cycles_are_indeterminate() {
        let model = model_of(
            "
nodes:
  - id: a
    path: ./a
    inputs: { tick: astrs/timer/hz/10, back: b/out }
    outputs: [out]
  - id: b
    path: ./b
    inputs: { fwd: a/out }
    outputs: [out]
",
        );
        for id in ["a", "b"] {
            assert_eq!(
                model.rates().reason(&NodeId::new(id)),
                Some(Indeterminate::FeedbackCycle),
                "{id}"
            );
        }
    }

    #[test]
    fn self_loops_are_cycles_too() {
        let model = model_of(
            "
nodes:
  - id: loopy
    path: ./loopy
    inputs: { back: loopy/out }
    outputs: [out]
",
        );
        assert_eq!(
            model.rates().reason(&NodeId::new("loopy")),
            Some(Indeterminate::FeedbackCycle)
        );
    }

    #[test]
    fn derivation_is_deterministic() {
        let yaml = "
nodes:
  - id: a
    path: ./a
    inputs: { tick: astrs/timer/hz/7 }
    outputs: [out]
  - id: b
    path: ./b
    inputs: { tick: astrs/timer/millis/250, x: a/out }
    outputs: [out]
  - id: c
    path: ./c
    inputs: { y: b/out }
";
        let first = model_of(yaml);
        let second = model_of(yaml);
        assert_eq!(first.rates(), second.rates());
        // 7 Hz + 4 Hz = 11 Hz.
        assert_eq!(first.rate_of(&NodeId::new("b")), Rate::new(11, 1));
        assert_eq!(first.rate_of(&NodeId::new("c")), Rate::new(11, 1));
    }

    #[test]
    fn every_node_lands_in_exactly_one_map() {
        let model = model_of(
            "
nodes:
  - id: src
    path: ./src
    outputs: [out]
  - id: timed
    path: ./timed
    inputs: { tick: astrs/timer/hz/5 }
    outputs: [out]
  - id: sink
    path: ./sink
    inputs: { a: src/out, b: timed/out }
",
        );
        let rates = model.rates();
        for id in ["src", "timed", "sink"] {
            let node = NodeId::new(id);
            assert_ne!(
                rates.rate_of(&node).is_some(),
                rates.reason(&node).is_some(),
                "{id} must be in exactly one map"
            );
        }
    }

    #[test]
    fn indeterminate_reasons_all_explain_themselves() {
        for reason in [
            Indeterminate::UndeclaredSource,
            Indeterminate::AperiodicSource,
            Indeterminate::FeedbackCycle,
            Indeterminate::UpstreamIndeterminate,
            Indeterminate::RateOverflow,
        ] {
            assert!(!reason.explanation().is_empty());
        }
    }
}
