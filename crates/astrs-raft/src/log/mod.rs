//! The replicated log: entries, the durability contract, and its two
//! implementations.
//!
//! - [`entry`] — [`LogEntry`], [`HardState`], [`SnapshotMeta`]: everything
//!   Figure 2 requires on stable storage.
//! - [`store`] — the [`LogStore`] trait plus [`MemoryLog`].
//! - [`wal`] — [`WalLog`], the file-backed implementation, with recovery and
//!   compaction.
//!
//! # Examples
//!
//! ```
//! use astrs_raft::{LogEntry, LogIndex, LogStore, MemoryLog, Term};
//!
//! let mut log = MemoryLog::new();
//! log.append(&[
//!     LogEntry::command(Term::new(1), LogIndex::new(1), b"a".to_vec()),
//!     LogEntry::command(Term::new(1), LogIndex::new(2), b"b".to_vec()),
//! ])?;
//!
//! // A conflicting suffix is truncated before the correct one is appended.
//! log.truncate_from(LogIndex::new(2))?;
//! log.append(&[LogEntry::command(Term::new(2), LogIndex::new(2), b"B".to_vec())])?;
//! assert_eq!(log.last_term(), Term::new(2));
//! # Ok::<(), astrs_raft::RaftError>(())
//! ```

pub mod entry;
pub mod store;
pub mod wal;

pub use entry::{EntryPayload, HardState, LogEntry, Snapshot, SnapshotMeta};
pub use store::{LogStore, MemoryLog};
pub use wal::{FsyncPolicy, RecoveryOutcome, WalLog};
