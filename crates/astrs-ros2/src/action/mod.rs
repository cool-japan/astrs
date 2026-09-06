//! Actions: the goal/feedback/result protocol, over the standard quintet.
//!
//! An action is not a wire primitive. It is five endpoints on names derived
//! from one, three of them services and two of them topics
//! (<https://design.ros2.org/articles/actions.html>):
//!
//! ```text
//!   <action>/_action/send_goal     service  <Action>_SendGoal
//!   <action>/_action/cancel_goal   service  action_msgs/srv/CancelGoal
//!   <action>/_action/get_result    service  <Action>_GetResult
//!   <action>/_action/feedback      topic    <Action>_FeedbackMessage
//!   <action>/_action/status        topic    action_msgs/msg/GoalStatusArray
//! ```
//!
//! # The sequence
//!
//! ```text
//!   client                                       server
//!     │  send_goal(goal_id, goal)  ───────────────▶  │
//!     │  ◀────────────  (accepted, stamp)            │  ACCEPTED
//!     │  get_result(goal_id)       ───────────────▶  │  … parked …
//!     │                                              │  EXECUTING
//!     │  ◀────────────  feedback (topic)             │
//!     │  ◀────────────  status array (topic)         │
//!     │  cancel_goal(…)            ───────────────▶  │  CANCELING
//!     │  ◀────────────  (goals canceling)            │
//!     │  ◀────────────  (status, result)             │  terminal
//! ```
//!
//! The `get_result` request goes out **when the goal is accepted**, not when
//! the client wants the answer — that is what makes the result arrive the
//! instant the goal finishes rather than one polling interval later. It is
//! also why this crate's service server separates taking a request from
//! answering it (see [`crate::service`]): a callback-shaped service API
//! cannot express a reply that is minutes away.
//!
//! # What each side owns
//!
//! | | [`ActionServer`] | [`ActionClient`] |
//! |---|---|---|
//! | goal lifecycle | the registry and the state machine | the latest status the server announced |
//! | feedback | publishes | demultiplexes by goal id |
//! | cancel | applies the four request forms | sends them |
//! | result | answers every parked request on termination | one outstanding request per goal |
//!
//! # QoS
//!
//! [`ActionQos`] carries the five profiles. The one that is not the default
//! is `status`: transient-local, so a client that attaches after a goal has
//! already finished still learns what happened to it.
//!
//! # Example
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use std::time::Duration;
//! use astrs_ros2::action::{ActionClient, ActionQos};
//! use astrs_ros2::msg::example_interfaces::FibonacciGoal;
//! use astrs_ros2::names::FullName;
//! use astrs_ros2::node::{ContextOptions, Ros2Context};
//!
//! # async fn example() -> astrs_ros2::Ros2Result<()> {
//! # astrs_ros2::ros_action! {
//! #     pub struct Fibonacci: "example_interfaces/action/Fibonacci" {
//! #         goal: astrs_ros2::msg::example_interfaces::FibonacciGoal,
//! #         result: astrs_ros2::msg::example_interfaces::FibonacciResult,
//! #         feedback: astrs_ros2::msg::example_interfaces::FibonacciFeedback,
//! #         send_goal_request: astrs_ros2::msg::example_interfaces::FibonacciSendGoalRequest,
//! #         send_goal_response: astrs_ros2::msg::example_interfaces::FibonacciSendGoalResponse,
//! #         get_result_request: astrs_ros2::msg::example_interfaces::FibonacciGetResultRequest,
//! #         get_result_response: astrs_ros2::msg::example_interfaces::FibonacciGetResultResponse,
//! #         feedback_message: astrs_ros2::msg::example_interfaces::FibonacciFeedbackMessage,
//! #     }
//! # }
//! let context = Ros2Context::new(ContextOptions::default()).await?;
//! let client = ActionClient::<Fibonacci>::new(
//!     Arc::clone(&context),
//!     FullName::action("/fibonacci")?,
//!     ActionQos::default(),
//!     None,
//! )
//! .await?;
//!
//! client.wait_for_server(Duration::from_secs(5)).await?;
//! let goal = client
//!     .send_goal(FibonacciGoal { order: 10 }, Duration::from_secs(5))
//!     .await?;
//! let (status, result) = goal.result(Duration::from_secs(30)).await?;
//! println!("{status}: {:?}", result.sequence);
//! # Ok(())
//! # }
//! ```

