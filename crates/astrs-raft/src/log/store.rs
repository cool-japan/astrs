//! [`LogStore`]: the durability contract [`crate::RaftNode`] is written
//! against, plus the in-memory implementation the simulation harness uses.
//!
//! Everything Figure 2 says must survive a crash goes through this trait:
//! `currentTerm`/`votedFor` ([`LogStore::save_hard_state`]), the log itself
//! ([`LogStore::append`], [`LogStore::truncate_from`]) and the compacted
//! prefix ([`LogStore::save_snapshot`], [`LogStore::install_snapshot`]).
//! Nothing else in the crate touches a file, which is what lets the
//! deterministic simulator in [`crate::sim`] run the *same* consensus code
//! as a production replica with [`MemoryLog`] swapped in for
//! [`crate::WalLog`].
//!
//! # The index space
//!
//! ```text
//!            snapshot.last_index
//!                    v
//!  ...  compacted ...│ first_index .......... last_index
//!                    │←──── entries actually held ────→│
//! ```
//!
//! [`LogStore::first_index`] is `snapshot.last_index + 1`, or
//! [`LogIndex::FIRST`] when nothing has been compacted.
//! [`LogStore::term_at`] answers for `snapshot.last_index` too — that one
//! term outlives its entry precisely so a leader can still state
//! `prev_log_term` for the entry that follows a snapshot.

use crate::error::{RaftError, Result};
use crate::log::entry::{HardState, LogEntry, Snapshot, SnapshotMeta};
use crate::types::{LogIndex, Term};

/// Durable storage for one Raft replica's log and hard state.
///
/// # Contract
///
/// - [`LogStore::append`] takes entries that are **contiguous** and start at
///   `last_index() + 1`. A caller that wants to overwrite a conflicting
///   suffix calls [`LogStore::truncate_from`] first.
/// - Every mutating method must be durable by the time it returns `Ok`,
///   subject to the implementation's own documented fsync policy (see
///   [`crate::FsyncPolicy`]).
/// - Reads never allocate more than the caller asked for: a corrupted or
///   forged length must never turn into a huge allocation.
pub trait LogStore {
    /// The persisted term and vote.
    fn hard_state(&self) -> HardState;

    /// Persists a new term and vote.
    ///
    /// # Errors
    ///
    /// Whatever the backing store reports; see [`RaftError`].
    fn save_hard_state(&mut self, state: HardState) -> Result<()>;

    /// Appends contiguous entries starting at `last_index() + 1`.
    ///
    /// # Errors
    ///
    /// [`RaftError::Unavailable`] if the first entry is not at
    /// `last_index() + 1`, or the entries are not contiguous.
    fn append(&mut self, entries: &[LogEntry]) -> Result<()>;

    /// Removes every entry at or after `index`.
    ///
    /// # Errors
    ///
    /// [`RaftError::Compacted`] if `index` is already inside the snapshot.
    fn truncate_from(&mut self, index: LogIndex) -> Result<()>;

    /// The entry at `index`, or `None` when it is outside the held range.
    ///
    /// # Errors
    ///
    /// Whatever the backing store reports.
    fn entry(&self, index: LogIndex) -> Result<Option<LogEntry>>;

    /// Up to `max_entries` entries starting at `from`.
    ///
    /// # Errors
    ///
    /// [`RaftError::Compacted`] if `from` is inside the snapshot.
    fn entries(&self, from: LogIndex, max_entries: usize) -> Result<Vec<LogEntry>>;

    /// The term of the entry at `index`.
    ///
    /// Answers for `snapshot.last_index` as well as for held entries, and
    /// for [`LogIndex::ZERO`] (which is [`Term::INITIAL`], the "before the
    /// log" sentinel).
    ///
    /// # Errors
    ///
    /// Whatever the backing store reports.
    fn term_at(&self, index: LogIndex) -> Result<Option<Term>>;

    /// The first index still held (`snapshot.last_index + 1`, or
    /// [`LogIndex::FIRST`]).
    fn first_index(&self) -> LogIndex;

    /// The last index held, or the snapshot's last index when the log is
    /// empty behind a snapshot, or [`LogIndex::ZERO`] for a fresh replica.
    fn last_index(&self) -> LogIndex;

    /// The term at [`LogStore::last_index`].
    fn last_term(&self) -> Term;

    /// The metadata of the snapshot this log is compacted against.
    fn snapshot_meta(&self) -> Option<SnapshotMeta>;

    /// The most recent snapshot, bytes included, for shipping to a follower
    /// that has fallen behind the compacted prefix.
    ///
    /// # Errors
    ///
    /// Whatever the backing store reports.
    fn snapshot(&self) -> Result<Option<Snapshot>>;

