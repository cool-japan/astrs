//! [`ActionClient`] and [`ClientGoalHandle`]: sending a goal, following it,
//! and getting the outcome.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::error::{Ros2Error, Ros2Result};
use crate::msg::action_msgs;
use crate::names::{ActionEndpoint, FullName};
use crate::node::Ros2Context;
use crate::pubsub::{RawSubscription, Subscription};
use crate::service::{CancelGoal, RequestId, ServiceClient};
use crate::time::RosTime;
use crate::types::{ActionType, GetResultService, SendGoalService};

use super::ActionQos;
use super::goal::{CancelOutcome, CancelRequest, GoalStatus, GoalUuid};

/// An action client: three service clients and two subscriptions.
///
/// # Two demultiplexers
///
/// The feedback and status topics are shared by every client of the action,
/// and feedback carries a goal id rather than a client id — so a client with
/// two goals in flight receives both streams interleaved on one
/// subscription. One background task per client sorts feedback into a queue
/// per goal and keeps the latest status array, so
/// [`ClientGoalHandle::next_feedback`] and [`ClientGoalHandle::status`] can
/// each answer about *their* goal.
///
/// Feedback for a goal this client did not send is discarded rather than
/// queued: another client's goal is not this one's business, and queueing it
/// would grow without bound.
#[derive(Debug)]
pub struct ActionClient<A: ActionType> {
    inner: Arc<ClientInner<A>>,
}

impl<A: ActionType> Clone for ActionClient<A> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[derive(Debug)]
struct ClientInner<A: ActionType> {
    context: Arc<Ros2Context>,
    action: FullName,
    goal_client: ServiceClient<SendGoalService<A>>,
    result_client: ServiceClient<GetResultService<A>>,
    cancel_client: ServiceClient<CancelGoal>,
    feedback: RawSubscription,
    status: Subscription<action_msgs::GoalStatusArray>,
    inbox: FeedbackInbox<A>,
    latest_status: Mutex<BTreeMap<GoalUuid, GoalStatus>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl<A: ActionType> Drop for ClientInner<A> {
    fn drop(&mut self) {
        if let Ok(mut tasks) = self.tasks.try_lock() {
            for task in tasks.drain(..) {
                task.abort();
            }
        }
    }
}

/// Feedback that has arrived, per goal.
#[derive(Debug)]
struct FeedbackInbox<A: ActionType> {
    queues: Mutex<BTreeMap<GoalUuid, VecDeque<A::Feedback>>>,
    arrived: Notify,
}

impl<A: ActionType> Default for FeedbackInbox<A> {
    fn default() -> Self {
        Self {
            queues: Mutex::new(BTreeMap::new()),
            arrived: Notify::new(),
        }
    }
}

impl<A: ActionType> FeedbackInbox<A> {
    /// Start following `goal`, so its feedback is kept rather than
    /// discarded.
    async fn follow(&self, goal: GoalUuid) {
        self.queues.lock().await.entry(goal).or_default();
    }

    /// Stop following `goal` and drop whatever is queued for it.
    async fn forget(&self, goal: GoalUuid) {
        self.queues.lock().await.remove(&goal);
    }

    /// Queue one feedback value, if the goal is being followed.
    async fn deliver(&self, goal: GoalUuid, feedback: A::Feedback) {
        let mut queues = self.queues.lock().await;
        if let Some(queue) = queues.get_mut(&goal) {
            queue.push_back(feedback);
            drop(queues);
            self.arrived.notify_waiters();
        }
    }

