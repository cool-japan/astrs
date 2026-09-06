//! [`Quaternion`] — a unit quaternion representing an `SO(3)` rotation.
//!
//! Stored `x, y, z, w` (scalar last), the ROS 2 / Eigen convention
//! `astrs_data::urn::layouts::geometry::quaternion_layout` also documents.

use std::ops::Mul;

use astrs_data::array::{ArrayExt, ArrayRef, Float64Array, IntoArrayRef, StructArray};
use astrs_data::urn::layouts::geometry::quaternion_layout;
use astrs_data::{AstrsMessage, DataError, DataType, RecordBatch, Result as DataResult};

use crate::math::vector3::Vector3;

/// Below this squared norm a quaternion is treated as degenerate: it has no
/// well-defined direction to normalize toward. `1e-20` is the square of
/// `1e-10`, itself many orders of magnitude below any `f64` rounding error a
/// legitimately-computed near-unit quaternion could accumulate — this
/// rejects only genuinely zero (or NaN-poisoned) input.
const DEGENERATE_NORM_SQUARED: f64 = 1e-20;

/// Below this dot product, [`Quaternion::slerp`] falls back to a normalized
/// linear interpolation rather than the full spherical formula, because
/// `sin(theta_0)` in the denominator loses precision catastrophically as
/// `theta_0 → 0`. `0.9995` is the threshold most shipped slerp
/// implementations (including this crate's own reference, `oxicar-ros2`)
/// converge on: below one part in `2000` of angular separation the linear
/// and spherical results agree to within `f64` rounding anyway.
const SLERP_LINEAR_FALLBACK_DOT: f64 = 0.9995;

/// A unit quaternion representing a rotation in `SO(3)`.
///
/// Nothing in this type *enforces* unit length on construction — [`Quaternion::new`]
/// accepts any four components, the same way `geometry_msgs/msg/Quaternion`
/// accepts any four `float64`s on the wire. [`Quaternion::normalize`] is the
/// one operation that turns an arbitrary four-vector into a rotation;
/// [`crate::buffer::TransformBuffer::set_transform`] calls it on every
/// incoming rotation so a mildly denormalized publisher (accumulated
/// floating-point drift is common in real tf2 traffic) is corrected rather
/// than rejected, while a genuinely degenerate one (zero norm, `NaN`) is
/// refused.
///
/// ```
/// use astrs_tf::math::{Quaternion, Vector3};
/// use std::f64::consts::FRAC_PI_2;
///
/// // A quarter turn about Z maps the X axis onto the Y axis.
/// let quarter_turn_z = Quaternion::from_axis_angle(Vector3::UNIT_Z, FRAC_PI_2);
/// let rotated = quarter_turn_z.rotate_vector(Vector3::UNIT_X);
/// assert!((rotated - Vector3::UNIT_Y).norm() < 1e-9);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quaternion {
    /// The X component of the vector (imaginary) part.
    pub x: f64,
    /// The Y component of the vector (imaginary) part.
    pub y: f64,
    /// The Z component of the vector (imaginary) part.
    pub z: f64,
    /// The scalar (real) part.
    pub w: f64,
}

impl Quaternion {
    /// The identity rotation: `(0, 0, 0, 1)`.
    ///
    /// [`Quaternion`]'s [`Default`] impl returns this — **not** derived,
    /// since `#[derive(Default)]` would give the all-zero quaternion, which
    /// is degenerate (zero norm) rather than a rotation.
    pub const IDENTITY: Self = Self::new(0.0, 0.0, 0.0, 1.0);

    /// Builds a quaternion from its four components, scalar last.
    #[must_use]
    pub const fn new(x: f64, y: f64, z: f64, w: f64) -> Self {
        Self { x, y, z, w }
    }

