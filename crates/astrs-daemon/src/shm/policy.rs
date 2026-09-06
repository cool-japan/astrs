//! Which plane a route belongs on (§6.2, §6.3).
//!
//! > *Every output starts on the **reliable daemon path**. When the daemon has
//! > confirmed that *all* static same-host consumers attached to the ring …
//! > it issues `RouteUpgrade` to the producer.*
//!
//! This module answers the first half of that sentence: given one consumer of
//! one output, is it a consumer the daemon should even *try* to move onto a
//! ring? [`ShmPolicy::verdict`] is a pure function of facts the event loop
//! already has, with a typed refusal for every "no" so the reason reaches a
//! log line and a metric label rather than evaporating into a `false`.
//!
//! # What is not decided here
//!
//! **Message size.** §6.2 puts the zero-copy threshold in the *producer's*
//! hands — "a heap payload ≥ threshold is copied once into a slot; below
//! threshold it rides the UDS control channel" — and the daemon cannot know
//! the size of a message that has not been produced. So an upgraded route is
//! an *option* the node exercises per message, not a promise that every
//! message avoids the daemon. The threshold travels to the node in
//! [`astrs_wire::NodeConfig::zero_copy_threshold`], and
//! [`ShmPolicy::zero_copy_threshold`] exists only so the daemon reports the
//! same number it handed out.
//!
//! **Attachment.** Whether every consumer has actually mapped the segment is
//! [`crate::shm::AttachmentLedger`]'s question. This module decides who is
//! *expected*; the ledger decides who has arrived.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::shm::{ConsumerFacts, ShmPolicy, ShmRefusal, ShmVerdict};
//!
//! let policy = ShmPolicy::new();
//! assert_eq!(policy.verdict(&ConsumerFacts::default()), ShmVerdict::Eligible);
//!
//! let dynamic = ConsumerFacts { is_dynamic: true, ..ConsumerFacts::default() };
//! assert_eq!(
//!     policy.verdict(&dynamic),
//!     ShmVerdict::Ineligible(ShmRefusal::DynamicConsumer),
//! );
//! ```

use astrs_shm::{DEFAULT_ZERO_COPY_THRESHOLD, SegmentConfig, ShmResult};

/// How many segments one daemon brokers before it stops offering upgrades.
///
/// A ceiling, not a target: each segment costs a mapping, a file descriptor
/// and (by default) 8 MiB of address space, and a graph that would exceed this
/// is better served by the reliable path than by an `ENOMEM` mid-run. Passing
/// it increments the fallback counter, which is exactly the visibility §6.2
/// asks for.
pub const DEFAULT_SEGMENT_BUDGET: usize = 256;

/// Everything the policy needs to know about one consumer of one output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConsumerFacts {
    /// The consumer is a `path: dynamic` node (§8.3): nobody spawned it, its
    /// lifetime is not the daemon's to reason about, and it may attach and
    /// detach at will.
    pub is_dynamic: bool,
    /// The consumer runs on another machine, so no mapping can reach it.
    pub is_remote: bool,
    /// The producer is a virtual source (`astrs/timer/*`, `astrs/logs/*`) —
    /// the daemon itself, which publishes into mailboxes and owns no ring.
    pub is_virtual_source: bool,
    /// A debug tap is copying this output (§13), so the daemon must keep
    /// seeing the bytes.
    pub tap_active: bool,
    /// The consumer has not registered yet, so its process cannot have mapped
    /// anything.
    pub is_unregistered: bool,
}

impl ConsumerFacts {
    /// The facts for an ordinary, spawned, same-host, registered consumer.
    #[must_use]
    pub const fn same_host() -> Self {
        Self {
            is_dynamic: false,
            is_remote: false,
            is_virtual_source: false,
            tap_active: false,
            is_unregistered: false,
        }
    }

    /// Marks the consumer dynamic.
    #[must_use]
    pub const fn dynamic(mut self) -> Self {
        self.is_dynamic = true;
        self
    }

    /// Marks the consumer remote.
    #[must_use]
    pub const fn remote(mut self) -> Self {
        self.is_remote = true;
        self
    }

    /// Marks the producer a virtual source.
    #[must_use]
    pub const fn virtual_source(mut self) -> Self {
        self.is_virtual_source = true;
        self
    }

    /// Marks the output tapped.
    #[must_use]
    pub const fn tapped(mut self) -> Self {
        self.tap_active = true;
        self
    }

    /// Marks the consumer not yet registered.
    #[must_use]
    pub const fn unregistered(mut self) -> Self {
        self.is_unregistered = true;
        self
    }
}

