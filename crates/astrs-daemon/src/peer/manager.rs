//! [`PeerManager`] — the peer connection table and the route conversation
//! (§6.4, §7.3).
//!
//! One entry per peer [`DaemonId`], reached by dialling out or by being
//! dialled: a cluster where every daemon dials every other would need a full
//! mesh of configuration, and one where only the coordinator dials would fall
//! over when the coordinator does. Both directions land in the same table, and
//! the table is keyed by *identity* rather than address so a daemon that moves
//! keeps its routes.
//!
//! ```text
//!   connect_peer(id, addr) ──dial──┐
//!                                  ├──► PeerLink ──► install() ──► links[id]
//!   accept loop (bind + serve) ────┘
//!
//!   setup_route()  ─── RouteSetup ───────────────►  handle_frame
//!   handle_frame   ◄── RouteAccept ──────────────   answer_setup()
//!   forward()      ─── Output ───────────────────►  PeerReaction::Deliver
//!                  ─── OutputClosed ─────────────►  PeerReaction::InputClosed
//! ```
//!
//! # What this module does not decide
//!
//! Whether to accept a peer's `RouteSetup` needs the graph: does this daemon
//! host that consumer, is the dataflow still running, does the port type
//! match? None of that belongs here, so an inbound setup surfaces as
//! [`PeerReaction::SetupRequested`] and the event loop answers with
//! [`PeerManager::answer_setup`]. The manager owns *connections and handles*;
//! [`crate::server`] owns *meaning*.
//!
//! # Reconnect semantics (§12)
//!
//! A lost peer takes its routes with it, and every inbound route becomes an
//! `InputClosed` for the local consumer — a circuit breaker surfaced as an
//! ordinary event, exactly as §12 asks. When the peer comes back the routes
//! are set up again from scratch and the consumers see `InputRecovered`; the
//! handles are *not* reused, because a stale `Output` for a recycled handle
//! would be delivered to the wrong consumer.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use astrs_transport::{
    CloseReason, Connection, HandshakeParams, StreamConnection, TransportAddr, backend,
};
use astrs_wire::{
    Compression, DaemonId, DataflowId, Metadata, PeerEvent, PortRef, RouteAcceptance,
    RouteCloseReason, RouteId, RouteSpec, SessionAssignment, SessionId, TypeUrn,
};

use crate::error::{DaemonError, DaemonResult};
use crate::peer::config::PeerConfig;
use crate::peer::link::PeerLink;
use crate::peer::routes::{PeerRouteTable, RemoteRoute};
use crate::session::{DaemonEvent, DaemonHandle};

/// Something the event loop must act on after a peer frame arrived.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum PeerReaction {
    /// A peer wants to open a route to a consumer this daemon hosts.
    ///
    /// The loop answers with [`PeerManager::answer_setup`].
    SetupRequested {
        /// The peer asking.
        daemon: DaemonId,
        /// The handle it minted.
        route_id: RouteId,
        /// What it wants.
        spec: Box<RouteSpec>,
        /// The producer incarnation the route belongs to (§12).
        generation: u64,
        /// The producer's declared port type, if it has one.
        type_urn: Option<TypeUrn>,
    },
    /// A route this daemon opened is now usable.
    RouteEstablished {
        /// The peer.
        daemon: DaemonId,
        /// The handle.
        route_id: RouteId,
        /// The negotiated codec (§6.4).
        compression: Compression,
    },
    /// A route this daemon opened was refused.
    RouteRejected {
        /// The peer.
        daemon: DaemonId,
        /// The handle.
        route_id: RouteId,
        /// Why, as text for the log.
        reason: String,
        /// Whether the same request could succeed a moment later
        /// ([`astrs_wire::RouteRejection::is_transient`]).
        ///
        /// A transient refusal is a race — the peer had not been told about
        /// the dataflow yet, or the consumer's spawn had not reached it — and
        /// the route has already been forgotten so the reconciliation
        /// backstop re-opens it. The caller's job is only to make that
        /// backstop run *soon* rather than at its next scheduled sweep.
        transient: bool,
    },
    /// A payload arrived for a local consumer.
    Deliver {
        /// The dataflow it belongs to.
        dataflow: DataflowId,
        /// The remote producer port.
        source: PortRef,
        /// The local consumer port.
        consumer: PortRef,
        /// The per-route sequence number.
        seq: u64,
        /// The metadata riding beside it (§6.1).
        metadata: Box<Metadata>,
        /// The payload bytes.
        payload: Vec<u8>,
    },
    /// A local consumer's remote input will receive nothing further.
    InputClosed {
        /// The dataflow it belongs to.
        dataflow: DataflowId,
        /// The remote producer port.
        source: PortRef,
        /// The local consumer port.
        consumer: PortRef,
        /// Why.
        reason: RouteCloseReason,
    },
}

