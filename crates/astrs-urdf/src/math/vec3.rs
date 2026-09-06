//! [`Vec3`] — a translation, axis, or scale component in `f64`.

use std::ops::{Add, Mul, Neg, Sub};

/// A three-component vector: a translation, a joint axis, or a `<box
/// size="x y z"/>` extent, depending on context.
///
/// This crate's own copy of the same small value `astrs_tf::math::Vector3`
/// is — see this crate's `math` module docs for why URDF's kinematics math
/// is not built directly on `astrs-tf`'s types.
///
/// ```
/// use astrs_urdf::math::Vec3;
///
/// let a = Vec3::new(1.0, 0.0, 0.0);
/// let b = Vec3::new(0.0, 1.0, 0.0);
/// assert_eq!(a.dot(b), 0.0); // orthogonal
/// assert_eq!(a.cross(b), Vec3::UNIT_Z);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec3 {
    /// The X component.
    pub x: f64,
    /// The Y component.
    pub y: f64,
    /// The Z component.
    pub z: f64,
}

impl Vec3 {
    /// The zero vector — also [`Vec3::default`].
    pub const ZERO: Self = Self::new(0.0, 0.0, 0.0);
    /// The unit vector along X — URDF's own default joint axis (an
    /// omitted `<axis>` element means `xyz="1 0 0"`).
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

    /// The squared Euclidean norm — cheaper than [`Vec3::norm`] when only a
    /// comparison is needed.
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

    /// This vector scaled to unit length.
    ///
    /// Returns `None` when the norm is not finite or is too close to zero
    /// to establish a direction — the same `1e-10` floor
    /// `crate::math::Quat::from_axis_angle` uses for a degenerate axis, so
    /// the two agree on what "no direction" means.
    #[must_use]
    pub fn normalize(self) -> Option<Self> {
        let norm = self.norm();
        if !norm.is_finite() || norm < 1e-10 {
            return None;
        }
        Some(self * norm.recip())
    }

    /// Builds two unit vectors `(u, v)` orthogonal to `self` and to each
    /// other, with `u.cross(v) == self` (treated as a unit normal) — an
    /// arbitrary (but deterministic) right-handed orthonormal basis for
    /// the plane perpendicular to `self`.
    ///
    /// This is what gives [`JointKind::Planar`](crate::JointKind::Planar)'s
    /// otherwise-unconstrained in-plane `x`/`y` a concrete meaning relative
    /// to a joint's own axis, which URDF itself does not pin down (the
    /// spec only says a planar joint moves in the plane perpendicular to
    /// `<axis>`, not which in-plane directions `x` and `y` name). For
    /// `self == UNIT_Z` this returns exactly `(UNIT_X, UNIT_Y)`, the plane
    /// a planar joint mounted the conventional way (axis pointing up)
    /// actually moves in.
    ///
    /// Returns `None` under the same degeneracy [`Vec3::normalize`] does —
    /// this treats `self` as the plane's normal, so a zero or non-finite
    /// `self` has no defined perpendicular plane either.
    #[must_use]
    pub fn orthonormal_basis(self) -> Option<(Self, Self)> {
        let normal = self.normalize()?;
        let least_aligned = if normal.x.abs() <= normal.y.abs() && normal.x.abs() <= normal.z.abs()
        {
            Self::UNIT_X
        } else if normal.y.abs() <= normal.z.abs() {
            Self::UNIT_Y
        } else {
            Self::UNIT_Z
        };
        // `least_aligned` is a world unit axis chosen to have the smallest
        // possible component along `normal` (at most 1/sqrt(3) ~ 0.577),
        // so `normal.cross(least_aligned)` is always well clear of zero —
        // `.normalize()` below can only fail on a `self` already refused
        // above. `u = v.cross(normal)` needs no normalization of its own:
        // `v` and `normal` are already orthogonal unit vectors, and the
        // cross product of two orthogonal unit vectors is itself unit
        // length (`|a x b| = |a||b|sin(90°) = 1`) — algebraically
        // guaranteed, not merely typical, so there is no second fallible
        // step to propagate.
        let v = normal.cross(least_aligned).normalize()?;
        let u = v.cross(normal);
        Some((u, v))
    }
}

impl Add for Vec3 {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self::new(self.x + rhs.x, self.y + rhs.y, self.z + rhs.z)
    }
}

impl Sub for Vec3 {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        Self::new(self.x - rhs.x, self.y - rhs.y, self.z - rhs.z)
    }
}

impl Neg for Vec3 {
    type Output = Self;

    fn neg(self) -> Self {
        Self::new(-self.x, -self.y, -self.z)
    }
}

