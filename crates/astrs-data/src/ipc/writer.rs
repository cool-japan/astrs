//! The Arrow IPC **streaming format** writer.
//!
//! A stream is one schema message followed by any number of record batch
//! messages and, finally, the end-of-stream marker:
//!
//! ```text
//! ┌────────────────┬─────────────────┬─────┬─────────────────┬──────────┐
//! │ Schema message │ RecordBatch #0  │ ... │ RecordBatch #n  │ EOS      │
//! └────────────────┴─────────────────┴─────┴─────────────────┴──────────┘
//! ```
//!
//! Every message is an encapsulated message ([`crate::ipc::message`]) whose
//! metadata block and body are padded to [`WriteOptions::alignment`] — 64
//! bytes by default, so that a body buffer read back out of a mapped payload
//! starts on a cache line and can be handed straight to SIMD code (blueprint
//! §6.1). The Arrow specification's minimum is 8; anything AstRS writes is
//! readable by arrow-rs, pyarrow and arrow-cpp, which all follow the explicit
//! `Buffer` offsets rather than assuming an alignment.
//!
//! # Two ways in
//!
//! ```
//! use astrs_data::array::{Float32Array, IntoArrayRef};
//! use astrs_data::ipc::{write_ipc_stream, IpcStreamWriter};
//! use astrs_data::RecordBatch;
//!
//! let batch = RecordBatch::from_payload(Float32Array::from_values([1.0, 2.0]).into_array_ref());
//!
//! // One call, one or many batches:
//! let mut bytes: Vec<u8> = Vec::new();
//! write_ipc_stream(&mut bytes, std::slice::from_ref(&batch))?;
//! assert!(bytes.len() > 8);
//!
//! // Or incrementally, which also allows a stream with no batches at all:
//! let mut sink: Vec<u8> = Vec::new();
//! let mut writer = IpcStreamWriter::try_new(&mut sink, batch.schema_ref())?;
//! writer.write(&batch)?;
//! writer.finish()?;
//! assert_eq!(sink, bytes);
//! # Ok::<(), astrs_data::ipc::IpcError>(())
//! ```

use std::io::Write;
use std::sync::Arc;

use crate::buffer::AlignedBuf;
use crate::datatype::Schema;
use crate::ipc::encode::BatchLayout;
use crate::ipc::error::{IpcError, Result};
use crate::ipc::fb::FbBuilder;
use crate::ipc::format::{DEFAULT_MESSAGE_ALIGNMENT, message_header, record_batch};
use crate::ipc::message::{
    build_message, build_schema_message, write_end_of_stream, write_message,
};
use crate::record_batch::RecordBatch;

/// How a stream is framed.
///
/// ```
/// use astrs_data::ipc::WriteOptions;
///
/// assert_eq!(WriteOptions::new().alignment(), 64);
/// // Anything not a power of two is rounded up; 8 is the specification floor.
/// assert_eq!(WriteOptions::new().with_alignment(3).alignment(), 8);
/// assert_eq!(WriteOptions::new().with_alignment(100).alignment(), 128);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct WriteOptions {
    /// The boundary message blocks and body buffers start on.
    alignment: usize,
}

impl Default for WriteOptions {
    #[inline]
    fn default() -> Self {
        Self {
            alignment: DEFAULT_MESSAGE_ALIGNMENT,
        }
    }
}

impl WriteOptions {
    /// The defaults: 64-byte alignment, metadata version V5.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the alignment, rounded up to the next power of two and never below
    /// the Arrow specification's 8-byte floor.
    ///
    /// Rounding rather than rejecting keeps the builder infallible; the
    /// effective value is always [`WriteOptions::alignment`].
    #[must_use]
    pub const fn with_alignment(mut self, alignment: usize) -> Self {
        let rounded = if alignment <= 8 {
            8
        } else {
            alignment.next_power_of_two()
        };
        self.alignment = rounded;
        self
    }

    /// The effective alignment.
    #[inline]
    #[must_use]
    pub const fn alignment(&self) -> usize {
        self.alignment
    }
}

