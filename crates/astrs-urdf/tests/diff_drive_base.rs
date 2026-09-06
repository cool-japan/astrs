//! Golden forward-kinematics test for `tests/fixtures/diff_drive_base.urdf`
//! — the differential-drive mobile-base shape (`base_footprint` ->
//! `base_link` -> two continuous wheel joints + a fixed caster).
//!
//! Every expected value below was independently derived by hand and
//! cross-checked with a standalone Python/NumPy reference implementation —
//! see `tests/three_dof_arm.rs`'s own docs for the identical methodology.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::f64::consts::PI;

use astrs_tf::TfStamp;
use astrs_tf::buffer::TransformBuffer;
use astrs_urdf::JointKind;
use astrs_urdf::kinematics::{
    JointPosition, Topology, forward_kinematics, populate_static_transforms,
};
use astrs_urdf::math::{Quat, Vec3};

const FIXTURE: &str = include_str!("fixtures/diff_drive_base.urdf");
const EPS: f64 = 1e-9;

fn assert_vec_close(actual: Vec3, expected: Vec3) {
    assert!(
        (actual - expected).norm() < EPS,
        "expected {expected:?}, got {actual:?}"
    );
}

fn assert_quat_close(actual: Quat, expected: Quat) {
    assert!(
        (actual.dot(expected).abs() - 1.0).abs() < EPS,
        "expected {expected:?} (up to sign), got {actual:?}"
    );
}

#[test]
fn the_fixture_parses_and_validates() {
    let robot = astrs_urdf::parse_str(FIXTURE).expect("well-formed URDF");
    assert_eq!(robot.name, "diff_drive_base");
    assert_eq!(robot.links.len(), 5);
    assert_eq!(robot.joints.len(), 4);
    robot.validate().expect("a valid rooted tree");
}

#[test]
fn base_footprint_is_the_root_not_base_link() {
    let robot = astrs_urdf::parse_str(FIXTURE).unwrap();
    let topology = Topology::build(&robot).unwrap();
    assert_eq!(topology.root(), "base_footprint");
}

#[test]
fn joint_kinds_match_the_fixture() {
    let robot = astrs_urdf::parse_str(FIXTURE).unwrap();
    assert_eq!(robot.joint("base_joint").unwrap().kind, JointKind::Fixed);
    assert_eq!(
        robot.joint("left_wheel_joint").unwrap().kind,
        JointKind::Continuous
    );
    assert_eq!(
        robot.joint("right_wheel_joint").unwrap().kind,
        JointKind::Continuous
    );
    assert_eq!(robot.joint("caster_joint").unwrap().kind, JointKind::Fixed);
}

/// Hand-derivation: `base_joint` is fixed with `origin xyz="0 0 0.1"` — a
/// pure translation, no rotation contributed by the joint itself.
/// `base_link` therefore sits exactly at `(0, 0, 0.1)`.
#[test]
fn base_link_sits_ten_centimeters_above_the_footprint() {
    let robot = astrs_urdf::parse_str(FIXTURE).unwrap();
    let result = forward_kinematics(&robot, &HashMap::new()).unwrap();
    assert_vec_close(result["base_link"].translation, Vec3::new(0.0, 0.0, 0.1));
    assert_quat_close(result["base_link"].rotation, Quat::IDENTITY);
}

/// Hand-derivation: at wheel angle 0 (the default — both wheels
/// unspecified in the position map), each wheel joint contributes no
/// rotation, so each wheel link sits exactly at `base_link.translation +
/// origin`: left at `(0, 0.2, 0.1) + (0, 0, -0.05) = (0, 0.2, 0.05)`,
/// right at `(0, 0.1) + (0, -0.2, -0.05) = (0, -0.2, 0.05)`.
#[test]
fn both_wheels_sit_symmetrically_at_the_home_position() {
    let robot = astrs_urdf::parse_str(FIXTURE).unwrap();
    let result = forward_kinematics(&robot, &HashMap::new()).unwrap();

    assert_vec_close(
        result["left_wheel_link"].translation,
        Vec3::new(0.0, 0.2, 0.05),
    );
    assert_vec_close(
        result["right_wheel_link"].translation,
        Vec3::new(0.0, -0.2, 0.05),
    );
    assert_quat_close(result["left_wheel_link"].rotation, Quat::IDENTITY);
    assert_quat_close(result["right_wheel_link"].rotation, Quat::IDENTITY);
}

