//! [`Chain`] — an ordered sequence of joints from a base link down to a tip
//! link.

use crate::UrdfError;
use crate::model::{Joint, Robot};

use super::topology::Topology;

/// The ordered sequence of joints from `base` to `tip`: `joints[0]`'s
/// parent is `base`, `joints[i]`'s parent is `joints[i-1]`'s child, and
/// `joints.last()`'s child is `tip`.
///
/// This is a kinematic chain in the classical robotics sense — the
/// `base_link -> ... -> tip_link` path a manipulator's forward/inverse
/// kinematics is solved along — extracted from the general tree a
/// [`Robot`] describes (a full robot is usually more than one chain: a
/// mobile base with an arm on top has an `odom -> base` chain and a
/// `base -> gripper` chain, both extractable from the same [`Robot`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    base: String,
    tip: String,
    /// Joint names, in order from `base` to `tip`.
    joint_names: Vec<String>,
}

impl Chain {
    /// Extracts the chain from `base` to `tip`.
    ///
    /// `tip` must be a descendant of `base` in the robot's tree — this
    /// finds the single path a rooted tree guarantees exists between an
    /// ancestor and its descendant, not a general shortest path (unlike
    /// `astrs_tf::buffer::TransformBuffer::lookup_transform`, which finds a
    /// path between *any* two frames via their lowest common ancestor; a
    /// classical kinematic chain is specifically base-to-descendant, since
    /// that is what a manipulator's own joint sequence *is*).
    ///
    /// # Errors
    ///
    /// [`UrdfError::UnknownLink`] if `base` or `tip` is not in the robot.
    /// [`UrdfError::NoChainBetween`] if `tip` is not a descendant of
    /// `base` (including the case where they are simply unrelated, on
    /// different branches).
    pub fn extract(robot: &Robot, base: &str, tip: &str) -> crate::Result<Self> {
        let topology = Topology::build(robot)?;
        Self::extract_with_topology(&topology, base, tip)
    }

    /// [`Chain::extract`], reusing an already-built [`Topology`] — the
    /// entry point [`super::forward_kinematics`] uses so extracting a
    /// chain and then solving it does not re-validate and re-index the
    /// same robot twice.
    pub(super) fn extract_with_topology(
        topology: &Topology<'_>,
        base: &str,
        tip: &str,
    ) -> crate::Result<Self> {
        // Walk upward from `tip` toward the root, collecting joints, until
        // `base` is reached. `topology.parent_joint` itself reports
        // `UrdfError::UnknownLink` for a `tip` not in the robot; `base` is
        // checked explicitly since a walk that never reaches it (because it
        // does not exist at all) would otherwise report the same
        // `NoChainBetween` a real, unrelated link would.
        topology.parent_joint(base)?;

        let mut joints_reversed: Vec<&Joint> = Vec::new();
        let mut current = tip;
        loop {
            if current == base {
                joints_reversed.reverse();
                return Ok(Self {
                    base: base.to_owned(),
                    tip: tip.to_owned(),
                    joint_names: joints_reversed
                        .into_iter()
                        .map(|j| j.name.clone())
                        .collect(),
                });
            }
            match topology.parent_joint(current)? {
                Some(joint) => {
                    joints_reversed.push(joint);
                    current = joint.parent.as_str();
                }
                None => {
                    // Reached the robot's actual root without ever passing
                    // through `base` — `base` is not an ancestor of `tip`.
                    return Err(UrdfError::NoChainBetween {
                        base: base.to_owned(),
                        tip: tip.to_owned(),
                    });
                }
            }
        }
    }

    /// The chain's base (proximal) link name.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The chain's tip (distal) link name.
    #[must_use]
    pub fn tip(&self) -> &str {
        &self.tip
    }

    /// This chain's joints, in order from base to tip.
    #[must_use]
    pub fn joint_names(&self) -> &[String] {
        &self.joint_names
    }

    /// The number of joints in this chain.
    #[must_use]
    pub fn len(&self) -> usize {
        self.joint_names.len()
    }

