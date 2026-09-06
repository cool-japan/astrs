//! Size-based rotating file writer for on-disk daemon logs.

use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{IoResultExt, LogError, Result};
use crate::record::LogRecord;

/// How often the writer calls `fsync` (via [`File::sync_data`]) after a
/// write.
///
/// Every policy other than `Never` trades throughput for durability: a
/// crash between an unsynced write and the next sync can lose the tail of
/// the log (never a torn/partial line — writes themselves are always
/// complete before the size/count bookkeeping advances).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FsyncPolicy {
    /// Never call `fsync` explicitly; rely on the OS to flush eventually.
    /// Fastest, least durable.
    #[default]
    Never,
    /// Call `fsync` after every single record. Slowest, most durable.
    EveryWrite,
    /// Call `fsync` after every `N` records (`N == 0` behaves like
    /// [`FsyncPolicy::Never`]).
    EveryN(u32),
}

/// Configuration for a [`RotatingWriter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationConfig {
    /// Rotate once the active file would exceed this many bytes.
    /// `0` disables size-based rotation entirely — the writer appends to
    /// one ever-growing file.
    pub max_log_size: u64,
    /// How many rotated (archived) files to retain, named `<path>.1`
    /// (most recent) through `<path>.<max_rotated_files>` (oldest).
    /// `0` means "keep no archives": rotation just truncates the active
    /// file in place instead of renaming it out.
    pub max_rotated_files: usize,
    /// When to call `fsync` after a write.
    pub fsync_policy: FsyncPolicy,
}

impl Default for RotationConfig {
    /// `max_log_size: 10 MiB`, `max_rotated_files: 5` (the blueprint's
    /// documented default, §8.3), `fsync_policy: Never`. The size default
    /// is this crate's own judgment call — the blueprint does not specify
    /// one — and is overridden per-node by the manifest's `max_log_size`
    /// field.
    fn default() -> Self {
        Self {
            max_log_size: 10 * 1024 * 1024,
            max_rotated_files: 5,
            fsync_policy: FsyncPolicy::Never,
        }
    }
}

/// The path an archived (rotated-out) log file lives at: `<base>.<index>`.
///
/// `index == 1` is the most recently rotated file; higher indices are
/// older. Exposed so a log reader (or the CLI) can enumerate a node's
/// archived files without duplicating this naming rule.
///
/// # Examples
///
/// ```
/// use astrs_log::rotated_path;
/// use std::path::Path;
///
/// assert_eq!(rotated_path(Path::new("/var/log/astrs/daemon.log"), 1).to_str().unwrap(), "/var/log/astrs/daemon.log.1");
/// ```
#[must_use]
pub fn rotated_path(base: &Path, index: usize) -> PathBuf {
    let mut name = base.as_os_str().to_owned();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

/// Removes a file if it exists; a missing file is not an error.
fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(LogError::Io {
            path: path.to_path_buf(),
            source: e,
        }),
    }
}

/// The mutable state behind [`RotatingWriter`]'s mutex.
#[derive(Debug)]
struct Inner {
    path: PathBuf,
    file: File,
    current_size: u64,
    writes_since_sync: u32,
    config: RotationConfig,
}

impl Inner {
    /// Writes one line (a newline is appended; `line` must not itself
    /// contain one), rotating first if this write would push the active
    /// file over [`RotationConfig::max_log_size`].
    ///
    /// A record larger than `max_log_size` on its own is written anyway
    /// after at most one rotation — the `current_size > 0` guard below
    /// stops an already-empty file from being pointlessly rotated, which
    /// both avoids an infinite rotate loop and avoids evicting an archive
    /// slot for zero bytes of content.
    fn write_line(&mut self, line: &str) -> Result<()> {
        let line_len = line.len() as u64 + 1; // +1 for the trailing newline
        let would_exceed = self.config.max_log_size > 0
            && self.current_size > 0
            && self.current_size + line_len > self.config.max_log_size;
        if would_exceed {
            self.rotate()?;
        }
        self.file.write_all(line.as_bytes()).log_io(&self.path)?;
        self.file.write_all(b"\n").log_io(&self.path)?;
        self.current_size += line_len;
        self.writes_since_sync += 1;
        self.maybe_sync()?;
        Ok(())
    }