    /// The dot product, treating both quaternions as plain 4-vectors.
    #[must_use]
    pub fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y + self.z * other.z + self.w * other.w
    }

    /// The squared norm — cheaper than [`Quaternion::norm`] when only a
    /// comparison (e.g. against `DEGENERATE_NORM_SQUARED`) is needed.
    #[must_use]
    pub fn norm_squared(self) -> f64 {
        self.dot(self)
    }

    /// The norm (length), treating this quaternion as a plain 4-vector.
    #[must_use]
    pub fn norm(self) -> f64 {
        self.norm_squared().sqrt()
    }

    /// `true` when every component is finite (neither `NaN` nor `±∞`).
    #[must_use]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite() && self.w.is_finite()
    }

    /// Normalizes to unit length.
    ///
    /// Returns `None` for a degenerate quaternion — non-finite, or a norm
    /// too close to zero to establish a direction (see
    /// `DEGENERATE_NORM_SQUARED`) — rather than dividing by (near) zero.
    /// This is a plain `Option` rather than a [`crate::error::TfError`]
    /// because this module has no frame-name context to attach to an
    /// error; [`crate::buffer::TransformBuffer::set_transform`] is what
    /// turns a `None` here into [`crate::error::TfError::DegenerateQuaternion`],
    /// naming the child frame the offending transform was for.
    ///
    /// ```
    /// use astrs_tf::math::Quaternion;
    ///
    /// // Mild accumulated drift is corrected, not rejected.
    /// let drifted = Quaternion::new(0.0, 0.0, 0.0, 1.000_001);
    /// let normalized = drifted.normalize().expect("drift is not degenerate");
    /// assert!((normalized.norm() - 1.0).abs() < 1e-9);
    ///
    /// // A genuinely zero-norm quaternion has no direction to normalize.
    /// assert_eq!(Quaternion::new(0.0, 0.0, 0.0, 0.0).normalize(), None);
    /// ```
    #[must_use]
    pub fn normalize(self) -> Option<Self> {
        if !self.is_finite() {
            return None;
        }
        let norm_squared = self.norm_squared();
        if norm_squared < DEGENERATE_NORM_SQUARED {
            return None;
        }
        let inv_norm = norm_squared.sqrt().recip();
        Some(Self::new(
            self.x * inv_norm,
            self.y * inv_norm,
            self.z * inv_norm,
            self.w * inv_norm,
        ))
    }

    /// The conjugate `(-x, -y, -z, w)`.
    ///
    /// Equal to [`Quaternion::inverse`] for any unit quaternion, and much
    /// cheaper (no division) — the operation actually used throughout this
    /// crate, since every rotation flowing through the transform buffer is
    /// normalized on the way in.
    #[must_use]
    pub fn conjugate(self) -> Self {
        Self::new(-self.x, -self.y, -self.z, self.w)
    }

    /// The general multiplicative inverse: `conjugate() / norm_squared()`.
    ///
    /// Correct for a non-unit quaternion, unlike [`Quaternion::conjugate`].
    /// Returns `None` under the same degenerate-norm condition as
    /// [`Quaternion::normalize`].
    #[must_use]
    pub fn inverse(self) -> Option<Self> {
        let norm_squared = self.norm_squared();
        if !norm_squared.is_finite() || norm_squared < DEGENERATE_NORM_SQUARED {
            return None;
        }
        let inv = norm_squared.recip();
        let conj = self.conjugate();
        Some(Self::new(
            conj.x * inv,
            conj.y * inv,
            conj.z * inv,
            conj.w * inv,
        ))
    }

    /// The Hamilton product `self ⊗ other`.
    ///
    /// Composes rotations so that applying the *result* to a vector equals
    /// applying `other`'s rotation first and then `self`'s:
    /// `(self * other).rotate_vector(v) == self.rotate_vector(other.rotate_vector(v))`
    /// (property-tested below). This is also [`Mul`]'s implementation.
    #[must_use]
    pub fn hamilton_product(self, other: Self) -> Self {
        let (px, py, pz, pw) = (self.x, self.y, self.z, self.w);
        let (qx, qy, qz, qw) = (other.x, other.y, other.z, other.w);
        Self::new(
            pw * qx + px * qw + py * qz - pz * qy,
            pw * qy - px * qz + py * qw + pz * qx,
            pw * qz + px * qy - py * qx + pz * qw,
            pw * qw - px * qx - py * qy - pz * qz,
        )
    }

    /// Rotates `v` by this quaternion, treated as unit.
    ///
    /// Implemented via the optimized (no full quaternion-quaternion
    /// product) form `v + 2w(q_xyz × v) + 2 q_xyz × (q_xyz × v)`, which is
    /// algebraically identical to the sandwich product
    /// `self ⊗ (v, 0) ⊗ self.conjugate()` — cross-checked against that
    /// direct form in this module's property tests rather than only
    /// asserted by derivation.
    #[must_use]
    pub fn rotate_vector(self, v: Vector3) -> Vector3 {
        let q_xyz = Vector3::new(self.x, self.y, self.z);
        let t = q_xyz.cross(v) * 2.0;
        v + t * self.w + q_xyz.cross(t)
    }

    /// Spherical linear interpolation from `self` (`t = 0`) to `other`
    /// (`t = 1`), taking the shortest arc.
    ///
    /// Falls back to a normalized linear interpolation when `self` and
    /// `other` are nearly parallel (see `SLERP_LINEAR_FALLBACK_DOT`),
    /// where the spherical formula's `sin(theta_0)` denominator loses
    /// precision. `t` is not clamped, matching [`Vector3::lerp`]'s own
    /// convention.
    ///
    /// ```
    /// use astrs_tf::math::{Quaternion, Vector3};
    /// use std::f64::consts::FRAC_PI_2;
    ///
    /// let start = Quaternion::IDENTITY;
    /// let end = Quaternion::from_axis_angle(Vector3::UNIT_Z, FRAC_PI_2);
    /// let halfway = start.slerp(end, 0.5);
    /// let (_, angle) = halfway.to_axis_angle();
    /// assert!((angle - FRAC_PI_2 / 2.0).abs() < 1e-9);
    /// ```
    #[must_use]
    pub fn slerp(self, other: Self, t: f64) -> Self {
        let mut dot = self.dot(other);
        // Take the shorter arc: `q` and `-q` represent the same rotation,
        // so negate `other` when the quaternions are more than 90 degrees
        // apart as plain 4-vectors.
        let other = if dot < 0.0 {
            dot = -dot;
            Self::new(-other.x, -other.y, -other.z, -other.w)
        } else {
            other
        };
        if dot > SLERP_LINEAR_FALLBACK_DOT {
            return nlerp(self, other, t);
        }
        let theta_0 = dot.clamp(-1.0, 1.0).acos();
        let theta = theta_0 * t;
        let sin_theta_0 = theta_0.sin();
        let s0 = (theta_0 - theta).sin() / sin_theta_0;
        let s1 = theta.sin() / sin_theta_0;
        Self::new(
            s0 * self.x + s1 * other.x,
            s0 * self.y + s1 * other.y,
            s0 * self.z + s1 * other.z,
            s0 * self.w + s1 * other.w,
        )
    }

    /// Builds the rotation of `angle` radians about `axis`.
    ///
    /// `axis` need not be normalized. Returns [`Quaternion::IDENTITY`] if
    /// `axis` is (near) the zero vector, since no rotation axis is then
    /// defined — a rotation by any angle about no axis is a no-op.
    #[must_use]
    pub fn from_axis_angle(axis: Vector3, angle_rad: f64) -> Self {
        let norm = axis.norm();
        if !norm.is_finite() || norm < 1e-10 {
            return Self::IDENTITY;
        }
        let half = angle_rad * 0.5;
        let (sin_half, cos_half) = half.sin_cos();
        let scale = sin_half / norm;
        Self::new(axis.x * scale, axis.y * scale, axis.z * scale, cos_half)
    }

    /// Extracts `(axis, angle_rad)` from this quaternion, treated as unit.
    ///
    /// Returns `(Vector3::UNIT_X, 0.0)` for a rotation indistinguishable
    /// from identity (`w` near `±1`), the same "no axis is defined" case
    /// [`Quaternion::from_axis_angle`] handles on the way in — any axis is
    /// as correct as any other for a zero-angle rotation, so `UNIT_X` is
    /// chosen only for a deterministic return value.
    #[must_use]
    pub fn to_axis_angle(self) -> (Vector3, f64) {
        let w = self.w.clamp(-1.0, 1.0);
        let angle = 2.0 * w.acos();
        let sin_half_sq = 1.0 - w * w;
        if sin_half_sq < 1e-20 {
            return (Vector3::UNIT_X, 0.0);
        }
        let inv_sin_half = sin_half_sq.sqrt().recip();
        (
            Vector3::new(
                self.x * inv_sin_half,
                self.y * inv_sin_half,
                self.z * inv_sin_half,
            ),
            angle,
        )
    }

    /// Builds the rotation for `(roll, pitch, yaw)` radians about
    /// `(X, Y, Z)` — REP 103's body-fixed convention (extrinsic
    /// `Rz(yaw) · Ry(pitch) · Rx(roll)`, the same convention
    /// `tf2::Quaternion::setRPY` implements).
    #[must_use]
    pub fn from_euler_rpy(roll: f64, pitch: f64, yaw: f64) -> Self {
        let (sr, cr) = (roll * 0.5).sin_cos();
        let (sp, cp) = (pitch * 0.5).sin_cos();
        let (sy, cy) = (yaw * 0.5).sin_cos();
        Self::new(
            sr * cp * cy - cr * sp * sy,
            cr * sp * cy + sr * cp * sy,
            cr * cp * sy - sr * sp * cy,
            cr * cp * cy + sr * sp * sy,
        )
    }

    /// Extracts `(roll, pitch, yaw)` radians, the inverse of
    /// [`Quaternion::from_euler_rpy`].
    ///
    /// Treats `self` as unit. `pitch` clamps to `±π/2` at the gimbal-lock
    /// boundary (`|sin(pitch)| ≥ 1`, which only ever exceeds `1` by
    /// rounding error) rather than feeding an out-of-domain value to
    /// `asin`; `roll`/`yaw` are not individually recoverable exactly at
    /// that boundary (a well-known property of Euler angles, not specific
    /// to this implementation), so round-tripping through this pair is
    /// only exact away from it — see this module's property test for the
    /// range that holds over.
    #[must_use]
    pub fn to_euler_rpy(self) -> (f64, f64, f64) {
        let (x, y, z, w) = (self.x, self.y, self.z, self.w);

        let sinr_cosp = 2.0 * (w * x + y * z);
        let cosr_cosp = 1.0 - 2.0 * (x * x + y * y);
        let roll = sinr_cosp.atan2(cosr_cosp);

        let sinp = 2.0 * (w * y - z * x);
        let pitch = if sinp.abs() >= 1.0 {
            std::f64::consts::FRAC_PI_2.copysign(sinp)
        } else {
            sinp.asin()
        };

        let siny_cosp = 2.0 * (w * z + x * y);
        let cosy_cosp = 1.0 - 2.0 * (y * y + z * z);
        let yaw = siny_cosp.atan2(cosy_cosp);

        (roll, pitch, yaw)
    }

    /// The row-major `3x3` rotation matrix `R` such that `R * v ==
    /// self.rotate_vector(v)` for any column vector `v` — `tf2::Matrix3x3`'s
    /// own representation of a rotation, `R[row][col]`.
    ///
    /// Treats `self` as unit; a non-unit `self` produces a scaled (not
    /// orthonormal) matrix, the same caveat [`Quaternion::rotate_vector`]
    /// carries.
    #[must_use]
    pub fn to_rotation_matrix(self) -> [[f64; 3]; 3] {
        let (x, y, z, w) = (self.x, self.y, self.z, self.w);
        let (xx, yy, zz) = (x * x, y * y, z * z);
        let (xy, xz, yz) = (x * y, x * z, y * z);
        let (wx, wy, wz) = (w * x, w * y, w * z);
        [
            [1.0 - 2.0 * (yy + zz), 2.0 * (xy - wz), 2.0 * (xz + wy)],
            [2.0 * (xy + wz), 1.0 - 2.0 * (xx + zz), 2.0 * (yz - wx)],
            [2.0 * (xz - wy), 2.0 * (yz + wx), 1.0 - 2.0 * (xx + yy)],
        ]
    }

    /// Extracts the unit quaternion representing the same rotation as `m`,
    /// the inverse of [`Quaternion::to_rotation_matrix`].
    ///
    /// `m` is assumed orthonormal (a genuine rotation matrix); this does
    /// not validate that. Uses the trace-branching method (Shepperd 1978 /
    /// Bar-Itzhack 2000's numerically stable selection among four
    /// algebraically equivalent extraction formulas — the same algorithm
    /// `Eigen::Quaternion::FromRotationMatrix` and `tf2::Matrix3x3::getRotation`
    /// both implement), which avoids the near-zero divisions a single fixed
    /// formula would hit close to a 180-degree rotation about certain axes.
    /// The result is already unit length (no separate normalization needed)
    /// and, by quaternion double-cover, may differ in overall sign from
    /// whatever quaternion originally produced `m` — both signs represent
    /// the identical rotation.
    #[must_use]
    pub fn from_rotation_matrix(m: [[f64; 3]; 3]) -> Self {
        let trace = m[0][0] + m[1][1] + m[2][2];
        if trace > 0.0 {
            let s = 0.5 / (trace + 1.0).sqrt();
            Self::new(
                (m[2][1] - m[1][2]) * s,
                (m[0][2] - m[2][0]) * s,
                (m[1][0] - m[0][1]) * s,
                0.25 / s,
            )
        } else if m[0][0] > m[1][1] && m[0][0] > m[2][2] {
            let s = 2.0 * (1.0 + m[0][0] - m[1][1] - m[2][2]).sqrt();
            Self::new(
                0.25 * s,
                (m[0][1] + m[1][0]) / s,
                (m[0][2] + m[2][0]) / s,
                (m[2][1] - m[1][2]) / s,
            )
        } else if m[1][1] > m[2][2] {
            let s = 2.0 * (1.0 + m[1][1] - m[0][0] - m[2][2]).sqrt();
            Self::new(
                (m[0][1] + m[1][0]) / s,
                0.25 * s,
                (m[1][2] + m[2][1]) / s,
                (m[0][2] - m[2][0]) / s,
            )
        } else {
            let s = 2.0 * (1.0 + m[2][2] - m[0][0] - m[1][1]).sqrt();
            Self::new(
                (m[0][2] + m[2][0]) / s,
                (m[1][2] + m[2][1]) / s,
                0.25 * s,
                (m[1][0] - m[0][1]) / s,
            )
        }
    }
}

