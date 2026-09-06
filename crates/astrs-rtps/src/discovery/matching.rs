//! Endpoint QoS sets and the request-versus-offered rules that pair them.
//!
//! [`WriterQos`] and [`ReaderQos`] are the two bundles an endpoint announces
//! over SEDP. They are deliberately *not* one type with a flag: a writer
//! announces `OWNERSHIP_STRENGTH` and `LIFESPAN`, which mean nothing on a
//! reader, and a reader announces `TIME_BASED_FILTER`, which means nothing on
//! a writer. Making that structural removes a whole class of "why is this
//! field zero" question.
//!
//! # Request versus offered
//!
//! DDS matches a reader to a writer by running every RxO policy in one
//! direction: *the reader must not request more than the writer offers*. The
//! comparison differs per policy — reliability and durability order by kind,
//! deadline and latency budget order by duration the other way, ownership
//! must be identical — so each policy owns its own `is_satisfied_by` and
//! [`check_qos`] simply runs them in a fixed order. The order matters only
//! because the *first* failure is the one reported, and reporting
//! `RELIABILITY` before `DEADLINE` is what a DDS user expects.
//!
//! # What is not matched
//!
//! `HISTORY`, `LIFESPAN`, `RESOURCE_LIMITS`, `OWNERSHIP_STRENGTH` and
//! `TIME_BASED_FILTER` are local policies. They ride along in the discovery
//! sample so that graph introspection can show them, and they never block a
//! match.
//!
//! # Example
//!
//! ```
//! use astrs_rtps::behavior::QosPolicyId;
//! use astrs_rtps::discovery::qos::{DurabilityQos, ReliabilityQos};
//! use astrs_rtps::discovery::{ReaderQos, WriterQos, check_qos};
//!
//! let writer = WriterQos::default();                 // RELIABLE, VOLATILE
//! let reader = ReaderQos {
//!     durability: DurabilityQos::transient_local(),  // asks for replay
//!     ..ReaderQos::default()
//! };
//! assert_eq!(check_qos(&reader, &writer), Err(QosPolicyId::Durability));
//!
//! let generous = WriterQos {
//!     durability: DurabilityQos::transient_local(),
//!     ..WriterQos::default()
//! };
//! assert_eq!(check_qos(&reader, &generous), Ok(()));
//! # let _ = ReliabilityQos::reliable();
//! ```

use crate::behavior::error::QosPolicyId;
use crate::discovery::qos::{
    DeadlineQos, DestinationOrderQos, DurabilityQos, HistoryQos, LatencyBudgetQos, LifespanQos,
    LivelinessQos, OwnershipQos, OwnershipStrengthQos, PresentationQos, ReliabilityQos,
    ResourceLimitsQos,
};
use crate::structure::DdsDuration;

/// The QoS a writer offers.
///
/// Every field's [`Default`] is the DDS default for a `DataWriter`, which is
/// also what ROS 2 sends unless the application overrides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WriterQos {
    /// Whether samples are retransmitted. Writers default to `RELIABLE`.
    pub reliability: ReliabilityQos,
    /// Whether late joiners get history. Defaults to `VOLATILE`.
    pub durability: DurabilityQos,
    /// How much history to keep. Local; defaults to `KEEP_LAST 1`.
    pub history: HistoryQos,
    /// The publication-rate contract. Defaults to infinite.
    pub deadline: DeadlineQos,
    /// How long a sample stays valid. Local; defaults to infinite.
    pub lifespan: LifespanQos,
    /// How liveliness is asserted. Defaults to `AUTOMATIC`, infinite lease.
    pub liveliness: LivelinessQos,
    /// Shared or exclusive instance ownership. Defaults to `SHARED`.
    pub ownership: OwnershipQos,
    /// Strength under `EXCLUSIVE` ownership. Local; defaults to zero.
    pub ownership_strength: OwnershipStrengthQos,
    /// Which timestamp orders samples. Defaults to reception time.
    pub destination_order: DestinationOrderQos,
    /// Permitted batching delay. Defaults to zero.
    pub latency_budget: LatencyBudgetQos,
    /// Coherency and ordering scope. Defaults to `INSTANCE`, neither.
    pub presentation: PresentationQos,
    /// Cache ceilings. Local; defaults to unlimited.
    pub resource_limits: ResourceLimitsQos,
}