impl PeerReaction {
    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::SetupRequested { .. } => "setup_requested",
            Self::RouteEstablished { .. } => "route_established",
            Self::RouteRejected { .. } => "route_rejected",
            Self::Deliver { .. } => "deliver",
            Self::InputClosed { .. } => "input_closed",
        }
    }
}

/// Every peer daemon this daemon talks to (§6.4).
#[derive(Debug)]
pub struct PeerManager {
    /// This daemon's identity, presented in every greeting.
    local: DaemonId,
    /// How it talks to peers.
    config: PeerConfig,
    /// One link per connected peer.
    links: BTreeMap<DaemonId, PeerLink>,
    /// Every cross-host route, in both directions.
    routes: PeerRouteTable,
    /// The peer listener, once bound.
    listener: Option<Arc<astrs_transport::backend::tcp::TcpListener>>,
    /// Where it is listening.
    listen_addr: Option<SocketAddr>,
    /// How many dials succeeded.
    dialled: u64,
    /// How many connections were accepted.
    accepted: u64,
    /// How many frames arrived.
    frames_received: u64,
    /// How many payload bytes arrived.
    bytes_received: u64,
}

impl PeerManager {
    /// A manager for `local`, configured by `config`.
    #[must_use]
    pub fn new(local: DaemonId, config: PeerConfig) -> Self {
        Self {
            local,
            config,
            links: BTreeMap::new(),
            routes: PeerRouteTable::new(),
            listener: None,
            listen_addr: None,
            dialled: 0,
            accepted: 0,
            frames_received: 0,
            bytes_received: 0,
        }
    }

    /// This daemon's identity.
    #[must_use]
    pub const fn local(&self) -> &DaemonId {
        &self.local
    }

    /// How it talks to peers.
    #[must_use]
    pub const fn config(&self) -> &PeerConfig {
        &self.config
    }

    /// Every cross-host route.
    #[must_use]
    pub const fn routes(&self) -> &PeerRouteTable {
        &self.routes
    }

    /// Every cross-host route, mutably.
    ///
    /// The same seam [`crate::server::Daemon::state_mut`] is, and used the
    /// same way: a caller reconciling the table against a plan it owns — the
    /// coordinator-driven route book (§6.4), or a test seeding the state a
    /// socket would otherwise have to produce. Every *ordinary* change to a
    /// route goes through [`PeerManager::setup_route`],
    /// [`PeerManager::answer_setup`] or [`PeerManager::handle_frame`], which
    /// keep the table and the link's logical streams in step.
    pub const fn routes_mut(&mut self) -> &mut PeerRouteTable {
        &mut self.routes
    }

    /// Binds the peer listener named by the configuration.
    ///
    /// Returns the address actually bound, which for a configured port of `0`
    /// is the one the operating system chose.
    ///
    /// # Errors
    ///
    /// - [`DaemonError::NoListener`] if the configuration names no address.
    /// - [`DaemonError::Transport`] if the port cannot be bound.
    pub async fn bind(&mut self) -> DaemonResult<SocketAddr> {
        let Some(addr) = self.config.listen() else {
            return Err(DaemonError::NoListener);
        };
        let listener =
            astrs_transport::backend::tcp::TcpListener::bind(addr, self.config.transport().clone())
                .await?;
        let bound = listener.local_addr()?;
        self.listener = Some(Arc::new(listener));
        self.listen_addr = Some(bound);
        Ok(bound)
    }

    /// Where the peer listener is bound, if it is.
    #[must_use]
    pub const fn listen_addr(&self) -> Option<SocketAddr> {
        self.listen_addr
    }

    /// Starts serving the peer listener.
    ///
    /// Every accepted connection is reported as
    /// [`crate::session::DaemonEvent::PeerAttached`]; a failed handshake is one
    /// bad connection, not a reason to stop listening. Returns whether a
    /// listener was there to serve.
    pub fn spawn_accept_loop(&self, handle: DaemonHandle) -> bool {
        let Some(listener) = self.listener.clone() else {
            return false;
        };
        let acceptor = astrs_transport::acceptor_from_config(
            self.config.transport(),
            self.config.auth().clone(),
            PeerConfig::accepted_roles(),
            true,
        );
        tokio::spawn(async move {
            while handle.is_open() {
                let accepted = listener
                    .accept(&acceptor, SessionAssignment::Fresh(SessionId::generate()))
                    .await;
                match accepted {
                    Ok((connection, channels)) => {
                        let Some(daemon) = peer_identity(&connection) else {
                            tracing::warn!(
                                peer = %connection.peer().describe(),
                                "a peer greeted without a daemon id; refusing"
                            );
                            let _ = connection
                                .close(CloseReason::Protocol {
                                    detail: "a peer greeted without a daemon id".into(),
                                })
                                .await;
                            continue;
                        };
                        let link =
                            PeerLink::establish(daemon, connection, channels, handle.clone());
                        if !handle.send(DaemonEvent::PeerAttached {
                            link: Box::new(link),
                        }) {
                            return;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "a peer connection failed to establish");
                    }
                }
            }
        });
        true
    }

