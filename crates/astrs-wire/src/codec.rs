//! The payload codec: one configuration, exact size hints, no trailing bytes.
//!
//! Blueprint §7.1 fixes the payload encoding to **oxicode with a little-endian,
//! varint configuration**, requires `encode_size_hint()` on every
//! payload-carrying type so buffers can be pre-sized exactly, and requires the
//! decoder to **reject trailing bytes**.
//!
//! Those three rules are expressed here as [`WIRE_CONFIG`], [`WireEncode`] and
//! [`WireDecode`]. Both traits have blanket implementations over
//! [`oxicode::Encode`] / [`oxicode::Decode`], so every wire type in this crate
//! gets them by deriving the two oxicode traits — there is exactly one codec
//! and no type can opt into a different one.
//!
//! # Why trailing bytes are fatal
//!
//! A decoder that stops at the end of a message it recognises and ignores what
//! follows will happily accept a payload written by a peer with a *different*
//! idea of the message's shape. The mismatch then surfaces later, somewhere
//! unrelated, as corrupt data. Rejecting the remainder turns a silent
//! compatibility break into an immediate, named error at the connection that
//! caused it.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{WireDecode, WireEncode, WireError};
//!
//! let value: u32 = 4_242;
//! let bytes = value.encode_to_vec()?;
//! assert_eq!(bytes.len(), value.encode_size_hint()?);
//! assert_eq!(u32::decode_exact(&bytes)?, 4_242);
//!
//! // One extra byte is an error, never a shrug.
//! let mut corrupted = bytes.clone();
//! corrupted.push(0);
//! assert!(matches!(
//!     u32::decode_exact(&corrupted),
//!     Err(WireError::TrailingBytes { .. })
//! ));
//! # Ok::<(), astrs_wire::WireError>(())
//! ```

use oxicode::config::Configuration;
use oxicode::enc::Writer;

use crate::error::{WireError, WireResult};

/// The one and only payload codec configuration: little-endian, varint,
/// no built-in length limit.
///
/// The limit is deliberately left to [`crate::FrameLimits`], which enforces it
/// on the frame as a whole rather than per nested collection — one cap, checked
/// in one place, before any allocation.
pub type WireConfig = Configuration;

/// The payload codec configuration mandated by blueprint §7.1.
///
/// # Examples
///
/// ```
/// // Varint encoding: a small integer costs one byte, not four.
/// let bytes = oxicode::encode_to_vec_with_config(&1u32, astrs_wire::WIRE_CONFIG)?;
/// assert_eq!(bytes, vec![1]);
/// # Ok::<(), oxicode::error::Error>(())
/// ```
pub const WIRE_CONFIG: WireConfig = oxicode::config::standard();

/// A [`Writer`] that appends to a caller-owned [`Vec<u8>`].
///
/// `oxicode::enc::VecWriter` owns its buffer, which would force an extra
/// allocation and copy every time a payload is appended to a frame that
/// already contains a header. This adapter writes straight into the tail of an
/// existing buffer instead.
struct VecAppendWriter<'a> {
    /// The buffer being appended to.
    out: &'a mut Vec<u8>,
}

impl Writer for VecAppendWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> Result<(), oxicode::error::Error> {
        self.out.extend_from_slice(bytes);
        Ok(())
    }
}

/// Encoding half of the wire codec.
///
/// Implemented blanket-style for every [`oxicode::Encode`] type, so wire
/// messages acquire it by `#[derive(oxicode::Encode)]`.
///
/// [`WireEncode::encode_size_hint`] is **exact**, not an estimate: it performs
/// a real size pass with `oxicode`'s counting writer. Every other method
/// depends on that exactness, and [`WireEncode::encode_presized`] verifies it
/// on every call.
pub trait WireEncode {
    /// The exact number of bytes [`WireEncode::encode_presized`] will produce.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::Codec`] if the value cannot be encoded at all
    /// (for instance a collection longer than `u64::MAX`).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::WireEncode;
    ///
    /// // Varint: 300 needs more than one byte, 1 does not.
    /// assert_eq!(1u32.encode_size_hint()?, 1);
    /// assert!(300u32.encode_size_hint()? > 1);
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    fn encode_size_hint(&self) -> WireResult<usize>;

