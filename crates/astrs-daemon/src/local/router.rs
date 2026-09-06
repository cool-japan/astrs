//! The local reliable path — daemon-mediated node→node delivery (§4.3, §6.3).
//!
//! > *Data messages do **not** transit the daemon on the happy path: nodes
//! > publish directly into SHM rings (same host) or hand off to the daemon
//! > only for cross-host egress. The daemon is a control-plane actor plus the
//! > *reliability* path (slow-start handshake, §6.3).*
//!
//! This module is that reliability path. Every route begins here — it is the
//! *slow start* of the slow-start handshake — and stays here until stage 2's
//! broker upgrades it to shared memory. It must therefore be correct before it
//! is fast, and fast enough that "correct" is not a synonym for "slow": one
//! producer message becomes one [`astrs_wire::NodeEvent::Input`] per consumer,
//! each pushed into that consumer's [`crate::local::NodeMailbox`], with the
//! payload cloned exactly once per consumer and never re-encoded.
//!
//! ```text
//!   camera ──SendMessage{output: image, payload}──► daemon
//!                                                     │ RouteTable lookup
//!                    ┌────────────────────────────────┴──────────────┐
//!                    ▼                                               ▼
//!   detect ◄──Input{id: frames, source: camera/image}   record ◄──Input{id: raw, …}
//! ```
//!
//! # What a fan-out costs
//!
//! One [`Vec<u8>`] clone per consumer, and nothing else: the metadata is
//! cloned (it is small and owned), the [`astrs_wire::PortRef`] is cloned, and
//! the payload bytes are cloned once per destination because each destination
//! owns its queued event. A single-consumer route — by far the common case —
//! moves the payload with no clone at all
//! ([`LocalRouter::deliver`] hands the last consumer the original `Vec`).
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::local::{LocalRouter, NodeMailbox, PayloadOrigin};
//! use astrs_daemon::state::RouteTable;
//! use astrs_wire::{DataId, InputSpec, Metadata, NodeId, PortRef};
//! use std::collections::BTreeMap;
//!
//! let mut routes = RouteTable::new();
//! let image = PortRef::from_parts("camera", "image")?;
//! routes.insert(NodeId::new("detect")?, InputSpec::new(DataId::new("frames")?, image.clone()));
//!
//! let mut mailboxes: BTreeMap<NodeId, NodeMailbox> = BTreeMap::new();
//! mailboxes.insert(NodeId::new("detect")?, NodeMailbox::new());
//!
//! let outcome = LocalRouter::deliver(
//!     &routes, &mut mailboxes, &image, Metadata::default(), vec![1, 2, 3],
//!     PayloadOrigin::Inline,
//! );
//! assert_eq!(outcome.delivered, 1);
//! assert_eq!(outcome.dropped, 0);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Why a fan-out has to know where the bytes came from
//!
//! An upgraded route (§6.3) is not *only* a ring. §6.2 keeps the threshold
//! rule — *"a heap payload ≥ threshold is copied once into a slot; below
//! threshold it rides the UDS control channel"* — so a producer whose output
//! is on the shared-memory plane still publishes its small messages through
//! the daemon. Those bytes are in no ring, and a consumer reading the ring
//! will never see them.
//!
//! So "is this consumer on the shared-memory plane?" is the wrong question for
//! a fan-out to ask. The right one is [`PayloadOrigin`]: bytes that came *out
//! of* a ring ([`PayloadOrigin::Ring`], the daemon's bridge draining a slot
//! reference) are already in the hands of every consumer attached to it, and
//! delivering them again would duplicate; bytes that arrived *inline*
//! ([`PayloadOrigin::Inline`]) are in nobody's hands yet and go to everyone.

use std::collections::BTreeMap;

use astrs_wire::{DataId, Metadata, NodeEvent, NodeId, PortRef, RouteCloseReason};

