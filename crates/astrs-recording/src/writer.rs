//! [`Writer`] — streaming `.arec` output with bounded memory and optional
//! rotation (blueprint §14).
//!
//! Each [`Writer::append`] call encodes and writes exactly one entry
//! frame and flushes it before returning: nothing about a payload's bytes
//! is ever buffered beyond the call that wrote them, so memory use tracks
//! the *number* of entries recorded (one small [`crate::index::IndexEntry`]
//! per entry, kept for the closing footer) rather than their total size.
//! This is also what makes the writer crash-tolerant: every entry
//! [`Writer::append`] returns `Ok` for is already durable on disk (the OS
//! page cache aside), so a process killed at any later point leaves a
//! file [`crate::recover::scan`] can read back to exactly that point —
//! see that module for the recovery half of this contract.
//!
//! # Rotation
//!
//! [`RotationPolicy`] triggers a new file once the current one exceeds a
//! byte or wall-clock bound. Rotated files are always numbered, including
//! the first: a base path `session.arec` becomes `session-0001.arec`,
//! `session-0002.arec`, and so on — a writer created with rotation
//! *disabled* uses the base path exactly as given, with no suffix. This
//! asymmetry (numbered only when rotation might produce more than one
//! file) is deliberate: a fixed suffix on every recording, rotated or
//! not, would make the common single-file case harder to name from the
//! command line for no benefit.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use astrs_time::HlcTimestamp;
use astrs_wire::{DataId, DataflowId, Metadata, NodeId};

use crate::entry::Entry;
use crate::error::RecordingError;
use crate::header::Header;
use crate::index::{Footer, IndexEntry};

/// When a [`Writer`] should stop appending to its current file and start a
/// new one.
///
/// The default policy never rotates — see [`RotationPolicy::none`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RotationPolicy {
    /// Rotate once the current file's byte size would exceed this.
    pub max_bytes: Option<u64>,
    /// Rotate once the current file has been open this long.
    pub max_duration: Option<Duration>,
}

impl RotationPolicy {
    /// Never rotates; one file for the whole session.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            max_bytes: None,
            max_duration: None,
        }
    }

    /// Rotates once a file's size would exceed `bytes`.
    #[must_use]
    pub const fn by_size(bytes: u64) -> Self {
        Self {
            max_bytes: Some(bytes),
            max_duration: None,
        }
    }

    /// Rotates once a file has been open longer than `duration`.
    #[must_use]
    pub const fn by_duration(duration: Duration) -> Self {
        Self {
            max_bytes: None,
            max_duration: Some(duration),
        }
    }

    /// Whether either bound is set.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.max_bytes.is_some() || self.max_duration.is_some()
    }

    /// Whether `size` bytes / `age` of file life trips this policy.
    #[must_use]
    fn is_exceeded(&self, size: u64, age: Duration) -> bool {
        self.max_bytes.is_some_and(|max| size > max)
            || self.max_duration.is_some_and(|max| age > max)
    }
}

/// The fixed session settings a [`Writer`] carries across every file it
/// opens (including across rotation).
#[derive(Debug, Clone)]
pub struct WriterOptions {
    /// The dataflow being recorded.
    pub dataflow: DataflowId,
    /// The HLC reading recording began at, stamped into every file's
    /// header.
    pub hlc_epoch: HlcTimestamp,
    /// The dataflow's manifest YAML, embedded in every file's header.
    pub manifest_yaml: String,
    /// The rotation policy.
    pub rotation: RotationPolicy,
}

impl WriterOptions {
    /// Options for `dataflow`, with no manifest and no rotation.
    #[must_use]
    pub fn new(dataflow: DataflowId, hlc_epoch: HlcTimestamp) -> Self {
        Self {
            dataflow,
            hlc_epoch,
            manifest_yaml: String::new(),
            rotation: RotationPolicy::none(),
        }
    }

    /// Attaches the dataflow's manifest YAML.
    #[must_use]
    pub fn with_manifest_yaml(mut self, yaml: impl Into<String>) -> Self {
        self.manifest_yaml = yaml.into();
        self
    }

    /// Sets the rotation policy.
    #[must_use]
    pub const fn with_rotation(mut self, rotation: RotationPolicy) -> Self {
        self.rotation = rotation;
        self
    }
}

