//! IDL `sequence<T>` and `T[N]`.
//!
//! The two collections differ in exactly one thing — whether a length is
//! written — and that difference is the whole reason IDL distinguishes them:
//!
//! | IDL | ROS 2 `.msg` | Rust | Wire form |
//! |---|---|---|---|
//! | `sequence<T>` | `T[]` | [`Vec<T>`], `&[T]` | `unsigned long` count, then the elements |
//! | `T[N]` | `T[N]` | `[T; N]` | the elements, nothing else |
//!
//! # The XCDR2 DHEADER rule
//!
//! Under XCDR2 a collection whose **element type is not a primitive** is
//! preceded by a DHEADER giving the octet length of the whole collection
//! (OMG DDS-XTypes 1.3 §7.4.3.5.3). `sequence<long>` therefore has no
//! DHEADER, `sequence<Point>` does, and the decision is read off
//! [`CdrType::IS_PRIMITIVE`] of the element type — which is why that constant
//! is part of the trait rather than something the collection could work out
//! for itself.
//!
//! An enumerated type is **not** in the specification's list of primitive
//! types, so `sequence<SomeEnum>` gets a DHEADER here. That is the
//! conservative reading; a generated type may override `IS_PRIMITIVE` if a
//! peer is shown to disagree.
//!
//! Under XCDR1 no DHEADER is ever written, whatever the element type.
//!
//! # Hostile lengths
//!
//! A sequence length is checked against the octets that remain, scaled by the
//! element type's [`CdrType::MIN_SERIALIZED_SIZE`], **before** the `Vec` is
//! allocated. A declared count of `0xffff_ffff` costs one comparison.

use crate::error::{CdrError, CdrResult};
use crate::reader::CdrReader;
use crate::traits::{CdrDefault, CdrDeserialize, CdrSerialize, CdrType};
use crate::writer::CdrWriter;

/// Write `items` as an IDL `sequence<T>`: the count, then the elements,
/// wrapped in a DHEADER when XCDR2 requires one.
///
/// # Errors
///
/// [`CdrError::SequenceTooLong`] for a count above [`u32::MAX`], plus
/// whatever the elements return.
pub fn write_sequence<T: CdrSerialize>(writer: &mut CdrWriter, items: &[T]) -> CdrResult<()> {
    if writer.encoding().is_v2() && !T::IS_PRIMITIVE {
        writer.delimited(|inner| write_sequence_body(inner, items))
    } else {
        write_sequence_body(writer, items)
    }
}

fn write_sequence_body<T: CdrSerialize>(writer: &mut CdrWriter, items: &[T]) -> CdrResult<()> {
    writer.write_sequence_len(items.len())?;
    for item in items {
        item.serialize(writer)?;
    }
    Ok(())
}

/// Read an IDL `sequence<T>`.
///
/// # Errors
///
/// [`CdrError::LengthOverflow`] for a count the buffer cannot hold, plus
/// whatever the elements return.
pub fn read_sequence<'de, T: CdrDeserialize<'de>>(
    reader: &mut CdrReader<'de>,
) -> CdrResult<Vec<T>> {
    if reader.encoding().is_v2() && !T::IS_PRIMITIVE {
        reader.delimited(read_sequence_body::<T>)
    } else {
        read_sequence_body::<T>(reader)
    }
}

fn read_sequence_body<'de, T: CdrDeserialize<'de>>(
    reader: &mut CdrReader<'de>,
) -> CdrResult<Vec<T>> {
    let count = reader.read_sequence_len(T::MIN_SERIALIZED_SIZE, "sequence")?;
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        items.push(reader.deserialize()?);
    }
    Ok(items)
}

/// Write an IDL `T[N]` array: the elements, with no length prefix.
///
/// # Errors
///
/// Whatever the elements return.
pub fn write_array<T: CdrSerialize>(writer: &mut CdrWriter, items: &[T]) -> CdrResult<()> {
    if writer.encoding().is_v2() && !T::IS_PRIMITIVE {
        writer.delimited(|inner| write_array_body(inner, items))
    } else {
        write_array_body(writer, items)
    }
}

fn write_array_body<T: CdrSerialize>(writer: &mut CdrWriter, items: &[T]) -> CdrResult<()> {
    for item in items {
        item.serialize(writer)?;
    }
    Ok(())
}

/// Read an IDL `T[N]` array.
///
/// # Errors
///
/// [`CdrError::Truncated`] when the buffer is short, plus whatever the
/// elements return.
pub fn read_array<'de, T: CdrDeserialize<'de>, const N: usize>(
    reader: &mut CdrReader<'de>,
) -> CdrResult<[T; N]> {
    if reader.encoding().is_v2() && !T::IS_PRIMITIVE {
        reader.delimited(read_array_body::<T, N>)
    } else {
        read_array_body::<T, N>(reader)
    }
}