    /// Dials `addr` and installs the resulting link.
    ///
    /// The connection's inbound half is pumped from the moment it is
    /// established, so a peer that sends immediately is not raced.
    ///
    /// # Errors
    ///
    /// [`DaemonError::Transport`] if the dial or the handshake fails.
    pub async fn connect_peer(
        &mut self,
        daemon: DaemonId,
        addr: &TransportAddr,
        handle: DaemonHandle,
    ) -> DaemonResult<()> {
        let params = HandshakeParams::from_config(
            self.config.transport(),
            self.config.identity(&self.local),
            self.config.auth().clone(),
            addr.requires_crc(),
        );
        let (connection, channels) =
            backend::connect(addr, self.config.transport(), &params).await?;
        let link = PeerLink::establish(daemon, connection, channels, handle);
        self.dialled = self.dialled.saturating_add(1);
        self.install(link);
        Ok(())
    }

    /// Installs an established link, replacing any previous one for that peer.
    ///
    /// A replacement is a reconnect: the old link's routes are dropped, which
    /// is what turns a partition into `InputClosed` and its repair into a
    /// fresh setup (§12).
    pub fn install(&mut self, link: PeerLink) -> Vec<RemoteRoute> {
        let daemon = link.daemon().clone();
        let stale = if self.links.contains_key(&daemon) {
            self.routes.remove_peer(&daemon)
        } else {
            Vec::new()
        };
        self.accepted = self.accepted.saturating_add(1);
        self.links.insert(daemon, link);
        stale
    }

    /// Forgets a peer and every route it carried.
    pub fn remove(&mut self, daemon: &DaemonId) -> Vec<RemoteRoute> {
        self.links.remove(daemon);
        self.routes.remove_peer(daemon)
    }

    /// Closes one peer's connection and forgets what it carried (§12).
    ///
    /// The deliberate half of [`PeerManager::remove`]: `remove` drops the
    /// bookkeeping for a link that is *already* gone, while this actually
    /// closes the socket first, so the peer at the other end observes the
    /// partition instead of a connection that quietly stops carrying frames.
    /// The routes come back on their own — the daemon re-dials under
    /// [`crate::server::Daemon::reconcile_peer`] and the consumer sees
    /// `InputRecovered` (§12's circuit-breaker semantics).
    ///
    /// Returns the routes the link carried, so the caller can close the local
    /// consumers' inputs exactly as the abrupt path does.
    pub async fn disconnect(&mut self, daemon: &DaemonId, reason: &str) -> Vec<RemoteRoute> {
        if let Some(mut link) = self.links.remove(daemon) {
            link.close(CloseReason::local(reason.to_owned())).await;
        }
        self.routes.remove_peer(daemon)
    }

    /// Whether a peer is connected.
    #[must_use]
    pub fn is_connected(&self, daemon: &DaemonId) -> bool {
        self.links.get(daemon).is_some_and(|link| !link.is_closed())
    }

    /// One peer's link.
    #[must_use]
    pub fn link(&self, daemon: &DaemonId) -> Option<&PeerLink> {
        self.links.get(daemon)
    }

    /// How many peers are connected.
    #[must_use]
    pub fn len(&self) -> usize {
        self.links.len()
    }

    /// Whether no peers are connected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    /// Every connected peer, in identity order.
    pub fn peers(&self) -> impl Iterator<Item = &DaemonId> {
        self.links.keys()
    }

    /// How many frames have arrived from peers.
    #[must_use]
    pub const fn frames_received(&self) -> u64 {
        self.frames_received
    }

    /// How many payload bytes have arrived from peers.
    #[must_use]
    pub const fn bytes_received(&self) -> u64 {
        self.bytes_received
    }

    /// How many frames the *currently connected* links have written.
    ///
    /// A per-connection view: a link that was replaced by a reconnect took its
    /// counts with it. The process-lifetime figure — the one
    /// [`astrs_wire::DaemonStats::frames_sent`] carries and `astrs top`
    /// displays — is the monotone
    /// [`crate::metrics::names::PEER_FRAMES_SENT_TOTAL`] counter, which is
    /// authoritative. This one answers "how busy is the link in front of me
    /// right now", which is a different and equally useful question.
    #[must_use]
    pub fn frames_sent(&self) -> u64 {
        self.links.values().map(PeerLink::frames_sent).sum()
    }

    /// How many payload bytes the currently connected links have written.
    ///
    /// See [`PeerManager::frames_sent`] for why this and the registry counter
    /// legitimately differ, and which of the two is normative.
    #[must_use]
    pub fn bytes_sent(&self) -> u64 {
        self.links.values().map(PeerLink::bytes_sent).sum()
    }

