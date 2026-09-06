//! [`QosProfile`] and the four ROS-level policy enums.

use core::fmt;
use std::time::Duration as StdDuration;

use astrs_rtps::behavior::QosPolicyId;
use astrs_rtps::discovery::qos::{
    DeadlineQos, DestinationOrderQos, DurabilityQos, HistoryQos, LatencyBudgetQos, LifespanQos,
    LivelinessQos, OwnershipQos, OwnershipStrengthQos, PresentationQos, ReliabilityQos,
    ResourceLimitsQos,
};
use astrs_rtps::discovery::{ReaderQos, WriterQos, check_qos};
use astrs_rtps::structure::DdsDuration;

/// The history depth `rmw_qos_profile_default` uses.
pub const DEFAULT_DEPTH: i32 = 10;

/// The history depth the sensor-data profile uses.
pub const SENSOR_DATA_DEPTH: i32 = 5;

/// The history depth the parameter profiles use.
///
/// A thousand looks absurd until a launch file declares four hundred
/// parameters in one atomic set and every one of them produces an event.
pub const PARAMETERS_DEPTH: i32 = 1_000;

/// The history depth `/rosout` uses.
pub const ROSOUT_DEPTH: i32 = 1_000;

/// How long a `/rosout` message stays valid.
pub const ROSOUT_LIFESPAN: StdDuration = StdDuration::from_secs(10);

/// Whether losses are repaired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Reliability {
    /// Retransmit until every matched reader acknowledges.
    #[default]
    Reliable,
    /// Send once; a lost sample stays lost.
    BestEffort,
    /// Whatever the middleware chooses — [`Reliability::Reliable`] here.
    SystemDefault,
}

impl Reliability {
    /// Every variant.
    pub const ALL: [Self; 3] = [Self::Reliable, Self::BestEffort, Self::SystemDefault];

    /// The concrete kind, with `SystemDefault` resolved.
    #[must_use]
    pub const fn resolved(self) -> Self {
        match self {
            Self::SystemDefault | Self::Reliable => Self::Reliable,
            Self::BestEffort => Self::BestEffort,
        }
    }

    /// True once resolved to `RELIABLE`.
    #[must_use]
    pub const fn is_reliable(self) -> bool {
        matches!(self.resolved(), Self::Reliable)
    }

    /// The name `ros2 topic info --verbose` prints.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Reliable => "RELIABLE",
            Self::BestEffort => "BEST_EFFORT",
            Self::SystemDefault => "SYSTEM_DEFAULT",
        }
    }

    /// The RTPS policy value.
    #[must_use]
    pub const fn to_rtps(self) -> ReliabilityQos {
        if self.is_reliable() {
            ReliabilityQos::reliable()
        } else {
            ReliabilityQos::best_effort()
        }
    }

    /// Read an RTPS policy value back.
    #[must_use]
    pub const fn from_rtps(qos: ReliabilityQos) -> Self {
        if qos.is_reliable() {
            Self::Reliable
        } else {
            Self::BestEffort
        }
    }
}

impl fmt::Display for Reliability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Whether a late joiner is replayed the history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Durability {
    /// Nothing is kept for a reader that joins later.
    #[default]
    Volatile,
    /// The writer's history is replayed to a late joiner — ROS 2's
    /// "latching".
    TransientLocal,
    /// Whatever the middleware chooses — [`Durability::Volatile`] here.
    SystemDefault,
}

impl Durability {
    /// Every variant.
    pub const ALL: [Self; 3] = [Self::Volatile, Self::TransientLocal, Self::SystemDefault];

    /// The concrete kind, with `SystemDefault` resolved.
    #[must_use]
    pub const fn resolved(self) -> Self {
        match self {
            Self::SystemDefault | Self::Volatile => Self::Volatile,
            Self::TransientLocal => Self::TransientLocal,
        }
    }

