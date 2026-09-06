//! [`forward_kinematics`] — joint-position map to per-link transforms.

use std::collections::HashMap;

use crate::JointKind;
use crate::math::{Quat, Transform};
use crate::model::{Joint, Robot};

use super::position::JointPosition;
use super::topology::Topology;

/// The recursion budget for [`resolve_position`]'s mimic resolution: no
/// legitimate URDF nests `<mimic>` chains anywhere near this deep (a mimic
/// chain longer than a handful of joints has never been observed in a real
/// robot description — every practical use is a single gripper finger
/// mirroring its actuated twin, one hop), so hitting this is itself
/// diagnostic of a mimic cycle [`crate::model::Robot::validate`] was, for
/// whatever reason, not run before this function — not a legitimate deep
/// chain this function is refusing to follow. See [`resolve_position`]'s
/// own docs for why a hard budget is the guard here rather than a
/// `HashSet` of visited joints (the far more common cycle-guard shape
/// elsewhere in this workspace, e.g.
/// `astrs_tf::buffer::TransformBuffer::walk_to_root`): a mimic chain is a
/// simple linked list (one outgoing edge per joint, no branching), so a
/// depth budget catches an actual cycle exactly as reliably while being
/// cheaper to check on every recursive call and needing no allocation at
/// all.
const MAX_MIMIC_CHAIN_DEPTH: u32 = 64;

/// Computes every link's transform relative to `robot`'s root, for the
/// joint configuration `positions` describes.
///
/// # The position map
///
/// `positions` need not name every joint: a joint absent from it (or a
/// [`JointKind::Fixed`] joint, which has no position to give regardless)
/// is resolved as follows, in order:
///
/// 1. **A `<mimic>` relationship**, if the joint has one — its position is
///    computed from its mimic target's *own* resolved position (which may
///    itself come from `positions`, from `JointPosition::zero`, or —
///    transitively — from another mimic relationship), following
///    `crate::model::JointMimic`'s `multiplier * target_position + offset`
///    definition. [`crate::model::Robot::validate`] already guarantees the
///    target is a real, single-DOF joint and that no chain of these cycles
///    back on itself; this function's own mimic resolution still carries a
///    defensive recursion-depth guard for a caller that skipped
///    validation.
/// 2. **[`JointPosition::zero`]** otherwise — the joint's home position.
///
/// # Frame convention
///
/// The returned map's values follow `astrs_tf::math::Isometry3`'s own
/// "parent ← child" convention (see [`crate::math::Transform`]'s docs):
/// `result[link]` maps a point expressed in `link`'s own frame to the same
/// point expressed in the root's frame. `result[robot's root]` is always
/// [`Transform::IDENTITY`].
///
/// # Errors
///
/// Whatever [`Topology::build`] raises for a robot that does not pass
/// [`crate::model::Robot::validate`]'s tree-shape checks (see that
/// function's own docs on why this is re-verified rather than trusted).
/// [`crate::UrdfError::JointPositionKindMismatch`] if `positions` supplies
/// a value whose [`JointPosition::degrees_of_freedom`] does not match the
/// joint's own [`JointKind::degrees_of_freedom`].
pub fn forward_kinematics(
    robot: &Robot,
    positions: &HashMap<String, JointPosition>,
) -> crate::Result<HashMap<String, Transform>> {
    let topology = Topology::build(robot)?;
    let joints_by_name: HashMap<&str, &Joint> =
        robot.joints.iter().map(|j| (j.name.as_str(), j)).collect();

    let mut result: HashMap<String, Transform> = HashMap::with_capacity(robot.links.len());
    for link in &robot.links {
        let transform = link_transform(&topology, &joints_by_name, positions, &link.name, 0)?;
        result.insert(link.name.clone(), transform);
    }
    Ok(result)
}

/// Computes one link's root-relative transform by composing its parent
/// chain — recursively, but bounded by the robot's own (already-validated,
/// finite) tree depth, not by anything user-controlled, so this needs no
/// separate depth guard of its own the way [`resolve_position`]'s mimic
/// resolution does. `depth` here is not a safety mechanism; it exists
/// solely as the
/// unused-but-threaded parameter that would let a future caching layer key
/// on recursion depth without changing every call site's signature — today
/// every link is computed fresh, since `astrs_data`-scale robots (URDF
/// trees are, in practice, well under a few hundred links) make memoizing
/// this walk not worth the bookkeeping.
fn link_transform(
    topology: &Topology<'_>,
    joints_by_name: &HashMap<&str, &Joint>,
    positions: &HashMap<String, JointPosition>,
    link_name: &str,
    depth: u32,
) -> crate::Result<Transform> {
    let _ = depth;
    let Some(parent_joint) = topology.parent_joint(link_name)? else {
        return Ok(Transform::IDENTITY);
    };
    let parent_transform = link_transform(
        topology,
        joints_by_name,
        positions,
        &parent_joint.parent,
        depth + 1,
    )?;
    let joint_motion = joint_motion_transform(joints_by_name, positions, parent_joint)?;
    Ok(parent_transform
        .compose(parent_joint.origin)
        .compose(joint_motion))
}

