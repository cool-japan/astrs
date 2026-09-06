//! `<geometry>` parsing: dispatches on its single child element
//! (`box`/`cylinder`/`sphere`/`mesh`) to build a [`Geometry`].

use crate::UrdfError;
use crate::math::Vec3;
use crate::model::Geometry;
use crate::xml::{Event, Reader};

use super::attrs::{attr_f64, attr_str, attr_vec3_opt, find_attr, parse_vec3};
use super::cursor::consume_element_body;

/// Parses a `<geometry>` element's body: exactly one of `<box>`,
/// `<cylinder>`, `<sphere>`, `<mesh>`. Called with the reader positioned
/// just after `<geometry>`'s own `StartElement`; returns having consumed
/// through `<geometry>`'s own `EndElement` (see [`super::cursor`]'s module
/// docs for this shared contract) — note that finding the recognized shape
/// child is *not* itself enough to return: this keeps looping (tolerating,
/// not erroring on, anything else `<geometry>` might still contain) until
/// its *own* `EndElement` closes it, exactly the way every other
/// container-shaped parser in this module does.
///
/// # Errors
///
/// [`UrdfError::MissingChildElement`] if `<geometry>` closes with no shape
/// child; [`UrdfError::UnexpectedElement`] if its child is not one of the
/// four recognized shape tags; whatever the recognized shape's own
/// attribute parsing raises otherwise.
pub(super) fn parse_geometry(
    reader: &mut Reader<'_>,
    geometry_span: crate::xml::Span,
) -> crate::Result<Geometry> {
    let mut geometry: Option<Geometry> = None;
    loop {
        match reader.next_event()? {
            Event::StartElement {
                name: "box",
                attributes,
                span,
                self_closing,
            } => {
                let size_attr =
                    find_attr(&attributes, "size").ok_or(UrdfError::MissingAttribute {
                        element: "box",
                        attribute: "size",
                        span,
                    })?;
                let size = parse_vec3("box", "size", size_attr)?;
                consume_element_body(reader, self_closing)?;
                geometry.get_or_insert(Geometry::Box { size });
            }
            Event::StartElement {
                name: "cylinder",
                attributes,
                span,
                self_closing,
            } => {
                let radius = attr_f64("cylinder", "radius", &attributes, span)?;
                let length = attr_f64("cylinder", "length", &attributes, span)?;
                consume_element_body(reader, self_closing)?;
                geometry.get_or_insert(Geometry::Cylinder { radius, length });
            }
            Event::StartElement {
                name: "sphere",
                attributes,
                span,
                self_closing,
            } => {
                let radius = attr_f64("sphere", "radius", &attributes, span)?;
                consume_element_body(reader, self_closing)?;
                geometry.get_or_insert(Geometry::Sphere { radius });
            }
            Event::StartElement {
                name: "mesh",
                attributes,
                span,
                self_closing,
            } => {
                let filename = attr_str("mesh", "filename", &attributes, span)?.to_owned();
                let scale = attr_vec3_opt("mesh", "scale", &attributes, Vec3::new(1.0, 1.0, 1.0))?;
                consume_element_body(reader, self_closing)?;
                geometry.get_or_insert(Geometry::Mesh { filename, scale });
            }
            Event::StartElement {
                name,
                span,
                self_closing,
                ..
            } => {
                consume_element_body(reader, self_closing)?;
                return Err(UrdfError::UnexpectedElement {
                    expected: "box, cylinder, sphere, or mesh",
                    found: name.to_owned(),
                    span,
                });
            }
            Event::EndElement { .. } => {
                return geometry.ok_or(UrdfError::MissingChildElement {
                    parent: "geometry",
                    expected: "box, cylinder, sphere, or mesh",
                    span: geometry_span,
                });
            }
            Event::Eof { span } => {
                return Err(UrdfError::Xml(crate::xml::XmlError::new(
                    crate::xml::XmlErrorKind::UnclosedElement {
                        name: "geometry".to_owned(),
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
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn parse(xml: &str) -> crate::Result<Geometry> {
        let mut reader = Reader::new(xml);
        let span = match reader.next_event().unwrap() {
            Event::StartElement { span, .. } => span,
            other => panic!("expected StartElement, got {other:?}"),
        };
        parse_geometry(&mut reader, span)
    }

    #[test]
    fn parses_a_box() {
        let geometry = parse(r#"<geometry><box size="1 2 3"/></geometry>"#).unwrap();
        assert_eq!(
            geometry,
            Geometry::Box {
                size: Vec3::new(1.0, 2.0, 3.0)
            }
        );
    }

    #[test]
    fn parses_a_cylinder() {
        let geometry =
            parse(r#"<geometry><cylinder radius="0.5" length="2"/></geometry>"#).unwrap();
        assert_eq!(
            geometry,
            Geometry::Cylinder {
                radius: 0.5,
                length: 2.0
            }
        );
    }

    #[test]
    fn parses_a_sphere() {
        let geometry = parse(r#"<geometry><sphere radius="0.5"/></geometry>"#).unwrap();
        assert_eq!(geometry, Geometry::Sphere { radius: 0.5 });
    }

    #[test]
    fn parses_a_mesh_with_default_scale() {
        let geometry = parse(r#"<geometry><mesh filename="arm.stl"/></geometry>"#).unwrap();
        assert_eq!(
            geometry,
            Geometry::Mesh {
                filename: "arm.stl".to_owned(),
                scale: Vec3::new(1.0, 1.0, 1.0),
            }
        );
    }

    #[test]
    fn parses_a_mesh_with_an_explicit_scale() {
        let geometry =
            parse(r#"<geometry><mesh filename="arm.stl" scale="2 2 2"/></geometry>"#).unwrap();
        assert_eq!(
            geometry,
            Geometry::Mesh {
                filename: "arm.stl".to_owned(),
                scale: Vec3::new(2.0, 2.0, 2.0),
            }
        );
    }

    #[test]
    fn an_empty_geometry_element_is_rejected() {
        let error = parse("<geometry></geometry>").unwrap_err();
        assert!(matches!(error, UrdfError::MissingChildElement { .. }));
    }

    #[test]
    fn an_unrecognized_shape_element_is_rejected() {
        let error = parse(r#"<geometry><cone radius="1"/></geometry>"#).unwrap_err();
        assert!(matches!(error, UrdfError::UnexpectedElement { .. }));
    }

    #[test]
    fn a_box_missing_its_size_attribute_is_rejected() {
        let error = parse("<geometry><box/></geometry>").unwrap_err();
        assert!(matches!(
            error,
            UrdfError::MissingAttribute {
                element: "box",
                attribute: "size",
                ..
            }
        ));
    }

    #[test]
    fn the_reader_lands_on_the_next_sibling_after_a_geometry_element() {
        // Wrapped in `<root>` since a bare document can only have one
        // top-level element — see `crate::xml`'s `ContentAfterRoot`.
        let mut reader =
            Reader::new("<root><geometry><sphere radius=\"1\"/></geometry><next/></root>");
        reader.next_event().unwrap(); // <root>
        let span = match reader.next_event().unwrap() {
            Event::StartElement { span, .. } => span,
            other => panic!("expected StartElement, got {other:?}"),
        };
        parse_geometry(&mut reader, span).unwrap();
        assert!(matches!(
            reader.next_event().unwrap(),
            Event::StartElement { name: "next", .. }
        ));
    }
}