    /// True once resolved to `TRANSIENT_LOCAL`.
    #[must_use]
    pub const fn is_transient_local(self) -> bool {
        matches!(self.resolved(), Self::TransientLocal)
    }

    /// The name `ros2 topic info --verbose` prints.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Volatile => "VOLATILE",
            Self::TransientLocal => "TRANSIENT_LOCAL",
            Self::SystemDefault => "SYSTEM_DEFAULT",
        }
    }

    /// The RTPS policy value.
    #[must_use]
    pub const fn to_rtps(self) -> DurabilityQos {
        if self.is_transient_local() {
            DurabilityQos::transient_local()
        } else {
            DurabilityQos::volatile()
        }
    }

    /// Read an RTPS policy value back.
    #[must_use]
    pub const fn from_rtps(qos: DurabilityQos) -> Self {
        if qos.kind.replays_history() {
            Self::TransientLocal
        } else {
            Self::Volatile
        }
    }
}

impl fmt::Display for Durability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// How much history an endpoint keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum History {
    /// Keep the last `depth` samples.
    KeepLast {
        /// How many.
        depth: i32,
    },
    /// Keep everything, up to the resource limits.
    KeepAll,
    /// Whatever the middleware chooses — `KEEP_LAST(10)` here.
    SystemDefault,
}

impl Default for History {
    fn default() -> Self {
        Self::KeepLast {
            depth: DEFAULT_DEPTH,
        }
    }
}

impl History {
    /// The concrete kind, with `SystemDefault` resolved.
    #[must_use]
    pub const fn resolved(self) -> Self {
        match self {
            Self::SystemDefault => Self::KeepLast {
                depth: DEFAULT_DEPTH,
            },
            other => other,
        }
    }

    /// The depth, or `None` under `KEEP_ALL`.
    #[must_use]
    pub const fn depth(self) -> Option<i32> {
        match self.resolved() {
            Self::KeepLast { depth } => Some(depth),
            _ => None,
        }
    }

    /// The name `ros2 topic info --verbose` prints.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::KeepLast { .. } => "KEEP_LAST",
            Self::KeepAll => "KEEP_ALL",
            Self::SystemDefault => "SYSTEM_DEFAULT",
        }
    }

    /// The RTPS policy value.
    #[must_use]
    pub const fn to_rtps(self) -> HistoryQos {
        match self.resolved() {
            Self::KeepLast { depth } => HistoryQos::keep_last(depth),
            _ => HistoryQos::keep_all(),
        }
    }

    /// Read an RTPS policy value back.
    #[must_use]
    pub const fn from_rtps(qos: HistoryQos) -> Self {
        match qos.retained() {
            Some(_) => Self::KeepLast { depth: qos.depth },
            None => Self::KeepAll,
        }
    }
}

impl fmt::Display for History {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.resolved() {
            Self::KeepLast { depth } => write!(formatter, "KEEP_LAST({depth})"),
            _ => formatter.write_str(self.name()),
        }
    }
}

/// How liveliness is asserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Liveliness {
    /// The middleware asserts it as long as the participant is alive.
    #[default]
    Automatic,
    /// The application asserts it per topic, by publishing or by calling
    /// `assert_liveliness`.
    ManualByTopic,
    /// Whatever the middleware chooses — [`Liveliness::Automatic`] here.
    SystemDefault,
}

impl Liveliness {
    /// Every variant.
    pub const ALL: [Self; 3] = [Self::Automatic, Self::ManualByTopic, Self::SystemDefault];

    /// The concrete kind, with `SystemDefault` resolved.
    #[must_use]
    pub const fn resolved(self) -> Self {
        match self {
            Self::SystemDefault | Self::Automatic => Self::Automatic,
            Self::ManualByTopic => Self::ManualByTopic,
        }
    }

