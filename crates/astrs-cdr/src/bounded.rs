//! Bounded IDL types: `string<N>`, `wstring<N>` and `sequence<T, N>`.
//!
//! A bound is an IDL contract, not a wire format: `string<64>` is encoded
//! exactly like `string`, and the only difference is that a value longer than
//! 64 is illegal. ROS 2 spells the same idea `string<=64` and `int32[<=8]`.
//!
//! The bound is enforced in three places, so an over-long value can neither
//! be built, sent, nor accepted:
//!
//! 1. construction — [`BoundedString::new`] and friends are fallible;
//! 2. encode — [`CdrSerialize`] re-checks, because a value can be mutated
//!    through [`BoundedSequence::as_mut_vec`];
//! 3. decode — [`CdrDeserialize`] checks the length *before* reading the
//!    elements, so a peer that ignores the bound cannot make this side
//!    allocate past it.
//!
//! Fixed-size IDL arrays (`T[N]`) need no wrapper: `[T; N]` already carries
//! the length in its type.
//!
//! ```
//! use astrs_cdr::{BoundedString, CdrError};
//!
//! let name = BoundedString::<8>::new("odom")?;
//! assert_eq!(name.as_str(), "odom");
//!
//! assert_eq!(
//!     BoundedString::<3>::new("too long").unwrap_err(),
//!     CdrError::BoundExceeded { bound: 3, actual: 8, context: "string<N>" },
//! );
//! # Ok::<(), CdrError>(())
//! ```

use core::ops::Deref;

use crate::error::{CdrError, CdrResult};
use crate::impls::string::WString;
use crate::reader::CdrReader;
use crate::traits::{CdrDefault, CdrDeserialize, CdrSerialize, CdrType};
use crate::writer::CdrWriter;

fn check_bound(actual: usize, bound: usize, context: &'static str) -> CdrResult<()> {
    if actual > bound {
        Err(CdrError::BoundExceeded {
            bound,
            actual,
            context,
        })
    } else {
        Ok(())
    }
}

/// An IDL `string<N>`: UTF-8 text of at most `N` octets, terminator excluded.
///
/// The bound counts octets, matching the IDL definition — a `string<4>` holds
/// four ASCII characters or two two-octet ones.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BoundedString<const N: usize> {
    value: String,
}

impl<const N: usize> BoundedString<N> {
    /// The declared bound.
    pub const BOUND: usize = N;

    /// Build from text, checking the bound.
    ///
    /// # Errors
    ///
    /// [`CdrError::BoundExceeded`] when `value` is longer than `N` octets.
    pub fn new(value: impl Into<String>) -> CdrResult<Self> {
        let value = value.into();
        check_bound(value.len(), N, "string<N>")?;
        Ok(Self { value })
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Take ownership of the text.
    #[must_use]
    pub fn into_string(self) -> String {
        self.value
    }

    /// Octets of text, terminator excluded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.value.len()
    }

    /// True when the text is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }
}

impl<const N: usize> Deref for BoundedString<N> {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<const N: usize> TryFrom<String> for BoundedString<N> {
    type Error = CdrError;

    fn try_from(value: String) -> CdrResult<Self> {
        Self::new(value)
    }
}

impl<const N: usize> TryFrom<&str> for BoundedString<N> {
    type Error = CdrError;

    fn try_from(value: &str) -> CdrResult<Self> {
        Self::new(value)
    }
}

impl<const N: usize> CdrType for BoundedString<N> {
    const MIN_SERIALIZED_SIZE: usize = 5;
}

impl<const N: usize> CdrSerialize for BoundedString<N> {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        check_bound(self.value.len(), N, "string<N>")?;
        writer.write_str(&self.value)
    }
}

impl<'de, const N: usize> CdrDeserialize<'de> for BoundedString<N> {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        let value = reader.read_str()?;
        check_bound(value.len(), N, "string<N>")?;
        Ok(Self {
            value: value.to_owned(),
        })
    }
}

