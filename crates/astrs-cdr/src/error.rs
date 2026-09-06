//! The CDR error taxonomy.
//!
//! Every fallible path in this crate returns [`CdrError`]. The variants are
//! deliberately fine-grained: `astrs-rtps` discards malformed discovery
//! samples silently but logs *why*, and a single `InvalidData` bucket would
//! make that log useless. The taxonomy splits into five groups:
//!
//! 1. **Framing** — [`CdrError::UnknownEncapsulation`],
//!    [`CdrError::PaddingOverrun`]: the four-byte encapsulation header is
//!    unusable, so nothing below it can be trusted.
//! 2. **Bounds** — [`CdrError::Truncated`], [`CdrError::TrailingBytes`],
//!    [`CdrError::LengthOverflow`]: the buffer and the stream disagree about
//!    how many bytes exist. These are the variants a fuzzer drives.
//! 3. **Content** — [`CdrError::InvalidUtf8`], [`CdrError::InvalidUtf16`],
//!    [`CdrError::MissingNulTerminator`], [`CdrError::InteriorNul`],
//!    [`CdrError::InvalidBoolean`], [`CdrError::UnknownEnumerator`]: the
//!    bytes are present but do not spell a legal value of the target type.
//! 4. **IDL contract** — [`CdrError::BoundExceeded`],
//!    [`CdrError::SequenceTooLong`]: the value violates a bound the IDL
//!    declared (`string<64>`, `int32[<=8]`).
//! 5. **XCDR2 / ParameterList structure** — [`CdrError::BadEmHeader`],
//!    [`CdrError::MemberIdOutOfRange`], [`CdrError::DelimiterOverrun`],
//!    [`CdrError::UnknownMustUnderstand`], [`CdrError::MissingSentinel`],
//!    [`CdrError::UnsupportedParameter`].
//!
//! [`CdrError`] is `Clone + PartialEq + Eq`, which lets tests assert on an
//! exact expected error instead of a `matches!` shape.

use core::str::Utf8Error;

use thiserror::Error;

/// The result type every fallible CDR operation returns.
pub type CdrResult<T> = Result<T, CdrError>;

