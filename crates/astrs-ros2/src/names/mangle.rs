//! ROS 2 names ⇄ DDS names: the `rt/`, `rq/`, `rr/` prefixes and the
//! `pkg::msg::dds_::Type_` type spelling.
//!
//! Two vocabularies meet here. A ROS 2 name is absolute and
//! slash-separated (`/robot/scan`); a DDS topic name is a flat string with
//! no leading slash and a two-letter prefix that says what kind of endpoint
//! it belongs to. Everything below the `astrs-ros2` API speaks the second;
//! everything above speaks the first, and this module is the only place
//! that knows both.
//!
//! # The prefixes
//!
//! | Prefix | Suffix | Carries |
//! |---|---|---|
//! | `rt/` | — | a topic's samples |
//! | `rq/` | `Request` | a service's requests |
//! | `rr/` | `Reply` | a service's replies |
//! | `rs/` | `Reply` | a service's status (defined, unused by ROS 2 today) |
//!
//! One name escapes the scheme entirely: `ros_discovery_info`, the
//! `rmw_dds_common` graph topic, is announced **unprefixed**, because it is
//! how two `rmw` implementations find each other's *node* names and so
//! cannot itself depend on the mangling one of them uses. See
//! [`GRAPH_TOPIC`].
//!
//! # Actions
//!
//! An action is not a wire primitive: it is five endpoints on derived
//! names, three of them services and two of them topics.
//!
//! ```text
//!   <action>/_action/send_goal     service   (rq/… + rr/…)
//!   <action>/_action/cancel_goal   service   (rq/… + rr/…)
//!   <action>/_action/get_result    service   (rq/… + rr/…)
//!   <action>/_action/feedback      topic     (rt/…)
//!   <action>/_action/status        topic     (rt/…)
//! ```
//!
//! [`ActionEndpoint`] enumerates them and [`action_name`] derives the ROS
//! name, so the quintet is written down once.
//!
//! # Types
//!
//! `rosidl`'s DDS type name for `std_msgs/msg/String` is
//! `std_msgs::msg::dds_::String_`. The `dds_` component and the trailing
//! `_` are both part of the convention, and a service or action adds a
//! `_Request`/`_Response` (or `_SendGoal_Request`, …) *before* the trailing
//! underscore. [`dds_type_name`] and [`ros_type_name`] convert both ways.

use crate::error::NameFault;
use crate::names::validate::validate_full_name;

/// The DDS topic-name prefix for a ROS 2 topic.
pub const TOPIC_PREFIX: &str = "rt/";

/// The DDS topic-name prefix for a ROS 2 service request.
pub const REQUEST_PREFIX: &str = "rq/";

/// The DDS topic-name prefix for a ROS 2 service reply.
pub const REPLY_PREFIX: &str = "rr/";

/// The DDS topic-name prefix ROS 2 reserves for a service status stream.
pub const STATUS_PREFIX: &str = "rs/";

/// The suffix a service request's DDS topic name carries.
pub const REQUEST_SUFFIX: &str = "Request";

/// The suffix a service reply's DDS topic name carries.
pub const REPLY_SUFFIX: &str = "Reply";

/// The name component every action endpoint is nested under.
pub const ACTION_INFIX: &str = "/_action/";

/// The `rmw_dds_common` graph topic, announced with **no** prefix.
pub const GRAPH_TOPIC: &str = "ros_discovery_info";

/// The DDS type name the graph topic carries.
pub const GRAPH_TYPE: &str = "rmw_dds_common::msg::dds_::ParticipantEntitiesInfo_";

/// What a DDS topic name's prefix says it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TopicKind {
    /// `rt/` — a topic's samples.
    Topic,
    /// `rq/` — a service's requests.
    Request,
    /// `rr/` — a service's replies.
    Reply,
    /// `rs/` — a service's status.
    Status,
}

impl TopicKind {
    /// Every kind, for a test that must cover all of them.
    pub const ALL: [Self; 4] = [Self::Topic, Self::Request, Self::Reply, Self::Status];

