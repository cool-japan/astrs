//! Well-known metadata keys, and the action-goal status they encode.
//!
//! Blueprint §6.1 lists the reserved keys; §9.4 explains what they are for:
//! services, actions and streams all ride ordinary graph edges and are
//! correlated purely through metadata, with no separate RPC subsystem.
//!
//! | Key | Shape | Pattern |
//! |---|---|---|
//! | [`REQUEST_ID`] | string | service request ↔ response |
//! | [`GOAL_ID`] | string | action goal identity |
//! | [`GOAL_STATUS`] | integer ([`GoalStatus`]) | action goal FSM |
//! | [`SESSION_ID`] | string | stream session |
//! | [`SEGMENT_ID`] | integer | stream segment within a session |
//! | [`SEQ`] | integer | sequence number within a segment |
//! | [`FIN`] | bool | last chunk of a segment |
//! | [`FLUSH`] | bool | deliver buffered chunks now |
//! | [`SCHEMA_HASH`] | string | internal: payload schema fingerprint |
//!
//! Keys beginning with `_` are AstRS-internal and are removed by
//! [`crate::Metadata::strip_internal`] before an event reaches user code.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::metadata::keys;
//!
//! assert!(keys::is_correlation_key(keys::REQUEST_ID));
//! assert!(!keys::is_correlation_key(keys::SEQ));
//! assert!(keys::is_internal_key(keys::SCHEMA_HASH));
//! ```

use core::fmt;

use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// Correlates a service response with the request that caused it (§9.4).
pub const REQUEST_ID: &str = "request_id";

/// Identifies one action goal across its whole lifetime (§9.4).
pub const GOAL_ID: &str = "goal_id";

/// Carries the current [`GoalStatus`] of an action goal (§9.4).
pub const GOAL_STATUS: &str = "goal_status";

/// Identifies one stream session (§9.4).
pub const SESSION_ID: &str = "session_id";

/// Identifies one segment within a stream session (§9.4).
pub const SEGMENT_ID: &str = "segment_id";

/// The sequence number of a chunk within its segment (§9.4).
pub const SEQ: &str = "seq";

/// Marks the final chunk of a stream segment (§9.4).
pub const FIN: &str = "fin";

/// Asks the receiver to deliver buffered chunks immediately (§9.4).
pub const FLUSH: &str = "flush";

/// Internal: the payload's schema fingerprint, stamped by `astrs-data`'s
/// `SchemaHash` so receivers can cache decoded schemas and detect type drift
/// cheaply (§6.1).
pub const SCHEMA_HASH: &str = "_schema_hash";

/// The prefix that marks a key as AstRS-internal.
pub const INTERNAL_PREFIX: char = '_';

/// Every well-known key, in documentation order.
///
/// # Examples
///
/// ```
/// use astrs_wire::metadata::keys;
///
/// assert!(keys::WELL_KNOWN.contains(&keys::SEQ));
/// assert_eq!(keys::WELL_KNOWN.len(), 9);
/// ```
pub const WELL_KNOWN: &[&str] = &[
    REQUEST_ID,
    GOAL_ID,
    GOAL_STATUS,
    SESSION_ID,
    SEGMENT_ID,
    SEQ,
    FIN,
    FLUSH,
    SCHEMA_HASH,
];

/// The keys that make a message *correlated*.
///
/// Blueprint §11.2: correlated messages are immune to queue eviction, because
/// dropping a service response or a goal-status update would wedge a client
/// forever. This is the exact list the scheduler consults.
///
/// # Examples
///
/// ```
/// use astrs_wire::metadata::keys;
///
/// assert_eq!(keys::CORRELATION, &["request_id", "goal_id", "goal_status"]);
/// ```
pub const CORRELATION: &[&str] = &[REQUEST_ID, GOAL_ID, GOAL_STATUS];

/// The keys that describe a chunk's place in a stream.
///
/// # Examples
///
/// ```
/// use astrs_wire::metadata::keys;
///
/// assert!(keys::STREAM.contains(&keys::SEGMENT_ID));
/// ```
pub const STREAM: &[&str] = &[SESSION_ID, SEGMENT_ID, SEQ, FIN, FLUSH];

