//! `<origin xyz="x y z" rpy="r p y"/>` — the one element shape shared
//! verbatim by `<joint>`, `<inertial>`, `<visual>` and `<collision>`.

use crate::math::{Quat, Transform, Vec3};
use crate::xml::Attribute;

use super::attrs::attr_vec3_opt;

/// Parses an `<origin>` element's attributes into a [`Transform`] — both
/// `xyz` and `rpy` default to zero (identity) when absent, matching URDF's
/// own documented default.
///
/// `element` names the *enclosing* element (`"joint"`, `"inertial"`, ...)
/// for [`crate::UrdfError::InvalidAttributeValue`]'s message, since
/// `<origin>` itself carries no distinguishing content beyond these two
/// attributes.
///
/// # Errors
///
/// [`crate::UrdfError::InvalidAttributeValue`] if `xyz` or `rpy` is present
/// but malformed.
pub(super) fn parse_origin(
    element: &'static str,
    attributes: &[Attribute<'_>],
) -> crate::Result<Transform> {
    let translation = attr_vec3_opt(element, "xyz", attributes, Vec3::ZERO)?;
    let rpy = attr_vec3_opt(element, "rpy", attributes, Vec3::ZERO)?;
    let rotation = Quat::from_euler_rpy(rpy.x, rpy.y, rpy.z);
    Ok(Transform::new(translation, rotation))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::xml::{Event, Reader};

    fn attrs_of(xml: &str) -> Vec<Attribute<'_>> {
        let mut reader = Reader::new(xml);
        match reader.next_event().unwrap() {
            Event::StartElement { attributes, .. } => attributes,
            other => panic!("expected StartElement, got {other:?}"),
        }
    }

    #[test]
    fn an_absent_origin_is_identity() {
        let attrs = attrs_of("<origin/>");
        assert_eq!(parse_origin("joint", &attrs).unwrap(), Transform::IDENTITY);
    }

    #[test]
    fn xyz_alone_is_a_pure_translation() {
        let attrs = attrs_of(r#"<origin xyz="1 2 3"/>"#);
        let transform = parse_origin("joint", &attrs).unwrap();
        assert_eq!(transform.translation, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(transform.rotation, Quat::IDENTITY);
    }

    #[test]
    fn rpy_alone_is_a_pure_rotation() {
        let attrs = attrs_of(r#"<origin rpy="0 0 1.5707963267948966"/>"#);
        let transform = parse_origin("joint", &attrs).unwrap();
        assert_eq!(transform.translation, Vec3::ZERO);
        let rotated = transform.rotation.rotate_vector(Vec3::UNIT_X);
        assert!((rotated - Vec3::UNIT_Y).norm() < 1e-9);
    }

    #[test]
    fn a_malformed_xyz_is_rejected() {
        let attrs = attrs_of(r#"<origin xyz="1 2"/>"#);
        assert!(parse_origin("joint", &attrs).is_err());
    }
}