use crate::local::mailbox::{DeliveryReport, NodeMailbox};
use crate::state::routes::{Consumer, DeliveryPlane, RouteTable};

/// Where the bytes of one fan-out came from.
///
/// The fact — not the policy — that decides whether a consumer on the
/// shared-memory plane already holds this message. See the module
/// documentation for why the consumer's plane alone cannot answer that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PayloadOrigin {
    /// The producer sent the bytes on the control channel: §6.2's
    /// below-threshold rule, or a §6.2 fallback after a full ring. Nobody has
    /// them yet, so every consumer gets a copy — including one whose route is
    /// otherwise on the ring.
    Inline,
    /// The bytes were read out of the producer's ring by the daemon's own
    /// bridge. Every consumer attached to that ring already has them.
    Ring,
}

impl PayloadOrigin {
    /// Whether consumers attached to the producer's ring already hold these
    /// bytes.
    #[must_use]
    pub const fn already_in_ring(self) -> bool {
        matches!(self, Self::Ring)
    }

    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Ring => "ring",
        }
    }
}

impl core::fmt::Display for PayloadOrigin {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one fan-out did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FanOutOutcome {
    /// How many consumers queued the message.
    pub delivered: usize,
    /// How many refused it (a full backpressure buffer).
    pub dropped: usize,
    /// How many are on a plane that bypasses the daemon (stage 2).
    pub bypassed: usize,
    /// The per-consumer reports worth logging or metering.
    pub signals: Vec<(NodeId, DataId, DeliveryReport)>,
}

impl FanOutOutcome {
    /// How many consumers this message was offered to.
    #[must_use]
    pub const fn attempted(&self) -> usize {
        self.delivered + self.dropped
    }

    /// Whether the message reached nobody at all.
    #[must_use]
    pub const fn is_orphaned(&self) -> bool {
        self.delivered == 0 && self.dropped == 0 && self.bypassed == 0
    }

    /// Whether anything was lost.
    #[must_use]
    pub fn lost_anything(&self) -> bool {
        self.dropped > 0
            || self
                .signals
                .iter()
                .any(|(_, _, report)| report.dropped_something())
    }
}

/// The daemon-mediated fan-out.
///
/// A unit struct rather than a stateful actor: every fact it needs is in the
/// [`RouteTable`] and the mailboxes, and keeping it stateless is what lets the
/// event loop call it from the one task that already owns both.
#[derive(Debug, Clone, Copy)]
pub struct LocalRouter;

impl LocalRouter {
    /// Fans `payload` out to every local consumer of `source` that does not
    /// already hold it.
    ///
    /// The payload is moved into the last recipient's event and cloned for the
    /// others, so a single-consumer route — the common case — costs no clone
    /// at all.
    ///
    /// `origin` says where the bytes came from, which is what decides whether
    /// a consumer on the shared-memory plane is a recipient: bytes drained out
    /// of the producer's ring ([`PayloadOrigin::Ring`]) are already in its
    /// hands, and bytes that arrived inline ([`PayloadOrigin::Inline`], §6.2's
    /// below-threshold rule) are not.
    pub fn deliver(
        routes: &RouteTable,
        mailboxes: &mut BTreeMap<NodeId, NodeMailbox>,
        source: &PortRef,
        metadata: Metadata,
        payload: Vec<u8>,
        origin: PayloadOrigin,
    ) -> FanOutOutcome {
        let consumers = routes.consumers(source);
        let mut outcome = FanOutOutcome::default();
        if consumers.is_empty() {
            return outcome;
        }

        // A consumer reading the ring is counted, not delivered to — but only
        // when these bytes went through that ring. An inline publish on an
        // upgraded route reached no ring at all, so skipping it here would
        // lose the message outright (§6.2's below-threshold rule).
        let mediated: Vec<&Consumer> = consumers
            .iter()
            .filter(|consumer| {
                if consumer.plane.is_daemon_mediated() || !origin.already_in_ring() {
                    true
                } else {
                    outcome.bypassed += 1;
                    false
                }
            })
            .collect();

        let last = mediated.len().saturating_sub(1);
        let mut payload = Some(payload);
        for (index, consumer) in mediated.iter().enumerate() {
            let bytes = if index == last {
                payload.take().unwrap_or_default()
            } else {
                payload.as_ref().cloned().unwrap_or_default()
            };
            let event = NodeEvent::Input {
                id: consumer.spec.id.clone(),
                source: source.clone(),
                metadata: metadata.clone(),
                payload: bytes,
            };
            Self::push_to(mailboxes, consumer, event, &mut outcome);
        }
        outcome
    }

