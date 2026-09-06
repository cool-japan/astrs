//! Declarative macros that write the [`CdrSerde`](crate::CdrSerde) impls.
//!
//! `astrs-idl` emits these impls with `quote!`, spelling out every member; the
//! macros here do the same job for hand-written types — the golden vectors in
//! this crate's tests, the discovery structures in `astrs-rtps`, and anything
//! a user writes by hand. They exist as `macro_rules!` rather than a derive
//! so that this crate carries no proc-macro dependency.
//!
//! Three macros:
//!
//! - [`cdr_struct!`](crate::cdr_struct) declares a struct *and* implements
//!   the pair.
//! - [`cdr_struct_impls!`](crate::cdr_struct_impls) implements the pair for a
//!   struct that already exists.
//! - [`impl_cdr_enum!`](crate::impl_cdr_enum) implements the pair for a type
//!   that already implements [`CdrEnum`](crate::CdrEnum).
//!
//! # What they do not cover
//!
//! The struct macros handle **non-generic structs whose members are owned**.
//! A type that borrows from the input buffer — `struct Sample<'de> { topic:
//! &'de str }` — needs `impl<'de> CdrDeserialize<'de> for Sample<'de>`, whose
//! lifetime relationship a `macro_rules!` pattern cannot express. Write those
//! three impls by hand (the [`traits`](crate::traits) module has a worked
//! example) or, in generated code, emit them with `quote!`, which is what
//! `astrs-idl` does and why this limitation costs it nothing.
//!
//! # Extensibility
//!
//! Both struct macros accept an optional extensibility, which controls
//! whether a DHEADER wraps the members under XCDR2. Omitted, it is `Final` —
//! what every `rosidl`-generated ROS 2 type is.
//!
//! ```
//! astrs_cdr::cdr_struct! {
//!     /// An `@appendable` type: a newer sender may add members at the tail.
//!     #[derive(Debug, Clone, PartialEq)]
//!     pub struct Reading: Appendable {
//!         pub stamp: u64,
//!         pub value: f64,
//!     }
//! }
//!
//! use astrs_cdr::{CdrType, Extensibility};
//! assert_eq!(Reading::EXTENSIBILITY, Extensibility::Appendable);
//! ```

/// Implement [`CdrType`](crate::CdrType), [`CdrSerialize`](crate::CdrSerialize)
/// and [`CdrDeserialize`](crate::CdrDeserialize) for an existing struct.
///
/// Members are serialized in the order given, which must be IDL declaration
/// order. The generated [`CdrType::MIN_SERIALIZED_SIZE`](crate::CdrType::MIN_SERIALIZED_SIZE)
/// is the sum of the members', saturating rather than overflowing.
///
/// ```
/// #[derive(Debug, PartialEq)]
/// struct Header {
///     stamp_sec: i32,
///     stamp_nanosec: u32,
///     frame_id: String,
/// }
///
/// astrs_cdr::cdr_struct_impls!(Header {
///     stamp_sec: i32,
///     stamp_nanosec: u32,
///     frame_id: String,
/// });
///
/// let value = Header {
///     stamp_sec: 7,
///     stamp_nanosec: 8,
///     frame_id: "map".to_owned(),
/// };
/// let bytes = astrs_cdr::to_vec_ros2(&value)?;
/// assert_eq!(astrs_cdr::from_bytes::<Header>(&bytes)?, value);
/// # Ok::<(), astrs_cdr::CdrError>(())
/// ```
#[macro_export]
macro_rules! cdr_struct_impls {
    ($name:ident { $($field:ident : $ty:ty),* $(,)? }) => {
        $crate::cdr_struct_impls!($name : Final { $($field : $ty),* });
    };
    ($name:ident : $extensibility:ident { $($field:ident : $ty:ty),* $(,)? }) => {
        impl $crate::CdrType for $name {
            const EXTENSIBILITY: $crate::Extensibility =
                $crate::Extensibility::$extensibility;
            const MIN_SERIALIZED_SIZE: usize = 0_usize
                $(.saturating_add(<$ty as $crate::CdrType>::MIN_SERIALIZED_SIZE))*;
        }

        impl $crate::CdrSerialize for $name {
            fn serialize(&self, writer: &mut $crate::CdrWriter) -> $crate::CdrResult<()> {
                writer.write_struct::<Self, ()>(|writer| {
                    $(
                        $crate::CdrSerialize::serialize(&self.$field, writer)?;
                    )*
                    Ok(())
                })
            }
        }

        impl<'de> $crate::CdrDeserialize<'de> for $name {
            fn deserialize(
                reader: &mut $crate::CdrReader<'de>,
            ) -> $crate::CdrResult<Self> {
                reader.read_struct::<Self, Self>(|reader| {
                    Ok(Self {
                        $(
                            $field: <$ty as $crate::CdrDeserialize<'de>>::deserialize(reader)?,
                        )*
                    })
                })
            }
        }
    };
}

