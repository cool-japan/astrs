//! Strict, allocation-bounded base64 for environment-carried blobs.
//!
//! Blueprint §24.2 hands a spawned node its configuration in
//! `ASTRS_NODE_CONFIG` as an **oxicode + base64** blob, "never YAML-in-env".
//! oxicode gives the bytes; this module gives the transport-safe text, because
//! an environment variable is a NUL-terminated byte string that a shell, a
//! process manager and a container runtime all feel free to inspect.
//!
//! The alphabet is RFC 4648 §4 (standard, `+` and `/`) with mandatory `=`
//! padding — the form every other tool in a robotics deployment already reads.
//!
//! # Why the decoder is strict
//!
//! The input arrives from the process environment, which is *outside* the
//! authenticated wire: whoever spawned this process chose it, and a
//! misconfigured supervisor is as likely a source as a hostile one. So the
//! decoder refuses everything ambiguous rather than repairing it:
//!
//! | Refused | Why |
//! |---|---|
//! | Any byte outside the alphabet — whitespace and newlines included | A blob that survived a line-wrapping editor is not the blob that was written |
//! | A length that is not a multiple of four | Truncation, not a shorter message |
//! | `=` anywhere but the last one or two positions | The only padding shapes that exist |
//! | Non-zero bits in a padded group's tail | Two distinct texts would decode to one byte string, which is a canonicalisation hole |
//!
//! [`decode`] also bounds its allocation *before* it allocates: the output
//! length is a pure function of the input length, so an oversized blob is
//! rejected by [`decode_with_limit`] without a single byte being reserved.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::base64;
//!
//! let text = base64::encode(b"astrs");
//! assert_eq!(text, "YXN0cnM=");
//! assert_eq!(base64::decode(&text)?, b"astrs");
//!
//! // Whitespace is not silently skipped.
//! assert!(base64::decode("YXN0 cnM=").is_err());
//! // Nor are non-canonical trailing bits.
//! assert!(base64::decode("YXN0cnN=").is_err());
//! # Ok::<(), astrs_wire::base64::Base64Error>(())
//! ```

use core::fmt;

/// The RFC 4648 §4 alphabet, in index order.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// The padding character.
const PAD: u8 = b'=';

/// Sentinel for "this byte is not in the alphabet" in [`DECODE_TABLE`].
const INVALID: u8 = 0xFF;

/// The inverse of [`ALPHABET`], built at compile time so decoding is a table
/// lookup rather than a search.
const DECODE_TABLE: [u8; 256] = {
    let mut table = [INVALID; 256];
    let mut index = 0usize;
    while index < 64 {
        table[ALPHABET[index] as usize] = index as u8;
        index += 1;
    }
    table
};

/// The default ceiling [`decode`] applies to its output: 16 MiB.
///
/// A node configuration blob is kilobytes; anything approaching this is a
/// mistake or an attack, and either way the answer is the same.
pub const DEFAULT_MAX_DECODED_LEN: usize = 16 * 1024 * 1024;

/// Why a base64 text could not be decoded.
///
/// # Examples
///
/// ```
/// use astrs_wire::base64::{self, Base64Error};
///
/// assert!(matches!(
///     base64::decode("abc"),
///     Err(Base64Error::UnalignedLength { len: 3 })
/// ));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Base64Error {
    /// The text's length was not a multiple of four.
    UnalignedLength {
        /// The length that was offered.
        len: usize,
    },
    /// A byte outside the alphabet (and not padding) appeared.
    InvalidByte {
        /// Where it appeared, counted in bytes from the start.
        index: usize,
        /// The offending byte.
        byte: u8,
    },
    /// Padding appeared somewhere other than the final one or two positions.
    MisplacedPadding {
        /// Where the stray `=` appeared.
        index: usize,
    },
    /// The final group's unused bits were not zero, so a second, different
    /// text would decode to the same bytes.
    NonCanonicalTail,
    /// The text would decode to more bytes than the caller allows.
    TooLong {
        /// How many bytes it would produce.
        len: usize,
        /// The ceiling that was applied.
        max: usize,
    },
}

