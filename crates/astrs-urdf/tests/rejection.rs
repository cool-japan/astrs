//! Malformed-XML and invalid-tree rejection tests: every document here is
//! deliberately broken in exactly one way, and the assertion checks this
//! crate reports the *specific* failure that one break causes — not merely
//! that parsing or validation fails somehow.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_urdf::UrdfError;

// ---------------------------------------------------------------------
// Malformed XML — rejected by `parse_str` itself, before any URDF-level
// check runs at all.
// ---------------------------------------------------------------------

#[test]
fn an_unclosed_tag_is_rejected_as_xml() {
    let error = astrs_urdf::parse_str(r#"<robot name="a"><link name="base"></robot>"#).unwrap_err();
    assert!(matches!(&error, UrdfError::Xml(_)), "{error:?}");
}

#[test]
fn a_mismatched_closing_tag_is_rejected_as_xml() {
    let error =
        astrs_urdf::parse_str(r#"<robot name="a"><link name="base"></joint></robot>"#).unwrap_err();
    assert!(matches!(&error, UrdfError::Xml(_)), "{error:?}");
}

#[test]
fn an_unescaped_ampersand_is_rejected_as_xml() {
    let error =
        astrs_urdf::parse_str(r#"<robot name="a & b"><link name="base"/></robot>"#).unwrap_err();
    assert!(matches!(&error, UrdfError::Xml(_)), "{error:?}");
}

#[test]
fn a_document_with_no_root_element_is_rejected_as_xml() {
    let error = astrs_urdf::parse_str("   \n  ").unwrap_err();
    assert!(matches!(&error, UrdfError::Xml(_)), "{error:?}");
}

#[test]
fn a_root_element_other_than_robot_is_rejected() {
    let error = astrs_urdf::parse_str(r#"<not_a_robot name="a"/>"#).unwrap_err();
    assert!(
        matches!(
            &error,
            UrdfError::UnexpectedElement {
                expected: "robot",
                ..
            }
        ),
        "{error:?}"
    );
}

// ---------------------------------------------------------------------
// Well-formed XML, malformed URDF shape.
// ---------------------------------------------------------------------

#[test]
fn a_joint_with_an_unknown_type_is_rejected() {
    let xml = r#"<robot name="a">
        <link name="base"/><link name="tip"/>
        <joint name="j1" type="spherical">
            <parent link="base"/><child link="tip"/>
        </joint>
    </robot>"#;
    let error = astrs_urdf::parse_str(xml).unwrap_err();
    assert!(
        matches!(&error, UrdfError::UnknownJointType { found, .. } if found == "spherical"),
        "{error:?}"
    );
}

#[test]
fn a_joint_missing_its_parent_element_is_rejected() {
    let xml = r#"<robot name="a">
        <link name="base"/><link name="tip"/>
        <joint name="j1" type="fixed"><child link="tip"/></joint>
    </robot>"#;
    let error = astrs_urdf::parse_str(xml).unwrap_err();
    assert!(
        matches!(
            &error,
            UrdfError::MissingChildElement {
                parent: "joint",
                expected: "parent",
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_link_missing_its_name_attribute_is_rejected() {
    let error = astrs_urdf::parse_str(r#"<robot name="a"><link/></robot>"#).unwrap_err();
    assert!(
        matches!(
            &error,
            UrdfError::MissingAttribute {
                element: "link",
                attribute: "name",
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_zero_length_joint_axis_is_rejected() {
    let xml = r#"<robot name="a">
        <link name="base"/><link name="tip"/>
        <joint name="j1" type="revolute">
            <parent link="base"/><child link="tip"/>
            <axis xyz="0 0 0"/>
        </joint>
    </robot>"#;
    let error = astrs_urdf::parse_str(xml).unwrap_err();
    assert!(
        matches!(
            &error,
            UrdfError::InvalidAttributeValue {
                element: "axis",
                attribute: "xyz",
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_non_numeric_mass_value_is_rejected() {
    let xml = r#"<robot name="a">
        <link name="base">
            <inertial>
                <mass value="heavy"/>
                <inertia ixx="1" ixy="0" ixz="0" iyy="1" iyz="0" izz="1"/>
            </inertial>
        </link>
    </robot>"#;
    let error = astrs_urdf::parse_str(xml).unwrap_err();
    assert!(
        matches!(
            &error,
            UrdfError::InvalidAttributeValue {
                element: "mass",
                attribute: "value",
                ..
            }
        ),
        "{error:?}"
    );
}

// ---------------------------------------------------------------------
// Well-formed URDF, invalid tree — caught by `Robot::validate`.
// ---------------------------------------------------------------------

fn parse_and_validate(xml: &str) -> UrdfError {
    astrs_urdf::parse_str(xml)
        .expect("well-formed URDF syntax")
        .validate()
        .expect_err("an invalid tree")
}

#[test]
fn two_links_sharing_a_name_is_rejected() {
    let xml = r#"<robot name="a">
        <link name="dup"/><link name="dup"/>
    </robot>"#;
    let error = parse_and_validate(xml);
    assert!(
        matches!(&error, UrdfError::DuplicateLinkName { name } if name == "dup"),
        "{error:?}"
    );
}

#[test]
fn two_joints_sharing_a_name_is_rejected() {
    let xml = r#"<robot name="a">
        <link name="a"/><link name="b"/><link name="c"/>
        <joint name="dup" type="fixed"><parent link="a"/><child link="b"/></joint>
        <joint name="dup" type="fixed"><parent link="b"/><child link="c"/></joint>
    </robot>"#;
    let error = parse_and_validate(xml);
    assert!(
        matches!(&error, UrdfError::DuplicateJointName { name } if name == "dup"),
        "{error:?}"
    );
}

#[test]
fn a_joint_referencing_an_undeclared_parent_link_is_rejected() {
    let xml = r#"<robot name="a">
        <link name="tip"/>
        <joint name="j1" type="fixed">
            <parent link="nonexistent"/><child link="tip"/>
        </joint>
    </robot>"#;
    let error = parse_and_validate(xml);
    assert!(
        matches!(&error, UrdfError::UnknownParentLink { link, .. } if link == "nonexistent"),
        "{error:?}"
    );
}

#[test]
fn a_joint_referencing_an_undeclared_child_link_is_rejected() {
    let xml = r#"<robot name="a">
        <link name="base"/>
        <joint name="j1" type="fixed">
            <parent link="base"/><child link="nonexistent"/>
        </joint>
    </robot>"#;
    let error = parse_and_validate(xml);
    assert!(
        matches!(&error, UrdfError::UnknownChildLink { link, .. } if link == "nonexistent"),
        "{error:?}"
    );
}

#[test]
fn a_link_with_two_parent_joints_is_rejected() {
    let xml = r#"<robot name="a">
        <link name="a"/><link name="b"/><link name="c"/>
        <joint name="j1" type="fixed"><parent link="a"/><child link="c"/></joint>
        <joint name="j2" type="fixed"><parent link="b"/><child link="c"/></joint>
    </robot>"#;
    let error = parse_and_validate(xml);
    assert!(
        matches!(&error, UrdfError::MultipleParentJoints { link, .. } if link == "c"),
        "{error:?}"
    );
}

#[test]
fn a_full_cycle_with_no_root_is_rejected() {
    let xml = r#"<robot name="a">
        <link name="a"/><link name="b"/>
        <joint name="j1" type="fixed"><parent link="a"/><child link="b"/></joint>
        <joint name="j2" type="fixed"><parent link="b"/><child link="a"/></joint>
    </robot>"#;
    let error = parse_and_validate(xml);
    assert_eq!(error, UrdfError::NoRootLink);
}

#[test]
fn two_disconnected_trees_report_multiple_roots() {
    let xml = r#"<robot name="a">
        <link name="root1"/><link name="child1"/>
        <link name="root2"/><link name="child2"/>
        <joint name="j1" type="fixed"><parent link="root1"/><child link="child1"/></joint>
        <joint name="j2" type="fixed"><parent link="root2"/><child link="child2"/></joint>
    </robot>"#;
    let error = parse_and_validate(xml);
    assert!(
        matches!(&error, UrdfError::MultipleRootLinks { .. }),
        "{error:?}"
    );
}

#[test]
fn a_mimic_referencing_an_undeclared_joint_is_rejected() {
    let xml = r#"<robot name="a">
        <link name="a"/><link name="b"/>
        <joint name="j1" type="revolute">
            <parent link="a"/><child link="b"/>
            <mimic joint="nonexistent"/>
        </joint>
    </robot>"#;
    let error = parse_and_validate(xml);
    assert!(
        matches!(&error, UrdfError::UnknownMimicTarget { target, .. } if target == "nonexistent"),
        "{error:?}"
    );
}

#[test]
fn a_two_joint_mimic_cycle_is_rejected() {
    let xml = r#"<robot name="a">
        <link name="root"/><link name="a"/><link name="b"/>
        <joint name="j1" type="revolute">
            <parent link="root"/><child link="a"/>
            <mimic joint="j2"/>
        </joint>
        <joint name="j2" type="revolute">
            <parent link="a"/><child link="b"/>
            <mimic joint="j1"/>
        </joint>
    </robot>"#;
    let error = parse_and_validate(xml);
    assert!(matches!(&error, UrdfError::MimicCycle { .. }), "{error:?}");
}

#[test]
fn an_undeclared_bare_material_reference_is_rejected() {
    let xml = r#"<robot name="a">
        <link name="base">
            <visual>
                <geometry><sphere radius="1"/></geometry>
                <material name="nonexistent"/>
            </visual>
        </link>
    </robot>"#;
    let error = parse_and_validate(xml);
    assert!(
        matches!(&error, UrdfError::UnknownMaterial { material, .. } if material == "nonexistent"),
        "{error:?}"
    );
}

// ---------------------------------------------------------------------
// A syntactically- and structurally-valid document that still fails at
// the kinematics layer.
// ---------------------------------------------------------------------

#[test]
fn a_chain_query_between_unrelated_branches_is_rejected() {
    use astrs_urdf::kinematics::Chain;

    let xml = r#"<robot name="a">
        <link name="root"/><link name="branch_a"/><link name="branch_b"/>
        <joint name="ja" type="fixed"><parent link="root"/><child link="branch_a"/></joint>
        <joint name="jb" type="fixed"><parent link="root"/><child link="branch_b"/></joint>
    </robot>"#;
    let robot = astrs_urdf::parse_str(xml).unwrap();
    robot.validate().unwrap();
    let error = Chain::extract(&robot, "branch_a", "branch_b").unwrap_err();
    assert!(
        matches!(&error, UrdfError::NoChainBetween { .. }),
        "{error:?}"
    );
}

#[test]
fn a_position_map_with_the_wrong_dof_shape_is_rejected() {
    use std::collections::HashMap;

    use astrs_urdf::kinematics::{JointPosition, forward_kinematics};

    let xml = r#"<robot name="a">
        <link name="base"/><link name="tip"/>
        <joint name="j1" type="revolute">
            <parent link="base"/><child link="tip"/>
        </joint>
    </robot>"#;
    let robot = astrs_urdf::parse_str(xml).unwrap();
    let mut positions = HashMap::new();
    // A revolute joint is 1-DOF (Scalar); Floating is 6-DOF.
    positions.insert(
        "j1".to_owned(),
        JointPosition::Floating(astrs_urdf::math::Transform::IDENTITY),
    );
    let error = forward_kinematics(&robot, &positions).unwrap_err();
    assert!(
        matches!(&error, UrdfError::JointPositionKindMismatch { joint, .. } if joint == "j1"),
        "{error:?}"
    );
}