/// The transform a single joint's *motion* contributes, on top of its own
/// fixed `origin` (composed by [`link_transform`], not here — see
/// [`crate::model::Joint::origin`]'s own docs on why the two are separate).
fn joint_motion_transform(
    joints_by_name: &HashMap<&str, &Joint>,
    positions: &HashMap<String, JointPosition>,
    joint: &Joint,
) -> crate::Result<Transform> {
    let position = resolve_position(joints_by_name, positions, joint, 0)?;
    check_kind_matches(joint, position)?;

    match (joint.kind, position) {
        (JointKind::Fixed, JointPosition::Fixed) => Ok(Transform::IDENTITY),
        (JointKind::Revolute | JointKind::Continuous, JointPosition::Scalar(angle)) => Ok(
            Transform::from_rotation(Quat::from_axis_angle(joint.axis, angle)),
        ),
        (JointKind::Prismatic, JointPosition::Scalar(distance)) => {
            Ok(Transform::from_translation(joint.axis * distance))
        }
        (JointKind::Planar, JointPosition::Planar { x, y, theta }) => {
            // `axis` (already unit under normal use — `crate::parse`
            // normalizes and rejects a degenerate one at parse time) is
            // the plane's normal; `(u, v)` give `x`/`y` a concrete
            // in-plane meaning (see `Vec3::orthonormal_basis`'s own docs
            // on why URDF itself leaves this choice open). Still routed
            // through the fallible path rather than assuming `axis` is
            // unit — `Joint::axis` is a public field a hand-built `Robot`
            // can set to anything, bypassing `crate::parse`'s own
            // normalization entirely (see `UrdfError::DegenerateJointAxis`'s
            // own docs) — matching this crate's "re-derive, never trust an
            // upstream invariant silently" stance (see `Topology`'s own
            // "Trust boundary" docs).
            let (u, v) = joint.axis.orthonormal_basis().ok_or_else(|| {
                crate::UrdfError::DegenerateJointAxis {
                    joint: joint.name.clone(),
                }
            })?;
            let translation = u * x + v * y;
            let rotation = Quat::from_axis_angle(joint.axis, theta);
            Ok(Transform::new(translation, rotation))
        }
        (JointKind::Floating, JointPosition::Floating(transform)) => Ok(transform),
        _ => Err(crate::UrdfError::JointPositionKindMismatch {
            joint: joint.name.clone(),
            kind: joint.kind,
        }),
    }
}

fn check_kind_matches(joint: &Joint, position: JointPosition) -> crate::Result<()> {
    if position.degrees_of_freedom() == joint.kind.degrees_of_freedom() {
        Ok(())
    } else {
        Err(crate::UrdfError::JointPositionKindMismatch {
            joint: joint.name.clone(),
            kind: joint.kind,
        })
    }
}

