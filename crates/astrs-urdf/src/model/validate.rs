//! [`Robot::validate`] — blueprint §5.3's tree-shape checks: unique names,
//! every `parent`/`child` link exists, and the joint graph is a rooted
//! tree — plus mimic-relationship validation (target exists, both ends are
//! single-DOF, no mimic cycle), a natural extension of the same
//! cross-reference checking once [`super::joint::JointMimic`] exists as a
//! modeled field at all.

use std::collections::{HashMap, HashSet};

use crate::UrdfError;
use crate::model::joint::Joint;

use super::robot::Robot;

impl Robot {
    /// Validates this robot's cross-references and tree shape.
    ///
    /// Checks run in a fixed order, each assuming every earlier one already
    /// passed (so, for instance, the tree-shape check below can freely
    /// assume every joint's `parent`/`child` names a real link):
    ///
    /// 1. No two links share a name; no two joints share a name.
    /// 2. Every joint's `parent` and `child` name a declared link.
    /// 3. No two joints claim the same link as `child`.
    /// 4. Exactly one link has no parent joint (the root); every other
    ///    link is reachable from it.
    /// 5. Every `<mimic joint="...">` names a declared, single-DOF joint,
    ///    the mimicking joint is itself single-DOF, and no chain of mimic
    ///    relationships cycles back on itself.
    /// 6. Every bare `<material name="foo"/>` reference (no inline color
    ///    or texture) inside a `<visual>` names a declared robot-level
    ///    material (see [`Robot::resolve_material`]).
    ///
    /// # Errors
    ///
    /// The first check above that fails — see [`UrdfError`]'s "Tree
    /// validation" group for the specific variants.
    pub fn validate(&self) -> crate::Result<()> {
        self.validate_unique_names()?;
        self.validate_references()?;
        self.validate_tree_shape()?;
        self.validate_mimic()?;
        self.validate_material_references()?;
        Ok(())
    }

    fn validate_unique_names(&self) -> crate::Result<()> {
        let mut seen_links: HashSet<&str> = HashSet::new();
        for link in &self.links {
            if !seen_links.insert(link.name.as_str()) {
                return Err(UrdfError::DuplicateLinkName {
                    name: link.name.clone(),
                });
            }
        }
        let mut seen_joints: HashSet<&str> = HashSet::new();
        for joint in &self.joints {
            if !seen_joints.insert(joint.name.as_str()) {
                return Err(UrdfError::DuplicateJointName {
                    name: joint.name.clone(),
                });
            }
        }
        Ok(())
    }

    fn validate_references(&self) -> crate::Result<()> {
        let link_names: HashSet<&str> = self.links.iter().map(|l| l.name.as_str()).collect();
        for joint in &self.joints {
            if !link_names.contains(joint.parent.as_str()) {
                return Err(UrdfError::UnknownParentLink {
                    joint: joint.name.clone(),
                    link: joint.parent.clone(),
                });
            }
            if !link_names.contains(joint.child.as_str()) {
                return Err(UrdfError::UnknownChildLink {
                    joint: joint.name.clone(),
                    link: joint.child.clone(),
                });
            }
        }
        Ok(())
    }

    fn validate_tree_shape(&self) -> crate::Result<()> {
        // At most one joint may claim a given link as `child` — checked
        // before root-counting so a later step can rely on every link
        // having at most one parent (see `DisconnectedLinks`'s own docs
        // for why that invariant matters to the connectivity check below).
        let mut claimed_by: HashMap<&str, &str> = HashMap::new();
        for joint in &self.joints {
            if let Some(&first_joint) = claimed_by.get(joint.child.as_str()) {
                return Err(UrdfError::MultipleParentJoints {
                    link: joint.child.clone(),
                    first_joint: first_joint.to_owned(),
                    second_joint: joint.name.clone(),
                });
            }
            claimed_by.insert(joint.child.as_str(), joint.name.as_str());
        }

        let children: HashSet<&str> = self.joints.iter().map(|j| j.child.as_str()).collect();
        let roots: Vec<&str> = self
            .links
            .iter()
            .map(|l| l.name.as_str())
            .filter(|name| !children.contains(name))
            .collect();

        let root = match roots.as_slice() {
            [] => return Err(UrdfError::NoRootLink),
            [only] => *only,
            _ => {
                return Err(UrdfError::MultipleRootLinks {
                    roots: roots.iter().map(|&s| s.to_owned()).collect(),
                });
            }
        };

        // Every other link has exactly one parent (just established), so a
        // link unreached by this walk cannot be missing an edge — it can
        // only be part of a component disconnected from `root` in which
        // every member's sole in-edge comes from within that same
        // component, which (being finite) forces a cycle among them. See
        // `UrdfError::DisconnectedLinks`'s own docs for the full argument.
        let mut visited: HashSet<&str> = HashSet::new();
        let mut stack: Vec<&str> = vec![root];
        while let Some(current) = stack.pop() {
            if !visited.insert(current) {
                continue;
            }
            for joint in self.joints_with_parent(current) {
                stack.push(joint.child.as_str());
            }
        }

        let unreached: Vec<String> = self
            .links
            .iter()
            .filter(|l| !visited.contains(l.name.as_str()))
            .map(|l| l.name.clone())
            .collect();
        if !unreached.is_empty() {
            return Err(UrdfError::DisconnectedLinks { links: unreached });
        }
        Ok(())
    }

