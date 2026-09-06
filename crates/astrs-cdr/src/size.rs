//! Serialized-size computation without allocating the octets.
//!
//! RTPS needs to know how large a `SerializedPayload` will be before it
//! decides whether to fragment it, and a writer that has to serialize twice
//! to find out is a writer that serializes twice. The functions here run the
//! caller's own [`CdrSerialize`] impl against a counting sink, so the answer
//! is produced by the same code that produces the octets and cannot drift
//! from it.
//!
//! ```
//! use astrs_cdr::{serialized_size, Encoding};
//!
//! // Four header octets, a string of length 3 ("hi" + NUL) and its
//! // four-octet length prefix.
//! assert_eq!(serialized_size(&"hi".to_owned(), Encoding::ROS2)?, 4 + 4 + 3);
//! # Ok::<(), astrs_cdr::CdrError>(())
//! ```
//!
//! The count includes alignment padding, because the padding depends on the
//! stream position and the counting sink tracks that position exactly as the
//! buffered one does.

use crate::align::{ALIGN_4, padding_to};
use crate::encoding::Encoding;
use crate::error::CdrResult;
use crate::traits::CdrSerialize;
use crate::writer::CdrWriter;

/// Octets [`crate::to_vec`] would produce for `value`, encapsulation header
/// included.
///
/// # Errors
///
/// Whatever `value`'s [`CdrSerialize`] impl returns — a bound violation or an
/// interior NUL fails here exactly as it would when encoding.
pub fn serialized_size<T: CdrSerialize + ?Sized>(
    value: &T,
    encoding: Encoding,
) -> CdrResult<usize> {
    let mut writer = CdrWriter::measuring(encoding, true);
    writer.serialize(value)?;
    Ok(writer.written())
}

/// Octets [`crate::to_vec_headerless`] would produce for `value`.
///
/// # Errors
///
/// Whatever `value`'s [`CdrSerialize`] impl returns.
pub fn serialized_size_headerless<T: CdrSerialize + ?Sized>(
    value: &T,
    encoding: Encoding,
) -> CdrResult<usize> {
    let mut writer = CdrWriter::measuring(encoding, false);
    writer.serialize(value)?;
    Ok(writer.written())
}

/// Octets [`crate::to_vec_padded`] would produce for `value`: the size
/// rounded up so the body is a multiple of four.
///
/// # Errors
///
/// Whatever `value`'s [`CdrSerialize`] impl returns.
pub fn serialized_size_padded<T: CdrSerialize + ?Sized>(
    value: &T,
    encoding: Encoding,
) -> CdrResult<usize> {
    let mut writer = CdrWriter::measuring(encoding, true);
    writer.serialize(value)?;
    let pad = padding_to(writer.position(), ALIGN_4);
    Ok(writer.written() + pad)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::encoding::EncapsulationKind;
    use crate::error::CdrError;
    use crate::writer::{to_vec, to_vec_headerless, to_vec_padded};

    /// One shape to check: yields `(counted, encoded)` for an encoding.
    type SizeCase = Box<dyn Fn(Encoding) -> CdrResult<(usize, usize)>>;

    #[test]
    fn size_agrees_with_the_encoder_for_every_shape() {
        let cases: Vec<SizeCase> = vec![
            Box::new(|encoding| {
                Ok((
                    serialized_size(&1_u8, encoding)?,
                    to_vec(&1_u8, encoding)?.len(),
                ))
            }),
            Box::new(|encoding| {
                Ok((
                    serialized_size(&1.5_f64, encoding)?,
                    to_vec(&1.5_f64, encoding)?.len(),
                ))
            }),
            Box::new(|encoding| {
                let value = "a longer string".to_owned();
                Ok((
                    serialized_size(&value, encoding)?,
                    to_vec(&value, encoding)?.len(),
                ))
            }),
            Box::new(|encoding| {
                let value = vec![1_u64, 2, 3];
                Ok((
                    serialized_size(&value, encoding)?,
                    to_vec(&value, encoding)?.len(),
                ))
            }),
            Box::new(|encoding| {
                let value = vec![vec![1_u8], vec![2, 3]];
                Ok((
                    serialized_size(&value, encoding)?,
                    to_vec(&value, encoding)?.len(),
                ))
            }),
        ];

        for kind in [
            EncapsulationKind::CdrLe,
            EncapsulationKind::CdrBe,
            EncapsulationKind::Cdr2Le,
        ] {
            let encoding = Encoding::new(kind);
            for case in &cases {
                let (counted, encoded) = case(encoding).expect("size and encode agree");
                assert_eq!(counted, encoded, "{kind:?}");
            }
        }
    }

    #[test]
    fn headerless_size_excludes_the_four_header_octets() {
        let value = vec![1_u32, 2];
        let with = serialized_size(&value, Encoding::ROS2).expect("size");
        let without = serialized_size_headerless(&value, Encoding::ROS2).expect("size");
        assert_eq!(with - without, 4);
        assert_eq!(
            without,
            to_vec_headerless(&value, Encoding::ROS2)
                .expect("encode")
                .len()
        );
    }

    #[test]
    fn padded_size_rounds_the_body_up_to_four() {
        let value = 1_u8;
        assert_eq!(serialized_size(&value, Encoding::ROS2).expect("size"), 5);
        assert_eq!(
            serialized_size_padded(&value, Encoding::ROS2).expect("size"),
            8
        );
        assert_eq!(
            serialized_size_padded(&value, Encoding::ROS2).expect("size"),
            to_vec_padded(&value, Encoding::ROS2).expect("encode").len()
        );
    }

    #[test]
    fn alignment_padding_is_counted() {
        // u8 then f64: 1 octet, 7 pad, 8 octets under XCDR1.
        let mut writer = CdrWriter::measuring(Encoding::ROS2, true);
        writer.write_u8(1).expect("write");
        writer.write_f64(1.0).expect("write");
        assert_eq!(writer.written(), 4 + 16);
    }

    #[test]
    fn a_failing_value_fails_the_same_way_it_would_when_encoding() {
        let bad = "a\0b".to_owned();
        assert_eq!(
            serialized_size(&bad, Encoding::ROS2),
            Err(CdrError::InteriorNul { index: 1 })
        );
    }
}
