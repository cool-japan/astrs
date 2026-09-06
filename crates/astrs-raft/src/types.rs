//! The three scalars every Raft invariant is expressed against: [`Term`],
//! [`PeerId`] and [`LogIndex`].
//!
//! Raft's safety argument never talks about wall-clock time, byte offsets or
//! socket addresses. It talks about *terms* (which election is current),
//! *indices* (where in the replicated log an entry sits) and *peers* (who
//! voted, who acknowledged). Giving each of those a newtype with the
//! comparisons spelled as named methods is what stops the classic
//! off-by-one-and-inverted-comparison family of Raft bugs: no call site in
//! this crate ever has to re-derive which direction of `<` means "stale".
//!
//! # Examples
//!
//! ```
//! use astrs_raft::{LogIndex, PeerId, Term};
//!
//! let term = Term::new(4);
//! assert!(term.is_stale_against(Term::new(5)));
//! assert!(term.rejects(Term::new(3)));
//!
//! // Indices start at 1; index 0 is the "empty log" sentinel.
//! assert_eq!(LogIndex::ZERO.next(), LogIndex::new(1));
//! assert_eq!(PeerId::new(2).to_string(), "peer 2");
//! ```

use core::fmt;

use oxicode::{Decode, Encode};

/// A Raft term: the monotonic election counter every safety argument in the
/// protocol is expressed against.
///
/// A term begins when a candidate stands for election and ends when a higher
/// term is observed anywhere in the cluster. Terms are totally ordered, and
/// that order is the whole rule: a peer seeing a term greater than its own
/// steps down to follower before processing the message; a message carrying a
/// term lower than the receiver's is rejected without being applied.
///
/// # Examples
///
/// ```
/// use astrs_raft::Term;
///
/// let start = Term::INITIAL;
/// assert_eq!(start.get(), 0);
/// assert_eq!(start.next(), Term::new(1));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Encode, Decode)]
pub struct Term(u64);

impl Term {
    /// The term a freshly initialized peer starts in, before any election has
    /// ever been held (Raft's term `0`).
    pub const INITIAL: Self = Self(0);

    /// A term with an explicit value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// This term's raw counter value, for encoding onto the wire.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next term — what a candidate moves to when it stands for election.
    ///
    /// Saturating rather than wrapping: a wrapped term would silently make
    /// every peer in the cluster look stale to every other one, which is a
    /// split-brain, whereas saturating at [`u64::MAX`] merely stops elections
    /// after 2^64 of them.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Whether `observed` is strictly newer than this term — the condition
    /// under which a peer must step down to follower and adopt `observed`.
    #[must_use]
    pub const fn is_stale_against(self, observed: Self) -> bool {
        self.0 < observed.0
    }

    /// Whether a message carrying `incoming` must be rejected because it
    /// comes from an older term than this peer's own.
    #[must_use]
    pub const fn rejects(self, incoming: Self) -> bool {
        incoming.0 < self.0
    }
}

impl fmt::Display for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "term {}", self.0)
    }
}

/// A position in the replicated log.
///
/// Raft numbers log entries from `1`; [`LogIndex::ZERO`] is the sentinel for
/// "before the first entry", which is what a leader's `prev_log_index` is
/// when it replicates from the very beginning and what `last_index` reports
/// for an empty log.
///
/// # Examples
///
/// ```
/// use astrs_raft::LogIndex;
///
/// assert!(LogIndex::ZERO.is_empty_sentinel());
/// assert_eq!(LogIndex::new(7).previous(), LogIndex::new(6));
/// assert_eq!(LogIndex::ZERO.previous(), LogIndex::ZERO);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Encode, Decode)]
pub struct LogIndex(u64);

impl LogIndex {
    /// The "before the first entry" sentinel.
    pub const ZERO: Self = Self(0);

    /// The index of the first entry a brand-new log will hold.
    pub const FIRST: Self = Self(1);

    /// An index with an explicit value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw index value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Whether this is the [`LogIndex::ZERO`] sentinel.
    #[must_use]
    pub const fn is_empty_sentinel(self) -> bool {
        self.0 == 0
    }

    /// The next index, saturating at [`u64::MAX`].
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// The previous index, saturating at [`LogIndex::ZERO`].
    #[must_use]
    pub const fn previous(self) -> Self {
        Self(self.0.saturating_sub(1))
    }

