//! [`Quat`] — a unit quaternion representing an `SO(3)` rotation, `x, y, z,
//! w` (scalar last) — the same layout `astrs_tf::math::Quaternion` uses.

use std::ops::Mul;

use super::vec3::Vec3;

/// Below this squared norm a quaternion is treated as degenerate: no
/// well-defined direction to normalize toward. Mirrors
/// `astrs_tf::math::quaternion`'s own `DEGENERATE_NORM_SQUARED` (`1e-20`,
/// the square of `1e-10`) and its rationale: many orders of magnitude below
/// any `f64` rounding error a legitimately-computed near-unit quaternion
/// could accumulate.
const DEGENERATE_NORM_SQUARED: f64 = 1e-20;

/// A unit quaternion representing a rotation in `SO(3)`.
///
/// ```
/// use astrs_urdf::math::{Quat, Vec3};
/// use std::f64::consts::FRAC_PI_2;
///
/// let quarter_turn_z = Quat::from_axis_angle(Vec3::UNIT_Z, FRAC_PI_2);
/// let rotated = quarter_turn_z.rotate_vector(Vec3::UNIT_X);
/// assert!((rotated - Vec3::UNIT_Y).norm() < 1e-9);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quat {
    /// The X component of the vector (imaginary) part.
    pub x: f64,
    /// The Y component of the vector (imaginary) part.
    pub y: f64,
    /// The Z component of the vector (imaginary) part.
    pub z: f64,
    /// The scalar (real) part.
    pub w: f64,
}

impl Quat {
    /// The identity rotation: `(0, 0, 0, 1)`.
    ///
    /// Hand-written as [`Quat`]'s [`Default`], not derived — a derived
    /// `Default` would give the all-zero (degenerate, non-unit)
    /// quaternion; see `astrs_tf::math::Quaternion::IDENTITY`'s identical
    /// rationale.
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

    /// The squared norm.
    #[must_use]
    pub fn norm_squared(self) -> f64 {
        self.dot(self)
    }

    /// The norm (length).
    #[must_use]
    pub fn norm(self) -> f64 {
        self.norm_squared().sqrt()
    }

    /// `true` when every component is finite.
    #[must_use]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite() && self.w.is_finite()
    }

    /// Normalizes to unit length; `None` for a degenerate (non-finite or
    /// near-zero-norm) quaternion.
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

    /// The conjugate `(-x, -y, -z, w)` — the inverse rotation for any unit
    /// quaternion.
    #[must_use]
    pub fn conjugate(self) -> Self {
        Self::new(-self.x, -self.y, -self.z, self.w)
    }

    /// The Hamilton product `self ⊗ other`: composes rotations so that
    /// `(self * other).rotate_vector(v) ==
    /// self.rotate_vector(other.rotate_vector(v))`. Also [`Mul`]'s
    /// implementation.
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

    /// Rotates `v` by this quaternion, treated as unit — the optimized
    /// `v + 2w(q_xyz × v) + 2 q_xyz × (q_xyz × v)` form, algebraically
    /// identical to the sandwich product `self ⊗ (v, 0) ⊗
    /// self.conjugate()`.
    #[must_use]
    pub fn rotate_vector(self, v: Vec3) -> Vec3 {
        let q_xyz = Vec3::new(self.x, self.y, self.z);
        let t = q_xyz.cross(v) * 2.0;
        v + t * self.w + q_xyz.cross(t)
    }

    /// Builds the rotation of `angle_rad` radians about `axis` — URDF
    /// `<joint>` kinematics' primary constructor (`<axis>` plus a scalar
    /// joint position). `axis` need not be normalized; returns
    /// [`Quat::IDENTITY`] if `axis` is (near) the zero vector, since no
    /// rotation axis is then defined.
    #[must_use]
    pub fn from_axis_angle(axis: Vec3, angle_rad: f64) -> Self {
        let Some(unit_axis) = axis.normalize() else {
            return Self::IDENTITY;
        };
        let half = angle_rad * 0.5;
        let (sin_half, cos_half) = half.sin_cos();
        Self::new(
            unit_axis.x * sin_half,
            unit_axis.y * sin_half,
            unit_axis.z * sin_half,
            cos_half,
        )
    }

    /// Builds the rotation for `(roll, pitch, yaw)` radians about `(X, Y,
    /// Z)` — URDF `<origin rpy="r p y"/>`'s own convention, REP 103's
    /// body-fixed extrinsic `Rz(yaw) · Ry(pitch) · Rx(roll)` (identical to
    /// `astrs_tf::math::Quaternion::from_euler_rpy`).
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
}

impl Default for Quat {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Mul for Quat {
    type Output = Self;