impl WriterQos {
    /// The ROS 2 "sensor data" profile: best-effort, keep-last 5.
    #[must_use]
    pub fn sensor_data() -> Self {
        Self {
            reliability: ReliabilityQos::best_effort(),
            history: HistoryQos::keep_last(5),
            ..Self::default()
        }
    }

    /// The ROS 2 "services default" profile: reliable, keep-last 10.
    #[must_use]
    pub fn services_default() -> Self {
        Self {
            reliability: ReliabilityQos::reliable(),
            history: HistoryQos::keep_last(10),
            ..Self::default()
        }
    }

    /// A latching profile: reliable, transient-local, keep-last `depth`.
    ///
    /// What `/rosout`, `/tf_static` and every "publish once, be seen forever"
    /// topic uses.
    #[must_use]
    pub fn latched(depth: i32) -> Self {
        Self {
            reliability: ReliabilityQos::reliable(),
            durability: DurabilityQos::transient_local(),
            history: HistoryQos::keep_last(depth),
            ..Self::default()
        }
    }

    /// The QoS every builtin discovery writer uses: reliable,
    /// transient-local, keep-last 1.
    ///
    /// SPDP is the exception — it is best-effort, because a participant
    /// announcement that is missed is simply re-announced.
    #[must_use]
    pub fn builtin_sedp() -> Self {
        Self {
            reliability: ReliabilityQos::reliable(),
            durability: DurabilityQos::transient_local(),
            history: HistoryQos::keep_last(1),
            ..Self::default()
        }
    }

    /// The QoS the SPDP participant writer uses: best-effort, volatile.
    #[must_use]
    pub fn builtin_spdp() -> Self {
        Self {
            reliability: ReliabilityQos::best_effort(),
            durability: DurabilityQos::transient_local(),
            history: HistoryQos::keep_last(1),
            ..Self::default()
        }
    }

    /// True when this writer must retransmit until acknowledged.
    #[must_use]
    pub const fn is_reliable(&self) -> bool {
        self.reliability.is_reliable()
    }

    /// True when a late-joining reader must be replayed the history.
    #[must_use]
    pub const fn replays_history(&self) -> bool {
        self.durability.kind.replays_history()
    }

    /// The lifespan as a `std::time::Duration`, or `None` when infinite.
    #[must_use]
    pub fn lifespan(&self) -> Option<std::time::Duration> {
        if self.lifespan.is_infinite() {
            None
        } else {
            self.lifespan.duration.to_std()
        }
    }

    /// The deadline period as a `std::time::Duration`, or `None` when there
    /// is no deadline.
    #[must_use]
    pub fn deadline(&self) -> Option<std::time::Duration> {
        if self.deadline.is_infinite() {
            None
        } else {
            self.deadline.period.to_std()
        }
    }
}

/// The QoS a reader requests.
///
/// Defaults are the DDS `DataReader` defaults — which differ from the writer
/// defaults in exactly one place: `RELIABILITY` defaults to `BEST_EFFORT`, so
/// a reader that says nothing matches any writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaderQos {
    /// Whether losses must be repaired. Readers default to `BEST_EFFORT`.
    pub reliability: ReliabilityQos,
    /// Whether history is wanted on join. Defaults to `VOLATILE`.
    pub durability: DurabilityQos,
    /// How much history to keep. Local; defaults to `KEEP_LAST 1`.
    pub history: HistoryQos,
    /// The publication-rate requirement. Defaults to infinite.
    pub deadline: DeadlineQos,
    /// The liveliness requirement. Defaults to `AUTOMATIC`, infinite lease.
    pub liveliness: LivelinessQos,
    /// Shared or exclusive. Defaults to `SHARED`.
    pub ownership: OwnershipQos,
    /// Which timestamp orders samples. Defaults to reception time.
    pub destination_order: DestinationOrderQos,
    /// Tolerated batching delay. Defaults to zero.
    pub latency_budget: LatencyBudgetQos,
    /// Coherency and ordering scope. Defaults to `INSTANCE`, neither.
    pub presentation: PresentationQos,
    /// Cache ceilings. Local; defaults to unlimited.
    pub resource_limits: ResourceLimitsQos,
    /// Minimum separation between delivered samples of one instance. Local.
    pub time_based_filter: DdsDuration,
}

