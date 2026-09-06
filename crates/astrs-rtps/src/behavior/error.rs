//! The behavior half's error taxonomy.
//!
//! The message model's [`RtpsError`] is the *wire format* taxonomy: it names
//! the octet that broke. This module is its counterpart for everything that
//! can go wrong once a participant is running — a socket that will not bind, a
//! multicast group the kernel refuses to join, a reader whose QoS request no
//! writer can satisfy, a fragment series that contradicts itself.
//!
//! The two never merge. [`RtpsError`] stays exactly as the message-model half
//! left it and [`BehaviorError`] converts from it through `#[from]`, so a
//! parse failure surfacing out of the receive loop keeps its precise wire
//! diagnosis while gaining the context of *which* participant dropped it.
//!
//! # Shape
//!
//! ```text
//! BehaviorError
//! ├── transport      Bind, Send, Receive, MulticastJoin, DatagramTooLarge, …
//! ├── configuration  DomainIdOutOfRange, DuplicateEntity, NameTooLong, …
//! ├── protocol state UnknownWriter, Shutdown, HistoryFull, LeaseExpired, …
//! ├── discovery      MissingParameter, MalformedParameter, IncompatibleQos, …
//! └── wrapped        Wire(RtpsError), Cdr(CdrError)
//! ```
//!
//! Every variant is `Clone + PartialEq + Eq`, which is what lets a test assert
//! the exact error rather than a substring of its `Display`. `std::io::Error`
//! is neither, so the three transport variants that wrap one carry an
//! [`IoFailure`] — the [`ErrorKind`](std::io::ErrorKind) plus the message —
//! instead of the original.

use core::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};

use astrs_cdr::CdrError;
use thiserror::Error;

use crate::error::RtpsError;
use crate::structure::{EntityId, Guid, Locator, SequenceNumber};

/// The behavior half's result alias.
pub type BehaviorResult<T> = Result<T, BehaviorError>;

/// A `std::io::Error` reduced to the parts that can be compared and cloned.
///
/// `io::Error` is neither `Clone` nor `PartialEq`, and it can carry an
/// arbitrary boxed payload. A protocol error type that tests assert on cannot
/// hold one, so the two facts worth keeping — the classification and the
/// human-readable text — are copied out and the original is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoFailure {
    /// The operating system's classification of the failure.
    pub kind: io::ErrorKind,
    /// The message the original error rendered to.
    pub message: String,
}

impl IoFailure {
    /// Capture the comparable parts of `error`.
    #[must_use]
    pub fn new(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    /// True when the operating system refused the operation on policy
    /// grounds — the sandbox case a multicast join hits on macOS.
    #[must_use]
    pub fn is_refusal(&self) -> bool {
        matches!(
            self.kind,
            io::ErrorKind::PermissionDenied
                | io::ErrorKind::AddrNotAvailable
                | io::ErrorKind::Unsupported
        )
    }
}

impl fmt::Display for IoFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({:?})", self.message, self.kind)
    }
}

impl std::error::Error for IoFailure {}

impl From<&io::Error> for IoFailure {
    fn from(error: &io::Error) -> Self {
        Self::new(error)
    }
}

impl From<io::Error> for IoFailure {
    fn from(error: io::Error) -> Self {
        Self::new(&error)
    }
}

/// Names the QoS policy an RxO check rejected.
///
/// The DDS specification numbers its policies; a mismatch is reported against
/// the policy rather than as free text so that `astrs-ros2` can map it onto
/// the `OFFERED_INCOMPATIBLE_QOS` / `REQUESTED_INCOMPATIBLE_QOS` status a ROS
/// application expects, without re-parsing a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum QosPolicyId {
    /// `RELIABILITY` — a `BEST_EFFORT` writer cannot serve a `RELIABLE` reader.
    Reliability,
    /// `DURABILITY` — a `VOLATILE` writer cannot serve a `TRANSIENT_LOCAL` reader.
    Durability,
    /// `DEADLINE` — the writer's period is longer than the reader accepts.
    Deadline,
    /// `LIVELINESS` — kind or lease duration is weaker than requested.
    Liveliness,
    /// `LATENCY_BUDGET` — the writer intends to delay longer than the reader
    /// tolerates.
    LatencyBudget,
    /// `OWNERSHIP` — the two kinds differ, which DDS requires to match exactly.
    Ownership,
    /// `DESTINATION_ORDER` — the writer orders more weakly than requested.
    DestinationOrder,
    /// `PRESENTATION` — access scope or coherency is weaker than requested.
    Presentation,
    /// `PARTITION` — no partition name in common.
    Partition,
}

