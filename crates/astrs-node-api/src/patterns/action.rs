//! Long-running goals with a status FSM (blueprint §9.4,
//! `goal_id`/`goal_status`).
//!
//! ```text
//!            ┌──────────► Canceling ──► Canceled
//!            │                ▲
//!   Accepted ┴──► Executing ──┼──► Succeeded
//!                             └──► Aborted
//! ```
//!
//! The states are [`astrs_wire::GoalStatus`], whose discriminants match ROS 2's
//! `action_msgs/msg/GoalStatus` exactly, so `astrs-ros2` bridges an action in
//! either direction without a translation table.
//!
//! # The FSM is enforced, not documented
//!
//! [`GoalTracker`] refuses an illegal transition — `Succeeded → Executing`, a
//! status after a terminal one, a status for a goal it has never seen — with
//! [`crate::NodeError::Pattern`]. A client that trusts a server's status
//! sequence without checking it will eventually meet a server that resends,
//! reorders, or reports a terminal status twice; the tracker turns each of
//! those into an error at the moment it happens rather than a wrong decision
//! ten seconds later.
//!
//! # Feedback
//!
//! A feedback message is a status update carrying [`GoalStatus::Executing`]
//! and a payload. There is no separate feedback topic, because there does not
//! need to be one: it is the same edge, the same `goal_id`, and the queue
//! immunity that `goal_status` already confers (§11.2).

use core::fmt;
use std::collections::BTreeMap;

use astrs_data::AstrsMessage;
use astrs_wire::{DataId, GoalStatus, Metadata};

use crate::error::{NodeError, Result};
use crate::events::Event;
use crate::node::Node;
use crate::output::{Output, RawOutput};
use crate::payload::Payload;

/// One action goal's identity, stable for its whole lifetime (§9.4).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GoalId(String);

impl GoalId {
    /// A fresh id.
    #[must_use]
    pub fn generate() -> Self {
        Self(super::fresh_id())
    }

    /// Wraps an existing id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Unwraps the id.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for GoalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for GoalId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// One status update, as a client or server sees it.
#[derive(Debug)]
pub struct ActionOutcome {
    /// The input it arrived on.
    pub input: DataId,
    /// The goal it concerns.
    pub goal: GoalId,
    /// The status it reports.
    pub status: GoalStatus,
    /// Its metadata.
    pub metadata: Metadata,
    /// The payload — a result for a terminal status, feedback otherwise.
    pub payload: Payload,
}

impl ActionOutcome {
    /// Reads a status update out of an event, if it is one.
    ///
    /// An input with a `goal_id` but no `goal_status` is the *goal itself*
    /// (what a server receives), not an update; it is returned unchanged so a
    /// server can handle it with [`ActionOutcome::goal_of`].
    pub fn from_event(event: Event) -> core::result::Result<Self, Box<Event>> {
        let Event::Input { id, meta, data } = event else {
            return Err(Box::new(event));
        };
        let (Some(goal), Some(status)) = (meta.goal_id().map(GoalId::new), meta.goal_status())
        else {
            return Err(Box::new(Event::Input { id, meta, data }));
        };
        Ok(Self {
            input: id,
            goal,
            status,
            metadata: meta,
            payload: data,
        })
    }

    /// The goal id of an event that carries one, whether or not it is a
    /// status update.
    #[must_use]
    pub fn goal_of(event: &Event) -> Option<GoalId> {
        event
            .metadata()
            .and_then(Metadata::goal_id)
            .map(GoalId::new)
    }

    /// Whether this update ends the goal.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    /// Whether this update is feedback rather than an outcome.
    #[must_use]
    pub fn is_feedback(&self) -> bool {
        self.status == GoalStatus::Executing && !self.payload.is_empty()
    }

    /// Decodes the payload.
    ///
    /// # Errors
    ///
    /// [`NodeError::Data`] when the payload is not a `T`.
    pub fn view<T: crate::message::FromPayload>(&self) -> Result<T> {
        self.payload.view()
    }
}

/// A client-side view of every goal in flight, with the FSM enforced.
#[derive(Debug, Default)]
pub struct GoalTracker {
    /// The last status seen for each goal.
    states: BTreeMap<GoalId, GoalStatus>,
}

impl GoalTracker {
    /// An empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a goal this node just sent, in [`GoalStatus::Accepted`].
    pub fn track(&mut self, goal: GoalId) {
        let _previous = self.states.insert(goal, GoalStatus::Accepted);
    }

