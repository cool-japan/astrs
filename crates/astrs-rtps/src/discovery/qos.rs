//! DDS QoS policies, in the shape SEDP puts them on the wire.
//!
//! Every policy here is a small plain value with one job: to survive the
//! round trip through a `PID_*` parameter and come back identical. Each one
//! implements `astrs-cdr`'s [`CdrSerialize`] and [`CdrDeserialize`], so
//! writing a policy into a discovery sample is
//! [`ParameterList::push_value`](astrs_cdr::ParameterList::push_value) and
//! reading one back is
//! [`Parameter::decode_value`](astrs_cdr::Parameter::decode_value) — the
//! parameter framing, the padding and the four-octet alignment are all
//! `astrs-cdr`'s problem, not this module's.
//!
//! # The enumerations do not match the DDS API
//!
//! This is the trap. `DDS::ReliabilityQosPolicyKind` numbers `BEST_EFFORT` 0
//! and `RELIABLE` 1, but the *wire* numbers them 1 and 2 — the
//! DDSI-RTPS 2.3 interoperability profile (§9.6.2.2) shifts them by one so
//! that a zeroed field is not a valid kind. Every other kind in this module
//! is zero-based; reliability alone is not. Getting it wrong turns a reliable
//! writer into a best-effort one silently, so [`ReliabilityKind`] carries its
//! wire value in the discriminant and the conversion is total.
//!
//! # Durations
//!
//! QoS durations are **DDS** durations — seconds plus *nanoseconds*
//! ([`DdsDuration`]) — not RTPS durations, whose second field is a 2⁻³²
//! binary fraction. Both are eight octets, so a confusion between them is a
//! factor-of-4.29 error rather than a decode failure. Only
//! `PID_PARTICIPANT_LEASE_DURATION` uses the RTPS flavour; every policy here
//! uses the DDS one.
//!
//! # Defaults
//!
//! Each policy's [`Default`] is the DDS specification's default, which is
//! also what ROS 2 sends when the application does not override it — except
//! [`ReliabilityQos`], where DDS defaults a *writer* to `RELIABLE` and a
//! *reader* to `BEST_EFFORT`. `Default` here is the reliable one and
//! [`ReaderQos`](crate::discovery::ReaderQos) overrides it, so the asymmetry
//! lives in exactly one place.
//!
//! # Example
//!
//! ```
//! use astrs_cdr::{Encoding, ParameterId, ParameterList, pid};
//! use astrs_rtps::discovery::qos::{DurabilityQos, ReliabilityQos};
//!
//! let mut announcement = ParameterList::new(Encoding::DISCOVERY);
//! announcement.push_value(ParameterId::new(pid::RELIABILITY), &ReliabilityQos::reliable())?;
//! announcement.push_value(ParameterId::new(pid::DURABILITY), &DurabilityQos::transient_local())?;
//!
//! if let Some(parameter) = announcement.get_by_base(pid::RELIABILITY) {
//!     let read: ReliabilityQos = parameter.decode_value(Encoding::DISCOVERY)?;
//!     assert!(read.is_reliable());
//! }
//! # Ok::<(), astrs_cdr::CdrError>(())
//! ```

use core::fmt;

use astrs_cdr::{CdrDeserialize, CdrReader, CdrResult, CdrSerialize, CdrType, CdrWriter};

use crate::structure::DdsDuration;

/// Octets a `RELIABILITY` policy occupies: kind plus a DDS duration.
pub const RELIABILITY_LEN: usize = 12;
/// Octets a `DURABILITY` policy occupies: one kind.
pub const DURABILITY_LEN: usize = 4;
/// Octets a `HISTORY` policy occupies: kind plus depth.
pub const HISTORY_LEN: usize = 8;
/// Octets a `DEADLINE`, `LIFESPAN` or `LATENCY_BUDGET` policy occupies.
pub const DURATION_POLICY_LEN: usize = 8;
/// Octets a `LIVELINESS` policy occupies: kind plus a DDS duration.
pub const LIVELINESS_LEN: usize = 12;
/// Octets an `OWNERSHIP` or `DESTINATION_ORDER` policy occupies.
pub const KIND_ONLY_LEN: usize = 4;
/// Octets a `PRESENTATION` policy occupies: scope plus two booleans, padded.
pub const PRESENTATION_LEN: usize = 8;
/// Octets a `RESOURCE_LIMITS` policy occupies: three limits.
pub const RESOURCE_LIMITS_LEN: usize = 12;

/// A depth of `-1` in a `HISTORY` policy: unlimited, as `KEEP_ALL` implies.
pub const UNLIMITED_DEPTH: i32 = -1;

/// The default `KEEP_LAST` depth, per the DDS specification.
pub const DEFAULT_HISTORY_DEPTH: i32 = 1;

// ────────────────────────────────────────────────────────────────────────────
// RELIABILITY
// ────────────────────────────────────────────────────────────────────────────

/// `ReliabilityQosPolicyKind`, in its **wire** numbering.
///
/// One-based, not zero-based. See the [module documentation](self).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(i32)]
pub enum ReliabilityKind {
    /// `BEST_EFFORT` — samples may be lost; nothing is retransmitted.
    BestEffort = 1,
    /// `RELIABLE` — every sample is retransmitted until acknowledged.
    #[default]
    Reliable = 2,
}

impl ReliabilityKind {
    /// The octets this kind is written as.
    #[must_use]
    pub const fn to_wire(self) -> i32 {
        self as i32
    }

