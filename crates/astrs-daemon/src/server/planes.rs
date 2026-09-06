//! Where the planes meet the loop (§6.2, §6.3, §6.4, §12, §13).
//!
//! [`crate::shm`], [`crate::peer`], [`crate::health`] and [`crate::tap`] are
//! each a self-contained state machine with no I/O and no knowledge of the
//! graph. This module is the third half of
//! [`crate::server::core::Daemon`] — after [`crate::server::handlers`] — and
//! its job is to hold those four against the daemon's own state and turn their
//! answers into messages:
//!
//! | The plane says | The loop does |
//! |---|---|
//! | [`crate::shm::UpgradeAction::Offer`] | `NodeEvent::RouteUpgrade` to the producer |
//! | [`crate::shm::UpgradeAction::Downgrade`] | `NodeEvent::RouteDowngrade`, and the reliable path resumes |
//! | a segment's producer died | `InputClosed` to every consumer of that output |
//! | [`crate::peer::PeerReaction::Deliver`] | push into the local consumer's mailbox |
//! | [`crate::peer::PeerReaction::SetupRequested`] | check the graph, answer `RouteAccept` |
//! | [`crate::health::HealthExpiry`] | fail the node with `HealthCheckTimeout` |
//! | a tap captured a frame | `DaemonEvent::TopicTapData` to the report sink |
//!
//! # Why it is one module and not four
//!
//! Every one of those rows needs `&mut Daemon`: the route table to find the
//! consumers, the mailboxes to deliver into, the session index to reach a
//! socket. Splitting them across four modules would mean four sets of
//! `pub(crate)` accessors into the same state and no clearer boundary than the
//! table above. The *decisions* are separated — which is where the testing
//! leverage is — and the *plumbing* is here.

use std::collections::BTreeMap;
use std::time::Instant;

use astrs_wire::{
    Compression, DaemonId, DataId, DataflowId, DurationMs, Metadata, NodeEvent, NodeExitCause,
    NodeId, PeerEvent, Plane, PortRef, RouteAcceptance, RouteCloseReason, RouteDowngradeReason,
    RouteId, RouteRejection, RouteSpec, WireDecode,
};

use crate::health::NodeSampleRequest;
use crate::peer::{PeerLink, PeerReaction};
use crate::server::core::Daemon;
use crate::shm::{ConsumerFacts, OutputKey, UpgradeAction};
use crate::state::{DeliveryPlane, is_virtual_port};

/// How many messages one bridged drain forwards per publish notification.
const BRIDGE_DRAIN: usize = 64;

/// One node's shared-memory occupancy, summed across every ring it produces
/// (§6.2, §13).
///
/// Summed rather than per-ring because that is the question `astrs top` asks:
/// "is this node about to start falling back?" A node with two rings, one
/// full and one empty, is at 50% and one publish away from a fallback, and
/// both halves of that sentence come from the sum.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ShmOccupancy {
    /// Slots published and not yet reclaimed.
    pub(crate) in_use: u32,
    /// Slots those rings hold in total.
    pub(crate) total: u32,
    /// Publishes those rings have refused, cumulative (§6.2).
    pub(crate) fallbacks: u64,
}

impl Daemon {
    // ───────────────────────────── health (§12) ─────────────────────────────

    /// Fails every node that missed its liveness deadline.
    pub(crate) fn fire_health(&mut self, now: Instant) {
        for expiry in self.health.expired(now) {
            self.metrics.record_health_timeout();
            let cause = expiry.cause();
            tracing::warn!(
                dataflow = %expiry.dataflow,
                node = %expiry.node,
                after = ?expiry.after,
                "node missed its liveness deadline"
            );
            self.fail_node(expiry.dataflow, &expiry.node, expiry.generation, cause);
        }
    }

    /// Records that a node spoke, which is the liveness signal (§12).
    pub(crate) fn touch_health(&mut self, session: astrs_wire::SessionId, now: Instant) {
        let Some(binding) = self.state.session(session) else {
            return;
        };
        let (dataflow, node) = (binding.dataflow, binding.node.clone());
        self.health.touch(dataflow, &node, now);
    }

    /// Starts watching a node that has just registered.
    pub(crate) fn arm_health(&mut self, dataflow: DataflowId, node: &NodeId, now: Instant) {
        let (generation, timeout) = match self
            .state
            .dataflow(dataflow)
            .and_then(|state| state.node(node))
        {
            Some(state) => (
                state.generation(),
                state
                    .spec()
                    .health_check_timeout
                    .map(DurationMs::to_duration),
            ),
            None => return,
        };
        self.health
            .arm(dataflow, node.clone(), generation, timeout, now);
    }

    /// Records that a node is waiting on the daemon for its next event.
    pub(crate) fn park_health(&mut self, session: astrs_wire::SessionId, now: Instant) {
        let Some(binding) = self.state.session(session) else {
            return;
        };
        let (dataflow, node) = (binding.dataflow, binding.node.clone());
        self.health.park(dataflow, &node, now);
    }

    // ─────────────────────────── shared memory (§6.3) ───────────────────────

    /// Recomputes which consumers of one producer port belong on a ring, and
    /// creates or releases its segment accordingly.
    ///
    /// Called whenever the answer can change: a consumer subscribes, a node
    /// registers, a tap starts, a producer restarts. Public because the
    /// dynamic-topology verbs (§17 `graph add-edge` / `remove-edge`) change the
    /// answer too, and an embedder editing the route table directly must be
    /// able to say so.
    pub fn plan_shm_output(&mut self, dataflow: DataflowId, source: &PortRef) {
        if !self.shm.is_enabled() || is_virtual_port(source) {
            return;
        }
        let key = OutputKey::from_port(dataflow, source);
        let Some(state) = self.state.dataflow(dataflow) else {
            return;
        };
        let Some(producer) = state.node(&key.node) else {
            return;
        };
        let generation = producer.generation();
        let pool_size = producer
            .spec()
            .output(&key.output)
            .map_or(astrs_wire::DEFAULT_SHM_POOL_SIZE, |spec| spec.pool_size());
        let tapped = self.taps.taps_output(dataflow, source);

        let consumers: Vec<(NodeId, ConsumerFacts)> = state
            .routes()
            .consumers(source)
            .iter()
            .map(|consumer| {
                let mut facts = ConsumerFacts::same_host();
                facts.tap_active = tapped;
                match state.node(&consumer.node) {
                    Some(node) => {
                        facts.is_dynamic = node.is_dynamic();
                        facts.is_unregistered = !node.is_registered();
                    }
                    // A consumer with no local node state is on another
                    // machine: the route table carries the edge, but the
                    // process is not this daemon's to reason about.
                    None => facts.is_remote = true,
                }
                if matches!(consumer.plane, DeliveryPlane::Remote { .. }) {
                    facts.is_remote = true;
                }
                (consumer.node.clone(), facts)
            })
            .collect();

        match self
            .shm
            .plan_output(key.clone(), generation, &consumers, pool_size)
        {
            // A ring exists and every consumer may map it: tell each consumer
            // which segment its input reads from, so it can attach. This is
            // the *first* half of §6.3 — the producer is offered its own
            // upgrade only once the daemon has observed those attachments in
            // the segment's consumer table.
            Ok(Some(spec)) => {
                let actions = self.plan_input_routes(dataflow, source, &spec);
                self.apply_upgrade_actions(actions);
            }
            // No ring: nobody consumes this output, or one consumer cannot map
            // it. Any consumer previously told to read one must be told to
            // stop, or it keeps a mapping the daemon has stopped feeding.
            Ok(None) => {
                let actions = self.revoke_input_routes(
                    &key,
                    RouteDowngradeReason::DaemonRequest {
                        message: "this output has no shared-memory ring".into(),
                    },
                );
                self.apply_upgrade_actions(actions);
            }
            Err(error) => {
                tracing::debug!(%source, %error, "no shared-memory ring for this output");
            }
        }
    }