/// Why a consumer stays on the reliable daemon path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShmRefusal {
    /// This build or this host has no shared-memory plane.
    PlaneUnsupported,
    /// The daemon was configured without one.
    PlaneDisabled,
    /// The consumer is dynamic (§8.3).
    DynamicConsumer,
    /// The consumer is on another machine (§6.4).
    RemoteConsumer,
    /// The producer is the daemon itself (§8.4).
    VirtualSource,
    /// A debug tap needs the daemon to keep seeing the bytes (§13).
    TapActive,
    /// The consumer has not registered, so it cannot have attached.
    ConsumerUnregistered,
    /// The daemon is already brokering as many segments as it will.
    SegmentBudgetExhausted,
}

impl ShmRefusal {
    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PlaneUnsupported => "plane_unsupported",
            Self::PlaneDisabled => "plane_disabled",
            Self::DynamicConsumer => "dynamic_consumer",
            Self::RemoteConsumer => "remote_consumer",
            Self::VirtualSource => "virtual_source",
            Self::TapActive => "tap_active",
            Self::ConsumerUnregistered => "consumer_unregistered",
            Self::SegmentBudgetExhausted => "segment_budget_exhausted",
        }
    }

    /// Whether the refusal could change on its own.
    ///
    /// A dynamic consumer will never stop being dynamic; an unregistered one
    /// registers a millisecond later. The distinction is what lets the loop
    /// re-evaluate only the routes that could move, instead of every route on
    /// every tick.
    #[must_use]
    pub const fn is_transient(self) -> bool {
        matches!(
            self,
            Self::ConsumerUnregistered | Self::TapActive | Self::SegmentBudgetExhausted
        )
    }

    /// Whether this refusal should be counted as a §6.2 fallback.
    ///
    /// Only pressure counts. A remote consumer on the reliable path is the
    /// design working, not a resource shortfall, and inflating
    /// `shm_fallback_total` with it would make the one counter an operator
    /// watches useless.
    #[must_use]
    pub const fn is_fallback(self) -> bool {
        matches!(self, Self::SegmentBudgetExhausted)
    }
}

/// What the policy thinks of one consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShmVerdict {
    /// It should be expected to attach to the ring.
    Eligible,
    /// It stays on the reliable daemon path, for this reason.
    Ineligible(ShmRefusal),
}

impl ShmVerdict {
    /// Whether the consumer is expected to attach.
    #[must_use]
    pub const fn is_eligible(self) -> bool {
        matches!(self, Self::Eligible)
    }

    /// The refusal, if there is one.
    #[must_use]
    pub const fn refusal(self) -> Option<ShmRefusal> {
        match self {
            Self::Eligible => None,
            Self::Ineligible(refusal) => Some(refusal),
        }
    }
}

/// The daemon's shared-memory plane policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShmPolicy {
    /// Whether the operator wants the plane at all.
    enabled: bool,
    /// The threshold handed to nodes in their configuration (§24.2).
    zero_copy_threshold: u64,
    /// How many segments this daemon will broker.
    segment_budget: usize,
}

