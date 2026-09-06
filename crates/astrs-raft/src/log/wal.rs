//! [`WalLog`]: the file-backed write-ahead log a production replica runs on.
//!
//! # The file layout
//!
//! ```text
//! ┌────────────────────────────────────────────────────────────────────┐
//! │ FILE_MAGIC "ASTRSWAL" (8) │ FORMAT_VERSION u16 LE                  │
//! ├────────────────────────────────────────────────────────────────────┤
//! │ RECORD │ RECORD │ RECORD │ ...            (append-only, in order)  │
//! └────────────────────────────────────────────────────────────────────┘
//!
//! RECORD:
//! ┌──────────┬──────────────┬──────────────────────┬──────────────┐
//! │ tag: u32 │ body_len:u32 │ body (oxicode)       │ crc32c: u32  │
//! │ LE       │ LE           │                      │ LE           │
//! └──────────┴──────────────┴──────────────────────┴──────────────┘
//! ```
//!
//! The framing is deliberately the same shape `astrs-recording`'s `.arec`
//! container uses (blueprint §14) — a tag, a length, an `oxicode` body and a
//! CRC-32C that **covers the tag and the length as well as the body**. A
//! checksum over the body alone would let a flipped bit in the length prefix
//! through as "not enough bytes yet", which is exactly the corruption a
//! torn-tail recovery must be able to tell apart from a real truncation.
//!
//! # The record kinds
//!
//! | Tag | Meaning |
//! |---|---|
//! | `AWe1` | one [`LogEntry`] appended at its own index |
//! | `AWh1` | a new [`HardState`] (term + vote) |
//! | `AWt1` | a truncation: every entry at or after this index is gone |
//! | `AWs1` | a compaction boundary: [`SnapshotMeta`] whose data is in the sidecar |
//!
//! Replaying the records in order reconstructs the log exactly. Truncation
//! is a *record*, not a rewrite of the file, so the append-only property
//! holds for every ordinary operation: only compaction rewrites the file
//! (see [`WalLog::save_snapshot`]).
//!
//! # Recovery: torn tails are repaired, not tolerated
//!
//! A replica killed mid-`write` leaves a partial record at the end of the
//! file. [`WalLog::open`] replays every record that is complete and checksums
//! correctly, stops at the first one that does not, and then **truncates the
//! file to that offset** with `set_len`. Merely stopping the scan would be a
//! latent corruption: the next append would land *after* the garbage, and the
//! next open would then stop at the same torn record and silently lose every
//! entry written after it. See
//! [`WalLog::recovered_bytes`]/[`WalLog::repaired_tail`] for what a caller can
//! observe about a repair, and this module's tests for the exhaustive form of
//! the claim.
//!
//! # Durability
//!
//! [`FsyncPolicy`] is explicit and defaults to [`FsyncPolicy::Always`],
//! because Raft's correctness argument assumes a peer that answered
//! `AppendEntries` will still have those entries after a power cut. The other
//! policies exist for tests and for deployments that have decided their
//! failure model is process crash, not machine crash — a decision that must
//! be made out loud.
//!
//! # Examples
//!
//! ```
//! use astrs_raft::{FsyncPolicy, LogEntry, LogIndex, LogStore, Term, WalLog};
//!
//! let dir = std::env::temp_dir().join("astrs-raft-wal-doctest");
//! std::fs::create_dir_all(&dir).expect("a scratch directory");
//! let path = dir.join("raft.wal");
//! let _ = std::fs::remove_file(&path);
//!
//! let mut log = WalLog::open(&path, FsyncPolicy::Always)?;
//! log.append(&[LogEntry::command(Term::new(1), LogIndex::new(1), vec![42])])?;
//! drop(log);
//!
//! // Everything an `Ok` append reported is there after a restart.
//! let reopened = WalLog::open(&path, FsyncPolicy::Always)?;
//! assert_eq!(reopened.last_index(), LogIndex::new(1));
//! # std::fs::remove_file(&path).ok();
//! # Ok::<(), astrs_raft::RaftError>(())
//! ```

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use astrs_wire::crc32c::Crc32c;
use astrs_wire::{WireDecode, WireEncode};

use crate::error::{RaftError, Result};
use crate::log::entry::{HardState, LogEntry, Snapshot, SnapshotMeta};
use crate::log::store::{LogStore, MemoryLog};
use crate::types::{LogIndex, Term};

/// The literal byte string every Raft write-ahead log opens with.
pub const FILE_MAGIC: &[u8; 8] = b"ASTRSWAL";

/// The container format version this build writes, and the highest it reads.
pub const FORMAT_VERSION: u16 = 1;

/// The fixed prologue length: magic plus a two-byte version.
pub const PROLOGUE_LEN: u64 = FILE_MAGIC.len() as u64 + 2;