    fn validate_mimic(&self) -> crate::Result<()> {
        let joints_by_name: HashMap<&str, &Joint> =
            self.joints.iter().map(|j| (j.name.as_str(), j)).collect();

        for joint in &self.joints {
            let Some(mimic) = &joint.mimic else {
                continue;
            };
            let Some(&target) = joints_by_name.get(mimic.joint.as_str()) else {
                return Err(UrdfError::UnknownMimicTarget {
                    joint: joint.name.clone(),
                    target: mimic.joint.clone(),
                });
            };
            if joint.kind.degrees_of_freedom() != 1 || target.kind.degrees_of_freedom() != 1 {
                return Err(UrdfError::MimicRequiresSingleDofJoint {
                    joint: joint.name.clone(),
                    other: mimic.joint.clone(),
                });
            }
        }

        // Cycle detection over the mimic graph: every joint has at most one
        // outgoing mimic edge (to `mimic.joint`), so following it from any
        // start either terminates at a non-mimicking joint or revisits a
        // node already on the current walk — a cycle. `resolved` memoizes
        // every node already proven to terminate, so no walk repeats work
        // an earlier one already did (mirroring `astrs_tf::buffer::
        // TransformBuffer::validate`'s own `visited`-across-starts shape).
        let mut resolved: HashSet<&str> = HashSet::new();
        for joint in &self.joints {
            if resolved.contains(joint.name.as_str()) {
                continue;
            }
            let mut path: Vec<&str> = Vec::new();
            let mut current = joint.name.as_str();
            loop {
                if resolved.contains(current) {
                    break;
                }
                if let Some(cycle_start) = path.iter().position(|&name| name == current) {
                    let mut cycle: Vec<String> =
                        path[cycle_start..].iter().map(|&s| s.to_owned()).collect();
                    cycle.push(current.to_owned());
                    return Err(UrdfError::MimicCycle { path: cycle });
                }
                path.push(current);
                match joints_by_name.get(current).and_then(|j| j.mimic.as_ref()) {
                    Some(mimic) => current = mimic.joint.as_str(),
                    None => break,
                }
            }
            resolved.extend(path);
        }
        Ok(())
    }

