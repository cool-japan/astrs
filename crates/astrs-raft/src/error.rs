//! [`RaftError`]: every way this crate can fail, named.
//!
//! Raft draws a hard line between *"this peer cannot do that right now"* (a
//! follower asked to accept a proposal, a leader whose lease has lapsed) and
//! *"the durable state is broken"* (a WAL whose checksum does not match).
//! The first is routine and the caller retries or redirects; the second must
//! never be papered over, because a Raft replica that keeps serving from a
//! log it cannot verify is exactly how a cluster loses committed data.
//! Keeping both in one enum with clearly different variants is what lets a
//! call site match on the difference instead of inspecting a message string.

use std::path::PathBuf;

use crate::types::{LogIndex, PeerId, Term};

/// The result type every fallible operation in this crate returns.
pub type Result<T> = std::result::Result<T, RaftError>;

/// Everything that can go wrong inside a Raft replica.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RaftError {
    /// A proposal reached a peer that is not the leader.
    ///
    /// Carries the leader this peer last heard from, when it knows one, so
    /// the caller can redirect rather than poll.
    #[error("not the leader{}", match .leader {
        Some(peer) => format!(" (the leader is {peer})"),
        None => " (no leader is known)".to_owned(),
    })]
    NotLeader {
        /// The peer this replica last accepted an `AppendEntries` from.
        leader: Option<PeerId>,
    },

    /// A leader-lease read was attempted after the lease lapsed.
    #[error("the leader lease has lapsed; this read would not be linearizable")]
    LeaseExpired,

    /// A membership change was proposed while an earlier one is still
    /// uncommitted.
    ///
    /// Raft's single-server membership algorithm is only safe when at most
    /// one change is in flight; overlapping changes can produce two disjoint
    /// majorities.
    #[error("a membership change at {pending} is still uncommitted")]
    ConfigChangeInFlight {
        /// The log index of the change already in flight.
        pending: LogIndex,
    },

    /// A membership change would have produced a configuration that cannot
    /// elect a leader, or that this peer refuses for the stated reason.
    #[error("invalid membership change: {reason}")]
    InvalidMembership {
        /// Why the change was refused.
        reason: &'static str,
    },

    /// A read of the log asked for an index the log no longer holds because
    /// it was compacted into a snapshot.
    #[error("index {index} was compacted; the log starts at {first}")]
    Compacted {
        /// The index asked for.
        index: LogIndex,
        /// The first index still present.
        first: LogIndex,
    },

    /// A read of the log asked for an index past its end.
    #[error("index {index} is past the end of the log at {last}")]
    Unavailable {
        /// The index asked for.
        index: LogIndex,
        /// The last index present.
        last: LogIndex,
    },

    /// The configuration this replica was handed cannot be used.
    #[error("invalid configuration: {reason}")]
    InvalidConfig {
        /// Why the configuration was refused.
        reason: &'static str,
    },

    /// A write-ahead-log file could not be read or written.
    #[error("write-ahead log I/O failed while {what} at {}: {source}", .path.display())]
    Io {
        /// What the replica was doing.
        what: &'static str,
        /// The file involved.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// A durable record failed its integrity check or could not be decoded.
    ///
    /// Distinct from a *torn tail*, which is expected after a crash and is
    /// repaired silently on open (see [`crate::WalLog`]): this is corruption
    /// in the middle of a log that was believed intact.
    #[error("corrupt write-ahead log at byte offset {offset}: {reason}")]
    Corrupt {
        /// Where the bad record starts.
        offset: u64,
        /// What was wrong with it.
        reason: &'static str,
    },

    /// A record could not be encoded to, or decoded from, its `oxicode`
    /// form.
    #[error("codec failure: {0}")]
    Codec(#[from] astrs_wire::WireError),

    /// The state machine refused to apply a committed command.
    ///
    /// A state machine that cannot apply a *committed* entry has no safe
    /// recovery inside Raft — every other replica will apply it — so this is
    /// surfaced rather than retried.
    #[error("the state machine failed to apply index {index}: {reason}")]
    Apply {
        /// The entry that could not be applied.
        index: LogIndex,
        /// What the state machine reported.
        reason: String,
    },

    /// A snapshot could not be produced or restored.
    #[error("snapshot {what} failed: {reason}")]
    Snapshot {
        /// Which half failed: `"capture"` or `"restore"`.
        what: &'static str,
        /// What the state machine reported.
        reason: String,
    },

    /// A message could not be handed to the transport.
    #[error("transport failure sending to {peer}: {reason}")]
    Transport {
        /// The intended recipient.
        peer: PeerId,
        /// What the transport reported.
        reason: String,
    },

    /// A peer was named that this replica's configuration does not know.
    #[error("unknown peer {peer}")]
    UnknownPeer {
        /// The unrecognized id.
        peer: PeerId,
    },

    /// A durable record was written by a build whose format this one does
    /// not understand.
    #[error("unsupported write-ahead-log format version {found}; this build writes {supported}")]
    UnsupportedVersion {
        /// The version found on disk.
        found: u16,
        /// The version this build writes.
        supported: u16,
    },

    /// A proposal was accepted but the term changed before it committed, so
    /// it may or may not be in the log.
    #[error("proposal at {index} was superseded: this replica left {term}")]
    ProposalLost {
        /// Where the proposal was appended.
        index: LogIndex,
        /// The term it was appended in.
        term: Term,
    },

    /// A replica was asked to do something after it shut down.
    #[error("this Raft replica has shut down")]
    ShuttingDown,
}

impl RaftError {
    /// Builds an [`RaftError::Io`] for `path`.
    #[must_use]
    pub fn io(what: &'static str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            what,
            path: path.into(),
            source,
        }
    }

    /// Whether the caller should retry this operation somewhere else — at
    /// the leader, or after an election.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_raft::{PeerId, RaftError};
    ///
    /// let redirect = RaftError::NotLeader { leader: Some(PeerId::new(2)) };
    /// assert!(redirect.is_redirectable());
    /// assert_eq!(redirect.leader_hint(), Some(PeerId::new(2)));
    /// ```
    #[must_use]
    pub const fn is_redirectable(&self) -> bool {
        matches!(self, Self::NotLeader { .. } | Self::LeaseExpired)
    }

    /// The leader this error knows about, when it knows one.
    ///
    /// This is what a server built on this crate turns into a redirect hint
    /// for its own clients.
    #[must_use]
    pub const fn leader_hint(&self) -> Option<PeerId> {
        match self {
            Self::NotLeader { leader } => *leader,
            _ => None,
        }
    }

    /// Whether this error means the durable state is untrustworthy.
    ///
    /// A replica seeing one of these must not keep serving: it has lost the
    /// ability to prove what it has committed.
    #[must_use]
    pub const fn is_corruption(&self) -> bool {
        matches!(
            self,
            Self::Corrupt { .. } | Self::UnsupportedVersion { .. } | Self::Codec(_)
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn not_leader_names_the_leader_when_it_knows_one() {
        let known = RaftError::NotLeader {
            leader: Some(PeerId::new(4)),
        };
        assert!(known.to_string().contains("peer 4"));
        assert_eq!(known.leader_hint(), Some(PeerId::new(4)));

        let unknown = RaftError::NotLeader { leader: None };
        assert!(unknown.to_string().contains("no leader is known"));
        assert_eq!(unknown.leader_hint(), None);
    }

    #[test]
    fn redirectable_errors_are_exactly_the_two_leadership_ones() {
        assert!(RaftError::NotLeader { leader: None }.is_redirectable());
        assert!(RaftError::LeaseExpired.is_redirectable());
        assert!(
            !RaftError::Corrupt {
                offset: 0,
                reason: "bad crc",
            }
            .is_redirectable()
        );
    }

    #[test]
    fn corruption_is_distinguished_from_ordinary_failure() {
        assert!(
            RaftError::Corrupt {
                offset: 12,
                reason: "checksum mismatch",
            }
            .is_corruption()
        );
        assert!(
            RaftError::UnsupportedVersion {
                found: 9,
                supported: 1,
            }
            .is_corruption()
        );
        assert!(!RaftError::ShuttingDown.is_corruption());
        assert!(
            !RaftError::Io {
                what: "opening",
                path: PathBuf::from("/nowhere"),
                source: std::io::Error::other("gone"),
            }
            .is_corruption()
        );
    }

    #[test]
    fn compaction_and_unavailability_read_differently() {
        let compacted = RaftError::Compacted {
            index: LogIndex::new(3),
            first: LogIndex::new(9),
        };
        assert!(compacted.to_string().contains("compacted"));
        let unavailable = RaftError::Unavailable {
            index: LogIndex::new(30),
            last: LogIndex::new(9),
        };
        assert!(unavailable.to_string().contains("past the end"));
    }

    #[test]
    fn io_errors_carry_the_path_and_the_activity() {
        let error = RaftError::io(
            "appending an entry",
            PathBuf::from("/tmp/raft.wal"),
            std::io::Error::other("disk full"),
        );
        let text = error.to_string();
        assert!(text.contains("appending an entry"), "{text}");
        assert!(text.contains("raft.wal"), "{text}");
        assert!(text.contains("disk full"), "{text}");
    }

    #[test]
    fn config_change_in_flight_names_the_blocking_index() {
        let error = RaftError::ConfigChangeInFlight {
            pending: LogIndex::new(11),
        };
        assert!(error.to_string().contains("index 11"));
    }
}
