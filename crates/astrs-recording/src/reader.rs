//! [`Reader`] — opening an `.arec` file and iterating its entries
//! (blueprint §14: "iterate all / by time range / by node+output").
//!
//! Every iteration form reads entries in **HLC order**, regardless of how
//! they happen to sit on disk (a recorder's own input stream interleaves
//! multiple producers in arrival order, not global HLC order) — the
//! order is computed once, from the footer index alone (no entry bodies
//! touched), and ties are broken by `(node, output, on-disk position)` so
//! two entries that land on the exact same timestamp still iterate in a
//! fixed, repeatable order.
//!
//! Reading stays bounded in memory the same way [`crate::Writer`] writing
//! does: the whole footer index is kept (proportional to entry *count*),
//! but only one entry's decompressed bytes are held at a time, fetched by
//! seeking to that entry's known offset rather than by loading the file.
//! Zero-copy access to a memory-mapped file is future work for 0.1.0 —
//! today's reader always copies a decompressed entry out to an owned
//! [`crate::Entry`], which is still far cheaper than the network or IPC
//! hop the recording replaces.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use astrs_time::HlcTimestamp;
use astrs_wire::{DataId, NodeId, WireDecode};

use crate::entry::Entry;
use crate::error::RecordingError;
use crate::format::{self, MINI_HEADER_LEN};
use crate::header::Header;
use crate::index::IndexEntry;
use crate::recover::{self, RecoveryReport};

/// An open `.arec` file, ready to iterate.
///
/// # Examples
///
/// ```
/// use astrs_recording::{Entry, Reader, Writer, WriterOptions};
/// use astrs_time::HlcTimestamp;
/// use astrs_wire::{DataId, DataflowId, Metadata, NodeId};
///
/// let path = std::env::temp_dir().join(format!("astrs-recording-reader-doctest-{}.arec", std::process::id()));
/// let mut writer = Writer::create(&path, WriterOptions::new(DataflowId::from_u128(1), HlcTimestamp::EPOCH))?;
/// writer.append(Entry::new(
///     NodeId::new("camera")?,
///     DataId::new("frames")?,
///     Metadata::new(HlcTimestamp::new(1, 0)),
///     vec![1, 2, 3],
/// ))?;
/// writer.finish()?;
///
/// let mut reader = Reader::open(&path)?;
/// assert_eq!(reader.len(), 1);
/// let entries: Vec<_> = reader.iter_all().collect::<Result<_, _>>()?;
/// assert_eq!(entries[0].payload, vec![1, 2, 3]);
/// # std::fs::remove_file(&path).ok();
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct Reader {
    file: File,
    file_len: u64,
    header: Header,
    /// The footer index, in on-disk (append) order.
    index: Vec<IndexEntry>,
}

impl Reader {
    /// Opens `path` using its trailer and footer.
    ///
    /// # Errors
    ///
    /// [`RecordingError::NoTrailer`] if the file has no valid trailer —
    /// use [`Reader::open_or_recover`] to fall back to a full scan in
    /// that case — plus whatever the header or footer frame itself
    /// report for a file whose trailer is fine but whose header or
    /// footer bytes are not.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RecordingError> {
        let path = path.as_ref();
        let mut file =
            File::open(path).map_err(|error| RecordingError::io("opening .arec file", error))?;
        let file_len = file
            .metadata()
            .map_err(|error| RecordingError::io("reading .arec file metadata", error))?
            .len();
        if file_len < format::TRAILER_LEN {
            return Err(RecordingError::TooShort { len: file_len });
        }

        file.seek(SeekFrom::Start(file_len - format::TRAILER_LEN))
            .map_err(|error| RecordingError::io("seeking to the trailer", error))?;
        let mut tail = vec![0u8; format::TRAILER_LEN as usize];
        file.read_exact(&mut tail)
            .map_err(|error| RecordingError::io("reading the trailer", error))?;
        let trailer = format::read_trailer(&tail)?;

