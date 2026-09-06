//! Goal identity and the goal state machine.
//!
//! # The states
//!
//! `action_msgs/msg/GoalStatus` defines seven, and the transitions between
//! them are the contract every action server and client agrees on
//! (<https://design.ros2.org/articles/actions.html>):
//!
//! ```text
//!                    ┌──────────┐
//!                    │ ACCEPTED │
//!                    └────┬─────┘
//!            execute      │      cancel
//!         ┌───────────────┴───────────────┐
//!         ▼                               ▼
//!   ┌───────────┐   cancel          ┌───────────┐
//!   │ EXECUTING ├──────────────────▶│ CANCELING │
//!   └─────┬─────┘                   └─────┬─────┘
//!         │                               │
//!    succeed│abort                 canceled│abort
//!         ▼                               ▼
//!   ┌───────────┐ ┌──────────┐ ┌──────────┐
//!   │ SUCCEEDED │ │ ABORTED  │ │ CANCELED │
//!   └───────────┘ └──────────┘ └──────────┘
//! ```
//!
//! `UNKNOWN` is the eighth value and is not a state a goal reaches: it is
//! what a status field holds before anything has been said about the goal.
//!
//! [`GoalStatus::can_transition_to`] is that diagram as a predicate, and it
//! is what stops a server from aborting a goal it already succeeded — a
//! mistake that would otherwise show up as a client receiving two results
//! for one goal.
//!
//! # Cancel matching
//!
//! `action_msgs/srv/CancelGoal` overloads its one `GoalInfo` field into four
//! requests, distinguished by which halves are zero:
//!
//! | `goal_id` | `stamp` | Means |
//! |---|---|---|
//! | zero | zero | cancel every goal |
//! | zero | set | cancel every goal accepted at or before `stamp` |
//! | set | zero | cancel that one goal |
//! | set | set | cancel that goal, *and* every goal accepted at or before `stamp` |
//!
//! [`CancelRequest`] parses the four and [`CancelRequest::matches`] applies
//! them, because getting this wrong means a `ros2 action send_goal -f` that
//! silently cancels a robot's other goals.

use core::fmt;

use crate::msg::{action_msgs, unique_identifier_msgs};
use crate::time::RosTime;

/// Octets in a goal's UUID.
pub const GOAL_UUID_LEN: usize = 16;

/// A goal's identity: sixteen octets, as `unique_identifier_msgs/msg/UUID`.
///
/// A newtype rather than a bare `[u8; 16]` so that a goal id and a GID —
/// both sixteen octets — cannot be swapped by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct GoalUuid([u8; GOAL_UUID_LEN]);

impl GoalUuid {
    /// The all-zero UUID, which `CancelGoal` reads as "every goal".
    pub const ZERO: Self = Self([0; GOAL_UUID_LEN]);

    /// Wrap sixteen octets.
    #[must_use]
    pub const fn new(octets: [u8; GOAL_UUID_LEN]) -> Self {
        Self(octets)
    }

    /// A fresh, process-unique identifier.
    ///
    /// Not a cryptographic UUID: a goal id only has to be unique among the
    /// goals one client sends to one server, and a monotonic counter mixed
    /// with the process id and a clock reading gives that with no
    /// dependency. The version and variant nibbles are set to 4 and 8 so
    /// that a ROS tool rendering it as a UUID string produces something
    /// well-formed.
    #[must_use]
    pub fn generate() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0_u64, |since| since.as_nanos() as u64);
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = u64::from(std::process::id());

        let mut octets = [0_u8; GOAL_UUID_LEN];
        octets[..8].copy_from_slice(&nanos.to_be_bytes());
        octets[8..12].copy_from_slice(&(pid as u32).to_be_bytes());
        octets[12..].copy_from_slice(&(counter as u32).to_be_bytes());
        octets[6] = (octets[6] & 0x0f) | 0x40;
        octets[8] = (octets[8] & 0x3f) | 0x80;
        Self(octets)
    }

    /// The octets.
    #[must_use]
    pub const fn octets(&self) -> [u8; GOAL_UUID_LEN] {
        self.0
    }

    /// True when every octet is zero.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.0.iter().all(|octet| *octet == 0)
    }

    /// Read the wire message.
    #[must_use]
    pub const fn from_message(message: &unique_identifier_msgs::UUID) -> Self {
        Self(message.uuid)
    }

    /// Render as the wire message.
    #[must_use]
    pub const fn to_message(self) -> unique_identifier_msgs::UUID {
        unique_identifier_msgs::UUID { uuid: self.0 }
    }
}

