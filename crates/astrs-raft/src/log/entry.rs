//! The three durable shapes: [`LogEntry`], [`HardState`] and
//! [`SnapshotMeta`].
//!
//! Figure 2 names exactly three things a Raft peer must have on stable
//! storage before it answers an RPC: `currentTerm`, `votedFor` and `log[]`.
//! The first two are [`HardState`]; the third is a sequence of [`LogEntry`].
//! [`SnapshotMeta`] is the fourth thing the extended algorithm needs — the
//! `(index, term)` a compacted log prefix used to end at, without which a
//! peer cannot answer `prev_log_term` for the entry after a snapshot.
//!
//! All three carry explicit `#[oxicode(variant = N)]` / field ordering and
//! are treated as **append-only** (blueprint design principle 4): they are
//! durable state that must stay decodable across a coordinator upgrade, so
//! their encodings are frozen exactly like a wire enum's.
//!
//! # Examples
//!
//! ```
//! use astrs_raft::{EntryPayload, LogEntry, LogIndex, Term};
//!
//! let entry = LogEntry::command(Term::new(2), LogIndex::new(7), b"set gain 3".to_vec());
//! assert_eq!(entry.term(), Term::new(2));
//! assert_eq!(entry.index(), LogIndex::new(7));
//! assert_eq!(entry.command_bytes(), Some(&b"set gain 3"[..]));
//! assert!(!entry.is_config());
//!
//! let noop = LogEntry::new(Term::new(2), LogIndex::new(8), EntryPayload::Noop);
//! assert_eq!(noop.command_bytes(), None);
//! ```

use core::fmt;

use oxicode::{Decode, Encode};

use crate::membership::Membership;
use crate::types::{LogIndex, PeerId, Term};

/// What a log entry carries.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
#[non_exhaustive]
pub enum EntryPayload {
    /// An opaque state-machine command. This crate never looks inside it.
    #[oxicode(variant = 0)]
    Command(Vec<u8>),
    /// The no-op a new leader appends the moment it takes office.
    ///
    /// Raft leaders may not consider entries from *earlier* terms committed
    /// by replication count alone (the Figure 8 hazard). Committing one
    /// entry of the leader's own term settles every earlier entry with it,
    /// and a no-op is the cheapest such entry — it also gives the leader a
    /// concrete index to wait on before serving reads, which is what makes a
    /// freshly elected leader's view of the state machine up to date.
    #[oxicode(variant = 1)]
    Noop,
    /// A new cluster configuration, in force from the moment this entry is
    /// *appended* (see [`crate::membership`] for why not on commit).
    #[oxicode(variant = 2)]
    Config(Membership),
}

impl EntryPayload {
    /// A stable lower-case name for logs and metrics.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Command(_) => "command",
            Self::Noop => "noop",
            Self::Config(_) => "config",
        }
    }
}

/// One entry in the replicated log.
///
/// The index is stored in the entry rather than implied by its position, so
/// a record read back from the middle of a write-ahead log is
/// self-describing: recovery does not have to have parsed every preceding
/// record correctly to know where this one belongs.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct LogEntry {
    /// The term the leader that created this entry was in.
    term: Term,
    /// This entry's position in the log.
    index: LogIndex,
    /// What the entry carries.
    payload: EntryPayload,
}

impl LogEntry {
    /// An entry with an explicit payload.
    #[must_use]
    pub const fn new(term: Term, index: LogIndex, payload: EntryPayload) -> Self {
        Self {
            term,
            index,
            payload,
        }
    }

    /// A state-machine command entry.
    #[must_use]
    pub const fn command(term: Term, index: LogIndex, command: Vec<u8>) -> Self {
        Self::new(term, index, EntryPayload::Command(command))
    }

    /// The term this entry was created in.
    #[must_use]
    pub const fn term(&self) -> Term {
        self.term
    }

    /// This entry's log position.
    #[must_use]
    pub const fn index(&self) -> LogIndex {
        self.index
    }

    /// What this entry carries.
    #[must_use]
    pub const fn payload(&self) -> &EntryPayload {
        &self.payload
    }

    /// The command bytes, when this entry carries a command.
    #[must_use]
    pub fn command_bytes(&self) -> Option<&[u8]> {
        match &self.payload {
            EntryPayload::Command(bytes) => Some(bytes),
            EntryPayload::Noop | EntryPayload::Config(_) => None,
        }
    }

