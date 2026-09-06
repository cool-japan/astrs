//! [`ArmState`] — an articulated arm's live joint state, driving
//! [`astrs_urdf`]'s forward kinematics.
//!
//! # A library capability, deliberately not wired to the sim node
//!
//! [`crate::World`] and the `astrs-sim-node` binary (see the crate's own
//! top-level docs for the full node/dataflow picture) model exactly one
//! moving body: a differential-drive base, publishing `scan`/`odom`/`tf`.
//! [`ArmState`] is a *separate* capability this crate also provides —
//! §5.3's "articulated arms via astrs-urdf FK" — for an embedding
//! application or tutorial that wants a manipulator, not a wheeled base.
//! It is not merely unfinished integration; it is a deliberate scope
//! boundary, for a concrete reason: [`ArmState::link_transforms`] can
//! produce one transform *per link* — an arbitrary, URDF-dependent count
//! known only once a robot description is loaded — and there is no
//! `std/tf/v1` batched-transform wire type in the registry to carry more
//! than one at a time (§24.3 defines `std/geometry/v1/Transform` as a
//! single rigid-body transform, full stop). Inventing one is a wire-schema
//! change this crate does not have standing to make unilaterally.
//! [`ArmState::populate_transform_buffer`] is therefore this capability's
//! actual boundary: it hands a caller a live-updated
//! [`astrs_tf::buffer::TransformBuffer`] they can query locally
//! (`lookup_transform`, exactly as [`astrs_urdf`]'s own docs describe as
//! the natural next layer above forward kinematics), rather than a set of
//! ports this crate cannot honestly publish yet.

use std::collections::HashMap;

use astrs_tf::TfStamp;
use astrs_tf::buffer::TransformBuffer;
use astrs_urdf::kinematics::{JointPosition, Topology, forward_kinematics};
use astrs_urdf::math::{Transform, to_isometry3};
use astrs_urdf::{Robot, UrdfError};

use crate::error::{Result, SimError};

/// A [`Robot`] plus the live scalar position of whichever of its joints a
/// caller has set — everything [`astrs_urdf::kinematics::forward_kinematics`]
/// needs to place every link.
///
/// Only scalar (1-degree-of-freedom: revolute/continuous/prismatic)
/// joints can be driven through [`ArmState::set_joint_position`] — the
/// overwhelming majority of manipulator joints, and the only kind a single
/// `f64` can describe. A [`astrs_urdf::JointKind::Planar`]/[`astrs_urdf::JointKind::Floating`]
/// joint (3 or 6 DOF) is left at its
/// [`astrs_urdf::kinematics::JointPosition::zero`] home position, exactly
/// as an unset scalar joint already is — see
/// [`astrs_urdf::kinematics::forward_kinematics`]'s own docs on that
/// default.
#[derive(Debug, Clone)]
pub struct ArmState {
    robot: Robot,
    positions: HashMap<String, f64>,
}

impl ArmState {
    /// Builds an arm state from an already-parsed `robot`, validating its
    /// tree shape first.
    ///
    /// # Errors
    ///
    /// Whatever [`Robot::validate`] reports for a document that is not a
    /// legal kinematic tree.
    pub fn new(robot: Robot) -> std::result::Result<Self, UrdfError> {
        robot.validate()?;
        Ok(Self {
            robot,
            positions: HashMap::new(),
        })
    }

    /// The underlying robot description.
    #[must_use]
    pub const fn robot(&self) -> &Robot {
        &self.robot
    }

    /// Sets `joint`'s live scalar position, in radians (revolute/
    /// continuous) or metres (prismatic).
    ///
    /// # Errors
    ///
    /// [`UrdfError::UnknownJoint`] if `joint` names no joint in this
    /// state's [`Robot`]. This does *not* check that `joint` is actually a
    /// scalar-DOF kind — [`ArmState::link_transforms`]'s own call into
    /// [`forward_kinematics`] is what reports
    /// [`UrdfError::JointPositionKindMismatch`] for a value set on, say, a
    /// [`astrs_urdf::JointKind::Fixed`] joint, so that one check lives in one
    /// place rather than being duplicated here.
    pub fn set_joint_position(
        &mut self,
        joint: &str,
        position: f64,
    ) -> std::result::Result<(), UrdfError> {
        if self.robot.joint(joint).is_none() {
            return Err(UrdfError::UnknownJoint {
                joint: joint.to_owned(),
            });
        }
        self.positions.insert(joint.to_owned(), position);
        Ok(())
    }

