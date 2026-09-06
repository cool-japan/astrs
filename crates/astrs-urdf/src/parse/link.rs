//! `<link>` parsing: `<inertial>`, any number of `<visual>`/`<collision>`.

use crate::UrdfError;
use crate::math::Transform;
use crate::model::{Collision, InertiaTensor, Inertial, Link, Visual};
use crate::xml::{Attribute, Event, Reader, Span};

use super::attrs::{attr_f64, attr_str};
use super::cursor::consume_element_body;
use super::geometry::parse_geometry;
use super::material::parse_material;
use super::origin::parse_origin;

/// Parses a `<link>` element's attributes and body. Called with the reader
/// positioned just after `<link>`'s own `StartElement`; returns having
/// consumed through `<link>`'s own `EndElement`.
///
/// # Errors
///
/// [`UrdfError::MissingAttribute`] if `name` is absent; whatever a nested
/// `<inertial>`/`<visual>`/`<collision>` raises otherwise.
pub(super) fn parse_link(
    reader: &mut Reader<'_>,
    attributes: &[Attribute<'_>],
    span: Span,
    self_closing: bool,
) -> crate::Result<Link> {
    let name = attr_str("link", "name", attributes, span)?.to_owned();
    if self_closing {
        // Drains the reader's pending synthesized `EndElement` for this
        // self-closing tag before returning — otherwise it would still be
        // queued when the *caller's* loop next polls the reader, and get
        // misread as the caller's own closing tag, corrupting every
        // sibling parsed after this one.
        consume_element_body(reader, self_closing)?;
        return Ok(Link::named(name));
    }
    let mut link = Link::named(name);

    loop {
        match reader.next_event()? {
            Event::StartElement {
                name: "inertial",
                span,
                self_closing,
                ..
            } => {
                link.inertial = Some(parse_inertial(reader, span, self_closing)?);
            }
            Event::StartElement {
                name: "visual",
                attributes,
                span,
                self_closing,
            } => {
                link.visuals
                    .push(parse_visual(reader, &attributes, span, self_closing)?);
            }
            Event::StartElement {
                name: "collision",
                attributes,
                span,
                self_closing,
            } => {
                link.collisions
                    .push(parse_collision(reader, &attributes, span, self_closing)?);
            }
            Event::StartElement { self_closing, .. } => {
                // An extension element this crate does not model
                // (`<gazebo>`, a custom tag) — see `super::cursor`'s module
                // docs on why skipping it is exactly as correct as parsing
                // a recognized one.
                consume_element_body(reader, self_closing)?;
            }
            Event::EndElement { .. } => break,
            Event::Eof { span } => {
                return Err(UrdfError::Xml(crate::xml::XmlError::new(
                    crate::xml::XmlErrorKind::UnclosedElement {
                        name: "link".to_owned(),
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

    Ok(link)
}

fn parse_inertial(
    reader: &mut Reader<'_>,
    inertial_span: Span,
    self_closing: bool,
) -> crate::Result<Inertial> {
    if self_closing {
        return Err(UrdfError::MissingChildElement {
            parent: "inertial",
            expected: "mass",
            span: inertial_span,
        });
    }

    let mut origin = Transform::IDENTITY;
    let mut mass: Option<f64> = None;
    let mut inertia: Option<InertiaTensor> = None;
    loop {
        match reader.next_event()? {
            Event::StartElement {
                name: "origin",
                attributes,
                self_closing,
                ..
            } => {
                origin = parse_origin("inertial", &attributes)?;
                consume_element_body(reader, self_closing)?;
            }
            Event::StartElement {
                name: "mass",
                attributes,
                span,
                self_closing,
            } => {
                mass = Some(attr_f64("mass", "value", &attributes, span)?);
                consume_element_body(reader, self_closing)?;
            }
            Event::StartElement {
                name: "inertia",
                attributes,
                span,
                self_closing,
            } => {
                inertia = Some(InertiaTensor {
                    ixx: attr_f64("inertia", "ixx", &attributes, span)?,
                    ixy: attr_f64("inertia", "ixy", &attributes, span)?,
                    ixz: attr_f64("inertia", "ixz", &attributes, span)?,
                    iyy: attr_f64("inertia", "iyy", &attributes, span)?,
                    iyz: attr_f64("inertia", "iyz", &attributes, span)?,
                    izz: attr_f64("inertia", "izz", &attributes, span)?,
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
                        name: "inertial".to_owned(),
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

    let mass = mass.ok_or(UrdfError::MissingChildElement {
        parent: "inertial",
        expected: "mass",
        span: inertial_span,
    })?;
    let inertia = inertia.ok_or(UrdfError::MissingChildElement {
        parent: "inertial",
        expected: "inertia",
        span: inertial_span,
    })?;
    Ok(Inertial {
        origin,
        mass,
        inertia,
    })
}

fn parse_visual(
    reader: &mut Reader<'_>,
    attributes: &[Attribute<'_>],
    visual_span: Span,
    self_closing: bool,
) -> crate::Result<Visual> {
    let name = super::attrs::find_attr(attributes, "name").map(|a| a.value.to_string());
    if self_closing {
        return Err(UrdfError::MissingChildElement {
            parent: "visual",
            expected: "geometry",
            span: visual_span,
        });
    }

    let mut origin = Transform::IDENTITY;
    let mut geometry = None;
    let mut material = None;
    loop {
        match reader.next_event()? {
            Event::StartElement {
                name: "origin",
                attributes,
                self_closing,
                ..
            } => {
                origin = parse_origin("visual", &attributes)?;
                consume_element_body(reader, self_closing)?;
            }
            Event::StartElement {
                name: "geometry",
                span,
                ..
            } => {
                geometry = Some(parse_geometry(reader, span)?);
            }
            Event::StartElement {
                name: "material",
                attributes,
                span,
                self_closing,
            } => {
                material = Some(parse_material(reader, &attributes, span, self_closing)?);
            }
            Event::StartElement { self_closing, .. } => {
                consume_element_body(reader, self_closing)?;
            }
            Event::EndElement { .. } => break,
            Event::Eof { span } => {
                return Err(UrdfError::Xml(crate::xml::XmlError::new(
                    crate::xml::XmlErrorKind::UnclosedElement {
                        name: "visual".to_owned(),
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

    let geometry = geometry.ok_or(UrdfError::MissingChildElement {
        parent: "visual",
        expected: "geometry",
        span: visual_span,
    })?;
    Ok(Visual {
        name,
        origin,
        geometry,
        material,
    })
}

fn parse_collision(
    reader: &mut Reader<'_>,
    attributes: &[Attribute<'_>],
    collision_span: Span,
    self_closing: bool,
) -> crate::Result<Collision> {
    let name = super::attrs::find_attr(attributes, "name").map(|a| a.value.to_string());
    if self_closing {
        return Err(UrdfError::MissingChildElement {
            parent: "collision",
            expected: "geometry",
            span: collision_span,
        });
    }

    let mut origin = Transform::IDENTITY;
    let mut geometry = None;
    loop {
        match reader.next_event()? {
            Event::StartElement {
                name: "origin",
                attributes,
                self_closing,
                ..
            } => {
                origin = parse_origin("collision", &attributes)?;
                consume_element_body(reader, self_closing)?;
            }
            Event::StartElement {
                name: "geometry",
                span,
                ..
            } => {
                geometry = Some(parse_geometry(reader, span)?);
            }
            Event::StartElement { self_closing, .. } => {
                consume_element_body(reader, self_closing)?;
            }
            Event::EndElement { .. } => break,
            Event::Eof { span } => {
                return Err(UrdfError::Xml(crate::xml::XmlError::new(
                    crate::xml::XmlErrorKind::UnclosedElement {
                        name: "collision".to_owned(),
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

    let geometry = geometry.ok_or(UrdfError::MissingChildElement {
        parent: "collision",
        expected: "geometry",
        span: collision_span,
    })?;
    Ok(Collision {
        name,
        origin,
        geometry,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::math::Vec3;
    use crate::model::Geometry;

    fn parse(xml: &str) -> crate::Result<Link> {
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
        parse_link(&mut reader, &attributes, span, self_closing)
    }

    #[test]
    fn a_minimal_self_closing_link_has_no_inertial_or_geometry() {
        let link = parse(r#"<link name="base_link"/>"#).unwrap();
        assert_eq!(link.name, "base_link");
        assert_eq!(link.inertial, None);
        assert!(link.visuals.is_empty());
        assert!(link.collisions.is_empty());
    }

    #[test]
    fn an_empty_bodied_link_has_no_inertial_or_geometry() {
        let link = parse(r#"<link name="base_link"></link>"#).unwrap();
        assert_eq!(link.name, "base_link");
        assert!(link.visuals.is_empty());
    }

    #[test]
    fn a_full_inertial_element_parses() {
        let xml = r#"<link name="a">
            <inertial>
                <origin xyz="0.1 0 0" rpy="0 0 0"/>
                <mass value="1.5"/>
                <inertia ixx="1" ixy="0" ixz="0" iyy="1" iyz="0" izz="1"/>
            </inertial>
        </link>"#;
        let link = parse(xml).unwrap();
        let inertial = link.inertial.unwrap();
        assert_eq!(inertial.mass, 1.5);
        assert_eq!(inertial.origin.translation, Vec3::new(0.1, 0.0, 0.0));
        assert_eq!(inertial.inertia.ixx, 1.0);
        assert_eq!(inertial.inertia.izz, 1.0);
    }

    #[test]
    fn an_inertial_missing_mass_is_rejected() {
        let xml = r#"<link name="a"><inertial><inertia ixx="1" ixy="0" ixz="0" iyy="1" iyz="0" izz="1"/></inertial></link>"#;
        let error = parse(xml).unwrap_err();
        assert!(matches!(
            error,
            UrdfError::MissingChildElement {
                parent: "inertial",
                expected: "mass",
                ..
            }
        ));
    }

    #[test]
    fn multiple_visuals_all_parse_in_document_order() {
        let xml = r#"<link name="a">
            <visual><geometry><sphere radius="1"/></geometry></visual>
            <visual><geometry><box size="1 1 1"/></geometry></visual>
        </link>"#;
        let link = parse(xml).unwrap();
        assert_eq!(link.visuals.len(), 2);
        assert_eq!(link.visuals[0].geometry, Geometry::Sphere { radius: 1.0 });
        assert!(matches!(link.visuals[1].geometry, Geometry::Box { .. }));
    }

    #[test]
    fn a_visual_missing_geometry_is_rejected() {
        let xml = r#"<link name="a"><visual/></link>"#;
        let error = parse(xml).unwrap_err();
        assert!(matches!(
            error,
            UrdfError::MissingChildElement {
                parent: "visual",
                expected: "geometry",
                ..
            }
        ));
    }

    #[test]
    fn a_visual_with_an_inline_material_parses_its_color() {
        let xml = r#"<link name="a"><visual>
            <geometry><sphere radius="1"/></geometry>
            <material name="red"><color rgba="1 0 0 1"/></material>
        </visual></link>"#;
        let link = parse(xml).unwrap();
        let material = link.visuals[0].material.as_ref().unwrap();
        assert_eq!(material.name, "red");
        assert!(material.color.is_some());
    }

    #[test]
    fn a_collision_parses_geometry_and_origin_but_has_no_material_field() {
        let xml = r#"<link name="a"><collision>
            <origin xyz="0 0 1"/>
            <geometry><cylinder radius="0.1" length="1"/></geometry>
        </collision></link>"#;
        let link = parse(xml).unwrap();
        assert_eq!(link.collisions.len(), 1);
        assert_eq!(
            link.collisions[0].origin.translation,
            Vec3::new(0.0, 0.0, 1.0)
        );
    }

    #[test]
    fn an_unrecognized_extension_element_is_skipped() {
        let xml = r#"<link name="a"><gazebo><material>Gazebo/Blue</material></gazebo></link>"#;
        let link = parse(xml).unwrap();
        assert_eq!(link.name, "a");
        assert!(link.visuals.is_empty());
    }

    #[test]
    fn the_reader_lands_on_the_next_sibling_after_a_link_element() {
        // Wrapped in `<root>` since a bare document can only have one
        // top-level element — see `crate::xml`'s `ContentAfterRoot`.
        let mut reader = Reader::new(r#"<root><link name="a"/><next/></root>"#);
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
        parse_link(&mut reader, &attributes, span, self_closing).unwrap();
        assert!(matches!(
            reader.next_event().unwrap(),
            Event::StartElement { name: "next", .. }
        ));
    }
}