    /// Read a kind from the wire.
    ///
    /// An unrecognised value decodes as `BEST_EFFORT`, the weaker of the two:
    /// a peer that means something this build does not understand must not be
    /// promoted to reliable by accident.
    #[must_use]
    pub const fn from_wire(value: i32) -> Self {
        match value {
            2 => Self::Reliable,
            _ => Self::BestEffort,
        }
    }

    /// The DDS specification's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::BestEffort => "BEST_EFFORT",
            Self::Reliable => "RELIABLE",
        }
    }
}

impl fmt::Display for ReliabilityKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// `RELIABILITY` (`PID_RELIABILITY`, `0x001a`).
///
/// Twelve octets: the kind, then `max_blocking_time` as a DDS duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReliabilityQos {
    /// Whether lost samples are retransmitted.
    pub kind: ReliabilityKind,
    /// How long a write may block waiting for history space.
    pub max_blocking_time: DdsDuration,
}

impl ReliabilityQos {
    /// `RELIABLE`, blocking up to 100 ms — the DDS writer default, and what
    /// ROS 2's default profile sends.
    #[must_use]
    pub const fn reliable() -> Self {
        Self {
            kind: ReliabilityKind::Reliable,
            max_blocking_time: DdsDuration::from_millis(100),
        }
    }

    /// `BEST_EFFORT`, which never blocks.
    #[must_use]
    pub const fn best_effort() -> Self {
        Self {
            kind: ReliabilityKind::BestEffort,
            max_blocking_time: DdsDuration::ZERO,
        }
    }

    /// True when this policy asks for retransmission.
    #[must_use]
    pub const fn is_reliable(self) -> bool {
        matches!(self.kind, ReliabilityKind::Reliable)
    }

    /// Request-versus-offered: a reader may ask for no more than the writer
    /// offers, and `RELIABLE` is more than `BEST_EFFORT`.
    ///
    /// `self` is the reader's request, `offered` the writer's offer.
    #[must_use]
    pub const fn is_satisfied_by(self, offered: Self) -> bool {
        self.kind.to_wire() <= offered.kind.to_wire()
    }
}

impl Default for ReliabilityQos {
    fn default() -> Self {
        Self::reliable()
    }
}

impl CdrType for ReliabilityQos {
    const MIN_SERIALIZED_SIZE: usize = RELIABILITY_LEN;
}

impl CdrSerialize for ReliabilityQos {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_i32(self.kind.to_wire())?;
        self.max_blocking_time.serialize(writer)
    }
}

impl<'de> CdrDeserialize<'de> for ReliabilityQos {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        let kind = ReliabilityKind::from_wire(reader.read_i32()?);
        let max_blocking_time = DdsDuration::deserialize(reader)?;
        Ok(Self {
            kind,
            max_blocking_time,
        })
    }
}

// ────────────────────────────────────────────────────────────────────────────
// DURABILITY
// ────────────────────────────────────────────────────────────────────────────

/// `DurabilityQosPolicyKind`, zero-based as the wire has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(i32)]
pub enum DurabilityKind {
    /// `VOLATILE` — a late-joining reader gets nothing that came before it.
    #[default]
    Volatile = 0,
    /// `TRANSIENT_LOCAL` — the writer replays its history to a late joiner.
    TransientLocal = 1,
    /// `TRANSIENT` — durability service backed, not implemented by this crate.
    Transient = 2,
    /// `PERSISTENT` — disk backed, not implemented by this crate.
    Persistent = 3,
}

impl DurabilityKind {
    /// The octets this kind is written as.
    #[must_use]
    pub const fn to_wire(self) -> i32 {
        self as i32
    }

    /// Read a kind from the wire; anything unrecognised is `VOLATILE`.
    #[must_use]
    pub const fn from_wire(value: i32) -> Self {
        match value {
            1 => Self::TransientLocal,
            2 => Self::Transient,
            3 => Self::Persistent,
            _ => Self::Volatile,
        }
    }

    /// True when a writer of this kind must replay history to a late joiner.
    #[must_use]
    pub const fn replays_history(self) -> bool {
        !matches!(self, Self::Volatile)
    }

    /// The DDS specification's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Volatile => "VOLATILE",
            Self::TransientLocal => "TRANSIENT_LOCAL",
            Self::Transient => "TRANSIENT",
            Self::Persistent => "PERSISTENT",
        }
    }
}

impl fmt::Display for DurabilityKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// `DURABILITY` (`PID_DURABILITY`, `0x001d`). Four octets: one kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DurabilityQos {
    /// How long the writer keeps samples for readers that are not there yet.
    pub kind: DurabilityKind,
}

impl DurabilityQos {
    /// `VOLATILE` — the DDS default.
    #[must_use]
    pub const fn volatile() -> Self {
        Self {
            kind: DurabilityKind::Volatile,
        }
    }

    /// `TRANSIENT_LOCAL` — replay history to late joiners.
    #[must_use]
    pub const fn transient_local() -> Self {
        Self {
            kind: DurabilityKind::TransientLocal,
        }
    }

    /// Request-versus-offered: a reader may ask for no more durability than
    /// the writer offers.
    #[must_use]
    pub const fn is_satisfied_by(self, offered: Self) -> bool {
        self.kind.to_wire() <= offered.kind.to_wire()
    }
}

impl CdrType for DurabilityQos {
    const MIN_SERIALIZED_SIZE: usize = DURABILITY_LEN;
}

impl CdrSerialize for DurabilityQos {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_i32(self.kind.to_wire())
    }
}