impl<const N: usize> CdrDefault for BoundedString<N> {
    fn cdr_default() -> Self {
        Self {
            value: String::new(),
        }
    }
}

/// An IDL `wstring<N>`: at most `N` UTF-16 code units.
///
/// The bound counts code units, which is what the wire length prefix counts —
/// an astral-plane character costs two.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BoundedWString<const N: usize> {
    value: WString,
}

impl<const N: usize> BoundedWString<N> {
    /// The declared bound, in UTF-16 code units.
    pub const BOUND: usize = N;

    /// Build from a `wstring`, checking the bound.
    ///
    /// # Errors
    ///
    /// [`CdrError::BoundExceeded`] when the value holds more than `N` code
    /// units.
    pub fn new(value: impl Into<WString>) -> CdrResult<Self> {
        let value = value.into();
        check_bound(value.len(), N, "wstring<N>")?;
        Ok(Self { value })
    }

    /// The `wstring`.
    #[must_use]
    pub const fn as_wstring(&self) -> &WString {
        &self.value
    }

    /// Take ownership of the `wstring`.
    #[must_use]
    pub fn into_wstring(self) -> WString {
        self.value
    }

    /// Number of UTF-16 code units.
    #[must_use]
    pub fn len(&self) -> usize {
        self.value.len()
    }

    /// True when there are no code units.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }
}

impl<const N: usize> CdrType for BoundedWString<N> {
    const MIN_SERIALIZED_SIZE: usize = 4;
}

impl<const N: usize> CdrSerialize for BoundedWString<N> {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        check_bound(self.value.len(), N, "wstring<N>")?;
        writer.write_wstr(self.value.units())
    }
}

impl<'de, const N: usize> CdrDeserialize<'de> for BoundedWString<N> {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        let units = reader.read_wstring()?;
        check_bound(units.len(), N, "wstring<N>")?;
        Ok(Self {
            value: WString::from_units(units),
        })
    }
}

impl<const N: usize> CdrDefault for BoundedWString<N> {
    fn cdr_default() -> Self {
        Self {
            value: WString::new(),
        }
    }
}

/// An IDL `sequence<T, N>`: at most `N` elements.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BoundedSequence<T, const N: usize> {
    items: Vec<T>,
}

impl<T, const N: usize> BoundedSequence<T, N> {
    /// The declared bound.
    pub const BOUND: usize = N;

    /// An empty sequence.
    #[must_use]
    pub const fn new() -> Self {
        Self { items: Vec::new() }
    }

    /// Build from a vector, checking the bound.
    ///
    /// # Errors
    ///
    /// [`CdrError::BoundExceeded`] when there are more than `N` elements.
    pub fn from_vec(items: Vec<T>) -> CdrResult<Self> {
        check_bound(items.len(), N, "sequence<T, N>")?;
        Ok(Self { items })
    }

    /// Append an element.
    ///
    /// # Errors
    ///
    /// [`CdrError::BoundExceeded`] when the sequence is already full.
    pub fn push(&mut self, item: T) -> CdrResult<()> {
        check_bound(self.items.len() + 1, N, "sequence<T, N>")?;
        self.items.push(item);
        Ok(())
    }

    /// The elements.
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.items
    }

    /// Mutable access to the backing vector.
    ///
    /// The bound is **not** enforced here, which is why encoding re-checks
    /// it: a caller that grows the vector past `N` gets an error when the
    /// value is serialized, not silent truncation.
    pub fn as_mut_vec(&mut self) -> &mut Vec<T> {
        &mut self.items
    }

    /// Take ownership of the elements.
    #[must_use]
    pub fn into_vec(self) -> Vec<T> {
        self.items
    }

    /// Number of elements.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when there are no elements.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

impl<T, const N: usize> Deref for BoundedSequence<T, N> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        &self.items
    }
}

impl<T, const N: usize> TryFrom<Vec<T>> for BoundedSequence<T, N> {
    type Error = CdrError;

