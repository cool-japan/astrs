//! `Routing` — resolving a manifest's `operators:` list onto one
//! connected node's ports (blueprint §9.3).
//!
//! `astrs-manifest`'s own validation pass deliberately does **not** resolve
//! [`astrs_manifest::OperatorConfig::inputs`] (see that crate's
//! `validate` module docs): an operator's `source:` may legitimately name a
//! sibling operator hosted in the same runtime process, a graph the
//! manifest's node/output index knows nothing about. This module is the
//! "`astrs-runtime`'s concern" that documentation comment points to.
//!
//! # Resolving one operator input's `source:`
//!
//! For each `producer/port` string, in order:
//!
//! 1. **Sibling first.** If `producer` names another entry in the same
//!    node's `operators:` list *and that entry declares `port` among its
//!    own `outputs`*, this is an intra-runtime edge — delivered by a direct
//!    push into the producer's own worker thread, never touching the
//!    node's session (blueprint §4.2: "a crop → NMS chain costs a function
//!    call rather than an IPC hop").
//! 2. **External, by resolved port.** Otherwise, if the string parses as a
//!    [`PortRef`] and some input the *connected node itself* declares
//!    resolves from that exact port ([`Node::input_source`]), this input is
//!    fed from that node-level input — whatever the daemon actually wired,
//!    not a re-derivation of the manifest.
//! 3. **External, by name.** A virtual source (blueprint §8.4, e.g.
//!    `astrs/timer/hz/50`) cannot round-trip through a two-part `PortRef`
//!    at all (more than one `/`), so as a last resort this input's own
//!    *name* is tried directly against the node's declared input ids. A
//!    manifest author feeding a virtual source to a specific operator is
//!    expected to give that operator's input the same name as the node's
//!    own input for it — the same convention `astrs-node-api`'s own ports
//!    already rely on names for.
//! 4. Unresolved is a construction-time error
//!    ([`RuntimeError::UnresolvedOperatorInput`]), not a silently-dropped
//!    input: a manifest whose operator wiring cannot be honored is a bug
//!    worth catching before any thread runs, not after.
//!
//! Precedence is sibling-first; if a producer name happens to match both a
//! sibling and (coincidentally) an external port the node also resolves the
//! same [`PortRef`] from, the sibling wins and this is logged at `WARN` —
//! see `Routing::build`.
//!
//! # Resolving one operator output
//!
//! An output's fan-out is the union of two, non-exclusive, destinations:
//! every sibling whose input names `this_operator/this_output` (found while
//! resolving inputs, above), and — if the connected node itself declares an
//! output of the very same name — the node's own output. Both may apply at
//! once; neither is required.

use std::collections::HashMap;

use astrs_manifest::OperatorConfig;
use astrs_node_api::Node;
use astrs_operator_api::OpSend;
use astrs_wire::{DataId, Metadata, NodeId, PortRef, RouteCloseReason};

use crate::error::RuntimeError;
use crate::inbox::{OperatorInbox, log_queue_signal};
use crate::output_sink::OutputSink;

/// One resolved edge target: the operator (by index into
/// [`crate::config::RuntimeConfig::operators`]) and the local input name on
/// it.
pub(crate) type Target = (usize, DataId);

/// The fully-resolved wiring for one [`crate::RuntimeHost`] (blueprint
/// §9.3).
#[derive(Debug, Default)]
pub(crate) struct Routing {
    /// Each operator's id, in the same order as
    /// [`crate::config::RuntimeConfig::operators`] — kept alongside the
    /// routing tables (rather than re-borrowed from the config every time)
    /// so a worker thread can build a synthetic [`PortRef`] for its own
    /// outputs without holding a reference back into the config.
    pub(crate) operator_ids: Vec<NodeId>,
    /// A node-level input's id -> every operator input fed from it.
    external_inputs: HashMap<DataId, Vec<Target>>,
    /// `(producer operator index, its output name)` -> every sibling input
    /// fed from it.
    sibling_edges: HashMap<Target, Vec<Target>>,
    /// `(operator index, its output name)` entries that also publish on the
    /// connected node's own output of the same name.
    external_outputs: HashMap<Target, DataId>,
}

