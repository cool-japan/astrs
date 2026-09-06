//! [`Topology`] — the precomputed parent/child index over an
//! already-[`Robot::validate`]d robot, shared by [`super::Chain`] and
//! [`super::forward_kinematics`] so neither has to re-scan `robot.joints`
//! linearly on every query.

use std::collections::HashMap;

use crate::UrdfError;
use crate::model::{Joint, Robot};

/// A precomputed index of a [`Robot`]'s tree structure: each link's parent
/// joint (if any) and each joint's position in `robot.joints`.
///
/// # Why this exists apart from [`Robot`] itself
///
/// [`Robot::joint_with_child`]/[`Robot::joints_with_parent`] both do a
/// linear scan of `robot.joints` — fine for the occasional lookup
/// [`Robot`]'s own public API is meant for, but [`super::Chain::extract`]
/// and [`super::forward_kinematics`] each need it *repeatedly*, once per
/// link on a chain and once per joint in the whole robot respectively. This
/// type is the `O(links + joints)` index both build once and share.
///
/// # Trust boundary
///
/// [`Topology::build`] re-derives the same facts [`Robot::validate`]
/// already checked (a unique parent joint per link, a single root) rather
/// than trusting a caller's claim that validation already ran — the same
/// non-negotiable stance `astrs_tf::buffer::TransformBuffer::walk_to_root`'s
/// own *reactive* cycle guard takes even though `set_transform` already
/// runs a *proactive* one (see that module's "Two-layer cycle protection"
/// docs): a proactive check elsewhere is a good reason an insert-time
/// failure is rare, never a reason a read-time walk gets to skip its own
/// guarantee.
#[derive(Debug, Clone)]
pub struct Topology<'a> {
    robot: &'a Robot,
    /// Link name -> index into `robot.joints` of that link's parent joint.
    /// A link with no entry is a root.
    parent_joint_index: HashMap<&'a str, usize>,
}

impl<'a> Topology<'a> {
    /// Builds the index, and validates the tree-shape invariants
    /// [`super::Chain::extract`]/[`super::forward_kinematics`] depend on
    /// while doing it (see this struct's "Trust boundary" docs).
    ///
    /// # Errors
    ///
    /// [`UrdfError::MultipleParentJoints`] if two joints claim the same
    /// child link; [`UrdfError::NoRootLink`]/[`UrdfError::MultipleRootLinks`]
    /// if the robot does not have exactly one root; [`UrdfError::JointGraphCycle`]
    /// if walking from that root does not reach every link (the read-time
    /// reactive counterpart to [`Robot::validate`]'s own proactive
    /// [`UrdfError::DisconnectedLinks`] — see this struct's docs).
    pub fn build(robot: &'a Robot) -> crate::Result<Self> {
        let mut parent_joint_index: HashMap<&'a str, usize> = HashMap::new();
        for (index, joint) in robot.joints.iter().enumerate() {
            if let Some(&existing) = parent_joint_index.get(joint.child.as_str()) {
                return Err(UrdfError::MultipleParentJoints {
                    link: joint.child.clone(),
                    first_joint: robot.joints[existing].name.clone(),
                    second_joint: joint.name.clone(),
                });
            }
            parent_joint_index.insert(joint.child.as_str(), index);
        }

        let roots: Vec<&str> = robot
            .links
            .iter()
            .map(|link| link.name.as_str())
            .filter(|name| !parent_joint_index.contains_key(name))
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

        let topology = Self {
            robot,
            parent_joint_index,
        };

        let mut visited: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut stack: Vec<&str> = vec![root];
        while let Some(current) = stack.pop() {
            if !visited.insert(current) {
                continue;
            }
            for joint in robot.joints_with_parent(current) {
                stack.push(joint.child.as_str());
            }
        }
        if visited.len() != robot.links.len() {
            let mut path: Vec<String> = robot
                .links
                .iter()
                .map(|l| l.name.clone())
                .filter(|name| !visited.contains(name.as_str()))
                .collect();
            path.sort_unstable();
            return Err(UrdfError::JointGraphCycle { path });
        }

        Ok(topology)
    }

