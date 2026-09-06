//! [`CdrSerde`](crate::CdrSerde) for the IDL primitive types.
//!
//! The mapping from IDL to Rust, as `astrs-idl` codegen emits it:
//!
//! | IDL | ROS 2 `.msg` | Rust | Octets | XCDR1 align | XCDR2 align |
//! |---|---|---|---:|---:|---:|
//! | `boolean` | `bool` | [`bool`] | 1 | 1 | 1 |
//! | `octet` | `byte`, `uint8` | [`u8`] | 1 | 1 | 1 |
//! | `int8` | `int8` | [`i8`] | 1 | 1 | 1 |
//! | `unsigned short` | `uint16` | [`u16`] | 2 | 2 | 2 |
//! | `short` | `int16` | [`i16`] | 2 | 2 | 2 |
//! | `unsigned long` | `uint32` | [`u32`] | 4 | 4 | 4 |
//! | `long` | `int32` | [`i32`] | 4 | 4 | 4 |
//! | `unsigned long long` | `uint64` | [`u64`] | 8 | 8 | 4 |
//! | `long long` | `int64` | [`i64`] | 8 | 8 | 4 |
//! | `float` | `float32` | [`f32`] | 4 | 4 | 4 |
//! | `double` | `float64` | [`f64`] | 8 | 8 | 4 |
//!
//! Every one of these reports [`CdrType::IS_PRIMITIVE`] as `true`, which is
//! what keeps a `sequence<long>` free of the DHEADER that XCDR2 puts in front
//! of a `sequence<SomeStruct>`.
//!
//! IDL `char` is a single octet and maps to [`u8`]; Rust's [`char`] is a
//! Unicode scalar value and is deliberately **not** implemented, because the
//! conversion would be lossy in one direction and surprising in the other.
//! IDL `wchar` is a UTF-16 code unit and travels as [`u16`] inside a
//! [`WString`](crate::WString).

use crate::error::CdrResult;
use crate::reader::CdrReader;
use crate::traits::{CdrDefault, CdrDeserialize, CdrSerialize, CdrType};
use crate::writer::CdrWriter;

macro_rules! impl_primitive {
    ($ty:ty, $octets:expr, $write:ident, $read:ident, $default:expr) => {
        impl CdrType for $ty {
            const IS_PRIMITIVE: bool = true;
            const MIN_SERIALIZED_SIZE: usize = $octets;
        }

        impl CdrSerialize for $ty {
            fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
                writer.$write(*self)
            }
        }

        impl<'de> CdrDeserialize<'de> for $ty {
            fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
                reader.$read()
            }
        }

        impl CdrDefault for $ty {
            fn cdr_default() -> Self {
                $default
            }
        }
    };
}

impl_primitive!(bool, 1, write_bool, read_bool, false);
impl_primitive!(u8, 1, write_u8, read_u8, 0);
impl_primitive!(i8, 1, write_i8, read_i8, 0);
impl_primitive!(u16, 2, write_u16, read_u16, 0);
impl_primitive!(i16, 2, write_i16, read_i16, 0);
impl_primitive!(u32, 4, write_u32, read_u32, 0);
impl_primitive!(i32, 4, write_i32, read_i32, 0);
impl_primitive!(u64, 8, write_u64, read_u64, 0);
impl_primitive!(i64, 8, write_i64, read_i64, 0);
impl_primitive!(f32, 4, write_f32, read_f32, 0.0);
impl_primitive!(f64, 8, write_f64, read_f64, 0.0);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::encoding::{EncapsulationKind, Encoding};
    use crate::reader::from_bytes;
    use crate::writer::{to_vec, to_vec_headerless};

    fn round_trip<T>(value: T, encoding: Encoding) -> T
    where
        T: CdrSerialize + for<'de> CdrDeserialize<'de> + core::fmt::Debug,
    {
        let bytes = to_vec(&value, encoding).expect("encode");
        from_bytes::<T>(&bytes).expect("decode")
    }

    #[test]
    fn every_primitive_round_trips_in_both_versions_and_byte_orders() {
        for kind in [
            EncapsulationKind::CdrLe,
            EncapsulationKind::CdrBe,
            EncapsulationKind::Cdr2Le,
            EncapsulationKind::Cdr2Be,
        ] {
            let encoding = Encoding::new(kind);
            assert!(round_trip(true, encoding));
            assert_eq!(round_trip(0xab_u8, encoding), 0xab);
            assert_eq!(round_trip(-5_i8, encoding), -5);
            assert_eq!(round_trip(0xabcd_u16, encoding), 0xabcd);
            assert_eq!(round_trip(-300_i16, encoding), -300);
            assert_eq!(round_trip(0xdead_beef_u32, encoding), 0xdead_beef);
            assert_eq!(round_trip(-70_000_i32, encoding), -70_000);
            assert_eq!(round_trip(u64::MAX, encoding), u64::MAX);
            assert_eq!(round_trip(i64::MIN, encoding), i64::MIN);
            assert_eq!(round_trip(1.5_f32, encoding), 1.5);
            assert_eq!(round_trip(-2.25_f64, encoding), -2.25);
        }
    }

    #[test]
    fn nan_payloads_survive_bit_for_bit() {
        let signalling = f64::from_bits(0x7ff0_0000_0000_0001);
        let bytes = to_vec(&signalling, Encoding::ROS2).expect("encode");
        let decoded = from_bytes::<f64>(&bytes).expect("decode");
        assert_eq!(decoded.to_bits(), signalling.to_bits());

        let quiet = f32::from_bits(0xffc0_0042);
        let bytes = to_vec(&quiet, Encoding::ROS2).expect("encode");
        assert_eq!(
            from_bytes::<f32>(&bytes).expect("decode").to_bits(),
            quiet.to_bits()
        );
    }

    #[test]
    fn primitives_declare_their_octet_widths() {
        assert_eq!(<bool as CdrType>::MIN_SERIALIZED_SIZE, 1);
        assert_eq!(<u16 as CdrType>::MIN_SERIALIZED_SIZE, 2);
        assert_eq!(<f32 as CdrType>::MIN_SERIALIZED_SIZE, 4);
        assert_eq!(<f64 as CdrType>::MIN_SERIALIZED_SIZE, 8);
        const { assert!(<u8 as CdrType>::IS_PRIMITIVE) };
        const { assert!(<i64 as CdrType>::IS_PRIMITIVE) };
    }

    #[test]
    fn idl_defaults_are_the_zero_values() {
        assert!(!bool::cdr_default());
        assert_eq!(u8::cdr_default(), 0);
        assert_eq!(i32::cdr_default(), 0);
        assert_eq!(f64::cdr_default(), 0.0);
    }

    #[test]
    fn big_endian_bodies_are_the_byte_reverse_of_little_endian_ones() {
        let le = to_vec_headerless(&0x0102_0304_u32, Encoding::new(EncapsulationKind::CdrLe))
            .expect("encode");
        let be = to_vec_headerless(&0x0102_0304_u32, Encoding::new(EncapsulationKind::CdrBe))
            .expect("encode");
        assert_eq!(le, [0x04, 0x03, 0x02, 0x01]);
        assert_eq!(be, [0x01, 0x02, 0x03, 0x04]);
    }
}
