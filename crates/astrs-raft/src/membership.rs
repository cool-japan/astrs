//! Who votes: [`Membership`], quorum arithmetic, and single-server changes.
//!
//! # Why single-server changes, not joint consensus
//!
//! Raft's original paper changes membership with a *joint consensus* config
//! that is a member of both the old and the new cluster at once. Ongaro's
//! dissertation (§4.3) then shows that restricting every change to **adding
//! or removing exactly one server** makes joint consensus unnecessary: any
//! old-majority and any new-majority must overlap when the two
//! configurations differ by one member, so there is no window in which two
//! disjoint majorities exist. That is the algorithm implemented here, and
//! [`MembershipChange`] can express nothing else — a caller cannot reach the
//! unsafe multi-server case by mistake.
//!
//! # Applied on append, not on commit
//!
//! A configuration entry takes effect the moment it is **appended** to a
//! peer's log, before it is known to be committed. This is deliberate and is
//! the dissertation's rule: a server removed by an uncommitted change must
//! still be able to help commit that very change, and a leader that waited
//! for commit before adopting the new configuration would be counting the
//! wrong quorum while doing so. The cost is that an appended-then-discarded
//! configuration must be *rolled back*, which is why [`crate::RaftNode`]
//! recomputes membership from the log whenever it truncates
//! (see [`Membership::from_entries`]).
//!
//! # Examples
//!
//! ```
//! use astrs_raft::{Membership, MembershipChange, PeerId};
//!
//! let three = Membership::new([PeerId::new(1), PeerId::new(2), PeerId::new(3)]);
//! assert_eq!(three.quorum(), 2);
//! assert!(three.is_quorum(2));
//! assert!(!three.is_quorum(1));
//!
//! let four = three.apply(MembershipChange::Add(PeerId::new(4)))?;
//! assert_eq!(four.quorum(), 3);
//! # Ok::<(), astrs_raft::RaftError>(())
//! ```

use std::collections::BTreeSet;
use std::fmt;

use oxicode::{Decode, Encode};

use crate::error::{RaftError, Result};
use crate::types::PeerId;

/// The set of peers entitled to vote and to be counted toward a quorum.
///
/// Ordered (a [`BTreeSet`]) rather than hashed, because every replica must
/// derive **byte-identical** encodings of the same configuration: a hashed
/// set's iteration order would make the same membership encode differently
/// on two machines and break the log-matching property the moment a config
/// entry's bytes were compared.
#[derive(Debug, Clone, PartialEq, Eq, Default, Encode, Decode)]
pub struct Membership {
    /// The voters, in id order.
    voters: BTreeSet<PeerId>,
}

impl Membership {
    /// A configuration over `voters`.
    #[must_use]
    pub fn new(voters: impl IntoIterator<Item = PeerId>) -> Self {
        Self {
            voters: voters.into_iter().collect(),
        }
    }

    /// The voters, in id order.
    #[must_use]
    pub const fn voters(&self) -> &BTreeSet<PeerId> {
        &self.voters
    }

    /// How many voters this configuration has.
    #[must_use]
    pub fn len(&self) -> usize {
        self.voters.len()
    }

