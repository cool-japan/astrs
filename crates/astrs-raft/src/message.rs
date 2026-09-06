//! The Raft RPC family: [`RaftMessage`] and its [`Envelope`].
//!
//! This is **its own** message enum, not a variant appended to any of
//! `astrs-wire`'s five control-plane families. Raft traffic is peer-to-peer
//! between coordinator replicas, never multiplexed onto a CLI or daemon
//! connection, so giving it a private enum keeps the frozen wire enums frozen
//! (blueprint §3, principle 4) while still using the same `oxicode`
//! configuration and the same frame layout — see [`crate::tcp`].
//!
//! # Figure 2's two RPCs, plus two extensions
//!
//! `RequestVote` and `AppendEntries` are the whole of Figure 2.
//! `InstallSnapshot` is the compaction extension (§7 of the paper) and
//! `PreVote` is the disruption fix (dissertation §9.6). All four appear here
//! with their responses, and every variant carries the sender's term first,
//! because the term check is the first thing every handler does.
//!
//! # The envelope carries identity, the message carries state
//!
//! Figure 2 puts `candidateId` inside `RequestVote` and `leaderId` inside
//! `AppendEntries`. Here both live in [`Envelope::from`] instead: every
//! message needs a sender for routing anyway, and duplicating it inside the
//! payload creates a second, disagreeable source of truth — a message whose
//! envelope says one peer and whose body says another has no defined
//! meaning.
//!
//! # Examples
//!
//! ```
//! use astrs_raft::{Envelope, LogIndex, PeerId, RaftMessage, Term};
//!
//! let heartbeat = RaftMessage::AppendEntries {
//!     term: Term::new(3),
//!     prev_log_index: LogIndex::new(9),
//!     prev_log_term: Term::new(2),
//!     entries: Vec::new(),
//!     leader_commit: LogIndex::new(9),
//! };
//! assert!(heartbeat.is_heartbeat());
//! assert_eq!(heartbeat.term(), Term::new(3));
//!
//! let envelope = Envelope::new(PeerId::new(1), PeerId::new(2), heartbeat);
//! assert_eq!(envelope.from, PeerId::new(1));
//! assert_eq!(envelope.message.variant_name(), "AppendEntries");
//! ```

use core::fmt;

use oxicode::{Decode, Encode};

use crate::log::entry::{LogEntry, Snapshot};
use crate::types::{LogIndex, PeerId, Term};

/// One Raft RPC or its response.
///
/// Variant indices are explicit and **append-only**: a replica upgraded
/// mid-rollout must still understand every message its not-yet-upgraded peers
/// send.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
#[non_exhaustive]
pub enum RaftMessage {
    /// "Would you vote for me if I stood?" — asked at `term + 1` **without**
    /// the sender having adopted that term.
    #[oxicode(variant = 0)]
    PreVoteRequest {
        /// The term the sender *would* campaign in: its own term plus one.
        term: Term,
        /// The sender's last log index.
        last_log_index: LogIndex,
        /// The term of that entry.
        last_log_term: Term,
    },
    /// The answer to [`RaftMessage::PreVoteRequest`].
    ///
    /// `term` is the **responder's own** term, not the hypothetical one it
    /// was asked about, so a pre-candidate learns it is behind.
    #[oxicode(variant = 1)]
    PreVoteResponse {
        /// The responder's current term.
        term: Term,
        /// Whether the responder would grant a real vote.
        granted: bool,
    },
    /// Figure 2's `RequestVote`.
    #[oxicode(variant = 2)]
    VoteRequest {
        /// The candidate's term.
        term: Term,
        /// The candidate's last log index.
        last_log_index: LogIndex,
        /// The term of that entry.
        last_log_term: Term,
    },
    /// Figure 2's `RequestVote` response.
    #[oxicode(variant = 3)]
    VoteResponse {
        /// The responder's current term.
        term: Term,
        /// Whether the vote was granted.
        granted: bool,
    },
    /// Figure 2's `AppendEntries`, heartbeat and replication alike.
    #[oxicode(variant = 4)]
    AppendEntries {
        /// The leader's term.
        term: Term,
        /// The index immediately preceding `entries`.
        prev_log_index: LogIndex,
        /// The term of the entry at `prev_log_index`.
        prev_log_term: Term,
        /// The entries to store; empty for a heartbeat.
        entries: Vec<LogEntry>,
        /// The leader's commit index.
        leader_commit: LogIndex,
    },
    /// Figure 2's `AppendEntries` response, extended with the conflict hint
    /// that turns linear back-off into a single round trip.
    #[oxicode(variant = 5)]
    AppendEntriesResponse {
        /// The responder's current term.
        term: Term,
        /// Whether the entries were accepted.
        success: bool,
        /// On success, the highest index now known to match the leader's
        /// log. Carried explicitly rather than inferred by the leader,
        /// because a delayed duplicate response would otherwise advance
        /// `match_index` past what that follower actually acknowledged.
        match_index: LogIndex,
        /// On rejection, the first index of the conflicting term — where the
        /// leader should retry from.
        conflict_index: LogIndex,
        /// On rejection, the term found at the conflict point, or
        /// [`Term::INITIAL`] when the follower's log is simply too short.
        conflict_term: Term,
    },
    /// The compaction extension: the leader's snapshot, sent when the
    /// follower needs entries the leader has already discarded.
    #[oxicode(variant = 6)]
    InstallSnapshot {
        /// The leader's term.
        term: Term,
        /// The snapshot, metadata and bytes together.
        snapshot: Box<Snapshot>,
    },
    /// The answer to [`RaftMessage::InstallSnapshot`].
    #[oxicode(variant = 7)]
    InstallSnapshotResponse {
        /// The responder's current term.
        term: Term,
        /// The index the responder's log now starts after.
        last_index: LogIndex,
    },
    /// "Campaign now" — sent by a leader stepping aside deliberately, so a
    /// chosen successor need not wait out an election timeout.
    #[oxicode(variant = 8)]
    TimeoutNow {
        /// The leader's term.
        term: Term,
    },
}