impl Mul<f64> for Vec3 {
    type Output = Self;

    /// Scales every component by `rhs`.
    fn mul(self, rhs: f64) -> Self {
        Self::new(self.x * rhs, self.y * rhs, self.z * rhs)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn zero_is_the_default() {
        assert_eq!(Vec3::default(), Vec3::ZERO);
    }

    #[test]
    fn add_sub_neg_scale_are_componentwise() {
        let a = Vec3::new(1.0, 2.0, 3.0);
        let b = Vec3::new(4.0, 5.0, 6.0);
        assert_eq!(a + b, Vec3::new(5.0, 7.0, 9.0));
        assert_eq!(b - a, Vec3::new(3.0, 3.0, 3.0));
        assert_eq!(-a, Vec3::new(-1.0, -2.0, -3.0));
        assert_eq!(a * 2.0, Vec3::new(2.0, 4.0, 6.0));
    }

    #[test]
    fn cross_of_x_and_y_is_z() {
        assert_eq!(Vec3::UNIT_X.cross(Vec3::UNIT_Y), Vec3::UNIT_Z);
    }

    #[test]
    fn norm_of_a_3_4_0_triangle_is_5() {
        let v = Vec3::new(3.0, 4.0, 0.0);
        assert_eq!(v.norm_squared(), 25.0);
        assert_eq!(v.norm(), 5.0);
    }

    #[test]
    fn is_finite_rejects_nan_and_infinity() {
        assert!(Vec3::ZERO.is_finite());
        assert!(!Vec3::new(f64::NAN, 0.0, 0.0).is_finite());
        assert!(!Vec3::new(0.0, f64::INFINITY, 0.0).is_finite());
    }

    #[test]
    fn normalize_scales_to_unit_length() {
        let v = Vec3::new(3.0, 4.0, 0.0).normalize().unwrap();
        assert!((v.norm() - 1.0).abs() < 1e-12);
        assert!((v - Vec3::new(0.6, 0.8, 0.0)).norm() < 1e-12);
    }

    #[test]
    fn normalize_rejects_the_zero_vector() {
        assert_eq!(Vec3::ZERO.normalize(), None);
    }

    #[test]
    fn normalize_rejects_non_finite_components() {
        assert_eq!(Vec3::new(f64::NAN, 0.0, 0.0).normalize(), None);
    }

    fn assert_is_orthonormal_basis_for(normal: Vec3, u: Vec3, v: Vec3) {
        const EPS: f64 = 1e-9;
        assert!((u.norm() - 1.0).abs() < EPS, "u not unit: {u:?}");
        assert!((v.norm() - 1.0).abs() < EPS, "v not unit: {v:?}");
        assert!(u.dot(v).abs() < EPS, "u, v not orthogonal: {u:?} {v:?}");
        assert!(u.dot(normal).abs() < EPS, "u not perpendicular to normal");
        assert!(v.dot(normal).abs() < EPS, "v not perpendicular to normal");
        assert!(
            (u.cross(v) - normal.normalize().unwrap()).norm() < EPS,
            "(u, v, normal) is not right-handed: u={u:?} v={v:?} normal={normal:?}"
        );
    }

    #[test]
    fn orthonormal_basis_is_perpendicular_to_each_world_axis() {
        for axis in [Vec3::UNIT_X, Vec3::UNIT_Y, Vec3::UNIT_Z] {
            let (u, v) = axis.orthonormal_basis().unwrap();
            assert_is_orthonormal_basis_for(axis, u, v);
        }
    }

    #[test]
    fn orthonormal_basis_of_unit_z_is_the_standard_xy_plane() {
        let (u, v) = Vec3::UNIT_Z.orthonormal_basis().unwrap();
        assert_eq!(u, Vec3::UNIT_X);
        assert_eq!(v, Vec3::UNIT_Y);
    }

    #[test]
    fn orthonormal_basis_handles_an_arbitrary_diagonal_normal() {
        let normal = Vec3::new(1.0, 1.0, 1.0);
        let (u, v) = normal.orthonormal_basis().unwrap();
        assert_is_orthonormal_basis_for(normal, u, v);
    }

    #[test]
    fn orthonormal_basis_is_none_for_the_zero_vector() {
        assert_eq!(Vec3::ZERO.orthonormal_basis(), None);
    }

    #[test]
    fn orthonormal_basis_accepts_a_non_unit_input() {
        let (u, v) = Vec3::new(0.0, 0.0, 5.0).orthonormal_basis().unwrap();
        assert_is_orthonormal_basis_for(Vec3::UNIT_Z, u, v);
    }
}