/// The fixed per-record overhead: `tag(4) + body_len(4) + crc(4)`.
pub const RECORD_OVERHEAD: usize = 12;

/// The largest record body this build will read back.
///
/// A forged or corrupted length prefix must never become a huge allocation;
/// 64 MiB matches the wire's own frame ceiling (blueprint §7.1), which is the
/// largest single thing the coordinator ever replicates.
pub const MAX_RECORD_BODY: u32 = 64 * 1024 * 1024;

/// The tag of a [`LogEntry`] record.
const TAG_ENTRY: u32 = u32::from_le_bytes(*b"AWe1");
/// The tag of a [`HardState`] record.
const TAG_HARD_STATE: u32 = u32::from_le_bytes(*b"AWh1");
/// The tag of a truncation record.
const TAG_TRUNCATE: u32 = u32::from_le_bytes(*b"AWt1");
/// The tag of a compaction-boundary record.
const TAG_SNAPSHOT: u32 = u32::from_le_bytes(*b"AWs1");

/// When a [`WalLog`] forces its writes to durable storage.
///
/// # Examples
///
/// ```
/// use astrs_raft::FsyncPolicy;
///
/// assert_eq!(FsyncPolicy::default(), FsyncPolicy::Always);
/// assert!(FsyncPolicy::Always.is_crash_safe());
/// assert!(!FsyncPolicy::Never.is_crash_safe());
/// assert!(!FsyncPolicy::EveryN(8).is_crash_safe());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FsyncPolicy {
    /// `fsync` after every write. The only policy under which Raft's
    /// durability assumption actually holds against a machine crash.
    #[default]
    Always,
    /// `fsync` after every `n` writes. Bounds the window rather than closing
    /// it; `0` and `1` behave as [`FsyncPolicy::Always`].
    EveryN(u32),
    /// Never `fsync`. Data reaches the OS page cache and survives a *process*
    /// crash but not a machine one — a legitimate choice for tests and for
    /// deployments that have said so deliberately.
    Never,
}

impl FsyncPolicy {
    /// Whether this policy makes every acknowledged write survive a machine
    /// crash.
    #[must_use]
    pub const fn is_crash_safe(self) -> bool {
        matches!(self, Self::Always)
    }

    /// Whether a write that brings the un-synced count to `writes` should
    /// force a sync now.
    #[must_use]
    const fn should_sync(self, writes: u32) -> bool {
        match self {
            Self::Always => true,
            // A zero interval is a degenerate "every write", not "never".
            Self::EveryN(0 | 1) => true,
            Self::EveryN(n) => writes >= n,
            Self::Never => false,
        }
    }
}

/// Why [`WalLog::open`] stopped replaying records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecoveryOutcome {
    /// Every byte in the file parsed as a complete, valid record.
    Intact,
    /// A record at the end was incomplete — the signature of a process
    /// killed mid-write. The file was truncated back to the last good
    /// record.
    TornTail,
    /// A record failed its checksum or could not be decoded. Everything
    /// before it was recovered and the file truncated to that point.
    Corrupt,
}

impl RecoveryOutcome {
    /// Whether the file had to be repaired on open.
    #[must_use]
    pub const fn was_repaired(self) -> bool {
        !matches!(self, Self::Intact)
    }
}

/// A file-backed [`LogStore`].
///
/// The uncompacted suffix of the log is mirrored in memory (a [`MemoryLog`])
/// so reads never touch the disk: a leader replicating to three followers
/// re-reads the same recent entries constantly, and that suffix is bounded by
/// the snapshot threshold the replica compacts at. The file is the *authority*
/// — the mirror is rebuilt from it on every open — but it is written, never
/// read, during normal operation.
#[derive(Debug)]
pub struct WalLog {
    /// The write-ahead log file's path.
    path: PathBuf,
    /// The snapshot sidecar's path.
    snapshot_path: PathBuf,
    /// The open append handle.
    file: File,
    /// The in-memory mirror reads are served from.
    mirror: MemoryLog,
    /// When to force writes to disk.
    policy: FsyncPolicy,
    /// Writes since the last `fsync`.
    unsynced: u32,
    /// How many bytes were replayed successfully on open.
    recovered_bytes: u64,
    /// What [`WalLog::open`] found.
    outcome: RecoveryOutcome,
}