/// Writes an Arrow IPC stream message by message.
///
/// The writer owns one [`FbBuilder`], reset and reused for every message, so
/// after the first batch the per-message metadata encoding allocates nothing.
/// Body buffers are streamed straight out of the arrays: no intermediate copy
/// of the payload is ever made.
///
/// Dropping a writer without calling [`IpcStreamWriter::finish`] leaves a
/// stream without its end-of-stream marker. That is still readable — a clean
/// end of input is a valid end of stream — but a truncated *message* is not,
/// so finish explicitly whenever the sink outlives the writer.
#[derive(Debug)]
pub struct IpcStreamWriter<W: Write> {
    sink: W,
    schema: Arc<Schema>,
    options: WriteOptions,
    builder: FbBuilder,
    written: usize,
    batches: usize,
    finished: bool,
}

impl<W: Write> IpcStreamWriter<W> {
    /// Starts a stream, writing the schema message immediately.
    ///
    /// # Errors
    ///
    /// [`IpcError::Io`] when the sink fails, or [`IpcError::NestingTooDeep`]
    /// when a field nests past [`crate::ipc::layout::MAX_NESTING_DEPTH`].
    pub fn try_new(sink: W, schema: Arc<Schema>) -> Result<Self> {
        Self::try_new_with_options(sink, schema, WriteOptions::new())
    }

    /// [`IpcStreamWriter::try_new`] with explicit framing options.
    ///
    /// # Errors
    ///
    /// As [`IpcStreamWriter::try_new`].
    pub fn try_new_with_options(
        mut sink: W,
        schema: Arc<Schema>,
        options: WriteOptions,
    ) -> Result<Self> {
        let mut builder = FbBuilder::new();
        build_schema_message(&mut builder, &schema)?;
        let written = {
            let metadata = builder.finished_bytes();
            write_message(&mut sink, metadata, options.alignment)?
        };
        Ok(Self {
            sink,
            schema,
            options,
            builder,
            written,
            batches: 0,
            finished: false,
        })
    }

    /// The schema every batch must match.
    #[inline]
    #[must_use]
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }

    /// Bytes written so far, including the schema message.
    #[inline]
    #[must_use]
    pub const fn bytes_written(&self) -> usize {
        self.written
    }

    /// Record batches written so far.
    #[inline]
    #[must_use]
    pub const fn batches_written(&self) -> usize {
        self.batches
    }

    /// The framing options in force.
    #[inline]
    #[must_use]
    pub const fn options(&self) -> WriteOptions {
        self.options
    }

    /// Appends one record batch, returning the bytes it took.
    ///
    /// # Errors
    ///
    /// * [`IpcError::SchemaMismatch`] when the batch's schema differs from the
    ///   one the stream opened with.
    /// * [`IpcError::Io`] when the sink fails.
    /// * [`IpcError::Data`] when a column's concrete array type disagrees with
    ///   its declared [`crate::DataType`].
    pub fn write(&mut self, batch: &RecordBatch) -> Result<usize> {
        if !schemas_match(&self.schema, batch.schema()) {
            return Err(IpcError::SchemaMismatch {
                index: self.batches,
            });
        }
        let layout = BatchLayout::plan(batch, self.options.alignment)?;
        let body_length = layout.body_length();

        self.builder.reset();
        let nodes = self.builder.create_struct_pair_vector(layout.nodes());
        let buffers = self.builder.create_struct_pair_vector(layout.buffers());
        let header = {
            let table = self.builder.start_table();
            self.builder.push_slot_i64(
                record_batch::LENGTH,
                i64::try_from(batch.num_rows()).unwrap_or(i64::MAX),
                0,
            );
            self.builder.push_slot_offset(record_batch::NODES, nodes);
            self.builder
                .push_slot_offset(record_batch::BUFFERS, buffers);
            self.builder.end_table(table)
        };
        let message = build_message(
            &mut self.builder,
            message_header::RECORD_BATCH,
            header,
            body_length,
        );
        self.builder.finish(message);

        let mut written = {
            let metadata = self.builder.finished_bytes();
            write_message(&mut self.sink, metadata, self.options.alignment)?
        };
        written += layout.write_body(&mut self.sink)?;
        self.written += written;
        self.batches += 1;
        Ok(written)
    }

    /// Appends every batch in `batches`.
    ///
    /// # Errors
    ///
    /// As [`IpcStreamWriter::write`].
    pub fn write_all(&mut self, batches: &[RecordBatch]) -> Result<usize> {
        let mut written = 0;
        for batch in batches {
            written += self.write(batch)?;
        }
        Ok(written)
    }

    /// Writes the end-of-stream marker and flushes the sink.
    ///
    /// Calling this twice is harmless: the second call is a no-op.
    ///
    /// # Errors
    ///
    /// [`IpcError::Io`] when the sink fails.
    pub fn finish(&mut self) -> Result<usize> {
        if self.finished {
            return Ok(0);
        }
        let written = write_end_of_stream(&mut self.sink)?;
        self.sink
            .flush()
            .map_err(|err| IpcError::io("flushing the stream", err))?;
        self.written += written;
        self.finished = true;
        Ok(written)
    }

    /// Gives the sink back, finishing the stream first.
    ///
    /// # Errors
    ///
    /// [`IpcError::Io`] when the sink fails.
    pub fn into_inner(mut self) -> Result<W> {
        self.finish()?;
        Ok(self.sink)
    }
}