impl Default for ReaderQos {
    fn default() -> Self {
        Self {
            reliability: ReliabilityQos::best_effort(),
            durability: DurabilityQos::default(),
            history: HistoryQos::default(),
            deadline: DeadlineQos::default(),
            liveliness: LivelinessQos::default(),
            ownership: OwnershipQos::default(),
            destination_order: DestinationOrderQos::default(),
            latency_budget: LatencyBudgetQos::default(),
            presentation: PresentationQos::default(),
            resource_limits: ResourceLimitsQos::default(),
            time_based_filter: DdsDuration::ZERO,
        }
    }
}

impl ReaderQos {
    /// The ROS 2 "sensor data" profile: best-effort, keep-last 5.
    #[must_use]
    pub fn sensor_data() -> Self {
        Self {
            reliability: ReliabilityQos::best_effort(),
            history: HistoryQos::keep_last(5),
            ..Self::default()
        }
    }

    /// The ROS 2 "default" profile: reliable, keep-last 10.
    #[must_use]
    pub fn reliable(depth: i32) -> Self {
        Self {
            reliability: ReliabilityQos::reliable(),
            history: HistoryQos::keep_last(depth),
            ..Self::default()
        }
    }

    /// A latching profile: reliable, transient-local, keep-last `depth`.
    #[must_use]
    pub fn latched(depth: i32) -> Self {
        Self {
            reliability: ReliabilityQos::reliable(),
            durability: DurabilityQos::transient_local(),
            history: HistoryQos::keep_last(depth),
            ..Self::default()
        }
    }

    /// The QoS every builtin SEDP reader uses.
    #[must_use]
    pub fn builtin_sedp() -> Self {
        Self {
            reliability: ReliabilityQos::reliable(),
            durability: DurabilityQos::transient_local(),
            history: HistoryQos::keep_last(1),
            ..Self::default()
        }
    }

    /// The QoS the SPDP participant reader uses.
    #[must_use]
    pub fn builtin_spdp() -> Self {
        Self {
            reliability: ReliabilityQos::best_effort(),
            durability: DurabilityQos::transient_local(),
            history: HistoryQos::keep_last(1),
            ..Self::default()
        }
    }

    /// True when this reader expects losses to be repaired.
    #[must_use]
    pub const fn is_reliable(&self) -> bool {
        self.reliability.is_reliable()
    }

    /// True when this reader wants the writer's history on join.
    #[must_use]
    pub const fn wants_history(&self) -> bool {
        self.durability.kind.replays_history()
    }
}

/// Run every request-versus-offered rule, in the order DDS reports them.
///
/// Returns the *first* policy that fails, which is what a DDS
/// `OFFERED_INCOMPATIBLE_QOS` status carries. `Ok(())` means the pair may be
/// matched.
///
/// # Errors
///
/// The [`QosPolicyId`] of the first policy the writer cannot satisfy.
pub fn check_qos(requested: &ReaderQos, offered: &WriterQos) -> Result<(), QosPolicyId> {
    if !requested.reliability.is_satisfied_by(offered.reliability) {
        return Err(QosPolicyId::Reliability);
    }
    if !requested.durability.is_satisfied_by(offered.durability) {
        return Err(QosPolicyId::Durability);
    }
    if !requested.ownership.is_satisfied_by(offered.ownership) {
        return Err(QosPolicyId::Ownership);
    }
    if !requested.deadline.is_satisfied_by(offered.deadline) {
        return Err(QosPolicyId::Deadline);
    }
    if !requested.liveliness.is_satisfied_by(offered.liveliness) {
        return Err(QosPolicyId::Liveliness);
    }
    if !requested
        .destination_order
        .is_satisfied_by(offered.destination_order)
    {
        return Err(QosPolicyId::DestinationOrder);
    }
    if !requested
        .latency_budget
        .is_satisfied_by(offered.latency_budget)
    {
        return Err(QosPolicyId::LatencyBudget);
    }
    if !requested.presentation.is_satisfied_by(offered.presentation) {
        return Err(QosPolicyId::Presentation);
    }
    Ok(())
}