fn read_array_body<'de, T: CdrDeserialize<'de>, const N: usize>(
    reader: &mut CdrReader<'de>,
) -> CdrResult<[T; N]> {
    let mut items = Vec::with_capacity(N);
    for _ in 0..N {
        items.push(reader.deserialize::<T>()?);
    }
    // Cannot fail: exactly `N` elements were pushed.
    items
        .try_into()
        .map_err(|_| CdrError::SizeOverflow("collecting a fixed-size array"))
}

impl<T: CdrType> CdrType for [T] {
    // The four-octet count; an empty sequence is nothing more.
    const MIN_SERIALIZED_SIZE: usize = 4;
}

impl<T: CdrSerialize> CdrSerialize for [T] {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        write_sequence(writer, self)
    }
}

impl<T: CdrType> CdrType for Vec<T> {
    const MIN_SERIALIZED_SIZE: usize = 4;
}

impl<T: CdrSerialize> CdrSerialize for Vec<T> {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        write_sequence(writer, self)
    }
}

impl<'de, T: CdrDeserialize<'de>> CdrDeserialize<'de> for Vec<T> {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        read_sequence(reader)
    }
}

impl<T> CdrDefault for Vec<T> {
    fn cdr_default() -> Self {
        Self::new()
    }
}

/// `&'de [u8]` reads an IDL `sequence<octet>` without copying.
///
/// This is the accessor that keeps a `sensor_msgs/Image` payload out of the
/// allocator: the octets stay in the receive buffer.
impl<'de> CdrDeserialize<'de> for &'de [u8] {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        reader.read_octet_sequence()
    }
}

impl<T: CdrType, const N: usize> CdrType for [T; N] {
    const EXTENSIBILITY: crate::xcdr2::Extensibility = T::EXTENSIBILITY;
    // An array has no count of its own, so its floor is the product.
    const MIN_SERIALIZED_SIZE: usize = N.saturating_mul(T::MIN_SERIALIZED_SIZE);
}

impl<T: CdrSerialize, const N: usize> CdrSerialize for [T; N] {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        write_array(writer, self)
    }
}

impl<'de, T: CdrDeserialize<'de>, const N: usize> CdrDeserialize<'de> for [T; N] {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        read_array::<T, N>(reader)
    }
}

impl<T: CdrDefault, const N: usize> CdrDefault for [T; N] {
    fn cdr_default() -> Self {
        core::array::from_fn(|_| T::cdr_default())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::encoding::{EncapsulationKind, Encoding};
    use crate::reader::{from_bytes, from_bytes_headerless};
    use crate::writer::{to_vec, to_vec_headerless};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Pair {
        left: u8,
        right: u8,
    }

    impl CdrType for Pair {
        const MIN_SERIALIZED_SIZE: usize = 2;
    }

    impl CdrSerialize for Pair {
        fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
            writer.write_u8(self.left)?;
            writer.write_u8(self.right)
        }
    }

