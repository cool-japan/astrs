//! [`Link`] and its three child elements: [`Inertial`], [`Visual`],
//! [`Collision`].

use crate::math::Transform;
use crate::model::geometry::Geometry;
use crate::model::material::Material;

/// The six independent components of a symmetric 3x3 inertia tensor,
/// exactly as `<inertia ixx=".." ixy=".." ixz=".." iyy=".." iyz=".."
/// izz=".."/>` names them.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct InertiaTensor {
    /// `ixx`.
    pub ixx: f64,
    /// `ixy`.
    pub ixy: f64,
    /// `ixz`.
    pub ixz: f64,
    /// `iyy`.
    pub iyy: f64,
    /// `iyz`.
    pub iyz: f64,
    /// `izz`.
    pub izz: f64,
}

/// A `<link><inertial>` element: mass distribution, used by a dynamics
/// solver (not this crate) rather than by forward kinematics itself — FK
/// only needs the joint tree's *shape*, not any link's mass. Carried here
/// unchanged since a URDF consumer that *does* need dynamics (e.g.
/// `astrs-sim`, blueprint §5.3) should not have to re-parse the document.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Inertial {
    /// This link's inertial frame, relative to the link's own origin —
    /// `<inertial><origin .../>`, identity if omitted.
    pub origin: Transform,
    /// The link's mass, in kilograms — `<mass value="..."/>`.
    pub mass: f64,
    /// The link's inertia tensor about `origin`'s frame.
    pub inertia: InertiaTensor,
}

/// A `<link><visual>` element: one piece of this link's rendered geometry.
///
/// URDF allows a link to declare any number of `<visual>` elements (each
/// contributing separately to the rendered model), so [`Link::visuals`] is
/// a `Vec` rather than an `Option<Visual>`.
#[derive(Debug, Clone, PartialEq)]
pub struct Visual {
    /// This element's own `name` attribute, if given — URDF makes it
    /// optional and does not require it to be unique even when present.
    pub name: Option<String>,
    /// This visual's frame, relative to the link's own origin —
    /// `<visual><origin .../>`, identity if omitted.
    pub origin: Transform,
    /// The shape.
    pub geometry: Geometry,
    /// The shape's appearance, if given. A bare `<material name="foo"/>`
    /// reference to a robot-level material is already resolved to that
    /// material's full definition by [`crate::parse`] — see
    /// [`Material`]'s own docs.
    pub material: Option<Material>,
}

/// A `<link><collision>` element: one piece of this link's collision
/// geometry — deliberately the same shape as [`Visual`] minus `material`
/// (collision geometry has no appearance), since URDF gives the two
/// elements identical `origin`/`geometry` structure.
#[derive(Debug, Clone, PartialEq)]
pub struct Collision {
    /// This element's own `name` attribute, if given.
    pub name: Option<String>,
    /// This collision shape's frame, relative to the link's own origin.
    pub origin: Transform,
    /// The shape.
    pub geometry: Geometry,
}

/// A `<link>` element: one rigid body in the kinematic tree.
///
/// A link's *position* in the tree is not stored on the link itself — it
/// is implied by which [`super::Joint`]s name it as `parent`/`child` (see
/// [`super::Robot::validate`]). [`Link`] only carries what URDF actually
/// nests inside `<link>`: its name and its (optional) inertial/visual/
/// collision content.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    /// This link's name — `<link name="...">`. Unique within a
    /// [`super::Robot`] (checked by [`super::Robot::validate`]), and the
    /// identifier every [`super::Joint::parent`]/[`super::Joint::child`]
    /// and every [`crate::kinematics`] query names it by.
    pub name: String,
    /// Mass properties, if given.
    pub inertial: Option<Inertial>,
    /// Every `<visual>` child, in document order.
    pub visuals: Vec<Visual>,
    /// Every `<collision>` child, in document order.
    pub collisions: Vec<Collision>,
}

impl Link {
    /// Builds a link with `name` and nothing else — the shape a bare
    /// `<link name="..."/>` (legal URDF: a purely structural link, common
    /// for e.g. a sensor mount frame) parses to.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            inertial: None,
            visuals: Vec::new(),
            collisions: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn named_has_no_inertial_and_no_geometry() {
        let link = Link::named("base_link");
        assert_eq!(link.name, "base_link");
        assert_eq!(link.inertial, None);
        assert!(link.visuals.is_empty());
        assert!(link.collisions.is_empty());
    }

    #[test]
    fn inertia_tensor_default_is_all_zero() {
        let tensor = InertiaTensor::default();
        assert_eq!(tensor.ixx, 0.0);
        assert_eq!(tensor.izz, 0.0);
    }
}
