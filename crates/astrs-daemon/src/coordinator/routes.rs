//! Cross-daemon routes, as told by the coordinator (blueprint §4.2, §6.4,
//! §12).
//!
//! A daemon can derive every *same-host* route from the `Spawn` events it is
//! given: an input's source names a producer, and if that producer is a node
//! this daemon also spawned, the edge is local. The edges it cannot derive are
//! the ones whose other end is on another machine — it has no way to know
//! which daemon that is, or how to reach it. The coordinator does, and says so
//! in [`astrs_wire::CoordinatorEvent::PeerRoutes`].
//!
//! ```text
//!   coordinator ── PeerRoutes{ route, peer, address } ──► daemon A (camera)
//!               └─ PeerRoutes{ route, peer, address } ──► daemon B (detect)
//!
//!   whichever daemon has the *lower* id dials ────────────► one link
//!   the producing daemon then opens the route  ── RouteSetup ─►
//!                                              ◄─ RouteAccept ──
//!   camera publishes ─────────────────────────► Output ──► detect's mailbox
//! ```
//!
//! # Who dials
//!
//! Whoever has the lower [`DaemonId`]. A graph split across two machines
//! usually has edges in *both* directions (sensor → planner on B, planner →
//! actuator back on A), so "the producer dials" would have both daemons
//! dialling each other at the same instant — and
//! [`crate::peer::PeerManager::install`] drops the routes of the link it
//! replaces, so the second connection to arrive would tear down the routes the
//! first had just established, and the two would flap. A total order on ids
//! breaks the tie with no extra message: exactly one side dials, and the
//! single resulting link carries routes in both directions (a `RouteSetup` is
//! valid from either end).
//!
//! # Why the directives are remembered rather than acted on
//!
//! Because every precondition for acting on one can arrive later, in any
//! order:
//!
//! - the **link** may not exist yet (this daemon is the side that waits to be
//!   dialled, or the peer's process has not started);
//! - the **producer** may not be spawned yet — a `PeerRoutes` that overtook
//!   its `Spawn` would otherwise stamp the route with generation zero, and the
//!   generation stamp is exactly what §12 uses to reject stale messages;
//! - the link may **come back** after a partition, at which point every route
//!   has to be opened again from scratch (§12).
//!
//! So [`PeerDirectory`] records what *should* be open, and
//! [`crate::server::Daemon::flush_peer_routes`] reconciles that against what
//! *is* open — from every event that could change the answer, and once more on
//! a timer so a race nobody anticipated still converges.

use std::collections::{BTreeMap, BTreeSet};

use astrs_transport::{HandshakeParams, TransportAddr};
use astrs_wire::{
    DaemonId, DataflowId, InputSpec, PeerRouteDirective, RouteCloseReason, RouteKey, RouteSpec,
};

use crate::peer::{PeerConfig, PeerLink};
use crate::server::core::Daemon;
use crate::session::{DaemonEvent, DaemonHandle};
use crate::state::DeliveryPlane;

/// How often an unreconciled peer is looked at again.
///
/// The backstop under the targeted calls, not the mechanism: every event that
/// can make a route openable already calls
/// [`Daemon::reconcile_peer`]. This bounds how long a case nobody anticipated
/// — a dial that failed, a `Spawn` that arrived from a coordinator whose
/// `PeerRoutes` was dropped by a full buffer — takes to resolve itself.
pub const PEER_RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// What this daemon knows about one peer.
#[derive(Debug, Clone)]
struct PeerEntry {
    /// Where to dial it.
    address: TransportAddr,
    /// Every cross-daemon edge that crosses to it, in either direction.
    edges: BTreeMap<RouteKey, RouteSpec>,
    /// Whether a dial is already in flight, so a burst of directives does not
    /// become a burst of connections.
    dialing: bool,
}

