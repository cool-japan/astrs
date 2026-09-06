//! [`ImageView`] — the `std/media/v1/Image` accessor.
//!
//! [`crate::urn::layouts::media::image_layout`] fixes the columnar shape:
//! `{width, height, stride: UInt32, channels: UInt8, data: List<sample>}`,
//! one row per image, `sample` being `UInt8`, `UInt16` or `Float32`
//! depending on the port's `pixel` parameter. `ImageView` is what turns one
//! such row back into the `(height, width, channels)` [`TensorView`] a
//! vision node actually wants to index — reading `width`/`height`/`channels`
//! from the row itself, so it never needs the URN or a caller-supplied
//! shape at all.
//!
//! ```
//! use astrs_data::array::{Array, IntoArrayRef, ListArray, StructArray, UInt32Array, UInt8Array};
//! use astrs_data::tensor::ImageView;
//! use astrs_data::urn::layouts::media::image_layout;
//! use astrs_data::{DataType, Field};
//!
//! // One 2x2 mono8 image, packed row-major.
//! let DataType::Struct(fields) = image_layout("mono8")? else { unreachable!() };
//! let data = ListArray::try_from_lengths(
//!     Field::required("sample", DataType::UInt8),
//!     [4],
//!     UInt8Array::from_values([10, 20, 30, 40]).into_array_ref(),
//! )?
//! .into_array_ref();
//! let row = StructArray::try_new(
//!     fields,
//!     vec![
//!         UInt32Array::from_values([2u32]).into_array_ref(), // width
//!         UInt32Array::from_values([2u32]).into_array_ref(), // height
//!         UInt32Array::from_values([2u32]).into_array_ref(), // stride
//!         UInt8Array::from_values([1u8]).into_array_ref(),   // channels
//!         data,
//!     ],
//!     None,
//! )?;
//!
//! let view = ImageView::from_struct_row(&row, 0)?;
//! assert_eq!((view.height(), view.width(), view.channels()), (2, 2, 1));
//! assert_eq!(view.get_f64(&[1, 0, 0])?, 30.0);
//! # Ok::<(), astrs_data::DataError>(())
//! ```

use crate::array::structs::StructArray;
use crate::array::{
    Array, ArrayExt, Float32Array, ListArray, UInt8Array, UInt16Array, UInt32Array,
};
use crate::datatype::DataType;
use crate::error::{DataError, Result};
use crate::tensor::view::TensorView;

/// Reads the `UInt32` value of column `field` at `row`.
///
/// # Errors
///
/// [`DataError::FieldNotFound`], [`DataError::DowncastFailed`],
/// [`DataError::RequiredFieldIsNull`], or [`DataError::IndexOutOfBounds`].
fn read_u32_field(array: &StructArray, field: &str, row: usize) -> Result<u32> {
    let column = array.try_column_by_name(field)?;
    let typed = column.try_downcast::<UInt32Array>()?;
    typed.get(row).ok_or_else(|| {
        if row < typed.len() {
            DataError::RequiredFieldIsNull {
                field: field.to_owned(),
                row,
            }
        } else {
            DataError::IndexOutOfBounds {
                index: row,
                len: typed.len(),
            }
        }
    })
}

/// Reads the `UInt8` value of column `field` at `row`. See [`read_u32_field`].
fn read_u8_field(array: &StructArray, field: &str, row: usize) -> Result<u8> {
    let column = array.try_column_by_name(field)?;
    let typed = column.try_downcast::<UInt8Array>()?;
    typed.get(row).ok_or_else(|| {
        if row < typed.len() {
            DataError::RequiredFieldIsNull {
                field: field.to_owned(),
                row,
            }
        } else {
            DataError::IndexOutOfBounds {
                index: row,
                len: typed.len(),
            }
        }
    })
}

/// A `(height, width, channels)` view over one `std/media/v1/Image` row.
///
/// The sample type is exactly the row's `data` column's element type —
/// `UInt8`, `UInt16` or `Float32`, per [`image_layout`](crate::urn::layouts::media::image_layout).
/// Match on the variant to work with the concrete [`TensorView`] directly,
/// or use [`ImageView::get_f64`] for a type-erased read.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ImageView {
    /// `sample` is `UInt8` (`mono8`, `rgb8`, `rgba8`, `bgr8`, `bgra8`, the
    /// Bayer mosaics).
    U8(TensorView<u8>),
    /// `sample` is `UInt16` (`mono16`, `rgb16`, `rgba16`, `bgr16`, `bgra16`).
    U16(TensorView<u16>),
    /// `sample` is `Float32` (`mono32f`, `rgb32f`, `rgba32f`).
    F32(TensorView<f32>),
}