impl<'de> CdrDeserialize<'de> for DurabilityQos {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        Ok(Self {
            kind: DurabilityKind::from_wire(reader.read_i32()?),
        })
    }
}

// ────────────────────────────────────────────────────────────────────────────
// HISTORY
// ────────────────────────────────────────────────────────────────────────────

/// `HistoryQosPolicyKind`, zero-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(i32)]
pub enum HistoryKind {
    /// `KEEP_LAST` — retain the newest `depth` samples per instance.
    #[default]
    KeepLast = 0,
    /// `KEEP_ALL` — retain every sample until it is acknowledged.
    KeepAll = 1,
}

impl HistoryKind {
    /// The octets this kind is written as.
    #[must_use]
    pub const fn to_wire(self) -> i32 {
        self as i32
    }

    /// Read a kind from the wire; anything unrecognised is `KEEP_LAST`.
    #[must_use]
    pub const fn from_wire(value: i32) -> Self {
        match value {
            1 => Self::KeepAll,
            _ => Self::KeepLast,
        }
    }

    /// The DDS specification's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::KeepLast => "KEEP_LAST",
            Self::KeepAll => "KEEP_ALL",
        }
    }
}

impl fmt::Display for HistoryKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// `HISTORY` (`PID_HISTORY`, `0x0040`). Eight octets: kind, then depth.
///
/// `HISTORY` is a **local** policy — DDS does not subject it to
/// request-versus-offered matching — but it is announced anyway, because a
/// tool that draws the graph wants to show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryQos {
    /// Whether the depth bounds the history.
    pub kind: HistoryKind,
    /// Samples retained per instance under `KEEP_LAST`.
    pub depth: i32,
}

impl HistoryQos {
    /// `KEEP_LAST` with the given depth.
    ///
    /// A depth of zero or below is raised to one: DDS defines `KEEP_LAST 0`
    /// as invalid, and silently keeping nothing is the worst possible reading
    /// of it.
    #[must_use]
    pub const fn keep_last(depth: i32) -> Self {
        Self {
            kind: HistoryKind::KeepLast,
            depth: if depth < 1 { 1 } else { depth },
        }
    }

    /// `KEEP_ALL`, whose depth field is unlimited.
    #[must_use]
    pub const fn keep_all() -> Self {
        Self {
            kind: HistoryKind::KeepAll,
            depth: UNLIMITED_DEPTH,
        }
    }

    /// The number of samples the cache should retain per instance, or `None`
    /// for unbounded.
    #[must_use]
    pub const fn retained(self) -> Option<usize> {
        match self.kind {
            HistoryKind::KeepAll => None,
            HistoryKind::KeepLast => {
                if self.depth < 1 {
                    Some(1)
                } else {
                    Some(self.depth as usize)
                }
            }
        }
    }
}

impl Default for HistoryQos {
    fn default() -> Self {
        Self::keep_last(DEFAULT_HISTORY_DEPTH)
    }
}

impl CdrType for HistoryQos {
    const MIN_SERIALIZED_SIZE: usize = HISTORY_LEN;
}

impl CdrSerialize for HistoryQos {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_i32(self.kind.to_wire())?;
        writer.write_i32(self.depth)
    }
}

impl<'de> CdrDeserialize<'de> for HistoryQos {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        let kind = HistoryKind::from_wire(reader.read_i32()?);
        let depth = reader.read_i32()?;
        Ok(Self { kind, depth })
    }
}

// ────────────────────────────────────────────────────────────────────────────
// DEADLINE, LIFESPAN, LATENCY_BUDGET, TIME_BASED_FILTER
// ────────────────────────────────────────────────────────────────────────────

/// `DEADLINE` (`PID_DEADLINE`, `0x0023`). Eight octets: one DDS duration.
///
/// The contract a writer offers: successive samples of an instance arrive no
/// further apart than `period`. RxO runs the other way from most policies —
/// the reader asks for a period *at least* as long as the writer offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadlineQos {
    /// The maximum gap between successive samples of one instance.
    pub period: DdsDuration,
}

impl DeadlineQos {
    /// No deadline — the DDS default.
    #[must_use]
    pub const fn infinite() -> Self {
        Self {
            period: DdsDuration::INFINITE,
        }
    }

    /// A deadline of `millis` milliseconds.
    #[must_use]
    pub const fn from_millis(millis: u32) -> Self {
        Self {
            period: DdsDuration::from_millis(millis),
        }
    }

    /// True when no deadline is being asked for.
    #[must_use]
    pub const fn is_infinite(self) -> bool {
        self.period.is_infinite()
    }

    /// Request-versus-offered: the reader's period must be at least the
    /// writer's, because a writer that promises "every second" cannot serve a
    /// reader that demands "every 100 ms".
    #[must_use]
    pub fn is_satisfied_by(self, offered: Self) -> bool {
        compare_dds_durations(self.period, offered.period).is_ge()
    }
}

impl Default for DeadlineQos {
    fn default() -> Self {
        Self::infinite()
    }
}

impl CdrType for DeadlineQos {
    const MIN_SERIALIZED_SIZE: usize = DURATION_POLICY_LEN;
}

impl CdrSerialize for DeadlineQos {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        self.period.serialize(writer)
    }
}

impl<'de> CdrDeserialize<'de> for DeadlineQos {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        Ok(Self {
            period: DdsDuration::deserialize(reader)?,
        })
    }
}