    /// Take the oldest queued feedback for `goal`.
    async fn take(&self, goal: GoalUuid) -> Option<A::Feedback> {
        self.queues
            .lock()
            .await
            .get_mut(&goal)
            .and_then(VecDeque::pop_front)
    }
}

impl<A: ActionType> ActionClient<A> {
    /// Create all five endpoints and start the demultiplexers.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`] when an endpoint cannot be created, and
    /// [`Ros2Error::NameTooLong`] when a derived endpoint name overflows.
    pub async fn new(
        context: Arc<Ros2Context>,
        action: FullName,
        qos: ActionQos,
        owner: Option<String>,
    ) -> Ros2Result<Self> {
        let goal_client = ServiceClient::<SendGoalService<A>>::new(
            Arc::clone(&context),
            action.action_endpoint(ActionEndpoint::SendGoal)?,
            qos.goal_service,
            owner.clone(),
        )
        .await?;
        let result_client = ServiceClient::<GetResultService<A>>::new(
            Arc::clone(&context),
            action.action_endpoint(ActionEndpoint::GetResult)?,
            qos.result_service,
            owner.clone(),
        )
        .await?;
        let cancel_client = ServiceClient::<CancelGoal>::new(
            Arc::clone(&context),
            action.action_endpoint(ActionEndpoint::CancelGoal)?,
            qos.cancel_service,
            owner.clone(),
        )
        .await?;
        let feedback = RawSubscription::owned_by(
            Arc::clone(&context),
            action.action_endpoint(ActionEndpoint::Feedback)?,
            A::FEEDBACK_DDS_NAME,
            qos.feedback,
            owner.clone(),
        )
        .await?;
        let status = Subscription::<action_msgs::GoalStatusArray>::new(
            Arc::clone(&context),
            action.action_endpoint(ActionEndpoint::Status)?,
            qos.status,
            owner,
        )
        .await?;

        let inner = Arc::new(ClientInner {
            context,
            action,
            goal_client,
            result_client,
            cancel_client,
            feedback,
            status,
            inbox: FeedbackInbox::default(),
            latest_status: Mutex::new(BTreeMap::new()),
            tasks: Mutex::new(Vec::new()),
        });

        let feedback_task = tokio::spawn(pump_feedback(Arc::clone(&inner)));
        let status_task = tokio::spawn(pump_status(Arc::clone(&inner)));
        inner
            .tasks
            .lock()
            .await
            .extend([feedback_task, status_task]);

        Ok(Self { inner })
    }

    /// The fully-qualified action name.
    #[must_use]
    pub fn action(&self) -> &FullName {
        &self.inner.action
    }

    /// True when a server has been discovered on every endpoint.
    pub async fn is_available(&self) -> bool {
        self.inner.goal_client.is_available().await
            && self.inner.result_client.is_available().await
            && self.inner.cancel_client.is_available().await
    }

    /// Wait until a server has been discovered on every endpoint.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::ServiceUnavailable`] naming the action.
    pub async fn wait_for_server(&self, timeout: StdDuration) -> Ros2Result<()> {
        crate::pubsub::wait_for(
            &self.inner.context,
            timeout,
            "waiting for an action server",
            || async { self.is_available().await },
        )
        .await
        .map_err(|error| match error {
            Ros2Error::Timeout { .. } => Ros2Error::ServiceUnavailable {
                name: self.inner.action.as_str().to_owned(),
            },
            other => other,
        })
    }

    /// Send a goal and wait for the server to accept or reject it.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::GoalRejected`] when the server refuses, plus whatever
    /// the underlying service call reports.
    pub async fn send_goal(
        &self,
        goal: A::Goal,
        timeout: StdDuration,
    ) -> Ros2Result<ClientGoalHandle<A>> {
        self.send_goal_with_id(GoalUuid::generate(), goal, timeout)
            .await
    }

    /// Send a goal under an identifier the caller chose.
    ///
    /// # Errors
    ///
    /// As [`send_goal`](Self::send_goal).
    pub async fn send_goal_with_id(
        &self,
        id: GoalUuid,
        goal: A::Goal,
        timeout: StdDuration,
    ) -> Ros2Result<ClientGoalHandle<A>> {
        // Follow the goal *before* sending it: feedback can arrive between
        // the server accepting the goal and this client hearing the answer,
        // and feedback for an unfollowed goal is discarded.
        self.inner.inbox.follow(id).await;

        let request = A::make_send_goal_request(id.to_message(), goal);
        let response = match self.inner.goal_client.call(&request, timeout).await {
            Ok(response) => response,
            Err(error) => {
                self.inner.inbox.forget(id).await;
                return Err(error);
            }
        };
        let (accepted, stamp) = A::split_send_goal_response(&response);
        if !accepted {
            self.inner.inbox.forget(id).await;
            return Err(Ros2Error::GoalRejected {
                goal: id.to_string(),
            });
        }

        // The `get_result` request goes out immediately, exactly as
        // `rclcpp` does: the answer is the goal's outcome, and asking now
        // means the server has somewhere to send it the moment the goal
        // finishes.
        let result_request = A::make_get_result_request(id.to_message());
        let result_id = self
            .inner
            .result_client
            .send_request(&result_request)
            .await?;

        Ok(ClientGoalHandle {
            client: self.clone(),
            id,
            accepted_at: RosTime::from_message(&stamp),
            result_request: result_id,
        })
    }

    /// Ask the server to cancel goals, in any of the four `CancelGoal`
    /// forms.
    ///
    /// # Errors
    ///
    /// Whatever the underlying service call reports.
    pub async fn cancel(
        &self,
        request: CancelRequest,
        timeout: StdDuration,
    ) -> Ros2Result<CancelOutcome> {
        let body = action_msgs::CancelGoalRequest {
            goal_info: request.to_goal_info(),
        };
        let response = self.inner.cancel_client.call(&body, timeout).await?;
        Ok(CancelOutcome::from_message(&response))
    }

