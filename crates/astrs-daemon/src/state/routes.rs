//! [`RouteTable`] — who receives what, and over which plane.
//!
//! One entry per graph edge: a producer port ([`astrs_wire::PortRef`]) mapped
//! to the consumers subscribed to it, each carrying the
//! [`astrs_wire::InputSpec`] that names its queue size, policy, priority lane
//! and deadline (§11.2). The daemon's local reliable path
//! ([`crate::local::router`]) is a lookup in this table followed by a push
//! into each consumer's queue.
//!
//! # Planes
//!
//! Stage 1 delivers everything over [`DeliveryPlane::Local`] — the
//! daemon-mediated reliable path, which is also the slow-start plane every
//! route begins on (§6.3). The enum exists now, with the variants stage 2
//! fills in, so adding shared memory and remote daemons is an added match arm
//! rather than a reshaped table:
//!
//! | Plane | Stage | Payload path |
//! |---|---|---|
//! | [`DeliveryPlane::Local`] | 1 | producer → daemon → consumer, as `NodeEvent::Input` |
//! | [`DeliveryPlane::Shm`] | 2 | producer → ring → consumer; the daemon only brokers the upgrade |
//! | [`DeliveryPlane::Remote`] | 2 | producer → daemon → peer daemon → consumer |
//!
//! # Virtual sources
//!
//! `astrs/timer/hz/50`, `astrs/logs/warn`, `astrs/status` (§8.4) are edges
//! whose producer is the daemon itself. They ride the same table, under the
//! reserved producer node `astrs`, with the source's path segments joined by
//! `.` because [`astrs_wire::DataId`] admits `.` and not `/`
//! ([`virtual_port_ref`]). One table, one lookup, no special case in the
//! router.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::state::{DeliveryPlane, RouteTable, virtual_port_ref};
//! use astrs_wire::{DataId, InputSpec, NodeId, PortRef};
//!
//! let mut routes = RouteTable::new();
//! let camera_image = PortRef::from_parts("camera", "image")?;
//! routes.insert(
//!     NodeId::new("detect")?,
//!     InputSpec::new(DataId::new("frames")?, camera_image.clone()),
//! );
//!
//! assert_eq!(routes.consumers(&camera_image).len(), 1);
//! assert_eq!(routes.consumers(&camera_image)[0].plane, DeliveryPlane::Local);
//!
//! let tick = virtual_port_ref("astrs/timer/hz/50")?;
//! assert_eq!(tick.to_string(), "astrs/timer.hz.50");
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use std::collections::{BTreeMap, BTreeSet};

use astrs_wire::{DaemonId, DataId, IdError, InputSpec, NodeId, PortRef};

/// The reserved producer node id every virtual source lives under (§8.4).
///
/// Re-exported from `astrs-wire`, which owns the encoding: the coordinator
/// writes these port references into the [`astrs_wire::NodeSpawnSpec`]s it
/// dispatches and this daemon reads them back, so one definition serves both
/// ends of the wire (see [`astrs_wire::common::virtual_source`]).
pub use astrs_wire::VIRTUAL_NODE;

/// Where a route's payload actually travels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeliveryPlane {
    /// Daemon-mediated, same host — the reliable path every route starts on.
    #[default]
    Local,
    /// A shared-memory ring the producer publishes into directly (stage 2).
    Shm {
        /// The segment the consumer attaches to.
        segment: String,
        /// The producer incarnation that owns it (§12).
        generation: u64,
    },
    /// A consumer on another machine, reached through its daemon (stage 2).
    Remote {
        /// The daemon hosting the consumer.
        daemon: DaemonId,
    },
}

impl DeliveryPlane {
    /// Whether the daemon copies the payload on this plane.
    #[must_use]
    pub const fn is_daemon_mediated(&self) -> bool {
        matches!(self, Self::Local | Self::Remote { .. })
    }

    /// Whether this consumer is a process on *this* machine.
    ///
    /// True for [`Self::Local`] and [`Self::Shm`] — both name a node the
    /// daemon holds state and a mailbox for — and false for [`Self::Remote`],
    /// whose node belongs to another daemon entirely. The distinction matters
    /// wherever a control event is queued: an `InputClosed` for a
    /// shared-memory consumer still travels through its mailbox (the ring
    /// carries payloads, not closures), while the same event for a remote
    /// consumer travels as an [`astrs_wire::PeerEvent::OutputClosed`] and must
    /// not be queued locally for a node that does not exist here.
    #[must_use]
    pub const fn is_local(&self) -> bool {
        matches!(self, Self::Local | Self::Shm { .. })
    }