impl ImageView {
    /// Builds a view over row `row` of a `std/media/v1/Image`-shaped struct
    /// array (or of a payload's own top-level column downcast to one).
    ///
    /// # Errors
    ///
    /// * [`DataError::FieldNotFound`] — `array` is missing one of
    ///   `width`/`height`/`channels`/`data`.
    /// * [`DataError::DowncastFailed`] — a field's column is not the type
    ///   [`image_layout`](crate::urn::layouts::media::image_layout) declares.
    /// * [`DataError::RequiredFieldIsNull`] — `width`/`height`/`channels` or
    ///   the whole row is null.
    /// * [`DataError::IndexOutOfBounds`] — `row >= array.len()`.
    /// * [`DataError::TypeMismatch`] — `data`'s element type is not
    ///   `UInt8`/`UInt16`/`Float32`.
    /// * [`DataError::TensorShapeMismatch`] — `data`'s row does not hold
    ///   exactly `width * height * channels` samples.
    pub fn from_struct_row(array: &StructArray, row: usize) -> Result<Self> {
        let width = read_u32_field(array, "width", row)? as usize;
        let height = read_u32_field(array, "height", row)? as usize;
        let channels = read_u8_field(array, "channels", row)? as usize;
        let shape = [height, width, channels];

        let data_column = array.try_column_by_name("data")?;
        let list = data_column.try_downcast::<ListArray>()?;
        let Some(samples) = list.get(row) else {
            return Err(if row < list.len() {
                DataError::RequiredFieldIsNull {
                    field: "data".to_owned(),
                    row,
                }
            } else {
                DataError::IndexOutOfBounds {
                    index: row,
                    len: list.len(),
                }
            });
        };

        match samples.data_type() {
            DataType::UInt8 => {
                let typed = samples.try_downcast::<UInt8Array>()?;
                Ok(Self::U8(TensorView::from_primitive(typed, shape)?))
            }
            DataType::UInt16 => {
                let typed = samples.try_downcast::<UInt16Array>()?;
                Ok(Self::U16(TensorView::from_primitive(typed, shape)?))
            }
            DataType::Float32 => {
                let typed = samples.try_downcast::<Float32Array>()?;
                Ok(Self::F32(TensorView::from_primitive(typed, shape)?))
            }
            other => Err(DataError::type_mismatch(DataType::UInt8, other.clone())),
        }
    }

    /// The `(height, width, channels)` shape, whatever the sample type.
    #[must_use]
    pub fn shape(&self) -> &[usize] {
        match self {
            Self::U8(tensor) => tensor.shape(),
            Self::U16(tensor) => tensor.shape(),
            Self::F32(tensor) => tensor.shape(),
        }
    }

    /// Row count.
    #[inline]
    #[must_use]
    pub fn height(&self) -> usize {
        self.shape()[0]
    }

    /// Column count.
    #[inline]
    #[must_use]
    pub fn width(&self) -> usize {
        self.shape()[1]
    }

    /// Samples per pixel.
    #[inline]
    #[must_use]
    pub fn channels(&self) -> usize {
        self.shape()[2]
    }

    /// The `UInt8`-sample tensor, if this view holds one.
    #[must_use]
    pub fn as_u8(&self) -> Option<&TensorView<u8>> {
        match self {
            Self::U8(tensor) => Some(tensor),
            _ => None,
        }
    }

    /// The `UInt16`-sample tensor, if this view holds one.
    #[must_use]
    pub fn as_u16(&self) -> Option<&TensorView<u16>> {
        match self {
            Self::U16(tensor) => Some(tensor),
            _ => None,
        }
    }

    /// The `Float32`-sample tensor, if this view holds one.
    #[must_use]
    pub fn as_f32(&self) -> Option<&TensorView<f32>> {
        match self {
            Self::F32(tensor) => Some(tensor),
            _ => None,
        }
    }