/// Whether `key` is AstRS-internal (starts with `_`).
///
/// # Examples
///
/// ```
/// use astrs_wire::metadata::keys;
///
/// assert!(keys::is_internal_key("_schema_hash"));
/// assert!(!keys::is_internal_key("seq"));
/// assert!(!keys::is_internal_key(""));
/// ```
#[must_use]
pub fn is_internal_key(key: &str) -> bool {
    key.starts_with(INTERNAL_PREFIX)
}

/// Whether `key` is one of the correlation keys of [`CORRELATION`].
///
/// # Examples
///
/// ```
/// use astrs_wire::metadata::keys;
///
/// assert!(keys::is_correlation_key("goal_id"));
/// assert!(!keys::is_correlation_key("fin"));
/// ```
#[must_use]
pub fn is_correlation_key(key: &str) -> bool {
    CORRELATION.contains(&key)
}

/// Whether `key` is one of the stream keys of [`STREAM`].
///
/// # Examples
///
/// ```
/// use astrs_wire::metadata::keys;
///
/// assert!(keys::is_stream_key("seq"));
/// assert!(!keys::is_stream_key("request_id"));
/// ```
#[must_use]
pub fn is_stream_key(key: &str) -> bool {
    STREAM.contains(&key)
}

/// Whether `key` is reserved by AstRS.
///
/// Reserved keys are the well-known ones plus anything beginning with `_`.
/// A node may still set them — that is how patterns are implemented — but a
/// manifest that names one as a user parameter is making a mistake.
///
/// # Examples
///
/// ```
/// use astrs_wire::metadata::keys;
///
/// assert!(keys::is_reserved_key("seq"));
/// assert!(keys::is_reserved_key("_anything"));
/// assert!(!keys::is_reserved_key("my_key"));
/// ```
#[must_use]
pub fn is_reserved_key(key: &str) -> bool {
    is_internal_key(key) || WELL_KNOWN.contains(&key)
}

/// The lifecycle state of an action goal (blueprint §9.4).
///
/// The discriminants match ROS 2's `action_msgs/msg/GoalStatus` constants
/// exactly, so `astrs-ros2` can bridge an action in either direction without
/// a translation table. The AstRS FSM proper is
/// `Accepted → Executing → {Succeeded, Aborted, Canceled}`; `Canceling` and
/// `Unknown` exist for ROS 2 fidelity.
///
/// # Examples
///
/// ```
/// use astrs_wire::GoalStatus;
///
/// assert_eq!(GoalStatus::Executing.as_i64(), 2);
/// assert_eq!(GoalStatus::from_i64(4), Some(GoalStatus::Succeeded));
/// assert!(GoalStatus::Succeeded.is_terminal());
/// assert!(GoalStatus::Accepted.can_transition_to(GoalStatus::Executing));
/// assert!(!GoalStatus::Succeeded.can_transition_to(GoalStatus::Executing));
/// ```
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Encode,
    Decode,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GoalStatus {
    /// The goal's state is not known (ROS 2 `STATUS_UNKNOWN`).
    #[default]
    #[oxicode(variant = 0)]
    Unknown,
    /// The server accepted the goal but has not started it
    /// (ROS 2 `STATUS_ACCEPTED`).
    #[oxicode(variant = 1)]
    Accepted,
    /// The server is executing the goal (ROS 2 `STATUS_EXECUTING`).
    #[oxicode(variant = 2)]
    Executing,
    /// A cancellation was requested and is being processed
    /// (ROS 2 `STATUS_CANCELING`).
    #[oxicode(variant = 3)]
    Canceling,
    /// The goal completed successfully (ROS 2 `STATUS_SUCCEEDED`).
    #[oxicode(variant = 4)]
    Succeeded,
    /// The goal was cancelled (ROS 2 `STATUS_CANCELED`).
    #[oxicode(variant = 5)]
    Canceled,
    /// The goal failed (ROS 2 `STATUS_ABORTED`).
    #[oxicode(variant = 6)]
    Aborted,
}

