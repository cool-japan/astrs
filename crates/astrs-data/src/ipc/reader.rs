//! The Arrow IPC **streaming format** reader.
//!
//! [`IpcStreamReader`] decodes a schema message followed by any number of
//! record batch messages, stopping at the end-of-stream marker or at a clean
//! end of input (the pre-0.15 encoding writes no marker).
//!
//! # Copy versus zero-copy
//!
//! | Constructor | Copies | Decoded arrays |
//! |---|---|---|
//! | [`IpcStreamReader::new`] | nothing | windows into the buffer you passed |
//! | [`IpcStreamReader::from_slice`] | the stream, once, into a 64-byte-aligned buffer | windows into that copy |
//! | [`IpcStreamReader::from_reader`] | the stream, once, as it is read | windows into that copy |
//!
//! [`IpcStreamReader::new`] is the path AstRS itself takes: a payload mapped
//! out of shared memory arrives as a [`Buffer`] over the mapping, and the
//! arrays this reader hands back point straight into it. Nothing is copied,
//! not even for a 200 MB tensor — which is the entire point of aligning the
//! buffers on the way out ([`crate::ipc::writer`]).
//!
//! One caveat makes the difference visible: reading `[i64]` out of an
//! unaligned address is undefined behaviour, so a typed buffer that does not
//! land on its natural alignment *is* copied ([`crate::ipc::decode`]). Streams
//! written by AstRS put every body buffer on a 64-byte boundary, so passing a
//! [`crate::AlignedBuf`]-backed [`Buffer`] to
//! [`IpcStreamReader::new`] keeps the whole decode copy-free.
//!
//! ```
//! use astrs_data::array::{Array, Float32Array, IntoArrayRef};
//! use astrs_data::ipc::{to_ipc_bytes, IpcStreamReader};
//! use astrs_data::{Buffer, RecordBatch};
//!
//! let batch = RecordBatch::from_payload(
//!     Float32Array::from_opt_iter([Some(1.5), None, Some(2.5)]).into_array_ref(),
//! );
//! let bytes = to_ipc_bytes(std::slice::from_ref(&batch))?;
//!
//! let mut reader = IpcStreamReader::new(Buffer::from(bytes))?;
//! assert_eq!(reader.schema().len(), 1);
//! let decoded = reader.next_batch()?.expect("one batch");
//! assert_eq!(decoded, batch);
//! assert!(reader.next_batch()?.is_none(), "the stream ends after one batch");
//! # Ok::<(), astrs_data::ipc::IpcError>(())
//! ```

use std::io::Read;
use std::sync::Arc;

use crate::buffer::{AlignedBuf, Buffer};
use crate::datatype::Schema;
use crate::ipc::decode::{assemble_batch, decode_columns};
use crate::ipc::error::{IpcError, Result};
use crate::ipc::format::{body_compression, message_header, record_batch};
use crate::ipc::message::{MessageLimits, decode_message, scan_message};
use crate::ipc::schema::decode_schema;
use crate::record_batch::RecordBatch;

/// Default ceiling on how many bytes [`IpcStreamReader::from_reader`] will
/// pull out of an unbounded source: 1 GiB, four whole payloads.
pub const DEFAULT_MAX_STREAM_BYTES: usize = 1024 * 1024 * 1024;

/// Chunk size used while slurping a reader.
const READ_CHUNK: usize = 64 * 1024;

/// What a reader accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReadOptions {
    /// Per-message caps.
    pub limits: MessageLimits,
    /// Cap on a whole stream slurped from an [`std::io::Read`].
    pub max_stream_bytes: usize,
}

impl Default for ReadOptions {
    #[inline]
    fn default() -> Self {
        Self {
            limits: MessageLimits::default(),
            max_stream_bytes: DEFAULT_MAX_STREAM_BYTES,
        }
    }
}

