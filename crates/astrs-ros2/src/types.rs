//! The three traits that give a generated Rust type its ROS identity, and
//! the macros that implement them.
//!
//! `astrs-idl` emits a plain struct per ROS type, with two inherent
//! constants — `ROS_TYPE_NAME` and `DDS_TYPE_NAME` — and the
//! [`CdrSerde`] pair. That is everything a *message*
//! needs and nothing a *generic* publisher can use: `Publisher<T>` has to
//! ask `T` for its DDS type name at compile time, and an inherent constant
//! is not something a trait bound can name.
//!
//! So this module defines three traits and three macros:
//!
//! | Trait | Implemented by | Gives |
//! |---|---|---|
//! | [`MessageType`] | one `.msg` type | the topic's type name |
//! | [`ServiceType`] | a marker per `.srv` | the request/response pair and both type names |
//! | [`ActionType`] | a marker per `.action` | the eight wire types and the five endpoints' names |
//!
//! # Why the trait constants are spelled differently
//!
//! [`MessageType::TYPE_NAME`], not `ROS_TYPE_NAME`: a generated type
//! already has an inherent `ROS_TYPE_NAME`, and an impl body that wrote
//! `const ROS_TYPE_NAME: &str = Self::ROS_TYPE_NAME;` would be reading
//! itself. The different spelling makes
//! [`ros_message!`](crate::ros_message) a one-liner that cannot
//! accidentally recurse.
//!
//! # Why an action declares no service markers
//!
//! An action's `send_goal` and `get_result` endpoints are services, and the
//! service client and server machinery is generic over [`ServiceType`]. It
//! would be natural for [`ros_action!`](crate::ros_action) to emit a marker
//! struct for each — and wrong, because two actions declared in one module
//! would collide on the generated names. [`SendGoalService`] and
//! [`GetResultService`] are generic adapters instead: one blanket impl each,
//! no generated names, and the `cancel_goal` endpoint needs nothing at all
//! because every action's cancel service is the same
//! `action_msgs/srv/CancelGoal`.
//!
//! # Example
//!
//! ```
//! use astrs_ros2::MessageType;
//! use astrs_ros2::interfaces::rcl_interfaces::Log;
//!
//! assert_eq!(<Log as MessageType>::TYPE_NAME, "rcl_interfaces/msg/Log");
//! assert_eq!(
//!     <Log as MessageType>::DDS_NAME,
//!     "rcl_interfaces::msg::dds_::Log_"
//! );
//! ```

use core::marker::PhantomData;

use astrs_cdr::CdrSerde;

use crate::idl::generated::builtin_interfaces;
use crate::interfaces::unique_identifier_msgs::UUID;

/// A ROS 2 message type: something a topic can carry.
///
/// Implemented for every generated `.msg` type through
/// [`ros_message!`](crate::ros_message).
pub trait MessageType: CdrSerde + Send + Sync + 'static {
    /// The ROS 2 spelling: `pkg/msg/Type`.
    const TYPE_NAME: &'static str;
    /// The DDS spelling: `pkg::msg::dds_::Type_`.
    const DDS_NAME: &'static str;
}

/// A ROS 2 service type: a request/response pair with two DDS type names.
///
/// Implemented for a marker struct per `.srv` file through
/// [`ros_service!`](crate::ros_service) — a marker rather than one of the
/// two message types, because neither half of a service is the service.
pub trait ServiceType: Send + Sync + 'static {
    /// What a client sends.
    type Request: CdrSerde + Send + Sync + 'static;
    /// What a server answers with.
    type Response: CdrSerde + Send + Sync + 'static;

    /// The ROS 2 spelling: `pkg/srv/Service`.
    const TYPE_NAME: &'static str;
    /// The request half's DDS type name.
    const REQUEST_DDS_NAME: &'static str;
    /// The reply half's DDS type name.
    const RESPONSE_DDS_NAME: &'static str;
}