/// Normalized linear interpolation: [`Vector3::lerp`] applied
/// componentwise across all four components, then renormalized.
/// [`Quaternion::slerp`]'s near-parallel fallback.
fn nlerp(a: Quaternion, b: Quaternion, t: f64) -> Quaternion {
    let x = a.x + (b.x - a.x) * t;
    let y = a.y + (b.y - a.y) * t;
    let z = a.z + (b.z - a.z) * t;
    let w = a.w + (b.w - a.w) * t;
    Quaternion::new(x, y, z, w)
        .normalize()
        .unwrap_or(Quaternion::IDENTITY)
}

impl Default for Quaternion {
    /// The identity rotation — **hand-written, not derived**: a derived
    /// `Default` would give `(0, 0, 0, 0)`, which has zero norm and is not
    /// a rotation at all. See [`Quaternion::IDENTITY`].
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Mul for Quaternion {
    type Output = Self;

    fn mul(self, rhs: Self) -> Self {
        self.hamilton_product(rhs)
    }
}

impl Quaternion {
    /// Builds this quaternion's bare columnar representation — a one-row
    /// `Struct{x, y, z, w}` array — without wrapping it in a [`RecordBatch`].
    /// See [`Vector3::to_array_ref`] for why this exists as a crate-internal
    /// helper distinct from [`AstrsMessage::to_record_batch`].
    ///
    /// # Errors
    ///
    /// Whatever [`StructArray::try_new_with_len`] rejects — practically
    /// unreachable for this fixed, four-`Float64`-field shape.
    pub(crate) fn to_array_ref(self) -> DataResult<ArrayRef> {
        let DataType::Struct(fields) = Self::data_type() else {
            return Err(DataError::type_mismatch(Self::data_type(), DataType::Null));
        };
        let columns: Vec<ArrayRef> = vec![
            Float64Array::from_values([self.x]).into_array_ref(),
            Float64Array::from_values([self.y]).into_array_ref(),
            Float64Array::from_values([self.z]).into_array_ref(),
            Float64Array::from_values([self.w]).into_array_ref(),
        ];
        let strukt = StructArray::try_new_with_len(fields, columns, 1, None)?;
        Ok(strukt.into_array_ref())
    }