    fn mul(self, rhs: Self) -> Self {
        self.hamilton_product(rhs)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use proptest::prelude::*;

    const EPS: f64 = 1e-9;

    fn assert_vec_close(a: Vec3, b: Vec3, eps: f64) {
        assert!((a - b).norm() < eps, "{a:?} vs {b:?}");
    }

    #[test]
    fn default_and_identity_are_the_unit_quaternion() {
        assert_eq!(Quat::default(), Quat::IDENTITY);
        assert_eq!(Quat::IDENTITY, Quat::new(0.0, 0.0, 0.0, 1.0));
    }

    #[test]
    fn normalize_rejects_zero_and_non_finite() {
        assert_eq!(Quat::new(0.0, 0.0, 0.0, 0.0).normalize(), None);
        assert_eq!(Quat::new(f64::NAN, 0.0, 0.0, 1.0).normalize(), None);
    }

    #[test]
    fn identity_rotation_is_a_no_op() {
        let v = Vec3::new(1.0, 2.0, 3.0);
        assert_eq!(Quat::IDENTITY.rotate_vector(v), v);
    }

    #[test]
    fn a_quarter_turn_about_z_maps_x_to_y() {
        let q = Quat::from_axis_angle(Vec3::UNIT_Z, std::f64::consts::FRAC_PI_2);
        assert_vec_close(q.rotate_vector(Vec3::UNIT_X), Vec3::UNIT_Y, EPS);
    }

    #[test]
    fn a_half_turn_about_x_maps_y_to_negative_y() {
        let q = Quat::from_axis_angle(Vec3::UNIT_X, std::f64::consts::PI);
        assert_vec_close(q.rotate_vector(Vec3::UNIT_Y), -Vec3::UNIT_Y, EPS);
    }

    #[test]
    fn composed_rotation_matches_sequential_application() {
        let q1 = Quat::from_axis_angle(Vec3::UNIT_X, 0.4);
        let q2 = Quat::from_axis_angle(Vec3::UNIT_Y, 0.9);
        let v = Vec3::new(1.0, -1.0, 2.0);
        let composed = (q1 * q2).rotate_vector(v);
        let sequential = q1.rotate_vector(q2.rotate_vector(v));
        assert_vec_close(composed, sequential, EPS);
    }

    #[test]
    fn zero_axis_is_identity() {
        assert_eq!(Quat::from_axis_angle(Vec3::ZERO, 3.0), Quat::IDENTITY);
    }

    #[test]
    fn a_non_unit_axis_still_produces_a_unit_quaternion() {
        let q = Quat::from_axis_angle(Vec3::new(0.0, 0.0, 5.0), std::f64::consts::FRAC_PI_2);
        assert!((q.norm() - 1.0).abs() < EPS);
        assert_vec_close(q.rotate_vector(Vec3::UNIT_X), Vec3::UNIT_Y, EPS);
    }

    #[test]
    fn euler_identity_is_zero_zero_zero() {
        assert_eq!(Quat::from_euler_rpy(0.0, 0.0, 0.0), Quat::IDENTITY);
    }

    #[test]
    fn euler_yaw_only_matches_axis_angle_about_z() {
        let by_euler = Quat::from_euler_rpy(0.0, 0.0, std::f64::consts::FRAC_PI_2);
        let by_axis_angle = Quat::from_axis_angle(Vec3::UNIT_Z, std::f64::consts::FRAC_PI_2);
        assert_vec_close(
            by_euler.rotate_vector(Vec3::UNIT_X),
            by_axis_angle.rotate_vector(Vec3::UNIT_X),
            EPS,
        );
    }

    proptest! {
        #[test]
        fn rotate_vector_matches_the_sandwich_product(
            ax in -1f64..1.0, ay in -1f64..1.0, az in -1f64..1.0, angle in -6.3f64..6.3,
            vx in -10f64..10.0, vy in -10f64..10.0, vz in -10f64..10.0,
        ) {
            let axis = Vec3::new(ax, ay, az);
            prop_assume!(axis.norm() > 1e-6);
            let q = Quat::from_axis_angle(axis, angle);
            let v = Vec3::new(vx, vy, vz);

            let v_quat = Quat::new(v.x, v.y, v.z, 0.0);
            let sandwich = q.hamilton_product(v_quat).hamilton_product(q.conjugate());
            let sandwich_vec = Vec3::new(sandwich.x, sandwich.y, sandwich.z);

            prop_assert!((q.rotate_vector(v) - sandwich_vec).norm() < 1e-9);
        }

        #[test]
        fn from_axis_angle_always_yields_a_unit_quaternion(
            ax in -1f64..1.0, ay in -1f64..1.0, az in -1f64..1.0, angle in -6.3f64..6.3,
        ) {
            let axis = Vec3::new(ax, ay, az);
            prop_assume!(axis.norm() > 1e-6);
            let q = Quat::from_axis_angle(axis, angle);
            prop_assert!((q.norm() - 1.0).abs() < 1e-9);
        }
    }
}
