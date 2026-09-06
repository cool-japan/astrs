//! [`TensorView`] — an N-dimensional, row-major view over a
//! [`PrimitiveArray`]'s flat values.
//!
//! A [`FixedSizeListArray`](crate::array::FixedSizeListArray) of `FixedSizeList`s already models a tensor one
//! dimension at a time (its own module doc gives the canonical example: a
//! `FixedSizeList(FixedSizeList(UInt8, 3), 640)` is one row of RGB pixels).
//! `TensorView` is the other half of that story — given a flat
//! [`PrimitiveArray`] and a shape, it hands back checked, zero-copy N-D
//! indexing and sub-view slicing over that same flat buffer, without ever
//! materialising the nested nested-array structure. [`crate::tensor::image`]
//! is the concrete accessor built on it for `std/media/v1/Image`.
//!
//! ```
//! use astrs_data::array::Float32Array;
//! use astrs_data::tensor::TensorView;
//!
//! // A 2x3 matrix, row-major: [[1, 2, 3], [4, 5, 6]].
//! let values = Float32Array::from_values([1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
//! let tensor = TensorView::from_primitive(&values, [2, 3])?;
//!
//! assert_eq!(tensor.get(&[0, 2])?, 3.0);
//! assert_eq!(tensor.get(&[1, 0])?, 4.0);
//! assert_eq!(tensor.as_slice(), Some(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0][..]));
//!
//! // Row 1 alone, still zero-copy.
//! let row = tensor.slice_axis(0, 1, 1)?;
//! assert_eq!(row.as_slice(), Some(&[4.0, 5.0, 6.0][..]));
//! # Ok::<(), astrs_data::DataError>(())
//! ```
//!
//! # Validity is the caller's problem
//!
//! `TensorView` reads [`PrimitiveArray::values`] — every slot, including the
//! placeholder AstRS stores in a null one — because a tensor has no room for
//! a validity bitmap of its own and the layouts it is built for
//! ([`crate::urn::layouts::media::image_layout`] and friends) declare their
//! sample columns non-nullable. Check
//! [`Array::null_count`] first if the
//! source column's nullability is not already pinned down by its schema.

use crate::array::{Array, PrimitiveArray};
use crate::datatype::ArrowNativeType;
use crate::error::{DataError, Result};

/// Row-major (C-order) element strides for `shape`: the last axis is
/// contiguous (`stride == 1`), and each earlier axis's stride is the product
/// of every axis after it.
///
/// Only ever called after [`checked_shape_len`] has proven `shape`'s full
/// product fits in a `usize` — every stride computed here is a *partial*
/// product of the same dimensions, so it is bounded by that same total and
/// cannot overflow either; `saturating_mul` is therefore never actually
/// asked to saturate, it is just cheaper than re-deriving a `checked_mul`
/// proof the caller already has.
fn row_major_strides(shape: &[usize]) -> Vec<usize> {
    let mut strides = vec![1usize; shape.len()];
    for axis in (0..shape.len().saturating_sub(1)).rev() {
        strides[axis] = strides[axis + 1].saturating_mul(shape[axis + 1]);
    }
    strides
}

/// The product of `shape`'s dimensions, or `None` if it overflows `usize`.
///
/// A tensor's total element count and every stride [`row_major_strides`]
/// derives from it are bounded by this same product, so checking it once
/// here — instead of letting [`Iterator::product`] panic on overflow in a
/// debug build (or silently wrap in release) — is what keeps every later
/// arithmetic step in this module (strides, [`TensorView::flat_index`]'s
/// running sum) provably in range.
fn checked_shape_len(shape: &[usize]) -> Option<usize> {
    shape
        .iter()
        .try_fold(1usize, |acc, &dim| acc.checked_mul(dim))
}

/// A checked, zero-copy N-dimensional view over a [`PrimitiveArray`]'s flat
/// values.
///
/// Owns a clone of the source [`PrimitiveArray`] rather than borrowing it —
/// cloning one is an `Arc` bump (see [`PrimitiveArray`]'s own layout doc), so
/// this is still zero-copy, and it is what lets [`ImageView`](super::ImageView)
/// build one from a value it downcast out of a row's `Arc<dyn Array>`
/// without fighting a borrow that does not outlive the match arm it came
/// from.
///
/// See the [module documentation](self) for the shape it expects and what it
/// deliberately does not check (validity).
#[derive(Debug, Clone)]
pub struct TensorView<T: ArrowNativeType> {
    /// The flat backing values. Always at least as long as `offset` plus the
    /// product of `shape` times the largest stride — never shrunk by
    /// [`TensorView::slice_axis`], which only adjusts `offset` and `shape`.
    values: PrimitiveArray<T>,
    /// Element offset of this view's first logical value inside `values`.
    offset: usize,
    /// Extent of each dimension, outermost first.
    shape: Vec<usize>,
    /// Element stride of each dimension — how many flat elements to advance
    /// `values` by to move one step along that axis.
    strides: Vec<usize>,
}

