//! The buffer layout every Arrow IPC record batch follows — the single source
//! of truth the writer emits against and the reader validates against.
//!
//! An Arrow `RecordBatch` message describes its body with two flat vectors:
//!
//! * `nodes: [FieldNode]` — one `(length, null_count)` pair per array in the
//!   schema, **depth-first, parents before children** ([`node_count`]).
//! * `buffers: [Buffer]` — one `(offset, length)` pair per physical buffer,
//!   in the same depth-first order ([`buffer_count`]).
//!
//! Neither vector is self-describing: a decoder can only split them correctly
//! if it derives the exact same counts from the schema that the encoder used.
//! Every framing bug in this format is ultimately a writer and a reader
//! disagreeing about how many buffers a type contributes, so both sides call
//! into this module rather than each carrying their own `match`.
//!
//! # The table
//!
//! | Type | Own buffers | Children |
//! |---|---|---|
//! | `Null` | — (none at all) | 0 |
//! | `Bool` | validity, values (bit-packed) | 0 |
//! | `Int*`/`UInt*`/`Float*`/`Timestamp`/`Duration` | validity, values | 0 |
//! | `Binary`/`Utf8` | validity, offsets (`i32`), values | 0 |
//! | `LargeBinary`/`LargeUtf8` | validity, offsets (`i64`), values | 0 |
//! | `FixedSizeBinary(w)` | validity, values | 0 |
//! | `List` | validity, offsets (`i32`) | 1 |
//! | `FixedSizeList` | validity | 1 |
//! | `Struct` | validity | n |
//!
//! `Null` is the one type with no buffers at all — not even a validity
//! bitmap — because every slot is null by construction (Arrow columnar
//! specification, `Null` layout).
//!
//! ```
//! use astrs_data::ipc::layout::{buffer_count, buffer_roles, node_count, BufferRole};
//! use astrs_data::{DataType, Field};
//!
//! let list = DataType::list(Field::new("item", DataType::Utf8, true));
//! // list(validity, offsets) + utf8(validity, offsets, values)
//! assert_eq!(buffer_count(&list), 5);
//! assert_eq!(node_count(&list), 2);
//! assert_eq!(buffer_roles(&DataType::Null), &[] as &[BufferRole]);
//! assert_eq!(buffer_roles(&DataType::Int32), &[BufferRole::Validity, BufferRole::Values]);
//! ```

use crate::datatype::DataType;
use crate::ipc::error::{IpcError, Result};

/// How deeply nested a type may be before the codec refuses it.
///
/// The schema decoder and both array codecs recurse, so a hostile stream that
/// declares ten thousand nested `List` fields would otherwise overflow the
/// stack. Sixty-four levels is far past anything a robotics payload uses (the
/// deepest `std` layout in blueprint §24.3 is three).
pub const MAX_NESTING_DEPTH: usize = 64;

/// What one `Buffer` entry of a record batch carries.
///
/// The role decides how the reader validates the entry's length against the
/// field node's row count, and it names the buffer in
/// [`IpcError::BufferTooShort`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BufferRole {
    /// The validity bitmap: one bit per slot, `1` meaning valid.
    ///
    /// May be written with length zero when the field node declares no nulls
    /// — the reader then treats every slot as valid.
    Validity,
    /// The offset buffer of a variable-length type: `len + 1` entries.
    Offsets,
    /// The value buffer: bit-packed for `Bool`, otherwise byte-packed.
    Values,
}

impl BufferRole {
    /// The name used in error messages.
    ///
    /// ```
    /// use astrs_data::ipc::layout::BufferRole;
    ///
    /// assert_eq!(BufferRole::Offsets.name(), "offsets");
    /// ```
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Validity => "validity",
            Self::Offsets => "offsets",
            Self::Values => "values",
        }
    }
}