    /// Writes one event to a peer.
    ///
    /// # Errors
    ///
    /// [`DaemonError::UnknownPeer`] if the peer is not connected, plus
    /// anything [`PeerLink::send`] can return.
    pub fn send(&mut self, daemon: &DaemonId, event: &PeerEvent) -> DaemonResult<()> {
        let Some(link) = self.links.get_mut(daemon) else {
            return Err(DaemonError::UnknownPeer {
                daemon: daemon.clone(),
            });
        };
        link.send(event)
    }

    /// Opens a route to a consumer on `daemon` (§6.4).
    ///
    /// Sends [`astrs_wire::PeerEvent::RouteSetup`] and opens the route's
    /// logical stream so the first payload does not have to wait for one.
    ///
    /// # Errors
    ///
    /// [`DaemonError::UnknownPeer`] if the peer is not connected, plus
    /// anything [`PeerLink::send`] and [`PeerLink::open_route`] can return.
    pub fn setup_route(
        &mut self,
        daemon: &DaemonId,
        spec: RouteSpec,
        generation: u64,
        type_urn: Option<TypeUrn>,
        max_payload_bytes: u64,
    ) -> DaemonResult<RouteId> {
        if !self.links.contains_key(daemon) {
            return Err(DaemonError::UnknownPeer {
                daemon: daemon.clone(),
            });
        }
        let spec = spec.with_compression(self.config.compression());
        let route_id = self.routes.open(daemon.clone(), spec.clone(), generation);
        let descriptor = self
            .routes
            .outbound(daemon, route_id)
            .map(RemoteRoute::descriptor)
            .unwrap_or_default();

        let Some(link) = self.links.get_mut(daemon) else {
            return Err(DaemonError::UnknownPeer {
                daemon: daemon.clone(),
            });
        };
        link.open_route(route_id, &descriptor)?;
        link.send(&PeerEvent::RouteSetup {
            route_id,
            route: spec,
            generation,
            type_urn,
            max_payload_bytes,
            pool_hint_bytes: None,
        })?;
        Ok(route_id)
    }

    /// Answers a peer's [`PeerReaction::SetupRequested`].
    ///
    /// # Errors
    ///
    /// [`DaemonError::UnknownPeer`] if the peer went away in the meantime.
    pub fn answer_setup(
        &mut self,
        daemon: &DaemonId,
        route_id: RouteId,
        spec: RouteSpec,
        generation: u64,
        acceptance: RouteAcceptance,
    ) -> DaemonResult<()> {
        self.routes
            .admit(daemon.clone(), route_id, spec, generation, &acceptance);
        self.send(
            daemon,
            &PeerEvent::RouteAccept {
                route_id,
                acceptance,
            },
        )
    }

    /// Tears one route down, telling the peer.
    ///
    /// # Errors
    ///
    /// As [`PeerManager::send`]; the route is forgotten locally either way,
    /// because a teardown this daemon could not deliver is still a teardown.
    pub fn teardown_route(
        &mut self,
        daemon: &DaemonId,
        route_id: RouteId,
        reason: RouteCloseReason,
    ) -> DaemonResult<()> {
        self.routes.close(daemon, route_id, reason.clone());
        if let Some(link) = self.links.get_mut(daemon) {
            link.close_route(route_id);
        }
        self.send(daemon, &PeerEvent::RouteTeardown { route_id, reason })
    }

    /// Forwards one published message to every established remote route for
    /// `source`.
    ///
    /// Returns how many peers it reached. A route whose queue is full counts
    /// as a drop, not an error: the publish path must not fail a local
    /// producer because a remote consumer is slow (§11.2).
    pub fn forward(
        &mut self,
        dataflow: DataflowId,
        source: &PortRef,
        metadata: &Metadata,
        payload: &[u8],
    ) -> usize {
        let targets = self.routes.established_for(dataflow, source);
        let mut sent = 0;
        for (daemon, route_id) in targets {
            let Some(seq) = self.routes.next_seq(&daemon, route_id) else {
                continue;
            };
            let event = PeerEvent::Output {
                route_id,
                seq,
                metadata: metadata.clone(),
                payload: payload.to_vec(),
            };
            if self.send(&daemon, &event).is_ok() {
                self.routes
                    .record_sent(&daemon, route_id, payload.len() as u64);
                sent += 1;
            }
        }
        sent
    }

    /// Tells every established remote route for `source` that it is finished.
    ///
    /// Returns how many peers were told.
    pub fn close_output(
        &mut self,
        dataflow: DataflowId,
        source: &PortRef,
        reason: RouteCloseReason,
    ) -> usize {
        let targets = self.routes.established_for(dataflow, source);
        let mut told = 0;
        for (daemon, route_id) in targets {
            let final_seq = self
                .routes
                .outbound(&daemon, route_id)
                .map_or(0, |route| route.seq.saturating_sub(1));
            let event = PeerEvent::OutputClosed {
                route_id,
                final_seq,
                reason: reason.clone(),
            };
            if self.send(&daemon, &event).is_ok() {
                told += 1;
            }
        }
        told
    }