/// True when `requested` and `offered` may be matched.
///
/// The boolean form of [`check_qos`], for a caller that does not need to
/// report *which* policy failed.
#[must_use]
pub fn qos_matches(requested: &ReaderQos, offered: &WriterQos) -> bool {
    check_qos(requested, offered).is_ok()
}

/// Whether two endpoints agree on topic and type, and their QoS is
/// compatible.
///
/// The complete matching predicate SEDP applies: names first, because a
/// mismatch there is not an error worth reporting to the application, then
/// QoS, because a mismatch there is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchOutcome {
    /// The endpoints belong to different topics or different types; nothing
    /// to report.
    Unrelated,
    /// Same topic and type, but a QoS policy conflicts.
    Incompatible {
        /// The first policy that failed.
        policy: QosPolicyId,
    },
    /// The endpoints match and should be wired together.
    Matched,
}

impl MatchOutcome {
    /// True only for [`MatchOutcome::Matched`].
    #[must_use]
    pub const fn is_matched(self) -> bool {
        matches!(self, Self::Matched)
    }

    /// The failing policy, when there was one.
    #[must_use]
    pub const fn incompatible_policy(self) -> Option<QosPolicyId> {
        match self {
            Self::Incompatible { policy } => Some(policy),
            _ => None,
        }
    }
}