        let header = read_header_only(&mut file, file_len)?;
        let footer_bytes = read_exact_at(
            &mut file,
            file_len,
            trailer.footer_offset,
            trailer.footer_len as u64,
        )?;
        let footer = crate::index::Footer::read(&footer_bytes, trailer.footer_offset)?;

        Ok(Self {
            file,
            file_len,
            header,
            index: footer.entries,
        })
    }

    /// Opens `path`, falling back to a full scan (see [`crate::recover`])
    /// when [`Reader::open`] cannot find or trust a trailer.
    ///
    /// Returns the [`RecoveryReport`] describing what happened either
    /// way — [`RecoveryReport::was_recovered`] is `false` for the normal,
    /// trailer-present case.
    ///
    /// # Errors
    ///
    /// [`RecordingError::BadMagic`], [`RecordingError::TooShort`] or
    /// similar if even the header cannot be read — a file with nothing to
    /// recover at all.
    pub fn open_or_recover(
        path: impl AsRef<Path>,
    ) -> Result<(Self, RecoveryReport), RecordingError> {
        let path = path.as_ref();
        match Self::open(path) {
            Ok(reader) => {
                let report = RecoveryReport::clean(reader.header.clone());
                Ok((reader, report))
            }
            Err(_) => {
                let report = recover::scan(path)?;
                let reader = Self::reopen_with(path, report.header.clone(), report.index.clone())?;
                Ok((reader, report))
            }
        }
    }

    /// Reopens `path` for random access with an already-known header and
    /// index — the shape both [`Reader::open`] and
    /// [`Reader::open_or_recover`] converge on.
    fn reopen_with(
        path: &Path,
        header: Header,
        index: Vec<IndexEntry>,
    ) -> Result<Self, RecordingError> {
        let file =
            File::open(path).map_err(|error| RecordingError::io("opening .arec file", error))?;
        let file_len = file
            .metadata()
            .map_err(|error| RecordingError::io("reading .arec file metadata", error))?
            .len();
        Ok(Self {
            file,
            file_len,
            header,
            index,
        })
    }

    /// The recording's header.
    #[must_use]
    pub const fn header(&self) -> &Header {
        &self.header
    }

    /// How many entries the footer (or, after recovery, the scan) found.
    #[must_use]
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Whether there are no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// The footer index, in on-disk (append) order.
    #[must_use]
    pub fn index(&self) -> &[IndexEntry] {
        &self.index
    }

    /// Reads one entry, given its index row.
    ///
    /// `entry` need not come from [`Reader::index`] on `self` specifically
    /// — [`crate::merge()`] calls this across readers — only its `offset`
    /// must be a valid entry-frame start in *this* reader's file.
    ///
    /// # Errors
    ///
    /// As [`format::read_frame`], plus [`RecordingError::UnknownFrameTag`]
    /// if the offset does not actually point at an entry frame.
    pub fn read_at(&mut self, entry: &IndexEntry) -> Result<Entry, RecordingError> {
        let frame = read_frame_seeking(&mut self.file, self.file_len, entry.offset)?;
        if frame.tag != format::ENTRY_FRAME_TAG {
            return Err(RecordingError::UnknownFrameTag {
                offset: entry.offset,
                found: frame.tag,
            });
        }
        Entry::decode_exact(&frame.body).map_err(|error| RecordingError::Malformed {
            offset: entry.offset,
            reason: error.to_string(),
        })
    }

    /// Iterates every entry, in HLC order.
    pub fn iter_all(&mut self) -> EntryIter<'_> {
        let order = hlc_order(&self.index, |_| true);
        EntryIter {
            reader: self,
            order,
            cursor: 0,
        }
    }

    /// Iterates entries whose HLC timestamp falls in `[from, to]`
    /// (inclusive both ends), in HLC order.
    pub fn iter_time_range(&mut self, from: HlcTimestamp, to: HlcTimestamp) -> EntryIter<'_> {
        let order = hlc_order(&self.index, |entry| entry.hlc >= from && entry.hlc <= to);
        EntryIter {
            reader: self,
            order,
            cursor: 0,
        }
    }

    /// Iterates entries recorded from `node`/`output`, in HLC order.
    pub fn iter_port(&mut self, node: &NodeId, output: &DataId) -> EntryIter<'_> {
        let order = hlc_order(&self.index, |entry| {
            &entry.node == node && &entry.output == output
        });
        EntryIter {
            reader: self,
            order,
            cursor: 0,
        }
    }
}

