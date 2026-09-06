//! Column accessors shared by every [`AstrsMessage`](super::AstrsMessage)
//! implementation.
//!
//! The mirror of [`build`](super::build): four shapes to read back, plus the
//! bounds and type checks that turn "this payload is not what the port
//! promised" into a named error rather than a wrong number.
//!
//! Every accessor here is total and checked. A payload arrives from another
//! process — possibly an older build, possibly a `type: any` port, possibly a
//! ROS 2 bridge — so a decoder that indexed without checking would be reading
//! whatever the sender happened to send.
//!
//! # Examples
//!
//! ```
//! use astrs_node_api::message::{build, read};
//!
//! let column = build::structure(vec![("x", build::primitive::<f64>(&[2.5]))])?;
//! let row = read::structure(&column)?;
//! assert_eq!(read::f64_at(read::field(row, "x")?, 0)?, 2.5);
//! assert!(read::field(row, "missing").is_err());
//! # Ok::<(), astrs_data::DataError>(())
//! ```

use astrs_data::array::{
    Array, ArrayExt, ArrayRef, BinaryArray, BooleanArray, FixedSizeListArray, ListArray,
    PrimitiveArray, StringArray, StructArray, TimestampArray,
};
use astrs_data::{ArrowNativeType, DataError, DataType, Result};

/// The struct array behind a column, or a named error.
///
/// # Errors
///
/// [`DataError`] when the column is not a `Struct`.
pub fn structure(column: &ArrayRef) -> Result<&StructArray> {
    column
        .downcast::<StructArray>()
        .ok_or_else(|| type_error("Struct", column))
}

/// The list array behind a column.
///
/// # Errors
///
/// [`DataError`] when the column is not a `List`.
pub fn list(column: &ArrayRef) -> Result<&ListArray> {
    column
        .downcast::<ListArray>()
        .ok_or_else(|| type_error("List", column))
}

/// The fixed-size list array behind a column.
///
/// # Errors
///
/// [`DataError`] when the column is not a `FixedSizeList`.
pub fn fixed_list(column: &ArrayRef) -> Result<&FixedSizeListArray> {
    column
        .downcast::<FixedSizeListArray>()
        .ok_or_else(|| type_error("FixedSizeList", column))
}

/// One named child column of a struct.
///
/// # Errors
///
/// [`DataError`] when the struct has no such field.
pub fn field<'a>(array: &'a StructArray, name: &str) -> Result<&'a ArrayRef> {
    array.try_column_by_name(name)
}

/// A named child of the struct behind `column`.
///
/// The two-step form callers use most: `read::child(column, "position")`.
///
/// # Errors
///
/// As [`structure`] and [`field`].
pub fn child<'a>(column: &'a ArrayRef, name: &str) -> Result<&'a ArrayRef> {
    field(structure(column)?, name)
}

/// One value of a primitive column.
///
/// # Errors
///
/// [`DataError`] when the column is not of `T`'s type, or `row` is out
/// of range, or the slot is null (the std layouts have no nullable fields).
pub fn primitive_at<T: ArrowNativeType>(column: &ArrayRef, row: usize) -> Result<T> {
    let array = column
        .downcast::<PrimitiveArray<T>>()
        .ok_or_else(|| type_error_for(&T::DATA_TYPE, column))?;
    array.value(row).ok_or_else(|| row_error(row, array.len()))
}

/// One `Float64` value.
///
/// # Errors
///
/// As [`primitive_at`].
pub fn f64_at(column: &ArrayRef, row: usize) -> Result<f64> {
    primitive_at::<f64>(column, row)
}

/// One `Float32` value.
///
/// # Errors
///
/// As [`primitive_at`].
pub fn f32_at(column: &ArrayRef, row: usize) -> Result<f32> {
    primitive_at::<f32>(column, row)
}

/// One `Bool` value.
///
/// # Errors
///
/// As [`primitive_at`].
pub fn bool_at(column: &ArrayRef, row: usize) -> Result<bool> {
    let array = column
        .downcast::<BooleanArray>()
        .ok_or_else(|| type_error("Bool", column))?;
    array.value(row).ok_or_else(|| row_error(row, array.len()))
}

/// One `Utf8` value.
///
/// # Errors
///
/// As [`primitive_at`].
pub fn str_at(column: &ArrayRef, row: usize) -> Result<&str> {
    let array = column
        .downcast::<StringArray>()
        .ok_or_else(|| type_error("Utf8", column))?;
    array.value(row).ok_or_else(|| row_error(row, array.len()))
}

/// One `Binary` value.
///
/// # Errors
///
/// As [`primitive_at`].
pub fn bytes_at(column: &ArrayRef, row: usize) -> Result<&[u8]> {
    let array = column
        .downcast::<BinaryArray>()
        .ok_or_else(|| type_error("Binary", column))?;
    array.value(row).ok_or_else(|| row_error(row, array.len()))
}