impl fmt::Display for GoalUuid {
    /// The canonical 8-4-4-4-12 hexadecimal form.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, octet) in self.0.iter().enumerate() {
            if matches!(index, 4 | 6 | 8 | 10) {
                formatter.write_str("-")?;
            }
            write!(formatter, "{octet:02x}")?;
        }
        Ok(())
    }
}

impl From<[u8; GOAL_UUID_LEN]> for GoalUuid {
    fn from(octets: [u8; GOAL_UUID_LEN]) -> Self {
        Self(octets)
    }
}

/// One goal's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum GoalStatus {
    /// Nothing has been said about this goal.
    #[default]
    Unknown,
    /// The server took it and has not started.
    Accepted,
    /// The server is working on it.
    Executing,
    /// A cancel request was accepted; the server is stopping.
    Canceling,
    /// It finished successfully.
    Succeeded,
    /// It stopped because it was canceled.
    Canceled,
    /// The server gave up on it.
    Aborted,
}

impl GoalStatus {
    /// Every state.
    pub const ALL: [Self; 7] = [
        Self::Unknown,
        Self::Accepted,
        Self::Executing,
        Self::Canceling,
        Self::Succeeded,
        Self::Canceled,
        Self::Aborted,
    ];

    /// The `action_msgs/msg/GoalStatus` constant.
    #[must_use]
    pub const fn code(self) -> i8 {
        match self {
            Self::Unknown => action_msgs::GoalStatus::STATUS_UNKNOWN,
            Self::Accepted => action_msgs::GoalStatus::STATUS_ACCEPTED,
            Self::Executing => action_msgs::GoalStatus::STATUS_EXECUTING,
            Self::Canceling => action_msgs::GoalStatus::STATUS_CANCELING,
            Self::Succeeded => action_msgs::GoalStatus::STATUS_SUCCEEDED,
            Self::Canceled => action_msgs::GoalStatus::STATUS_CANCELED,
            Self::Aborted => action_msgs::GoalStatus::STATUS_ABORTED,
        }
    }

    /// Read a status code, with anything unrecognized reading as
    /// [`Unknown`](Self::Unknown).
    #[must_use]
    pub const fn from_code(code: i8) -> Self {
        match code {
            action_msgs::GoalStatus::STATUS_ACCEPTED => Self::Accepted,
            action_msgs::GoalStatus::STATUS_EXECUTING => Self::Executing,
            action_msgs::GoalStatus::STATUS_CANCELING => Self::Canceling,
            action_msgs::GoalStatus::STATUS_SUCCEEDED => Self::Succeeded,
            action_msgs::GoalStatus::STATUS_CANCELED => Self::Canceled,
            action_msgs::GoalStatus::STATUS_ABORTED => Self::Aborted,
            _ => Self::Unknown,
        }
    }

    /// The name `ros2 action` prints.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unknown => "UNKNOWN",
            Self::Accepted => "ACCEPTED",
            Self::Executing => "EXECUTING",
            Self::Canceling => "CANCELING",
            Self::Succeeded => "SUCCEEDED",
            Self::Canceled => "CANCELED",
            Self::Aborted => "ABORTED",
        }
    }

    /// True for a state a goal never leaves.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Canceled | Self::Aborted)
    }

    /// True while the server is still working on the goal.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Accepted | Self::Executing | Self::Canceling)
    }

    /// True when a cancel request may still be accepted for this state.
    #[must_use]
    pub const fn is_cancelable(self) -> bool {
        matches!(self, Self::Accepted | Self::Executing)
    }

    /// True when the transition to `next` is one the design document allows.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Unknown, Self::Accepted)
                | (Self::Accepted, Self::Executing | Self::Canceling)
                | (Self::Executing, Self::Canceling)
                | (
                    Self::Accepted | Self::Executing,
                    Self::Succeeded | Self::Aborted
                )
                | (
                    Self::Canceling,
                    Self::Canceled | Self::Succeeded | Self::Aborted
                )
        )
    }
}