    /// The configuration this entry installs, when it is a config entry.
    #[must_use]
    pub const fn config(&self) -> Option<&Membership> {
        match &self.payload {
            EntryPayload::Config(membership) => Some(membership),
            _ => None,
        }
    }

    /// Whether this entry changes the cluster configuration.
    #[must_use]
    pub const fn is_config(&self) -> bool {
        matches!(self.payload, EntryPayload::Config(_))
    }
}

impl fmt::Display for LogEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}@{}/{}",
            self.payload.as_str(),
            self.index.get(),
            self.term.get()
        )
    }
}

/// The two values Figure 2 requires on stable storage before a peer answers
/// any RPC: the term it is in, and who it voted for in that term.
///
/// Persisting `voted_for` is what stops a peer from voting twice in one term
/// across a crash — the single most direct route to two leaders at once.
///
/// # Examples
///
/// ```
/// use astrs_raft::{HardState, PeerId, Term};
///
/// let fresh = HardState::default();
/// assert_eq!(fresh.term, Term::INITIAL);
/// assert!(fresh.voted_for.is_none());
///
/// let voted = HardState::new(Term::new(3), Some(PeerId::new(2)));
/// assert!(voted.has_voted_for(PeerId::new(2)));
/// assert!(!voted.has_voted_for(PeerId::new(1)));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Encode, Decode)]
pub struct HardState {
    /// The latest term this peer has seen.
    pub term: Term,
    /// Who this peer voted for in `term`, if anyone.
    pub voted_for: Option<PeerId>,
}

impl HardState {
    /// A hard state with explicit values.
    #[must_use]
    pub const fn new(term: Term, voted_for: Option<PeerId>) -> Self {
        Self { term, voted_for }
    }

    /// Whether this peer has already voted for `candidate` in its current
    /// term — the "or already voted for this candidate" half of Figure 2's
    /// `RequestVote` rule, which makes a duplicated request idempotent.
    #[must_use]
    pub fn has_voted_for(&self, candidate: PeerId) -> bool {
        self.voted_for == Some(candidate)
    }

    /// Whether a vote may still be cast in this term.
    #[must_use]
    pub const fn can_vote(&self) -> bool {
        self.voted_for.is_none()
    }
}

impl fmt::Display for HardState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.voted_for {
            Some(peer) => write!(f, "{} voted {}", self.term, peer),
            None => write!(f, "{} unvoted", self.term),
        }
    }
}

/// What a snapshot replaces: everything up to and including
/// [`SnapshotMeta::last_index`].
///
/// The `(index, term)` pair is not redundant with the state-machine bytes: a
/// leader replicating the entry *after* a snapshot must state that entry's
/// `prev_log_term`, and after compaction the only place that term still
/// exists is here.
///
/// The membership is carried too, because a peer restored from a snapshot
/// has no configuration entries left in its log to derive one from.
///
/// # Examples
///
/// ```
/// use astrs_raft::{Membership, PeerId, SnapshotMeta, LogIndex, Term};
///
/// let meta = SnapshotMeta::new(
///     LogIndex::new(40),
///     Term::new(3),
///     Membership::new([PeerId::new(1), PeerId::new(2)]),
/// );
/// assert!(meta.covers(LogIndex::new(40)));
/// assert!(!meta.covers(LogIndex::new(41)));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default, Encode, Decode)]
pub struct SnapshotMeta {
    /// The last log index folded into this snapshot.
    last_index: LogIndex,
    /// The term of the entry at [`SnapshotMeta::last_index`].
    last_term: Term,
    /// The configuration in force as of that index.
    membership: Membership,
}

impl SnapshotMeta {
    /// Snapshot metadata with explicit values.
    #[must_use]
    pub const fn new(last_index: LogIndex, last_term: Term, membership: Membership) -> Self {
        Self {
            last_index,
            last_term,
            membership,
        }
    }

    /// The last log index folded into this snapshot.
    #[must_use]
    pub const fn last_index(&self) -> LogIndex {
        self.last_index
    }

    /// The term of the entry at [`SnapshotMeta::last_index`].
    #[must_use]
    pub const fn last_term(&self) -> Term {
        self.last_term
    }

    /// The configuration in force as of [`SnapshotMeta::last_index`].
    #[must_use]
    pub const fn membership(&self) -> &Membership {
        &self.membership
    }

    /// Whether `index` is already folded into this snapshot.
    #[must_use]
    pub fn covers(&self, index: LogIndex) -> bool {
        index <= self.last_index
    }
}

