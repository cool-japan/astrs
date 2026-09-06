//! [`Reader`] — opens an `.mcap` file (blueprint §10.6) and iterates its
//! messages, reading both the chunked and unchunked layouts, with lz4/zstd
//! chunk decompression and a chunk-index-assisted fast path for metadata.
//!
//! # File structure this reader understands
//!
//! ```text
//! <Magic><Header><Data section>[<Summary section>][<Summary Offset section>]<Footer><Magic>
//! ```
//!
//! [`Reader::open`] always reads [`Header`] (the first record) and, since
//! the `Footer` record's shape is permanently fixed by spec (`"The
//! Message, DataEnd, and Footer records will not be extended"` — 8+8+4 =
//! 20 body bytes, so it always sits at exactly `file_len - 8 (trailing
//! magic) - 29 (its own framing)`), attempts a **fast path**: read the
//! footer directly by seeking from the end, and if `summary_start != 0`,
//! parse the summary section it points at for [`Reader::schemas`],
//! [`Reader::channels`], [`Reader::statistics`], and the chunk/attachment/
//! metadata indexes — all without touching a single `Chunk`'s
//! (potentially large, possibly compressed) payload.
//!
//! Messages themselves are never in the summary section (only `Statistics`
//! and index records are), so [`Reader::iter_messages`] always walks the
//! Data section directly — correctly handling both an indexed file and one
//! with no summary section at all (in which case the fast path simply
//! found nothing, [`Reader::is_indexed`] reports `false`, and every
//! `Schema`/`Channel` this reader knows about comes from the same forward
//! walk that also yields messages).
//!
//! # Bounded memory
//!
//! The Data section is walked by seeking one record at a time — never
//! read into memory wholesale. A `Chunk` record's own framing is bounded
//! by the file's real length (the same discipline
//! `astrs_recording::format::read_frame` uses); its *declared*
//! `uncompressed_size` is separately checked against
//! [`MAX_CHUNK_UNCOMPRESSED_BYTES`] before decompression is ever attempted
//! (a 40-byte `Chunk` record cannot make this reader allocate a
//! multi-gigabyte buffer), and a chunk's decompressed `records` bytes
//! containing another `Chunk` opcode is rejected
//! ([`RosbagError::NestedChunk`]) rather than recursed into.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::RosbagError;
use crate::mcap::primitives::Cursor;
use crate::mcap::records::{
    Attachment, AttachmentIndex, Channel, Chunk, ChunkIndex, Footer, Header, Message, Metadata,
    MetadataIndex, Opcode, Schema, Statistics, SummaryOffset,
};

/// The literal magic bytes an `.mcap` file must open, and close, with.
pub const MAGIC: [u8; 8] = [0x89, b'M', b'C', b'A', b'P', 0x30, b'\r', b'\n'];

/// The largest `Chunk::uncompressed_size` this crate will decompress,
/// refused *before* decompression is attempted (see the module docs'
/// bounded-memory note and [`RosbagError::ChunkTooLarge`]). 1 GiB: ample
/// for any single chunk a real writer produces (mcap chunks are typically
/// a few MiB), while still bounding the worst case a tiny forged `Chunk`
/// record could otherwise claim.
pub const MAX_CHUNK_UNCOMPRESSED_BYTES: u64 = 1 << 30;

/// A record's fixed mini-header length: `opcode(1) + len_body(uint64, 8)`.
const RECORD_HEADER_LEN: u64 = 9;

/// `Footer`'s own total on-disk length: its 9-byte framing plus its
/// permanently-fixed 20-byte body (`summary_start(8) +
/// summary_offset_start(8) + summary_crc(4)`) — see the module docs.
const FOOTER_RECORD_LEN: u64 = RECORD_HEADER_LEN + 20;

/// One fully resolved message: its channel and (if the channel names one)
/// schema, alongside the message itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMessage {
    /// The channel this message was published on.
    pub channel: Channel,
    /// The channel's schema, or `None` if `channel.schema_id == 0`.
    pub schema: Option<Schema>,
    /// The message itself.
    pub message: Message,
}

/// An open `.mcap` file.
///
/// # Examples
///
/// The smallest valid mcap file the specification names (`[Header]
/// [Footer]`, no messages), assembled by hand: this repository carries no
/// C/C++-derived fixtures (astrs.md §18), so every mcap byte this crate's
/// own tests exercise — this doctest included — is built directly against
/// the specification rather than captured from an external tool. See
/// `mcap::reader::tests` for round trips that also exercise `Schema`,
/// `Channel`, `Message`, and chunked/compressed layouts.
///
/// ```
/// use astrs_rosbag::mcap::Reader;
///
/// fn record(opcode: u8, body: &[u8]) -> Vec<u8> {
///     let mut out = vec![opcode];
///     out.extend((body.len() as u64).to_le_bytes());
///     out.extend_from_slice(body);
///     out
/// }
///
/// const MAGIC: [u8; 8] = [0x89, b'M', b'C', b'A', b'P', 0x30, b'\r', b'\n'];
/// let header_body = [0u32.to_le_bytes(), 0u32.to_le_bytes()].concat(); // profile="", library=""
/// let mut footer_body = 0u64.to_le_bytes().to_vec(); // summary_start
/// footer_body.extend(0u64.to_le_bytes()); // summary_offset_start
/// footer_body.extend(0u32.to_le_bytes()); // summary_crc
///
/// let mut bytes = MAGIC.to_vec();
/// bytes.extend(record(0x01, &header_body)); // Header
/// bytes.extend(record(0x02, &footer_body)); // Footer, no summary section
/// bytes.extend(MAGIC);
///
/// let dir = std::env::temp_dir().join(format!("astrs-rosbag-mcap-doctest-{}", std::process::id()));
/// std::fs::create_dir_all(&dir)?;
/// let path = dir.join("minimal.mcap");
/// std::fs::write(&path, &bytes)?;
///
/// let mut reader = Reader::open(&path)?;
/// assert_eq!(reader.header().profile, "");
/// assert!(!reader.is_indexed(), "a footer with summary_start=0 has no summary section");
/// assert_eq!(reader.iter_messages().count(), 0);
/// # std::fs::remove_dir_all(&dir).ok();
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct Reader {
    file: File,
    file_len: u64,
    path: PathBuf,
    header: Header,
    /// Where the Data section begins: right after the `Header` record
    /// (`8` for the leading magic, plus however long the `Header` record
    /// itself was).
    data_start: u64,
    /// Where the Data section ends: `footer.summary_start` if the fast
    /// path found one, else `file_len - 8` (right before the trailing
    /// magic).
    data_section_end: u64,
    schemas: BTreeMap<u16, Schema>,
    channels: BTreeMap<u16, Channel>,
    statistics: Option<Statistics>,
    attachment_indexes: Vec<AttachmentIndex>,
    metadata_indexes: Vec<MetadataIndex>,
    chunk_indexes: Vec<ChunkIndex>,
    indexed: bool,
    warnings: Vec<String>,
}

impl Reader {
    /// Opens `path`: validates the leading/trailing magic, reads the
    /// [`Header`] record, and attempts the summary-section fast path for
    /// [`Reader::schemas`]/[`Reader::channels`]/[`Reader::statistics`]
    /// (see the module docs). A missing, absent, or malformed summary
    /// section is never fatal — [`Reader::is_indexed`] reports `false`
    /// and [`Reader::iter_messages`] still works, walking the Data
    /// section directly.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Io`] if `path` cannot be opened,
    /// [`RosbagError::BadMagic`] if the leading or trailing magic does
    /// not match, or [`RosbagError::Truncated`]/[`RosbagError::Malformed`]
    /// if even the [`Header`] record cannot be read.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RosbagError> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path).map_err(|source| RosbagError::io(&path, source))?;
        let file_len = file
            .metadata()
            .map_err(|source| RosbagError::io(&path, source))?
            .len();

        if file_len < (MAGIC.len() as u64) * 2 {
            return Err(RosbagError::BadMagic {
                path,
                reason: "file is shorter than two magic sequences",
            });
        }

        let mut leading = [0u8; 8];
        file.read_exact(&mut leading)
            .map_err(|source| RosbagError::io(&path, source))?;
        if leading != MAGIC {
            return Err(RosbagError::BadMagic {
                path,
                reason: "missing the leading mcap magic",
            });
        }
        let mut trailing = [0u8; 8];
        file.seek(SeekFrom::Start(file_len - MAGIC.len() as u64))
            .map_err(|source| RosbagError::io(&path, source))?;
        file.read_exact(&mut trailing)
            .map_err(|source| RosbagError::io(&path, source))?;
        if trailing != MAGIC {
            return Err(RosbagError::BadMagic {
                path,
                reason: "missing the trailing mcap magic",
            });
        }

        let (header, header_consumed) = read_record_at(&mut file, file_len, &path, 8)?;
        let Some(Opcode::Header) = Opcode::from_u8(header.0) else {
            return Err(RosbagError::Malformed {
                path,
                offset: 8,
                reason: "the first record after the magic must be a Header".to_owned(),
            });
        };
        let header = Header::read(&mut Cursor::new(&header.1, 8, &path))?;
        let data_start = 8 + header_consumed;