impl Routing {
    /// Resolves `operators` against `node`'s own declared ports.
    ///
    /// # Errors
    ///
    /// See the module docs: a malformed id, a duplicate operator id, or an
    /// input whose `source:` resolves nowhere.
    pub(crate) fn build(operators: &[OperatorConfig], node: &Node) -> Result<Self, RuntimeError> {
        let operator_ids = validate_operator_ids(operators)?;
        let index_of: HashMap<&str, usize> = operators
            .iter()
            .enumerate()
            .map(|(index, op)| (op.id.as_str(), index))
            .collect();

        let mut routing = Self {
            operator_ids,
            external_inputs: HashMap::new(),
            sibling_edges: HashMap::new(),
            external_outputs: HashMap::new(),
        };

        for (index, op) in operators.iter().enumerate() {
            for output_name in &op.outputs {
                let output_id = DataId::new(output_name).map_err(|source| {
                    RuntimeError::invalid_port_name(&op.id, output_name, source)
                })?;
                if node.descriptor().output(&output_id).is_some() {
                    routing
                        .external_outputs
                        .insert((index, output_id.clone()), output_id);
                }
            }
        }

        for (index, op) in operators.iter().enumerate() {
            for (input_name, input) in &op.inputs {
                let local_id = DataId::new(input_name).map_err(|source| {
                    RuntimeError::invalid_port_name(&op.id, input_name, source)
                })?;
                routing.resolve_input(
                    operators,
                    &index_of,
                    node,
                    index,
                    op,
                    &local_id,
                    &input.source,
                )?;
            }
        }

        Ok(routing)
    }

    /// Resolves one operator input's `source:` string, recording the edge
    /// it implies.
    #[allow(clippy::too_many_arguments)]
    fn resolve_input(
        &mut self,
        operators: &[OperatorConfig],
        index_of: &HashMap<&str, usize>,
        node: &Node,
        index: usize,
        op: &OperatorConfig,
        local_id: &DataId,
        source: &str,
    ) -> Result<(), RuntimeError> {
        let parsed: Option<PortRef> = source.parse().ok();

        if let Some(port) = &parsed
            && let Some(&producer_index) = index_of.get(port.node().as_str())
            && operators.get(producer_index).is_some_and(|producer| {
                producer
                    .outputs
                    .iter()
                    .any(|name| name == port.port().as_str())
            })
        {
            if let Some(external) = parsed
                .as_ref()
                .and_then(|port| find_external_input(node, port))
            {
                tracing::warn!(
                    operator = %op.id,
                    input = %local_id,
                    source,
                    external = %external,
                    "source matches both a sibling operator's output and an external node \
                     input resolving the same port; the sibling wins"
                );
            }
            self.sibling_edges
                .entry((producer_index, port.port().clone()))
                .or_default()
                .push((index, local_id.clone()));
            return Ok(());
        }

        if let Some(port) = &parsed
            && let Some(node_input) = find_external_input(node, port)
        {
            self.external_inputs
                .entry(node_input)
                .or_default()
                .push((index, local_id.clone()));
            return Ok(());
        }

        // Last resort: a virtual source (or any string that will never
        // parse as a two-part `PortRef`) resolved by the input's own name.
        if node.input_ids().contains(local_id) {
            self.external_inputs
                .entry(local_id.clone())
                .or_default()
                .push((index, local_id.clone()));
            return Ok(());
        }

        Err(RuntimeError::UnresolvedOperatorInput {
            operator: op.id.clone(),
            input: local_id.as_str().to_owned(),
            wanted: source.to_owned(),
        })
    }