/// A lazy, HLC-ordered iterator over some subset of a [`Reader`]'s
/// entries.
///
/// Each [`Iterator::next`] seeks and decodes exactly one entry — nothing
/// beyond the precomputed order (index-sized, not payload-sized) is held
/// in memory across calls.
pub struct EntryIter<'a> {
    reader: &'a mut Reader,
    order: Vec<usize>,
    cursor: usize,
}

impl EntryIter<'_> {
    /// How many entries this iterator will yield in total.
    #[must_use]
    pub fn remaining_len(&self) -> usize {
        self.order.len() - self.cursor
    }
}

impl Iterator for EntryIter<'_> {
    type Item = Result<Entry, RecordingError>;

    fn next(&mut self) -> Option<Self::Item> {
        let position = *self.order.get(self.cursor)?;
        self.cursor += 1;
        let target = self.reader.index[position].clone();
        Some(self.reader.read_at(&target))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.remaining_len();
        (remaining, Some(remaining))
    }
}

/// The HLC-sorted, `predicate`-filtered positions into `index`.
///
/// Ties break on `(node, output, on-disk position)`, in that order, so
/// two entries sharing a timestamp always iterate in the same order run
/// to run.
fn hlc_order(index: &[IndexEntry], predicate: impl Fn(&IndexEntry) -> bool) -> Vec<usize> {
    let mut order: Vec<usize> = index
        .iter()
        .enumerate()
        .filter(|(_, entry)| predicate(entry))
        .map(|(position, _)| position)
        .collect();
    order.sort_by(|&a, &b| {
        index[a]
            .hlc
            .cmp(&index[b].hlc)
            .then_with(|| index[a].node.as_str().cmp(index[b].node.as_str()))
            .then_with(|| index[a].output.as_str().cmp(index[b].output.as_str()))
            .then_with(|| a.cmp(&b))
    });
    order
}

/// Reads only the fixed prologue and header frame from an open file,
/// using [`read_frame_seeking`] so the header's own size (dominated by
/// however large its embedded manifest YAML is) is discovered from its
/// own length prefix rather than guessed at with a fixed read window.
fn read_header_only(file: &mut File, file_len: u64) -> Result<Header, RecordingError> {
    if file_len < format::PROLOGUE_LEN {
        return Err(RecordingError::TooShort { len: file_len });
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| RecordingError::io("seeking to the header", error))?;
    let mut prologue = vec![0u8; format::PROLOGUE_LEN as usize];
    file.read_exact(&mut prologue)
        .map_err(|error| RecordingError::io("reading the header prologue", error))?;
    format::check_prologue(&prologue)?;

    let frame = read_frame_seeking(file, file_len, format::PROLOGUE_LEN)?;
    if frame.tag != format::HEADER_FRAME_TAG {
        return Err(RecordingError::UnknownFrameTag {
            offset: format::PROLOGUE_LEN,
            found: frame.tag,
        });
    }
    Header::decode_exact(&frame.body).map_err(|error| RecordingError::Malformed {
        offset: format::PROLOGUE_LEN,
        reason: error.to_string(),
    })
}

/// Reads exactly `len` bytes at `offset`, refusing to read past
/// `file_len`.
fn read_exact_at(
    file: &mut File,
    file_len: u64,
    offset: u64,
    len: u64,
) -> Result<Vec<u8>, RecordingError> {
    let end = offset
        .checked_add(len)
        .filter(|end| *end <= file_len)
        .ok_or(RecordingError::IndexOffsetOutOfRange {
            offset,
            len: file_len,
        })?;
    let _ = end;
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| RecordingError::io("seeking", error))?;
    let mut buffer = vec![0u8; len as usize];
    file.read_exact(&mut buffer)
        .map_err(|error| RecordingError::io("reading a frame", error))?;
    Ok(buffer)
}

