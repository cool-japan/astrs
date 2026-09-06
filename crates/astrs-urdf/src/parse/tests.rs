//! End-to-end [`super::parse_str`] tests: a whole document at a time,
//! rather than one element in isolation (each element parser already has
//! its own focused tests in its own module).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::{JointKind, UrdfError};

#[test]
fn a_minimal_single_link_robot_parses() {
    let robot = parse_str(r#"<robot name="demo"><link name="base_link"/></robot>"#).unwrap();
    assert_eq!(robot.name, "demo");
    assert_eq!(robot.links.len(), 1);
    assert_eq!(robot.links[0].name, "base_link");
}

#[test]
fn the_xml_declaration_and_comments_before_robot_are_tolerated() {
    let xml = r#"<?xml version="1.0"?>
<!-- a demo robot -->
<robot name="demo"><link name="base_link"/></robot>"#;
    let robot = parse_str(xml).unwrap();
    assert_eq!(robot.name, "demo");
}

#[test]
fn a_two_link_one_joint_robot_parses_and_validates() {
    let xml = r#"<robot name="demo">
        <link name="base_link"/>
        <link name="arm_link"/>
        <joint name="base_to_arm" type="revolute">
            <parent link="base_link"/>
            <child link="arm_link"/>
            <axis xyz="0 0 1"/>
            <limit lower="-1.57" upper="1.57" velocity="1" effort="10"/>
        </joint>
    </robot>"#;
    let robot = parse_str(xml).unwrap();
    assert_eq!(robot.links.len(), 2);
    assert_eq!(robot.joints.len(), 1);
    assert_eq!(robot.joints[0].kind, JointKind::Revolute);
    assert_eq!(robot.validate(), Ok(()));
}

#[test]
fn a_root_element_other_than_robot_is_rejected() {
    let error = parse_str(r#"<not_a_robot/>"#).unwrap_err();
    assert!(matches!(
        error,
        UrdfError::UnexpectedElement {
            expected: "robot",
            ..
        }
    ));
}

#[test]
fn malformed_xml_surfaces_as_a_wrapped_xml_error() {
    // `<visual>` never closes before the mismatched `</robot>` arrives —
    // an XML-syntax-level failure (deep inside `<link>`'s own dispatch
    // loop), not a URDF-shape one, so it must come back as `UrdfError::Xml`
    // rather than any of the URDF-specific variants. `<link name="a">`
    // itself is well-formed (a real `name` attribute) precisely so this
    // test isolates the XML-syntax failure from an unrelated
    // `MissingAttribute` one.
    let error = parse_str(r#"<robot name="demo"><link name="a"><visual></robot>"#).unwrap_err();
    assert!(matches!(error, UrdfError::Xml(_)), "{error:?}");
}

#[test]
fn an_empty_document_reports_a_missing_root_element() {
    let error = parse_str("").unwrap_err();
    assert!(matches!(error, UrdfError::Xml(_)));
}

#[test]
fn robot_level_materials_and_visual_references_both_parse() {
    let xml = r#"<robot name="demo">
        <material name="blue"><color rgba="0 0 1 1"/></material>
        <link name="base_link">
            <visual>
                <geometry><box size="1 1 1"/></geometry>
                <material name="blue"/>
            </visual>
        </link>
    </robot>"#;
    let robot = parse_str(xml).unwrap();
    assert_eq!(robot.materials.len(), 1);
    assert_eq!(robot.validate(), Ok(()));
    let visual_material = robot.links[0].visuals[0].material.as_ref().unwrap();
    let resolved = robot.resolve_material(visual_material).unwrap();
    assert_eq!(
        resolved.color,
        Some(crate::model::Color::new(0.0, 0.0, 1.0, 1.0))
    );
}

#[test]
fn a_full_three_dof_arm_with_all_joint_kinds_of_interest_parses() {
    let xml = r#"<robot name="arm">
        <link name="base_link"/>
        <link name="link1"/>
        <link name="link2"/>
        <link name="link3"/>
        <joint name="joint1" type="revolute">
            <parent link="base_link"/><child link="link1"/>
            <axis xyz="0 0 1"/>
            <limit lower="-3.14" upper="3.14" velocity="1" effort="10"/>
        </joint>
        <joint name="joint2" type="continuous">
            <parent link="link1"/><child link="link2"/>
            <axis xyz="1 0 0"/>
        </joint>
        <joint name="joint3" type="prismatic">
            <parent link="link2"/><child link="link3"/>
            <axis xyz="0 1 0"/>
            <limit lower="0" upper="0.5" velocity="0.5" effort="5"/>
        </joint>
    </robot>"#;
    let robot = parse_str(xml).unwrap();
    assert_eq!(robot.joints.len(), 3);
    assert_eq!(robot.joints[0].kind, JointKind::Revolute);
    assert_eq!(robot.joints[1].kind, JointKind::Continuous);
    assert_eq!(robot.joints[2].kind, JointKind::Prismatic);
    assert_eq!(robot.validate(), Ok(()));
}
