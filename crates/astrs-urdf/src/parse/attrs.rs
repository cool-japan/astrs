//! Attribute-level parsing helpers shared by every element parser in this
//! module: required/optional string and float lookups, and the
//! space-separated float lists URDF uses for `xyz="x y z"`, `rpy="r p y"`
//! and `rgba="r g b a"`.
//!
//! Every failure here is an [`UrdfError::InvalidAttributeValue`] or
//! [`UrdfError::MissingAttribute`] carrying the *attribute's own*
//! [`Span`] (not the enclosing element's) wherever the caller has one in
//! hand — the reason this crate carries its own XML reader rather than a
//! byte-offset-only one is exactly so a bad `mass="NaN"` points at `NaN`,
//! not at the start of the `<mass>` tag it lives on.

use crate::UrdfError;
use crate::math::Vec3;
use crate::xml::{Attribute, Span};

/// Finds the attribute named `name` among `attrs`, in document order (the
/// first match — [`crate::xml::Reader`] already rejects a genuine
/// duplicate at the XML layer, so "first" vs. "any" never actually
/// matters).
pub(super) fn find_attr<'b, 'a>(
    attrs: &'b [Attribute<'a>],
    name: &str,
) -> Option<&'b Attribute<'a>> {
    attrs.iter().find(|a| a.name == name)
}

/// A required string attribute.
///
/// Every `attr_*` helper in this module takes its parameters in the same
/// order — `(element, attribute, attrs, span)` — deliberately, since
/// `element` and `attribute` are both `&'static str` and therefore
/// impossible for the compiler to catch a transposed call of; a fixed,
/// uniform order across every helper is what actually prevents that
/// mistake, not the type system.
///
/// # Errors
///
/// [`UrdfError::MissingAttribute`] if `attrs` has no attribute named
/// `attribute`.
pub(super) fn attr_str<'b>(
    element: &'static str,
    attribute: &'static str,
    attrs: &'b [Attribute<'_>],
    element_span: Span,
) -> crate::Result<&'b str> {
    find_attr(attrs, attribute)
        .map(|a| a.value.as_ref())
        .ok_or(UrdfError::MissingAttribute {
            element,
            attribute,
            span: element_span,
        })
}

/// Parses one already-located token as a finite `f64` — the shared
/// non-finite guard every float-bearing attribute in this crate goes
/// through (see this module's docs on why non-finite values are rejected
/// here, at the attribute, rather than downstream).
fn parse_finite_f64(
    element: &'static str,
    attribute: &'static str,
    token: &str,
    span: Span,
) -> crate::Result<f64> {
    let invalid = || UrdfError::InvalidAttributeValue {
        element,
        attribute,
        value: token.to_owned(),
        span,
    };
    let value: f64 = token.parse().map_err(|_| invalid())?;
    if !value.is_finite() {
        return Err(invalid());
    }
    Ok(value)
}