impl<T: ArrowNativeType> TensorView<T> {
    /// Builds a view over `array`'s values with the given `shape`.
    ///
    /// # Errors
    ///
    /// * [`DataError::TensorShapeOverflow`] when `shape`'s dimensions
    ///   multiply out to more elements than `usize` can represent — checked
    ///   before anything else, so this never panics on a debug build the way
    ///   an unchecked `shape.iter().product()` would.
    /// * [`DataError::TensorShapeMismatch`] when `array.len()` is not the
    ///   product of `shape`'s dimensions.
    pub fn from_primitive(array: &PrimitiveArray<T>, shape: impl Into<Vec<usize>>) -> Result<Self> {
        let shape = shape.into();
        let expected = checked_shape_len(&shape).ok_or_else(|| DataError::TensorShapeOverflow {
            shape: shape.clone(),
        })?;
        if array.len() != expected {
            return Err(DataError::TensorShapeMismatch {
                expected,
                actual: array.len(),
            });
        }
        let strides = row_major_strides(&shape);
        Ok(Self {
            values: array.clone(),
            offset: 0,
            shape,
            strides,
        })
    }

    /// Builds a view over freshly supplied values — mainly for tests and
    /// synthetic tensors that are not already backed by a decoded column.
    ///
    /// # Errors
    ///
    /// Whatever [`TensorView::from_primitive`] reports.
    pub fn from_values(
        values: impl IntoIterator<Item = T>,
        shape: impl Into<Vec<usize>>,
    ) -> Result<Self> {
        Self::from_primitive(&PrimitiveArray::from_values(values), shape)
    }

    /// Extent of each dimension, outermost first.
    #[inline]
    #[must_use]
    pub fn shape(&self) -> &[usize] {
        &self.shape
    }

    /// Element stride of each dimension.
    #[inline]
    #[must_use]
    pub fn strides(&self) -> &[usize] {
        &self.strides
    }

    /// Number of dimensions.
    #[inline]
    #[must_use]
    pub fn ndim(&self) -> usize {
        self.shape.len()
    }

    /// Total number of logical elements — the product of [`TensorView::shape`],
    /// `1` for a zero-dimensional (scalar) tensor.
    #[must_use]
    pub fn len(&self) -> usize {
        self.shape.iter().product()
    }

    /// Returns `true` when any dimension is zero.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns `true` when this view's memory is one contiguous run in
    /// row-major order — true for a freshly built tensor and for any
    /// [`TensorView::slice_axis`] result on axis 0, false once a slice on
    /// any other axis has narrowed a dimension with extent greater than one
    /// that is not the outermost.
    ///
    /// A size-one dimension's stride never contributes to which memory a
    /// read touches (there is only one valid index into it, `0`), so — like
    /// NumPy's own C-contiguity check — this walks the axes from innermost
    /// out, skipping any dimension whose extent is `1`, rather than
    /// comparing every stride against the naive `row_major_strides` verbatim; the
    /// naive comparison would call a size-one axis "non-contiguous" for
    /// carrying a stride left over from before it was narrowed to `1`, even
    /// though no read the shape allows can tell the difference.
    #[must_use]
    pub fn is_contiguous(&self) -> bool {
        let mut expected_stride = 1usize;
        for axis in (0..self.shape.len()).rev() {
            let dim = self.shape[axis];
            if dim == 0 {
                return true; // an empty tensor has no addresses to disagree on
            }
            if dim == 1 {
                continue; // this axis's stride is unobservable
            }
            if self.strides[axis] != expected_stride {
                return false;
            }
            expected_stride = expected_stride.saturating_mul(dim);
        }
        true
    }