    /// The name `ros2 topic info --verbose` prints.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Automatic => "AUTOMATIC",
            Self::ManualByTopic => "MANUAL_BY_TOPIC",
            Self::SystemDefault => "SYSTEM_DEFAULT",
        }
    }

    /// The RTPS policy value, given a lease.
    #[must_use]
    pub fn to_rtps(self, lease: Option<StdDuration>) -> LivelinessQos {
        let lease = lease.map_or(DdsDuration::INFINITE, DdsDuration::from_std);
        match self.resolved() {
            Self::ManualByTopic => LivelinessQos::manual_by_topic(lease),
            _ => LivelinessQos {
                lease_duration: lease,
                ..LivelinessQos::automatic()
            },
        }
    }

    /// Read an RTPS policy value back.
    #[must_use]
    pub const fn from_rtps(qos: LivelinessQos) -> Self {
        if qos.kind.is_manual() {
            Self::ManualByTopic
        } else {
            Self::Automatic
        }
    }
}

impl fmt::Display for Liveliness {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// One ROS 2 QoS profile: the seven policies `rmw_qos_profile_t` carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QosProfile {
    /// How much history to keep.
    pub history: History,
    /// Whether losses are repaired.
    pub reliability: Reliability,
    /// Whether late joiners are replayed.
    pub durability: Durability,
    /// The maximum expected gap between samples; `None` is "no contract".
    pub deadline: Option<StdDuration>,
    /// How long a sample stays valid; `None` is "forever".
    pub lifespan: Option<StdDuration>,
    /// How liveliness is asserted.
    pub liveliness: Liveliness,
    /// The liveliness lease; `None` is "infinite".
    pub liveliness_lease: Option<StdDuration>,
    /// When true, the topic name is used verbatim rather than mangled with
    /// `rt/`/`rq/`/`rr/`.
    ///
    /// `rmw_qos_profile_t::avoid_ros_namespace_conventions`. Exactly one
    /// topic in a normal ROS 2 system sets it —
    /// [`ros_discovery_info`](crate::names::mangle::GRAPH_TOPIC) — and a
    /// bridge to a non-ROS DDS system is the other use.
    pub avoid_ros_namespace_conventions: bool,
}

impl Default for QosProfile {
    /// `rmw_qos_profile_default`: reliable, volatile, keep-last 10.
    fn default() -> Self {
        Self {
            history: History::KeepLast {
                depth: DEFAULT_DEPTH,
            },
            reliability: Reliability::Reliable,
            durability: Durability::Volatile,
            deadline: None,
            lifespan: None,
            liveliness: Liveliness::Automatic,
            liveliness_lease: None,
            avoid_ros_namespace_conventions: false,
        }
    }
}

impl QosProfile {
    /// `rmw_qos_profile_sensor_data`: best-effort, volatile, keep-last 5.
    #[must_use]
    pub fn sensor_data() -> Self {
        Self {
            history: History::KeepLast {
                depth: SENSOR_DATA_DEPTH,
            },
            reliability: Reliability::BestEffort,
            ..Self::default()
        }
    }

    /// `rmw_qos_profile_services_default`: reliable, volatile, keep-last 10.
    #[must_use]
    pub fn services_default() -> Self {
        Self::default()
    }

    /// `rmw_qos_profile_parameters`: reliable, volatile, keep-last 1000.
    #[must_use]
    pub fn parameters() -> Self {
        Self {
            history: History::KeepLast {
                depth: PARAMETERS_DEPTH,
            },
            ..Self::default()
        }
    }

    /// `rmw_qos_profile_parameter_events`: reliable, volatile, keep-last
    /// 1000.
    #[must_use]
    pub fn parameter_events() -> Self {
        Self::parameters()
    }

    /// `rmw_qos_profile_system_default`: every policy left to the
    /// middleware.
    #[must_use]
    pub fn system_default() -> Self {
        Self {
            history: History::SystemDefault,
            reliability: Reliability::SystemDefault,
            durability: Durability::SystemDefault,
            liveliness: Liveliness::SystemDefault,
            ..Self::default()
        }
    }

