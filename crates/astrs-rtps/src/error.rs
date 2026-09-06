//! The RTPS wire-format error taxonomy.
//!
//! Every fallible operation in [`crate::structure`] and [`crate::messages`]
//! returns [`RtpsError`]. The variants are deliberately fine-grained for the
//! same reason `astrs-cdr`'s are: a participant drops a malformed datagram
//! silently, but the log line that records *why* has to be actionable, and a
//! single `InvalidData` bucket makes it useless.
//!
//! The taxonomy splits into five groups.
//!
//! 1. **Framing** — [`RtpsError::BadProtocolId`],
//!    [`RtpsError::UnsupportedProtocolVersion`],
//!    [`RtpsError::MisalignedSubmessage`], [`RtpsError::SubmessageOverrun`],
//!    [`RtpsError::TrailingOctets`]: the datagram is not a well-formed RTPS
//!    message, so nothing inside it can be trusted.
//! 2. **Bounds** — [`RtpsError::Truncated`], [`RtpsError::TooLong`],
//!    [`RtpsError::TooManySubmessages`], [`RtpsError::TooManyLocators`]: the
//!    buffer and the declared lengths disagree, or a length cannot be
//!    expressed on the wire at all. These are the variants a fuzzer drives.
//! 3. **Flags** — [`RtpsError::ConflictingDataFlags`],
//!    [`RtpsError::MissingSubmessageBody`]: the flag octet asks for something
//!    the wire format cannot express.
//! 4. **Validity** — [`RtpsError::InvalidSequenceNumber`],
//!    [`RtpsError::InvalidSequenceNumberSet`],
//!    [`RtpsError::InvalidFragmentNumber`],
//!    [`RtpsError::InvalidFragmentNumberSet`],
//!    [`RtpsError::InvalidHeartbeatRange`],
//!    [`RtpsError::InvalidFragmentGeometry`],
//!    [`RtpsError::InvalidOctetsToInlineQos`]: the octets decode, but the
//!    values violate a validity clause of OMG DDSI-RTPS 2.3 §8.3.7.
//! 5. **Content** — [`RtpsError::UnknownLocatorKind`],
//!    [`RtpsError::UnsupportedLocator`], [`RtpsError::Cdr`]: a field's
//!    payload is not something this stack can act on.
//!
//! [`RtpsError`] is `Clone + PartialEq + Eq`, so a test asserts an exact
//! expected error instead of a `matches!` shape — the same contract
//! `astrs-cdr` established, and the golden-packet tests in `tests/` depend on
//! it.
//!
//! # Relationship to the behavior half
//!
//! This type covers the **wire format only**: reading and writing octets.
//! The behavior half (`src/behavior/`, `src/discovery/`) defines its own
//! error types for protocol *state* — an unmatched reader, an expired lease,
//! a socket that would not bind — and converts through
//! `#[from] RtpsError`. It never extends this enum, which is what keeps this
//! file out of the second half's diff (see the module boundary in the
//! [crate root](crate)).

use astrs_cdr::CdrError;
use thiserror::Error;

/// The result type every fallible RTPS wire-format operation returns.
pub type RtpsResult<T> = Result<T, RtpsError>;