impl fmt::Display for GoalStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// A `CancelGoal` request, in the four forms its one field encodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelRequest {
    /// Cancel every goal.
    All,
    /// Cancel every goal accepted at or before `stamp`.
    AcceptedBefore {
        /// The cutoff, inclusive.
        stamp: RosTime,
    },
    /// Cancel one goal.
    One {
        /// Which goal.
        goal: GoalUuid,
    },
    /// Cancel one goal, and every goal accepted at or before `stamp`.
    OneAndBefore {
        /// Which goal.
        goal: GoalUuid,
        /// The cutoff, inclusive.
        stamp: RosTime,
    },
}

impl CancelRequest {
    /// Read a request out of its `GoalInfo`.
    #[must_use]
    pub fn from_goal_info(info: &action_msgs::GoalInfo) -> Self {
        let goal = GoalUuid::from_message(&info.goal_id);
        let stamp = RosTime::from_message(&info.stamp);
        match (goal.is_zero(), stamp.is_zero()) {
            (true, true) => Self::All,
            (true, false) => Self::AcceptedBefore { stamp },
            (false, true) => Self::One { goal },
            (false, false) => Self::OneAndBefore { goal, stamp },
        }
    }

    /// Render back into a `GoalInfo`.
    #[must_use]
    pub fn to_goal_info(self) -> action_msgs::GoalInfo {
        let (goal, stamp) = match self {
            Self::All => (GoalUuid::ZERO, RosTime::ZERO),
            Self::AcceptedBefore { stamp } => (GoalUuid::ZERO, stamp),
            Self::One { goal } => (goal, RosTime::ZERO),
            Self::OneAndBefore { goal, stamp } => (goal, stamp),
        };
        action_msgs::GoalInfo {
            goal_id: goal.to_message(),
            stamp: stamp.to_message(),
        }
    }

    /// True when this request asks to cancel the goal `id`, accepted at
    /// `accepted_at`.
    #[must_use]
    pub fn matches(&self, id: GoalUuid, accepted_at: RosTime) -> bool {
        match *self {
            Self::All => true,
            Self::AcceptedBefore { stamp } => accepted_at <= stamp,
            Self::One { goal } => goal == id,
            Self::OneAndBefore { goal, stamp } => goal == id || accepted_at <= stamp,
        }
    }
}

/// The `CancelGoal` response codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CancelResponseCode {
    /// At least one goal accepted the cancel request.
    None,
    /// The request was rejected outright.
    Rejected,
    /// The named goal does not exist.
    UnknownGoalId,
    /// The named goal has already finished.
    GoalTerminated,
}

impl CancelResponseCode {
    /// Every code.
    pub const ALL: [Self; 4] = [
        Self::None,
        Self::Rejected,
        Self::UnknownGoalId,
        Self::GoalTerminated,
    ];

    /// The `action_msgs/srv/CancelGoal` constant.
    #[must_use]
    pub const fn code(self) -> i8 {
        match self {
            Self::None => action_msgs::CancelGoalResponse::ERROR_NONE,
            Self::Rejected => action_msgs::CancelGoalResponse::ERROR_REJECTED,
            Self::UnknownGoalId => action_msgs::CancelGoalResponse::ERROR_UNKNOWN_GOAL_ID,
            Self::GoalTerminated => action_msgs::CancelGoalResponse::ERROR_GOAL_TERMINATED,
        }
    }