    /// The [`UpgradeAction::OfferInput`]s one plan pass owes the consumers of
    /// `source` (§6.3, consumer side).
    ///
    /// Empty when every consumer has already been told about this generation,
    /// which is the common case: [`Daemon::plan_shm_output`] runs whenever the
    /// answer *could* change, and a re-offer would make a consumer detach and
    /// re-attach its ring for nothing.
    fn plan_input_routes(
        &self,
        dataflow: DataflowId,
        source: &PortRef,
        segment: &astrs_wire::ShmSegmentSpec,
    ) -> Vec<UpgradeAction> {
        let key = OutputKey::from_port(dataflow, source);
        let Some(state) = self.state.dataflow(dataflow) else {
            return Vec::new();
        };
        state
            .routes()
            .consumers(source)
            .iter()
            .filter_map(|consumer| {
                let route = crate::shm::InputRouteKey::new(
                    key.clone(),
                    consumer.node.clone(),
                    consumer.spec.id.clone(),
                );
                self.shm
                    .input_routes()
                    .needs_offer(&route, segment.generation)
                    .then(|| UpgradeAction::OfferInput {
                        key: key.clone(),
                        consumer: consumer.node.clone(),
                        input: consumer.spec.id.clone(),
                        segment: Box::new(segment.clone()),
                    })
            })
            .collect()
    }

    /// The [`UpgradeAction::RevokeInput`]s owed to every consumer that was
    /// told to read `key`'s ring, clearing the ledger as it goes.
    fn revoke_input_routes(
        &mut self,
        key: &OutputKey,
        reason: RouteDowngradeReason,
    ) -> Vec<UpgradeAction> {
        self.shm
            .input_routes_mut()
            .take_all(key)
            .into_iter()
            .map(|route| UpgradeAction::RevokeInput {
                key: route.output,
                consumer: route.consumer,
                input: route.input,
                reason: reason.clone(),
            })
            .collect()
    }

    /// One maintenance pass over the shared-memory plane and the peer links.
    pub(crate) fn poll_planes(&mut self, now: Instant) {
        if self.shm.is_enabled() {
            let pids = self.pid_index();
            let outcome = self.shm.poll(&pids, now);
            for key in outcome.closed {
                self.close_shm_output(&key);
            }
            self.apply_upgrade_actions(outcome.actions);
        }
        for (daemon, routes) in self.peers.reap_closed() {
            tracing::warn!(peer = %daemon, routes = routes.len(), "peer connection lost");
            self.close_remote_inputs(&routes);
        }
        self.publish_gauges();
    }

    /// The process id of every live node, so an entry in a segment's consumer
    /// table can be matched to a graph node.
    fn pid_index(&self) -> BTreeMap<u32, NodeId> {
        let mut index = BTreeMap::new();
        for state in self.state.dataflows() {
            for node in state.nodes() {
                if let Some(pid) = node.pid() {
                    index.insert(pid, node.id().clone());
                }
            }
        }
        index
    }

    /// Sends the slow-start messages a plane pass produced, at either end of
    /// the route (§6.3).
    pub(crate) fn apply_upgrade_actions(&mut self, actions: Vec<UpgradeAction>) {
        for action in actions {
            match action {
                UpgradeAction::Offer {
                    key,
                    segment,
                    consumers,
                } => {
                    self.metrics.record_route_upgrade();
                    tracing::info!(
                        node = %key.node,
                        output = %key.output,
                        segment = %segment.name,
                        "offering the shared-memory plane"
                    );
                    let sent = self.send_to(
                        key.dataflow,
                        &key.node,
                        NodeEvent::RouteUpgrade {
                            output: key.output.clone(),
                            segment: *segment,
                            consumers,
                        },
                    );
                    if !sent {
                        // The producer has not registered yet, or its session
                        // just ended. Leaving the route `Offered` would stall
                        // the plane for the whole acknowledgement timeout
                        // waiting on a question nobody was asked.
                        self.shm.withdraw_offer(&key);
                    }
                }
                UpgradeAction::Downgrade { key, reason } => {
                    self.metrics.record_route_downgrade();
                    if reason.is_capacity_pressure() {
                        self.metrics.record_shm_fallback();
                    }
                    self.set_route_planes(&key, false);
                    // The consumers stay attached, deliberately. A consumer's
                    // attachment follows the *segment*, not the producer's
                    // plane: a live ring whose producer is temporarily back on
                    // the daemon path costs the consumer nothing (it simply
                    // reads nothing, and §6.2's inline delivery reaches it),
                    // and it is the daemon's *observation* of that attachment
                    // that lets §6.3 offer the producer its upgrade again.
                    // Revoking here deadlocked exactly that: every transient
                    // downgrade — an unanswered offer, a debug tap, pool
                    // pressure — detached the consumers, and nothing put them
                    // back. Revocation belongs where the ring itself goes
                    // away: `close_shm_output` and `plan_shm_output`'s
                    // no-ring arm.
                    self.send_to(
                        key.dataflow,
                        &key.node,
                        NodeEvent::RouteDowngrade {
                            output: key.output.clone(),
                            reason,
                        },
                    );
                }
                UpgradeAction::OfferInput {
                    key,
                    consumer,
                    input,
                    segment,
                } => {
                    let generation = segment.generation;
                    tracing::info!(
                        consumer = %consumer,
                        input = %input,
                        source = %key.port(),
                        segment = %segment.name,
                        "telling a consumer which ring its input reads"
                    );
                    let sent = self.send_to(
                        key.dataflow,
                        &consumer,
                        NodeEvent::InputRouteUpgrade {
                            input: input.clone(),
                            source: key.port(),
                            segment: *segment,
                            consumer: PortRef::new(consumer.clone(), input.clone()),
                        },
                    );
                    // Recorded only on a successful send: a consumer with no
                    // live session has no sink, and remembering an offer it
                    // never received would silence every later attempt.
                    if sent {
                        self.shm.input_routes_mut().record_offer(
                            crate::shm::InputRouteKey::new(key, consumer, input),
                            generation,
                        );
                    }
                }
                UpgradeAction::RevokeInput {
                    key,
                    consumer,
                    input,
                    reason,
                } => {
                    tracing::debug!(
                        consumer = %consumer,
                        input = %input,
                        source = %key.port(),
                        %reason,
                        "taking a consumer input off the shared-memory plane"
                    );
                    self.send_to(
                        key.dataflow,
                        &consumer,
                        NodeEvent::InputRouteDowngrade { input, reason },
                    );
                }
            }
        }
    }

    /// Records a producer's answer to a [`NodeEvent::RouteUpgrade`] (§6.3).
    pub(crate) fn apply_upgrade_ack(
        &mut self,
        session: astrs_wire::SessionId,
        output: &DataId,
        accepted: bool,
        reason: Option<String>,
    ) {
        let Some(binding) = self.state.session(session).cloned() else {
            return;
        };
        let key = OutputKey::new(binding.dataflow, binding.node.clone(), output.clone());
        if !self.shm.acknowledge(&key, accepted, reason.clone()) {
            // A stale acknowledgement: the offer it answers was withdrawn.
            return;
        }
        if accepted {
            self.set_route_planes(&key, true);
        } else {
            tracing::info!(
                node = %key.node,
                output = %key.output,
                reason = reason.unwrap_or_default(),
                "the producer refused the shared-memory plane"
            );
        }
    }