/// Whether a batch may be written into a stream opened with `expected`.
fn schemas_match(expected: &Arc<Schema>, actual: &Arc<Schema>) -> bool {
    Arc::ptr_eq(expected, actual) || expected == actual
}

/// Writes a complete stream: schema, every batch, end-of-stream marker.
///
/// The schema is taken from the first batch, so `batches` must not be empty;
/// use [`IpcStreamWriter`] directly to write a schema-only stream.
///
/// Returns the number of bytes written.
///
/// # Errors
///
/// * [`IpcError::MissingSchema`] when `batches` is empty.
/// * [`IpcError::SchemaMismatch`] when the batches disagree.
/// * [`IpcError::Io`] when the sink fails.
///
/// ```
/// use astrs_data::array::{Int64Array, IntoArrayRef};
/// use astrs_data::ipc::{write_ipc_stream, IpcError};
/// use astrs_data::RecordBatch;
///
/// let batch = RecordBatch::from_payload(Int64Array::from_values([7]).into_array_ref());
/// let mut bytes: Vec<u8> = Vec::new();
/// let written = write_ipc_stream(&mut bytes, &[batch])?;
/// assert_eq!(written, bytes.len());
///
/// let empty: Vec<RecordBatch> = Vec::new();
/// assert!(matches!(
///     write_ipc_stream(&mut Vec::new(), &empty),
///     Err(IpcError::MissingSchema)
/// ));
/// # Ok::<(), astrs_data::ipc::IpcError>(())
/// ```
pub fn write_ipc_stream<W: Write>(sink: W, batches: &[RecordBatch]) -> Result<usize> {
    let schema = batches
        .first()
        .map(RecordBatch::schema_ref)
        .ok_or(IpcError::MissingSchema)?;
    write_ipc_stream_with(sink, schema, batches, WriteOptions::new())
}

/// [`write_ipc_stream`] with an explicit schema and framing options.
///
/// An empty `batches` slice is legal here: the result is a schema-only stream,
/// which is exactly what a producer that has not yet published data sends.
///
/// # Errors
///
/// As [`write_ipc_stream`], minus [`IpcError::MissingSchema`].
pub fn write_ipc_stream_with<W: Write>(
    sink: W,
    schema: Arc<Schema>,
    batches: &[RecordBatch],
    options: WriteOptions,
) -> Result<usize> {
    let mut writer = IpcStreamWriter::try_new_with_options(sink, schema, options)?;
    writer.write_all(batches)?;
    writer.finish()?;
    Ok(writer.bytes_written())
}

/// Writes a stream into a fresh 64-byte-aligned buffer.
///
/// The convenience form of [`write_ipc_stream`] for callers that want bytes
/// rather than a sink — and the one to prefer over `Vec<u8>`, because the
/// result is [`crate::ALIGNMENT`]-aligned, which is what makes the decoder's
/// zero-copy path usable (see [`crate::ipc::IpcStreamReader`]).
///
/// # Errors
///
/// As [`write_ipc_stream`].
///
/// ```
/// use astrs_data::array::{UInt8Array, IntoArrayRef};
/// use astrs_data::ipc::to_ipc_bytes;
/// use astrs_data::{RecordBatch, ALIGNMENT};
///
/// let batch = RecordBatch::from_payload(UInt8Array::from_values([1, 2, 3]).into_array_ref());
/// let bytes = to_ipc_bytes(&[batch])?;
/// assert_eq!(bytes.as_ptr() as usize % ALIGNMENT, 0);
/// # Ok::<(), astrs_data::ipc::IpcError>(())
/// ```
pub fn to_ipc_bytes(batches: &[RecordBatch]) -> Result<AlignedBuf> {
    let schema = batches
        .first()
        .map(RecordBatch::schema_ref)
        .ok_or(IpcError::MissingSchema)?;
    to_ipc_bytes_with(schema, batches, WriteOptions::new())
}

