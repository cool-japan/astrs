//! [`parse_str`] and `<robot>` dispatch: the outermost loop that finds the
//! document's single `<robot>` root and routes each of its children to
//! `<link>`, `<joint>`, or `<material>` parsing.

use crate::UrdfError;
use crate::model::Robot;
use crate::xml::{Event, Reader};

use super::cursor::consume_element_body;
use super::joint::parse_joint;
use super::link::parse_link;
use super::material::parse_material;

/// Parses `input` as a URDF document into a [`Robot`].
///
/// Does **not** run [`Robot::validate`] — see this module's docs for why
/// that stays a caller-invoked step.
///
/// # Errors
///
/// [`UrdfError::Xml`] if `input` is not well-formed XML.
/// [`UrdfError::UnexpectedElement`] if the document's root element is not
/// `<robot>`. [`UrdfError::MissingAttribute`]/
/// [`UrdfError::InvalidAttributeValue`]/[`UrdfError::MissingChildElement`]/
/// [`UrdfError::UnknownJointType`] for any element or attribute this crate
/// models that is itself malformed.
pub fn parse_str(input: &str) -> crate::Result<Robot> {
    let mut reader = Reader::new(input);
    loop {
        match reader.next_event()? {
            Event::StartElement {
                name: "robot",
                attributes,
                span,
                self_closing,
            } => {
                return parse_robot_body(&mut reader, &attributes, span, self_closing);
            }
            Event::StartElement {
                name,
                span,
                self_closing,
                ..
            } => {
                consume_element_body(&mut reader, self_closing)?;
                return Err(UrdfError::UnexpectedElement {
                    expected: "robot",
                    found: name.to_owned(),
                    span,
                });
            }
            Event::Eof { span } => {
                return Err(UrdfError::Xml(crate::xml::XmlError::new(
                    crate::xml::XmlErrorKind::MissingRootElement,
                    span,
                )));
            }
            // `Reader` itself never emits an `EndElement` before any
            // `StartElement` has been seen (a stray closing tag is instead
            // `XmlErrorKind::UnexpectedClosingTag`, surfaced through the
            // `?` above) — this loop only runs before the very first
            // `StartElement`, so this arm is structurally unreachable. Kept
            // as a typed error rather than `unreachable!()` for the same
            // reason `super::cursor::consume_element_body` is — see that
            // function's own comment.
            Event::EndElement { span, .. } => {
                return Err(UrdfError::Xml(crate::xml::XmlError::new(
                    crate::xml::XmlErrorKind::UnexpectedClosingTag {
                        found: String::new(),
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

fn parse_robot_body(
    reader: &mut Reader<'_>,
    attributes: &[crate::xml::Attribute<'_>],
    span: crate::xml::Span,
    self_closing: bool,
) -> crate::Result<Robot> {
    let name = super::attrs::attr_str("robot", "name", attributes, span)?.to_owned();
    if self_closing {
        // Drains the reader's pending synthesized `EndElement` before
        // returning, for the same reason `parse::link::parse_link` does —
        // even though `parse_str` (this function's only caller) does not
        // read the reader again afterward today, keeping this in lockstep
        // with every other `parse_*` function's documented postcondition
        // (see `super::cursor`'s module docs) means a future caller never
        // has to special-case this one.
        consume_element_body(reader, self_closing)?;
        return Ok(Robot::named(name));
    }
    let mut robot = Robot::named(name);

    loop {
        match reader.next_event()? {
            Event::StartElement {
                name: "link",
                attributes,
                span,
                self_closing,
            } => {
                robot
                    .links
                    .push(parse_link(reader, &attributes, span, self_closing)?);
            }
            Event::StartElement {
                name: "joint",
                attributes,
                span,
                self_closing,
            } => {
                robot
                    .joints
                    .push(parse_joint(reader, &attributes, span, self_closing)?);
            }
            Event::StartElement {
                name: "material",
                attributes,
                span,
                self_closing,
            } => {
                robot
                    .materials
                    .push(parse_material(reader, &attributes, span, self_closing)?);
            }
            Event::StartElement { self_closing, .. } => {
                // A `<robot>`-level extension this crate does not model
                // (e.g. a bare `<gazebo>` reference, common in real-world
                // URDFs) — skipped, not rejected; see `super::cursor`'s
                // module docs.
                consume_element_body(reader, self_closing)?;
            }
            Event::EndElement { .. } => break,
            Event::Eof { span } => {
                return Err(UrdfError::Xml(crate::xml::XmlError::new(
                    crate::xml::XmlErrorKind::UnclosedElement {
                        name: "robot".to_owned(),
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

    Ok(robot)
}