    /// Whether the configuration has no voters at all — the state a replica
    /// starts in before its first configuration entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.voters.is_empty()
    }

    /// Whether `peer` may vote in this configuration.
    #[must_use]
    pub fn contains(&self, peer: PeerId) -> bool {
        self.voters.contains(&peer)
    }

    /// The number of votes a decision needs: a strict majority.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_raft::{Membership, PeerId};
    ///
    /// assert_eq!(Membership::new([PeerId::new(1)]).quorum(), 1);
    /// assert_eq!(Membership::new((1..=3).map(PeerId::new)).quorum(), 2);
    /// assert_eq!(Membership::new((1..=4).map(PeerId::new)).quorum(), 3);
    /// assert_eq!(Membership::new((1..=5).map(PeerId::new)).quorum(), 3);
    /// ```
    #[must_use]
    pub fn quorum(&self) -> usize {
        self.voters.len() / 2 + 1
    }

    /// Whether `votes` is enough to decide.
    #[must_use]
    pub fn is_quorum(&self, votes: usize) -> bool {
        !self.voters.is_empty() && votes >= self.quorum()
    }

    /// The configuration that results from `change`.
    ///
    /// # Errors
    ///
    /// [`RaftError::InvalidMembership`] when the change is a no-op (adding a
    /// member already present, removing one that is absent) or would empty
    /// the cluster. A no-op is refused rather than accepted silently because
    /// a caller that believes it changed the configuration and did not will
    /// wait forever for a change that never commits.
    pub fn apply(&self, change: MembershipChange) -> Result<Self> {
        let mut next = self.clone();
        match change {
            MembershipChange::Add(peer) => {
                if !next.voters.insert(peer) {
                    return Err(RaftError::InvalidMembership {
                        reason: "that peer is already a voter",
                    });
                }
            }
            MembershipChange::Remove(peer) => {
                if !next.voters.remove(&peer) {
                    return Err(RaftError::InvalidMembership {
                        reason: "that peer is not a voter",
                    });
                }
                if next.voters.is_empty() {
                    return Err(RaftError::InvalidMembership {
                        reason: "removing the last voter would leave no cluster",
                    });
                }
            }
        }
        Ok(next)
    }

    /// The configuration in force after appending `entries` to a log whose
    /// membership was `self`.
    ///
    /// Used after a log truncation, when configuration entries that were
    /// appended (and therefore already in force) turn out to belong to a
    /// term that lost: the surviving prefix is replayed to recover the
    /// configuration that prefix implies.
    #[must_use]
    pub fn from_entries<'a>(
        base: &Self,
        entries: impl IntoIterator<Item = &'a crate::log::LogEntry>,
    ) -> Self {
        let mut current = base.clone();
        for entry in entries {
            if let crate::log::EntryPayload::Config(membership) = entry.payload() {
                current = membership.clone();
            }
        }
        current
    }
}

impl fmt::Display for Membership {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{")?;
        for (position, peer) in self.voters.iter().enumerate() {
            if position > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}", peer.get())?;
        }
        write!(f, "}}")
    }
}

impl FromIterator<PeerId> for Membership {
    fn from_iter<T: IntoIterator<Item = PeerId>>(iter: T) -> Self {
        Self::new(iter)
    }
}

/// A single-server membership change — the only shape this crate accepts.
///
/// # Examples
///
/// ```
/// use astrs_raft::{MembershipChange, PeerId};
///
/// let add = MembershipChange::Add(PeerId::new(4));
/// assert_eq!(add.peer(), PeerId::new(4));
/// assert!(add.is_add());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
#[non_exhaustive]
pub enum MembershipChange {
    /// Add one voter.
    #[oxicode(variant = 0)]
    Add(PeerId),
    /// Remove one voter.
    #[oxicode(variant = 1)]
    Remove(PeerId),
}

impl MembershipChange {
    /// The peer being added or removed.
    #[must_use]
    pub const fn peer(self) -> PeerId {
        match self {
            Self::Add(peer) | Self::Remove(peer) => peer,
        }
    }

    /// Whether this change adds a voter.
    #[must_use]
    pub const fn is_add(self) -> bool {
        matches!(self, Self::Add(_))
    }
}

