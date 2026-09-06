//! The `CdrSerde` trait pair that generated code implements.
//!
//! `astrs-idl` turns a `.msg`, `.srv` or `.action` file into a Rust struct
//! plus three small impls. The split into three traits is deliberate:
//!
//! - [`CdrType`] carries the *static description* of the type — its
//!   extensibility, whether it is an IDL primitive, and a floor on its
//!   serialized size. None of these need a value, so the reader can consult
//!   them before it has decoded anything.
//! - [`CdrSerialize`] writes a value. It takes `&self` and is implemented for
//!   unsized types (`str`, `[T]`) as well as owned ones.
//! - [`CdrDeserialize<'de>`] reads a value, and is generic over the lifetime
//!   of the input buffer so a generated type can hold `&'de str` and
//!   `&'de [u8]` instead of copying.
//!
//! [`CdrSerde`] is the blanket alias over the pair, for the common case of a
//! type that owns its data and therefore round-trips at any lifetime.
//!
//! # Writing an impl by hand
//!
//! ```
//! use astrs_cdr::{CdrDeserialize, CdrReader, CdrResult, CdrSerialize, CdrType, CdrWriter};
//!
//! /// `builtin_interfaces/msg/Time`.
//! #[derive(Debug, Clone, Copy, PartialEq, Eq)]
//! pub struct Time {
//!     pub sec: i32,
//!     pub nanosec: u32,
//! }
//!
//! impl CdrType for Time {
//!     // Two four-octet members, no padding possible between them.
//!     const MIN_SERIALIZED_SIZE: usize = 8;
//! }
//!
//! impl CdrSerialize for Time {
//!     fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
//!         writer.write_i32(self.sec)?;
//!         writer.write_u32(self.nanosec)
//!     }
//! }
//!
//! impl<'de> CdrDeserialize<'de> for Time {
//!     fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
//!         Ok(Self {
//!             sec: reader.read_i32()?,
//!             nanosec: reader.read_u32()?,
//!         })
//!     }
//! }
//!
//! let bytes = astrs_cdr::to_vec(&Time { sec: 1, nanosec: 2 }, Default::default())?;
//! assert_eq!(bytes, [0x00, 0x01, 0x00, 0x00, 1, 0, 0, 0, 2, 0, 0, 0]);
//! assert_eq!(astrs_cdr::from_bytes::<Time>(&bytes)?, Time { sec: 1, nanosec: 2 });
//! # Ok::<(), astrs_cdr::CdrError>(())
//! ```
//!
//! The [`cdr_struct!`](crate::cdr_struct) macro writes all three impls for a
//! plain struct, which is what the hand-written types in this crate's tests
//! use.

use crate::error::{CdrError, CdrResult};
use crate::reader::CdrReader;
use crate::writer::CdrWriter;
use crate::xcdr2::Extensibility;

/// Static description of an IDL type, independent of any value.
///
/// Every constant has a default, so a generated impl only states what differs
/// from "a final, non-primitive type of at least one octet".
pub trait CdrType {
    /// The type's XTypes extensibility (OMG DDS-XTypes 1.3 §7.2.4.4).
    ///
    /// Governs whether a DHEADER and per-member EMHEADERs appear under XCDR2.
    /// `rosidl`-generated ROS 2 types are all [`Extensibility::Final`], which
    /// is the default.
    const EXTENSIBILITY: Extensibility = Extensibility::Final;

    /// True when this is an IDL **primitive** type — `boolean`, `octet`,
    /// `char`, `wchar`, the sized integers, `float`, `double`.
    ///
    /// Under XCDR2 a sequence or array of non-primitive elements is preceded
    /// by a DHEADER (OMG DDS-XTypes 1.3 §7.4.3.5.3), so this constant is what
    /// decides whether `Vec<T>` emits one. It is `false` by default, the
    /// conservative choice: an enumerated type is not in the specification's
    /// list of primitive types, so a `sequence<MyEnum>` gets a DHEADER here.
    /// A generated type may override the constant if a peer proves otherwise.
    const IS_PRIMITIVE: bool = false;

    /// Lower bound on the octets one value of this type occupies.
    ///
    /// This is the fuzz-hardening lever: before a sequence of `n` elements is
    /// read, the reader checks `n * MIN_SERIALIZED_SIZE` against the octets
    /// that remain, so a hostile length can never reach an allocation. The
    /// bound must be a true lower bound — reporting more than a value can
    /// occupy rejects legal input.
    ///
    /// The default of `1` is the safe answer for any type. A type whose
    /// serialization can be empty should still report `1`: the reader clamps
    /// the multiplier to at least one so that "there cannot be more elements
    /// than there are octets left" always holds.
    const MIN_SERIALIZED_SIZE: usize = 1;
}

