//! [`ShmPlane`] — the shared-memory plane as one thing the loop can hold.
//!
//! Four collaborators, one owner:
//!
//! ```text
//!   ShmPolicy ──── who may attach ───┐
//!   SegmentRegistry ── the rings ────┼──► ShmPlane ──► UpgradeAction
//!   AttachmentLedger ── who has ─────┤                 (offer · downgrade)
//!   UpgradeTable ── where we are ────┘
//!   SegmentBridge ── the bytes, during a transition
//! ```
//!
//! The event loop never touches the four directly. It calls four methods —
//! [`ShmPlane::plan_output`] when subscriptions change,
//! [`ShmPlane::poll`] on every tick, [`ShmPlane::acknowledge`] when a producer
//! answers, and one of the `on_*_exit` family when something dies — and sends
//! whatever [`crate::shm::UpgradeAction`]s come back. That keeps the §6.3
//! state machine in one testable place instead of smeared across
//! [`crate::server::handlers`].
//!
//! # The upgrade condition, exactly
//!
//! An output is offered an upgrade when **all** of these hold:
//!
//! 1. The plane is enabled and the platform supports it.
//! 2. Every consumer of the output is [`crate::shm::ShmVerdict::Eligible`] —
//!    same host, spawned (not dynamic), registered, and untapped. One
//!    ineligible consumer disqualifies the whole output, because a ring the
//!    daemon has stepped out of cannot serve a consumer that cannot map it.
//! 3. A segment exists for the producer's current incarnation.
//! 4. Every eligible consumer has been *observed* in the segment's consumer
//!    table.
//!
//! Condition 2 is the one worth restating: §6.3 says "all static same-host
//! consumers attached", and a remote consumer is served by the daemon
//! bridging from the ring — which this plane supports
//! ([`crate::shm::SegmentBridge`]) but only as a *transition*, never as a
//! steady state, because a permanent bridge is the daemon copying every
//! message anyway and costs a shared mapping for nothing.
//!
//! # Examples
//!
//! ```
//! use std::collections::BTreeMap;
//! use std::time::Instant;
//! use astrs_daemon::shm::{ConsumerFacts, OutputKey, ShmPlane};
//! use astrs_wire::{DataId, DataflowId, NodeId};
//!
//! let mut plane = ShmPlane::disabled();
//! let key = OutputKey::new(
//!     DataflowId::from_u128(1),
//!     NodeId::new("camera")?,
//!     DataId::new("image")?,
//! );
//!
//! // With no plane, nothing is ever eligible and nothing is ever offered.
//! let consumers = vec![(NodeId::new("detect")?, ConsumerFacts::same_host())];
//! assert!(plane.plan_output(key.clone(), 1, &consumers, 1 << 20).is_err());
//! assert!(plane.poll(&BTreeMap::new(), Instant::now()).is_quiet());
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use astrs_wire::{DataflowId, NodeId, PortRef, RouteDowngradeReason, ShmSegmentSpec};

use crate::error::{DaemonError, DaemonResult};
use crate::shm::attach::AttachmentLedger;
use crate::shm::bridge::SegmentBridge;
use crate::shm::keys::OutputKey;
use crate::shm::policy::{ConsumerFacts, ShmPolicy, ShmRefusal};
use crate::shm::segments::SegmentRegistry;
use crate::shm::upgrade::{UpgradeAction, UpgradeState, UpgradeTable};

/// What one [`ShmPlane::poll`] found.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PlaneOutcome {
    /// Messages the loop must send to producers.
    pub actions: Vec<UpgradeAction>,
    /// Outputs whose segment went away, so their consumers' inputs close.
    pub closed: Vec<OutputKey>,
}

impl PlaneOutcome {
    /// Whether the poll found nothing to do.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.actions.is_empty() && self.closed.is_empty()
    }

    /// Merges another outcome into this one.
    pub fn absorb(&mut self, other: Self) {
        self.actions.extend(other.actions);
        self.closed.extend(other.closed);
    }
}

/// The daemon's shared-memory plane (§6.2, §6.3).
#[derive(Debug)]
pub struct ShmPlane {
    /// Who may attach.
    policy: ShmPolicy,
    /// The rings themselves.
    registry: SegmentRegistry,
    /// Who has attached.
    ledger: AttachmentLedger,
    /// Where each output sits in the slow-start handshake.
    upgrades: UpgradeTable,
    /// Which consumers have been told which ring to read — the consumer end
    /// of the same handshake.
    input_routes: crate::shm::InputRouteTable,
    /// The daemon's readers, during transitions.
    bridge: SegmentBridge,
    /// How many times a route stayed (or fell back) on the reliable path
    /// under resource pressure (§6.2).
    fallbacks: u64,
}

