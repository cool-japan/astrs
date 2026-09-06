//! ROS 2 names: validation, expansion, remapping and DDS mangling.
//!
//! Three layers, in the order a name passes through them:
//!
//! 1. [`validate`] — is this string a legal ROS 2 name at all?
//! 2. [`expand`] — what does it mean *for this node*? (`~/scan` under
//!    `/robot`'s `talker` is `/robot/talker/scan`), and does a remapping
//!    rule replace it?
//! 3. [`mangle`] — what does DDS call it? (`/robot/scan` is `rt/robot/scan`;
//!    `/add_two_ints` is `rq/add_two_intsRequest` and
//!    `rr/add_two_intsReply`.)
//!
//! This module re-exports the checked value types the rest of the crate
//! passes around: [`NodeName`], which is a node's name and namespace
//! together and is the thing every expansion is relative to, and
//! [`FullName`], a name that has been through all three steps and is
//! therefore safe to mangle without another check.
//!
//! # Example
//!
//! ```
//! use astrs_ros2::names::{FullName, NodeName};
//! use astrs_ros2::names::mangle::TopicKind;
//!
//! let node = NodeName::new("talker", "/robot")?;
//! assert_eq!(node.fully_qualified(), "/robot/talker");
//!
//! let topic = node.resolve_topic("~/scan")?;
//! assert_eq!(topic.as_str(), "/robot/talker/scan");
//! assert_eq!(topic.dds_name(TopicKind::Topic), "rt/robot/talker/scan");
//! # Ok::<(), astrs_ros2::Ros2Error>(())
//! ```

pub mod expand;
pub mod mangle;
pub mod validate;

use crate::error::{NameFault, NameKind, Ros2Error, Ros2Result};

pub use expand::{RemapRule, RemapRules};
pub use mangle::{ActionEndpoint, TopicKind, TypeNamespace};
pub use validate::{MAX_NAME_LEN, MAX_NODE_NAME_LEN, ROOT_NAMESPACE, is_hidden};

/// Wrap a name fault as the error for a topic-shaped name.
fn topic_error(name: &str, fault: NameFault) -> Ros2Error {
    Ros2Error::InvalidTopicName {
        name: name.to_owned(),
        fault,
    }
}

/// Wrap a name fault as the error for a service-, action- or
/// parameter-shaped name.
fn service_error(kind: NameKind, name: &str, fault: NameFault) -> Ros2Error {
    Ros2Error::InvalidServiceName {
        kind,
        name: name.to_owned(),
        fault,
    }
}

/// Reject a name that will not fit in a discovery announcement.
fn check_length(kind: NameKind, name: &str, limit: usize) -> Ros2Result<()> {
    if name.len() > limit {
        return Err(Ros2Error::NameTooLong {
            kind,
            len: name.len(),
            limit,
        });
    }
    Ok(())
}

/// A node's name and namespace, checked.
///
/// The identity every relative name in a node is resolved against, and the
/// identity the ROS graph shows. Both halves are validated on construction,
/// so nothing downstream re-checks them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeName {
    name: String,
    namespace: String,
}

impl NodeName {
    /// Check a name and a namespace and pair them.
    ///
    /// An empty namespace is read as the root `/`, because that is what
    /// every launch file that omits `namespace:` means.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::InvalidNodeName`], [`Ros2Error::InvalidNamespace`] or
    /// [`Ros2Error::NameTooLong`].
    pub fn new(name: impl Into<String>, namespace: impl Into<String>) -> Ros2Result<Self> {
        let name = name.into();
        let namespace = namespace.into();
        let namespace = if namespace.is_empty() {
            ROOT_NAMESPACE.to_owned()
        } else {
            namespace
        };

        validate::validate_node_name(&name).map_err(|fault| Ros2Error::InvalidNodeName {
            name: name.clone(),
            fault,
        })?;
        check_length(NameKind::NodeName, &name, MAX_NODE_NAME_LEN)?;
        validate::validate_namespace(&namespace).map_err(|fault| Ros2Error::InvalidNamespace {
            namespace: namespace.clone(),
            fault,
        })?;
        check_length(NameKind::Namespace, &namespace, MAX_NAME_LEN)?;

        Ok(Self { name, namespace })
    }

