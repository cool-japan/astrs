//! [`Vector3`] — a translation, direction, or velocity component in `f64`.

use std::ops::{Add, Mul, Neg, Sub};

use astrs_data::array::{ArrayExt, ArrayRef, Float64Array, IntoArrayRef, StructArray};
use astrs_data::urn::layouts::geometry::vector3_layout;
use astrs_data::{AstrsMessage, DataError, DataType, RecordBatch, Result as DataResult};

/// A three-component vector: a translation, a free direction, or a linear
/// velocity, depending on context.
///
/// Implements [`AstrsMessage`] under `std/geometry/v1/Vector3` — the
/// curated layout `astrs_data::urn::layouts::geometry::vector3_layout`
/// defines (`{x, y, z}`, all `Float64`), so a bare `Vector3` crosses the
/// columnar wire exactly the way `geometry_msgs/msg/Vector3` crosses the
/// CDR one (see [`crate::interop::geometry_msgs`] for that bridge).
///
/// ```
/// use astrs_tf::math::Vector3;
///
/// let a = Vector3::new(1.0, 0.0, 0.0);
/// let b = Vector3::new(0.0, 1.0, 0.0);
/// assert_eq!(a.dot(b), 0.0); // orthogonal
/// assert_eq!(a.cross(b), Vector3::UNIT_Z);
/// assert_eq!((a + b).norm(), 2.0f64.sqrt());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vector3 {
    /// The X component.
    pub x: f64,
    /// The Y component.
    pub y: f64,
    /// The Z component.
    pub z: f64,
}

impl Vector3 {
    /// The zero vector — also [`Vector3::default`].
    pub const ZERO: Self = Self::new(0.0, 0.0, 0.0);
    /// The unit vector along X.
    pub const UNIT_X: Self = Self::new(1.0, 0.0, 0.0);
    /// The unit vector along Y.
    pub const UNIT_Y: Self = Self::new(0.0, 1.0, 0.0);
    /// The unit vector along Z.
    pub const UNIT_Z: Self = Self::new(0.0, 0.0, 1.0);

    /// Builds a vector from its three components.
    #[must_use]
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    /// The dot (scalar) product.
    #[must_use]
    pub fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    /// The cross product `self × other`.
    #[must_use]
    pub fn cross(self, other: Self) -> Self {
        Self::new(
            self.y * other.z - self.z * other.y,
            self.z * other.x - self.x * other.z,
            self.x * other.y - self.y * other.x,
        )
    }

    /// The squared Euclidean norm — cheaper than [`Vector3::norm`] when only
    /// a comparison is needed.
    #[must_use]
    pub fn norm_squared(self) -> f64 {
        self.dot(self)
    }

    /// The Euclidean norm (length).
    #[must_use]
    pub fn norm(self) -> f64 {
        self.norm_squared().sqrt()
    }

    /// `true` when every component is finite (neither `NaN` nor `±∞`).
    #[must_use]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }

    /// Linear interpolation between `self` (`t = 0`) and `other` (`t = 1`).
    ///
    /// `t` is not clamped — a caller that has already established `self`
    /// and `other` bracket the query (as
    /// [`crate::buffer::TransformBuffer::lookup_transform`] does) knows
    /// `t ∈ [0, 1]` without a redundant clamp, and a caller extrapolating
    /// deliberately can pass `t` outside that range.
    ///
    /// ```
    /// use astrs_tf::math::Vector3;
    ///
    /// let start = Vector3::new(0.0, 0.0, 0.0);
    /// let end = Vector3::new(10.0, 0.0, 0.0);
    /// assert_eq!(start.lerp(end, 0.0), start);
    /// assert_eq!(start.lerp(end, 1.0), end);
    /// assert_eq!(start.lerp(end, 0.5), Vector3::new(5.0, 0.0, 0.0));
    /// ```
    #[must_use]
    pub fn lerp(self, other: Self, t: f64) -> Self {
        Self::new(
            self.x + (other.x - self.x) * t,
            self.y + (other.y - self.y) * t,
            self.z + (other.z - self.z) * t,
        )
    }
}

impl Add for Vector3 {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self::new(self.x + rhs.x, self.y + rhs.y, self.z + rhs.z)
    }
}

