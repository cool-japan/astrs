//! The single error type every framing and codec operation returns.
//!
//! [`WireError`] is deliberately *typed*: a peer that hands us a malformed
//! frame produces a variant that names the exact invariant that broke, with
//! the offending values attached. Nothing in this crate reports a framing
//! fault as a string, and nothing panics on hostile input (blueprint §3.8).
//!
//! Attacker-controlled text is never echoed back verbatim at unbounded
//! length — [`IdError`] truncates the values it quotes (see
//! [`preview`]) so a peer cannot make a log line arbitrarily large by
//! sending an arbitrarily long identifier.

use core::fmt;

use crate::frame::{Compression, FrameKind};

/// Longest attacker-controlled fragment quoted inside an error message.
///
/// Values longer than this are truncated by [`preview`] and marked with a
/// trailing ellipsis.
pub const ERROR_VALUE_PREVIEW_LEN: usize = 48;

/// Truncates `value` to at most [`ERROR_VALUE_PREVIEW_LEN`] characters for
/// inclusion in an error message.
///
/// Truncation happens on a `char` boundary, so the result is always valid
/// UTF-8, and an ellipsis (`…`) marks that something was cut.
///
/// # Examples
///
/// ```
/// use astrs_wire::error::preview;
///
/// assert_eq!(preview("camera"), "camera");
/// assert_eq!(preview(&"x".repeat(100)).chars().count(), 49);
/// ```
#[must_use]
pub fn preview(value: &str) -> String {
    let mut out = String::with_capacity(ERROR_VALUE_PREVIEW_LEN + 3);
    for (count, ch) in value.chars().enumerate() {
        if count == ERROR_VALUE_PREVIEW_LEN {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

/// Which family of identifier an [`IdError`] refers to.
///
/// Used purely for human-readable messages; the validation rules themselves
/// are identical for every [`IdKind::Name`]-shaped identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum IdKind {
    /// A [`crate::NodeId`].
    Node,
    /// A [`crate::DataId`] (an input or output port name).
    Data,
    /// An operator name inside a runtime-hosted node.
    Operator,
    /// A machine / host label carried by [`crate::DaemonId`].
    Machine,
    /// A parameter key ([`crate::ParamKey`]).
    Param,
    /// A port type URN ([`crate::TypeUrn`]).
    TypeUrn,
    /// A generic name with the shared `[A-Za-z0-9_.-]+` grammar.
    Name,
}

impl IdKind {
    /// The lower-case noun used in error messages.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::error::IdKind;
    ///
    /// assert_eq!(IdKind::Node.as_str(), "node id");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node id",
            Self::Data => "data id",
            Self::Operator => "operator id",
            Self::Machine => "machine name",
            Self::Param => "parameter key",
            Self::TypeUrn => "type urn",
            Self::Name => "name",
        }
    }
}

impl fmt::Display for IdKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an identifier failed validation.
///
/// Every identifier newtype in this crate rejects invalid values on *every*
/// construction path — [`std::str::FromStr`], `serde` deserialization and
/// `oxicode` decoding all funnel through the same validator, so a hostile
/// peer cannot inject an unvalidated identifier by encoding one directly.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IdError {
    /// The value was the empty string.
    #[error("{kind} must not be empty")]
    Empty {
        /// Which identifier family rejected the value.
        kind: IdKind,
    },
    /// The value was longer than the identifier family's byte limit.
    #[error("{kind} is {len} bytes, over the {max}-byte limit")]
    TooLong {
        /// Which identifier family rejected the value.
        kind: IdKind,
        /// Actual length in bytes.
        len: usize,
        /// Maximum permitted length in bytes.
        max: usize,
    },
    /// The value contained a character outside the permitted grammar.
    #[error("{kind} `{value}` contains the invalid character {ch:?} at byte {index}")]
    InvalidChar {
        /// Which identifier family rejected the value.
        kind: IdKind,
        /// A truncated preview of the offending value (see [`preview`]).
        value: String,
        /// The first character that violated the grammar.
        ch: char,
        /// Byte offset of `ch` within the original value.
        index: usize,
    },
    /// The value was structurally malformed in a family-specific way — a
    /// [`crate::DaemonId`] without a parseable UUID suffix, for example.
    #[error("{kind} `{value}` is malformed: {reason}")]
    Malformed {
        /// Which identifier family rejected the value.
        kind: IdKind,
        /// A truncated preview of the offending value (see [`preview`]).
        value: String,
        /// A short static explanation of the structural problem.
        reason: &'static str,
    },
}