    /// The DDS topic-name prefix.
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Topic => TOPIC_PREFIX,
            Self::Request => REQUEST_PREFIX,
            Self::Reply => REPLY_PREFIX,
            Self::Status => STATUS_PREFIX,
        }
    }

    /// The DDS topic-name suffix, empty for a plain topic.
    #[must_use]
    pub const fn suffix(self) -> &'static str {
        match self {
            Self::Topic => "",
            Self::Request => REQUEST_SUFFIX,
            Self::Reply | Self::Status => REPLY_SUFFIX,
        }
    }

    /// True when this kind belongs to a service rather than a topic.
    #[must_use]
    pub const fn is_service(self) -> bool {
        matches!(self, Self::Request | Self::Reply | Self::Status)
    }
}

/// Which of an action's five endpoints a name belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionEndpoint {
    /// The `send_goal` service.
    SendGoal,
    /// The `cancel_goal` service.
    CancelGoal,
    /// The `get_result` service.
    GetResult,
    /// The `feedback` topic.
    Feedback,
    /// The `status` topic.
    Status,
}

impl ActionEndpoint {
    /// All five, in the order the action protocol uses them.
    pub const ALL: [Self; 5] = [
        Self::SendGoal,
        Self::CancelGoal,
        Self::GetResult,
        Self::Feedback,
        Self::Status,
    ];

    /// The last component of the endpoint's ROS name.
    #[must_use]
    pub const fn leaf(self) -> &'static str {
        match self {
            Self::SendGoal => "send_goal",
            Self::CancelGoal => "cancel_goal",
            Self::GetResult => "get_result",
            Self::Feedback => "feedback",
            Self::Status => "status",
        }
    }

    /// True when this endpoint is a service rather than a topic.
    #[must_use]
    pub const fn is_service(self) -> bool {
        matches!(self, Self::SendGoal | Self::CancelGoal | Self::GetResult)
    }

    /// Parse a leaf name back into an endpoint.
    #[must_use]
    pub fn from_leaf(leaf: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.leaf() == leaf)
    }
}

/// The ROS name of one of an action's five endpoints.
///
/// `action_name("/fibonacci", ActionEndpoint::SendGoal)` is
/// `/fibonacci/_action/send_goal`.
#[must_use]
pub fn action_name(action: &str, endpoint: ActionEndpoint) -> String {
    let action = action.strip_suffix('/').unwrap_or(action);
    format!("{action}{ACTION_INFIX}{}", endpoint.leaf())
}

/// Split an action endpoint's ROS name back into `(action, endpoint)`.
///
/// `None` when the name is not an action endpoint at all.
#[must_use]
pub fn split_action_name(name: &str) -> Option<(&str, ActionEndpoint)> {
    let index = name.rfind(ACTION_INFIX)?;
    let action = name.get(..index)?;
    let leaf = name.get(index.saturating_add(ACTION_INFIX.len())..)?;
    Some((action, ActionEndpoint::from_leaf(leaf)?))
}

/// Mangle a fully-qualified ROS name into its DDS topic name.
///
/// # Errors
///
/// Whatever [`validate_full_name`] reports.
pub fn dds_topic_name(ros_name: &str, kind: TopicKind) -> Result<String, NameFault> {
    validate_full_name(ros_name)?;
    let body = ros_name.strip_prefix('/').unwrap_or(ros_name);
    Ok(format!("{}{body}{}", kind.prefix(), kind.suffix()))
}

/// The reverse of [`dds_topic_name`].
///
/// `None` when the DDS name carries no ROS 2 prefix, or when a service
/// prefix appears without the suffix that must accompany it — a
/// half-mangled name is a peer's bug, not a topic this stack should show in
/// the graph.
#[must_use]
pub fn ros_topic_name(dds_name: &str) -> Option<(TopicKind, String)> {
    for kind in TopicKind::ALL {
        let Some(rest) = dds_name.strip_prefix(kind.prefix()) else {
            continue;
        };
        let body = if kind.suffix().is_empty() {
            rest
        } else {
            rest.strip_suffix(kind.suffix())?
        };
        if body.is_empty() {
            return None;
        }
        return Some((kind, format!("/{body}")));
    }
    None
}

/// True when a DDS topic name belongs to ROS 2 at all.
///
/// The graph topic counts: it is unprefixed by design, not by accident.
#[must_use]
pub fn is_ros_topic(dds_name: &str) -> bool {
    dds_name == GRAPH_TOPIC || ros_topic_name(dds_name).is_some()
}

