//! `<joint>` parsing: `type`, `<parent>`/`<child>`, `<origin>`, `<axis>`,
//! `<limit>`, `<dynamics>`, `<mimic>`, `<safety_controller>`.

use crate::JointKind;
use crate::UrdfError;
use crate::math::{Transform, Vec3};
use crate::model::{Joint, JointDynamics, JointLimit, JointMimic, JointSafetyController};
use crate::xml::{Attribute, Event, Reader, Span};

use super::attrs::{attr_f64, attr_f64_opt, attr_str, find_attr};
use super::cursor::consume_element_body;
use super::origin::parse_origin;

/// Parses a `<joint>` element's attributes and body. Called with the
/// reader positioned just after `<joint>`'s own `StartElement`; returns
/// having consumed through `<joint>`'s own `EndElement`.
///
/// # Errors
///
/// [`UrdfError::MissingAttribute`] if `name` or `type` is absent;
/// [`UrdfError::UnknownJointType`] if `type` is not one of URDF's six;
/// [`UrdfError::MissingChildElement`] if `<parent>` or `<child>` is
/// missing; [`UrdfError::InvalidAttributeValue`] if `<axis xyz="0 0 0"/>`
/// (a degenerate axis, which URDF forbids since it names no direction —
/// see [`Joint::axis`]'s own docs on why this crate always normalizes and
/// therefore must reject a zero vector rather than silently defaulting
/// it); whatever a nested element's own parsing raises otherwise.
pub(super) fn parse_joint(
    reader: &mut Reader<'_>,
    attributes: &[Attribute<'_>],
    joint_span: Span,
    self_closing: bool,
) -> crate::Result<Joint> {
    let name = attr_str("joint", "name", attributes, joint_span)?.to_owned();
    let type_attr = find_attr(attributes, "type").ok_or(UrdfError::MissingAttribute {
        element: "joint",
        attribute: "type",
        span: joint_span,
    })?;
    let kind =
        JointKind::from_urdf_str(&type_attr.value).ok_or_else(|| UrdfError::UnknownJointType {
            joint: name.clone(),
            found: type_attr.value.to_string(),
            span: type_attr.value_span,
        })?;

    if self_closing {
        return Err(UrdfError::MissingChildElement {
            parent: "joint",
            expected: "parent",
            span: joint_span,
        });
    }

    let mut origin = Transform::IDENTITY;
    let mut axis = Vec3::UNIT_X;
    let mut parent: Option<String> = None;
    let mut child: Option<String> = None;
    let mut limit = None;
    let mut dynamics = None;
    let mut mimic = None;
    let mut safety_controller = None;

    loop {
        match reader.next_event()? {
            Event::StartElement {
                name: "origin",
                attributes,
                self_closing,
                ..
            } => {
                origin = parse_origin("joint", &attributes)?;
                consume_element_body(reader, self_closing)?;
            }
            Event::StartElement {
                name: "axis",
                attributes,
                span,
                self_closing,
            } => {
                let xyz_attr =
                    find_attr(&attributes, "xyz").ok_or(UrdfError::MissingAttribute {
                        element: "axis",
                        attribute: "xyz",
                        span,
                    })?;
                let raw_axis = super::attrs::parse_vec3("axis", "xyz", xyz_attr)?;
                axis = raw_axis
                    .normalize()
                    .ok_or(UrdfError::InvalidAttributeValue {
                        element: "axis",
                        attribute: "xyz",
                        value: xyz_attr.value.to_string(),
                        span: xyz_attr.value_span,
                    })?;
                consume_element_body(reader, self_closing)?;
            }
            Event::StartElement {
                name: "parent",
                attributes,
                span,
                self_closing,
            } => {
                parent = Some(attr_str("parent", "link", &attributes, span)?.to_owned());
                consume_element_body(reader, self_closing)?;
            }
            Event::StartElement {
                name: "child",
                attributes,
                span,
                self_closing,
            } => {
                child = Some(attr_str("child", "link", &attributes, span)?.to_owned());
                consume_element_body(reader, self_closing)?;
            }
            Event::StartElement {
                name: "limit",
                attributes,
                span,
                self_closing,
            } => {
                limit = Some(JointLimit {
                    lower: attr_f64_opt("limit", "lower", &attributes, 0.0)?,
                    upper: attr_f64_opt("limit", "upper", &attributes, 0.0)?,
                    velocity: attr_f64("limit", "velocity", &attributes, span)?,
                    effort: attr_f64("limit", "effort", &attributes, span)?,
                });
                consume_element_body(reader, self_closing)?;
            }
            Event::StartElement {
                name: "dynamics",
                attributes,
                self_closing,
                ..
            } => {
                dynamics = Some(JointDynamics {
                    damping: attr_f64_opt("dynamics", "damping", &attributes, 0.0)?,
                    friction: attr_f64_opt("dynamics", "friction", &attributes, 0.0)?,
                });
                consume_element_body(reader, self_closing)?;
            }
            Event::StartElement {
                name: "mimic",
                attributes,
                span,
                self_closing,
            } => {
                mimic = Some(JointMimic {
                    joint: attr_str("mimic", "joint", &attributes, span)?.to_owned(),
                    multiplier: attr_f64_opt("mimic", "multiplier", &attributes, 1.0)?,
                    offset: attr_f64_opt("mimic", "offset", &attributes, 0.0)?,
                });
                consume_element_body(reader, self_closing)?;
            }
            Event::StartElement {
                name: "safety_controller",
                attributes,
                self_closing,
                ..
            } => {
                safety_controller = Some(JointSafetyController {
                    soft_lower_limit: attr_f64_opt(
                        "safety_controller",
                        "soft_lower_limit",
                        &attributes,
                        0.0,
                    )?,
                    soft_upper_limit: attr_f64_opt(
                        "safety_controller",
                        "soft_upper_limit",
                        &attributes,
                        0.0,
                    )?,
                    k_position: attr_f64_opt("safety_controller", "k_position", &attributes, 0.0)?,
                    k_velocity: attr_f64_opt("safety_controller", "k_velocity", &attributes, 0.0)?,
                });
                consume_element_body(reader, self_closing)?;
            }
            Event::StartElement { self_closing, .. } => {
                consume_element_body(reader, self_closing)?;
            }
            Event::EndElement { .. } => break,
            Event::Eof { span } => {
                return Err(UrdfError::Xml(crate::xml::XmlError::new(
                    crate::xml::XmlErrorKind::UnclosedElement {
                        name: "joint".to_owned(),
                    },
                    span,
                )));
            }
            Event::Text { .. }
            | Event::CData { .. }
            | Event::Comment { .. }
            | Event::ProcessingInstruction { .. } => {}
        }
    }

    let parent = parent.ok_or(UrdfError::MissingChildElement {
        parent: "joint",
        expected: "parent",
        span: joint_span,
    })?;
    let child = child.ok_or(UrdfError::MissingChildElement {
        parent: "joint",
        expected: "child",
        span: joint_span,
    })?;

    Ok(Joint {
        name,
        kind,
        parent,
        child,
        origin,
        axis,
        limit,
        dynamics,
        mimic,
        safety_controller,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn parse(xml: &str) -> crate::Result<Joint> {
        let mut reader = Reader::new(xml);
        let (attributes, span, self_closing) = match reader.next_event().unwrap() {
            Event::StartElement {
                attributes,
                span,
                self_closing,
                ..
            } => (attributes, span, self_closing),
            other => panic!("expected StartElement, got {other:?}"),
        };
        parse_joint(&mut reader, &attributes, span, self_closing)
    }

    #[test]
    fn a_minimal_revolute_joint_has_default_origin_and_axis() {
        let xml = r#"<joint name="j1" type="revolute">
            <parent link="a"/><child link="b"/>
        </joint>"#;
        let joint = parse(xml).unwrap();
        assert_eq!(joint.name, "j1");
        assert_eq!(joint.kind, JointKind::Revolute);
        assert_eq!(joint.parent, "a");
        assert_eq!(joint.child, "b");
        assert_eq!(joint.origin, Transform::IDENTITY);
        assert_eq!(joint.axis, Vec3::UNIT_X);
    }

    #[test]
    fn a_non_unit_axis_is_normalized() {
        let xml = r#"<joint name="j1" type="revolute">
            <parent link="a"/><child link="b"/>
            <axis xyz="0 0 5"/>
        </joint>"#;
        let joint = parse(xml).unwrap();
        assert_eq!(joint.axis, Vec3::UNIT_Z);
    }

    #[test]
    fn a_zero_axis_is_rejected() {
        let xml = r#"<joint name="j1" type="revolute">
            <parent link="a"/><child link="b"/>
            <axis xyz="0 0 0"/>
        </joint>"#;
        let error = parse(xml).unwrap_err();
        assert!(matches!(
            error,
            UrdfError::InvalidAttributeValue {
                element: "axis",
                attribute: "xyz",
                ..
            }
        ));
    }

    #[test]
    fn an_unknown_joint_type_is_rejected() {
        let xml = r#"<joint name="j1" type="ball">
            <parent link="a"/><child link="b"/>
        </joint>"#;
        let error = parse(xml).unwrap_err();
        assert!(matches!(
            error,
            UrdfError::UnknownJointType { joint, found, .. }
                if joint == "j1" && found == "ball"
        ));
    }

    #[test]
    fn a_missing_type_attribute_is_rejected() {
        let xml = r#"<joint name="j1"><parent link="a"/><child link="b"/></joint>"#;
        let error = parse(xml).unwrap_err();
        assert!(matches!(
            error,
            UrdfError::MissingAttribute {
                element: "joint",
                attribute: "type",
                ..
            }
        ));
    }

    #[test]
    fn a_missing_parent_is_rejected() {
        let xml = r#"<joint name="j1" type="fixed"><child link="b"/></joint>"#;
        let error = parse(xml).unwrap_err();
        assert!(matches!(
            error,
            UrdfError::MissingChildElement {
                parent: "joint",
                expected: "parent",
                ..
            }
        ));
    }

    #[test]
    fn a_missing_child_is_rejected() {
        let xml = r#"<joint name="j1" type="fixed"><parent link="a"/></joint>"#;
        let error = parse(xml).unwrap_err();
        assert!(matches!(
            error,
            UrdfError::MissingChildElement {
                parent: "joint",
                expected: "child",
                ..
            }
        ));
    }

    #[test]
    fn origin_xyz_and_rpy_both_parse() {
        let xml = r#"<joint name="j1" type="fixed">
            <parent link="a"/><child link="b"/>
            <origin xyz="1 2 3" rpy="0 0 0"/>
        </joint>"#;
        let joint = parse(xml).unwrap();
        assert_eq!(joint.origin.translation, Vec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn a_full_limit_element_parses_all_four_fields() {
        let xml = r#"<joint name="j1" type="revolute">
            <parent link="a"/><child link="b"/>
            <limit lower="-1.5" upper="1.5" velocity="2" effort="10"/>
        </joint>"#;
        let joint = parse(xml).unwrap();
        let limit = joint.limit.unwrap();
        assert_eq!(limit.lower, -1.5);
        assert_eq!(limit.upper, 1.5);
        assert_eq!(limit.velocity, 2.0);
        assert_eq!(limit.effort, 10.0);
    }

    #[test]
    fn a_limit_missing_required_velocity_is_rejected() {
        let xml = r#"<joint name="j1" type="revolute">
            <parent link="a"/><child link="b"/>
            <limit lower="-1" upper="1" effort="10"/>
        </joint>"#;
        let error = parse(xml).unwrap_err();
        assert!(matches!(
            error,
            UrdfError::MissingAttribute {
                element: "limit",
                attribute: "velocity",
                ..
            }
        ));
    }

    #[test]
    fn dynamics_defaults_when_attributes_are_absent() {
        let xml = r#"<joint name="j1" type="revolute">
            <parent link="a"/><child link="b"/>
            <dynamics/>
        </joint>"#;
        let joint = parse(xml).unwrap();
        let dynamics = joint.dynamics.unwrap();
        assert_eq!(dynamics.damping, 0.0);
        assert_eq!(dynamics.friction, 0.0);
    }

    #[test]
    fn dynamics_parses_explicit_values() {
        let xml = r#"<joint name="j1" type="revolute">
            <parent link="a"/><child link="b"/>
            <dynamics damping="0.5" friction="0.1"/>
        </joint>"#;
        let joint = parse(xml).unwrap();
        let dynamics = joint.dynamics.unwrap();
        assert_eq!(dynamics.damping, 0.5);
        assert_eq!(dynamics.friction, 0.1);
    }

    #[test]
    fn mimic_parses_with_default_multiplier_and_offset() {
        let xml = r#"<joint name="j1" type="revolute">
            <parent link="a"/><child link="b"/>
            <mimic joint="other"/>
        </joint>"#;
        let joint = parse(xml).unwrap();
        let mimic = joint.mimic.unwrap();
        assert_eq!(mimic.joint, "other");
        assert_eq!(mimic.multiplier, 1.0);
        assert_eq!(mimic.offset, 0.0);
    }

    #[test]
    fn mimic_parses_explicit_multiplier_and_offset() {
        let xml = r#"<joint name="j1" type="revolute">
            <parent link="a"/><child link="b"/>
            <mimic joint="other" multiplier="2" offset="0.1"/>
        </joint>"#;
        let joint = parse(xml).unwrap();
        let mimic = joint.mimic.unwrap();
        assert_eq!(mimic.multiplier, 2.0);
        assert_eq!(mimic.offset, 0.1);
    }

    #[test]
    fn safety_controller_parses_all_four_fields() {
        let xml = r#"<joint name="j1" type="revolute">
            <parent link="a"/><child link="b"/>
            <safety_controller soft_lower_limit="-1" soft_upper_limit="1" k_position="10" k_velocity="5"/>
        </joint>"#;
        let joint = parse(xml).unwrap();
        let sc = joint.safety_controller.unwrap();
        assert_eq!(sc.soft_lower_limit, -1.0);
        assert_eq!(sc.soft_upper_limit, 1.0);
        assert_eq!(sc.k_position, 10.0);
        assert_eq!(sc.k_velocity, 5.0);
    }

    #[test]
    fn a_self_closing_joint_is_rejected_for_missing_parent() {
        let error = parse(r#"<joint name="j1" type="fixed"/>"#).unwrap_err();
        assert!(matches!(
            error,
            UrdfError::MissingChildElement {
                parent: "joint",
                expected: "parent",
                ..
            }
        ));
    }

    #[test]
    fn the_reader_lands_on_the_next_sibling_after_a_joint_element() {
        // Wrapped in `<root>` since a bare document can only have one
        // top-level element — see `crate::xml`'s `ContentAfterRoot`.
        let xml = r#"<root><joint name="j1" type="fixed"><parent link="a"/><child link="b"/></joint><next/></root>"#;
        let mut reader = Reader::new(xml);
        reader.next_event().unwrap(); // <root>
        let (attributes, span, self_closing) = match reader.next_event().unwrap() {
            Event::StartElement {
                attributes,
                span,
                self_closing,
                ..
            } => (attributes, span, self_closing),
            other => panic!("expected StartElement, got {other:?}"),
        };
        parse_joint(&mut reader, &attributes, span, self_closing).unwrap();
        assert!(matches!(
            reader.next_event().unwrap(),
            Event::StartElement { name: "next", .. }
        ));
    }
}