impl RaftMessage {
    /// The term this message carries.
    ///
    /// Every handler's first act is to compare this against its own term, so
    /// it is worth one accessor rather than nine match arms at each call
    /// site.
    #[must_use]
    pub const fn term(&self) -> Term {
        match self {
            Self::PreVoteRequest { term, .. }
            | Self::PreVoteResponse { term, .. }
            | Self::VoteRequest { term, .. }
            | Self::VoteResponse { term, .. }
            | Self::AppendEntries { term, .. }
            | Self::AppendEntriesResponse { term, .. }
            | Self::InstallSnapshot { term, .. }
            | Self::InstallSnapshotResponse { term, .. }
            | Self::TimeoutNow { term } => *term,
        }
    }

    /// Whether this is a response rather than a request.
    #[must_use]
    pub const fn is_response(&self) -> bool {
        matches!(
            self,
            Self::PreVoteResponse { .. }
                | Self::VoteResponse { .. }
                | Self::AppendEntriesResponse { .. }
                | Self::InstallSnapshotResponse { .. }
        )
    }

    /// Whether this is a pre-vote message.
    ///
    /// Pre-vote traffic is *hypothetical*: a request at `term + 1` must never
    /// make the receiver adopt that term, and a response must never make the
    /// sender step down. Both rules key off this predicate.
    #[must_use]
    pub const fn is_pre_vote(&self) -> bool {
        matches!(
            self,
            Self::PreVoteRequest { .. } | Self::PreVoteResponse { .. }
        )
    }

    /// Whether this is an `AppendEntries` carrying no entries.
    #[must_use]
    pub fn is_heartbeat(&self) -> bool {
        matches!(self, Self::AppendEntries { entries, .. } if entries.is_empty())
    }

    /// A stable name for logs, metrics labels and test assertions.
    #[must_use]
    pub const fn variant_name(&self) -> &'static str {
        match self {
            Self::PreVoteRequest { .. } => "PreVoteRequest",
            Self::PreVoteResponse { .. } => "PreVoteResponse",
            Self::VoteRequest { .. } => "VoteRequest",
            Self::VoteResponse { .. } => "VoteResponse",
            Self::AppendEntries { .. } => "AppendEntries",
            Self::AppendEntriesResponse { .. } => "AppendEntriesResponse",
            Self::InstallSnapshot { .. } => "InstallSnapshot",
            Self::InstallSnapshotResponse { .. } => "InstallSnapshotResponse",
            Self::TimeoutNow { .. } => "TimeoutNow",
        }
    }

    /// How many log entries this message carries, for metrics and for the
    /// simulator's own accounting.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        match self {
            Self::AppendEntries { entries, .. } => entries.len(),
            _ => 0,
        }
    }
}

impl fmt::Display for RaftMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.variant_name(), self.term().get())
    }
}

/// One [`RaftMessage`] addressed from one peer to another.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct Envelope {
    /// The sender — also the `candidateId`/`leaderId` of Figure 2.
    pub from: PeerId,
    /// The intended recipient.
    pub to: PeerId,
    /// The message itself.
    pub message: RaftMessage,
}

impl Envelope {
    /// An envelope with explicit parts.
    #[must_use]
    pub const fn new(from: PeerId, to: PeerId, message: RaftMessage) -> Self {
        Self { from, to, message }
    }

    /// The term the carried message declares.
    #[must_use]
    pub const fn term(&self) -> Term {
        self.message.term()
    }
}