/// One `Timestamp(ns)` value, in nanoseconds.
///
/// # Errors
///
/// As [`primitive_at`].
pub fn timestamp_at(column: &ArrayRef, row: usize) -> Result<i64> {
    let array = column
        .downcast::<TimestampArray>()
        .ok_or_else(|| type_error("Timestamp", column))?;
    array.value(row).ok_or_else(|| row_error(row, array.len()))
}

/// The primitive values of one list entry, copied out.
///
/// # Errors
///
/// [`DataError`] when the column is not a `List<T>`, or `row` is out of
/// range.
pub fn primitive_list_at<T: ArrowNativeType>(column: &ArrayRef, row: usize) -> Result<Vec<T>> {
    let lists = list(column)?;
    let values = lists
        .value(row)
        .ok_or_else(|| row_error(row, lists.len()))?;
    let array = values
        .downcast::<PrimitiveArray<T>>()
        .ok_or_else(|| type_error_for(&T::DATA_TYPE, &values))?;
    Ok(array.values().to_vec())
}

/// The flattened values of one `List<FixedSizeList<T, width>>` entry, plus
/// the tuple width actually found.
///
/// # Errors
///
/// [`DataError`] when the column is not that shape, or `row` is out of
/// range.
pub fn fixed_tuple_list_at<T: ArrowNativeType>(
    column: &ArrayRef,
    row: usize,
) -> Result<(Vec<T>, usize)> {
    let lists = list(column)?;
    let entry = lists
        .value(row)
        .ok_or_else(|| row_error(row, lists.len()))?;
    let tuples = fixed_list(&entry)?;
    let width = match tuples.data_type() {
        DataType::FixedSizeList(_, size) => usize::try_from(*size).unwrap_or(0),
        other => {
            return Err(DataError::type_mismatch(
                DataType::fixed_size_list(astrs_data::Field::required("v", T::DATA_TYPE), 1),
                other.clone(),
            ));
        }
    };
    let mut flat = Vec::with_capacity(tuples.len() * width);
    for index in 0..tuples.len() {
        let tuple = tuples
            .value(index)
            .ok_or_else(|| row_error(index, tuples.len()))?;
        let array = tuple
            .downcast::<PrimitiveArray<T>>()
            .ok_or_else(|| type_error_for(&T::DATA_TYPE, &tuple))?;
        flat.extend_from_slice(array.values());
    }
    Ok((flat, width))
}

/// The values of one `FixedSizeList<T, width>` row, copied out.
///
/// # Errors
///
/// [`DataError`] when the column is not that shape, `row` is out of
/// range, or the tuple is not `expected` wide.
pub fn fixed_tuple_at<T: ArrowNativeType>(
    column: &ArrayRef,
    row: usize,
    expected: usize,
) -> Result<Vec<T>> {
    let tuples = fixed_list(column)?;
    let tuple = tuples
        .value(row)
        .ok_or_else(|| row_error(row, tuples.len()))?;
    let array = tuple
        .downcast::<PrimitiveArray<T>>()
        .ok_or_else(|| type_error_for(&T::DATA_TYPE, &tuple))?;
    let values = array.values();
    if values.len() != expected {
        return Err(DataError::ChildLengthMismatch {
            expected,
            actual: values.len(),
        });
    }
    Ok(values.to_vec())
}

/// The nested child column and the `(offset, length)` window of one list row.
///
/// For list elements that are themselves structs, where copying the values
/// out would mean decoding them twice.
///
/// # Errors
///
/// [`DataError`] when the column is not a `List`, or `row` is out of
/// range.
pub fn list_window(column: &ArrayRef, row: usize) -> Result<(ArrayRef, usize)> {
    let lists = list(column)?;
    let values = lists
        .value(row)
        .ok_or_else(|| row_error(row, lists.len()))?;
    let len = values.len();
    Ok((values, len))
}

/// Checks that `row` is addressable in a column of `len` rows.
///
/// # Errors
///
/// [`DataError`] when it is not.
pub fn check_row(row: usize, len: usize) -> Result<()> {
    if row < len {
        Ok(())
    } else {
        Err(row_error(row, len))
    }
}

/// A "wrong shape" error naming what was expected and what was found.
fn type_error(expected: &'static str, column: &ArrayRef) -> DataError {
    DataError::DowncastFailed {
        expected,
        actual: Box::new(column.data_type().clone()),
    }
}

/// The same, when the expectation is expressible as a [`DataType`].
fn type_error_for(expected: &DataType, column: &ArrayRef) -> DataError {
    DataError::type_mismatch(expected.clone(), column.data_type().clone())
}