    /// The whole view as one flat slice, when [`TensorView::is_contiguous`].
    ///
    /// ```
    /// use astrs_data::array::Int32Array;
    /// use astrs_data::tensor::TensorView;
    ///
    /// let values = Int32Array::from_values(0..12);
    /// let tensor = TensorView::from_primitive(&values, [3, 4])?;
    /// assert_eq!(tensor.as_slice().map(<[i32]>::len), Some(12));
    ///
    /// // Slicing a non-outermost axis breaks contiguity.
    /// let column = tensor.slice_axis(1, 1, 1)?;
    /// assert!(!column.is_contiguous());
    /// assert_eq!(column.as_slice(), None);
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    #[must_use]
    pub fn as_slice(&self) -> Option<&[T]> {
        if !self.is_contiguous() {
            return None;
        }
        self.values
            .values()
            .get(self.offset..self.offset + self.len())
    }

    /// The flat element offset `index` names, checked against the shape.
    ///
    /// # Errors
    ///
    /// * [`DataError::TensorRankMismatch`] when `index.len() != self.ndim()`.
    /// * [`DataError::TensorIndexOutOfBounds`] when a component is not
    ///   smaller than its axis's extent.
    pub fn flat_index(&self, index: &[usize]) -> Result<usize> {
        if index.len() != self.shape.len() {
            return Err(DataError::TensorRankMismatch {
                expected: self.shape.len(),
                actual: index.len(),
            });
        }
        let mut flat = self.offset;
        for (axis, ((&component, &dim), &stride)) in index
            .iter()
            .zip(self.shape.iter())
            .zip(self.strides.iter())
            .enumerate()
        {
            if component >= dim {
                return Err(DataError::TensorIndexOutOfBounds {
                    axis,
                    index: component,
                    dim,
                });
            }
            flat += component * stride;
        }
        Ok(flat)
    }

    /// The value at `index`.
    ///
    /// # Errors
    ///
    /// Whatever [`TensorView::flat_index`] reports.
    pub fn get(&self, index: &[usize]) -> Result<T> {
        let flat = self.flat_index(index)?;
        self.values
            .values()
            .get(flat)
            .copied()
            .ok_or(DataError::IndexOutOfBounds {
                index: flat,
                len: self.values.len(),
            })
    }

    /// A zero-copy sub-view: `axis` narrowed to `[start, start + len)`,
    /// clamped to the axis's extent (the crate-wide slicing convention —
    /// see [`Array::slice`]).
    ///
    /// # Errors
    ///
    /// [`DataError::TensorAxisOutOfBounds`] when `axis >= self.ndim()`.
    pub fn slice_axis(&self, axis: usize, start: usize, len: usize) -> Result<Self> {
        let dim = *self
            .shape
            .get(axis)
            .ok_or(DataError::TensorAxisOutOfBounds {
                axis,
                ndim: self.ndim(),
            })?;
        let start = start.min(dim);
        let len = len.min(dim - start);
        let mut shape = self.shape.clone();
        shape[axis] = len;
        Ok(Self {
            values: self.values.clone(),
            offset: self.offset + start * self.strides[axis],
            shape,
            strides: self.strides.clone(),
        })
    }