impl QosPolicyId {
    /// The specification's name for the policy, as it appears in DDS status
    /// reports.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Reliability => "RELIABILITY",
            Self::Durability => "DURABILITY",
            Self::Deadline => "DEADLINE",
            Self::Liveliness => "LIVELINESS",
            Self::LatencyBudget => "LATENCY_BUDGET",
            Self::Ownership => "OWNERSHIP",
            Self::DestinationOrder => "DESTINATION_ORDER",
            Self::Presentation => "PRESENTATION",
            Self::Partition => "PARTITION",
        }
    }
}

impl fmt::Display for QosPolicyId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Why a fragment series could not be reassembled.
///
/// Reassembly is the one place where a *sequence* of well-formed submessages
/// can still be nonsense, so the reason is a type rather than a string: the
/// reassembler decides, the caller can match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReassemblyDefect {
    /// A later `DATA_FRAG` declared a different `sampleSize` than the first.
    SampleSizeChanged {
        /// The size the first fragment of the sample declared.
        first: u32,
        /// The size this fragment declared.
        then: u32,
    },
    /// A later `DATA_FRAG` declared a different `fragmentSize` than the first.
    FragmentSizeChanged {
        /// The fragment size the first fragment of the sample declared.
        first: u16,
        /// The fragment size this fragment declared.
        then: u16,
    },
    /// The fragment's window runs past the declared sample size.
    WindowPastSample {
        /// One past the last octet the window covers.
        end: u64,
        /// The declared total size of the sample.
        sample_size: u32,
    },
    /// A fragment arrived carrying different octets than one already stored.
    Contradiction {
        /// The first fragment number whose octets disagree.
        fragment: u32,
    },
    /// More fragments are outstanding than the reassembler will hold.
    TooManyFragments {
        /// The number the sample would need.
        needed: u64,
        /// The configured ceiling.
        limit: u64,
    },
}

impl fmt::Display for ReassemblyDefect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SampleSizeChanged { first, then } => write!(
                formatter,
                "sampleSize changed from {first} to {then} mid-series"
            ),
            Self::FragmentSizeChanged { first, then } => write!(
                formatter,
                "fragmentSize changed from {first} to {then} mid-series"
            ),
            Self::WindowPastSample { end, sample_size } => write!(
                formatter,
                "fragment window ends at {end}, past the {sample_size}-octet sample"
            ),
            Self::Contradiction { fragment } => {
                write!(formatter, "fragment {fragment} was resent with new octets")
            }
            Self::TooManyFragments { needed, limit } => write!(
                formatter,
                "sample needs {needed} fragments, more than the {limit} allowed"
            ),
        }
    }
}