impl ShmPolicy {
    /// The blueprint defaults: plane on, 4 KiB threshold, 256 segments.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            enabled: true,
            zero_copy_threshold: DEFAULT_ZERO_COPY_THRESHOLD as u64,
            segment_budget: DEFAULT_SEGMENT_BUDGET,
        }
    }

    /// A policy that never offers an upgrade.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            zero_copy_threshold: DEFAULT_ZERO_COPY_THRESHOLD as u64,
            segment_budget: 0,
        }
    }

    /// Turns the plane on or off.
    #[must_use]
    pub const fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Sets the zero-copy threshold reported to nodes.
    #[must_use]
    pub const fn with_zero_copy_threshold(mut self, bytes: u64) -> Self {
        self.zero_copy_threshold = bytes;
        self
    }

    /// Sets how many segments the daemon will broker.
    #[must_use]
    pub const fn with_segment_budget(mut self, budget: usize) -> Self {
        self.segment_budget = budget;
        self
    }

    /// Whether the plane is on.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled && Self::platform_supported()
    }

    /// The zero-copy threshold nodes were told about (§24.2).
    #[must_use]
    pub const fn zero_copy_threshold(&self) -> u64 {
        self.zero_copy_threshold
    }

    /// The segment ceiling.
    #[must_use]
    pub const fn segment_budget(&self) -> usize {
        self.segment_budget
    }

    /// Whether this build has a shared-memory plane at all.
    ///
    /// `astrs-shm` compiles everywhere and returns
    /// [`astrs_shm::ShmError::Unsupported`] from every constructor on a
    /// platform it does not implement, so this is a `cfg` rather than a
    /// runtime probe — and it is the *only* `cfg` the daemon needs, which is
    /// the point of that design.
    ///
    /// `true` on Unix (the full plane: segments, the broker, doorbells,
    /// liveness watches) and on Windows (§22, 0.2.0: `astrs_shm::Segment`
    /// maps a named section — see `astrs_shm::os::windows`'s module docs).
    /// This does **not** yet mean a Windows daemon actually brokers
    /// anything: [`ShmPolicy::is_enabled`] also requires
    /// [`crate::shm::SegmentRegistry::is_enabled`], and
    /// `SegmentRegistry::bind` still fails on Windows today because
    /// [`astrs_shm::SegmentBroker`] — the descriptor-passing half, needing a
    /// `windows-sys` feature this workspace does not yet enable — stays
    /// `#[cfg(unix)]`-only. `SegmentRegistry::bind_or_disabled`, the
    /// daemon's actual startup path, already degrades to
    /// [`crate::shm::SegmentRegistry::disabled`] on that failure, so lifting
    /// this `cfg` today changes nothing observable; it removes the one place
    /// that would otherwise have to change again, unrelated to this file,
    /// the day the broker is ported.
    #[must_use]
    pub const fn platform_supported() -> bool {
        cfg!(any(unix, windows))
    }

    /// What the policy thinks of one consumer.
    #[must_use]
    pub fn verdict(&self, facts: &ConsumerFacts) -> ShmVerdict {
        self.verdict_with_segments(facts, 0)
    }

    /// [`ShmPolicy::verdict`], told how many segments are already open.
    ///
    /// The order of the checks is the order an operator would want them
    /// reported in: structural impossibilities first (no plane, wrong kind of
    /// consumer), then the situational ones that may resolve themselves.
    #[must_use]
    pub fn verdict_with_segments(&self, facts: &ConsumerFacts, segments_open: usize) -> ShmVerdict {
        if !Self::platform_supported() {
            return ShmVerdict::Ineligible(ShmRefusal::PlaneUnsupported);
        }
        if !self.enabled {
            return ShmVerdict::Ineligible(ShmRefusal::PlaneDisabled);
        }
        if facts.is_virtual_source {
            return ShmVerdict::Ineligible(ShmRefusal::VirtualSource);
        }
        if facts.is_remote {
            return ShmVerdict::Ineligible(ShmRefusal::RemoteConsumer);
        }
        if facts.is_dynamic {
            return ShmVerdict::Ineligible(ShmRefusal::DynamicConsumer);
        }
        if facts.tap_active {
            return ShmVerdict::Ineligible(ShmRefusal::TapActive);
        }
        if facts.is_unregistered {
            return ShmVerdict::Ineligible(ShmRefusal::ConsumerUnregistered);
        }
        if segments_open >= self.segment_budget {
            return ShmVerdict::Ineligible(ShmRefusal::SegmentBudgetExhausted);
        }
        ShmVerdict::Eligible
    }

    /// The ring geometry for an output whose manifest asks for `pool_size`
    /// bytes (§24.2 `shm_pool_size`).
    ///
    /// # Errors
    ///
    /// [`astrs_shm::ShmError`] if the requested pool cannot be turned into a
    /// legal geometry — a pool so small that no slot fits, or so large that it
    /// exceeds [`astrs_shm::MAX_SEGMENT_LEN`].
    pub fn segment_config(&self, pool_size: u64) -> ShmResult<SegmentConfig> {
        SegmentConfig::from_pool_size(pool_size)
    }
}