    /// Folds one inbound frame into the tables, returning what the loop must
    /// do about it.
    pub fn handle_frame(&mut self, daemon: &DaemonId, event: PeerEvent) -> Vec<PeerReaction> {
        self.frames_received = self.frames_received.saturating_add(1);
        self.bytes_received = self
            .bytes_received
            .saturating_add(event.payload_len() as u64);

        match event {
            PeerEvent::RouteSetup {
                route_id,
                route,
                generation,
                type_urn,
                ..
            } => vec![PeerReaction::SetupRequested {
                daemon: daemon.clone(),
                route_id,
                spec: Box::new(route),
                generation,
                type_urn,
            }],
            PeerEvent::RouteAccept {
                route_id,
                acceptance,
            } => self.on_accept(daemon, route_id, &acceptance),
            PeerEvent::Output {
                route_id,
                seq,
                metadata,
                payload,
            } => self.on_output(daemon, route_id, seq, metadata, payload),
            PeerEvent::OutputClosed {
                route_id, reason, ..
            } => self.on_input_closed(daemon, route_id, reason, false),
            PeerEvent::RouteTeardown { route_id, reason } => {
                self.on_input_closed(daemon, route_id, reason, true)
            }
            PeerEvent::Ping { .. } => {
                if let Some(pong) = event_pong(&event) {
                    let _ = self.send(daemon, &pong);
                }
                Vec::new()
            }
            // `PeerEvent` is `#[non_exhaustive]`: a variant this build does
            // not know is ignored rather than mistaken for one it does.
            _ => Vec::new(),
        }
    }

    /// A peer answered one of this daemon's setups.
    fn on_accept(
        &mut self,
        daemon: &DaemonId,
        route_id: RouteId,
        acceptance: &RouteAcceptance,
    ) -> Vec<PeerReaction> {
        if !self.routes.settle(daemon, route_id, acceptance) {
            return Vec::new();
        }
        match acceptance {
            RouteAcceptance::Accepted { compression, .. } => {
                vec![PeerReaction::RouteEstablished {
                    daemon: daemon.clone(),
                    route_id,
                    compression: *compression,
                }]
            }
            RouteAcceptance::Rejected { reason } => {
                if let Some(link) = self.links.get_mut(daemon) {
                    link.close_route(route_id);
                }
                // A *transient* refusal is a race, not an answer: the peer had
                // not been told about the dataflow yet, or the consumer's
                // spawn had not reached it. Forgetting the route makes the
                // edge look unopened, so `flush_peer_routes` re-opens it on
                // the next reconciliation instead of leaving a `Rejected`
                // entry that `has_outbound` reports as "already open" for
                // ever. Without this, a cluster whose two `Spawn`s and
                // `PeerRoutes` directives arrived in the wrong order lost that
                // edge for the whole run — visible as a consumer that received
                // nothing while its producer published happily.
                let transient = reason.is_transient();
                if transient {
                    self.routes.forget_outbound(daemon, route_id);
                }
                vec![PeerReaction::RouteRejected {
                    daemon: daemon.clone(),
                    route_id,
                    reason: reason.to_string(),
                    transient,
                }]
            }
            // `RouteAcceptance` is `#[non_exhaustive]`: an answer this build
            // does not understand is not retried, because nothing here can
            // tell whether retrying would help.
            _ => vec![PeerReaction::RouteRejected {
                daemon: daemon.clone(),
                route_id,
                reason: "unrecognised route acceptance".into(),
                transient: false,
            }],
        }
    }

    /// A payload arrived on a route a peer opened with this daemon.
    fn on_output(
        &mut self,
        daemon: &DaemonId,
        route_id: RouteId,
        seq: u64,
        metadata: Metadata,
        payload: Vec<u8>,
    ) -> Vec<PeerReaction> {
        let Some(route) = self
            .routes
            .record_received(daemon, route_id, payload.len() as u64)
        else {
            return Vec::new();
        };
        vec![PeerReaction::Deliver {
            dataflow: route.dataflow(),
            source: route.producer().clone(),
            consumer: route.consumer().clone(),
            seq,
            metadata: Box::new(metadata),
            payload,
        }]
    }

