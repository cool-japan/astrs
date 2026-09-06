//! The slow-start route handshake, as a state machine (§6.3).
//!
//! > *Every output starts on the reliable daemon path. When the daemon has
//! > confirmed that all static same-host consumers attached to the ring … it
//! > issues `RouteUpgrade` to the producer, which switches to direct SHM
//! > publishing. Any consumer crash/downgrade flips the route back.*
//!
//! ```text
//!                    every expected consumer attached
//!      ┌──────────┐ ─────────────────────────────────► ┌──────────┐
//!      │ Reliable │                                    │ Offered  │
//!      └──────────┘ ◄───────────────────────────────── └──────────┘
//!            ▲       ack{accepted:false} · ack timeout      │
//!            │                                              │ ack{accepted:true}
//!            │            downgrade                         ▼
//!            └───────────────────────────────────────  ┌──────────┐
//!                  consumer detached · crashed ·       │ Upgraded │
//!                  segment closed · pool exhausted     └──────────┘
//! ```
//!
//! # Why `Offered` is a state and not a flag
//!
//! [`astrs_wire::NodeEvent::RouteUpgrade`] is acknowledged
//! ([`astrs_wire::NodeRequest::RouteUpgradeAck`]) precisely so no message can
//! fall between the planes: while the offer is outstanding the daemon **keeps
//! brokering** the route, because the producer may still be publishing through
//! it. Only the acknowledgement moves the route table's plane. A state machine
//! with a middle state makes that impossible to get wrong; a boolean would
//! have made it a comment.
//!
//! An offer that is never answered — a node that ignores the event, or one
//! whose event loop died between the offer and the reply — expires after
//! [`UPGRADE_ACK_TIMEOUT`] and the route stays reliable. Silence costs
//! throughput, never correctness.
//!
//! # The other end of the same handshake
//!
//! The state machine above is the **producer's**. A route has two ends, and
//! the consumer's is [`InputRouteTable`]: which consumers have been told which
//! ring one of their *inputs* now reads from
//! ([`astrs_wire::NodeEvent::InputRouteUpgrade`]).
//!
//! The two are sequenced, not symmetric, and the order is forced by condition
//! 4 of [`crate::shm::ShmPlane`]'s upgrade condition — *every eligible
//! consumer has been observed in the segment's consumer table*:
//!
//! ```text
//!   segment created ──► InputRouteUpgrade to each consumer   (InputRouteTable)
//!                            │
//!                            ▼  consumers attach; the daemon observes them
//!                       RouteUpgrade to the producer         (UpgradeTable)
//!                            │
//!                            ▼  ack
//!                       the route table says `shm`
//! ```
//!
//! A consumer therefore attaches while the producer is still on the reliable
//! path — which is safe precisely because the producer *is* still on it: the
//! ring is empty and stays empty until it switches, so an early attach can
//! neither miss a message nor read one twice.
//!
//! # Refusal is remembered
//!
//! A node that answers `accepted: false` (it could not map the segment) is not
//! asked again for that incarnation: re-offering every time a consumer
//! reattaches would turn one broken mapping into an offer storm. The refusal
//! is cleared when the producer restarts, because the next incarnation gets a
//! new segment and may well succeed.
//!
//! # Examples
//!
//! ```
//! use std::time::Instant;
//! use astrs_daemon::shm::{OutputKey, UpgradeState, UpgradeTable};
//! use astrs_wire::{DataId, DataflowId, NodeId, ShmSegmentSpec};
//!
//! let key = OutputKey::new(
//!     DataflowId::from_u128(1),
//!     NodeId::new("camera")?,
//!     DataId::new("image")?,
//! );
//! let spec = ShmSegmentSpec::new("seg", 1, 8, 4096);
//!
//! let mut table = UpgradeTable::new();
//! assert_eq!(table.state(&key), UpgradeState::Reliable);
//!
//! assert!(table.offer(key.clone(), 1, spec, Vec::new(), Instant::now()).is_some());
//! assert!(table.state(&key).is_offered());
//!
//! table.acknowledge(&key, true, None);
//! assert_eq!(table.state(&key), UpgradeState::Upgraded { generation: 1 });
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use astrs_wire::{DataId, NodeId, PortRef, RouteDowngradeReason, ShmSegmentSpec};

use crate::shm::keys::OutputKey;