/// `LIFESPAN` (`PID_LIFESPAN`, `0x002b`). Eight octets: one DDS duration.
///
/// Purely local to the writer: a sample older than `duration` is dropped from
/// the history and never retransmitted. Not subject to RxO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifespanQos {
    /// How long a sample stays valid after it is written.
    pub duration: DdsDuration,
}

impl LifespanQos {
    /// Samples never expire — the DDS default.
    #[must_use]
    pub const fn infinite() -> Self {
        Self {
            duration: DdsDuration::INFINITE,
        }
    }

    /// A lifespan of `millis` milliseconds.
    #[must_use]
    pub const fn from_millis(millis: u32) -> Self {
        Self {
            duration: DdsDuration::from_millis(millis),
        }
    }

    /// True when samples never expire.
    #[must_use]
    pub const fn is_infinite(self) -> bool {
        self.duration.is_infinite()
    }
}

impl Default for LifespanQos {
    fn default() -> Self {
        Self::infinite()
    }
}

impl CdrType for LifespanQos {
    const MIN_SERIALIZED_SIZE: usize = DURATION_POLICY_LEN;
}

impl CdrSerialize for LifespanQos {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        self.duration.serialize(writer)
    }
}

impl<'de> CdrDeserialize<'de> for LifespanQos {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        Ok(Self {
            duration: DdsDuration::deserialize(reader)?,
        })
    }
}

/// `LATENCY_BUDGET` (`PID_LATENCY_BUDGET`, `0x0027`). Eight octets.
///
/// A hint, not a contract: how long the middleware may batch before
/// delivering. RxO requires the reader's duration to be at least the
/// writer's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencyBudgetQos {
    /// The permitted delay.
    pub duration: DdsDuration,
}

impl Default for LatencyBudgetQos {
    fn default() -> Self {
        Self::immediate()
    }
}

impl LatencyBudgetQos {
    /// Deliver as soon as possible — the DDS default of zero.
    #[must_use]
    pub const fn immediate() -> Self {
        Self {
            duration: DdsDuration::ZERO,
        }
    }

    /// Request-versus-offered: the reader must tolerate at least what the
    /// writer intends to take.
    #[must_use]
    pub fn is_satisfied_by(self, offered: Self) -> bool {
        compare_dds_durations(self.duration, offered.duration).is_ge()
    }
}

impl CdrType for LatencyBudgetQos {
    const MIN_SERIALIZED_SIZE: usize = DURATION_POLICY_LEN;
}

impl CdrSerialize for LatencyBudgetQos {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        self.duration.serialize(writer)
    }
}

impl<'de> CdrDeserialize<'de> for LatencyBudgetQos {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        Ok(Self {
            duration: DdsDuration::deserialize(reader)?,
        })
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LIVELINESS
// ────────────────────────────────────────────────────────────────────────────

/// `LivelinessQosPolicyKind`, zero-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(i32)]
pub enum LivelinessKind {
    /// `AUTOMATIC` — the participant's own SPDP announcements prove liveliness.
    #[default]
    Automatic = 0,
    /// `MANUAL_BY_PARTICIPANT` — any assertion from the participant counts for
    /// all of its writers, carried on the WLP topic.
    ManualByParticipant = 1,
    /// `MANUAL_BY_TOPIC` — each writer must assert itself, by writing a sample
    /// or by sending a HEARTBEAT with the `L` flag set.
    ManualByTopic = 2,
}

impl LivelinessKind {
    /// The octets this kind is written as.
    #[must_use]
    pub const fn to_wire(self) -> i32 {
        self as i32
    }

    /// Read a kind from the wire; anything unrecognised is `AUTOMATIC`.
    #[must_use]
    pub const fn from_wire(value: i32) -> Self {
        match value {
            1 => Self::ManualByParticipant,
            2 => Self::ManualByTopic,
            _ => Self::Automatic,
        }
    }

    /// True when the application, not the middleware, must assert liveliness.
    #[must_use]
    pub const fn is_manual(self) -> bool {
        !matches!(self, Self::Automatic)
    }

    /// The DDS specification's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Automatic => "AUTOMATIC",
            Self::ManualByParticipant => "MANUAL_BY_PARTICIPANT",
            Self::ManualByTopic => "MANUAL_BY_TOPIC",
        }
    }
}

impl fmt::Display for LivelinessKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// `LIVELINESS` (`PID_LIVELINESS`, `0x001b`). Twelve octets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LivelinessQos {
    /// Who is responsible for asserting liveliness.
    pub kind: LivelinessKind,
    /// How long an assertion remains good for.
    pub lease_duration: DdsDuration,
}

impl LivelinessQos {
    /// `AUTOMATIC` with an infinite lease — the DDS default.
    #[must_use]
    pub const fn automatic() -> Self {
        Self {
            kind: LivelinessKind::Automatic,
            lease_duration: DdsDuration::INFINITE,
        }
    }

    /// `MANUAL_BY_TOPIC` with the given lease.
    #[must_use]
    pub const fn manual_by_topic(lease: DdsDuration) -> Self {
        Self {
            kind: LivelinessKind::ManualByTopic,
            lease_duration: lease,
        }
    }

    /// `MANUAL_BY_PARTICIPANT` with the given lease.
    #[must_use]
    pub const fn manual_by_participant(lease: DdsDuration) -> Self {
        Self {
            kind: LivelinessKind::ManualByParticipant,
            lease_duration: lease,
        }
    }

