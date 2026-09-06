//! The verification model: a dataflow graph reduced to exactly what the
//! obligations reason about.
//!
//! # What the model adds to the graph
//!
//! [`astrs_graph::DataflowGraph`] already knows the topology, the queue
//! configuration and the service/action pairings. The model adds the three
//! things a proof needs and a graph does not carry:
//!
//! 1. **Exact rates.** Timer sources are turned into exact rationals and
//!    propagated through the merged event loop — see [`DerivedRates`].
//! 2. **A blocking story.** Which channels can drop, which are
//!    eviction-immune, and which nodes block awaiting a correlated
//!    response. This is what makes a *wait-for* cycle expressible: AstRS
//!    input queues never stall a producer (both `queue_policy` values drop
//!    — blueprint §11.2), so the only blocking wait a manifest can
//!    express runs through a request/response correlation.
//! 3. **Declared expectations.** Per-input `timeout` values and anything a
//!    verification profile supplied, converted once onto the integer
//!    scales in [`crate::scale`].
//!
//! # The firing model
//!
//! One firing consumes one event and produces at most one message on each
//! declared output. See [`Model::messages_per_firing`] for why "at most
//! one" is both the right default and the *safe* direction for every
//! obligation here.

mod channel;
mod node;
mod rates;

use std::collections::{BTreeMap, BTreeSet};

pub use channel::{BACKPRESSURE_OVERBUFFER, Channel, Producer, virtual_source_of};
pub use node::{Activation, Node, ServiceTime};
pub use rates::{DerivedRates, Indeterminate};

use astrs_graph::{DataflowGraph, EdgeKey, NodeId, PatternPair, PortName};
use astrs_manifest::{TypeRule, Urn};

use crate::profile::{Profile, ProfilePath};
use crate::scale::{Nanos, Rate, Window};

/// A dataflow reduced to the facts the obligations reason about.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    nodes: BTreeMap<NodeId, Node>,
    channels: BTreeMap<EdgeKey, Channel>,
    pattern_pairs: Vec<PatternPair>,
    rates: DerivedRates,
    window: Window,
    paths: Vec<LatencyPath>,
    declared_rates: BTreeMap<NodeId, Rate>,
    type_rules: Vec<TypeRule>,
    strict_types: bool,
    edge_types: BTreeMap<EdgeKey, EdgeTypes>,
    profile_applied: bool,
}

/// The declared type URNs on both ends of one channel.
///
/// Kept on the model rather than re-derived per obligation because the
/// type obligation needs both ends together, and an input or output whose
/// `input_types`/`output_types` entry is absent is *untyped* — an explicit
/// opt-out (blueprint §3.7), not a missing lookup to retry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EdgeTypes {
    /// The producing output's declared type, if any.
    pub produced: Option<Urn>,
    /// The consuming input's declared type, if any.
    pub consumed: Option<Urn>,
}

impl EdgeTypes {
    /// Both ends, when both are declared.
    #[must_use]
    pub fn both(&self) -> Option<(&Urn, &Urn)> {
        Some((self.produced.as_ref()?, self.consumed.as_ref()?))
    }
}

impl Model {
    /// How many messages one firing puts on one declared output.
    ///
    /// **One**, and deliberately an upper bound rather than an exact
    /// count: a node's handler may emit on an output or skip it, and
    /// nothing in the manifest says which. Every obligation is written so
    /// that this upper bound errs in the safe direction:
    ///
    /// - The deadlock encoding assumes *maximal* production, so a channel
    ///   it still proves can never carry a message really never can.
    /// - The rate and latency encodings assume *maximal* arrival, so a
    ///   bound they prove holds for any handler that emits less.
    ///
    /// A profile that needs a different multiplicity (a node that fans one
    /// input into several outputs per firing) is a 0.2 concern; the field
    /// is documented here rather than silently assumed.
    pub const fn messages_per_firing() -> u64 {
        1
    }

