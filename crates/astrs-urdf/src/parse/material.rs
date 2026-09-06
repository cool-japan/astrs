//! `<material>` parsing, used both for `<robot><material>` (declares a
//! reusable, named material) and `<visual><material>` (either the same
//! full declaration inline, or a bare `<material name="foo"/>` reference —
//! see [`crate::model::Material`]'s own docs on why this crate does not
//! try to resolve that reference during this single streaming pass).

use crate::UrdfError;
use crate::model::{Color, Material};
use crate::xml::{Event, Reader};

use super::attrs::{attr_str, find_attr, parse_rgba};
use super::cursor::consume_element_body;

/// Parses a `<material>` element's attributes and body. Called with the
/// reader positioned just after `<material>`'s own `StartElement`
/// (`attributes`/`span`/`self_closing` already in hand from that event, the
/// same shape [`super::cursor`]'s module docs describe for a leaf-shaped
/// consumer that still needs to look at a nested child); returns having
/// consumed through `<material>`'s own `EndElement`.
///
/// # Errors
///
/// [`UrdfError::MissingAttribute`] if `name` is absent;
/// [`UrdfError::InvalidAttributeValue`] if a nested `<color rgba="..">` is
/// malformed.
pub(super) fn parse_material(
    reader: &mut Reader<'_>,
    attributes: &[crate::xml::Attribute<'_>],
    span: crate::xml::Span,
    self_closing: bool,
) -> crate::Result<Material> {
    let name = attr_str("material", "name", attributes, span)?.to_owned();
    if self_closing {
        // See `parse::link::parse_link`'s identical comment: draining the
        // reader's pending synthesized `EndElement` here is required, not
        // optional — otherwise the caller's own loop reads it next and
        // misreads it as its own closing tag.
        consume_element_body(reader, self_closing)?;
        return Ok(Material::named(name));
    }

    let mut color = None;
    let mut texture_filename = None;
    loop {
        match reader.next_event()? {
            Event::StartElement {
                name: "color",
                attributes,
                span,
                self_closing,
            } => {
                let rgba_attr =
                    find_attr(&attributes, "rgba").ok_or(UrdfError::MissingAttribute {
                        element: "color",
                        attribute: "rgba",
                        span,
                    })?;
                let (r, g, b, a) = parse_rgba("color", "rgba", rgba_attr)?;
                color = Some(Color::new(r, g, b, a));
                consume_element_body(reader, self_closing)?;
            }
            Event::StartElement {
                name: "texture",
                attributes,
                span,
                self_closing,
            } => {
                let filename_attr = attr_str("texture", "filename", &attributes, span)?;
                texture_filename = Some(filename_attr.to_owned());
                consume_element_body(reader, self_closing)?;
            }
            Event::StartElement { self_closing, .. } => {
                consume_element_body(reader, self_closing)?;
            }
            Event::EndElement { .. } => break,
            Event::Eof { span } => {
                return Err(UrdfError::Xml(crate::xml::XmlError::new(
                    crate::xml::XmlErrorKind::UnclosedElement {
                        name: "material".to_owned(),
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

    Ok(Material {
        name,
        color,
        texture_filename,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn parse(xml: &str) -> crate::Result<Material> {
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
        parse_material(&mut reader, &attributes, span, self_closing)
    }

    #[test]
    fn a_bare_self_closing_reference_has_no_color_or_texture() {
        let material = parse(r#"<material name="foo"/>"#).unwrap();
        assert_eq!(material, Material::named("foo"));
    }

    #[test]
    fn a_bare_empty_element_reference_has_no_color_or_texture() {
        let material = parse(r#"<material name="foo"></material>"#).unwrap();
        assert_eq!(material, Material::named("foo"));
    }

    #[test]
    fn a_declaration_with_a_color_parses_the_rgba_quadruple() {
        let material =
            parse(r#"<material name="blue"><color rgba="0 0 1 1"/></material>"#).unwrap();
        assert_eq!(material.name, "blue");
        assert_eq!(material.color, Some(Color::new(0.0, 0.0, 1.0, 1.0)));
        assert_eq!(material.texture_filename, None);
    }

    #[test]
    fn a_declaration_with_a_texture_parses_the_filename() {
        let material =
            parse(r#"<material name="wood"><texture filename="wood.png"/></material>"#).unwrap();
        assert_eq!(material.texture_filename, Some("wood.png".to_owned()));
        assert_eq!(material.color, None);
    }

    #[test]
    fn a_missing_name_attribute_is_rejected() {
        let error = parse("<material/>").unwrap_err();
        assert!(matches!(
            error,
            UrdfError::MissingAttribute {
                element: "material",
                attribute: "name",
                ..
            }
        ));
    }

    #[test]
    fn the_reader_lands_on_the_next_sibling_after_a_material_element() {
        // Wrapped in `<root>` since a bare document can only have one
        // top-level element — see `crate::xml`'s `ContentAfterRoot`.
        let mut reader = Reader::new(r#"<root><material name="foo"/><next/></root>"#);
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
        parse_material(&mut reader, &attributes, span, self_closing).unwrap();
        assert!(matches!(
            reader.next_event().unwrap(),
            Event::StartElement { name: "next", .. }
        ));
    }
}