    /// A node in the root namespace.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub fn in_root(name: impl Into<String>) -> Ros2Result<Self> {
        Self::new(name, ROOT_NAMESPACE)
    }

    /// The node's name, without its namespace.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The node's namespace, always absolute.
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// The namespace and name joined: what `ros2 node list` prints.
    #[must_use]
    pub fn fully_qualified(&self) -> String {
        expand::join(&self.namespace, &self.name)
    }

    /// True when this node's name is hidden from a default `ros2 node list`.
    #[must_use]
    pub fn is_hidden(&self) -> bool {
        validate::is_hidden(&self.fully_qualified())
    }

    /// Expand and remap a topic name against this node.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::InvalidTopicName`] or [`Ros2Error::NameTooLong`].
    pub fn resolve_topic(&self, name: &str) -> Ros2Result<FullName> {
        self.resolve_with(name, &RemapRules::new(), NameKind::TopicName)
    }

    /// Expand and remap a service name against this node.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::InvalidServiceName`] or [`Ros2Error::NameTooLong`].
    pub fn resolve_service(&self, name: &str) -> Ros2Result<FullName> {
        self.resolve_with(name, &RemapRules::new(), NameKind::ServiceName)
    }

    /// Expand and remap an action name against this node.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::InvalidServiceName`] or [`Ros2Error::NameTooLong`].
    pub fn resolve_action(&self, name: &str) -> Ros2Result<FullName> {
        self.resolve_with(name, &RemapRules::new(), NameKind::ActionName)
    }

    /// Expand `name` against this node, applying `rules`.
    ///
    /// # Errors
    ///
    /// The naming error matching `kind`, or [`Ros2Error::NameTooLong`].
    pub fn resolve_with(
        &self,
        name: &str,
        rules: &RemapRules,
        kind: NameKind,
    ) -> Ros2Result<FullName> {
        let wrap = |fault: NameFault| match kind {
            NameKind::TopicName => topic_error(name, fault),
            other => service_error(other, name, fault),
        };
        let expanded = rules
            .apply(name, &self.name, &self.namespace)
            .map_err(wrap)?;
        validate::validate_full_name(&expanded).map_err(wrap)?;
        check_length(kind, &expanded, MAX_NAME_LEN)?;
        Ok(FullName {
            name: expanded,
            kind,
        })
    }
}

impl core::fmt::Display for NodeName {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(&self.fully_qualified())
    }
}

/// A name that has been expanded, remapped and validated.
///
/// The proof obligation this type carries: everything in it starts with `/`,
/// contains no `~` and no `{substitution}`, and is short enough to mangle.
/// [`dds_name`](FullName::dds_name) can therefore be infallible.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FullName {
    name: String,
    kind: NameKind,
}

impl FullName {
    /// Check a name that is already fully qualified.
    ///
    /// # Errors
    ///
    /// The naming error matching `kind`, or [`Ros2Error::NameTooLong`].
    pub fn new(name: impl Into<String>, kind: NameKind) -> Ros2Result<Self> {
        let name = name.into();
        let wrap = |fault: NameFault| match kind {
            NameKind::TopicName => topic_error(&name, fault),
            other => service_error(other, &name, fault),
        };
        validate::validate_full_name(&name).map_err(wrap)?;
        check_length(kind, &name, MAX_NAME_LEN)?;
        Ok(Self { name, kind })
    }