impl WalLog {
    /// Opens (or creates) the write-ahead log at `path`, replaying it and
    /// repairing a torn tail if there is one.
    ///
    /// The snapshot sidecar is `path` with `.snap` appended, so a directory
    /// holding `raft.wal` also holds `raft.wal.snap`.
    ///
    /// # Errors
    ///
    /// - [`RaftError::Io`] if the file cannot be opened, read or truncated.
    /// - [`RaftError::UnsupportedVersion`] if the prologue names a format
    ///   version this build does not understand.
    /// - [`RaftError::Corrupt`] if the prologue itself is not a Raft WAL.
    pub fn open(path: impl AsRef<Path>, policy: FsyncPolicy) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let snapshot_path = snapshot_sidecar(&path);

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|source| RaftError::io("creating the log directory", parent, source))?;
        }

        let existing = match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => return Err(RaftError::io("reading the log", &path, source)),
        };

        let snapshot = read_snapshot(&snapshot_path)?;
        let mut mirror = MemoryLog::new();
        if let Some(snapshot) = &snapshot {
            mirror.install_snapshot(snapshot)?;
        }

        let (recovered_bytes, outcome) = match &existing {
            Some(bytes) => replay(bytes, &mut mirror)?,
            None => (PROLOGUE_LEN, RecoveryOutcome::Intact),
        };

        // Opened in append mode, not plain write mode: after a `set_len`
        // repair below, a write-mode handle would still sit at offset zero
        // and overwrite the prologue with the next record.
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|source| RaftError::io("opening the log", &path, source))?;

        if existing.is_none() {
            let mut prologue = Vec::with_capacity(PROLOGUE_LEN as usize);
            prologue.extend_from_slice(FILE_MAGIC);
            prologue.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
            file.write_all(&prologue)
                .map_err(|source| RaftError::io("writing the prologue", &path, source))?;
            file.sync_all()
                .map_err(|source| RaftError::io("syncing the prologue", &path, source))?;
        } else if outcome.was_repaired() {
            // Repair, do not merely stop reading: an append landing after a
            // torn record would be invisible to the next open.
            file.set_len(recovered_bytes)
                .map_err(|source| RaftError::io("truncating a torn tail", &path, source))?;
            file.sync_all()
                .map_err(|source| RaftError::io("syncing a repaired log", &path, source))?;
        }

        Ok(Self {
            path,
            snapshot_path,
            file,
            mirror,
            policy,
            unsynced: 0,
            recovered_bytes,
            outcome,
        })
    }

    /// How many bytes of the file were replayed successfully on open.
    #[must_use]
    pub const fn recovered_bytes(&self) -> u64 {
        self.recovered_bytes
    }

    /// What [`WalLog::open`] found in the file.
    #[must_use]
    pub const fn outcome(&self) -> RecoveryOutcome {
        self.outcome
    }

    /// Whether opening this log had to repair a damaged tail.
    #[must_use]
    pub const fn repaired_tail(&self) -> bool {
        self.outcome.was_repaired()
    }

    /// The path this log is stored at.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The durability policy in force.
    #[must_use]
    pub const fn policy(&self) -> FsyncPolicy {
        self.policy
    }

    /// Appends one framed record and honours the fsync policy.
    fn write_record(&mut self, tag: u32, body: &[u8]) -> Result<()> {
        let mut framed = Vec::with_capacity(body.len() + RECORD_OVERHEAD);
        let body_len = u32::try_from(body.len()).map_err(|_| RaftError::Corrupt {
            offset: 0,
            reason: "a record body larger than 4 GiB cannot be framed",
        })?;
        framed.extend_from_slice(&tag.to_le_bytes());
        framed.extend_from_slice(&body_len.to_le_bytes());
        framed.extend_from_slice(body);
        framed.extend_from_slice(&record_crc(tag, body).to_le_bytes());

        self.file
            .write_all(&framed)
            .map_err(|source| RaftError::io("appending a record", &self.path, source))?;
        self.unsynced = self.unsynced.saturating_add(1);
        if self.policy.should_sync(self.unsynced) {
            self.sync()?;
        }
        Ok(())
    }

    /// Rewrites the file from scratch: prologue, compaction boundary, the
    /// surviving entry suffix and the current hard state.
    ///
    /// This is the only operation that is not a tail append. It writes a
    /// temporary file and renames it over the original, so a crash part-way
    /// through leaves the *old* complete log rather than a half-written new
    /// one.
    fn regenerate(&mut self) -> Result<()> {
        let temporary = self.path.with_extension("wal-regen");
        let mut fresh = File::create(&temporary)
            .map_err(|source| RaftError::io("creating a regenerated log", &temporary, source))?;

        let mut buffer = Vec::new();
        buffer.extend_from_slice(FILE_MAGIC);
        buffer.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        if let Some(meta) = self.mirror.snapshot_meta() {
            append_framed(&mut buffer, TAG_SNAPSHOT, &meta.encode_to_vec()?)?;
        }
        append_framed(
            &mut buffer,
            TAG_HARD_STATE,
            &self.mirror.hard_state().encode_to_vec()?,
        )?;
        for entry in self.mirror.held_entries() {
            append_framed(&mut buffer, TAG_ENTRY, &entry.encode_to_vec()?)?;
        }

        fresh
            .write_all(&buffer)
            .map_err(|source| RaftError::io("writing a regenerated log", &temporary, source))?;
        fresh
            .sync_all()
            .map_err(|source| RaftError::io("syncing a regenerated log", &temporary, source))?;
        drop(fresh);

        std::fs::rename(&temporary, &self.path)
            .map_err(|source| RaftError::io("installing a regenerated log", &self.path, source))?;

        self.file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| RaftError::io("reopening a regenerated log", &self.path, source))?;
        self.unsynced = 0;
        self.recovered_bytes = buffer.len() as u64;
        Ok(())
    }

    /// Writes the snapshot sidecar atomically.
    fn write_snapshot_sidecar(&self, snapshot: &Snapshot) -> Result<()> {
        let temporary = self.snapshot_path.with_extension("snap-tmp");
        let body = snapshot.encode_to_vec()?;
        let mut framed = Vec::with_capacity(body.len() + RECORD_OVERHEAD + 2);
        framed.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        append_framed(&mut framed, TAG_SNAPSHOT, &body)?;

        let mut file = File::create(&temporary)
            .map_err(|source| RaftError::io("creating the snapshot", &temporary, source))?;
        file.write_all(&framed)
            .map_err(|source| RaftError::io("writing the snapshot", &temporary, source))?;
        file.sync_all()
            .map_err(|source| RaftError::io("syncing the snapshot", &temporary, source))?;
        drop(file);
        std::fs::rename(&temporary, &self.snapshot_path).map_err(|source| {
            RaftError::io("installing the snapshot", &self.snapshot_path, source)
        })?;
        Ok(())
    }
}