    /// Moves every consumer of one output on or off the shared-memory plane in
    /// the route table, so the fan-out stops (or resumes) copying.
    fn set_route_planes(&mut self, key: &OutputKey, upgraded: bool) {
        let source = key.port();
        let plane = if upgraded {
            match self.shm.registry().spec(key) {
                Some(spec) => DeliveryPlane::Shm {
                    segment: spec.name,
                    generation: spec.generation,
                },
                None => return,
            }
        } else {
            DeliveryPlane::Local
        };
        let Some(state) = self.state.dataflow_mut(key.dataflow) else {
            return;
        };
        let consumers: Vec<NodeId> = state
            .routes()
            .consumers(&source)
            .iter()
            .map(|consumer| consumer.node.clone())
            .collect();
        for consumer in consumers {
            state
                .routes_mut()
                .set_plane(&source, &consumer, plane.clone());
        }
    }

    /// A segment went away: its consumers' inputs close (§6.2).
    ///
    /// The generation is the producer incarnation that died, because that is
    /// the field a consumer uses to discard the state it built from it (§12);
    /// reporting a constant would make it worse than absent.
    fn close_shm_output(&mut self, key: &OutputKey) {
        let generation = self
            .state
            .dataflow(key.dataflow)
            .and_then(|state| state.node(&key.node))
            .map_or(0, crate::state::NodeState::generation);
        self.set_route_planes(key, false);
        // The ledger is cleared, but **no** `InputRouteDowngrade` is sent: the
        // `InputClosed` below already takes the consumer off the ring, and it
        // is the message that must arrive *last*, behind the frames the
        // producer committed before it went (§6.2: mark closed, let consumers
        // drain, unlink). Sending both split one ordering into two racing
        // paths — the detach drained the ring on one thread while the closure
        // was queued on another — and the last frame of a stream was lost
        // whenever the closure won.
        let _forgotten = self.shm.input_routes_mut().take_all(key);
        let source = key.port();
        self.close_source(
            key.dataflow,
            &source,
            RouteCloseReason::ProducerCrashed { generation },
        );
    }

    /// Drains a ring the daemon still bridges and fans the result out.
    ///
    /// The path a producer takes when it publishes a slot reference
    /// ([`astrs_wire::OutputPayload::Shm`]) while a downgrade is in flight:
    /// the bytes are in the ring, the daemon reads them, and the consumers see
    /// an ordinary `Input`.
    pub(crate) fn drain_bridged(
        &mut self,
        dataflow: DataflowId,
        node: &NodeId,
        output: &DataId,
        generation: u64,
    ) -> usize {
        let key = OutputKey::new(dataflow, node.clone(), output.clone());
        if !self.shm.bridge().is_attached(&key) && !self.shm.attach_bridge(&key) {
            self.shm.record_fallback();
            self.metrics.record_shm_fallback();
            return 0;
        }
        if self.shm.bridge().generation_of(&key) != Some(generation) {
            // A slot reference minted before a restart: the ring it names is
            // gone, and delivering from the current one would be a lie.
            self.shm.record_fallback();
            self.metrics.record_shm_fallback();
            return 0;
        }

        let drained = self.shm.bridge_mut().drain(&key, BRIDGE_DRAIN);
        let source = key.port();
        let mut delivered = 0;
        for message in drained {
            let metadata = self.metadata_of(&message);
            // Out of the ring: every consumer attached to it already has these
            // bytes, so only the daemon-mediated ones are delivered to.
            self.fan_out(
                dataflow,
                &source,
                metadata,
                message.payload,
                crate::local::PayloadOrigin::Ring,
            );
            delivered += 1;
        }
        delivered
    }

    /// The metadata a bridged message carried, or a fresh stamp when it
    /// carried none.
    ///
    /// The producer commits its [`astrs_wire::Metadata`] into the slot's
    /// metadata region beside the payload (§6.1, §6.2), and it is *not*
    /// replaceable: `request_id`, `goal_id`, `seq` and `_schema_hash` are what
    /// make a service exchange correlate (§9.4) and a schema drift detectable.
    /// Re-stamping would deliver the bytes and lose the message.
    fn metadata_of(&self, message: &crate::shm::BridgedMessage) -> Metadata {
        if message.metadata.is_empty() {
            return Metadata::new(self.clock.now());
        }
        match Metadata::decode_exact(&message.metadata) {
            Ok(metadata) => metadata,
            Err(error) => {
                // A slot whose metadata will not decode is a producer running a
                // different build, or a corrupt region. The payload is still
                // deliverable, so it goes out with a fresh stamp rather than
                // being dropped.
                tracing::warn!(%error, "undecodable slot metadata; re-stamping");
                Metadata::new(self.clock.now())
            }
        }
    }

    /// Forces one output back onto the reliable daemon path (§6.3).
    ///
    /// The operator-facing form of a downgrade — a rolling reconfiguration, a
    /// `graph replace-node`, an operator taking a ring out of service. The
    /// plane attaches the daemon's own reader first, so anything the producer
    /// commits between the instruction and its switch is still delivered.
    ///
    /// Returns whether the output was on the ring to begin with.
    pub fn downgrade_output(
        &mut self,
        dataflow: DataflowId,
        source: &PortRef,
        reason: RouteDowngradeReason,
    ) -> bool {
        let key = OutputKey::from_port(dataflow, source);
        match self.shm.downgrade(&key, reason) {
            Some(action) => {
                self.apply_upgrade_actions(vec![action]);
                true
            }
            None => false,
        }
    }

    // ───────────────────────────── peers (§6.4) ─────────────────────────────

    /// Installs a peer connection the dial or accept task established.
    pub(crate) fn handle_peer_attached(&mut self, link: PeerLink) {
        let daemon = link.daemon().clone();
        tracing::info!(peer = %daemon, addr = %link.addr(), "peer connected");
        let stale = self.peers.install(link);
        self.close_remote_inputs(&stale);
        self.publish_gauges();
    }

    /// Tells every peer that this daemon's routes are going away (§12).
    ///
    /// A *clean* goodbye, unlike the abrupt link-loss path: the peer sees a
    /// `RouteTeardown` carrying
    /// [`RouteCloseReason::DaemonShutdown`] and reports it to its consumers as
    /// an expected closure, rather than inferring a partition from a socket
    /// that stopped answering. Returns how many routes were torn down.
    pub fn teardown_peer_routes(&mut self) -> usize {
        let routes: Vec<(DaemonId, RouteId)> = self
            .peers
            .routes()
            .all()
            .map(|route| (route.daemon.clone(), route.route_id))
            .collect();
        let count = routes.len();
        for (daemon, route_id) in routes {
            // A teardown this daemon could not deliver is still a teardown: the
            // route is forgotten locally either way.
            let _ = self
                .peers
                .teardown_route(&daemon, route_id, RouteCloseReason::DaemonShutdown);
        }
        count
    }

    /// Closes every peer connection, waiting for the goodbye to be written.
    ///
    /// Separate from [`Daemon::begin_shutdown`] because closing a connection
    /// is an `await` and the shutdown path is driven from a synchronous
    /// handler; [`Daemon::run`] performs it once its loop has ended.
    pub async fn close_peers(&mut self) {
        self.peers.shutdown().await;
    }

    /// Handles a peer connection ending (§12 peer partition).
    pub(crate) fn handle_peer_lost(&mut self, daemon: &DaemonId, reason: &str) {
        tracing::warn!(peer = %daemon, reason, "peer disconnected");
        let routes = self.peers.remove(daemon);
        self.close_remote_inputs(&routes);
        self.publish_gauges();
    }