/// Reads one frame by seeking to `offset` first, holding only that one
/// frame's bytes at a time.
///
/// # Errors
///
/// [`RecordingError::Incomplete`] if fewer than a frame's worth of bytes
/// remain from `offset` to the end of the file, computed from the file's
/// known length rather than from a failed read — so this reports the
/// same, precise "how many bytes were actually available" every other
/// [`RecordingError::Incomplete`] site does.
pub(crate) fn read_frame_seeking(
    file: &mut File,
    file_len: u64,
    offset: u64,
) -> Result<format::ParsedFrame, RecordingError> {
    let available = file_len.saturating_sub(offset);
    if available < MINI_HEADER_LEN as u64 {
        return Err(RecordingError::Incomplete {
            offset,
            needed: MINI_HEADER_LEN,
            available: available as usize,
        });
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| RecordingError::io("seeking to a frame", error))?;
    let mut mini = [0u8; MINI_HEADER_LEN];
    file.read_exact(&mut mini)
        .map_err(|error| RecordingError::io("reading a frame header", error))?;
    let body_len = u32::from_le_bytes([mini[5], mini[6], mini[7], mini[8]]) as u64;
    let total_len = MINI_HEADER_LEN as u64 + body_len + 4;
    if available < total_len {
        return Err(RecordingError::Incomplete {
            offset,
            needed: total_len as usize,
            available: available as usize,
        });
    }
    let mut rest = vec![0u8; (total_len - MINI_HEADER_LEN as u64) as usize];
    file.read_exact(&mut rest)
        .map_err(|error| RecordingError::io("reading a frame body", error))?;

    let mut combined = Vec::with_capacity(total_len as usize);
    combined.extend_from_slice(&mini);
    combined.extend_from_slice(&rest);
    format::read_frame(&combined, offset)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::writer::{Writer, WriterOptions};
    use astrs_wire::{DataflowId, Metadata};
    use std::path::PathBuf;

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "astrs-recording-reader-test-{}-{}-{label}.arec",
            std::process::id(),
            uniq()
        ))
    }

    fn write_sample(path: &Path, ports: &[(&str, &str, i64)]) {
        let options = WriterOptions::new(DataflowId::from_u128(9), HlcTimestamp::new(1, 0))
            .with_manifest_yaml("nodes: []\n");
        let mut writer = Writer::create(path, options).unwrap();
        for (index, (node, output, hlc_physical)) in ports.iter().enumerate() {
            let mut meta = Metadata::new(HlcTimestamp::new(*hlc_physical as u64, 0));
            meta.set_seq(index as i64);
            writer
                .append_parts(
                    NodeId::new(*node).unwrap(),
                    DataId::new(*output).unwrap(),
                    meta,
                    vec![index as u8; 8],
                )
                .unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn open_reads_back_every_entry_in_hlc_order() {
        let path = temp_path("open-basic");
        // Written out of HLC order on purpose.
        write_sample(
            &path,
            &[
                ("camera", "frames", 30),
                ("camera", "frames", 10),
                ("detector", "boxes", 20),
            ],
        );
        let mut reader = Reader::open(&path).unwrap();
        assert_eq!(reader.len(), 3);
        assert_eq!(reader.header().dataflow, DataflowId::from_u128(9));

        let entries: Vec<Entry> = reader.iter_all().collect::<Result<_, _>>().unwrap();
        let hlcs: Vec<u64> = entries.iter().map(|e| e.hlc().physical_ns()).collect();
        assert_eq!(hlcs, vec![10, 20, 30], "must be delivered in HLC order");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn iter_time_range_filters_inclusively() {
        let path = temp_path("time-range");
        write_sample(
            &path,
            &[
                ("a", "x", 10),
                ("a", "x", 20),
                ("a", "x", 30),
                ("a", "x", 40),
            ],
        );
        let mut reader = Reader::open(&path).unwrap();
        let entries: Vec<Entry> = reader
            .iter_time_range(HlcTimestamp::new(20, 0), HlcTimestamp::new(30, 0))
            .collect::<Result<_, _>>()
            .unwrap();
        let hlcs: Vec<u64> = entries.iter().map(|e| e.hlc().physical_ns()).collect();
        assert_eq!(hlcs, vec![20, 30]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn iter_port_filters_by_node_and_output() {
        let path = temp_path("by-port");
        write_sample(
            &path,
            &[
                ("camera", "frames", 10),
                ("detector", "boxes", 20),
                ("camera", "frames", 30),
            ],
        );
        let mut reader = Reader::open(&path).unwrap();
        let entries: Vec<Entry> = reader
            .iter_port(
                &NodeId::new("camera").unwrap(),
                &DataId::new("frames").unwrap(),
            )
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.node.as_str() == "camera"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tied_timestamps_iterate_in_a_fixed_deterministic_order() {
        let path = temp_path("tied");
        write_sample(&path, &[("b", "y", 5), ("a", "z", 5), ("a", "y", 5)]);
        let mut reader = Reader::open(&path).unwrap();
        let first: Vec<(String, String)> = reader
            .iter_all()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .into_iter()
            .map(|e| (e.node.into_string(), e.output.into_string()))
            .collect();
        // node order first ("a" before "b"), then output order within "a".
        assert_eq!(
            first,
            vec![
                ("a".to_string(), "y".to_string()),
                ("a".to_string(), "z".to_string()),
                ("b".to_string(), "y".to_string()),
            ]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn opening_a_file_with_no_trailer_fails_but_recovers() {
        let path = temp_path("no-trailer");
        let options = WriterOptions::new(DataflowId::from_u128(1), HlcTimestamp::EPOCH);
        let mut writer = Writer::create(&path, options).unwrap();
        writer
            .append_parts(
                NodeId::new("a").unwrap(),
                DataId::new("b").unwrap(),
                Metadata::new(HlcTimestamp::new(1, 0)),
                vec![1, 2, 3],
            )
            .unwrap();
        // No `finish()`, and `mem::forget` rather than a plain `drop`:
        // `Writer`'s own `Drop` impl finalizes on an early drop by
        // design (see its docs), so producing a genuinely footerless
        // file — the shape a killed *process* leaves, which runs no
        // destructors at all — means skipping that destructor here too.
        std::mem::forget(writer);

        assert!(matches!(
            Reader::open(&path),
            Err(RecordingError::NoTrailer)
        ));
        let (mut reader, report) = Reader::open_or_recover(&path).unwrap();
        assert!(report.was_recovered());
        assert_eq!(reader.len(), 1);
        assert_eq!(reader.iter_all().count(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_empty_recording_opens_and_iterates_nothing() {
        let path = temp_path("empty");
        let options = WriterOptions::new(DataflowId::from_u128(1), HlcTimestamp::EPOCH);
        let mut writer = Writer::create(&path, options).unwrap();
        writer.finish().unwrap();
        let mut reader = Reader::open(&path).unwrap();
        assert!(reader.is_empty());
        assert_eq!(reader.iter_all().count(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remaining_len_counts_down_as_the_iterator_advances() {
        let path = temp_path("remaining-len");
        write_sample(&path, &[("a", "x", 1), ("a", "x", 2), ("a", "x", 3)]);
        let mut reader = Reader::open(&path).unwrap();
        let mut iter = reader.iter_all();
        assert_eq!(iter.remaining_len(), 3);
        iter.next();
        assert_eq!(iter.remaining_len(), 2);
        assert_eq!(iter.size_hint(), (2, Some(2)));
        let _ = std::fs::remove_file(&path);
    }
}