    /// The action status topic's profile: reliable, transient-local,
    /// keep-last 1.
    ///
    /// Transient-local because a client that attaches after a goal has
    /// already reached a terminal state still has to learn what happened.
    #[must_use]
    pub fn action_status_default() -> Self {
        Self {
            history: History::KeepLast { depth: 1 },
            durability: Durability::TransientLocal,
            ..Self::default()
        }
    }

    /// `/rosout`'s profile: reliable, transient-local, keep-last 1000, with
    /// a ten-second lifespan.
    #[must_use]
    pub fn rosout() -> Self {
        Self {
            history: History::KeepLast {
                depth: ROSOUT_DEPTH,
            },
            durability: Durability::TransientLocal,
            lifespan: Some(ROSOUT_LIFESPAN),
            ..Self::default()
        }
    }

    /// `/clock`'s profile: best-effort, volatile, keep-last 1.
    #[must_use]
    pub fn clock() -> Self {
        Self {
            history: History::KeepLast { depth: 1 },
            reliability: Reliability::BestEffort,
            ..Self::default()
        }
    }

    /// The `ros_discovery_info` graph topic's profile.
    ///
    /// Reliable, transient-local, keep-last 1, and the one profile in the
    /// system that sets
    /// [`avoid_ros_namespace_conventions`](Self::avoid_ros_namespace_conventions):
    /// the topic through which two `rmw` implementations exchange node names
    /// cannot itself be mangled by one of their conventions.
    #[must_use]
    pub fn graph() -> Self {
        Self {
            history: History::KeepLast { depth: 1 },
            durability: Durability::TransientLocal,
            avoid_ros_namespace_conventions: true,
            ..Self::default()
        }
    }

    /// A latching profile: reliable, transient-local, keep-last `depth`.
    ///
    /// What `/tf_static` and every "publish once, be seen forever" topic
    /// uses.
    #[must_use]
    pub fn latched(depth: i32) -> Self {
        Self {
            history: History::KeepLast { depth },
            durability: Durability::TransientLocal,
            ..Self::default()
        }
    }

    /// Replace the history depth, keeping `KEEP_LAST`.
    #[must_use]
    pub const fn with_depth(mut self, depth: i32) -> Self {
        self.history = History::KeepLast { depth };
        self
    }

    /// Replace the reliability.
    #[must_use]
    pub const fn with_reliability(mut self, reliability: Reliability) -> Self {
        self.reliability = reliability;
        self
    }

    /// Replace the durability.
    #[must_use]
    pub const fn with_durability(mut self, durability: Durability) -> Self {
        self.durability = durability;
        self
    }

    /// Replace the deadline.
    #[must_use]
    pub const fn with_deadline(mut self, deadline: Option<StdDuration>) -> Self {
        self.deadline = deadline;
        self
    }

    /// Replace the lifespan.
    #[must_use]
    pub const fn with_lifespan(mut self, lifespan: Option<StdDuration>) -> Self {
        self.lifespan = lifespan;
        self
    }

    /// Replace the liveliness kind and lease.
    #[must_use]
    pub const fn with_liveliness(
        mut self,
        liveliness: Liveliness,
        lease: Option<StdDuration>,
    ) -> Self {
        self.liveliness = liveliness;
        self.liveliness_lease = lease;
        self
    }

    /// The history depth, or `None` under `KEEP_ALL`.
    #[must_use]
    pub const fn depth(&self) -> Option<i32> {
        self.history.depth()
    }

    /// True when this profile asks for retransmission.
    #[must_use]
    pub const fn is_reliable(&self) -> bool {
        self.reliability.is_reliable()
    }

    /// True when this profile latches.
    #[must_use]
    pub const fn is_transient_local(&self) -> bool {
        self.durability.is_transient_local()
    }

    /// Every `SystemDefault` replaced by what AstRS chooses.
    ///
    /// What a graph introspection view shows after negotiation, as opposed
    /// to what the application wrote.
    #[must_use]
    pub const fn resolved(mut self) -> Self {
        self.history = self.history.resolved();
        self.reliability = self.reliability.resolved();
        self.durability = self.durability.resolved();
        self.liveliness = self.liveliness.resolved();
        self
    }