impl IdError {
    /// The identifier family that produced this error.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::error::{IdError, IdKind};
    ///
    /// let err = IdError::Empty { kind: IdKind::Node };
    /// assert_eq!(err.kind(), IdKind::Node);
    /// ```
    #[must_use]
    pub const fn kind(&self) -> IdKind {
        match self {
            Self::Empty { kind }
            | Self::TooLong { kind, .. }
            | Self::InvalidChar { kind, .. }
            | Self::Malformed { kind, .. } => *kind,
        }
    }
}

/// Why an [`crate::AuthToken`] failed validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AuthTokenError {
    /// The textual form was not exactly [`crate::AUTH_TOKEN_HEX_LEN`]
    /// characters long.
    #[error("auth token must be exactly {expected} hex characters, found {found}")]
    BadLength {
        /// Required character count.
        expected: usize,
        /// Character count actually supplied.
        found: usize,
    },
    /// The textual form contained something other than `[0-9a-f]`.
    ///
    /// Upper-case hex is rejected on purpose: the canonical form is
    /// lower-case, and accepting both would make two spellings of the same
    /// token compare unequal as strings.
    #[error("auth token contains the non-lowercase-hex character {ch:?} at position {index}")]
    NotLowercaseHex {
        /// The offending character.
        ch: char,
        /// Its character offset in the supplied string.
        index: usize,
    },
    /// The platform CSPRNG refused to produce token bytes.
    #[error("auth token generation failed: {reason}")]
    RandomnessUnavailable {
        /// The underlying failure, rendered by the RNG implementation.
        reason: String,
    },
}