/// Everything that can go wrong encoding or decoding RTPS octets.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum RtpsError {
    /// The datagram does not begin with the four-octet `"RTPS"` protocol id.
    ///
    /// OMG DDSI-RTPS 2.3 §8.3.3.1.1. This is the cheapest possible reject and
    /// the one that fires when a stray non-RTPS datagram lands on a
    /// metatraffic port.
    #[error("not an RTPS message: protocol id {found:02x?} is not \"RTPS\"")]
    BadProtocolId {
        /// The first four octets, as received.
        found: [u8; 4],
    },

    /// The header declares a protocol version this stack cannot parse.
    ///
    /// A 2.x minor above our own is *accepted*: §8.6 requires a receiver to
    /// process the submessages it understands and ignore the rest. Only a
    /// different major version is fatal.
    #[error("unsupported RTPS protocol version {major}.{minor}: only 2.x is understood")]
    UnsupportedProtocolVersion {
        /// Major version from the header.
        major: u8,
        /// Minor version from the header.
        minor: u8,
    },

    /// The buffer ended before a fixed-size field could be read.
    ///
    /// `context` names the field so a dropped-datagram log line points at the
    /// exact octet that was missing.
    #[error("truncated RTPS message reading {context}: need {needed} octet(s), {available} left")]
    Truncated {
        /// Octets the read required.
        needed: usize,
        /// Octets actually available.
        available: usize,
        /// Static description of the field being read.
        context: &'static str,
    },

    /// A submessage's `octetsToNextHeader` points past the end of the
    /// message.
    #[error(
        "submessage 0x{id:02x} declares {declared} octet(s) to the next header \
         but only {available} remain"
    )]
    SubmessageOverrun {
        /// The `submessageId` octet.
        id: u8,
        /// The declared `octetsToNextHeader`.
        declared: usize,
        /// Octets left in the message after this submessage's header.
        available: usize,
    },

    /// A submessage body whose length is not a multiple of four was asked to
    /// carry another submessage after it.
    ///
    /// §8.3.3 puts every submessage on a four-octet boundary, and the only
    /// part of a body that can break the alignment is a `DATA` or
    /// `DATA_FRAG` payload — whose end no receiver could then find, because
    /// `octetsToNextHeader` is its only delimiter. Padding it would be
    /// indistinguishable from lengthening it, so the encoder refuses instead.
    /// Pad the payload itself (`astrs-cdr` records the pad count in the
    /// encapsulation options) or make the submessage the last one.
    #[error(
        "submessage 0x{id:02x} has a body of {body_len} octet(s), which is not \
         four-octet aligned, so no submessage may follow it"
    )]
    UnalignedBody {
        /// The `submessageId` octet.
        id: u8,
        /// The body length that broke the alignment.
        body_len: usize,
    },

    /// A submessage with an empty body was asked to carry another submessage
    /// after it, on a kind where an empty body cannot be expressed.
    ///
    /// §8.3.3.2.3 reads `octetsToNextHeader == 0` as "this submessage extends
    /// to the end of the message" for every kind except `PAD` and `INFO_TS`.
    /// An empty body on any other kind is therefore only writable as the last
    /// submessage — anything after it would be swallowed on decode. Only an
    /// [`Opaque`](crate::messages::Opaque) can reach this: every kind this
    /// crate decodes has mandatory fields.
    #[error(
        "submessage 0x{id:02x} has an empty body, which §8.3.3.2.3 reads as \
         \"to the end of the message\", so no submessage may follow it"
    )]
    EmptyBodyNotLast {
        /// The `submessageId` octet.
        id: u8,
    },

    /// A submessage would start at an offset that is not a multiple of four.
    ///
    /// §8.3.3 requires every submessage to begin on a four-octet boundary; a
    /// sender that mis-declares `octetsToNextHeader` breaks the alignment of
    /// everything that follows, so parsing stops here rather than
    /// mis-decoding the rest of the datagram.
    #[error("submessage at offset {offset} is not four-octet aligned")]
    MisalignedSubmessage {
        /// Offset from the first octet of the message.
        offset: usize,
    },

    /// A submessage body decoded successfully but octets were left over.
    ///
    /// The strict-decode rule: `octetsToNextHeader` is the authority on where
    /// the body ends, and a body longer than the fields it declares is a
    /// version or vendor mismatch, not something to shrug at. The one
    /// exception is a submessage whose grammar ends in an open-ended
    /// element (`serializedPayload`, `PAD`), which consumes the remainder by
    /// construction.
    #[error("submessage 0x{id:02x} has {remaining} unconsumed octet(s) after its last field")]
    TrailingOctets {
        /// The `submessageId` octet.
        id: u8,
        /// Octets left unconsumed inside the submessage body.
        remaining: usize,
    },

    /// A length does not fit the sixteen-bit `octetsToNextHeader` field and
    /// the submessage is not in a position where zero may stand in for
    /// "extends to the end of the message".
    #[error(
        "submessage 0x{id:02x} body of {length} octet(s) exceeds the {maximum} \
         octet octetsToNextHeader field and is not the last submessage"
    )]
    TooLong {
        /// The `submessageId` octet.
        id: u8,
        /// The body length that could not be declared.
        length: usize,
        /// The largest declarable body length.
        maximum: usize,
    },

    /// A message carries more submessages than [`crate::messages::MAX_SUBMESSAGES`].
    #[error("RTPS message carries more than {maximum} submessages")]
    TooManySubmessages {
        /// The ceiling that was hit.
        maximum: usize,
    },

    /// A locator list declares more entries than
    /// [`crate::structure::MAX_LOCATORS`], or than the octets can hold.
    #[error("locator list declares {declared} entries, more than the {maximum} allowed")]
    TooManyLocators {
        /// The declared count.
        declared: u64,
        /// The ceiling that was hit.
        maximum: usize,
    },

    /// A `DATA` submessage set both the `D` (data) and `K` (key) flags.
    ///
    /// §8.3.7.2.5 states the two are exclusive: the `serializedPayload` is
    /// either the sample or its key, never both.
    #[error("DATA submessage sets both the DataFlag and the KeyFlag, which are exclusive")]
    ConflictingDataFlags,

    /// The flag octet promises a field the body does not contain.
    #[error("submessage 0x{id:02x} sets the {flag} flag but the body has no room for {field}")]
    MissingSubmessageBody {
        /// The `submessageId` octet.
        id: u8,
        /// The flag letter as the specification names it (`Q`, `M`, `G`, …).
        flag: &'static str,
        /// The field the flag promised.
        field: &'static str,
    },

    /// A sequence number violates a validity clause of §8.3.7.
    ///
    /// Every submessage that carries a `writerSN`, a `firstSN` or a
    /// `gapStart` requires it to be strictly positive and not
    /// `SEQUENCENUMBER_UNKNOWN`.
    #[error("{context} sequence number {value} is not a valid positive sequence number")]
    InvalidSequenceNumber {
        /// The offending value.
        value: i64,
        /// Static description of the field.
        context: &'static str,
    },

    /// A `SequenceNumberSet` violates §9.4.2.6.
    #[error("invalid SequenceNumberSet in {context}: {reason}")]
    InvalidSequenceNumberSet {
        /// Why the set is invalid.
        reason: SetDefect,
        /// Static description of the field.
        context: &'static str,
    },

    /// A fragment number violates a validity clause of §8.3.7.
    ///
    /// Fragment numbers are one-based, so zero is never legal.
    #[error("{context} fragment number {value} is not a valid positive fragment number")]
    InvalidFragmentNumber {
        /// The offending value.
        value: u32,
        /// Static description of the field.
        context: &'static str,
    },

    /// A `FragmentNumberSet` violates §9.4.2.8.
    #[error("invalid FragmentNumberSet in {context}: {reason}")]
    InvalidFragmentNumberSet {
        /// Why the set is invalid.
        reason: SetDefect,
        /// Static description of the field.
        context: &'static str,
    },

    /// A `HEARTBEAT` announced a range no writer can hold.
    ///
    /// §8.3.7.5.3 requires `lastSN >= firstSN - 1`; the equality case is how
    /// a writer with no samples yet announces an empty history.
    #[error("HEARTBEAT range is invalid: lastSN {last} is below firstSN {first} minus one")]
    InvalidHeartbeatRange {
        /// `firstSN` as received.
        first: i64,
        /// `lastSN` as received.
        last: i64,
    },

    /// A `DATA_FRAG` describes a fragmentation geometry that cannot exist.
    ///
    /// §8.3.7.3.3, transcribed in
    /// [`crate::messages::DataFrag::validate`].
    #[error("DATA_FRAG geometry is invalid: {reason}")]
    InvalidFragmentGeometry {
        /// Which clause failed.
        reason: FragmentDefect,
    },

    /// `octetsToInlineQos` would place the inline QoS inside a field that
    /// precedes it.
    ///
    /// §8.3.7.2.2 fixes the distance from the octet after
    /// `octetsToInlineQos` to the start of `inlineQos` at 16 for `DATA` and
    /// 28 for `DATA_FRAG`. A larger value is legal — it is the forward
    /// compatibility escape a future minor version would use, and the extra
    /// octets are skipped — but a smaller one overlaps `writerSN` and is
    /// refused.
    #[error(
        "submessage 0x{id:02x} declares octetsToInlineQos {declared}, \
         below the {minimum} its fixed fields occupy"
    )]
    InvalidOctetsToInlineQos {
        /// The `submessageId` octet.
        id: u8,
        /// The declared value.
        declared: usize,
        /// The value the fixed fields require.
        minimum: usize,
    },

    /// A `Locator` carries a `kind` outside the values §9.4.2.11 defines.
    ///
    /// Reserved and vendor-specific kinds decode into
    /// [`crate::structure::LocatorKind::Other`] rather than failing; this
    /// variant is what a caller gets when it asks a locator of an unusable
    /// kind for a socket address.
    #[error("locator kind {kind} is not one of the RTPS-defined kinds")]
    UnknownLocatorKind {
        /// The `kind` field as received.
        kind: i32,
    },

    /// A locator is well-formed but names a transport this build cannot use.
    #[error("locator of kind {kind} cannot be addressed by the UDPv4 transport")]
    UnsupportedLocator {
        /// The `kind` field as received.
        kind: i32,
    },

    /// A nested CDR value — inline QoS, a serialized payload, a parameter —
    /// failed to encode or decode.
    #[error("CDR error inside an RTPS submessage: {0}")]
    Cdr(#[from] CdrError),
}