impl fmt::Display for Envelope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}→{} {}", self.from.get(), self.to.get(), self.message)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::log::entry::SnapshotMeta;
    use crate::membership::Membership;
    use astrs_wire::codec::round_trip;

    fn every_variant() -> Vec<RaftMessage> {
        vec![
            RaftMessage::PreVoteRequest {
                term: Term::new(5),
                last_log_index: LogIndex::new(9),
                last_log_term: Term::new(4),
            },
            RaftMessage::PreVoteResponse {
                term: Term::new(4),
                granted: true,
            },
            RaftMessage::VoteRequest {
                term: Term::new(5),
                last_log_index: LogIndex::new(9),
                last_log_term: Term::new(4),
            },
            RaftMessage::VoteResponse {
                term: Term::new(5),
                granted: false,
            },
            RaftMessage::AppendEntries {
                term: Term::new(5),
                prev_log_index: LogIndex::new(9),
                prev_log_term: Term::new(4),
                entries: vec![LogEntry::command(
                    Term::new(5),
                    LogIndex::new(10),
                    vec![1, 2],
                )],
                leader_commit: LogIndex::new(9),
            },
            RaftMessage::AppendEntriesResponse {
                term: Term::new(5),
                success: false,
                match_index: LogIndex::ZERO,
                conflict_index: LogIndex::new(7),
                conflict_term: Term::new(3),
            },
            RaftMessage::InstallSnapshot {
                term: Term::new(5),
                snapshot: Box::new(Snapshot::new(
                    SnapshotMeta::new(
                        LogIndex::new(20),
                        Term::new(4),
                        Membership::new([PeerId::new(1)]),
                    ),
                    vec![7; 8],
                )),
            },
            RaftMessage::InstallSnapshotResponse {
                term: Term::new(5),
                last_index: LogIndex::new(20),
            },
            RaftMessage::TimeoutNow { term: Term::new(5) },
        ]
    }

    #[test]
    fn every_variant_survives_the_wire_codec() {
        for message in every_variant() {
            assert_eq!(round_trip(&message).unwrap(), message);
        }
    }

    #[test]
    fn envelopes_survive_the_wire_codec() {
        for message in every_variant() {
            let envelope = Envelope::new(PeerId::new(1), PeerId::new(7), message);
            assert_eq!(round_trip(&envelope).unwrap(), envelope);
        }
    }

    #[test]
    fn every_variant_exposes_its_term() {
        for message in every_variant() {
            // Nothing carries term 0 in this table, so an accessor that
            // silently returned a default would be caught here.
            assert!(message.term().get() > 0, "{message}");
        }
    }

    #[test]
    fn responses_are_classified_exactly() {
        let responses: Vec<&'static str> = every_variant()
            .iter()
            .filter(|message| message.is_response())
            .map(RaftMessage::variant_name)
            .collect();
        assert_eq!(
            responses,
            [
                "PreVoteResponse",
                "VoteResponse",
                "AppendEntriesResponse",
                "InstallSnapshotResponse"
            ]
        );
    }

    #[test]
    fn pre_vote_traffic_is_identifiable_in_both_directions() {
        let pre_vote: Vec<&'static str> = every_variant()
            .iter()
            .filter(|message| message.is_pre_vote())
            .map(RaftMessage::variant_name)
            .collect();
        assert_eq!(pre_vote, ["PreVoteRequest", "PreVoteResponse"]);
    }

    #[test]
    fn a_heartbeat_is_an_append_with_no_entries() {
        let heartbeat = RaftMessage::AppendEntries {
            term: Term::new(1),
            prev_log_index: LogIndex::ZERO,
            prev_log_term: Term::INITIAL,
            entries: Vec::new(),
            leader_commit: LogIndex::ZERO,
        };
        assert!(heartbeat.is_heartbeat());
        assert_eq!(heartbeat.entry_count(), 0);

        let replication = RaftMessage::AppendEntries {
            term: Term::new(1),
            prev_log_index: LogIndex::ZERO,
            prev_log_term: Term::INITIAL,
            entries: vec![LogEntry::command(Term::new(1), LogIndex::new(1), vec![])],
            leader_commit: LogIndex::ZERO,
        };
        assert!(!replication.is_heartbeat());
        assert_eq!(replication.entry_count(), 1);
        assert_eq!(
            RaftMessage::TimeoutNow { term: Term::new(1) }.entry_count(),
            0
        );
    }

    #[test]
    fn variant_names_cover_the_whole_enum() {
        let names: std::collections::BTreeSet<&'static str> = every_variant()
            .iter()
            .map(RaftMessage::variant_name)
            .collect();
        assert_eq!(names.len(), 9, "one sample per variant, all distinct");
    }

    #[test]
    fn display_names_the_variant_and_its_term() {
        let message = RaftMessage::VoteRequest {
            term: Term::new(6),
            last_log_index: LogIndex::ZERO,
            last_log_term: Term::INITIAL,
        };
        assert_eq!(message.to_string(), "VoteRequest@6");
        let envelope = Envelope::new(PeerId::new(2), PeerId::new(3), message);
        assert_eq!(envelope.to_string(), "2→3 VoteRequest@6");
        assert_eq!(envelope.term(), Term::new(6));
    }
}