    /// Build a model from a graph, with no verification profile.
    ///
    /// Every source node's rate is then undeclared, and every obligation
    /// that needs one reports so rather than assuming a number.
    ///
    /// # Errors
    ///
    /// Returns [`crate::VerifyError`] if a declared duration is not
    /// representable, or if the declared rates admit no common integer
    /// analysis window (see [`crate::scale`]).
    pub fn from_graph(graph: &DataflowGraph) -> crate::Result<Self> {
        Self::build(graph, &Profile::empty())
    }

    /// Build a model from a graph and a verification profile.
    ///
    /// # Errors
    ///
    /// As [`Model::from_graph`].
    pub fn from_graph_with_profile(
        graph: &DataflowGraph,
        profile: &Profile,
    ) -> crate::Result<Self> {
        Self::build(graph, profile)
    }

    fn build(graph: &DataflowGraph, profile: &Profile) -> crate::Result<Self> {
        let pattern_pairs = graph.pattern_pairs();
        let correlated: BTreeSet<EdgeKey> = pattern_pairs
            .iter()
            .flat_map(PatternPair::edges)
            .cloned()
            .collect();

        let mut channels = BTreeMap::new();
        for (key, edge) in &graph.edges {
            let channel = Channel::from_edge(key, edge, correlated.contains(key))?;
            channels.insert(key.clone(), channel);
        }

        let declared_rates = profile.declared_rates();
        let rates = rates::derive(graph, &channels, &declared_rates);

        let mut nodes = BTreeMap::new();
        for (id, graph_node) in &graph.nodes {
            let inputs: Vec<EdgeKey> = graph.edges_into(id).map(|(key, _)| key.clone()).collect();
            let outputs: Vec<PortName> = graph_node.outputs.keys().cloned().collect();
            let produces: Vec<EdgeKey> = graph.edges_from(id).map(|(key, _)| key.clone()).collect();
            let activation = if !inputs.is_empty() {
                Activation::EventDriven
            } else {
                match declared_rates.get(id) {
                    Some(rate) => Activation::DeclaredSource(*rate),
                    None => Activation::UndeclaredSource,
                }
            };
            nodes.insert(
                id.clone(),
                Node {
                    id: id.clone(),
                    activation,
                    service_time: profile.service_time_of(id),
                    inputs,
                    outputs,
                    produces,
                    pattern: graph_node.pattern,
                    derived_rate: rates.rate_of(id),
                },
            );
        }

        let window = Window::covering(
            rates
                .rates
                .values()
                .copied()
                .chain(channels.values().filter_map(|c| c.producer.declared_rate())),
        )?;

        let paths = collect_paths(&channels, profile);
        let edge_types = collect_edge_types(graph);

        Ok(Self {
            nodes,
            channels,
            pattern_pairs,
            rates,
            window,
            paths,
            declared_rates,
            type_rules: graph.type_rules.clone(),
            strict_types: graph.strict_types,
            edge_types,
            profile_applied: !profile.is_empty(),
        })
    }

    /// Every rate a verification profile declared, whether or not the
    /// model used it.
    ///
    /// A declaration on an *event-driven* node is not used to set that
    /// node's rate — the graph already determines it — but it is not
    /// discarded either: [`crate::obligations::rate`] checks the
    /// declaration against the derived value, which is precisely the
    /// "declared rates versus what the graph delivers" obligation.
    #[must_use]
    pub fn declared_rates(&self) -> &BTreeMap<NodeId, Rate> {
        &self.declared_rates
    }

    /// The manifest's implicit type-coercion rules.
    #[must_use]
    pub fn type_rules(&self) -> &[TypeRule] {
        &self.type_rules
    }

    /// Whether the manifest asked for strict type checking.
    #[must_use]
    pub fn strict_types(&self) -> bool {
        self.strict_types
    }