impl RtpsError {
    /// True for the variants that mean "the buffer ran out": a truncated
    /// datagram rather than a semantically wrong one.
    ///
    /// The receive loop counts these separately, because a burst of them
    /// points at a path-MTU or reassembly problem rather than at a peer
    /// speaking a dialect we do not understand.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_rtps::RtpsError;
    ///
    /// let short = RtpsError::Truncated { needed: 8, available: 3, context: "writerSN" };
    /// assert!(short.is_truncation());
    /// assert!(!RtpsError::ConflictingDataFlags.is_truncation());
    /// ```
    #[must_use]
    pub const fn is_truncation(&self) -> bool {
        match self {
            Self::Truncated { .. } | Self::SubmessageOverrun { .. } => true,
            Self::Cdr(inner) => inner.is_truncation(),
            _ => false,
        }
    }

    /// True when the message is not RTPS at all, or is a major version this
    /// stack cannot speak.
    ///
    /// A participant that sees this on a metatraffic socket is sharing a port
    /// with something else; it is never a peer bug.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_rtps::RtpsError;
    ///
    /// assert!(RtpsError::BadProtocolId { found: *b"HTTP" }.is_foreign());
    /// ```
    #[must_use]
    pub const fn is_foreign(&self) -> bool {
        matches!(
            self,
            Self::BadProtocolId { .. } | Self::UnsupportedProtocolVersion { .. }
        )
    }

