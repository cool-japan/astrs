//! Services: request/reply over `rq/` and `rr/`, correlated by sample
//! identity.
//!
//! # What a ROS 2 service is on the wire
//!
//! Two topics and a twenty-four-octet header:
//!
//! ```text
//!   client ──── rq/<service>Request ────▶ server
//!          ◀─── rr/<service>Reply   ─────
//! ```
//!
//! Both topics are shared by every client and every server of the service,
//! so a reply reaches clients that did not ask. [`SampleIdentity`] — the
//! client's request-writer GUID plus a client-local sequence number, echoed
//! verbatim by the server — is what makes a reply belong to a request. Its
//! module documents the layout octet by octet.
//!
//! # The API shape, and why it is not a callback
//!
//! [`ServiceServer::take_request`] hands back a [`RequestId`] with the
//! request, and [`ServiceServer::send_response`] takes it back. The two are
//! separate calls because an action's `get_result` service answers *when
//! the goal finishes*, which may be minutes later; a `fn(Request) ->
//! Response` shape cannot express that without pinning a task open for the
//! whole goal. [`ServiceServer::serve`] is the callback form, built on the
//! general one.
//!
//! # Bundled service declarations
//!
//! The services this crate itself needs are declared here with
//! [`ros_service!`](crate::ros_service): the six parameter services
//! ([`crate::parameters`] uses them), `action_msgs/srv/CancelGoal` (every
//! action's third endpoint), and the three `std_srvs` services plus
//! `example_interfaces/srv/AddTwoInts`, which cost two lines each and save
//! every downstream test from re-declaring them.
//!
//! # Example
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use std::time::Duration;
//! use astrs_ros2::msg::example_interfaces::{AddTwoIntsRequest, AddTwoIntsResponse};
//! use astrs_ros2::node::{ContextOptions, Ros2Context};
//! use astrs_ros2::names::FullName;
//! use astrs_ros2::qos::QosProfile;
//! use astrs_ros2::service::{AddTwoInts, ServiceClient};
//!
//! # async fn example() -> astrs_ros2::Ros2Result<()> {
//! let context = Ros2Context::new(ContextOptions::default()).await?;
//! let client = ServiceClient::<AddTwoInts>::new(
//!     Arc::clone(&context),
//!     FullName::service("/add_two_ints")?,
//!     QosProfile::services_default(),
//!     None,
//! )
//! .await?;
//!
//! let sum = client
//!     .call_when_ready(&AddTwoIntsRequest { a: 2, b: 40 }, Duration::from_secs(5))
//!     .await?
//!     .sum;
//! assert_eq!(sum, 42);
//! # Ok(())
//! # }
//! ```

pub mod client;
pub mod identity;
pub mod server;

pub use client::ServiceClient;
pub use identity::{
    RequestId, SAMPLE_IDENTITY_LEN, SampleIdentity, decode_with_identity, encode_with_identity,
    peek_identity,
};
pub use server::ServiceServer;

use crate::msg::{action_msgs, example_interfaces, rcl_interfaces, std_srvs};

crate::ros_service! {
    /// `rcl_interfaces/srv/GetParameters` — read declared parameters.
    pub struct GetParameters: "rcl_interfaces/srv/GetParameters" {
        request: rcl_interfaces::GetParametersRequest,
        response: rcl_interfaces::GetParametersResponse,
    }
}

crate::ros_service! {
    /// `rcl_interfaces/srv/GetParameterTypes` — read parameter types.
    pub struct GetParameterTypes: "rcl_interfaces/srv/GetParameterTypes" {
        request: rcl_interfaces::GetParameterTypesRequest,
        response: rcl_interfaces::GetParameterTypesResponse,
    }
}

crate::ros_service! {
    /// `rcl_interfaces/srv/SetParameters` — set parameters one at a time.
    pub struct SetParameters: "rcl_interfaces/srv/SetParameters" {
        request: rcl_interfaces::SetParametersRequest,
        response: rcl_interfaces::SetParametersResponse,
    }
}

crate::ros_service! {
    /// `rcl_interfaces/srv/SetParametersAtomically` — set parameters as one
    /// transaction.
    pub struct SetParametersAtomically: "rcl_interfaces/srv/SetParametersAtomically" {
        request: rcl_interfaces::SetParametersAtomicallyRequest,
        response: rcl_interfaces::SetParametersAtomicallyResponse,
    }
}

crate::ros_service! {
    /// `rcl_interfaces/srv/ListParameters` — enumerate parameter names.
    pub struct ListParameters: "rcl_interfaces/srv/ListParameters" {
        request: rcl_interfaces::ListParametersRequest,
        response: rcl_interfaces::ListParametersResponse,
    }
}