    /// Read a response code.
    #[must_use]
    pub const fn from_code(code: i8) -> Self {
        match code {
            action_msgs::CancelGoalResponse::ERROR_REJECTED => Self::Rejected,
            action_msgs::CancelGoalResponse::ERROR_UNKNOWN_GOAL_ID => Self::UnknownGoalId,
            action_msgs::CancelGoalResponse::ERROR_GOAL_TERMINATED => Self::GoalTerminated,
            _ => Self::None,
        }
    }

    /// True when at least one goal is now canceling.
    #[must_use]
    pub const fn is_accepted(self) -> bool {
        matches!(self, Self::None)
    }

    /// The name a diagnostic prints.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "ERROR_NONE",
            Self::Rejected => "ERROR_REJECTED",
            Self::UnknownGoalId => "ERROR_UNKNOWN_GOAL_ID",
            Self::GoalTerminated => "ERROR_GOAL_TERMINATED",
        }
    }
}

impl fmt::Display for CancelResponseCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// What a cancel request achieved.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CancelOutcome {
    /// Why, or why not.
    pub code: i8,
    /// The goals that are now canceling.
    pub canceling: Vec<GoalUuid>,
}

impl CancelOutcome {
    /// Read the wire response.
    #[must_use]
    pub fn from_message(message: &action_msgs::CancelGoalResponse) -> Self {
        Self {
            code: message.return_code,
            canceling: message
                .goals_canceling
                .iter()
                .map(|info| GoalUuid::from_message(&info.goal_id))
                .collect(),
        }
    }

    /// The code as an enum.
    #[must_use]
    pub const fn response_code(&self) -> CancelResponseCode {
        CancelResponseCode::from_code(self.code)
    }