impl Sub for Vector3 {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        Self::new(self.x - rhs.x, self.y - rhs.y, self.z - rhs.z)
    }
}

impl Neg for Vector3 {
    type Output = Self;

    fn neg(self) -> Self {
        Self::new(-self.x, -self.y, -self.z)
    }
}

impl Mul<f64> for Vector3 {
    type Output = Self;

    /// Scales every component by `rhs`.
    fn mul(self, rhs: f64) -> Self {
        Self::new(self.x * rhs, self.y * rhs, self.z * rhs)
    }
}

impl Vector3 {
    /// Builds this vector's bare columnar representation — a one-row
    /// `Struct{x, y, z}` array — without wrapping it in a [`RecordBatch`].
    ///
    /// [`AstrsMessage::to_record_batch`] wraps this directly;
    /// [`crate::math::isometry::Isometry3`] reuses it to build its own
    /// `translation` column without duplicating the field-construction
    /// logic, the same way [`crate::interop::geometry_msgs`] reuses
    /// `astrs_idl::runtime::ColumnValue` for the ROS-interop types.
    ///
    /// # Errors
    ///
    /// Whatever [`StructArray::try_new_with_len`] rejects — practically
    /// unreachable for this fixed, three-`Float64`-field shape.
    pub(crate) fn to_array_ref(self) -> DataResult<ArrayRef> {
        let DataType::Struct(fields) = Self::data_type() else {
            return Err(DataError::type_mismatch(Self::data_type(), DataType::Null));
        };
        let columns: Vec<ArrayRef> = vec![
            Float64Array::from_values([self.x]).into_array_ref(),
            Float64Array::from_values([self.y]).into_array_ref(),
            Float64Array::from_values([self.z]).into_array_ref(),
        ];
        let strukt = StructArray::try_new_with_len(fields, columns, 1, None)?;
        Ok(strukt.into_array_ref())
    }

    /// Reads a vector back from a one-row `Struct{x, y, z}` array built by
    /// [`Vector3::to_array_ref`] (or an equivalent foreign column).
    ///
    /// # Errors
    ///
    /// [`DataError::DowncastFailed`] when `array` is not a `Struct`,
    /// [`DataError::ColumnCountMismatch`] when it does not have exactly
    /// three columns, or [`DataError::RequiredFieldIsNull`] when a
    /// component is null.
    pub(crate) fn from_array_ref(array: &ArrayRef) -> DataResult<Self> {
        let strukt = array.try_downcast::<StructArray>()?;
        let columns = strukt.columns();
        let component = |index: usize, name: &'static str| -> DataResult<f64> {
            let column = columns.get(index).ok_or(DataError::ColumnCountMismatch {
                fields: 3,
                columns: columns.len(),
            })?;
            column
                .try_downcast::<Float64Array>()?
                .get(0)
                .ok_or_else(|| DataError::RequiredFieldIsNull {
                    field: name.to_owned(),
                    row: 0,
                })
        };
        Ok(Self {
            x: component(0, "x")?,
            y: component(1, "y")?,
            z: component(2, "z")?,
        })
    }
}

impl AstrsMessage for Vector3 {
    const URN: &'static str = "std/geometry/v1/Vector3";

    fn data_type() -> DataType {
        vector3_layout()
    }

    fn to_record_batch(&self) -> DataResult<RecordBatch> {
        Ok(RecordBatch::from_payload(self.to_array_ref()?))
    }