impl<T: CdrType + ?Sized> CdrType for &T {
    const EXTENSIBILITY: Extensibility = T::EXTENSIBILITY;
    const IS_PRIMITIVE: bool = T::IS_PRIMITIVE;
    const MIN_SERIALIZED_SIZE: usize = T::MIN_SERIALIZED_SIZE;
}

/// Write a value into a CDR stream.
///
/// Implementations write members in IDL declaration order and let the writer
/// handle alignment; they must not emit padding themselves.
pub trait CdrSerialize: CdrType {
    /// Serialize `self` at the writer's current position.
    ///
    /// # Errors
    ///
    /// Whatever the member writes fail with — most often
    /// [`CdrError::BoundExceeded`] for a bounded type or
    /// [`CdrError::InteriorNul`] for a string that is not a legal IDL
    /// `string`.
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()>;
}

impl<T: CdrSerialize + ?Sized> CdrSerialize for &T {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        (**self).serialize(writer)
    }
}

/// Read a value out of a CDR stream borrowed for `'de`.
///
/// The lifetime is what makes zero-copy reads possible: an implementation may
/// return `&'de str` or `&'de [u8]` pointing into the caller's buffer instead
/// of allocating. Owned types simply ignore it, which is why the blanket
/// [`CdrSerde`] alias quantifies over every `'de`.
pub trait CdrDeserialize<'de>: CdrType + Sized {
    /// Deserialize one value at the reader's current position.
    ///
    /// # Errors
    ///
    /// [`CdrError::Truncated`] when the stream ends early, and whatever
    /// content errors the members raise.
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self>;
}

/// The `CdrSerde` pair: a type that both writes and reads, at any input
/// lifetime.
///
/// This is a blanket alias, never implemented by hand — implement
/// [`CdrSerialize`] and [`CdrDeserialize`] and this follows. A borrowed type
/// such as `&'de str` deliberately does *not* satisfy it (it only reads at
/// one specific lifetime), which is why the two halves stay separate traits.
pub trait CdrSerde: CdrSerialize + for<'de> CdrDeserialize<'de> {}

impl<T> CdrSerde for T where T: CdrSerialize + for<'de> CdrDeserialize<'de> {}

/// The IDL default value of a type.
///
/// ROS 2 `.msg` files may give a field an explicit default; fields without
/// one take the IDL zero value — numeric `0`, `false`, the empty string, the
/// empty sequence, an array of defaults. `astrs-idl` emits `Default` impls in
/// terms of this trait so that "the IDL default" and "the Rust default" stay
/// distinguishable: `f64`'s Rust default is `0.0` and so is its IDL default,
/// but a bounded sequence's Rust default would have to invent a bound.
pub trait CdrDefault {
    /// The IDL default value.
    #[must_use]
    fn cdr_default() -> Self;
}

/// An IDL enumerated type.
///
/// CDR serializes an enumerated type as an unsigned integer of the width its
/// `@bit_bound` declares — 32 bits unless stated otherwise (OMG DDS-XTypes
/// 1.3 §7.3.1.2.1.5). Implement this trait and hand the type to
/// [`impl_cdr_enum!`](crate::impl_cdr_enum) to get the
/// [`CdrSerialize`]/[`CdrDeserialize`] pair for free.
///
/// ```
/// use astrs_cdr::{CdrEnum, CdrError, CdrResult, CdrType};
///
/// #[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// pub enum Shape {
///     Square = 0,
///     Circle = 1,
///     Triangle = 2,
/// }
///
/// impl CdrType for Shape {
///     const MIN_SERIALIZED_SIZE: usize = 4;
/// }
///
/// impl CdrEnum for Shape {
///     const TYPE_NAME: &'static str = "Shape";
///
///     fn discriminant(self) -> u32 {
///         self as u32
///     }
///
///     fn from_discriminant(value: u32) -> CdrResult<Self> {
///         match value {
///             0 => Ok(Self::Square),
///             1 => Ok(Self::Circle),
///             2 => Ok(Self::Triangle),
///             other => Err(CdrError::UnknownEnumerator {
///                 type_name: Self::TYPE_NAME,
///                 discriminant: other,
///             }),
///         }
///     }
/// }
///
/// astrs_cdr::impl_cdr_enum!(Shape);
///
/// let bytes = astrs_cdr::to_vec(&Shape::Circle, Default::default())?;
/// assert_eq!(bytes, [0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]);
/// # Ok::<(), astrs_cdr::CdrError>(())
/// ```
pub trait CdrEnum: CdrType + Copy + Sized {
    /// IDL name of the type, used in [`CdrError::UnknownEnumerator`].
    const TYPE_NAME: &'static str;

    /// The `@bit_bound`: 8, 16 or 32. Defaults to 32, the IDL default.
    const BIT_BOUND: u16 = 32;

    /// This enumerator's discriminant.
    #[must_use]
    fn discriminant(self) -> u32;

