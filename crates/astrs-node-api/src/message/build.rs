//! Column constructors shared by every [`AstrsMessage`](super::AstrsMessage)
//! implementation.
//!
//! The std layouts of §24.3 are built from four shapes — a primitive column, a
//! `List` of a primitive, a `FixedSizeList` of a primitive, and a `Struct`
//! composing those — so four constructors cover all of them. Writing the
//! implementations against these rather than against the array types directly
//! keeps every one of them short enough to read against the normative layout
//! it is supposed to produce.
//!
//! Everything here builds **non-nullable** columns. The std types are total:
//! a `Pose` has a position, and a null one would be a different type
//! (`Option<Pose>` is expressed as an absent message, not a null row). Nothing
//! in this module can therefore produce a validity bitmap, which is also why
//! [`astrs_data::RecordBatch::from_payload`] marks the field non-nullable for
//! every payload built here.
//!
//! # Examples
//!
//! ```
//! use astrs_data::{DataType, Field};
//! use astrs_node_api::message::build;
//!
//! let column = build::primitive::<f64>(&[1.0, 2.0, 3.0]);
//! assert_eq!(column.len(), 3);
//!
//! let lists = build::primitive_lists::<f32>("range", &[&[1.0, 2.0][..], &[3.0][..]])?;
//! assert_eq!(lists.len(), 2);
//! assert_eq!(
//!     lists.data_type(),
//!     &DataType::list(Field::required("range", DataType::Float32))
//! );
//! # Ok::<(), astrs_data::DataError>(())
//! ```

use astrs_data::array::{
    ArrayRef, BinaryArray, BooleanArray, FixedSizeListArray, IntoArrayRef, ListArray,
    PrimitiveArray, StringArray, StructArray, TimestampArray,
};
use astrs_data::buffer::ScalarBuffer;
use astrs_data::{ArrowNativeType, DataError, DataType, Field, Result};

/// A non-nullable primitive column.
///
/// # Examples
///
/// ```
/// use astrs_node_api::message::build;
///
/// assert_eq!(build::primitive::<u32>(&[1, 2]).len(), 2);
/// ```
#[must_use]
pub fn primitive<T: ArrowNativeType>(values: &[T]) -> ArrayRef {
    PrimitiveArray::<T>::try_new(ScalarBuffer::from_slice(values), None)
        .map_or_else(|_| empty_primitive::<T>(), IntoArrayRef::into_array_ref)
}

/// An empty primitive column, used as the total fallback above.
///
/// `PrimitiveArray::try_new` with no validity bitmap cannot fail, so this is
/// unreachable; it exists so [`primitive`] is infallible at its call sites
/// (the layouts it feeds are all fixed, and a `Result` there would propagate
/// an error that cannot happen through every message implementation).
fn empty_primitive<T: ArrowNativeType>() -> ArrayRef {
    PrimitiveArray::<T>::from_values(core::iter::empty()).into_array_ref()
}

/// A non-nullable boolean column.
#[must_use]
pub fn boolean(values: &[bool]) -> ArrayRef {
    BooleanArray::from_values(values.iter().copied()).into_array_ref()
}

/// A non-nullable `Utf8` column.
#[must_use]
pub fn strings<S: AsRef<str>>(values: &[S]) -> ArrayRef {
    StringArray::from_values(values.iter().map(AsRef::as_ref)).into_array_ref()
}

/// A non-nullable `Binary` column.
#[must_use]
pub fn binaries<S: AsRef<[u8]>>(values: &[S]) -> ArrayRef {
    BinaryArray::from_values(values.iter().map(AsRef::as_ref)).into_array_ref()
}

/// A non-nullable `Timestamp(ns)` column.
#[must_use]
pub fn timestamps(nanos: &[i64]) -> ArrayRef {
    TimestampArray::from_nanos(nanos.iter().copied()).into_array_ref()
}

/// A `List<item_name: T>` column, one list per entry of `rows`.
///
/// # Errors
///
/// [`DataError`] if the offsets cannot be laid out, which
/// requires a total element count beyond `i32::MAX`.
///
/// # Examples
///
/// ```
/// use astrs_node_api::message::build;
///
/// let column = build::primitive_lists::<i8>("cell", &[&[1, 2, 3][..], &[][..]])?;
/// assert_eq!(column.len(), 2);
/// # Ok::<(), astrs_data::DataError>(())
/// ```
pub fn primitive_lists<T: ArrowNativeType>(item_name: &str, rows: &[&[T]]) -> Result<ArrayRef> {
    let total: usize = rows.iter().map(|row| row.len()).sum();
    let mut flat = Vec::with_capacity(total);
    let mut offsets = Vec::with_capacity(rows.len() + 1);
    let mut cursor = 0i32;
    offsets.push(cursor);
    for row in rows {
        flat.extend_from_slice(row);
        cursor = cursor.saturating_add(i32::try_from(row.len()).unwrap_or(i32::MAX));
        offsets.push(cursor);
    }
    let field = Field::required(item_name, T::DATA_TYPE);
    let values = primitive::<T>(&flat);
    Ok(
        ListArray::try_new(field, ScalarBuffer::from_slice(&offsets), values, None)?
            .into_array_ref(),
    )
}

