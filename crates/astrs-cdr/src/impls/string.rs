//! IDL `string` and `wstring`, and the [`WString`] type that carries the
//! latter.
//!
//! The two string types are encoded by **different** rules, and getting them
//! confused is the classic CDR interoperability bug:
//!
//! | | Length prefix counts | Terminator | Empty value |
//! |---|---|---|---|
//! | `string` | **octets, including the NUL** | one NUL octet | `01 00 00 00 00` |
//! | `wstring` | **`wchar` elements** | none | `00 00 00 00` |
//!
//! `string` follows OMG CDR (CORBA 3.0 §15.3.2.6): the `unsigned long`
//! counts the octets of the value *plus* its terminating NUL, so the empty
//! string has length `1`, never `0`. `wstring` follows OMG DDS-XTypes 1.3
//! §7.4.3.5.1: the `unsigned long` counts UTF-16 code units and no
//! terminator is written, so the empty `wstring` has length `0`.
//!
//! # Zero-copy
//!
//! `&'de str` implements [`CdrDeserialize`], so a generated message type may
//! borrow directly out of the received datagram:
//!
//! ```
//! # use astrs_cdr::{CdrReader, CdrError};
//! let bytes = [0x00, 0x01, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, b'/', b's', b'c', b'a', b'n', 0x00];
//! let mut reader = CdrReader::new(&bytes)?;
//! let topic: &str = reader.deserialize()?;
//! assert_eq!(topic, "/scan");
//! # Ok::<(), CdrError>(())
//! ```
//!
//! `wstring` cannot be borrowed: its code units are two-octet values in
//! stream byte order, so they have to be decoded into a `Vec<u16>` rather
//! than reinterpreted in place.
//!
//! # Encoding
//!
//! ROS 2 defines `string` as UTF-8 and `wstring` as UTF-16, so both are
//! validated. A `String` holding an interior NUL is refused on encode as well
//! as decode: OMG CDR strings are C strings, and accepting one on the way in
//! would produce a value this crate could not send back out.

use crate::error::{CdrError, CdrResult};
use crate::reader::CdrReader;
use crate::traits::{CdrDefault, CdrDeserialize, CdrSerialize, CdrType};
use crate::writer::CdrWriter;

/// An IDL `wstring`: a sequence of UTF-16 code units.
///
/// Stored as `Vec<u16>` rather than `String` so the round trip is total —
/// a peer may send an unpaired surrogate, and losing it silently would make
/// re-encoding produce different octets. Use [`WString::try_to_string`] to
/// convert with validation or [`WString::to_string_lossy`] to substitute
/// replacement characters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WString {
    units: Vec<u16>,
}

impl WString {
    /// An empty `wstring`.
    #[must_use]
    pub const fn new() -> Self {
        Self { units: Vec::new() }
    }

    /// Wrap UTF-16 code units, validating nothing.
    #[must_use]
    pub const fn from_units(units: Vec<u16>) -> Self {
        Self { units }
    }

    /// Encode a Rust string into UTF-16.
    #[must_use]
    pub fn from_str_lossless(text: &str) -> Self {
        Self {
            units: text.encode_utf16().collect(),
        }
    }

    /// The code units.
    #[must_use]
    pub fn units(&self) -> &[u16] {
        &self.units
    }

    /// Take ownership of the code units.
    #[must_use]
    pub fn into_units(self) -> Vec<u16> {
        self.units
    }

    /// Number of UTF-16 code units — the value the wire length prefix
    /// carries, which is *not* the number of characters for astral-plane
    /// text.
    #[must_use]
    pub fn len(&self) -> usize {
        self.units.len()
    }

    /// True when there are no code units.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// Append one code unit.
    pub fn push_unit(&mut self, unit: u16) {
        self.units.push(unit);
    }

    /// Append a Rust string's UTF-16 encoding.
    pub fn push_str(&mut self, text: &str) {
        self.units.extend(text.encode_utf16());
    }