/// Decide whether a reader and a writer should be wired together.
///
/// `topic` and `type_name` come from the SEDP samples; DDS requires both to
/// be identical, and this crate does not implement the XTypes assignability
/// relaxation that would let two different type names match.
#[must_use]
pub fn match_endpoints(
    reader_topic: &str,
    reader_type: &str,
    requested: &ReaderQos,
    writer_topic: &str,
    writer_type: &str,
    offered: &WriterQos,
) -> MatchOutcome {
    if reader_topic != writer_topic || reader_type != writer_type {
        return MatchOutcome::Unrelated;
    }
    match check_qos(requested, offered) {
        Ok(()) => MatchOutcome::Matched,
        Err(policy) => MatchOutcome::Incompatible { policy },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::discovery::qos::{HistoryKind, LivelinessQos};

    #[test]
    fn writer_defaults_reliable_reader_defaults_best_effort() {
        assert!(WriterQos::default().is_reliable());
        assert!(!ReaderQos::default().is_reliable());
    }

    #[test]
    fn defaults_match_each_other() {
        assert_eq!(
            check_qos(&ReaderQos::default(), &WriterQos::default()),
            Ok(())
        );
    }

    #[test]
    fn a_best_effort_writer_cannot_serve_a_reliable_reader() {
        let reader = ReaderQos::reliable(10);
        let writer = WriterQos::sensor_data();
        assert_eq!(
            check_qos(&reader, &writer),
            Err(QosPolicyId::Reliability),
            "best-effort cannot satisfy reliable"
        );
        assert!(!qos_matches(&reader, &writer));
    }

    #[test]
    fn a_volatile_writer_cannot_serve_a_transient_local_reader() {
        let reader = ReaderQos::latched(1);
        let writer = WriterQos::services_default();
        assert_eq!(check_qos(&reader, &writer), Err(QosPolicyId::Durability));
        assert_eq!(check_qos(&reader, &WriterQos::latched(1)), Ok(()));
    }

    #[test]
    fn reliability_is_reported_before_durability() {
        let reader = ReaderQos::latched(1);
        let writer = WriterQos::sensor_data();
        assert_eq!(
            check_qos(&reader, &writer),
            Err(QosPolicyId::Reliability),
            "both fail; reliability is reported first"
        );
    }

    #[test]
    fn ownership_mismatch_is_reported() {
        let reader = ReaderQos {
            ownership: crate::discovery::qos::OwnershipQos::exclusive(),
            ..ReaderQos::default()
        };
        assert_eq!(
            check_qos(&reader, &WriterQos::default()),
            Err(QosPolicyId::Ownership)
        );
    }

    #[test]
    fn deadline_mismatch_is_reported() {
        let reader = ReaderQos {
            deadline: DeadlineQos::from_millis(100),
            ..ReaderQos::default()
        };
        let writer = WriterQos {
            deadline: DeadlineQos::from_millis(500),
            ..WriterQos::default()
        };
        assert_eq!(check_qos(&reader, &writer), Err(QosPolicyId::Deadline));
        let punctual = WriterQos {
            deadline: DeadlineQos::from_millis(50),
            ..WriterQos::default()
        };
        assert_eq!(check_qos(&reader, &punctual), Ok(()));
    }

    #[test]
    fn liveliness_mismatch_is_reported() {
        let reader = ReaderQos {
            liveliness: LivelinessQos::manual_by_topic(DdsDuration::from_millis(500)),
            ..ReaderQos::default()
        };
        assert_eq!(
            check_qos(&reader, &WriterQos::default()),
            Err(QosPolicyId::Liveliness)
        );
    }

    #[test]
    fn history_never_blocks_a_match() {
        let reader = ReaderQos {
            history: HistoryQos::keep_all(),
            ..ReaderQos::default()
        };
        let writer = WriterQos {
            history: HistoryQos::keep_last(1),
            ..WriterQos::default()
        };
        assert_eq!(check_qos(&reader, &writer), Ok(()));
        assert_eq!(reader.history.kind, HistoryKind::KeepAll);
    }

    #[test]
    fn different_topics_are_unrelated_not_incompatible() {
        let outcome = match_endpoints(
            "rt/left",
            "std_msgs::msg::dds_::String_",
            &ReaderQos::reliable(1),
            "rt/right",
            "std_msgs::msg::dds_::String_",
            &WriterQos::sensor_data(),
        );
        assert_eq!(outcome, MatchOutcome::Unrelated);
        assert!(!outcome.is_matched());
        assert_eq!(outcome.incompatible_policy(), None);
    }

    #[test]
    fn different_types_on_one_topic_are_unrelated() {
        let outcome = match_endpoints(
            "rt/chatter",
            "std_msgs::msg::dds_::String_",
            &ReaderQos::default(),
            "rt/chatter",
            "std_msgs::msg::dds_::Int32_",
            &WriterQos::default(),
        );
        assert_eq!(outcome, MatchOutcome::Unrelated);
    }

    #[test]
    fn same_topic_incompatible_qos_names_the_policy() {
        let outcome = match_endpoints(
            "rt/chatter",
            "std_msgs::msg::dds_::String_",
            &ReaderQos::reliable(1),
            "rt/chatter",
            "std_msgs::msg::dds_::String_",
            &WriterQos::sensor_data(),
        );
        assert_eq!(
            outcome,
            MatchOutcome::Incompatible {
                policy: QosPolicyId::Reliability
            }
        );
        assert_eq!(
            outcome.incompatible_policy(),
            Some(QosPolicyId::Reliability)
        );
    }

    #[test]
    fn a_compatible_pair_matches() {
        let outcome = match_endpoints(
            "rt/chatter",
            "std_msgs::msg::dds_::String_",
            &ReaderQos::reliable(10),
            "rt/chatter",
            "std_msgs::msg::dds_::String_",
            &WriterQos::services_default(),
        );
        assert!(outcome.is_matched());
    }

    #[test]
    fn builtin_profiles_pair_up() {
        assert_eq!(
            check_qos(&ReaderQos::builtin_sedp(), &WriterQos::builtin_sedp()),
            Ok(())
        );
        assert_eq!(
            check_qos(&ReaderQos::builtin_spdp(), &WriterQos::builtin_spdp()),
            Ok(())
        );
        assert!(!WriterQos::builtin_spdp().is_reliable());
        assert!(WriterQos::builtin_sedp().is_reliable());
        assert!(WriterQos::builtin_sedp().replays_history());
    }

    #[test]
    fn lifespan_and_deadline_render_as_std_durations() {
        let writer = WriterQos {
            lifespan: LifespanQos::from_millis(1_500),
            deadline: DeadlineQos::from_millis(250),
            ..WriterQos::default()
        };
        assert_eq!(
            writer.lifespan(),
            Some(std::time::Duration::from_millis(1_500))
        );
        assert_eq!(
            writer.deadline(),
            Some(std::time::Duration::from_millis(250))
        );
        assert_eq!(WriterQos::default().lifespan(), None);
        assert_eq!(WriterQos::default().deadline(), None);
    }

    #[test]
    fn latched_readers_want_history() {
        assert!(ReaderQos::latched(5).wants_history());
        assert!(!ReaderQos::sensor_data().wants_history());
    }
}