    /// Tells every consumer of `source` that it will receive nothing further.
    pub fn close(
        routes: &RouteTable,
        mailboxes: &mut BTreeMap<NodeId, NodeMailbox>,
        source: &PortRef,
        reason: RouteCloseReason,
    ) -> FanOutOutcome {
        let mut outcome = FanOutOutcome::default();
        for consumer in routes.consumers(source) {
            let event = NodeEvent::InputClosed {
                id: consumer.spec.id.clone(),
                source: source.clone(),
                reason: reason.clone(),
            };
            Self::push_to(mailboxes, consumer, event, &mut outcome);
        }
        outcome
    }

    /// Tells every consumer of `source` that its producer came back (§12).
    pub fn recover(
        routes: &RouteTable,
        mailboxes: &mut BTreeMap<NodeId, NodeMailbox>,
        source: &PortRef,
        generation: u64,
    ) -> FanOutOutcome {
        let mut outcome = FanOutOutcome::default();
        for consumer in routes.consumers(source) {
            let event = NodeEvent::InputRecovered {
                id: consumer.spec.id.clone(),
                source: source.clone(),
                generation,
            };
            Self::push_to(mailboxes, consumer, event, &mut outcome);
        }
        outcome
    }

    /// Sends one event to one node, if it has a mailbox.
    ///
    /// The event is addressed to `input`, which for a control event that is
    /// not about a particular input (a `Stop`, an `AllInputsClosed`) is the
    /// synthetic `astrs.status` port — one queue, one lane, one ordering.
    pub fn send_to_node(
        mailboxes: &mut BTreeMap<NodeId, NodeMailbox>,
        node: &NodeId,
        input: &DataId,
        event: NodeEvent,
    ) -> Option<DeliveryReport> {
        mailboxes
            .get_mut(node)
            .map(|mailbox| mailbox.push(input, event))
    }

    /// Broadcasts one event to a set of nodes on their `astrs.status` port.
    ///
    /// The `NodeFailed` / `Restarted` fan-out (§12): a peer that needs to know
    /// a producer died gets told on the same queue as every other lifecycle
    /// event, so the ordering between "your input closed" and "the node behind
    /// it failed" is well defined.
    pub fn broadcast_status<'a, I>(
        mailboxes: &mut BTreeMap<NodeId, NodeMailbox>,
        nodes: I,
        event: &NodeEvent,
    ) -> usize
    where
        I: IntoIterator<Item = &'a NodeId>,
    {
        let status = crate::local::status_port();
        let mut delivered = 0;
        for node in nodes {
            if let Some(mailbox) = mailboxes.get_mut(node)
                && mailbox.push(&status, event.clone()).accepted()
            {
                delivered += 1;
            }
        }
        delivered
    }