    /// Records a snapshot this replica took of its own state machine and
    /// compacts the log against it, **keeping** any entries after
    /// `snapshot.meta.last_index`.
    ///
    /// # Errors
    ///
    /// [`RaftError::InvalidConfig`] if the snapshot is older than one
    /// already held; otherwise whatever the backing store reports.
    fn save_snapshot(&mut self, snapshot: &Snapshot) -> Result<()>;

    /// Installs a snapshot received from a leader, discarding **all** local
    /// entries: a replica accepting one is by definition behind, and any
    /// entry it still held past the snapshot's end came from a log the
    /// leader has already overwritten.
    ///
    /// # Errors
    ///
    /// Whatever the backing store reports.
    fn install_snapshot(&mut self, snapshot: &Snapshot) -> Result<()>;

    /// Forces everything written so far to durable storage.
    ///
    /// A no-op for stores that are already synchronous.
    ///
    /// # Errors
    ///
    /// Whatever the backing store reports.
    fn sync(&mut self) -> Result<()>;
}

/// Checks that `entries` are contiguous and start where `last` expects.
///
/// Shared by every [`LogStore`] implementation so the "contiguous append"
/// half of the trait contract is enforced identically by all of them.
///
/// # Errors
///
/// [`RaftError::Unavailable`] naming the index that broke the sequence.
pub(crate) fn check_contiguous(last: LogIndex, entries: &[LogEntry]) -> Result<()> {
    let mut expected = last.next();
    for entry in entries {
        if entry.index() != expected {
            return Err(RaftError::Unavailable {
                index: entry.index(),
                last: expected.previous(),
            });
        }
        expected = expected.next();
    }
    Ok(())
}

/// An entirely in-memory [`LogStore`].
///
/// This is not a toy: it is what the deterministic simulator runs on, and
/// what an embedded single-process coordinator uses when it has no store
/// directory. It implements the same contract [`crate::WalLog`] does, with
/// [`LogStore::sync`] a no-op — which is exactly the honest statement that
/// its durability ends when the process does.
///
/// # Examples
///
/// ```
/// use astrs_raft::{LogEntry, LogIndex, LogStore, MemoryLog, Term};
///
/// let mut log = MemoryLog::new();
/// assert_eq!(log.last_index(), LogIndex::ZERO);
///
/// log.append(&[LogEntry::command(Term::new(1), LogIndex::new(1), vec![7])])?;
/// assert_eq!(log.last_index(), LogIndex::new(1));
/// assert_eq!(log.last_term(), Term::new(1));
/// assert_eq!(log.entry(LogIndex::new(1))?.map(|e| e.term()), Some(Term::new(1)));
/// # Ok::<(), astrs_raft::RaftError>(())
/// ```
#[derive(Debug, Clone, Default)]
pub struct MemoryLog {
    /// The persisted term and vote.
    hard_state: HardState,
    /// Entries held, ascending; `entries[0].index() == first_index()`.
    entries: Vec<LogEntry>,
    /// The snapshot the log is compacted against.
    snapshot: Option<Snapshot>,
}

impl MemoryLog {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many entries are currently held (excluding the compacted
    /// prefix).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no entries are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every held entry, for tests and invariant checks.
    #[must_use]
    pub fn held_entries(&self) -> &[LogEntry] {
        &self.entries
    }

    /// The position of `index` within [`MemoryLog::entries`], if held.
    fn position(&self, index: LogIndex) -> Option<usize> {
        let first = self.first_index();
        if index < first {
            return None;
        }
        usize::try_from(first.distance_to(index)).ok()
    }
}

impl LogStore for MemoryLog {
    fn hard_state(&self) -> HardState {
        self.hard_state
    }

    fn save_hard_state(&mut self, state: HardState) -> Result<()> {
        self.hard_state = state;
        Ok(())
    }

    fn append(&mut self, entries: &[LogEntry]) -> Result<()> {
        check_contiguous(self.last_index(), entries)?;
        self.entries.extend_from_slice(entries);
        Ok(())
    }

    fn truncate_from(&mut self, index: LogIndex) -> Result<()> {
        let first = self.first_index();
        if index < first {
            return Err(RaftError::Compacted { index, first });
        }
        if let Some(position) = self.position(index) {
            self.entries.truncate(position);
        }
        Ok(())
    }

    fn entry(&self, index: LogIndex) -> Result<Option<LogEntry>> {
        Ok(self
            .position(index)
            .and_then(|position| self.entries.get(position))
            .cloned())
    }

    fn entries(&self, from: LogIndex, max_entries: usize) -> Result<Vec<LogEntry>> {
        let first = self.first_index();
        if from < first {
            return Err(RaftError::Compacted { index: from, first });
        }
        let Some(position) = self.position(from) else {
            return Ok(Vec::new());
        };
        Ok(self
            .entries
            .get(position..)
            .unwrap_or_default()
            .iter()
            .take(max_entries)
            .cloned()
            .collect())
    }