/// How long an unanswered [`astrs_wire::NodeEvent::RouteUpgrade`] stands.
///
/// Generous by design: the offer arrives on the node's ordinary event stream,
/// so a node with a deep queue may take a while to reach it, and expiring an
/// offer only costs the throughput the route would have gained.
pub const UPGRADE_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Where one output sits in the slow-start handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum UpgradeState {
    /// The daemon brokers every message (§6.3, the starting state).
    Reliable,
    /// An upgrade has been offered and not yet answered.
    Offered {
        /// The producer incarnation the offer belongs to.
        generation: u64,
    },
    /// The producer acknowledged and publishes into the ring directly.
    Upgraded {
        /// The producer incarnation that owns the ring.
        generation: u64,
    },
    /// The producer refused the offer and will not be asked again for this
    /// incarnation.
    Refused {
        /// The producer incarnation that refused.
        generation: u64,
        /// What it said, when it said anything.
        reason: Option<String>,
    },
}

impl UpgradeState {
    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Reliable => "reliable",
            Self::Offered { .. } => "offered",
            Self::Upgraded { .. } => "upgraded",
            Self::Refused { .. } => "refused",
        }
    }

    /// Whether an offer is outstanding.
    #[must_use]
    pub const fn is_offered(&self) -> bool {
        matches!(self, Self::Offered { .. })
    }

    /// Whether the producer publishes into the ring.
    #[must_use]
    pub const fn is_upgraded(&self) -> bool {
        matches!(self, Self::Upgraded { .. })
    }

    /// Whether the daemon still brokers every message.
    ///
    /// True for everything except [`UpgradeState::Upgraded`] — including
    /// `Offered`, which is the whole reason the middle state exists.
    #[must_use]
    pub const fn is_daemon_mediated(&self) -> bool {
        !self.is_upgraded()
    }

    /// The producer incarnation this state belongs to, if any.
    #[must_use]
    pub const fn generation(&self) -> Option<u64> {
        match self {
            Self::Reliable => None,
            Self::Offered { generation }
            | Self::Upgraded { generation }
            | Self::Refused { generation, .. } => Some(*generation),
        }
    }
}

/// Something the event loop must send as a result of a transition.
///
/// Both ends of a route are here. The producer's pair ([`UpgradeAction::Offer`]
/// / [`UpgradeAction::Downgrade`]) is the §6.3 handshake proper; the consumer's
/// pair ([`UpgradeAction::OfferInput`] / [`UpgradeAction::RevokeInput`]) tells
/// the other end of the same edge which segment its *input* reads from, and is
/// what makes the plane engage without a node assembling the segment's identity
/// by hand.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum UpgradeAction {
    /// Send [`astrs_wire::NodeEvent::RouteUpgrade`] to the producer.
    Offer {
        /// The output being upgraded.
        key: OutputKey,
        /// The segment to publish into.
        segment: Box<ShmSegmentSpec>,
        /// The consumers that attached, for the node's own accounting.
        consumers: Vec<PortRef>,
    },
    /// Send [`astrs_wire::NodeEvent::RouteDowngrade`] to the producer.
    Downgrade {
        /// The output going back to the reliable path.
        key: OutputKey,
        /// Why.
        reason: RouteDowngradeReason,
    },
    /// Send [`astrs_wire::NodeEvent::InputRouteUpgrade`] to one consumer.
    ///
    /// Issued as soon as the ring exists and every consumer is eligible —
    /// *before* the producer is offered its own upgrade, because §6.3 only
    /// lets the producer switch once the daemon has observed every consumer in
    /// the segment's consumer table, and nothing can be observed before it
    /// attaches.
    OfferInput {
        /// The producer output whose ring this is.
        key: OutputKey,
        /// The consumer node being told.
        consumer: NodeId,
        /// The input of that node which now reads from the ring.
        input: DataId,
        /// The segment to attach to.
        segment: Box<ShmSegmentSpec>,
    },
    /// Send [`astrs_wire::NodeEvent::InputRouteDowngrade`] to one consumer.
    RevokeInput {
        /// The producer output whose ring this was.
        key: OutputKey,
        /// The consumer node being told.
        consumer: NodeId,
        /// The input going back to the daemon path.
        input: DataId,
        /// Why.
        reason: RouteDowngradeReason,
    },
}