    /// The RTPS publication QoS this profile means.
    ///
    /// Every field is written explicitly — see this module's docs on the
    /// writer/reader default asymmetry.
    #[must_use]
    pub fn to_writer_qos(&self) -> WriterQos {
        WriterQos {
            reliability: self.reliability.to_rtps(),
            durability: self.durability.to_rtps(),
            history: self.history.to_rtps(),
            deadline: duration_to_deadline(self.deadline),
            lifespan: duration_to_lifespan(self.lifespan),
            liveliness: self.liveliness.to_rtps(self.liveliness_lease),
            ownership: OwnershipQos::shared(),
            ownership_strength: OwnershipStrengthQos::new(0),
            destination_order: DestinationOrderQos::by_reception(),
            latency_budget: LatencyBudgetQos::immediate(),
            presentation: PresentationQos::instance(),
            resource_limits: ResourceLimitsQos::unlimited(),
        }
    }

    /// The RTPS subscription QoS this profile means.
    #[must_use]
    pub fn to_reader_qos(&self) -> ReaderQos {
        ReaderQos {
            reliability: self.reliability.to_rtps(),
            durability: self.durability.to_rtps(),
            history: self.history.to_rtps(),
            deadline: duration_to_deadline(self.deadline),
            liveliness: self.liveliness.to_rtps(self.liveliness_lease),
            ownership: OwnershipQos::shared(),
            destination_order: DestinationOrderQos::by_reception(),
            latency_budget: LatencyBudgetQos::immediate(),
            presentation: PresentationQos::instance(),
            resource_limits: ResourceLimitsQos::unlimited(),
            time_based_filter: DdsDuration::ZERO,
        }
    }

    /// Read a remote publication's announced QoS back into ROS vocabulary.
    ///
    /// What `ros2 topic info --verbose` prints for a discovered publisher.
    /// `LIFESPAN` survives; `OWNERSHIP` and the rest have no ROS spelling
    /// and are dropped, which is exactly what `rmw` does.
    #[must_use]
    pub fn from_writer_qos(qos: &WriterQos) -> Self {
        Self {
            history: History::from_rtps(qos.history),
            reliability: Reliability::from_rtps(qos.reliability),
            durability: Durability::from_rtps(qos.durability),
            deadline: qos.deadline(),
            lifespan: qos.lifespan(),
            liveliness: Liveliness::from_rtps(qos.liveliness),
            liveliness_lease: lease_of(qos.liveliness),
            avoid_ros_namespace_conventions: false,
        }
    }

    /// Read a remote subscription's announced QoS back into ROS vocabulary.
    #[must_use]
    pub fn from_reader_qos(qos: &ReaderQos) -> Self {
        Self {
            history: History::from_rtps(qos.history),
            reliability: Reliability::from_rtps(qos.reliability),
            durability: Durability::from_rtps(qos.durability),
            deadline: dds_to_std(qos.deadline.period),
            lifespan: None,
            liveliness: Liveliness::from_rtps(qos.liveliness),
            liveliness_lease: lease_of(qos.liveliness),
            avoid_ros_namespace_conventions: false,
        }
    }

    /// True when a subscription with this profile can be served by a
    /// publisher with `offered`.
    #[must_use]
    pub fn can_be_served_by(&self, offered: &Self) -> bool {
        compatible(self, offered)
    }
}

impl fmt::Display for QosProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {} {}",
            self.reliability, self.durability, self.history
        )
    }
}

/// True when a subscription with `requested` may be matched to a publisher
/// with `offered`.
#[must_use]
pub fn compatible(requested: &QosProfile, offered: &QosProfile) -> bool {
    incompatible_policy(requested, offered).is_none()
}

