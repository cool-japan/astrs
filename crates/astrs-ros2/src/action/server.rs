//! [`ActionServer`]: the five endpoints, the goal registry, and the state
//! machine that ties them together.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::error::{Ros2Error, Ros2Result};
use crate::msg::action_msgs;
use crate::names::{ActionEndpoint, FullName};
use crate::node::Ros2Context;
use crate::pubsub::{Publisher, RawPublisher};
use crate::service::{CancelGoal, RequestId, ServiceServer};
use crate::time::RosTime;
use crate::types::{ActionType, GetResultService, SendGoalService};

use super::ActionQos;
use super::goal::{CancelRequest, CancelResponseCode, GoalStatus, GoalUuid};

/// A goal that has arrived and not yet been accepted or rejected.
///
/// Held rather than answered immediately because accepting a goal is an
/// application decision — a server that is already running one goal may
/// refuse a second — and because the answer carries the acceptance
/// timestamp, which is what `CancelGoal`'s "before this time" form is
/// measured against.
#[derive(Debug)]
pub struct PendingGoal<A: ActionType> {
    id: GoalUuid,
    goal: A::Goal,
    request: RequestId,
}

impl<A: ActionType> PendingGoal<A> {
    /// The goal's identifier.
    #[must_use]
    pub const fn id(&self) -> GoalUuid {
        self.id
    }

    /// The goal itself.
    #[must_use]
    pub const fn goal(&self) -> &A::Goal {
        &self.goal
    }

    /// Take the goal out.
    #[must_use]
    pub fn into_goal(self) -> A::Goal {
        self.goal
    }

    /// The service request this goal arrived on.
    #[must_use]
    pub const fn request_id(&self) -> RequestId {
        self.request
    }
}

/// What the server knows about one goal.
#[derive(Debug)]
struct GoalRecord<A: ActionType> {
    status: GoalStatus,
    accepted_at: RosTime,
    cancel_requested: bool,
    result: Option<(i8, A::Result)>,
    /// `get_result` requests parked until the goal terminates.
    waiting: Vec<RequestId>,
}

/// An action server: three services and two topics.
///
/// # The shape, and why `get_result` forces it
///
/// `get_result` is a service whose answer is the goal's *outcome*, which
/// may be minutes away. A client sends its `get_result` request the moment
/// the goal is accepted, precisely so that it is already waiting when the
/// goal finishes. The server therefore has to park the request and answer
/// later — which is what
/// [`ServiceServer::take_request`](crate::service::ServiceServer::take_request)
/// plus [`send_response`](crate::service::ServiceServer::send_response)
/// exist for, and why this crate's service API is not a callback.
///
/// Two background tasks run per server: one parks and answers `get_result`
/// requests, one applies `cancel_goal` requests to the registry. Both are
/// aborted by [`shutdown`](ActionServer::shutdown) and when the last clone
/// is dropped.
#[derive(Debug)]
pub struct ActionServer<A: ActionType> {
    inner: Arc<ServerInner<A>>,
}