    /// Every operator input fed from `node_input`, if any.
    #[must_use]
    pub(crate) fn external_targets(&self, node_input: &DataId) -> &[Target] {
        self.external_inputs
            .get(node_input)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Every sibling input fed from `(producer, output)`, if any.
    #[must_use]
    pub(crate) fn sibling_targets(&self, producer: usize, output: &DataId) -> &[Target] {
        self.sibling_edges
            .get(&(producer, output.clone()))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// The node-level output `(producer, output)` also publishes on, if any.
    #[must_use]
    pub(crate) fn external_output(&self, producer: usize, output: &DataId) -> Option<&DataId> {
        self.external_outputs.get(&(producer, output.clone()))
    }

    /// Every `(producer, output)` pair with at least one sibling consumer —
    /// used to notify siblings when a producer terminates permanently.
    #[must_use]
    pub(crate) fn outputs_of(&self, producer: usize) -> Vec<DataId> {
        self.sibling_edges
            .keys()
            .filter(|(index, _)| *index == producer)
            .map(|(_, output)| output.clone())
            .collect()
    }
}

/// The output mux: turns one operator's drained [`OpSend`]s into deliveries
/// to sibling inboxes and/or the connected node's own output (blueprint
/// §9.3's "operator sends -> node outputs").
///
/// Borrowed for the lifetime of one [`std::thread::scope`] call by every
/// operator worker thread at once — [`Routing`], the inbox slice and
/// [`OutputSink`] are all internally synchronized (or read-only), so no
/// `Arc` is needed to share this across threads that all outlive it only as
/// long as the scope does.
#[derive(Clone, Copy)]
pub(crate) struct Forwarder<'a> {
    routing: &'a Routing,
    inboxes: &'a [OperatorInbox],
    sink: &'a OutputSink,
}

impl<'a> Forwarder<'a> {
    /// Builds a forwarder over the whole host's routing table, inboxes and
    /// output sink.
    #[must_use]
    pub(crate) fn new(
        routing: &'a Routing,
        inboxes: &'a [OperatorInbox],
        sink: &'a OutputSink,
    ) -> Self {
        Self {
            routing,
            inboxes,
            sink,
        }
    }

    /// Delivers every send `from` operator `index` just produced to its
    /// resolved destinations — any number of siblings, the node's own
    /// output, both, or (if nothing consumes that output) neither.
    pub(crate) fn forward(&self, from: usize, sends: Vec<OpSend>) {
        let Some(producer_id) = self.routing.operator_ids.get(from) else {
            return;
        };
        for send in sends {
            let (id, metadata, payload) = send.into_parts();
            let source = PortRef::new(producer_id.clone(), id.clone());
            self.deliver_to_siblings(from, &id, &source, &metadata, &payload);
            if let Some(external_id) = self.routing.external_output(from, &id) {
                let _ignored = self.sink.send(external_id, metadata, &payload);
            }
        }
    }

    /// Marks every sibling consumer of `index`'s outputs closed — called
    /// once, when that operator's run through this host ends permanently
    /// (blueprint §9.3: siblings must still learn their input is gone, even
    /// though the wire protocol never sees this edge at all).
    pub(crate) fn close_outputs(&self, index: usize, reason: RouteCloseReason) {
        let Some(producer_id) = self.routing.operator_ids.get(index) else {
            return;
        };
        for output in self.routing.outputs_of(index) {
            let source = PortRef::new(producer_id.clone(), output.clone());
            for (target_index, target_input) in self.routing.sibling_targets(index, &output) {
                if let Some(inbox) = self.inboxes.get(*target_index) {
                    let _ignored = inbox.push_closed(target_input, source.clone(), reason.clone());
                }
            }
        }
    }