impl GoalStatus {
    /// Every status, in discriminant order.
    pub const ALL: &'static [Self] = &[
        Self::Unknown,
        Self::Accepted,
        Self::Executing,
        Self::Canceling,
        Self::Succeeded,
        Self::Canceled,
        Self::Aborted,
    ];

    /// The numeric form stored under [`GOAL_STATUS`].
    #[must_use]
    pub const fn as_i64(self) -> i64 {
        match self {
            Self::Unknown => 0,
            Self::Accepted => 1,
            Self::Executing => 2,
            Self::Canceling => 3,
            Self::Succeeded => 4,
            Self::Canceled => 5,
            Self::Aborted => 6,
        }
    }

    /// Parses the numeric form.
    ///
    /// Returns `None` for a code this build does not know, rather than
    /// collapsing it to [`GoalStatus::Unknown`] — a caller that wants that
    /// fallback should say so.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::GoalStatus;
    ///
    /// assert_eq!(GoalStatus::from_i64(6), Some(GoalStatus::Aborted));
    /// assert_eq!(GoalStatus::from_i64(7), None);
    /// assert_eq!(GoalStatus::from_i64(-1), None);
    /// ```
    #[must_use]
    pub const fn from_i64(code: i64) -> Option<Self> {
        match code {
            0 => Some(Self::Unknown),
            1 => Some(Self::Accepted),
            2 => Some(Self::Executing),
            3 => Some(Self::Canceling),
            4 => Some(Self::Succeeded),
            5 => Some(Self::Canceled),
            6 => Some(Self::Aborted),
            _ => None,
        }
    }

    /// A stable, lower-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Accepted => "accepted",
            Self::Executing => "executing",
            Self::Canceling => "canceling",
            Self::Succeeded => "succeeded",
            Self::Canceled => "canceled",
            Self::Aborted => "aborted",
        }
    }

    /// Whether the goal has reached a final state.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::GoalStatus;
    ///
    /// assert!(GoalStatus::Canceled.is_terminal());
    /// assert!(!GoalStatus::Canceling.is_terminal());
    /// ```
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Canceled | Self::Aborted)
    }

    /// Whether the goal is still in flight.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Accepted | Self::Executing | Self::Canceling)
    }

    /// Whether `next` is a legal successor of `self` in the goal FSM.
    ///
    /// Servers use this to reject a status update that would move a goal
    /// backwards or out of a terminal state, which is the failure mode that
    /// leaves a client waiting forever.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::GoalStatus;
    ///
    /// assert!(GoalStatus::Unknown.can_transition_to(GoalStatus::Accepted));
    /// assert!(GoalStatus::Executing.can_transition_to(GoalStatus::Canceling));
    /// assert!(GoalStatus::Canceling.can_transition_to(GoalStatus::Canceled));
    /// assert!(!GoalStatus::Accepted.can_transition_to(GoalStatus::Unknown));
    /// assert!(!GoalStatus::Aborted.can_transition_to(GoalStatus::Aborted));
    /// ```
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        match self {
            Self::Unknown => matches!(next, Self::Accepted),
            Self::Accepted => matches!(
                next,
                Self::Executing
                    | Self::Canceling
                    | Self::Succeeded
                    | Self::Canceled
                    | Self::Aborted
            ),
            Self::Executing => matches!(
                next,
                Self::Canceling | Self::Succeeded | Self::Canceled | Self::Aborted
            ),
            Self::Canceling => matches!(next, Self::Canceled | Self::Succeeded | Self::Aborted),
            Self::Succeeded | Self::Canceled | Self::Aborted => false,
        }
    }
}