/// A ROS 2 action type: three declared types plus the five synthesized wire
/// types the action protocol runs on.
///
/// Implemented for a marker struct per `.action` file through
/// [`ros_action!`](crate::ros_action). The `make_*` and `split_*` functions
/// are the seam that keeps [`crate::action`] free of any knowledge of a
/// particular action's field names: `astrs-idl` generates
/// `FibonacciSendGoalRequest { goal_id, goal }`, and the macro is what
/// knows that.
pub trait ActionType: Send + Sync + 'static {
    /// The goal the client sends.
    ///
    /// `Clone` is required because one goal is handed to the application
    /// and kept in the server's registry at the same time; every generated
    /// type derives it.
    type Goal: CdrSerde + Clone + core::fmt::Debug + Send + Sync + 'static;
    /// The result the server produces.
    ///
    /// `Clone` because a terminal result answers *every* `get_result`
    /// request parked on that goal, and there may be several; `Default`
    /// because a rejected or canceled goal still has to answer with
    /// something.
    type Result: CdrSerde + Clone + Default + core::fmt::Debug + Send + Sync + 'static;
    /// The feedback the server streams.
    type Feedback: CdrSerde + Clone + core::fmt::Debug + Send + Sync + 'static;

    /// `<Action>_SendGoal_Request`: a goal id and a goal.
    type SendGoalRequest: CdrSerde + Send + Sync + 'static;
    /// `<Action>_SendGoal_Response`: accepted, and when.
    type SendGoalResponse: CdrSerde + Send + Sync + 'static;
    /// `<Action>_GetResult_Request`: a goal id.
    type GetResultRequest: CdrSerde + Send + Sync + 'static;
    /// `<Action>_GetResult_Response`: a status and a result.
    type GetResultResponse: CdrSerde + Send + Sync + 'static;
    /// `<Action>_FeedbackMessage`: a goal id and one feedback value.
    type FeedbackMessage: CdrSerde + Send + Sync + 'static;

    /// The ROS 2 spelling: `pkg/action/Action`.
    const TYPE_NAME: &'static str;
    /// The `send_goal` service's request DDS type name.
    const SEND_GOAL_REQUEST_DDS_NAME: &'static str;
    /// The `send_goal` service's response DDS type name.
    const SEND_GOAL_RESPONSE_DDS_NAME: &'static str;
    /// The `get_result` service's request DDS type name.
    const GET_RESULT_REQUEST_DDS_NAME: &'static str;
    /// The `get_result` service's response DDS type name.
    const GET_RESULT_RESPONSE_DDS_NAME: &'static str;
    /// The feedback topic's DDS type name.
    const FEEDBACK_DDS_NAME: &'static str;

    /// Build a `send_goal` request.
    fn make_send_goal_request(goal_id: UUID, goal: Self::Goal) -> Self::SendGoalRequest;
    /// Take a `send_goal` request apart.
    fn split_send_goal_request(request: Self::SendGoalRequest) -> (UUID, Self::Goal);
    /// Build a `send_goal` response.
    fn make_send_goal_response(
        accepted: bool,
        stamp: builtin_interfaces::Time,
    ) -> Self::SendGoalResponse;
    /// Read a `send_goal` response.
    fn split_send_goal_response(
        response: &Self::SendGoalResponse,
    ) -> (bool, builtin_interfaces::Time);
    /// Build a `get_result` request.
    fn make_get_result_request(goal_id: UUID) -> Self::GetResultRequest;
    /// Read a `get_result` request's goal id.
    fn get_result_request_goal_id(request: &Self::GetResultRequest) -> UUID;
    /// Build a `get_result` response.
    fn make_get_result_response(status: i8, result: Self::Result) -> Self::GetResultResponse;
    /// Take a `get_result` response apart.
    fn split_get_result_response(response: Self::GetResultResponse) -> (i8, Self::Result);
    /// Build a feedback message.
    fn make_feedback_message(goal_id: UUID, feedback: Self::Feedback) -> Self::FeedbackMessage;
    /// Take a feedback message apart.
    fn split_feedback_message(message: Self::FeedbackMessage) -> (UUID, Self::Feedback);
}

/// An action's `send_goal` endpoint, as a [`ServiceType`].
///
/// A zero-sized adapter rather than a generated marker: see this module's
/// docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SendGoalService<A: ActionType>(PhantomData<fn() -> A>);

impl<A: ActionType> ServiceType for SendGoalService<A> {
    type Request = A::SendGoalRequest;
    type Response = A::SendGoalResponse;
    const TYPE_NAME: &'static str = A::TYPE_NAME;
    const REQUEST_DDS_NAME: &'static str = A::SEND_GOAL_REQUEST_DDS_NAME;
    const RESPONSE_DDS_NAME: &'static str = A::SEND_GOAL_RESPONSE_DDS_NAME;
}

/// An action's `get_result` endpoint, as a [`ServiceType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct GetResultService<A: ActionType>(PhantomData<fn() -> A>);