    /// True when a peer sent structurally valid octets whose *values* break a
    /// validity clause of §8.3.7.
    ///
    /// These are the interesting ones: they name a peer whose state machine
    /// disagrees with ours, and they are worth logging at a higher level than
    /// a truncation.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_rtps::RtpsError;
    ///
    /// let bad = RtpsError::InvalidSequenceNumber { value: 0, context: "DATA writerSN" };
    /// assert!(bad.is_validity_violation());
    /// ```
    #[must_use]
    pub const fn is_validity_violation(&self) -> bool {
        matches!(
            self,
            Self::InvalidSequenceNumber { .. }
                | Self::InvalidSequenceNumberSet { .. }
                | Self::InvalidFragmentNumber { .. }
                | Self::InvalidFragmentNumberSet { .. }
                | Self::InvalidHeartbeatRange { .. }
                | Self::InvalidFragmentGeometry { .. }
                | Self::InvalidOctetsToInlineQos { .. }
                | Self::ConflictingDataFlags
        )
    }

    /// Build a [`RtpsError::Truncated`] for a read of `needed` octets when
    /// only `available` remain.
    #[must_use]
    pub const fn truncated(context: &'static str, needed: usize, available: usize) -> Self {
        Self::Truncated {
            needed,
            available,
            context,
        }
    }
}