/// The buffers a type contributes **itself**, excluding its children.
///
/// ```
/// use astrs_data::ipc::layout::{buffer_roles, BufferRole};
/// use astrs_data::{DataType, Field};
///
/// assert_eq!(
///     buffer_roles(&DataType::Utf8),
///     &[BufferRole::Validity, BufferRole::Offsets, BufferRole::Values]
/// );
/// assert_eq!(
///     buffer_roles(&DataType::strukt([Field::new("a", DataType::Int8, true)])),
///     &[BufferRole::Validity]
/// );
/// ```
#[must_use]
pub const fn buffer_roles(data_type: &DataType) -> &'static [BufferRole] {
    use BufferRole::{Offsets, Validity, Values};
    const NONE: &[BufferRole] = &[];
    const VALIDITY: &[BufferRole] = &[Validity];
    const VALIDITY_VALUES: &[BufferRole] = &[Validity, Values];
    const VALIDITY_OFFSETS: &[BufferRole] = &[Validity, Offsets];
    const VALIDITY_OFFSETS_VALUES: &[BufferRole] = &[Validity, Offsets, Values];

    match data_type {
        DataType::Null => NONE,
        DataType::Bool
        | DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::UInt64
        | DataType::Float16
        | DataType::Float32
        | DataType::Float64
        | DataType::Timestamp
        | DataType::Duration
        | DataType::FixedSizeBinary(_) => VALIDITY_VALUES,
        DataType::Binary | DataType::LargeBinary | DataType::Utf8 | DataType::LargeUtf8 => {
            VALIDITY_OFFSETS_VALUES
        }
        DataType::List(_) => VALIDITY_OFFSETS,
        DataType::FixedSizeList(_, _) | DataType::Struct(_) => VALIDITY,
    }
}

/// The child fields a type carries, in the order their nodes and buffers
/// appear in the message.
///
/// ```
/// use astrs_data::ipc::layout::children_of;
/// use astrs_data::{DataType, Field};
///
/// let point = DataType::strukt([
///     Field::new("x", DataType::Float64, false),
///     Field::new("y", DataType::Float64, false),
/// ]);
/// assert_eq!(children_of(&point).len(), 2);
/// assert!(children_of(&DataType::Int32).is_empty());
/// ```
#[must_use]
pub fn children_of(data_type: &DataType) -> Vec<&crate::datatype::Field> {
    data_type.children()
}

/// Number of `FieldNode` entries a type contributes: itself plus, depth first,
/// every descendant.
///
/// ```
/// use astrs_data::ipc::layout::node_count;
/// use astrs_data::{DataType, Field};
///
/// let nested = DataType::list(Field::new(
///     "item",
///     DataType::strukt([Field::new("a", DataType::Int32, true)]),
///     true,
/// ));
/// assert_eq!(node_count(&nested), 3);
/// ```
#[must_use]
pub fn node_count(data_type: &DataType) -> usize {
    1 + data_type
        .children()
        .into_iter()
        .map(|child| node_count(child.data_type()))
        .sum::<usize>()
}

/// Number of `Buffer` entries a type contributes, including its descendants.
///
/// ```
/// use astrs_data::ipc::layout::buffer_count;
/// use astrs_data::DataType;
///
/// assert_eq!(buffer_count(&DataType::Null), 0);
/// assert_eq!(buffer_count(&DataType::Bool), 2);
/// assert_eq!(buffer_count(&DataType::LargeUtf8), 3);
/// ```
#[must_use]
pub fn buffer_count(data_type: &DataType) -> usize {
    buffer_roles(data_type).len()
        + data_type
            .children()
            .into_iter()
            .map(|child| buffer_count(child.data_type()))
            .sum::<usize>()
}

/// The total node count of a whole schema, in field order.
#[must_use]
pub fn schema_node_count(fields: &[crate::datatype::Field]) -> usize {
    fields
        .iter()
        .map(|field| node_count(field.data_type()))
        .sum()
}

/// The total buffer count of a whole schema, in field order.
#[must_use]
pub fn schema_buffer_count(fields: &[crate::datatype::Field]) -> usize {
    fields
        .iter()
        .map(|field| buffer_count(field.data_type()))
        .sum()
}

/// How many levels of nesting a type has, counting itself as one.
///
/// ```
/// use astrs_data::ipc::layout::nesting_depth;
/// use astrs_data::{DataType, Field};
///
/// assert_eq!(nesting_depth(&DataType::Int32), 1);
/// assert_eq!(
///     nesting_depth(&DataType::list(Field::new("item", DataType::Int32, true))),
///     2
/// );
/// ```
#[must_use]
pub fn nesting_depth(data_type: &DataType) -> usize {
    1 + data_type
        .children()
        .into_iter()
        .map(|child| nesting_depth(child.data_type()))
        .max()
        .unwrap_or(0)
}