impl<A: ActionType> ServiceType for GetResultService<A> {
    type Request = A::GetResultRequest;
    type Response = A::GetResultResponse;
    const TYPE_NAME: &'static str = A::TYPE_NAME;
    const REQUEST_DDS_NAME: &'static str = A::GET_RESULT_REQUEST_DDS_NAME;
    const RESPONSE_DDS_NAME: &'static str = A::GET_RESULT_RESPONSE_DDS_NAME;
}

/// Implement [`MessageType`] for one or more generated `.msg` types.
///
/// Each type must carry `astrs-idl`'s inherent `ROS_TYPE_NAME` and
/// `DDS_TYPE_NAME` constants, which every generated type does.
///
/// ```
/// use astrs_ros2::MessageType;
/// use astrs_ros2::interfaces::rcl_interfaces::Parameter;
///
/// // `astrs-ros2` already implements it for every bundled type; this is
/// // what that expands to for a type generated in a downstream crate.
/// assert_eq!(<Parameter as MessageType>::TYPE_NAME, "rcl_interfaces/msg/Parameter");
/// ```
#[macro_export]
macro_rules! ros_message {
    ($($path:ty),+ $(,)?) => {
        $(
            impl $crate::MessageType for $path {
                const TYPE_NAME: &'static str = <$path>::ROS_TYPE_NAME;
                const DDS_NAME: &'static str = <$path>::DDS_TYPE_NAME;
            }
        )+
    };
}