impl fmt::Display for MembershipChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Add(peer) => write!(f, "add {peer}"),
            Self::Remove(peer) => write!(f, "remove {peer}"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::log::{EntryPayload, LogEntry};
    use crate::types::{LogIndex, Term};

    fn peers(range: std::ops::RangeInclusive<u64>) -> Membership {
        Membership::new(range.map(PeerId::new))
    }

    #[test]
    fn quorum_is_a_strict_majority_at_every_size() {
        for size in 1u64..=9 {
            let membership = peers(1..=size);
            let quorum = membership.quorum();
            assert!(
                quorum * 2 > size as usize,
                "size {size} quorum {quorum} is not a strict majority"
            );
            assert!(
                (quorum - 1) * 2 <= size as usize,
                "size {size} quorum {quorum} is larger than it needs to be"
            );
        }
    }

    #[test]
    fn an_empty_configuration_can_never_reach_quorum() {
        let empty = Membership::default();
        assert!(empty.is_empty());
        assert!(!empty.is_quorum(0));
        assert!(!empty.is_quorum(100));
    }

    #[test]
    fn adding_and_removing_are_the_only_shapes_and_both_are_checked() {
        let three = peers(1..=3);
        let four = three.apply(MembershipChange::Add(PeerId::new(4))).unwrap();
        assert_eq!(four.len(), 4);
        assert!(four.contains(PeerId::new(4)));

        let back = four
            .apply(MembershipChange::Remove(PeerId::new(4)))
            .unwrap();
        assert_eq!(back, three);
    }

    #[test]
    fn a_no_op_change_is_refused_rather_than_silently_accepted() {
        let three = peers(1..=3);
        // Adding an existing voter: the caller would otherwise wait forever
        // for a change that never alters anything.
        let error = three
            .apply(MembershipChange::Add(PeerId::new(2)))
            .unwrap_err();
        assert!(matches!(error, RaftError::InvalidMembership { .. }));

        let error = three
            .apply(MembershipChange::Remove(PeerId::new(9)))
            .unwrap_err();
        assert!(matches!(error, RaftError::InvalidMembership { .. }));
    }

    #[test]
    fn the_last_voter_cannot_be_removed() {
        let one = peers(1..=1);
        let error = one
            .apply(MembershipChange::Remove(PeerId::new(1)))
            .unwrap_err();
        assert!(matches!(error, RaftError::InvalidMembership { .. }));
    }

    #[test]
    fn membership_is_recovered_by_replaying_config_entries() {
        let base = peers(1..=3);
        let four = base.apply(MembershipChange::Add(PeerId::new(4))).unwrap();
        let five = four.apply(MembershipChange::Add(PeerId::new(5))).unwrap();

        let entries = vec![
            LogEntry::new(Term::new(1), LogIndex::new(1), EntryPayload::Noop),
            LogEntry::new(
                Term::new(1),
                LogIndex::new(2),
                EntryPayload::Config(four.clone()),
            ),
            LogEntry::new(
                Term::new(2),
                LogIndex::new(3),
                EntryPayload::Config(five.clone()),
            ),
        ];
        assert_eq!(Membership::from_entries(&base, &entries), five);
        // Dropping the last entry (a truncation) rolls the configuration
        // back to what the surviving prefix implies.
        assert_eq!(Membership::from_entries(&base, &entries[..2]), four);
        assert_eq!(Membership::from_entries(&base, &entries[..1]), base);
    }

    #[test]
    fn display_is_stable_and_ordered() {
        let membership = Membership::new([PeerId::new(3), PeerId::new(1), PeerId::new(2)]);
        assert_eq!(membership.to_string(), "{1, 2, 3}");
        assert_eq!(
            MembershipChange::Remove(PeerId::new(2)).to_string(),
            "remove peer 2"
        );
    }

    #[test]
    fn changes_report_their_peer_and_direction() {
        assert!(MembershipChange::Add(PeerId::new(1)).is_add());
        assert!(!MembershipChange::Remove(PeerId::new(1)).is_add());
        assert_eq!(
            MembershipChange::Remove(PeerId::new(8)).peer(),
            PeerId::new(8)
        );
    }

    #[test]
    fn membership_collects_from_an_iterator() {
        let membership: Membership = [PeerId::new(2), PeerId::new(1)].into_iter().collect();
        assert_eq!(membership, peers(1..=2));
    }
}