    /// Check a fully-qualified topic name.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub fn topic(name: impl Into<String>) -> Ros2Result<Self> {
        Self::new(name, NameKind::TopicName)
    }

    /// Check a fully-qualified service name.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub fn service(name: impl Into<String>) -> Ros2Result<Self> {
        Self::new(name, NameKind::ServiceName)
    }

    /// Check a fully-qualified action name.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub fn action(name: impl Into<String>) -> Ros2Result<Self> {
        Self::new(name, NameKind::ActionName)
    }

    /// The name itself.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.name
    }

    /// What kind of name it is.
    #[must_use]
    pub const fn kind(&self) -> NameKind {
        self.kind
    }

    /// Take the string back out.
    #[must_use]
    pub fn into_string(self) -> String {
        self.name
    }

    /// True when `ros2 topic list` would hide it.
    #[must_use]
    pub fn is_hidden(&self) -> bool {
        validate::is_hidden(&self.name)
    }

    /// The DDS topic name for this ROS name, at `kind`.
    ///
    /// Infallible: every path that produces a [`FullName`] has already
    /// checked what [`mangle::dds_topic_name`] would check.
    #[must_use]
    pub fn dds_name(&self, kind: TopicKind) -> String {
        let body = self.name.strip_prefix('/').unwrap_or(&self.name);
        format!("{}{body}{}", kind.prefix(), kind.suffix())
    }

    /// Derive one of an action's five endpoint names.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::NameTooLong`] — the derived name is longer than this one
    /// by the `/_action/<leaf>` infix, and that can push it past the limit.
    pub fn action_endpoint(&self, endpoint: ActionEndpoint) -> Ros2Result<Self> {
        let derived = mangle::action_name(&self.name, endpoint);
        let kind = if endpoint.is_service() {
            NameKind::ServiceName
        } else {
            NameKind::TopicName
        };
        check_length(kind, &derived, MAX_NAME_LEN)?;
        Ok(Self {
            name: derived,
            kind,
        })
    }
}

impl core::fmt::Display for FullName {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(&self.name)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_node_name_joins_with_its_namespace() {
        let node = NodeName::new("talker", "/robot").unwrap();
        assert_eq!(node.name(), "talker");
        assert_eq!(node.namespace(), "/robot");
        assert_eq!(node.fully_qualified(), "/robot/talker");
        assert_eq!(node.to_string(), "/robot/talker");
    }

    #[test]
    fn an_empty_namespace_means_the_root() {
        let node = NodeName::new("talker", "").unwrap();
        assert_eq!(node.namespace(), "/");
        assert_eq!(node.fully_qualified(), "/talker");
        assert_eq!(node, NodeName::in_root("talker").unwrap());
    }

    #[test]
    fn a_bad_node_name_names_the_fault() {
        let error = NodeName::new("/talker", "/").expect_err("slashes are illegal");
        assert!(matches!(
            error,
            Ros2Error::InvalidNodeName {
                fault: NameFault::UnexpectedSlash { offset: 0 },
                ..
            }
        ));
        assert!(error.is_naming());
    }

    #[test]
    fn a_bad_namespace_names_the_fault() {
        let error = NodeName::new("talker", "robot").expect_err("must be absolute");
        assert!(matches!(
            error,
            Ros2Error::InvalidNamespace {
                fault: NameFault::NotAbsolute,
                ..
            }
        ));
    }

    #[test]
    fn an_over_long_node_name_is_refused() {
        let long = "n".repeat(MAX_NODE_NAME_LEN + 1);
        assert!(matches!(
            NodeName::new(long, "/").expect_err("too long"),
            Ros2Error::NameTooLong {
                kind: NameKind::NodeName,
                ..
            }
        ));
    }

    #[test]
    fn resolving_covers_the_four_written_forms() {
        let node = NodeName::new("talker", "/robot").unwrap();
        assert_eq!(node.resolve_topic("/scan").unwrap().as_str(), "/scan");
        assert_eq!(node.resolve_topic("scan").unwrap().as_str(), "/robot/scan");
        assert_eq!(
            node.resolve_topic("~/scan").unwrap().as_str(),
            "/robot/talker/scan"
        );
        assert_eq!(
            node.resolve_topic("{node}/scan").unwrap().as_str(),
            "/robot/talker/scan"
        );
    }