/// Why a `SequenceNumberSet` or `FragmentNumberSet` is invalid.
///
/// Both set types share one wire grammar (§9.4.2.6, §9.4.2.8) and therefore
/// one defect vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SetDefect {
    /// `numBits` exceeds the 256 the specification caps a set at.
    NumBitsTooLarge {
        /// The declared bit count.
        declared: u32,
    },
    /// The base is not a valid positive sequence or fragment number.
    ///
    /// §8.3.7.1.3 (`ACKNACK`) and §8.3.7.11.3 (`NACK_FRAG`) both require
    /// `bitmapBase >= 1`.
    BaseNotPositive,
    /// A number was inserted that the 256-bit window from `bitmapBase` does
    /// not reach.
    ///
    /// Only reachable through the builder API: the wire form cannot express
    /// it, because the trailing words are not transmitted.
    BitOutOfRange {
        /// How far the number lay from `bitmapBase`, signed — negative when
        /// it was *below* the base. The arithmetic is done in `i128` so the
        /// distance between the extremes of a `SequenceNumber` cannot wrap.
        offset: i128,
    },
}

impl core::fmt::Display for SetDefect {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NumBitsTooLarge { declared } => {
                write!(f, "numBits {declared} exceeds the maximum of 256")
            }
            Self::BaseNotPositive => write!(f, "bitmapBase must be at least 1"),
            Self::BitOutOfRange { offset } => write!(
                f,
                "an offset of {offset} from bitmapBase lies outside the 256-bit window"
            ),
        }
    }
}

/// Which clause of the `DATA_FRAG` validity rules (§8.3.7.3.3) failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FragmentDefect {
    /// `fragmentSize` is zero, so no fragment could carry anything.
    ZeroFragmentSize,
    /// `fragmentsInSubmessage` is zero, so the submessage carries nothing.
    NoFragments,
    /// `fragmentSize` exceeds `sampleSize`: one fragment would already be
    /// larger than the whole sample.
    FragmentLargerThanSample {
        /// `fragmentSize` as received.
        fragment_size: u16,
        /// `sampleSize` as received.
        sample_size: u32,
    },
    /// The fragment window runs past the last fragment the sample has.
    WindowPastEnd {
        /// One-based number of the first fragment in the submessage.
        starting: u32,
        /// How many fragments the submessage carries.
        count: u16,
        /// Total fragments the sample is divided into.
        total: u32,
    },
    /// The `serializedPayload` is too short for the fragments claimed.
    ///
    /// Every fragment but the very last one must be exactly `fragmentSize`
    /// octets, so the payload has a computable minimum length.
    PayloadTooShort {
        /// Octets the fragments require.
        needed: usize,
        /// Octets the payload holds.
        available: usize,
    },
}