    /// Rotates the active file out and starts a fresh one.
    ///
    /// With `max_rotated_files == 0` this truncates the currently open
    /// file in place (no rename, no archive). Otherwise it evicts the
    /// oldest archive, shifts every remaining archive up by one index
    /// (processing from the highest surviving index down to `1`, so each
    /// destination is vacated by the previous step before being written
    /// to), renames the active file to `<path>.1`, and opens a fresh
    /// active file. Renaming a file this process still has open is safe
    /// on every platform Rust targets here: std's `File` opens with
    /// share-delete permissions on Windows specifically so this pattern
    /// works, and POSIX renames never invalidate open descriptors.
    fn rotate(&mut self) -> Result<()> {
        if self.config.max_rotated_files == 0 {
            self.file.set_len(0).log_io(&self.path)?;
            self.file.seek(SeekFrom::Start(0)).log_io(&self.path)?;
        } else {
            let oldest = rotated_path(&self.path, self.config.max_rotated_files);
            remove_if_exists(&oldest)?;
            for i in (1..self.config.max_rotated_files).rev() {
                let src = rotated_path(&self.path, i);
                if src.exists() {
                    fs::rename(&src, rotated_path(&self.path, i + 1)).log_io(&src)?;
                }
            }
            fs::rename(&self.path, rotated_path(&self.path, 1)).log_io(&self.path)?;
            self.file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .log_io(&self.path)?;
        }
        self.current_size = 0;
        self.writes_since_sync = 0;
        Ok(())
    }

    fn maybe_sync(&mut self) -> Result<()> {
        let due = match self.config.fsync_policy {
            FsyncPolicy::Never => false,
            FsyncPolicy::EveryWrite => true,
            FsyncPolicy::EveryN(n) => n > 0 && self.writes_since_sync >= n,
        };
        if due {
            self.file.sync_data().log_io(&self.path)?;
            self.writes_since_sync = 0;
        }
        Ok(())
    }
}

/// A size-rotating, retention-limited, line-oriented JSON log file writer.
///
/// Safe to share across threads behind a plain `&RotatingWriter` (typically
/// via `Arc`): every write locks an internal [`Mutex`], so callers never
/// need their own external synchronization. If a writer thread panics
/// mid-write the mutex is recovered rather than left permanently poisoned
/// (`lock().unwrap_or_else(PoisonError::into_inner)`) — the in-memory
/// bookkeeping (`current_size`, the open `File`) stays internally
/// consistent even after a poisoning panic, since panics can only occur in
/// the `serde_json`/`io` calls this module itself makes, never mid-mutation
/// of that bookkeeping. The one residual risk every `Mutex`-guarded logger
/// shares: a panic strictly *between* the two internal `write_all` calls
/// that make up one record write can leave a torn final line with no trailing
/// newline; the next successful write still appends correctly after it.
///
/// # Examples
///
/// ```
/// use astrs_log::{HlcTimestamp, LogLevel, LogRecord, RotatingWriter, RotationConfig};
/// use std::sync::atomic::{AtomicU64, Ordering};
/// use std::sync::Arc;
///
/// static COUNTER: AtomicU64 = AtomicU64::new(0);
/// let mut dir = std::env::temp_dir();
/// dir.push(format!("astrs-log-doctest-{}", COUNTER.fetch_add(1, Ordering::Relaxed)));
/// std::fs::create_dir_all(&dir).unwrap();
/// let path = dir.join("daemon.log");
///
/// let writer = RotatingWriter::open(&path, RotationConfig::default()).unwrap();
/// let record = LogRecord::new(HlcTimestamp::default(), LogLevel::Info, "t", "hello");
/// writer.write_record(&record).unwrap();
/// assert!(writer.current_size() > 0);
///
/// std::fs::remove_dir_all(&dir).ok();
/// ```
#[derive(Debug)]
pub struct RotatingWriter {
    inner: Mutex<Inner>,
}