    fn term_at(&self, index: LogIndex) -> Result<Option<Term>> {
        if index.is_empty_sentinel() {
            return Ok(Some(Term::INITIAL));
        }
        if let Some(snapshot) = &self.snapshot
            && snapshot.meta.last_index() == index
        {
            return Ok(Some(snapshot.meta.last_term()));
        }
        Ok(self.entry(index)?.map(|entry| entry.term()))
    }

    fn first_index(&self) -> LogIndex {
        match &self.snapshot {
            Some(snapshot) => snapshot.meta.last_index().next(),
            None => LogIndex::FIRST,
        }
    }

    fn last_index(&self) -> LogIndex {
        match self.entries.last() {
            Some(entry) => entry.index(),
            None => self
                .snapshot
                .as_ref()
                .map_or(LogIndex::ZERO, |snapshot| snapshot.meta.last_index()),
        }
    }

    fn last_term(&self) -> Term {
        match self.entries.last() {
            Some(entry) => entry.term(),
            None => self
                .snapshot
                .as_ref()
                .map_or(Term::INITIAL, |snapshot| snapshot.meta.last_term()),
        }
    }

    fn snapshot_meta(&self) -> Option<SnapshotMeta> {
        self.snapshot.as_ref().map(|snapshot| snapshot.meta.clone())
    }

    fn snapshot(&self) -> Result<Option<Snapshot>> {
        Ok(self.snapshot.clone())
    }

    fn save_snapshot(&mut self, snapshot: &Snapshot) -> Result<()> {
        let boundary = snapshot.meta.last_index();
        if let Some(existing) = &self.snapshot
            && existing.meta.last_index() > boundary
        {
            return Err(RaftError::InvalidConfig {
                reason: "a newer snapshot is already held",
            });
        }
        let keep_from = boundary.next();
        self.entries.retain(|entry| entry.index() >= keep_from);
        self.snapshot = Some(snapshot.clone());
        Ok(())
    }

