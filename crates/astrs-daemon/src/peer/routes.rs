//! [`PeerRouteTable`] — every cross-host edge, in both directions (§6.4).
//!
//! One entry per `RouteSetup`/`RouteAccept` exchange, filed by the peer it
//! belongs to and the handle that names it. The table is pure state: it has no
//! sockets, no tasks and no clock, so a whole route conversation can be
//! replayed against it synchronously — which is what the peer-to-peer tests do
//! before they ever open a socket.
//!
//! # Two directions, one table
//!
//! ```text
//!   producer's daemon                      consumer's daemon
//!   ─────────────────                      ─────────────────
//!   open()  ──── RouteSetup ────────────►  admit()
//!               (outbound, Pending)        (inbound, Established)
//!   settle() ◄── RouteAccept ────────────
//!               (outbound, Established)
//!
//!   next_seq() ─ Output ────────────────►  deliver
//!   close()    ─ OutputClosed ──────────►  InputClosed
//!              ─ RouteTeardown ─────────►  forget
//! ```
//!
//! The handle is minted by the *producer's* daemon and is unique on that
//! connection ([`astrs_wire::PeerEvent::RouteSetup`]). Both sides therefore
//! file a route under the same `(peer, handle)` pair without a second
//! negotiation, and a handle collision is impossible because each side only
//! ever mints for its own outbound half.
//!
//! # Sequence numbers
//!
//! Each established outbound route carries a counter starting at zero
//! ([`astrs_wire::PeerEvent::Output::seq`]) so a consumer can see a gap left by
//! a dropped datagram. It lives here, beside the route, rather than in the
//! link: a reconnect replaces the socket, not the route, and restarting the
//! count would make a gap look like a rewind.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::peer::PeerRouteTable;
//! use astrs_wire::{
//!     Compression, DaemonId, DataflowId, Plane, RouteAcceptance, RouteKey, RouteSpec,
//! };
//!
//! let peer = DaemonId::generate(None);
//! let key = RouteKey::new(
//!     DataflowId::from_u128(1),
//!     "camera/image".parse()?,
//!     "detect/frames".parse()?,
//! );
//!
//! let mut routes = PeerRouteTable::new();
//! let route = routes.open(peer.clone(), RouteSpec::new(key).with_plane(Plane::Tcp), 1);
//! assert!(routes.state(&peer, route).is_some_and(|state| state.is_pending()));
//!
//! routes.settle(&peer, route, &RouteAcceptance::accepted(Plane::Tcp, Compression::None, 1 << 20));
//! assert_eq!(routes.next_seq(&peer, route), Some(0));
//! assert_eq!(routes.next_seq(&peer, route), Some(1));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::BTreeMap;

use astrs_wire::{
    Compression, DaemonId, DataflowId, PortRef, RouteAcceptance, RouteCloseReason, RouteId,
    RouteKey, RouteRejection, RouteSpec,
};

/// Where one cross-host route sits.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RemoteRouteState {
    /// A `RouteSetup` was sent and no answer has arrived.
    Pending,
    /// Both ends agreed; payloads may flow.
    Established,
    /// The peer refused it.
    Rejected {
        /// Why.
        rejection: RouteRejection,
    },
    /// The producer said it will send nothing more, but the route is still
    /// open so the consumer can drain (§24.1 `OutputClosed`).
    Draining {
        /// The last sequence number sent.
        final_seq: u64,
        /// Why the output closed.
        reason: RouteCloseReason,
    },
    /// The route is gone.
    Closed {
        /// Why.
        reason: RouteCloseReason,
    },
}

impl RemoteRouteState {
    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Established => "established",
            Self::Rejected { .. } => "rejected",
            Self::Draining { .. } => "draining",
            Self::Closed { .. } => "closed",
        }
    }

    /// Whether the route is waiting for an answer.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }

    /// Whether payloads may be sent on it.
    #[must_use]
    pub const fn is_established(&self) -> bool {
        matches!(self, Self::Established)
    }

    /// Whether the route has finished, one way or another.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Rejected { .. } | Self::Closed { .. })
    }
}