    /// This index advanced by `delta`, saturating.
    #[must_use]
    pub const fn saturating_add(self, delta: u64) -> Self {
        Self(self.0.saturating_add(delta))
    }

    /// How many entries lie in `self..=other`, or `0` when `other` is not
    /// ahead of `self`.
    #[must_use]
    pub const fn distance_to(self, other: Self) -> u64 {
        other.0.saturating_sub(self.0)
    }
}

impl fmt::Display for LogIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "index {}", self.0)
    }
}

/// A Raft peer's cluster-unique identity.
///
/// Deliberately an opaque integer rather than a socket address: a peer that
/// moves to a new address is still the same voter, and the identity a vote is
/// recorded against must not change when a deployment re-homes a process. The
/// id → address mapping lives in [`crate::RaftConfig`], where a restart can
/// rewrite it without invalidating a single durable record.
///
/// # Examples
///
/// ```
/// use astrs_raft::PeerId;
///
/// let peer = PeerId::new(3);
/// assert_eq!(peer.get(), 3);
/// assert_eq!(peer.to_string(), "peer 3");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Encode, Decode)]
pub struct PeerId(u64);

impl PeerId {
    /// A peer id with an explicit value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw id value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "peer {}", self.0)
    }
}

impl From<u64> for PeerId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

/// Which of Figure 2's three states a peer is in, plus the pre-vote
/// half-state that precedes candidacy.
///
/// [`Role::PreCandidate`] is not in Figure 2. It is the pre-vote extension
/// (Ongaro's dissertation §9.6): a peer that has lost contact with the leader
/// asks the cluster whether it *would* win an election **before** bumping its
/// own term. A partitioned peer that keeps timing out therefore stops
/// disrupting a perfectly healthy leader by returning with an inflated term
/// every few seconds.
///
/// # Examples
///
/// ```
/// use astrs_raft::Role;
///
/// assert!(Role::Leader.is_leader());
/// assert!(Role::PreCandidate.is_campaigning());
/// assert!(Role::Candidate.is_campaigning());
/// assert_eq!(Role::Follower.as_str(), "follower");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Role {
    /// Passive: appends what the leader sends, votes when asked.
    #[default]
    Follower,
    /// Soliciting *hypothetical* votes at `term + 1` without having adopted
    /// that term.
    PreCandidate,
    /// Standing for election at its own, already-incremented term.
    Candidate,
    /// Replicating to the cluster.
    Leader,
}

impl Role {
    /// Whether this peer is the leader.
    #[must_use]
    pub const fn is_leader(self) -> bool {
        matches!(self, Self::Leader)
    }

    /// Whether this peer is a follower.
    #[must_use]
    pub const fn is_follower(self) -> bool {
        matches!(self, Self::Follower)
    }

    /// Whether this peer is soliciting votes, real or hypothetical.
    #[must_use]
    pub const fn is_campaigning(self) -> bool {
        matches!(self, Self::PreCandidate | Self::Candidate)
    }