/// The numbered path for rotation segment `sequence` (1-based) of `base`.
///
/// `session.arec` at sequence 1 becomes `session-0001.arec`; a base path
/// with no extension keeps it that way (`session` → `session-0001`).
#[must_use]
fn segment_path(base: &Path, sequence: u32) -> PathBuf {
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("recording");
    let name = match base.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem}-{sequence:04}.{ext}"),
        None => format!("{stem}-{sequence:04}"),
    };
    match base.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

/// A streaming `.arec` writer.
///
/// # Examples
///
/// ```
/// use astrs_recording::{Entry, Writer, WriterOptions};
/// use astrs_time::HlcTimestamp;
/// use astrs_wire::{DataId, DataflowId, Metadata, NodeId};
///
/// let dir = std::env::temp_dir().join(format!("astrs-recording-doctest-{}", std::process::id()));
/// std::fs::create_dir_all(&dir)?;
/// let path = dir.join("session.arec");
///
/// let options = WriterOptions::new(DataflowId::from_u128(1), HlcTimestamp::new(1_000, 0));
/// let mut writer = Writer::create(&path, options)?;
/// writer.append(Entry::new(
///     NodeId::new("camera")?,
///     DataId::new("frames")?,
///     Metadata::new(HlcTimestamp::new(1_000, 1)),
///     vec![1, 2, 3],
/// ))?;
/// let files = writer.finish()?;
/// assert_eq!(files, vec![path.clone()]);
/// # std::fs::remove_dir_all(&dir).ok();
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct Writer {
    base_path: PathBuf,
    options: WriterOptions,
    sequence: u32,
    current_path: PathBuf,
    file: BufWriter<File>,
    /// Bytes written to the current file so far (header included).
    offset: u64,
    index: Vec<IndexEntry>,
    opened_at: Instant,
    completed_files: Vec<PathBuf>,
    finished: bool,
}

impl Writer {
    /// Creates a new `.arec` writer at `path` (or, with rotation enabled,
    /// at that path's first numbered segment — see the module docs).
    ///
    /// # Errors
    ///
    /// [`RecordingError::Io`] if the file cannot be created, or
    /// [`RecordingError::Wire`] if the header cannot be encoded.
    pub fn create(path: impl AsRef<Path>, options: WriterOptions) -> Result<Self, RecordingError> {
        let base_path = path.as_ref().to_path_buf();
        let sequence = 1;
        let current_path = if options.rotation.is_enabled() {
            segment_path(&base_path, sequence)
        } else {
            base_path.clone()
        };

        let mut writer = Self {
            base_path,
            options,
            sequence,
            current_path: current_path.clone(),
            file: open_for_write(&current_path)?,
            offset: 0,
            index: Vec::new(),
            opened_at: Instant::now(),
            completed_files: Vec::new(),
            finished: false,
        };
        writer.write_header()?;
        Ok(writer)
    }

    /// The file currently being written.
    #[must_use]
    pub fn current_path(&self) -> &Path {
        &self.current_path
    }

    /// How many entries have been written to the current file.
    #[must_use]
    pub fn entries_in_current_file(&self) -> usize {
        self.index.len()
    }

    /// The current file's size on disk so far, header included.
    #[must_use]
    pub const fn current_size(&self) -> u64 {
        self.offset
    }

    /// Appends one entry, flushing it to disk before returning, and
    /// rotates to a fresh file afterward if the rotation policy now
    /// requires it.
    ///
    /// # Errors
    ///
    /// [`RecordingError::Io`] on a write or flush failure, or
    /// [`RecordingError::Wire`] if the entry cannot be encoded.
    pub fn append(&mut self, entry: Entry) -> Result<(), RecordingError> {
        let index_entry = IndexEntry {
            offset: self.offset,
            hlc: entry.hlc(),
            node: entry.node.clone(),
            output: entry.output.clone(),
            payload_len: entry.payload.len() as u64,
        };

        let mut buffer = Vec::new();
        entry.write(&mut buffer)?;
        self.write_all(&buffer)?;
        self.index.push(index_entry);

        if self
            .options
            .rotation
            .is_exceeded(self.offset, self.opened_at.elapsed())
        {
            self.rotate()?;
        }
        Ok(())
    }

    /// A convenience for callers building an entry from its parts rather
    /// than an already-constructed [`Entry`].
    ///
    /// # Errors
    ///
    /// As [`Writer::append`].
    pub fn append_parts(
        &mut self,
        node: NodeId,
        output: DataId,
        meta: Metadata,
        payload: Vec<u8>,
    ) -> Result<(), RecordingError> {
        self.append(Entry::new(node, output, meta, payload))
    }

