//! [`populate_static_transforms`] — the fixed-frame skeleton (every
//! [`JointKind::Fixed`] joint, at its one and only position) into
//! `astrs_tf::buffer::TransformBuffer` static transforms.

use astrs_tf::TfStamp;
use astrs_tf::buffer::TransformBuffer;

use crate::JointKind;
use crate::math::to_isometry3;
use crate::model::Robot;

use super::forward::forward_kinematics;

/// Registers every fixed link-to-link relationship in `robot` as a static
/// transform in `buffer`.
///
/// # Why only fixed joints
///
/// A [`JointKind::Fixed`] joint has exactly one position, ever — its
/// `origin` *is* the whole relationship between parent and child, nothing
/// else can move it — which is precisely `astrs_tf::buffer::TransformBuffer`'s
/// own definition of a **static** frame (see
/// `astrs_tf::buffer::TransformBuffer::set_transform`'s docs: "valid at
/// any query time"). Every other joint kind has a position that can
/// change at runtime, which is what a **dynamic** frame is for — but
/// publishing a live joint's current angle onto `/tf` on every tick is a
/// runtime robot-state-publisher's job (reading encoder feedback, e.g.
/// `astrs-sim` or a real joint-state source), not a one-shot conversion
/// from a static URDF document. This function converts exactly the part
/// of the tree that *is* fixed at parse time — the "fixed-frame skeleton"
/// blueprint §5.3 asks for — and leaves the rest to whatever runtime
/// component actually knows the robot's live joint state.
///
/// Registers each fixed joint's transform directly from its `origin` (a
/// fixed joint's own motion contribution is always
/// [`crate::math::Transform::IDENTITY`] composed on top of `origin` — see
/// [`super::forward_kinematics`]'s own docs on how a joint's `origin` and
/// its motion compose — so this function does not need to run a full
/// forward-kinematics pass at all, only walk `robot.joints` once),
/// converted to `astrs_tf`'s own [`astrs_tf::math::Isometry3`] via
/// [`crate::math::to_isometry3`] — see that function's docs on why the two
/// crates' conventions already agree
/// field-for-field.
///
/// `stamp` is recorded on every registered transform for introspection
/// (see `TransformBuffer::set_transform`'s own docs: a static frame's
/// stamp does not affect *lookup*, since a static frame answers at any
/// query time) — typically [`TfStamp::EPOCH`] or whatever this call is
/// conceptually "publishing" the fixed skeleton at.
///
/// # Errors
///
/// [`crate::UrdfError::Tf`] wrapping whatever
/// `TransformBuffer::set_transform` itself raises — a degenerate rotation
/// or non-finite translation in some fixed joint's `<origin>` (unreachable
/// for a joint parsed by [`crate::parse`], which already rejects a
/// non-finite `xyz`/`rpy`, but reachable for a hand-built [`Robot`] the
/// same way [`crate::UrdfError::DegenerateJointAxis`] is), or a frame-graph
/// cycle (unreachable for a [`Robot::validate`]d robot, whose fixed-joint
/// subgraph is necessarily acyclic since the *whole* graph already is).
pub fn populate_static_transforms(
    robot: &Robot,
    buffer: &mut TransformBuffer,
    stamp: TfStamp,
) -> crate::Result<()> {
    for joint in &robot.joints {
        if joint.kind != JointKind::Fixed {
            continue;
        }
        let isometry = to_isometry3(joint.origin);
        buffer.set_transform(&joint.parent, &joint.child, isometry, stamp, true)?;
    }
    Ok(())
}

