//! The type model: [`DataType`], [`Field`], [`Schema`], [`F16`] and the
//! [`ArrowNativeType`] bridge between Rust scalars and Arrow buffers.
//!
//! # The closed P0 set
//!
//! Blueprint §6.1 fixes the array types AstRS 0.1.0 supports. It is a *closed*
//! set chosen for robotics payloads — tensors, images, point clouds, scalar
//! channels and string maps — and deliberately excludes Arrow's dictionary,
//! union, map, decimal, interval and view types (P2). [`DataType`] models
//! exactly that set and nothing more, so an exhaustive `match` over it is a
//! complete Arrow encoder.
//!
//! Two of Arrow's parameterised types are pinned rather than parameterised:
//! `Timestamp` is always nanoseconds and timezone-less, and `Duration` is
//! always nanoseconds. AstRS timestamps come from the HLC in `astrs-time`,
//! which is nanosecond-resolution and UTC by construction, so the parameters
//! would only ever carry one value — and pinning them keeps the wire encoding
//! and the type-URN mapping unambiguous.
//!
//! ```
//! use astrs_data::{DataType, Field, Schema};
//!
//! let schema = Schema::new(vec![
//!     Field::new("stamp", DataType::Timestamp, false),
//!     Field::new("points", DataType::fixed_size_list(Field::new("xyz", DataType::Float32, false), 3), true),
//! ]);
//! assert_eq!(schema.len(), 2);
//! assert!(schema.field_by_name("points").is_some());
//! ```

pub mod f16;
pub mod field;
pub mod native;

use std::fmt;

pub use crate::datatype::f16::F16;
pub use crate::datatype::field::{Field, Schema};
pub use crate::datatype::native::ArrowNativeType;

/// The closed set of array types AstRS 0.1.0 encodes (blueprint §6.1).
///
/// `#[non_exhaustive]` because the append-only evolution rule (§3.4) applies
/// to the payload format too: 0.2 adds dictionary and map types at the tail.
///
/// ```
/// use astrs_data::DataType;
///
/// assert!(DataType::Int32.is_primitive());
/// assert_eq!(DataType::Int32.primitive_width(), Some(4));
/// assert_eq!(DataType::Utf8.to_string(), "Utf8");
/// assert!(DataType::list(astrs_data::Field::new("item", DataType::Utf8, true)).is_nested());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum DataType {
    /// All values are null; carries no buffers at all.
    Null,
    /// One bit per value, LSB-numbered like the validity bitmap.
    Bool,
    /// Signed 8-bit integer.
    Int8,
    /// Signed 16-bit integer.
    Int16,
    /// Signed 32-bit integer.
    Int32,
    /// Signed 64-bit integer.
    Int64,
    /// Unsigned 8-bit integer.
    UInt8,
    /// Unsigned 16-bit integer.
    UInt16,
    /// Unsigned 32-bit integer.
    UInt32,
    /// Unsigned 64-bit integer.
    UInt64,
    /// IEEE 754 binary16, stored as [`F16`].
    Float16,
    /// IEEE 754 binary32.
    Float32,
    /// IEEE 754 binary64.
    Float64,
    /// Variable-length bytes with 32-bit offsets.
    Binary,
    /// Variable-length bytes with 64-bit offsets.
    LargeBinary,
    /// Variable-length UTF-8 with 32-bit offsets.
    Utf8,
    /// Variable-length UTF-8 with 64-bit offsets.
    LargeUtf8,
    /// A fixed number of bytes per value.
    FixedSizeBinary(
        /// Bytes per value. Must be positive.
        i32,
    ),
    /// A fixed number of child values per value.
    FixedSizeList(
        /// The child field, describing one element.
        Box<Field>,
        /// Child values per parent value. Must be positive.
        i32,
    ),
    /// A variable number of child values per value, with 32-bit offsets.
    List(
        /// The child field, describing one element.
        Box<Field>,
    ),
    /// A fixed set of named child columns, all of the same length.
    Struct(
        /// The child fields, in column order.
        Vec<Field>,
    ),
    /// Nanoseconds since the Unix epoch, timezone-less (blueprint §6.1).
    Timestamp,
    /// A nanosecond count (blueprint §6.1).
    Duration,
}