    /// Request-versus-offered: the writer must assert at least as strongly
    /// and at least as often as the reader asks.
    #[must_use]
    pub fn is_satisfied_by(self, offered: Self) -> bool {
        if self.kind.to_wire() > offered.kind.to_wire() {
            return false;
        }
        compare_dds_durations(self.lease_duration, offered.lease_duration).is_ge()
    }
}

impl Default for LivelinessQos {
    fn default() -> Self {
        Self::automatic()
    }
}

impl CdrType for LivelinessQos {
    const MIN_SERIALIZED_SIZE: usize = LIVELINESS_LEN;
}

impl CdrSerialize for LivelinessQos {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_i32(self.kind.to_wire())?;
        self.lease_duration.serialize(writer)
    }
}

impl<'de> CdrDeserialize<'de> for LivelinessQos {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        let kind = LivelinessKind::from_wire(reader.read_i32()?);
        let lease_duration = DdsDuration::deserialize(reader)?;
        Ok(Self {
            kind,
            lease_duration,
        })
    }
}

// ────────────────────────────────────────────────────────────────────────────
// OWNERSHIP / DESTINATION_ORDER
// ────────────────────────────────────────────────────────────────────────────

/// `OwnershipQosPolicyKind`, zero-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(i32)]
pub enum OwnershipKind {
    /// `SHARED` — every writer's samples reach the reader.
    #[default]
    Shared = 0,
    /// `EXCLUSIVE` — only the strongest writer of an instance is delivered.
    Exclusive = 1,
}

impl OwnershipKind {
    /// The octets this kind is written as.
    #[must_use]
    pub const fn to_wire(self) -> i32 {
        self as i32
    }

    /// Read a kind from the wire; anything unrecognised is `SHARED`.
    #[must_use]
    pub const fn from_wire(value: i32) -> Self {
        match value {
            1 => Self::Exclusive,
            _ => Self::Shared,
        }
    }

    /// The DDS specification's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Shared => "SHARED",
            Self::Exclusive => "EXCLUSIVE",
        }
    }
}

impl fmt::Display for OwnershipKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// `OWNERSHIP` (`PID_OWNERSHIP`, `0x001f`). Four octets.
///
/// The one policy DDS requires to match *exactly* rather than by ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OwnershipQos {
    /// Shared or exclusive.
    pub kind: OwnershipKind,
}

impl OwnershipQos {
    /// `SHARED` — the DDS default.
    #[must_use]
    pub const fn shared() -> Self {
        Self {
            kind: OwnershipKind::Shared,
        }
    }

    /// `EXCLUSIVE`.
    #[must_use]
    pub const fn exclusive() -> Self {
        Self {
            kind: OwnershipKind::Exclusive,
        }
    }

    /// Request-versus-offered: the kinds must be identical.
    #[must_use]
    pub const fn is_satisfied_by(self, offered: Self) -> bool {
        self.kind.to_wire() == offered.kind.to_wire()
    }
}

impl CdrType for OwnershipQos {
    const MIN_SERIALIZED_SIZE: usize = KIND_ONLY_LEN;
}

impl CdrSerialize for OwnershipQos {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_i32(self.kind.to_wire())
    }
}

impl<'de> CdrDeserialize<'de> for OwnershipQos {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        Ok(Self {
            kind: OwnershipKind::from_wire(reader.read_i32()?),
        })
    }
}

/// `OWNERSHIP_STRENGTH` (`PID_OWNERSHIP_STRENGTH`, `0x0006`). Four octets.
///
/// A writer-only policy; readers do not announce it and RxO ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct OwnershipStrengthQos {
    /// Higher wins under `EXCLUSIVE` ownership.
    pub value: i32,
}

impl OwnershipStrengthQos {
    /// A writer with the given strength.
    #[must_use]
    pub const fn new(value: i32) -> Self {
        Self { value }
    }
}

impl CdrType for OwnershipStrengthQos {
    const MIN_SERIALIZED_SIZE: usize = KIND_ONLY_LEN;
}

impl CdrSerialize for OwnershipStrengthQos {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_i32(self.value)
    }
}

impl<'de> CdrDeserialize<'de> for OwnershipStrengthQos {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        Ok(Self {
            value: reader.read_i32()?,
        })
    }
}

/// `DestinationOrderQosPolicyKind`, zero-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(i32)]
pub enum DestinationOrderKind {
    /// `BY_RECEPTION_TIMESTAMP` — order by when the reader saw the sample.
    #[default]
    ByReceptionTimestamp = 0,
    /// `BY_SOURCE_TIMESTAMP` — order by the `INFO_TS` the writer sent.
    BySourceTimestamp = 1,
}

impl DestinationOrderKind {
    /// The octets this kind is written as.
    #[must_use]
    pub const fn to_wire(self) -> i32 {
        self as i32
    }

    /// Read a kind from the wire; anything unrecognised orders by reception.
    #[must_use]
    pub const fn from_wire(value: i32) -> Self {
        match value {
            1 => Self::BySourceTimestamp,
            _ => Self::ByReceptionTimestamp,
        }
    }

    /// The DDS specification's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ByReceptionTimestamp => "BY_RECEPTION_TIMESTAMP",
            Self::BySourceTimestamp => "BY_SOURCE_TIMESTAMP",
        }
    }
}

impl fmt::Display for DestinationOrderKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// `DESTINATION_ORDER` (`PID_DESTINATION_ORDER`, `0x0025`). Four octets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DestinationOrderQos {
    /// Which timestamp orders samples of one instance.
    pub kind: DestinationOrderKind,
}