/// The first QoS policy that stops a subscription from matching a
/// publisher, if any.
///
/// The ROS-level spelling of DDS's `OFFERED_INCOMPATIBLE_QOS` status, so a
/// diagnostic can say `RELIABILITY` rather than "no match".
#[must_use]
pub fn incompatible_policy(requested: &QosProfile, offered: &QosProfile) -> Option<QosPolicyId> {
    check_qos(&requested.to_reader_qos(), &offered.to_writer_qos()).err()
}

/// A ROS deadline as an RTPS `DEADLINE`.
fn duration_to_deadline(deadline: Option<StdDuration>) -> DeadlineQos {
    match deadline {
        Some(period) => DeadlineQos {
            period: DdsDuration::from_std(period),
        },
        None => DeadlineQos::infinite(),
    }
}

/// A ROS lifespan as an RTPS `LIFESPAN`.
fn duration_to_lifespan(lifespan: Option<StdDuration>) -> LifespanQos {
    match lifespan {
        Some(duration) => LifespanQos {
            duration: DdsDuration::from_std(duration),
        },
        None => LifespanQos::infinite(),
    }
}

/// An RTPS duration as a `std` one, with `INFINITE` reading as `None`.
fn dds_to_std(duration: DdsDuration) -> Option<StdDuration> {
    if duration.is_infinite() {
        None
    } else {
        duration.to_std()
    }
}