    fn validate_material_references(&self) -> crate::Result<()> {
        for link in &self.links {
            for visual in &link.visuals {
                let Some(material) = &visual.material else {
                    continue;
                };
                if self.resolve_material(material).is_none() {
                    return Err(UrdfError::UnknownMaterial {
                        link: link.name.clone(),
                        material: material.name.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::JointKind;
    use crate::model::joint::JointMimic;
    use crate::model::link::Link;

    fn link(name: &str) -> Link {
        Link::named(name)
    }

    fn joint(name: &str, parent: &str, child: &str) -> Joint {
        Joint::new(name, JointKind::Fixed, parent, child)
    }

    fn chain_robot() -> Robot {
        let mut robot = Robot::named("demo");
        robot.links.push(link("base_link"));
        robot.links.push(link("mid_link"));
        robot.links.push(link("tip_link"));
        robot.joints.push(joint("j1", "base_link", "mid_link"));
        robot.joints.push(joint("j2", "mid_link", "tip_link"));
        robot
    }

    #[test]
    fn a_valid_chain_validates() {
        assert_eq!(chain_robot().validate(), Ok(()));
    }

    #[test]
    fn duplicate_link_names_are_rejected() {
        let mut robot = chain_robot();
        robot.links.push(link("mid_link"));
        assert_eq!(
            robot.validate(),
            Err(UrdfError::DuplicateLinkName {
                name: "mid_link".to_owned()
            })
        );
    }

    #[test]
    fn duplicate_joint_names_are_rejected() {
        let mut robot = chain_robot();
        robot.joints.push(joint("j1", "mid_link", "tip_link"));
        assert_eq!(
            robot.validate(),
            Err(UrdfError::DuplicateJointName {
                name: "j1".to_owned()
            })
        );
    }

    #[test]
    fn an_unknown_parent_link_is_rejected() {
        let mut robot = chain_robot();
        robot.joints.push(joint("j3", "nonexistent", "tip_link"));
        assert_eq!(
            robot.validate(),
            Err(UrdfError::UnknownParentLink {
                joint: "j3".to_owned(),
                link: "nonexistent".to_owned(),
            })
        );
    }

    #[test]
    fn an_unknown_child_link_is_rejected() {
        let mut robot = chain_robot();
        robot.joints.push(joint("j3", "tip_link", "nonexistent"));
        assert_eq!(
            robot.validate(),
            Err(UrdfError::UnknownChildLink {
                joint: "j3".to_owned(),
                link: "nonexistent".to_owned(),
            })
        );
    }

    #[test]
    fn two_joints_claiming_the_same_child_is_rejected() {
        let mut robot = chain_robot();
        robot.joints.push(joint("j3", "base_link", "tip_link"));
        assert_eq!(
            robot.validate(),
            Err(UrdfError::MultipleParentJoints {
                link: "tip_link".to_owned(),
                first_joint: "j2".to_owned(),
                second_joint: "j3".to_owned(),
            })
        );
    }

    #[test]
    fn a_link_with_no_joints_at_all_is_its_own_trivial_tree() {
        let mut robot = Robot::named("demo");
        robot.links.push(link("solo"));
        assert_eq!(robot.validate(), Ok(()));
    }

    #[test]
    fn a_full_cycle_covering_every_link_has_no_root() {
        let mut robot = Robot::named("demo");
        robot.links.push(link("a"));
        robot.links.push(link("b"));
        robot.joints.push(joint("j1", "a", "b"));
        robot.joints.push(joint("j2", "b", "a"));
        assert_eq!(robot.validate(), Err(UrdfError::NoRootLink));
    }

    #[test]
    fn two_separate_trees_have_two_roots() {
        let mut robot = Robot::named("demo");
        robot.links.push(link("root_a"));
        robot.links.push(link("child_a"));
        robot.links.push(link("root_b"));
        robot.links.push(link("child_b"));
        robot.joints.push(joint("j1", "root_a", "child_a"));
        robot.joints.push(joint("j2", "root_b", "child_b"));
        assert_eq!(
            robot.validate(),
            Err(UrdfError::MultipleRootLinks {
                roots: vec!["root_a".to_owned(), "root_b".to_owned()],
            })
        );
    }

    #[test]
    fn a_disjoint_cycle_not_touching_the_root_is_reported_as_disconnected() {
        let mut robot = Robot::named("demo");
        robot.links.push(link("root"));
        robot.links.push(link("child"));
        robot.links.push(link("x"));
        robot.links.push(link("y"));
        robot.joints.push(joint("j1", "root", "child"));
        robot.joints.push(joint("j2", "x", "y"));
        robot.joints.push(joint("j3", "y", "x"));
        assert_eq!(
            robot.validate(),
            Err(UrdfError::DisconnectedLinks {
                links: vec!["x".to_owned(), "y".to_owned()],
            })
        );
    }

    #[test]
    fn an_unknown_mimic_target_is_rejected() {
        let mut robot = chain_robot();
        robot.joints[1].mimic = Some(JointMimic {
            joint: "nonexistent".to_owned(),
            multiplier: 1.0,
            offset: 0.0,
        });
        assert_eq!(
            robot.validate(),
            Err(UrdfError::UnknownMimicTarget {
                joint: "j2".to_owned(),
                target: "nonexistent".to_owned(),
            })
        );
    }

    #[test]
    fn a_mimic_on_a_multi_dof_joint_is_rejected() {
        let mut robot = chain_robot();
        robot.joints[1].kind = JointKind::Planar;
        robot.joints[1].mimic = Some(JointMimic {
            joint: "j1".to_owned(),
            multiplier: 1.0,
            offset: 0.0,
        });
        assert_eq!(
            robot.validate(),
            Err(UrdfError::MimicRequiresSingleDofJoint {
                joint: "j2".to_owned(),
                other: "j1".to_owned(),
            })
        );
    }

    #[test]
    fn a_valid_single_dof_mimic_relationship_validates() {
        let mut robot = chain_robot();
        robot.joints[0].kind = JointKind::Revolute;
        robot.joints[1].kind = JointKind::Revolute;
        robot.joints[1].mimic = Some(JointMimic {
            joint: "j1".to_owned(),
            multiplier: 2.0,
            offset: 0.1,
        });
        assert_eq!(robot.validate(), Ok(()));
    }

    #[test]
    fn a_self_mimic_is_a_cycle() {
        let mut robot = chain_robot();
        robot.joints[0].kind = JointKind::Revolute;
        robot.joints[0].mimic = Some(JointMimic {
            joint: "j1".to_owned(),
            multiplier: 1.0,
            offset: 0.0,
        });
        assert_eq!(
            robot.validate(),
            Err(UrdfError::MimicCycle {
                path: vec!["j1".to_owned(), "j1".to_owned()],
            })
        );
    }

    #[test]
    fn a_two_joint_mimic_cycle_is_rejected() {
        let mut robot = chain_robot();
        robot.joints[0].kind = JointKind::Revolute;
        robot.joints[1].kind = JointKind::Revolute;
        robot.joints[0].mimic = Some(JointMimic {
            joint: "j2".to_owned(),
            multiplier: 1.0,
            offset: 0.0,
        });
        robot.joints[1].mimic = Some(JointMimic {
            joint: "j1".to_owned(),
            multiplier: 1.0,
            offset: 0.0,
        });
        let error = robot.validate().unwrap_err();
        assert!(matches!(error, UrdfError::MimicCycle { .. }));
    }

    #[test]
    fn a_bare_material_reference_to_a_declared_robot_level_material_validates() {
        use crate::math::Transform;
        use crate::model::link::Visual;
        use crate::model::material::Material;

        let mut robot = chain_robot();
        robot.materials.push(Material {
            name: "blue".to_owned(),
            color: Some(crate::model::Color::new(0.0, 0.0, 1.0, 1.0)),
            texture_filename: None,
        });
        robot.links[0].visuals.push(Visual {
            name: None,
            origin: Transform::IDENTITY,
            geometry: crate::model::Geometry::Sphere { radius: 1.0 },
            material: Some(Material::named("blue")),
        });
        assert_eq!(robot.validate(), Ok(()));
    }

    #[test]
    fn a_bare_material_reference_to_an_undeclared_material_is_rejected() {
        use crate::math::Transform;
        use crate::model::link::Visual;
        use crate::model::material::Material;

        let mut robot = chain_robot();
        robot.links[0].visuals.push(Visual {
            name: None,
            origin: Transform::IDENTITY,
            geometry: crate::model::Geometry::Sphere { radius: 1.0 },
            material: Some(Material::named("nonexistent")),
        });
        assert_eq!(
            robot.validate(),
            Err(UrdfError::UnknownMaterial {
                link: "base_link".to_owned(),
                material: "nonexistent".to_owned(),
            })
        );
    }

    #[test]
    fn an_inline_material_with_a_color_needs_no_robot_level_declaration() {
        use crate::math::Transform;
        use crate::model::link::Visual;
        use crate::model::material::Material;

        let mut robot = chain_robot();
        robot.links[0].visuals.push(Visual {
            name: None,
            origin: Transform::IDENTITY,
            geometry: crate::model::Geometry::Sphere { radius: 1.0 },
            material: Some(Material {
                name: "inline_red".to_owned(),
                color: Some(crate::model::Color::new(1.0, 0.0, 0.0, 1.0)),
                texture_filename: None,
            }),
        });
        assert_eq!(robot.validate(), Ok(()));
    }
}