impl UpgradeAction {
    /// The output this action concerns.
    #[must_use]
    pub const fn key(&self) -> &OutputKey {
        match self {
            Self::Offer { key, .. }
            | Self::Downgrade { key, .. }
            | Self::OfferInput { key, .. }
            | Self::RevokeInput { key, .. } => key,
        }
    }

    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Offer { .. } => "offer",
            Self::Downgrade { .. } => "downgrade",
            Self::OfferInput { .. } => "offer_input",
            Self::RevokeInput { .. } => "revoke_input",
        }
    }

    /// Whether this action is addressed to a consumer rather than the
    /// producer.
    #[must_use]
    pub const fn is_consumer_side(&self) -> bool {
        matches!(self, Self::OfferInput { .. } | Self::RevokeInput { .. })
    }
}

/// One consumer's view of one producer output: the thing an
/// [`UpgradeAction::OfferInput`] is remembered under.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InputRouteKey {
    /// The producer output whose ring it is.
    pub output: OutputKey,
    /// The consumer node.
    pub consumer: NodeId,
    /// The input of that node.
    pub input: DataId,
}

impl InputRouteKey {
    /// The key for `consumer`'s `input`, fed by `output`.
    #[must_use]
    pub const fn new(output: OutputKey, consumer: NodeId, input: DataId) -> Self {
        Self {
            output,
            consumer,
            input,
        }
    }

    /// The consumer end as a port reference, which is what the wire carries.
    #[must_use]
    pub fn consumer_port(&self) -> PortRef {
        PortRef::new(self.consumer.clone(), self.input.clone())
    }
}

impl core::fmt::Display for InputRouteKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}/{} ← {}", self.consumer, self.input, self.output)
    }
}

/// Which consumers have been told which ring to read (§6.3, consumer side).
///
/// # Why a ledger and not a broadcast
///
/// [`crate::server::Daemon::plan_shm_output`] runs whenever the answer *could*
/// change — a consumer subscribes, a node registers, a tap starts or stops,
/// the dynamic-topology verbs edit an edge — which for a graph of any size is
/// often. Re-sending [`astrs_wire::NodeEvent::InputRouteUpgrade`] on every one
/// of those passes would be an offer storm, and a consumer that answered it
/// honestly would detach and re-attach its ring each time.
///
/// So the ledger records what each consumer was last told, keyed by the
/// producer's **generation**: the same generation is silence, a different one
/// is a genuine re-offer (the producer restarted and its ring was replaced),
/// and a revocation clears the entry so the next offer lands.
///
/// # Recorded on send, not on decision
///
/// [`InputRouteTable::record_offer`] is deliberately separate from
/// [`InputRouteTable::needs_offer`]: a consumer with no live session has no
/// sink to send to, and recording an offer that was never delivered would
/// silence every later attempt. The caller records only what it actually sent.
#[derive(Debug, Default, Clone)]
pub struct InputRouteTable {
    /// The producer generation each consumer input was last told about.
    told: BTreeMap<InputRouteKey, u64>,
}

impl InputRouteTable {
    /// An empty ledger.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            told: BTreeMap::new(),
        }
    }

    /// Whether `key` still has to be told about `generation`.
    #[must_use]
    pub fn needs_offer(&self, key: &InputRouteKey, generation: u64) -> bool {
        self.told.get(key) != Some(&generation)
    }

    /// Records an offer that was actually sent.
    ///
    /// Returns the generation this consumer was told about before, if any.
    pub fn record_offer(&mut self, key: InputRouteKey, generation: u64) -> Option<u64> {
        self.told.insert(key, generation)
    }

    /// The generation `key` was last told about.
    #[must_use]
    pub fn told_generation(&self, key: &InputRouteKey) -> Option<u64> {
        self.told.get(key).copied()
    }

    /// Forgets one consumer's entry, so the next plan pass offers again.
    ///
    /// Returns whether anything was forgotten.
    pub fn forget(&mut self, key: &InputRouteKey) -> bool {
        self.told.remove(key).is_some()
    }

    /// Every consumer told about `output`, forgetting them all.
    ///
    /// What a revocation iterates: each entry becomes one
    /// [`UpgradeAction::RevokeInput`], and the cleared ledger lets the next
    /// upgrade be offered from scratch.
    pub fn take_all(&mut self, output: &OutputKey) -> Vec<InputRouteKey> {
        let taken: Vec<InputRouteKey> = self
            .told
            .keys()
            .filter(|key| key.output == *output)
            .cloned()
            .collect();
        for key in &taken {
            self.told.remove(key);
        }
        taken
    }

    /// Forgets every entry naming `node`, at either end.
    ///
    /// A node that died is neither a producer whose ring survives nor a
    /// consumer that is still attached to one.
    pub fn forget_node(&mut self, dataflow: astrs_wire::DataflowId, node: &NodeId) -> usize {
        let before = self.told.len();
        self.told.retain(|key, _| {
            !(key.output.dataflow == dataflow
                && (key.output.node == *node || key.consumer == *node))
        });
        before - self.told.len()
    }

    /// Forgets every entry of one dataflow.
    pub fn forget_dataflow(&mut self, dataflow: astrs_wire::DataflowId) -> usize {
        let before = self.told.len();
        self.told.retain(|key, _| key.output.dataflow != dataflow);
        before - self.told.len()
    }

    /// How many consumer inputs are on a ring as far as the daemon knows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.told.len()
    }

    /// Whether the ledger remembers nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.told.is_empty()
    }

    /// Every remembered consumer input and its generation, in key order.
    pub fn entries(&self) -> impl Iterator<Item = (&InputRouteKey, u64)> {
        self.told.iter().map(|(key, generation)| (key, *generation))
    }
}