    /// Tells local consumers that a peer's routes are gone.
    ///
    /// The reason is always [`RouteCloseReason::PlaneFailed`] because this is
    /// the *abrupt* path: a link that stopped answering. A peer shutting down
    /// cleanly sends a `RouteTeardown` first, which arrives as an ordinary
    /// frame and carries its own reason (§12).
    fn close_remote_inputs(&mut self, routes: &[crate::peer::RemoteRoute]) {
        for route in routes {
            let dataflow = route.dataflow();
            let consumer = route.consumer().clone();
            let source = route.producer().clone();
            let node = consumer.node().clone();
            let input = consumer.port().clone();
            if self
                .state
                .dataflow(dataflow)
                .and_then(|state| state.node(&node))
                .is_none()
            {
                continue;
            }
            if let Some(state) = self.state.dataflow_mut(dataflow)
                && let Some(node_state) = state.node_mut(&node)
            {
                node_state.close_input(&input);
            }
            self.mailbox(dataflow, &node).push(
                &input,
                NodeEvent::InputClosed {
                    id: input.clone(),
                    source,
                    reason: RouteCloseReason::PlaneFailed {
                        plane: Plane::Tcp,
                        message: "the peer connection ended".into(),
                    },
                },
            );
            self.propagate_input_closure(dataflow);
        }
    }

    /// Handles one frame from a peer daemon.
    pub(crate) fn handle_peer_frame(&mut self, daemon: &DaemonId, event: PeerEvent) {
        self.metrics
            .record_peer_received(event.payload_len() as u64);
        for reaction in self.peers.handle_frame(daemon, event) {
            self.apply_peer_reaction(reaction);
        }
    }

    /// Applies one peer reaction.
    fn apply_peer_reaction(&mut self, reaction: PeerReaction) {
        match reaction {
            PeerReaction::SetupRequested {
                daemon,
                route_id,
                spec,
                generation,
                type_urn,
            } => self.answer_route_setup(&daemon, route_id, *spec, generation, type_urn.is_some()),
            PeerReaction::RouteEstablished {
                daemon, route_id, ..
            } => {
                tracing::info!(peer = %daemon, route = %route_id.get(), "remote route established");
                self.publish_gauges();
            }
            PeerReaction::RouteRejected {
                daemon,
                route_id,
                reason,
                transient,
            } => {
                tracing::warn!(
                    peer = %daemon,
                    route = %route_id.get(),
                    reason,
                    transient,
                    "remote route refused"
                );
                // A transient refusal is a race between this daemon's
                // `PeerRoutes` directive and the peer's own `Spawn`, and the
                // route has already been forgotten. Arming the backstop for
                // the next tick rather than its regular sweep is what keeps a
                // graph that finishes in a quarter of a second from losing the
                // edge for its whole life (`PEER_RECONCILE_INTERVAL` is 500 ms
                // — longer than several of this workspace's own example
                // dataflows take to run).
                if transient {
                    self.cluster_mut().reconcile_soon();
                }
            }
            PeerReaction::Deliver {
                dataflow,
                source,
                consumer,
                metadata,
                payload,
                ..
            } => self.deliver_remote(dataflow, &source, &consumer, *metadata, payload),
            PeerReaction::InputClosed {
                dataflow,
                source,
                consumer,
                reason,
            } => self.close_remote_input(dataflow, &source, &consumer, reason),
        }
    }

    /// Decides whether to accept a peer's `RouteSetup` and answers it.
    fn answer_route_setup(
        &mut self,
        daemon: &DaemonId,
        route_id: RouteId,
        spec: RouteSpec,
        generation: u64,
        _typed: bool,
    ) {
        let dataflow = spec.key.dataflow;
        let consumer = spec.key.consumer.clone();
        let source = spec.key.producer.clone();
        let acceptance = self.judge_route(dataflow, &consumer);
        let accepted = acceptance.is_accepted();
        if let Err(error) = self
            .peers
            .answer_setup(daemon, route_id, spec, generation, acceptance)
        {
            tracing::warn!(peer = %daemon, %error, "could not answer a route setup");
        }
        if accepted {
            self.recover_remote_input(dataflow, &source, &consumer, generation);
        }
        self.publish_gauges();
    }

    /// Tells a local consumer that a remote input it had lost is live again.
    ///
    /// §12's circuit-breaker semantics: a peer partition closes the input, and
    /// its repair reopens it as an ordinary event the application decides what
    /// to do about. Sending it only when the input was *actually* closed is
    /// what keeps a first-time route setup from looking like a recovery.
    fn recover_remote_input(
        &mut self,
        dataflow: DataflowId,
        source: &PortRef,
        consumer: &PortRef,
        generation: u64,
    ) {
        let node = consumer.node().clone();
        let input = consumer.port().clone();
        let reopened = self
            .state
            .dataflow_mut(dataflow)
            .and_then(|state| state.node_mut(&node))
            .is_some_and(|node_state| node_state.reopen_input(&input));
        if !reopened {
            return;
        }
        tracing::info!(%consumer, %source, "a remote input recovered");
        self.mailbox(dataflow, &node).push(
            &input,
            NodeEvent::InputRecovered {
                id: input.clone(),
                source: source.clone(),
                generation,
            },
        );
    }

    /// Whether this daemon can serve a route to `consumer`.
    fn judge_route(&self, dataflow: DataflowId, consumer: &PortRef) -> RouteAcceptance {
        let Some(state) = self.state.dataflow(dataflow) else {
            return RouteAcceptance::Rejected {
                reason: RouteRejection::UnknownDataflow,
            };
        };
        if self.state.is_shutting_down() || state.status().is_terminal() {
            return RouteAcceptance::Rejected {
                reason: RouteRejection::ShuttingDown,
            };
        }
        let hosts_consumer = state
            .node(consumer.node())
            .is_some_and(|node| node.spec().input(consumer.port()).is_some());
        if !hosts_consumer {
            return RouteAcceptance::Rejected {
                reason: RouteRejection::UnknownPort {
                    port: consumer.clone(),
                },
            };
        }
        RouteAcceptance::accepted(
            Plane::Tcp,
            self.config.peer().compression(),
            self.config.limits().max_payload_bytes,
        )
    }

    /// Pushes a payload that arrived from a peer into its consumer's mailbox.
    fn deliver_remote(
        &mut self,
        dataflow: DataflowId,
        source: &PortRef,
        consumer: &PortRef,
        metadata: Metadata,
        payload: Vec<u8>,
    ) {
        let node = consumer.node().clone();
        let input = consumer.port().clone();
        let report = self.mailbox(dataflow, &node).push(
            &input,
            NodeEvent::Input {
                id: input.clone(),
                source: source.clone(),
                metadata,
                payload,
            },
        );
        if report.dropped_something() {
            self.metrics.record_queue_drops(1);
        }
        // Queueing is not delivering. A same-host publish reaches its consumer
        // because `apply_publish` pushes to that consumer's session
        // (`push_to_waiting_consumers`), and a timer tick because `fire_timers`
        // does the same; a payload that arrived over the peer leg had no
        // equivalent, so it sat in the mailbox until `sweep_deliveries` next
        // ran — an idle-tick backstop, not a delivery path. A cross-daemon
        // consumer in §9.1's canonical `while let Some(event) = events.recv()`
        // loop therefore saw its inputs arrive late, in bursts, or (once its
        // producer had finished and the dataflow was tearing down) not at all.
        self.push_to_node(dataflow, &node);
    }

    /// Closes one remote input on its local consumer.
    fn close_remote_input(
        &mut self,
        dataflow: DataflowId,
        source: &PortRef,
        consumer: &PortRef,
        reason: RouteCloseReason,
    ) {
        let node = consumer.node().clone();
        let input = consumer.port().clone();
        if let Some(state) = self.state.dataflow_mut(dataflow)
            && let Some(node_state) = state.node_mut(&node)
        {
            node_state.close_input(&input);
        }
        self.mailbox(dataflow, &node).push(
            &input,
            NodeEvent::InputClosed {
                id: input.clone(),
                source: source.clone(),
                reason,
            },
        );
        self.propagate_input_closure(dataflow);
    }