/// Which IDL sub-namespace a type lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TypeNamespace {
    /// `pkg/msg/Type`.
    Msg,
    /// `pkg/srv/Type`.
    Srv,
    /// `pkg/action/Type`.
    Action,
}

impl TypeNamespace {
    /// Every namespace.
    pub const ALL: [Self; 3] = [Self::Msg, Self::Srv, Self::Action];

    /// The component's spelling in both name forms.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Msg => "msg",
            Self::Srv => "srv",
            Self::Action => "action",
        }
    }

    /// Parse a namespace component.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }
}

/// Mangle `pkg`, a namespace and a type name into the `rosidl` DDS
/// spelling.
///
/// `suffix` is what a service or action adds before the trailing underscore:
/// `"_Request"`, `"_SendGoal_Response"`, `"_FeedbackMessage"`, or `""` for a
/// plain message.
#[must_use]
pub fn dds_type_name(
    package: &str,
    namespace: TypeNamespace,
    type_name: &str,
    suffix: &str,
) -> String {
    format!(
        "{package}::{}::dds_::{type_name}{suffix}_",
        namespace.as_str()
    )
}

/// Render the ROS 2 spelling of a type: `pkg/msg/Type`.
#[must_use]
pub fn ros_type_name(package: &str, namespace: TypeNamespace, type_name: &str) -> String {
    format!("{package}/{}/{type_name}", namespace.as_str())
}

/// Turn a DDS type name back into the ROS 2 spelling.
///
/// `rcl_interfaces::msg::dds_::Log_` becomes `rcl_interfaces/msg/Log`. A
/// service's `_Request`/`_Response` survives into the ROS spelling —
/// `..._Request_` becomes `pkg/srv/Foo_Request` — because that is the name
/// `rosidl` itself gives the generated message, and dropping it would make
/// two distinct types collide.
#[must_use]
pub fn demangle_type_name(dds_type: &str) -> Option<String> {
    let stripped = dds_type.strip_suffix('_')?;
    let mut parts = stripped.split("::");
    let package = parts.next()?;
    let namespace = TypeNamespace::parse(parts.next()?)?;
    if parts.next()? != "dds_" {
        return None;
    }
    let type_name = parts.next()?;
    if parts.next().is_some() || package.is_empty() || type_name.is_empty() {
        return None;
    }
    Some(ros_type_name(package, namespace, type_name))
}