/// Everything that can go wrong running an RTPS participant.
///
/// See the [module documentation](self) for how this relates to
/// [`RtpsError`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum BehaviorError {
    // ── Transport ────────────────────────────────────────────────────────
    /// A socket could not be bound to the address the configuration asked for.
    #[error("cannot bind {address}: {source}")]
    Bind {
        /// The address that was requested.
        address: SocketAddr,
        /// What the operating system said.
        source: IoFailure,
    },

    /// A datagram could not be handed to the operating system.
    #[error("cannot send {len} octet(s) to {target}: {source}")]
    Send {
        /// Where the datagram was headed.
        target: SocketAddr,
        /// How many octets it was.
        len: usize,
        /// What the operating system said.
        source: IoFailure,
    },

    /// The receive side of a socket failed.
    #[error("cannot receive on {local}: {source}")]
    Receive {
        /// The socket that failed.
        local: SocketAddr,
        /// What the operating system said.
        source: IoFailure,
    },

    /// The kernel refused to add the socket to a multicast group.
    ///
    /// Expected on a sandboxed macOS host, which is exactly why every
    /// deterministic protocol assertion in this crate's tests runs over
    /// unicast loopback instead. The error is reported rather than swallowed
    /// so a probe can assert its own outcome.
    #[error("cannot join multicast group {group} on interface {interface}: {source}")]
    MulticastJoin {
        /// The group that was requested.
        group: Ipv4Addr,
        /// The interface address the join was scoped to.
        interface: Ipv4Addr,
        /// What the operating system said.
        source: IoFailure,
    },

    /// A message serialized to more octets than a UDP datagram can hold.
    #[error("datagram of {len} octet(s) exceeds the {limit}-octet transport limit")]
    DatagramTooLarge {
        /// The size the message serialized to.
        len: usize,
        /// The transport's ceiling.
        limit: usize,
    },

    /// A discovered locator names a transport this participant cannot use.
    #[error("locator {locator} is not reachable over the UDPv4 transport")]
    UnreachableLocator {
        /// The locator as it was announced.
        locator: Locator,
    },

    /// A remote endpoint announced no locator that can be addressed.
    #[error("{role} {guid} announced no addressable locator")]
    NoAddressableLocator {
        /// `"writer"` or `"reader"`.
        role: &'static str,
        /// The endpoint that cannot be reached.
        guid: Guid,
    },

    // ── Configuration ────────────────────────────────────────────────────
    /// The domain id is above the range the port mapping can express.
    #[error("domain id {domain_id} is above the maximum of {maximum}")]
    DomainIdOutOfRange {
        /// The domain id that was asked for.
        domain_id: u32,
        /// The largest domain id the §9.6.1.1 mapping supports.
        maximum: u32,
    },

    /// The participant id pushes the port mapping past a `u16`.
    #[error("participant id {participant_id} does not fit domain {domain_id}'s port range")]
    ParticipantIdOutOfRange {
        /// The participant id that was asked for.
        participant_id: u32,
        /// The domain it was combined with.
        domain_id: u32,
    },

    /// A participant was configured with neither multicast nor initial peers.
    #[error("participant has no discovery path: multicast is disabled and no initial peer is set")]
    NoDiscoveryPath,

    /// Two endpoints were created with the same entity id.
    #[error("entity {guid} already exists on this participant")]
    DuplicateEntity {
        /// The GUID that collided.
        guid: Guid,
    },

    /// The participant has handed out every entity key of a given kind.
    #[error("no entity key left for a user-defined endpoint of kind {kind}")]
    EntityKeysExhausted {
        /// The entity kind whose counter overflowed.
        kind: u8,
    },

    /// A topic or type name was empty, which DDS does not allow.
    #[error("{field} must not be empty")]
    EmptyName {
        /// Which name it was: `"topic name"` or `"type name"`.
        field: &'static str,
    },

    /// A topic or type name is longer than the discovery encoding allows.
    #[error("{field} is {len} octet(s), above the {limit}-octet limit")]
    NameTooLong {
        /// Which name it was.
        field: &'static str,
        /// The length that was offered.
        len: usize,
        /// The ceiling.
        limit: usize,
    },

    // ── Protocol state ───────────────────────────────────────────────────
    /// A handle referred to a writer this participant does not own.
    #[error("no writer {guid} on this participant")]
    UnknownWriter {
        /// The GUID that was looked up.
        guid: Guid,
    },

    /// A handle referred to a reader this participant does not own.
    #[error("no reader {guid} on this participant")]
    UnknownReader {
        /// The GUID that was looked up.
        guid: Guid,
    },

    /// The participant has been shut down and will not act again.
    #[error("participant has been shut down")]
    Shutdown,

    /// A `KEEP_ALL` history is full of unacknowledged samples.
    ///
    /// `KEEP_LAST` never reports this — it evicts instead — so seeing it means
    /// a reliable reader has stopped acknowledging.
    #[error("KEEP_ALL history is full at {depth} unacknowledged sample(s)")]
    HistoryFull {
        /// The number of samples being held.
        depth: usize,
    },

    /// A sample is larger than the writer will fragment.
    #[error("sample of {len} octet(s) exceeds the {limit}-octet ceiling")]
    SampleTooLarge {
        /// The size of the sample.
        len: usize,
        /// The configured ceiling.
        limit: usize,
    },

    /// A `DATA_FRAG` series could not be reassembled.
    #[error("cannot reassemble sample {sequence_number} from {writer}: {defect}")]
    Reassembly {
        /// The writer the fragments came from.
        writer: Guid,
        /// The sample the fragments belong to.
        sequence_number: SequenceNumber,
        /// What went wrong.
        defect: ReassemblyDefect,
    },

    /// A remote participant's lease ran out before it announced again.
    #[error("lease of participant {participant} expired")]
    LeaseExpired {
        /// The participant that went quiet.
        participant: Guid,
    },

    // ── Discovery ────────────────────────────────────────────────────────
    /// A discovery sample is missing a parameter it cannot do without.
    #[error("{context} is missing the required parameter 0x{pid:04x}")]
    MissingParameter {
        /// Which sample: `"SPDP participant data"`, `"SEDP publication"`, …
        context: &'static str,
        /// The parameter id that was absent.
        pid: u16,
    },

    /// A discovery parameter is present but cannot be interpreted.
    #[error("{context} parameter 0x{pid:04x} is malformed: {reason}")]
    MalformedParameter {
        /// Which sample the parameter came from.
        context: &'static str,
        /// The parameter id.
        pid: u16,
        /// What is wrong with it.
        reason: &'static str,
    },

    /// A discovery sample used an encapsulation this crate does not read.
    #[error(
        "discovery sample uses encapsulation 0x{identifier:04x}, which is not a parameter list"
    )]
    NotAParameterList {
        /// The encapsulation identifier that was found.
        identifier: u16,
    },

    /// A reader and a writer cannot be matched because a QoS policy conflicts.
    #[error("{policy} is incompatible: reader {reader} requests more than writer {writer} offers")]
    IncompatibleQos {
        /// The policy that failed the request-versus-offered check.
        policy: QosPolicyId,
        /// The requesting reader.
        reader: Guid,
        /// The offering writer.
        writer: Guid,
    },

    /// An SEDP sample announced an entity id whose kind contradicts the topic
    /// it arrived on — a "publication" that is really a reader, say.
    #[error("{context} announced entity id {entity_id}, whose kind is wrong for that topic")]
    WrongEntityKind {
        /// Which builtin topic the sample arrived on.
        context: &'static str,
        /// The entity id that does not belong there.
        entity_id: EntityId,
    },

    // ── Wrapped ──────────────────────────────────────────────────────────
    /// The message model rejected a datagram.
    #[error("RTPS wire error: {0}")]
    Wire(#[from] RtpsError),

    /// A security transform refused, or an endpoint's security settings do
    /// not make sense.
    ///
    /// Only endpoint *creation* surfaces this to a caller. A submessage that
    /// fails to verify at run time is dropped and logged, because §8.3.4.1's
    /// rule — discard the submessage, keep parsing — is exactly the right
    /// behaviour for a forged one too.
    #[error("DDS-Security error: {0}")]
    Security(#[from] crate::security::SecurityError),

    /// A CDR payload could not be encoded or decoded.
    #[error("CDR error: {0}")]
    Cdr(#[from] CdrError),
}

impl BehaviorError {
    /// True when the failure came from the network stack rather than from a
    /// peer's octets or this participant's configuration.
    ///
    /// The receive loop uses this to decide whether to keep running: a
    /// malformed datagram is one peer's problem, a dead socket is everyone's.
    #[must_use]
    pub const fn is_transport(&self) -> bool {
        matches!(
            self,
            Self::Bind { .. }
                | Self::Send { .. }
                | Self::Receive { .. }
                | Self::MulticastJoin { .. }
                | Self::DatagramTooLarge { .. }
                | Self::UnreachableLocator { .. }
                | Self::NoAddressableLocator { .. }
        )
    }

    /// True when a *peer* is at fault: the datagram, the discovery sample or
    /// the QoS it announced is the problem.
    ///
    /// Errors of this class are logged and the datagram dropped; they never
    /// stop a participant.
    #[must_use]
    pub const fn is_peer_fault(&self) -> bool {
        matches!(
            self,
            Self::Wire(_)
                | Self::Cdr(_)
                | Self::MissingParameter { .. }
                | Self::MalformedParameter { .. }
                | Self::NotAParameterList { .. }
                | Self::WrongEntityKind { .. }
                | Self::Reassembly { .. }
                | Self::UnreachableLocator { .. }
        )
    }

    /// True when the participant's own configuration is wrong, so retrying
    /// will not help.
    #[must_use]
    pub const fn is_configuration(&self) -> bool {
        matches!(
            self,
            Self::DomainIdOutOfRange { .. }
                | Self::ParticipantIdOutOfRange { .. }
                | Self::NoDiscoveryPath
                | Self::DuplicateEntity { .. }
                | Self::EntityKeysExhausted { .. }
                | Self::EmptyName { .. }
                | Self::NameTooLong { .. }
        )
    }

    /// True when the operating system refused an optional capability rather
    /// than failing outright.
    ///
    /// A sandboxed macOS host denies `IP_ADD_MEMBERSHIP`; this is how the
    /// multicast probe tells "the kernel said no" apart from "the group
    /// address was nonsense".
    #[must_use]
    pub fn is_capability_refusal(&self) -> bool {
        match self {
            Self::MulticastJoin { source, .. } | Self::Bind { source, .. } => source.is_refusal(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::structure::{ENTITYID_PARTICIPANT, GuidPrefix};

    fn guid() -> Guid {
        Guid::new(GuidPrefix::new([7; 12]), ENTITYID_PARTICIPANT)
    }

    #[test]
    fn io_failure_keeps_kind_and_message() {
        let error = io::Error::new(io::ErrorKind::PermissionDenied, "operation not permitted");
        let failure = IoFailure::new(&error);
        assert_eq!(failure.kind, io::ErrorKind::PermissionDenied);
        assert!(failure.message.contains("not permitted"));
        assert!(failure.is_refusal());
    }

    #[test]
    fn io_failure_is_comparable() {
        let left = IoFailure::new(&io::Error::new(io::ErrorKind::WouldBlock, "again"));
        let right = IoFailure::new(&io::Error::new(io::ErrorKind::WouldBlock, "again"));
        assert_eq!(left, right);
    }

    #[test]
    fn connection_refused_is_not_a_capability_refusal() {
        let failure = IoFailure::new(&io::Error::new(io::ErrorKind::ConnectionRefused, "nope"));
        assert!(!failure.is_refusal());
    }

    #[test]
    fn multicast_denial_is_a_capability_refusal() {
        let error = BehaviorError::MulticastJoin {
            group: Ipv4Addr::new(239, 255, 0, 1),
            interface: Ipv4Addr::UNSPECIFIED,
            source: IoFailure::new(&io::Error::from(io::ErrorKind::PermissionDenied)),
        };
        assert!(error.is_capability_refusal());
        assert!(error.is_transport());
        assert!(!error.is_peer_fault());
    }

    #[test]
    fn wire_errors_are_peer_faults() {
        let error = BehaviorError::from(RtpsError::truncated("header", 20, 4));
        assert!(error.is_peer_fault());
        assert!(!error.is_transport());
        assert!(!error.is_configuration());
    }

    #[test]
    fn configuration_errors_are_classified() {
        let error = BehaviorError::DomainIdOutOfRange {
            domain_id: 500,
            maximum: 232,
        };
        assert!(error.is_configuration());
        assert!(!error.is_peer_fault());
    }

    #[test]
    fn qos_policy_names_match_the_specification() {
        assert_eq!(QosPolicyId::Reliability.name(), "RELIABILITY");
        assert_eq!(
            QosPolicyId::DestinationOrder.to_string(),
            "DESTINATION_ORDER"
        );
    }

    #[test]
    fn reassembly_defects_render_the_numbers() {
        let defect = ReassemblyDefect::SampleSizeChanged {
            first: 1024,
            then: 2048,
        };
        let rendered = defect.to_string();
        assert!(rendered.contains("1024"), "{rendered}");
        assert!(rendered.contains("2048"), "{rendered}");
    }

    #[test]
    fn reassembly_error_names_the_writer_and_sample() {
        let error = BehaviorError::Reassembly {
            writer: guid(),
            sequence_number: SequenceNumber::new(9),
            defect: ReassemblyDefect::Contradiction { fragment: 3 },
        };
        let rendered = error.to_string();
        assert!(rendered.contains('9'), "{rendered}");
        assert!(error.is_peer_fault());
    }

    #[test]
    fn incompatible_qos_names_both_sides() {
        let error = BehaviorError::IncompatibleQos {
            policy: QosPolicyId::Durability,
            reader: guid(),
            writer: guid(),
        };
        assert!(error.to_string().contains("DURABILITY"));
        assert!(!error.is_transport());
    }

    #[test]
    fn cdr_errors_convert() {
        let error = BehaviorError::from(CdrError::ParameterTooLong {
            id: 0x0050,
            length: 70_000,
        });
        assert!(error.is_peer_fault());
    }
}