    /// Opens a route to a consumer hosted by `daemon` (§6.4).
    ///
    /// # Errors
    ///
    /// Anything [`crate::peer::PeerManager::setup_route`] can return.
    pub fn open_remote_route(
        &mut self,
        daemon: &DaemonId,
        dataflow: DataflowId,
        producer: PortRef,
        consumer: PortRef,
    ) -> crate::error::DaemonResult<RouteId> {
        let generation = self
            .state
            .dataflow(dataflow)
            .and_then(|state| state.node(producer.node()))
            .map_or(0, crate::state::NodeState::generation);
        let type_urn = self
            .state
            .dataflow(dataflow)
            .and_then(|state| state.node(producer.node()))
            .and_then(|node| node.spec().output(producer.port()).cloned())
            .and_then(|spec| spec.type_urn);
        let spec = RouteSpec::new(astrs_wire::RouteKey::new(dataflow, producer, consumer))
            .with_plane(Plane::Tcp);
        let max_payload = self.config.limits().max_payload_bytes;
        let route = self
            .peers
            .setup_route(daemon, spec, generation, type_urn, max_payload)?;
        self.publish_gauges();
        Ok(route)
    }

    /// Forwards one published message to every remote consumer (§6.4).
    ///
    /// Returns how many peers it reached.
    pub(crate) fn forward_remote(
        &mut self,
        dataflow: DataflowId,
        source: &PortRef,
        metadata: &Metadata,
        payload: &[u8],
    ) -> usize {
        let sent = self.peers.forward(dataflow, source, metadata, payload);
        // One frame per peer, each carrying the whole payload: a fan-out to
        // three peers is three frames and three payloads on the wire, and the
        // counters must say so or `astrs top`'s bandwidth figure is wrong.
        for _ in 0..sent {
            self.metrics.record_peer_sent(payload.len() as u64);
        }
        sent
    }

    /// Tells every remote consumer of `source` that it is finished.
    pub(crate) fn close_remote_outputs(
        &mut self,
        dataflow: DataflowId,
        source: &PortRef,
        reason: RouteCloseReason,
    ) -> usize {
        self.peers.close_output(dataflow, source, reason)
    }

    // ────────────────────────────── taps (§13) ──────────────────────────────

    /// Copies one published message to every debug tap that wants it.
    pub(crate) fn capture_tap(
        &mut self,
        dataflow: DataflowId,
        source: &PortRef,
        metadata: &Metadata,
        payload: &[u8],
    ) {
        if self.taps.is_empty() {
            return;
        }
        let frames = self
            .taps
            .capture(dataflow, source, metadata, payload, Instant::now());
        for frame in frames {
            let dropped = self.taps.dropped(frame.subscription);
            self.metrics.record_tap_message();
            self.sink.report(astrs_wire::DaemonEvent::TopicTapData {
                frame: Box::new(frame),
                dropped,
            });
        }
    }