/// One cross-host route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRoute {
    /// The handle both ends use.
    pub route_id: RouteId,
    /// The peer at the other end.
    pub daemon: DaemonId,
    /// The edge it serves, its plane and its negotiated codec.
    pub spec: RouteSpec,
    /// The producer incarnation it belongs to (§12).
    pub generation: u64,
    /// Where it sits.
    pub state: RemoteRouteState,
    /// The next sequence number to stamp on an outbound payload.
    pub seq: u64,
    /// How many payloads have crossed it.
    pub messages: u64,
    /// How many payload bytes have crossed it.
    pub bytes: u64,
}

impl RemoteRoute {
    /// The producing port.
    #[must_use]
    pub const fn producer(&self) -> &PortRef {
        &self.spec.key.producer
    }

    /// The consuming port.
    #[must_use]
    pub const fn consumer(&self) -> &PortRef {
        &self.spec.key.consumer
    }

    /// The dataflow this route belongs to.
    #[must_use]
    pub const fn dataflow(&self) -> DataflowId {
        self.spec.key.dataflow
    }

    /// The negotiated codec.
    #[must_use]
    pub const fn compression(&self) -> Compression {
        self.spec.compression
    }

    /// The route's mux descriptor: the bytes the transport uses to name the
    /// logical stream this route rides on.
    ///
    /// Derived from the key rather than random so both ends can log the same
    /// name for the same stream, and so a reconnect that re-opens the stream
    /// describes it identically.
    #[must_use]
    pub fn descriptor(&self) -> Vec<u8> {
        format!(
            "{}|{}|{}",
            self.spec.key.dataflow, self.spec.key.producer, self.spec.key.consumer
        )
        .into_bytes()
    }
}

/// Every cross-host route this daemon takes part in.
#[derive(Debug, Default, Clone)]
pub struct PeerRouteTable {
    /// Routes this daemon opened, as the producer's host.
    outbound: BTreeMap<(DaemonId, RouteId), RemoteRoute>,
    /// Routes a peer opened with this daemon, as the consumer's host.
    inbound: BTreeMap<(DaemonId, RouteId), RemoteRoute>,
    /// The next handle to mint.
    next_handle: u64,
}