        let mut reader = Self {
            file,
            file_len,
            path: path.clone(),
            header,
            data_start,
            data_section_end: file_len - MAGIC.len() as u64,
            schemas: BTreeMap::new(),
            channels: BTreeMap::new(),
            statistics: None,
            attachment_indexes: Vec::new(),
            metadata_indexes: Vec::new(),
            chunk_indexes: Vec::new(),
            indexed: false,
            warnings: Vec::new(),
        };

        match reader.try_fast_path() {
            Ok(Some(summary_start)) => {
                reader.data_section_end = summary_start;
                reader.indexed = true;
            }
            Ok(None) => {
                // No summary section (`summary_start == 0`) — a
                // structurally normal, unindexed file.
            }
            Err(err) => {
                reader.warnings.push(format!(
                    "the summary-section fast path was not usable, so `channels`/`schemas`/ \
                     `statistics` are empty until `iter_messages` walks the data section: {err}"
                ));
            }
        }

        Ok(reader)
    }

    /// The `.mcap` path this reader opened.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file's `Header` record.
    #[must_use]
    pub const fn header(&self) -> &Header {
        &self.header
    }

    /// Whether [`Reader::open`]'s summary-section fast path found and
    /// used a usable footer/summary section. `false` means
    /// [`Reader::schemas`]/[`Reader::channels`]/[`Reader::statistics`]
    /// reflect only what has been seen so far by
    /// [`Reader::iter_messages`] — call it to completion first for a
    /// complete picture of an unindexed file.
    #[must_use]
    pub const fn is_indexed(&self) -> bool {
        self.indexed
    }

    /// Non-fatal notes accumulated while opening or iterating — a summary
    /// section that could not be used, an unrecognized chunk compression
    /// skipped, and similar best-effort degradations.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Every schema known so far — complete after [`Reader::open`] if
    /// [`Reader::is_indexed`], otherwise complete only once
    /// [`Reader::iter_messages`] has been fully drained.
    #[must_use]
    pub fn schemas(&self) -> &BTreeMap<u16, Schema> {
        &self.schemas
    }

    /// Every channel known so far — see [`Reader::schemas`]'s
    /// completeness note.
    #[must_use]
    pub fn channels(&self) -> &BTreeMap<u16, Channel> {
        &self.channels
    }

    /// The file's `Statistics` record, if the summary-section fast path
    /// found one.
    #[must_use]
    pub const fn statistics(&self) -> Option<&Statistics> {
        self.statistics.as_ref()
    }

    /// Every `AttachmentIndex` the summary section carried (empty unless
    /// [`Reader::is_indexed`]).
    #[must_use]
    pub fn attachment_indexes(&self) -> &[AttachmentIndex] {
        &self.attachment_indexes
    }

    /// Every `MetadataIndex` the summary section carried (empty unless
    /// [`Reader::is_indexed`]).
    #[must_use]
    pub fn metadata_indexes(&self) -> &[MetadataIndex] {
        &self.metadata_indexes
    }

    /// Every `ChunkIndex` the summary section carried (empty unless
    /// [`Reader::is_indexed`]) — one per `Chunk` record, in summary-section
    /// order.
    ///
    /// [`Reader::iter_messages_in_time_range`] does not consult this list
    /// directly (it trusts each `Chunk`'s own `message_start_time`/
    /// `message_end_time` instead, which is sound for both indexed and
    /// unindexed files uniformly — see that method's own docs); this
    /// accessor exists for callers that want the summary's own per-chunk
    /// statistics (compressed/uncompressed size, on-disk span) directly,
    /// e.g. a future `astrs bag info --verbose` chunk breakdown.
    #[must_use]
    pub fn chunk_indexes(&self) -> &[ChunkIndex] {
        &self.chunk_indexes
    }

    /// Reads one `Attachment` record given its `AttachmentIndex`.
    ///
    /// # Errors
    ///
    /// As [`Reader::open`]'s record-reading errors.
    pub fn read_attachment(&mut self, index: &AttachmentIndex) -> Result<Attachment, RosbagError> {
        let (record, _) = read_record_at(&mut self.file, self.file_len, &self.path, index.offset)?;
        let Some(Opcode::Attachment) = Opcode::from_u8(record.0) else {
            return Err(RosbagError::Malformed {
                path: self.path.clone(),
                offset: index.offset,
                reason: "AttachmentIndex does not point at an Attachment record".to_owned(),
            });
        };
        Attachment::read(&mut Cursor::new(&record.1, index.offset, &self.path))
    }

    /// Reads one `Metadata` record given its `MetadataIndex`.
    ///
    /// # Errors
    ///
    /// As [`Reader::open`]'s record-reading errors.
    pub fn read_metadata(&mut self, index: &MetadataIndex) -> Result<Metadata, RosbagError> {
        let (record, _) = read_record_at(&mut self.file, self.file_len, &self.path, index.offset)?;
        let Some(Opcode::Metadata) = Opcode::from_u8(record.0) else {
            return Err(RosbagError::Malformed {
                path: self.path.clone(),
                offset: index.offset,
                reason: "MetadataIndex does not point at a Metadata record".to_owned(),
            });
        };
        Metadata::read(&mut Cursor::new(&record.1, index.offset, &self.path))
    }

    /// Iterates every message in the Data section, in on-disk order
    /// (chunked or not) — never sorted by `log_time`, since the spec does
    /// not require that ordering and this crate does not assume it (an
    /// `.arec` conversion's own [`astrs_recording::Reader`] sorts by HLC
    /// on replay regardless of write order, so nothing downstream needs
    /// this reader to pre-sort).
    #[must_use]
    pub fn iter_messages(&mut self) -> MessageIter<'_> {
        MessageIter {
            pos: self.data_start,
            reader: self,
            pending: Vec::new().into_iter(),
            done: false,
            time_range: None,
        }
    }

    /// As [`Reader::iter_messages`], but skips whole `Chunk` records whose
    /// own `message_start_time`/`message_end_time` cannot overlap
    /// `[start, end]` (inclusive both ends) — without decompressing them at
    /// all — and filters every yielded message (chunked or top-level) by
    /// `log_time` falling in that same range.
    ///
    /// This trusts each chunk's self-reported time span — the same trust
    /// every mcap reader places in it, since that is what the field exists
    /// for — rather than consulting [`Reader::chunk_indexes`]: a `Chunk`
    /// record's own `message_start_time`/`message_end_time` are present on
    /// *every* chunk, indexed file or not, so this one code path is sound
    /// whether or not [`Reader::is_indexed`] is `true`, and it is never
    /// fooled by a summary section that is present but incomplete (a
    /// `ChunkIndex` list missing an entry for some chunk would otherwise be
    /// a silent-message-loss trap for a naive summary-driven skip). A
    /// chunk whose declared span is wrong (an adversarial or buggy writer)
    /// can still cause a real in-range message to be missed here — the
    /// same caveat applies to every index-assisted mcap reader — so a
    /// caller that cannot trust its input should use [`Reader::iter_messages`]
    /// with its own filter instead.
    ///
    /// **A skipped chunk's `Schema`/`Channel` records are skipped too.**
    /// A real "ros2"-profile writer always mirrors every `Schema`/`Channel`
    /// into the summary section (so an indexed file is unaffected), but an
    /// *unindexed* chunked file that defines a channel inside one chunk
    /// and then reuses that same channel id from a later, in-range chunk
    /// will surface [`RosbagError::Malformed`] here — "no preceding
    /// `Channel` record" — even though [`Reader::iter_messages`] on the
    /// same file would resolve it fine. Loud and typed, never silent
    /// message loss, but worth knowing before reaching for this method on
    /// an unindexed file of unknown provenance.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_rosbag::mcap::Reader;
    ///
    /// fn record(opcode: u8, body: &[u8]) -> Vec<u8> {
    ///     let mut out = vec![opcode];
    ///     out.extend((body.len() as u64).to_le_bytes());
    ///     out.extend_from_slice(body);
    ///     out
    /// }
    /// fn prefixed(s: &str) -> Vec<u8> {
    ///     let mut out = (s.len() as u32).to_le_bytes().to_vec();
    ///     out.extend_from_slice(s.as_bytes());
    ///     out
    /// }
    ///
    /// const MAGIC: [u8; 8] = [0x89, b'M', b'C', b'A', b'P', 0x30, b'\r', b'\n'];
    /// let mut bytes = MAGIC.to_vec();
    /// bytes.extend(record(0x01, &[0u32.to_le_bytes(), 0u32.to_le_bytes()].concat())); // Header
    /// let mut channel_body = 1u16.to_le_bytes().to_vec();
    /// channel_body.extend(0u16.to_le_bytes());
    /// channel_body.extend(prefixed("/scan"));
    /// channel_body.extend(prefixed("cdr"));
    /// channel_body.extend(0u32.to_le_bytes());
    /// bytes.extend(record(0x04, &channel_body)); // Channel
    /// for (log_time, payload) in [(10u64, [1u8]), (50u64, [2u8]), (90u64, [3u8])] {
    ///     let mut body = 1u16.to_le_bytes().to_vec();
    ///     body.extend(0u32.to_le_bytes());
    ///     body.extend(log_time.to_le_bytes());
    ///     body.extend(log_time.to_le_bytes());
    ///     body.extend_from_slice(&payload);
    ///     bytes.extend(record(0x05, &body)); // Message
    /// }
    /// let mut footer_body = 0u64.to_le_bytes().to_vec(); // summary_start
    /// footer_body.extend(0u64.to_le_bytes()); // summary_offset_start
    /// footer_body.extend(0u32.to_le_bytes()); // summary_crc
    /// bytes.extend(record(0x02, &footer_body)); // Footer
    /// bytes.extend(MAGIC);
    ///
    /// let dir = std::env::temp_dir().join(format!("astrs-rosbag-mcap-time-range-doctest-{}", std::process::id()));
    /// std::fs::create_dir_all(&dir)?;
    /// let path = dir.join("session.mcap");
    /// std::fs::write(&path, &bytes)?;
    ///
    /// let mut reader = Reader::open(&path)?;
    /// let messages: Vec<_> = reader.iter_messages_in_time_range(40, 60).collect::<Result<_, _>>()?;
    /// assert_eq!(messages.len(), 1);
    /// assert_eq!(messages[0].message.log_time, 50);
    /// # std::fs::remove_dir_all(&dir).ok();
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn iter_messages_in_time_range(&mut self, start: u64, end: u64) -> MessageIter<'_> {
        MessageIter {
            pos: self.data_start,
            reader: self,
            pending: Vec::new().into_iter(),
            done: false,
            time_range: Some((start, end)),
        }
    }

    /// The fast path: read the fixed-size `Footer` record from the end of
    /// the file, and if it names a summary section, parse every record in
    /// it. Returns `Ok(Some(summary_start))` on success,
    /// `Ok(None)` if the footer is well-formed but names no summary
    /// section, and `Err` for anything that made the footer itself
    /// unusable (never for a problem *within* an otherwise-valid summary
    /// section — see [`Self::parse_summary_section`]).
    fn try_fast_path(&mut self) -> Result<Option<u64>, RosbagError> {
        if self.file_len < MAGIC.len() as u64 + FOOTER_RECORD_LEN {
            return Err(RosbagError::Truncated {
                path: self.path.clone(),
                offset: 0,
                needed: (MAGIC.len() as u64 + FOOTER_RECORD_LEN) as usize,
                available: self.file_len as usize,
            });
        }
        let footer_offset = self.file_len - MAGIC.len() as u64 - FOOTER_RECORD_LEN;
        let (record, _) = read_record_at(&mut self.file, self.file_len, &self.path, footer_offset)?;
        if Opcode::from_u8(record.0) != Some(Opcode::Footer) {
            return Err(RosbagError::Malformed {
                path: self.path.clone(),
                offset: footer_offset,
                reason: "no Footer record at the expected fixed offset".to_owned(),
            });
        }
        let footer = Footer::read(&mut Cursor::new(&record.1, footer_offset, &self.path))?;
        if footer.summary_start == 0 {
            return Ok(None);
        }
        self.parse_summary_section(footer.summary_start, footer_offset)?;
        Ok(Some(footer.summary_start))
    }

    /// Parses every record from `start` to `end` (the footer's own
    /// offset) as summary-section records, populating this reader's
    /// tables.
    fn parse_summary_section(&mut self, start: u64, end: u64) -> Result<(), RosbagError> {
        let mut pos = start;
        while pos < end {
            let (record, consumed) =
                read_record_at(&mut self.file, self.file_len, &self.path, pos)?;
            let offset = pos;
            pos += consumed;
            match Opcode::from_u8(record.0) {
                Some(Opcode::Schema) => {
                    let schema = Schema::read(&mut Cursor::new(&record.1, offset, &self.path))?;
                    if schema.id != 0 {
                        self.schemas.insert(schema.id, schema);
                    }
                }
                Some(Opcode::Channel) => {
                    let channel = Channel::read(&mut Cursor::new(&record.1, offset, &self.path))?;
                    self.channels.insert(channel.id, channel);
                }
                Some(Opcode::Statistics) => {
                    self.statistics = Some(Statistics::read(&mut Cursor::new(
                        &record.1, offset, &self.path,
                    ))?);
                }
                Some(Opcode::ChunkIndex) => {
                    let chunk_index =
                        ChunkIndex::read(&mut Cursor::new(&record.1, offset, &self.path))?;
                    self.chunk_indexes.push(chunk_index);
                }
                Some(Opcode::AttachmentIndex) => {
                    self.attachment_indexes
                        .push(AttachmentIndex::read(&mut Cursor::new(
                            &record.1, offset, &self.path,
                        ))?);
                }
                Some(Opcode::MetadataIndex) => {
                    self.metadata_indexes
                        .push(MetadataIndex::read(&mut Cursor::new(
                            &record.1, offset, &self.path,
                        ))?);
                }
                Some(Opcode::SummaryOffset) => {
                    let _ = SummaryOffset::read(&mut Cursor::new(&record.1, offset, &self.path))?;
                }
                _ => {
                    // Forward compatibility (see `Opcode::from_u8`'s
                    // docs) and records the spec never places in the
                    // summary section (`Message`, `Chunk`, …) — both
                    // skipped rather than rejected.
                }
            }
        }
        Ok(())
    }
}