impl LogStore for WalLog {
    fn hard_state(&self) -> HardState {
        self.mirror.hard_state()
    }

    fn save_hard_state(&mut self, state: HardState) -> Result<()> {
        self.write_record(TAG_HARD_STATE, &state.encode_to_vec()?)?;
        self.mirror.save_hard_state(state)
    }

    fn append(&mut self, entries: &[LogEntry]) -> Result<()> {
        crate::log::store::check_contiguous(self.mirror.last_index(), entries)?;
        for entry in entries {
            self.write_record(TAG_ENTRY, &entry.encode_to_vec()?)?;
        }
        self.mirror.append(entries)
    }

    fn truncate_from(&mut self, index: LogIndex) -> Result<()> {
        // Recorded, not rewritten: truncation stays a tail append so the
        // file keeps its append-only shape outside compaction.
        self.mirror.truncate_from(index)?;
        self.write_record(TAG_TRUNCATE, &index.encode_to_vec()?)
    }

    fn entry(&self, index: LogIndex) -> Result<Option<LogEntry>> {
        self.mirror.entry(index)
    }

    fn entries(&self, from: LogIndex, max_entries: usize) -> Result<Vec<LogEntry>> {
        self.mirror.entries(from, max_entries)
    }

    fn term_at(&self, index: LogIndex) -> Result<Option<Term>> {
        self.mirror.term_at(index)
    }

    fn first_index(&self) -> LogIndex {
        self.mirror.first_index()
    }

    fn last_index(&self) -> LogIndex {
        self.mirror.last_index()
    }

    fn last_term(&self) -> Term {
        self.mirror.last_term()
    }

    fn snapshot_meta(&self) -> Option<SnapshotMeta> {
        self.mirror.snapshot_meta()
    }

    fn snapshot(&self) -> Result<Option<Snapshot>> {
        self.mirror.snapshot()
    }

    fn save_snapshot(&mut self, snapshot: &Snapshot) -> Result<()> {
        self.mirror.save_snapshot(snapshot)?;
        self.write_snapshot_sidecar(snapshot)?;
        self.regenerate()
    }

    fn install_snapshot(&mut self, snapshot: &Snapshot) -> Result<()> {
        self.mirror.install_snapshot(snapshot)?;
        self.write_snapshot_sidecar(snapshot)?;
        self.regenerate()
    }

    fn sync(&mut self) -> Result<()> {
        self.file
            .sync_data()
            .map_err(|source| RaftError::io("syncing the log", &self.path, source))?;
        self.unsynced = 0;
        Ok(())
    }
}

/// The snapshot sidecar path for a WAL at `path`.
fn snapshot_sidecar(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".snap");
    PathBuf::from(name)
}

/// The CRC-32C of one record's tag, declared length and body, in the order
/// they lie on disk.
fn record_crc(tag: u32, body: &[u8]) -> u32 {
    let mut hasher = Crc32c::new();
    hasher.update(&tag.to_le_bytes());
    hasher.update(&(body.len() as u32).to_le_bytes());
    hasher.update(body);
    hasher.finalize()
}