    /// Finalizes every file this writer produced: writes the current
    /// file's footer and trailer, and returns every completed file's
    /// path, in the order they were opened.
    ///
    /// Idempotent: calling it again returns the same list without
    /// re-writing anything.
    ///
    /// # Errors
    ///
    /// [`RecordingError::Io`] on a write/flush failure, or
    /// [`RecordingError::Wire`] if the footer cannot be encoded.
    pub fn finish(&mut self) -> Result<Vec<PathBuf>, RecordingError> {
        if !self.finished {
            self.finalize_current()?;
            self.finished = true;
        }
        Ok(self.completed_files.clone())
    }

    /// Writes this writer's header to its (freshly opened) current file.
    fn write_header(&mut self) -> Result<(), RecordingError> {
        let header = Header::new(self.options.dataflow, self.options.hlc_epoch)
            .with_manifest_yaml(self.options.manifest_yaml.clone());
        let mut buffer = Vec::new();
        header.write(&mut buffer)?;
        self.write_all(&buffer)
    }

    /// Writes `bytes` to the current file and advances [`Writer::offset`].
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), RecordingError> {
        self.file
            .write_all(bytes)
            .map_err(|error| RecordingError::io("writing an .arec frame", error))?;
        self.file
            .flush()
            .map_err(|error| RecordingError::io("flushing an .arec frame", error))?;
        self.offset += bytes.len() as u64;
        Ok(())
    }

    /// Writes the footer and trailer to the current file, without
    /// touching [`Writer::finished`] — used by both [`Writer::finish`]
    /// and [`Writer::rotate`].
    fn finalize_current(&mut self) -> Result<(), RecordingError> {
        let footer = Footer {
            entries: std::mem::take(&mut self.index),
        };
        let mut buffer = Vec::new();
        let (footer_offset, footer_len) = footer.write(&mut buffer)?;
        crate::format::write_trailer(&mut buffer, self.offset + footer_offset, footer_len);
        self.write_all(&buffer)?;
        self.completed_files.push(self.current_path.clone());
        Ok(())
    }

    /// Finalizes the current file and opens the next numbered segment.
    fn rotate(&mut self) -> Result<(), RecordingError> {
        self.finalize_current()?;
        self.sequence += 1;
        self.current_path = segment_path(&self.base_path, self.sequence);
        self.file = open_for_write(&self.current_path)?;
        self.offset = 0;
        self.opened_at = Instant::now();
        self.write_header()
    }
}

impl Drop for Writer {
    /// Best-effort finalization on an early drop (a `?` unwind, a panic
    /// elsewhere in the same scope). A process that is killed outright
    /// still leaves a scan-recoverable file (see [`crate::recover`]) —
    /// this is the graceful-shutdown half of that same contract, letting
    /// the fast path (the file's own footer) apply whenever it can.
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.finalize_current();
        }
    }
}