    /// The status last seen for `goal`.
    #[must_use]
    pub fn status(&self, goal: &GoalId) -> Option<GoalStatus> {
        self.states.get(goal).copied()
    }

    /// How many goals are still in flight.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.states
            .values()
            .filter(|status| !status.is_terminal())
            .count()
    }

    /// Every goal being tracked.
    #[must_use]
    pub fn goals(&self) -> Vec<GoalId> {
        self.states.keys().cloned().collect()
    }

    /// Applies a status update, checking the FSM (§9.4).
    ///
    /// # Errors
    ///
    /// [`NodeError::Pattern`] for a goal that was never tracked, a transition
    /// the FSM does not allow, or any update after a terminal status.
    pub fn observe(&mut self, outcome: &ActionOutcome) -> Result<GoalStatus> {
        let Some(current) = self.states.get(&outcome.goal).copied() else {
            return Err(NodeError::Pattern(format!(
                "status `{}` for goal `{}`, which this node never sent",
                outcome.status, outcome.goal
            )));
        };
        if current.is_terminal() {
            return Err(NodeError::Pattern(format!(
                "goal `{}` already ended as `{current}`; it cannot now be `{}`",
                outcome.goal, outcome.status
            )));
        }
        if current == outcome.status {
            // A repeated non-terminal status is a heartbeat, not a
            // transition: feedback arrives as repeated `Executing`.
            return Ok(current);
        }
        if !current.can_transition_to(outcome.status) {
            return Err(NodeError::Pattern(format!(
                "goal `{}` cannot go from `{current}` to `{}`",
                outcome.goal, outcome.status
            )));
        }
        let _previous = self.states.insert(outcome.goal.clone(), outcome.status);
        Ok(outcome.status)
    }

    /// Forgets every goal that has ended.
    ///
    /// Returns how many were removed. A long-running client that never prunes
    /// grows one entry per goal forever, which is the sort of leak that only
    /// shows up after a week of uptime.
    pub fn prune_terminal(&mut self) -> usize {
        let before = self.states.len();
        self.states.retain(|_, status| !status.is_terminal());
        before - self.states.len()
    }
}

impl Node {
    /// Sends a typed goal, returning its id (§9.4).
    ///
    /// The goal carries `goal_status = Accepted`, which is the first state of
    /// the FSM: a client that has sent a goal and heard nothing knows it is
    /// accepted-and-not-yet-executing rather than in no state at all.
    ///
    /// # Errors
    ///
    /// As [`Output::send`].
    pub fn goal<T: AstrsMessage>(
        &self,
        output: &mut Output<T>,
        value: impl Into<T>,
    ) -> Result<GoalId> {
        let goal = GoalId::generate();
        let mut metadata = self.metadata();
        metadata.set_goal_id(goal.as_str());
        metadata.set_goal_status(GoalStatus::Accepted);
        output.send(value, metadata)?;
        Ok(goal)
    }

    /// Sends an untyped goal, returning its id.
    ///
    /// # Errors
    ///
    /// As [`RawOutput::send_bytes`].
    pub fn goal_bytes(&self, output: &mut RawOutput, payload: impl AsRef<[u8]>) -> Result<GoalId> {
        let goal = GoalId::generate();
        let mut metadata = self.metadata();
        metadata.set_goal_id(goal.as_str());
        metadata.set_goal_status(GoalStatus::Accepted);
        output.send_bytes(payload, metadata)?;
        Ok(goal)
    }

    /// Reports a typed status update for `goal` (§9.4).
    ///
    /// # Errors
    ///
    /// As [`Output::send`].
    pub fn goal_status<T: AstrsMessage>(
        &self,
        output: &mut Output<T>,
        goal: &GoalId,
        status: GoalStatus,
        value: impl Into<T>,
    ) -> Result<()> {
        output.send(value, self.goal_metadata(goal, status))
    }

    /// Reports an untyped status update for `goal`.
    ///
    /// # Errors
    ///
    /// As [`RawOutput::send_bytes`].
    pub fn goal_status_bytes(
        &self,
        output: &mut RawOutput,
        goal: &GoalId,
        status: GoalStatus,
        payload: impl AsRef<[u8]>,
    ) -> Result<()> {
        output.send_bytes(payload, self.goal_metadata(goal, status))
    }