    /// Checked [`TensorView::slice_axis`].
    ///
    /// # Errors
    ///
    /// [`DataError::TensorAxisOutOfBounds`] when `axis >= self.ndim()`, or
    /// [`DataError::SliceOutOfBounds`] when `[start, start + len)` leaves the
    /// axis's extent.
    pub fn try_slice_axis(&self, axis: usize, start: usize, len: usize) -> Result<Self> {
        let dim = *self
            .shape
            .get(axis)
            .ok_or(DataError::TensorAxisOutOfBounds {
                axis,
                ndim: self.ndim(),
            })?;
        if start.saturating_add(len) > dim {
            return Err(DataError::SliceOutOfBounds {
                offset: start,
                len,
                available: dim,
            });
        }
        self.slice_axis(axis, start, len)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{Float32Array, Int32Array};

    #[test]
    fn row_major_strides_match_a_hand_worked_example() {
        // A [2, 3, 4] tensor: moving one step in the outermost axis skips a
        // whole 3x4 = 12-element plane; the innermost axis is contiguous.
        assert_eq!(row_major_strides(&[2, 3, 4]), vec![12, 4, 1]);
        assert_eq!(row_major_strides(&[5]), vec![1]);
        assert_eq!(row_major_strides(&[]), Vec::<usize>::new());
    }

    #[test]
    fn shape_product_overflow_is_a_checked_error_not_a_panic() {
        // `usize::MAX * 2` overflows regardless of pointer width; a naive
        // `shape.iter().product()` panics under debug-mode overflow checks
        // (and silently wraps in release), either of which would violate
        // this crate's "never panics on data-dependent input" policy.
        let values = Int32Array::from_values([1, 2]);
        assert_eq!(
            TensorView::from_primitive(&values, [usize::MAX, 2]).unwrap_err(),
            DataError::TensorShapeOverflow {
                shape: vec![usize::MAX, 2]
            }
        );
        // A shape whose product overflows only partway through must still
        // be caught, not just one whose very first dimension already does.
        assert_eq!(
            TensorView::from_primitive(&values, [2, 3, usize::MAX / 4]).unwrap_err(),
            DataError::TensorShapeOverflow {
                shape: vec![2, 3, usize::MAX / 4]
            }
        );
    }

    #[test]
    fn construction_checks_the_element_count() {
        let values = Int32Array::from_values(0..12);
        assert!(TensorView::from_primitive(&values, [3, 4]).is_ok());
        assert_eq!(
            TensorView::from_primitive(&values, [3, 3]).unwrap_err(),
            DataError::TensorShapeMismatch {
                expected: 9,
                actual: 12
            }
        );
    }

    #[test]
    fn a_scalar_tensor_has_one_element_and_no_dimensions() {
        let values = Int32Array::from_values([7]);
        let tensor = TensorView::from_primitive(&values, []).unwrap();
        assert_eq!(tensor.ndim(), 0);
        assert_eq!(tensor.len(), 1);
        assert_eq!(tensor.get(&[]), Ok(7));
        assert_eq!(tensor.as_slice(), Some(&[7][..]));
    }

    #[test]
    fn an_empty_dimension_is_a_zero_length_tensor() {
        let values = Int32Array::from_values([] as [i32; 0]);
        let tensor = TensorView::from_primitive(&values, [0, 5]).unwrap();
        assert!(tensor.is_empty());
        assert_eq!(tensor.len(), 0);
    }

    #[test]
    fn indexing_walks_row_major_order() {
        // [[0, 1, 2], [3, 4, 5]]
        let tensor = TensorView::from_values(0..6, [2, 3]).unwrap();
        assert_eq!(tensor.get(&[0, 0]), Ok(0));
        assert_eq!(tensor.get(&[0, 2]), Ok(2));
        assert_eq!(tensor.get(&[1, 0]), Ok(3));
        assert_eq!(tensor.get(&[1, 2]), Ok(5));
    }

    #[test]
    fn indexing_rejects_the_wrong_rank() {
        let tensor = TensorView::from_values(0..6, [2, 3]).unwrap();
        assert_eq!(
            tensor.get(&[0]).unwrap_err(),
            DataError::TensorRankMismatch {
                expected: 2,
                actual: 1
            }
        );
        assert_eq!(
            tensor.get(&[0, 0, 0]).unwrap_err(),
            DataError::TensorRankMismatch {
                expected: 2,
                actual: 3
            }
        );
    }

    #[test]
    fn indexing_rejects_an_out_of_range_component() {
        let tensor = TensorView::from_values(0..6, [2, 3]).unwrap();
        assert_eq!(
            tensor.get(&[2, 0]).unwrap_err(),
            DataError::TensorIndexOutOfBounds {
                axis: 0,
                index: 2,
                dim: 2
            }
        );
        assert_eq!(
            tensor.get(&[0, 3]).unwrap_err(),
            DataError::TensorIndexOutOfBounds {
                axis: 1,
                index: 3,
                dim: 3
            }
        );
    }

    #[test]
    fn slicing_the_outermost_axis_stays_contiguous() {
        let tensor = TensorView::from_values(0..12, [3, 4]).unwrap();
        let rows = tensor.slice_axis(0, 1, 2).unwrap();
        assert_eq!(rows.shape(), &[2, 4]);
        assert!(rows.is_contiguous());
        assert_eq!(rows.as_slice(), Some(&[4, 5, 6, 7, 8, 9, 10, 11][..]));
        assert_eq!(rows.get(&[0, 0]), Ok(4));
        assert_eq!(rows.get(&[1, 3]), Ok(11));
    }

    #[test]
    fn slicing_an_inner_axis_breaks_contiguity_but_not_indexing() {
        let tensor = TensorView::from_values(0..12, [3, 4]).unwrap();
        let column = tensor.slice_axis(1, 1, 2).unwrap();
        assert_eq!(column.shape(), &[3, 2]);
        assert!(!column.is_contiguous());
        assert_eq!(column.as_slice(), None);
        // Row 0: [0,1,2,3] -> columns 1..3 -> [1, 2]
        assert_eq!(column.get(&[0, 0]), Ok(1));
        assert_eq!(column.get(&[0, 1]), Ok(2));
        // Row 2: [8,9,10,11] -> columns 1..3 -> [9, 10]
        assert_eq!(column.get(&[2, 0]), Ok(9));
        assert_eq!(column.get(&[2, 1]), Ok(10));
    }

    #[test]
    fn narrowing_a_middle_axis_to_one_slot_still_leaves_a_gap() {
        // Contrast with `slices_compose`: here the *outer* axis still has
        // real (>1) extent when the middle one is narrowed to a single
        // slot, so consecutive logical elements are not consecutive in
        // memory — picking one slice out of three leaves the other two
        // slices' worth of elements in the gap between "row" 0 and "row" 1.
        let tensor = TensorView::from_values(0..24, [2, 3, 4]).unwrap();
        let narrowed = tensor.slice_axis(1, 2, 1).unwrap();
        assert_eq!(narrowed.shape(), &[2, 1, 4]);
        assert!(!narrowed.is_contiguous());
        assert_eq!(narrowed.as_slice(), None);
        assert_eq!(narrowed.get(&[0, 0, 0]), Ok(8));
        assert_eq!(narrowed.get(&[1, 0, 0]), Ok(20));
    }

    #[test]
    fn a_size_one_axis_does_not_block_contiguity_when_it_is_not_sandwiched() {
        // Unlike the case above, a fresh tensor whose middle dimension is
        // *already* size 1 (not narrowed down from something bigger) is
        // genuinely one contiguous run: there is no "other slot" being
        // skipped, so the middle axis's stride is unobservable either way.
        let tensor = TensorView::from_values(0..8, [2, 1, 4]).unwrap();
        assert!(tensor.is_contiguous());
        assert_eq!(tensor.as_slice(), Some(&[0, 1, 2, 3, 4, 5, 6, 7][..]));
    }

    #[test]
    fn slices_compose() {
        let tensor = TensorView::from_values(0..24, [2, 3, 4]).unwrap();
        let plane = tensor.slice_axis(0, 1, 1).unwrap();
        let row = plane.slice_axis(1, 2, 1).unwrap();
        assert_eq!(row.shape(), &[1, 1, 4]);
        assert_eq!(row.as_slice(), Some(&[20, 21, 22, 23][..]));
    }

    #[test]
    fn slice_axis_clamps_like_every_other_slice_in_the_crate() {
        let tensor = TensorView::from_values(0..12, [3, 4]).unwrap();
        assert_eq!(tensor.slice_axis(0, 2, 99).unwrap().shape(), &[1, 4]);
        assert_eq!(tensor.slice_axis(0, 99, 1).unwrap().shape(), &[0, 4]);
    }

    #[test]
    fn slice_axis_rejects_an_axis_that_does_not_exist() {
        let tensor = TensorView::from_values(0..12, [3, 4]).unwrap();
        assert_eq!(
            tensor.slice_axis(2, 0, 1).unwrap_err(),
            DataError::TensorAxisOutOfBounds { axis: 2, ndim: 2 }
        );
    }

    #[test]
    fn try_slice_axis_reports_out_of_range_instead_of_clamping() {
        let tensor = TensorView::from_values(0..12, [3, 4]).unwrap();
        assert!(tensor.try_slice_axis(0, 1, 2).is_ok());
        assert_eq!(
            tensor.try_slice_axis(0, 2, 2).unwrap_err(),
            DataError::SliceOutOfBounds {
                offset: 2,
                len: 2,
                available: 3
            }
        );
        assert_eq!(
            tensor.try_slice_axis(9, 0, 1).unwrap_err(),
            DataError::TensorAxisOutOfBounds { axis: 9, ndim: 2 }
        );
    }

    #[test]
    fn from_primitive_shares_the_underlying_buffer() {
        let values = Float32Array::from_values([1.0, 2.0, 3.0, 4.0]);
        let tensor = TensorView::from_primitive(&values, [2, 2]).unwrap();
        assert_eq!(
            tensor.as_slice().map(<[f32]>::as_ptr),
            Some(values.values().as_ptr()),
            "no copy happened"
        );
    }

    #[test]
    fn values_include_whatever_a_null_slot_holds() {
        // TensorView does not consult validity — see the module doc.
        let values = Int32Array::from_opt_iter([Some(1), None, Some(3), Some(4)]);
        let tensor = TensorView::from_primitive(&values, [2, 2]).unwrap();
        assert_eq!(tensor.get(&[0, 1]), Ok(0), "null slot's placeholder value");
    }
}