impl core::fmt::Display for FragmentDefect {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ZeroFragmentSize => write!(f, "fragmentSize is zero"),
            Self::NoFragments => write!(f, "fragmentsInSubmessage is zero"),
            Self::FragmentLargerThanSample {
                fragment_size,
                sample_size,
            } => write!(
                f,
                "fragmentSize {fragment_size} exceeds sampleSize {sample_size}"
            ),
            Self::WindowPastEnd {
                starting,
                count,
                total,
            } => write!(
                f,
                "fragments {starting}..={} run past the {total} the sample has",
                u64::from(*starting) + u64::from(*count) - 1
            ),
            Self::PayloadTooShort { needed, available } => write!(
                f,
                "payload holds {available} octet(s), {needed} required by the fragment geometry"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn truncation_is_recognised_through_the_cdr_wrapper() {
        let inner = CdrError::Truncated {
            needed: 4,
            available: 1,
            context: "u32",
        };
        assert!(RtpsError::Cdr(inner).is_truncation());
        // `astrs-cdr` counts a missing PID_SENTINEL as a short buffer too,
        // and this classifier defers to it rather than second-guessing.
        assert!(RtpsError::Cdr(CdrError::MissingSentinel).is_truncation());
        assert!(!RtpsError::Cdr(CdrError::InvalidBoolean(2)).is_truncation());
    }

    #[test]
    fn foreign_traffic_is_separated_from_peer_bugs() {
        assert!(
            RtpsError::BadProtocolId {
                found: *b"\0\0\0\0"
            }
            .is_foreign()
        );
        assert!(RtpsError::UnsupportedProtocolVersion { major: 1, minor: 2 }.is_foreign());
        assert!(!RtpsError::ConflictingDataFlags.is_foreign());
    }

    #[test]
    fn validity_violations_are_their_own_class() {
        assert!(RtpsError::ConflictingDataFlags.is_validity_violation());
        assert!(RtpsError::InvalidHeartbeatRange { first: 5, last: 1 }.is_validity_violation());
        assert!(!RtpsError::truncated("x", 1, 0).is_validity_violation());
    }

    #[test]
    fn defect_displays_name_the_broken_clause() {
        assert_eq!(
            SetDefect::NumBitsTooLarge { declared: 300 }.to_string(),
            "numBits 300 exceeds the maximum of 256"
        );
        assert_eq!(
            SetDefect::BaseNotPositive.to_string(),
            "bitmapBase must be at least 1"
        );
        assert_eq!(
            SetDefect::BitOutOfRange { offset: -1 }.to_string(),
            "an offset of -1 from bitmapBase lies outside the 256-bit window"
        );
        assert_eq!(
            FragmentDefect::WindowPastEnd {
                starting: 3,
                count: 4,
                total: 5,
            }
            .to_string(),
            "fragments 3..=6 run past the 5 the sample has"
        );
        assert_eq!(
            FragmentDefect::FragmentLargerThanSample {
                fragment_size: 1024,
                sample_size: 100,
            }
            .to_string(),
            "fragmentSize 1024 exceeds sampleSize 100"
        );
        assert_eq!(
            FragmentDefect::ZeroFragmentSize.to_string(),
            "fragmentSize is zero"
        );
        assert_eq!(
            FragmentDefect::NoFragments.to_string(),
            "fragmentsInSubmessage is zero"
        );
        assert_eq!(
            FragmentDefect::PayloadTooShort {
                needed: 40,
                available: 8,
            }
            .to_string(),
            "payload holds 8 octet(s), 40 required by the fragment geometry"
        );
    }

    #[test]
    fn error_messages_name_the_offending_value() {
        assert_eq!(
            RtpsError::BadProtocolId { found: *b"HTTP" }.to_string(),
            "not an RTPS message: protocol id [48, 54, 54, 50] is not \"RTPS\""
        );
        assert_eq!(
            RtpsError::MisalignedSubmessage { offset: 23 }.to_string(),
            "submessage at offset 23 is not four-octet aligned"
        );
        assert_eq!(
            RtpsError::InvalidOctetsToInlineQos {
                id: 0x15,
                declared: 4,
                minimum: 16,
            }
            .to_string(),
            "submessage 0x15 declares octetsToInlineQos 4, below the 16 its fixed fields occupy"
        );
    }
}