    /// Pushes one event, recording the outcome.
    fn push_to(
        mailboxes: &mut BTreeMap<NodeId, NodeMailbox>,
        consumer: &Consumer,
        event: NodeEvent,
        outcome: &mut FanOutOutcome,
    ) {
        let Some(mailbox) = mailboxes.get_mut(&consumer.node) else {
            // A consumer with no mailbox has not registered yet. Its route
            // exists (the graph declared it) but there is nowhere to put the
            // message, which is the same situation as a full queue as far as
            // accounting is concerned.
            outcome.dropped += 1;
            return;
        };
        // Register lazily from the specification so the queue gets the
        // manifest's size, policy and lane rather than the fallback defaults.
        let _ = mailbox.register(&consumer.spec);
        let report = mailbox.push(&consumer.spec.id, event);
        if report.accepted() {
            outcome.delivered += 1;
        } else {
            outcome.dropped += 1;
        }
        if report.signal.is_some() || report.dropped_something() {
            outcome
                .signals
                .push((consumer.node.clone(), consumer.spec.id.clone(), report));
        }
    }

    /// The plane a route currently uses, for a diagnostic.
    #[must_use]
    pub fn plane_of(routes: &RouteTable, source: &PortRef, node: &NodeId) -> Option<DeliveryPlane> {
        routes
            .consumers(source)
            .iter()
            .find(|consumer| consumer.node == *node)
            .map(|consumer| consumer.plane.clone())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_wire::{InputSpec, QueuePolicy};

    use super::*;

    fn node(name: &str) -> NodeId {
        NodeId::new(name).unwrap()
    }

    fn data(name: &str) -> DataId {
        DataId::new(name).unwrap()
    }

    fn image() -> PortRef {
        PortRef::from_parts("camera", "image").unwrap()
    }

    fn routes() -> RouteTable {
        let mut routes = RouteTable::new();
        routes.insert(node("detect"), InputSpec::new(data("frames"), image()));
        routes.insert(node("record"), InputSpec::new(data("raw"), image()));
        routes
    }

    fn mailboxes(names: &[&str]) -> BTreeMap<NodeId, NodeMailbox> {
        names
            .iter()
            .map(|name| (node(name), NodeMailbox::new()))
            .collect()
    }

    #[test]
    fn a_message_reaches_every_local_consumer() {
        let routes = routes();
        let mut mailboxes = mailboxes(&["detect", "record"]);
        let outcome = LocalRouter::deliver(
            &routes,
            &mut mailboxes,
            &image(),
            Metadata::default(),
            vec![1, 2, 3],
            PayloadOrigin::Inline,
        );

        assert_eq!(outcome.delivered, 2);
        assert_eq!(outcome.dropped, 0);
        assert_eq!(outcome.attempted(), 2);
        assert!(!outcome.is_orphaned());
        assert!(!outcome.lost_anything());

        for (name, input) in [("detect", "frames"), ("record", "raw")] {
            let mailbox = mailboxes.get(&node(name)).expect("present");
            let (id, event) = mailbox.try_recv().expect("queued");
            assert_eq!(id, data(input));
            match event {
                NodeEvent::Input {
                    id,
                    source,
                    payload,
                    ..
                } => {
                    assert_eq!(id, data(input));
                    assert_eq!(source, image());
                    assert_eq!(payload, [1, 2, 3]);
                }
                other => panic!("expected an input event, got {other}"),
            }
        }
    }

    #[test]
    fn a_message_with_no_consumers_is_orphaned_not_an_error() {
        let routes = RouteTable::new();
        let mut mailboxes = mailboxes(&["detect"]);
        let outcome = LocalRouter::deliver(
            &routes,
            &mut mailboxes,
            &image(),
            Metadata::default(),
            vec![1],
            PayloadOrigin::Inline,
        );
        assert!(outcome.is_orphaned());
        assert_eq!(outcome.delivered, 0);
    }

    #[test]
    fn a_consumer_with_no_mailbox_is_counted_as_dropped() {
        let routes = routes();
        let mut mailboxes = mailboxes(&["detect"]);
        let outcome = LocalRouter::deliver(
            &routes,
            &mut mailboxes,
            &image(),
            Metadata::default(),
            vec![1],
            PayloadOrigin::Inline,
        );
        assert_eq!(outcome.delivered, 1);
        assert_eq!(outcome.dropped, 1, "record has not registered");
    }

    /// A route table with `detect` reading `camera/image` off a ring.
    fn upgraded_routes() -> RouteTable {
        let mut routes = routes();
        routes.set_plane(
            &image(),
            &node("detect"),
            DeliveryPlane::Shm {
                segment: "seg".into(),
                generation: 1,
            },
        );
        routes
    }

    #[test]
    fn a_bypassing_plane_is_counted_but_not_delivered_to() {
        let routes = upgraded_routes();
        let mut mailboxes = mailboxes(&["detect", "record"]);
        let outcome = LocalRouter::deliver(
            &routes,
            &mut mailboxes,
            &image(),
            Metadata::default(),
            vec![1],
            // Drained out of the ring: `detect` is reading that ring itself.
            PayloadOrigin::Ring,
        );
        assert_eq!(outcome.bypassed, 1);
        assert_eq!(outcome.delivered, 1);
        assert!(
            mailboxes.get(&node("detect")).unwrap().try_recv().is_none(),
            "the producer published into the ring instead"
        );
        assert_eq!(
            LocalRouter::plane_of(&routes, &image(), &node("record")),
            Some(DeliveryPlane::Local)
        );
    }

    #[test]
    fn an_inline_payload_reaches_a_consumer_on_the_ring_as_well() {
        // §6.2's below-threshold rule: a small message published by a producer
        // whose output is on the shared-memory plane never entered a ring, so
        // the consumer reading that ring will never see it — unless the daemon
        // delivers it, which is exactly what `PayloadOrigin::Inline` says.
        // Skipping it here was silent data loss the moment consumers began
        // attaching to rings by themselves.
        let routes = upgraded_routes();
        let mut mailboxes = mailboxes(&["detect", "record"]);
        let outcome = LocalRouter::deliver(
            &routes,
            &mut mailboxes,
            &image(),
            Metadata::default(),
            vec![7],
            PayloadOrigin::Inline,
        );
        assert_eq!(outcome.bypassed, 0, "no ring carried these bytes");
        assert_eq!(outcome.delivered, 2);
        let event = mailboxes
            .get_mut(&node("detect"))
            .expect("present")
            .try_recv()
            .expect("the consumer on the ring was told too");
        assert_eq!(event.1.payload_len(), 1);
    }

    #[test]
    fn the_two_origins_describe_themselves() {
        assert_eq!(PayloadOrigin::Inline.as_str(), "inline");
        assert_eq!(PayloadOrigin::Ring.to_string(), "ring");
        assert!(PayloadOrigin::Ring.already_in_ring());
        assert!(!PayloadOrigin::Inline.already_in_ring());
    }

    #[test]
    fn the_manifest_queue_configuration_is_applied_lazily() {
        let mut routes = RouteTable::new();
        routes.insert(
            node("detect"),
            InputSpec::new(data("frames"), image()).with_queue(1, QueuePolicy::DropOldest),
        );
        let mut mailboxes = mailboxes(&["detect"]);

        for index in 0..3u8 {
            LocalRouter::deliver(
                &routes,
                &mut mailboxes,
                &image(),
                Metadata::default(),
                vec![index],
                PayloadOrigin::Inline,
            );
        }
        let mailbox = mailboxes.get(&node("detect")).expect("present");
        assert_eq!(
            mailbox.depth(&data("frames")),
            Some(1),
            "queue_size honoured"
        );
    }

    #[test]
    fn an_eviction_is_reported_as_a_signal() {
        let mut routes = RouteTable::new();
        routes.insert(
            node("detect"),
            InputSpec::new(data("frames"), image()).with_queue(1, QueuePolicy::DropOldest),
        );
        let mut mailboxes = mailboxes(&["detect"]);
        LocalRouter::deliver(
            &routes,
            &mut mailboxes,
            &image(),
            Metadata::default(),
            vec![0],
            PayloadOrigin::Inline,
        );
        let outcome = LocalRouter::deliver(
            &routes,
            &mut mailboxes,
            &image(),
            Metadata::default(),
            vec![1],
            PayloadOrigin::Inline,
        );

        assert_eq!(outcome.delivered, 1);
        assert!(outcome.lost_anything());
        assert_eq!(outcome.signals.len(), 1);
        assert_eq!(outcome.signals[0].0, node("detect"));
        assert_eq!(outcome.signals[0].1, data("frames"));
    }

    #[test]
    fn closing_a_source_tells_every_consumer() {
        let routes = routes();
        let mut mailboxes = mailboxes(&["detect", "record"]);
        let outcome = LocalRouter::close(
            &routes,
            &mut mailboxes,
            &image(),
            RouteCloseReason::ProducerFinished,
        );
        assert_eq!(outcome.delivered, 2);

        let mailbox = mailboxes.get(&node("detect")).expect("present");
        match mailbox.try_recv().expect("queued").1 {
            NodeEvent::InputClosed { id, reason, .. } => {
                assert_eq!(id, data("frames"));
                assert_eq!(reason, RouteCloseReason::ProducerFinished);
            }
            other => panic!("expected an input-closed event, got {other}"),
        }
    }

    #[test]
    fn recovery_carries_the_new_generation() {
        let routes = routes();
        let mut mailboxes = mailboxes(&["detect"]);
        LocalRouter::recover(&routes, &mut mailboxes, &image(), 4);
        let mailbox = mailboxes.get(&node("detect")).expect("present");
        match mailbox.try_recv().expect("queued").1 {
            NodeEvent::InputRecovered { generation, .. } => assert_eq!(generation, 4),
            other => panic!("expected a recovery event, got {other}"),
        }
    }

    #[test]
    fn a_directed_send_reaches_one_node() {
        let mut mailboxes = mailboxes(&["detect"]);
        let report = LocalRouter::send_to_node(
            &mut mailboxes,
            &node("detect"),
            &data("frames"),
            NodeEvent::AllInputsClosed,
        );
        assert!(report.expect("delivered").accepted());
        assert!(
            LocalRouter::send_to_node(
                &mut mailboxes,
                &node("missing"),
                &data("frames"),
                NodeEvent::AllInputsClosed,
            )
            .is_none()
        );
    }

    #[test]
    fn a_status_broadcast_reaches_every_listed_node() {
        let mut mailboxes = mailboxes(&["detect", "record", "plan"]);
        let event = NodeEvent::NodeFailed {
            peer: node("camera"),
            cause: astrs_wire::NodeExitCause::ExitCode { code: 1 },
        };
        let audience = [node("detect"), node("record"), node("missing")];
        let delivered = LocalRouter::broadcast_status(&mut mailboxes, audience.iter(), &event);
        assert_eq!(delivered, 2);

        let mailbox = mailboxes.get(&node("detect")).expect("present");
        let (input, received) = mailbox.try_recv().expect("queued");
        assert_eq!(input, crate::local::status_port());
        assert!(received.is_fault());
        assert!(mailboxes.get(&node("plan")).unwrap().try_recv().is_none());
    }

    #[test]
    fn a_single_consumer_route_moves_the_payload() {
        let mut routes = RouteTable::new();
        routes.insert(node("detect"), InputSpec::new(data("frames"), image()));
        let mut mailboxes = mailboxes(&["detect"]);
        let payload = vec![9u8; 1024];
        let outcome = LocalRouter::deliver(
            &routes,
            &mut mailboxes,
            &image(),
            Metadata::default(),
            payload.clone(),
            PayloadOrigin::Inline,
        );
        assert_eq!(outcome.delivered, 1);

        let mailbox = mailboxes.get(&node("detect")).expect("present");
        match mailbox.try_recv().expect("queued").1 {
            NodeEvent::Input { payload: got, .. } => assert_eq!(got, payload),
            other => panic!("expected an input event, got {other}"),
        }
    }
}
