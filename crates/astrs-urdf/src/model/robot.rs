//! [`Robot`] — the top-level `<robot>` element: every link, joint and
//! material, plus the tree-shape validation blueprint §5.3 asks for.

use crate::model::joint::Joint;
use crate::model::link::Link;
use crate::model::material::Material;

/// A fully-parsed URDF document: `<robot name="...">` and everything it
/// declares.
///
/// A freshly-[`crate::parse::parse_str`]d [`Robot`] is *not* guaranteed to
/// be a valid kinematic tree yet — parsing only checks that the XML is
/// well-formed URDF *syntax* (every element and attribute means something);
/// whether the `parent`/`child` names it references actually exist, and
/// whether they form a rooted tree rather than a forest or a cycle, is
/// [`Robot::validate`]'s job. [`crate::kinematics::Topology::build`]
/// requires a [`Robot`] to have already passed [`Robot::validate`] (and
/// re-derives the same facts, rather than trusting an unchecked caller —
/// see that function's docs).
#[derive(Debug, Clone, PartialEq)]
pub struct Robot {
    /// The robot's name — `<robot name="...">`.
    pub name: String,
    /// Every `<link>`, in document order.
    pub links: Vec<Link>,
    /// Every `<joint>`, in document order.
    pub joints: Vec<Joint>,
    /// Every robot-level `<material>` — declared directly under `<robot>`,
    /// as opposed to inline inside a `<visual>` (see [`Material`]'s docs).
    /// [`crate::parse`] uses this list to resolve a bare `<material
    /// name="foo"/>` reference inside a `<visual>` at parse time, but it
    /// stays populated here too, unchanged, so a caller can enumerate every
    /// material the document declares independent of which links use them.
    pub materials: Vec<Material>,
}

impl Robot {
    /// Builds an empty robot named `name` — no links, joints or materials.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            links: Vec::new(),
            joints: Vec::new(),
            materials: Vec::new(),
        }
    }

    /// The link named `name`, if declared.
    #[must_use]
    pub fn link(&self, name: &str) -> Option<&Link> {
        self.links.iter().find(|link| link.name == name)
    }

    /// The joint named `name`, if declared.
    #[must_use]
    pub fn joint(&self, name: &str) -> Option<&Joint> {
        self.joints.iter().find(|joint| joint.name == name)
    }

    /// The robot-level material named `name`, if declared.
    #[must_use]
    pub fn material(&self, name: &str) -> Option<&Material> {
        self.materials.iter().find(|material| material.name == name)
    }

    /// Resolves a [`super::Visual::material`] to the [`Material`] it
    /// actually means: `visual_material` itself when it already carries a
    /// color or a texture (an inline `<material>` needs no lookup), or the
    /// robot-level material with the same name otherwise (a bare
    /// `<material name="foo"/>` reference — see [`Material`]'s own docs).
    ///
    /// Returns `None` only for a bare reference to a name no robot-level
    /// `<material>` declares — [`Robot::validate`] checks that this never
    /// happens for any visual actually reachable in the document, so a
    /// caller working with an already-validated [`Robot`] can treat this as
    /// infallible in practice.
    #[must_use]
    pub fn resolve_material<'a>(&'a self, visual_material: &'a Material) -> Option<&'a Material> {
        if visual_material.color.is_some() || visual_material.texture_filename.is_some() {
            return Some(visual_material);
        }
        self.material(&visual_material.name)
    }

    /// The joint whose `child` is `link_name`, if any — `link_name`'s
    /// unique parent joint in a [`Robot::validate`]d tree.
    #[must_use]
    pub fn joint_with_child(&self, link_name: &str) -> Option<&Joint> {
        self.joints.iter().find(|joint| joint.child == link_name)
    }

    /// Every joint whose `parent` is `link_name`, in document order —
    /// `link_name`'s children in a [`Robot::validate`]d tree.
    pub fn joints_with_parent<'a>(&'a self, link_name: &'a str) -> impl Iterator<Item = &'a Joint> {
        self.joints
            .iter()
            .filter(move |joint| joint.parent == link_name)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::JointKind;

    fn sample() -> Robot {
        let mut robot = Robot::named("demo");
        robot.links.push(Link::named("base_link"));
        robot.links.push(Link::named("arm_link"));
        robot.joints.push(Joint::new(
            "base_to_arm",
            JointKind::Revolute,
            "base_link",
            "arm_link",
        ));
        robot
    }

    #[test]
    fn named_is_empty() {
        let robot = Robot::named("demo");
        assert_eq!(robot.name, "demo");
        assert!(robot.links.is_empty());
        assert!(robot.joints.is_empty());
        assert!(robot.materials.is_empty());
    }

    #[test]
    fn link_and_joint_lookup_finds_declared_names_and_nothing_else() {
        let robot = sample();
        assert!(robot.link("base_link").is_some());
        assert!(robot.link("nonexistent").is_none());
        assert!(robot.joint("base_to_arm").is_some());
        assert!(robot.joint("nonexistent").is_none());
    }

    #[test]
    fn joint_with_child_finds_the_owning_joint() {
        let robot = sample();
        assert_eq!(
            robot.joint_with_child("arm_link").map(|j| j.name.as_str()),
            Some("base_to_arm")
        );
        assert!(robot.joint_with_child("base_link").is_none());
    }

    #[test]
    fn joints_with_parent_finds_every_child_joint_in_document_order() {
        let mut robot = sample();
        robot.links.push(Link::named("sensor_link"));
        robot.joints.push(Joint::new(
            "base_to_sensor",
            JointKind::Fixed,
            "base_link",
            "sensor_link",
        ));
        let names: Vec<&str> = robot
            .joints_with_parent("base_link")
            .map(|j| j.name.as_str())
            .collect();
        assert_eq!(names, ["base_to_arm", "base_to_sensor"]);
    }

    #[test]
    fn resolve_material_returns_an_inline_material_directly() {
        let robot = Robot::named("demo");
        let inline = Material {
            name: "unused_name".to_owned(),
            color: Some(crate::model::Color::WHITE),
            texture_filename: None,
        };
        let resolved = robot.resolve_material(&inline).unwrap();
        assert_eq!(resolved.color, Some(crate::model::Color::WHITE));
    }

    #[test]
    fn resolve_material_looks_up_a_bare_reference_by_name() {
        let mut robot = Robot::named("demo");
        robot.materials.push(Material {
            name: "blue".to_owned(),
            color: Some(crate::model::Color::new(0.0, 0.0, 1.0, 1.0)),
            texture_filename: None,
        });
        let reference = Material::named("blue");
        let resolved = robot.resolve_material(&reference).unwrap();
        assert_eq!(
            resolved.color,
            Some(crate::model::Color::new(0.0, 0.0, 1.0, 1.0))
        );
    }

    #[test]
    fn resolve_material_fails_for_an_undeclared_bare_reference() {
        let robot = Robot::named("demo");
        let reference = Material::named("nonexistent");
        assert!(robot.resolve_material(&reference).is_none());
    }
}