    impl<'de> CdrDeserialize<'de> for Pair {
        fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
            Ok(Self {
                left: reader.read_u8()?,
                right: reader.read_u8()?,
            })
        }
    }

    #[test]
    fn sequence_writes_a_count_then_the_elements() {
        let octets = to_vec_headerless(&vec![1_u32, 2, 3], Encoding::ROS2).expect("encode");
        assert_eq!(octets, [3, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]);
    }

    #[test]
    fn array_writes_no_count() {
        let octets = to_vec_headerless(&[1_u32, 2, 3], Encoding::ROS2).expect("encode");
        assert_eq!(octets, [1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]);
    }

    #[test]
    fn empty_sequence_is_four_zero_octets() {
        let octets = to_vec_headerless(&Vec::<u64>::new(), Encoding::ROS2).expect("encode");
        assert_eq!(octets, [0, 0, 0, 0]);
    }

    #[test]
    fn element_alignment_is_applied_between_elements() {
        // Three u16s after the count: the count leaves position 4, so the
        // elements pack with no padding.
        let octets = to_vec_headerless(&vec![1_u16, 2, 3], Encoding::ROS2).expect("encode");
        assert_eq!(octets, [3, 0, 0, 0, 1, 0, 2, 0, 3, 0]);

        // A sequence of doubles starts at position 4 and must pad to 8.
        let octets = to_vec_headerless(&vec![1.0_f64], Encoding::ROS2).expect("encode");
        assert_eq!(octets.len(), 4 + 4 + 8);
        assert_eq!(&octets[4..8], &[0, 0, 0, 0]);
    }

    #[test]
    fn sequences_and_arrays_round_trip() {
        for kind in [
            EncapsulationKind::CdrLe,
            EncapsulationKind::CdrBe,
            EncapsulationKind::Cdr2Le,
        ] {
            let encoding = Encoding::new(kind);
            let values = vec![1_i64, -2, 3];
            let bytes = to_vec(&values, encoding).expect("encode");
            assert_eq!(from_bytes::<Vec<i64>>(&bytes).expect("decode"), values);

            let array = [1.5_f32, 2.5, 3.5, 4.5];
            let bytes = to_vec(&array, encoding).expect("encode");
            assert_eq!(from_bytes::<[f32; 4]>(&bytes).expect("decode"), array);

            let nested = vec![vec![1_u8, 2], vec![], vec![3]];
            let bytes = to_vec(&nested, encoding).expect("encode");
            assert_eq!(from_bytes::<Vec<Vec<u8>>>(&bytes).expect("decode"), nested);
        }
    }

    #[test]
    fn xcdr2_adds_a_dheader_only_for_non_primitive_elements() {
        let encoding = Encoding::new(EncapsulationKind::Cdr2Le);
        // sequence<octet>: primitive element, no DHEADER.
        let primitive = to_vec_headerless(&vec![1_u8, 2], encoding).expect("encode");
        assert_eq!(primitive, [2, 0, 0, 0, 1, 2]);

        // sequence<Pair>: not primitive, so a DHEADER of 8 (the count plus
        // two two-octet pairs) comes first.
        let pairs = vec![Pair { left: 1, right: 2 }, Pair { left: 3, right: 4 }];
        let structured = to_vec_headerless(&pairs, encoding).expect("encode");
        assert_eq!(structured, [8, 0, 0, 0, 2, 0, 0, 0, 1, 2, 3, 4]);
        assert_eq!(
            from_bytes_headerless::<Vec<Pair>>(&structured, encoding).expect("decode"),
            pairs
        );
    }

    #[test]
    fn xcdr1_never_writes_a_dheader() {
        let pairs = vec![Pair { left: 1, right: 2 }];
        let octets = to_vec_headerless(&pairs, Encoding::ROS2).expect("encode");
        assert_eq!(octets, [1, 0, 0, 0, 1, 2]);
    }

    #[test]
    fn borrowed_octet_sequences_avoid_the_allocator() {
        let bytes = to_vec(&vec![9_u8, 8, 7], Encoding::ROS2).expect("encode");
        let borrowed: &[u8] = from_bytes(&bytes).expect("decode");
        assert_eq!(borrowed, &[9, 8, 7]);
        // The slice points into `bytes`, not into a copy.
        assert!(core::ptr::eq(borrowed.as_ptr(), bytes[8..].as_ptr()));
    }

    #[test]
    fn a_hostile_sequence_count_is_refused_before_allocation() {
        // Count 0xffff_ffff of eight-octet elements, with four octets left.
        let hostile = [0x00, 0x01, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 1, 2, 3, 4];
        assert_eq!(
            from_bytes::<Vec<f64>>(&hostile),
            Err(CdrError::LengthOverflow {
                declared: 0xffff_ffff,
                available: 4,
                element_size: 8,
                context: "sequence",
            })
        );
    }

    #[test]
    fn a_truncated_array_is_refused() {
        let short = [0x00, 0x01, 0x00, 0x00, 1, 0, 0, 0];
        assert!(from_bytes::<[u32; 4]>(&short).is_err());
    }

    #[test]
    fn slices_serialize_as_sequences() {
        let slice: &[u16] = &[1, 2];
        let octets = to_vec_headerless(&slice, Encoding::ROS2).expect("encode");
        assert_eq!(octets, [2, 0, 0, 0, 1, 0, 2, 0]);
    }

    #[test]
    fn min_serialized_sizes_are_lower_bounds() {
        assert_eq!(<Vec<f64> as CdrType>::MIN_SERIALIZED_SIZE, 4);
        assert_eq!(<[u8] as CdrType>::MIN_SERIALIZED_SIZE, 4);
        assert_eq!(<[f64; 3] as CdrType>::MIN_SERIALIZED_SIZE, 24);
        assert_eq!(<[Pair; 2] as CdrType>::MIN_SERIALIZED_SIZE, 4);
    }

    #[test]
    fn idl_defaults_are_empty_and_zeroed() {
        assert!(Vec::<u8>::cdr_default().is_empty());
        assert_eq!(<[i32; 3]>::cdr_default(), [0, 0, 0]);
    }

    #[test]
    fn multi_dimensional_arrays_nest() {
        let matrix = [[1.0_f64, 2.0, 3.0], [4.0, 5.0, 6.0], [7.0, 8.0, 9.0]];
        let bytes = to_vec(&matrix, Encoding::ROS2).expect("encode");
        // 4 header octets plus nine doubles, no length prefixes anywhere.
        assert_eq!(bytes.len(), 4 + 72);
        assert_eq!(from_bytes::<[[f64; 3]; 3]>(&bytes).expect("decode"), matrix);
    }
}