    /// Whether each published payload is copied into this consumer's local
    /// mailbox.
    ///
    /// Only [`Self::Local`]: a shared-memory consumer reads the ring itself
    /// (§6.2) and a remote one is reached over the peer leg (§6.4), so both
    /// are *bypassed* by the local fan-out even though the daemon is still
    /// involved in each.
    #[must_use]
    pub const fn is_local_mailbox(&self) -> bool {
        matches!(self, Self::Local)
    }

    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Shm { .. } => "shm",
            Self::Remote { .. } => "remote",
        }
    }
}

/// One consumer of one producer port.
#[derive(Debug, Clone, PartialEq)]
pub struct Consumer {
    /// The node receiving the messages.
    pub node: NodeId,
    /// Its input specification — queue size, policy, lane, deadline.
    pub spec: InputSpec,
    /// Which plane this route currently uses.
    pub plane: DeliveryPlane,
}

impl Consumer {
    /// A consumer on the daemon-mediated plane.
    #[must_use]
    pub fn local(node: NodeId, spec: InputSpec) -> Self {
        Self {
            node,
            spec,
            plane: DeliveryPlane::Local,
        }
    }

    /// The input this consumer receives on.
    #[must_use]
    pub const fn input(&self) -> &DataId {
        &self.spec.id
    }

    /// The producer port it is subscribed to.
    #[must_use]
    pub const fn source(&self) -> &PortRef {
        &self.spec.source
    }
}

/// Every edge of one dataflow, indexed by producer port.
#[derive(Debug, Default, Clone)]
pub struct RouteTable {
    /// Producer port → consumers, in insertion order per port.
    by_source: BTreeMap<PortRef, Vec<Consumer>>,
}

impl RouteTable {
    /// An empty table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            by_source: BTreeMap::new(),
        }
    }

    /// Adds a consumer, replacing any previous entry for the same
    /// `(node, input)` pair.
    ///
    /// Replacing rather than duplicating is what makes a re-registration
    /// idempotent: a node that reconnects and subscribes again must not end up
    /// receiving every message twice.
    pub fn insert(&mut self, node: NodeId, spec: InputSpec) {
        self.insert_on(node, spec, DeliveryPlane::Local);
    }

    /// Adds a consumer on an explicit plane.
    pub fn insert_on(&mut self, node: NodeId, spec: InputSpec, plane: DeliveryPlane) {
        let source = spec.source.clone();
        let consumers = self.by_source.entry(source).or_default();
        let consumer = Consumer { node, spec, plane };
        match consumers
            .iter_mut()
            .find(|existing| existing.node == consumer.node && existing.spec.id == consumer.spec.id)
        {
            Some(existing) => *existing = consumer,
            None => consumers.push(consumer),
        }
    }

    /// The consumers of `source`.
    #[must_use]
    pub fn consumers(&self, source: &PortRef) -> &[Consumer] {
        self.by_source.get(source).map_or(&[], Vec::as_slice)
    }

    /// Whether anything consumes `source`.
    #[must_use]
    pub fn has_consumers(&self, source: &PortRef) -> bool {
        !self.consumers(source).is_empty()
    }

    /// Every producer port with at least one consumer.
    pub fn sources(&self) -> impl Iterator<Item = &PortRef> {
        self.by_source.keys()
    }

    /// Every route in the table.
    pub fn all(&self) -> impl Iterator<Item = (&PortRef, &Consumer)> {
        self.by_source
            .iter()
            .flat_map(|(source, consumers)| consumers.iter().map(move |c| (source, c)))
    }

    /// How many routes the table holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_source.values().map(Vec::len).sum()
    }

    /// Whether the table holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_source.values().all(Vec::is_empty)
    }

    /// The ports `node` produces on that anybody consumes.
    #[must_use]
    pub fn produced_by(&self, node: &NodeId) -> Vec<&PortRef> {
        self.by_source
            .keys()
            .filter(|source| source.node() == node)
            .collect()
    }

    /// The producer ports `node` consumes from.
    #[must_use]
    pub fn consumed_by(&self, node: &NodeId) -> Vec<&PortRef> {
        self.by_source
            .iter()
            .filter(|(_, consumers)| consumers.iter().any(|consumer| consumer.node == *node))
            .map(|(source, _)| source)
            .collect()
    }

    /// Every node that consumes anything `node` produces — the audience for a
    /// `NodeFailed` or `Restarted` fan-out (§12).
    #[must_use]
    pub fn downstream_of(&self, node: &NodeId) -> BTreeSet<NodeId> {
        self.by_source
            .iter()
            .filter(|(source, _)| source.node() == node)
            .flat_map(|(_, consumers)| consumers.iter().map(|consumer| consumer.node.clone()))
            .collect()
    }

    /// Every node whose output `node` consumes.
    #[must_use]
    pub fn upstream_of(&self, node: &NodeId) -> BTreeSet<NodeId> {
        self.by_source
            .iter()
            .filter(|(_, consumers)| consumers.iter().any(|consumer| consumer.node == *node))
            .map(|(source, _)| source.node().clone())
            .filter(|producer| producer.as_str() != VIRTUAL_NODE)
            .collect()
    }

    /// Changes the plane a route uses — the seam stage 2's route upgrade
    /// writes through (§6.3).
    ///
    /// Returns whether the route was found.
    pub fn set_plane(&mut self, source: &PortRef, node: &NodeId, plane: DeliveryPlane) -> bool {
        let Some(consumers) = self.by_source.get_mut(source) else {
            return false;
        };
        match consumers.iter_mut().find(|consumer| consumer.node == *node) {
            Some(consumer) => {
                consumer.plane = plane;
                true
            }
            None => false,
        }
    }

    /// Removes every route a node takes part in, as consumer or producer.
    pub fn remove_node(&mut self, node: &NodeId) {
        self.by_source.retain(|source, _| source.node() != node);
        for consumers in self.by_source.values_mut() {
            consumers.retain(|consumer| consumer.node != *node);
        }
        self.by_source.retain(|_, consumers| !consumers.is_empty());
    }

    /// Removes one consumer's subscription.
    pub fn remove_consumer(&mut self, node: &NodeId, input: &DataId) -> bool {
        let mut removed = false;
        for consumers in self.by_source.values_mut() {
            let before = consumers.len();
            consumers.retain(|consumer| !(consumer.node == *node && consumer.spec.id == *input));
            removed |= consumers.len() != before;
        }
        self.by_source.retain(|_, consumers| !consumers.is_empty());
        removed
    }

    /// Every virtual source anybody subscribes to.
    #[must_use]
    pub fn virtual_sources(&self) -> Vec<&PortRef> {
        self.by_source
            .keys()
            .filter(|source| is_virtual_port(source))
            .collect()
    }
}