/// [`to_ipc_bytes`] with an explicit schema and framing options.
///
/// # Errors
///
/// As [`write_ipc_stream_with`].
pub fn to_ipc_bytes_with(
    schema: Arc<Schema>,
    batches: &[RecordBatch],
    options: WriteOptions,
) -> Result<AlignedBuf> {
    let mut buffer = AlignedBuf::with_capacity(estimated_len(batches, options));
    {
        let mut sink = AlignedSink(&mut buffer);
        write_ipc_stream_with(&mut sink, schema, batches, options)?;
    }
    Ok(buffer)
}

/// A rough size for the output buffer, so the common case never reallocates.
fn estimated_len(batches: &[RecordBatch], options: WriteOptions) -> usize {
    let payload: usize = batches
        .iter()
        .map(|batch| batch.buffer_memory_size() + 4 * options.alignment)
        .sum();
    payload + 1024
}

/// An [`std::io::Write`] sink that appends to an [`AlignedBuf`].
///
/// Keeps the 64-byte alignment guarantee that `Vec<u8>` cannot make.
pub(crate) struct AlignedSink<'a>(pub(crate) &'a mut AlignedBuf);

impl Write for AlignedSink<'_> {
    #[inline]
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    #[inline]
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.0.extend_from_slice(bytes);
        Ok(())
    }

    #[inline]
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{Int32Array, IntoArrayRef, StringArray};
    use crate::datatype::{DataType, Field};
    use crate::ipc::format::CONTINUATION_MARKER;
    use crate::ipc::message::{MessageLimits, decode_message, scan_message};

    fn sample_batch() -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("i", DataType::Int32, true),
            Field::new("s", DataType::Utf8, true),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Int32Array::from_opt_iter([Some(1), None, Some(3)]).into_array_ref(),
                StringArray::from_opt_iter([Some("aa"), Some("bbb"), None]).into_array_ref(),
            ],
        )
        .expect("batch")
    }

    fn messages(stream: &[u8]) -> Vec<(u8, usize, usize)> {
        let limits = MessageLimits::default();
        let mut out = Vec::new();
        let mut pos = 0usize;
        while let Some(message) = scan_message(stream, pos, &limits).expect("scan") {
            let info = decode_message(&stream[message.metadata.clone()]).expect("decode");
            out.push((info.header_type, message.metadata.len(), message.body.len()));
            pos = message.next;
        }
        out
    }

    #[test]
    fn a_stream_is_schema_then_batches_then_the_marker() {
        let batch = sample_batch();
        let mut stream = Vec::new();
        let written = write_ipc_stream(&mut stream, &[batch.clone(), batch]).expect("write");
        assert_eq!(written, stream.len());

        let seen = messages(&stream);
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].0, message_header::SCHEMA);
        assert_eq!(seen[1].0, message_header::RECORD_BATCH);
        assert_eq!(seen[2].0, message_header::RECORD_BATCH);

        let tail = &stream[stream.len() - 8..];
        assert_eq!(tail[..4], CONTINUATION_MARKER.to_le_bytes());
        assert_eq!(tail[4..], 0i32.to_le_bytes());
    }

    #[test]
    fn every_message_starts_on_the_alignment() {
        for alignment in [8usize, 16, 32, 64, 256] {
            let batch = sample_batch();
            let mut stream = Vec::new();
            write_ipc_stream_with(
                &mut stream,
                batch.schema_ref(),
                std::slice::from_ref(&batch),
                WriteOptions::new().with_alignment(alignment),
            )
            .expect("write");

            let limits = MessageLimits::default();
            let mut pos = 0usize;
            while let Some(message) = scan_message(&stream, pos, &limits).expect("scan") {
                assert_eq!(
                    pos % alignment,
                    0,
                    "message at {pos}, alignment {alignment}"
                );
                assert_eq!(message.body.start % alignment, 0);
                pos = message.next;
            }
            assert_eq!(pos % alignment, 0);
        }
    }

    #[test]
    fn a_schema_only_stream_is_valid() {
        let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Bool, true)]));
        let mut stream = Vec::new();
        write_ipc_stream_with(&mut stream, schema, &[], WriteOptions::new()).expect("write");
        let seen = messages(&stream);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, message_header::SCHEMA);
        assert_eq!(seen[0].2, 0, "a schema message has no body");
    }

    #[test]
    fn writing_without_batches_needs_a_schema() {
        let err = write_ipc_stream(&mut Vec::new(), &[]).unwrap_err();
        assert!(matches!(err, IpcError::MissingSchema), "{err}");
        let err = to_ipc_bytes(&[]).unwrap_err();
        assert!(matches!(err, IpcError::MissingSchema), "{err}");
    }

    #[test]
    fn a_batch_with_a_different_schema_is_rejected() {
        let first = sample_batch();
        let other = RecordBatch::from_payload(Int32Array::from_values([1]).into_array_ref());
        let mut stream = Vec::new();
        let err = write_ipc_stream(&mut stream, &[first, other]).unwrap_err();
        assert!(
            matches!(err, IpcError::SchemaMismatch { index: 1 }),
            "{err}"
        );
    }

    #[test]
    fn the_writer_reports_its_own_counters() {
        let batch = sample_batch();
        let mut sink = Vec::new();
        let mut writer = IpcStreamWriter::try_new(&mut sink, batch.schema_ref()).expect("writer");
        assert_eq!(writer.batches_written(), 0);
        assert!(writer.bytes_written() > 0);
        assert_eq!(writer.options().alignment(), 64);
        assert_eq!(writer.schema().len(), 2);
        let batch_bytes = writer.write(&batch).expect("write");
        assert!(batch_bytes > 0);
        assert_eq!(writer.batches_written(), 1);
        let end = writer.finish().expect("finish");
        assert_eq!(end, 8);
        assert_eq!(writer.finish().expect("idempotent"), 0);
        assert_eq!(writer.bytes_written(), sink.len());
    }

    #[test]
    fn into_inner_finishes_the_stream() {
        let batch = sample_batch();
        let writer = IpcStreamWriter::try_new(Vec::new(), batch.schema_ref()).expect("writer");
        let sink = writer.into_inner().expect("inner");
        assert_eq!(
            &sink[sink.len() - 8..sink.len() - 4],
            &CONTINUATION_MARKER.to_le_bytes()
        );
    }

    #[test]
    fn aligned_output_matches_the_sink_output() {
        let batch = sample_batch();
        let mut stream = Vec::new();
        write_ipc_stream(&mut stream, std::slice::from_ref(&batch)).expect("write");
        let aligned = to_ipc_bytes(std::slice::from_ref(&batch)).expect("aligned");
        assert_eq!(aligned.as_slice(), stream.as_slice());
        assert_eq!(aligned.as_ptr() as usize % crate::ALIGNMENT, 0);
    }

    #[test]
    fn alignment_is_rounded_to_a_power_of_two() {
        assert_eq!(WriteOptions::new().with_alignment(0).alignment(), 8);
        assert_eq!(WriteOptions::new().with_alignment(1).alignment(), 8);
        assert_eq!(WriteOptions::new().with_alignment(8).alignment(), 8);
        assert_eq!(WriteOptions::new().with_alignment(9).alignment(), 16);
        assert_eq!(WriteOptions::new().with_alignment(64).alignment(), 64);
        assert_eq!(WriteOptions::new().with_alignment(129).alignment(), 256);
        assert_eq!(WriteOptions::default(), WriteOptions::new());
    }

    #[test]
    fn io_failures_surface_as_io_errors() {
        struct Failing(usize);
        impl Write for Failing {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.0 == 0 {
                    return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "full"));
                }
                self.0 = self.0.saturating_sub(bytes.len());
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let batch = sample_batch();
        let err = write_ipc_stream(Failing(0), std::slice::from_ref(&batch)).unwrap_err();
        assert!(matches!(err, IpcError::Io { .. }), "{err}");
    }

    #[test]
    fn body_lengths_are_declared_exactly() {
        let batch = sample_batch();
        let mut stream = Vec::new();
        write_ipc_stream(&mut stream, std::slice::from_ref(&batch)).expect("write");
        let limits = MessageLimits::default();
        let schema_message = scan_message(&stream, 0, &limits)
            .expect("scan")
            .expect("msg");
        let batch_message = scan_message(&stream, schema_message.next, &limits)
            .expect("scan")
            .expect("msg");
        let info = decode_message(&stream[batch_message.metadata.clone()]).expect("decode");
        assert_eq!(
            usize::try_from(info.body_length).expect("body"),
            batch_message.body.len()
        );
        assert_eq!(batch_message.body.len() % 64, 0);
    }
}