/// Reads one record's framing (`opcode(1) + len_body(uint64,8)`) and body
/// from `file` at `offset`, checked against `file_len` before any
/// length-sized allocation.
///
/// Returns `((opcode_byte, body), consumed_bytes)`.
fn read_record_at(
    file: &mut File,
    file_len: u64,
    path: &Path,
    offset: u64,
) -> Result<((u8, Vec<u8>), u64), RosbagError> {
    let available = file_len.saturating_sub(offset);
    if available < RECORD_HEADER_LEN {
        return Err(RosbagError::Truncated {
            path: path.to_path_buf(),
            offset,
            needed: RECORD_HEADER_LEN as usize,
            available: available as usize,
        });
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|source| RosbagError::io(path, source))?;
    let mut mini = [0u8; RECORD_HEADER_LEN as usize];
    file.read_exact(&mut mini)
        .map_err(|source| RosbagError::io(path, source))?;
    let opcode = mini[0];
    let len_body = u64::from_le_bytes([
        mini[1], mini[2], mini[3], mini[4], mini[5], mini[6], mini[7], mini[8],
    ]);
    let total = RECORD_HEADER_LEN
        .checked_add(len_body)
        .filter(|total| *total <= available)
        .ok_or(RosbagError::Truncated {
            path: path.to_path_buf(),
            offset,
            needed: usize::try_from(len_body).unwrap_or(usize::MAX),
            available: available as usize,
        })?;
    let body_len = usize::try_from(len_body).map_err(|_| RosbagError::Truncated {
        path: path.to_path_buf(),
        offset,
        needed: usize::MAX,
        available: available as usize,
    })?;
    let mut body = vec![0u8; body_len];
    file.read_exact(&mut body)
        .map_err(|source| RosbagError::io(path, source))?;
    Ok(((opcode, body), total))
}

/// `(opcode, body, next_pos)` — one record parsed from an in-memory slice.
type InMemoryRecord<'a> = (u8, &'a [u8], usize);

/// Parses one record's framing from an in-memory slice (a decompressed
/// chunk's `records` bytes) rather than a file — same bounds discipline as
/// [`read_record_at`], but returning a borrowed body slice since no I/O is
/// involved.
fn next_record_in_memory<'a>(
    bytes: &'a [u8],
    pos: usize,
    base_offset: u64,
    path: &Path,
) -> Result<Option<InMemoryRecord<'a>>, RosbagError> {
    if pos == bytes.len() {
        return Ok(None);
    }
    let available = bytes.len() - pos;
    if available < RECORD_HEADER_LEN as usize {
        return Err(RosbagError::Truncated {
            path: path.to_path_buf(),
            offset: base_offset + pos as u64,
            needed: RECORD_HEADER_LEN as usize,
            available,
        });
    }
    let opcode = bytes[pos];
    let len_body = u64::from_le_bytes([
        bytes[pos + 1],
        bytes[pos + 2],
        bytes[pos + 3],
        bytes[pos + 4],
        bytes[pos + 5],
        bytes[pos + 6],
        bytes[pos + 7],
        bytes[pos + 8],
    ]);
    let body_len = usize::try_from(len_body).map_err(|_| RosbagError::Truncated {
        path: path.to_path_buf(),
        offset: base_offset + pos as u64,
        needed: usize::MAX,
        available,
    })?;
    let total = (RECORD_HEADER_LEN as usize)
        .checked_add(body_len)
        .filter(|total| *total <= available)
        .ok_or(RosbagError::Truncated {
            path: path.to_path_buf(),
            offset: base_offset + pos as u64,
            needed: body_len,
            available,
        })?;
    let body = &bytes[pos + RECORD_HEADER_LEN as usize..pos + total];
    Ok(Some((opcode, body, pos + total)))
}