    /// Appends the encoded form to `out`, reserving the exact space first.
    ///
    /// Returns the number of bytes appended. `out` is left untouched — not
    /// merely truncated back to a plausible length, but restored to its
    /// original length — if encoding fails part-way through.
    ///
    /// # Errors
    ///
    /// - [`WireError::Codec`] if the value cannot be encoded.
    /// - [`WireError::SizeHintMismatch`] if the number of bytes written differs
    ///   from [`WireEncode::encode_size_hint`], which indicates a broken
    ///   `oxicode::Encode` implementation.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::WireEncode;
    ///
    /// let mut buffer = vec![0xAA];
    /// let written = 7u32.encode_presized(&mut buffer)?;
    /// assert_eq!(written, 1);
    /// assert_eq!(buffer, vec![0xAA, 7]);
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    fn encode_presized(&self, out: &mut Vec<u8>) -> WireResult<usize>;

    /// Encodes into a freshly allocated, exactly-sized [`Vec<u8>`].
    ///
    /// # Errors
    ///
    /// As [`WireEncode::encode_presized`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::WireEncode;
    ///
    /// let bytes = (-1i32).encode_to_vec()?;
    /// assert!(!bytes.is_empty());
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    fn encode_to_vec(&self) -> WireResult<Vec<u8>> {
        let hint = self.encode_size_hint()?;
        let mut out = Vec::with_capacity(hint);
        self.encode_presized(&mut out)?;
        Ok(out)
    }
}

impl<T: oxicode::Encode> WireEncode for T {
    fn encode_size_hint(&self) -> WireResult<usize> {
        Ok(oxicode::encoded_size_with_config(self, WIRE_CONFIG)?)
    }

    fn encode_presized(&self, out: &mut Vec<u8>) -> WireResult<usize> {
        let hint = self.encode_size_hint()?;
        let start = out.len();
        out.reserve(hint);

        let result = {
            let writer = VecAppendWriter { out };
            oxicode::encode_into_writer(self, writer, WIRE_CONFIG)
        };
        if let Err(err) = result {
            out.truncate(start);
            return Err(WireError::Codec(err));
        }

        let written = out.len() - start;
        if written != hint {
            out.truncate(start);
            return Err(WireError::SizeHintMismatch { hint, written });
        }
        Ok(written)
    }
}

/// Decoding half of the wire codec.
///
/// Implemented blanket-style for every [`oxicode::Decode`] type.
pub trait WireDecode: Sized {
    /// Decodes a value that must occupy **all** of `bytes`.
    ///
    /// # Errors
    ///
    /// - [`WireError::Codec`] if the bytes are not a valid encoding.
    /// - [`WireError::TrailingBytes`] if a value decoded successfully but did
    ///   not consume the whole slice.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{WireDecode, WireEncode};
    ///
    /// let bytes = "camera".to_owned().encode_to_vec()?;
    /// assert_eq!(String::decode_exact(&bytes)?, "camera");
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    fn decode_exact(bytes: &[u8]) -> WireResult<Self>;

    /// Decodes a value from the front of `bytes`, returning it together with
    /// the number of bytes consumed.
    ///
    /// Unlike [`WireDecode::decode_exact`] this tolerates a remainder — it
    /// exists for callers that are parsing a concatenation they framed
    /// themselves (the protocol snapshot writer, for one), never for frame
    /// payloads.
    ///
    /// # Errors
    ///
    /// [`WireError::Codec`] if the prefix is not a valid encoding.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{WireDecode, WireEncode};
    ///
    /// let mut bytes = 9u16.encode_to_vec()?;
    /// bytes.extend_from_slice(b"tail");
    /// let (value, consumed) = u16::decode_prefix(&bytes)?;
    /// assert_eq!(value, 9);
    /// assert_eq!(&bytes[consumed..], b"tail");
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    fn decode_prefix(bytes: &[u8]) -> WireResult<(Self, usize)>;
}

impl<T: oxicode::Decode> WireDecode for T {
    fn decode_exact(bytes: &[u8]) -> WireResult<Self> {
        let (value, consumed) = Self::decode_prefix(bytes)?;
        if consumed != bytes.len() {
            return Err(WireError::TrailingBytes {
                consumed,
                trailing: bytes.len() - consumed,
            });
        }
        Ok(value)
    }

    fn decode_prefix(bytes: &[u8]) -> WireResult<(Self, usize)> {
        Ok(oxicode::decode_from_slice_with_config(bytes, WIRE_CONFIG)?)
    }
}