/// An out-of-range row error.
fn row_error(row: usize, len: usize) -> DataError {
    DataError::IndexOutOfBounds { index: row, len }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::message::build;

    #[test]
    fn primitives_are_read_back_by_row() {
        let column = build::primitive::<f64>(&[1.0, 2.0, 3.0]);
        assert_eq!(f64_at(&column, 0).unwrap(), 1.0);
        assert_eq!(f64_at(&column, 2).unwrap(), 3.0);
        assert!(f64_at(&column, 3).is_err(), "out of range");
        assert!(f32_at(&column, 0).is_err(), "wrong element type");
    }

    #[test]
    fn booleans_strings_bytes_and_timestamps_read_back() {
        assert!(bool_at(&build::boolean(&[true]), 0).unwrap());
        assert_eq!(str_at(&build::strings(&["hello"]), 0).unwrap(), "hello");
        assert_eq!(
            bytes_at(&build::binaries(&[&b"raw"[..]]), 0).unwrap(),
            b"raw"
        );
        assert_eq!(timestamp_at(&build::timestamps(&[42]), 0).unwrap(), 42);

        let wrong = build::primitive::<i32>(&[1]);
        assert!(bool_at(&wrong, 0).is_err());
        assert!(str_at(&wrong, 0).is_err());
        assert!(bytes_at(&wrong, 0).is_err());
        assert!(timestamp_at(&wrong, 0).is_err());
    }

    #[test]
    fn struct_children_are_found_by_name() {
        let column = build::structure(vec![
            ("x", build::primitive::<f64>(&[1.0])),
            ("y", build::primitive::<f64>(&[2.0])),
        ])
        .unwrap();
        assert_eq!(f64_at(child(&column, "y").unwrap(), 0).unwrap(), 2.0);
        assert!(child(&column, "z").is_err());
        assert!(structure(&build::primitive::<f64>(&[1.0])).is_err());
    }

    #[test]
    fn primitive_lists_read_back_per_row() {
        let column =
            build::primitive_lists::<f32>("range", &[&[1.0, 2.0][..], &[][..], &[3.0][..]])
                .unwrap();
        assert_eq!(
            primitive_list_at::<f32>(&column, 0).unwrap(),
            vec![1.0, 2.0]
        );
        assert!(primitive_list_at::<f32>(&column, 1).unwrap().is_empty());
        assert_eq!(primitive_list_at::<f32>(&column, 2).unwrap(), vec![3.0]);
        assert!(primitive_list_at::<f32>(&column, 3).is_err());
        assert!(primitive_list_at::<f64>(&column, 0).is_err());
    }

    #[test]
    fn fixed_tuple_lists_read_back_flat_with_their_width() {
        let column = build::fixed_tuple_lists::<f32>(
            "xywh",
            "v",
            4,
            &[&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0][..]],
        )
        .unwrap();
        let (flat, width) = fixed_tuple_list_at::<f32>(&column, 0).unwrap();
        assert_eq!(width, 4);
        assert_eq!(flat.len(), 8);
        assert_eq!(flat[4], 4.0);
        assert!(fixed_tuple_list_at::<f32>(&column, 1).is_err());
    }

    #[test]
    fn fixed_tuples_check_their_width() {
        let flat: Vec<f64> = (0..9).map(f64::from).collect();
        let column = build::fixed_tuples::<f64>("v", 9, &flat).unwrap();
        assert_eq!(fixed_tuple_at::<f64>(&column, 0, 9).unwrap().len(), 9);
        assert!(
            fixed_tuple_at::<f64>(&column, 0, 36).is_err(),
            "a 9-wide covariance is not a 36-wide one"
        );
        assert!(fixed_tuple_at::<f64>(&column, 1, 9).is_err());
    }

    #[test]
    fn list_windows_expose_the_nested_child() {
        let child =
            build::structure(vec![("v", build::primitive::<f64>(&[1.0, 2.0, 3.0]))]).unwrap();
        let item_type = child.data_type().clone();
        let column = build::nested_lists("item", item_type, &[2, 1], child).unwrap();
        let (values, len) = list_window(&column, 0).unwrap();
        assert_eq!(len, 2);
        assert_eq!(values.len(), 2);
        assert!(list_window(&column, 5).is_err());
    }

    #[test]
    fn row_bounds_are_checked_explicitly() {
        assert!(check_row(0, 1).is_ok());
        assert!(check_row(1, 1).is_err());
        assert!(check_row(0, 0).is_err());
    }

    #[test]
    fn shape_errors_name_what_was_expected() {
        let column = build::primitive::<f64>(&[1.0]);
        let error = list(&column).unwrap_err();
        assert!(error.to_string().contains("List"), "{error}");
        let error = fixed_list(&column).unwrap_err();
        assert!(error.to_string().contains("FixedSizeList"), "{error}");
        assert!(matches!(error, DataError::DowncastFailed { .. }));
    }
}