pub mod client;
pub mod goal;
pub mod server;

pub use client::{ActionClient, ClientGoalHandle};
pub use goal::{
    CancelOutcome, CancelRequest, CancelResponseCode, GOAL_UUID_LEN, GoalStatus, GoalUuid,
};
pub use server::{ActionServer, PendingGoal};

use crate::qos::QosProfile;

/// The five QoS profiles an action's endpoints use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionQos {
    /// The `send_goal` service's profile.
    pub goal_service: QosProfile,
    /// The `cancel_goal` service's profile.
    pub cancel_service: QosProfile,
    /// The `get_result` service's profile.
    pub result_service: QosProfile,
    /// The feedback topic's profile.
    pub feedback: QosProfile,
    /// The status topic's profile.
    pub status: QosProfile,
}

impl Default for ActionQos {
    /// `rcl_action`'s defaults: services default everywhere except the
    /// status topic, which is transient-local so a late-joining client
    /// learns what already happened.
    fn default() -> Self {
        Self {
            goal_service: QosProfile::services_default(),
            cancel_service: QosProfile::services_default(),
            result_service: QosProfile::services_default(),
            feedback: QosProfile::default(),
            status: QosProfile::action_status_default(),
        }
    }
}

impl ActionQos {
    /// Replace the feedback topic's profile.
    ///
    /// The one an application tunes: high-rate feedback is routinely
    /// best-effort.
    #[must_use]
    pub const fn with_feedback(mut self, qos: QosProfile) -> Self {
        self.feedback = qos;
        self
    }

    /// Replace the status topic's profile.
    #[must_use]
    pub const fn with_status(mut self, qos: QosProfile) -> Self {
        self.status = qos;
        self
    }

    /// Replace all three service profiles at once.
    #[must_use]
    pub const fn with_services(mut self, qos: QosProfile) -> Self {
        self.goal_service = qos;
        self.cancel_service = qos;
        self.result_service = qos;
        self
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::names::{ActionEndpoint, FullName, TopicKind};

    #[test]
    fn the_status_topic_is_the_only_latched_endpoint() {
        let qos = ActionQos::default();
        assert!(qos.status.is_transient_local());
        assert!(!qos.feedback.is_transient_local());
        assert!(!qos.goal_service.is_transient_local());
        assert!(!qos.cancel_service.is_transient_local());
        assert!(!qos.result_service.is_transient_local());
    }

    #[test]
    fn the_builders_replace_what_they_name() {
        let qos = ActionQos::default()
            .with_feedback(QosProfile::sensor_data())
            .with_status(QosProfile::default())
            .with_services(QosProfile::sensor_data());
        assert_eq!(qos.feedback, QosProfile::sensor_data());
        assert_eq!(qos.status, QosProfile::default());
        assert_eq!(qos.goal_service, QosProfile::sensor_data());
        assert_eq!(qos.result_service, QosProfile::sensor_data());
    }

    #[test]
    fn the_five_endpoint_names_are_derived_from_one() {
        let action = FullName::action("/robot/fibonacci").expect("valid");
        let names: Vec<String> = ActionEndpoint::ALL
            .into_iter()
            .map(|endpoint| {
                action
                    .action_endpoint(endpoint)
                    .expect("derivable")
                    .as_str()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            names,
            vec![
                "/robot/fibonacci/_action/send_goal".to_owned(),
                "/robot/fibonacci/_action/cancel_goal".to_owned(),
                "/robot/fibonacci/_action/get_result".to_owned(),
                "/robot/fibonacci/_action/feedback".to_owned(),
                "/robot/fibonacci/_action/status".to_owned(),
            ]
        );
    }

    #[test]
    fn the_service_endpoints_mangle_to_rq_and_rr() {
        let action = FullName::action("/fibonacci").expect("valid");
        let send_goal = action
            .action_endpoint(ActionEndpoint::SendGoal)
            .expect("derivable");
        assert_eq!(
            send_goal.dds_name(TopicKind::Request),
            "rq/fibonacci/_action/send_goalRequest"
        );
        assert_eq!(
            send_goal.dds_name(TopicKind::Reply),
            "rr/fibonacci/_action/send_goalReply"
        );

        let feedback = action
            .action_endpoint(ActionEndpoint::Feedback)
            .expect("derivable");
        assert_eq!(
            feedback.dds_name(TopicKind::Topic),
            "rt/fibonacci/_action/feedback"
        );
    }
}