crate::ros_service! {
    /// `rcl_interfaces/srv/DescribeParameters` — read parameter descriptors.
    pub struct DescribeParameters: "rcl_interfaces/srv/DescribeParameters" {
        request: rcl_interfaces::DescribeParametersRequest,
        response: rcl_interfaces::DescribeParametersResponse,
    }
}

crate::ros_service! {
    /// `action_msgs/srv/CancelGoal` — every action's third endpoint.
    pub struct CancelGoal: "action_msgs/srv/CancelGoal" {
        request: action_msgs::CancelGoalRequest,
        response: action_msgs::CancelGoalResponse,
    }
}

crate::ros_service! {
    /// `std_srvs/srv/Empty`.
    pub struct Empty: "std_srvs/srv/Empty" {
        request: std_srvs::EmptyRequest,
        response: std_srvs::EmptyResponse,
    }
}

crate::ros_service! {
    /// `std_srvs/srv/SetBool`.
    pub struct SetBool: "std_srvs/srv/SetBool" {
        request: std_srvs::SetBoolRequest,
        response: std_srvs::SetBoolResponse,
    }
}

crate::ros_service! {
    /// `std_srvs/srv/Trigger`.
    pub struct Trigger: "std_srvs/srv/Trigger" {
        request: std_srvs::TriggerRequest,
        response: std_srvs::TriggerResponse,
    }
}

crate::ros_service! {
    /// `example_interfaces/srv/AddTwoInts`.
    pub struct AddTwoInts: "example_interfaces/srv/AddTwoInts" {
        request: example_interfaces::AddTwoIntsRequest,
        response: example_interfaces::AddTwoIntsResponse,
    }
}

/// The six parameter services, in the order `rcl` creates them.
///
/// Named so that [`crate::parameters`] and a graph-introspection test can
/// agree on the set without either one hard-coding six strings.
pub const PARAMETER_SERVICE_NAMES: [&str; 6] = [
    "get_parameters",
    "get_parameter_types",
    "set_parameters",
    "set_parameters_atomically",
    "describe_parameters",
    "list_parameters",
];

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::ServiceType;

    #[test]
    fn every_bundled_service_names_both_halves_consistently() {
        fn check<S: ServiceType>() {
            let prefix = S::TYPE_NAME;
            assert!(
                S::REQUEST_DDS_NAME.ends_with("_Request_"),
                "{prefix}: {}",
                S::REQUEST_DDS_NAME
            );
            assert!(
                S::RESPONSE_DDS_NAME.ends_with("_Response_"),
                "{prefix}: {}",
                S::RESPONSE_DDS_NAME
            );
            let package = prefix.split('/').next().expect("a package");
            assert!(
                S::REQUEST_DDS_NAME.starts_with(package),
                "{prefix} does not live in {package}"
            );
        }

        check::<GetParameters>();
        check::<GetParameterTypes>();
        check::<SetParameters>();
        check::<SetParametersAtomically>();
        check::<ListParameters>();
        check::<DescribeParameters>();
        check::<CancelGoal>();
        check::<Empty>();
        check::<SetBool>();
        check::<Trigger>();
        check::<AddTwoInts>();
    }

    #[test]
    fn the_six_parameter_services_are_the_ones_rcl_creates() {
        assert_eq!(PARAMETER_SERVICE_NAMES.len(), 6);
        let mut sorted = PARAMETER_SERVICE_NAMES;
        sorted.sort_unstable();
        let mut deduped = sorted.to_vec();
        deduped.dedup();
        assert_eq!(deduped.len(), 6, "two entries share a name");
    }

    #[test]
    fn a_service_marker_is_zero_sized() {
        assert_eq!(core::mem::size_of::<AddTwoInts>(), 0);
        // Written generically rather than as `AddTwoInts::default()`
        // (clippy's `default_constructed_unit_structs`, correctly, wants the
        // literal unit value there instead) so this still actually exercises
        // `AddTwoInts: Default` — the property under test, for the marker
        // type real service-registration code bounds on.
        fn default_of<T: Default + PartialEq>(value: T) -> bool {
            value == T::default()
        }
        assert!(default_of(AddTwoInts));
    }

    #[test]
    fn the_cancel_service_is_the_same_for_every_action() {
        assert_eq!(
            <CancelGoal as ServiceType>::REQUEST_DDS_NAME,
            "action_msgs::srv::dds_::CancelGoal_Request_"
        );
        assert_eq!(
            <CancelGoal as ServiceType>::RESPONSE_DDS_NAME,
            "action_msgs::srv::dds_::CancelGoal_Response_"
        );
    }
}