impl RotatingWriter {
    /// Opens (creating if absent, and creating parent directories if
    /// absent) the log file at `path` for appending, seeding the rotation
    /// bookkeeping from the file's *current* length — reopening a
    /// 90%-full file and continuing to write it rotates at the right
    /// cumulative size instead of resetting the threshold.
    ///
    /// # Errors
    ///
    /// Returns [`LogError::Io`] if the parent directory cannot be created,
    /// the file cannot be opened, or its metadata cannot be read.
    pub fn open(path: impl Into<PathBuf>, config: RotationConfig) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).log_io(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .log_io(&path)?;
        let current_size = file.metadata().log_io(&path)?.len();
        Ok(Self {
            inner: Mutex::new(Inner {
                path,
                file,
                current_size,
                writes_since_sync: 0,
                config,
            }),
        })
    }

    /// Serializes `record` to compact JSON and appends it as one line,
    /// rotating first if needed.
    ///
    /// # Errors
    ///
    /// Returns [`LogError::Encode`] if JSON serialization fails (see that
    /// variant's docs — effectively unreachable for well-formed records)
    /// or [`LogError::Io`] if a filesystem operation fails.
    pub fn write_record(&self, record: &LogRecord) -> Result<()> {
        let line = serde_json::to_string(record).map_err(LogError::Encode)?;
        self.lock().write_line(&line)
    }

    /// Appends a pre-formatted line as-is (a trailing newline is added;
    /// `line` must not contain an embedded one — this is not validated,
    /// since there is no safe recovery once bytes have been written).
    ///
    /// Lower-level than [`RotatingWriter::write_record`]; intended for
    /// callers that already hold a serialized line (e.g. one being
    /// relayed byte-for-byte from another writer) and want to skip a
    /// decode/re-encode round trip.
    ///
    /// # Errors
    ///
    /// Returns [`LogError::Io`] if a filesystem operation fails.
    pub fn write_raw_line(&self, line: &str) -> Result<()> {
        self.lock().write_line(line)
    }

    /// The log file's current path (the active file, not an archive).
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.lock().path.clone()
    }

    /// The active file's current size in bytes, as tracked by this
    /// writer (matches the file's actual length as long as nothing else
    /// on the system is writing to the same path).
    #[must_use]
    pub fn current_size(&self) -> u64 {
        self.lock().current_size
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for RotatingWriter {
    /// Best-effort final `fsync`, regardless of [`FsyncPolicy`], so a
    /// clean shutdown (the writer simply going out of scope) does not
    /// lose the last few writes an `EveryN`/`Never` policy had not yet
    /// flushed. `Drop` cannot return a `Result` and must not panic, so
    /// any I/O error here is silently discarded — no fsync policy weaker
    /// than `EveryWrite` ever promised durability across an *unclean*
    /// shutdown (a crash) in the first place.
    fn drop(&mut self) {
        let inner = self.lock();
        let _ = inner.file.sync_data();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::level::LogLevel;
    use crate::test_util::unique_temp_dir;
    use astrs_time::HlcTimestamp;
    use std::fs;
    use std::sync::Arc;

    fn rec(seq: u64) -> LogRecord {
        LogRecord::new(
            HlcTimestamp::new(seq, 0),
            LogLevel::Info,
            "t",
            "x".repeat(50),
        )
        .with_seq(seq)
    }

    fn line_count(path: &Path) -> usize {
        fs::read_to_string(path)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    #[test]
    fn writes_are_line_oriented_json() {
        let dir = unique_temp_dir("rotate-basic");
        let path = dir.join("log.jsonl");
        let writer = RotatingWriter::open(&path, RotationConfig::default()).unwrap();
        writer.write_record(&rec(0)).unwrap();
        writer.write_record(&rec(1)).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let parsed: LogRecord = serde_json::from_str(line).unwrap();
            assert_eq!(parsed.target, "t");
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rotates_when_size_threshold_is_crossed() {
        let dir = unique_temp_dir("rotate-threshold");
        let path = dir.join("log.jsonl");
        // Each record serializes to ~90 bytes; cap tight enough that the
        // 2nd record cannot fit alongside the 1st.
        let config = RotationConfig {
            max_log_size: 100,
            max_rotated_files: 5,
            ..RotationConfig::default()
        };
        let writer = RotatingWriter::open(&path, config).unwrap();
        writer.write_record(&rec(0)).unwrap();
        writer.write_record(&rec(1)).unwrap();
        assert!(
            rotated_path(&path, 1).exists(),
            "first rotation should have produced .1"
        );
        assert_eq!(
            line_count(&rotated_path(&path, 1)),
            1,
            "archive holds exactly the first record"
        );
        assert_eq!(
            line_count(&path),
            1,
            "active file holds exactly the second record"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rename_ordering_keeps_1_as_most_recent() {
        let dir = unique_temp_dir("rotate-order");
        let path = dir.join("log.jsonl");
        let config = RotationConfig {
            max_log_size: 100,
            max_rotated_files: 3,
            ..RotationConfig::default()
        };
        let writer = RotatingWriter::open(&path, config).unwrap();
        // Each write is its own file's worth (tiny cap), so N writes produce N-1 rotations.
        for i in 0..4u64 {
            writer.write_record(&rec(i)).unwrap();
        }
        // Records 0,1,2 have each been rotated out in order; record 3 is active.
        // Archive .1 = most recently rotated = record 2; .2 = record 1; .3 = record 0.
        let read_seq = |p: &Path| -> u64 {
            let content = fs::read_to_string(p).unwrap();
            let parsed: LogRecord = serde_json::from_str(content.lines().next().unwrap()).unwrap();
            parsed.seq
        };
        assert_eq!(read_seq(&rotated_path(&path, 1)), 2);
        assert_eq!(read_seq(&rotated_path(&path, 2)), 1);
        assert_eq!(read_seq(&rotated_path(&path, 3)), 0);
        assert_eq!(read_seq(&path), 3);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn retention_drops_the_oldest_archive() {
        let dir = unique_temp_dir("rotate-retention");
        let path = dir.join("log.jsonl");
        let config = RotationConfig {
            max_log_size: 100,
            max_rotated_files: 2,
            ..RotationConfig::default()
        };
        let writer = RotatingWriter::open(&path, config).unwrap();
        for i in 0..5u64 {
            writer.write_record(&rec(i)).unwrap();
        }
        assert!(rotated_path(&path, 1).exists());
        assert!(rotated_path(&path, 2).exists());
        assert!(
            !rotated_path(&path, 3).exists(),
            "retention caps archives at max_rotated_files"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn zero_retention_truncates_in_place_without_archiving() {
        let dir = unique_temp_dir("rotate-zero-retention");
        let path = dir.join("log.jsonl");
        let config = RotationConfig {
            max_log_size: 100,
            max_rotated_files: 0,
            ..RotationConfig::default()
        };
        let writer = RotatingWriter::open(&path, config).unwrap();
        for i in 0..5u64 {
            writer.write_record(&rec(i)).unwrap();
        }
        assert!(
            !rotated_path(&path, 1).exists(),
            "no archives are ever created"
        );
        assert_eq!(
            line_count(&path),
            1,
            "only the latest record survives each truncation"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn zero_max_log_size_disables_rotation() {
        let dir = unique_temp_dir("rotate-unlimited");
        let path = dir.join("log.jsonl");
        let config = RotationConfig {
            max_log_size: 0,
            max_rotated_files: 5,
            ..RotationConfig::default()
        };
        let writer = RotatingWriter::open(&path, config).unwrap();
        for i in 0..50u64 {
            writer.write_record(&rec(i)).unwrap();
        }
        assert!(!rotated_path(&path, 1).exists());
        assert_eq!(line_count(&path), 50);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oversized_single_record_rotates_at_most_once_then_is_written() {
        let dir = unique_temp_dir("rotate-oversized");
        let path = dir.join("log.jsonl");
        let config = RotationConfig {
            max_log_size: 10,
            max_rotated_files: 5,
            ..RotationConfig::default()
        };
        let writer = RotatingWriter::open(&path, config).unwrap();
        // The very first record already exceeds max_log_size (10 bytes);
        // it must still be written, into the (empty) active file, with no
        // rotation (nothing to preserve by rotating an empty file).
        writer.write_record(&rec(0)).unwrap();
        assert!(!rotated_path(&path, 1).exists());
        assert_eq!(line_count(&path), 1);
        // The *second* write now sees a non-empty, over-cap active file
        // and rotates it out before writing.
        writer.write_record(&rec(1)).unwrap();
        assert!(rotated_path(&path, 1).exists());
        assert_eq!(line_count(&path), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopening_an_existing_file_seeds_size_from_disk() {
        let dir = unique_temp_dir("rotate-reopen");
        let path = dir.join("log.jsonl");
        // Each `rec(_)` line is 143 bytes including its newline: two fit
        // comfortably under 300, three (429) do not.
        let config = RotationConfig {
            max_log_size: 300,
            max_rotated_files: 5,
            ..RotationConfig::default()
        };
        {
            let writer = RotatingWriter::open(&path, config.clone()).unwrap();
            writer.write_record(&rec(0)).unwrap();
            writer.write_record(&rec(1)).unwrap();
            assert!(
                !rotated_path(&path, 1).exists(),
                "two records should still fit under 300 bytes"
            );
        }
        // Reopen: a writer that started current_size at 0 instead of the
        // file's actual length would need 2 more records to hit the cap;
        // seeded correctly, 1 more record should be enough to rotate.
        {
            let writer = RotatingWriter::open(&path, config).unwrap();
            assert!(
                writer.current_size() > 0,
                "size must be seeded from the reopened file, not reset to 0"
            );
            writer.write_record(&rec(2)).unwrap();
            assert!(
                rotated_path(&path, 1).exists(),
                "reopened writer must respect the pre-existing size"
            );
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fsync_policies_do_not_error() {
        for policy in [
            FsyncPolicy::Never,
            FsyncPolicy::EveryWrite,
            FsyncPolicy::EveryN(2),
        ] {
            let dir = unique_temp_dir("rotate-fsync");
            let path = dir.join("log.jsonl");
            let config = RotationConfig {
                fsync_policy: policy,
                ..RotationConfig::default()
            };
            let writer = RotatingWriter::open(&path, config).unwrap();
            for i in 0..5u64 {
                writer.write_record(&rec(i)).unwrap();
            }
            assert_eq!(line_count(&path), 5);
            fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn concurrent_writers_do_not_corrupt_or_lose_lines() {
        let dir = unique_temp_dir("rotate-concurrent");
        let path = dir.join("log.jsonl");
        // Rotation is deliberately disabled here (max_log_size: 0): this
        // test isolates the Mutex's mutual-exclusion guarantee (no torn
        // or interleaved lines from concurrent writers) from rotation and
        // retention, which have their own dedicated single-threaded tests
        // above and would otherwise legitimately evict older records,
        // muddying what a failure here would mean.
        let config = RotationConfig {
            max_log_size: 0,
            ..RotationConfig::default()
        };
        let writer = Arc::new(RotatingWriter::open(&path, config).unwrap());

        const THREADS: u64 = 8;
        const PER_THREAD: u64 = 50;
        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let writer = Arc::clone(&writer);
                scope.spawn(move || {
                    for i in 0..PER_THREAD {
                        let record = LogRecord::new(
                            HlcTimestamp::new(t * PER_THREAD + i, 0),
                            LogLevel::Info,
                            "t",
                            "y".repeat(20),
                        )
                        .with_node(format!("thread-{t}"))
                        .with_seq(i);
                        writer.write_record(&record).unwrap();
                    }
                });
            }
        });

        // Every line must be valid, complete JSON -- a corrupted or
        // interleaved write would produce a line that fails to parse --
        // and every one of the 400 records written must be present.
        let content = fs::read_to_string(&path).unwrap();
        let mut total = 0usize;
        for line in content.lines() {
            let _: LogRecord = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("corrupted line: {e} -- line was {line:?}"));
            total += 1;
        }
        assert_eq!(
            total as u64,
            THREADS * PER_THREAD,
            "no record was lost or corrupted"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