    /// A route ended, either half-way (`OutputClosed`) or entirely
    /// (`RouteTeardown`).
    fn on_input_closed(
        &mut self,
        daemon: &DaemonId,
        route_id: RouteId,
        reason: RouteCloseReason,
        forget: bool,
    ) -> Vec<PeerReaction> {
        let route = if forget {
            self.routes.close(daemon, route_id, reason.clone())
        } else {
            self.routes
                .drain(daemon, route_id, 0, reason.clone())
                .cloned()
        };
        let Some(route) = route else {
            return Vec::new();
        };
        if forget && let Some(link) = self.links.get_mut(daemon) {
            link.close_route(route_id);
        }
        vec![PeerReaction::InputClosed {
            dataflow: route.dataflow(),
            source: route.producer().clone(),
            consumer: route.consumer().clone(),
            reason,
        }]
    }

    /// Probes every connected peer (§24.1 `PeerEvent::Ping`).
    ///
    /// Returns how many probes were written.
    pub fn ping_all(&mut self, now: astrs_time::HlcTimestamp) -> usize {
        let peers: Vec<DaemonId> = self.links.keys().cloned().collect();
        let mut sent = 0;
        for (index, daemon) in peers.into_iter().enumerate() {
            let probe = PeerEvent::ping(index as u64, now);
            if self.send(&daemon, &probe).is_ok() {
                sent += 1;
            }
        }
        sent
    }

    /// Drops every link whose connection has ended, returning what they
    /// carried.
    pub fn reap_closed(&mut self) -> Vec<(DaemonId, Vec<RemoteRoute>)> {
        let closed: Vec<DaemonId> = self
            .links
            .iter()
            .filter(|(_, link)| link.is_closed())
            .map(|(daemon, _)| daemon.clone())
            .collect();
        closed
            .into_iter()
            .map(|daemon| {
                let routes = self.remove(&daemon);
                (daemon, routes)
            })
            .collect()
    }

    /// Closes every link and forgets every route.
    pub async fn shutdown(&mut self) {
        for (_, mut link) in std::mem::take(&mut self.links) {
            link.close(CloseReason::local("the daemon is shutting down"))
                .await;
        }
        self.listener = None;
        self.listen_addr = None;
    }
}

/// The peer's [`DaemonId`], from the label it greeted with (§7.2).
fn peer_identity(connection: &StreamConnection) -> Option<DaemonId> {
    connection
        .peer()
        .label
        .as_deref()
        .and_then(|label| label.parse::<DaemonId>().ok())
}