/// Hand-derivation: the left wheel spun 90 degrees about its own Y axis
/// does not move its origin (rotation about a joint's own origin never
/// translates the link) — it stays at `(0, 0.2, 0.05)`, only its
/// *rotation* changes to `Ry(90°)`.
#[test]
fn spinning_the_left_wheel_rotates_in_place() {
    let robot = astrs_urdf::parse_str(FIXTURE).unwrap();
    let mut positions = HashMap::new();
    positions.insert(
        "left_wheel_joint".to_owned(),
        JointPosition::Scalar(PI / 2.0),
    );
    let result = forward_kinematics(&robot, &positions).unwrap();

    assert_vec_close(
        result["left_wheel_link"].translation,
        Vec3::new(0.0, 0.2, 0.05),
    );
    assert_quat_close(
        result["left_wheel_link"].rotation,
        Quat::from_axis_angle(Vec3::UNIT_Y, PI / 2.0),
    );
}

/// The two wheels are independent joints: rotating one leaves the other
/// exactly at its own home position.
#[test]
fn wheel_rotations_are_independent_of_each_other() {
    let robot = astrs_urdf::parse_str(FIXTURE).unwrap();
    let mut positions = HashMap::new();
    positions.insert("right_wheel_joint".to_owned(), JointPosition::Scalar(PI));
    let result = forward_kinematics(&robot, &positions).unwrap();

    assert_quat_close(
        result["right_wheel_link"].rotation,
        Quat::from_axis_angle(Vec3::UNIT_Y, PI),
    );
    // Left wheel untouched: still at home (identity rotation).
    assert_quat_close(result["left_wheel_link"].rotation, Quat::IDENTITY);
}

/// Hand-derivation: the caster is a fixed joint with `origin xyz="0.15 0
/// -0.08"` relative to `base_link`, which itself sits at `(0, 0, 0.1)` —
/// so `caster_link` sits at `(0.15, 0, 0.02)`.
#[test]
fn the_caster_sits_at_its_fixed_offset_from_base_link() {
    let robot = astrs_urdf::parse_str(FIXTURE).unwrap();
    let result = forward_kinematics(&robot, &HashMap::new()).unwrap();
    assert_vec_close(
        result["caster_link"].translation,
        Vec3::new(0.15, 0.0, 0.02),
    );
}

/// The fixed-frame skeleton (`base_joint` and `caster_joint`, both
/// `JointKind::Fixed`) becomes static `astrs_tf` transforms; the two
/// continuous wheel joints do not, since their position is not fixed at
/// parse time — see `populate_static_transforms`'s own docs.
#[test]
fn populate_static_transforms_registers_only_the_fixed_joints() {
    let robot = astrs_urdf::parse_str(FIXTURE).unwrap();
    let mut buffer = TransformBuffer::new();
    populate_static_transforms(&robot, &mut buffer, TfStamp::EPOCH).unwrap();

    assert!(buffer.is_known_frame("base_link"));
    assert!(buffer.is_known_frame("caster_link"));
    assert!(!buffer.is_known_frame("left_wheel_link"));
    assert!(!buffer.is_known_frame("right_wheel_link"));

    let looked_up = buffer
        .lookup_transform("base_footprint", "caster_link", astrs_tf::TimePoint::Latest)
        .unwrap();
    assert!((looked_up.translation.x - 0.15).abs() < EPS);
    assert!((looked_up.translation.z - 0.02).abs() < EPS);
}

#[test]
fn both_wheels_bare_reference_the_same_robot_level_material() {
    let robot = astrs_urdf::parse_str(FIXTURE).unwrap();
    assert_eq!(robot.materials.len(), 2);

    for link_name in ["left_wheel_link", "right_wheel_link"] {
        let visual = &robot.link(link_name).unwrap().visuals[0];
        let reference = visual.material.as_ref().unwrap();
        assert_eq!(reference.name, "wheel_black");
        // A bare reference (no inline color/texture) resolves against the
        // robot-level declaration.
        let resolved = robot.resolve_material(reference).unwrap();
        assert_eq!(
            resolved.color,
            Some(astrs_urdf::model::Color::new(0.1, 0.1, 0.1, 1.0))
        );
    }
}
