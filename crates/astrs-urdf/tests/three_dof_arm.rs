//! Golden forward-kinematics test for `tests/fixtures/three_dof_arm.urdf`.
//!
//! Every expected value below was independently derived — worked by hand
//! (see the comments on each case) and cross-checked with a standalone
//! Python/NumPy reference implementation of the exact same composition
//! rule (`parent ∘ origin ∘ motion`, quaternion Hamilton product, "rotate
//! then translate" point transform) — never by running this crate's own
//! code and copying its output. See `tests/fixtures/three_dof_arm.urdf`
//! for the joint layout this file's numbers are derived from.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::f64::consts::FRAC_PI_2;

use astrs_urdf::JointKind;
use astrs_urdf::kinematics::{Chain, JointPosition, forward_kinematics};

const FIXTURE: &str = include_str!("fixtures/three_dof_arm.urdf");
const EPS: f64 = 1e-9;

fn assert_vec_close(actual: astrs_urdf::math::Vec3, expected: astrs_urdf::math::Vec3) {
    assert!(
        (actual - expected).norm() < EPS,
        "expected {expected:?}, got {actual:?}"
    );
}

fn assert_quat_close(actual: astrs_urdf::math::Quat, expected: astrs_urdf::math::Quat) {
    // Unit quaternions double-cover SO(3): q and -q represent the same
    // rotation, so compare via |dot product| rather than direct equality.
    assert!(
        (actual.dot(expected).abs() - 1.0).abs() < EPS,
        "expected {expected:?} (up to sign), got {actual:?}"
    );
}

#[test]
fn the_fixture_parses_and_validates() {
    let robot = astrs_urdf::parse_str(FIXTURE).expect("well-formed URDF");
    assert_eq!(robot.name, "three_dof_arm");
    assert_eq!(robot.links.len(), 4);
    assert_eq!(robot.joints.len(), 3);
    robot.validate().expect("a valid rooted tree");
}

#[test]
fn joint_kinds_and_axes_match_the_fixture() {
    let robot = astrs_urdf::parse_str(FIXTURE).unwrap();
    assert_eq!(robot.joint("j1").unwrap().kind, JointKind::Revolute);
    assert_eq!(robot.joint("j2").unwrap().kind, JointKind::Revolute);
    assert_eq!(robot.joint("j3").unwrap().kind, JointKind::Prismatic);
    assert_eq!(
        robot.joint("j1").unwrap().axis,
        astrs_urdf::math::Vec3::UNIT_Z
    );
    assert_eq!(
        robot.joint("j2").unwrap().axis,
        astrs_urdf::math::Vec3::UNIT_Y
    );
    assert_eq!(
        robot.joint("j3").unwrap().axis,
        astrs_urdf::math::Vec3::UNIT_X
    );
}

#[test]
fn the_full_chain_extracts_all_three_joints_in_order() {
    let robot = astrs_urdf::parse_str(FIXTURE).unwrap();
    let chain = Chain::extract(&robot, "base_link", "link3").unwrap();
    assert_eq!(chain.joint_names(), ["j1", "j2", "j3"]);
    assert_eq!(chain.degrees_of_freedom(&robot), 3);
}

/// Hand-derivation: at the home position (every joint at zero), no joint
/// contributes any rotation or translation beyond its own `origin`. The
/// chain is a straight line along X: `link1` at the origin (j1's own
/// `origin` is identity), `link2` at `link1 + (1,0,0) = (1,0,0)` (j2's
/// `origin`), `link3` at `link2 + (1,0,0) = (2,0,0)` (j3's `origin`, j3's
/// own motion at position 0 contributes nothing extra).
#[test]
fn home_position_places_every_link_along_the_x_axis() {
    let robot = astrs_urdf::parse_str(FIXTURE).unwrap();
    let result = forward_kinematics(&robot, &HashMap::new()).unwrap();

    assert_vec_close(
        result["base_link"].translation,
        astrs_urdf::math::Vec3::ZERO,
    );
    assert_vec_close(result["link1"].translation, astrs_urdf::math::Vec3::ZERO);
    assert_vec_close(
        result["link2"].translation,
        astrs_urdf::math::Vec3::new(1.0, 0.0, 0.0),
    );
    assert_vec_close(
        result["link3"].translation,
        astrs_urdf::math::Vec3::new(2.0, 0.0, 0.0),
    );
    for link in ["base_link", "link1", "link2", "link3"] {
        assert_quat_close(result[link].rotation, astrs_urdf::math::Quat::IDENTITY);
    }
}