    /// Starts tapping one dataflow's outputs (§13, §17 `topic echo`).
    ///
    /// Returns whether the subscription was accepted: a dataflow that never
    /// opted in with `debug: true` refuses. An output that was on the
    /// shared-memory plane is downgraded, because a tap needs the daemon to
    /// keep seeing the bytes.
    pub fn start_tap(
        &mut self,
        subscription: astrs_wire::SubscriptionId,
        dataflow: DataflowId,
        source: Option<PortRef>,
        max_hz: Option<f64>,
    ) -> bool {
        if !self
            .taps
            .subscribe(subscription, dataflow, source.clone(), max_hz)
        {
            return false;
        }
        let sources: Vec<PortRef> = match self.state.dataflow(dataflow) {
            Some(state) => state
                .routes()
                .sources()
                .filter(|port| source.as_ref().is_none_or(|wanted| *port == wanted))
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        for port in sources {
            let key = OutputKey::from_port(dataflow, &port);
            if let Some(action) = self.shm.downgrade(
                &key,
                RouteDowngradeReason::DaemonRequest {
                    message: "a debug tap needs the daemon path".into(),
                },
            ) {
                self.apply_upgrade_actions(vec![action]);
            }
            self.plan_shm_output(dataflow, &port);
        }
        true
    }

    /// Stops one tap, allowing its outputs back onto the ring.
    pub fn stop_tap(&mut self, subscription: astrs_wire::SubscriptionId) -> bool {
        if !self.taps.unsubscribe(subscription) {
            return false;
        }
        let pairs: Vec<(DataflowId, PortRef)> = self
            .state
            .dataflows()
            .flat_map(|state| {
                let id = state.id();
                state.routes().sources().map(move |port| (id, port.clone()))
            })
            .collect();
        for (dataflow, port) in pairs {
            self.plan_shm_output(dataflow, &port);
        }
        true
    }

    // ─────────────────────────── telemetry (§13) ────────────────────────────

    /// Samples every live node's process and queue figures.
    pub(crate) fn sample_nodes(&mut self, now: Instant) {
        if !self.samples.due(now) {
            return;
        }
        let timestamp = self.clock.now();
        // The wheel-wide reading, published once per round regardless of
        // whether any dataflow has a live node to sample below — it
        // describes the shared timer wheel (§11.1), not any one dataflow.
        self.metrics
            .set_timer_jitter_p99_us(self.timers.overall_jitter_p99_us());
        let dataflows: Vec<DataflowId> = self.state.dataflow_ids().collect();
        for dataflow in dataflows {
            let requests = self.sample_requests(dataflow);
            if requests.is_empty() {
                continue;
            }
            let io = self.io_samples(dataflow, &requests, timestamp);
            let samples = self.samples.sample(dataflow, &requests, timestamp, now);
            self.metrics.record_metric_sample();
            self.sink
                .report(astrs_wire::DaemonEvent::NodeMetrics { dataflow, samples });
            // Immediately after, and for the same node set, so a coordinator
            // holding both never pairs readings from different rounds (§13).
            self.sink.report(astrs_wire::DaemonEvent::NodeIoMetrics {
                dataflow,
                samples: io,
            });
        }
        // Advance the cadence exactly once per due round, whether or not
        // anything was sampled. Every other deadline in `next_deadline` clears
        // itself when it fires — an expiry is marked reported, an offer's
        // timestamp is taken, a heartbeat moves its own clock — and this one
        // does not, so a round that found nothing to sample would otherwise
        // leave a deadline permanently in the past and spin the loop.
        self.samples.arm_next(now);
    }

    /// The sampling request for every live node of one dataflow.
    fn sample_requests(&self, dataflow: DataflowId) -> Vec<NodeSampleRequest> {
        let Some(state) = self.state.dataflow(dataflow) else {
            return Vec::new();
        };
        // Every node that has not finished, not only the running ones: a node
        // hanging before it registers is exactly the one whose CPU and RSS an
        // operator wants to see, and a dynamic node with no pid still has
        // queue depths worth reporting.
        state
            .nodes()
            .filter(|node| !node.is_terminal())
            .map(|node| {
                let mut request =
                    NodeSampleRequest::new(node.id().clone(), node.pid().unwrap_or(0));
                if let Some(mailbox) = self.mailboxes.get(&(dataflow, node.id().clone())) {
                    for (input, snapshot) in mailbox.snapshot_all() {
                        request.queue_depths.insert(
                            input.clone(),
                            u32::try_from(snapshot.depth).unwrap_or(u32::MAX),
                        );
                        request
                            .received_total
                            .insert(input.clone(), snapshot.delivered);
                        request.dropped_total.insert(input, snapshot.dropped);
                    }
                }
                let key = node.id().clone();
                // The *ring* count, not slot occupancy: this is the frozen
                // `NodeMetricsSample` field and its meaning must not drift.
                // Real occupancy travels in `NodeIoSample` — see
                // `Daemon::io_samples`.
                request.shm_slots_in_use = u32::try_from(
                    self.shm
                        .registry()
                        .keys()
                        .filter(|open| open.dataflow == dataflow && open.node == key)
                        .count(),
                )
                .unwrap_or(u32::MAX);
                // §13's `sent_total`, which nothing populated before: the
                // per-output publish counts the egress ledger has been
                // accumulating on every `fan_out` (both planes).
                request.sent_total = self.io.sent_messages(dataflow, &key);
                // §11.1's timer jitter, read straight off the shared wheel's
                // running P² estimator for whichever `astrs/timer/*`
                // interval(s) this node subscribes to — `0` for a node with
                // none, or one that has not ticked yet.
                request.timer_jitter_p99_us = self.timers.jitter_p99_us(dataflow, &key);
                request
            })
            .collect()
    }

    /// The bandwidth half of one round's samples (§6.2, §13).
    ///
    /// Built from the same `requests` the message-count samples are built
    /// from, so the two describe exactly the same node set — see
    /// [`astrs_wire::DaemonEvent::NodeIoMetrics`].
    fn io_samples(
        &self,
        dataflow: DataflowId,
        requests: &[NodeSampleRequest],
        timestamp: astrs_time::HlcTimestamp,
    ) -> Vec<astrs_wire::NodeIoSample> {
        requests
            .iter()
            .map(|request| {
                let mut sample = astrs_wire::NodeIoSample::new(request.node.clone(), timestamp);
                sample.sent_bytes_total = self.io.sent_bytes(dataflow, &request.node);
                if let Some(mailbox) = self.mailboxes.get(&(dataflow, request.node.clone())) {
                    sample.received_bytes_total = mailbox.received_bytes().clone();
                }
                let occupancy = self.shm_occupancy(dataflow, &request.node);
                sample.shm_slots_in_use = occupancy.in_use;
                sample.shm_slots_total = occupancy.total;
                sample.shm_fallback_total = occupancy.fallbacks;
                sample
            })
            .collect()
    }

    /// How full one node's shared-memory rings are, right now (§6.2).
    ///
    /// Read from each segment's header rather than by scanning its slot table:
    /// `write_seq - reclaimed_seq` is exactly "published and not yet
    /// reclaimed", it is two atomic loads, and a sampling pass that walked
    /// every slot of every ring every two seconds would be a measurable cost
    /// on the very graphs whose occupancy matters most.
    ///
    /// The difference is clamped to the ring's own slot count: the two
    /// sequence numbers are read independently and a producer publishing
    /// between the two loads can make the raw difference exceed the ring, and
    /// an occupancy above 100% would be a display artefact rather than a fact.
    fn shm_occupancy(&self, dataflow: DataflowId, node: &NodeId) -> ShmOccupancy {
        let mut occupancy = ShmOccupancy::default();
        let keys: Vec<_> = self
            .shm
            .registry()
            .keys()
            .filter(|key| key.dataflow == dataflow && key.node == *node)
            .cloned()
            .collect();
        for key in keys {
            let Some(record) = self.shm.registry().record(&key) else {
                continue;
            };
            let view = record.segment().view();
            let resident = view.write_seq.saturating_sub(view.reclaimed_seq);
            let in_use = u32::try_from(resident)
                .unwrap_or(u32::MAX)
                .min(view.slot_count);
            occupancy.in_use = occupancy.in_use.saturating_add(in_use);
            occupancy.total = occupancy.total.saturating_add(view.slot_count);
            occupancy.fallbacks = occupancy.fallbacks.saturating_add(view.fallback_total);
        }
        occupancy
    }

    /// Emits the coordinator heartbeat when one is due (§12).
    pub(crate) fn emit_heartbeat(&mut self, now: Instant) {
        if !self.heartbeat.due(now) {
            return;
        }
        let ft = self.metrics.ft_stats();
        let stats = self.metrics.daemon_stats(
            DurationMs::from_duration(self.heartbeat.uptime(now)),
            u32::try_from(self.state.running_node_count()).unwrap_or(u32::MAX),
            u32::try_from(self.state.dataflow_count()).unwrap_or(u32::MAX),
            0.0,
            0,
            self.shm.mapped_bytes(),
        );
        let timestamp = self.clock.now();
        let event = self.heartbeat.emit(stats, ft, timestamp, now);
        self.metrics.record_heartbeat();
        if ft.has_incidents() {
            tracing::debug!(summary = %ft.summary(), "daemon heartbeat");
        }
        self.sink.report(event);
    }

    /// Republishes the daemon's gauges (§13).
    pub(crate) fn publish_gauges(&mut self) {
        let mut local = 0u64;
        let mut shm = 0u64;
        for state in self.state.dataflows() {
            for (_, consumer) in state.routes().all() {
                match consumer.plane {
                    DeliveryPlane::Shm { .. } => shm += 1,
                    _ => local += 1,
                }
            }
        }
        self.metrics.set_routes_on_plane("local", local);
        self.metrics.set_routes_on_plane("shm", shm);
        self.metrics
            .set_routes_on_plane("remote", self.peers.routes().established_count() as u64);
        self.metrics
            .set_nodes_running(self.state.running_node_count() as u64);
        self.metrics.set_peers_connected(self.peers.len() as u64);
        self.metrics
            .set_segments_open(self.shm.segment_count() as u64);
        self.metrics
            .set_dataflows(self.state.dataflow_count() as u64);
    }

    /// Reports a node's exit upward (§7.3).
    pub(crate) fn report_node_stopped(
        &mut self,
        dataflow: DataflowId,
        node: &NodeId,
        generation: u64,
        cause: &NodeExitCause,
        restarting: bool,
    ) {
        if restarting {
            self.metrics.record_restart();
        }
        self.sink.report(astrs_wire::DaemonEvent::NodeStopped {
            dataflow,
            node: node.clone(),
            generation,
            cause: cause.clone(),
            restarting,
        });
    }

    /// The compression this daemon offers on a peer route (§6.4).
    #[must_use]
    pub fn peer_compression(&self) -> Compression {
        self.config.peer().compression()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::Arc;
    use std::time::Duration;

    use astrs_manifest::Manifest;
    use astrs_wire::{DataflowId, NodeId, SubscriptionId};

    use super::*;
    use crate::config::{DaemonConfig, ListenConfig, RuntimePaths};
    use crate::dataflow::plan_dataflow;
    use crate::health::RecordingSink;
    use crate::metrics::names;

    const PIPELINE: &str = "\
nodes:
  - id: camera
    path: dynamic
    outputs: [image]
  - id: detect
    path: dynamic
    inputs:
      frames: camera/image
";

    fn dataflow() -> DataflowId {
        DataflowId::from_u128(77)
    }

    fn daemon(name: &str) -> Daemon {
        let root = std::env::temp_dir().join(format!("astrs-planes-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let config = DaemonConfig::new(RuntimePaths::under(root))
            .with_listen(ListenConfig::none())
            .with_shm(false);
        let mut daemon = Daemon::new(config).expect("a daemon");
        let manifest = Manifest::from_yaml_str(PIPELINE).expect("a manifest");
        let plan = plan_dataflow(dataflow(), &manifest, &BTreeMap::new()).expect("a plan");
        daemon.admit(&plan).expect("admitted");
        daemon
    }

    #[tokio::test]
    async fn a_daemon_without_a_plane_still_starts() {
        let daemon = daemon("no-plane");
        assert!(!daemon.shm().is_enabled());
        assert!(daemon.peers().is_empty());
        assert!(daemon.health().is_empty());
        assert!(daemon.taps().is_empty());
    }

    #[tokio::test]
    async fn the_gauges_describe_the_admitted_graph() {
        let mut daemon = daemon("gauges");
        daemon.publish_gauges();
        let batch = daemon.metrics().snapshot(astrs_time::HlcTimestamp::EPOCH);

        let routes = batch
            .points
            .iter()
            .find(|point| point.name == names::ROUTES && point.label("plane") == Some("local"))
            .expect("a local routes series");
        assert_eq!(routes.value.as_f64(), 1.0);
        assert!(
            batch
                .points
                .iter()
                .any(|point| point.name == names::DATAFLOWS && point.value.as_f64() == 1.0)
        );
    }

    #[tokio::test]
    async fn a_heartbeat_reaches_the_sink_on_its_cadence() {
        let mut daemon = daemon("heartbeat");
        let sink = Arc::new(RecordingSink::new());
        daemon.set_sink(sink.clone());

        let start = Instant::now();
        daemon.emit_heartbeat(start);
        assert!(sink.is_empty(), "not yet due");

        daemon.emit_heartbeat(start + Duration::from_secs(6));
        assert!(sink.contains("Heartbeat"));
    }

    #[tokio::test]
    async fn node_metrics_reach_the_sink() {
        let mut daemon = daemon("metrics");
        let sink = Arc::new(RecordingSink::new());
        daemon.set_sink(sink.clone());
        let camera = NodeId::new("camera").unwrap();
        daemon
            .state_mut()
            .dataflow_mut(dataflow())
            .unwrap()
            .node_mut(&camera)
            .unwrap()
            .mark_spawning(std::process::id());

        daemon.sample_nodes(Instant::now() + Duration::from_secs(3));
        assert!(sink.contains("NodeMetrics"), "{:?}", sink.snapshot());
    }

    /// §11.1 end to end: a node subscribed to `astrs/timer/*` whose
    /// deliveries have run late reaches a sampling round with a nonzero
    /// `timer_jitter_p99_us`, both on its own `NodeMetricsSample` and on the
    /// daemon-wide [`names::TIMER_JITTER_P99_US`] gauge.
    #[tokio::test]
    async fn a_running_timers_jitter_reaches_a_sampled_node_and_the_gauge() {
        let root = std::env::temp_dir().join(format!(
            "astrs-planes-jitter-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::create_dir_all(&root);
        let config = DaemonConfig::new(RuntimePaths::under(root))
            .with_listen(ListenConfig::none())
            .with_shm(false);
        let mut daemon = Daemon::new(config).expect("a daemon");

        let ticking = DataflowId::from_u128(0x71_C4_E7);
        let manifest = Manifest::from_yaml_str(
            "nodes:\n  - id: planner\n    path: dynamic\n    inputs:\n      \
             tick: astrs/timer/millis/10\n",
        )
        .expect("a manifest");
        let plan = plan_dataflow(ticking, &manifest, &BTreeMap::new()).expect("a plan");
        daemon.admit(&plan).expect("admitted");

        let planner = NodeId::new("planner").unwrap();
        daemon
            .state_mut()
            .dataflow_mut(ticking)
            .unwrap()
            .node_mut(&planner)
            .unwrap()
            .mark_awaiting_attach();

        let sink = Arc::new(RecordingSink::new());
        daemon.set_sink(sink.clone());

        let start = Instant::now();
        // Advance the shared wheel directly: on time, then twice a few
        // milliseconds late — real jitter, exactly `local::virtual_src`'s
        // own jitter tests, but through the `Daemon`-owned registry.
        let _ = daemon.timers.advance(start + Duration::from_millis(10));
        let _ = daemon.timers.advance(start + Duration::from_millis(25));
        let _ = daemon.timers.advance(start + Duration::from_millis(38));

        daemon.sample_nodes(start + Duration::from_secs(3));

        let samples = sink
            .snapshot()
            .into_iter()
            .find_map(|event| match event {
                astrs_wire::DaemonEvent::NodeMetrics { samples, .. } => Some(samples),
                _ => None,
            })
            .expect("a NodeMetrics event was reported");
        let sample = samples
            .iter()
            .find(|sample| sample.node == planner)
            .expect("the timer node was sampled");
        assert!(
            sample.timer_jitter_p99_us > 0,
            "expected nonzero per-node jitter, got {}",
            sample.timer_jitter_p99_us
        );

        let batch = daemon.metrics().snapshot(astrs_time::HlcTimestamp::EPOCH);
        let gauge = batch
            .points
            .iter()
            .find(|point| point.name == names::TIMER_JITTER_P99_US)
            .map(|point| point.value.as_f64())
            .expect("the gauge is registered");
        assert!(
            gauge > 0.0,
            "expected a nonzero daemon-wide gauge, got {gauge}"
        );
    }

    #[tokio::test]
    async fn a_health_expiry_fails_its_node() {
        let mut daemon = daemon("health");
        let camera = NodeId::new("camera").unwrap();
        let start = Instant::now();
        // Attached rather than spawned, deliberately: a health expiry is a
        // presumed-hung node, and `fail_node` therefore *kills the process it
        // holds a handle for* (§12) — correctly. `mark_spawning(pid)` installs
        // a real `ProcessHandle`, so passing this test's own pid here made the
        // daemon SIGKILL the test runner. A `path: dynamic` node is live,
        // misses liveness deadlines the same way, and owns no process, which
        // is exactly the part of the machinery this test is about.
        daemon
            .state_mut()
            .dataflow_mut(dataflow())
            .unwrap()
            .node_mut(&camera)
            .unwrap()
            .mark_awaiting_attach();
        daemon.health.arm(
            dataflow(),
            camera.clone(),
            0,
            Some(Duration::from_millis(10)),
            start,
        );

        daemon.fire_health(start + Duration::from_secs(1));
        let node = daemon
            .dataflow(dataflow())
            .and_then(|state| state.node(&camera))
            .expect("present");
        assert!(matches!(
            node.exit_cause(),
            Some(NodeExitCause::HealthCheckTimeout { .. })
        ));
        assert_eq!(daemon.metrics().ft_stats().health_timeouts, 1);
    }

    #[tokio::test]
    async fn a_tap_needs_its_dataflow_to_opt_in() {
        let mut daemon = daemon("tap-gate");
        let subscription = SubscriptionId::new(1);
        assert!(!daemon.start_tap(subscription, dataflow(), None, None));

        daemon.taps_mut().enable(dataflow());
        assert!(daemon.start_tap(subscription, dataflow(), None, None));
        assert!(daemon.stop_tap(subscription));
        assert!(!daemon.stop_tap(subscription));
    }

    #[tokio::test]
    async fn a_tapped_message_reaches_the_sink() {
        let mut daemon = daemon("tap-capture");
        let sink = Arc::new(RecordingSink::new());
        daemon.set_sink(sink.clone());
        daemon.taps_mut().enable(dataflow());
        assert!(daemon.start_tap(SubscriptionId::new(1), dataflow(), None, None));

        let source: PortRef = "camera/image".parse().unwrap();
        daemon.capture_tap(dataflow(), &source, &Metadata::default(), b"frame");
        assert!(sink.contains("TopicTapData"));
        assert_eq!(
            daemon
                .metrics()
                .snapshot(astrs_time::HlcTimestamp::EPOCH)
                .points
                .iter()
                .find(|point| point.name == names::TAP_MESSAGES_TOTAL)
                .map(|point| point.value.as_f64()),
            Some(1.0)
        );
    }

    #[tokio::test]
    async fn a_route_setup_for_an_unknown_dataflow_is_refused() {
        let daemon = daemon("judge");
        let acceptance =
            daemon.judge_route(DataflowId::from_u128(9), &"detect/frames".parse().unwrap());
        assert!(!acceptance.is_accepted());
        assert_eq!(
            acceptance.rejection().map(RouteRejection::kind_name),
            Some("unknown_dataflow")
        );
    }

    #[tokio::test]
    async fn a_route_setup_for_an_unknown_port_is_refused() {
        let daemon = daemon("judge-port");
        let acceptance = daemon.judge_route(dataflow(), &"detect/nothing".parse().unwrap());
        assert_eq!(
            acceptance.rejection().map(RouteRejection::kind_name),
            Some("unknown_port")
        );
    }

    #[tokio::test]
    async fn a_route_setup_this_daemon_can_serve_is_accepted() {
        let daemon = daemon("judge-ok");
        let acceptance = daemon.judge_route(dataflow(), &"detect/frames".parse().unwrap());
        assert!(acceptance.is_accepted());
        assert_eq!(acceptance.plane(), Some(Plane::Tcp));
    }

    #[tokio::test]
    async fn a_lost_peer_closes_the_inputs_it_carried() {
        let mut daemon = daemon("peer-lost");
        let peer = DaemonId::generate(None);
        daemon
            .peers_mut()
            .answer_setup(
                &peer,
                RouteId::FIRST,
                RouteSpec::new(astrs_wire::RouteKey::new(
                    dataflow(),
                    "far/image".parse().unwrap(),
                    "detect/frames".parse().unwrap(),
                )),
                1,
                RouteAcceptance::accepted(Plane::Tcp, Compression::None, 1 << 20),
            )
            .ok();

        daemon.handle_peer_lost(&peer, "the socket closed");
        let mailbox = daemon
            .mailbox(dataflow(), &NodeId::new("detect").unwrap())
            .snapshot_all();
        assert!(
            mailbox
                .iter()
                .any(|(input, snapshot)| input.as_str() == "frames" && snapshot.depth > 0),
            "the consumer was told its remote input closed"
        );
    }

    #[tokio::test]
    async fn a_remote_payload_reaches_its_consumer() {
        let mut daemon = daemon("remote-deliver");
        let source: PortRef = "far/image".parse().unwrap();
        let consumer: PortRef = "detect/frames".parse().unwrap();
        daemon.deliver_remote(
            dataflow(),
            &source,
            &consumer,
            Metadata::default(),
            b"payload".to_vec(),
        );

        let depth = daemon
            .mailbox(dataflow(), &NodeId::new("detect").unwrap())
            .depth(&DataId::new("frames").unwrap());
        assert_eq!(depth, Some(1));
    }

    #[tokio::test]
    async fn planning_an_output_without_a_plane_does_nothing() {
        let mut daemon = daemon("plan-noop");
        let source: PortRef = "camera/image".parse().unwrap();
        daemon.plan_shm_output(dataflow(), &source);
        assert_eq!(daemon.shm().segment_count(), 0);
    }

    /// The segment a plan pass would hand out for `camera/image`.
    fn spec(generation: u64) -> astrs_wire::ShmSegmentSpec {
        astrs_wire::ShmSegmentSpec::new(
            format!("astrs/df/camera/image/{generation}"),
            generation,
            8,
            4096,
        )
    }

    fn route_key() -> crate::shm::InputRouteKey {
        crate::shm::InputRouteKey::new(
            OutputKey::from_port(dataflow(), &"camera/image".parse().unwrap()),
            NodeId::new("detect").unwrap(),
            DataId::new("frames").unwrap(),
        )
    }

    #[tokio::test]
    async fn every_consumer_of_a_ring_is_offered_its_input_route() {
        let daemon = daemon("input-offer");
        let source: PortRef = "camera/image".parse().unwrap();
        let actions = daemon.plan_input_routes(dataflow(), &source, &spec(1));

        assert_eq!(actions.len(), 1, "one consumer, one offer: {actions:?}");
        let UpgradeAction::OfferInput {
            consumer,
            input,
            segment,
            key,
        } = &actions[0]
        else {
            panic!("expected an OfferInput, got {:?}", actions[0]);
        };
        assert_eq!(consumer.as_str(), "detect");
        assert_eq!(input.as_str(), "frames", "the *consumer's* name for it");
        assert_eq!(segment.generation, 1);
        assert_eq!(key.node.as_str(), "camera");
        assert!(actions[0].is_consumer_side());
    }

    #[tokio::test]
    async fn an_offer_already_made_is_not_made_again() {
        let mut daemon = daemon("input-idempotent");
        let source: PortRef = "camera/image".parse().unwrap();

        // `plan_shm_output` runs on every subscribe, register, tap and edge
        // edit. Without the ledger that would be an offer storm, and a
        // consumer answering each one would detach and re-attach its ring.
        daemon
            .shm_mut()
            .input_routes_mut()
            .record_offer(route_key(), 1);
        assert!(
            daemon
                .plan_input_routes(dataflow(), &source, &spec(1))
                .is_empty()
        );

        // A restarted producer replaced its ring, so that *is* a re-offer.
        assert_eq!(
            daemon
                .plan_input_routes(dataflow(), &source, &spec(2))
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn an_offer_is_recorded_only_when_it_was_sent() {
        let mut daemon = daemon("input-record");
        let source: PortRef = "camera/image".parse().unwrap();
        let actions = daemon.plan_input_routes(dataflow(), &source, &spec(1));

        // `detect` has no session in this harness, so the send fails — and an
        // offer nobody received must not silence the next attempt.
        daemon.apply_upgrade_actions(actions);
        assert_eq!(
            daemon.shm().input_routes().told_generation(&route_key()),
            None
        );
        assert_eq!(
            daemon
                .plan_input_routes(dataflow(), &source, &spec(1))
                .len(),
            1,
            "the undelivered offer is retried"
        );
    }

    #[tokio::test]
    async fn a_revocation_clears_the_ledger_so_the_next_offer_lands() {
        let mut daemon = daemon("input-revoke");
        let source: PortRef = "camera/image".parse().unwrap();
        let key = OutputKey::from_port(dataflow(), &source);
        daemon
            .shm_mut()
            .input_routes_mut()
            .record_offer(route_key(), 1);

        let actions = daemon.revoke_input_routes(&key, RouteDowngradeReason::PoolExhausted);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], UpgradeAction::RevokeInput { .. }));
        assert!(daemon.shm().input_routes().is_empty());
        assert_eq!(
            daemon
                .plan_input_routes(dataflow(), &source, &spec(1))
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_producer_downgrade_leaves_its_consumers_attached() {
        let mut daemon = daemon("input-downgrade");
        let key = OutputKey::from_port(dataflow(), &"camera/image".parse().unwrap());
        daemon
            .shm_mut()
            .input_routes_mut()
            .record_offer(route_key(), 1);

        // A consumer's attachment follows the *segment*, not the producer's
        // plane. Revoking it here deadlocks §6.3: the producer is re-offered
        // its upgrade only once the daemon *observes* the consumers in the
        // segment's consumer table, so detaching them on every transient
        // downgrade — an unanswered offer, a debug tap, pool pressure — takes
        // away the very thing that would let the route come back.
        daemon.apply_upgrade_actions(vec![UpgradeAction::Downgrade {
            key,
            reason: RouteDowngradeReason::PoolExhausted,
        }]);
        assert_eq!(
            daemon.shm().input_routes().len(),
            1,
            "the consumer stays on the ring the producer stopped filling"
        );
        assert_eq!(daemon.metrics().ft_stats().route_downgrades, 1);
    }

    #[tokio::test]
    async fn an_offer_the_producer_never_received_is_withdrawn_at_once() {
        let mut daemon = daemon("offer-undeliverable");
        let key = OutputKey::from_port(dataflow(), &"camera/image".parse().unwrap());

        // `camera` has no session in this harness, so the offer cannot be
        // delivered — and a route left `Offered` would stall for the whole
        // acknowledgement timeout waiting on a question nobody was asked.
        daemon.apply_upgrade_actions(vec![UpgradeAction::Offer {
            key: key.clone(),
            segment: Box::new(spec(1)),
            consumers: Vec::new(),
        }]);
        assert_eq!(
            daemon.shm().state(&key),
            crate::shm::UpgradeState::Reliable,
            "the undelivered offer was withdrawn, not left outstanding"
        );
    }

    #[tokio::test]
    async fn a_virtual_source_never_gets_a_ring() {
        let mut daemon = daemon("plan-virtual");
        let tick = crate::state::virtual_port_ref("astrs/timer/hz/10").unwrap();
        daemon.plan_shm_output(dataflow(), &tick);
        assert_eq!(daemon.shm().segment_count(), 0);
    }
}