impl fmt::Display for SnapshotMeta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "snapshot through {}/{} over {}",
            self.last_index.get(),
            self.last_term.get(),
            self.membership
        )
    }
}

/// A snapshot's metadata together with the state-machine bytes it describes.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct Snapshot {
    /// What this snapshot replaces.
    pub meta: SnapshotMeta,
    /// The state machine's serialized form as of
    /// [`SnapshotMeta::last_index`].
    pub data: Vec<u8>,
}

impl Snapshot {
    /// A snapshot with explicit parts.
    #[must_use]
    pub const fn new(meta: SnapshotMeta, data: Vec<u8>) -> Self {
        Self { meta, data }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use astrs_wire::codec::round_trip;

    #[test]
    fn every_payload_shape_survives_the_codec() {
        for payload in [
            EntryPayload::Command(vec![1, 2, 3]),
            EntryPayload::Command(Vec::new()),
            EntryPayload::Noop,
            EntryPayload::Config(Membership::new([PeerId::new(1), PeerId::new(2)])),
        ] {
            let entry = LogEntry::new(Term::new(9), LogIndex::new(4), payload);
            assert_eq!(round_trip(&entry).unwrap(), entry);
        }
    }

    #[test]
    fn an_entry_is_self_describing_about_its_own_position() {
        // Recovery reads records out of a byte stream: an entry that only
        // knew its payload could not be placed without a perfect parse of
        // everything before it.
        let entry = LogEntry::command(Term::new(3), LogIndex::new(11), vec![7]);
        let decoded = round_trip(&entry).unwrap();
        assert_eq!(decoded.index(), LogIndex::new(11));
        assert_eq!(decoded.term(), Term::new(3));
    }

    #[test]
    fn payload_accessors_are_exclusive() {
        let command = LogEntry::command(Term::new(1), LogIndex::new(1), vec![4, 5]);
        assert_eq!(command.command_bytes(), Some(&[4u8, 5][..]));
        assert!(command.config().is_none());
        assert!(!command.is_config());

        let membership = Membership::new([PeerId::new(1)]);
        let config = LogEntry::new(
            Term::new(1),
            LogIndex::new(2),
            EntryPayload::Config(membership.clone()),
        );
        assert_eq!(config.config(), Some(&membership));
        assert!(config.command_bytes().is_none());
        assert!(config.is_config());

        let noop = LogEntry::new(Term::new(1), LogIndex::new(3), EntryPayload::Noop);
        assert!(noop.command_bytes().is_none());
        assert!(noop.config().is_none());
    }

    #[test]
    fn payload_names_are_stable() {
        assert_eq!(EntryPayload::Noop.as_str(), "noop");
        assert_eq!(EntryPayload::Command(vec![]).as_str(), "command");
        assert_eq!(
            EntryPayload::Config(Membership::default()).as_str(),
            "config"
        );
    }

    #[test]
    fn hard_state_round_trips_and_answers_the_vote_rule() {
        let state = HardState::new(Term::new(4), Some(PeerId::new(2)));
        assert_eq!(round_trip(&state).unwrap(), state);
        assert!(state.has_voted_for(PeerId::new(2)));
        assert!(!state.has_voted_for(PeerId::new(3)));
        assert!(!state.can_vote());
        assert!(HardState::default().can_vote());
        assert_eq!(state.to_string(), "term 4 voted peer 2");
        assert_eq!(HardState::default().to_string(), "term 0 unvoted");
    }

    #[test]
    fn snapshot_metadata_states_what_it_covers() {
        let meta = SnapshotMeta::new(
            LogIndex::new(12),
            Term::new(2),
            Membership::new([PeerId::new(1)]),
        );
        assert!(meta.covers(LogIndex::new(1)));
        assert!(meta.covers(LogIndex::new(12)));
        assert!(!meta.covers(LogIndex::new(13)));
        assert_eq!(meta.last_term(), Term::new(2));
        assert_eq!(meta.membership().len(), 1);
        assert_eq!(round_trip(&meta).unwrap(), meta);
    }

    #[test]
    fn snapshots_carry_their_bytes() {
        let snapshot = Snapshot::new(SnapshotMeta::default(), vec![9; 32]);
        assert_eq!(round_trip(&snapshot).unwrap(), snapshot);
    }

    #[test]
    fn display_is_compact() {
        let entry = LogEntry::command(Term::new(2), LogIndex::new(7), vec![]);
        assert_eq!(entry.to_string(), "command@7/2");
    }
}