/// Decompresses a [`Chunk`]'s `records` bytes, guarded by
/// [`MAX_CHUNK_UNCOMPRESSED_BYTES`] and a declared/actual size match.
///
/// # Errors
///
/// [`RosbagError::ChunkTooLarge`] if `chunk.uncompressed_size` exceeds the
/// ceiling (checked *before* any decompression is attempted),
/// [`RosbagError::UnknownCompression`] for anything other than `""`,
/// `"lz4"`, or `"zstd"`, [`RosbagError::Codec`] if the codec itself
/// rejects the bytes, or [`RosbagError::ChunkSizeMismatch`] if
/// decompression succeeds but produces a different length than declared.
fn decompress_chunk(chunk: &Chunk, offset: u64, path: &Path) -> Result<Vec<u8>, RosbagError> {
    if chunk.uncompressed_size > MAX_CHUNK_UNCOMPRESSED_BYTES {
        return Err(RosbagError::ChunkTooLarge {
            path: path.to_path_buf(),
            offset,
            declared: chunk.uncompressed_size,
            limit: MAX_CHUNK_UNCOMPRESSED_BYTES,
        });
    }
    let declared = usize::try_from(chunk.uncompressed_size).unwrap_or(usize::MAX);
    let decompressed = match chunk.compression.as_str() {
        "" => chunk.records.clone(),
        "lz4" => oxiarc_lz4::decompress(&chunk.records, declared).map_err(|source| {
            RosbagError::Codec {
                path: path.to_path_buf(),
                offset,
                reason: source.to_string(),
            }
        })?,
        "zstd" => oxiarc_zstd::decompress(&chunk.records).map_err(|source| RosbagError::Codec {
            path: path.to_path_buf(),
            offset,
            reason: source.to_string(),
        })?,
        other => {
            return Err(RosbagError::UnknownCompression {
                path: path.to_path_buf(),
                offset,
                compression: other.to_owned(),
            });
        }
    };
    if decompressed.len() != declared {
        return Err(RosbagError::ChunkSizeMismatch {
            path: path.to_path_buf(),
            offset,
            declared: chunk.uncompressed_size,
            actual: decompressed.len(),
        });
    }
    Ok(decompressed)
}

/// A lazy iterator over a [`Reader`]'s Data section, yielding every
/// [`ResolvedMessage`] (see [`Reader::iter_messages`] and
/// [`Reader::iter_messages_in_time_range`]).
///
/// Holds at most one chunk's decompressed bytes at a time (see the module
/// docs' bounded-memory note): its own `pending` field is that chunk's
/// records, already parsed into owned [`Message`]s, drained one at a
/// time before the next top-level record is read.
pub struct MessageIter<'a> {
    reader: &'a mut Reader,
    pos: u64,
    pending: std::vec::IntoIter<(Channel, Option<Schema>, Message)>,
    done: bool,
    /// `Some((start, end))` for [`Reader::iter_messages_in_time_range`];
    /// `None` for the unfiltered [`Reader::iter_messages`] — see that
    /// method's own docs for exactly what this changes.
    time_range: Option<(u64, u64)>,
}

impl Iterator for MessageIter<'_> {
    type Item = Result<ResolvedMessage, RosbagError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some((channel, schema, message)) = self.pending.next() {
                return Some(Ok(ResolvedMessage {
                    channel,
                    schema,
                    message,
                }));
            }
            if self.done || self.pos >= self.reader.data_section_end {
                self.done = true;
                return None;
            }
            match self.advance() {
                Ok(()) => {}
                Err(err) => {
                    self.done = true;
                    return Some(Err(err));
                }
            }
        }
    }
}