impl fmt::Display for Base64Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnalignedLength { len } => {
                write!(f, "base64 length {len} is not a multiple of four")
            }
            Self::InvalidByte { index, byte } => {
                write!(f, "byte 0x{byte:02x} at offset {index} is not base64")
            }
            Self::MisplacedPadding { index } => {
                write!(f, "base64 padding at offset {index} is not at the end")
            }
            Self::NonCanonicalTail => f.write_str("base64 tail has non-zero unused bits"),
            Self::TooLong { len, max } => {
                write!(
                    f,
                    "base64 would decode to {len} bytes, over the {max}-byte limit"
                )
            }
        }
    }
}

impl std::error::Error for Base64Error {}

/// The exact number of text bytes [`encode`] produces for `len` input bytes.
///
/// # Examples
///
/// ```
/// use astrs_wire::base64;
///
/// assert_eq!(base64::encoded_len(0), 0);
/// assert_eq!(base64::encoded_len(1), 4);
/// assert_eq!(base64::encoded_len(3), 4);
/// assert_eq!(base64::encoded_len(4), 8);
/// ```
#[must_use]
pub const fn encoded_len(len: usize) -> usize {
    len.div_ceil(3) * 4
}

/// The exact number of bytes a `len`-byte text with `pad` padding characters
/// decodes to.
#[must_use]
const fn decoded_len(len: usize, pad: usize) -> usize {
    (len / 4) * 3 - pad
}

/// Encodes `bytes` as standard, padded base64.
///
/// # Examples
///
/// ```
/// use astrs_wire::base64;
///
/// assert_eq!(base64::encode(b""), "");
/// assert_eq!(base64::encode(b"f"), "Zg==");
/// assert_eq!(base64::encode(b"fo"), "Zm8=");
/// assert_eq!(base64::encode(b"foo"), "Zm9v");
/// assert_eq!(base64::encode(b"foobar"), "Zm9vYmFy");
/// ```
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(encoded_len(bytes.len()));
    encode_into(bytes, &mut out);
    out
}

/// Appends the base64 form of `bytes` to `out`, reserving the exact space
/// first.
///
/// The pre-sized variant, for a caller building a larger string (an
/// environment assignment, a command line) without a second allocation.
///
/// # Examples
///
/// ```
/// use astrs_wire::base64;
///
/// let mut assignment = String::from("ASTRS_NODE_CONFIG=");
/// base64::encode_into(b"astrs", &mut assignment);
/// assert_eq!(assignment, "ASTRS_NODE_CONFIG=YXN0cnM=");
/// ```
pub fn encode_into(bytes: &[u8], out: &mut String) {
    out.reserve(encoded_len(bytes.len()));
    // `as_chunks::<3>` types each group as `&[u8; 3]`, so the destructuring
    // below is infallible and the length is checked once, at compile time,
    // rather than on every iteration of this hot path.
    let (groups, tail) = bytes.as_chunks::<3>();
    for &[high, mid, low] in groups {
        let group = (u32::from(high) << 16) | (u32::from(mid) << 8) | u32::from(low);
        push_symbol(out, (group >> 18) & 0x3F);
        push_symbol(out, (group >> 12) & 0x3F);
        push_symbol(out, (group >> 6) & 0x3F);
        push_symbol(out, group & 0x3F);
    }
    match tail.len() {
        1 => {
            let group = u32::from(tail[0]) << 16;
            push_symbol(out, (group >> 18) & 0x3F);
            push_symbol(out, (group >> 12) & 0x3F);
            out.push('=');
            out.push('=');
        }
        2 => {
            let group = (u32::from(tail[0]) << 16) | (u32::from(tail[1]) << 8);
            push_symbol(out, (group >> 18) & 0x3F);
            push_symbol(out, (group >> 12) & 0x3F);
            push_symbol(out, (group >> 6) & 0x3F);
            out.push('=');
        }
        // `as_chunks::<3>` leaves a remainder of 0, 1 or 2 bytes.
        _ => {}
    }
}

/// Pushes the alphabet symbol for a six-bit group.
///
/// `index` is always masked to `0..64` by every caller, so the lookup cannot
/// fail; the fallback keeps the function total without a panic.
fn push_symbol(out: &mut String, index: u32) {
    let symbol = ALPHABET.get(index as usize).copied().unwrap_or(b'A');
    out.push(char::from(symbol));
}