impl ReadOptions {
    /// The defaults, spelled out.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the per-message caps.
    #[inline]
    #[must_use]
    pub const fn with_limits(mut self, limits: MessageLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets the whole-stream cap used by [`IpcStreamReader::from_reader`].
    #[inline]
    #[must_use]
    pub const fn with_max_stream_bytes(mut self, bytes: usize) -> Self {
        self.max_stream_bytes = bytes;
        self
    }
}

/// Decodes an Arrow IPC stream held in memory.
///
/// Implements [`Iterator`], yielding `Result<RecordBatch>` — see the [module
/// documentation](self) for the copy/zero-copy matrix.
#[derive(Debug)]
pub struct IpcStreamReader {
    /// The whole stream.
    bytes: Buffer,
    /// The schema every batch is decoded against.
    schema: Arc<Schema>,
    /// Offset of the next message.
    position: usize,
    /// Configured caps.
    options: ReadOptions,
    /// Batches yielded so far.
    batches_read: usize,
    /// Whether the end of the stream has been reached.
    finished: bool,
}

impl IpcStreamReader {
    /// Reads the schema message of a stream already in memory, **without
    /// copying it**.
    ///
    /// # Errors
    ///
    /// * [`IpcError::EmptyStream`] when the stream carries no message at all.
    /// * [`IpcError::UnexpectedMessage`] when it does not open with a schema.
    /// * Any decode error from the schema message.
    pub fn new(bytes: Buffer) -> Result<Self> {
        Self::with_options(bytes, ReadOptions::new())
    }

    /// [`IpcStreamReader::new`] with explicit caps.
    ///
    /// # Errors
    ///
    /// As [`IpcStreamReader::new`].
    pub fn with_options(bytes: Buffer, options: ReadOptions) -> Result<Self> {
        let Some(message) = scan_message(bytes.as_slice(), 0, &options.limits)? else {
            return Err(IpcError::EmptyStream);
        };
        let info = decode_message(&bytes.as_slice()[message.metadata.clone()])?;
        if info.header_type != message_header::SCHEMA {
            return Err(IpcError::UnexpectedMessage {
                expected: "Schema",
                found: message_header::name(info.header_type),
            });
        }
        let schema = Arc::new(decode_schema(&info.header)?);
        Ok(Self {
            bytes,
            schema,
            position: message.next,
            options,
            batches_read: 0,
            finished: false,
        })
    }

    /// Copies `bytes` into a 64-byte-aligned buffer and reads its schema.
    ///
    /// The copy is what makes the arrays that come out of the decode safe to
    /// reinterpret as `[i64]` and friends whatever the caller's slice was
    /// aligned to. Use [`IpcStreamReader::new`] to avoid it.
    ///
    /// # Errors
    ///
    /// As [`IpcStreamReader::new`].
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        Self::new(Buffer::from(AlignedBuf::from_slice(bytes)))
    }

    /// Reads a whole stream out of `reader` into a 64-byte-aligned buffer.
    ///
    /// Bounded by [`ReadOptions::max_stream_bytes`]: a source that keeps
    /// producing bytes is cut off with [`IpcError::TooLarge`] rather than
    /// exhausting memory.
    ///
    /// # Errors
    ///
    /// * [`IpcError::Io`] when the source fails.
    /// * [`IpcError::TooLarge`] when the source exceeds the cap.
    /// * As [`IpcStreamReader::new`] for the schema message.
    pub fn from_reader<R: Read>(reader: R) -> Result<Self> {
        Self::from_reader_with_options(reader, ReadOptions::new())
    }