/// A `List<FixedSizeList<item_name: T, width>>` column.
///
/// `rows` holds, per row, the flattened tuples of that row: a row of two
/// four-wide boxes is eight values. This is the shape `Detections.boxes` and
/// `Keypoints.points` need.
///
/// # Errors
///
/// [`DataError`] when a row's length is not a multiple of
/// `width`, or when the offsets cannot be laid out.
pub fn fixed_tuple_lists<T: ArrowNativeType>(
    outer_name: &str,
    item_name: &str,
    width: i32,
    rows: &[&[T]],
) -> Result<ArrayRef> {
    let stride = usize::try_from(width.max(1)).unwrap_or(1);
    let total: usize = rows.iter().map(|row| row.len()).sum();
    let mut flat = Vec::with_capacity(total);
    let mut offsets = Vec::with_capacity(rows.len() + 1);
    let mut cursor = 0i32;
    offsets.push(cursor);
    for row in rows {
        if !row.len().is_multiple_of(stride) {
            return Err(DataError::ChildLengthMismatch {
                expected: row.len().next_multiple_of(stride),
                actual: row.len(),
            });
        }
        flat.extend_from_slice(row);
        let tuples = row.len() / stride;
        cursor = cursor.saturating_add(i32::try_from(tuples).unwrap_or(i32::MAX));
        offsets.push(cursor);
    }
    let inner = FixedSizeListArray::try_new(
        Field::required(item_name, T::DATA_TYPE),
        width,
        primitive::<T>(&flat),
        None,
    )?;
    let outer_field = Field::required(
        outer_name,
        DataType::fixed_size_list(Field::required(item_name, T::DATA_TYPE), width),
    );
    Ok(ListArray::try_new(
        outer_field,
        ScalarBuffer::from_slice(&offsets),
        inner.into_array_ref(),
        None,
    )?
    .into_array_ref())
}

/// A `FixedSizeList<item_name: T, width>` column, one tuple per row.
///
/// `flat` holds `rows * width` values in row-major order — the shape every
/// covariance matrix in §24.3 uses.
///
/// # Errors
///
/// [`DataError`] when `flat.len()` is not a multiple of `width`.
pub fn fixed_tuples<T: ArrowNativeType>(
    item_name: &str,
    width: i32,
    flat: &[T],
) -> Result<ArrayRef> {
    Ok(FixedSizeListArray::try_new(
        Field::required(item_name, T::DATA_TYPE),
        width,
        primitive::<T>(flat),
        None,
    )?
    .into_array_ref())
}

/// A non-nullable `Struct` column from named child columns.
///
/// The field types are taken from the columns themselves, so a caller cannot
/// declare one shape and pass another.
///
/// # Errors
///
/// [`DataError`] when the children disagree on length, or when
/// there are no children at all (a struct's row count would be unknowable).
///
/// # Examples
///
/// ```
/// use astrs_node_api::message::build;
///
/// let column = build::structure(vec![
///     ("x", build::primitive::<f64>(&[1.0])),
///     ("y", build::primitive::<f64>(&[2.0])),
/// ])?;
/// assert_eq!(column.len(), 1);
/// # Ok::<(), astrs_data::DataError>(())
/// ```
pub fn structure(children: Vec<(&str, ArrayRef)>) -> Result<ArrayRef> {
    let mut fields = Vec::with_capacity(children.len());
    let mut columns = Vec::with_capacity(children.len());
    for (name, column) in children {
        fields.push(Field::required(name, column.data_type().clone()));
        columns.push(column);
    }
    Ok(StructArray::try_new(fields, columns, None)?.into_array_ref())
}