/// Rejects a type nested deeper than [`MAX_NESTING_DEPTH`].
///
/// # Errors
///
/// [`IpcError::NestingTooDeep`] when the type exceeds the limit.
///
/// ```
/// use astrs_data::ipc::layout::check_nesting_depth;
/// use astrs_data::DataType;
///
/// assert!(check_nesting_depth(&DataType::Float32).is_ok());
/// ```
pub fn check_nesting_depth(data_type: &DataType) -> Result<()> {
    let depth = nesting_depth(data_type);
    if depth > MAX_NESTING_DEPTH {
        return Err(IpcError::NestingTooDeep {
            depth,
            limit: MAX_NESTING_DEPTH,
        });
    }
    Ok(())
}

/// The byte width of one value of a fixed-width type, or `None` for the
/// bit-packed, variable-length and nested types.
///
/// `Bool` is `None` because a boolean value is one *bit*: see
/// [`values_byte_len`].
///
/// ```
/// use astrs_data::ipc::layout::value_byte_width;
/// use astrs_data::DataType;
///
/// assert_eq!(value_byte_width(&DataType::Int64), Some(8));
/// assert_eq!(value_byte_width(&DataType::FixedSizeBinary(7)), Some(7));
/// assert_eq!(value_byte_width(&DataType::Bool), None);
/// assert_eq!(value_byte_width(&DataType::Utf8), None);
/// ```
#[must_use]
pub fn value_byte_width(data_type: &DataType) -> Option<usize> {
    match data_type {
        DataType::FixedSizeBinary(width) => usize::try_from(*width).ok(),
        other => other.primitive_width(),
    }
}

/// The exact byte length the values buffer of `data_type` needs for `rows`
/// slots, or `None` when the length depends on the data (variable-length and
/// nested types).
///
/// ```
/// use astrs_data::ipc::layout::values_byte_len;
/// use astrs_data::DataType;
///
/// assert_eq!(values_byte_len(&DataType::Int32, 3), Some(12));
/// assert_eq!(values_byte_len(&DataType::Bool, 9), Some(2));
/// assert_eq!(values_byte_len(&DataType::Utf8, 3), None);
/// ```
#[must_use]
pub fn values_byte_len(data_type: &DataType, rows: usize) -> Option<usize> {
    match data_type {
        DataType::Bool => Some(rows.div_ceil(8)),
        other => value_byte_width(other).map(|width| width.saturating_mul(rows)),
    }
}

/// The byte width of one offset entry, or `None` for types without offsets.
///
/// ```
/// use astrs_data::ipc::layout::offset_byte_width;
/// use astrs_data::DataType;
///
/// assert_eq!(offset_byte_width(&DataType::Utf8), Some(4));
/// assert_eq!(offset_byte_width(&DataType::LargeBinary), Some(8));
/// assert_eq!(offset_byte_width(&DataType::Int8), None);
/// ```
#[must_use]
pub const fn offset_byte_width(data_type: &DataType) -> Option<usize> {
    match data_type {
        DataType::Binary | DataType::Utf8 => Some(4),
        DataType::LargeBinary | DataType::LargeUtf8 => Some(8),
        DataType::List(_) => Some(4),
        _ => None,
    }
}