/// Declare a marker type for a `.srv` and implement [`ServiceType`] for it.
///
/// ```
/// use astrs_ros2::ServiceType;
/// use astrs_ros2::interfaces::example_interfaces::{AddTwoIntsRequest, AddTwoIntsResponse};
///
/// astrs_ros2::ros_service! {
///     /// `example_interfaces/srv/AddTwoInts`.
///     pub struct AddTwoInts: "example_interfaces/srv/AddTwoInts" {
///         request: AddTwoIntsRequest,
///         response: AddTwoIntsResponse,
///     }
/// }
///
/// assert_eq!(
///     <AddTwoInts as ServiceType>::REQUEST_DDS_NAME,
///     "example_interfaces::srv::dds_::AddTwoInts_Request_"
/// );
/// ```
#[macro_export]
macro_rules! ros_service {
    (
        $(#[$meta:meta])*
        $visibility:vis struct $marker:ident : $type_name:literal {
            request: $request:ty,
            response: $response:ty $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
        $visibility struct $marker;

        impl $crate::ServiceType for $marker {
            type Request = $request;
            type Response = $response;
            const TYPE_NAME: &'static str = $type_name;
            const REQUEST_DDS_NAME: &'static str = <$request>::DDS_TYPE_NAME;
            const RESPONSE_DDS_NAME: &'static str = <$response>::DDS_TYPE_NAME;
        }
    };
}

/// Declare a marker type for an `.action` and implement [`ActionType`] for
/// it.
///
/// Every field of the five synthesized wire types is assigned by name, so
/// this macro is where the layout `astrs-idl`'s `codegen/action.rs` produces
/// is written down on the consuming side. If that layout ever changes, this
/// macro stops compiling — which is the point.
///
/// ```
/// use astrs_ros2::ActionType;
/// use astrs_ros2::interfaces::example_interfaces::{
///     FibonacciFeedback, FibonacciFeedbackMessage, FibonacciGetResultRequest,
///     FibonacciGetResultResponse, FibonacciGoal, FibonacciResult,
///     FibonacciSendGoalRequest, FibonacciSendGoalResponse,
/// };
///
/// astrs_ros2::ros_action! {
///     /// `example_interfaces/action/Fibonacci`.
///     pub struct Fibonacci: "example_interfaces/action/Fibonacci" {
///         goal: FibonacciGoal,
///         result: FibonacciResult,
///         feedback: FibonacciFeedback,
///         send_goal_request: FibonacciSendGoalRequest,
///         send_goal_response: FibonacciSendGoalResponse,
///         get_result_request: FibonacciGetResultRequest,
///         get_result_response: FibonacciGetResultResponse,
///         feedback_message: FibonacciFeedbackMessage,
///     }
/// }
///
/// assert_eq!(
///     <Fibonacci as ActionType>::FEEDBACK_DDS_NAME,
///     "example_interfaces::action::dds_::Fibonacci_FeedbackMessage_"
/// );
/// ```
#[macro_export]
macro_rules! ros_action {
    (
        $(#[$meta:meta])*
        $visibility:vis struct $marker:ident : $type_name:literal {
            goal: $goal:ty,
            result: $result:ty,
            feedback: $feedback:ty,
            send_goal_request: $send_goal_request:ty,
            send_goal_response: $send_goal_response:ty,
            get_result_request: $get_result_request:ty,
            get_result_response: $get_result_response:ty,
            feedback_message: $feedback_message:ty $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
        $visibility struct $marker;

        impl $crate::ActionType for $marker {
            type Goal = $goal;
            type Result = $result;
            type Feedback = $feedback;
            type SendGoalRequest = $send_goal_request;
            type SendGoalResponse = $send_goal_response;
            type GetResultRequest = $get_result_request;
            type GetResultResponse = $get_result_response;
            type FeedbackMessage = $feedback_message;

            const TYPE_NAME: &'static str = $type_name;
            const SEND_GOAL_REQUEST_DDS_NAME: &'static str =
                <$send_goal_request>::DDS_TYPE_NAME;
            const SEND_GOAL_RESPONSE_DDS_NAME: &'static str =
                <$send_goal_response>::DDS_TYPE_NAME;
            const GET_RESULT_REQUEST_DDS_NAME: &'static str =
                <$get_result_request>::DDS_TYPE_NAME;
            const GET_RESULT_RESPONSE_DDS_NAME: &'static str =
                <$get_result_response>::DDS_TYPE_NAME;
            const FEEDBACK_DDS_NAME: &'static str = <$feedback_message>::DDS_TYPE_NAME;

            fn make_send_goal_request(
                goal_id: $crate::interfaces::unique_identifier_msgs::UUID,
                goal: Self::Goal,
            ) -> Self::SendGoalRequest {
                let mut request =
                    <$send_goal_request as ::core::default::Default>::default();
                request.goal_id = goal_id;
                request.goal = goal;
                request
            }

            fn split_send_goal_request(
                request: Self::SendGoalRequest,
            ) -> (
                $crate::interfaces::unique_identifier_msgs::UUID,
                Self::Goal,
            ) {
                (request.goal_id, request.goal)
            }

            fn make_send_goal_response(
                accepted: bool,
                stamp: $crate::idl::generated::builtin_interfaces::Time,
            ) -> Self::SendGoalResponse {
                let mut response =
                    <$send_goal_response as ::core::default::Default>::default();
                response.accepted = accepted;
                response.stamp = stamp;
                response
            }

            fn split_send_goal_response(
                response: &Self::SendGoalResponse,
            ) -> (bool, $crate::idl::generated::builtin_interfaces::Time) {
                (response.accepted, response.stamp.clone())
            }

            fn make_get_result_request(
                goal_id: $crate::interfaces::unique_identifier_msgs::UUID,
            ) -> Self::GetResultRequest {
                let mut request =
                    <$get_result_request as ::core::default::Default>::default();
                request.goal_id = goal_id;
                request
            }

            fn get_result_request_goal_id(
                request: &Self::GetResultRequest,
            ) -> $crate::interfaces::unique_identifier_msgs::UUID {
                request.goal_id.clone()
            }

            fn make_get_result_response(
                status: i8,
                result: Self::Result,
            ) -> Self::GetResultResponse {
                let mut response =
                    <$get_result_response as ::core::default::Default>::default();
                response.status = status;
                response.result = result;
                response
            }

            fn split_get_result_response(
                response: Self::GetResultResponse,
            ) -> (i8, Self::Result) {
                (response.status, response.result)
            }

            fn make_feedback_message(
                goal_id: $crate::interfaces::unique_identifier_msgs::UUID,
                feedback: Self::Feedback,
            ) -> Self::FeedbackMessage {
                let mut message =
                    <$feedback_message as ::core::default::Default>::default();
                message.goal_id = goal_id;
                message.feedback = feedback;
                message
            }

            fn split_feedback_message(
                message: Self::FeedbackMessage,
            ) -> (
                $crate::interfaces::unique_identifier_msgs::UUID,
                Self::Feedback,
            ) {
                (message.goal_id, message.feedback)
            }
        }
    };
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::interfaces::example_interfaces::{
        FibonacciFeedback, FibonacciFeedbackMessage, FibonacciGetResultRequest,
        FibonacciGetResultResponse, FibonacciGoal, FibonacciResult, FibonacciSendGoalRequest,
        FibonacciSendGoalResponse,
    };

    crate::ros_action! {
        /// `example_interfaces/action/Fibonacci`, for this module's tests.
        pub struct Fibonacci: "example_interfaces/action/Fibonacci" {
            goal: FibonacciGoal,
            result: FibonacciResult,
            feedback: FibonacciFeedback,
            send_goal_request: FibonacciSendGoalRequest,
            send_goal_response: FibonacciSendGoalResponse,
            get_result_request: FibonacciGetResultRequest,
            get_result_response: FibonacciGetResultResponse,
            feedback_message: FibonacciFeedbackMessage,
        }
    }

    fn uuid(seed: u8) -> UUID {
        UUID { uuid: [seed; 16] }
    }

    #[test]
    fn a_message_type_reports_both_spellings() {
        use crate::interfaces::rcl_interfaces::ParameterValue;
        assert_eq!(
            <ParameterValue as MessageType>::TYPE_NAME,
            "rcl_interfaces/msg/ParameterValue"
        );
        assert_eq!(
            <ParameterValue as MessageType>::DDS_NAME,
            "rcl_interfaces::msg::dds_::ParameterValue_"
        );
    }

    #[test]
    fn a_service_marker_reports_both_halves() {
        use crate::service::AddTwoInts;
        assert_eq!(
            <AddTwoInts as ServiceType>::TYPE_NAME,
            "example_interfaces/srv/AddTwoInts"
        );
        assert_eq!(
            <AddTwoInts as ServiceType>::REQUEST_DDS_NAME,
            "example_interfaces::srv::dds_::AddTwoInts_Request_"
        );
        assert_eq!(
            <AddTwoInts as ServiceType>::RESPONSE_DDS_NAME,
            "example_interfaces::srv::dds_::AddTwoInts_Response_"
        );
    }

    #[test]
    fn an_action_reports_its_five_endpoints_type_names() {
        assert_eq!(
            <Fibonacci as ActionType>::TYPE_NAME,
            "example_interfaces/action/Fibonacci"
        );
        assert_eq!(
            <Fibonacci as ActionType>::SEND_GOAL_REQUEST_DDS_NAME,
            "example_interfaces::action::dds_::Fibonacci_SendGoal_Request_"
        );
        assert_eq!(
            <Fibonacci as ActionType>::GET_RESULT_RESPONSE_DDS_NAME,
            "example_interfaces::action::dds_::Fibonacci_GetResult_Response_"
        );
        assert_eq!(
            <Fibonacci as ActionType>::FEEDBACK_DDS_NAME,
            "example_interfaces::action::dds_::Fibonacci_FeedbackMessage_"
        );
    }

    #[test]
    fn the_send_goal_wire_types_round_trip_through_the_accessors() {
        let goal = FibonacciGoal { order: 9 };
        let request = Fibonacci::make_send_goal_request(uuid(3), goal.clone());
        let (id, back) = Fibonacci::split_send_goal_request(request);
        assert_eq!(id, uuid(3));
        assert_eq!(back, goal);

        let stamp = builtin_interfaces::Time {
            sec: 5,
            nanosec: 250,
        };
        let response = Fibonacci::make_send_goal_response(true, stamp.clone());
        assert_eq!(
            Fibonacci::split_send_goal_response(&response),
            (true, stamp)
        );
    }

    #[test]
    fn the_get_result_wire_types_round_trip_through_the_accessors() {
        let request = Fibonacci::make_get_result_request(uuid(4));
        assert_eq!(Fibonacci::get_result_request_goal_id(&request), uuid(4));

        let result = FibonacciResult {
            sequence: vec![0, 1, 1, 2],
        };
        let response = Fibonacci::make_get_result_response(4, result.clone());
        assert_eq!(Fibonacci::split_get_result_response(response), (4, result));
    }

    #[test]
    fn the_feedback_message_round_trips_through_the_accessors() {
        let feedback = FibonacciFeedback {
            partial_sequence: vec![0, 1, 1],
        };
        let message = Fibonacci::make_feedback_message(uuid(5), feedback.clone());
        let (id, back) = Fibonacci::split_feedback_message(message);
        assert_eq!(id, uuid(5));
        assert_eq!(back, feedback);
    }

    #[test]
    fn the_generic_service_adapters_carry_the_actions_names() {
        assert_eq!(
            <SendGoalService<Fibonacci> as ServiceType>::REQUEST_DDS_NAME,
            <Fibonacci as ActionType>::SEND_GOAL_REQUEST_DDS_NAME
        );
        assert_eq!(
            <GetResultService<Fibonacci> as ServiceType>::RESPONSE_DDS_NAME,
            <Fibonacci as ActionType>::GET_RESULT_RESPONSE_DDS_NAME
        );
        assert_eq!(
            <SendGoalService<Fibonacci> as ServiceType>::TYPE_NAME,
            "example_interfaces/action/Fibonacci"
        );
    }
}