/// Everything that can go wrong encoding or decoding CDR.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum CdrError {
    /// The stream ended before the required number of bytes was available.
    ///
    /// `context` names the field being read (`"u32"`, `"string length"`,
    /// `"EMHEADER"`) so a discovery-packet log line is actionable.
    #[error("truncated CDR stream reading {context}: need {needed} byte(s), {available} left")]
    Truncated {
        /// Bytes the read required, *after* any alignment padding.
        needed: usize,
        /// Bytes actually available before the end of the current scope.
        available: usize,
        /// Static description of what was being read.
        context: &'static str,
    },

    /// The value decoded successfully but bytes were left over.
    ///
    /// Refusing trailing bytes is a hard rule of this crate: a sample whose
    /// declared type does not consume its whole payload is a type mismatch,
    /// and silently ignoring the tail is how a schema drift goes unnoticed
    /// for a week. Use [`crate::CdrReader::finish_tolerant`] when the caller
    /// genuinely owns padding it cannot describe (the ParameterList case).
    #[error("trailing CDR bytes: {remaining} byte(s) left after the value was decoded")]
    TrailingBytes {
        /// Bytes left unconsumed.
        remaining: usize,
    },

    /// The two-byte `representation_identifier` is not one this crate knows.
    #[error("unknown CDR encapsulation identifier 0x{identifier:04x}")]
    UnknownEncapsulation {
        /// The identifier as read, in host order (already byte-swapped from
        /// the big-endian wire form).
        identifier: u16,
    },

    /// The encapsulation options declared more trailing padding than the
    /// payload contains.
    #[error("encapsulation options declare {padding} pad byte(s) but only {available} remain")]
    PaddingOverrun {
        /// Padding count taken from the two low bits of the options field.
        padding: u8,
        /// Payload bytes present after the four-byte header.
        available: usize,
    },

    /// A declared length (sequence, string, DHEADER, parameter) cannot fit in
    /// the bytes that remain, so it is rejected *before* anything is
    /// allocated.
    ///
    /// This is the variant that stops `0xffff_ffff`-length hostile input from
    /// reaching `Vec::with_capacity`.
    #[error(
        "declared {context} length {declared} exceeds the {available} byte(s) available \
         (min {element_size} byte(s) per element)"
    )]
    LengthOverflow {
        /// The length as declared on the wire.
        declared: u64,
        /// Bytes available in the current scope.
        available: usize,
        /// Lower bound on the bytes a single element occupies.
        element_size: usize,
        /// Static description of the length's owner.
        context: &'static str,
    },

    /// A sequence length exceeds what a CDR `unsigned long` can express, or
    /// exceeds a [`crate::CdrReader`] configured element ceiling.
    #[error("sequence length {length} exceeds the maximum of {maximum}")]
    SequenceTooLong {
        /// The offending length.
        length: u64,
        /// The largest length accepted here.
        maximum: u64,
    },

    /// A bounded IDL type (`string<N>`, `sequence<T, N>`) was handed more
    /// elements than its declared bound.
    #[error("{context} bound of {bound} exceeded by a value of length {actual}")]
    BoundExceeded {
        /// The IDL-declared bound.
        bound: usize,
        /// The length actually presented.
        actual: usize,
        /// Static description of the bounded type.
        context: &'static str,
    },

    /// A CDR `string` did not end with the NUL octet its length promised.
    #[error("CDR string is not NUL-terminated")]
    MissingNulTerminator,

    /// A CDR `string` contained a NUL octet before its terminator.
    ///
    /// OMG CDR strings are C strings: the terminator is the only NUL. A
    /// Rust `String` may legally hold interior NULs, so this crate rejects
    /// such values on *encode* as well as decode, keeping the round trip
    /// total.
    #[error("CDR string contains an interior NUL at byte {index}")]
    InteriorNul {
        /// Index of the offending octet within the string body.
        index: usize,
    },

    /// A CDR `string` body was not valid UTF-8.
    ///
    /// ROS 2 defines `string` as UTF-8 (`rosidl` `String`), so this is a
    /// content error rather than a lossy-conversion opportunity.
    #[error("CDR string is not valid UTF-8: {0}")]
    InvalidUtf8(#[from] Utf8Error),

    /// A CDR `wstring` body was not valid UTF-16 (an unpaired surrogate).
    #[error("CDR wstring contains an unpaired surrogate at code unit {index}")]
    InvalidUtf16 {
        /// Index of the offending UTF-16 code unit.
        index: usize,
    },

    /// A CDR `boolean` octet was neither `0` nor `1`.
    ///
    /// OMG CDR 15.3.1 fixes the two legal encodings; accepting "any non-zero
    /// is true" would make the round trip non-total.
    #[error("CDR boolean octet must be 0 or 1, got {0}")]
    InvalidBoolean(u8),

    /// An enumerated type received a discriminant with no matching
    /// enumerator.
    #[error("no enumerator of {type_name} has discriminant {discriminant}")]
    UnknownEnumerator {
        /// IDL name of the enumerated type.
        type_name: &'static str,
        /// The discriminant read from the stream.
        discriminant: u32,
    },

    /// An enumerator's discriminant does not fit its declared `@bit_bound`.
    #[error("discriminant {discriminant} of {type_name} does not fit a {bit_bound}-bit bound")]
    BitBoundExceeded {
        /// IDL name of the enumerated type.
        type_name: &'static str,
        /// The offending discriminant.
        discriminant: u32,
        /// The declared bit bound (8, 16 or 32).
        bit_bound: u16,
    },

    /// An XCDR2 EMHEADER was structurally invalid.
    #[error("malformed XCDR2 EMHEADER: {0}")]
    BadEmHeader(&'static str),

    /// A member id does not fit the EMHEADER's 28-bit field.
    #[error("member id {0} exceeds the 28-bit EMHEADER field")]
    MemberIdOutOfRange(u32),

    /// A DHEADER or EMHEADER length runs past the end of the scope that
    /// contains it.
    ///
    /// Under-consumption of a delimited scope is *not* an error — that is
    /// exactly the forward compatibility a DHEADER buys, and
    /// [`crate::CdrReader::delimited`] skips forward instead. Over-declaring
    /// is, because it means the delimiter and the buffer disagree.
    #[error("delimited scope declares {declared} byte(s) but only {available} remain")]
    DelimiterOverrun {
        /// Length the DHEADER or EMHEADER declared.
        declared: usize,
        /// Bytes left in the enclosing scope.
        available: usize,
    },

    /// An XCDR2 mutable struct carried a member flagged `must_understand`
    /// whose id the reader does not know.
    #[error("unknown must-understand member id {member_id} — the sample must be discarded")]
    UnknownMustUnderstand {
        /// The member id that could not be interpreted.
        member_id: u32,
    },

    /// A `PL_CDR` parameter list ran out of bytes without a `PID_SENTINEL`.
    #[error("ParameterList ended without a PID_SENTINEL terminator")]
    MissingSentinel,

    /// A parameter id this crate refuses to guess at.
    ///
    /// `PID_EXTENDED` is the practical case: its long-form layout is not
    /// reproduced here because no in-repo, C/C++-free source pins it (§18),
    /// and a fabricated layout is worse than a clean refusal.
    #[error("unsupported ParameterList entry: parameter id 0x{id:04x}")]
    UnsupportedParameter {
        /// The parameter id encountered.
        id: u16,
    },

    /// A parameter's value is longer than the 16-bit `parameterLength` field.
    #[error("parameter 0x{id:04x} value of {length} byte(s) exceeds the 16-bit length field")]
    ParameterTooLong {
        /// The parameter id.
        id: u16,
        /// The padded value length that did not fit.
        length: usize,
    },

    /// The operation is not defined for the stream's encapsulation kind.
    ///
    /// Reading an EMHEADER out of an XCDR1 stream, or a ParameterList out of
    /// a `CDR_LE` stream, lands here.
    #[error("operation unsupported for this encapsulation: {0}")]
    UnsupportedEncapsulation(&'static str),

    /// A value's serialized size does not fit the addressable range (only
    /// reachable on 16-bit targets or with a corrupt length).
    #[error("serialized size overflow while {0}")]
    SizeOverflow(&'static str),
}

impl CdrError {
    /// True when the error means "this buffer was cut short", as opposed to
    /// "these bytes are wrong".
    ///
    /// `astrs-rtps` uses this to distinguish a fragmentation bug (retryable,
    /// worth a warning) from a peer speaking a different type (permanent,
    /// worth dropping the match).
    #[must_use]
    pub const fn is_truncation(&self) -> bool {
        matches!(
            self,
            Self::Truncated { .. }
                | Self::LengthOverflow { .. }
                | Self::PaddingOverrun { .. }
                | Self::DelimiterOverrun { .. }
                | Self::MissingSentinel
                | Self::MissingNulTerminator
        )
    }

    /// True when the error indicates a *type* disagreement between writer and
    /// reader rather than a damaged buffer.
    #[must_use]
    pub const fn is_type_mismatch(&self) -> bool {
        matches!(
            self,
            Self::TrailingBytes { .. }
                | Self::UnknownEnumerator { .. }
                | Self::UnknownMustUnderstand { .. }
                | Self::BoundExceeded { .. }
                | Self::BitBoundExceeded { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_classification_covers_the_short_buffer_family() {
        assert!(
            CdrError::Truncated {
                needed: 4,
                available: 1,
                context: "u32",
            }
            .is_truncation()
        );
        assert!(CdrError::MissingSentinel.is_truncation());
        assert!(CdrError::MissingNulTerminator.is_truncation());
        assert!(!CdrError::InvalidBoolean(2).is_truncation());
    }

    #[test]
    fn type_mismatch_classification_excludes_short_buffers() {
        assert!(CdrError::TrailingBytes { remaining: 3 }.is_type_mismatch());
        assert!(
            CdrError::UnknownEnumerator {
                type_name: "Colour",
                discriminant: 9,
            }
            .is_type_mismatch()
        );
        assert!(
            !CdrError::Truncated {
                needed: 8,
                available: 0,
                context: "u64",
            }
            .is_type_mismatch()
        );
    }

    #[test]
    fn errors_compare_by_value() {
        let a = CdrError::UnknownEncapsulation { identifier: 0x1234 };
        let b = CdrError::UnknownEncapsulation { identifier: 0x1234 };
        let c = CdrError::UnknownEncapsulation { identifier: 0x0004 };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn display_mentions_the_offending_values() {
        let text = CdrError::LengthOverflow {
            declared: 4_294_967_295,
            available: 12,
            element_size: 4,
            context: "sequence",
        }
        .to_string();
        assert!(text.contains("4294967295"), "{text}");
        assert!(text.contains("12"), "{text}");
        assert!(text.contains("sequence"), "{text}");
    }

    #[test]
    fn utf8_errors_convert_with_the_question_mark_operator() {
        fn decode(bytes: &[u8]) -> CdrResult<&str> {
            Ok(core::str::from_utf8(bytes)?)
        }
        assert!(matches!(
            decode(&[0xff, 0xfe]),
            Err(CdrError::InvalidUtf8(_))
        ));
        assert_eq!(decode(b"ok"), Ok("ok"));
    }
}