    /// The latest status the server announced for `id`.
    pub async fn status_of(&self, id: GoalUuid) -> Option<GoalStatus> {
        self.inner.latest_status.lock().await.get(&id).copied()
    }

    /// Stop the demultiplexers and delete every endpoint.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`] or [`Ros2Error::Cdr`].
    pub async fn shutdown(&self) -> Ros2Result<()> {
        for task in self.inner.tasks.lock().await.drain(..) {
            task.abort();
        }
        self.inner.goal_client.destroy().await?;
        self.inner.result_client.destroy().await?;
        self.inner.cancel_client.destroy().await?;
        self.inner.feedback.destroy().await?;
        self.inner.status.destroy().await?;
        Ok(())
    }
}

/// One goal this client sent, and everything that can be asked about it.
#[derive(Debug)]
pub struct ClientGoalHandle<A: ActionType> {
    client: ActionClient<A>,
    id: GoalUuid,
    accepted_at: RosTime,
    result_request: RequestId,
}

impl<A: ActionType> ClientGoalHandle<A> {
    /// The goal's identifier.
    #[must_use]
    pub const fn id(&self) -> GoalUuid {
        self.id
    }

    /// When the server said it accepted the goal.
    #[must_use]
    pub const fn accepted_at(&self) -> RosTime {
        self.accepted_at
    }

    /// The latest status the server announced.
    pub async fn status(&self) -> Option<GoalStatus> {
        self.client.status_of(self.id).await
    }

    /// Take the next feedback message for this goal.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Timeout`] when none arrives in time.
    pub async fn next_feedback(&self, timeout: StdDuration) -> Ros2Result<A::Feedback> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(feedback) = self.client.inner.inbox.take(self.id).await {
                return Ok(feedback);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(crate::pubsub::timed_out("action feedback", timeout));
            }
            if tokio::time::timeout(remaining, self.client.inner.inbox.arrived.notified())
                .await
                .is_err()
                && self.client.inner.inbox.take(self.id).await.is_none()
            {
                return Err(crate::pubsub::timed_out("action feedback", timeout));
            }
        }
    }

    /// Wait for the goal's outcome.
    ///
    /// The `get_result` request was sent when the goal was accepted; this
    /// waits for its reply.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Timeout`] when the goal does not finish in time, plus
    /// whatever the underlying service reports.
    pub async fn result(&self, timeout: StdDuration) -> Ros2Result<(GoalStatus, A::Result)> {
        let response = self
            .client
            .inner
            .result_client
            .take_response(self.result_request, timeout)
            .await?;
        let (code, result) = A::split_get_result_response(response);
        Ok((GoalStatus::from_code(code), result))
    }

    /// Ask the server to cancel this goal.
    ///
    /// # Errors
    ///
    /// Whatever the underlying service call reports.
    pub async fn cancel(&self, timeout: StdDuration) -> Ros2Result<CancelOutcome> {
        self.client
            .cancel(CancelRequest::One { goal: self.id }, timeout)
            .await
    }

    /// Stop following this goal, dropping any queued feedback.
    ///
    /// Called automatically when the handle is dropped is *not* possible —
    /// forgetting needs to await — so a long-lived client that sends many
    /// goals should call it once each goal is done.
    pub async fn release(&self) {
        self.client.inner.inbox.forget(self.id).await;
    }
}

/// The feedback demultiplexer.
async fn pump_feedback<A: ActionType>(inner: Arc<ClientInner<A>>) {
    loop {
        let (payload, _) = inner.feedback.recv().await;
        match astrs_cdr::from_bytes::<A::FeedbackMessage>(&payload) {
            Ok(message) => {
                let (id, feedback) = A::split_feedback_message(message);
                inner
                    .inbox
                    .deliver(GoalUuid::from_message(&id), feedback)
                    .await;
            }
            Err(error) => tracing::debug!(
                action = %inner.action,
                %error,
                "dropping feedback that would not decode"
            ),
        }
    }
}

/// The status-array follower.
async fn pump_status<A: ActionType>(inner: Arc<ClientInner<A>>) {
    loop {
        match inner.status.recv().await {
            Ok((array, _)) => {
                let mut latest = inner.latest_status.lock().await;
                for entry in &array.status_list {
                    latest.insert(
                        GoalUuid::from_message(&entry.goal_info.goal_id),
                        GoalStatus::from_code(entry.status),
                    );
                }
            }
            Err(error) => tracing::debug!(
                action = %inner.action,
                %error,
                "dropping a status array that would not decode"
            ),
        }
    }
}