impl<A: ActionType> Clone for ActionServer<A> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[derive(Debug)]
struct ServerInner<A: ActionType> {
    context: Arc<Ros2Context>,
    action: FullName,
    goal_service: ServiceServer<SendGoalService<A>>,
    result_service: ServiceServer<GetResultService<A>>,
    cancel_service: ServiceServer<CancelGoal>,
    feedback: RawPublisher,
    status: Publisher<action_msgs::GoalStatusArray>,
    goals: Mutex<BTreeMap<GoalUuid, GoalRecord<A>>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl<A: ActionType> Drop for ServerInner<A> {
    fn drop(&mut self) {
        if let Ok(mut tasks) = self.tasks.try_lock() {
            for task in tasks.drain(..) {
                task.abort();
            }
        }
    }
}

impl<A: ActionType> ActionServer<A> {
    /// Create all five endpoints and start the two background tasks.
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
        let goal_service = ServiceServer::<SendGoalService<A>>::new(
            Arc::clone(&context),
            action.action_endpoint(ActionEndpoint::SendGoal)?,
            qos.goal_service,
            owner.clone(),
        )
        .await?;
        let result_service = ServiceServer::<GetResultService<A>>::new(
            Arc::clone(&context),
            action.action_endpoint(ActionEndpoint::GetResult)?,
            qos.result_service,
            owner.clone(),
        )
        .await?;
        let cancel_service = ServiceServer::<CancelGoal>::new(
            Arc::clone(&context),
            action.action_endpoint(ActionEndpoint::CancelGoal)?,
            qos.cancel_service,
            owner.clone(),
        )
        .await?;
        let feedback = RawPublisher::owned_by(
            Arc::clone(&context),
            action.action_endpoint(ActionEndpoint::Feedback)?,
            A::FEEDBACK_DDS_NAME,
            qos.feedback,
            owner.clone(),
        )
        .await?;
        let status = Publisher::<action_msgs::GoalStatusArray>::new(
            Arc::clone(&context),
            action.action_endpoint(ActionEndpoint::Status)?,
            qos.status,
            owner,
        )
        .await?;

        let inner = Arc::new(ServerInner {
            context,
            action,
            goal_service,
            result_service,
            cancel_service,
            feedback,
            status,
            goals: Mutex::new(BTreeMap::new()),
            tasks: Mutex::new(Vec::new()),
        });

        let result_task = tokio::spawn(serve_results(Arc::clone(&inner)));
        let cancel_task = tokio::spawn(serve_cancels(Arc::clone(&inner)));
        inner.tasks.lock().await.extend([result_task, cancel_task]);

        Ok(Self { inner })
    }

    /// The fully-qualified action name.
    #[must_use]
    pub fn action(&self) -> &FullName {
        &self.inner.action
    }

    /// How many clients have been discovered on the `send_goal` endpoint.
    pub async fn client_count(&self) -> usize {
        self.inner.goal_service.client_count().await
    }

    /// Wait until a client has been discovered on every endpoint.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Timeout`].
    pub async fn wait_for_client(&self, timeout: StdDuration) -> Ros2Result<()> {
        crate::pubsub::wait_for(
            &self.inner.context,
            timeout,
            "waiting for an action client",
            || async {
                self.inner.goal_service.client_count().await > 0
                    && self.inner.result_service.client_count().await > 0
                    && self.inner.cancel_service.client_count().await > 0
                    && self.inner.feedback.subscription_count().await > 0
                    && self.inner.status.subscription_count().await > 0
            },
        )
        .await
    }

    /// Take the next goal request, waiting for one.
    pub async fn take_goal(&self) -> PendingGoal<A> {
        let (request, body) = self.inner.goal_service.take_request().await;
        let (id, goal) = A::split_send_goal_request(body);
        PendingGoal {
            id: GoalUuid::from_message(&id),
            goal,
            request,
        }
    }

    /// Take the next goal request, or give up after `timeout`.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Timeout`].
    pub async fn take_goal_within(&self, timeout: StdDuration) -> Ros2Result<PendingGoal<A>> {
        tokio::time::timeout(timeout, self.take_goal())
            .await
            .map_err(|_| crate::pubsub::timed_out("taking an action goal", timeout))
    }

    /// Accept a goal: register it as `ACCEPTED`, answer the client, and
    /// publish the new status array.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Cdr`] or [`Ros2Error::Rtps`].
    pub async fn accept(&self, pending: PendingGoal<A>) -> Ros2Result<GoalUuid> {
        let id = pending.id;
        let stamp = self.inner.context.clock().now();
        {
            let mut goals = self.inner.goals.lock().await;
            goals.insert(
                id,
                GoalRecord {
                    status: GoalStatus::Accepted,
                    accepted_at: stamp,
                    cancel_requested: false,
                    result: None,
                    waiting: Vec::new(),
                },
            );
        }
        self.inner
            .goal_service
            .send_response(
                pending.request,
                &A::make_send_goal_response(true, stamp.to_message()),
            )
            .await?;
        self.publish_status().await?;
        Ok(id)
    }