    /// Convert to a Rust `String`, rejecting unpaired surrogates.
    ///
    /// # Errors
    ///
    /// [`CdrError::InvalidUtf16`] with the index of the offending code unit.
    pub fn try_to_string(&self) -> CdrResult<String> {
        let mut text = String::with_capacity(self.units.len());
        let mut index = 0_usize;
        for decoded in char::decode_utf16(self.units.iter().copied()) {
            match decoded {
                Ok(character) => {
                    text.push(character);
                    index += character.len_utf16();
                }
                Err(_) => return Err(CdrError::InvalidUtf16 { index }),
            }
        }
        Ok(text)
    }

    /// Convert to a Rust `String`, replacing unpaired surrogates with
    /// `U+FFFD`.
    #[must_use]
    pub fn to_string_lossy(&self) -> String {
        String::from_utf16_lossy(&self.units)
    }
}

impl From<&str> for WString {
    fn from(value: &str) -> Self {
        Self::from_str_lossless(value)
    }
}

impl From<String> for WString {
    fn from(value: String) -> Self {
        Self::from_str_lossless(&value)
    }
}

impl From<Vec<u16>> for WString {
    fn from(units: Vec<u16>) -> Self {
        Self::from_units(units)
    }
}

impl FromIterator<u16> for WString {
    fn from_iter<I: IntoIterator<Item = u16>>(iter: I) -> Self {
        Self {
            units: iter.into_iter().collect(),
        }
    }
}

impl CdrType for str {
    // Four octets of length plus the mandatory NUL.
    const MIN_SERIALIZED_SIZE: usize = 5;
}

impl CdrSerialize for str {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_str(self)
    }
}

impl<'de> CdrDeserialize<'de> for &'de str {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        reader.read_str()
    }
}

impl CdrType for String {
    const MIN_SERIALIZED_SIZE: usize = 5;
}

impl CdrSerialize for String {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_str(self)
    }
}

impl<'de> CdrDeserialize<'de> for String {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        reader.read_string()
    }
}

impl CdrDefault for String {
    fn cdr_default() -> Self {
        Self::new()
    }
}

impl CdrType for WString {
    // Four octets of length, and nothing more for the empty value.
    const MIN_SERIALIZED_SIZE: usize = 4;
}

impl CdrSerialize for WString {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_wstr(&self.units)
    }
}

impl<'de> CdrDeserialize<'de> for WString {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        Ok(Self::from_units(reader.read_wstring()?))
    }
}

impl CdrDefault for WString {
    fn cdr_default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::encoding::{EncapsulationKind, Encoding, WCharWidth};
    use crate::reader::from_bytes;
    use crate::writer::{to_vec, to_vec_headerless};

    #[test]
    fn string_body_matches_the_corba_rule() {
        let octets = to_vec_headerless(&"astrs".to_owned(), Encoding::ROS2).expect("encode");
        // Length 6 = five octets plus the NUL.
        assert_eq!(
            octets,
            [0x06, 0x00, 0x00, 0x00, b'a', b's', b't', b'r', b's', 0x00]
        );
    }