impl DataType {
    /// Convenience constructor for [`DataType::List`].
    #[must_use]
    pub fn list(field: Field) -> Self {
        Self::List(Box::new(field))
    }

    /// Convenience constructor for [`DataType::FixedSizeList`].
    #[must_use]
    pub fn fixed_size_list(field: Field, size: i32) -> Self {
        Self::FixedSizeList(Box::new(field), size)
    }

    /// Convenience constructor for [`DataType::Struct`].
    #[must_use]
    pub fn strukt(fields: impl IntoIterator<Item = Field>) -> Self {
        Self::Struct(fields.into_iter().collect())
    }

    /// Returns `true` for the fixed-width scalar types that
    /// [`crate::array::PrimitiveArray`] covers.
    ///
    /// `Bool` is excluded: it is bit-packed, not byte-packed.
    #[must_use]
    pub const fn is_primitive(&self) -> bool {
        matches!(
            self,
            Self::Int8
                | Self::Int16
                | Self::Int32
                | Self::Int64
                | Self::UInt8
                | Self::UInt16
                | Self::UInt32
                | Self::UInt64
                | Self::Float16
                | Self::Float32
                | Self::Float64
                | Self::Timestamp
                | Self::Duration
        )
    }

    /// Returns `true` for the signed and unsigned integer types.
    #[must_use]
    pub const fn is_integer(&self) -> bool {
        matches!(
            self,
            Self::Int8
                | Self::Int16
                | Self::Int32
                | Self::Int64
                | Self::UInt8
                | Self::UInt16
                | Self::UInt32
                | Self::UInt64
        )
    }

    /// Returns `true` for the signed integer types.
    #[must_use]
    pub const fn is_signed_integer(&self) -> bool {
        matches!(self, Self::Int8 | Self::Int16 | Self::Int32 | Self::Int64)
    }

    /// Returns `true` for the floating point types.
    #[must_use]
    pub const fn is_float(&self) -> bool {
        matches!(self, Self::Float16 | Self::Float32 | Self::Float64)
    }

    /// Returns `true` for the two nanosecond temporal types.
    #[must_use]
    pub const fn is_temporal(&self) -> bool {
        matches!(self, Self::Timestamp | Self::Duration)
    }

    /// Returns `true` for the offset-plus-values byte and string types.
    #[must_use]
    pub const fn is_variable_length(&self) -> bool {
        matches!(
            self,
            Self::Binary | Self::LargeBinary | Self::Utf8 | Self::LargeUtf8
        )
    }

    /// Returns `true` for the two UTF-8 types.
    #[must_use]
    pub const fn is_string(&self) -> bool {
        matches!(self, Self::Utf8 | Self::LargeUtf8)
    }

    /// Returns `true` for types that use 64-bit offsets.
    #[must_use]
    pub const fn uses_large_offsets(&self) -> bool {
        matches!(self, Self::LargeBinary | Self::LargeUtf8)
    }

    /// Returns `true` for the types that own child arrays.
    #[must_use]
    pub const fn is_nested(&self) -> bool {
        matches!(
            self,
            Self::List(_) | Self::FixedSizeList(..) | Self::Struct(_)
        )
    }