    fn try_from(items: Vec<T>) -> CdrResult<Self> {
        Self::from_vec(items)
    }
}

impl<T: CdrType, const N: usize> CdrType for BoundedSequence<T, N> {
    const MIN_SERIALIZED_SIZE: usize = 4;
}

impl<T: CdrSerialize, const N: usize> CdrSerialize for BoundedSequence<T, N> {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        check_bound(self.items.len(), N, "sequence<T, N>")?;
        crate::impls::collection::write_sequence(writer, &self.items)
    }
}

impl<'de, T: CdrDeserialize<'de>, const N: usize> CdrDeserialize<'de> for BoundedSequence<T, N> {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        // The bound is checked against the declared count before any element
        // is read, so an over-long sequence costs nothing to reject.
        let items = if reader.encoding().is_v2() && !T::IS_PRIMITIVE {
            reader.delimited(|inner| read_bounded_body::<T, N>(inner))?
        } else {
            read_bounded_body::<T, N>(reader)?
        };
        Ok(Self { items })
    }
}

fn read_bounded_body<'de, T: CdrDeserialize<'de>, const N: usize>(
    reader: &mut CdrReader<'de>,
) -> CdrResult<Vec<T>> {
    let count = reader.read_sequence_len(T::MIN_SERIALIZED_SIZE, "sequence<T, N>")?;
    check_bound(count, N, "sequence<T, N>")?;
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        items.push(reader.deserialize()?);
    }
    Ok(items)
}

impl<T, const N: usize> CdrDefault for BoundedSequence<T, N> {
    fn cdr_default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::encoding::{EncapsulationKind, Encoding};
    use crate::reader::{from_bytes, from_bytes_headerless};
    use crate::writer::{to_vec, to_vec_headerless};

    #[test]
    fn a_bounded_string_encodes_exactly_like_an_unbounded_one() {
        let bounded = BoundedString::<16>::new("map").expect("within bound");
        let plain = "map".to_owned();
        assert_eq!(
            to_vec_headerless(&bounded, Encoding::ROS2).expect("encode"),
            to_vec_headerless(&plain, Encoding::ROS2).expect("encode")
        );
    }

    #[test]
    fn construction_rejects_an_over_long_value() {
        assert_eq!(
            BoundedString::<3>::new("abcd"),
            Err(CdrError::BoundExceeded {
                bound: 3,
                actual: 4,
                context: "string<N>",
            })
        );
        assert_eq!(
            BoundedWString::<1>::new(WString::from("ab")),
            Err(CdrError::BoundExceeded {
                bound: 1,
                actual: 2,
                context: "wstring<N>",
            })
        );
        assert_eq!(
            BoundedSequence::<u8, 2>::from_vec(vec![1, 2, 3]),
            Err(CdrError::BoundExceeded {
                bound: 2,
                actual: 3,
                context: "sequence<T, N>",
            })
        );
    }

    #[test]
    fn decoding_rejects_a_peer_that_ignores_the_bound() {
        let long = to_vec(&"abcdefgh".to_owned(), Encoding::ROS2).expect("encode");
        assert_eq!(
            from_bytes::<BoundedString<4>>(&long),
            Err(CdrError::BoundExceeded {
                bound: 4,
                actual: 8,
                context: "string<N>",
            })
        );

        let many = to_vec(&vec![1_u32, 2, 3, 4], Encoding::ROS2).expect("encode");
        assert_eq!(
            from_bytes::<BoundedSequence<u32, 2>>(&many),
            Err(CdrError::BoundExceeded {
                bound: 2,
                actual: 4,
                context: "sequence<T, N>",
            })
        );
    }