impl PeerRouteTable {
    /// An empty table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            outbound: BTreeMap::new(),
            inbound: BTreeMap::new(),
            next_handle: 1,
        }
    }

    /// Mints the next handle.
    ///
    /// Handles start at [`RouteId::FIRST`] because zero is reserved
    /// ([`RouteId::NONE`]) and are never reused within a process: a stale
    /// `Output` for a handle that was closed and re-minted would otherwise be
    /// delivered to the wrong consumer.
    pub const fn allocate(&mut self) -> RouteId {
        let handle = RouteId::new(self.next_handle);
        self.next_handle = self.next_handle.saturating_add(1);
        handle
    }

    /// Opens an outbound route to `daemon`, returning its handle.
    pub fn open(&mut self, daemon: DaemonId, spec: RouteSpec, generation: u64) -> RouteId {
        let route_id = self.allocate();
        self.outbound.insert(
            (daemon.clone(), route_id),
            RemoteRoute {
                route_id,
                daemon,
                spec,
                generation,
                state: RemoteRouteState::Pending,
                seq: 0,
                messages: 0,
                bytes: 0,
            },
        );
        route_id
    }

    /// Records a peer's answer to one of this daemon's setups.
    ///
    /// Returns whether a pending route was found. A `false` is a stale answer
    /// — a peer replying about a handle this daemon already tore down — and is
    /// dropped rather than resurrecting the route.
    pub fn settle(
        &mut self,
        daemon: &DaemonId,
        route_id: RouteId,
        acceptance: &RouteAcceptance,
    ) -> bool {
        let Some(route) = self.outbound.get_mut(&(daemon.clone(), route_id)) else {
            return false;
        };
        if !route.state.is_pending() {
            return false;
        }
        match acceptance {
            RouteAcceptance::Accepted {
                plane, compression, ..
            } => {
                route.spec.plane = *plane;
                route.spec.compression = *compression;
                route.state = RemoteRouteState::Established;
            }
            RouteAcceptance::Rejected { reason } => {
                route.state = RemoteRouteState::Rejected {
                    rejection: reason.clone(),
                };
            }
            // `RouteAcceptance` is `#[non_exhaustive]`: an answer this build
            // cannot interpret is treated as a refusal, which keeps the route
            // on the reliable path rather than sending into a plane nobody
            // agreed to.
            _ => {
                route.state = RemoteRouteState::Rejected {
                    rejection: RouteRejection::Other {
                        message: "unrecognised route acceptance".into(),
                    },
                };
            }
        }
        true
    }

    /// Records a peer's setup, with the answer this daemon is giving.
    ///
    /// The inbound half is filed as established only when the answer accepts;
    /// a rejection is not remembered at all, because there is nothing left to
    /// remember it for.
    pub fn admit(
        &mut self,
        daemon: DaemonId,
        route_id: RouteId,
        spec: RouteSpec,
        generation: u64,
        acceptance: &RouteAcceptance,
    ) -> bool {
        if !acceptance.is_accepted() {
            return false;
        }
        let mut spec = spec;
        if let Some(compression) = acceptance.compression() {
            spec.compression = compression;
        }
        if let Some(plane) = acceptance.plane() {
            spec.plane = plane;
        }
        self.inbound.insert(
            (daemon.clone(), route_id),
            RemoteRoute {
                route_id,
                daemon,
                spec,
                generation,
                state: RemoteRouteState::Established,
                seq: 0,
                messages: 0,
                bytes: 0,
            },
        );
        true
    }

    /// The next sequence number for an outbound route, advancing the counter.
    ///
    /// [`None`] when the route is not established, which is what keeps a
    /// payload from being stamped on a route the peer never accepted.
    pub fn next_seq(&mut self, daemon: &DaemonId, route_id: RouteId) -> Option<u64> {
        let route = self.outbound.get_mut(&(daemon.clone(), route_id))?;
        if !route.state.is_established() {
            return None;
        }
        let seq = route.seq;
        route.seq = route.seq.saturating_add(1);
        route.messages = route.messages.saturating_add(1);
        Some(seq)
    }

    /// Records the bytes one outbound payload carried.
    pub fn record_sent(&mut self, daemon: &DaemonId, route_id: RouteId, bytes: u64) {
        if let Some(route) = self.outbound.get_mut(&(daemon.clone(), route_id)) {
            route.bytes = route.bytes.saturating_add(bytes);
        }
    }

    /// Records one inbound payload on a route a peer opened.
    ///
    /// Returns the route it belongs to, or [`None`] for a handle this daemon
    /// does not recognise — a peer that kept sending after a teardown.
    pub fn record_received(
        &mut self,
        daemon: &DaemonId,
        route_id: RouteId,
        bytes: u64,
    ) -> Option<&RemoteRoute> {
        let route = self.inbound.get_mut(&(daemon.clone(), route_id))?;
        if !route.state.is_established() {
            return None;
        }
        route.messages = route.messages.saturating_add(1);
        route.bytes = route.bytes.saturating_add(bytes);
        Some(route)
    }

    /// Marks an inbound route draining: its producer sent its last message.
    pub fn drain(
        &mut self,
        daemon: &DaemonId,
        route_id: RouteId,
        final_seq: u64,
        reason: RouteCloseReason,
    ) -> Option<&RemoteRoute> {
        let route = self.inbound.get_mut(&(daemon.clone(), route_id))?;
        route.state = RemoteRouteState::Draining { final_seq, reason };
        Some(route)
    }

    /// Closes a route in either direction, returning it if it existed.
    pub fn close(
        &mut self,
        daemon: &DaemonId,
        route_id: RouteId,
        reason: RouteCloseReason,
    ) -> Option<RemoteRoute> {
        let key = (daemon.clone(), route_id);
        let mut route = self
            .outbound
            .remove(&key)
            .or_else(|| self.inbound.remove(&key))?;
        route.state = RemoteRouteState::Closed { reason };
        Some(route)
    }

    /// Where one route sits, in either direction.
    #[must_use]
    pub fn state(&self, daemon: &DaemonId, route_id: RouteId) -> Option<&RemoteRouteState> {
        let key = (daemon.clone(), route_id);
        self.outbound
            .get(&key)
            .or_else(|| self.inbound.get(&key))
            .map(|route| &route.state)
    }

    /// One outbound route.
    #[must_use]
    pub fn outbound(&self, daemon: &DaemonId, route_id: RouteId) -> Option<&RemoteRoute> {
        self.outbound.get(&(daemon.clone(), route_id))
    }

    /// One inbound route.
    #[must_use]
    pub fn inbound(&self, daemon: &DaemonId, route_id: RouteId) -> Option<&RemoteRoute> {
        self.inbound.get(&(daemon.clone(), route_id))
    }

    /// Every established outbound route carrying `producer`'s messages.
    ///
    /// The lookup the publish path makes on every message, which is why it
    /// returns handles rather than references: the caller needs the table
    /// mutably a moment later to stamp the sequence number.
    #[must_use]
    pub fn established_for(
        &self,
        dataflow: DataflowId,
        producer: &PortRef,
    ) -> Vec<(DaemonId, RouteId)> {
        self.outbound
            .values()
            .filter(|route| {
                route.state.is_established()
                    && route.dataflow() == dataflow
                    && route.producer() == producer
            })
            .map(|route| (route.daemon.clone(), route.route_id))
            .collect()
    }

    /// Forgets one outbound route entirely, so the edge it served looks
    /// unopened again.
    ///
    /// The difference from [`PeerRouteTable::close`] is what the caller means
    /// by it. `close` records a route that *ended*; this erases one that never
    /// began, which is what a **transient** refusal
    /// ([`astrs_wire::RouteRejection::is_transient`]) calls for: the peer had
    /// not heard of the dataflow yet, or the consumer's spawn had not reached
    /// it. Leaving the rejected entry behind would make
    /// [`crate::coordinator`]'s reconciliation backstop skip the edge for
    /// ever — [`PeerRouteTable::has_outbound`] answers "yes, one exists" —
    /// and the route would never be retried even though the condition that
    /// refused it lasted milliseconds.
    ///
    /// Returns the route that was forgotten, if there was one.
    pub fn forget_outbound(&mut self, daemon: &DaemonId, route_id: RouteId) -> Option<RemoteRoute> {
        self.outbound.remove(&(daemon.clone(), route_id))
    }

    /// Whether an outbound route already exists for one edge.
    #[must_use]
    pub fn has_outbound(&self, daemon: &DaemonId, key: &RouteKey) -> bool {
        self.outbound_for_key(daemon, key).is_some()
    }

    /// The live outbound route for one edge, if this daemon opened one.
    ///
    /// The difference from [`PeerRouteTable::has_outbound`] is the route's
    /// `generation`: a producer that restarted is a *new* incarnation, and a
    /// route still stamped with the old one would let the peer accept
    /// messages the generation stamp exists to reject (§12). A caller
    /// reconciling routes therefore needs the number, not just the presence.
    #[must_use]
    pub fn outbound_for_key(&self, daemon: &DaemonId, key: &RouteKey) -> Option<&RemoteRoute> {
        self.outbound.values().find(|route| {
            route.daemon == *daemon && route.spec.key == *key && !route.state.is_terminal()
        })
    }

    /// Every route belonging to one peer, in both directions.
    #[must_use]
    pub fn routes_of(&self, daemon: &DaemonId) -> Vec<&RemoteRoute> {
        self.outbound
            .values()
            .chain(self.inbound.values())
            .filter(|route| route.daemon == *daemon)
            .collect()
    }

    /// Removes every route belonging to one peer, returning them.
    ///
    /// What a disconnect does: the routes are gone, and every inbound one
    /// becomes an `InputClosed` for its local consumer (§12).
    pub fn remove_peer(&mut self, daemon: &DaemonId) -> Vec<RemoteRoute> {
        let mut removed = Vec::new();
        self.outbound.retain(|(peer, _), route| {
            if peer == daemon {
                removed.push(route.clone());
                false
            } else {
                true
            }
        });
        self.inbound.retain(|(peer, _), route| {
            if peer == daemon {
                removed.push(route.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    /// Removes every route of one dataflow, returning them.
    pub fn remove_dataflow(&mut self, dataflow: DataflowId) -> Vec<RemoteRoute> {
        let mut removed = Vec::new();
        self.outbound.retain(|_, route| {
            if route.dataflow() == dataflow {
                removed.push(route.clone());
                false
            } else {
                true
            }
        });
        self.inbound.retain(|_, route| {
            if route.dataflow() == dataflow {
                removed.push(route.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    /// How many outbound routes exist.
    #[must_use]
    pub fn outbound_count(&self) -> usize {
        self.outbound.len()
    }

    /// How many inbound routes exist.
    #[must_use]
    pub fn inbound_count(&self) -> usize {
        self.inbound.len()
    }

    /// How many routes exist in total.
    #[must_use]
    pub fn len(&self) -> usize {
        self.outbound.len() + self.inbound.len()
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.outbound.is_empty() && self.inbound.is_empty()
    }

    /// How many routes are established, in either direction (§13).
    #[must_use]
    pub fn established_count(&self) -> usize {
        self.outbound
            .values()
            .chain(self.inbound.values())
            .filter(|route| route.state.is_established())
            .count()
    }

    /// Every route, in both directions.
    pub fn all(&self) -> impl Iterator<Item = &RemoteRoute> {
        self.outbound.values().chain(self.inbound.values())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_wire::Plane;

    use super::*;

    fn dataflow() -> DataflowId {
        DataflowId::from_u128(5)
    }

    fn key(producer: &str, consumer: &str) -> RouteKey {
        RouteKey::new(
            dataflow(),
            producer.parse().expect("a port"),
            consumer.parse().expect("a port"),
        )
    }

    fn spec(producer: &str, consumer: &str) -> RouteSpec {
        RouteSpec::new(key(producer, consumer)).with_plane(Plane::Tcp)
    }

    fn accepted() -> RouteAcceptance {
        RouteAcceptance::accepted(Plane::Tcp, Compression::None, 1 << 20)
    }

    fn rejected() -> RouteAcceptance {
        RouteAcceptance::Rejected {
            reason: RouteRejection::UnknownPort {
                port: "detect/frames".parse().expect("a port"),
            },
        }
    }

    #[test]
    fn an_empty_table_knows_nothing() {
        let routes = PeerRouteTable::new();
        assert!(routes.is_empty());
        assert_eq!(routes.len(), 0);
        assert_eq!(routes.established_count(), 0);
        assert!(routes.all().next().is_none());
        assert!(
            routes
                .established_for(dataflow(), &"camera/image".parse().unwrap())
                .is_empty()
        );
    }

    #[test]
    fn handles_start_at_one_and_never_repeat() {
        let mut routes = PeerRouteTable::new();
        assert_eq!(routes.allocate(), RouteId::FIRST);
        assert_eq!(routes.allocate(), RouteId::new(2));
        assert_ne!(routes.allocate(), RouteId::NONE);
    }

    #[test]
    fn an_outbound_route_is_pending_until_it_is_answered() {
        let peer = DaemonId::generate(None);
        let mut routes = PeerRouteTable::new();
        let route = routes.open(peer.clone(), spec("camera/image", "detect/frames"), 1);

        assert!(routes.state(&peer, route).expect("present").is_pending());
        assert_eq!(routes.next_seq(&peer, route), None, "not yet established");
        assert_eq!(routes.outbound_count(), 1);

        assert!(routes.settle(&peer, route, &accepted()));
        assert!(
            routes
                .state(&peer, route)
                .expect("present")
                .is_established()
        );
        assert_eq!(routes.established_count(), 1);
    }

    #[test]
    fn an_acceptance_applies_the_negotiated_terms() {
        let peer = DaemonId::generate(None);
        let mut routes = PeerRouteTable::new();
        let route = routes.open(peer.clone(), spec("camera/image", "detect/frames"), 1);
        routes.settle(
            &peer,
            route,
            &RouteAcceptance::accepted(Plane::Tcp, Compression::Zstd, 1 << 20),
        );
        let established = routes.outbound(&peer, route).expect("present");
        assert_eq!(established.compression(), Compression::Zstd);
        assert_eq!(established.spec.plane, Plane::Tcp);
    }

    #[test]
    fn a_rejection_is_remembered_with_its_cause() {
        let peer = DaemonId::generate(None);
        let mut routes = PeerRouteTable::new();
        let route = routes.open(peer.clone(), spec("camera/image", "detect/frames"), 1);
        assert!(routes.settle(&peer, route, &rejected()));

        match routes.state(&peer, route).expect("present") {
            RemoteRouteState::Rejected { rejection } => {
                assert_eq!(rejection.kind_name(), "unknown_port");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(routes.next_seq(&peer, route), None);
        assert_eq!(routes.established_count(), 0);
    }

    #[test]
    fn a_second_answer_changes_nothing() {
        let peer = DaemonId::generate(None);
        let mut routes = PeerRouteTable::new();
        let route = routes.open(peer.clone(), spec("camera/image", "detect/frames"), 1);
        assert!(routes.settle(&peer, route, &accepted()));
        assert!(!routes.settle(&peer, route, &rejected()));
        assert!(
            routes
                .state(&peer, route)
                .expect("present")
                .is_established()
        );
    }

    #[test]
    fn an_answer_for_an_unknown_handle_is_dropped() {
        let peer = DaemonId::generate(None);
        let mut routes = PeerRouteTable::new();
        assert!(!routes.settle(&peer, RouteId::new(99), &accepted()));
    }

    #[test]
    fn sequence_numbers_start_at_zero_and_increase() {
        let peer = DaemonId::generate(None);
        let mut routes = PeerRouteTable::new();
        let route = routes.open(peer.clone(), spec("camera/image", "detect/frames"), 1);
        routes.settle(&peer, route, &accepted());

        assert_eq!(routes.next_seq(&peer, route), Some(0));
        assert_eq!(routes.next_seq(&peer, route), Some(1));
        routes.record_sent(&peer, route, 128);
        let sent = routes.outbound(&peer, route).expect("present");
        assert_eq!(sent.messages, 2);
        assert_eq!(sent.bytes, 128);
    }

    #[test]
    fn an_inbound_route_is_filed_when_it_is_accepted() {
        let peer = DaemonId::generate(None);
        let mut routes = PeerRouteTable::new();
        assert!(routes.admit(
            peer.clone(),
            RouteId::FIRST,
            spec("camera/image", "detect/frames"),
            2,
            &RouteAcceptance::accepted(Plane::Tcp, Compression::Lz4, 1 << 20),
        ));

        let route = routes.inbound(&peer, RouteId::FIRST).expect("present");
        assert_eq!(route.generation, 2);
        assert_eq!(route.compression(), Compression::Lz4);
        assert_eq!(route.consumer(), &"detect/frames".parse().unwrap());
        assert_eq!(routes.inbound_count(), 1);
    }

    #[test]
    fn a_refused_setup_is_not_remembered() {
        let peer = DaemonId::generate(None);
        let mut routes = PeerRouteTable::new();
        assert!(!routes.admit(
            peer.clone(),
            RouteId::FIRST,
            spec("camera/image", "detect/frames"),
            1,
            &rejected(),
        ));
        assert!(routes.is_empty());
    }

    #[test]
    fn an_inbound_payload_is_accounted_against_its_route() {
        let peer = DaemonId::generate(None);
        let mut routes = PeerRouteTable::new();
        routes.admit(
            peer.clone(),
            RouteId::FIRST,
            spec("camera/image", "detect/frames"),
            1,
            &accepted(),
        );

        let route = routes
            .record_received(&peer, RouteId::FIRST, 64)
            .expect("known route");
        assert_eq!(route.bytes, 64);
        assert!(
            routes.record_received(&peer, RouteId::new(7), 64).is_none(),
            "a handle nobody opened"
        );
    }

    #[test]
    fn draining_keeps_the_route_open() {
        let peer = DaemonId::generate(None);
        let mut routes = PeerRouteTable::new();
        routes.admit(
            peer.clone(),
            RouteId::FIRST,
            spec("camera/image", "detect/frames"),
            1,
            &accepted(),
        );
        let route = routes
            .drain(
                &peer,
                RouteId::FIRST,
                17,
                RouteCloseReason::ProducerFinished,
            )
            .expect("present");
        assert_eq!(route.state.as_str(), "draining");
        assert!(routes.inbound(&peer, RouteId::FIRST).is_some());
    }

    #[test]
    fn closing_removes_the_route_from_either_direction() {
        let peer = DaemonId::generate(None);
        let mut routes = PeerRouteTable::new();
        let outbound = routes.open(peer.clone(), spec("camera/image", "detect/frames"), 1);
        routes.admit(
            peer.clone(),
            RouteId::new(50),
            spec("lidar/points", "plan/cloud"),
            1,
            &accepted(),
        );

        let closed = routes
            .close(&peer, outbound, RouteCloseReason::DataflowStopped)
            .expect("present");
        assert!(closed.state.is_terminal());
        assert_eq!(routes.outbound_count(), 0);

        assert!(
            routes
                .close(&peer, RouteId::new(50), RouteCloseReason::ConsumerGone)
                .is_some()
        );
        assert!(routes.is_empty());
        assert!(
            routes
                .close(&peer, RouteId::new(50), RouteCloseReason::ConsumerGone)
                .is_none()
        );
    }

    #[test]
    fn the_publish_path_finds_every_established_route_for_a_producer() {
        let first = DaemonId::generate(None);
        let second = DaemonId::generate(None);
        let mut routes = PeerRouteTable::new();
        let a = routes.open(first.clone(), spec("camera/image", "detect/frames"), 1);
        let b = routes.open(second.clone(), spec("camera/image", "plan/frames"), 1);
        let pending = routes.open(first.clone(), spec("camera/image", "record/raw"), 1);
        routes.settle(&first, a, &accepted());
        routes.settle(&second, b, &accepted());

        let found = routes.established_for(dataflow(), &"camera/image".parse().unwrap());
        assert_eq!(found.len(), 2, "the pending one is not carried on");
        assert!(found.contains(&(first.clone(), a)));
        assert!(found.contains(&(second, b)));
        assert!(!found.contains(&(first, pending)));
    }

    #[test]
    fn a_duplicate_edge_is_detected_before_it_is_opened_twice() {
        let peer = DaemonId::generate(None);
        let mut routes = PeerRouteTable::new();
        assert!(!routes.has_outbound(&peer, &key("camera/image", "detect/frames")));
        let route = routes.open(peer.clone(), spec("camera/image", "detect/frames"), 1);
        assert!(routes.has_outbound(&peer, &key("camera/image", "detect/frames")));

        routes.settle(&peer, route, &rejected());
        assert!(
            !routes.has_outbound(&peer, &key("camera/image", "detect/frames")),
            "a rejected route does not block a retry"
        );
    }

    #[test]
    fn a_disconnect_takes_every_route_of_that_peer() {
        let peer = DaemonId::generate(None);
        let other = DaemonId::generate(None);
        let mut routes = PeerRouteTable::new();
        routes.open(peer.clone(), spec("camera/image", "detect/frames"), 1);
        routes.admit(
            peer.clone(),
            RouteId::new(9),
            spec("lidar/points", "plan/cloud"),
            1,
            &accepted(),
        );
        routes.open(other.clone(), spec("camera/image", "far/frames"), 1);

        let removed = routes.remove_peer(&peer);
        assert_eq!(removed.len(), 2);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes.routes_of(&other).len(), 1);
        assert!(routes.routes_of(&peer).is_empty());
    }

    #[test]
    fn a_stopped_dataflow_takes_its_routes() {
        let peer = DaemonId::generate(None);
        let mut routes = PeerRouteTable::new();
        routes.open(peer.clone(), spec("camera/image", "detect/frames"), 1);
        let other = RouteSpec::new(RouteKey::new(
            DataflowId::from_u128(6),
            "camera/image".parse().unwrap(),
            "detect/frames".parse().unwrap(),
        ));
        routes.open(peer, other, 1);

        assert_eq!(routes.remove_dataflow(dataflow()).len(), 1);
        assert_eq!(routes.len(), 1);
    }

    #[test]
    fn a_route_describes_its_own_stream() {
        let peer = DaemonId::generate(None);
        let mut routes = PeerRouteTable::new();
        let route = routes.open(peer.clone(), spec("camera/image", "detect/frames"), 1);
        let descriptor = routes.outbound(&peer, route).expect("present").descriptor();
        let text = String::from_utf8(descriptor).expect("utf-8");
        assert!(text.contains("camera/image"), "{text}");
        assert!(text.contains("detect/frames"), "{text}");
    }

    #[test]
    fn states_have_distinct_labels() {
        let states = [
            RemoteRouteState::Pending,
            RemoteRouteState::Established,
            RemoteRouteState::Rejected {
                rejection: RouteRejection::Other {
                    message: "no".into(),
                },
            },
            RemoteRouteState::Draining {
                final_seq: 0,
                reason: RouteCloseReason::ProducerFinished,
            },
            RemoteRouteState::Closed {
                reason: RouteCloseReason::DaemonShutdown,
            },
        ];
        let mut labels: Vec<&str> = states.iter().map(RemoteRouteState::as_str).collect();
        let count = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), count);
    }
}