impl DestinationOrderQos {
    /// `BY_RECEPTION_TIMESTAMP` — the DDS default.
    #[must_use]
    pub const fn by_reception() -> Self {
        Self {
            kind: DestinationOrderKind::ByReceptionTimestamp,
        }
    }

    /// `BY_SOURCE_TIMESTAMP`.
    #[must_use]
    pub const fn by_source() -> Self {
        Self {
            kind: DestinationOrderKind::BySourceTimestamp,
        }
    }

    /// Request-versus-offered: the reader may ask for no stronger ordering
    /// than the writer offers.
    #[must_use]
    pub const fn is_satisfied_by(self, offered: Self) -> bool {
        self.kind.to_wire() <= offered.kind.to_wire()
    }
}

impl CdrType for DestinationOrderQos {
    const MIN_SERIALIZED_SIZE: usize = KIND_ONLY_LEN;
}

impl CdrSerialize for DestinationOrderQos {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_i32(self.kind.to_wire())
    }
}

impl<'de> CdrDeserialize<'de> for DestinationOrderQos {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        Ok(Self {
            kind: DestinationOrderKind::from_wire(reader.read_i32()?),
        })
    }
}

// ────────────────────────────────────────────────────────────────────────────
// PRESENTATION / RESOURCE_LIMITS
// ────────────────────────────────────────────────────────────────────────────

/// `PresentationQosPolicyAccessScopeKind`, zero-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(i32)]
pub enum PresentationScope {
    /// `INSTANCE` — ordering and coherency apply per instance.
    #[default]
    Instance = 0,
    /// `TOPIC` — across all instances of one topic.
    Topic = 1,
    /// `GROUP` — across every endpoint of one publisher or subscriber.
    Group = 2,
}

impl PresentationScope {
    /// The octets this scope is written as.
    #[must_use]
    pub const fn to_wire(self) -> i32 {
        self as i32
    }

    /// Read a scope from the wire; anything unrecognised is `INSTANCE`.
    #[must_use]
    pub const fn from_wire(value: i32) -> Self {
        match value {
            1 => Self::Topic,
            2 => Self::Group,
            _ => Self::Instance,
        }
    }

    /// The DDS specification's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Instance => "INSTANCE",
            Self::Topic => "TOPIC",
            Self::Group => "GROUP",
        }
    }
}

impl fmt::Display for PresentationScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// `PRESENTATION` (`PID_PRESENTATION`, `0x0021`). Eight octets.
///
/// Written as `long` scope, `boolean` coherent, `boolean` ordered, then two
/// octets of padding — CDR pads the struct to its four-octet alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PresentationQos {
    /// The scope coherency and ordering apply over.
    pub access_scope: PresentationScope,
    /// Whether samples are delivered as coherent sets.
    pub coherent_access: bool,
    /// Whether samples keep their written order across the scope.
    pub ordered_access: bool,
}

impl PresentationQos {
    /// The DDS default: instance scope, neither coherent nor ordered.
    #[must_use]
    pub const fn instance() -> Self {
        Self {
            access_scope: PresentationScope::Instance,
            coherent_access: false,
            ordered_access: false,
        }
    }

    /// Request-versus-offered: the writer must offer at least the scope the
    /// reader asks for, and must not withhold a guarantee the reader wants.
    #[must_use]
    pub const fn is_satisfied_by(self, offered: Self) -> bool {
        if self.access_scope.to_wire() > offered.access_scope.to_wire() {
            return false;
        }
        if self.coherent_access && !offered.coherent_access {
            return false;
        }
        !self.ordered_access || offered.ordered_access
    }
}

impl CdrType for PresentationQos {
    const MIN_SERIALIZED_SIZE: usize = PRESENTATION_LEN;
}

impl CdrSerialize for PresentationQos {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_i32(self.access_scope.to_wire())?;
        writer.write_bool(self.coherent_access)?;
        writer.write_bool(self.ordered_access)?;
        writer.write_zeros(2);
        Ok(())
    }
}

impl<'de> CdrDeserialize<'de> for PresentationQos {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        let access_scope = PresentationScope::from_wire(reader.read_i32()?);
        let coherent_access = reader.read_bool()?;
        let ordered_access = reader.read_bool()?;
        Ok(Self {
            access_scope,
            coherent_access,
            ordered_access,
        })
    }
}

/// `RESOURCE_LIMITS` (`PID_RESOURCE_LIMITS`, `0x0041`). Twelve octets.
///
/// A local policy; announced for introspection, not matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimitsQos {
    /// Total samples the endpoint may hold, or [`UNLIMITED_DEPTH`].
    pub max_samples: i32,
    /// Instances the endpoint may hold, or [`UNLIMITED_DEPTH`].
    pub max_instances: i32,
    /// Samples per instance, or [`UNLIMITED_DEPTH`].
    pub max_samples_per_instance: i32,
}

impl ResourceLimitsQos {
    /// Everything unlimited — the DDS default.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            max_samples: UNLIMITED_DEPTH,
            max_instances: UNLIMITED_DEPTH,
            max_samples_per_instance: UNLIMITED_DEPTH,
        }
    }

    /// The total-sample ceiling, or `None` when unlimited.
    #[must_use]
    pub const fn sample_ceiling(self) -> Option<usize> {
        if self.max_samples < 0 {
            None
        } else {
            Some(self.max_samples as usize)
        }
    }
}

impl Default for ResourceLimitsQos {
    fn default() -> Self {
        Self::unlimited()
    }
}