/// A required float attribute — one bare number, no whitespace-separated
/// list.
///
/// # Errors
///
/// [`UrdfError::MissingAttribute`] if absent;
/// [`UrdfError::InvalidAttributeValue`] if present but not a finite
/// number.
pub(super) fn attr_f64(
    element: &'static str,
    attribute: &'static str,
    attrs: &[Attribute<'_>],
    element_span: Span,
) -> crate::Result<f64> {
    let attr = find_attr(attrs, attribute).ok_or(UrdfError::MissingAttribute {
        element,
        attribute,
        span: element_span,
    })?;
    parse_finite_f64(element, attribute, &attr.value, attr.value_span)
}

/// An optional float attribute, defaulting to `default` when absent.
///
/// # Errors
///
/// [`UrdfError::InvalidAttributeValue`] if present but not a finite
/// number.
pub(super) fn attr_f64_opt(
    element: &'static str,
    attribute: &'static str,
    attrs: &[Attribute<'_>],
    default: f64,
) -> crate::Result<f64> {
    match find_attr(attrs, attribute) {
        Some(attr) => parse_finite_f64(element, attribute, &attr.value, attr.value_span),
        None => Ok(default),
    }
}

/// Splits `attr`'s value on whitespace and parses every token as a finite
/// `f64` — the shared step [`parse_vec3`] and [`parse_rgba`] both build
/// on.
///
/// # Errors
///
/// [`UrdfError::InvalidAttributeValue`] (located at `attr`'s own value
/// span) at the first token that is not a finite number.
fn split_floats(
    element: &'static str,
    attribute: &'static str,
    attr: &Attribute<'_>,
) -> crate::Result<Vec<f64>> {
    attr.value
        .split_whitespace()
        .map(|token| parse_finite_f64(element, attribute, token, attr.value_span))
        .collect()
}

/// Parses `attr`'s value as URDF's `"x y z"` triple — used for `xyz`,
/// `rpy`, and `<box size="x y z"/>`.
///
/// # Errors
///
/// [`UrdfError::InvalidAttributeValue`] if any token fails to parse as a
/// finite number, or the value does not have exactly three tokens.
pub(super) fn parse_vec3(
    element: &'static str,
    attribute: &'static str,
    attr: &Attribute<'_>,
) -> crate::Result<Vec3> {
    let floats = split_floats(element, attribute, attr)?;
    match floats.as_slice() {
        [x, y, z] => Ok(Vec3::new(*x, *y, *z)),
        _ => Err(UrdfError::InvalidAttributeValue {
            element,
            attribute,
            value: attr.value.to_string(),
            span: attr.value_span,
        }),
    }
}

/// An optional `"x y z"` triple attribute, defaulting to `default` when
/// absent — `<origin xyz=".."/>` (default [`Vec3::ZERO`]) and `<axis
/// xyz=".."/>` (default [`Vec3::UNIT_X`]) both go through this.
///
/// # Errors
///
/// [`UrdfError::InvalidAttributeValue`] if present but malformed (see
/// [`parse_vec3`]).
pub(super) fn attr_vec3_opt(
    element: &'static str,
    attribute: &'static str,
    attrs: &[Attribute<'_>],
    default: Vec3,
) -> crate::Result<Vec3> {
    match find_attr(attrs, attribute) {
        Some(attr) => parse_vec3(element, attribute, attr),
        None => Ok(default),
    }
}

/// Parses `attr`'s value as URDF's `"r g b a"` quadruple —
/// `<color rgba="r g b a"/>`.
///
/// # Errors
///
/// [`UrdfError::InvalidAttributeValue`] if any token fails to parse as a
/// finite number, or the value does not have exactly four tokens.
pub(super) fn parse_rgba(
    element: &'static str,
    attribute: &'static str,
    attr: &Attribute<'_>,
) -> crate::Result<(f64, f64, f64, f64)> {
    let floats = split_floats(element, attribute, attr)?;
    match floats.as_slice() {
        [r, g, b, a] => Ok((*r, *g, *b, *a)),
        _ => Err(UrdfError::InvalidAttributeValue {
            element,
            attribute,
            value: attr.value.to_string(),
            span: attr.value_span,
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::xml::{Event, Reader};

    /// Parses `xml` (expected to be a single self-closing or empty element)
    /// and returns its attributes plus the start tag's own span — a test
    /// convenience so every test below can build real `Attribute`s (with
    /// real spans) rather than hand-constructing them.
    fn attrs_of(xml: &str) -> (Vec<Attribute<'_>>, Span) {
        let mut reader = Reader::new(xml);
        match reader.next_event().expect("well-formed test fixture") {
            Event::StartElement {
                attributes, span, ..
            } => (attributes, span),
            other => panic!("expected StartElement, got {other:?}"),
        }
    }

    #[test]
    fn attr_str_finds_a_present_attribute() {
        let (attrs, span) = attrs_of(r#"<a x="hello"/>"#);
        assert_eq!(attr_str("a", "x", &attrs, span).unwrap(), "hello");
    }

    #[test]
    fn attr_str_reports_a_missing_attribute() {
        let (attrs, span) = attrs_of("<a/>");
        assert_eq!(
            attr_str("a", "x", &attrs, span),
            Err(UrdfError::MissingAttribute {
                element: "a",
                attribute: "x",
                span,
            })
        );
    }

    #[test]
    fn attr_f64_parses_a_bare_number() {
        let (attrs, span) = attrs_of(r#"<a x="1.5"/>"#);
        assert_eq!(attr_f64("a", "x", &attrs, span).unwrap(), 1.5);
    }

    #[test]
    fn attr_f64_rejects_nan_and_infinity() {
        let (attrs, span) = attrs_of(r#"<a x="NaN"/>"#);
        assert!(matches!(
            attr_f64("a", "x", &attrs, span),
            Err(UrdfError::InvalidAttributeValue { .. })
        ));
        let (attrs, span) = attrs_of(r#"<a x="inf"/>"#);
        assert!(matches!(
            attr_f64("a", "x", &attrs, span),
            Err(UrdfError::InvalidAttributeValue { .. })
        ));
    }

    #[test]
    fn attr_f64_opt_falls_back_to_the_default_when_absent() {
        let (attrs, _) = attrs_of("<a/>");
        assert_eq!(attr_f64_opt("a", "x", &attrs, 7.0).unwrap(), 7.0);
    }

    #[test]
    fn parse_vec3_parses_three_whitespace_separated_numbers() {
        let (attrs, _) = attrs_of(r#"<a xyz="1 2.5 -3"/>"#);
        let attr = find_attr(&attrs, "xyz").unwrap();
        assert_eq!(
            parse_vec3("a", "xyz", attr).unwrap(),
            Vec3::new(1.0, 2.5, -3.0)
        );
    }

    #[test]
    fn parse_vec3_rejects_the_wrong_number_of_tokens() {
        let (attrs, _) = attrs_of(r#"<a xyz="1 2"/>"#);
        let attr = find_attr(&attrs, "xyz").unwrap();
        assert!(matches!(
            parse_vec3("a", "xyz", attr),
            Err(UrdfError::InvalidAttributeValue { .. })
        ));
    }

    #[test]
    fn parse_rgba_parses_four_whitespace_separated_numbers() {
        let (attrs, _) = attrs_of(r#"<a rgba="1 0 0 1"/>"#);
        let attr = find_attr(&attrs, "rgba").unwrap();
        assert_eq!(parse_rgba("a", "rgba", attr).unwrap(), (1.0, 0.0, 0.0, 1.0));
    }

    #[test]
    fn attr_vec3_opt_falls_back_to_the_default_when_absent() {
        let (attrs, _) = attrs_of("<a/>");
        assert_eq!(
            attr_vec3_opt("a", "xyz", &attrs, Vec3::UNIT_X).unwrap(),
            Vec3::UNIT_X
        );
    }

    #[test]
    fn attr_vec3_opt_parses_a_present_value() {
        let (attrs, _) = attrs_of(r#"<a xyz="0 0 1"/>"#);
        assert_eq!(
            attr_vec3_opt("a", "xyz", &attrs, Vec3::UNIT_X).unwrap(),
            Vec3::UNIT_Z
        );
    }
}
