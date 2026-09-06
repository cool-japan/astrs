//! [`HaLog`]: one concrete Raft log type, backed by a file or by memory.
//!
//! [`astrs_raft::RaftReplica`] is generic over its log store, which is right
//! for the library but wrong for a caller that decides between two backends at
//! *runtime* from a `--store` flag: two generic instantiations would mean two
//! replica types, two task types and a branch duplicated at every use.
//!
//! This enum collapses that choice into one type. The delegation is entirely
//! mechanical — every method forwards — which is the point: there is no
//! behaviour here to get wrong, only a runtime choice made once at startup.
//!
//! # When each is right
//!
//! - [`HaLog::Wal`] — a real deployment. Durability is what makes Raft's
//!   safety argument hold across a machine crash.
//! - [`HaLog::Memory`] — tests, and a single-process cluster where the whole
//!   set dies together anyway. Choosing it in production would mean a
//!   coordinator that forgets which term it voted in, which is the direct
//!   route to two leaders.

use std::path::Path;

use astrs_raft::{
    FsyncPolicy, HardState, LogEntry, LogIndex, LogStore, MemoryLog, Result as RaftResult,
    Snapshot, SnapshotMeta, Term, WalLog,
};

/// A Raft log that is either file-backed or in memory.
#[derive(Debug)]
pub enum HaLog {
    /// The durable, file-backed log a real coordinator uses.
    Wal(Box<WalLog>),
    /// An in-memory log, for tests and single-process clusters.
    Memory(MemoryLog),
}

impl HaLog {
    /// Opens a file-backed log at `path`.
    ///
    /// # Errors
    ///
    /// Whatever [`WalLog::open`] reports: the file cannot be created, or its
    /// contents are corrupt beyond a repairable tail.
    pub fn open(path: impl AsRef<Path>, fsync: FsyncPolicy) -> RaftResult<Self> {
        Ok(Self::Wal(Box::new(WalLog::open(path, fsync)?)))
    }

    /// An in-memory log.
    #[must_use]
    pub fn in_memory() -> Self {
        Self::Memory(MemoryLog::new())
    }

    /// Whether this log survives the process that wrote it.
    #[must_use]
    pub const fn is_durable(&self) -> bool {
        matches!(self, Self::Wal(_))
    }

    /// A short name for logs and startup banners.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Wal(_) => "wal",
            Self::Memory(_) => "memory",
        }
    }
}

macro_rules! delegate {
    ($self:ident, $method:ident $(, $argument:expr)*) => {
        match $self {
            Self::Wal(log) => log.$method($($argument),*),
            Self::Memory(log) => log.$method($($argument),*),
        }
    };
}

impl LogStore for HaLog {
    fn hard_state(&self) -> HardState {
        delegate!(self, hard_state)
    }

    fn save_hard_state(&mut self, state: HardState) -> RaftResult<()> {
        delegate!(self, save_hard_state, state)
    }

    fn append(&mut self, entries: &[LogEntry]) -> RaftResult<()> {
        delegate!(self, append, entries)
    }

    fn truncate_from(&mut self, index: LogIndex) -> RaftResult<()> {
        delegate!(self, truncate_from, index)
    }

    fn entry(&self, index: LogIndex) -> RaftResult<Option<LogEntry>> {
        delegate!(self, entry, index)
    }

    fn entries(&self, from: LogIndex, max_entries: usize) -> RaftResult<Vec<LogEntry>> {
        delegate!(self, entries, from, max_entries)
    }

    fn term_at(&self, index: LogIndex) -> RaftResult<Option<Term>> {
        delegate!(self, term_at, index)
    }

    fn first_index(&self) -> LogIndex {
        delegate!(self, first_index)
    }

    fn last_index(&self) -> LogIndex {
        delegate!(self, last_index)
    }

    fn last_term(&self) -> Term {
        delegate!(self, last_term)
    }

    fn snapshot_meta(&self) -> Option<SnapshotMeta> {
        delegate!(self, snapshot_meta)
    }

    fn snapshot(&self) -> RaftResult<Option<Snapshot>> {
        delegate!(self, snapshot)
    }