    #[test]
    fn a_mutated_sequence_is_caught_at_encode_time() {
        let mut sequence = BoundedSequence::<u8, 2>::new();
        sequence.push(1).expect("room");
        sequence.push(2).expect("room");
        assert_eq!(
            sequence.push(3),
            Err(CdrError::BoundExceeded {
                bound: 2,
                actual: 3,
                context: "sequence<T, N>",
            })
        );
        // The escape hatch bypasses the check…
        sequence.as_mut_vec().push(3);
        assert_eq!(sequence.len(), 3);
        // …and encoding catches it.
        assert_eq!(
            to_vec_headerless(&sequence, Encoding::ROS2),
            Err(CdrError::BoundExceeded {
                bound: 2,
                actual: 3,
                context: "sequence<T, N>",
            })
        );
    }

    #[test]
    fn bounded_values_round_trip() {
        let text = BoundedString::<32>::new("base_link").expect("within bound");
        let bytes = to_vec(&text, Encoding::ROS2).expect("encode");
        assert_eq!(
            from_bytes::<BoundedString<32>>(&bytes).expect("decode"),
            text
        );

        let wide = BoundedWString::<8>::new(WString::from("hi")).expect("within bound");
        let bytes = to_vec(&wide, Encoding::ROS2).expect("encode");
        assert_eq!(
            from_bytes::<BoundedWString<8>>(&bytes).expect("decode"),
            wide
        );

        let numbers = BoundedSequence::<i32, 4>::from_vec(vec![1, -2, 3]).expect("within bound");
        for kind in [EncapsulationKind::CdrBe, EncapsulationKind::Cdr2Le] {
            let encoding = Encoding::new(kind);
            let bytes = to_vec(&numbers, encoding).expect("encode");
            assert_eq!(
                from_bytes::<BoundedSequence<i32, 4>>(&bytes).expect("decode"),
                numbers
            );
        }
    }

    #[test]
    fn accessors_and_conversions() {
        let text = BoundedString::<8>::try_from("odom").expect("within bound");
        assert_eq!(text.as_str(), "odom");
        assert_eq!(text.len(), 4);
        assert!(!text.is_empty());
        assert_eq!(&*text, "odom");
        assert_eq!(BoundedString::<8>::BOUND, 8);
        assert_eq!(text.clone().into_string(), "odom");
        assert!(BoundedString::<8>::cdr_default().is_empty());
        assert!(BoundedString::<4>::try_from("abcde".to_owned()).is_err());

        let wide = BoundedWString::<4>::new("ab").expect("within bound");
        assert_eq!(wide.as_wstring().units(), &[0x61, 0x62]);
        assert_eq!(wide.len(), 2);
        assert!(!wide.is_empty());
        assert_eq!(wide.clone().into_wstring(), WString::from("ab"));
        assert!(BoundedWString::<4>::cdr_default().is_empty());
        assert_eq!(BoundedWString::<4>::BOUND, 4);

        let sequence = BoundedSequence::<u8, 4>::try_from(vec![1, 2]).expect("within bound");
        assert_eq!(sequence.as_slice(), &[1, 2]);
        assert_eq!(&*sequence, &[1, 2]);
        assert_eq!(sequence.len(), 2);
        assert!(!sequence.is_empty());
        assert_eq!(sequence.clone().into_vec(), vec![1, 2]);
        assert!(BoundedSequence::<u8, 4>::cdr_default().is_empty());
        assert_eq!(BoundedSequence::<u8, 4>::BOUND, 4);
    }

    #[test]
    fn a_bounded_sequence_of_structs_still_gets_its_xcdr2_dheader() {
        let encoding = Encoding::new(EncapsulationKind::Cdr2Le);
        let sequence =
            BoundedSequence::<String, 4>::from_vec(vec!["a".to_owned()]).expect("within bound");
        let octets = to_vec_headerless(&sequence, encoding).expect("encode");
        // After the DHEADER: the count (4) + the string's length prefix (4) +
        // "a\0" (2) = 10 octets, which is what the DHEADER declares.
        assert_eq!(octets, [10, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, b'a', 0]);
        assert_eq!(
            from_bytes_headerless::<BoundedSequence<String, 4>>(&octets, encoding).expect("decode"),
            sequence
        );
    }
}