    /// `true` when `base == tip` (a trivial, zero-joint chain).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.joint_names.is_empty()
    }

    /// The total degrees of freedom this chain contributes: the sum of
    /// [`crate::JointKind::degrees_of_freedom`] over every joint on it.
    #[must_use]
    pub fn degrees_of_freedom(&self, robot: &Robot) -> usize {
        self.joint_names
            .iter()
            .filter_map(|name| robot.joint(name))
            .map(|joint| joint.kind.degrees_of_freedom())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::JointKind;
    use crate::model::Link;

    fn arm_robot() -> Robot {
        let mut robot = Robot::named("arm");
        for name in ["base_link", "link1", "link2", "link3"] {
            robot.links.push(Link::named(name));
        }
        robot
            .joints
            .push(Joint::new("j1", JointKind::Revolute, "base_link", "link1"));
        robot
            .joints
            .push(Joint::new("j2", JointKind::Revolute, "link1", "link2"));
        robot
            .joints
            .push(Joint::new("j3", JointKind::Prismatic, "link2", "link3"));
        robot
    }

    #[test]
    fn a_full_chain_lists_every_joint_in_order() {
        let robot = arm_robot();
        let chain = Chain::extract(&robot, "base_link", "link3").unwrap();
        assert_eq!(chain.base(), "base_link");
        assert_eq!(chain.tip(), "link3");
        assert_eq!(chain.joint_names(), ["j1", "j2", "j3"]);
        assert_eq!(chain.len(), 3);
        assert!(!chain.is_empty());
    }

    #[test]
    fn a_partial_chain_stops_at_the_requested_base() {
        let robot = arm_robot();
        let chain = Chain::extract(&robot, "link1", "link3").unwrap();
        assert_eq!(chain.joint_names(), ["j2", "j3"]);
    }

    #[test]
    fn a_chain_from_a_link_to_itself_is_empty() {
        let robot = arm_robot();
        let chain = Chain::extract(&robot, "link2", "link2").unwrap();
        assert!(chain.is_empty());
        assert_eq!(chain.len(), 0);
    }

    #[test]
    fn a_chain_where_tip_is_not_a_descendant_of_base_is_rejected() {
        let robot = arm_robot();
        // link1 is *above* base_link's child, not below it.
        let error = Chain::extract(&robot, "link1", "base_link").unwrap_err();
        assert_eq!(
            error,
            UrdfError::NoChainBetween {
                base: "link1".to_owned(),
                tip: "base_link".to_owned(),
            }
        );
    }

    #[test]
    fn a_chain_between_unrelated_links_is_rejected() {
        let mut robot = Robot::named("demo");
        robot.links.push(Link::named("root"));
        robot.links.push(Link::named("branch_a"));
        robot.links.push(Link::named("branch_b"));
        robot
            .joints
            .push(Joint::new("ja", JointKind::Fixed, "root", "branch_a"));
        robot
            .joints
            .push(Joint::new("jb", JointKind::Fixed, "root", "branch_b"));
        let error = Chain::extract(&robot, "branch_a", "branch_b").unwrap_err();
        assert!(matches!(error, UrdfError::NoChainBetween { .. }));
    }

    #[test]
    fn an_unknown_tip_is_rejected() {
        let robot = arm_robot();
        assert_eq!(
            Chain::extract(&robot, "base_link", "nonexistent"),
            Err(UrdfError::UnknownLink {
                link: "nonexistent".to_owned()
            })
        );
    }

    #[test]
    fn an_unknown_base_is_rejected() {
        let robot = arm_robot();
        assert_eq!(
            Chain::extract(&robot, "nonexistent", "link3"),
            Err(UrdfError::UnknownLink {
                link: "nonexistent".to_owned()
            })
        );
    }

    #[test]
    fn degrees_of_freedom_sums_across_the_chain() {
        let robot = arm_robot();
        let chain = Chain::extract(&robot, "base_link", "link3").unwrap();
        // Two revolute (1 each) + one prismatic (1) = 3.
        assert_eq!(chain.degrees_of_freedom(&robot), 3);
    }
}