/// Appends one framed record to `out`.
fn append_framed(out: &mut Vec<u8>, tag: u32, body: &[u8]) -> Result<()> {
    let body_len = u32::try_from(body.len()).map_err(|_| RaftError::Corrupt {
        offset: 0,
        reason: "a record body larger than 4 GiB cannot be framed",
    })?;
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&body_len.to_le_bytes());
    out.extend_from_slice(body);
    out.extend_from_slice(&record_crc(tag, body).to_le_bytes());
    Ok(())
}

/// Reads the snapshot sidecar, if there is one.
fn read_snapshot(path: &Path) -> Result<Option<Snapshot>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(RaftError::io("reading the snapshot", path, source)),
    };
    let Some(version_bytes) = bytes.get(..2) else {
        // A sidecar too short to hold a version is a crash during its own
        // creation; the WAL alone is still authoritative.
        return Ok(None);
    };
    let version = u16::from_le_bytes([version_bytes[0], version_bytes[1]]);
    if version > FORMAT_VERSION {
        return Err(RaftError::UnsupportedVersion {
            found: version,
            supported: FORMAT_VERSION,
        });
    }
    let body = bytes.get(2..).unwrap_or_default();
    match read_record(body, 2)? {
        RecordRead::Complete { tag, body, .. } if tag == TAG_SNAPSHOT => {
            Ok(Some(Snapshot::decode_exact(body)?))
        }
        // A half-written sidecar is the same class of event as a torn WAL
        // tail: the previous snapshot's absence is recoverable from the log.
        _ => Ok(None),
    }
}

/// What [`read_record`] found at one offset.
#[derive(Debug)]
enum RecordRead<'a> {
    /// A complete, checksum-verified record.
    Complete {
        /// Its tag.
        tag: u32,
        /// Its body.
        body: &'a [u8],
        /// Its total on-disk length.
        total_len: usize,
    },
    /// Fewer bytes remain than the record claims — a torn tail.
    Incomplete,
    /// A complete-looking record whose checksum did not match.
    Corrupt,
}

/// Parses one record from the front of `bytes`.
///
/// Never allocates on a forged length: the declared body length is checked
/// against both [`MAX_RECORD_BODY`] and the bytes actually on hand *before*
/// any slice of body size is taken.
fn read_record(bytes: &[u8], offset: u64) -> Result<RecordRead<'_>> {
    let Some(header) = bytes.get(..8) else {
        return Ok(RecordRead::Incomplete);
    };
    let tag = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let declared = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    if declared > MAX_RECORD_BODY {
        return Err(RaftError::Corrupt {
            offset,
            reason: "a record declares a body larger than this build will read",
        });
    }
    let body_len = declared as usize;
    let Some(body) = bytes.get(8..8 + body_len) else {
        return Ok(RecordRead::Incomplete);
    };
    let Some(crc_bytes) = bytes.get(8 + body_len..8 + body_len + 4) else {
        return Ok(RecordRead::Incomplete);
    };
    let found = u32::from_le_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
    if found != record_crc(tag, body) {
        return Ok(RecordRead::Corrupt);
    }
    Ok(RecordRead::Complete {
        tag,
        body,
        total_len: 8 + body_len + 4,
    })
}

/// Replays `bytes` into `mirror`, returning how many bytes parsed and why
/// the replay stopped.
fn replay(bytes: &[u8], mirror: &mut MemoryLog) -> Result<(u64, RecoveryOutcome)> {
    let Some(prologue) = bytes.get(..PROLOGUE_LEN as usize) else {
        // Even the prologue is torn: treat the whole file as a fresh one to
        // be rewritten, which is what a crash during creation leaves behind.
        return Ok((0, RecoveryOutcome::TornTail));
    };
    if prologue.get(..FILE_MAGIC.len()) != Some(&FILE_MAGIC[..]) {
        return Err(RaftError::Corrupt {
            offset: 0,
            reason: "the file does not start with the ASTRSWAL magic",
        });
    }
    let version = u16::from_le_bytes([prologue[8], prologue[9]]);
    if version > FORMAT_VERSION {
        return Err(RaftError::UnsupportedVersion {
            found: version,
            supported: FORMAT_VERSION,
        });
    }

    let mut cursor = PROLOGUE_LEN as usize;
    loop {
        let remaining = bytes.get(cursor..).unwrap_or_default();
        if remaining.is_empty() {
            return Ok((cursor as u64, RecoveryOutcome::Intact));
        }
        match read_record(remaining, cursor as u64)? {
            RecordRead::Complete {
                tag,
                body,
                total_len,
            } => {
                apply_record(tag, body, mirror, cursor as u64)?;
                cursor += total_len;
            }
            RecordRead::Incomplete => return Ok((cursor as u64, RecoveryOutcome::TornTail)),
            RecordRead::Corrupt => return Ok((cursor as u64, RecoveryOutcome::Corrupt)),
        }
    }
}