/// A `List<item_name: item_type>` column over already-built per-row children.
///
/// Used where the list's element is itself a struct — `Path`'s stamped poses
/// — so the child column is built once for every row concatenated and the
/// offsets slice it back apart.
///
/// # Errors
///
/// [`DataError`] when the lengths do not sum to the child's
/// length, or when the child's type differs from `item_type`.
pub fn nested_lists(
    item_name: &str,
    item_type: DataType,
    lengths: &[usize],
    values: ArrayRef,
) -> Result<ArrayRef> {
    let field = Field::required(item_name, item_type);
    Ok(ListArray::try_from_lengths(field, lengths.iter().copied(), values)?.into_array_ref())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_data::array::{ArrayExt, Float32Array, Float64Array};

    #[test]
    fn primitive_columns_carry_their_values_and_type() {
        let column = primitive::<f64>(&[1.0, 2.0]);
        assert_eq!(column.data_type(), &DataType::Float64);
        let values = column.downcast::<Float64Array>().unwrap();
        assert_eq!(values.values(), &[1.0, 2.0]);
        assert_eq!(column.null_count(), 0);
        assert_eq!(primitive::<u8>(&[]).len(), 0);
    }

    #[test]
    fn boolean_string_binary_and_timestamp_columns() {
        assert_eq!(boolean(&[true, false]).len(), 2);
        assert_eq!(boolean(&[]).data_type(), &DataType::Bool);
        assert_eq!(strings(&["a", "bb"]).len(), 2);
        assert_eq!(strings::<&str>(&[]).data_type(), &DataType::Utf8);
        assert_eq!(binaries(&[&b"x"[..]]).data_type(), &DataType::Binary);
        assert_eq!(timestamps(&[1, 2, 3]).data_type(), &DataType::Timestamp);
    }

    #[test]
    fn primitive_lists_slice_their_rows_apart() {
        let column =
            primitive_lists::<f32>("range", &[&[1.0, 2.0][..], &[][..], &[3.0][..]]).unwrap();
        assert_eq!(column.len(), 3);
        let lists = column.downcast::<ListArray>().unwrap();
        assert_eq!(lists.value_length(0), Some(2));
        assert_eq!(lists.value_length(1), Some(0));
        assert_eq!(lists.value_length(2), Some(1));
        let first = lists.value(0).unwrap();
        assert_eq!(
            first.downcast::<Float32Array>().unwrap().values(),
            &[1.0, 2.0]
        );
    }

    #[test]
    fn fixed_tuple_lists_group_by_width() {
        let column = fixed_tuple_lists::<f32>(
            "xywh",
            "v",
            4,
            &[&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0][..]],
        )
        .unwrap();
        assert_eq!(column.len(), 1, "one row");
        let lists = column.downcast::<ListArray>().unwrap();
        assert_eq!(lists.value_length(0), Some(2), "two boxes in the row");
    }

    #[test]
    fn a_row_that_is_not_a_whole_number_of_tuples_is_refused() {
        let error = fixed_tuple_lists::<f32>("xywh", "v", 4, &[&[0.0, 1.0][..]]).unwrap_err();
        assert!(
            matches!(error, DataError::ChildLengthMismatch { .. }),
            "{error}"
        );
    }

    #[test]
    fn fixed_tuples_lay_out_covariances() {
        let flat: Vec<f64> = (0..18).map(f64::from).collect();
        let column = fixed_tuples::<f64>("v", 9, &flat).unwrap();
        assert_eq!(column.len(), 2, "two rows of nine");
        assert_eq!(
            column.data_type(),
            &DataType::fixed_size_list(Field::required("v", DataType::Float64), 9)
        );
        assert!(fixed_tuples::<f64>("v", 9, &flat[..10]).is_err());
    }

    #[test]
    fn structures_take_their_types_from_their_columns() {
        let column = structure(vec![
            ("x", primitive::<f64>(&[1.0, 4.0])),
            ("y", primitive::<f64>(&[2.0, 5.0])),
            ("z", primitive::<f64>(&[3.0, 6.0])),
        ])
        .unwrap();
        assert_eq!(column.len(), 2);
        assert_eq!(
            column.data_type(),
            &DataType::strukt([
                Field::required("x", DataType::Float64),
                Field::required("y", DataType::Float64),
                Field::required("z", DataType::Float64),
            ])
        );
    }

    #[test]
    fn a_structure_with_mismatched_children_is_refused() {
        let error = structure(vec![
            ("x", primitive::<f64>(&[1.0, 2.0])),
            ("y", primitive::<f64>(&[3.0])),
        ])
        .unwrap_err();
        assert!(!error.to_string().is_empty());
        assert!(structure(Vec::new()).is_err(), "no children, no row count");
    }

    #[test]
    fn nested_lists_slice_a_shared_child() {
        let child = structure(vec![
            ("stamp", timestamps(&[1, 2, 3])),
            ("value", primitive::<f64>(&[1.0, 2.0, 3.0])),
        ])
        .unwrap();
        let item_type = child.data_type().clone();
        let column = nested_lists("pose", item_type, &[2, 1], child).unwrap();
        assert_eq!(column.len(), 2);
        let lists = column.downcast::<ListArray>().unwrap();
        assert_eq!(lists.value_length(0), Some(2));
        assert_eq!(lists.value_length(1), Some(1));
    }

    #[test]
    fn empty_inputs_produce_empty_columns_not_errors() {
        assert_eq!(primitive_lists::<f32>("v", &[]).unwrap().len(), 0);
        assert_eq!(
            fixed_tuple_lists::<f32>("xy", "v", 2, &[]).unwrap().len(),
            0
        );
        assert_eq!(fixed_tuples::<f64>("v", 4, &[]).unwrap().len(), 0);
    }
}