/// Hand-derivation: `j1` rotates 90° about Z at the base — a rotation with
/// no translation of its own (j1's `origin` is identity). `link1`'s frame
/// is therefore `(0,0,0)` translation, `Rz(90°)` rotation. `link2`'s
/// world position is `link1.translation + Rz(90°) * (1,0,0) = (0,0,0) +
/// (0,1,0) = (0,1,0)` — the `origin` translation `(1,0,0)` gets rotated
/// into `+Y` by the accumulated Z rotation before being added, per
/// `Transform::compose`'s "rotate then translate" rule. `link2`'s own
/// rotation is still `Rz(90°)` (j2 is at zero). `link3` continues the
/// same pattern one more unit-X hop, rotated by the same accumulated
/// `Rz(90°)`: `(0,1,0) + Rz(90°) * (1,0,0) = (0,1,0) + (0,1,0) = (0,2,0)`.
#[test]
fn rotating_j1_by_a_quarter_turn_swings_the_whole_arm_onto_the_y_axis() {
    let robot = astrs_urdf::parse_str(FIXTURE).unwrap();
    let mut positions = HashMap::new();
    positions.insert("j1".to_owned(), JointPosition::Scalar(FRAC_PI_2));
    let result = forward_kinematics(&robot, &positions).unwrap();

    assert_vec_close(result["link1"].translation, astrs_urdf::math::Vec3::ZERO);
    assert_vec_close(
        result["link2"].translation,
        astrs_urdf::math::Vec3::new(0.0, 1.0, 0.0),
    );
    assert_vec_close(
        result["link3"].translation,
        astrs_urdf::math::Vec3::new(0.0, 2.0, 0.0),
    );

    let expected_rotation =
        astrs_urdf::math::Quat::from_axis_angle(astrs_urdf::math::Vec3::UNIT_Z, FRAC_PI_2);
    assert_quat_close(result["link1"].rotation, expected_rotation);
    assert_quat_close(result["link2"].rotation, expected_rotation);
    assert_quat_close(result["link3"].rotation, expected_rotation);
}

/// Hand-derivation (golden values cross-checked against the standalone
/// NumPy reference — see this file's own docs): `j1 = pi/2` gives `link1`
/// rotation `Rz(90°)`, translation `(0,0,0)` as in the previous case.
/// `j2`'s own motion is a further `Ry(90°)` rotation *composed on top of*
/// the already-accumulated `Rz(90°)` (Hamilton product `Rz(90°) *
/// Ry(90°)`, since `j2`'s motion is generated in `link1`'s already-rotated
/// frame — see `crate::model::Joint::axis`'s own docs on why `axis` lives
/// in the joint's own frame). `link2`'s translation is unaffected by its
/// *own* motion rotation (only the parent's accumulated rotation affects
/// where `origin`'s translation lands): still `(0,1,0)`. `link3` continues
/// one more unit-X hop from `link2`, now rotated by the combined `Rz(90°)
/// * Ry(90°)` quaternion — which is NOT simply "rotate by 90 about Y in
/// world frame" (composed rotations do not commute), landing at
/// `(0,1,-1)`, not the naively-expected `(1,1,0)`.
#[test]
fn combining_j1_and_j2_composes_rotations_in_the_correct_order() {
    let robot = astrs_urdf::parse_str(FIXTURE).unwrap();
    let mut positions = HashMap::new();
    positions.insert("j1".to_owned(), JointPosition::Scalar(FRAC_PI_2));
    positions.insert("j2".to_owned(), JointPosition::Scalar(FRAC_PI_2));
    let result = forward_kinematics(&robot, &positions).unwrap();

    assert_vec_close(
        result["link2"].translation,
        astrs_urdf::math::Vec3::new(0.0, 1.0, 0.0),
    );
    assert_vec_close(
        result["link3"].translation,
        astrs_urdf::math::Vec3::new(0.0, 1.0, -1.0),
    );

    let rz = astrs_urdf::math::Quat::from_axis_angle(astrs_urdf::math::Vec3::UNIT_Z, FRAC_PI_2);
    let ry = astrs_urdf::math::Quat::from_axis_angle(astrs_urdf::math::Vec3::UNIT_Y, FRAC_PI_2);
    let expected_link2_rotation = rz * ry;
    assert_quat_close(result["link2"].rotation, expected_link2_rotation);
    assert_quat_close(result["link3"].rotation, expected_link2_rotation);
}

/// Extends the previous case: `j3 = 0.5` slides `link3` an additional
/// 0.5m along its own local X axis — which, after the `Rz(90°) * Ry(90°)`
/// accumulated rotation, points along world `-Z` (the same direction
/// `j3`'s own `origin` translation was rotated into in the previous
/// case). So `link3` moves from `(0,1,-1)` to `(0,1,-1.5)`.
#[test]
fn extending_the_prismatic_joint_slides_the_tip_along_its_rotated_axis() {
    let robot = astrs_urdf::parse_str(FIXTURE).unwrap();
    let mut positions = HashMap::new();
    positions.insert("j1".to_owned(), JointPosition::Scalar(FRAC_PI_2));
    positions.insert("j2".to_owned(), JointPosition::Scalar(FRAC_PI_2));
    positions.insert("j3".to_owned(), JointPosition::Scalar(0.5));
    let result = forward_kinematics(&robot, &positions).unwrap();

    assert_vec_close(
        result["link3"].translation,
        astrs_urdf::math::Vec3::new(0.0, 1.0, -1.5),
    );
    // link2 (upstream of j3) is unaffected by j3's own position.
    assert_vec_close(
        result["link2"].translation,
        astrs_urdf::math::Vec3::new(0.0, 1.0, 0.0),
    );
}

#[test]
fn a_partial_chain_from_link1_to_link3_skips_j1() {
    let robot = astrs_urdf::parse_str(FIXTURE).unwrap();
    let chain = Chain::extract(&robot, "link1", "link3").unwrap();
    assert_eq!(chain.joint_names(), ["j2", "j3"]);
}

#[test]
fn the_root_link_is_base_link() {
    use astrs_urdf::kinematics::Topology;
    let robot = astrs_urdf::parse_str(FIXTURE).unwrap();
    let topology = Topology::build(&robot).unwrap();
    assert_eq!(topology.root(), "base_link");
}