/// Folds one replayed record into `mirror`.
fn apply_record(tag: u32, body: &[u8], mirror: &mut MemoryLog, offset: u64) -> Result<()> {
    match tag {
        TAG_ENTRY => {
            let entry = LogEntry::decode_exact(body)?;
            // A replayed entry that duplicates one already held is the
            // signature of a record written twice; overwrite rather than
            // refuse, since the later copy is the one that was durable.
            if entry.index() <= mirror.last_index() && !entry.index().is_empty_sentinel() {
                mirror.truncate_from(entry.index())?;
            }
            mirror.append(std::slice::from_ref(&entry))
        }
        TAG_HARD_STATE => {
            let state = HardState::decode_exact(body)?;
            mirror.save_hard_state(state)
        }
        TAG_TRUNCATE => {
            let index = LogIndex::decode_exact(body)?;
            if index < mirror.first_index() {
                // A truncation into an already-compacted prefix is a no-op
                // on replay: the snapshot sidecar already removed it.
                return Ok(());
            }
            mirror.truncate_from(index)
        }
        TAG_SNAPSHOT => {
            let meta = SnapshotMeta::decode_exact(body)?;
            match mirror.snapshot_meta() {
                // The sidecar (read first, with its data) already installed
                // this boundary; the in-log record only re-states it.
                Some(existing) if existing.last_index() >= meta.last_index() => Ok(()),
                _ => mirror.save_snapshot(&Snapshot::new(meta, Vec::new())),
            }
        }
        _ => Err(RaftError::Corrupt {
            offset,
            reason: "unknown record tag",
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::membership::Membership;
    use crate::types::PeerId;

    /// A unique scratch directory per test, under the platform temp dir.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("astrs-raft-wal-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(term: u64, index: u64) -> LogEntry {
        LogEntry::command(Term::new(term), LogIndex::new(index), vec![index as u8; 4])
    }

    #[test]
    fn a_fresh_log_writes_a_prologue_and_reopens_empty() {
        let dir = scratch("fresh");
        let path = dir.join("raft.wal");
        {
            let log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
            assert_eq!(log.last_index(), LogIndex::ZERO);
            assert!(!log.repaired_tail());
        }
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), PROLOGUE_LEN as usize);
        assert_eq!(&bytes[..8], FILE_MAGIC);

        let reopened = WalLog::open(&path, FsyncPolicy::Always).unwrap();
        assert_eq!(reopened.last_index(), LogIndex::ZERO);
        assert_eq!(reopened.outcome(), RecoveryOutcome::Intact);
    }

    #[test]
    fn entries_and_hard_state_survive_a_reopen() {
        let dir = scratch("reopen");
        let path = dir.join("raft.wal");
        {
            let mut log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
            log.append(&[entry(1, 1), entry(1, 2), entry(2, 3)])
                .unwrap();
            log.save_hard_state(HardState::new(Term::new(2), Some(PeerId::new(3))))
                .unwrap();
        }
        let log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
        assert_eq!(log.last_index(), LogIndex::new(3));
        assert_eq!(log.last_term(), Term::new(2));
        assert_eq!(
            log.hard_state(),
            HardState::new(Term::new(2), Some(PeerId::new(3)))
        );
        assert_eq!(log.entry(LogIndex::new(2)).unwrap().unwrap(), entry(1, 2));
    }

    #[test]
    fn a_truncation_replays_as_a_truncation() {
        let dir = scratch("truncate");
        let path = dir.join("raft.wal");
        {
            let mut log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
            log.append(&[entry(1, 1), entry(1, 2), entry(1, 3)])
                .unwrap();
            log.truncate_from(LogIndex::new(2)).unwrap();
            log.append(&[entry(5, 2)]).unwrap();
        }
        let log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
        assert_eq!(log.last_index(), LogIndex::new(2));
        assert_eq!(
            log.entry(LogIndex::new(2)).unwrap().unwrap().term(),
            Term::new(5)
        );
    }

    /// The contract of this module's header: a file truncated at *any* byte
    /// offset comes back holding exactly the entries that were completely
    /// written before the cut, never an error, never a panic — and the file
    /// is repaired so the next append is not orphaned.
    #[test]
    fn a_torn_tail_at_every_offset_recovers_the_complete_prefix_and_is_repaired() {
        let dir = scratch("torn");
        let path = dir.join("raft.wal");
        {
            let mut log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
            log.append(&(1..=6).map(|index| entry(1, index)).collect::<Vec<_>>())
                .unwrap();
        }
        let whole = std::fs::read(&path).unwrap();

        for cut in 0..whole.len() {
            let cut_path = dir.join(format!("cut-{cut}.wal"));
            std::fs::write(&cut_path, &whole[..cut]).unwrap();
            let log = WalLog::open(&cut_path, FsyncPolicy::Always).unwrap();

            let recovered = log.last_index().get();
            assert!(recovered <= 6, "cut {cut} invented entries");
            // Every recovered entry is readable and correct.
            for index in 1..=recovered {
                assert_eq!(
                    log.entry(LogIndex::new(index)).unwrap().unwrap(),
                    entry(1, index),
                    "cut {cut} corrupted index {index}"
                );
            }
            let expected_len = log.recovered_bytes();
            drop(log);
            let repaired = std::fs::metadata(&cut_path).unwrap().len();
            assert_eq!(
                repaired, expected_len,
                "cut {cut} left a torn tail on disk instead of repairing it"
            );
        }
    }

    /// The reason repair (rather than "stop scanning") matters: after a
    /// torn tail is repaired, appends land where the next open will find
    /// them.
    #[test]
    fn appending_after_a_repair_is_visible_to_the_next_open() {
        let dir = scratch("append-after-repair");
        let path = dir.join("raft.wal");
        {
            let mut log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
            log.append(&[entry(1, 1), entry(1, 2)]).unwrap();
        }
        // Simulate a process killed mid-write: append a partial record.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(&TAG_ENTRY.to_le_bytes());
        bytes.extend_from_slice(&999u32.to_le_bytes());
        bytes.extend_from_slice(&[0xAB; 5]);
        std::fs::write(&path, &bytes).unwrap();

        {
            let mut log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
            assert!(log.repaired_tail());
            assert_eq!(log.outcome(), RecoveryOutcome::TornTail);
            assert_eq!(log.last_index(), LogIndex::new(2));
            log.append(&[entry(1, 3)]).unwrap();
        }

        let log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
        assert_eq!(
            log.last_index(),
            LogIndex::new(3),
            "the append after a repair must not have been written past garbage"
        );
        assert_eq!(log.outcome(), RecoveryOutcome::Intact);
    }

    #[test]
    fn a_flipped_bit_inside_a_record_is_corruption_not_truncation() {
        let dir = scratch("bitflip");
        let path = dir.join("raft.wal");
        {
            let mut log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
            log.append(&[entry(1, 1), entry(1, 2), entry(1, 3)])
                .unwrap();
        }
        let mut bytes = std::fs::read(&path).unwrap();
        // Corrupt a byte inside the *second* record's body.
        let target = PROLOGUE_LEN as usize + 20;
        bytes[target] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        let log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
        assert_eq!(log.outcome(), RecoveryOutcome::Corrupt);
        assert!(log.last_index() < LogIndex::new(3));
    }

    #[test]
    fn a_corrupt_length_prefix_is_caught_by_the_checksum_over_it() {
        let dir = scratch("badlen");
        let path = dir.join("raft.wal");
        {
            let mut log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
            log.append(&[entry(1, 1)]).unwrap();
        }
        let mut bytes = std::fs::read(&path).unwrap();
        // Shrink the declared length: without the CRC covering it, the
        // record would decode as a different, wrong-but-plausible one.
        let len_offset = PROLOGUE_LEN as usize + 4;
        bytes[len_offset] = bytes[len_offset].wrapping_sub(1);
        std::fs::write(&path, &bytes).unwrap();

        let log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
        assert_eq!(log.outcome(), RecoveryOutcome::Corrupt);
        assert_eq!(log.last_index(), LogIndex::ZERO);
    }

    #[test]
    fn a_bad_magic_is_a_hard_error_not_a_silent_fresh_log() {
        let dir = scratch("magic");
        let path = dir.join("raft.wal");
        std::fs::write(&path, b"NOTAWALL\x01\x00").unwrap();
        let error = WalLog::open(&path, FsyncPolicy::Always).unwrap_err();
        assert!(error.is_corruption(), "{error}");
    }

    #[test]
    fn a_newer_format_version_is_refused() {
        let dir = scratch("version");
        let path = dir.join("raft.wal");
        let mut bytes = FILE_MAGIC.to_vec();
        bytes.extend_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            WalLog::open(&path, FsyncPolicy::Always).unwrap_err(),
            RaftError::UnsupportedVersion { .. }
        ));
    }

    #[test]
    fn a_snapshot_compacts_the_file_and_survives_a_reopen() {
        let dir = scratch("snapshot");
        let path = dir.join("raft.wal");
        let membership = Membership::new([PeerId::new(1), PeerId::new(2)]);
        let before_len;
        {
            let mut log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
            log.append(&(1..=50).map(|index| entry(1, index)).collect::<Vec<_>>())
                .unwrap();
            before_len = std::fs::metadata(&path).unwrap().len();
            log.save_snapshot(&Snapshot::new(
                SnapshotMeta::new(LogIndex::new(40), Term::new(1), membership.clone()),
                vec![0xCD; 16],
            ))
            .unwrap();
            assert!(std::fs::metadata(&path).unwrap().len() < before_len);
        }

        let log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
        assert_eq!(log.first_index(), LogIndex::new(41));
        assert_eq!(log.last_index(), LogIndex::new(50));
        let snapshot = log.snapshot().unwrap().unwrap();
        assert_eq!(snapshot.data, vec![0xCD; 16]);
        assert_eq!(snapshot.meta.membership(), &membership);
        assert_eq!(log.term_at(LogIndex::new(40)).unwrap(), Some(Term::new(1)));
    }

    #[test]
    fn appending_after_a_compaction_reopens_correctly() {
        let dir = scratch("append-after-compaction");
        let path = dir.join("raft.wal");
        {
            let mut log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
            log.append(&(1..=10).map(|index| entry(1, index)).collect::<Vec<_>>())
                .unwrap();
            log.save_snapshot(&Snapshot::new(
                SnapshotMeta::new(LogIndex::new(10), Term::new(1), Membership::default()),
                vec![1, 2, 3],
            ))
            .unwrap();
            log.append(&[entry(2, 11), entry(2, 12)]).unwrap();
        }
        let log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
        assert_eq!(log.first_index(), LogIndex::new(11));
        assert_eq!(log.last_index(), LogIndex::new(12));
        assert_eq!(log.entries(LogIndex::new(11), 10).unwrap().len(), 2);
    }

    #[test]
    fn an_installed_snapshot_discards_local_entries_on_disk_too() {
        let dir = scratch("install");
        let path = dir.join("raft.wal");
        {
            let mut log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
            log.append(&(1..=5).map(|index| entry(1, index)).collect::<Vec<_>>())
                .unwrap();
            log.install_snapshot(&Snapshot::new(
                SnapshotMeta::new(LogIndex::new(3), Term::new(7), Membership::default()),
                vec![9],
            ))
            .unwrap();
        }
        let log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
        assert_eq!(log.last_index(), LogIndex::new(3));
        assert_eq!(log.last_term(), Term::new(7));
        assert_eq!(log.first_index(), LogIndex::new(4));
    }

    #[test]
    fn fsync_policies_decide_when_a_sync_happens() {
        assert!(FsyncPolicy::Always.should_sync(1));
        assert!(!FsyncPolicy::Never.should_sync(1_000));
        assert!(!FsyncPolicy::EveryN(4).should_sync(3));
        assert!(FsyncPolicy::EveryN(4).should_sync(4));
        // A zero interval is a degenerate "every write", not "never".
        assert!(FsyncPolicy::EveryN(0).should_sync(1));
    }

    #[test]
    fn a_never_syncing_log_is_still_correct_within_the_process() {
        let dir = scratch("nosync");
        let path = dir.join("raft.wal");
        {
            let mut log = WalLog::open(&path, FsyncPolicy::Never).unwrap();
            log.append(&[entry(1, 1), entry(1, 2)]).unwrap();
            assert!(!log.policy().is_crash_safe());
        }
        let log = WalLog::open(&path, FsyncPolicy::Never).unwrap();
        assert_eq!(log.last_index(), LogIndex::new(2));
    }

    #[test]
    fn a_missing_snapshot_sidecar_leaves_the_log_usable() {
        let dir = scratch("no-sidecar");
        let path = dir.join("raft.wal");
        {
            let mut log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
            log.append(&[entry(1, 1)]).unwrap();
        }
        assert!(!snapshot_sidecar(&path).exists());
        let log = WalLog::open(&path, FsyncPolicy::Always).unwrap();
        assert!(log.snapshot().unwrap().is_none());
    }

    #[test]
    fn a_forged_record_length_is_refused_before_allocating() {
        let mut bytes = TAG_ENTRY.to_le_bytes().to_vec();
        bytes.extend_from_slice(&(MAX_RECORD_BODY + 1).to_le_bytes());
        let error = read_record(&bytes, 10).unwrap_err();
        assert!(matches!(error, RaftError::Corrupt { offset: 10, .. }));
    }

    #[test]
    fn unknown_record_tags_are_corruption() {
        let dir = scratch("unknown-tag");
        let path = dir.join("raft.wal");
        {
            let _ = WalLog::open(&path, FsyncPolicy::Always).unwrap();
        }
        let mut bytes = std::fs::read(&path).unwrap();
        append_framed(&mut bytes, 0xDEAD_BEEF, b"body").unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let error = WalLog::open(&path, FsyncPolicy::Always).unwrap_err();
        assert!(matches!(error, RaftError::Corrupt { .. }), "{error}");
    }

    #[test]
    fn the_sidecar_path_sits_beside_the_log() {
        let sidecar = snapshot_sidecar(Path::new("/var/lib/astrs/raft.wal"));
        assert!(sidecar.to_string_lossy().ends_with("raft.wal.snap"));
    }
}