    /// True when at least one goal accepted the request.
    #[must_use]
    pub fn is_accepted(&self) -> bool {
        self.response_code().is_accepted() && !self.canceling.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn uuid(seed: u8) -> GoalUuid {
        GoalUuid::new([seed; GOAL_UUID_LEN])
    }

    #[test]
    fn a_generated_uuid_is_unique_and_well_formed() {
        let first = GoalUuid::generate();
        let second = GoalUuid::generate();
        assert_ne!(first, second);
        assert!(!first.is_zero());
        assert_eq!(
            first.octets()[6] & 0xf0,
            0x40,
            "the version nibble reads as 4"
        );
        assert_eq!(
            first.octets()[8] & 0xc0,
            0x80,
            "the variant bits read as RFC 4122"
        );
    }

    #[test]
    fn a_uuid_renders_in_the_canonical_form() {
        let rendered = uuid(0xab).to_string();
        assert_eq!(rendered, "abababab-abab-abab-abab-abababababab");
        assert_eq!(rendered.len(), 36);
        assert_eq!(rendered.matches('-').count(), 4);
    }

    #[test]
    fn a_uuid_round_trips_through_its_message() {
        let id = uuid(7);
        assert_eq!(GoalUuid::from_message(&id.to_message()), id);
        assert_eq!(GoalUuid::from([7_u8; 16]), id);
        assert!(GoalUuid::ZERO.is_zero());
        assert_eq!(GoalUuid::default(), GoalUuid::ZERO);
    }

    #[test]
    fn every_status_round_trips_through_its_code() {
        for status in GoalStatus::ALL {
            assert_eq!(GoalStatus::from_code(status.code()), status);
            assert!(!status.name().is_empty());
            assert_eq!(status.to_string(), status.name());
        }
        assert_eq!(GoalStatus::from_code(99), GoalStatus::Unknown);
        assert_eq!(GoalStatus::default(), GoalStatus::Unknown);
    }

    #[test]
    fn the_status_codes_are_the_generated_constants() {
        assert_eq!(GoalStatus::Unknown.code(), 0);
        assert_eq!(GoalStatus::Accepted.code(), 1);
        assert_eq!(GoalStatus::Executing.code(), 2);
        assert_eq!(GoalStatus::Canceling.code(), 3);
        assert_eq!(GoalStatus::Succeeded.code(), 4);
        assert_eq!(GoalStatus::Canceled.code(), 5);
        assert_eq!(GoalStatus::Aborted.code(), 6);
    }

    #[test]
    fn terminal_and_active_partition_the_reachable_states() {
        for status in GoalStatus::ALL {
            if status == GoalStatus::Unknown {
                assert!(!status.is_terminal() && !status.is_active());
                continue;
            }
            assert_ne!(
                status.is_terminal(),
                status.is_active(),
                "{status} is both or neither"
            );
        }
    }

    #[test]
    fn only_accepted_and_executing_are_cancelable() {
        assert!(GoalStatus::Accepted.is_cancelable());
        assert!(GoalStatus::Executing.is_cancelable());
        assert!(!GoalStatus::Canceling.is_cancelable());
        assert!(!GoalStatus::Succeeded.is_cancelable());
        assert!(!GoalStatus::Unknown.is_cancelable());
    }

    #[test]
    fn the_transition_table_matches_the_design_document() {
        assert!(GoalStatus::Unknown.can_transition_to(GoalStatus::Accepted));
        assert!(GoalStatus::Accepted.can_transition_to(GoalStatus::Executing));
        assert!(GoalStatus::Accepted.can_transition_to(GoalStatus::Canceling));
        assert!(GoalStatus::Accepted.can_transition_to(GoalStatus::Succeeded));
        assert!(GoalStatus::Executing.can_transition_to(GoalStatus::Succeeded));
        assert!(GoalStatus::Executing.can_transition_to(GoalStatus::Aborted));
        assert!(GoalStatus::Executing.can_transition_to(GoalStatus::Canceling));
        assert!(GoalStatus::Canceling.can_transition_to(GoalStatus::Canceled));
        assert!(
            GoalStatus::Canceling.can_transition_to(GoalStatus::Succeeded),
            "a goal may finish before it notices the cancel"
        );
    }

    #[test]
    fn a_terminal_goal_transitions_nowhere() {
        for terminal in [
            GoalStatus::Succeeded,
            GoalStatus::Canceled,
            GoalStatus::Aborted,
        ] {
            for next in GoalStatus::ALL {
                assert!(
                    !terminal.can_transition_to(next),
                    "{terminal} must not become {next}"
                );
            }
        }
    }

    #[test]
    fn a_goal_never_goes_backwards() {
        assert!(!GoalStatus::Executing.can_transition_to(GoalStatus::Accepted));
        assert!(!GoalStatus::Canceling.can_transition_to(GoalStatus::Executing));
        assert!(!GoalStatus::Accepted.can_transition_to(GoalStatus::Accepted));
        assert!(!GoalStatus::Accepted.can_transition_to(GoalStatus::Canceled));
    }

    #[test]
    fn the_four_cancel_forms_parse_from_one_field() {
        let all = action_msgs::GoalInfo {
            goal_id: GoalUuid::ZERO.to_message(),
            stamp: RosTime::ZERO.to_message(),
        };
        assert_eq!(CancelRequest::from_goal_info(&all), CancelRequest::All);

        let before = action_msgs::GoalInfo {
            goal_id: GoalUuid::ZERO.to_message(),
            stamp: RosTime::new(10, 0).to_message(),
        };
        assert_eq!(
            CancelRequest::from_goal_info(&before),
            CancelRequest::AcceptedBefore {
                stamp: RosTime::new(10, 0)
            }
        );

        let one = action_msgs::GoalInfo {
            goal_id: uuid(3).to_message(),
            stamp: RosTime::ZERO.to_message(),
        };
        assert_eq!(
            CancelRequest::from_goal_info(&one),
            CancelRequest::One { goal: uuid(3) }
        );

        let both = action_msgs::GoalInfo {
            goal_id: uuid(3).to_message(),
            stamp: RosTime::new(10, 0).to_message(),
        };
        assert_eq!(
            CancelRequest::from_goal_info(&both),
            CancelRequest::OneAndBefore {
                goal: uuid(3),
                stamp: RosTime::new(10, 0)
            }
        );
    }

    #[test]
    fn every_cancel_form_round_trips_through_its_goal_info() {
        let forms = [
            CancelRequest::All,
            CancelRequest::AcceptedBefore {
                stamp: RosTime::new(5, 0),
            },
            CancelRequest::One { goal: uuid(1) },
            CancelRequest::OneAndBefore {
                goal: uuid(1),
                stamp: RosTime::new(5, 0),
            },
        ];
        for form in forms {
            assert_eq!(CancelRequest::from_goal_info(&form.to_goal_info()), form);
        }
    }

    #[test]
    fn cancel_all_matches_every_goal() {
        assert!(CancelRequest::All.matches(uuid(1), RosTime::new(100, 0)));
        assert!(CancelRequest::All.matches(uuid(2), RosTime::ZERO));
    }

    #[test]
    fn cancel_before_matches_by_acceptance_time_inclusively() {
        let request = CancelRequest::AcceptedBefore {
            stamp: RosTime::new(10, 0),
        };
        assert!(request.matches(uuid(1), RosTime::new(9, 0)));
        assert!(
            request.matches(uuid(1), RosTime::new(10, 0)),
            "the cutoff is inclusive"
        );
        assert!(!request.matches(uuid(1), RosTime::new(11, 0)));
    }

    #[test]
    fn cancel_one_matches_only_that_goal() {
        let request = CancelRequest::One { goal: uuid(1) };
        assert!(request.matches(uuid(1), RosTime::new(100, 0)));
        assert!(!request.matches(uuid(2), RosTime::ZERO));
    }

    #[test]
    fn cancel_one_and_before_is_the_union_of_the_two() {
        let request = CancelRequest::OneAndBefore {
            goal: uuid(1),
            stamp: RosTime::new(10, 0),
        };
        assert!(
            request.matches(uuid(1), RosTime::new(99, 0)),
            "the named goal matches whenever it was accepted"
        );
        assert!(
            request.matches(uuid(2), RosTime::new(5, 0)),
            "and so does any goal before the cutoff"
        );
        assert!(!request.matches(uuid(2), RosTime::new(50, 0)));
    }

    #[test]
    fn every_response_code_round_trips() {
        for code in CancelResponseCode::ALL {
            assert_eq!(CancelResponseCode::from_code(code.code()), code);
            assert_eq!(code.to_string(), code.name());
        }
        assert_eq!(
            CancelResponseCode::from_code(99),
            CancelResponseCode::None,
            "an unrecognized code reads as the permissive one"
        );
        assert!(CancelResponseCode::None.is_accepted());
        assert!(!CancelResponseCode::Rejected.is_accepted());
    }

    #[test]
    fn an_outcome_needs_both_a_good_code_and_a_goal() {
        let refused = CancelOutcome {
            code: CancelResponseCode::UnknownGoalId.code(),
            canceling: Vec::new(),
        };
        assert!(!refused.is_accepted());

        let empty = CancelOutcome {
            code: CancelResponseCode::None.code(),
            canceling: Vec::new(),
        };
        assert!(
            !empty.is_accepted(),
            "ERROR_NONE with nothing canceling cancelled nothing"
        );

        let accepted = CancelOutcome {
            code: CancelResponseCode::None.code(),
            canceling: vec![uuid(1)],
        };
        assert!(accepted.is_accepted());
    }

    #[test]
    fn an_outcome_reads_its_wire_response() {
        let message = action_msgs::CancelGoalResponse {
            return_code: CancelResponseCode::None.code(),
            goals_canceling: vec![action_msgs::GoalInfo {
                goal_id: uuid(4).to_message(),
                stamp: RosTime::new(1, 0).to_message(),
            }],
        };
        let outcome = CancelOutcome::from_message(&message);
        assert_eq!(outcome.canceling, vec![uuid(4)]);
        assert_eq!(outcome.response_code(), CancelResponseCode::None);
        assert!(outcome.is_accepted());
    }
}