    /// The declared types on one channel's two ends.
    #[must_use]
    pub fn edge_types(&self, key: &EdgeKey) -> Option<&EdgeTypes> {
        self.edge_types.get(key)
    }

    /// Every node, in id order.
    #[must_use]
    pub fn nodes(&self) -> &BTreeMap<NodeId, Node> {
        &self.nodes
    }

    /// One node, by id.
    #[must_use]
    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// Every channel, in edge-key order.
    #[must_use]
    pub fn channels(&self) -> &BTreeMap<EdgeKey, Channel> {
        &self.channels
    }

    /// One channel, by edge key.
    #[must_use]
    pub fn channel(&self, key: &EdgeKey) -> Option<&Channel> {
        self.channels.get(key)
    }

    /// Every derived service/action correlation.
    #[must_use]
    pub fn pattern_pairs(&self) -> &[PatternPair] {
        &self.pattern_pairs
    }

    /// The derived rate map, including the reasons rates are missing.
    #[must_use]
    pub fn rates(&self) -> &DerivedRates {
        &self.rates
    }

    /// One node's derived firing rate.
    #[must_use]
    pub fn rate_of(&self, id: &NodeId) -> Option<Rate> {
        self.rates.rate_of(id)
    }

    /// The arrival rate on one channel.
    #[must_use]
    pub fn arrival_rate(&self, key: &EdgeKey) -> Option<Rate> {
        self.channels
            .get(key)
            .and_then(|c| self.rates.arrival_rate(c))
    }

    /// The analysis window every flow count is expressed over.
    #[must_use]
    pub fn window(&self) -> Window {
        self.window
    }

    /// Every latency path with a budget, in a stable order.
    #[must_use]
    pub fn paths(&self) -> &[LatencyPath] {
        &self.paths
    }

    /// Whether a non-empty verification profile contributed to this model.
    #[must_use]
    pub fn profile_applied(&self) -> bool {
        self.profile_applied
    }

    /// Every channel whose consumer is `node`, in edge-key order.
    pub fn inputs_of<'a>(&'a self, node: &NodeId) -> impl Iterator<Item = &'a Channel> + use<'a> {
        let node = node.clone();
        self.channels
            .values()
            .filter(move |channel| channel.consumer() == &node)
    }

    /// Every channel fed by `node`, in edge-key order.
    pub fn outputs_of<'a>(&'a self, node: &NodeId) -> impl Iterator<Item = &'a Channel> + use<'a> {
        let node = node.clone();
        self.channels
            .values()
            .filter(move |channel| channel.producer.node() == Some(&node))
    }

    /// How many nodes and channels this model carries.
    #[must_use]
    pub fn size(&self) -> (usize, usize) {
        (self.nodes.len(), self.channels.len())
    }
}

/// Where a latency budget came from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PathOrigin {
    /// The manifest's own per-input `timeout` (blueprint §8.3), read as
    /// "a message must reach this input within this long of the tick that
    /// caused it".
    InputTimeout {
        /// The input the timeout was declared on.
        target: String,
    },
    /// A named end-to-end path a verification profile declared.
    Profile {
        /// The path's name in the profile.
        name: String,
    },
}

impl PathOrigin {
    /// A short label for reports.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::InputTimeout { target } => format!("timeout on {target}"),
            Self::Profile { name } => name.clone(),
        }
    }
}

/// One end-to-end latency obligation: a target and the budget it must meet.
///
/// The *route* is not stored: worst-case age is computed by the latency
/// encoding as a maximum over every incoming channel of every node on the
/// way, which is linear in the graph rather than exponential in its paths
/// — see [`crate::obligations::latency`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatencyPath {
    /// Where this budget came from.
    pub origin: PathOrigin,
    /// The channel whose delivery must meet the budget.
    pub target: EdgeKey,
    /// The budget.
    pub budget: Nanos,
}