/// A liveliness policy's lease, with `INFINITE` reading as `None`.
fn lease_of(qos: LivelinessQos) -> Option<StdDuration> {
    dds_to_std(qos.lease_duration)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_default_profile_is_reliable_on_both_sides() {
        let profile = QosProfile::default();
        assert!(profile.to_writer_qos().is_reliable());
        assert!(
            profile.to_reader_qos().is_reliable(),
            "the DDS reader default is BEST_EFFORT; the ROS default is not, and this \
             mapping must not inherit the DDS one"
        );
        assert_eq!(profile.depth(), Some(DEFAULT_DEPTH));
        assert!(!profile.is_transient_local());
    }

    #[test]
    fn a_default_pair_matches_itself() {
        let profile = QosProfile::default();
        assert!(profile.can_be_served_by(&profile));
        assert_eq!(incompatible_policy(&profile, &profile), None);
    }

    #[test]
    fn sensor_data_is_best_effort_keep_last_five() {
        let profile = QosProfile::sensor_data();
        assert_eq!(profile.reliability, Reliability::BestEffort);
        assert_eq!(profile.depth(), Some(SENSOR_DATA_DEPTH));
        assert!(!profile.is_reliable());
    }

    #[test]
    fn a_best_effort_publisher_cannot_serve_a_reliable_subscription() {
        let reliable = QosProfile::default();
        let best_effort = QosProfile::sensor_data();
        assert!(!reliable.can_be_served_by(&best_effort));
        assert_eq!(
            incompatible_policy(&reliable, &best_effort),
            Some(QosPolicyId::Reliability)
        );
        assert!(
            best_effort.can_be_served_by(&reliable),
            "a best-effort subscription takes anything"
        );
    }

    #[test]
    fn a_volatile_publisher_cannot_serve_a_latched_subscription() {
        let latched = QosProfile::latched(1);
        assert!(!latched.can_be_served_by(&QosProfile::default()));
        assert_eq!(
            incompatible_policy(&latched, &QosProfile::default()),
            Some(QosPolicyId::Durability)
        );
        assert!(latched.can_be_served_by(&QosProfile::latched(10)));
    }

    #[test]
    fn a_slower_publisher_cannot_meet_a_tighter_deadline() {
        let strict = QosProfile::default().with_deadline(Some(StdDuration::from_millis(100)));
        let slow = QosProfile::default().with_deadline(Some(StdDuration::from_millis(500)));
        assert_eq!(
            incompatible_policy(&strict, &slow),
            Some(QosPolicyId::Deadline)
        );
        let punctual = QosProfile::default().with_deadline(Some(StdDuration::from_millis(50)));
        assert!(strict.can_be_served_by(&punctual));
        assert!(
            slow.can_be_served_by(&strict),
            "a publisher that promises more often than asked is always acceptable"
        );
    }

    #[test]
    fn an_infinite_publisher_deadline_cannot_serve_a_finite_request() {
        let strict = QosProfile::default().with_deadline(Some(StdDuration::from_millis(100)));
        assert_eq!(
            incompatible_policy(&strict, &QosProfile::default()),
            Some(QosPolicyId::Deadline)
        );
    }

    #[test]
    fn system_default_resolves_to_the_default_profile() {
        let resolved = QosProfile::system_default().resolved();
        let default = QosProfile::default();
        assert_eq!(resolved.reliability, default.reliability);
        assert_eq!(resolved.durability, default.durability);
        assert_eq!(resolved.history, default.history);
        assert_eq!(resolved.liveliness, default.liveliness);
        assert_ne!(
            QosProfile::system_default(),
            default,
            "resolving is a view, not an identity: the unresolved profile still \
             remembers that the application said nothing"
        );
    }

    #[test]
    fn system_default_and_default_match_each_other_on_the_wire() {
        assert!(QosProfile::system_default().can_be_served_by(&QosProfile::default()));
        assert!(QosProfile::default().can_be_served_by(&QosProfile::system_default()));
    }

    #[test]
    fn the_named_profiles_are_what_ros_defines() {
        assert_eq!(QosProfile::parameters().depth(), Some(PARAMETERS_DEPTH));
        assert_eq!(
            QosProfile::parameter_events().depth(),
            Some(PARAMETERS_DEPTH)
        );
        assert_eq!(QosProfile::services_default(), QosProfile::default());

        let status = QosProfile::action_status_default();
        assert!(status.is_transient_local());
        assert_eq!(status.depth(), Some(1));

        let rosout = QosProfile::rosout();
        assert!(rosout.is_transient_local());
        assert_eq!(rosout.depth(), Some(ROSOUT_DEPTH));
        assert_eq!(rosout.lifespan, Some(ROSOUT_LIFESPAN));

        let clock = QosProfile::clock();
        assert!(!clock.is_reliable());
        assert_eq!(clock.depth(), Some(1));

        let graph = QosProfile::graph();
        assert!(graph.avoid_ros_namespace_conventions);
        assert!(graph.is_transient_local());
    }

    #[test]
    fn keep_all_has_no_depth() {
        let profile = QosProfile {
            history: History::KeepAll,
            ..QosProfile::default()
        };
        assert_eq!(profile.depth(), None);
        assert_eq!(profile.history.to_rtps(), HistoryQos::keep_all());
        assert_eq!(profile.history.to_string(), "KEEP_ALL");
    }

    #[test]
    fn a_writer_qos_round_trips_through_the_profile() {
        let profile = QosProfile::default()
            .with_depth(7)
            .with_durability(Durability::TransientLocal)
            .with_deadline(Some(StdDuration::from_millis(250)))
            .with_lifespan(Some(StdDuration::from_secs(3)))
            .with_liveliness(
                Liveliness::ManualByTopic,
                Some(StdDuration::from_millis(500)),
            );
        let back = QosProfile::from_writer_qos(&profile.to_writer_qos());
        assert_eq!(back.history, profile.history);
        assert_eq!(back.reliability, profile.reliability);
        assert_eq!(back.durability, profile.durability);
        assert_eq!(back.deadline, profile.deadline);
        assert_eq!(back.lifespan, profile.lifespan);
        assert_eq!(back.liveliness, profile.liveliness);
        assert_eq!(back.liveliness_lease, profile.liveliness_lease);
    }

    #[test]
    fn a_reader_qos_round_trips_through_the_profile() {
        let profile = QosProfile::sensor_data()
            .with_deadline(Some(StdDuration::from_millis(20)))
            .with_liveliness(Liveliness::ManualByTopic, Some(StdDuration::from_secs(1)));
        let back = QosProfile::from_reader_qos(&profile.to_reader_qos());
        assert_eq!(back.reliability, profile.reliability);
        assert_eq!(back.deadline, profile.deadline);
        assert_eq!(back.liveliness_lease, profile.liveliness_lease);
        assert_eq!(
            back.lifespan, None,
            "a reader announces no LIFESPAN; there is nothing to read back"
        );
    }

    #[test]
    fn an_absent_duration_maps_to_infinite_and_back() {
        let profile = QosProfile::default();
        assert!(profile.to_writer_qos().deadline.is_infinite());
        assert!(profile.to_writer_qos().lifespan.is_infinite());
        assert!(
            profile
                .to_writer_qos()
                .liveliness
                .lease_duration
                .is_infinite()
        );
        let back = QosProfile::from_writer_qos(&profile.to_writer_qos());
        assert_eq!(back.deadline, None);
        assert_eq!(back.lifespan, None);
        assert_eq!(back.liveliness_lease, None);
    }

    #[test]
    fn every_policy_enum_resolves_idempotently() {
        for reliability in Reliability::ALL {
            assert_eq!(reliability.resolved(), reliability.resolved().resolved());
            assert!(!reliability.name().is_empty());
            assert_eq!(reliability.to_string(), reliability.name());
        }
        for durability in Durability::ALL {
            assert_eq!(durability.resolved(), durability.resolved().resolved());
            assert_eq!(durability.to_string(), durability.name());
        }
        for liveliness in Liveliness::ALL {
            assert_eq!(liveliness.resolved(), liveliness.resolved().resolved());
            assert_eq!(liveliness.to_string(), liveliness.name());
        }
        for history in [
            History::KeepAll,
            History::SystemDefault,
            History::KeepLast { depth: 3 },
        ] {
            assert_eq!(history.resolved(), history.resolved().resolved());
        }
    }

    #[test]
    fn the_rtps_conversions_are_inverses_for_every_concrete_kind() {
        for reliability in [Reliability::Reliable, Reliability::BestEffort] {
            assert_eq!(Reliability::from_rtps(reliability.to_rtps()), reliability);
        }
        for durability in [Durability::Volatile, Durability::TransientLocal] {
            assert_eq!(Durability::from_rtps(durability.to_rtps()), durability);
        }
        for liveliness in [Liveliness::Automatic, Liveliness::ManualByTopic] {
            assert_eq!(Liveliness::from_rtps(liveliness.to_rtps(None)), liveliness);
        }
        for history in [History::KeepAll, History::KeepLast { depth: 4 }] {
            assert_eq!(History::from_rtps(history.to_rtps()), history);
        }
    }

    #[test]
    fn the_display_form_names_the_three_policies_that_decide_matching() {
        assert_eq!(
            QosProfile::default().to_string(),
            "RELIABLE VOLATILE KEEP_LAST(10)"
        );
        assert_eq!(
            QosProfile::sensor_data().to_string(),
            "BEST_EFFORT VOLATILE KEEP_LAST(5)"
        );
    }

    #[test]
    fn liveliness_leases_survive_the_mapping() {
        let profile = QosProfile::default()
            .with_liveliness(Liveliness::ManualByTopic, Some(StdDuration::from_secs(2)));
        let qos = profile.to_writer_qos();
        assert_eq!(qos.liveliness.lease_duration, DdsDuration::from_secs(2));
        assert!(qos.liveliness.kind.is_manual());
    }

    #[test]
    fn a_manual_subscription_cannot_be_served_by_an_automatic_publisher() {
        let manual = QosProfile::default()
            .with_liveliness(Liveliness::ManualByTopic, Some(StdDuration::from_secs(1)));
        assert_eq!(
            incompatible_policy(&manual, &QosProfile::default()),
            Some(QosPolicyId::Liveliness)
        );
    }
}