/// [`populate_static_transforms`], but for the *whole* fixed-frame
/// skeleton at once, including non-fixed joints resolved at
/// [`super::JointPosition::zero`] — every link's transform relative to the
/// robot's root, published as one flattened static frame per link (parent:
/// the root, not each link's own immediate URDF parent).
///
/// Where [`populate_static_transforms`] registers only the joints that are
/// *structurally* incapable of moving (preserving the original tree
/// shape — each fixed link keeps its real URDF parent), this instead asks
/// "if I froze every joint at its home position right now, where does
/// every link sit relative to the root" and registers *that* flattened
/// answer — useful for a caller building a one-shot visualization or a
/// sanity-check reference pose, at the cost of losing the tree structure
/// non-fixed edges implied (every link becomes a direct child of the
/// root).
///
/// # Errors
///
/// Whatever [`super::forward_kinematics`] or `TransformBuffer::set_transform`
/// raises.
pub fn populate_home_pose_transforms(
    robot: &Robot,
    buffer: &mut TransformBuffer,
    stamp: TfStamp,
) -> crate::Result<()> {
    let positions = std::collections::HashMap::new();
    let transforms = forward_kinematics(robot, &positions)?;
    let topology = super::topology::Topology::build(robot)?;
    let root = topology.root();
    for link in &robot.links {
        if link.name == root {
            continue;
        }
        let isometry = to_isometry3(transforms[&link.name]);
        buffer.set_transform(root, &link.name, isometry, stamp, true)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::JointKind;
    use crate::math::{Quat, Transform, Vec3};
    use crate::model::{Joint, Link};
    use astrs_tf::TimePoint;

    #[test]
    fn a_fixed_joint_becomes_a_static_transform() {
        let mut robot = Robot::named("demo");
        robot.links.push(Link::named("base_link"));
        robot.links.push(Link::named("sensor_link"));
        let mut joint = Joint::new("mount", JointKind::Fixed, "base_link", "sensor_link");
        joint.origin = Transform::from_translation(Vec3::new(0.1, 0.0, 0.2));
        robot.joints.push(joint);

        let mut buffer = TransformBuffer::new();
        populate_static_transforms(&robot, &mut buffer, TfStamp::EPOCH).unwrap();

        let looked_up = buffer
            .lookup_transform("base_link", "sensor_link", TimePoint::Latest)
            .unwrap();
        assert!((looked_up.translation.x - 0.1).abs() < 1e-9);
        assert!((looked_up.translation.z - 0.2).abs() < 1e-9);
        assert_eq!(
            buffer.frame_kind("sensor_link"),
            Some(astrs_tf::error::FrameKind::Static)
        );
    }

    #[test]
    fn a_non_fixed_joint_is_not_registered() {
        let mut robot = Robot::named("demo");
        robot.links.push(Link::named("base_link"));
        robot.links.push(Link::named("arm_link"));
        robot.joints.push(Joint::new(
            "j1",
            JointKind::Revolute,
            "base_link",
            "arm_link",
        ));

        let mut buffer = TransformBuffer::new();
        populate_static_transforms(&robot, &mut buffer, TfStamp::EPOCH).unwrap();
        assert!(!buffer.is_known_frame("arm_link"));
    }

    #[test]
    fn multiple_fixed_joints_chain_correctly() {
        let mut robot = Robot::named("demo");
        robot.links.push(Link::named("base_link"));
        robot.links.push(Link::named("mid_link"));
        robot.links.push(Link::named("tip_link"));
        let mut j1 = Joint::new("j1", JointKind::Fixed, "base_link", "mid_link");
        j1.origin = Transform::from_translation(Vec3::new(1.0, 0.0, 0.0));
        let mut j2 = Joint::new("j2", JointKind::Fixed, "mid_link", "tip_link");
        j2.origin = Transform::from_translation(Vec3::new(0.0, 1.0, 0.0));
        robot.joints.push(j1);
        robot.joints.push(j2);

        let mut buffer = TransformBuffer::new();
        populate_static_transforms(&robot, &mut buffer, TfStamp::EPOCH).unwrap();
        let looked_up = buffer
            .lookup_transform("base_link", "tip_link", TimePoint::Latest)
            .unwrap();
        assert!((looked_up.translation.x - 1.0).abs() < 1e-9);
        assert!((looked_up.translation.y - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_degenerate_origin_rotation_is_a_typed_tf_error() {
        let mut robot = Robot::named("demo");
        robot.links.push(Link::named("base_link"));
        robot.links.push(Link::named("child_link"));
        let mut joint = Joint::new("j1", JointKind::Fixed, "base_link", "child_link");
        joint.origin = Transform::from_rotation(Quat::new(0.0, 0.0, 0.0, 0.0));
        robot.joints.push(joint);

        let mut buffer = TransformBuffer::new();
        let error = populate_static_transforms(&robot, &mut buffer, TfStamp::EPOCH).unwrap_err();
        assert!(matches!(error, crate::UrdfError::Tf(_)));
    }

    #[test]
    fn home_pose_transforms_flattens_every_link_relative_to_the_root() {
        let mut robot = Robot::named("demo");
        robot.links.push(Link::named("base_link"));
        robot.links.push(Link::named("mid_link"));
        robot.links.push(Link::named("tip_link"));
        let mut j1 = Joint::new("j1", JointKind::Prismatic, "base_link", "mid_link");
        j1.axis = Vec3::UNIT_X;
        j1.origin = Transform::from_translation(Vec3::new(1.0, 0.0, 0.0));
        let mut j2 = Joint::new("j2", JointKind::Prismatic, "mid_link", "tip_link");
        j2.axis = Vec3::UNIT_Y;
        j2.origin = Transform::from_translation(Vec3::new(0.0, 1.0, 0.0));
        robot.joints.push(j1);
        robot.joints.push(j2);

        let mut buffer = TransformBuffer::new();
        populate_home_pose_transforms(&robot, &mut buffer, TfStamp::EPOCH).unwrap();
        // Both zero position (prismatic joints at home = 0 displacement),
        // so tip sits at base + (1,0,0) + (0,1,0) = (1,1,0), flattened
        // directly under base_link (not via mid_link).
        assert_eq!(buffer.parent_of("tip_link"), Some("base_link"));
        let looked_up = buffer
            .lookup_transform("base_link", "tip_link", TimePoint::Latest)
            .unwrap();
        assert!((looked_up.translation.x - 1.0).abs() < 1e-9);
        assert!((looked_up.translation.y - 1.0).abs() < 1e-9);
    }
}