    /// The [`Robot`] this topology indexes.
    #[must_use]
    pub const fn robot(&self) -> &'a Robot {
        self.robot
    }

    /// `link_name`'s parent joint, or `None` if it is the root.
    ///
    /// # Errors
    ///
    /// [`UrdfError::UnknownLink`] if `link_name` is not in the robot at
    /// all (distinct from "is the root," which returns `Ok(None)`).
    pub fn parent_joint(&self, link_name: &str) -> crate::Result<Option<&'a Joint>> {
        if !self.robot.links.iter().any(|l| l.name == link_name) {
            return Err(UrdfError::UnknownLink {
                link: link_name.to_owned(),
            });
        }
        Ok(self
            .parent_joint_index
            .get(link_name)
            .map(|&index| &self.robot.joints[index]))
    }

    /// The single root link's name (no parent joint).
    #[must_use]
    pub fn root(&self) -> &'a str {
        // `Topology::build` already established exactly one such link
        // exists; re-deriving it here (rather than caching it as a field)
        // keeps this type's only stored state the one index every method
        // actually needs, at the cost of one more linear scan on the rare
        // caller that asks for the root explicitly.
        self.robot
            .links
            .iter()
            .map(|l| l.name.as_str())
            .find(|name| !self.parent_joint_index.contains_key(name))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::JointKind;
    use crate::model::Link;

    fn chain_robot() -> Robot {
        let mut robot = Robot::named("demo");
        robot.links.push(Link::named("base_link"));
        robot.links.push(Link::named("mid_link"));
        robot.links.push(Link::named("tip_link"));
        robot.joints.push(Joint::new(
            "j1",
            JointKind::Revolute,
            "base_link",
            "mid_link",
        ));
        robot.joints.push(Joint::new(
            "j2",
            JointKind::Revolute,
            "mid_link",
            "tip_link",
        ));
        robot
    }

    #[test]
    fn root_is_the_link_with_no_parent_joint() {
        let robot = chain_robot();
        let topology = Topology::build(&robot).unwrap();
        assert_eq!(topology.root(), "base_link");
    }

    #[test]
    fn parent_joint_is_none_for_the_root() {
        let robot = chain_robot();
        let topology = Topology::build(&robot).unwrap();
        assert_eq!(topology.parent_joint("base_link").unwrap(), None);
    }

    #[test]
    fn parent_joint_finds_the_owning_joint_for_a_non_root_link() {
        let robot = chain_robot();
        let topology = Topology::build(&robot).unwrap();
        let joint = topology.parent_joint("tip_link").unwrap().unwrap();
        assert_eq!(joint.name, "j2");
    }

    #[test]
    fn parent_joint_reports_an_unknown_link() {
        let robot = chain_robot();
        let topology = Topology::build(&robot).unwrap();
        assert_eq!(
            topology.parent_joint("nonexistent"),
            Err(UrdfError::UnknownLink {
                link: "nonexistent".to_owned()
            })
        );
    }

    #[test]
    fn build_rejects_a_robot_with_no_root() {
        let mut robot = Robot::named("demo");
        robot.links.push(Link::named("a"));
        robot.links.push(Link::named("b"));
        robot
            .joints
            .push(Joint::new("j1", JointKind::Fixed, "a", "b"));
        robot
            .joints
            .push(Joint::new("j2", JointKind::Fixed, "b", "a"));
        // `Topology` deliberately has no `PartialEq` (it borrows `&Robot`
        // and production code never needs to compare two of them), so
        // every assertion here compares the extracted `Err` value instead
        // of the `Result` as a whole.
        assert_eq!(Topology::build(&robot).unwrap_err(), UrdfError::NoRootLink);
    }

    #[test]
    fn build_rejects_a_robot_with_multiple_roots() {
        let mut robot = Robot::named("demo");
        robot.links.push(Link::named("root_a"));
        robot.links.push(Link::named("root_b"));
        assert_eq!(
            Topology::build(&robot).unwrap_err(),
            UrdfError::MultipleRootLinks {
                roots: vec!["root_a".to_owned(), "root_b".to_owned()],
            }
        );
    }

    #[test]
    fn build_rejects_multiple_parent_joints_for_one_link() {
        let mut robot = chain_robot();
        robot
            .joints
            .push(Joint::new("j3", JointKind::Fixed, "base_link", "tip_link"));
        assert_eq!(
            Topology::build(&robot).unwrap_err(),
            UrdfError::MultipleParentJoints {
                link: "tip_link".to_owned(),
                first_joint: "j2".to_owned(),
                second_joint: "j3".to_owned(),
            }
        );
    }

    #[test]
    fn build_detects_a_disjoint_cycle_reactively() {
        let mut robot = chain_robot();
        robot.links.push(Link::named("x"));
        robot.links.push(Link::named("y"));
        robot
            .joints
            .push(Joint::new("j3", JointKind::Fixed, "x", "y"));
        robot
            .joints
            .push(Joint::new("j4", JointKind::Fixed, "y", "x"));
        let error = Topology::build(&robot).unwrap_err();
        assert!(matches!(error, UrdfError::JointGraphCycle { .. }));
    }

    #[test]
    fn a_single_link_robot_with_no_joints_has_itself_as_root() {
        let mut robot = Robot::named("demo");
        robot.links.push(Link::named("solo"));
        let topology = Topology::build(&robot).unwrap();
        assert_eq!(topology.root(), "solo");
    }
}