/// Resolves `joint`'s effective position: `positions[joint.name]` if
/// present, else its mimic target's resolved position transformed by
/// `multiplier`/`offset` if it has a `<mimic>`, else
/// [`JointPosition::zero`]. See [`forward_kinematics`]'s own docs for the
/// full three-tier rule this implements.
fn resolve_position(
    joints_by_name: &HashMap<&str, &Joint>,
    positions: &HashMap<String, JointPosition>,
    joint: &Joint,
    depth: u32,
) -> crate::Result<JointPosition> {
    if let Some(&explicit) = positions.get(&joint.name) {
        return Ok(explicit);
    }
    let Some(mimic) = &joint.mimic else {
        return Ok(JointPosition::zero(joint.kind));
    };
    if depth >= MAX_MIMIC_CHAIN_DEPTH {
        return Err(crate::UrdfError::MimicCycle {
            path: vec![joint.name.clone()],
        });
    }
    let Some(&target) = joints_by_name.get(mimic.joint.as_str()) else {
        return Err(crate::UrdfError::UnknownMimicTarget {
            joint: joint.name.clone(),
            target: mimic.joint.clone(),
        });
    };
    let target_position = resolve_position(joints_by_name, positions, target, depth + 1)?;
    let Some(target_scalar) = target_position.as_scalar() else {
        return Err(crate::UrdfError::MimicRequiresSingleDofJoint {
            joint: joint.name.clone(),
            other: mimic.joint.clone(),
        });
    };
    if joint.kind.degrees_of_freedom() != 1 {
        return Err(crate::UrdfError::MimicRequiresSingleDofJoint {
            joint: joint.name.clone(),
            other: mimic.joint.clone(),
        });
    }
    Ok(JointPosition::Scalar(
        mimic.multiplier * target_scalar + mimic.offset,
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::math::Vec3;
    use crate::model::{JointMimic, Link};

    const EPS: f64 = 1e-9;

    fn assert_translation_close(t: Transform, expected: Vec3) {
        assert!(
            (t.translation - expected).norm() < EPS,
            "{:?} vs {expected:?}",
            t.translation
        );
    }

    fn single_joint_robot(kind: JointKind, axis: Vec3, origin: Transform) -> Robot {
        let mut robot = Robot::named("demo");
        robot.links.push(Link::named("base_link"));
        robot.links.push(Link::named("child_link"));
        let mut joint = Joint::new("j1", kind, "base_link", "child_link");
        joint.axis = axis;
        joint.origin = origin;
        robot.joints.push(joint);
        robot
    }

    #[test]
    fn the_root_link_is_always_identity() {
        let robot = single_joint_robot(JointKind::Fixed, Vec3::UNIT_X, Transform::IDENTITY);
        let result = forward_kinematics(&robot, &HashMap::new()).unwrap();
        assert_eq!(result["base_link"], Transform::IDENTITY);
    }

    #[test]
    fn an_unspecified_joint_defaults_to_zero_position() {
        let robot = single_joint_robot(
            JointKind::Revolute,
            Vec3::UNIT_Z,
            Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
        );
        let result = forward_kinematics(&robot, &HashMap::new()).unwrap();
        // Zero rotation: child sits exactly at the joint's own origin.
        assert_translation_close(result["child_link"], Vec3::new(1.0, 0.0, 0.0));
    }

    #[test]
    fn a_revolute_joint_rotates_about_its_axis() {
        let robot = single_joint_robot(
            JointKind::Revolute,
            Vec3::UNIT_Z,
            Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
        );
        let mut positions = HashMap::new();
        positions.insert(
            "j1".to_owned(),
            JointPosition::Scalar(std::f64::consts::FRAC_PI_2),
        );
        let result = forward_kinematics(&robot, &positions).unwrap();
        // The child's own origin (1,0,0 in the joint frame) does not move
        // relative to the joint itself under a pure rotation of the joint
        // *frame* about its own origin — the joint's position transform
        // is applied on TOP of `origin`, so this checks the rotation
        // itself via a probe point instead: rotate UNIT_X by the joint's
        // resolved rotation and confirm it lands on UNIT_Y.
        let child_transform = result["child_link"];
        let rotated = child_transform.rotation.rotate_vector(Vec3::UNIT_X);
        assert!((rotated - Vec3::UNIT_Y).norm() < EPS);
    }

    #[test]
    fn a_prismatic_joint_translates_along_its_axis() {
        let robot = single_joint_robot(JointKind::Prismatic, Vec3::UNIT_X, Transform::IDENTITY);
        let mut positions = HashMap::new();
        positions.insert("j1".to_owned(), JointPosition::Scalar(2.5));
        let result = forward_kinematics(&robot, &positions).unwrap();
        assert_translation_close(result["child_link"], Vec3::new(2.5, 0.0, 0.0));
    }

    #[test]
    fn a_continuous_joint_behaves_like_revolute_for_fk() {
        let robot = single_joint_robot(JointKind::Continuous, Vec3::UNIT_Z, Transform::IDENTITY);
        let mut positions = HashMap::new();
        positions.insert("j1".to_owned(), JointPosition::Scalar(std::f64::consts::PI));
        let result = forward_kinematics(&robot, &positions).unwrap();
        let rotated = result["child_link"].rotation.rotate_vector(Vec3::UNIT_X);
        assert!((rotated - (-Vec3::UNIT_X)).norm() < EPS);
    }

    #[test]
    fn a_fixed_joint_never_moves_regardless_of_the_position_map() {
        let origin = Transform::from_translation(Vec3::new(0.0, 0.0, 1.0));
        let robot = single_joint_robot(JointKind::Fixed, Vec3::UNIT_X, origin);
        let result = forward_kinematics(&robot, &HashMap::new()).unwrap();
        assert_translation_close(result["child_link"], Vec3::new(0.0, 0.0, 1.0));
    }

    #[test]
    fn a_two_link_chain_composes_parent_and_child_transforms() {
        let mut robot = Robot::named("demo");
        robot.links.push(Link::named("base_link"));
        robot.links.push(Link::named("mid_link"));
        robot.links.push(Link::named("tip_link"));
        let mut j1 = Joint::new("j1", JointKind::Prismatic, "base_link", "mid_link");
        j1.axis = Vec3::UNIT_X;
        let mut j2 = Joint::new("j2", JointKind::Prismatic, "mid_link", "tip_link");
        j2.axis = Vec3::UNIT_Y;
        robot.joints.push(j1);
        robot.joints.push(j2);

        let mut positions = HashMap::new();
        positions.insert("j1".to_owned(), JointPosition::Scalar(3.0));
        positions.insert("j2".to_owned(), JointPosition::Scalar(4.0));
        let result = forward_kinematics(&robot, &positions).unwrap();
        assert_translation_close(result["tip_link"], Vec3::new(3.0, 4.0, 0.0));
    }

    #[test]
    fn a_wrong_shaped_position_is_a_typed_error() {
        let robot = single_joint_robot(JointKind::Revolute, Vec3::UNIT_Z, Transform::IDENTITY);
        let mut positions = HashMap::new();
        positions.insert(
            "j1".to_owned(),
            JointPosition::Planar {
                x: 0.0,
                y: 0.0,
                theta: 0.0,
            },
        );
        let error = forward_kinematics(&robot, &positions).unwrap_err();
        assert!(matches!(
            error,
            crate::UrdfError::JointPositionKindMismatch { joint, .. } if joint == "j1"
        ));
    }

    #[test]
    fn a_mimic_joint_derives_its_position_from_its_target() {
        let mut robot = single_joint_robot(JointKind::Revolute, Vec3::UNIT_Z, Transform::IDENTITY);
        robot.links.push(Link::named("mimic_link"));
        let mut mimic_joint = Joint::new("j2", JointKind::Revolute, "base_link", "mimic_link");
        mimic_joint.axis = Vec3::UNIT_Z;
        mimic_joint.mimic = Some(JointMimic {
            joint: "j1".to_owned(),
            multiplier: 2.0,
            offset: 0.0,
        });
        robot.joints.push(mimic_joint);

        let mut positions = HashMap::new();
        positions.insert("j1".to_owned(), JointPosition::Scalar(0.1));
        let result = forward_kinematics(&robot, &positions).unwrap();
        // j2's rotation should be exactly double j1's: verify indirectly
        // by rotating a probe vector through both and comparing angles.
        let rotated_j1 = result["child_link"].rotation.rotate_vector(Vec3::UNIT_X);
        let rotated_j2 = result["mimic_link"].rotation.rotate_vector(Vec3::UNIT_X);
        let angle_j1 = rotated_j1.y.atan2(rotated_j1.x);
        let angle_j2 = rotated_j2.y.atan2(rotated_j2.x);
        assert!((angle_j2 - 2.0 * angle_j1).abs() < EPS);
    }

    #[test]
    fn a_mimic_joint_applies_its_offset() {
        let mut robot = single_joint_robot(JointKind::Revolute, Vec3::UNIT_Z, Transform::IDENTITY);
        robot.links.push(Link::named("mimic_link"));
        let mut mimic_joint = Joint::new("j2", JointKind::Revolute, "base_link", "mimic_link");
        mimic_joint.axis = Vec3::UNIT_Z;
        mimic_joint.mimic = Some(JointMimic {
            joint: "j1".to_owned(),
            multiplier: 1.0,
            offset: std::f64::consts::FRAC_PI_2,
        });
        robot.joints.push(mimic_joint);

        // j1 unspecified -> zero; j2 should sit at exactly offset = pi/2.
        let result = forward_kinematics(&robot, &HashMap::new()).unwrap();
        let rotated = result["mimic_link"].rotation.rotate_vector(Vec3::UNIT_X);
        assert!((rotated - Vec3::UNIT_Y).norm() < EPS);
    }

    #[test]
    fn an_explicit_position_overrides_a_mimic_relationship() {
        let mut robot = single_joint_robot(JointKind::Revolute, Vec3::UNIT_Z, Transform::IDENTITY);
        robot.links.push(Link::named("mimic_link"));
        let mut mimic_joint = Joint::new("j2", JointKind::Revolute, "base_link", "mimic_link");
        mimic_joint.axis = Vec3::UNIT_Z;
        mimic_joint.mimic = Some(JointMimic {
            joint: "j1".to_owned(),
            multiplier: 5.0,
            offset: 0.0,
        });
        robot.joints.push(mimic_joint);

        let mut positions = HashMap::new();
        positions.insert("j1".to_owned(), JointPosition::Scalar(1.0));
        positions.insert("j2".to_owned(), JointPosition::Scalar(0.0)); // explicit override
        let result = forward_kinematics(&robot, &positions).unwrap();
        let rotated = result["mimic_link"].rotation.rotate_vector(Vec3::UNIT_X);
        assert!((rotated - Vec3::UNIT_X).norm() < EPS); // zero rotation, not 5x
    }

    #[test]
    fn a_transitive_mimic_chain_resolves_correctly() {
        // j1 -> j2 mimics j1 (x2) -> j3 mimics j2 (x2) => j3 = 4 * j1.
        let mut robot = Robot::named("demo");
        robot.links.push(Link::named("base_link"));
        robot.links.push(Link::named("l1"));
        robot.links.push(Link::named("l2"));
        robot.links.push(Link::named("l3"));
        let mut j1 = Joint::new("j1", JointKind::Prismatic, "base_link", "l1");
        j1.axis = Vec3::UNIT_X;
        let mut j2 = Joint::new("j2", JointKind::Prismatic, "l1", "l2");
        j2.axis = Vec3::UNIT_X;
        j2.mimic = Some(JointMimic {
            joint: "j1".to_owned(),
            multiplier: 2.0,
            offset: 0.0,
        });
        let mut j3 = Joint::new("j3", JointKind::Prismatic, "l2", "l3");
        j3.axis = Vec3::UNIT_X;
        j3.mimic = Some(JointMimic {
            joint: "j2".to_owned(),
            multiplier: 2.0,
            offset: 0.0,
        });
        robot.joints.push(j1);
        robot.joints.push(j2);
        robot.joints.push(j3);

        let mut positions = HashMap::new();
        positions.insert("j1".to_owned(), JointPosition::Scalar(1.0));
        let result = forward_kinematics(&robot, &positions).unwrap();
        // l1 at x=1 (j1), l2 at x=1+2=3 (j1 + 2*j1), l3 at x=3+4=7 (+ 4*j1).
        assert_translation_close(result["l1"], Vec3::new(1.0, 0.0, 0.0));
        assert_translation_close(result["l2"], Vec3::new(3.0, 0.0, 0.0));
        assert_translation_close(result["l3"], Vec3::new(7.0, 0.0, 0.0));
    }

    #[test]
    fn an_unknown_mimic_target_is_a_typed_error() {
        let mut robot = Robot::named("demo");
        robot.links.push(Link::named("base_link"));
        robot.links.push(Link::named("child_link"));
        let mut joint = Joint::new("j1", JointKind::Revolute, "base_link", "child_link");
        joint.mimic = Some(JointMimic {
            joint: "nonexistent".to_owned(),
            multiplier: 1.0,
            offset: 0.0,
        });
        robot.joints.push(joint);
        let error = forward_kinematics(&robot, &HashMap::new()).unwrap_err();
        assert!(matches!(
            error,
            crate::UrdfError::UnknownMimicTarget { target, .. } if target == "nonexistent"
        ));
    }

    #[test]
    fn a_planar_joint_uses_the_orthonormal_basis_for_its_in_plane_axes() {
        let robot = single_joint_robot(JointKind::Planar, Vec3::UNIT_Z, Transform::IDENTITY);
        let mut positions = HashMap::new();
        positions.insert(
            "j1".to_owned(),
            JointPosition::Planar {
                x: 2.0,
                y: 3.0,
                theta: 0.0,
            },
        );
        let result = forward_kinematics(&robot, &positions).unwrap();
        // UNIT_Z's own orthonormal_basis is (UNIT_X, UNIT_Y) exactly.
        assert_translation_close(result["child_link"], Vec3::new(2.0, 3.0, 0.0));
    }

    #[test]
    fn a_floating_joint_passes_its_transform_through_directly() {
        let robot = single_joint_robot(JointKind::Floating, Vec3::UNIT_X, Transform::IDENTITY);
        let free_transform = Transform::new(
            Vec3::new(1.0, 2.0, 3.0),
            Quat::from_axis_angle(Vec3::UNIT_Y, 0.5),
        );
        let mut positions = HashMap::new();
        positions.insert("j1".to_owned(), JointPosition::Floating(free_transform));
        let result = forward_kinematics(&robot, &positions).unwrap();
        assert_translation_close(result["child_link"], Vec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn a_robot_with_no_joints_at_all_places_its_solo_link_at_identity() {
        let mut robot = Robot::named("demo");
        robot.links.push(Link::named("solo"));
        let result = forward_kinematics(&robot, &HashMap::new()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result["solo"], Transform::IDENTITY);
    }
}