/// Everything that can go wrong while framing, encoding or decoding.
///
/// Framing faults ([`WireError::BadMagic`] … [`WireError::CrcMismatch`])
/// describe a byte-level violation of the §7.1 frame layout. Codec faults
/// wrap [`oxicode::error::Error`]. I/O faults wrap [`std::io::Error`] and
/// are the only variant a caller should normally retry.
///
/// # Examples
///
/// ```
/// use astrs_wire::{FrameLimits, HEADER_LEN, WireError, decode_frame};
///
/// // Too few bytes to hold a header is truncation, not corruption: the magic
/// // cannot be judged until a whole header has arrived.
/// let err = decode_frame(b"XX", &FrameLimits::default()).unwrap_err();
/// assert!(matches!(err, WireError::Truncated { .. }));
///
/// // A full-length header that does not start with the "AS" magic is
/// // rejected, not misinterpreted.
/// let err = decode_frame(&[b'X'; HEADER_LEN], &FrameLimits::default()).unwrap_err();
/// assert!(matches!(err, WireError::BadMagic { .. }));
/// ```
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WireError {
    /// The first two bytes were not the `"AS"` magic.
    #[error("frame magic mismatch: expected {expected:02x?}, found {found:02x?}")]
    BadMagic {
        /// The magic this build writes and expects.
        expected: [u8; 2],
        /// The two bytes actually found.
        found: [u8; 2],
    },
    /// The frame-layout version byte named a layout this build cannot parse.
    ///
    /// This is *not* the protocol version (that is negotiated in the
    /// handshake); it is the version of the byte layout itself, which has
    /// only ever been `1`.
    #[error("unsupported frame layout version {found} (this build speaks version {supported})")]
    UnsupportedFrameVersion {
        /// The version byte read off the wire.
        found: u8,
        /// The version this build writes.
        supported: u8,
    },
    /// The `kind` field held a discriminant this build does not know.
    ///
    /// Routers may choose to forward such frames opaquely; decoders reject
    /// them.
    #[error("unknown frame kind discriminant {found}")]
    UnknownKind {
        /// The 16-bit discriminant read off the wire.
        found: u16,
    },
    /// The `flags` byte had reserved bits set.
    ///
    /// Reserved bits are rejected rather than ignored so that a future
    /// version cannot silently give an old peer a frame it half-understands.
    #[error("frame flags 0x{bits:02x} set reserved bits 0x{reserved:02x}")]
    ReservedFlags {
        /// The complete flags byte.
        bits: u8,
        /// Just the bits that are not defined by this build.
        reserved: u8,
    },
    /// Both the `lz4` and the `zstd` compression bits were set.
    #[error("frame flags select both lz4 and zstd compression")]
    ConflictingCompression,
    /// A compressed payload reached a code path that does not decompress.
    ///
    /// Compression is negotiated per route and applied by `astrs-transport`,
    /// which owns the `oxiarc` codecs (blueprint §6.4). This crate reads and
    /// writes the flag bits but never transforms a payload, so it refuses
    /// rather than handing the codec a compressed byte stream.
    #[error("payload is {compression}-compressed; decompress it before decoding")]
    CompressedPayload {
        /// The codec the frame's flags select.
        compression: Compression,
    },
    /// The connection requires a CRC-32C trailer and the frame carried none.
    ///
    /// Blueprint §7.1: the checksum is mandatory on network legs and optional
    /// on Unix-domain sockets.
    #[error("frame carries no crc32c trailer, which this connection requires")]
    MissingCrc,
    /// The declared payload length exceeded the negotiated maximum.
    ///
    /// This is checked *before* any buffer is sized, so a ten-byte header can
    /// never provoke a large allocation.
    #[error("frame payload of {len} bytes exceeds the maximum of {max} bytes")]
    FrameTooLarge {
        /// Payload length declared by the header (or measured on encode).
        len: usize,
        /// The configured maximum.
        max: usize,
    },
    /// The input ended before a complete frame had been read.
    #[error("frame truncated: needed {expected} bytes, found {found}")]
    Truncated {
        /// How many bytes the frame required at this point.
        expected: usize,
        /// How many were available.
        found: usize,
    },
    /// The CRC-32C trailer did not match the computed checksum.
    #[error("crc32c mismatch: frame carries {expected:#010x}, computed {computed:#010x}")]
    CrcMismatch {
        /// The checksum carried by the frame.
        expected: u32,
        /// The checksum computed over header + payload.
        computed: u32,
    },
    /// A payload decoded successfully but did not consume every byte.
    ///
    /// Trailing bytes are always an error: they mean the sender and receiver
    /// disagree about the shape of the message, and silently ignoring the
    /// remainder is how protocol drift goes unnoticed for a release cycle.
    #[error("{trailing} trailing byte(s) after a complete payload of {consumed} byte(s)")]
    TrailingBytes {
        /// Bytes the decoder consumed.
        consumed: usize,
        /// Bytes left over.
        trailing: usize,
    },
    /// [`crate::WireEncode::encode_size_hint`] disagreed with the number of
    /// bytes the encoder actually wrote.
    ///
    /// The hint is contractually exact; a mismatch is a bug in a `WireEncode`
    /// implementation, surfaced loudly rather than papered over.
    #[error(
        "size hint mismatch: encode_size_hint() promised {hint} bytes, encoding wrote {written}"
    )]
    SizeHintMismatch {
        /// The promised size.
        hint: usize,
        /// The size actually produced.
        written: usize,
    },
    /// A typed decode helper was handed a frame of the wrong family.
    #[error("expected a {expected} frame, found {found}")]
    KindMismatch {
        /// The family the caller asked for.
        expected: FrameKind,
        /// The family the frame declared.
        found: FrameKind,
    },
    /// The `oxicode` payload codec failed.
    #[error("payload codec error: {0}")]
    Codec(#[from] oxicode::error::Error),
    /// The underlying stream failed.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// An identifier carried by a payload was invalid.
    #[error("invalid identifier: {0}")]
    Id(#[from] IdError),
    /// An authentication token carried by a payload was invalid.
    #[error("invalid auth token: {0}")]
    Auth(#[from] AuthTokenError),
}

impl WireError {
    /// Whether this error describes a *protocol* violation by the peer, as
    /// opposed to a local I/O failure.
    ///
    /// Connections that see a protocol violation should be closed: the byte
    /// stream is no longer trustworthy at any offset. I/O failures may be
    /// retried by the caller's reconnect policy.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::WireError;
    ///
    /// let protocol = WireError::ConflictingCompression;
    /// assert!(protocol.is_protocol_violation());
    ///
    /// let io = WireError::Io(std::io::Error::from(std::io::ErrorKind::WouldBlock));
    /// assert!(!io.is_protocol_violation());
    /// ```
    #[must_use]
    pub const fn is_protocol_violation(&self) -> bool {
        !matches!(self, Self::Io(_) | Self::SizeHintMismatch { .. })
    }

    /// Whether this error means the input simply ended early and more bytes
    /// might complete the frame.
    ///
    /// Buffered readers use this to distinguish "need more data" from "this
    /// stream is corrupt".
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameLimits, decode_frame};
    ///
    /// let err = decode_frame(b"AS\x01", &FrameLimits::default()).unwrap_err();
    /// assert!(err.is_incomplete());
    /// ```
    #[must_use]
    pub const fn is_incomplete(&self) -> bool {
        matches!(self, Self::Truncated { .. })
    }
}

/// Convenience alias for fallible wire operations.
pub type WireResult<T> = Result<T, WireError>;

/// Builds an [`oxicode::error::Error`] carrying a validation message.
///
/// Manual `Decode` implementations use this to reject values that are
/// syntactically well-formed but semantically invalid, so that validation
/// applies on the decode path exactly as it does on [`std::str::FromStr`].
pub(crate) fn codec_invalid(message: impl Into<String>) -> oxicode::error::Error {
    oxicode::error::Error::OwnedCustom {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn preview_passes_short_values_through() {
        assert_eq!(preview(""), "");
        assert_eq!(preview("camera/frames"), "camera/frames");
    }

    #[test]
    fn preview_truncates_long_values_on_char_boundaries() {
        let long = "ん".repeat(200);
        let short = preview(&long);
        assert_eq!(short.chars().count(), ERROR_VALUE_PREVIEW_LEN + 1);
        assert!(short.ends_with('…'));
        // Still valid UTF-8 by construction (String), and no partial char.
        assert!(
            short
                .chars()
                .take(ERROR_VALUE_PREVIEW_LEN)
                .all(|c| c == 'ん')
        );
    }

    #[test]
    fn preview_at_exact_boundary_does_not_add_ellipsis() {
        let exact = "a".repeat(ERROR_VALUE_PREVIEW_LEN);
        assert_eq!(preview(&exact), exact);
    }

    #[test]
    fn id_kind_display_matches_as_str() {
        for kind in [
            IdKind::Node,
            IdKind::Data,
            IdKind::Operator,
            IdKind::Machine,
            IdKind::Param,
            IdKind::TypeUrn,
            IdKind::Name,
        ] {
            assert_eq!(kind.to_string(), kind.as_str());
        }
    }

    #[test]
    fn id_error_reports_its_kind() {
        assert_eq!(IdError::Empty { kind: IdKind::Data }.kind(), IdKind::Data);
        assert_eq!(
            IdError::TooLong {
                kind: IdKind::Param,
                len: 9,
                max: 8,
            }
            .kind(),
            IdKind::Param
        );
        assert_eq!(
            IdError::InvalidChar {
                kind: IdKind::Node,
                value: "a b".to_owned(),
                ch: ' ',
                index: 1,
            }
            .kind(),
            IdKind::Node
        );
        assert_eq!(
            IdError::Malformed {
                kind: IdKind::Machine,
                value: "x".to_owned(),
                reason: "nope",
            }
            .kind(),
            IdKind::Machine
        );
    }

    #[test]
    fn protocol_violations_are_distinguished_from_io() {
        assert!(WireError::ConflictingCompression.is_protocol_violation());
        assert!(
            WireError::CrcMismatch {
                expected: 1,
                computed: 2
            }
            .is_protocol_violation()
        );
        assert!(
            !WireError::Io(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
                .is_protocol_violation()
        );
        assert!(
            !WireError::SizeHintMismatch {
                hint: 1,
                written: 2
            }
            .is_protocol_violation()
        );
    }

    #[test]
    fn incomplete_is_only_truncation() {
        assert!(
            WireError::Truncated {
                expected: 10,
                found: 3
            }
            .is_incomplete()
        );
        assert!(!WireError::ConflictingCompression.is_incomplete());
    }

    #[test]
    fn errors_render_useful_messages() {
        let err = WireError::BadMagic {
            expected: *b"AS",
            found: *b"XY",
        };
        let text = err.to_string();
        assert!(text.contains("magic"), "{text}");

        let err = WireError::FrameTooLarge { len: 100, max: 64 };
        assert!(err.to_string().contains("64"));

        let err = AuthTokenError::BadLength {
            expected: 64,
            found: 8,
        };
        assert!(err.to_string().contains("64"));
    }

    #[test]
    fn wire_error_stays_small_enough_for_result_returns() {
        // The `Result<T, WireError>` returned by every public API must stay
        // cheap to move; keep an eye on it if a variant grows.
        assert!(
            core::mem::size_of::<WireError>() <= 128,
            "WireError grew to {} bytes",
            core::mem::size_of::<WireError>()
        );
    }

    #[test]
    fn codec_invalid_carries_the_message() {
        let err = codec_invalid("bad node id");
        match err {
            oxicode::error::Error::OwnedCustom { message } => assert_eq!(message, "bad node id"),
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
