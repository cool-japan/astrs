//! [`Transform`] — a rigid-body transform: a rotation composed with a
//! translation, matching `astrs_tf::math::Isometry3`'s exact convention.

use std::ops::Mul;

use super::quat::Quat;
use super::vec3::Vec3;

/// A rigid-body transform (an element of `SE(3)`): a rotation composed with
/// a translation.
///
/// # Convention
///
/// `Transform::new(translation, rotation).transform_point(p)` maps a point
/// expressed in the *source* frame (a URDF joint's child link, or an
/// `<origin>`'s own local frame) to the same point expressed in the
/// *target* frame (the joint's parent link) — identical to
/// `astrs_tf::math::Isometry3`'s "parent ← child" convention (see that
/// type's own docs), so [`crate::kinematics::populate_static_transforms`]
/// can hand a [`Transform`] straight to
/// `astrs_tf::buffer::TransformBuffer::set_transform` with no convention
/// translation beyond the field-level [`super::to_isometry3`] conversion.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Transform {
    /// The translation component.
    pub translation: Vec3,
    /// The rotation component.
    pub rotation: Quat,
}

impl Transform {
    /// The identity transform.
    pub const IDENTITY: Self = Self::new(Vec3::ZERO, Quat::IDENTITY);

    /// Builds a transform from its translation and rotation.
    #[must_use]
    pub const fn new(translation: Vec3, rotation: Quat) -> Self {
        Self {
            translation,
            rotation,
        }
    }

    /// A pure translation, no rotation — [`Vec3::ZERO`] rotation is
    /// [`Quat::IDENTITY`].
    #[must_use]
    pub const fn from_translation(translation: Vec3) -> Self {
        Self::new(translation, Quat::IDENTITY)
    }

    /// A pure rotation about the origin, no translation.
    #[must_use]
    pub const fn from_rotation(rotation: Quat) -> Self {
        Self::new(Vec3::ZERO, rotation)
    }

    /// Composes `self` with `other`: `self ∘ other`. Applying the result to
    /// a point equals applying `other` first, then `self` — also [`Mul`]'s
    /// implementation.
    ///
    /// ```
    /// use astrs_urdf::math::{Transform, Vec3};
    ///
    /// let base_from_link1 = Transform::from_translation(Vec3::new(1.0, 0.0, 0.0));
    /// let link1_from_link2 = Transform::from_translation(Vec3::new(0.0, 1.0, 0.0));
    /// let base_from_link2 = base_from_link1.compose(link1_from_link2);
    /// assert_eq!(base_from_link2.translation, Vec3::new(1.0, 1.0, 0.0));
    /// ```
    #[must_use]
    pub fn compose(self, other: Self) -> Self {
        let rotation = self.rotation * other.rotation;
        let translation = self.translation + self.rotation.rotate_vector(other.translation);
        Self::new(translation, rotation)
    }

    /// Maps a point expressed in this transform's source frame into its
    /// target frame: rotates, then translates.
    #[must_use]
    pub fn transform_point(self, point: Vec3) -> Vec3 {
        self.rotation.rotate_vector(point) + self.translation
    }

    /// `true` when the translation is finite and the rotation is finite —
    /// does not check the rotation is unit length.
    #[must_use]
    pub fn is_finite(self) -> bool {
        self.translation.is_finite() && self.rotation.is_finite()
    }
}

impl Mul for Transform {
    type Output = Self;

    fn mul(self, rhs: Self) -> Self {
        self.compose(rhs)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn identity_is_the_default() {
        assert_eq!(Transform::default(), Transform::IDENTITY);
    }

    #[test]
    fn identity_transform_point_is_a_no_op() {
        let p = Vec3::new(1.0, 2.0, 3.0);
        assert_eq!(Transform::IDENTITY.transform_point(p), p);
    }

    #[test]
    fn compose_matches_sequential_application() {
        let a = Transform::new(
            Vec3::new(1.0, 0.0, 0.0),
            Quat::from_axis_angle(Vec3::UNIT_Z, 0.5),
        );
        let b = Transform::new(
            Vec3::new(0.0, 2.0, 0.0),
            Quat::from_axis_angle(Vec3::UNIT_X, 0.8),
        );
        let p = Vec3::new(3.0, -1.0, 2.0);
        let composed = a.compose(b).transform_point(p);
        let sequential = a.transform_point(b.transform_point(p));
        assert!((composed - sequential).norm() < 1e-9);
    }

    #[test]
    fn a_quarter_turn_about_z_then_translate() {
        let transform = Transform::new(
            Vec3::new(1.0, 0.0, 0.0),
            Quat::from_axis_angle(Vec3::UNIT_Z, std::f64::consts::FRAC_PI_2),
        );
        let result = transform.transform_point(Vec3::new(1.0, 0.0, 0.0));
        assert!((result - Vec3::new(1.0, 1.0, 0.0)).norm() < 1e-9);
    }
}