impl Default for ShmPolicy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn an_ordinary_same_host_consumer_is_eligible() {
        let policy = ShmPolicy::new();
        assert_eq!(
            policy.verdict(&ConsumerFacts::same_host()),
            ShmVerdict::Eligible
        );
        assert!(policy.verdict(&ConsumerFacts::same_host()).is_eligible());
        assert!(
            policy
                .verdict(&ConsumerFacts::same_host())
                .refusal()
                .is_none()
        );
    }

    #[test]
    fn the_default_facts_describe_an_ordinary_consumer() {
        assert_eq!(ConsumerFacts::default(), ConsumerFacts::same_host());
    }

    #[test]
    fn every_disqualifying_fact_has_its_own_refusal() {
        let policy = ShmPolicy::new();
        let cases = [
            (
                ConsumerFacts::same_host().dynamic(),
                ShmRefusal::DynamicConsumer,
            ),
            (
                ConsumerFacts::same_host().remote(),
                ShmRefusal::RemoteConsumer,
            ),
            (
                ConsumerFacts::same_host().virtual_source(),
                ShmRefusal::VirtualSource,
            ),
            (ConsumerFacts::same_host().tapped(), ShmRefusal::TapActive),
            (
                ConsumerFacts::same_host().unregistered(),
                ShmRefusal::ConsumerUnregistered,
            ),
        ];
        for (facts, expected) in cases {
            assert_eq!(
                policy.verdict(&facts),
                ShmVerdict::Ineligible(expected),
                "{facts:?}"
            );
        }
    }

    #[test]
    fn a_disabled_policy_refuses_everything() {
        let policy = ShmPolicy::disabled();
        assert!(!policy.is_enabled());
        assert_eq!(
            policy.verdict(&ConsumerFacts::same_host()).refusal(),
            Some(ShmRefusal::PlaneDisabled)
        );
    }

    #[test]
    fn the_enabled_flag_can_be_toggled_back_on() {
        let policy = ShmPolicy::disabled().with_enabled(true);
        assert_eq!(policy.is_enabled(), ShmPolicy::platform_supported());
    }

    #[test]
    fn a_full_segment_budget_refuses_new_upgrades() {
        let policy = ShmPolicy::new().with_segment_budget(2);
        assert_eq!(policy.segment_budget(), 2);
        assert!(
            policy
                .verdict_with_segments(&ConsumerFacts::same_host(), 1)
                .is_eligible()
        );
        assert_eq!(
            policy
                .verdict_with_segments(&ConsumerFacts::same_host(), 2)
                .refusal(),
            Some(ShmRefusal::SegmentBudgetExhausted)
        );
    }

    #[test]
    fn a_structural_refusal_beats_a_situational_one() {
        let policy = ShmPolicy::new().with_segment_budget(0);
        let facts = ConsumerFacts::same_host().remote();
        assert_eq!(
            policy.verdict_with_segments(&facts, 100).refusal(),
            Some(ShmRefusal::RemoteConsumer),
            "the reported reason is the one an operator can act on"
        );
    }

    #[test]
    fn only_pressure_counts_as_a_fallback() {
        assert!(ShmRefusal::SegmentBudgetExhausted.is_fallback());
        for refusal in [
            ShmRefusal::PlaneUnsupported,
            ShmRefusal::PlaneDisabled,
            ShmRefusal::DynamicConsumer,
            ShmRefusal::RemoteConsumer,
            ShmRefusal::VirtualSource,
            ShmRefusal::TapActive,
            ShmRefusal::ConsumerUnregistered,
        ] {
            assert!(!refusal.is_fallback(), "{refusal:?}");
        }
    }

    #[test]
    fn transient_refusals_are_the_ones_worth_rechecking() {
        assert!(ShmRefusal::ConsumerUnregistered.is_transient());
        assert!(ShmRefusal::TapActive.is_transient());
        assert!(ShmRefusal::SegmentBudgetExhausted.is_transient());
        assert!(!ShmRefusal::DynamicConsumer.is_transient());
        assert!(!ShmRefusal::RemoteConsumer.is_transient());
    }

    #[test]
    fn refusal_labels_are_distinct() {
        let refusals = [
            ShmRefusal::PlaneUnsupported,
            ShmRefusal::PlaneDisabled,
            ShmRefusal::DynamicConsumer,
            ShmRefusal::RemoteConsumer,
            ShmRefusal::VirtualSource,
            ShmRefusal::TapActive,
            ShmRefusal::ConsumerUnregistered,
            ShmRefusal::SegmentBudgetExhausted,
        ];
        let mut labels: Vec<&str> = refusals.iter().map(|r| r.as_str()).collect();
        let count = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), count);
    }

    #[test]
    fn the_threshold_is_reported_as_configured() {
        let policy = ShmPolicy::new().with_zero_copy_threshold(65_536);
        assert_eq!(policy.zero_copy_threshold(), 65_536);
        assert_eq!(
            ShmPolicy::new().zero_copy_threshold(),
            DEFAULT_ZERO_COPY_THRESHOLD as u64
        );
    }

    #[test]
    fn a_pool_size_becomes_a_legal_geometry() {
        let policy = ShmPolicy::new();
        let config = policy.segment_config(8 * 1024 * 1024).unwrap();
        assert!(config.slot_count() > 0);
        assert!(config.payload_capacity() > 0);
        config.validate().unwrap();
    }

    #[test]
    fn an_impossible_pool_size_is_an_error_not_a_panic() {
        let policy = ShmPolicy::new();
        assert!(policy.segment_config(0).is_err());
    }

    #[test]
    fn the_default_policy_is_the_blueprint_one() {
        assert_eq!(ShmPolicy::default(), ShmPolicy::new());
        assert_eq!(ShmPolicy::new().segment_budget(), DEFAULT_SEGMENT_BUDGET);
    }
}