    /// Reject a goal: answer the client and register nothing.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Cdr`] or [`Ros2Error::Rtps`].
    pub async fn reject(&self, pending: PendingGoal<A>) -> Ros2Result<()> {
        let stamp = self.inner.context.clock().now();
        self.inner
            .goal_service
            .send_response(
                pending.request,
                &A::make_send_goal_response(false, stamp.to_message()),
            )
            .await
    }

    /// Move a goal from `ACCEPTED` to `EXECUTING`.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::UnknownGoal`] or [`Ros2Error::GoalAlreadyTerminal`].
    pub async fn execute(&self, id: GoalUuid) -> Ros2Result<()> {
        self.transition(id, GoalStatus::Executing).await?;
        self.publish_status().await
    }

    /// The status of a goal, if the server knows it.
    pub async fn status_of(&self, id: GoalUuid) -> Option<GoalStatus> {
        self.inner
            .goals
            .lock()
            .await
            .get(&id)
            .map(|record| record.status)
    }

    /// True when a cancel request has been accepted for this goal.
    ///
    /// `rclcpp`'s `ServerGoalHandle::is_canceling`: the application polls it
    /// between units of work and calls
    /// [`canceled`](ActionServer::canceled) when it has stopped.
    pub async fn is_cancel_requested(&self, id: GoalUuid) -> bool {
        self.inner
            .goals
            .lock()
            .await
            .get(&id)
            .is_some_and(|record| record.cancel_requested)
    }

    /// Every goal the server is still working on.
    pub async fn active_goals(&self) -> Vec<GoalUuid> {
        self.inner
            .goals
            .lock()
            .await
            .iter()
            .filter(|(_, record)| record.status.is_active())
            .map(|(id, _)| *id)
            .collect()
    }

    /// Publish one feedback message for a goal.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Cdr`] or [`Ros2Error::Rtps`].
    pub async fn publish_feedback(&self, id: GoalUuid, feedback: &A::Feedback) -> Ros2Result<()> {
        let message = A::make_feedback_message(id.to_message(), feedback.clone());
        let payload = astrs_cdr::to_vec_ros2(&message)?;
        self.inner.feedback.publish_bytes(payload).await
    }

    /// Publish the current status array.
    ///
    /// Called automatically after every transition; exposed because a
    /// server that has just discovered a late-joining client may want to
    /// republish without changing anything.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Cdr`] or [`Ros2Error::Rtps`].
    pub async fn publish_status(&self) -> Ros2Result<()> {
        let array = self.status_array().await;
        self.inner.status.publish(&array).await
    }

    /// The status array as it currently stands.
    pub async fn status_array(&self) -> action_msgs::GoalStatusArray {
        let goals = self.inner.goals.lock().await;
        action_msgs::GoalStatusArray {
            status_list: goals
                .iter()
                .map(|(id, record)| action_msgs::GoalStatus {
                    goal_info: action_msgs::GoalInfo {
                        goal_id: id.to_message(),
                        stamp: record.accepted_at.to_message(),
                    },
                    status: record.status.code(),
                })
                .collect(),
        }
    }

    /// Finish a goal successfully.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::UnknownGoal`], [`Ros2Error::GoalAlreadyTerminal`], or a
    /// transport failure while answering parked `get_result` requests.
    pub async fn succeed(&self, id: GoalUuid, result: A::Result) -> Ros2Result<()> {
        self.terminate(id, GoalStatus::Succeeded, result).await
    }