impl MessageIter<'_> {
    /// Reads and dispatches exactly one top-level Data-section record,
    /// updating `self.reader`'s schema/channel tables, filling
    /// `self.pending` with any messages it directly or indirectly (via a
    /// `Chunk`) contained, and advancing `self.pos`.
    fn advance(&mut self) -> Result<(), RosbagError> {
        let path = self.reader.path.clone();
        let ((opcode, body), consumed) =
            read_record_at(&mut self.reader.file, self.reader.file_len, &path, self.pos)?;
        let offset = self.pos;
        self.pos += consumed;

        match Opcode::from_u8(opcode) {
            Some(Opcode::Schema) => {
                let schema = Schema::read(&mut Cursor::new(&body, offset, &path))?;
                if schema.id != 0 {
                    self.reader.schemas.insert(schema.id, schema);
                }
            }
            Some(Opcode::Channel) => {
                let channel = Channel::read(&mut Cursor::new(&body, offset, &path))?;
                self.reader.channels.insert(channel.id, channel);
            }
            Some(Opcode::Message) => {
                let message = Message::read(&mut Cursor::new(&body, offset, &path))?;
                if self.in_range(message.log_time) {
                    let resolved = self.resolve(message, offset)?;
                    self.pending = vec![resolved].into_iter();
                }
            }
            Some(Opcode::Chunk) => {
                let chunk = Chunk::read(&mut Cursor::new(&body, offset, &path))?;
                // Outside `self.time_range` (never true when it is `None`):
                // the chunk's own declared span proves it holds nothing
                // this iterator would yield, so it is never even
                // decompressed — see `Reader::iter_messages_in_time_range`'s
                // docs.
                if self.chunk_may_overlap(&chunk) {
                    let decompressed = decompress_chunk(&chunk, offset, &path)?;
                    let resolved = self.drain_chunk(&decompressed, offset, &path)?;
                    self.pending = resolved.into_iter();
                }
            }
            // `MessageIndex` sequences after a chunk, `Attachment`s,
            // `Metadata`, and `DataEnd` all carry no `Message` content;
            // an unrecognized opcode is forward-compatibility (see
            // `Opcode::from_u8`'s docs). All skipped uniformly.
            _ => {}
        }
        Ok(())
    }

    /// Whether `log_time` is inside this iterator's `time_range` —
    /// unconditionally `true` when it is `None` (the unfiltered
    /// [`Reader::iter_messages`] case).
    fn in_range(&self, log_time: u64) -> bool {
        match self.time_range {
            None => true,
            Some((start, end)) => log_time >= start && log_time <= end,
        }
    }

    /// Whether `chunk` might hold a message this iterator would yield —
    /// unconditionally `true` when `self.time_range` is `None`, otherwise
    /// whether `chunk`'s own declared `[message_start_time,
    /// message_end_time]` overlaps `self.time_range` (both ends inclusive,
    /// matching [`Self::in_range`]'s own boundary treatment).
    fn chunk_may_overlap(&self, chunk: &Chunk) -> bool {
        match self.time_range {
            None => true,
            Some((start, end)) => {
                chunk.message_start_time <= end && chunk.message_end_time >= start
            }
        }
    }

    /// Parses a decompressed chunk's `records` bytes into resolved
    /// messages, updating `self.reader`'s schema/channel tables from any
    /// `Schema`/`Channel` records the chunk itself carries (the spec
    /// explicitly allows both inside a chunk).
    ///
    /// # Errors
    ///
    /// [`RosbagError::NestedChunk`] if a chunk's own decompressed bytes
    /// contain another `Chunk` record.
    fn drain_chunk(
        &mut self,
        decompressed: &[u8],
        chunk_offset: u64,
        path: &Path,
    ) -> Result<Vec<(Channel, Option<Schema>, Message)>, RosbagError> {
        let mut resolved = Vec::new();
        let mut pos = 0usize;
        while let Some((opcode, body, next_pos)) =
            next_record_in_memory(decompressed, pos, chunk_offset, path)?
        {
            let offset = chunk_offset + pos as u64;
            pos = next_pos;
            match Opcode::from_u8(opcode) {
                Some(Opcode::Schema) => {
                    let schema = Schema::read(&mut Cursor::new(body, offset, path))?;
                    if schema.id != 0 {
                        self.reader.schemas.insert(schema.id, schema);
                    }
                }
                Some(Opcode::Channel) => {
                    let channel = Channel::read(&mut Cursor::new(body, offset, path))?;
                    self.reader.channels.insert(channel.id, channel);
                }
                Some(Opcode::Message) => {
                    let message = Message::read(&mut Cursor::new(body, offset, path))?;
                    // The chunk as a whole was already proven to overlap
                    // `self.time_range` (see `chunk_may_overlap` — this
                    // point is only reached when it did, or when there is
                    // no filter at all), but an overlapping *chunk*
                    // commonly still holds individual messages outside the
                    // exact queried window, so each one is filtered here
                    // too.
                    if self.in_range(message.log_time) {
                        resolved.push(self.resolve(message, offset)?);
                    }
                }
                Some(Opcode::Chunk) => {
                    return Err(RosbagError::NestedChunk {
                        path: path.to_path_buf(),
                        offset: chunk_offset,
                    });
                }
                _ => {}
            }
        }
        Ok(resolved)
    }

    /// Looks up a message's channel (and schema, if any), producing a
    /// typed error rather than a fabricated/default channel when the
    /// spec's own ordering guarantee ("a Channel record must occur at
    /// least once... prior to any message referring to its channel id")
    /// was violated.
    fn resolve(
        &self,
        message: Message,
        offset: u64,
    ) -> Result<(Channel, Option<Schema>, Message), RosbagError> {
        let channel = self
            .reader
            .channels
            .get(&message.channel_id)
            .cloned()
            .ok_or_else(|| RosbagError::Malformed {
                path: self.reader.path.clone(),
                offset,
                reason: format!(
                    "message references channel id {} with no preceding Channel record",
                    message.channel_id
                ),
            })?;
        let schema = if channel.schema_id == 0 {
            None
        } else {
            Some(
                self.reader
                    .schemas
                    .get(&channel.schema_id)
                    .cloned()
                    .ok_or_else(|| RosbagError::Malformed {
                        path: self.reader.path.clone(),
                        offset,
                        reason: format!(
                            "channel {} references schema id {} with no preceding Schema record",
                            channel.id, channel.schema_id
                        ),
                    })?,
            )
        };
        Ok((channel, schema, message))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn prefixed_string(s: &str) -> Vec<u8> {
        let mut out = (s.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(s.as_bytes());
        out
    }

    fn prefixed_map(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut inner = Vec::new();
        for (k, v) in entries {
            inner.extend(prefixed_string(k));
            inner.extend(prefixed_string(v));
        }
        let mut out = (inner.len() as u32).to_le_bytes().to_vec();
        out.extend(inner);
        out
    }

    /// Assembles a byte-exact `.mcap` file from records built directly
    /// against the specification (see the module docs' provenance note):
    /// no external `mcap` tool ever touches these bytes.
    struct Builder {
        header: Vec<u8>,
        data: Vec<u8>,
        summary: Vec<u8>,
    }

    fn record(opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![opcode];
        out.extend((payload.len() as u64).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    impl Builder {
        fn new() -> Self {
            let mut header_body = prefixed_string("ros2");
            header_body.extend(prefixed_string("astrs-test"));
            Self {
                header: record(0x01, &header_body),
                data: Vec::new(),
                summary: Vec::new(),
            }
        }

        fn schema(mut self, id: u16, name: &str, encoding: &str, data: &[u8]) -> Self {
            let mut body = id.to_le_bytes().to_vec();
            body.extend(prefixed_string(name));
            body.extend(prefixed_string(encoding));
            body.extend((data.len() as u32).to_le_bytes());
            body.extend_from_slice(data);
            self.data.extend(record(0x03, &body));
            self
        }

        fn channel(mut self, id: u16, schema_id: u16, topic: &str, encoding: &str) -> Self {
            let mut body = id.to_le_bytes().to_vec();
            body.extend(schema_id.to_le_bytes());
            body.extend(prefixed_string(topic));
            body.extend(prefixed_string(encoding));
            body.extend(prefixed_map(&[]));
            self.data.extend(record(0x04, &body));
            self
        }

        fn message(mut self, channel_id: u16, log_time: u64, data: &[u8]) -> Self {
            let mut body = channel_id.to_le_bytes().to_vec();
            body.extend(0u32.to_le_bytes()); // sequence
            body.extend(log_time.to_le_bytes());
            body.extend(log_time.to_le_bytes()); // publish_time == log_time
            body.extend_from_slice(data);
            self.data.extend(record(0x05, &body));
            self
        }

        /// Appends a raw record directly into the data section — used to
        /// build a chunk's inner `records` bytes (Schema/Channel/Message,
        /// or deliberately a nested Chunk for the negative test).
        fn raw_record(opcode: u8, payload: &[u8]) -> Vec<u8> {
            record(opcode, payload)
        }

        fn chunk(
            mut self,
            compression: &str,
            declared_uncompressed_size: u64,
            inner_plain: &[u8],
        ) -> Self {
            let on_disk = match compression {
                "" => inner_plain.to_vec(),
                "lz4" => oxiarc_lz4::compress(inner_plain).unwrap(),
                "zstd" => oxiarc_zstd::compress(inner_plain).unwrap(),
                other => panic!("test helper does not know compression {other}"),
            };
            let mut body = 0u64.to_le_bytes().to_vec(); // message_start_time
            body.extend(0u64.to_le_bytes()); // message_end_time
            body.extend(declared_uncompressed_size.to_le_bytes());
            body.extend(0u32.to_le_bytes()); // uncompressed_crc (not verified)
            body.extend(prefixed_string(compression));
            body.extend((on_disk.len() as u64).to_le_bytes());
            body.extend_from_slice(&on_disk);
            self.data.extend(record(0x06, &body));
            self
        }

        /// Appends a chunk whose on-disk `records` bytes are exactly
        /// `raw_on_disk` (already "compressed", or deliberately corrupt) —
        /// for tests that need to control the compressed bytes directly
        /// rather than via a real codec.
        fn chunk_raw(
            mut self,
            compression: &str,
            declared_uncompressed_size: u64,
            raw_on_disk: &[u8],
        ) -> Self {
            let mut body = 0u64.to_le_bytes().to_vec();
            body.extend(0u64.to_le_bytes());
            body.extend(declared_uncompressed_size.to_le_bytes());
            body.extend(0u32.to_le_bytes());
            body.extend(prefixed_string(compression));
            body.extend((raw_on_disk.len() as u64).to_le_bytes());
            body.extend_from_slice(raw_on_disk);
            self.data.extend(record(0x06, &body));
            self
        }

        /// As [`Builder::chunk`], but with explicit `message_start_time`/
        /// `message_end_time` rather than `chunk`'s hardcoded `0`/`0` —
        /// needed by `iter_messages_in_time_range`'s own tests, which must
        /// control each chunk's declared span.
        fn chunk_timed(
            mut self,
            compression: &str,
            message_start_time: u64,
            message_end_time: u64,
            declared_uncompressed_size: u64,
            inner_plain: &[u8],
        ) -> Self {
            let on_disk = match compression {
                "" => inner_plain.to_vec(),
                "lz4" => oxiarc_lz4::compress(inner_plain).unwrap(),
                "zstd" => oxiarc_zstd::compress(inner_plain).unwrap(),
                other => panic!("test helper does not know compression {other}"),
            };
            let mut body = message_start_time.to_le_bytes().to_vec();
            body.extend(message_end_time.to_le_bytes());
            body.extend(declared_uncompressed_size.to_le_bytes());
            body.extend(0u32.to_le_bytes());
            body.extend(prefixed_string(compression));
            body.extend((on_disk.len() as u64).to_le_bytes());
            body.extend_from_slice(&on_disk);
            self.data.extend(record(0x06, &body));
            self
        }

        /// As [`Builder::chunk_raw`], but with explicit
        /// `message_start_time`/`message_end_time` — the corrupt-payload
        /// counterpart of [`Builder::chunk_timed`], for proving a chunk
        /// outside a queried time range is never decompressed.
        fn chunk_raw_timed(
            mut self,
            compression: &str,
            message_start_time: u64,
            message_end_time: u64,
            declared_uncompressed_size: u64,
            raw_on_disk: &[u8],
        ) -> Self {
            let mut body = message_start_time.to_le_bytes().to_vec();
            body.extend(message_end_time.to_le_bytes());
            body.extend(declared_uncompressed_size.to_le_bytes());
            body.extend(0u32.to_le_bytes());
            body.extend(prefixed_string(compression));
            body.extend((raw_on_disk.len() as u64).to_le_bytes());
            body.extend_from_slice(raw_on_disk);
            self.data.extend(record(0x06, &body));
            self
        }

        fn summary_schema(mut self, id: u16, name: &str, encoding: &str, data: &[u8]) -> Self {
            let mut body = id.to_le_bytes().to_vec();
            body.extend(prefixed_string(name));
            body.extend(prefixed_string(encoding));
            body.extend((data.len() as u32).to_le_bytes());
            body.extend_from_slice(data);
            self.summary.extend(record(0x03, &body));
            self
        }

        fn summary_channel(mut self, id: u16, schema_id: u16, topic: &str, encoding: &str) -> Self {
            let mut body = id.to_le_bytes().to_vec();
            body.extend(schema_id.to_le_bytes());
            body.extend(prefixed_string(topic));
            body.extend(prefixed_string(encoding));
            body.extend(prefixed_map(&[]));
            self.summary.extend(record(0x04, &body));
            self
        }

        fn summary_statistics(mut self, message_count: u64, channel_count: u32) -> Self {
            let mut body = message_count.to_le_bytes().to_vec();
            body.extend(0u16.to_le_bytes()); // schema_count
            body.extend(channel_count.to_le_bytes());
            body.extend(0u32.to_le_bytes()); // attachment_count
            body.extend(0u32.to_le_bytes()); // metadata_count
            body.extend(0u32.to_le_bytes()); // chunk_count
            body.extend(0u64.to_le_bytes()); // message_start_time
            body.extend(0u64.to_le_bytes()); // message_end_time
            body.extend(prefixed_map(&[])); // (reused for the u16/u64 map's empty case too)
            self.summary.extend(record(0x0b, &body));
            self
        }

        /// Finishes without a summary section: `Footer.summary_start = 0`.
        fn finish_unindexed(self) -> Vec<u8> {
            let mut bytes = MAGIC.to_vec();
            bytes.extend(self.header);
            bytes.extend(self.data);
            let mut footer_body = 0u64.to_le_bytes().to_vec();
            footer_body.extend(0u64.to_le_bytes());
            footer_body.extend(0u32.to_le_bytes());
            bytes.extend(record(0x02, &footer_body));
            bytes.extend(MAGIC);
            bytes
        }

        /// Finishes with the accumulated summary records, pointing the
        /// footer's `summary_start` at them.
        fn finish_indexed(self) -> Vec<u8> {
            let mut bytes = MAGIC.to_vec();
            bytes.extend(&self.header);
            bytes.extend(&self.data);
            let summary_start = bytes.len() as u64;
            bytes.extend(&self.summary);
            let mut footer_body = summary_start.to_le_bytes().to_vec();
            footer_body.extend(0u64.to_le_bytes()); // summary_offset_start: none
            footer_body.extend(0u32.to_le_bytes());
            bytes.extend(record(0x02, &footer_body));
            bytes.extend(MAGIC);
            bytes
        }
    }

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn write_temp(bytes: &[u8], label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "astrs-rosbag-mcap-test-{}-{}-{label}.mcap",
            std::process::id(),
            uniq()
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn opens_a_minimal_header_and_footer_only_file() {
        let bytes = Builder::new().finish_unindexed();
        let path = write_temp(&bytes, "minimal");
        let mut reader = Reader::open(&path).unwrap();
        assert_eq!(reader.header().profile, "ros2");
        assert!(!reader.is_indexed());
        assert_eq!(reader.iter_messages().count(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn round_trips_unchunked_messages_with_schema_and_channel() {
        let bytes = Builder::new()
            .schema(
                1,
                "sensor_msgs/msg/LaserScan",
                "ros2msg",
                b"float32[] ranges",
            )
            .channel(1, 1, "/scan", "cdr")
            .message(1, 100, &[1, 2, 3])
            .message(1, 200, &[4, 5, 6])
            .finish_unindexed();
        let path = write_temp(&bytes, "unchunked");
        let mut reader = Reader::open(&path).unwrap();
        let messages: Vec<ResolvedMessage> =
            reader.iter_messages().collect::<Result<_, _>>().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].channel.topic, "/scan");
        assert_eq!(
            messages[0].schema.as_ref().unwrap().name,
            "sensor_msgs/msg/LaserScan"
        );
        assert_eq!(messages[0].message.data, vec![1, 2, 3]);
        assert_eq!(messages[1].message.log_time, 200);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn round_trips_a_message_with_no_schema() {
        let bytes = Builder::new()
            .channel(1, 0, "/topic", "json")
            .message(1, 1, b"{}")
            .finish_unindexed();
        let path = write_temp(&bytes, "no-schema");
        let mut reader = Reader::open(&path).unwrap();
        let messages: Vec<ResolvedMessage> =
            reader.iter_messages().collect::<Result<_, _>>().unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].schema.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn round_trips_an_uncompressed_chunk() {
        let mut inner = Vec::new();
        let mut channel_body = 1u16.to_le_bytes().to_vec();
        channel_body.extend(0u16.to_le_bytes());
        channel_body.extend(prefixed_string("/chunked"));
        channel_body.extend(prefixed_string("cdr"));
        channel_body.extend(prefixed_map(&[]));
        inner.extend(Builder::raw_record(0x04, &channel_body));

        let mut message_body = 1u16.to_le_bytes().to_vec();
        message_body.extend(0u32.to_le_bytes());
        message_body.extend(50u64.to_le_bytes());
        message_body.extend(50u64.to_le_bytes());
        message_body.extend_from_slice(b"payload");
        inner.extend(Builder::raw_record(0x05, &message_body));

        let bytes = Builder::new()
            .chunk("", inner.len() as u64, &inner)
            .finish_unindexed();
        let path = write_temp(&bytes, "chunk-uncompressed");
        let mut reader = Reader::open(&path).unwrap();
        let messages: Vec<ResolvedMessage> =
            reader.iter_messages().collect::<Result<_, _>>().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].channel.topic, "/chunked");
        assert_eq!(messages[0].message.data, b"payload");
        let _ = std::fs::remove_file(&path);
    }

    fn build_chunk_inner(topic: &str, payload: &[u8]) -> Vec<u8> {
        let mut inner = Vec::new();
        let mut channel_body = 1u16.to_le_bytes().to_vec();
        channel_body.extend(0u16.to_le_bytes());
        channel_body.extend(prefixed_string(topic));
        channel_body.extend(prefixed_string("cdr"));
        channel_body.extend(prefixed_map(&[]));
        inner.extend(Builder::raw_record(0x04, &channel_body));

        let mut message_body = 1u16.to_le_bytes().to_vec();
        message_body.extend(0u32.to_le_bytes());
        message_body.extend(1u64.to_le_bytes());
        message_body.extend(1u64.to_le_bytes());
        message_body.extend_from_slice(payload);
        inner.extend(Builder::raw_record(0x05, &message_body));
        inner
    }

    /// As [`build_chunk_inner`], but with an explicit `channel_id` and
    /// `log_time` — needed by `iter_messages_in_time_range`'s own tests,
    /// which control both across several chunks rather than accepting
    /// `build_chunk_inner`'s hardcoded channel `1` / `log_time` `1`.
    fn chunk_inner_at(channel_id: u16, topic: &str, log_time: u64, payload: &[u8]) -> Vec<u8> {
        let mut inner = Vec::new();
        let mut channel_body = channel_id.to_le_bytes().to_vec();
        channel_body.extend(0u16.to_le_bytes());
        channel_body.extend(prefixed_string(topic));
        channel_body.extend(prefixed_string("cdr"));
        channel_body.extend(prefixed_map(&[]));
        inner.extend(Builder::raw_record(0x04, &channel_body));

        let mut message_body = channel_id.to_le_bytes().to_vec();
        message_body.extend(0u32.to_le_bytes());
        message_body.extend(log_time.to_le_bytes());
        message_body.extend(log_time.to_le_bytes());
        message_body.extend_from_slice(payload);
        inner.extend(Builder::raw_record(0x05, &message_body));
        inner
    }

    #[test]
    fn round_trips_an_lz4_compressed_chunk() {
        let inner = build_chunk_inner("/lz4", b"lz4-payload-data-that-compresses-ok");
        let bytes = Builder::new()
            .chunk("lz4", inner.len() as u64, &inner)
            .finish_unindexed();
        let path = write_temp(&bytes, "chunk-lz4");
        let mut reader = Reader::open(&path).unwrap();
        let messages: Vec<ResolvedMessage> =
            reader.iter_messages().collect::<Result<_, _>>().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].message.data,
            b"lz4-payload-data-that-compresses-ok"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn round_trips_a_zstd_compressed_chunk() {
        let inner = build_chunk_inner("/zstd", b"zstd-payload-data-that-compresses-ok");
        let bytes = Builder::new()
            .chunk("zstd", inner.len() as u64, &inner)
            .finish_unindexed();
        let path = write_temp(&bytes, "chunk-zstd");
        let mut reader = Reader::open(&path).unwrap();
        let messages: Vec<ResolvedMessage> =
            reader.iter_messages().collect::<Result<_, _>>().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].message.data,
            b"zstd-payload-data-that-compresses-ok"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn schema_and_channel_may_live_inside_a_chunk() {
        // Already exercised by every chunk test above (the Channel record
        // is always inside the chunk); this test makes the point
        // explicit by also asserting the reader's own tables were
        // updated as a side effect of draining the chunk.
        let inner = build_chunk_inner("/inside", b"x");
        let bytes = Builder::new()
            .chunk("", inner.len() as u64, &inner)
            .finish_unindexed();
        let path = write_temp(&bytes, "chunk-updates-tables");
        let mut reader = Reader::open(&path).unwrap();
        assert!(reader.channels().is_empty(), "not yet drained");
        let _ = reader.iter_messages().count();
        assert_eq!(reader.channels().len(), 1);
        assert_eq!(reader.channels()[&1].topic, "/inside");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_indexed_file_populates_schemas_channels_and_statistics_before_iterating() {
        let bytes = Builder::new()
            .schema(9, "std_msgs/msg/String", "ros2msg", b"string data")
            .channel(1, 9, "/topic", "cdr")
            .message(1, 1, b"x")
            .summary_schema(9, "std_msgs/msg/String", "ros2msg", b"string data")
            .summary_channel(1, 9, "/topic", "cdr")
            .summary_statistics(1, 1)
            .finish_indexed();
        let path = write_temp(&bytes, "indexed");
        let reader = Reader::open(&path).unwrap();
        assert!(reader.is_indexed());
        assert_eq!(reader.schemas().len(), 1);
        assert_eq!(reader.schemas()[&9].name, "std_msgs/msg/String");
        assert_eq!(reader.channels().len(), 1);
        assert_eq!(reader.statistics().unwrap().message_count, 1);
        assert!(reader.warnings().is_empty(), "{:?}", reader.warnings());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unindexed_file_has_empty_tables_until_iteration_drains_them() {
        let bytes = Builder::new()
            .channel(1, 0, "/topic", "cdr")
            .message(1, 1, b"x")
            .finish_unindexed();
        let path = write_temp(&bytes, "unindexed-lazy");
        let mut reader = Reader::open(&path).unwrap();
        assert!(!reader.is_indexed());
        assert!(reader.channels().is_empty());
        assert!(reader.statistics().is_none());
        let count = reader.iter_messages().count();
        assert_eq!(count, 1);
        assert_eq!(reader.channels().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_leading_magic_is_bad_magic_not_a_panic() {
        let mut bytes = Builder::new().finish_unindexed();
        bytes[0] = 0x00;
        let path = write_temp(&bytes, "bad-leading-magic");
        assert!(matches!(
            Reader::open(&path),
            Err(RosbagError::BadMagic { .. })
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_trailing_magic_is_bad_magic_not_a_panic() {
        let mut bytes = Builder::new().finish_unindexed();
        let last = bytes.len() - 1;
        bytes[last] = 0x00;
        let path = write_temp(&bytes, "bad-trailing-magic");
        assert!(matches!(
            Reader::open(&path),
            Err(RosbagError::BadMagic { .. })
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_shorter_than_two_magics_is_bad_magic_not_a_panic() {
        let path = write_temp(&[0x89, b'M', b'C'], "too-short");
        assert!(matches!(
            Reader::open(&path),
            Err(RosbagError::BadMagic { .. })
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_truncated_record_is_truncated_not_a_panic_at_every_cut_point() {
        let bytes = Builder::new()
            .channel(1, 0, "/topic", "cdr")
            .message(1, 1, b"payload-bytes")
            .finish_unindexed();
        // Cutting anywhere in the *data section* (never inside the
        // trailing magic/footer, which `open` itself already validated
        // above) must never panic.
        let data_section_end = bytes.len() - MAGIC.len() - 29 /* footer */;
        for cut in 16..data_section_end {
            let path = write_temp(&bytes[..cut], "truncated");
            match Reader::open(&path) {
                Err(RosbagError::BadMagic { .. } | RosbagError::Truncated { .. }) => {}
                other => panic!("cut at {cut}: {other:?}"),
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn a_message_referencing_an_unknown_channel_is_malformed() {
        let bytes = Builder::new().message(99, 1, b"orphan").finish_unindexed();
        let path = write_temp(&bytes, "unknown-channel");
        let mut reader = Reader::open(&path).unwrap();
        let err = reader.iter_messages().next().unwrap().unwrap_err();
        assert!(matches!(err, RosbagError::Malformed { .. }));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_channel_referencing_an_unknown_schema_is_malformed() {
        let bytes = Builder::new()
            .channel(1, 42, "/topic", "cdr")
            .message(1, 1, b"x")
            .finish_unindexed();
        let path = write_temp(&bytes, "unknown-schema");
        let mut reader = Reader::open(&path).unwrap();
        let err = reader.iter_messages().next().unwrap().unwrap_err();
        assert!(matches!(err, RosbagError::Malformed { .. }));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn schema_id_zero_is_ignored_per_spec() {
        let bytes = Builder::new()
            .schema(0, "invalid", "", b"")
            .channel(1, 0, "/topic", "cdr")
            .message(1, 1, b"x")
            .finish_unindexed();
        let path = write_temp(&bytes, "schema-zero");
        let mut reader = Reader::open(&path).unwrap();
        let messages: Vec<ResolvedMessage> =
            reader.iter_messages().collect::<Result<_, _>>().unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].schema.is_none());
        assert!(reader.schemas().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_chunk_claiming_an_oversized_uncompressed_size_is_refused_before_decompressing() {
        // A tiny on-disk payload with an enormous *declared* size: if the
        // ceiling were checked after attempting decompression, this would
        // either hang trying to inflate garbage or panic; checked first,
        // it is an immediate typed error.
        let bytes = Builder::new()
            .chunk_raw("lz4", MAX_CHUNK_UNCOMPRESSED_BYTES + 1, &[0u8; 8])
            .finish_unindexed();
        let path = write_temp(&bytes, "chunk-bomb");
        let mut reader = Reader::open(&path).unwrap();
        let err = reader.iter_messages().next().unwrap().unwrap_err();
        assert!(matches!(err, RosbagError::ChunkTooLarge { .. }), "{err:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_nested_chunk_is_refused() {
        let inner_chunk_body = {
            let mut body = 0u64.to_le_bytes().to_vec();
            body.extend(0u64.to_le_bytes());
            body.extend(0u64.to_le_bytes());
            body.extend(0u32.to_le_bytes());
            body.extend(prefixed_string(""));
            body.extend(0u64.to_le_bytes());
            body
        };
        let nested = Builder::raw_record(0x06, &inner_chunk_body);
        let bytes = Builder::new()
            .chunk("", nested.len() as u64, &nested)
            .finish_unindexed();
        let path = write_temp(&bytes, "nested-chunk");
        let mut reader = Reader::open(&path).unwrap();
        let err = reader.iter_messages().next().unwrap().unwrap_err();
        assert!(matches!(err, RosbagError::NestedChunk { .. }), "{err:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unknown_compression_is_refused() {
        let bytes = Builder::new()
            .chunk_raw("bzip2", 4, &[1, 2, 3, 4])
            .finish_unindexed();
        let path = write_temp(&bytes, "unknown-compression");
        let mut reader = Reader::open(&path).unwrap();
        let err = reader.iter_messages().next().unwrap().unwrap_err();
        assert!(
            matches!(err, RosbagError::UnknownCompression { .. }),
            "{err:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_declared_size_mismatch_after_decompression_is_refused() {
        // compression="" means `records` IS the "decompressed" bytes
        // verbatim, so declaring a different size than what is actually
        // there is a direct, controllable mismatch.
        let bytes = Builder::new()
            .chunk_raw("", 999, b"only-nine-bytes")
            .finish_unindexed();
        let path = write_temp(&bytes, "size-mismatch");
        let mut reader = Reader::open(&path).unwrap();
        let err = reader.iter_messages().next().unwrap().unwrap_err();
        assert!(
            matches!(err, RosbagError::ChunkSizeMismatch { .. }),
            "{err:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unrecognized_opcode_in_the_data_section_is_skipped() {
        let mut bytes_builder = Builder::new().channel(1, 0, "/topic", "cdr");
        // A private-use record (0x80) spliced in before the message.
        bytes_builder.data.extend(record(0x80, b"vendor-specific"));
        let bytes = bytes_builder.message(1, 1, b"x").finish_unindexed();
        let path = write_temp(&bytes, "unrecognized-opcode");
        let mut reader = Reader::open(&path).unwrap();
        let messages: Vec<ResolvedMessage> =
            reader.iter_messages().collect::<Result<_, _>>().unwrap();
        assert_eq!(messages.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_attachment_and_read_metadata_use_their_index_offsets() {
        // Build: [Header][Attachment][Metadata][Footer(indexed)],
        // recording each record's real offset for its index entry.
        let mut bytes = MAGIC.to_vec();
        let mut header_body = prefixed_string("");
        header_body.extend(prefixed_string(""));
        bytes.extend(record(0x01, &header_body));

        let attachment_offset = bytes.len() as u64;
        let mut attachment_body = 10u64.to_le_bytes().to_vec();
        attachment_body.extend(20u64.to_le_bytes());
        attachment_body.extend(prefixed_string("calib.yaml"));
        attachment_body.extend(prefixed_string("text/plain"));
        attachment_body.extend(4u64.to_le_bytes());
        attachment_body.extend_from_slice(b"data");
        attachment_body.extend(0u32.to_le_bytes());
        let attachment_record = record(0x09, &attachment_body);
        let attachment_len = attachment_record.len() as u64;
        bytes.extend(&attachment_record);

        let metadata_offset = bytes.len() as u64;
        let mut metadata_body = prefixed_string("info");
        metadata_body.extend(prefixed_map(&[("k", "v")]));
        let metadata_record = record(0x0c, &metadata_body);
        let metadata_len = metadata_record.len() as u64;
        bytes.extend(&metadata_record);

        let summary_start = bytes.len() as u64;
        let mut attachment_index_body = attachment_offset.to_le_bytes().to_vec();
        attachment_index_body.extend(attachment_len.to_le_bytes());
        attachment_index_body.extend(10u64.to_le_bytes());
        attachment_index_body.extend(20u64.to_le_bytes());
        attachment_index_body.extend(4u64.to_le_bytes());
        attachment_index_body.extend(prefixed_string("calib.yaml"));
        attachment_index_body.extend(prefixed_string("text/plain"));
        bytes.extend(record(0x0a, &attachment_index_body));

        let mut metadata_index_body = metadata_offset.to_le_bytes().to_vec();
        metadata_index_body.extend(metadata_len.to_le_bytes());
        metadata_index_body.extend(prefixed_string("info"));
        bytes.extend(record(0x0d, &metadata_index_body));

        let mut footer_body = summary_start.to_le_bytes().to_vec();
        footer_body.extend(0u64.to_le_bytes());
        footer_body.extend(0u32.to_le_bytes());
        bytes.extend(record(0x02, &footer_body));
        bytes.extend(MAGIC);

        let path = write_temp(&bytes, "attachment-metadata");
        let mut reader = Reader::open(&path).unwrap();
        assert_eq!(reader.attachment_indexes().len(), 1);
        assert_eq!(reader.metadata_indexes().len(), 1);
        let attachment_index = reader.attachment_indexes()[0].clone();
        let attachment = reader.read_attachment(&attachment_index).unwrap();
        assert_eq!(attachment.name, "calib.yaml");
        assert_eq!(attachment.data, b"data");
        let metadata_index = reader.metadata_indexes()[0].clone();
        let metadata = reader.read_metadata(&metadata_index).unwrap();
        assert_eq!(metadata.name, "info");
        assert_eq!(metadata.metadata.get("k").map(String::as_str), Some("v"));
        let _ = std::fs::remove_file(&path);
    }

    // -----------------------------------------------------------------
    // `iter_messages_in_time_range` and `chunk_indexes` — the message-index
    // -assisted read path (blueprint §10.6).
    // -----------------------------------------------------------------

    #[test]
    fn iter_messages_in_time_range_returns_only_messages_inside_the_window() {
        // Two chunks, both entirely outside the query window (one below,
        // one above), plus one top-level message inside it. Proves the
        // filter applies uniformly to chunked and unchunked messages in
        // the same file.
        let chunk_a = chunk_inner_at(1, "/a", 10, b"early");
        let chunk_c = chunk_inner_at(3, "/c", 200, b"very-late");
        let bytes = Builder::new()
            .chunk_timed("", 10, 10, chunk_a.len() as u64, &chunk_a)
            .channel(2, 0, "/top", "cdr")
            .message(2, 50, b"in-range-top-level")
            .chunk_timed("", 200, 200, chunk_c.len() as u64, &chunk_c)
            .finish_unindexed();
        let path = write_temp(&bytes, "time-range-basic");
        let mut reader = Reader::open(&path).unwrap();
        let messages: Vec<ResolvedMessage> = reader
            .iter_messages_in_time_range(40, 60)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message.data, b"in-range-top-level");
        assert_eq!(messages[0].message.log_time, 50);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn iter_messages_in_time_range_never_decompresses_a_chunk_outside_the_window() {
        // A `Chunk` record whose declared span is entirely outside the
        // query window carries deliberately corrupt "zstd" bytes:
        // `decompress_chunk` would return `RosbagError::Codec` if this
        // chunk were ever actually decompressed. The whole point of the
        // skip is that it never is — proven by iteration still succeeding.
        let good_inner = chunk_inner_at(1, "/good", 500, b"kept");
        let bytes = Builder::new()
            .chunk_raw_timed("zstd", 10, 20, 999, &[0xde, 0xad, 0xbe, 0xef])
            .chunk_timed("", 500, 500, good_inner.len() as u64, &good_inner)
            .finish_unindexed();
        let path = write_temp(&bytes, "time-range-skips-corrupt-chunk");
        let mut reader = Reader::open(&path).unwrap();
        let messages: Vec<ResolvedMessage> = reader
            .iter_messages_in_time_range(400, 600)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message.data, b"kept");

        // Sanity check the corrupt chunk really would fail if
        // decompressed — otherwise this test would not actually be
        // exercising the skip path at all. A fresh, unfiltered
        // `iter_messages()` call re-walks the data section from the start
        // and does attempt to decompress every chunk.
        let err = reader.iter_messages().next().unwrap().unwrap_err();
        assert!(matches!(err, RosbagError::Codec { .. }), "{err:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn iter_messages_in_time_range_is_inclusive_at_both_ends() {
        let bytes = Builder::new()
            .channel(1, 0, "/x", "cdr")
            .message(1, 9, b"before")
            .message(1, 10, b"at-start")
            .message(1, 20, b"at-end")
            .message(1, 21, b"after")
            .finish_unindexed();
        let path = write_temp(&bytes, "time-range-boundaries");
        let mut reader = Reader::open(&path).unwrap();
        let messages: Vec<ResolvedMessage> = reader
            .iter_messages_in_time_range(10, 20)
            .collect::<Result<_, _>>()
            .unwrap();
        let payloads: Vec<&[u8]> = messages.iter().map(|m| m.message.data.as_slice()).collect();
        assert_eq!(payloads, vec![b"at-start".as_slice(), b"at-end".as_slice()]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn iter_messages_in_time_range_filters_individual_messages_within_an_overlapping_chunk() {
        // The chunk's own span (0..100) overlaps the query (40..60), so it
        // is decompressed — but only one of its three messages actually
        // falls inside the window, proving `drain_chunk`'s own per-message
        // filter (not just the chunk-level skip) is what does the work
        // here.
        let mut inner = Vec::new();
        let mut channel_body = 1u16.to_le_bytes().to_vec();
        channel_body.extend(0u16.to_le_bytes());
        channel_body.extend(prefixed_string("/mixed"));
        channel_body.extend(prefixed_string("cdr"));
        channel_body.extend(prefixed_map(&[]));
        inner.extend(Builder::raw_record(0x04, &channel_body));
        for (log_time, payload) in [(0u64, [1u8]), (50u64, [2u8]), (100u64, [3u8])] {
            let mut body = 1u16.to_le_bytes().to_vec();
            body.extend(0u32.to_le_bytes());
            body.extend(log_time.to_le_bytes());
            body.extend(log_time.to_le_bytes());
            body.extend_from_slice(&payload);
            inner.extend(Builder::raw_record(0x05, &body));
        }
        let bytes = Builder::new()
            .chunk_timed("", 0, 100, inner.len() as u64, &inner)
            .finish_unindexed();
        let path = write_temp(&bytes, "time-range-within-chunk");
        let mut reader = Reader::open(&path).unwrap();
        let messages: Vec<ResolvedMessage> = reader
            .iter_messages_in_time_range(40, 60)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message.log_time, 50);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn chunk_indexes_are_retained_from_the_summary_section() {
        // Hand-built (same offset-tracking style as
        // `read_attachment_and_read_metadata_use_their_index_offsets`):
        // one uncompressed chunk in the Data section, plus a `ChunkIndex`
        // record in the summary section pointing back at it — proving
        // `Reader::chunk_indexes` retains what it parses rather than
        // discarding it.
        let mut bytes = MAGIC.to_vec();
        let header_body = [prefixed_string(""), prefixed_string("")].concat();
        bytes.extend(record(0x01, &header_body));

        let chunk_offset = bytes.len() as u64;
        let inner = chunk_inner_at(1, "/scan", 42, b"payload");
        let mut chunk_body = 42u64.to_le_bytes().to_vec(); // message_start_time
        chunk_body.extend(42u64.to_le_bytes()); // message_end_time
        chunk_body.extend((inner.len() as u64).to_le_bytes()); // uncompressed_size
        chunk_body.extend(0u32.to_le_bytes()); // uncompressed_crc
        chunk_body.extend(prefixed_string("")); // compression
        chunk_body.extend((inner.len() as u64).to_le_bytes());
        chunk_body.extend_from_slice(&inner);
        let chunk_record = record(0x06, &chunk_body);
        let chunk_len = chunk_record.len() as u64;
        bytes.extend(&chunk_record);

        let summary_start = bytes.len() as u64;
        let mut chunk_index_body = 42u64.to_le_bytes().to_vec(); // message_start_time
        chunk_index_body.extend(42u64.to_le_bytes()); // message_end_time
        chunk_index_body.extend(chunk_offset.to_le_bytes()); // chunk_start_offset
        chunk_index_body.extend(chunk_len.to_le_bytes()); // chunk_length
        chunk_index_body.extend(0u32.to_le_bytes()); // empty message_index_offsets map
        chunk_index_body.extend(0u64.to_le_bytes()); // message_index_length
        chunk_index_body.extend(prefixed_string("")); // compression
        chunk_index_body.extend((inner.len() as u64).to_le_bytes()); // compressed_size
        chunk_index_body.extend((inner.len() as u64).to_le_bytes()); // uncompressed_size
        bytes.extend(record(0x08, &chunk_index_body));

        let mut footer_body = summary_start.to_le_bytes().to_vec();
        footer_body.extend(0u64.to_le_bytes());
        footer_body.extend(0u32.to_le_bytes());
        bytes.extend(record(0x02, &footer_body));
        bytes.extend(MAGIC);

        let path = write_temp(&bytes, "chunk-indexes-retained");
        let reader = Reader::open(&path).unwrap();
        assert!(reader.is_indexed());
        assert_eq!(reader.chunk_indexes().len(), 1);
        let index = &reader.chunk_indexes()[0];
        assert_eq!(index.chunk_start_offset, chunk_offset);
        assert_eq!(index.chunk_length, chunk_len);
        assert_eq!(index.message_start_time, 42);
        assert_eq!(index.message_end_time, 42);
        let _ = std::fs::remove_file(&path);
    }
}