    fn from_record_batch(batch: &RecordBatch) -> DataResult<Self> {
        if batch.num_rows() != 1 {
            return Err(DataError::MessageRowCount {
                actual: batch.num_rows(),
            });
        }
        let column = batch
            .payload_column()
            .ok_or(DataError::ColumnCountMismatch {
                fields: 3,
                columns: 0,
            })?;
        Self::from_array_ref(column)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use proptest::prelude::*;

    #[test]
    fn zero_is_the_default() {
        assert_eq!(Vector3::default(), Vector3::ZERO);
        assert_eq!(Vector3::ZERO, Vector3::new(0.0, 0.0, 0.0));
    }

    #[test]
    fn add_sub_neg_are_componentwise() {
        let a = Vector3::new(1.0, 2.0, 3.0);
        let b = Vector3::new(4.0, 5.0, 6.0);
        assert_eq!(a + b, Vector3::new(5.0, 7.0, 9.0));
        assert_eq!(b - a, Vector3::new(3.0, 3.0, 3.0));
        assert_eq!(-a, Vector3::new(-1.0, -2.0, -3.0));
        assert_eq!(a * 2.0, Vector3::new(2.0, 4.0, 6.0));
    }

    #[test]
    fn dot_of_orthogonal_units_is_zero() {
        assert_eq!(Vector3::UNIT_X.dot(Vector3::UNIT_Y), 0.0);
        assert_eq!(Vector3::UNIT_X.dot(Vector3::UNIT_X), 1.0);
    }

    #[test]
    fn cross_of_x_and_y_is_z() {
        assert_eq!(Vector3::UNIT_X.cross(Vector3::UNIT_Y), Vector3::UNIT_Z);
        assert_eq!(Vector3::UNIT_Y.cross(Vector3::UNIT_X), -Vector3::UNIT_Z);
    }

    #[test]
    fn norm_of_a_3_4_0_triangle_is_5() {
        let v = Vector3::new(3.0, 4.0, 0.0);
        assert_eq!(v.norm_squared(), 25.0);
        assert_eq!(v.norm(), 5.0);
    }

    #[test]
    fn is_finite_rejects_nan_and_infinity() {
        assert!(Vector3::ZERO.is_finite());
        assert!(!Vector3::new(f64::NAN, 0.0, 0.0).is_finite());
        assert!(!Vector3::new(0.0, f64::INFINITY, 0.0).is_finite());
    }

    #[test]
    fn lerp_at_the_endpoints_is_exact() {
        let a = Vector3::new(0.0, 0.0, 0.0);
        let b = Vector3::new(10.0, -4.0, 2.0);
        assert_eq!(a.lerp(b, 0.0), a);
        assert_eq!(a.lerp(b, 1.0), b);
        assert_eq!(a.lerp(b, 0.5), Vector3::new(5.0, -2.0, 1.0));
    }

    #[test]
    fn round_trips_through_a_record_batch() {
        let v = Vector3::new(1.5, -2.5, 3.5);
        let batch = v.to_record_batch().unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(Vector3::from_record_batch(&batch).unwrap(), v);
    }

    #[test]
    fn data_type_matches_the_curated_geometry_layout() {
        assert_eq!(Vector3::data_type(), vector3_layout());
        assert_eq!(Vector3::URN, "std/geometry/v1/Vector3");
    }

    proptest! {
        #[test]
        fn round_trip_holds_for_arbitrary_finite_vectors(
            x in -1e6f64..1e6,
            y in -1e6f64..1e6,
            z in -1e6f64..1e6,
        ) {
            let v = Vector3::new(x, y, z);
            let batch = v.to_record_batch().unwrap();
            prop_assert_eq!(Vector3::from_record_batch(&batch).unwrap(), v);
        }

        #[test]
        fn lerp_stays_within_the_bracketing_interval(
            ax in -100f64..100.0, ay in -100f64..100.0, az in -100f64..100.0,
            bx in -100f64..100.0, by in -100f64..100.0, bz in -100f64..100.0,
            t in 0f64..=1.0,
        ) {
            let a = Vector3::new(ax, ay, az);
            let b = Vector3::new(bx, by, bz);
            let mid = a.lerp(b, t);
            let lo = Vector3::new(ax.min(bx), ay.min(by), az.min(bz));
            let hi = Vector3::new(ax.max(bx), ay.max(by), az.max(bz));
            prop_component_within(mid.x, lo.x, hi.x);
            prop_component_within(mid.y, lo.y, hi.y);
            prop_component_within(mid.z, lo.z, hi.z);
        }
    }

    /// `assert!` helper for the proptest above — floating-point interpolation
    /// can round a hair outside `[lo, hi]` at the extreme, so this allows a
    /// tiny epsilon rather than demanding bit-exact containment.
    fn prop_component_within(value: f64, lo: f64, hi: f64) {
        const EPS: f64 = 1e-9;
        assert!(
            value >= lo - EPS && value <= hi + EPS,
            "{value} not within [{lo}, {hi}]"
        );
    }
}