impl ShmPlane {
    /// A plane with `policy` over `registry`.
    #[must_use]
    pub fn new(policy: ShmPolicy, registry: SegmentRegistry) -> Self {
        Self {
            policy,
            registry,
            ledger: AttachmentLedger::new(),
            upgrades: UpgradeTable::new(),
            input_routes: crate::shm::InputRouteTable::new(),
            bridge: SegmentBridge::new(),
            fallbacks: 0,
        }
    }

    /// A plane that never upgrades anything.
    #[must_use]
    pub fn disabled() -> Self {
        Self::new(ShmPolicy::disabled(), SegmentRegistry::disabled())
    }

    /// Binds a broker at `socket_path`, degrading to
    /// [`ShmPlane::disabled`] if the platform has no plane.
    #[must_use]
    pub fn bind_or_disabled(socket_path: impl AsRef<Path>, policy: ShmPolicy) -> Self {
        let registry = SegmentRegistry::bind_or_disabled(socket_path);
        let policy = if registry.is_enabled() {
            policy
        } else {
            ShmPolicy::disabled()
        };
        Self::new(policy, registry)
    }

    /// Whether the plane can upgrade anything.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.policy.is_enabled() && self.registry.is_enabled()
    }

    /// The socket nodes dial to attach (§24.2).
    #[must_use]
    pub fn socket_path(&self) -> Option<&Path> {
        self.registry.socket_path()
    }

    /// The policy.
    #[must_use]
    pub const fn policy(&self) -> &ShmPolicy {
        &self.policy
    }

    /// The rings.
    #[must_use]
    pub const fn registry(&self) -> &SegmentRegistry {
        &self.registry
    }

    /// The attachment bookkeeping.
    #[must_use]
    pub const fn ledger(&self) -> &AttachmentLedger {
        &self.ledger
    }

    /// The slow-start state machine.
    #[must_use]
    pub const fn upgrades(&self) -> &UpgradeTable {
        &self.upgrades
    }

    /// Which consumers have been told which ring to read (§6.3, consumer
    /// side).
    #[must_use]
    pub const fn input_routes(&self) -> &crate::shm::InputRouteTable {
        &self.input_routes
    }

    /// The consumer-side ledger, mutably.
    ///
    /// The event loop records an offer here only once it has actually been
    /// sent, and takes revocations out of it when a ring goes away; see
    /// [`crate::shm::InputRouteTable`] for why the two are separate.
    pub const fn input_routes_mut(&mut self) -> &mut crate::shm::InputRouteTable {
        &mut self.input_routes
    }

    /// The daemon's ring readers.
    #[must_use]
    pub const fn bridge(&self) -> &SegmentBridge {
        &self.bridge
    }

    /// The daemon's ring readers, mutably — the publish path drains through
    /// this when a producer hands over a slot reference.
    pub const fn bridge_mut(&mut self) -> &mut SegmentBridge {
        &mut self.bridge
    }

    /// Whether one output currently publishes into its ring.
    #[must_use]
    pub fn is_upgraded(&self, key: &OutputKey) -> bool {
        self.upgrades.is_upgraded(key)
    }

    /// Where one output sits in the handshake.
    #[must_use]
    pub fn state(&self, key: &OutputKey) -> UpgradeState {
        self.upgrades.state(key)
    }

    /// How many §6.2 fallbacks this plane has recorded.
    #[must_use]
    pub const fn fallbacks(&self) -> u64 {
        self.fallbacks
    }

    /// Records one §6.2 fallback.
    pub const fn record_fallback(&mut self) {
        self.fallbacks = self.fallbacks.saturating_add(1);
    }

    /// The `shm_fallback_total` visible to an operator: the daemon's own
    /// count plus every producer's header counter (§6.2, §13).
    #[must_use]
    pub fn fallback_total(&self) -> u64 {
        self.fallbacks
            .saturating_add(self.registry.producer_fallbacks())
    }

    /// Decides which consumers of one output belong on the ring, creating or
    /// releasing its segment accordingly.
    ///
    /// `consumers` is every consumer of the output with the facts the policy
    /// judges it on. `pool_size` is the manifest's `shm_pool_size` for the
    /// output (§24.2).
    ///
    /// Returns the segment specification when the output has one, or [`None`]
    /// when it does not — the plane is off, nobody consumes it, or at least
    /// one consumer cannot map a ring.
    ///
    /// # Errors
    ///
    /// - [`DaemonError::ShmUnavailable`] when the plane is off but the caller
    ///   asked for a ring anyway; that is a caller bug worth surfacing, not a
    ///   silent `None`.
    /// - [`DaemonError::ShmSegment`] if the segment cannot be created.
    pub fn plan_output(
        &mut self,
        key: OutputKey,
        generation: u64,
        consumers: &[(NodeId, ConsumerFacts)],
        pool_size: u64,
    ) -> DaemonResult<Option<ShmSegmentSpec>> {
        if !self.is_enabled() {
            return Err(DaemonError::ShmUnavailable {
                message: "the shared-memory plane is not running".into(),
            });
        }
        if consumers.is_empty() {
            self.release(&key);
            return Ok(None);
        }

        let open = self.registry.open_count();
        let mut eligible = Vec::with_capacity(consumers.len());
        for (node, facts) in consumers {
            match self.policy.verdict_with_segments(facts, open).refusal() {
                None => eligible.push(node.clone()),
                Some(refusal) => {
                    if refusal.is_fallback() {
                        self.record_fallback();
                    }
                    // One consumer that cannot map the ring disqualifies the
                    // whole output: the daemon would have to keep copying for
                    // it anyway, and a route that is half zero-copy is a route
                    // whose ordering nobody can reason about.
                    self.release(&key);
                    return Ok(None);
                }
            }
        }

        let config =
            self.policy
                .segment_config(pool_size)
                .map_err(|error| DaemonError::ShmSegment {
                    segment: key.to_string(),
                    message: error.to_string(),
                })?;

        let needs_segment = self
            .registry
            .record(&key)
            .is_none_or(|record| record.generation != generation);
        let spec = if needs_segment {
            self.upgrades.reset(&key);
            self.registry.create(key.clone(), generation, config)?
        } else {
            self.registry
                .spec(&key)
                .ok_or_else(|| DaemonError::ShmSegment {
                    segment: key.to_string(),
                    message: "the segment vanished between two lookups".into(),
                })?
        };

        self.ledger.expect(key, eligible);
        Ok(Some(spec))
    }

    /// Releases one output's ring and forgets everything about it.
    ///
    /// Returns the downgrade the producer needs, if it was on the ring.
    pub fn release(&mut self, key: &OutputKey) -> Option<UpgradeAction> {
        let action = self.upgrades.downgrade(
            key,
            RouteDowngradeReason::SegmentClosed {
                generation: self.registry.record(key).map_or(0, |r| r.generation),
            },
        );
        self.bridge.detach(key);
        self.registry.close(key);
        self.ledger.forget(key);
        self.upgrades.reset(key);
        action
    }

    /// One maintenance pass over every tracked output.
    ///
    /// `pids` maps a process id to the node that owns it, which is how an
    /// entry in a segment's consumer table becomes a graph node. The event
    /// loop builds it from its own state; the plane never guesses.
    pub fn poll(&mut self, pids: &BTreeMap<u32, NodeId>, now: Instant) -> PlaneOutcome {
        let mut outcome = PlaneOutcome::default();
        if !self.is_enabled() {
            return outcome;
        }

        // A producer that died takes its ring with it (§6.2).
        for key in self.registry.maintain() {
            if let Some(action) = self.upgrades.downgrade(
                key_ref(&key),
                RouteDowngradeReason::SegmentClosed {
                    generation: self.upgrades.state(&key).generation().unwrap_or(0),
                },
            ) {
                outcome.actions.push(action);
            }
            self.bridge.detach(&key);
            self.ledger.forget(&key);
            self.upgrades.reset(&key);
            outcome.closed.push(key);
        }

        let tracked: Vec<OutputKey> = self.ledger.keys().cloned().collect();
        for key in tracked {
            outcome.absorb(self.poll_one(&key, pids, now));
        }

        outcome.actions.extend(self.upgrades.expire_offers(now));
        outcome
    }

    /// One output's share of a maintenance pass.
    fn poll_one(
        &mut self,
        key: &OutputKey,
        pids: &BTreeMap<u32, NodeId>,
        now: Instant,
    ) -> PlaneOutcome {
        let mut outcome = PlaneOutcome::default();
        let Some(record) = self.registry.record(key) else {
            return outcome;
        };
        let generation = record.generation;
        let present: BTreeMap<NodeId, u32> = record
            .attached_pids()
            .into_iter()
            .filter_map(|pid| pids.get(&pid).map(|node| (node.clone(), pid)))
            .collect();

        let delta = self.ledger.sync(key, present);
        if delta.lost_a_consumer() {
            let consumer = delta.detached.first().map_or_else(
                || PortRef::new(key.node.clone(), key.output.clone()),
                |node| PortRef::new(node.clone(), key.output.clone()),
            );
            if let Some(action) = self
                .upgrades
                .downgrade(key, RouteDowngradeReason::ConsumerDetached { consumer })
            {
                outcome.actions.push(action);
            }
            return outcome;
        }

        if !self.ledger.all_attached(key) {
            return outcome;
        }

        let consumers: Vec<PortRef> = self
            .ledger
            .attached(key)
            .into_iter()
            .map(|node| PortRef::new(node, key.output.clone()))
            .collect();
        let Some(spec) = self.registry.spec(key) else {
            return outcome;
        };
        if let Some(action) = self
            .upgrades
            .offer(key.clone(), generation, spec, consumers, now)
        {
            outcome.actions.push(action);
        }
        outcome
    }

    /// Withdraws an offer that could not be delivered.
    ///
    /// [`UpgradeTable::offer`] moves the route to `Offered` before the event
    /// reaches the wire, because the state machine cannot know whether it will.
    /// When it does not — a producer that has not registered yet, or whose
    /// session just ended — the route would otherwise sit in `Offered` for the
    /// whole [`crate::shm::UPGRADE_ACK_TIMEOUT`] waiting for an answer to a
    /// question nobody was asked, and the plane would take five seconds to
    /// engage for no reason. Withdrawing puts it straight back to `Reliable`,
    /// where the next maintenance pass re-offers it.
    ///
    /// Returns whether an offer was withdrawn.
    pub fn withdraw_offer(&mut self, key: &OutputKey) -> bool {
        if !self.upgrades.state(key).is_offered() {
            return false;
        }
        self.upgrades.reset(key)
    }

    /// Applies a producer's [`astrs_wire::NodeRequest::RouteUpgradeAck`].
    pub fn acknowledge(&mut self, key: &OutputKey, accepted: bool, reason: Option<String>) -> bool {
        if !accepted {
            self.record_fallback();
        }
        self.upgrades.acknowledge(key, accepted, reason)
    }

    /// Forces one output back onto the reliable path.
    ///
    /// The path a new ineligible consumer takes (§6.3
    /// [`RouteDowngradeReason::RemoteConsumerAdded`]): the producer is told to
    /// stop publishing into the ring, and the daemon attaches a bridge so the
    /// messages already committed are not lost while it switches.
    pub fn downgrade(
        &mut self,
        key: &OutputKey,
        reason: RouteDowngradeReason,
    ) -> Option<UpgradeAction> {
        let action = self.upgrades.downgrade(key, reason)?;
        self.attach_bridge(key);
        Some(action)
    }

    /// Attaches the daemon's own reader to one output's ring.
    ///
    /// Silent on failure: a bridge is a best-effort belt on top of the
    /// downgrade's braces, and refusing the downgrade because the daemon could
    /// not attach would leave the producer publishing into a ring nobody
    /// reads.
    pub fn attach_bridge(&mut self, key: &OutputKey) -> bool {
        let Some(record) = self.registry.record(key) else {
            return false;
        };
        let generation = record.generation;
        let segment = std::sync::Arc::clone(record.segment());
        self.bridge
            .attach(key.clone(), generation, &segment)
            .is_ok()
    }

    /// Handles a producer's exit: its rings close, its state is forgotten.
    ///
    /// Returns the outputs whose consumers must be told their input closed.
    pub fn on_producer_exit(&mut self, dataflow: DataflowId, node: &NodeId) -> Vec<OutputKey> {
        self.bridge.detach_producer(dataflow, node);
        let closed = self.registry.close_producer(dataflow, node);
        self.ledger.forget_producer(dataflow, node);
        self.upgrades.reset_producer(dataflow, node);
        closed
    }

    /// Handles a consumer's exit: every output it was attached to downgrades.
    pub fn on_consumer_exit(&mut self, dataflow: DataflowId, node: &NodeId) -> Vec<UpgradeAction> {
        let touched = self.ledger.forget_consumer(dataflow, node);
        let mut actions = Vec::new();
        for key in touched {
            let reason = RouteDowngradeReason::ConsumerCrashed {
                consumer: PortRef::new(node.clone(), key.output.clone()),
            };
            if let Some(action) = self.downgrade(&key, reason) {
                actions.push(action);
            }
        }
        actions
    }

    /// Handles a dataflow ending: every ring in it goes.
    pub fn on_dataflow_stopped(&mut self, dataflow: DataflowId) -> Vec<OutputKey> {
        self.bridge.detach_dataflow(dataflow);
        let closed = self.registry.close_dataflow(dataflow);
        self.ledger.forget_dataflow(dataflow);
        self.upgrades.reset_dataflow(dataflow);
        closed
    }

    /// When the plane next needs a tick, if it does.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.upgrades.next_deadline()
    }

    /// How many outputs publish into a ring right now (§13).
    #[must_use]
    pub fn upgraded_count(&self) -> usize {
        self.upgrades.upgraded_count()
    }

    /// How many segments the daemon brokers (§13).
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.registry.open_count()
    }

    /// How many bytes of shared memory the daemon has mapped (§13).
    #[must_use]
    pub fn mapped_bytes(&self) -> u64 {
        self.registry.mapped_bytes()
    }

    /// Stops the broker and closes everything.
    pub fn shutdown(&mut self) {
        self.registry.shutdown();
    }

    /// Why one consumer of one output is not on the ring, if it is not.
    ///
    /// A diagnostic for `astrs graph info` and for the daemon's own log: the
    /// same decision [`ShmPlane::plan_output`] makes, reported rather than
    /// acted on.
    #[must_use]
    pub fn explain(&self, facts: &ConsumerFacts) -> Option<ShmRefusal> {
        self.policy
            .verdict_with_segments(facts, self.registry.open_count())
            .refusal()
    }
}