    /// [`IpcStreamReader::from_reader`] with explicit caps.
    ///
    /// # Errors
    ///
    /// As [`IpcStreamReader::from_reader`].
    pub fn from_reader_with_options<R: Read>(mut reader: R, options: ReadOptions) -> Result<Self> {
        let mut buffer = AlignedBuf::new();
        let mut chunk = [0u8; READ_CHUNK];
        loop {
            let read = reader
                .read(&mut chunk)
                .map_err(|err| IpcError::io("reading an IPC stream", err))?;
            if read == 0 {
                break;
            }
            if buffer.len() + read > options.max_stream_bytes {
                return Err(IpcError::TooLarge {
                    what: "stream",
                    length: (buffer.len() + read) as u64,
                    cap: options.max_stream_bytes as u64,
                });
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        Self::with_options(Buffer::from(buffer), options)
    }

    /// The stream's schema.
    #[inline]
    #[must_use]
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }

    /// A cloned handle to the schema, for building batches.
    #[inline]
    #[must_use]
    pub fn schema_ref(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    /// Byte offset of the next message.
    #[inline]
    #[must_use]
    pub const fn position(&self) -> usize {
        self.position
    }

    /// Batches yielded so far.
    #[inline]
    #[must_use]
    pub const fn batches_read(&self) -> usize {
        self.batches_read
    }

    /// Whether the end of the stream has been reached.
    #[inline]
    #[must_use]
    pub const fn is_finished(&self) -> bool {
        self.finished
    }

    /// The bytes the reader was built over.
    #[inline]
    #[must_use]
    pub const fn bytes(&self) -> &Buffer {
        &self.bytes
    }

    /// Decodes the next record batch, or `None` at the end of the stream.
    ///
    /// # Errors
    ///
    /// Any structural or unsupported-feature variant of [`IpcError`].
    pub fn next_batch(&mut self) -> Result<Option<RecordBatch>> {
        if self.finished {
            return Ok(None);
        }
        let stream = self.bytes.as_slice();
        let Some(message) = scan_message(stream, self.position, &self.options.limits)? else {
            self.finished = true;
            return Ok(None);
        };
        let info = decode_message(&stream[message.metadata.clone()])?;
        match info.header_type {
            message_header::RECORD_BATCH => {}
            message_header::DICTIONARY_BATCH => return Err(IpcError::UnsupportedDictionary),
            message_header::SCHEMA => {
                return Err(IpcError::UnexpectedMessage {
                    expected: "RecordBatch",
                    found: "Schema",
                });
            }
            other => {
                return Err(IpcError::UnsupportedMessageHeader {
                    header: other,
                    name: message_header::name(other),
                });
            }
        }

        let header = &info.header;
        if let Some(compression) = header.table(record_batch::COMPRESSION)? {
            return Err(IpcError::UnsupportedCompression {
                codec: compression.i8(body_compression::CODEC, 0)?,
            });
        }
        let rows = usize::try_from(header.i64(record_batch::LENGTH, 0)?)
            .map_err(|_| IpcError::malformed("RecordBatch.length", message.metadata.start))?;
        let nodes = read_pairs(header, record_batch::NODES)?;
        let buffers = read_pairs(header, record_batch::BUFFERS)?;

        let body = self
            .bytes
            .slice(message.body.start, message.body.end - message.body.start);
        let columns = decode_columns(self.schema.fields(), rows, &body, &nodes, &buffers)?;
        let batch = assemble_batch(&self.schema, columns, rows)?;

        self.position = message.next;
        self.batches_read += 1;
        Ok(Some(batch))
    }

    /// Decodes every remaining batch.
    ///
    /// # Errors
    ///
    /// As [`IpcStreamReader::next_batch`].
    pub fn read_all(&mut self) -> Result<Vec<RecordBatch>> {
        let mut batches = Vec::new();
        while let Some(batch) = self.next_batch()? {
            batches.push(batch);
        }
        Ok(batches)
    }
}

impl Iterator for IpcStreamReader {
    type Item = Result<RecordBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_batch() {
            Ok(Some(batch)) => Some(Ok(batch)),
            Ok(None) => None,
            Err(err) => {
                // A failed message leaves the reader where it was; refusing to
                // continue keeps a broken stream from yielding garbage.
                self.finished = true;
                Some(Err(err))
            }
        }
    }
}

/// Reads a `[FieldNode]`/`[Buffer]` struct vector into owned pairs.
///
/// Both are vectors of two `i64`s, and both are small — one entry per array
/// or per buffer — so materialising them decouples the decoder from the
/// metadata block's lifetime for the cost of a few dozen bytes.
fn read_pairs(table: &crate::ipc::fb::Table<'_>, slot: usize) -> Result<Vec<(i64, i64)>> {
    let Some(vector) = table.vector(slot, 16)? else {
        return Ok(Vec::new());
    };
    let mut pairs = Vec::with_capacity(vector.len());
    for index in 0..vector.len() {
        pairs.push(vector.struct_pair(index)?);
    }
    Ok(pairs)
}

/// Decodes a whole stream from memory.
///
/// # Errors
///
/// As [`IpcStreamReader::from_slice`] and [`IpcStreamReader::next_batch`].
///
/// ```
/// use astrs_data::array::{Int16Array, IntoArrayRef};
/// use astrs_data::ipc::{read_ipc_stream, to_ipc_bytes};
/// use astrs_data::RecordBatch;
///
/// let batch = RecordBatch::from_payload(Int16Array::from_values([1, 2]).into_array_ref());
/// let bytes = to_ipc_bytes(std::slice::from_ref(&batch))?;
/// let (schema, batches) = read_ipc_stream(bytes.as_slice())?;
/// assert_eq!(schema.len(), 1);
/// assert_eq!(batches, vec![batch]);
/// # Ok::<(), astrs_data::ipc::IpcError>(())
/// ```
pub fn read_ipc_stream(bytes: &[u8]) -> Result<(Arc<Schema>, Vec<RecordBatch>)> {
    let mut reader = IpcStreamReader::from_slice(bytes)?;
    let batches = reader.read_all()?;
    Ok((reader.schema_ref(), batches))
}

/// Decodes a whole stream from an [`std::io::Read`].
///
/// # Errors
///
/// As [`IpcStreamReader::from_reader`] and [`IpcStreamReader::next_batch`].
pub fn read_ipc_stream_from<R: Read>(reader: R) -> Result<(Arc<Schema>, Vec<RecordBatch>)> {
    let mut reader = IpcStreamReader::from_reader(reader)?;
    let batches = reader.read_all()?;
    Ok((reader.schema_ref(), batches))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{Int32Array, IntoArrayRef, StringArray};
    use crate::datatype::{DataType, Field};
    use crate::ipc::format::CONTINUATION_MARKER;
    use crate::ipc::writer::{IpcStreamWriter, WriteOptions, to_ipc_bytes, write_ipc_stream_with};

    fn sample_batch(seed: i32) -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("i", DataType::Int32, true),
            Field::new("s", DataType::Utf8, true),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Int32Array::from_opt_iter([Some(seed), None, Some(seed + 2)]).into_array_ref(),
                StringArray::from_opt_iter([Some("aa"), Some("bbb"), None]).into_array_ref(),
            ],
        )
        .expect("batch")
    }

    #[test]
    fn a_stream_round_trips_through_the_reader() {
        let batches = vec![sample_batch(1), sample_batch(10)];
        let bytes = to_ipc_bytes(&batches).expect("write");
        let (schema, decoded) = read_ipc_stream(bytes.as_slice()).expect("read");
        assert_eq!(schema, batches[0].schema_ref());
        assert_eq!(decoded, batches);
    }

    #[test]
    fn the_reader_is_an_iterator() {
        let batches = vec![sample_batch(1), sample_batch(2), sample_batch(3)];
        let bytes = to_ipc_bytes(&batches).expect("write");
        let reader = IpcStreamReader::from_slice(bytes.as_slice()).expect("reader");
        let collected: Result<Vec<RecordBatch>> = reader.collect();
        assert_eq!(collected.expect("batches"), batches);
    }

    #[test]
    fn decoding_is_zero_copy_from_an_owned_buffer() {
        let batch = RecordBatch::from_payload(
            crate::array::UInt8Array::from_values((0..255u8).collect::<Vec<_>>()).into_array_ref(),
        );
        let bytes = to_ipc_bytes(std::slice::from_ref(&batch)).expect("write");
        let buffer = Buffer::from(bytes);
        let before = buffer.share_count();
        let mut reader = IpcStreamReader::new(buffer).expect("reader");
        let decoded = reader.next_batch().expect("decode").expect("batch");
        assert_eq!(decoded, batch);
        // The decoded column shares the reader's allocation instead of owning
        // a copy of it.
        assert!(
            reader.bytes().share_count() > before,
            "columns must window into the stream"
        );
    }

    #[test]
    fn a_schema_only_stream_yields_no_batches() {
        let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Bool, true)]));
        let mut bytes = Vec::new();
        write_ipc_stream_with(&mut bytes, Arc::clone(&schema), &[], WriteOptions::new())
            .expect("write");
        let (decoded, batches) = read_ipc_stream(&bytes).expect("read");
        assert_eq!(decoded, schema);
        assert!(batches.is_empty());
    }

    #[test]
    fn a_stream_without_a_terminator_still_reads() {
        let batch = sample_batch(4);
        let mut sink = Vec::new();
        {
            let mut writer =
                IpcStreamWriter::try_new(&mut sink, batch.schema_ref()).expect("writer");
            writer.write(&batch).expect("write");
            // Deliberately no `finish()`.
        }
        let (_, batches) = read_ipc_stream(&sink).expect("read");
        assert_eq!(batches, vec![batch]);
    }

    #[test]
    fn an_empty_stream_is_an_error() {
        let err = IpcStreamReader::from_slice(&[]).unwrap_err();
        assert!(matches!(err, IpcError::EmptyStream), "{err}");
        let mut only_marker = Vec::new();
        only_marker.extend_from_slice(&CONTINUATION_MARKER.to_le_bytes());
        only_marker.extend_from_slice(&0i32.to_le_bytes());
        let err = IpcStreamReader::from_slice(&only_marker).unwrap_err();
        assert!(matches!(err, IpcError::EmptyStream), "{err}");
    }

    #[test]
    fn a_truncated_prefix_is_an_end_of_stream_error() {
        let err = IpcStreamReader::from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0x10]).unwrap_err();
        assert!(
            matches!(err, IpcError::UnexpectedEndOfStream { .. }),
            "{err}"
        );
    }

    #[test]
    fn every_truncation_of_a_stream_is_rejected_or_short() {
        let batch = sample_batch(7);
        let bytes = to_ipc_bytes(std::slice::from_ref(&batch)).expect("write");
        for cut in 0..bytes.len() {
            match IpcStreamReader::from_slice(&bytes.as_slice()[..cut]) {
                Ok(mut reader) => {
                    // A truncated stream may still hold a valid schema; what
                    // it must never do is yield a bogus batch.
                    if let Ok(Some(decoded)) = reader.next_batch() {
                        assert_eq!(decoded, batch, "cut {cut}");
                    }
                }
                Err(IpcError::EmptyStream) => assert_eq!(cut, 0, "only an empty input is empty"),
                Err(err) => assert!(err.is_malformed(), "cut {cut}: {err}"),
            }
        }
    }

    #[test]
    fn a_batch_before_the_schema_is_rejected() {
        let batch = sample_batch(1);
        let bytes = to_ipc_bytes(std::slice::from_ref(&batch)).expect("write");
        // Skip the schema message: the first message is then a RecordBatch.
        let mut reader = IpcStreamReader::from_slice(bytes.as_slice()).expect("reader");
        let start = reader.position();
        let err = IpcStreamReader::from_slice(&bytes.as_slice()[start..]).unwrap_err();
        match err {
            IpcError::UnexpectedMessage { expected, found } => {
                assert_eq!(expected, "Schema");
                assert_eq!(found, "RecordBatch");
            }
            other => panic!("unexpected {other}"),
        }
        assert!(reader.next_batch().expect("batch").is_some());
    }

    #[test]
    fn oversized_streams_are_refused_when_slurped() {
        let batch = sample_batch(1);
        let bytes = to_ipc_bytes(std::slice::from_ref(&batch)).expect("write");
        let options = ReadOptions::new().with_max_stream_bytes(16);
        let err = IpcStreamReader::from_reader_with_options(bytes.as_slice(), options).unwrap_err();
        assert!(
            matches!(err, IpcError::TooLarge { what: "stream", .. }),
            "{err}"
        );
    }

    #[test]
    fn reading_from_an_io_source_matches_reading_from_memory() {
        let batches = vec![sample_batch(1), sample_batch(2)];
        let bytes = to_ipc_bytes(&batches).expect("write");
        let (_, from_reader) = read_ipc_stream_from(bytes.as_slice()).expect("read");
        assert_eq!(from_reader, batches);
    }

    #[test]
    fn counters_track_progress() {
        let batches = vec![sample_batch(1), sample_batch(2)];
        let bytes = to_ipc_bytes(&batches).expect("write");
        let mut reader = IpcStreamReader::from_slice(bytes.as_slice()).expect("reader");
        assert_eq!(reader.batches_read(), 0);
        assert!(!reader.is_finished());
        while reader.next_batch().expect("batch").is_some() {}
        assert_eq!(reader.batches_read(), 2);
        assert!(reader.is_finished());
        assert_eq!(reader.position(), bytes.len() - 8);
        assert!(reader.next_batch().expect("done").is_none());
    }

    #[test]
    fn options_are_configurable() {
        let options = ReadOptions::new()
            .with_max_stream_bytes(99)
            .with_limits(MessageLimits::new().with_max_body_bytes(7));
        assert_eq!(options.max_stream_bytes, 99);
        assert_eq!(options.limits.max_body_bytes, 7);
        assert_eq!(
            ReadOptions::default().max_stream_bytes,
            DEFAULT_MAX_STREAM_BYTES
        );
    }
}