/// The [`PortRef`] a virtual source string maps to (§8.4).
///
/// `astrs/timer/hz/50` becomes `astrs` / `timer.hz.50`: the `astrs/` prefix
/// becomes the reserved producer node, and the remaining `/`-separated
/// segments join with `.` because [`DataId`] admits `.` and not `/`.
///
/// Delegates to [`astrs_wire::virtual_port_ref`], which is where the encoding
/// lives so that the coordinator (which writes these into spawn specs) and
/// this daemon (which reads them back) cannot drift apart.
///
/// # Errors
///
/// [`IdError`] if the resulting port id is not a legal [`DataId`] — which for
/// a source that [`astrs_manifest::recognize_virtual_source`] accepted cannot
/// happen, and is therefore returned rather than asserted.
pub fn virtual_port_ref(source: &str) -> Result<PortRef, IdError> {
    astrs_wire::virtual_port_ref(source)
}

/// Whether a port belongs to the reserved virtual producer.
///
/// Delegates to [`astrs_wire::is_virtual_port`] — see [`virtual_port_ref`].
#[must_use]
pub fn is_virtual_port(port: &PortRef) -> bool {
    astrs_wire::is_virtual_port(port)
}

/// The virtual-source string a [`PortRef`] came from, for diagnostics.
///
/// Delegates to [`astrs_wire::virtual_source_text`] — see
/// [`virtual_port_ref`].
#[must_use]
pub fn virtual_source_text(port: &PortRef) -> String {
    astrs_wire::virtual_source_text(port)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_wire::{PriorityLane, QueuePolicy};

    use super::*;

    fn port(node: &str, output: &str) -> PortRef {
        PortRef::from_parts(node, output).unwrap()
    }

    fn node(name: &str) -> NodeId {
        NodeId::new(name).unwrap()
    }

    fn data(name: &str) -> DataId {
        DataId::new(name).unwrap()
    }

    fn table() -> RouteTable {
        let mut routes = RouteTable::new();
        routes.insert(
            node("detect"),
            InputSpec::new(data("frames"), port("camera", "image")),
        );
        routes.insert(
            node("record"),
            InputSpec::new(data("raw"), port("camera", "image")),
        );
        routes.insert(
            node("plan"),
            InputSpec::new(data("boxes"), port("detect", "detections")),
        );
        routes
    }

    #[test]
    fn an_empty_table_finds_nothing() {
        let routes = RouteTable::new();
        assert!(routes.is_empty());
        assert_eq!(routes.len(), 0);
        assert!(routes.consumers(&port("camera", "image")).is_empty());
        assert!(!routes.has_consumers(&port("camera", "image")));
        assert!(routes.downstream_of(&node("camera")).is_empty());
    }

    #[test]
    fn consumers_are_found_by_producer_port() {
        let routes = table();
        assert_eq!(routes.len(), 3);
        let consumers = routes.consumers(&port("camera", "image"));
        assert_eq!(consumers.len(), 2);
        let names: Vec<&str> = consumers.iter().map(|c| c.node.as_str()).collect();
        assert_eq!(names, ["detect", "record"]);
        assert_eq!(consumers[0].input(), &data("frames"));
        assert_eq!(consumers[0].source(), &port("camera", "image"));
    }

    #[test]
    fn a_repeated_subscription_replaces_rather_than_duplicates() {
        let mut routes = table();
        let spec = InputSpec::new(data("frames"), port("camera", "image"))
            .with_queue(99, QueuePolicy::Backpressure);
        routes.insert(node("detect"), spec);

        let consumers = routes.consumers(&port("camera", "image"));
        assert_eq!(consumers.len(), 2, "still two consumers");
        let detect = consumers
            .iter()
            .find(|c| c.node == node("detect"))
            .expect("present");
        assert_eq!(detect.spec.queue_size, 99);
        assert_eq!(detect.spec.queue_policy, QueuePolicy::Backpressure);
    }

    #[test]
    fn routes_default_to_the_daemon_mediated_plane() {
        let routes = table();
        for (_, consumer) in routes.all() {
            assert_eq!(consumer.plane, DeliveryPlane::Local);
            assert!(consumer.plane.is_daemon_mediated());
        }
        assert_eq!(DeliveryPlane::default(), DeliveryPlane::Local);
    }

    #[test]
    fn the_three_plane_predicates_classify_every_plane() {
        let local = DeliveryPlane::Local;
        let shm = DeliveryPlane::Shm {
            segment: "seg-1".into(),
            generation: 1,
        };
        let remote = DeliveryPlane::Remote {
            daemon: DaemonId::generate(None),
        };

        // Delivered into a local mailbox: only the reliable daemon path.
        assert!(local.is_local_mailbox());
        assert!(!shm.is_local_mailbox());
        assert!(!remote.is_local_mailbox());

        // A process on this machine: the two same-host planes.
        assert!(local.is_local());
        assert!(shm.is_local(), "a ring's consumer is still a local process");
        assert!(
            !remote.is_local(),
            "a remote consumer has no state, and no mailbox, on this daemon"
        );

        // The daemon copies the payload: local, and remote (onto the wire).
        assert!(local.is_daemon_mediated());
        assert!(!shm.is_daemon_mediated());
        assert!(remote.is_daemon_mediated());
    }

    #[test]
    fn a_remote_consumer_is_a_first_class_route_table_entry() {
        let mut routes = table();
        let peer = DaemonId::generate(None);
        routes.insert_on(
            node("far"),
            InputSpec::new(data("frames"), port("camera", "image")),
            DeliveryPlane::Remote {
                daemon: peer.clone(),
            },
        );
        // Presence is the point: without it the producer's output has no
        // consumers, so nothing tells the peer when the producer finishes.
        assert!(
            routes
                .produced_by(&node("camera"))
                .contains(&&port("camera", "image"))
        );
        let consumers = routes.consumers(&port("camera", "image"));
        let far = consumers.iter().find(|c| c.node == node("far")).unwrap();
        assert_eq!(far.plane, DeliveryPlane::Remote { daemon: peer });
        assert!(!far.plane.is_local());
    }

    #[test]
    fn a_plane_can_be_upgraded_in_place() {
        let mut routes = table();
        let upgraded = DeliveryPlane::Shm {
            segment: "seg-1".into(),
            generation: 4,
        };
        assert!(routes.set_plane(&port("camera", "image"), &node("detect"), upgraded.clone()));

        let consumers = routes.consumers(&port("camera", "image"));
        let detect = consumers.iter().find(|c| c.node == node("detect")).unwrap();
        assert_eq!(detect.plane, upgraded);
        assert!(!detect.plane.is_daemon_mediated());
        assert_eq!(detect.plane.kind_name(), "shm");

        assert!(!routes.set_plane(
            &port("camera", "image"),
            &node("missing"),
            DeliveryPlane::Local
        ));
        assert!(!routes.set_plane(
            &port("nobody", "out"),
            &node("detect"),
            DeliveryPlane::Local
        ));
    }

    #[test]
    fn a_remote_plane_names_its_daemon() {
        let daemon = DaemonId::generate(None);
        let plane = DeliveryPlane::Remote {
            daemon: daemon.clone(),
        };
        assert!(plane.is_daemon_mediated());
        assert_eq!(plane.kind_name(), "remote");
        assert_eq!(plane, DeliveryPlane::Remote { daemon });
    }

    #[test]
    fn the_downstream_audience_is_every_consumer_of_every_output() {
        let routes = table();
        assert_eq!(
            routes.downstream_of(&node("camera")),
            BTreeSet::from([node("detect"), node("record")])
        );
        assert_eq!(
            routes.downstream_of(&node("detect")),
            BTreeSet::from([node("plan")])
        );
        assert!(routes.downstream_of(&node("plan")).is_empty());
    }

    #[test]
    fn the_upstream_set_skips_virtual_producers() {
        let mut routes = table();
        routes.insert(
            node("plan"),
            InputSpec::new(data("tick"), virtual_port_ref("astrs/timer/hz/50").unwrap()),
        );
        assert_eq!(
            routes.upstream_of(&node("plan")),
            BTreeSet::from([node("detect")]),
            "the daemon is not a graph peer"
        );
    }

    #[test]
    fn produced_and_consumed_ports_are_listed() {
        let routes = table();
        assert_eq!(
            routes.produced_by(&node("camera")),
            [&port("camera", "image")]
        );
        assert_eq!(
            routes.consumed_by(&node("detect")),
            [&port("camera", "image")]
        );
        assert!(routes.produced_by(&node("plan")).is_empty());
    }

    #[test]
    fn removing_a_node_removes_it_from_both_sides() {
        let mut routes = table();
        routes.remove_node(&node("detect"));
        assert!(
            routes.consumers(&port("detect", "detections")).is_empty(),
            "its outputs are gone"
        );
        let camera = routes.consumers(&port("camera", "image"));
        assert_eq!(camera.len(), 1);
        assert_eq!(camera[0].node, node("record"));
    }

    #[test]
    fn removing_one_subscription_leaves_the_others() {
        let mut routes = table();
        assert!(routes.remove_consumer(&node("detect"), &data("frames")));
        assert!(!routes.remove_consumer(&node("detect"), &data("frames")));
        assert_eq!(routes.consumers(&port("camera", "image")).len(), 1);
        assert_eq!(routes.len(), 2);
    }

    #[test]
    fn empty_source_entries_are_pruned() {
        let mut routes = RouteTable::new();
        routes.insert(
            node("a"),
            InputSpec::new(data("in"), port("producer", "out")),
        );
        routes.remove_consumer(&node("a"), &data("in"));
        assert_eq!(routes.sources().count(), 0);
        assert!(routes.is_empty());
    }

    #[test]
    fn virtual_sources_map_to_the_reserved_producer() {
        for (source, expected) in [
            ("astrs/timer/hz/50", "astrs/timer.hz.50"),
            ("astrs/timer/millis/100", "astrs/timer.millis.100"),
            ("astrs/logs", "astrs/logs"),
            ("astrs/logs/warn", "astrs/logs.warn"),
            ("astrs/logs/warn/camera", "astrs/logs.warn.camera"),
            ("astrs/status", "astrs/status"),
        ] {
            let port = virtual_port_ref(source).unwrap();
            assert_eq!(port.to_string(), expected, "{source}");
            assert!(is_virtual_port(&port));
            assert_eq!(virtual_source_text(&port), source);
        }
    }

    #[test]
    fn an_ordinary_port_is_not_virtual() {
        assert!(!is_virtual_port(&port("camera", "image")));
    }

    #[test]
    fn virtual_subscriptions_are_listed_separately() {
        let mut routes = table();
        let tick = virtual_port_ref("astrs/timer/hz/10").unwrap();
        let mut spec = InputSpec::new(data("tick"), tick.clone());
        spec.priority_lane = PriorityLane::Control;
        routes.insert(node("plan"), spec);

        assert_eq!(routes.virtual_sources(), [&tick]);
        assert_eq!(routes.consumers(&tick).len(), 1);
        assert_eq!(
            routes.consumers(&tick)[0].spec.priority_lane,
            PriorityLane::Control
        );
    }

    #[test]
    fn a_source_without_the_prefix_still_maps_cleanly() {
        // Defensive: the recognizer already guarantees the prefix, but a
        // caller that strips it first must not produce a different port.
        assert_eq!(
            virtual_port_ref("timer/hz/50").unwrap(),
            virtual_port_ref("astrs/timer/hz/50").unwrap()
        );
    }
}
