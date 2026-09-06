//! [`Joint`] and its children: [`JointLimit`], [`JointDynamics`],
//! [`JointMimic`], [`JointSafetyController`].

use crate::JointKind;
use crate::math::{Transform, Vec3};

/// A `<joint><limit>` element: position, velocity and effort bounds.
///
/// Required by URDF for `revolute`/`prismatic` joints ([`JointKind::has_position_limits`]);
/// optional (and, if present, only `velocity`/`effort` are meaningful) for
/// every other kind — see [`super::Robot::validate`] for what is actually
/// enforced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JointLimit {
    /// The lower position bound, in radians (revolute) or meters
    /// (prismatic).
    pub lower: f64,
    /// The upper position bound.
    pub upper: f64,
    /// The maximum velocity magnitude, in rad/s or m/s.
    pub velocity: f64,
    /// The maximum effort magnitude, in N·m or N.
    pub effort: f64,
}

/// A `<joint><dynamics>` element: the physical properties a dynamics
/// simulator (not this crate) applies at the joint.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct JointDynamics {
    /// Viscous damping coefficient. Defaults to `0.0` when the element
    /// omits the attribute.
    pub damping: f64,
    /// Static (Coulomb) friction. Defaults to `0.0` when the element omits
    /// the attribute.
    pub friction: f64,
}

/// A `<joint><mimic joint="..." multiplier=".." offset=".."/>` element:
/// this joint's position is a fixed affine function of another joint's —
/// `this_position = multiplier * other_position + offset`.
///
/// [`super::Robot::validate`] checks that `joint` names a real,
/// single-degree-of-freedom joint and that no chain of mimic relationships
/// cycles back on itself; [`crate::kinematics::forward_kinematics`] is what
/// actually *applies* the relationship when resolving a joint the caller's
/// position map left unspecified — see that function's docs.
#[derive(Debug, Clone, PartialEq)]
pub struct JointMimic {
    /// The name of the joint this one mimics.
    pub joint: String,
    /// The multiplier applied to the target joint's position. Defaults to
    /// `1.0` when the element omits the attribute.
    pub multiplier: f64,
    /// The offset added after scaling. Defaults to `0.0` when the element
    /// omits the attribute.
    pub offset: f64,
}

/// A `<joint><safety_controller>` element: soft limits and a k-position/
/// k-velocity gain pair, consumed by a real-time controller (not this
/// crate) rather than by kinematics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JointSafetyController {
    /// The lower soft limit. Defaults to `0.0` when the attribute is
    /// omitted (URDF's own default, not necessarily a meaningful bound —
    /// callers that care should check the attribute was actually present
    /// via the raw XML if that distinction matters to them).
    pub soft_lower_limit: f64,
    /// The upper soft limit. Defaults to `0.0` when omitted.
    pub soft_upper_limit: f64,
    /// The position gain applied once past a soft limit.
    pub k_position: f64,
    /// The velocity gain.
    pub k_velocity: f64,
}

/// A `<joint>` element: the kinematic (and, optionally, dynamic)
/// relationship between two [`super::Link`]s.
#[derive(Debug, Clone, PartialEq)]
pub struct Joint {
    /// This joint's name — `<joint name="...">`. Unique within a
    /// [`super::Robot`] (checked by [`super::Robot::validate`]).
    pub name: String,
    /// This joint's kind — `<joint type="...">`.
    pub kind: JointKind,
    /// The name of this joint's parent link — `<parent link="..."/>`.
    pub parent: String,
    /// The name of this joint's child link — `<child link="..."/>`.
    pub child: String,
    /// This joint's frame in its parent link's frame, at the joint's own
    /// zero position — `<origin xyz=".." rpy=".."/>`, identity if omitted.
    /// [`crate::kinematics::forward_kinematics`]'s starting point for this
    /// joint's contribution to a chain: the joint's *motion* (rotation
    /// about, or translation along, [`Joint::axis`]) is composed on top of
    /// this fixed frame, never in place of it.
    pub origin: Transform,
    /// This joint's motion axis, expressed in the joint's own frame (i.e.
    /// after `origin`, not the parent link's frame) — `<axis xyz="x y
    /// z"/>`. Meaningful for `revolute`/`continuous`/`prismatic`
    /// (rotation/translation about this axis) and unused (but still
    /// carried, always [`Vec3::UNIT_X`] if the URDF omitted `<axis>`) for
    /// `fixed`/`floating`/`planar`.
    ///
    /// Always unit length: [`crate::parse`] normalizes whatever `<axis
    /// xyz="..">` gives (URDF explicitly allows a non-unit axis vector,
    /// e.g. `xyz="0 0 2"`, and defines it as meaning the same axis, scaled
    /// — the *direction*, not the magnitude, is what carries meaning), and
    /// rejects a degenerate (zero-length) one outright.
    pub axis: Vec3,
    /// Position/velocity/effort bounds. Required by URDF (and by
    /// [`super::Robot::validate`]) for `revolute`/`prismatic`; optional —
    /// and, when present, only `velocity`/`effort` mean anything — for
    /// every other kind.
    pub limit: Option<JointLimit>,
    /// Damping/friction, if given.
    pub dynamics: Option<JointDynamics>,
    /// A mimic relationship to another joint, if given.
    pub mimic: Option<JointMimic>,
    /// Soft limits and controller gains, if given.
    pub safety_controller: Option<JointSafetyController>,
}

impl Joint {
    /// Builds a joint with `name`/`kind`/`parent`/`child`, identity
    /// origin, the default axis ([`Vec3::UNIT_X`]), and no
    /// limit/dynamics/mimic/safety_controller — the shape a minimal
    /// `<joint name=".." type="..."><parent .../><child .../></joint>`
    /// parses to.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        kind: JointKind,
        parent: impl Into<String>,
        child: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            parent: parent.into(),
            child: child.into(),
            origin: Transform::IDENTITY,
            axis: Vec3::UNIT_X,
            limit: None,
            dynamics: None,
            mimic: None,
            safety_controller: None,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn new_has_identity_origin_and_the_default_axis() {
        let joint = Joint::new("j1", JointKind::Revolute, "a", "b");
        assert_eq!(joint.origin, Transform::IDENTITY);
        assert_eq!(joint.axis, Vec3::UNIT_X);
        assert_eq!(joint.limit, None);
        assert_eq!(joint.dynamics, None);
        assert_eq!(joint.mimic, None);
        assert_eq!(joint.safety_controller, None);
    }

    #[test]
    fn dynamics_default_is_zero_damping_and_friction() {
        let dynamics = JointDynamics::default();
        assert_eq!(dynamics.damping, 0.0);
        assert_eq!(dynamics.friction, 0.0);
    }
}