/// Turn a ROS 2 type spelling into the DDS one.
///
/// `sensor_msgs/msg/LaserScan` becomes
/// `sensor_msgs::msg::dds_::LaserScan_`. A two-component spelling
/// (`std_msgs/String`, which older manifests and `ros2 topic pub` both
/// accept) is read as a message.
#[must_use]
pub fn mangle_type_name(ros_type: &str) -> Option<String> {
    let parts: Vec<&str> = ros_type.split('/').collect();
    let (package, namespace, type_name) = match parts.as_slice() {
        [package, type_name] => (*package, TypeNamespace::Msg, *type_name),
        [package, namespace, type_name] => (*package, TypeNamespace::parse(namespace)?, *type_name),
        _ => return None,
    };
    if package.is_empty() || type_name.is_empty() {
        return None;
    }
    Some(dds_type_name(package, namespace, type_name, ""))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_topic_gets_the_rt_prefix_and_loses_its_leading_slash() {
        assert_eq!(
            dds_topic_name("/chatter", TopicKind::Topic).unwrap(),
            "rt/chatter"
        );
        assert_eq!(
            dds_topic_name("/robot/sensors/scan", TopicKind::Topic).unwrap(),
            "rt/robot/sensors/scan"
        );
    }

    #[test]
    fn a_service_gets_both_a_prefix_and_a_suffix() {
        assert_eq!(
            dds_topic_name("/add_two_ints", TopicKind::Request).unwrap(),
            "rq/add_two_intsRequest"
        );
        assert_eq!(
            dds_topic_name("/add_two_ints", TopicKind::Reply).unwrap(),
            "rr/add_two_intsReply"
        );
        assert_eq!(
            dds_topic_name("/add_two_ints", TopicKind::Status).unwrap(),
            "rs/add_two_intsReply"
        );
    }

    #[test]
    fn mangling_demands_a_fully_qualified_name() {
        assert_eq!(
            dds_topic_name("chatter", TopicKind::Topic),
            Err(NameFault::NotAbsolute)
        );
        assert_eq!(
            dds_topic_name("/~/chatter", TopicKind::Topic),
            Err(NameFault::MisplacedTilde { offset: 1 })
        );
    }

    #[test]
    fn every_kind_round_trips() {
        for kind in TopicKind::ALL {
            let dds = dds_topic_name("/robot/thing", kind).unwrap();
            let (back_kind, back_name) = ros_topic_name(&dds).expect("round trip");
            assert_eq!(back_name, "/robot/thing", "{kind:?}");
            if kind == TopicKind::Status {
                // `rs/` and `rr/` share the `Reply` suffix but not the prefix,
                // so the prefix is what distinguishes them and the round trip
                // is exact.
                assert_eq!(back_kind, TopicKind::Status);
            } else {
                assert_eq!(back_kind, kind);
            }
        }
    }

    #[test]
    fn an_unprefixed_name_is_not_a_ros_topic_except_the_graph_one() {
        assert_eq!(ros_topic_name("DCPSParticipant"), None);
        assert!(!is_ros_topic("DCPSParticipant"));
        assert!(is_ros_topic(GRAPH_TOPIC));
        assert_eq!(
            ros_topic_name(GRAPH_TOPIC),
            None,
            "the graph topic has no ROS name because it has no prefix"
        );
    }

    #[test]
    fn a_service_prefix_without_its_suffix_is_rejected() {
        assert_eq!(
            ros_topic_name("rq/add_two_ints"),
            None,
            "a request topic must end in `Request`"
        );
        assert_eq!(ros_topic_name("rr/add_two_ints"), None);
        assert_eq!(ros_topic_name("rt/"), None, "and the body cannot be empty");
        assert_eq!(ros_topic_name("rq/Request"), None);
    }

    #[test]
    fn the_action_quintet_derives_from_one_name() {
        assert_eq!(
            action_name("/fibonacci", ActionEndpoint::SendGoal),
            "/fibonacci/_action/send_goal"
        );
        assert_eq!(
            action_name("/fibonacci", ActionEndpoint::CancelGoal),
            "/fibonacci/_action/cancel_goal"
        );
        assert_eq!(
            action_name("/fibonacci", ActionEndpoint::GetResult),
            "/fibonacci/_action/get_result"
        );
        assert_eq!(
            action_name("/fibonacci", ActionEndpoint::Feedback),
            "/fibonacci/_action/feedback"
        );
        assert_eq!(
            action_name("/fibonacci", ActionEndpoint::Status),
            "/fibonacci/_action/status"
        );
    }

    #[test]
    fn three_of_the_five_are_services() {
        let services: Vec<&str> = ActionEndpoint::ALL
            .into_iter()
            .filter(|endpoint| endpoint.is_service())
            .map(ActionEndpoint::leaf)
            .collect();
        assert_eq!(services, vec!["send_goal", "cancel_goal", "get_result"]);
    }

    #[test]
    fn action_names_split_back_apart() {
        for endpoint in ActionEndpoint::ALL {
            let name = action_name("/robot/fibonacci", endpoint);
            assert_eq!(
                split_action_name(&name),
                Some(("/robot/fibonacci", endpoint))
            );
        }
        assert_eq!(split_action_name("/robot/fibonacci"), None);
        assert_eq!(split_action_name("/fibonacci/_action/bogus"), None);
    }

    #[test]
    fn an_action_name_with_a_trailing_slash_does_not_double_it() {
        assert_eq!(
            action_name("/fibonacci/", ActionEndpoint::Status),
            "/fibonacci/_action/status"
        );
    }

    #[test]
    fn type_names_mangle_the_rosidl_way() {
        assert_eq!(
            dds_type_name("std_msgs", TypeNamespace::Msg, "String", ""),
            "std_msgs::msg::dds_::String_"
        );
        assert_eq!(
            dds_type_name("std_srvs", TypeNamespace::Srv, "Trigger", "_Request"),
            "std_srvs::srv::dds_::Trigger_Request_"
        );
        assert_eq!(
            dds_type_name(
                "example_interfaces",
                TypeNamespace::Action,
                "Fibonacci",
                "_SendGoal_Request"
            ),
            "example_interfaces::action::dds_::Fibonacci_SendGoal_Request_"
        );
    }

    #[test]
    fn the_generated_types_agree_with_this_mangler() {
        use crate::interfaces::rcl_interfaces::Log;
        use crate::interfaces::unique_identifier_msgs::UUID;
        assert_eq!(
            Log::DDS_TYPE_NAME,
            dds_type_name("rcl_interfaces", TypeNamespace::Msg, "Log", "")
        );
        assert_eq!(
            UUID::DDS_TYPE_NAME,
            dds_type_name("unique_identifier_msgs", TypeNamespace::Msg, "UUID", "")
        );
    }

    #[test]
    fn the_generated_action_types_agree_with_this_mangler() {
        use crate::interfaces::example_interfaces::{
            FibonacciFeedbackMessage, FibonacciGetResultResponse, FibonacciSendGoalRequest,
        };
        assert_eq!(
            FibonacciSendGoalRequest::DDS_TYPE_NAME,
            dds_type_name(
                "example_interfaces",
                TypeNamespace::Action,
                "Fibonacci",
                "_SendGoal_Request"
            )
        );
        assert_eq!(
            FibonacciGetResultResponse::DDS_TYPE_NAME,
            dds_type_name(
                "example_interfaces",
                TypeNamespace::Action,
                "Fibonacci",
                "_GetResult_Response"
            )
        );
        assert_eq!(
            FibonacciFeedbackMessage::DDS_TYPE_NAME,
            dds_type_name(
                "example_interfaces",
                TypeNamespace::Action,
                "Fibonacci",
                "_FeedbackMessage"
            )
        );
    }

    #[test]
    fn type_names_demangle_back() {
        assert_eq!(
            demangle_type_name("std_msgs::msg::dds_::String_").as_deref(),
            Some("std_msgs/msg/String")
        );
        assert_eq!(
            demangle_type_name("std_srvs::srv::dds_::Trigger_Request_").as_deref(),
            Some("std_srvs/srv/Trigger_Request")
        );
        assert_eq!(demangle_type_name("std_msgs::msg::String_"), None);
        assert_eq!(demangle_type_name("std_msgs::msg::dds_::String"), None);
        assert_eq!(demangle_type_name("::msg::dds_::String_"), None);
        assert_eq!(
            demangle_type_name("a::b::dds_::C_"),
            None,
            "b is no namespace"
        );
    }

    #[test]
    fn the_ros_type_spelling_mangles_both_ways() {
        assert_eq!(
            mangle_type_name("sensor_msgs/msg/LaserScan").as_deref(),
            Some("sensor_msgs::msg::dds_::LaserScan_")
        );
        assert_eq!(
            mangle_type_name("std_msgs/String").as_deref(),
            Some("std_msgs::msg::dds_::String_"),
            "the two-component spelling means `msg`"
        );
        assert_eq!(mangle_type_name("LaserScan"), None);
        assert_eq!(mangle_type_name("a/b/c/d"), None);
        assert_eq!(mangle_type_name("sensor_msgs/bogus/LaserScan"), None);
        assert_eq!(mangle_type_name("/msg/LaserScan"), None);
    }

    #[test]
    fn every_ros_type_name_round_trips_through_the_dds_form() {
        for namespace in TypeNamespace::ALL {
            let ros = ros_type_name("some_pkg", namespace, "SomeType");
            let dds = mangle_type_name(&ros).expect("manglable");
            assert_eq!(demangle_type_name(&dds).as_deref(), Some(ros.as_str()));
        }
    }

    #[test]
    fn the_kind_table_is_self_consistent() {
        assert!(!TopicKind::Topic.is_service());
        assert!(TopicKind::Request.is_service());
        assert!(TopicKind::Reply.is_service());
        assert!(TopicKind::Status.is_service());
        assert!(TopicKind::Topic.suffix().is_empty());
        for kind in TopicKind::ALL {
            assert_eq!(kind.prefix().len(), 3, "{kind:?}");
            assert!(kind.prefix().ends_with('/'), "{kind:?}");
        }
    }
}