    /// `joint`'s currently-set live position, or [`None`] if it was never
    /// set (in which case [`ArmState::link_transforms`] resolves it via
    /// [`astrs_urdf`]'s own zero/mimic default — see that function's own
    /// docs).
    #[must_use]
    pub fn joint_position(&self, joint: &str) -> Option<f64> {
        self.positions.get(joint).copied()
    }

    /// Every link's transform relative to the robot's root, for the
    /// current joint state — a direct call into
    /// [`astrs_urdf::kinematics::forward_kinematics`].
    ///
    /// # Errors
    ///
    /// Whatever [`forward_kinematics`] itself reports.
    pub fn link_transforms(&self) -> std::result::Result<HashMap<String, Transform>, UrdfError> {
        let positions: HashMap<String, JointPosition> = self
            .positions
            .iter()
            .map(|(name, &value)| (name.clone(), JointPosition::Scalar(value)))
            .collect();
        forward_kinematics(&self.robot, &positions)
    }

    /// Publishes every link's current transform into `buffer`, flattened
    /// under the robot's root — every link becomes a direct child of the
    /// root frame, the same trade-off
    /// [`astrs_urdf::kinematics::populate_home_pose_transforms`] makes
    /// (see that function's own docs), except driven by this state's
    /// *live* joint positions rather than the URDF's home pose. Every
    /// registered frame is **dynamic** (`is_static: false`): unlike
    /// [`astrs_urdf::kinematics::populate_static_transforms`]'s
    /// fixed-joint skeleton, a link here can genuinely move between calls.
    ///
    /// # Errors
    ///
    /// Whatever [`ArmState::link_transforms`] or
    /// [`TransformBuffer::set_transform`] itself reports.
    pub fn populate_transform_buffer(
        &self,
        buffer: &mut TransformBuffer,
        stamp: TfStamp,
    ) -> Result<()> {
        let transforms = self.link_transforms().map_err(SimError::from)?;
        let topology = Topology::build(&self.robot).map_err(SimError::from)?;
        let root = topology.root();
        for link in &self.robot.links {
            if link.name == root {
                continue;
            }
            let Some(transform) = transforms.get(&link.name) else {
                // `forward_kinematics` always returns one entry per
                // `robot.links` (see that function's own docs), so this is
                // unreachable in practice — but reachable-in-principle for
                // a `Robot` mutated between the two calls above, which a
                // caller could (if unwisely) do since `robot()` returns a
                // shared reference, not an owned snapshot. Treated as "no
                // transform to publish for this link" rather than a panic.
                continue;
            };
            let isometry = to_isometry3(*transform);
            buffer
                .set_transform(root, &link.name, isometry, stamp, false)
                .map_err(SimError::from)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use astrs_tf::TimePoint;
    use astrs_urdf::JointKind;
    use astrs_urdf::math::Vec3;
    use astrs_urdf::model::{Joint, Link};

    /// A minimal two-joint arm: base -> shoulder (revolute about Z) ->
    /// forearm (prismatic along X), each link offset by 1m from its
    /// parent's origin along X once the joint itself is at zero.
    fn two_joint_arm() -> Robot {
        let mut robot = Robot::named("test_arm");
        robot.links.push(Link::named("base_link"));
        robot.links.push(Link::named("shoulder_link"));
        robot.links.push(Link::named("forearm_link"));

        let mut shoulder = Joint::new(
            "shoulder_joint",
            JointKind::Revolute,
            "base_link",
            "shoulder_link",
        );
        shoulder.axis = Vec3::UNIT_Z;
        shoulder.origin = Transform::from_translation(Vec3::new(1.0, 0.0, 0.0));
        robot.joints.push(shoulder);

        let mut forearm = Joint::new(
            "forearm_joint",
            JointKind::Prismatic,
            "shoulder_link",
            "forearm_link",
        );
        forearm.axis = Vec3::UNIT_X;
        robot.joints.push(forearm);

        robot
    }

    #[test]
    fn a_valid_robot_builds_an_arm_state() {
        assert!(ArmState::new(two_joint_arm()).is_ok());
    }

    #[test]
    fn an_invalid_robot_is_rejected_at_construction() {
        // A joint referencing an undeclared parent link fails `validate`.
        let mut robot = Robot::named("broken");
        robot.links.push(Link::named("only_link"));
        robot.joints.push(Joint::new(
            "bad",
            JointKind::Fixed,
            "nonexistent",
            "only_link",
        ));
        assert!(ArmState::new(robot).is_err());
    }

    #[test]
    fn set_joint_position_rejects_an_unknown_joint_name() {
        let mut arm = ArmState::new(two_joint_arm()).unwrap();
        let error = arm.set_joint_position("nonexistent", 1.0).unwrap_err();
        assert!(matches!(error, UrdfError::UnknownJoint { joint } if joint == "nonexistent"));
    }

    #[test]
    fn joint_position_reports_what_was_set_and_none_otherwise() {
        let mut arm = ArmState::new(two_joint_arm()).unwrap();
        assert_eq!(arm.joint_position("shoulder_joint"), None);
        arm.set_joint_position("shoulder_joint", 0.5).unwrap();
        assert_eq!(arm.joint_position("shoulder_joint"), Some(0.5));
    }

    #[test]
    fn link_transforms_reflects_the_zero_position_when_nothing_was_set() {
        let arm = ArmState::new(two_joint_arm()).unwrap();
        let transforms = arm.link_transforms().unwrap();
        assert_eq!(transforms.len(), 3);
        assert_eq!(transforms["base_link"], Transform::IDENTITY);
        // Shoulder at zero rotation: sits exactly at its origin.
        assert!((transforms["shoulder_link"].translation - Vec3::new(1.0, 0.0, 0.0)).norm() < 1e-9);
        // Forearm at zero extension: coincides with the shoulder.
        assert!((transforms["forearm_link"].translation - Vec3::new(1.0, 0.0, 0.0)).norm() < 1e-9);
    }

    #[test]
    fn link_transforms_reflects_a_live_joint_position() {
        let mut arm = ArmState::new(two_joint_arm()).unwrap();
        arm.set_joint_position("forearm_joint", 2.5).unwrap();
        let transforms = arm.link_transforms().unwrap();
        assert!(
            (transforms["forearm_link"].translation - Vec3::new(3.5, 0.0, 0.0)).norm() < 1e-9,
            "{:?}",
            transforms["forearm_link"]
        );
    }

    #[test]
    fn link_transforms_reports_a_kind_mismatch_for_a_non_scalar_joint() {
        // The two-joint arm has no fixed/planar/floating joint to
        // mis-drive, so build one purely to exercise the "wrong shape"
        // path `forward_kinematics` itself reports.
        let mut robot = Robot::named("mismatch");
        robot.links.push(Link::named("base_link"));
        robot.links.push(Link::named("child_link"));
        robot.joints.push(Joint::new(
            "fixed_joint",
            JointKind::Fixed,
            "base_link",
            "child_link",
        ));
        let mut arm = ArmState::new(robot).unwrap();
        arm.set_joint_position("fixed_joint", 1.0).unwrap();
        let error = arm.link_transforms().unwrap_err();
        assert!(matches!(
            error,
            UrdfError::JointPositionKindMismatch { joint, .. } if joint == "fixed_joint"
        ));
    }

    #[test]
    fn populate_transform_buffer_registers_every_non_root_link_as_dynamic() {
        let mut arm = ArmState::new(two_joint_arm()).unwrap();
        arm.set_joint_position("shoulder_joint", std::f64::consts::FRAC_PI_2)
            .unwrap();
        arm.set_joint_position("forearm_joint", 1.0).unwrap();

        let mut buffer = TransformBuffer::new();
        arm.populate_transform_buffer(&mut buffer, TfStamp::EPOCH)
            .unwrap();

        assert_eq!(
            buffer.frame_kind("shoulder_link"),
            Some(astrs_tf::error::FrameKind::Dynamic)
        );
        assert_eq!(buffer.parent_of("forearm_link"), Some("base_link"));
        let looked_up = buffer
            .lookup_transform("base_link", "forearm_link", TimePoint::Latest)
            .unwrap();
        // Shoulder rotated 90 degrees, then forearm extended 1m along its
        // (now-rotated) X: lands at approximately (1, 1, 0).
        assert!((looked_up.translation.x - 1.0).abs() < 1e-9);
        assert!((looked_up.translation.y - 1.0).abs() < 1e-9);
    }

    #[test]
    fn populate_transform_buffer_propagates_a_forward_kinematics_error() {
        let mut robot = Robot::named("mismatch");
        robot.links.push(Link::named("base_link"));
        robot.links.push(Link::named("child_link"));
        robot.joints.push(Joint::new(
            "fixed_joint",
            JointKind::Fixed,
            "base_link",
            "child_link",
        ));
        let mut arm = ArmState::new(robot).unwrap();
        arm.set_joint_position("fixed_joint", 1.0).unwrap();

        let mut buffer = TransformBuffer::new();
        let error = arm
            .populate_transform_buffer(&mut buffer, TfStamp::EPOCH)
            .unwrap_err();
        assert!(matches!(error, SimError::Urdf(_)));
    }
}
