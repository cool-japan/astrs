//! Small, self-contained `SE(3)` math for URDF kinematics: [`Vec3`],
//! [`Quat`], [`Transform`].
//!
//! # Why not build directly on `astrs_tf::math`
//!
//! `astrs_tf::math::{Vector3, Quaternion, Isometry3}` already implement
//! this exact algebra, and this module's three types are — deliberately —
//! shaped identically (same field layout, same scalar-last quaternion
//! convention, same `compose`/`rotate_vector` semantics, the same
//! `DEGENERATE_NORM_SQUARED` floor). They are still a separate,
//! from-scratch implementation rather than a re-export, for two reasons:
//!
//! - **Layering.** `astrs_tf::math` types implement [`astrs_data::AstrsMessage`]
//!   (`to_record_batch`/`from_record_batch`), which is the right shape for
//!   a value that crosses the columnar wire but pulls in `astrs-data` for
//!   every intermediate computation an FK solver does internally — a
//!   forward-kinematics sweep over a chain never puts a partial result on
//!   the wire, only the caller-visible per-link output does (via
//!   [`crate::kinematics::populate_static_transforms`]'s conversion at
//!   the boundary). Keeping the *hot path* on a dependency-free type and
//!   paying the `astrs_tf`/`astrs_data` conversion cost only once, at the
//!   edge, matches this workspace's own layering discipline (blueprint
//!   §4.1) rather than working around it.
//! - **Independent verification.** This module's own property tests (see
//!   [`Quat`] and [`Transform`]'s own test modules) are written and
//!   checked against the same invariants `astrs_tf::math`'s tests assert,
//!   independently — a bug shared between a URDF FK solver and the
//!   transform buffer it feeds would be far harder to notice than one
//!   either module's own tests alone would catch.
//!
//! [`to_isometry3`] and [`to_vector3`]/[`from_vector3`] are the
//! crate-internal seams this design implies: every conversion is a plain
//! function rather than a `From`/`Into` impl, matching
//! `astrs_tf::interop::geometry_msgs`'s own house style for the identical
//! situation (a foreign-type boundary where the orphan rule would only let
//! one direction be a trait impl anyway) — see that module's docs for the
//! full rationale. There is no `from_isometry3`/`from_quaternion`: nothing
//! in this crate ever needs to bring an `astrs_tf` value back into this
//! module's own types (kinematics only ever flows the other way, native to
//! `astrs_tf`, at [`crate::kinematics::populate_static_transforms`]'s
//! boundary), so only the directions actually used exist.

mod quat;
mod transform;
mod vec3;

pub use quat::Quat;
pub use transform::Transform;
pub use vec3::Vec3;

/// [`Vec3`] to `astrs_tf::math::Vector3`.
#[must_use]
pub fn to_vector3(v: Vec3) -> astrs_tf::math::Vector3 {
    astrs_tf::math::Vector3::new(v.x, v.y, v.z)
}

/// `astrs_tf::math::Vector3` to [`Vec3`].
#[must_use]
pub fn from_vector3(v: astrs_tf::math::Vector3) -> Vec3 {
    Vec3::new(v.x, v.y, v.z)
}

/// [`Quat`] to `astrs_tf::math::Quaternion` — both are scalar-last
/// `(x, y, z, w)`, so this is a direct field copy.
#[must_use]
pub fn to_quaternion(q: Quat) -> astrs_tf::math::Quaternion {
    astrs_tf::math::Quaternion::new(q.x, q.y, q.z, q.w)
}

/// [`Transform`] to `astrs_tf::math::Isometry3` — both share the same
/// "parent ← child" convention (see [`Transform`]'s own docs), so this is a
/// direct field-by-field conversion, not a re-derivation.
#[must_use]
pub fn to_isometry3(t: Transform) -> astrs_tf::math::Isometry3 {
    astrs_tf::math::Isometry3::new(to_vector3(t.translation), to_quaternion(t.rotation))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn to_vector3_and_from_vector3_round_trip() {
        let v = Vec3::new(1.0, -2.0, 3.5);
        assert_eq!(from_vector3(to_vector3(v)), v);
    }

    #[test]
    fn to_quaternion_copies_fields_in_scalar_last_order() {
        let q = Quat::new(0.1, 0.2, 0.3, 0.9);
        let ros = to_quaternion(q);
        assert_eq!((ros.x, ros.y, ros.z, ros.w), (0.1, 0.2, 0.3, 0.9));
    }

    #[test]
    fn to_isometry3_preserves_the_parent_child_convention() {
        let t = Transform::new(
            Vec3::new(1.0, 0.0, 0.0),
            Quat::from_axis_angle(Vec3::UNIT_Z, std::f64::consts::FRAC_PI_2),
        );
        let iso = to_isometry3(t);
        let native_result = t.transform_point(Vec3::new(1.0, 0.0, 0.0));
        let iso_result = iso.transform_point(to_vector3(Vec3::new(1.0, 0.0, 0.0)));
        assert!((native_result.x - iso_result.x).abs() < 1e-12);
        assert!((native_result.y - iso_result.y).abs() < 1e-12);
        assert!((native_result.z - iso_result.z).abs() < 1e-12);
    }
}