    /// Reports a status update with no payload — the common case for
    /// `Accepted`, `Canceling` and a bare `Aborted`.
    ///
    /// # Errors
    ///
    /// As [`RawOutput::send_bytes`].
    pub fn goal_signal(
        &self,
        output: &mut RawOutput,
        goal: &GoalId,
        status: GoalStatus,
    ) -> Result<()> {
        let batch = crate::message::Empty.to_record_batch()?;
        output.send_batch(&batch, self.goal_metadata(goal, status))
    }

    /// The metadata a status update for `goal` carries.
    #[must_use]
    pub fn goal_metadata(&self, goal: &GoalId, status: GoalStatus) -> Metadata {
        let mut metadata = self.metadata();
        metadata.set_goal_id(goal.as_str());
        metadata.set_goal_status(status);
        metadata
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::message::Scalar;
    use crate::testing::TestHarness;
    use astrs_time::HlcTimestamp;
    use std::time::Duration;

    fn outcome(goal: &GoalId, status: GoalStatus) -> ActionOutcome {
        let mut metadata = Metadata::new(HlcTimestamp::new(1, 0));
        metadata.set_goal_id(goal.as_str());
        metadata.set_goal_status(status);
        ActionOutcome {
            input: DataId::new("status").unwrap(),
            goal: goal.clone(),
            status,
            metadata,
            payload: Payload::empty(),
        }
    }

    #[test]
    fn goal_ids_are_fresh_and_printable() {
        let goal = GoalId::generate();
        assert_ne!(goal, GoalId::generate());
        assert_eq!(goal.to_string(), goal.as_str());
        assert_eq!(GoalId::new("g").into_string(), "g");
        assert_eq!(GoalId::from("g".to_owned()).as_str(), "g");
    }

    #[test]
    fn the_happy_path_is_accepted_executing_succeeded() {
        let goal = GoalId::generate();
        let mut tracker = GoalTracker::new();
        tracker.track(goal.clone());
        assert_eq!(tracker.status(&goal), Some(GoalStatus::Accepted));
        assert_eq!(tracker.in_flight(), 1);
        assert_eq!(tracker.goals(), vec![goal.clone()]);

        assert_eq!(
            tracker
                .observe(&outcome(&goal, GoalStatus::Executing))
                .unwrap(),
            GoalStatus::Executing
        );
        assert_eq!(
            tracker
                .observe(&outcome(&goal, GoalStatus::Succeeded))
                .unwrap(),
            GoalStatus::Succeeded
        );
        assert_eq!(tracker.in_flight(), 0);
        assert_eq!(tracker.prune_terminal(), 1);
        assert!(tracker.goals().is_empty());
    }

    #[test]
    fn a_repeated_executing_is_feedback_not_a_transition() {
        let goal = GoalId::generate();
        let mut tracker = GoalTracker::new();
        tracker.track(goal.clone());
        let _ = tracker
            .observe(&outcome(&goal, GoalStatus::Executing))
            .unwrap();
        assert_eq!(
            tracker
                .observe(&outcome(&goal, GoalStatus::Executing))
                .unwrap(),
            GoalStatus::Executing
        );
    }

    #[test]
    fn an_illegal_transition_is_refused() {
        let goal = GoalId::generate();
        let mut tracker = GoalTracker::new();
        tracker.track(goal.clone());
        let _ = tracker
            .observe(&outcome(&goal, GoalStatus::Succeeded))
            .unwrap();
        let error = tracker
            .observe(&outcome(&goal, GoalStatus::Executing))
            .unwrap_err();
        assert!(matches!(error, NodeError::Pattern(_)), "{error}");
        assert!(error.to_string().contains("already ended"), "{error}");
    }

    #[test]
    fn a_status_for_an_unknown_goal_is_refused() {
        let mut tracker = GoalTracker::new();
        let error = tracker
            .observe(&outcome(&GoalId::generate(), GoalStatus::Executing))
            .unwrap_err();
        assert!(error.to_string().contains("never sent"), "{error}");
    }

    #[test]
    fn a_status_update_is_recognised_and_a_bare_goal_is_not() {
        let goal = GoalId::generate();
        let update = ActionOutcome::from_event(Event::Input {
            id: DataId::new("status").unwrap(),
            meta: {
                let mut meta = Metadata::new(HlcTimestamp::new(1, 0));
                meta.set_goal_id(goal.as_str());
                meta.set_goal_status(GoalStatus::Executing);
                meta
            },
            data: Payload::empty(),
        })
        .expect("a status update");
        assert_eq!(update.goal, goal);
        assert!(!update.is_terminal());
        assert!(!update.is_feedback(), "an empty payload is not feedback");

        // A goal *request* carries `goal_id` but no `goal_status`.
        let mut meta = Metadata::new(HlcTimestamp::new(1, 0));
        meta.set_goal_id(goal.as_str());
        let event = Event::Input {
            id: DataId::new("goal").unwrap(),
            meta,
            data: Payload::empty(),
        };
        let back = ActionOutcome::from_event(event).unwrap_err();
        assert_eq!(ActionOutcome::goal_of(&back), Some(goal));
        assert_eq!(ActionOutcome::goal_of(&Event::AllInputsClosed), None);
    }

    #[test]
    fn a_full_action_between_two_nodes() {
        let daemon = crate::testing::MockDaemon::start().unwrap();
        let client_spec = astrs_wire::NodeSpawnSpec::new(
            daemon.dataflow(),
            astrs_wire::NodeId::new("client").unwrap(),
            0,
            astrs_wire::NodeSource::Dynamic,
        )
        .with_output(astrs_wire::OutputSpec::new(DataId::new("goal").unwrap()))
        .with_input(astrs_wire::InputSpec::new(
            DataId::new("status").unwrap(),
            astrs_wire::PortRef::from_parts("server", "status").unwrap(),
        ));
        let server_spec = astrs_wire::NodeSpawnSpec::new(
            daemon.dataflow(),
            astrs_wire::NodeId::new("server").unwrap(),
            0,
            astrs_wire::NodeSource::Dynamic,
        )
        .with_input(astrs_wire::InputSpec::new(
            DataId::new("goal").unwrap(),
            astrs_wire::PortRef::from_parts("client", "goal").unwrap(),
        ))
        .with_output(astrs_wire::OutputSpec::new(DataId::new("status").unwrap()));

        let (mut client, mut client_events) = daemon.connect_node(client_spec).unwrap();
        let (mut server, mut server_events) = daemon.connect_node(server_spec).unwrap();

        let mut goal_out = client.output::<Scalar<f64>>("goal").unwrap();
        let goal = client.goal(&mut goal_out, 3.0_f64).unwrap();
        let mut tracker = GoalTracker::new();
        tracker.track(goal.clone());

        // The server sees the goal and drives it to completion.
        let event = server_events
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .expect("the goal");
        let received = ActionOutcome::goal_of(&event).expect("a goal id");
        assert_eq!(received, goal);

        let mut status_out = server.output::<Scalar<f64>>("status").unwrap();
        server
            .goal_status(&mut status_out, &goal, GoalStatus::Executing, 0.5_f64)
            .unwrap();
        server
            .goal_status(&mut status_out, &goal, GoalStatus::Succeeded, 1.0_f64)
            .unwrap();

        // The client walks the FSM.
        let mut seen = Vec::new();
        while seen.len() < 2 {
            let event = client_events
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .expect("a status update");
            let Ok(update) = ActionOutcome::from_event(event) else {
                continue;
            };
            seen.push(tracker.observe(&update).unwrap());
            if update.status == GoalStatus::Executing {
                assert!(update.is_feedback(), "the payload makes it feedback");
                assert_eq!(update.view::<Scalar<f64>>().unwrap().into_inner(), 0.5);
            }
        }
        assert_eq!(seen, vec![GoalStatus::Executing, GoalStatus::Succeeded]);
        assert_eq!(tracker.in_flight(), 0);
    }

    #[test]
    fn a_bare_status_signal_carries_no_payload() {
        let mut harness = TestHarness::start().unwrap();
        let mut output = harness
            .node
            .raw_output(TestHarness::DEFAULT_OUTPUT)
            .unwrap();
        let goal = harness.node.goal_bytes(&mut output, vec![1]).unwrap();
        harness
            .node
            .goal_signal(&mut output, &goal, GoalStatus::Canceling)
            .unwrap();

        let sends = harness
            .daemon
            .wait_for_sends(
                harness.node.id(),
                &DataId::new(TestHarness::DEFAULT_OUTPUT).unwrap(),
                2,
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(sends[0].metadata.goal_status(), Some(GoalStatus::Accepted));
        assert_eq!(sends[1].metadata.goal_status(), Some(GoalStatus::Canceling));
        assert_eq!(sends[1].metadata.goal_id(), Some(goal.as_str()));
    }
}