    fn deliver_to_siblings(
        &self,
        from: usize,
        output: &DataId,
        source: &PortRef,
        metadata: &Metadata,
        payload: &[u8],
    ) {
        for (target_index, target_input) in self.routing.sibling_targets(from, output) {
            let Some(inbox) = self.inboxes.get(*target_index) else {
                continue;
            };
            let report = inbox.push_message(
                target_input,
                source.clone(),
                metadata.clone(),
                payload.to_vec(),
            );
            log_queue_signal(inbox, target_input, report);
        }
    }
}

/// Validates every operator id's charset and uniqueness, returning them as
/// [`NodeId`]s (the type a synthetic intra-runtime [`PortRef`] needs) in
/// declaration order.
fn validate_operator_ids(operators: &[OperatorConfig]) -> Result<Vec<NodeId>, RuntimeError> {
    let mut seen = std::collections::HashSet::with_capacity(operators.len());
    let mut ids = Vec::with_capacity(operators.len());
    for op in operators {
        if !seen.insert(op.id.as_str()) {
            return Err(RuntimeError::DuplicateOperatorId { id: op.id.clone() });
        }
        let id = NodeId::new(&op.id)
            .map_err(|source| RuntimeError::invalid_operator_id(&op.id, source))?;
        ids.push(id);
    }
    Ok(ids)
}

/// The node-level input id whose declared source resolves to exactly
/// `port`, if one exists.
fn find_external_input(node: &Node, port: &PortRef) -> Option<DataId> {
    node.input_ids()
        .into_iter()
        .find(|id| node.input_source(id.as_str()).as_ref() == Some(port))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_manifest::Input;
    use astrs_node_api::testing::TestHarness;
    use astrs_wire::{InputSpec, NodeId as WireNodeId, NodeSource, NodeSpawnSpec, OutputSpec};
    use std::collections::BTreeMap;

    fn op(id: &str, inputs: BTreeMap<String, Input>, outputs: &[&str]) -> OperatorConfig {
        OperatorConfig {
            id: id.to_owned(),
            operator: format!("{id}Type"),
            dylib: None,
            wasm: None,
            hub: None,
            inputs,
            outputs: outputs.iter().map(|s| s.to_owned().to_owned()).collect(),
            config: BTreeMap::new(),
        }
    }

    fn harness_with_input(input_name: &str, producer: &str, producer_port: &str) -> TestHarness {
        let daemon = astrs_node_api::MockDaemon::start().unwrap();
        let spec = NodeSpawnSpec::new(
            daemon.dataflow(),
            WireNodeId::new("runtime-host").unwrap(),
            0,
            NodeSource::Dynamic,
        )
        .with_input(InputSpec::new(
            DataId::new(input_name).unwrap(),
            PortRef::from_parts(producer, producer_port).unwrap(),
        ))
        .with_output(OutputSpec::new(DataId::new("detections").unwrap()));
        TestHarness::with_spec(spec).unwrap()
    }

    #[test]
    fn a_sibling_edge_resolves_without_touching_the_node() {
        let harness = harness_with_input("frames", "camera", "image");
        let mut boxes_inputs = BTreeMap::new();
        boxes_inputs.insert("frames".to_owned(), Input::from_source("camera/image"));
        let crop = op("crop", boxes_inputs, &["boxes"]);
        let mut nms_inputs = BTreeMap::new();
        nms_inputs.insert("boxes".to_owned(), Input::from_source("crop/boxes"));
        let nms = op("nms", nms_inputs, &["detections"]);

        let routing = Routing::build(&[crop, nms], &harness.node).unwrap();
        let targets = routing.sibling_targets(0, &DataId::new("boxes").unwrap());
        assert_eq!(targets, &[(1, DataId::new("boxes").unwrap())]);
        // `nms`'s `boxes` input is a pure sibling edge: it never touches
        // `external_targets` at all, unlike `crop`'s own `frames` input
        // (fed from the node's real `camera/image` source), which does.
        assert!(
            routing
                .external_targets(&DataId::new("boxes").unwrap())
                .is_empty()
        );
        assert_eq!(
            routing.external_targets(&DataId::new("frames").unwrap()),
            &[(0, DataId::new("frames").unwrap())]
        );
    }

    #[test]
    fn an_external_edge_resolves_via_the_nodes_own_input_source() {
        let harness = harness_with_input("frames", "camera", "image");
        let mut inputs = BTreeMap::new();
        inputs.insert("img".to_owned(), Input::from_source("camera/image"));
        let crop = op("crop", inputs, &["boxes"]);

        let routing = Routing::build(&[crop], &harness.node).unwrap();
        let targets = routing.external_targets(&DataId::new("frames").unwrap());
        assert_eq!(targets, &[(0, DataId::new("img").unwrap())]);
    }

    #[test]
    fn a_virtual_source_falls_back_to_name_matching() {
        let harness = harness_with_input("tick", "astrs", "timer.hz.50");
        // The node's own input is fed from a resolved, two-part `PortRef`
        // (whatever the daemon actually wired); the operator's own
        // manifest text is the un-round-trippable virtual-source string.
        let mut inputs = BTreeMap::new();
        inputs.insert("tick".to_owned(), Input::from_source("astrs/timer/hz/50"));
        let planner = op("planner", inputs, &[]);

        let routing = Routing::build(&[planner], &harness.node).unwrap();
        let targets = routing.external_targets(&DataId::new("tick").unwrap());
        assert_eq!(targets, &[(0, DataId::new("tick").unwrap())]);
    }

    #[test]
    fn an_output_matching_the_nodes_own_output_is_marked_external() {
        let harness = harness_with_input("frames", "camera", "image");
        let nms = op("nms", BTreeMap::new(), &["detections"]);
        let routing = Routing::build(&[nms], &harness.node).unwrap();
        assert_eq!(
            routing.external_output(0, &DataId::new("detections").unwrap()),
            Some(&DataId::new("detections").unwrap())
        );
    }

    #[test]
    fn an_unresolved_input_is_a_construction_error() {
        let harness = harness_with_input("frames", "camera", "image");
        let mut inputs = BTreeMap::new();
        inputs.insert("mystery".to_owned(), Input::from_source("nobody/nothing"));
        let op = op("crop", inputs, &[]);
        let error = Routing::build(&[op], &harness.node).unwrap_err();
        assert!(
            matches!(error, RuntimeError::UnresolvedOperatorInput { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_sibling_reference_to_an_undeclared_output_falls_through_to_unresolved() {
        let harness = harness_with_input("frames", "camera", "image");
        let crop = op("crop", BTreeMap::new(), &["boxes"]);
        let mut inputs = BTreeMap::new();
        // `crop` exists but never declared `wrong`.
        inputs.insert("boxes".to_owned(), Input::from_source("crop/wrong"));
        let nms = op("nms", inputs, &[]);
        let error = Routing::build(&[crop, nms], &harness.node).unwrap_err();
        assert!(
            matches!(error, RuntimeError::UnresolvedOperatorInput { .. }),
            "{error}"
        );
    }

    #[test]
    fn duplicate_operator_ids_are_rejected() {
        let harness = harness_with_input("frames", "camera", "image");
        let a = op("crop", BTreeMap::new(), &[]);
        let b = op("crop", BTreeMap::new(), &[]);
        let error = Routing::build(&[a, b], &harness.node).unwrap_err();
        assert!(matches!(error, RuntimeError::DuplicateOperatorId { id } if id == "crop"));
    }

    #[test]
    fn an_invalid_operator_id_is_rejected() {
        let harness = harness_with_input("frames", "camera", "image");
        let bad = op("bad id", BTreeMap::new(), &[]);
        let error = Routing::build(&[bad], &harness.node).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidOperatorId { .. }));
    }

    #[test]
    fn an_invalid_port_name_is_rejected() {
        let harness = harness_with_input("frames", "camera", "image");
        let bad = op("crop", BTreeMap::new(), &["bad port"]);
        let error = Routing::build(&[bad], &harness.node).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidPortName { .. }));
    }

    #[test]
    fn outputs_of_lists_every_sibling_consuming_output() {
        let harness = harness_with_input("frames", "camera", "image");
        let crop = op("crop", BTreeMap::new(), &["boxes"]);
        let mut nms_inputs = BTreeMap::new();
        nms_inputs.insert("a".to_owned(), Input::from_source("crop/boxes"));
        let nms = op("nms", nms_inputs, &[]);
        let mut viz_inputs = BTreeMap::new();
        viz_inputs.insert("b".to_owned(), Input::from_source("crop/boxes"));
        let viz = op("viz", viz_inputs, &[]);

        let routing = Routing::build(&[crop, nms, viz], &harness.node).unwrap();
        assert_eq!(routing.outputs_of(0), vec![DataId::new("boxes").unwrap()]);
        assert!(routing.outputs_of(1).is_empty());
    }
}