    /// Finish a goal unsuccessfully.
    ///
    /// # Errors
    ///
    /// As [`succeed`](Self::succeed).
    pub async fn abort(&self, id: GoalUuid, result: A::Result) -> Ros2Result<()> {
        self.terminate(id, GoalStatus::Aborted, result).await
    }

    /// Finish a goal because it was canceled.
    ///
    /// # Errors
    ///
    /// As [`succeed`](Self::succeed); additionally
    /// [`Ros2Error::GoalNotCancelable`] when the goal was never asked to
    /// cancel, because reporting `CANCELED` for a goal nobody canceled would
    /// make the client's outcome a lie.
    pub async fn canceled(&self, id: GoalUuid, result: A::Result) -> Ros2Result<()> {
        {
            let goals = self.inner.goals.lock().await;
            let record = goals.get(&id).ok_or_else(|| Ros2Error::UnknownGoal {
                goal: id.to_string(),
            })?;
            if record.status != GoalStatus::Canceling {
                return Err(Ros2Error::GoalNotCancelable {
                    goal: id.to_string(),
                    status: record.status.name(),
                });
            }
        }
        self.terminate(id, GoalStatus::Canceled, result).await
    }

    /// Forget a terminated goal.
    ///
    /// A server that runs for weeks would otherwise accumulate one record
    /// per goal forever. Only a terminal goal may be forgotten, and any
    /// `get_result` request still parked on it is answered first — by
    /// `terminate`, which runs before this can.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::UnknownGoal`], or [`Ros2Error::GoalNotCancelable`] when
    /// the goal is still active.
    pub async fn forget(&self, id: GoalUuid) -> Ros2Result<()> {
        {
            let mut goals = self.inner.goals.lock().await;
            let record = goals.get(&id).ok_or_else(|| Ros2Error::UnknownGoal {
                goal: id.to_string(),
            })?;
            if !record.status.is_terminal() {
                return Err(Ros2Error::GoalNotCancelable {
                    goal: id.to_string(),
                    status: record.status.name(),
                });
            }
            goals.remove(&id);
        }
        self.publish_status().await
    }

    /// Stop the background tasks and delete every endpoint.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`] or [`Ros2Error::Cdr`].
    pub async fn shutdown(&self) -> Ros2Result<()> {
        for task in self.inner.tasks.lock().await.drain(..) {
            task.abort();
        }
        self.inner.goal_service.destroy().await?;
        self.inner.result_service.destroy().await?;
        self.inner.cancel_service.destroy().await?;
        self.inner.feedback.destroy().await?;
        self.inner.status.destroy().await?;
        Ok(())
    }

    /// Apply a non-terminal transition.
    async fn transition(&self, id: GoalUuid, next: GoalStatus) -> Ros2Result<()> {
        let mut goals = self.inner.goals.lock().await;
        let record = goals.get_mut(&id).ok_or_else(|| Ros2Error::UnknownGoal {
            goal: id.to_string(),
        })?;
        if record.status.is_terminal() {
            return Err(Ros2Error::GoalAlreadyTerminal {
                goal: id.to_string(),
                status: record.status.name(),
            });
        }
        if !record.status.can_transition_to(next) {
            return Err(Ros2Error::GoalNotCancelable {
                goal: id.to_string(),
                status: record.status.name(),
            });
        }
        record.status = next;
        Ok(())
    }

    /// Apply a terminal transition, answer every parked request, publish.
    async fn terminate(
        &self,
        id: GoalUuid,
        status: GoalStatus,
        result: A::Result,
    ) -> Ros2Result<()> {
        let waiting = {
            let mut goals = self.inner.goals.lock().await;
            let record = goals.get_mut(&id).ok_or_else(|| Ros2Error::UnknownGoal {
                goal: id.to_string(),
            })?;
            if record.status.is_terminal() {
                return Err(Ros2Error::GoalAlreadyTerminal {
                    goal: id.to_string(),
                    status: record.status.name(),
                });
            }
            if !record.status.can_transition_to(status) {
                return Err(Ros2Error::GoalNotCancelable {
                    goal: id.to_string(),
                    status: record.status.name(),
                });
            }
            record.status = status;
            record.result = Some((status.code(), result.clone()));
            core::mem::take(&mut record.waiting)
        };

        for request in waiting {
            self.inner
                .result_service
                .send_response(
                    request,
                    &A::make_get_result_response(status.code(), result.clone()),
                )
                .await?;
        }
        self.publish_status().await
    }
}