/// Round-trips a value through the wire codec.
///
/// A convenience for tests and for callers that need a deep copy of a message
/// with wire semantics (i.e. one that proves the value survives the wire).
///
/// # Errors
///
/// Any [`WireError`] raised by encoding or decoding.
///
/// # Examples
///
/// ```
/// use astrs_wire::codec::round_trip;
///
/// assert_eq!(round_trip(&vec![1u8, 2, 3])?, vec![1, 2, 3]);
/// # Ok::<(), astrs_wire::WireError>(())
/// ```
pub fn round_trip<T>(value: &T) -> WireResult<T>
where
    T: oxicode::Encode + oxicode::Decode,
{
    let bytes = value.encode_to_vec()?;
    T::decode_exact(&bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn config_is_little_endian_varint() {
        // Varint: values below 251 occupy a single byte in oxicode's varint
        // encoding, and multi-byte forms are little-endian.
        let one = oxicode::encode_to_vec_with_config(&1u32, WIRE_CONFIG).unwrap();
        assert_eq!(one, vec![1]);

        let large = oxicode::encode_to_vec_with_config(&0x0102_0304u32, WIRE_CONFIG).unwrap();
        // Marker byte followed by little-endian bytes.
        assert!(large.len() > 1);
        assert_eq!(&large[1..], &[0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn size_hint_is_exact_for_a_range_of_shapes() {
        macro_rules! check {
            ($value:expr) => {{
                let value = $value;
                let hint = value.encode_size_hint().unwrap();
                let bytes = value.encode_to_vec().unwrap();
                assert_eq!(hint, bytes.len(), "hint mismatch for {:?}", value);
            }};
        }

        check!(0u8);
        check!(u64::MAX);
        check!(-1i64);
        check!(1.5f64);
        check!(String::from("astrs"));
        check!(vec![0u8; 1000]);
        check!(Some(7u32));
        check!(Option::<u32>::None);
        check!(BTreeMap::from([(1u8, "a".to_owned()), (2, "b".to_owned())]));
        check!((1u8, 2u16, 3u32));
    }

    #[test]
    fn encode_presized_appends_without_disturbing_the_prefix() {
        let mut buffer = b"header".to_vec();
        let written = vec![1u8, 2, 3].encode_presized(&mut buffer).unwrap();
        assert_eq!(&buffer[..6], b"header");
        assert_eq!(buffer.len(), 6 + written);
    }

    #[test]
    fn encode_presized_reserves_exactly_the_hint() {
        let value = vec![7u8; 4096];
        let hint = value.encode_size_hint().unwrap();
        let mut buffer = Vec::new();
        let written = value.encode_presized(&mut buffer).unwrap();
        assert_eq!(written, hint);
        assert_eq!(buffer.len(), hint);
    }

    #[test]
    fn decode_exact_rejects_trailing_bytes() {
        let mut bytes = 1234u32.encode_to_vec().unwrap();
        let good = bytes.len();
        bytes.extend_from_slice(&[0, 0, 0]);
        match u32::decode_exact(&bytes) {
            Err(WireError::TrailingBytes { consumed, trailing }) => {
                assert_eq!(consumed, good);
                assert_eq!(trailing, 3);
            }
            other => panic!("expected TrailingBytes, got {other:?}"),
        }
    }

    #[test]
    fn decode_exact_rejects_truncated_input() {
        let bytes = String::from("hello").encode_to_vec().unwrap();
        let err = String::decode_exact(&bytes[..bytes.len() - 1]).unwrap_err();
        assert!(matches!(err, WireError::Codec(_)));
    }

    #[test]
    fn decode_prefix_tolerates_a_remainder() {
        let mut bytes = 9u16.encode_to_vec().unwrap();
        bytes.extend_from_slice(b"rest");
        let (value, consumed) = u16::decode_prefix(&bytes).unwrap();
        assert_eq!(value, 9);
        assert_eq!(&bytes[consumed..], b"rest");
    }

    #[test]
    fn round_trip_preserves_nested_collections() {
        let value: BTreeMap<String, Vec<u32>> =
            BTreeMap::from([("a".to_owned(), vec![1, 2, 3]), ("b".to_owned(), vec![])]);
        assert_eq!(round_trip(&value).unwrap(), value);
    }

    #[test]
    fn empty_input_decodes_only_zero_sized_shapes() {
        assert_eq!(<()>::decode_exact(&[]).unwrap(), ());
        assert!(u8::decode_exact(&[]).is_err());
    }

    #[test]
    fn encoding_is_deterministic() {
        let value = BTreeMap::from([(3u8, "c"), (1, "a"), (2, "b")]);
        let first = value.encode_to_vec().unwrap();
        let second = value.encode_to_vec().unwrap();
        assert_eq!(first, second);
    }
}