/// Declare a struct and implement the [`CdrSerde`](crate::CdrSerde) pair for
/// it.
///
/// Attributes, visibility and per-member visibility all pass through. An
/// optional `: Final` / `: Appendable` / `: Mutable` after the name sets the
/// XTypes extensibility.
///
/// ```
/// astrs_cdr::cdr_struct! {
///     /// `builtin_interfaces/msg/Duration`.
///     #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
///     pub struct Duration {
///         pub sec: i32,
///         pub nanosec: u32,
///     }
/// }
///
/// let bytes = astrs_cdr::to_vec_ros2(&Duration { sec: -1, nanosec: 5 })?;
/// assert_eq!(bytes.len(), 4 + 8);
/// # Ok::<(), astrs_cdr::CdrError>(())
/// ```
#[macro_export]
macro_rules! cdr_struct {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident {
            $($(#[$field_meta:meta])* $field_vis:vis $field:ident : $ty:ty),* $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis struct $name {
            $($(#[$field_meta])* $field_vis $field: $ty),*
        }

        $crate::cdr_struct_impls!($name { $($field : $ty),* });
    };
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident : $extensibility:ident {
            $($(#[$field_meta:meta])* $field_vis:vis $field:ident : $ty:ty),* $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis struct $name {
            $($(#[$field_meta])* $field_vis $field: $ty),*
        }

        $crate::cdr_struct_impls!($name : $extensibility { $($field : $ty),* });
    };
}

/// Implement [`CdrSerialize`](crate::CdrSerialize) and
/// [`CdrDeserialize`](crate::CdrDeserialize) for a type that already
/// implements [`CdrEnum`](crate::CdrEnum) and [`CdrType`](crate::CdrType).
///
/// The wire width follows the type's `BIT_BOUND`: one octet for 8, two for
/// 16, four for the IDL default of 32. Decoding refuses a discriminant no
/// enumerator claims, which is what keeps the round trip total.
///
/// See [`CdrEnum`](crate::CdrEnum) for a worked example.
#[macro_export]
macro_rules! impl_cdr_enum {
    ($name:ty) => {
        impl $crate::CdrSerialize for $name {
            fn serialize(&self, writer: &mut $crate::CdrWriter) -> $crate::CdrResult<()> {
                let discriminant = <$name as $crate::CdrEnum>::discriminant(*self);
                <$name as $crate::CdrEnum>::check_bit_bound(discriminant)?;
                match <$name as $crate::CdrEnum>::BIT_BOUND {
                    8 => {
                        let narrow = u8::try_from(discriminant).map_err(|_| {
                            $crate::CdrError::BitBoundExceeded {
                                type_name: <$name as $crate::CdrEnum>::TYPE_NAME,
                                discriminant,
                                bit_bound: 8,
                            }
                        })?;
                        writer.write_u8(narrow)
                    }
                    16 => {
                        let narrow = u16::try_from(discriminant).map_err(|_| {
                            $crate::CdrError::BitBoundExceeded {
                                type_name: <$name as $crate::CdrEnum>::TYPE_NAME,
                                discriminant,
                                bit_bound: 16,
                            }
                        })?;
                        writer.write_u16(narrow)
                    }
                    32 => writer.write_u32(discriminant),
                    other => Err($crate::CdrError::BitBoundExceeded {
                        type_name: <$name as $crate::CdrEnum>::TYPE_NAME,
                        discriminant,
                        bit_bound: other,
                    }),
                }
            }
        }

        impl<'de> $crate::CdrDeserialize<'de> for $name {
            fn deserialize(reader: &mut $crate::CdrReader<'de>) -> $crate::CdrResult<Self> {
                let discriminant = match <$name as $crate::CdrEnum>::BIT_BOUND {
                    8 => u32::from(reader.read_u8()?),
                    16 => u32::from(reader.read_u16()?),
                    32 => reader.read_u32()?,
                    other => {
                        return Err($crate::CdrError::BitBoundExceeded {
                            type_name: <$name as $crate::CdrEnum>::TYPE_NAME,
                            discriminant: 0,
                            bit_bound: other,
                        });
                    }
                };
                <$name as $crate::CdrEnum>::from_discriminant(discriminant)
            }
        }
    };
}

/// Implement [`CdrDefault`](crate::CdrDefault) for a struct as the IDL zero
/// value of every member.
///
/// Kept separate from [`cdr_struct_impls!`](crate::cdr_struct_impls) because
/// a struct may hold a borrowed member, which has no IDL default.
///
/// ```
/// astrs_cdr::cdr_struct! {
///     #[derive(Debug, PartialEq)]
///     pub struct Vector3 {
///         pub x: f64,
///         pub y: f64,
///         pub z: f64,
///     }
/// }
/// astrs_cdr::cdr_default_impl!(Vector3 { x, y, z });
///
/// use astrs_cdr::CdrDefault;
/// assert_eq!(Vector3::cdr_default(), Vector3 { x: 0.0, y: 0.0, z: 0.0 });
/// ```
#[macro_export]
macro_rules! cdr_default_impl {
    ($name:ident { $($field:ident),* $(,)? }) => {
        impl $crate::CdrDefault for $name {
            fn cdr_default() -> Self {
                Self {
                    $($field: $crate::CdrDefault::cdr_default(),)*
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use crate::encoding::{EncapsulationKind, Encoding};
    use crate::error::{CdrError, CdrResult};
    use crate::reader::{CdrReader, from_bytes, from_bytes_headerless};
    use crate::traits::{CdrDefault, CdrEnum, CdrType};
    use crate::writer::{to_vec, to_vec_headerless};
    use crate::xcdr2::Extensibility;

    crate::cdr_struct! {
        /// `builtin_interfaces/msg/Time`.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct Time {
            /// Whole seconds.
            pub sec: i32,
            /// Nanoseconds within the second.
            pub nanosec: u32,
        }
    }

    crate::cdr_default_impl!(Time { sec, nanosec });

    crate::cdr_struct! {
        /// A nested type with a member of a non-primitive kind.
        #[derive(Debug, Clone, PartialEq)]
        pub struct Stamped {
            /// When.
            pub stamp: Time,
            /// Where.
            pub frame_id: String,
        }
    }

    crate::cdr_struct! {
        /// An appendable type: XCDR2 wraps it in a DHEADER.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct Growing: Appendable {
            /// The one member today's readers know.
            pub first: u32,
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Level {
        Debug,
        Info,
        Error,
    }

    impl CdrType for Level {
        const MIN_SERIALIZED_SIZE: usize = 4;
    }

    impl CdrEnum for Level {
        const TYPE_NAME: &'static str = "Level";

        fn discriminant(self) -> u32 {
            match self {
                Self::Debug => 10,
                Self::Info => 20,
                Self::Error => 40,
            }
        }

        fn from_discriminant(value: u32) -> CdrResult<Self> {
            match value {
                10 => Ok(Self::Debug),
                20 => Ok(Self::Info),
                40 => Ok(Self::Error),
                other => Err(CdrError::UnknownEnumerator {
                    type_name: Self::TYPE_NAME,
                    discriminant: other,
                }),
            }
        }
    }

    crate::impl_cdr_enum!(Level);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Narrow {
        Off,
        On,
    }

    impl CdrType for Narrow {
        const MIN_SERIALIZED_SIZE: usize = 1;
    }

    impl CdrEnum for Narrow {
        const TYPE_NAME: &'static str = "Narrow";
        const BIT_BOUND: u16 = 8;

        fn discriminant(self) -> u32 {
            match self {
                Self::Off => 0,
                Self::On => 1,
            }
        }

        fn from_discriminant(value: u32) -> CdrResult<Self> {
            match value {
                0 => Ok(Self::Off),
                1 => Ok(Self::On),
                other => Err(CdrError::UnknownEnumerator {
                    type_name: Self::TYPE_NAME,
                    discriminant: other,
                }),
            }
        }
    }

    crate::impl_cdr_enum!(Narrow);

    #[test]
    fn generated_struct_impls_write_members_in_order() {
        let value = Time {
            sec: 0x0102_0304,
            nanosec: 0x0a0b_0c0d,
        };
        let octets = to_vec_headerless(&value, Encoding::ROS2).expect("encode");
        assert_eq!(octets, [0x04, 0x03, 0x02, 0x01, 0x0d, 0x0c, 0x0b, 0x0a]);
        assert_eq!(Time::MIN_SERIALIZED_SIZE, 8);
        assert_eq!(Time::EXTENSIBILITY, Extensibility::Final);
    }

    #[test]
    fn generated_struct_impls_round_trip_nested_members() {
        let value = Stamped {
            stamp: Time {
                sec: -1,
                nanosec: 2,
            },
            frame_id: "base_link".to_owned(),
        };
        for kind in [
            EncapsulationKind::CdrLe,
            EncapsulationKind::CdrBe,
            EncapsulationKind::Cdr2Le,
        ] {
            let bytes = to_vec(&value, Encoding::new(kind)).expect("encode");
            assert_eq!(from_bytes::<Stamped>(&bytes).expect("decode"), value);
        }
        // MIN_SERIALIZED_SIZE sums the members: 8 for Time, 5 for a string.
        assert_eq!(Stamped::MIN_SERIALIZED_SIZE, 13);
    }

    #[test]
    fn appendable_types_gain_a_dheader_under_xcdr2_only() {
        let value = Growing { first: 7 };
        assert_eq!(Growing::EXTENSIBILITY, Extensibility::Appendable);

        let v1 = to_vec_headerless(&value, Encoding::ROS2).expect("encode");
        assert_eq!(v1, [7, 0, 0, 0]);

        let encoding = Encoding::new(EncapsulationKind::Cdr2Le);
        let v2 = to_vec_headerless(&value, encoding).expect("encode");
        assert_eq!(v2, [4, 0, 0, 0, 7, 0, 0, 0]);
        assert_eq!(
            from_bytes_headerless::<Growing>(&v2, encoding).expect("decode"),
            value
        );
    }

    #[test]
    fn an_appendable_reader_skips_members_it_does_not_know() {
        // A newer sender appended a second u32; the DHEADER says 8.
        let octets = [8, 0, 0, 0, 7, 0, 0, 0, 9, 0, 0, 0];
        let encoding = Encoding::new(EncapsulationKind::Cdr2Le);
        let value =
            from_bytes_headerless::<Growing>(&octets, encoding).expect("forward compatible");
        assert_eq!(value, Growing { first: 7 });
    }

    #[test]
    fn enums_travel_as_their_discriminant() {
        let octets = to_vec_headerless(&Level::Info, Encoding::ROS2).expect("encode");
        assert_eq!(octets, [20, 0, 0, 0]);
        assert_eq!(
            from_bytes_headerless::<Level>(&octets, Encoding::ROS2).expect("decode"),
            Level::Info
        );

        let narrow = to_vec_headerless(&Narrow::On, Encoding::ROS2).expect("encode");
        assert_eq!(narrow, [1]);
        assert_eq!(
            from_bytes_headerless::<Narrow>(&narrow, Encoding::ROS2).expect("decode"),
            Narrow::On
        );
    }

    #[test]
    fn an_unknown_discriminant_is_refused() {
        let octets = [30_u8, 0, 0, 0];
        assert_eq!(
            from_bytes_headerless::<Level>(&octets, Encoding::ROS2),
            Err(CdrError::UnknownEnumerator {
                type_name: "Level",
                discriminant: 30,
            })
        );
    }

    #[test]
    fn generated_defaults_are_the_idl_zero_values() {
        assert_eq!(Time::cdr_default(), Time { sec: 0, nanosec: 0 });
    }

    #[test]
    fn a_reader_can_be_driven_by_hand_over_generated_types() {
        let bytes = to_vec(&Time { sec: 1, nanosec: 2 }, Encoding::ROS2).expect("encode");
        let mut reader = CdrReader::new(&bytes).expect("header");
        let value: Time = reader.deserialize().expect("decode");
        assert_eq!(value.sec, 1);
        assert_eq!(reader.finish(), Ok(()));
    }
}