/// The `get_result` task: park requests for unfinished goals, answer the
/// rest at once.
async fn serve_results<A: ActionType>(inner: Arc<ServerInner<A>>) {
    loop {
        let (request, body) = inner.result_service.take_request().await;
        let id = GoalUuid::from_message(&A::get_result_request_goal_id(&body));

        let ready = {
            let mut goals = inner.goals.lock().await;
            match goals.get_mut(&id) {
                // Already finished: answer now.
                Some(record) if record.result.is_some() => record.result.clone(),
                // Known but unfinished: park until it terminates.
                Some(record) => {
                    record.waiting.push(request);
                    None
                }
                // Unknown goal: `UNKNOWN` with a default result, which is
                // what `rcl_action` answers rather than leaving the client
                // waiting forever for a goal that was never accepted.
                None => Some((GoalStatus::Unknown.code(), A::Result::default())),
            }
        };

        if let Some((status, result)) = ready
            && inner
                .result_service
                .send_response(request, &A::make_get_result_response(status, result))
                .await
                .is_err()
        {
            return;
        }
    }
}

/// The `cancel_goal` task: apply the four request forms to the registry.
async fn serve_cancels<A: ActionType>(inner: Arc<ServerInner<A>>) {
    loop {
        let (request, body) = inner.cancel_service.take_request().await;
        let ask = CancelRequest::from_goal_info(&body.goal_info);

        let (code, canceling) = {
            let mut goals = inner.goals.lock().await;
            let named_goal = match ask {
                CancelRequest::One { goal } | CancelRequest::OneAndBefore { goal, .. } => {
                    Some(goal)
                }
                _ => None,
            };
            let known = named_goal.is_none_or(|goal| goals.contains_key(&goal));
            let named_terminal = named_goal
                .and_then(|goal| goals.get(&goal))
                .is_some_and(|record| record.status.is_terminal());

            let mut canceling = Vec::new();
            for (id, record) in goals.iter_mut() {
                if record.status.is_cancelable() && ask.matches(*id, record.accepted_at) {
                    record.status = GoalStatus::Canceling;
                    record.cancel_requested = true;
                    canceling.push(*id);
                }
            }

            let code = if !known {
                CancelResponseCode::UnknownGoalId
            } else if canceling.is_empty() && named_terminal {
                CancelResponseCode::GoalTerminated
            } else {
                CancelResponseCode::None
            };
            (code, canceling)
        };

        let response = action_msgs::CancelGoalResponse {
            return_code: code.code(),
            goals_canceling: canceling
                .iter()
                .map(|id| action_msgs::GoalInfo {
                    goal_id: id.to_message(),
                    stamp: RosTime::ZERO.to_message(),
                })
                .collect(),
        };
        if inner
            .cancel_service
            .send_response(request, &response)
            .await
            .is_err()
        {
            return;
        }

        // Every accepted cancel changes a status, so the array is stale.
        let array = {
            let goals = inner.goals.lock().await;
            action_msgs::GoalStatusArray {
                status_list: goals
                    .iter()
                    .map(|(id, record)| action_msgs::GoalStatus {
                        goal_info: action_msgs::GoalInfo {
                            goal_id: id.to_message(),
                            stamp: record.accepted_at.to_message(),
                        },
                        status: record.status.code(),
                    })
                    .collect(),
            }
        };
        if inner.status.publish(&array).await.is_err() {
            return;
        }
    }
}