    /// Byte width of one value, for the fixed-width types only.
    ///
    /// Returns `None` for `Null`, `Bool` (bit-packed), the variable-length
    /// types and the nested types.
    ///
    /// ```
    /// use astrs_data::DataType;
    ///
    /// assert_eq!(DataType::Int16.primitive_width(), Some(2));
    /// assert_eq!(DataType::Float16.primitive_width(), Some(2));
    /// assert_eq!(DataType::Timestamp.primitive_width(), Some(8));
    /// assert_eq!(DataType::FixedSizeBinary(7).primitive_width(), Some(7));
    /// assert_eq!(DataType::Bool.primitive_width(), None);
    /// assert_eq!(DataType::Utf8.primitive_width(), None);
    /// ```
    #[must_use]
    pub const fn primitive_width(&self) -> Option<usize> {
        Some(match self {
            Self::Int8 | Self::UInt8 => 1,
            Self::Int16 | Self::UInt16 | Self::Float16 => 2,
            Self::Int32 | Self::UInt32 | Self::Float32 => 4,
            Self::Int64 | Self::UInt64 | Self::Float64 | Self::Timestamp | Self::Duration => 8,
            Self::FixedSizeBinary(size) => {
                if *size < 0 {
                    return None;
                }
                *size as usize
            }
            _ => return None,
        })
    }

    /// The child fields this type owns, in column order.
    ///
    /// Empty for every non-nested type.
    ///
    /// ```
    /// use astrs_data::{DataType, Field};
    ///
    /// let list = DataType::list(Field::new("item", DataType::Int8, true));
    /// assert_eq!(list.children().len(), 1);
    /// assert_eq!(DataType::Int8.children().len(), 0);
    /// ```
    #[must_use]
    pub fn children(&self) -> Vec<&Field> {
        match self {
            Self::List(field) | Self::FixedSizeList(field, _) => vec![field.as_ref()],
            Self::Struct(fields) => fields.iter().collect(),
            _ => Vec::new(),
        }
    }

    /// The stable, human-readable name used in diagnostics and in the type
    /// URN registry.
    ///
    /// Parameterised types render their parameters, so the name round-trips
    /// through [`fmt::Display`].
    #[must_use]
    pub fn name(&self) -> String {
        self.to_string()
    }