impl CdrType for ResourceLimitsQos {
    const MIN_SERIALIZED_SIZE: usize = RESOURCE_LIMITS_LEN;
}

impl CdrSerialize for ResourceLimitsQos {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_i32(self.max_samples)?;
        writer.write_i32(self.max_instances)?;
        writer.write_i32(self.max_samples_per_instance)
    }
}

impl<'de> CdrDeserialize<'de> for ResourceLimitsQos {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        Ok(Self {
            max_samples: reader.read_i32()?,
            max_instances: reader.read_i32()?,
            max_samples_per_instance: reader.read_i32()?,
        })
    }
}

/// Order two DDS durations, treating `INFINITE` as larger than everything.
///
/// `DdsDuration`'s `nanosec` field is unsigned, so a naive tuple comparison
/// already orders correctly — but only if `INFINITE`'s
/// `{0x7fffffff, 0xffffffff}` is genuinely the largest representable pair,
/// which it is. The function exists so every RxO check reads the same way and
/// so the reasoning lives in one place.
#[must_use]
pub fn compare_dds_durations(left: DdsDuration, right: DdsDuration) -> core::cmp::Ordering {
    (left.sec, left.nanosec).cmp(&(right.sec, right.nanosec))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use astrs_cdr::{Encoding, ParameterId, ParameterList, pid};

    fn round_trip<T>(value: T, expected_len: usize) -> T
    where
        T: CdrSerialize + for<'de> CdrDeserialize<'de> + PartialEq + fmt::Debug,
    {
        let octets = astrs_cdr::to_vec_headerless(&value, Encoding::DISCOVERY.plain()).unwrap();
        assert_eq!(octets.len(), expected_len, "unexpected serialized length");
        let decoded: T =
            astrs_cdr::from_bytes_headerless(&octets, Encoding::DISCOVERY.plain()).unwrap();
        assert_eq!(decoded, value);
        decoded
    }

    #[test]
    fn reliability_uses_the_one_based_wire_numbering() {
        assert_eq!(ReliabilityKind::BestEffort.to_wire(), 1);
        assert_eq!(ReliabilityKind::Reliable.to_wire(), 2);
        assert_eq!(ReliabilityKind::from_wire(0), ReliabilityKind::BestEffort);
        assert_eq!(ReliabilityKind::from_wire(7), ReliabilityKind::BestEffort);
    }

    #[test]
    fn reliability_round_trips_and_is_twelve_octets() {
        let policy = round_trip(ReliabilityQos::reliable(), RELIABILITY_LEN);
        assert!(policy.is_reliable());
        assert_eq!(policy.max_blocking_time, DdsDuration::from_millis(100));
    }

    #[test]
    fn reliability_rxo_is_one_directional() {
        let reliable = ReliabilityQos::reliable();
        let best_effort = ReliabilityQos::best_effort();
        assert!(best_effort.is_satisfied_by(reliable), "reliable serves all");
        assert!(!reliable.is_satisfied_by(best_effort), "the reverse fails");
        assert!(reliable.is_satisfied_by(reliable));
    }

    #[test]
    fn durability_round_trips_and_orders() {
        let policy = round_trip(DurabilityQos::transient_local(), DURABILITY_LEN);
        assert!(policy.kind.replays_history());
        assert!(DurabilityQos::volatile().is_satisfied_by(DurabilityQos::transient_local()));
        assert!(!DurabilityQos::transient_local().is_satisfied_by(DurabilityQos::volatile()));
    }

    #[test]
    fn history_keep_last_zero_is_raised_to_one() {
        assert_eq!(HistoryQos::keep_last(0).depth, 1);
        assert_eq!(HistoryQos::keep_last(-5).depth, 1);
        assert_eq!(HistoryQos::keep_last(10).retained(), Some(10));
        assert_eq!(HistoryQos::keep_all().retained(), None);
    }

    #[test]
    fn history_round_trips() {
        let policy = round_trip(HistoryQos::keep_last(17), HISTORY_LEN);
        assert_eq!(policy.depth, 17);
        let all = round_trip(HistoryQos::keep_all(), HISTORY_LEN);
        assert_eq!(all.depth, UNLIMITED_DEPTH);
    }

    #[test]
    fn deadline_rxo_runs_the_other_way() {
        let strict = DeadlineQos::from_millis(100);
        let lax = DeadlineQos::from_millis(1_000);
        assert!(
            lax.is_satisfied_by(strict),
            "a reader wanting 1 s is served by a writer promising 100 ms"
        );
        assert!(
            !strict.is_satisfied_by(lax),
            "a reader wanting 100 ms is not served by a writer promising 1 s"
        );
        assert!(DeadlineQos::infinite().is_satisfied_by(strict));
    }

    #[test]
    fn deadline_and_lifespan_round_trip() {
        round_trip(DeadlineQos::from_millis(250), DURATION_POLICY_LEN);
        round_trip(LifespanQos::from_millis(750), DURATION_POLICY_LEN);
        round_trip(LatencyBudgetQos::immediate(), DURATION_POLICY_LEN);
        assert!(LifespanQos::infinite().is_infinite());
        assert!(DeadlineQos::infinite().is_infinite());
    }

    #[test]
    fn liveliness_checks_both_kind_and_lease() {
        let requested = LivelinessQos::manual_by_topic(DdsDuration::from_millis(1_000));
        let weaker_kind = LivelinessQos::automatic();
        let same_kind_short_lease = LivelinessQos::manual_by_topic(DdsDuration::from_millis(500));
        assert!(!requested.is_satisfied_by(weaker_kind), "kind is too weak");
        assert!(
            requested.is_satisfied_by(same_kind_short_lease),
            "asserting more often than asked is fine"
        );
        assert!(
            !same_kind_short_lease.is_satisfied_by(requested),
            "asserting less often than asked is not"
        );
        assert!(LivelinessKind::ManualByTopic.is_manual());
        assert!(!LivelinessKind::Automatic.is_manual());
    }

    #[test]
    fn liveliness_round_trips() {
        let policy = round_trip(
            LivelinessQos::manual_by_participant(DdsDuration::from_millis(2_500)),
            LIVELINESS_LEN,
        );
        assert_eq!(policy.kind, LivelinessKind::ManualByParticipant);
    }

    #[test]
    fn ownership_must_match_exactly() {
        assert!(OwnershipQos::shared().is_satisfied_by(OwnershipQos::shared()));
        assert!(!OwnershipQos::shared().is_satisfied_by(OwnershipQos::exclusive()));
        assert!(!OwnershipQos::exclusive().is_satisfied_by(OwnershipQos::shared()));
        round_trip(OwnershipQos::exclusive(), KIND_ONLY_LEN);
        round_trip(OwnershipStrengthQos::new(42), KIND_ONLY_LEN);
    }

    #[test]
    fn destination_order_round_trips_and_orders() {
        round_trip(DestinationOrderQos::by_source(), KIND_ONLY_LEN);
        assert!(
            DestinationOrderQos::by_reception().is_satisfied_by(DestinationOrderQos::by_source())
        );
        assert!(
            !DestinationOrderQos::by_source().is_satisfied_by(DestinationOrderQos::by_reception())
        );
    }

    #[test]
    fn presentation_pads_to_eight_octets() {
        let octets = astrs_cdr::to_vec_headerless(
            &PresentationQos {
                access_scope: PresentationScope::Group,
                coherent_access: true,
                ordered_access: true,
            },
            Encoding::DISCOVERY.plain(),
        )
        .unwrap();
        assert_eq!(octets.len(), PRESENTATION_LEN);
        assert_eq!(&octets[4..6], &[1, 1]);
        assert_eq!(&octets[6..], &[0, 0], "the tail must be padding");
    }

    #[test]
    fn presentation_rxo_never_grants_more_than_offered() {
        let strict = PresentationQos {
            access_scope: PresentationScope::Topic,
            coherent_access: true,
            ordered_access: false,
        };
        let lax = PresentationQos::instance();
        assert!(!strict.is_satisfied_by(lax));
        assert!(lax.is_satisfied_by(strict));
    }

    #[test]
    fn resource_limits_round_trip() {
        let policy = round_trip(ResourceLimitsQos::unlimited(), RESOURCE_LIMITS_LEN);
        assert_eq!(policy.sample_ceiling(), None);
        let bounded = ResourceLimitsQos {
            max_samples: 100,
            ..ResourceLimitsQos::unlimited()
        };
        assert_eq!(bounded.sample_ceiling(), Some(100));
    }

    #[test]
    fn infinite_compares_greater_than_everything() {
        assert!(
            compare_dds_durations(DdsDuration::INFINITE, DdsDuration::from_millis(u32::MAX))
                .is_gt()
        );
        assert!(compare_dds_durations(DdsDuration::ZERO, DdsDuration::from_millis(1)).is_lt());
    }

    #[test]
    fn policies_survive_the_parameter_list_they_ride_in() {
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        list.push_value(
            ParameterId::new(pid::RELIABILITY),
            &ReliabilityQos::reliable(),
        )
        .unwrap();
        list.push_value(ParameterId::new(pid::HISTORY), &HistoryQos::keep_last(5))
            .unwrap();
        list.push_value(
            ParameterId::new(pid::DEADLINE),
            &DeadlineQos::from_millis(30),
        )
        .unwrap();

        let encoded = list.encode().unwrap();
        let (decoded, encoding) = ParameterList::decode(&encoded).unwrap();

        let reliability: ReliabilityQos = decoded
            .get_by_base(pid::RELIABILITY)
            .unwrap()
            .decode_value(encoding)
            .unwrap();
        let history: HistoryQos = decoded
            .get_by_base(pid::HISTORY)
            .unwrap()
            .decode_value(encoding)
            .unwrap();
        let deadline: DeadlineQos = decoded
            .get_by_base(pid::DEADLINE)
            .unwrap()
            .decode_value(encoding)
            .unwrap();

        assert!(reliability.is_reliable());
        assert_eq!(history.depth, 5);
        assert_eq!(deadline.period, DdsDuration::from_millis(30));
    }

    #[test]
    fn kind_names_are_the_specification_spellings() {
        assert_eq!(ReliabilityKind::Reliable.to_string(), "RELIABLE");
        assert_eq!(
            DurabilityKind::TransientLocal.to_string(),
            "TRANSIENT_LOCAL"
        );
        assert_eq!(HistoryKind::KeepAll.to_string(), "KEEP_ALL");
        assert_eq!(
            LivelinessKind::ManualByParticipant.to_string(),
            "MANUAL_BY_PARTICIPANT"
        );
        assert_eq!(OwnershipKind::Exclusive.to_string(), "EXCLUSIVE");
        assert_eq!(
            DestinationOrderKind::BySourceTimestamp.to_string(),
            "BY_SOURCE_TIMESTAMP"
        );
        assert_eq!(PresentationScope::Group.to_string(), "GROUP");
    }
}