/// The number of bytes a validity bitmap needs for `rows` slots.
///
/// ```
/// use astrs_data::ipc::layout::validity_byte_len;
///
/// assert_eq!(validity_byte_len(0), 0);
/// assert_eq!(validity_byte_len(1), 1);
/// assert_eq!(validity_byte_len(8), 1);
/// assert_eq!(validity_byte_len(9), 2);
/// ```
#[must_use]
pub const fn validity_byte_len(rows: usize) -> usize {
    rows.div_ceil(8)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::datatype::Field;

    fn every_leaf_type() -> Vec<DataType> {
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
            DataType::FixedSizeBinary(3),
            DataType::Timestamp,
            DataType::Duration,
        ]
    }

    #[test]
    fn leaf_types_have_no_children_and_one_node() {
        for data_type in every_leaf_type() {
            assert_eq!(node_count(&data_type), 1, "{data_type}");
            assert_eq!(nesting_depth(&data_type), 1, "{data_type}");
            assert_eq!(
                buffer_count(&data_type),
                buffer_roles(&data_type).len(),
                "{data_type}"
            );
        }
    }

    #[test]
    fn buffer_counts_match_the_specification_table() {
        assert_eq!(buffer_count(&DataType::Null), 0);
        assert_eq!(buffer_count(&DataType::Bool), 2);
        assert_eq!(buffer_count(&DataType::Int32), 2);
        assert_eq!(buffer_count(&DataType::FixedSizeBinary(4)), 2);
        assert_eq!(buffer_count(&DataType::Binary), 3);
        assert_eq!(buffer_count(&DataType::LargeUtf8), 3);
        let item = Field::new("item", DataType::Int8, true);
        assert_eq!(buffer_count(&DataType::list(item.clone())), 2 + 2);
        assert_eq!(
            buffer_count(&DataType::fixed_size_list(item.clone(), 2)),
            1 + 2
        );
        assert_eq!(buffer_count(&DataType::strukt([item])), 1 + 2);
    }

    #[test]
    fn nested_counts_add_up_depth_first() {
        // List<Struct<a: Int32, b: Utf8>>
        let inner = DataType::strukt([
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Utf8, true),
        ]);
        let list = DataType::list(Field::new("item", inner, true));
        assert_eq!(node_count(&list), 4, "list + struct + 2 children");
        assert_eq!(
            buffer_count(&list),
            2 + 1 + 2 + 3,
            "list(v,o) + struct(v) + int(v,v) + utf8(v,o,v)"
        );
        assert_eq!(nesting_depth(&list), 3);
    }

    #[test]
    fn struct_with_no_children_still_has_one_node_and_one_buffer() {
        let empty = DataType::strukt([]);
        assert_eq!(node_count(&empty), 1);
        assert_eq!(buffer_count(&empty), 1);
        assert!(children_of(&empty).is_empty());
    }

    #[test]
    fn schema_totals_sum_field_totals() {
        let fields = vec![
            Field::new("a", DataType::Null, true),
            Field::new("b", DataType::Utf8, true),
            Field::new(
                "c",
                DataType::fixed_size_list(Field::new("item", DataType::Float32, false), 3),
                false,
            ),
        ];
        assert_eq!(schema_node_count(&fields), 1 + 1 + 2);
        // `Null` contributes no buffer, `Utf8` three (validity, offsets,
        // values), the `FixedSizeList` one (validity) plus its `Float32`
        // child's two.
        let (null_buffers, utf8_buffers, list_buffers, child_buffers) = (0, 3, 1, 2);
        assert_eq!(
            schema_buffer_count(&fields),
            null_buffers + utf8_buffers + list_buffers + child_buffers
        );
    }

    #[test]
    fn value_and_offset_widths() {
        assert_eq!(value_byte_width(&DataType::Float16), Some(2));
        assert_eq!(value_byte_width(&DataType::Timestamp), Some(8));
        assert_eq!(value_byte_width(&DataType::Duration), Some(8));
        assert_eq!(value_byte_width(&DataType::FixedSizeBinary(-1)), None);
        assert_eq!(value_byte_width(&DataType::Utf8), None);
        assert_eq!(offset_byte_width(&DataType::Binary), Some(4));
        assert_eq!(
            offset_byte_width(&DataType::list(Field::new("item", DataType::Int8, true))),
            Some(4)
        );
        assert_eq!(offset_byte_width(&DataType::Bool), None);
        assert_eq!(values_byte_len(&DataType::Bool, 0), Some(0));
        assert_eq!(values_byte_len(&DataType::Float64, 5), Some(40));
        assert_eq!(values_byte_len(&DataType::FixedSizeBinary(3), 4), Some(12));
    }

    #[test]
    fn depth_limit_rejects_absurd_nesting() {
        let mut data_type = DataType::Int32;
        for _ in 0..MAX_NESTING_DEPTH {
            data_type = DataType::list(Field::new("item", data_type, true));
        }
        assert!(nesting_depth(&data_type) > MAX_NESTING_DEPTH);
        let err = check_nesting_depth(&data_type).unwrap_err();
        assert!(matches!(err, IpcError::NestingTooDeep { .. }), "{err}");
        assert!(err.is_malformed());
        assert!(check_nesting_depth(&DataType::Utf8).is_ok());
    }

    #[test]
    fn buffer_roles_are_named() {
        for role in [
            BufferRole::Validity,
            BufferRole::Offsets,
            BufferRole::Values,
        ] {
            assert!(!role.name().is_empty());
        }
        assert_eq!(BufferRole::Validity.name(), "validity");
        assert_eq!(BufferRole::Values.name(), "values");
    }

    #[test]
    fn validity_lengths_round_up() {
        for rows in 0..=64usize {
            assert_eq!(validity_byte_len(rows), rows.div_ceil(8));
        }
    }
}