/// Borrow helper: `downgrade` wants a reference while the key is owned.
const fn key_ref(key: &OutputKey) -> &OutputKey {
    key
}

impl Default for ShmPlane {
    fn default() -> Self {
        Self::disabled()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_wire::{DataId, DataflowId};

    use super::*;

    /// A per-thread identity, stable within one test.
    ///
    /// A thread-local counter distinguishes tests that share a process. It is
    /// deliberately *not* enough on its own to name anything machine-global —
    /// see [`dataflow`], which adds the process identity.
    fn test_id() -> u32 {
        use std::cell::Cell;
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(8192);
        thread_local! {
            static ID: Cell<u32> = const { Cell::new(0) };
        }
        ID.with(|id| {
            if id.get() == 0 {
                id.set(NEXT.fetch_add(1, Ordering::Relaxed));
            }
            id.get()
        })
    }

    /// The dataflow every test in this module works in.
    ///
    /// The POSIX shared-memory name a segment gets is derived from this id,
    /// and that namespace is *machine-global*, not per-process. `cargo
    /// nextest` runs each test in its own process, where a thread-local
    /// counter restarts from the same seed — so on the counter alone every
    /// test in every process picks the same id, and two running concurrently
    /// fail `shm_open` with `EEXIST`. Mixing in the pid makes the id unique
    /// across processes as well as threads, under either test runner.
    fn dataflow() -> DataflowId {
        DataflowId::from_u128((u128::from(std::process::id()) << 32) | u128::from(test_id()))
    }

    fn key(node: &str, output: &str) -> OutputKey {
        OutputKey::new(
            dataflow(),
            NodeId::new(node).unwrap(),
            DataId::new(output).unwrap(),
        )
    }

    fn node(name: &str) -> NodeId {
        NodeId::new(name).unwrap()
    }

    #[test]
    fn a_disabled_plane_is_inert() {
        let mut plane = ShmPlane::disabled();
        assert!(!plane.is_enabled());
        assert!(plane.socket_path().is_none());
        assert_eq!(plane.segment_count(), 0);
        assert_eq!(plane.upgraded_count(), 0);
        assert_eq!(plane.mapped_bytes(), 0);
        assert!(plane.next_deadline().is_none());
        assert!(plane.poll(&BTreeMap::new(), Instant::now()).is_quiet());
        assert!(
            plane
                .on_producer_exit(dataflow(), &node("camera"))
                .is_empty()
        );
        assert!(
            plane
                .on_consumer_exit(dataflow(), &node("detect"))
                .is_empty()
        );
        assert!(plane.on_dataflow_stopped(dataflow()).is_empty());
    }

    #[test]
    fn the_default_plane_is_disabled() {
        assert!(!ShmPlane::default().is_enabled());
    }

    #[test]
    fn planning_without_a_plane_is_an_error_rather_than_a_silent_none() {
        let mut plane = ShmPlane::disabled();
        let consumers = [(node("detect"), ConsumerFacts::same_host())];
        let error = plane
            .plan_output(key("camera", "image"), 1, &consumers, 1 << 20)
            .expect_err("no plane");
        assert!(matches!(error, DaemonError::ShmUnavailable { .. }));
    }

    #[test]
    fn an_outcome_absorbs_another() {
        let mut first = PlaneOutcome::default();
        assert!(first.is_quiet());
        let second = PlaneOutcome {
            actions: Vec::new(),
            closed: vec![key("camera", "image")],
        };
        first.absorb(second);
        assert!(!first.is_quiet());
        assert_eq!(first.closed.len(), 1);
    }

    #[cfg(unix)]
    mod enabled {
        #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

        use std::time::Duration;

        use astrs_shm::{AttachOptions, Consumer, Producer};

        use super::*;
        use crate::shm::segments::SegmentRegistry;

        fn socket(_name: &str) -> std::path::PathBuf {
            // Unix socket paths are capped at `SUN_LEN` (104 bytes on macOS)
            // and the temporary directory already eats most of it, so the name
            // stays short: the per-test id is enough to be unique here.
            std::env::temp_dir().join(format!(
                "as{}-{}.sock",
                std::process::id(),
                super::test_id()
            ))
        }

        fn plane(name: &str) -> ShmPlane {
            let registry = SegmentRegistry::bind(socket(name)).expect("bound");
            ShmPlane::new(ShmPolicy::new(), registry)
        }

        /// The daemon's own pid stands in for a node process: the plane maps
        /// pids to nodes through the table the caller supplies, so a test
        /// attaches in-process and calls that pid "detect".
        fn pids(name: &str) -> BTreeMap<u32, NodeId> {
            BTreeMap::from([(std::process::id(), node(name))])
        }

        #[test]
        fn a_plane_over_a_real_broker_is_enabled() {
            let plane = plane("enabled");
            assert!(plane.is_enabled());
            assert!(plane.socket_path().is_some());
        }

        #[test]
        fn planning_creates_a_segment_and_records_the_expected_set() {
            let mut plane = plane("plan");
            let consumers = [(node("detect"), ConsumerFacts::same_host())];
            let spec = plane
                .plan_output(key("camera", "image"), 1, &consumers, 1 << 20)
                .expect("planned")
                .expect("a segment");

            assert_eq!(spec.generation, 1);
            assert_eq!(plane.segment_count(), 1);
            assert_eq!(
                plane.ledger().expected(&key("camera", "image")),
                vec![node("detect")]
            );
            assert_eq!(plane.state(&key("camera", "image")), UpgradeState::Reliable);
        }

        #[test]
        fn one_ineligible_consumer_disqualifies_the_output() {
            let mut plane = plane("ineligible");
            let consumers = [
                (node("detect"), ConsumerFacts::same_host()),
                (node("remote"), ConsumerFacts::same_host().remote()),
            ];
            assert!(
                plane
                    .plan_output(key("camera", "image"), 1, &consumers, 1 << 20)
                    .expect("planned")
                    .is_none()
            );
            assert_eq!(plane.segment_count(), 0);
        }

        #[test]
        fn an_output_with_no_consumers_gets_no_ring() {
            let mut plane = plane("nobody");
            assert!(
                plane
                    .plan_output(key("camera", "image"), 1, &[], 1 << 20)
                    .expect("planned")
                    .is_none()
            );
            assert_eq!(plane.segment_count(), 0);
        }

        #[test]
        fn an_attached_consumer_completes_the_set_and_earns_an_offer() {
            let mut plane = plane("offer");
            let key = key("camera", "image");
            let consumers = [(node("detect"), ConsumerFacts::same_host())];
            plane
                .plan_output(key.clone(), 1, &consumers, 1 << 20)
                .expect("planned");

            let now = Instant::now();
            assert!(
                plane.poll(&pids("detect"), now).is_quiet(),
                "nobody has attached yet"
            );

            let segment =
                std::sync::Arc::clone(plane.registry().record(&key).expect("present").segment());
            let _consumer = Consumer::attach(segment, AttachOptions::default()).expect("attached");

            let outcome = plane.poll(&pids("detect"), now);
            assert_eq!(outcome.actions.len(), 1);
            assert_eq!(outcome.actions[0].kind_name(), "offer");
            assert!(plane.state(&key).is_offered());

            assert!(plane.acknowledge(&key, true, None));
            assert!(plane.is_upgraded(&key));
            assert_eq!(plane.upgraded_count(), 1);
        }

        #[test]
        fn a_detaching_consumer_downgrades_the_route() {
            let mut plane = plane("detach");
            let key = key("camera", "image");
            plane
                .plan_output(
                    key.clone(),
                    1,
                    &[(node("detect"), ConsumerFacts::same_host())],
                    1 << 20,
                )
                .expect("planned");

            let segment =
                std::sync::Arc::clone(plane.registry().record(&key).expect("present").segment());
            let mut consumer =
                Consumer::attach(std::sync::Arc::clone(&segment), AttachOptions::default())
                    .expect("attached");
            let now = Instant::now();
            plane.poll(&pids("detect"), now);
            plane.acknowledge(&key, true, None);
            assert!(plane.is_upgraded(&key));

            consumer.detach();
            let outcome = plane.poll(&pids("detect"), now);
            assert_eq!(outcome.actions.len(), 1);
            assert_eq!(outcome.actions[0].kind_name(), "downgrade");
            assert!(!plane.is_upgraded(&key));
        }

        #[test]
        fn a_refused_upgrade_counts_as_a_fallback() {
            let mut plane = plane("refused");
            let key = key("camera", "image");
            plane
                .plan_output(
                    key.clone(),
                    1,
                    &[(node("detect"), ConsumerFacts::same_host())],
                    1 << 20,
                )
                .expect("planned");
            let segment =
                std::sync::Arc::clone(plane.registry().record(&key).expect("present").segment());
            let _consumer = Consumer::attach(segment, AttachOptions::default()).expect("attached");
            plane.poll(&pids("detect"), Instant::now());

            assert!(plane.acknowledge(&key, false, Some("cannot map".into())));
            assert_eq!(plane.fallbacks(), 1);
            assert!(!plane.is_upgraded(&key));
        }

        #[test]
        fn an_unanswered_offer_expires_into_a_downgrade() {
            let mut plane = plane("expire");
            let key = key("camera", "image");
            plane
                .plan_output(
                    key.clone(),
                    1,
                    &[(node("detect"), ConsumerFacts::same_host())],
                    1 << 20,
                )
                .expect("planned");
            let segment =
                std::sync::Arc::clone(plane.registry().record(&key).expect("present").segment());
            let _consumer = Consumer::attach(segment, AttachOptions::default()).expect("attached");

            let now = Instant::now();
            plane.poll(&pids("detect"), now);
            assert!(plane.state(&key).is_offered());
            assert!(plane.next_deadline().is_some());

            let outcome = plane.poll(&pids("detect"), now + Duration::from_secs(30));
            assert!(
                outcome
                    .actions
                    .iter()
                    .any(|action| action.kind_name() == "downgrade"),
                "{outcome:?}"
            );
            assert_eq!(plane.state(&key), UpgradeState::Reliable);
        }

        #[test]
        fn a_forced_downgrade_attaches_a_bridge_so_nothing_is_lost() {
            let mut plane = plane("bridge");
            let key = key("camera", "image");
            plane
                .plan_output(
                    key.clone(),
                    1,
                    &[(node("detect"), ConsumerFacts::same_host())],
                    1 << 20,
                )
                .expect("planned");
            let segment =
                std::sync::Arc::clone(plane.registry().record(&key).expect("present").segment());
            let _consumer =
                Consumer::attach(std::sync::Arc::clone(&segment), AttachOptions::default())
                    .expect("attached");
            plane.poll(&pids("detect"), Instant::now());
            plane.acknowledge(&key, true, None);

            let action = plane
                .downgrade(
                    &key,
                    RouteDowngradeReason::RemoteConsumerAdded {
                        consumer: PortRef::from_parts("planner", "frames").unwrap(),
                    },
                )
                .expect("a downgrade");
            assert_eq!(action.kind_name(), "downgrade");
            assert!(plane.bridge().is_attached(&key));

            let mut producer = Producer::new(segment).expect("producer");
            producer.send(b"in-flight", b"").expect("sent");
            let drained = plane.bridge_mut().drain_batch(&key);
            assert_eq!(drained.len(), 1);
            assert_eq!(drained[0].payload, b"in-flight");
        }

        #[test]
        fn a_producer_exit_closes_its_rings() {
            let mut plane = plane("producer-exit");
            plane
                .plan_output(
                    key("camera", "image"),
                    1,
                    &[(node("detect"), ConsumerFacts::same_host())],
                    1 << 20,
                )
                .expect("planned");
            plane
                .plan_output(
                    key("camera", "depth"),
                    1,
                    &[(node("detect"), ConsumerFacts::same_host())],
                    1 << 20,
                )
                .expect("planned");

            let closed = plane.on_producer_exit(dataflow(), &node("camera"));
            assert_eq!(closed.len(), 2);
            assert_eq!(plane.segment_count(), 0);
            assert!(plane.ledger().is_empty());
        }

        #[test]
        fn a_consumer_exit_downgrades_what_it_was_on() {
            let mut plane = plane("consumer-exit");
            let key = key("camera", "image");
            plane
                .plan_output(
                    key.clone(),
                    1,
                    &[(node("detect"), ConsumerFacts::same_host())],
                    1 << 20,
                )
                .expect("planned");
            let segment =
                std::sync::Arc::clone(plane.registry().record(&key).expect("present").segment());
            let _consumer = Consumer::attach(segment, AttachOptions::default()).expect("attached");
            plane.poll(&pids("detect"), Instant::now());
            plane.acknowledge(&key, true, None);

            let actions = plane.on_consumer_exit(dataflow(), &node("detect"));
            assert_eq!(actions.len(), 1);
            assert_eq!(actions[0].kind_name(), "downgrade");
            assert!(!plane.is_upgraded(&key));
        }

        #[test]
        fn a_dataflow_stop_releases_everything() {
            let mut plane = plane("stop");
            plane
                .plan_output(
                    key("camera", "image"),
                    1,
                    &[(node("detect"), ConsumerFacts::same_host())],
                    1 << 20,
                )
                .expect("planned");
            assert_eq!(plane.on_dataflow_stopped(dataflow()).len(), 1);
            assert_eq!(plane.segment_count(), 0);
            assert!(plane.upgrades().is_empty());
        }

        #[test]
        fn a_restart_replaces_the_segment() {
            let mut plane = plane("restart");
            let key = key("camera", "image");
            let consumers = [(node("detect"), ConsumerFacts::same_host())];
            let first = plane
                .plan_output(key.clone(), 1, &consumers, 1 << 20)
                .expect("planned")
                .expect("a segment");
            let second = plane
                .plan_output(key.clone(), 2, &consumers, 1 << 20)
                .expect("planned")
                .expect("a segment");

            assert_ne!(first.name, second.name);
            assert_eq!(plane.segment_count(), 1);
        }

        #[test]
        fn planning_twice_for_one_generation_reuses_the_segment() {
            let mut plane = plane("reuse");
            let key = key("camera", "image");
            let consumers = [(node("detect"), ConsumerFacts::same_host())];
            let first = plane
                .plan_output(key.clone(), 1, &consumers, 1 << 20)
                .expect("planned")
                .expect("a segment");
            let second = plane
                .plan_output(key.clone(), 1, &consumers, 1 << 20)
                .expect("planned")
                .expect("a segment");
            assert_eq!(first.name, second.name);
            assert_eq!(plane.registry().created_total(), 1);
        }

        #[test]
        fn the_fallback_total_includes_the_producers_own_counter() {
            let mut plane = plane("fallbacks");
            plane
                .plan_output(
                    key("camera", "image"),
                    1,
                    &[(node("detect"), ConsumerFacts::same_host())],
                    1 << 20,
                )
                .expect("planned");
            plane.record_fallback();
            assert_eq!(plane.fallback_total(), 1, "no producer has fallen back yet");
        }

        #[test]
        fn an_explanation_names_the_refusal() {
            let plane = plane("explain");
            assert!(plane.explain(&ConsumerFacts::same_host()).is_none());
            assert_eq!(
                plane.explain(&ConsumerFacts::same_host().dynamic()),
                Some(ShmRefusal::DynamicConsumer)
            );
        }

        #[test]
        fn a_shutdown_closes_the_plane() {
            let mut plane = plane("shutdown");
            plane
                .plan_output(
                    key("camera", "image"),
                    1,
                    &[(node("detect"), ConsumerFacts::same_host())],
                    1 << 20,
                )
                .expect("planned");
            plane.shutdown();
            assert!(!plane.is_enabled());
            assert_eq!(plane.segment_count(), 0);
        }
    }
}