    /// The enumerator with the given discriminant.
    ///
    /// # Errors
    ///
    /// [`CdrError::UnknownEnumerator`] when no enumerator matches. Refusing
    /// unknown discriminants rather than mapping them to a catch-all is what
    /// makes an enum round trip total.
    fn from_discriminant(value: u32) -> CdrResult<Self>;

    /// Octets this enumerated type occupies on the wire.
    ///
    /// # Errors
    ///
    /// [`CdrError::BitBoundExceeded`] when `BIT_BOUND` is not 8, 16 or 32.
    fn wire_width() -> CdrResult<usize> {
        match Self::BIT_BOUND {
            8 => Ok(1),
            16 => Ok(2),
            32 => Ok(4),
            other => Err(CdrError::BitBoundExceeded {
                type_name: Self::TYPE_NAME,
                discriminant: 0,
                bit_bound: other,
            }),
        }
    }

    /// Check that `discriminant` fits the declared bit bound.
    ///
    /// # Errors
    ///
    /// [`CdrError::BitBoundExceeded`] when it does not.
    fn check_bit_bound(discriminant: u32) -> CdrResult<()> {
        let fits = match Self::BIT_BOUND {
            8 => discriminant <= u32::from(u8::MAX),
            16 => discriminant <= u32::from(u16::MAX),
            32 => true,
            _ => false,
        };
        if fits {
            Ok(())
        } else {
            Err(CdrError::BitBoundExceeded {
                type_name: Self::TYPE_NAME,
                discriminant,
                bit_bound: Self::BIT_BOUND,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Narrow {
        Zero,
        Big,
    }

    impl CdrType for Narrow {
        const MIN_SERIALIZED_SIZE: usize = 1;
    }

    impl CdrEnum for Narrow {
        const TYPE_NAME: &'static str = "Narrow";
        const BIT_BOUND: u16 = 8;

        fn discriminant(self) -> u32 {
            match self {
                Self::Zero => 0,
                Self::Big => 300,
            }
        }

        fn from_discriminant(value: u32) -> CdrResult<Self> {
            match value {
                0 => Ok(Self::Zero),
                300 => Ok(Self::Big),
                other => Err(CdrError::UnknownEnumerator {
                    type_name: Self::TYPE_NAME,
                    discriminant: other,
                }),
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Broken {
        Only,
    }

    impl CdrType for Broken {}

    impl CdrEnum for Broken {
        const TYPE_NAME: &'static str = "Broken";
        const BIT_BOUND: u16 = 24;

        fn discriminant(self) -> u32 {
            0
        }

        fn from_discriminant(_value: u32) -> CdrResult<Self> {
            Ok(Self::Only)
        }
    }

    #[test]
    fn cdr_type_defaults_describe_a_final_non_primitive() {
        struct Plain;
        impl CdrType for Plain {}
        assert_eq!(Plain::EXTENSIBILITY, Extensibility::Final);
        const { assert!(!Plain::IS_PRIMITIVE) };
        assert_eq!(Plain::MIN_SERIALIZED_SIZE, 1);
    }

    #[test]
    fn references_forward_every_constant() {
        struct Wide;
        impl CdrType for Wide {
            const EXTENSIBILITY: Extensibility = Extensibility::Appendable;
            const IS_PRIMITIVE: bool = true;
            const MIN_SERIALIZED_SIZE: usize = 16;
        }
        assert_eq!(<&Wide as CdrType>::EXTENSIBILITY, Extensibility::Appendable);
        const { assert!(<&Wide as CdrType>::IS_PRIMITIVE) };
        assert_eq!(<&Wide as CdrType>::MIN_SERIALIZED_SIZE, 16);
    }

    #[test]
    fn wire_width_follows_the_bit_bound() {
        assert_eq!(Narrow::wire_width(), Ok(1));
        assert_eq!(
            Broken::wire_width(),
            Err(CdrError::BitBoundExceeded {
                type_name: "Broken",
                discriminant: 0,
                bit_bound: 24,
            })
        );
    }

    #[test]
    fn bit_bound_check_rejects_a_discriminant_that_does_not_fit() {
        assert_eq!(Narrow::check_bit_bound(0), Ok(()));
        assert_eq!(Narrow::check_bit_bound(255), Ok(()));
        assert_eq!(
            Narrow::check_bit_bound(300),
            Err(CdrError::BitBoundExceeded {
                type_name: "Narrow",
                discriminant: 300,
                bit_bound: 8,
            })
        );
        assert_eq!(Narrow::Big.discriminant(), 300);
        assert_eq!(Narrow::from_discriminant(0), Ok(Narrow::Zero));
        assert_eq!(
            Narrow::from_discriminant(7),
            Err(CdrError::UnknownEnumerator {
                type_name: "Narrow",
                discriminant: 7,
            })
        );
    }
}