    #[test]
    fn empty_string_has_length_one_not_zero() {
        let octets = to_vec_headerless(&String::new(), Encoding::ROS2).expect("encode");
        assert_eq!(octets, [0x01, 0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn wstring_body_matches_the_xtypes_rule() {
        let value = WString::from("aé");
        let octets = to_vec_headerless(&value, Encoding::ROS2).expect("encode");
        // Two code units, no terminator.
        assert_eq!(octets, [0x02, 0x00, 0x00, 0x00, 0x61, 0x00, 0xe9, 0x00]);

        let empty = to_vec_headerless(&WString::new(), Encoding::ROS2).expect("encode");
        assert_eq!(empty, [0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn strings_round_trip_owned_and_borrowed() {
        let bytes = to_vec(&"hello".to_owned(), Encoding::ROS2).expect("encode");
        assert_eq!(from_bytes::<String>(&bytes).expect("decode"), "hello");
        assert_eq!(from_bytes::<&str>(&bytes).expect("decode"), "hello");
    }

    #[test]
    fn multibyte_utf8_is_counted_in_octets() {
        // "héllo" is six octets in UTF-8, so the prefix is 7.
        let bytes = to_vec_headerless(&"héllo".to_owned(), Encoding::ROS2).expect("encode");
        assert_eq!(bytes[0], 7);
        assert_eq!(bytes.len(), 4 + 7);
    }

    #[test]
    fn wstrings_round_trip_in_both_byte_orders() {
        let value = WString::from("日本語");
        for kind in [EncapsulationKind::CdrLe, EncapsulationKind::CdrBe] {
            let bytes = to_vec(&value, Encoding::new(kind)).expect("encode");
            assert_eq!(from_bytes::<WString>(&bytes).expect("decode"), value);
        }
    }

    #[test]
    fn wstring_survives_an_unpaired_surrogate() {
        // 0xd800 alone is not valid UTF-16, but it is valid on the wire and
        // must round trip unchanged.
        let value = WString::from_units(vec![0x0041, 0xd800]);
        let bytes = to_vec(&value, Encoding::ROS2).expect("encode");
        let decoded = from_bytes::<WString>(&bytes).expect("decode");
        assert_eq!(decoded, value);
        assert_eq!(
            decoded.try_to_string(),
            Err(CdrError::InvalidUtf16 { index: 1 })
        );
        assert_eq!(decoded.to_string_lossy(), "A\u{fffd}");
    }

    #[test]
    fn wstring_conversions_and_accessors() {
        let mut value = WString::new();
        assert!(value.is_empty());
        value.push_str("ab");
        value.push_unit(0x0063);
        assert_eq!(value.len(), 3);
        assert_eq!(value.units(), &[0x61, 0x62, 0x63]);
        assert_eq!(value.try_to_string().expect("valid"), "abc");
        assert_eq!(WString::from("abc".to_owned()), value);
        assert_eq!(WString::from(vec![0x61_u16, 0x62, 0x63]), value);
        assert_eq!(
            [0x61_u16, 0x62, 0x63].into_iter().collect::<WString>(),
            value
        );
        assert_eq!(value.clone().into_units(), vec![0x61, 0x62, 0x63]);
        assert_eq!(WString::cdr_default(), WString::new());
        assert_eq!(String::cdr_default(), "");
    }

    #[test]
    fn astral_plane_text_needs_two_code_units_per_character() {
        let value = WString::from("\u{1f680}");
        assert_eq!(value.len(), 2);
        let bytes = to_vec_headerless(&value, Encoding::ROS2).expect("encode");
        assert_eq!(bytes, [0x02, 0x00, 0x00, 0x00, 0x3d, 0xd8, 0x80, 0xde]);
        assert_eq!(value.try_to_string().expect("valid"), "\u{1f680}");
    }

    #[test]
    fn four_octet_wchar_mode_round_trips() {
        let encoding = Encoding::ROS2.with_wchar_width(WCharWidth::Four);
        let value = WString::from("hi");
        let bytes = to_vec_headerless(&value, encoding).expect("encode");
        assert_eq!(
            bytes,
            [
                0x02, 0x00, 0x00, 0x00, 0x68, 0x00, 0x00, 0x00, 0x69, 0x00, 0x00, 0x00
            ]
        );
        let mut reader = CdrReader::with_encoding(&bytes, encoding);
        assert_eq!(reader.deserialize::<WString>().expect("decode"), value);
    }

    #[test]
    fn string_min_sizes_are_honest_lower_bounds() {
        assert_eq!(<String as CdrType>::MIN_SERIALIZED_SIZE, 5);
        assert_eq!(<str as CdrType>::MIN_SERIALIZED_SIZE, 5);
        assert_eq!(<WString as CdrType>::MIN_SERIALIZED_SIZE, 4);
        assert_eq!(
            to_vec_headerless(&String::new(), Encoding::ROS2)
                .expect("encode")
                .len(),
            5
        );
        assert_eq!(
            to_vec_headerless(&WString::new(), Encoding::ROS2)
                .expect("encode")
                .len(),
            4
        );
    }
}