    /// The sample at `[row, column, channel]`, widened to `f64` — every
    /// sample type this view can hold widens losslessly.
    ///
    /// # Errors
    ///
    /// Whatever the underlying [`TensorView::get`] reports.
    pub fn get_f64(&self, index: &[usize]) -> Result<f64> {
        match self {
            Self::U8(tensor) => tensor.get(index).map(f64::from),
            Self::U16(tensor) => tensor.get(index).map(f64::from),
            Self::F32(tensor) => tensor.get(index).map(f64::from),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::Field;
    use crate::array::{Float32Array, IntoArrayRef, UInt16Array};
    use crate::urn::layouts::media::image_layout;

    fn struct_fields(pixel: &str) -> Vec<Field> {
        match image_layout(pixel).unwrap() {
            DataType::Struct(fields) => fields,
            _ => panic!("image_layout always returns a Struct"),
        }
    }

    fn mono8_row(width: u32, height: u32, samples: Vec<u8>) -> StructArray {
        let data = ListArray::try_from_lengths(
            Field::required("sample", DataType::UInt8),
            [samples.len()],
            UInt8Array::from_values(samples).into_array_ref(),
        )
        .unwrap()
        .into_array_ref();
        StructArray::try_new(
            struct_fields("mono8"),
            vec![
                UInt32Array::from_values([width]).into_array_ref(),
                UInt32Array::from_values([height]).into_array_ref(),
                UInt32Array::from_values([width]).into_array_ref(),
                UInt8Array::from_values([1u8]).into_array_ref(),
                data,
            ],
            None,
        )
        .unwrap()
    }

    #[test]
    fn a_mono8_row_round_trips() {
        let row = mono8_row(3, 2, vec![0, 1, 2, 3, 4, 5]);
        let view = ImageView::from_struct_row(&row, 0).unwrap();
        assert_eq!((view.height(), view.width(), view.channels()), (2, 3, 1));
        assert!(view.as_u8().is_some());
        assert!(view.as_u16().is_none());
        assert_eq!(view.get_f64(&[0, 0, 0]).unwrap(), 0.0);
        assert_eq!(view.get_f64(&[1, 2, 0]).unwrap(), 5.0);
    }

    #[test]
    fn a_multi_channel_row_shapes_the_channel_axis() {
        // 2x2 rgb8: 2*2*3 = 12 samples.
        let data = ListArray::try_from_lengths(
            Field::required("sample", DataType::UInt8),
            [12],
            UInt8Array::from_values(0..12u8).into_array_ref(),
        )
        .unwrap()
        .into_array_ref();
        let row = StructArray::try_new(
            struct_fields("rgb8"),
            vec![
                UInt32Array::from_values([2u32]).into_array_ref(),
                UInt32Array::from_values([2u32]).into_array_ref(),
                UInt32Array::from_values([6u32]).into_array_ref(),
                UInt8Array::from_values([3u8]).into_array_ref(),
                data,
            ],
            None,
        )
        .unwrap();

        let view = ImageView::from_struct_row(&row, 0).unwrap();
        assert_eq!(view.shape(), &[2, 2, 3]);
        // Row-major over (height, width, channels): pixel (row=1, col=0)
        // starts at flat index 1*2*3 + 0*3 = 6, so its channels are 6,7,8.
        assert_eq!(view.get_f64(&[1, 0, 0]).unwrap(), 6.0);
        assert_eq!(view.get_f64(&[1, 0, 2]).unwrap(), 8.0);
        // Pixel (row=1, col=1) is the last one: samples 9,10,11.
        assert_eq!(view.get_f64(&[1, 1, 0]).unwrap(), 9.0);
        assert_eq!(view.get_f64(&[1, 1, 2]).unwrap(), 11.0);
    }

    #[test]
    fn a_u16_row_selects_the_u16_variant() {
        let data = ListArray::try_from_lengths(
            Field::required("sample", DataType::UInt16),
            [4],
            UInt16Array::from_values([100u16, 200, 300, 400]).into_array_ref(),
        )
        .unwrap()
        .into_array_ref();
        let row = StructArray::try_new(
            struct_fields("mono16"),
            vec![
                UInt32Array::from_values([2u32]).into_array_ref(),
                UInt32Array::from_values([2u32]).into_array_ref(),
                UInt32Array::from_values([2u32]).into_array_ref(),
                UInt8Array::from_values([1u8]).into_array_ref(),
                data,
            ],
            None,
        )
        .unwrap();

        let view = ImageView::from_struct_row(&row, 0).unwrap();
        assert!(view.as_u16().is_some());
        assert_eq!(view.get_f64(&[0, 1, 0]).unwrap(), 200.0);
    }

    #[test]
    fn an_f32_row_selects_the_f32_variant() {
        let data = ListArray::try_from_lengths(
            Field::required("sample", DataType::Float32),
            [4],
            Float32Array::from_values([1.5f32, 2.5, 3.5, 4.5]).into_array_ref(),
        )
        .unwrap()
        .into_array_ref();
        let row = StructArray::try_new(
            struct_fields("mono32f"),
            vec![
                UInt32Array::from_values([2u32]).into_array_ref(),
                UInt32Array::from_values([2u32]).into_array_ref(),
                UInt32Array::from_values([2u32]).into_array_ref(),
                UInt8Array::from_values([1u8]).into_array_ref(),
                data,
            ],
            None,
        )
        .unwrap();

        let view = ImageView::from_struct_row(&row, 0).unwrap();
        assert!(view.as_f32().is_some());
        assert_eq!(view.get_f64(&[1, 1, 0]).unwrap(), 4.5);
    }

    #[test]
    fn a_second_row_in_a_multi_row_batch_is_independent() {
        let data = ListArray::try_from_lengths(
            Field::required("sample", DataType::UInt8),
            [4, 4],
            UInt8Array::from_values([0, 1, 2, 3, 10, 11, 12, 13]).into_array_ref(),
        )
        .unwrap()
        .into_array_ref();
        let row = StructArray::try_new(
            struct_fields("mono8"),
            vec![
                UInt32Array::from_values([2u32, 2]).into_array_ref(),
                UInt32Array::from_values([2u32, 2]).into_array_ref(),
                UInt32Array::from_values([2u32, 2]).into_array_ref(),
                UInt8Array::from_values([1u8, 1]).into_array_ref(),
                data,
            ],
            None,
        )
        .unwrap();

        let first = ImageView::from_struct_row(&row, 0).unwrap();
        let second = ImageView::from_struct_row(&row, 1).unwrap();
        assert_eq!(first.get_f64(&[0, 0, 0]).unwrap(), 0.0);
        assert_eq!(second.get_f64(&[0, 0, 0]).unwrap(), 10.0);
    }

    #[test]
    fn a_missing_field_is_reported() {
        let fields = vec![Field::required("width", DataType::UInt32)];
        let row = StructArray::try_new(
            fields,
            vec![UInt32Array::from_values([1u32]).into_array_ref()],
            None,
        )
        .unwrap();
        assert!(matches!(
            ImageView::from_struct_row(&row, 0),
            Err(DataError::FieldNotFound { .. })
        ));
    }

    #[test]
    fn a_null_required_field_is_reported_precisely() {
        // A struct's own row-level validity (tested elsewhere on
        // `StructArray` itself) does not null out its children — Arrow
        // leaves their values at a null parent slot unspecified, not
        // necessarily null — so this nulls the `width` *column* directly,
        // the only way `read_u32_field` can actually observe a null here.
        let width = UInt32Array::from_opt_iter([None]).into_array_ref();
        let height = UInt32Array::from_values([2u32]).into_array_ref();
        let stride = UInt32Array::from_values([2u32]).into_array_ref();
        let channels = UInt8Array::from_values([1u8]).into_array_ref();
        let data = ListArray::try_from_lengths(
            Field::required("sample", DataType::UInt8),
            [4],
            UInt8Array::from_values([0u8, 1, 2, 3]).into_array_ref(),
        )
        .unwrap()
        .into_array_ref();
        let row = StructArray::try_new(
            struct_fields("mono8"),
            vec![width, height, stride, channels, data],
            None,
        )
        .unwrap();

        assert!(matches!(
            ImageView::from_struct_row(&row, 0),
            Err(DataError::RequiredFieldIsNull { field, row: 0 }) if field == "width"
        ));
    }

    #[test]
    fn an_out_of_range_row_is_index_out_of_bounds() {
        let row = mono8_row(2, 2, vec![0, 1, 2, 3]);
        assert!(matches!(
            ImageView::from_struct_row(&row, 5),
            Err(DataError::IndexOutOfBounds { index: 5, .. })
        ));
    }

    #[test]
    fn a_shape_mismatch_is_reported() {
        // Declares 2x2x1 = 4 samples but the data column only holds 3.
        let data = ListArray::try_from_lengths(
            Field::required("sample", DataType::UInt8),
            [3],
            UInt8Array::from_values([0u8, 1, 2]).into_array_ref(),
        )
        .unwrap()
        .into_array_ref();
        let row = StructArray::try_new(
            struct_fields("mono8"),
            vec![
                UInt32Array::from_values([2u32]).into_array_ref(),
                UInt32Array::from_values([2u32]).into_array_ref(),
                UInt32Array::from_values([2u32]).into_array_ref(),
                UInt8Array::from_values([1u8]).into_array_ref(),
                data,
            ],
            None,
        )
        .unwrap();
        assert!(matches!(
            ImageView::from_struct_row(&row, 0),
            Err(DataError::TensorShapeMismatch {
                expected: 4,
                actual: 3
            })
        ));
    }
}