    fn install_snapshot(&mut self, snapshot: &Snapshot) -> Result<()> {
        self.entries.clear();
        self.snapshot = Some(snapshot.clone());
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::log::entry::EntryPayload;
    use crate::membership::Membership;
    use crate::types::PeerId;

    fn entry(term: u64, index: u64) -> LogEntry {
        LogEntry::command(Term::new(term), LogIndex::new(index), vec![index as u8])
    }

    fn filled(count: u64) -> MemoryLog {
        let mut log = MemoryLog::new();
        let entries: Vec<LogEntry> = (1..=count).map(|index| entry(1, index)).collect();
        log.append(&entries).unwrap();
        log
    }

    #[test]
    fn a_fresh_log_reports_the_empty_sentinels() {
        let log = MemoryLog::new();
        assert_eq!(log.last_index(), LogIndex::ZERO);
        assert_eq!(log.last_term(), Term::INITIAL);
        assert_eq!(log.first_index(), LogIndex::FIRST);
        assert!(log.is_empty());
        assert!(log.snapshot_meta().is_none());
        assert_eq!(log.term_at(LogIndex::ZERO).unwrap(), Some(Term::INITIAL));
    }

    #[test]
    fn appends_must_be_contiguous_from_the_end() {
        let mut log = filled(3);
        let error = log.append(&[entry(1, 5)]).unwrap_err();
        assert!(matches!(error, RaftError::Unavailable { .. }));
        // A gap inside the batch is caught too.
        let error = log.append(&[entry(1, 4), entry(1, 6)]).unwrap_err();
        assert!(matches!(error, RaftError::Unavailable { .. }));
        assert_eq!(log.last_index(), LogIndex::new(3));
    }

    #[test]
    fn truncation_drops_the_suffix_from_the_given_index_inclusive() {
        let mut log = filled(5);
        log.truncate_from(LogIndex::new(3)).unwrap();
        assert_eq!(log.last_index(), LogIndex::new(2));
        assert!(log.entry(LogIndex::new(3)).unwrap().is_none());
        // Truncating past the end is a no-op, not an error: a follower whose
        // log is already shorter than the conflict point must not fail.
        log.truncate_from(LogIndex::new(99)).unwrap();
        assert_eq!(log.last_index(), LogIndex::new(2));
    }

    #[test]
    fn entries_pages_and_refuses_compacted_reads() {
        let mut log = filled(10);
        let page = log.entries(LogIndex::new(3), 4).unwrap();
        assert_eq!(page.len(), 4);
        assert_eq!(page[0].index(), LogIndex::new(3));
        assert_eq!(page[3].index(), LogIndex::new(6));

        // Reading past the end yields an empty page rather than an error.
        assert!(log.entries(LogIndex::new(11), 4).unwrap().is_empty());

        let snapshot = Snapshot::new(
            SnapshotMeta::new(LogIndex::new(5), Term::new(1), Membership::default()),
            vec![1, 2, 3],
        );
        log.save_snapshot(&snapshot).unwrap();
        let error = log.entries(LogIndex::new(2), 1).unwrap_err();
        assert!(matches!(error, RaftError::Compacted { .. }));
    }

    #[test]
    fn a_saved_snapshot_compacts_the_prefix_and_keeps_the_suffix() {
        let mut log = filled(10);
        let snapshot = Snapshot::new(
            SnapshotMeta::new(
                LogIndex::new(6),
                Term::new(1),
                Membership::new([PeerId::new(1)]),
            ),
            vec![0xAB],
        );
        log.save_snapshot(&snapshot).unwrap();

        assert_eq!(log.first_index(), LogIndex::new(7));
        assert_eq!(log.last_index(), LogIndex::new(10));
        assert_eq!(log.len(), 4);
        // The compacted boundary's term outlives its entry, so a leader can
        // still state prev_log_term for index 7.
        assert_eq!(log.term_at(LogIndex::new(6)).unwrap(), Some(Term::new(1)));
        assert!(log.entry(LogIndex::new(6)).unwrap().is_none());
    }

    #[test]
    fn an_older_snapshot_is_refused() {
        let mut log = filled(10);
        let newer = Snapshot::new(
            SnapshotMeta::new(LogIndex::new(8), Term::new(1), Membership::default()),
            Vec::new(),
        );
        log.save_snapshot(&newer).unwrap();
        let older = Snapshot::new(
            SnapshotMeta::new(LogIndex::new(4), Term::new(1), Membership::default()),
            Vec::new(),
        );
        assert!(matches!(
            log.save_snapshot(&older).unwrap_err(),
            RaftError::InvalidConfig { .. }
        ));
    }

    #[test]
    fn an_installed_snapshot_discards_every_local_entry() {
        // A replica accepting InstallSnapshot is behind by definition; an
        // entry it still held past the snapshot came from a log the leader
        // has already overwritten, so keeping it would be keeping garbage.
        let mut log = filled(10);
        let snapshot = Snapshot::new(
            SnapshotMeta::new(LogIndex::new(4), Term::new(2), Membership::default()),
            vec![7],
        );
        log.install_snapshot(&snapshot).unwrap();
        assert!(log.is_empty());
        assert_eq!(log.first_index(), LogIndex::new(5));
        assert_eq!(log.last_index(), LogIndex::new(4));
        assert_eq!(log.last_term(), Term::new(2));
    }

    #[test]
    fn hard_state_survives_round_trips_through_the_store() {
        let mut log = MemoryLog::new();
        assert_eq!(log.hard_state(), HardState::default());
        let state = HardState::new(Term::new(6), Some(PeerId::new(3)));
        log.save_hard_state(state).unwrap();
        assert_eq!(log.hard_state(), state);
    }

    #[test]
    fn appending_after_a_snapshot_continues_the_index_space() {
        let mut log = MemoryLog::new();
        let snapshot = Snapshot::new(
            SnapshotMeta::new(LogIndex::new(20), Term::new(3), Membership::default()),
            Vec::new(),
        );
        log.install_snapshot(&snapshot).unwrap();
        log.append(&[entry(4, 21), entry(4, 22)]).unwrap();
        assert_eq!(log.last_index(), LogIndex::new(22));
        assert_eq!(log.first_index(), LogIndex::new(21));
        assert_eq!(
            log.entries(LogIndex::new(21), 10).unwrap().len(),
            2,
            "entries after a snapshot are readable from first_index"
        );
    }

    #[test]
    fn truncating_into_the_snapshot_is_an_error() {
        let mut log = filled(10);
        log.save_snapshot(&Snapshot::new(
            SnapshotMeta::new(LogIndex::new(5), Term::new(1), Membership::default()),
            Vec::new(),
        ))
        .unwrap();
        assert!(matches!(
            log.truncate_from(LogIndex::new(3)).unwrap_err(),
            RaftError::Compacted { .. }
        ));
    }

    #[test]
    fn config_entries_are_stored_like_any_other() {
        let mut log = MemoryLog::new();
        let membership = Membership::new([PeerId::new(1), PeerId::new(2)]);
        log.append(&[LogEntry::new(
            Term::new(1),
            LogIndex::new(1),
            EntryPayload::Config(membership.clone()),
        )])
        .unwrap();
        assert_eq!(
            log.entry(LogIndex::new(1)).unwrap().unwrap().config(),
            Some(&membership)
        );
        assert_eq!(log.held_entries().len(), 1);
    }
}