/// Every peer the coordinator has named, and the edges that cross to it.
///
/// # Examples
///
/// ```
/// use astrs_daemon::coordinator::PeerDirectory;
/// use astrs_transport::TransportAddr;
/// use astrs_wire::{DaemonId, DataflowId, RouteKey, RouteSpec};
///
/// let mut directory = PeerDirectory::new();
/// let peer = DaemonId::generate(None);
/// let address: TransportAddr = "tcp:10.0.0.4:7409".parse()?;
/// let route = RouteSpec::new(RouteKey::new(
///     DataflowId::from_u128(1),
///     "camera/image".parse()?,
///     "detect/frames".parse()?,
/// ));
///
/// assert!(directory.note_edge(peer.clone(), address.clone(), route.clone()));
/// assert!(!directory.note_edge(peer.clone(), address.clone(), route));
/// assert_eq!(directory.address_of(&peer), Some(&address));
/// assert_eq!(directory.edges_for(&peer).len(), 1);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Default)]
pub struct PeerDirectory {
    /// One entry per peer daemon.
    peers: BTreeMap<DaemonId, PeerEntry>,
}

impl PeerDirectory {
    /// An empty directory.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            peers: BTreeMap::new(),
        }
    }

    /// How many peers are known.
    #[must_use]
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Whether no peer has been named.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Every peer, in identity order.
    pub fn peers(&self) -> impl Iterator<Item = &DaemonId> {
        self.peers.keys()
    }

    /// Where a peer is reachable, if it has been named.
    #[must_use]
    pub fn address_of(&self, daemon: &DaemonId) -> Option<&TransportAddr> {
        self.peers.get(daemon).map(|entry| &entry.address)
    }

    /// Records how to reach `daemon`, replacing a previous address.
    ///
    /// A peer that moved keeps its edges: the directory is keyed by identity
    /// exactly as [`crate::peer::PeerManager`]'s table is, for the same
    /// reason.
    pub fn note_peer(&mut self, daemon: DaemonId, address: TransportAddr) {
        self.peers
            .entry(daemon)
            .and_modify(|entry| entry.address = address.clone())
            .or_insert(PeerEntry {
                address,
                edges: BTreeMap::new(),
                dialing: false,
            });
    }

    /// Records one cross-daemon edge, returning whether it was new.
    pub fn note_edge(
        &mut self,
        daemon: DaemonId,
        address: TransportAddr,
        route: RouteSpec,
    ) -> bool {
        self.note_peer(daemon.clone(), address);
        self.peers
            .get_mut(&daemon)
            .is_some_and(|entry| entry.edges.insert(route.key.clone(), route).is_none())
    }

    /// Every edge that crosses to `daemon`, in route-key order.
    #[must_use]
    pub fn edges_for(&self, daemon: &DaemonId) -> Vec<RouteSpec> {
        self.peers
            .get(daemon)
            .map(|entry| entry.edges.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Claims the right to dial `daemon`, returning its address exactly once
    /// until [`PeerDirectory::dial_finished`] releases the claim.
    #[must_use]
    pub fn claim_dial(&mut self, daemon: &DaemonId) -> Option<TransportAddr> {
        let entry = self.peers.get_mut(daemon)?;
        if entry.dialing {
            return None;
        }
        entry.dialing = true;
        Some(entry.address.clone())
    }

    /// Whether a dial to `daemon` is in flight.
    #[must_use]
    pub fn is_dialing(&self, daemon: &DaemonId) -> bool {
        self.peers.get(daemon).is_some_and(|entry| entry.dialing)
    }

    /// Releases a dial claim, whether the dial succeeded or not.
    pub fn dial_finished(&mut self, daemon: &DaemonId) {
        if let Some(entry) = self.peers.get_mut(daemon) {
            entry.dialing = false;
        }
    }

    /// Forgets every edge belonging to `dataflow`, and any peer left with
    /// nothing to do.
    pub fn forget_dataflow(&mut self, dataflow: DataflowId) {
        for entry in self.peers.values_mut() {
            entry.edges.retain(|key, _| key.dataflow != dataflow);
        }
        self.peers
            .retain(|_, entry| !entry.edges.is_empty() || entry.dialing);
    }

    /// Forgets a peer entirely.
    pub fn forget_peer(&mut self, daemon: &DaemonId) {
        self.peers.remove(daemon);
    }
}

impl Daemon {
    /// Applies the coordinator's cross-daemon route directives (§6.4).
    ///
    /// Returns how many were recorded. Nothing is opened here: see the module
    /// docs for why acting on a directive is deferred to
    /// [`Daemon::reconcile_peer`], which this calls once per named peer and
    /// which is safe to call again at any time.
    pub fn apply_peer_routes(
        &mut self,
        dataflow: DataflowId,
        directives: &[PeerRouteDirective],
    ) -> usize {
        let mut recorded = 0;
        let mut peers: BTreeSet<DaemonId> = BTreeSet::new();
        for directive in directives {
            if directive.dataflow() != dataflow {
                tracing::warn!(
                    route = %directive.route.key,
                    expected = %dataflow,
                    "a peer route directive named a different dataflow; ignoring"
                );
                continue;
            }
            let address = match directive.address.parse::<TransportAddr>() {
                Ok(address) => address,
                Err(error) => {
                    tracing::warn!(
                        peer = %directive.peer,
                        address = %directive.address,
                        %error,
                        "a peer route directive carried an address this build cannot parse"
                    );
                    continue;
                }
            };
            if self.peer_directory_mut().note_edge(
                directive.peer.clone(),
                address,
                directive.route.clone(),
            ) {
                tracing::info!(
                    route = %directive.route.key,
                    peer = %directive.peer,
                    "a cross-daemon route was announced"
                );
            }
            peers.insert(directive.peer.clone());
            recorded += 1;
        }
        for peer in peers {
            self.reconcile_peer(&peer);
        }
        recorded
    }

    /// Dials a peer if this daemon is the side that dials, then opens every
    /// route the link can carry.
    ///
    /// Idempotent, and called from every point that could change the answer: a
    /// directive arriving, a link attaching, a node spawning, a timer tick.
    pub fn reconcile_peer(&mut self, peer: &DaemonId) {
        if self.peers().is_connected(peer) {
            self.flush_peer_routes(peer);
            return;
        }
        if !self.should_dial(peer) || self.state().is_shutting_down() {
            return;
        }
        let Some(address) = self.peer_directory_mut().claim_dial(peer) else {
            return;
        };
        let local = self.config().id().clone();
        let config = self.config().peer().clone();
        let handle = self.handle();
        let target = peer.clone();
        tokio::spawn(dial_peer(local, target, address, config, handle));
    }

    /// Closes one peer connection deliberately and tells the local consumers
    /// their inputs are gone (§12).
    ///
    /// The operator-facing twin of a partition: an `astrs`-initiated
    /// disconnect must look to everything downstream exactly like a cable
    /// coming out, `InputClosed` and all — and must recover the same way, by
    /// the reconciliation backstop re-dialling and the consumers seeing
    /// `InputRecovered`. The directive book is deliberately *not* cleared:
    /// the graph has not changed, only the link.
    pub async fn disconnect_peer(&mut self, peer: &DaemonId, reason: &str) {
        let _ = self.peers_mut().disconnect(peer, reason).await;
        self.handle_peer_lost(peer, reason);
        self.peer_directory_mut().dial_finished(peer);
    }

    /// Re-examines every peer, opening whatever became possible.
    pub fn reconcile_peers(&mut self) {
        let peers: Vec<DaemonId> = self.peer_directory().peers().cloned().collect();
        for peer in peers {
            self.reconcile_peer(&peer);
        }
    }

    /// Whether *this* daemon is the one that dials `peer`.
    ///
    /// The total order on [`DaemonId`] is the tie-break: exactly one of any
    /// two daemons satisfies it, so exactly one link is built and neither side
    /// tears down the other's routes by replacing it.
    #[must_use]
    pub fn should_dial(&self, peer: &DaemonId) -> bool {
        self.config().id() < peer
    }

    /// Opens every route towards a connected peer that this daemon produces on
    /// and that is not open yet.
    ///
    /// Returns how many were opened. An edge whose producer this daemon does
    /// not host is skipped — the peer opens that one — and so is an edge whose
    /// producer has not been admitted yet, because the route carries the
    /// producer's generation and a wrong one is worse than a late route (§12).
    pub fn flush_peer_routes(&mut self, peer: &DaemonId) -> usize {
        let edges = self.peer_directory().edges_for(peer);
        let mut opened = 0;
        for route in edges {
            let key = route.key.clone();
            let hosts_producer = self
                .dataflow(key.dataflow)
                .is_some_and(|state| state.node(key.producer.node()).is_some());
            if !hosts_producer {
                continue;
            }
            self.register_remote_consumer(&route, peer);
            let generation = self
                .dataflow(key.dataflow)
                .and_then(|state| state.node(key.producer.node()))
                .map_or(0, crate::state::NodeState::generation);
            match self
                .peers()
                .routes()
                .outbound_for_key(peer, &key)
                .map(|route| route.generation)
            {
                Some(open) if open == generation => continue,
                Some(stale) => {
                    // The producer restarted under it. A route still stamped
                    // with the previous incarnation would let the peer accept
                    // messages the stamp exists to reject (§12), so it is torn
                    // down and re-opened at the current generation.
                    tracing::info!(
                        peer = %peer,
                        edge = %key,
                        stale,
                        generation,
                        "reopening a cross-daemon route for a new incarnation"
                    );
                    self.teardown_outbound(
                        peer,
                        &key,
                        RouteCloseReason::ProducerCrashed { generation: stale },
                    );
                }
                None => {}
            }
            match self.open_remote_route(
                peer,
                key.dataflow,
                key.producer.clone(),
                key.consumer.clone(),
            ) {
                Ok(route_id) => {
                    tracing::info!(
                        peer = %peer,
                        route = %route_id.get(),
                        edge = %key,
                        "opened a cross-daemon route"
                    );
                    opened += 1;
                }
                Err(error) => tracing::warn!(
                    peer = %peer,
                    edge = %key,
                    %error,
                    "could not open a cross-daemon route"
                ),
            }
        }
        opened
    }

    /// Adds the remote consumer to the local route table on
    /// [`DeliveryPlane::Remote`], if it is not already there.
    ///
    /// It carries no mailbox and receives no local delivery — the fan-out
    /// skips it — and the peer leg carries the payload instead. What the entry
    /// *does* give is presence: without it
    /// [`crate::state::RouteTable::produced_by`] reports no consumers for the
    /// producer's output, so nothing would tell the peer when the producer
    /// finishes, and `plan_shm_output` would offer a zero-copy ring to an
    /// output that has a consumer no ring can reach.
    fn register_remote_consumer(&mut self, route: &RouteSpec, peer: &DaemonId) {
        let dataflow = route.key.dataflow;
        let consumer = route.key.consumer.clone();
        let already = self.dataflow(dataflow).is_some_and(|state| {
            state
                .routes()
                .consumers(&route.key.producer)
                .iter()
                .any(|existing| {
                    existing.node == *consumer.node() && existing.spec.id == *consumer.port()
                })
        });
        if already {
            return;
        }
        let mut spec = InputSpec::new(consumer.port().clone(), route.key.producer.clone());
        spec.queue_size = route.queue_size.max(1);
        spec.queue_policy = route.queue_policy;
        if let Some(state) = self.state_mut().dataflow_mut(dataflow) {
            state.routes_mut().insert_on(
                consumer.node().clone(),
                spec,
                DeliveryPlane::Remote {
                    daemon: peer.clone(),
                },
            );
        }
        self.plan_shm_output(dataflow, &route.key.producer);
    }

    /// Tears one outbound route down, telling the peer.
    fn teardown_outbound(
        &mut self,
        peer: &DaemonId,
        key: &astrs_wire::RouteKey,
        reason: RouteCloseReason,
    ) {
        let Some(route_id) = self
            .peers()
            .routes()
            .outbound_for_key(peer, key)
            .map(|route| route.route_id)
        else {
            return;
        };
        // A teardown this daemon could not deliver is still a teardown: the
        // route is forgotten locally either way.
        let _ = self.peers_mut().teardown_route(peer, route_id, reason);
    }

    /// Tears down every cross-daemon route of one dataflow and forgets its
    /// directives.
    pub fn forget_peer_routes(&mut self, dataflow: DataflowId, reason: RouteCloseReason) {
        let routes: Vec<(DaemonId, astrs_wire::RouteId)> = self
            .peers()
            .routes()
            .all()
            .filter(|route| route.dataflow() == dataflow)
            .map(|route| (route.daemon.clone(), route.route_id))
            .collect();
        for (daemon, route_id) in routes {
            let _ = self
                .peers_mut()
                .teardown_route(&daemon, route_id, reason.clone());
        }
        self.peer_directory_mut().forget_dataflow(dataflow);
    }
}

/// Dials one peer and hands the established link to the event loop.
///
/// A free function, and a spawned task, for the same reason the accept loop is
/// one: the dial blocks on a network round trip, and the event loop owns every
/// mutable fact (§4.3). The link therefore arrives as an ordinary
/// [`DaemonEvent::PeerAttached`], exactly as an accepted one does, and a
/// failure arrives as [`DaemonEvent::PeerLost`] so the dial claim is released
/// on both paths.
///
/// [`astrs_transport::backend::connect_with_fallback`] is the §23 risk-5
/// dance: a `quic:` address is attempted over QUIC when this build carries the
/// feature, and retried over TCP with byte-identical framing when it does not,
/// or when the QUIC leg fails.
async fn dial_peer(
    local: DaemonId,
    peer: DaemonId,
    address: TransportAddr,
    config: PeerConfig,
    handle: DaemonHandle,
) {
    let params = HandshakeParams::from_config(
        config.transport(),
        config.identity(&local),
        config.auth().clone(),
        address.requires_crc(),
    );
    let dial =
        astrs_transport::backend::connect_with_fallback(&address, config.transport(), &params);
    match tokio::time::timeout(config.dial_timeout(), dial).await {
        Ok(Ok((connection, channels))) => {
            let link = PeerLink::establish(peer, connection, channels, handle.clone());
            handle.send(DaemonEvent::PeerAttached {
                link: Box::new(link),
            });
        }
        Ok(Err(error)) => {
            tracing::warn!(%peer, %address, %error, "a peer dial failed");
            handle.send(DaemonEvent::PeerLost {
                daemon: peer,
                reason: error.to_string(),
            });
        }
        Err(_) => {
            tracing::warn!(%peer, %address, "a peer dial timed out");
            handle.send(DaemonEvent::PeerLost {
                daemon: peer,
                reason: "the peer dial timed out".to_owned(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn route(producer: &str, consumer: &str) -> RouteSpec {
        RouteSpec::new(RouteKey::new(
            DataflowId::from_u128(1),
            producer.parse().unwrap(),
            consumer.parse().unwrap(),
        ))
    }

    fn address() -> TransportAddr {
        "tcp:127.0.0.1:7409".parse().unwrap()
    }

    #[test]
    fn a_fresh_directory_knows_nobody() {
        let directory = PeerDirectory::new();
        assert!(directory.is_empty());
        assert_eq!(directory.len(), 0);
        assert!(directory.peers().next().is_none());
    }

    #[test]
    fn noting_a_peer_records_where_it_is() {
        let mut directory = PeerDirectory::new();
        let peer = DaemonId::generate(None);
        directory.note_peer(peer.clone(), address());
        assert_eq!(directory.address_of(&peer), Some(&address()));
        assert!(directory.edges_for(&peer).is_empty());
    }

    #[test]
    fn an_edge_is_recorded_once() {
        let mut directory = PeerDirectory::new();
        let peer = DaemonId::generate(None);
        assert!(directory.note_edge(peer.clone(), address(), route("a/o", "b/i")));
        assert!(!directory.note_edge(peer.clone(), address(), route("a/o", "b/i")));
        assert_eq!(directory.edges_for(&peer).len(), 1);
    }

    #[test]
    fn edges_in_both_directions_are_kept_apart() {
        let mut directory = PeerDirectory::new();
        let peer = DaemonId::generate(None);
        directory.note_edge(peer.clone(), address(), route("a/o", "b/i"));
        directory.note_edge(peer.clone(), address(), route("b/o", "a/i"));
        assert_eq!(directory.edges_for(&peer).len(), 2);
    }

    #[test]
    fn a_moved_peer_keeps_its_edges() {
        let mut directory = PeerDirectory::new();
        let peer = DaemonId::generate(None);
        directory.note_edge(peer.clone(), address(), route("a/o", "b/i"));
        let moved: TransportAddr = "tcp:10.0.0.9:7409".parse().unwrap();
        directory.note_peer(peer.clone(), moved.clone());
        assert_eq!(directory.address_of(&peer), Some(&moved));
        assert_eq!(directory.edges_for(&peer).len(), 1);
    }

    #[test]
    fn a_dial_claim_is_exclusive_until_it_is_released() {
        let mut directory = PeerDirectory::new();
        let peer = DaemonId::generate(None);
        directory.note_peer(peer.clone(), address());
        assert_eq!(directory.claim_dial(&peer), Some(address()));
        assert!(directory.is_dialing(&peer));
        assert_eq!(directory.claim_dial(&peer), None, "already dialling");
        directory.dial_finished(&peer);
        assert!(!directory.is_dialing(&peer));
        assert_eq!(directory.claim_dial(&peer), Some(address()));
    }

    #[test]
    fn claiming_a_dial_for_an_unknown_peer_is_none() {
        let mut directory = PeerDirectory::new();
        assert_eq!(directory.claim_dial(&DaemonId::generate(None)), None);
    }

    #[test]
    fn forgetting_a_dataflow_drops_its_edges_and_then_the_peer() {
        let mut directory = PeerDirectory::new();
        let peer = DaemonId::generate(None);
        directory.note_edge(peer.clone(), address(), route("a/o", "b/i"));
        directory.forget_dataflow(DataflowId::from_u128(9));
        assert_eq!(directory.edges_for(&peer).len(), 1, "another dataflow");

        directory.forget_dataflow(DataflowId::from_u128(1));
        assert!(directory.is_empty(), "the peer had nothing else to do");
    }

    /// A daemon hosting `camera`, with the peer link faked by admitting the
    /// route on the peer table directly — [`crate::peer::PeerManager`]'s own
    /// tests do the same, and the *link* is not what this is about.
    fn producer_daemon(name: &str) -> Daemon {
        use crate::config::{DaemonConfig, ListenConfig, RuntimePaths};
        use astrs_wire::{DataId, NodeSource, NodeSpawnSpec, OutputSpec};

        let root = std::env::temp_dir().join(format!("as-cr-{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let config = DaemonConfig::new(RuntimePaths::under(root))
            .with_listen(ListenConfig::none())
            .with_shm(false);
        let mut daemon = Daemon::new(config).expect("a daemon");

        let dataflow = DataflowId::from_u128(1);
        let mut state = crate::state::DataflowState::new(dataflow, astrs_time::HlcTimestamp::EPOCH);
        state.add_node(
            NodeSpawnSpec::new(
                dataflow,
                astrs_wire::NodeId::new("camera").expect("a legal id"),
                1,
                NodeSource::Dynamic,
            )
            .with_output(OutputSpec::new(DataId::new("image").expect("a legal id"))),
        );
        daemon.state_mut().insert_dataflow(state);
        daemon
    }

    #[tokio::test]
    async fn a_route_is_reopened_when_its_producer_reaches_a_new_generation() {
        let mut daemon = producer_daemon("regen");
        let peer = DaemonId::generate(None);
        let spec = route("camera/image", "detect/frames");
        let key = spec.key.clone();
        daemon
            .peer_directory_mut()
            .note_edge(peer.clone(), address(), spec);

        // A link the peer table believes in, so `setup_route` can mint a
        // handle; the socket itself is irrelevant to the generation stamp.
        daemon.peers_mut().routes_mut().open(
            peer.clone(),
            route("camera/image", "detect/frames"),
            1,
        );
        assert_eq!(
            daemon
                .peers()
                .routes()
                .outbound_for_key(&peer, &key)
                .map(|route| route.generation),
            Some(1)
        );

        // A second reconcile with the generation unchanged leaves it alone.
        daemon.flush_peer_routes(&peer);
        assert_eq!(
            daemon
                .peers()
                .routes()
                .outbound_for_key(&peer, &key)
                .map(|route| route.generation),
            Some(1),
            "a route already open at the right generation is left alone"
        );

        // The producer restarts. The open route still carries generation 1,
        // which is exactly the stamp §12 uses to reject stale messages — so
        // the reconcile must tear it down rather than keep it.
        let camera = astrs_wire::NodeId::new("camera").expect("a legal id");
        {
            let state = daemon
                .state_mut()
                .dataflow_mut(DataflowId::from_u128(1))
                .expect("admitted");
            let node = state.node_mut(&camera).expect("present");
            node.begin_next_generation();
            node.set_generation(5);
        }
        daemon.flush_peer_routes(&peer);
        assert!(
            daemon
                .peers()
                .routes()
                .outbound_for_key(&peer, &key)
                .is_none_or(|route| route.generation == 5),
            "a route stamped with a replaced incarnation must not survive a reconcile"
        );
    }

    #[test]
    fn forgetting_a_peer_removes_it_outright() {
        let mut directory = PeerDirectory::new();
        let peer = DaemonId::generate(None);
        directory.note_edge(peer.clone(), address(), route("a/o", "b/i"));
        directory.forget_peer(&peer);
        assert!(directory.is_empty());
    }
}