/// Opens `path` for writing, truncating any existing file — creating
/// parent directories first if they do not exist.
fn open_for_write(path: &Path) -> Result<BufWriter<File>, RecordingError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| RecordingError::io("creating the recording directory", error))?;
    }
    let file =
        File::create(path).map_err(|error| RecordingError::io("creating .arec file", error))?;
    Ok(BufWriter::new(file))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "astrs-recording-writer-test-{}-{}-{label}.arec",
            std::process::id(),
            uniq()
        ))
    }

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn entry(seq: i64) -> Entry {
        let mut meta = Metadata::new(HlcTimestamp::new(1_000 + seq as u64, 0));
        meta.set_seq(seq);
        Entry::new(
            NodeId::new("camera").unwrap(),
            DataId::new("frames").unwrap(),
            meta,
            vec![seq as u8; 16],
        )
    }

    #[test]
    fn a_fresh_writer_starts_with_only_a_header() {
        let path = temp_path("fresh");
        let options = WriterOptions::new(DataflowId::from_u128(1), HlcTimestamp::EPOCH);
        let writer = Writer::create(&path, options).unwrap();
        assert_eq!(writer.current_path(), path);
        assert!(writer.current_size() > 0);
        assert_eq!(writer.entries_in_current_file(), 0);
        drop(writer);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn finish_is_idempotent_and_returns_the_written_paths() {
        let path = temp_path("finish-idempotent");
        let options = WriterOptions::new(DataflowId::from_u128(2), HlcTimestamp::EPOCH);
        let mut writer = Writer::create(&path, options).unwrap();
        writer.append(entry(1)).unwrap();
        let first = writer.finish().unwrap();
        let second = writer.finish().unwrap();
        assert_eq!(first, vec![path.clone()]);
        assert_eq!(first, second);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn no_rotation_keeps_the_exact_given_path() {
        let path = temp_path("no-rotation");
        let options = WriterOptions::new(DataflowId::from_u128(3), HlcTimestamp::EPOCH);
        let mut writer = Writer::create(&path, options).unwrap();
        for seq in 0..5 {
            writer.append(entry(seq)).unwrap();
        }
        let files = writer.finish().unwrap();
        assert_eq!(files, vec![path.clone()]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn size_based_rotation_produces_numbered_segments() {
        let path = temp_path("rotate-size");
        let options = WriterOptions::new(DataflowId::from_u128(4), HlcTimestamp::EPOCH)
            .with_rotation(RotationPolicy::by_size(64));
        let mut writer = Writer::create(&path, options).unwrap();
        for seq in 0..20 {
            writer.append(entry(seq)).unwrap();
        }
        let files = writer.finish().unwrap();
        assert!(files.len() > 1, "expected more than one segment");
        for (i, file) in files.iter().enumerate() {
            let expected = segment_path(&path, i as u32 + 1);
            assert_eq!(*file, expected);
            assert!(file.exists());
            let _ = std::fs::remove_file(file);
        }
    }

    #[test]
    fn duration_based_rotation_triggers_on_the_next_append() {
        let path = temp_path("rotate-duration");
        // A wide margin both ways: the threshold (30ms) comfortably
        // exceeds one append's own overhead, and the sleep (300ms)
        // comfortably exceeds the threshold — so the trigger depends on
        // the sleep, never on scheduler jitter around either append.
        let options = WriterOptions::new(DataflowId::from_u128(5), HlcTimestamp::EPOCH)
            .with_rotation(RotationPolicy::by_duration(Duration::from_millis(30)));
        let mut writer = Writer::create(&path, options).unwrap();
        writer.append(entry(0)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        writer.append(entry(1)).unwrap();
        let files = writer.finish().unwrap();
        assert_eq!(files.len(), 2);
        for file in &files {
            let _ = std::fs::remove_file(file);
        }
    }

    #[test]
    fn dropping_an_unfinished_writer_still_finalizes_it() {
        let path = temp_path("drop-finalizes");
        let options = WriterOptions::new(DataflowId::from_u128(6), HlcTimestamp::EPOCH);
        {
            let mut writer = Writer::create(&path, options).unwrap();
            writer.append(entry(0)).unwrap();
            // No explicit `finish()`: Drop must still leave a valid footer
            // and trailer.
        }
        let bytes = std::fs::read(&path).unwrap();
        assert!(crate::format::read_trailer(&bytes[bytes.len() - 24..]).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn manifest_yaml_is_embedded_in_the_header() {
        let path = temp_path("manifest-embedded");
        let options = WriterOptions::new(DataflowId::from_u128(7), HlcTimestamp::EPOCH)
            .with_manifest_yaml("nodes:\n  - id: camera\n    path: ./camera\n");
        let mut writer = Writer::create(&path, options).unwrap();
        writer.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let (header, _) = crate::header::Header::read(&bytes).unwrap();
        assert!(header.manifest_yaml.contains("camera"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn segment_path_numbers_the_first_and_later_segments() {
        // A pure string transformation, never touching the filesystem —
        // still built from `temp_dir()` rather than a literal absolute
        // path, so nothing here assumes a directory this environment may
        // not have.
        let root = std::env::temp_dir();
        let base = root.join("session.arec");
        assert_eq!(segment_path(&base, 1), root.join("session-0001.arec"));
        assert_eq!(segment_path(&base, 2), root.join("session-0002.arec"));
        let no_ext = root.join("session");
        assert_eq!(segment_path(&no_ext, 1), root.join("session-0001"));
    }

    #[test]
    fn rotation_policy_thresholds_are_exclusive_at_the_boundary() {
        let policy = RotationPolicy::by_size(100);
        assert!(!policy.is_exceeded(100, Duration::ZERO));
        assert!(policy.is_exceeded(101, Duration::ZERO));
        assert!(!RotationPolicy::none().is_exceeded(u64::MAX, Duration::MAX));
    }
}