    /// A stable lower-case name for logs and metrics labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Follower => "follower",
            Self::PreCandidate => "pre_candidate",
            Self::Candidate => "candidate",
            Self::Leader => "leader",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A deterministic, seeded pseudo-random generator for election timeouts.
///
/// Raft needs randomized election timeouts or split votes repeat forever;
/// this crate needs *reproducible* ones or the simulation harness in
/// [`crate::sim`] would not be a regression test. `xorshift64*` gives both:
/// the sequence is fixed by the seed, so a failing simulation seed replays
/// exactly, and it costs a handful of instructions per draw.
///
/// # Examples
///
/// ```
/// use astrs_raft::Rng;
///
/// let mut a = Rng::new(42);
/// let mut b = Rng::new(42);
/// assert_eq!(a.next_u64(), b.next_u64());
/// assert!((0..10).contains(&a.in_range(0, 10)));
/// ```
#[derive(Debug, Clone)]
pub struct Rng {
    /// The generator state; never zero (xorshift's fixed point).
    state: u64,
}

impl Rng {
    /// A generator seeded from `seed`.
    ///
    /// A zero seed is remapped, since `0` is `xorshift`'s absorbing state and
    /// would produce nothing but zeros forever.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    /// The next value in the sequence.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value in `low..high`, or `low` when the range is empty.
    pub fn in_range(&mut self, low: u64, high: u64) -> u64 {
        if high <= low {
            return low;
        }
        low + self.next_u64() % (high - low)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn a_fresh_peer_starts_at_term_zero() {
        assert_eq!(Term::INITIAL, Term::default());
        assert_eq!(Term::INITIAL.get(), 0);
    }

    #[test]
    fn standing_for_election_bumps_the_term() {
        assert_eq!(Term::new(3).next(), Term::new(4));
    }

    #[test]
    fn the_term_counter_saturates_rather_than_wrapping() {
        // Wrapping would make every peer look stale to every other one at
        // once — a split brain — where saturating merely stops elections.
        assert_eq!(Term::new(u64::MAX).next(), Term::new(u64::MAX));
    }

    #[test]
    fn a_higher_observed_term_makes_this_peer_stale() {
        assert!(Term::new(2).is_stale_against(Term::new(3)));
        assert!(!Term::new(3).is_stale_against(Term::new(3)));
        assert!(!Term::new(4).is_stale_against(Term::new(3)));
    }

    #[test]
    fn a_lower_incoming_term_is_rejected() {
        assert!(Term::new(5).rejects(Term::new(4)));
        assert!(!Term::new(5).rejects(Term::new(5)));
        assert!(!Term::new(5).rejects(Term::new(6)));
    }

    #[test]
    fn terms_are_totally_ordered() {
        let mut terms = [Term::new(9), Term::new(1), Term::new(4)];
        terms.sort_unstable();
        assert_eq!(terms, [Term::new(1), Term::new(4), Term::new(9)]);
    }

    #[test]
    fn display_names_the_term() {
        assert_eq!(Term::new(12).to_string(), "term 12");
    }

    #[test]
    fn indices_walk_forward_and_back_without_wrapping() {
        assert_eq!(LogIndex::ZERO.previous(), LogIndex::ZERO);
        assert_eq!(LogIndex::new(1).previous(), LogIndex::ZERO);
        assert_eq!(LogIndex::new(u64::MAX).next(), LogIndex::new(u64::MAX));
        assert_eq!(LogIndex::new(3).saturating_add(4), LogIndex::new(7));
    }

    #[test]
    fn distance_is_zero_when_the_other_index_is_behind() {
        assert_eq!(LogIndex::new(4).distance_to(LogIndex::new(9)), 5);
        assert_eq!(LogIndex::new(9).distance_to(LogIndex::new(4)), 0);
    }

    #[test]
    fn the_empty_sentinel_is_only_zero() {
        assert!(LogIndex::ZERO.is_empty_sentinel());
        assert!(!LogIndex::FIRST.is_empty_sentinel());
    }

    #[test]
    fn roles_classify_themselves() {
        assert!(Role::Leader.is_leader());
        assert!(!Role::Candidate.is_leader());
        assert!(Role::Follower.is_follower());
        assert!(Role::Candidate.is_campaigning());
        assert!(Role::PreCandidate.is_campaigning());
        assert!(!Role::Leader.is_campaigning());
        assert_eq!(Role::default(), Role::Follower);
    }

    #[test]
    fn role_names_are_stable() {
        let names: Vec<&str> = [
            Role::Follower,
            Role::PreCandidate,
            Role::Candidate,
            Role::Leader,
        ]
        .iter()
        .map(|role| role.as_str())
        .collect();
        assert_eq!(names, ["follower", "pre_candidate", "candidate", "leader"]);
        assert_eq!(Role::Leader.to_string(), "leader");
    }

    #[test]
    fn peer_ids_round_trip_and_display() {
        assert_eq!(PeerId::from(5).get(), 5);
        assert_eq!(PeerId::new(5).to_string(), "peer 5");
    }

    #[test]
    fn the_rng_is_reproducible_and_never_stuck_at_zero() {
        let mut seeded = Rng::new(0);
        let first: Vec<u64> = (0..8).map(|_| seeded.next_u64()).collect();
        assert!(first.iter().any(|&value| value != 0));

        let mut a = Rng::new(1234);
        let mut b = Rng::new(1234);
        for _ in 0..64 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn rng_ranges_are_respected_including_the_empty_one() {
        let mut rng = Rng::new(7);
        for _ in 0..256 {
            let value = rng.in_range(10, 20);
            assert!((10..20).contains(&value), "{value} out of range");
        }
        assert_eq!(rng.in_range(5, 5), 5);
        assert_eq!(rng.in_range(9, 3), 9);
    }
}