    /// Returns `true` when the two types agree ignoring child field *names*
    /// and nullability — the check the graph type system wants when a producer
    /// and a consumer describe the same layout with different labels.
    ///
    /// ```
    /// use astrs_data::{DataType, Field};
    ///
    /// let a = DataType::list(Field::new("item", DataType::Int32, true));
    /// let b = DataType::list(Field::new("element", DataType::Int32, false));
    /// assert_ne!(a, b);
    /// assert!(a.layout_eq(&b));
    /// ```
    #[must_use]
    pub fn layout_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::List(a), Self::List(b)) => a.data_type().layout_eq(b.data_type()),
            (Self::FixedSizeList(a, sa), Self::FixedSizeList(b, sb)) => {
                sa == sb && a.data_type().layout_eq(b.data_type())
            }
            (Self::Struct(a), Self::Struct(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|(x, y)| x.data_type().layout_eq(y.data_type()))
            }
            _ => self == other,
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("Null"),
            Self::Bool => f.write_str("Bool"),
            Self::Int8 => f.write_str("Int8"),
            Self::Int16 => f.write_str("Int16"),
            Self::Int32 => f.write_str("Int32"),
            Self::Int64 => f.write_str("Int64"),
            Self::UInt8 => f.write_str("UInt8"),
            Self::UInt16 => f.write_str("UInt16"),
            Self::UInt32 => f.write_str("UInt32"),
            Self::UInt64 => f.write_str("UInt64"),
            Self::Float16 => f.write_str("Float16"),
            Self::Float32 => f.write_str("Float32"),
            Self::Float64 => f.write_str("Float64"),
            Self::Binary => f.write_str("Binary"),
            Self::LargeBinary => f.write_str("LargeBinary"),
            Self::Utf8 => f.write_str("Utf8"),
            Self::LargeUtf8 => f.write_str("LargeUtf8"),
            Self::FixedSizeBinary(size) => write!(f, "FixedSizeBinary({size})"),
            Self::FixedSizeList(field, size) => {
                write!(f, "FixedSizeList({}, {size})", field.data_type())
            }
            Self::List(field) => write!(f, "List({})", field.data_type()),
            Self::Struct(fields) => {
                f.write_str("Struct{")?;
                for (index, field) in fields.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}: {}", field.name(), field.data_type())?;
                }
                f.write_str("}")
            }
            Self::Timestamp => f.write_str("Timestamp(ns)"),
            Self::Duration => f.write_str("Duration(ns)"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn every_type() -> Vec<DataType> {
        vec![
            DataType::Null,
            DataType::Bool,
            DataType::Int8,
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::UInt8,
            DataType::UInt16,
            DataType::UInt32,
            DataType::UInt64,
            DataType::Float16,
            DataType::Float32,
            DataType::Float64,
            DataType::Binary,
            DataType::LargeBinary,
            DataType::Utf8,
            DataType::LargeUtf8,
            DataType::FixedSizeBinary(16),
            DataType::fixed_size_list(Field::new("xyz", DataType::Float32, false), 3),
            DataType::list(Field::new("item", DataType::Utf8, true)),
            DataType::strukt([
                Field::new("a", DataType::Int32, false),
                Field::new("b", DataType::Bool, true),
            ]),
            DataType::Timestamp,
            DataType::Duration,
        ]
    }

    #[test]
    fn the_closed_set_has_the_documented_size() {
        assert_eq!(every_type().len(), 23, "blueprint §6.1 P0 set");
    }

    #[test]
    fn predicates_partition_the_set() {
        for data_type in every_type() {
            let classes = u8::from(data_type.is_primitive())
                + u8::from(data_type.is_variable_length())
                + u8::from(data_type.is_nested())
                + u8::from(matches!(
                    data_type,
                    DataType::Null | DataType::Bool | DataType::FixedSizeBinary(_)
                ));
            assert_eq!(classes, 1, "{data_type} must belong to exactly one class");
        }
    }

    #[test]
    fn numeric_predicates() {
        assert!(DataType::Int8.is_integer() && DataType::Int8.is_signed_integer());
        assert!(DataType::UInt8.is_integer() && !DataType::UInt8.is_signed_integer());
        assert!(!DataType::Float32.is_integer());
        assert!(DataType::Float16.is_float());
        assert!(DataType::Timestamp.is_temporal() && DataType::Duration.is_temporal());
        assert!(!DataType::Int64.is_temporal());
        assert!(DataType::Utf8.is_string() && !DataType::Binary.is_string());
        assert!(DataType::LargeUtf8.uses_large_offsets());
        assert!(!DataType::Utf8.uses_large_offsets());
    }

    #[test]
    fn primitive_widths() {
        assert_eq!(DataType::Int8.primitive_width(), Some(1));
        assert_eq!(DataType::UInt16.primitive_width(), Some(2));
        assert_eq!(DataType::Float16.primitive_width(), Some(2));
        assert_eq!(DataType::Float32.primitive_width(), Some(4));
        assert_eq!(DataType::Float64.primitive_width(), Some(8));
        assert_eq!(DataType::Timestamp.primitive_width(), Some(8));
        assert_eq!(DataType::Duration.primitive_width(), Some(8));
        assert_eq!(DataType::FixedSizeBinary(0).primitive_width(), Some(0));
        assert_eq!(DataType::FixedSizeBinary(-1).primitive_width(), None);
        assert_eq!(DataType::Null.primitive_width(), None);
        assert_eq!(DataType::Bool.primitive_width(), None);
        assert_eq!(DataType::LargeUtf8.primitive_width(), None);
        assert_eq!(
            DataType::list(Field::new("i", DataType::Int8, true)).primitive_width(),
            None
        );
    }

    #[test]
    fn children_of_nested_types() {
        let list = DataType::list(Field::new("item", DataType::Int8, true));
        assert_eq!(list.children().len(), 1);
        assert_eq!(list.children()[0].name(), "item");

        let fsl = DataType::fixed_size_list(Field::new("e", DataType::Float64, false), 4);
        assert_eq!(fsl.children().len(), 1);

        let strukt = DataType::strukt([
            Field::new("a", DataType::Int8, true),
            Field::new("b", DataType::Int8, true),
        ]);
        assert_eq!(strukt.children().len(), 2);
        assert!(DataType::Int8.children().is_empty());
    }

    #[test]
    fn display_is_stable_and_readable() {
        assert_eq!(DataType::Null.to_string(), "Null");
        assert_eq!(DataType::Bool.to_string(), "Bool");
        assert_eq!(DataType::Timestamp.to_string(), "Timestamp(ns)");
        assert_eq!(DataType::Duration.to_string(), "Duration(ns)");
        assert_eq!(
            DataType::FixedSizeBinary(12).to_string(),
            "FixedSizeBinary(12)"
        );
        assert_eq!(
            DataType::fixed_size_list(Field::new("xyz", DataType::Float32, false), 3).to_string(),
            "FixedSizeList(Float32, 3)"
        );
        assert_eq!(
            DataType::list(Field::new("item", DataType::Utf8, true)).to_string(),
            "List(Utf8)"
        );
        assert_eq!(
            DataType::strukt([
                Field::new("a", DataType::Int32, false),
                Field::new("b", DataType::Bool, true),
            ])
            .to_string(),
            "Struct{a: Int32, b: Bool}"
        );
        assert_eq!(DataType::Int8.name(), "Int8");
    }

    #[test]
    fn every_type_has_a_distinct_display() {
        let mut seen = std::collections::BTreeSet::new();
        for data_type in every_type() {
            assert!(seen.insert(data_type.to_string()), "{data_type} repeated");
        }
    }

    #[test]
    fn layout_eq_ignores_child_names_and_nullability() {
        let a = DataType::list(Field::new("item", DataType::Int32, true));
        let b = DataType::list(Field::new("element", DataType::Int32, false));
        assert_ne!(a, b);
        assert!(a.layout_eq(&b));
        assert!(!a.layout_eq(&DataType::list(Field::new("i", DataType::Int64, true))));

        let s1 = DataType::strukt([Field::new("x", DataType::Float32, false)]);
        let s2 = DataType::strukt([Field::new("y", DataType::Float32, true)]);
        assert!(s1.layout_eq(&s2));
        assert!(!s1.layout_eq(&DataType::strukt([] as [Field; 0])));

        let f1 = DataType::fixed_size_list(Field::new("a", DataType::Int8, true), 3);
        let f2 = DataType::fixed_size_list(Field::new("b", DataType::Int8, false), 3);
        let f3 = DataType::fixed_size_list(Field::new("b", DataType::Int8, false), 4);
        assert!(f1.layout_eq(&f2));
        assert!(!f1.layout_eq(&f3));
        assert!(DataType::Int8.layout_eq(&DataType::Int8));
        assert!(!DataType::Int8.layout_eq(&DataType::Int16));
    }

    #[test]
    fn serde_round_trips_every_type() {
        for data_type in every_type() {
            let json = serde_json::to_string(&data_type).unwrap();
            let back: DataType = serde_json::from_str(&json).unwrap();
            assert_eq!(back, data_type, "{json}");
        }
    }

    #[test]
    fn deeply_nested_types_work() {
        let inner = DataType::strukt([
            Field::new("stamp", DataType::Timestamp, false),
            Field::new(
                "points",
                DataType::fixed_size_list(Field::new("xyz", DataType::Float32, false), 3),
                true,
            ),
        ]);
        let outer = DataType::list(Field::new("frames", inner.clone(), true));
        assert!(outer.is_nested());
        assert_eq!(outer.children().len(), 1);
        assert_eq!(outer.children()[0].data_type(), &inner);
        let json = serde_json::to_string(&outer).unwrap();
        assert_eq!(serde_json::from_str::<DataType>(&json).unwrap(), outer);
    }
}