/// One output's entry.
#[derive(Debug, Clone)]
struct UpgradeEntry {
    /// Where it sits.
    state: UpgradeState,
    /// When the outstanding offer was made.
    offered_at: Option<Instant>,
}

impl UpgradeEntry {
    /// A fresh, reliable entry.
    const fn reliable() -> Self {
        Self {
            state: UpgradeState::Reliable,
            offered_at: None,
        }
    }
}

/// Where every output sits in the slow-start handshake (§6.3).
#[derive(Debug, Default, Clone)]
pub struct UpgradeTable {
    /// One entry per output that has ever been considered.
    entries: BTreeMap<OutputKey, UpgradeEntry>,
    /// How long an offer stands before it expires.
    ack_timeout: Duration,
}

impl UpgradeTable {
    /// A table with the default acknowledgement timeout.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            ack_timeout: UPGRADE_ACK_TIMEOUT,
        }
    }

    /// A table with an explicit acknowledgement timeout.
    #[must_use]
    pub const fn with_ack_timeout(timeout: Duration) -> Self {
        Self {
            entries: BTreeMap::new(),
            ack_timeout: timeout,
        }
    }

    /// How long an offer stands.
    #[must_use]
    pub const fn ack_timeout(&self) -> Duration {
        self.ack_timeout
    }

    /// Where one output sits.
    #[must_use]
    pub fn state(&self, key: &OutputKey) -> UpgradeState {
        self.entries
            .get(key)
            .map_or(UpgradeState::Reliable, |entry| entry.state.clone())
    }

    /// Whether one output publishes into a ring.
    #[must_use]
    pub fn is_upgraded(&self, key: &OutputKey) -> bool {
        self.state(key).is_upgraded()
    }

    /// Whether the daemon still brokers one output.
    #[must_use]
    pub fn is_daemon_mediated(&self, key: &OutputKey) -> bool {
        self.state(key).is_daemon_mediated()
    }

    /// Offers an upgrade, if the output is in a state that can take one.
    ///
    /// Returns the action to send, or [`None`] when the offer is not made: the
    /// route is already offered or upgraded, or the producer refused this
    /// incarnation.
    pub fn offer(
        &mut self,
        key: OutputKey,
        generation: u64,
        segment: ShmSegmentSpec,
        consumers: Vec<PortRef>,
        now: Instant,
    ) -> Option<UpgradeAction> {
        let entry = self
            .entries
            .entry(key.clone())
            .or_insert_with(UpgradeEntry::reliable);
        match &entry.state {
            UpgradeState::Offered { .. } | UpgradeState::Upgraded { .. } => return None,
            UpgradeState::Refused {
                generation: refused,
                ..
            } if *refused == generation => return None,
            _ => {}
        }
        entry.state = UpgradeState::Offered { generation };
        entry.offered_at = Some(now);
        Some(UpgradeAction::Offer {
            key,
            segment: Box::new(segment),
            consumers,
        })
    }

    /// Applies a producer's [`astrs_wire::NodeRequest::RouteUpgradeAck`].
    ///
    /// Returns whether the acknowledgement matched an outstanding offer — a
    /// `false` here is a stale reply from a node that outlived one, exactly
    /// the case stage 1's handler documented and could not act on.
    pub fn acknowledge(&mut self, key: &OutputKey, accepted: bool, reason: Option<String>) -> bool {
        let Some(entry) = self.entries.get_mut(key) else {
            return false;
        };
        let UpgradeState::Offered { generation } = entry.state else {
            return false;
        };
        entry.offered_at = None;
        entry.state = if accepted {
            UpgradeState::Upgraded { generation }
        } else {
            UpgradeState::Refused { generation, reason }
        };
        true
    }

    /// Moves an output back to the reliable path.
    ///
    /// Returns the action to send when the producer needs telling — which it
    /// does from `Offered` and `Upgraded`, and does not from `Reliable` or
    /// `Refused`, where it is already publishing through the daemon.
    pub fn downgrade(
        &mut self,
        key: &OutputKey,
        reason: RouteDowngradeReason,
    ) -> Option<UpgradeAction> {
        let entry = self.entries.get_mut(key)?;
        let needs_telling = matches!(
            entry.state,
            UpgradeState::Offered { .. } | UpgradeState::Upgraded { .. }
        );
        entry.state = UpgradeState::Reliable;
        entry.offered_at = None;
        needs_telling.then(|| UpgradeAction::Downgrade {
            key: key.clone(),
            reason,
        })
    }

    /// Expires offers that were never acknowledged.
    ///
    /// The route returns to `Reliable` — where it never actually left, since
    /// the daemon kept brokering — and the producer is told, because it may
    /// simply have been slow rather than deaf and must not be left believing
    /// an offer is still open.
    pub fn expire_offers(&mut self, now: Instant) -> Vec<UpgradeAction> {
        let stale: Vec<OutputKey> = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry
                    .offered_at
                    .is_some_and(|at| now.saturating_duration_since(at) >= self.ack_timeout)
            })
            .map(|(key, _)| key.clone())
            .collect();

        stale
            .into_iter()
            .filter_map(|key| {
                self.downgrade(
                    &key,
                    RouteDowngradeReason::DaemonRequest {
                        message: "route upgrade was not acknowledged".into(),
                    },
                )
            })
            .collect()
    }

    /// When the earliest outstanding offer expires.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.entries
            .values()
            .filter_map(|entry| entry.offered_at)
            .map(|at| at.checked_add(self.ack_timeout).unwrap_or(at))
            .min()
    }

    /// Clears everything the daemon remembers about one output.
    ///
    /// What a producer restart does: the refusal, the offer and the upgrade
    /// all belonged to an incarnation that no longer exists.
    pub fn reset(&mut self, key: &OutputKey) -> bool {
        self.entries.remove(key).is_some()
    }

    /// Clears every output one node produces.
    pub fn reset_producer(
        &mut self,
        dataflow: astrs_wire::DataflowId,
        node: &astrs_wire::NodeId,
    ) -> Vec<OutputKey> {
        let keys: Vec<OutputKey> = self
            .entries
            .keys()
            .filter(|key| key.dataflow == dataflow && key.node == *node)
            .cloned()
            .collect();
        for key in &keys {
            self.entries.remove(key);
        }
        keys
    }

    /// Clears every output of one dataflow.
    pub fn reset_dataflow(&mut self, dataflow: astrs_wire::DataflowId) -> usize {
        let before = self.entries.len();
        self.entries.retain(|key, _| key.dataflow != dataflow);
        before - self.entries.len()
    }

    /// How many outputs are upgraded right now.
    #[must_use]
    pub fn upgraded_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.state.is_upgraded())
            .count()
    }

    /// How many outputs have an offer outstanding.
    #[must_use]
    pub fn offered_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.state.is_offered())
            .count()
    }

    /// How many outputs the table remembers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table remembers nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every remembered output and its state, in key order.
    pub fn states(&self) -> impl Iterator<Item = (&OutputKey, &UpgradeState)> {
        self.entries.iter().map(|(key, entry)| (key, &entry.state))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_wire::DataflowId;

    use super::*;

    fn key() -> OutputKey {
        OutputKey::new(
            DataflowId::from_u128(1),
            NodeId::new("camera").unwrap(),
            DataId::new("image").unwrap(),
        )
    }

    fn spec() -> ShmSegmentSpec {
        ShmSegmentSpec::new("astrs/camera/image/1", 1, 8, 4096)
    }

    fn consumers() -> Vec<PortRef> {
        vec![PortRef::from_parts("detect", "frames").unwrap()]
    }

    fn detached() -> RouteDowngradeReason {
        RouteDowngradeReason::ConsumerDetached {
            consumer: PortRef::from_parts("detect", "frames").unwrap(),
        }
    }

    #[test]
    fn an_unknown_output_is_reliable() {
        let table = UpgradeTable::new();
        assert_eq!(table.state(&key()), UpgradeState::Reliable);
        assert!(table.is_daemon_mediated(&key()));
        assert!(!table.is_upgraded(&key()));
        assert!(table.is_empty());
        assert!(table.next_deadline().is_none());
    }

    #[test]
    fn the_happy_path_is_offer_then_acknowledge() {
        let now = Instant::now();
        let mut table = UpgradeTable::new();
        let action = table
            .offer(key(), 1, spec(), consumers(), now)
            .expect("an offer");
        assert_eq!(action.kind_name(), "offer");
        assert_eq!(action.key(), &key());
        match &action {
            UpgradeAction::Offer {
                segment, consumers, ..
            } => {
                assert_eq!(segment.generation, 1);
                assert_eq!(consumers.len(), 1);
            }
            other => panic!("unexpected {other:?}"),
        }

        assert!(
            table.is_daemon_mediated(&key()),
            "the daemon keeps brokering while the offer is outstanding"
        );
        assert!(table.acknowledge(&key(), true, None));
        assert_eq!(
            table.state(&key()),
            UpgradeState::Upgraded { generation: 1 }
        );
        assert!(table.is_upgraded(&key()));
        assert_eq!(table.upgraded_count(), 1);
        assert_eq!(table.offered_count(), 0);
    }

    #[test]
    fn a_second_offer_while_one_is_outstanding_is_refused() {
        let now = Instant::now();
        let mut table = UpgradeTable::new();
        assert!(table.offer(key(), 1, spec(), consumers(), now).is_some());
        assert!(table.offer(key(), 1, spec(), consumers(), now).is_none());
        assert_eq!(table.offered_count(), 1);
    }

    #[test]
    fn an_upgraded_route_is_not_offered_again() {
        let now = Instant::now();
        let mut table = UpgradeTable::new();
        table.offer(key(), 1, spec(), consumers(), now);
        table.acknowledge(&key(), true, None);
        assert!(table.offer(key(), 1, spec(), consumers(), now).is_none());
    }

    #[test]
    fn a_refusal_is_remembered_for_that_incarnation_only() {
        let now = Instant::now();
        let mut table = UpgradeTable::new();
        table.offer(key(), 1, spec(), consumers(), now);
        assert!(table.acknowledge(&key(), false, Some("cannot map".into())));
        assert_eq!(
            table.state(&key()),
            UpgradeState::Refused {
                generation: 1,
                reason: Some("cannot map".into())
            }
        );
        assert!(table.is_daemon_mediated(&key()));
        assert!(table.offer(key(), 1, spec(), consumers(), now).is_none());

        // The next incarnation gets a fresh segment and a fresh chance.
        assert!(table.offer(key(), 2, spec(), consumers(), now).is_some());
    }

    #[test]
    fn an_acknowledgement_without_an_offer_is_stale() {
        let mut table = UpgradeTable::new();
        assert!(!table.acknowledge(&key(), true, None));

        let now = Instant::now();
        table.offer(key(), 1, spec(), consumers(), now);
        table.acknowledge(&key(), true, None);
        assert!(
            !table.acknowledge(&key(), true, None),
            "a second acknowledgement answers nothing"
        );
    }

    #[test]
    fn a_downgrade_from_upgraded_tells_the_producer() {
        let now = Instant::now();
        let mut table = UpgradeTable::new();
        table.offer(key(), 1, spec(), consumers(), now);
        table.acknowledge(&key(), true, None);

        let action = table.downgrade(&key(), detached()).expect("an instruction");
        assert_eq!(action.kind_name(), "downgrade");
        match action {
            UpgradeAction::Downgrade { reason, .. } => {
                assert_eq!(reason.kind_name(), "consumer_detached");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(table.state(&key()), UpgradeState::Reliable);
    }

    #[test]
    fn a_downgrade_from_offered_also_tells_the_producer() {
        let now = Instant::now();
        let mut table = UpgradeTable::new();
        table.offer(key(), 1, spec(), consumers(), now);
        assert!(table.downgrade(&key(), detached()).is_some());
        assert_eq!(table.state(&key()), UpgradeState::Reliable);
    }

    #[test]
    fn a_downgrade_of_a_reliable_route_tells_nobody() {
        let mut table = UpgradeTable::new();
        assert!(table.downgrade(&key(), detached()).is_none(), "not tracked");

        let now = Instant::now();
        table.offer(key(), 1, spec(), consumers(), now);
        table.acknowledge(&key(), false, None);
        assert!(
            table.downgrade(&key(), detached()).is_none(),
            "a refused route is already on the reliable path"
        );
    }

    #[test]
    fn an_unanswered_offer_expires() {
        let now = Instant::now();
        let mut table = UpgradeTable::with_ack_timeout(Duration::from_millis(100));
        table.offer(key(), 1, spec(), consumers(), now);
        assert_eq!(
            table.next_deadline(),
            Some(now + Duration::from_millis(100))
        );

        assert!(
            table
                .expire_offers(now + Duration::from_millis(50))
                .is_empty()
        );
        let expired = table.expire_offers(now + Duration::from_millis(150));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].kind_name(), "downgrade");
        assert_eq!(table.state(&key()), UpgradeState::Reliable);
        assert!(table.next_deadline().is_none());
    }

    #[test]
    fn an_acknowledged_offer_does_not_expire() {
        let now = Instant::now();
        let mut table = UpgradeTable::with_ack_timeout(Duration::from_millis(10));
        table.offer(key(), 1, spec(), consumers(), now);
        table.acknowledge(&key(), true, None);
        assert!(table.expire_offers(now + Duration::from_secs(1)).is_empty());
        assert!(table.is_upgraded(&key()));
    }

    #[test]
    fn resetting_forgets_an_output_entirely() {
        let now = Instant::now();
        let mut table = UpgradeTable::new();
        table.offer(key(), 1, spec(), consumers(), now);
        table.acknowledge(&key(), false, None);
        assert!(table.reset(&key()));
        assert!(!table.reset(&key()));
        assert!(
            table.offer(key(), 1, spec(), consumers(), now).is_some(),
            "a reset clears the refusal"
        );
    }

    #[test]
    fn resetting_a_producer_clears_all_of_its_outputs() {
        let now = Instant::now();
        let mut table = UpgradeTable::new();
        let depth = OutputKey::new(
            DataflowId::from_u128(1),
            NodeId::new("camera").unwrap(),
            DataId::new("depth").unwrap(),
        );
        let other = OutputKey::new(
            DataflowId::from_u128(1),
            NodeId::new("lidar").unwrap(),
            DataId::new("points").unwrap(),
        );
        table.offer(key(), 1, spec(), consumers(), now);
        table.offer(depth, 1, spec(), consumers(), now);
        table.offer(other, 1, spec(), consumers(), now);

        let cleared =
            table.reset_producer(DataflowId::from_u128(1), &NodeId::new("camera").unwrap());
        assert_eq!(cleared.len(), 2);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn resetting_a_dataflow_leaves_the_others() {
        let now = Instant::now();
        let mut table = UpgradeTable::new();
        table.offer(key(), 1, spec(), consumers(), now);
        table.offer(
            OutputKey::new(
                DataflowId::from_u128(2),
                NodeId::new("camera").unwrap(),
                DataId::new("image").unwrap(),
            ),
            1,
            spec(),
            consumers(),
            now,
        );
        assert_eq!(table.reset_dataflow(DataflowId::from_u128(1)), 1);
        assert_eq!(table.len(), 1);
        assert_eq!(table.states().count(), 1);
    }

    #[test]
    fn states_have_distinct_labels_and_report_their_generation() {
        assert_eq!(UpgradeState::Reliable.as_str(), "reliable");
        assert_eq!(UpgradeState::Reliable.generation(), None);
        assert_eq!(
            UpgradeState::Offered { generation: 3 }.generation(),
            Some(3)
        );
        assert_eq!(
            UpgradeState::Upgraded { generation: 4 }.as_str(),
            "upgraded"
        );
        assert_eq!(
            UpgradeState::Refused {
                generation: 5,
                reason: None
            }
            .as_str(),
            "refused"
        );
    }

    #[test]
    fn the_default_timeout_is_the_documented_one() {
        assert_eq!(UpgradeTable::new().ack_timeout(), UPGRADE_ACK_TIMEOUT);
        assert_eq!(UpgradeTable::default().len(), 0);
    }

    // ─────────────────────── the consumer side (§6.3) ───────────────────────

    fn input_key() -> InputRouteKey {
        InputRouteKey::new(
            key(),
            NodeId::new("detect").unwrap(),
            DataId::new("frames").unwrap(),
        )
    }

    #[test]
    fn a_consumer_nobody_has_told_needs_an_offer() {
        let table = InputRouteTable::new();
        assert!(table.needs_offer(&input_key(), 1));
        assert_eq!(table.told_generation(&input_key()), None);
        assert!(table.is_empty());
        assert_eq!(table.entries().count(), 0);
    }

    #[test]
    fn an_offer_is_made_once_per_generation() {
        let mut table = InputRouteTable::new();
        assert_eq!(table.record_offer(input_key(), 1), None);
        assert!(
            !table.needs_offer(&input_key(), 1),
            "a plan pass that changes nothing must not re-offer"
        );
        assert_eq!(table.len(), 1);

        // A restarted producer replaced its ring: that is a genuine re-offer.
        assert!(table.needs_offer(&input_key(), 2));
        assert_eq!(table.record_offer(input_key(), 2), Some(1));
        assert_eq!(table.told_generation(&input_key()), Some(2));
    }

    #[test]
    fn a_revocation_clears_the_entry_so_the_next_offer_lands() {
        let mut table = InputRouteTable::new();
        table.record_offer(input_key(), 1);

        let revoked = table.take_all(&key());
        assert_eq!(revoked, vec![input_key()]);
        assert!(table.is_empty());
        assert!(
            table.needs_offer(&input_key(), 1),
            "the same generation is offered again after a revocation"
        );
        assert!(table.take_all(&key()).is_empty());
    }

    #[test]
    fn a_revocation_leaves_other_outputs_alone() {
        let mut table = InputRouteTable::new();
        let other = OutputKey::new(
            DataflowId::from_u128(1),
            NodeId::new("lidar").unwrap(),
            DataId::new("points").unwrap(),
        );
        table.record_offer(input_key(), 1);
        table.record_offer(
            InputRouteKey::new(
                other.clone(),
                NodeId::new("detect").unwrap(),
                DataId::new("points").unwrap(),
            ),
            1,
        );

        assert_eq!(table.take_all(&key()).len(), 1);
        assert_eq!(table.len(), 1);
        assert_eq!(table.take_all(&other).len(), 1);
    }

    #[test]
    fn forgetting_a_node_clears_both_ends() {
        let mut table = InputRouteTable::new();
        table.record_offer(input_key(), 1);
        assert_eq!(
            table.forget_node(DataflowId::from_u128(1), &NodeId::new("detect").unwrap()),
            1,
            "the consumer end"
        );

        table.record_offer(input_key(), 1);
        assert_eq!(
            table.forget_node(DataflowId::from_u128(1), &NodeId::new("camera").unwrap()),
            1,
            "the producer end"
        );

        table.record_offer(input_key(), 1);
        assert_eq!(
            table.forget_node(DataflowId::from_u128(2), &NodeId::new("camera").unwrap()),
            0,
            "a different dataflow's node of the same name is a different node"
        );
        assert_eq!(table.forget_dataflow(DataflowId::from_u128(1)), 1);
        assert!(!table.forget(&input_key()));
    }

    #[test]
    fn a_single_entry_can_be_forgotten() {
        let mut table = InputRouteTable::new();
        table.record_offer(input_key(), 3);
        assert!(table.forget(&input_key()));
        assert!(!table.forget(&input_key()));
    }

    #[test]
    fn an_input_route_key_names_both_ends() {
        let key = input_key();
        assert_eq!(key.consumer_port().to_string(), "detect/frames");
        assert!(key.to_string().contains("detect/frames"));
        assert!(key.to_string().contains("camera"));
    }

    #[test]
    fn consumer_side_actions_are_labelled_and_distinguished() {
        let offer = UpgradeAction::OfferInput {
            key: key(),
            consumer: NodeId::new("detect").unwrap(),
            input: DataId::new("frames").unwrap(),
            segment: Box::new(spec()),
        };
        assert_eq!(offer.kind_name(), "offer_input");
        assert!(offer.is_consumer_side());
        assert_eq!(offer.key(), &key());

        let revoke = UpgradeAction::RevokeInput {
            key: key(),
            consumer: NodeId::new("detect").unwrap(),
            input: DataId::new("frames").unwrap(),
            reason: detached(),
        };
        assert_eq!(revoke.kind_name(), "revoke_input");
        assert!(revoke.is_consumer_side());

        let producer_side = UpgradeAction::Downgrade {
            key: key(),
            reason: detached(),
        };
        assert!(!producer_side.is_consumer_side());
    }
}