/// Decodes standard, padded base64 under [`DEFAULT_MAX_DECODED_LEN`].
///
/// # Errors
///
/// Any [`Base64Error`]; see the module documentation for what is refused and
/// why.
///
/// # Examples
///
/// ```
/// use astrs_wire::base64;
///
/// assert_eq!(base64::decode("Zm9vYmFy")?, b"foobar");
/// assert!(base64::decode("Zm9vYmFy=").is_err());
/// # Ok::<(), astrs_wire::base64::Base64Error>(())
/// ```
pub fn decode(text: &str) -> Result<Vec<u8>, Base64Error> {
    decode_with_limit(text, DEFAULT_MAX_DECODED_LEN)
}

/// Decodes standard, padded base64, refusing anything that would exceed
/// `max_decoded_len` bytes **before** reserving memory for it.
///
/// # Errors
///
/// Any [`Base64Error`].
///
/// # Examples
///
/// ```
/// use astrs_wire::base64::{self, Base64Error};
///
/// assert!(matches!(
///     base64::decode_with_limit("Zm9vYmFy", 3),
///     Err(Base64Error::TooLong { len: 6, max: 3 })
/// ));
/// ```
pub fn decode_with_limit(text: &str, max_decoded_len: usize) -> Result<Vec<u8>, Base64Error> {
    let bytes = text.as_bytes();
    let len = bytes.len();
    if len == 0 {
        return Ok(Vec::new());
    }
    if !len.is_multiple_of(4) {
        return Err(Base64Error::UnalignedLength { len });
    }

    // Padding is only legal in the last two positions, and only as a suffix.
    let mut pad = 0usize;
    if bytes[len - 1] == PAD {
        pad = 1;
        if len >= 2 && bytes[len - 2] == PAD {
            pad = 2;
        }
    }

    let out_len = decoded_len(len, pad);
    if out_len > max_decoded_len {
        return Err(Base64Error::TooLong {
            len: out_len,
            max: max_decoded_len,
        });
    }

    let mut out = Vec::with_capacity(out_len);
    let groups = len / 4;
    for group_index in 0..groups {
        let base = group_index * 4;
        let last_group = group_index + 1 == groups;
        let mut symbols = [0u8; 4];
        for (offset, symbol) in symbols.iter_mut().enumerate() {
            let index = base + offset;
            let byte = bytes[index];
            if byte == PAD {
                // Legal only in the final group, and only in the positions
                // the padding count already accounted for.
                let padding_starts_at = 4 - pad;
                if !last_group || pad == 0 || offset < padding_starts_at {
                    return Err(Base64Error::MisplacedPadding { index });
                }
                *symbol = 0;
                continue;
            }
            let decoded = DECODE_TABLE[byte as usize];
            if decoded == INVALID {
                return Err(Base64Error::InvalidByte { index, byte });
            }
            *symbol = decoded;
        }

        let group = (u32::from(symbols[0]) << 18)
            | (u32::from(symbols[1]) << 12)
            | (u32::from(symbols[2]) << 6)
            | u32::from(symbols[3]);
        let keep = if last_group { 3 - pad } else { 3 };
        if keep >= 1 {
            out.push(((group >> 16) & 0xFF) as u8);
        }
        if keep >= 2 {
            out.push(((group >> 8) & 0xFF) as u8);
        }
        if keep >= 3 {
            out.push((group & 0xFF) as u8);
        }
        // A padded group must not carry bits that the kept bytes discard, or
        // several texts would decode to one byte string.
        if last_group && pad > 0 {
            let discarded_bits = pad * 8;
            let mask = (1u32 << discarded_bits) - 1;
            if group & mask != 0 {
                return Err(Base64Error::NonCanonicalTail);
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// RFC 4648 §10 test vectors.
    const RFC_VECTORS: &[(&[u8], &str)] = &[
        (b"", ""),
        (b"f", "Zg=="),
        (b"fo", "Zm8="),
        (b"foo", "Zm9v"),
        (b"foob", "Zm9vYg=="),
        (b"fooba", "Zm9vYmE="),
        (b"foobar", "Zm9vYmFy"),
    ];

    #[test]
    fn the_rfc_vectors_encode_and_decode() {
        for (bytes, text) in RFC_VECTORS {
            assert_eq!(&encode(bytes), text, "encoding {bytes:?}");
            assert_eq!(&decode(text).unwrap(), bytes, "decoding {text}");
            assert_eq!(encoded_len(bytes.len()), text.len());
        }
    }

    #[test]
    fn every_byte_value_survives_a_round_trip() {
        let all: Vec<u8> = (0..=255u8).collect();
        for len in 0..=all.len() {
            let slice = &all[..len];
            let text = encode(slice);
            assert_eq!(text.len(), encoded_len(len));
            assert_eq!(decode(&text).unwrap(), slice, "length {len}");
        }
    }

    #[test]
    fn the_alphabet_is_covered_in_both_directions() {
        for (index, symbol) in ALPHABET.iter().enumerate() {
            assert_eq!(usize::from(DECODE_TABLE[*symbol as usize]), index);
        }
        assert_eq!(DECODE_TABLE[PAD as usize], INVALID);
        assert_eq!(DECODE_TABLE[b' ' as usize], INVALID);
        assert_eq!(
            DECODE_TABLE[b'-' as usize], INVALID,
            "url alphabet is not accepted"
        );
        assert_eq!(
            DECODE_TABLE[b'_' as usize], INVALID,
            "url alphabet is not accepted"
        );
    }

    #[test]
    fn an_unaligned_length_is_refused() {
        for text in ["a", "ab", "abc", "Zm9vYmFyZ"] {
            assert!(matches!(
                decode(text),
                Err(Base64Error::UnalignedLength { .. })
            ));
        }
    }

    #[test]
    fn whitespace_and_newlines_are_not_skipped() {
        // Length-aligned, so the whitespace itself is what is refused.
        for text in ["Zm9v Ym8", "Zm9v\nYm8", "Zm9v\tYm8", "Z m9"] {
            assert!(
                matches!(decode(text), Err(Base64Error::InvalidByte { .. })),
                "{text}"
            );
        }
        // A line-wrapped blob usually also loses its alignment, and that is
        // caught one step earlier.
        assert!(matches!(
            decode("Zm9v YmFy"),
            Err(Base64Error::UnalignedLength { len: 9 })
        ));
    }

    #[test]
    fn misplaced_padding_is_refused() {
        for text in ["Zg==Zg==", "Z=g=", "=Zm9", "Zm=v"] {
            let err = decode(text).unwrap_err();
            assert!(
                matches!(
                    err,
                    Base64Error::MisplacedPadding { .. } | Base64Error::InvalidByte { .. }
                ),
                "{text}: {err}"
            );
        }
    }

    #[test]
    fn non_canonical_tails_are_refused() {
        // "Zg==" is `f`; "Zh==" would decode to the same byte with different
        // discarded bits, which is exactly the ambiguity we reject.
        assert_eq!(decode("Zg==").unwrap(), b"f");
        assert!(matches!(decode("Zh=="), Err(Base64Error::NonCanonicalTail)));
        assert_eq!(decode("Zm8=").unwrap(), b"fo");
        assert!(matches!(
            decode("Zm9=").unwrap_err(),
            Base64Error::NonCanonicalTail
        ));
    }

    #[test]
    fn the_limit_is_checked_before_allocating() {
        let text = encode(&[7u8; 3_000]);
        assert!(matches!(
            decode_with_limit(&text, 1_024),
            Err(Base64Error::TooLong {
                len: 3_000,
                max: 1_024
            })
        ));
        assert_eq!(decode_with_limit(&text, 3_000).unwrap().len(), 3_000);
    }

    #[test]
    fn encode_into_appends_rather_than_replacing() {
        let mut buffer = String::from("prefix:");
        encode_into(b"foobar", &mut buffer);
        assert_eq!(buffer, "prefix:Zm9vYmFy");
        encode_into(b"", &mut buffer);
        assert_eq!(buffer, "prefix:Zm9vYmFy");
    }

    #[test]
    fn errors_describe_themselves() {
        let errors = [
            Base64Error::UnalignedLength { len: 3 },
            Base64Error::InvalidByte {
                index: 1,
                byte: b' ',
            },
            Base64Error::MisplacedPadding { index: 0 },
            Base64Error::NonCanonicalTail,
            Base64Error::TooLong { len: 9, max: 8 },
        ];
        for error in errors {
            assert!(!error.to_string().is_empty());
            let _: &dyn std::error::Error = &error;
        }
    }
}