    /// Reads a quaternion back from a one-row `Struct{x, y, z, w}` array
    /// built by [`Quaternion::to_array_ref`] (or an equivalent foreign
    /// column). Does **not** normalize the result — a foreign column is not
    /// obliged to have been built from a unit quaternion, and this is a
    /// bare decode, not a validating one; see [`Quaternion::normalize`].
    ///
    /// # Errors
    ///
    /// [`DataError::DowncastFailed`] when `array` is not a `Struct`,
    /// [`DataError::ColumnCountMismatch`] when it does not have exactly
    /// four columns, or [`DataError::RequiredFieldIsNull`] when a
    /// component is null.
    pub(crate) fn from_array_ref(array: &ArrayRef) -> DataResult<Self> {
        let strukt = array.try_downcast::<StructArray>()?;
        let columns = strukt.columns();
        let component = |index: usize, name: &'static str| -> DataResult<f64> {
            let column = columns.get(index).ok_or(DataError::ColumnCountMismatch {
                fields: 4,
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
            w: component(3, "w")?,
        })
    }
}

impl AstrsMessage for Quaternion {
    const URN: &'static str = "std/geometry/v1/Quaternion";

    fn data_type() -> DataType {
        quaternion_layout()
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
                fields: 4,
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

    const EPS: f64 = 1e-9;

    fn assert_quat_close(a: Quaternion, b: Quaternion, eps: f64) {
        assert!((a.x - b.x).abs() < eps, "{a:?} vs {b:?}");
        assert!((a.y - b.y).abs() < eps, "{a:?} vs {b:?}");
        assert!((a.z - b.z).abs() < eps, "{a:?} vs {b:?}");
        assert!((a.w - b.w).abs() < eps, "{a:?} vs {b:?}");
    }

    fn assert_vec_close(a: Vector3, b: Vector3, eps: f64) {
        assert!((a - b).norm() < eps, "{a:?} vs {b:?}");
    }

    /// Rotates `v` by `q` via the direct sandwich product
    /// `q ⊗ (v, 0) ⊗ q.conjugate()` — the textbook definition
    /// [`Quaternion::rotate_vector`]'s optimized form is derived from.
    /// Kept test-only (never compiled outside `cfg(test)`) since the two
    /// forms always agree and production code has no reason to prefer the
    /// slower one; see `rotate_vector_matches_the_sandwich_product` and its
    /// property-test counterpart below.
    fn rotate_vector_by_sandwich_product(q: Quaternion, v: Vector3) -> Vector3 {
        let v_quat = Quaternion::new(v.x, v.y, v.z, 0.0);
        let rotated = q.hamilton_product(v_quat).hamilton_product(q.conjugate());
        Vector3::new(rotated.x, rotated.y, rotated.z)
    }

    #[test]
    fn default_and_identity_are_the_unit_quaternion() {
        assert_eq!(Quaternion::default(), Quaternion::IDENTITY);
        assert_eq!(Quaternion::IDENTITY, Quaternion::new(0.0, 0.0, 0.0, 1.0));
        assert_eq!(Quaternion::IDENTITY.norm(), 1.0);
    }

    #[test]
    fn normalize_rejects_zero_and_non_finite() {
        assert_eq!(Quaternion::new(0.0, 0.0, 0.0, 0.0).normalize(), None);
        assert_eq!(Quaternion::new(f64::NAN, 0.0, 0.0, 1.0).normalize(), None);
        assert_eq!(
            Quaternion::new(f64::INFINITY, 0.0, 0.0, 1.0).normalize(),
            None
        );
    }

    #[test]
    fn normalize_corrects_mild_drift() {
        let drifted = Quaternion::new(0.0, 0.0, 0.0, 1.000_001);
        let normalized = drifted.normalize().unwrap();
        assert!((normalized.norm() - 1.0).abs() < EPS);
    }

    #[test]
    fn conjugate_negates_the_vector_part_only() {
        let q = Quaternion::new(1.0, 2.0, 3.0, 4.0);
        assert_eq!(q.conjugate(), Quaternion::new(-1.0, -2.0, -3.0, 4.0));
    }

    #[test]
    fn inverse_of_a_unit_quaternion_equals_its_conjugate() {
        let q = Quaternion::from_axis_angle(Vector3::UNIT_Z, 1.234)
            .normalize()
            .unwrap();
        assert_quat_close(q.inverse().unwrap(), q.conjugate(), EPS);
    }

    #[test]
    fn inverse_is_none_for_the_zero_quaternion() {
        assert_eq!(Quaternion::new(0.0, 0.0, 0.0, 0.0).inverse(), None);
    }

    #[test]
    fn identity_times_anything_is_that_thing() {
        let q = Quaternion::from_axis_angle(Vector3::UNIT_X, 0.7)
            .normalize()
            .unwrap();
        assert_quat_close(Quaternion::IDENTITY * q, q, EPS);
        assert_quat_close(q * Quaternion::IDENTITY, q, EPS);
    }

    #[test]
    fn a_quaternion_times_its_inverse_is_identity() {
        let q = Quaternion::from_axis_angle(Vector3::new(1.0, 2.0, 3.0), 2.1)
            .normalize()
            .unwrap();
        assert_quat_close(q * q.inverse().unwrap(), Quaternion::IDENTITY, EPS);
    }

    #[test]
    fn rotate_vector_matches_the_sandwich_product() {
        let q = Quaternion::from_axis_angle(Vector3::new(1.0, 1.0, 0.0), 1.1)
            .normalize()
            .unwrap();
        let v = Vector3::new(3.0, -2.0, 5.0);
        assert_vec_close(
            q.rotate_vector(v),
            rotate_vector_by_sandwich_product(q, v),
            EPS,
        );
    }

    #[test]
    fn identity_rotation_is_a_no_op() {
        let v = Vector3::new(1.0, 2.0, 3.0);
        assert_eq!(Quaternion::IDENTITY.rotate_vector(v), v);
    }

    #[test]
    fn a_quarter_turn_about_z_maps_x_to_y() {
        let q = Quaternion::from_axis_angle(Vector3::UNIT_Z, std::f64::consts::FRAC_PI_2);
        assert_vec_close(q.rotate_vector(Vector3::UNIT_X), Vector3::UNIT_Y, EPS);
    }

    #[test]
    fn composed_rotation_matches_sequential_application() {
        let q1 = Quaternion::from_axis_angle(Vector3::UNIT_X, 0.4);
        let q2 = Quaternion::from_axis_angle(Vector3::UNIT_Y, 0.9);
        let v = Vector3::new(1.0, -1.0, 2.0);
        let composed = (q1 * q2).rotate_vector(v);
        let sequential = q1.rotate_vector(q2.rotate_vector(v));
        assert_vec_close(composed, sequential, EPS);
    }

    #[test]
    fn slerp_at_the_endpoints_returns_the_endpoints() {
        let a = Quaternion::from_axis_angle(Vector3::UNIT_X, 0.1);
        let b = Quaternion::from_axis_angle(Vector3::UNIT_Y, 1.7);
        assert_quat_close(a.slerp(b, 0.0), a, EPS);
        assert_quat_close(a.slerp(b, 1.0), b, EPS);
    }

    #[test]
    fn slerp_halfway_between_identity_and_a_right_angle_is_a_right_angle_quarter() {
        let a = Quaternion::IDENTITY;
        let b = Quaternion::from_axis_angle(Vector3::UNIT_Z, std::f64::consts::FRAC_PI_2);
        let mid = a.slerp(b, 0.5);
        let (_, angle) = mid.to_axis_angle();
        assert!((angle - std::f64::consts::FRAC_PI_4).abs() < 1e-9);
    }

    #[test]
    fn axis_angle_round_trips() {
        let axis = Vector3::new(1.0, 2.0, -1.0);
        let angle = 1.3;
        let q = Quaternion::from_axis_angle(axis, angle);
        let (out_axis, out_angle) = q.to_axis_angle();
        assert!((out_angle - angle).abs() < EPS);
        assert_vec_close(out_axis * axis.norm(), axis, 1e-6);
    }

    #[test]
    fn zero_axis_is_identity() {
        assert_eq!(
            Quaternion::from_axis_angle(Vector3::ZERO, 3.0),
            Quaternion::IDENTITY
        );
    }

    #[test]
    fn euler_round_trips_away_from_gimbal_lock() {
        let (roll, pitch, yaw) = (0.3, 0.4, -0.9);
        let q = Quaternion::from_euler_rpy(roll, pitch, yaw);
        let (out_roll, out_pitch, out_yaw) = q.to_euler_rpy();
        assert!((out_roll - roll).abs() < EPS);
        assert!((out_pitch - pitch).abs() < EPS);
        assert!((out_yaw - yaw).abs() < EPS);
    }

    #[test]
    fn euler_identity_is_zero_zero_zero() {
        assert_eq!(
            Quaternion::from_euler_rpy(0.0, 0.0, 0.0),
            Quaternion::IDENTITY
        );
        assert_eq!(Quaternion::IDENTITY.to_euler_rpy(), (0.0, 0.0, 0.0));
    }

    #[test]
    fn identity_rotation_matrix_is_the_identity_matrix() {
        assert_eq!(
            Quaternion::IDENTITY.to_rotation_matrix(),
            [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
        );
    }

    #[test]
    fn rotation_matrix_matches_rotate_vector() {
        let q = Quaternion::from_axis_angle(Vector3::new(1.0, 2.0, -1.0), 1.1);
        let m = q.to_rotation_matrix();
        for v in [
            Vector3::UNIT_X,
            Vector3::UNIT_Y,
            Vector3::UNIT_Z,
            Vector3::new(1.0, -2.0, 3.0),
        ] {
            let by_matrix = Vector3::new(
                m[0][0] * v.x + m[0][1] * v.y + m[0][2] * v.z,
                m[1][0] * v.x + m[1][1] * v.y + m[1][2] * v.z,
                m[2][0] * v.x + m[2][1] * v.y + m[2][2] * v.z,
            );
            assert_vec_close(by_matrix, q.rotate_vector(v), EPS);
        }
    }

    #[test]
    fn a_quarter_turn_about_z_matrix_matches_the_textbook_form() {
        let q = Quaternion::from_axis_angle(Vector3::UNIT_Z, std::f64::consts::FRAC_PI_2);
        let m = q.to_rotation_matrix();
        let expected = [[0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]];
        for row in 0..3 {
            for col in 0..3 {
                assert!(
                    (m[row][col] - expected[row][col]).abs() < EPS,
                    "m={m:?} expected={expected:?}"
                );
            }
        }
    }

    #[test]
    fn rotation_matrix_round_trips_through_from_rotation_matrix() {
        let q = Quaternion::from_axis_angle(Vector3::new(0.3, 0.7, -0.2), 2.0);
        let m = q.to_rotation_matrix();
        let back = Quaternion::from_rotation_matrix(m);
        // Double-cover: `back` may be `q` or `-q`; compare via `|dot|`.
        assert!((back.dot(q).abs() - 1.0).abs() < EPS);
        assert_vec_close(
            back.rotate_vector(Vector3::UNIT_X),
            q.rotate_vector(Vector3::UNIT_X),
            EPS,
        );
    }

    #[test]
    fn from_rotation_matrix_recovers_identity() {
        let identity_matrix = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        let q = Quaternion::from_rotation_matrix(identity_matrix);
        assert!((q.dot(Quaternion::IDENTITY).abs() - 1.0).abs() < EPS);
    }

    #[test]
    fn round_trips_through_a_record_batch() {
        let q = Quaternion::from_axis_angle(Vector3::new(1.0, 0.0, 1.0), 0.6);
        let batch = q.to_record_batch().unwrap();
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(Quaternion::from_record_batch(&batch).unwrap(), q);
    }

    #[test]
    fn data_type_matches_the_curated_geometry_layout() {
        assert_eq!(Quaternion::data_type(), quaternion_layout());
        assert_eq!(Quaternion::URN, "std/geometry/v1/Quaternion");
    }

    proptest! {
        /// Quaternion normalization invariant: whenever normalization
        /// succeeds, the result has unit norm (within tolerance) and is
        /// finite — one of this crate's five mandated property tests.
        #[test]
        fn normalize_always_yields_unit_norm_when_it_succeeds(
            x in -1e6f64..1e6, y in -1e6f64..1e6, z in -1e6f64..1e6, w in -1e6f64..1e6,
        ) {
            if let Some(normalized) = Quaternion::new(x, y, z, w).normalize() {
                prop_assert!(normalized.is_finite());
                prop_assert!((normalized.norm() - 1.0).abs() < 1e-9);
            }
        }

        /// Composed-rotation identity, generalized across random axes,
        /// angles and vectors: `(q1 * q2).rotate_vector(v) ==
        /// q1.rotate_vector(q2.rotate_vector(v))`. This is what makes
        /// `Isometry3` composition associative (see `math::isometry`'s own
        /// property test), so it is checked directly at the quaternion
        /// level too.
        #[test]
        fn hamilton_product_composes_rotations(
            ax in -1f64..1.0, ay in -1f64..1.0, az in -1f64..1.0, angle1 in -6.3f64..6.3,
            bx in -1f64..1.0, by in -1f64..1.0, bz in -1f64..1.0, angle2 in -6.3f64..6.3,
            vx in -10f64..10.0, vy in -10f64..10.0, vz in -10f64..10.0,
        ) {
            let axis1 = Vector3::new(ax, ay, az);
            let axis2 = Vector3::new(bx, by, bz);
            prop_assume!(axis1.norm() > 1e-6 && axis2.norm() > 1e-6);
            let q1 = Quaternion::from_axis_angle(axis1, angle1);
            let q2 = Quaternion::from_axis_angle(axis2, angle2);
            let v = Vector3::new(vx, vy, vz);
            let composed = (q1 * q2).rotate_vector(v);
            let sequential = q1.rotate_vector(q2.rotate_vector(v));
            prop_assert!((composed - sequential).norm() < 1e-6);
        }

        /// The optimized [`Quaternion::rotate_vector`] formula always
        /// agrees with the direct sandwich product it is derived from.
        #[test]
        fn rotate_vector_matches_sandwich_product_for_arbitrary_input(
            ax in -1f64..1.0, ay in -1f64..1.0, az in -1f64..1.0, angle in -6.3f64..6.3,
            vx in -10f64..10.0, vy in -10f64..10.0, vz in -10f64..10.0,
        ) {
            let axis = Vector3::new(ax, ay, az);
            prop_assume!(axis.norm() > 1e-6);
            let q = Quaternion::from_axis_angle(axis, angle);
            let v = Vector3::new(vx, vy, vz);
            prop_assert!(
                (q.rotate_vector(v) - rotate_vector_by_sandwich_product(q, v)).norm() < 1e-9
            );
        }

        /// [`Quaternion::slerp`] never leaves the unit sphere and always
        /// represents a rotation angle between the two endpoints' angular
        /// separation — the "interpolation stays inside its bracketing
        /// interval" property applied to rotation.
        #[test]
        fn slerp_stays_on_the_unit_sphere(
            ax in -1f64..1.0, ay in -1f64..1.0, az in -1f64..1.0, angle_a in -3.0f64..3.0,
            bx in -1f64..1.0, by in -1f64..1.0, bz in -1f64..1.0, angle_b in -3.0f64..3.0,
            t in 0f64..=1.0,
        ) {
            let axis_a = Vector3::new(ax, ay, az);
            let axis_b = Vector3::new(bx, by, bz);
            prop_assume!(axis_a.norm() > 1e-6 && axis_b.norm() > 1e-6);
            let a = Quaternion::from_axis_angle(axis_a, angle_a);
            let b = Quaternion::from_axis_angle(axis_b, angle_b);
            let mid = a.slerp(b, t);
            prop_assert!((mid.norm() - 1.0).abs() < 1e-6);
        }

        /// Euler round trip holds away from the pitch = +/- pi/2 gimbal
        /// lock boundary, where roll/yaw are not individually recoverable
        /// (a property of Euler angles, not this implementation).
        #[test]
        fn euler_round_trip_holds_away_from_gimbal_lock(
            roll in -3.0f64..3.0,
            pitch in -1.5f64..1.5,
            yaw in -3.0f64..3.0,
        ) {
            let q = Quaternion::from_euler_rpy(roll, pitch, yaw);
            let (out_roll, out_pitch, out_yaw) = q.to_euler_rpy();
            prop_assert!((out_roll - roll).abs() < 1e-6);
            prop_assert!((out_pitch - pitch).abs() < 1e-6);
            prop_assert!((out_yaw - yaw).abs() < 1e-6);
        }

        #[test]
        fn round_trip_holds_through_a_record_batch(
            ax in -1f64..1.0, ay in -1f64..1.0, az in -1f64..1.0, angle in -6.3f64..6.3,
        ) {
            let axis = Vector3::new(ax, ay, az);
            prop_assume!(axis.norm() > 1e-6);
            let q = Quaternion::from_axis_angle(axis, angle);
            let batch = q.to_record_batch().unwrap();
            prop_assert_eq!(Quaternion::from_record_batch(&batch).unwrap(), q);
        }

        /// `to_rotation_matrix` always agrees with `rotate_vector` (the
        /// primary, independently cross-checked rotation implementation),
        /// and `from_rotation_matrix` always recovers a quaternion
        /// representing the identical rotation.
        #[test]
        fn rotation_matrix_conversions_are_correct_for_arbitrary_rotations(
            ax in -1f64..1.0, ay in -1f64..1.0, az in -1f64..1.0, angle in -6.3f64..6.3,
            vx in -10f64..10.0, vy in -10f64..10.0, vz in -10f64..10.0,
        ) {
            let axis = Vector3::new(ax, ay, az);
            prop_assume!(axis.norm() > 1e-6);
            let q = Quaternion::from_axis_angle(axis, angle);
            let v = Vector3::new(vx, vy, vz);

            let m = q.to_rotation_matrix();
            let by_matrix = Vector3::new(
                m[0][0] * v.x + m[0][1] * v.y + m[0][2] * v.z,
                m[1][0] * v.x + m[1][1] * v.y + m[1][2] * v.z,
                m[2][0] * v.x + m[2][1] * v.y + m[2][2] * v.z,
            );
            prop_assert!((by_matrix - q.rotate_vector(v)).norm() < 1e-6);

            let recovered = Quaternion::from_rotation_matrix(m);
            prop_assert!((recovered.rotate_vector(v) - q.rotate_vector(v)).norm() < 1e-6);
        }
    }
}