    fn save_snapshot(&mut self, snapshot: &Snapshot) -> RaftResult<()> {
        delegate!(self, save_snapshot, snapshot)
    }

    fn install_snapshot(&mut self, snapshot: &Snapshot) -> RaftResult<()> {
        delegate!(self, install_snapshot, snapshot)
    }

    fn sync(&mut self) -> RaftResult<()> {
        delegate!(self, sync)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use astrs_raft::PeerId;

    fn entry(term: u64, index: u64) -> LogEntry {
        LogEntry::command(Term::new(term), LogIndex::new(index), vec![index as u8])
    }

    /// Both variants must behave identically for the whole trait, because the
    /// only difference between them is meant to be durability.
    fn exercise(mut log: HaLog) {
        assert_eq!(log.last_index(), LogIndex::ZERO);
        assert_eq!(log.first_index(), LogIndex::FIRST);
        assert_eq!(log.last_term(), Term::INITIAL);
        assert!(log.snapshot_meta().is_none());
        assert!(log.snapshot().unwrap().is_none());

        log.append(&[entry(1, 1), entry(1, 2), entry(2, 3)])
            .unwrap();
        assert_eq!(log.last_index(), LogIndex::new(3));
        assert_eq!(log.last_term(), Term::new(2));
        assert_eq!(log.entry(LogIndex::new(2)).unwrap(), Some(entry(1, 2)));
        assert_eq!(log.entries(LogIndex::new(2), 10).unwrap().len(), 2);
        assert_eq!(log.term_at(LogIndex::new(3)).unwrap(), Some(Term::new(2)));

        log.save_hard_state(HardState::new(Term::new(2), Some(PeerId::new(1))))
            .unwrap();
        assert_eq!(log.hard_state().term, Term::new(2));

        log.truncate_from(LogIndex::new(3)).unwrap();
        assert_eq!(log.last_index(), LogIndex::new(2));

        let snapshot = Snapshot::new(
            SnapshotMeta::new(
                LogIndex::new(2),
                Term::new(1),
                astrs_raft::Membership::new([PeerId::new(1)]),
            ),
            vec![7, 7],
        );
        log.save_snapshot(&snapshot).unwrap();
        assert_eq!(log.first_index(), LogIndex::new(3));
        assert_eq!(log.snapshot().unwrap().map(|s| s.data), Some(vec![7, 7]));

        log.install_snapshot(&snapshot).unwrap();
        log.sync().unwrap();
    }

    #[test]
    fn the_memory_variant_implements_the_whole_contract() {
        let log = HaLog::in_memory();
        assert!(!log.is_durable());
        assert_eq!(log.kind(), "memory");
        exercise(log);
    }

    #[test]
    fn the_file_variant_implements_the_whole_contract() {
        let dir = std::env::temp_dir().join("astrs-coordinator-halog");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = HaLog::open(dir.join("raft.wal"), FsyncPolicy::Never).unwrap();
        assert!(log.is_durable());
        assert_eq!(log.kind(), "wal");
        exercise(log);
    }

    #[test]
    fn a_file_backed_log_survives_a_reopen() {
        let dir = std::env::temp_dir().join("astrs-coordinator-halog-reopen");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("raft.wal");
        {
            let mut log = HaLog::open(&path, FsyncPolicy::Always).unwrap();
            log.append(&[entry(1, 1)]).unwrap();
            log.save_hard_state(HardState::new(Term::new(4), Some(PeerId::new(2))))
                .unwrap();
        }
        let reopened = HaLog::open(&path, FsyncPolicy::Always).unwrap();
        assert_eq!(reopened.last_index(), LogIndex::new(1));
        assert_eq!(reopened.hard_state().term, Term::new(4));
        assert_eq!(reopened.hard_state().voted_for, Some(PeerId::new(2)));
    }

    #[test]
    fn an_in_memory_log_forgets_everything_which_is_why_it_is_not_the_default() {
        let mut log = HaLog::in_memory();
        log.save_hard_state(HardState::new(Term::new(9), Some(PeerId::new(3))))
            .unwrap();
        drop(log);
        assert_eq!(HaLog::in_memory().hard_state(), HardState::default());
    }
}