/// Pair up the declared type of each channel's two ends.
fn collect_edge_types(graph: &DataflowGraph) -> BTreeMap<EdgeKey, EdgeTypes> {
    let mut out = BTreeMap::new();
    for (key, edge) in &graph.edges {
        let consumed = graph
            .nodes
            .get(&key.consumer)
            .and_then(|node| node.input_type(&key.input))
            .cloned();
        let produced = match &edge.from {
            astrs_graph::EdgeSource::NodeOutput { node, output } => graph
                .nodes
                .get(node)
                .and_then(|producer| producer.output_type(output))
                .cloned(),
            astrs_graph::EdgeSource::Virtual(_) => None,
        };
        out.insert(key.clone(), EdgeTypes { produced, consumed });
    }
    out
}

/// Collect every latency budget: the manifest's own input timeouts, then
/// the profile's named paths.
fn collect_paths(channels: &BTreeMap<EdgeKey, Channel>, profile: &Profile) -> Vec<LatencyPath> {
    let mut paths = Vec::new();
    for (key, channel) in channels {
        if let Some(budget) = channel.timeout {
            paths.push(LatencyPath {
                origin: PathOrigin::InputTimeout {
                    target: key.to_string(),
                },
                target: key.clone(),
                budget,
            });
        }
    }
    for ProfilePath {
        name,
        target,
        budget,
    } in profile.paths()
    {
        paths.push(LatencyPath {
            origin: PathOrigin::Profile { name: name.clone() },
            target: target.clone(),
            budget: *budget,
        });
    }
    paths
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use astrs_manifest::Manifest;

    fn model_of(yaml: &str) -> Model {
        let manifest = Manifest::from_yaml_str(yaml).expect("valid yaml");
        manifest.validate().expect("valid manifest");
        let (graph, _) = DataflowGraph::from_manifest(&manifest).expect("graph");
        Model::from_graph(&graph).expect("model")
    }

    const PIPELINE: &str = "
nodes:
  - id: camera
    path: ./camera
    inputs: { tick: astrs/timer/hz/50 }
    outputs: [frames]
  - id: detector
    path: ./detector
    inputs:
      frames:
        source: camera/frames
        queue_size: 4
        timeout: 0.5
    outputs: [dets]
";

    #[test]
    fn model_carries_nodes_and_channels() {
        let model = model_of(PIPELINE);
        assert_eq!(model.size(), (2, 2));
        assert!(model.node(&NodeId::new("camera")).is_some());
        assert!(
            model
                .channel(&EdgeKey::new(
                    NodeId::new("detector"),
                    PortName::new("frames")
                ))
                .is_some()
        );
    }

    #[test]
    fn timer_input_makes_a_node_event_driven() {
        let model = model_of(PIPELINE);
        let camera = model.node(&NodeId::new("camera")).expect("node");
        assert_eq!(camera.activation, Activation::EventDriven);
        assert!(!camera.is_source());
        assert_eq!(camera.derived_rate, Rate::new(50, 1));
    }

    #[test]
    fn produced_channels_are_precomputed() {
        let model = model_of(PIPELINE);
        let camera = model.node(&NodeId::new("camera")).expect("node");
        assert_eq!(
            camera.produces,
            vec![EdgeKey::new(
                NodeId::new("detector"),
                PortName::new("frames")
            )]
        );
        assert_eq!(camera.outputs, vec![PortName::new("frames")]);
    }

    #[test]
    fn input_timeouts_become_latency_paths() {
        let model = model_of(PIPELINE);
        assert_eq!(model.paths().len(), 1);
        let path = &model.paths()[0];
        assert_eq!(path.budget, Nanos::new(500_000_000));
        assert_eq!(
            path.origin,
            PathOrigin::InputTimeout {
                target: "detector.frames".to_string()
            }
        );
        assert_eq!(path.origin.label(), "timeout on detector.frames");
    }

    #[test]
    fn window_covers_every_declared_period() {
        let model = model_of(
            "
nodes:
  - id: a
    path: ./a
    inputs: { tick: astrs/timer/secs/3 }
    outputs: [out]
  - id: b
    path: ./b
    inputs: { tick: astrs/timer/secs/5 }
",
        );
        assert_eq!(model.window().seconds(), 15);
    }

    #[test]
    fn correlated_edges_are_marked_immune() {
        let model = model_of(
            "
nodes:
  - id: client
    path: ./client
    pattern: service-client
    inputs:
      tick: astrs/timer/hz/1
      response: server/response
    outputs: [request]
  - id: server
    path: ./server
    pattern: service-server
    inputs: { request: client/request }
    outputs: [response]
",
        );
        let request = EdgeKey::new(NodeId::new("server"), PortName::new("request"));
        let channel = model.channel(&request).expect("channel");
        assert!(channel.correlated, "request edges get eviction immunity");
        assert!(!channel.can_drop());
        assert_eq!(model.pattern_pairs().len(), 1);
    }

    #[test]
    fn inputs_and_outputs_iterate_in_key_order() {
        let model = model_of(PIPELINE);
        let detector_inputs: Vec<String> = model
            .inputs_of(&NodeId::new("detector"))
            .map(|c| c.key.to_string())
            .collect();
        assert_eq!(detector_inputs, vec!["detector.frames".to_string()]);
        let camera_outputs: Vec<String> = model
            .outputs_of(&NodeId::new("camera"))
            .map(|c| c.key.to_string())
            .collect();
        assert_eq!(camera_outputs, vec!["detector.frames".to_string()]);
    }

    #[test]
    fn arrival_rate_follows_the_producer() {
        let model = model_of(PIPELINE);
        let key = EdgeKey::new(NodeId::new("detector"), PortName::new("frames"));
        assert_eq!(model.arrival_rate(&key), Rate::new(50, 1));
    }

    #[test]
    fn source_nodes_without_a_profile_are_undeclared() {
        let model = model_of(
            "
nodes:
  - id: sensor
    path: ./sensor
    outputs: [raw]
  - id: sink
    path: ./sink
    inputs: { raw: sensor/raw }
",
        );
        let sensor = model.node(&NodeId::new("sensor")).expect("node");
        assert_eq!(sensor.activation, Activation::UndeclaredSource);
        assert!(sensor.activation.is_indeterminate());
        assert!(!model.profile_applied());
    }

    #[test]
    fn model_construction_is_deterministic() {
        let a = model_of(PIPELINE);
        let b = model_of(PIPELINE);
        assert_eq!(a, b);
    }

    #[test]
    fn edge_types_pair_up_both_ends() {
        let model = model_of(
            "
strict_types: true
nodes:
  - id: camera
    path: ./camera
    inputs: { tick: astrs/timer/hz/1 }
    outputs: [frames]
    output_types: { frames: \"std/media/v1/Image\" }
  - id: detector
    path: ./detector
    inputs: { frames: camera/frames }
    input_types: { frames: \"std/media/v1/Image\" }
",
        );
        let key = EdgeKey::new(NodeId::new("detector"), PortName::new("frames"));
        let types = model.edge_types(&key).expect("edge");
        let (produced, consumed) = types.both().expect("both declared");
        assert_eq!(produced.as_str(), "std/media/v1/Image");
        assert_eq!(consumed.as_str(), "std/media/v1/Image");
        assert!(model.strict_types());
        assert!(model.type_rules().is_empty());
    }

    #[test]
    fn virtual_sources_have_no_produced_type() {
        let model = model_of(PIPELINE);
        let key = EdgeKey::new(NodeId::new("camera"), PortName::new("tick"));
        let types = model.edge_types(&key).expect("edge");
        assert!(types.produced.is_none());
        assert!(types.both().is_none());
    }

    #[test]
    fn messages_per_firing_is_documented_as_one() {
        assert_eq!(Model::messages_per_firing(), 1);
    }
}