impl fmt::Display for GoalStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<GoalStatus> for i64 {
    fn from(status: GoalStatus) -> Self {
        status.as_i64()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::round_trip;

    #[test]
    fn well_known_keys_match_the_blueprint() {
        assert_eq!(
            WELL_KNOWN,
            &[
                "request_id",
                "goal_id",
                "goal_status",
                "session_id",
                "segment_id",
                "seq",
                "fin",
                "flush",
                "_schema_hash",
            ]
        );
    }

    #[test]
    fn well_known_keys_are_unique() {
        let unique: std::collections::BTreeSet<_> = WELL_KNOWN.iter().collect();
        assert_eq!(unique.len(), WELL_KNOWN.len());
    }

    #[test]
    fn correlation_and_stream_sets_are_disjoint_subsets() {
        for key in CORRELATION {
            assert!(WELL_KNOWN.contains(key));
            assert!(!STREAM.contains(key));
        }
        for key in STREAM {
            assert!(WELL_KNOWN.contains(key));
            assert!(!CORRELATION.contains(key));
        }
    }

    #[test]
    fn key_predicates_agree_with_the_sets() {
        for key in WELL_KNOWN {
            assert!(is_reserved_key(key), "{key}");
            assert_eq!(is_correlation_key(key), CORRELATION.contains(key));
            assert_eq!(is_stream_key(key), STREAM.contains(key));
        }
        assert!(is_internal_key(SCHEMA_HASH));
        assert!(!is_internal_key(SEQ));
        assert!(is_reserved_key("_future_internal_key"));
        assert!(!is_reserved_key("user_key"));
        assert!(!is_internal_key(""));
    }

    #[test]
    fn goal_status_codes_match_ros2() {
        // action_msgs/msg/GoalStatus: UNKNOWN=0, ACCEPTED=1, EXECUTING=2,
        // CANCELING=3, SUCCEEDED=4, CANCELED=5, ABORTED=6.
        assert_eq!(GoalStatus::Unknown.as_i64(), 0);
        assert_eq!(GoalStatus::Accepted.as_i64(), 1);
        assert_eq!(GoalStatus::Executing.as_i64(), 2);
        assert_eq!(GoalStatus::Canceling.as_i64(), 3);
        assert_eq!(GoalStatus::Succeeded.as_i64(), 4);
        assert_eq!(GoalStatus::Canceled.as_i64(), 5);
        assert_eq!(GoalStatus::Aborted.as_i64(), 6);
    }

    #[test]
    fn numeric_form_round_trips_and_rejects_unknown_codes() {
        for &status in GoalStatus::ALL {
            assert_eq!(GoalStatus::from_i64(status.as_i64()), Some(status));
            assert_eq!(i64::from(status), status.as_i64());
        }
        for code in [-2i64, -1, 7, 8, i64::MAX, i64::MIN] {
            assert_eq!(GoalStatus::from_i64(code), None, "code {code}");
        }
    }

    #[test]
    fn terminal_and_active_partition_everything_but_unknown() {
        for &status in GoalStatus::ALL {
            assert!(!(status.is_terminal() && status.is_active()));
            if status != GoalStatus::Unknown {
                assert!(status.is_terminal() || status.is_active(), "{status}");
            }
        }
        assert!(!GoalStatus::Unknown.is_terminal());
        assert!(!GoalStatus::Unknown.is_active());
    }

    #[test]
    fn terminal_states_have_no_successors() {
        for status in GoalStatus::ALL.iter().copied().filter(|s| s.is_terminal()) {
            for &next in GoalStatus::ALL {
                assert!(
                    !status.can_transition_to(next),
                    "{status} must not move to {next}"
                );
            }
        }
    }

    #[test]
    fn the_documented_happy_path_is_legal() {
        assert!(GoalStatus::Unknown.can_transition_to(GoalStatus::Accepted));
        assert!(GoalStatus::Accepted.can_transition_to(GoalStatus::Executing));
        assert!(GoalStatus::Executing.can_transition_to(GoalStatus::Succeeded));
        assert!(GoalStatus::Executing.can_transition_to(GoalStatus::Aborted));
        assert!(GoalStatus::Executing.can_transition_to(GoalStatus::Canceling));
        assert!(GoalStatus::Canceling.can_transition_to(GoalStatus::Canceled));
    }

    #[test]
    fn no_status_can_transition_to_itself() {
        for &status in GoalStatus::ALL {
            assert!(!status.can_transition_to(status), "{status}");
        }
    }

    #[test]
    fn names_are_unique_and_render() {
        let mut seen = std::collections::BTreeSet::new();
        for &status in GoalStatus::ALL {
            assert!(seen.insert(status.as_str()));
            assert_eq!(status.to_string(), status.as_str());
        }
    }

    #[test]
    fn codec_and_serde_round_trip() {
        for &status in GoalStatus::ALL {
            assert_eq!(round_trip(&status).unwrap(), status);
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(serde_json::from_str::<GoalStatus>(&json).unwrap(), status);
        }
        assert_eq!(GoalStatus::default(), GoalStatus::Unknown);
    }
}