    #[test]
    fn resolving_reports_the_right_error_kind() {
        let node = NodeName::in_root("talker").unwrap();
        assert!(matches!(
            node.resolve_topic("bad name").expect_err("space"),
            Ros2Error::InvalidTopicName { .. }
        ));
        assert!(matches!(
            node.resolve_service("bad name").expect_err("space"),
            Ros2Error::InvalidServiceName {
                kind: NameKind::ServiceName,
                ..
            }
        ));
        assert!(matches!(
            node.resolve_action("bad name").expect_err("space"),
            Ros2Error::InvalidServiceName {
                kind: NameKind::ActionName,
                ..
            }
        ));
    }

    #[test]
    fn remapping_applies_during_resolution() {
        let node = NodeName::in_root("talker").unwrap();
        let rules = RemapRules::new().with(RemapRule::new("scan", "/lidar/scan").unwrap());
        let resolved = node
            .resolve_with("scan", &rules, NameKind::TopicName)
            .unwrap();
        assert_eq!(resolved.as_str(), "/lidar/scan");
    }

    #[test]
    fn a_full_name_mangles_without_failing() {
        let name = FullName::topic("/robot/scan").unwrap();
        assert_eq!(name.dds_name(TopicKind::Topic), "rt/robot/scan");
        assert_eq!(name.kind(), NameKind::TopicName);
        assert_eq!(name.to_string(), "/robot/scan");
        assert_eq!(name.clone().into_string(), "/robot/scan");
    }

    #[test]
    fn a_full_name_agrees_with_the_free_mangler() {
        for kind in TopicKind::ALL {
            let name = FullName::service("/robot/add_two_ints").unwrap();
            assert_eq!(
                name.dds_name(kind),
                mangle::dds_topic_name("/robot/add_two_ints", kind).unwrap()
            );
        }
    }

    #[test]
    fn a_full_name_refuses_an_unexpanded_one() {
        assert!(matches!(
            FullName::topic("~/scan").expect_err("private"),
            Ros2Error::InvalidTopicName { .. }
        ));
        assert!(matches!(
            FullName::topic("scan").expect_err("relative"),
            Ros2Error::InvalidTopicName {
                fault: NameFault::NotAbsolute,
                ..
            }
        ));
    }

    #[test]
    fn action_endpoints_derive_from_a_full_name() {
        let action = FullName::action("/fibonacci").unwrap();
        let send_goal = action.action_endpoint(ActionEndpoint::SendGoal).unwrap();
        assert_eq!(send_goal.as_str(), "/fibonacci/_action/send_goal");
        assert_eq!(send_goal.kind(), NameKind::ServiceName);
        assert_eq!(
            send_goal.dds_name(TopicKind::Request),
            "rq/fibonacci/_action/send_goalRequest"
        );

        let feedback = action.action_endpoint(ActionEndpoint::Feedback).unwrap();
        assert_eq!(feedback.kind(), NameKind::TopicName);
        assert_eq!(
            feedback.dds_name(TopicKind::Topic),
            "rt/fibonacci/_action/feedback"
        );
    }

    #[test]
    fn hidden_names_are_recognized_at_both_levels() {
        assert!(NodeName::new("_ros2cli_31337", "/").unwrap().is_hidden());
        assert!(!NodeName::in_root("talker").unwrap().is_hidden());
        assert!(FullName::topic("/_internal/state").unwrap().is_hidden());
        assert!(!FullName::topic("/state").unwrap().is_hidden());
    }

    #[test]
    fn an_over_long_resolved_name_is_refused() {
        let node = NodeName::in_root("talker").unwrap();
        let long = "t".repeat(MAX_NAME_LEN);
        assert!(matches!(
            node.resolve_topic(&long).expect_err("too long"),
            Ros2Error::NameTooLong {
                kind: NameKind::TopicName,
                ..
            }
        ));
    }
}
