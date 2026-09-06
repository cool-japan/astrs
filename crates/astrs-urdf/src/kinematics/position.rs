//! [`JointPosition`] — one joint's instantaneous configuration, shaped to
//! match [`crate::JointKind::degrees_of_freedom`] exactly.

use crate::JointKind;
use crate::math::Transform;

/// One joint's position, in whatever shape its [`JointKind`] actually
/// needs — [`forward_kinematics`](super::forward_kinematics) rejects a
/// mismatch (a [`JointPosition::Scalar`] given for a
/// [`JointKind::Planar`] joint, say) as
/// [`crate::UrdfError::JointPositionKindMismatch`] rather than silently
/// reinterpreting it, since there is no principled way to turn one shape
/// into another.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JointPosition {
    /// [`JointKind::Fixed`] (0 DOF) — the only value a fixed joint's
    /// position can be.
    Fixed,
    /// [`JointKind::Revolute`]/[`JointKind::Continuous`] (an angle in
    /// radians) or [`JointKind::Prismatic`] (a displacement in meters) — 1
    /// DOF, a single number along [`crate::model::Joint::axis`].
    Scalar(f64),
    /// [`JointKind::Planar`] (3 DOF): `x`/`y` are in-plane translation
    /// coordinates and `theta` a rotation, all relative to the local basis
    /// [`forward_kinematics`](super::forward_kinematics) builds from
    /// [`crate::model::Joint::axis`] — see that function's docs for
    /// exactly how the basis is constructed, since URDF itself does not
    /// pin one down.
    Planar {
        /// In-plane translation along the basis's first axis.
        x: f64,
        /// In-plane translation along the basis's second axis.
        y: f64,
        /// Rotation about `axis` itself, in radians.
        theta: f64,
    },
    /// [`JointKind::Floating`] (6 DOF): unconstrained motion, represented
    /// directly as the [`Transform`] it contributes (relative to the
    /// joint's own `origin`) — packing 6 free numbers into a translation
    /// plus a unit quaternion rather than, say, a translation plus Euler
    /// angles, the same representation choice
    /// [`crate::math::Transform`] itself makes throughout this crate (see
    /// that type's own docs) and for the identical reason: no gimbal-lock
    /// singularity.
    Floating(Transform),
}

impl JointPosition {
    /// The zero (home) position for a joint of `kind` — what
    /// [`forward_kinematics`](super::forward_kinematics) uses for any
    /// joint the caller's position map leaves unspecified (see that
    /// function's own docs on why an unspecified joint defaults rather
    /// than errors).
    #[must_use]
    pub const fn zero(kind: JointKind) -> Self {
        match kind {
            JointKind::Fixed => Self::Fixed,
            JointKind::Revolute | JointKind::Continuous | JointKind::Prismatic => Self::Scalar(0.0),
            JointKind::Planar => Self::Planar {
                x: 0.0,
                y: 0.0,
                theta: 0.0,
            },
            JointKind::Floating => Self::Floating(Transform::IDENTITY),
        }
    }

    /// How many independent numbers this value carries — matched against
    /// [`JointKind::degrees_of_freedom`] to detect a shape mismatch.
    #[must_use]
    pub const fn degrees_of_freedom(self) -> usize {
        match self {
            Self::Fixed => 0,
            Self::Scalar(_) => 1,
            Self::Planar { .. } => 3,
            Self::Floating(_) => 6,
        }
    }

    /// The scalar value, if this is [`JointPosition::Scalar`] — the shape
    /// mimic resolution needs (`multiplier * position + offset` is only
    /// ever meaningful for a single-DOF joint;
    /// [`crate::model::Robot::validate`] already rejects a `<mimic>` on
    /// anything else).
    #[must_use]
    pub const fn as_scalar(self) -> Option<f64> {
        match self {
            Self::Scalar(value) => Some(value),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn zero_matches_each_kinds_own_degree_of_freedom_count() {
        for kind in [
            JointKind::Revolute,
            JointKind::Continuous,
            JointKind::Prismatic,
            JointKind::Fixed,
            JointKind::Floating,
            JointKind::Planar,
        ] {
            assert_eq!(
                JointPosition::zero(kind).degrees_of_freedom(),
                kind.degrees_of_freedom(),
                "{kind}"
            );
        }
    }

    #[test]
    fn as_scalar_extracts_only_from_the_scalar_variant() {
        assert_eq!(JointPosition::Scalar(1.5).as_scalar(), Some(1.5));
        assert_eq!(JointPosition::Fixed.as_scalar(), None);
        assert_eq!(
            JointPosition::Planar {
                x: 0.0,
                y: 0.0,
                theta: 0.0
            }
            .as_scalar(),
            None
        );
        assert_eq!(
            JointPosition::Floating(Transform::IDENTITY).as_scalar(),
            None
        );
    }
}