/// The echo for a probe, if it is one.
fn event_pong(event: &PeerEvent) -> Option<PeerEvent> {
    event.pong()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_time::HlcTimestamp;
    use astrs_wire::{AuthToken, Plane, RouteKey, RouteRejection};

    use super::*;

    fn dataflow() -> DataflowId {
        DataflowId::from_u128(4)
    }

    fn manager() -> PeerManager {
        PeerManager::new(
            DaemonId::generate(None),
            PeerConfig::new(AuthToken::from_bytes([9; 32])),
        )
    }

    fn spec() -> RouteSpec {
        RouteSpec::new(RouteKey::new(
            dataflow(),
            "camera/image".parse().unwrap(),
            "detect/frames".parse().unwrap(),
        ))
        .with_plane(Plane::Tcp)
    }

    #[test]
    fn a_fresh_manager_has_no_peers() {
        let manager = manager();
        assert!(manager.is_empty());
        assert_eq!(manager.len(), 0);
        assert!(manager.listen_addr().is_none());
        assert!(manager.peers().next().is_none());
        assert_eq!(manager.frames_received(), 0);
        assert_eq!(manager.frames_sent(), 0);
        assert!(manager.routes().is_empty());
    }

    #[test]
    fn sending_to_an_unconnected_peer_is_a_typed_error() {
        let mut manager = manager();
        let peer = DaemonId::generate(None);
        let error = manager
            .send(&peer, &PeerEvent::ping(1, HlcTimestamp::EPOCH))
            .expect_err("not connected");
        assert!(matches!(error, DaemonError::UnknownPeer { .. }));
        assert!(error.is_client_error());
        assert!(!manager.is_connected(&peer));
    }

    #[test]
    fn setting_up_a_route_without_a_link_is_refused_before_a_handle_is_minted() {
        let mut manager = manager();
        let peer = DaemonId::generate(None);
        assert!(
            manager
                .setup_route(&peer, spec(), 1, None, 1 << 20)
                .is_err()
        );
        assert!(
            manager.routes().is_empty(),
            "no handle was minted for a peer that is not there"
        );
    }

    #[tokio::test]
    async fn binding_without_a_configured_address_is_refused() {
        let mut manager = manager();
        assert!(matches!(manager.bind().await, Err(DaemonError::NoListener)));
    }

    #[tokio::test]
    async fn a_listener_binds_a_free_port_and_serves_it() {
        let mut manager = PeerManager::new(
            DaemonId::generate(None),
            PeerConfig::new(AuthToken::ZERO).with_loopback(0),
        );
        let addr = manager.bind().await.expect("bound");
        assert_ne!(addr.port(), 0, "the operating system chose a port");
        assert_eq!(manager.listen_addr(), Some(addr));

        let (handle, _events) = crate::session::event_channel();
        assert!(manager.spawn_accept_loop(handle));
    }

    #[test]
    fn an_accept_loop_without_a_listener_does_nothing() {
        let manager = manager();
        let (handle, _events) = crate::session::event_channel();
        assert!(!manager.spawn_accept_loop(handle));
    }

    #[test]
    fn an_inbound_setup_is_surfaced_for_the_loop_to_judge() {
        let mut manager = manager();
        let peer = DaemonId::generate(None);
        let reactions = manager.handle_frame(
            &peer,
            PeerEvent::RouteSetup {
                route_id: RouteId::FIRST,
                route: spec(),
                generation: 2,
                type_urn: None,
                max_payload_bytes: 1 << 20,
                pool_hint_bytes: None,
            },
        );

        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].kind_name(), "setup_requested");
        match &reactions[0] {
            PeerReaction::SetupRequested {
                route_id,
                generation,
                ..
            } => {
                assert_eq!(*route_id, RouteId::FIRST);
                assert_eq!(*generation, 2);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(
            manager.routes().is_empty(),
            "nothing is admitted until the loop answers"
        );
        assert_eq!(manager.frames_received(), 1);
    }

    #[test]
    fn an_answer_for_a_disconnected_peer_still_records_the_route() {
        let mut manager = manager();
        let peer = DaemonId::generate(None);
        // The link is gone, so the reply cannot be written, but the inbound
        // half is still filed: a disconnect removes every route of that peer
        // (`remove`/`reap_closed`), so the entry cannot outlive the link it
        // belongs to even though this particular send failed.
        assert!(
            manager
                .answer_setup(
                    &peer,
                    RouteId::FIRST,
                    spec(),
                    1,
                    RouteAcceptance::accepted(Plane::Tcp, Compression::None, 1 << 20),
                )
                .is_err()
        );
        assert_eq!(manager.routes().inbound_count(), 1);
    }

    #[test]
    fn a_payload_on_an_admitted_route_becomes_a_delivery() {
        let mut manager = manager();
        let peer = DaemonId::generate(None);
        let _ = manager.answer_setup(
            &peer,
            RouteId::FIRST,
            spec(),
            1,
            RouteAcceptance::accepted(Plane::Tcp, Compression::None, 1 << 20),
        );

        let reactions = manager.handle_frame(
            &peer,
            PeerEvent::Output {
                route_id: RouteId::FIRST,
                seq: 3,
                metadata: Metadata::default(),
                payload: vec![1, 2, 3, 4],
            },
        );
        assert_eq!(reactions.len(), 1);
        match &reactions[0] {
            PeerReaction::Deliver {
                dataflow: id,
                source,
                consumer,
                seq,
                payload,
                ..
            } => {
                assert_eq!(*id, dataflow());
                assert_eq!(source, &"camera/image".parse::<PortRef>().unwrap());
                assert_eq!(consumer, &"detect/frames".parse::<PortRef>().unwrap());
                assert_eq!(*seq, 3);
                assert_eq!(payload, &[1, 2, 3, 4]);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(manager.bytes_received(), 4);
    }

    #[test]
    fn a_payload_on_an_unknown_route_is_dropped() {
        let mut manager = manager();
        let peer = DaemonId::generate(None);
        let reactions = manager.handle_frame(
            &peer,
            PeerEvent::Output {
                route_id: RouteId::new(77),
                seq: 0,
                metadata: Metadata::default(),
                payload: vec![0; 8],
            },
        );
        assert!(reactions.is_empty());
    }

    #[test]
    fn an_output_closed_closes_the_local_input_without_forgetting_the_route() {
        let mut manager = manager();
        let peer = DaemonId::generate(None);
        let _ = manager.answer_setup(
            &peer,
            RouteId::FIRST,
            spec(),
            1,
            RouteAcceptance::accepted(Plane::Tcp, Compression::None, 1 << 20),
        );

        let reactions = manager.handle_frame(
            &peer,
            PeerEvent::OutputClosed {
                route_id: RouteId::FIRST,
                final_seq: 12,
                reason: RouteCloseReason::ProducerFinished,
            },
        );
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].kind_name(), "input_closed");
        assert!(
            manager.routes().inbound(&peer, RouteId::FIRST).is_some(),
            "the route stays open so the consumer can drain"
        );
    }

    #[test]
    fn a_teardown_forgets_the_route() {
        let mut manager = manager();
        let peer = DaemonId::generate(None);
        let _ = manager.answer_setup(
            &peer,
            RouteId::FIRST,
            spec(),
            1,
            RouteAcceptance::accepted(Plane::Tcp, Compression::None, 1 << 20),
        );

        let reactions = manager.handle_frame(
            &peer,
            PeerEvent::RouteTeardown {
                route_id: RouteId::FIRST,
                reason: RouteCloseReason::DataflowStopped,
            },
        );
        assert_eq!(reactions.len(), 1);
        assert!(manager.routes().is_empty());
    }

    #[test]
    fn an_acceptance_for_a_route_nobody_opened_is_ignored() {
        let mut manager = manager();
        let peer = DaemonId::generate(None);
        let reactions = manager.handle_frame(
            &peer,
            PeerEvent::RouteAccept {
                route_id: RouteId::new(3),
                acceptance: RouteAcceptance::accepted(Plane::Tcp, Compression::None, 1 << 20),
            },
        );
        assert!(reactions.is_empty());
    }

    #[test]
    fn a_rejection_is_reported_with_its_text() {
        let mut manager = manager();
        let peer = DaemonId::generate(None);
        // Open an outbound route directly on the table: the manager refuses to
        // mint a handle without a link, and this test is about the answer.
        let route_id = manager.routes.open(peer.clone(), spec(), 1);

        let reactions = manager.handle_frame(
            &peer,
            PeerEvent::RouteAccept {
                route_id,
                acceptance: RouteAcceptance::Rejected {
                    reason: RouteRejection::Other {
                        message: "the dataflow is stopping".into(),
                    },
                },
            },
        );
        assert_eq!(reactions.len(), 1);
        match &reactions[0] {
            PeerReaction::RouteRejected { reason, .. } => {
                assert!(reason.contains("stopping"), "{reason}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn an_acceptance_establishes_the_route_with_its_codec() {
        let mut manager = manager();
        let peer = DaemonId::generate(None);
        let route_id = manager.routes.open(peer.clone(), spec(), 1);

        let reactions = manager.handle_frame(
            &peer,
            PeerEvent::RouteAccept {
                route_id,
                acceptance: RouteAcceptance::accepted(Plane::Tcp, Compression::Lz4, 1 << 20),
            },
        );
        match &reactions[0] {
            PeerReaction::RouteEstablished { compression, .. } => {
                assert_eq!(*compression, Compression::Lz4);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(manager.routes().established_count(), 1);
    }

    #[test]
    fn forwarding_without_an_established_route_reaches_nobody() {
        let mut manager = manager();
        let source: PortRef = "camera/image".parse().unwrap();
        assert_eq!(
            manager.forward(dataflow(), &source, &Metadata::default(), b"frame"),
            0
        );
    }

    #[test]
    fn a_ping_is_answered_without_a_reaction() {
        let mut manager = manager();
        let peer = DaemonId::generate(None);
        let reactions = manager.handle_frame(&peer, PeerEvent::ping(1, HlcTimestamp::EPOCH));
        assert!(reactions.is_empty());
        assert_eq!(manager.frames_received(), 1);
    }

    #[test]
    fn an_echo_is_not_answered_again() {
        let echo = PeerEvent::Ping {
            nonce: 1,
            sent_at: HlcTimestamp::EPOCH,
            is_reply: true,
        };
        assert!(event_pong(&echo).is_none());
    }

    #[test]
    fn removing_a_peer_takes_its_routes() {
        let mut manager = manager();
        let peer = DaemonId::generate(None);
        manager.routes.open(peer.clone(), spec(), 1);
        let removed = manager.remove(&peer);
        assert_eq!(removed.len(), 1);
        assert!(manager.routes().is_empty());
    }

    #[test]
    fn pinging_with_no_peers_writes_nothing() {
        let mut manager = manager();
        assert_eq!(manager.ping_all(HlcTimestamp::EPOCH), 0);
        assert!(manager.reap_closed().is_empty());
    }

    #[test]
    fn reactions_have_distinct_labels() {
        let reactions = [
            PeerReaction::SetupRequested {
                daemon: DaemonId::generate(None),
                route_id: RouteId::FIRST,
                spec: Box::new(spec()),
                generation: 1,
                type_urn: None,
            },
            PeerReaction::RouteEstablished {
                daemon: DaemonId::generate(None),
                route_id: RouteId::FIRST,
                compression: Compression::None,
            },
            PeerReaction::RouteRejected {
                daemon: DaemonId::generate(None),
                route_id: RouteId::FIRST,
                reason: "no".into(),
                transient: false,
            },
            PeerReaction::Deliver {
                dataflow: dataflow(),
                source: "camera/image".parse().unwrap(),
                consumer: "detect/frames".parse().unwrap(),
                seq: 0,
                metadata: Box::new(Metadata::default()),
                payload: Vec::new(),
            },
            PeerReaction::InputClosed {
                dataflow: dataflow(),
                source: "camera/image".parse().unwrap(),
                consumer: "detect/frames".parse().unwrap(),
                reason: RouteCloseReason::ProducerFinished,
            },
        ];
        let mut labels: Vec<&str> = reactions.iter().map(PeerReaction::kind_name).collect();
        let count = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), count);
    }
}
