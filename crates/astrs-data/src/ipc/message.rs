//! Encapsulated message framing — the envelope every Arrow IPC message
//! travels in, and the `Message` table inside it.
//!
//! # The envelope
//!
//! ```text
//! ┌──────────────┬──────────────────┬───────────────────────┬──────────┐
//! │ 0xFFFFFFFF   │ metadata_length  │ Message flatbuffer    │ body     │
//! │ (4 bytes)    │ (int32, 4 bytes) │ + padding             │          │
//! └──────────────┴──────────────────┴───────────────────────┴──────────┘
//! ```
//!
//! * The **continuation marker** `0xFFFFFFFF` introduces every message in the
//!   current (post-0.15) encoding. Streams written by pre-0.15 producers omit
//!   it and start directly with `metadata_length`; AstRS reads both and always
//!   writes the marker.
//! * `metadata_length` **includes** the padding that follows the flatbuffer,
//!   chosen so the body starts on an alignment boundary.
//! * The body is `Message.bodyLength` bytes long, itself padded so the next
//!   message starts aligned again.
//! * A `metadata_length` of `0` is the **end-of-stream** marker. A stream may
//!   also simply end after its last message (arrow-rs writes no marker in the
//!   legacy format), so a clean EOF at a message boundary is equally valid.
//!
//! AstRS writes 64-byte alignment by default ([`crate::ipc::format::DEFAULT_MESSAGE_ALIGNMENT`])
//! rather than the 8-byte minimum, so that every body buffer lands on a cache
//! line and a mapped payload is directly usable as a SIMD source (blueprint
//! §6.1). Readers of the format must not assume any particular alignment, and
//! this one does not: it follows the explicit `Buffer` offsets.
//!
//! Scanning never trusts the length prefix on its own: it reads
//! `Message.bodyLength` out of the metadata block, so a message is located
//! only if its header really is a readable `Message` table.
//!
//! ```
//! use astrs_data::ipc::message::{
//!     decode_message, scan_message, write_end_of_stream, write_schema_message, MessageLimits,
//! };
//! use astrs_data::{DataType, Field, Schema};
//!
//! let schema = Schema::new(vec![Field::new("data", DataType::Int32, true)]);
//! let mut stream: Vec<u8> = Vec::new();
//! let written = write_schema_message(&mut stream, &schema, 64)?;
//! write_end_of_stream(&mut stream)?;
//! assert_eq!(written % 64, 0, "the body would start on the alignment boundary");
//!
//! let message = scan_message(&stream, 0, &MessageLimits::default())?.expect("a message");
//! assert!(message.continued, "AstRS always writes the continuation marker");
//! assert_eq!(message.body.len(), 0, "a schema message carries no body");
//!
//! let header = decode_message(&stream[message.metadata.clone()])?;
//! assert_eq!(header.body_length, 0);
//! // The end-of-stream marker reports "no more messages".
//! assert!(scan_message(&stream, message.next, &MessageLimits::default())?.is_none());
//! # Ok::<(), astrs_data::ipc::IpcError>(())
//! ```

use std::io::Write;
use std::ops::Range;

use crate::datatype::Schema;
use crate::ipc::error::{IpcError, Result};
use crate::ipc::fb::{FbBuilder, Table, WipOffset, read_i32, read_u32, root_table};
use crate::ipc::format::{CONTINUATION_MARKER, MetadataVersion, message, message_header};
use crate::ipc::schema::encode_schema;

/// Size of the continuation marker plus the metadata length prefix.
pub const PREFIX_LEN: usize = 8;

/// Size of the metadata length prefix on its own (the pre-0.15 encoding).
pub const LEGACY_PREFIX_LEN: usize = 4;

/// Caps a decoder applies to a message before it allocates or indexes
/// anything.
///
/// The defaults are the blueprint's 256 MB payload ceiling
/// ([`crate::MAX_PAYLOAD_BYTES`]) for the body and a far smaller one for
/// metadata: an Arrow header describing a legal schema is kilobytes, so a
/// 64 MiB header is a corrupt or hostile stream whatever it claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct MessageLimits {
    /// Largest accepted `metadata_length`.
    pub max_metadata_bytes: usize,
    /// Largest accepted `Message.bodyLength`.
    pub max_body_bytes: usize,
}

impl Default for MessageLimits {
    #[inline]
    fn default() -> Self {
        Self {
            max_metadata_bytes: 64 * 1024 * 1024,
            max_body_bytes: crate::MAX_PAYLOAD_BYTES,
        }
    }
}

impl MessageLimits {
    /// The defaults, spelled out.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the body cap.
    #[inline]
    #[must_use]
    pub const fn with_max_body_bytes(mut self, bytes: usize) -> Self {
        self.max_body_bytes = bytes;
        self
    }

    /// Sets the metadata cap.
    #[inline]
    #[must_use]
    pub const fn with_max_metadata_bytes(mut self, bytes: usize) -> Self {
        self.max_metadata_bytes = bytes;
        self
    }
}

/// Where one encapsulated message lives inside a stream.
///
/// Byte ranges rather than borrowed slices, so the reader can hand out
/// zero-copy [`crate::Buffer`] windows over the same backing allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawMessage {
    /// The flatbuffer metadata block, padding included.
    pub metadata: Range<usize>,
    /// The message body.
    pub body: Range<usize>,
    /// Offset of the next message.
    pub next: usize,
    /// Whether the message carried the continuation marker.
    pub continued: bool,
}

/// The decoded `Message` table header.
#[derive(Debug, Clone, Copy)]
pub struct MessageInfo<'a> {
    /// `Message.version`.
    pub version: MetadataVersion,
    /// `Message.header_type`, the union discriminant.
    pub header_type: u8,
    /// The header table itself (a `Schema` or a `RecordBatch`).
    pub header: Table<'a>,
    /// `Message.bodyLength`.
    pub body_length: i64,
}

/// Locates the message starting at `pos`.
///
/// Returns `Ok(None)` at the end of the stream — either the explicit
/// end-of-stream marker or a clean EOF at a message boundary.
///
/// # Errors
///
/// * [`IpcError::UnexpectedEndOfStream`] when the stream stops mid-message.
/// * [`IpcError::InvalidContinuation`] when the introducing word is neither
///   the marker nor a plausible legacy length.
/// * [`IpcError::NegativeMetadataLength`] for a negative length prefix.
/// * [`IpcError::TooLarge`] when a declared length exceeds `limits`.
pub fn scan_message(
    bytes: &[u8],
    pos: usize,
    limits: &MessageLimits,
) -> Result<Option<RawMessage>> {
    let available = bytes.len().saturating_sub(pos);
    if available == 0 {
        // A stream that simply ends at a message boundary: legal, and what
        // the pre-0.15 encoding produces (it writes no end-of-stream marker).
        return Ok(None);
    }
    if available < LEGACY_PREFIX_LEN {
        return Err(IpcError::UnexpectedEndOfStream {
            context: "message length prefix",
            available,
            expected: LEGACY_PREFIX_LEN,
        });
    }

    let first = read_u32(bytes, pos, "message prefix")?;
    let (metadata_start, raw_len, continued) = if first == CONTINUATION_MARKER {
        if available < PREFIX_LEN {
            return Err(IpcError::UnexpectedEndOfStream {
                context: "metadata length after the continuation marker",
                available,
                expected: PREFIX_LEN,
            });
        }
        (
            pos + PREFIX_LEN,
            read_i32(bytes, pos + LEGACY_PREFIX_LEN, "metadata length")?,
            true,
        )
    } else {
        // Pre-0.15 encoding: the first word *is* the length. Anything with the
        // high bit set would be a negative length, which is never valid, so it
        // is reported as a bad prefix rather than a bad length.
        if first > i32::MAX as u32 {
            return Err(IpcError::InvalidContinuation {
                marker: first,
                position: pos,
            });
        }
        (pos + LEGACY_PREFIX_LEN, first as i32, false)
    };

    if raw_len == 0 {
        // The end-of-stream marker.
        return Ok(None);
    }
    if raw_len < 0 {
        return Err(IpcError::NegativeMetadataLength {
            length: raw_len,
            position: pos,
        });
    }
    let metadata_len = raw_len as usize;
    if metadata_len > limits.max_metadata_bytes {
        return Err(IpcError::TooLarge {
            what: "metadata",
            length: metadata_len as u64,
            cap: limits.max_metadata_bytes as u64,
        });
    }
    let metadata_end = metadata_start
        .checked_add(metadata_len)
        .ok_or(IpcError::malformed("metadata length", pos))?;
    if metadata_end > bytes.len() {
        return Err(IpcError::UnexpectedEndOfStream {
            context: "message metadata",
            available: bytes.len().saturating_sub(metadata_start),
            expected: metadata_len,
        });
    }

    let body_length = message_body_length(&bytes[metadata_start..metadata_end])?;
    if body_length < 0 {
        return Err(IpcError::malformed("Message.bodyLength", metadata_start));
    }
    let body_len = usize::try_from(body_length)
        .map_err(|_| IpcError::malformed("Message.bodyLength", metadata_start))?;
    if body_len > limits.max_body_bytes {
        return Err(IpcError::TooLarge {
            what: "body",
            length: body_len as u64,
            cap: limits.max_body_bytes as u64,
        });
    }
    let body_end = metadata_end
        .checked_add(body_len)
        .ok_or(IpcError::malformed("Message.bodyLength", metadata_start))?;
    if body_end > bytes.len() {
        return Err(IpcError::UnexpectedEndOfStream {
            context: "message body",
            available: bytes.len().saturating_sub(metadata_end),
            expected: body_len,
        });
    }

    Ok(Some(RawMessage {
        metadata: metadata_start..metadata_end,
        body: metadata_end..body_end,
        next: body_end,
        continued,
    }))
}

/// Reads `Message.bodyLength` without decoding the rest of the header.
///
/// # Errors
///
/// [`IpcError::MalformedFlatbuffer`] when the metadata block is not a
/// readable `Message` table.
fn message_body_length(metadata: &[u8]) -> Result<i64> {
    let table = root_table(metadata)?;
    table.i64(message::BODY_LENGTH, 0)
}

/// Decodes the `Message` table in `metadata`.
///
/// # Errors
///
/// * [`IpcError::MalformedFlatbuffer`] for an unreadable table.
/// * [`IpcError::UnsupportedMetadataVersion`] for V1–V3.
/// * [`IpcError::MissingField`] when the header union is absent.
pub fn decode_message(metadata: &[u8]) -> Result<MessageInfo<'_>> {
    let table = root_table(metadata)?;
    let raw_version = table.i16(message::VERSION, 0)?;
    let version =
        MetadataVersion::from_raw(raw_version).ok_or(IpcError::UnsupportedMetadataVersion {
            version: raw_version,
        })?;
    let header_type = table.u8(message::HEADER_TYPE, message_header::NONE)?;
    let header = table
        .table(message::HEADER)?
        .ok_or_else(|| IpcError::missing("Message", "header"))?;
    let body_length = table.i64(message::BODY_LENGTH, 0)?;
    Ok(MessageInfo {
        version,
        header_type,
        header,
        body_length,
    })
}

/// Wraps `header` in a `Message` table.
///
/// `Message.version` is always written as V5: AstRS produces the current
/// metadata version, and V4 exists only on the read side.
pub fn build_message(
    builder: &mut FbBuilder,
    header_type: u8,
    header: WipOffset,
    body_length: i64,
) -> WipOffset {
    let table = builder.start_table();
    builder.push_slot_i16(message::VERSION, MetadataVersion::V5.as_raw(), 0);
    builder.push_slot_u8(message::HEADER_TYPE, header_type, message_header::NONE);
    builder.push_slot_offset(message::HEADER, header);
    builder.push_slot_i64(message::BODY_LENGTH, body_length, 0);
    builder.end_table(table)
}

/// Builds the complete `Message` flatbuffer for a schema, into `builder`.
///
/// The bytes are `builder.finished_bytes()` afterwards.
///
/// # Errors
///
/// [`IpcError::NestingTooDeep`] from the schema encoder.
pub fn build_schema_message(builder: &mut FbBuilder, schema: &Schema) -> Result<()> {
    builder.reset();
    let header = encode_schema(builder, schema)?;
    let message = build_message(builder, message_header::SCHEMA, header, 0);
    builder.finish(message);
    Ok(())
}

/// Encodes a standalone schema `Message`, envelope excluded.
///
/// # Errors
///
/// [`IpcError::NestingTooDeep`] from the schema encoder.
pub fn encode_schema_message(schema: &Schema) -> Result<Vec<u8>> {
    let mut builder = FbBuilder::new();
    build_schema_message(&mut builder, schema)?;
    Ok(builder.finished_bytes().to_vec())
}

/// Writes a schema as a complete encapsulated message.
///
/// # Errors
///
/// [`IpcError::Io`] when the sink fails, or [`IpcError::NestingTooDeep`] from
/// the schema encoder.
pub fn write_schema_message<W: Write>(
    sink: &mut W,
    schema: &Schema,
    alignment: usize,
) -> Result<usize> {
    let metadata = encode_schema_message(schema)?;
    write_message(sink, &metadata, alignment)
}

/// Writes one encapsulated message envelope: marker, length, metadata and the
/// padding that puts the body on an `alignment` boundary.
///
/// The body itself is not written here — the caller streams it straight after,
/// padded to the same alignment — and its length is already inside `metadata`
/// as `Message.bodyLength`.
///
/// Returns the number of bytes written (always a multiple of `alignment`).
///
/// # Errors
///
/// [`IpcError::Io`] when the sink fails.
pub fn write_message<W: Write>(sink: &mut W, metadata: &[u8], alignment: usize) -> Result<usize> {
    let alignment = alignment.max(8);
    let padded = (PREFIX_LEN + metadata.len()).next_multiple_of(alignment);
    let metadata_length = padded - PREFIX_LEN;
    let padding = metadata_length - metadata.len();

    // `metadata_length` is bounded by the 64 MiB metadata cap long before it
    // could overflow an i32, but the cast is written defensively anyway.
    let declared = i32::try_from(metadata_length).map_err(|_| IpcError::TooLarge {
        what: "metadata",
        length: metadata_length as u64,
        cap: i32::MAX as u64,
    })?;

    write_all(sink, &CONTINUATION_MARKER.to_le_bytes(), "message marker")?;
    write_all(sink, &declared.to_le_bytes(), "metadata length")?;
    write_all(sink, metadata, "message metadata")?;
    write_padding(sink, padding, "metadata padding")?;
    Ok(padded)
}

/// Writes the 8-byte end-of-stream marker.
///
/// # Errors
///
/// [`IpcError::Io`] when the sink fails.
pub fn write_end_of_stream<W: Write>(sink: &mut W) -> Result<usize> {
    write_all(
        sink,
        &CONTINUATION_MARKER.to_le_bytes(),
        "stream terminator",
    )?;
    write_all(sink, &0i32.to_le_bytes(), "stream terminator")?;
    Ok(PREFIX_LEN)
}

/// `write_all` with the IPC error type.
///
/// # Errors
///
/// [`IpcError::Io`] when the sink fails.
pub fn write_all<W: Write>(sink: &mut W, bytes: &[u8], context: &'static str) -> Result<()> {
    sink.write_all(bytes)
        .map_err(|err| IpcError::io(context, err))
}

/// Writes `count` zero bytes.
///
/// # Errors
///
/// [`IpcError::Io`] when the sink fails.
pub fn write_padding<W: Write>(sink: &mut W, count: usize, context: &'static str) -> Result<()> {
    const ZEROS: [u8; 64] = [0u8; 64];
    let mut left = count;
    while left > 0 {
        let chunk = left.min(ZEROS.len());
        write_all(sink, &ZEROS[..chunk], context)?;
        left -= chunk;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::datatype::{DataType, Field};

    fn schema() -> Schema {
        Schema::new(vec![Field::new("a", DataType::Int32, true)])
    }

    #[test]
    fn envelopes_are_aligned_and_self_describing() {
        for alignment in [8usize, 16, 32, 64, 128] {
            let mut stream = Vec::new();
            let written = write_schema_message(&mut stream, &schema(), alignment).expect("write");
            assert_eq!(written, stream.len());
            assert_eq!(stream.len() % alignment, 0, "alignment {alignment}");

            let message = scan_message(&stream, 0, &MessageLimits::default())
                .expect("scan")
                .expect("message");
            assert!(message.continued);
            assert_eq!(message.body.len(), 0);
            assert_eq!(message.next, stream.len());

            let info = decode_message(&stream[message.metadata]).expect("decode");
            assert_eq!(info.version, MetadataVersion::V5);
            assert_eq!(info.header_type, message_header::SCHEMA);
            assert_eq!(info.body_length, 0);
        }
    }

    #[test]
    fn end_of_stream_is_reported_as_none() {
        let mut stream = Vec::new();
        write_schema_message(&mut stream, &schema(), 8).expect("write");
        let end = stream.len();
        write_end_of_stream(&mut stream).expect("eos");
        assert_eq!(stream.len(), end + 8);

        let limits = MessageLimits::default();
        let first = scan_message(&stream, 0, &limits)
            .expect("scan")
            .expect("msg");
        assert!(
            scan_message(&stream, first.next, &limits)
                .expect("scan")
                .is_none()
        );
    }

    #[test]
    fn a_clean_eof_is_the_end_of_the_stream() {
        let mut stream = Vec::new();
        write_schema_message(&mut stream, &schema(), 8).expect("write");
        let limits = MessageLimits::default();
        let first = scan_message(&stream, 0, &limits)
            .expect("scan")
            .expect("msg");
        assert_eq!(first.next, stream.len());
        assert!(
            scan_message(&stream, stream.len(), &limits)
                .expect("scan")
                .is_none(),
            "no terminator is still a valid end of stream"
        );
    }

    #[test]
    fn legacy_framing_is_accepted() {
        // Same message, but with the 4-byte prefix the pre-0.15 encoding uses.
        let metadata = encode_schema_message(&schema()).expect("encode");
        let padded = (LEGACY_PREFIX_LEN + metadata.len()).next_multiple_of(8);
        let metadata_length = padded - LEGACY_PREFIX_LEN;
        let mut stream = Vec::new();
        stream.extend_from_slice(&(metadata_length as i32).to_le_bytes());
        stream.extend_from_slice(&metadata);
        stream.resize(padded, 0);

        let message = scan_message(&stream, 0, &MessageLimits::default())
            .expect("scan")
            .expect("message");
        assert!(!message.continued);
        let info = decode_message(&stream[message.metadata]).expect("decode");
        assert_eq!(info.header_type, message_header::SCHEMA);
    }

    #[test]
    fn truncated_prefixes_and_bodies_are_rejected() {
        let mut stream = Vec::new();
        write_schema_message(&mut stream, &schema(), 8).expect("write");
        let limits = MessageLimits::default();
        for cut in 1..stream.len() {
            match scan_message(&stream[..cut], 0, &limits) {
                Ok(Some(_)) => panic!("a truncated stream must not yield a message at {cut}"),
                Ok(None) => panic!("a truncated stream is not an empty one at {cut}"),
                Err(err) => assert!(err.is_malformed(), "{err}"),
            }
        }
    }

    #[test]
    fn negative_and_oversized_lengths_are_rejected() {
        let limits = MessageLimits::default();
        let mut stream = Vec::new();
        stream.extend_from_slice(&CONTINUATION_MARKER.to_le_bytes());
        stream.extend_from_slice(&(-4i32).to_le_bytes());
        stream.resize(64, 0);
        let err = scan_message(&stream, 0, &limits).unwrap_err();
        assert!(
            matches!(err, IpcError::NegativeMetadataLength { length: -4, .. }),
            "{err}"
        );

        let mut stream = Vec::new();
        stream.extend_from_slice(&CONTINUATION_MARKER.to_le_bytes());
        stream.extend_from_slice(&1_000_000i32.to_le_bytes());
        stream.resize(64, 0);
        let tight = MessageLimits::default().with_max_metadata_bytes(1024);
        let err = scan_message(&stream, 0, &tight).unwrap_err();
        assert!(
            matches!(
                err,
                IpcError::TooLarge {
                    what: "metadata",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn an_absurd_legacy_prefix_is_rejected() {
        let mut stream = vec![0u8; 32];
        stream[..4].copy_from_slice(&0xFFFF_FFFEu32.to_le_bytes());
        let err = scan_message(&stream, 0, &MessageLimits::default()).unwrap_err();
        assert!(matches!(err, IpcError::InvalidContinuation { .. }), "{err}");
    }

    #[test]
    fn oversized_bodies_are_rejected_before_indexing() {
        // A well-formed schema message whose body length claims 4 GiB.
        let mut builder = FbBuilder::new();
        builder.reset();
        let header = encode_schema(&mut builder, &schema()).expect("schema");
        let message = build_message(&mut builder, message_header::SCHEMA, header, 1 << 32);
        builder.finish(message);
        let metadata = builder.finished_bytes().to_vec();

        let mut stream = Vec::new();
        write_message(&mut stream, &metadata, 8).expect("write");
        let err = scan_message(&stream, 0, &MessageLimits::default()).unwrap_err();
        assert!(
            matches!(err, IpcError::TooLarge { what: "body", .. }),
            "{err}"
        );
    }

    #[test]
    fn unsupported_metadata_versions_are_rejected() {
        let mut builder = FbBuilder::new();
        let header = encode_schema(&mut builder, &schema()).expect("schema");
        let table = builder.start_table();
        builder.push_slot_i16(message::VERSION, 1, 0);
        builder.push_slot_u8(message::HEADER_TYPE, message_header::SCHEMA, 0);
        builder.push_slot_offset(message::HEADER, header);
        let message = builder.end_table(table);
        builder.finish(message);
        let metadata = builder.finished_bytes().to_vec();

        let err = decode_message(&metadata).unwrap_err();
        assert!(
            matches!(err, IpcError::UnsupportedMetadataVersion { version: 1 }),
            "{err}"
        );
    }

    #[test]
    fn a_message_without_a_header_is_malformed() {
        let mut builder = FbBuilder::new();
        let table = builder.start_table();
        builder.push_slot_i16(message::VERSION, MetadataVersion::V5.as_raw(), 0);
        let message = builder.end_table(table);
        builder.finish(message);
        let metadata = builder.finished_bytes().to_vec();
        let err = decode_message(&metadata).unwrap_err();
        assert!(matches!(err, IpcError::MissingField { .. }), "{err}");
    }

    #[test]
    fn padding_writes_exact_counts() {
        for count in [0usize, 1, 7, 64, 65, 200] {
            let mut sink = Vec::new();
            write_padding(&mut sink, count, "test").expect("pad");
            assert_eq!(sink.len(), count);
            assert!(sink.iter().all(|b| *b == 0));
        }
    }

    #[test]
    fn limits_are_configurable() {
        let limits = MessageLimits::new()
            .with_max_body_bytes(7)
            .with_max_metadata_bytes(9);
        assert_eq!(limits.max_body_bytes, 7);
        assert_eq!(limits.max_metadata_bytes, 9);
        assert_eq!(
            MessageLimits::default().max_body_bytes,
            crate::MAX_PAYLOAD_BYTES
        );
    }

    #[test]
    fn io_failures_are_reported_with_context() {
        struct Failing;
        impl Write for Failing {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let err = write_end_of_stream(&mut Failing).unwrap_err();
        assert!(matches!(err, IpcError::Io { .. }), "{err}");
        assert!(err.to_string().contains("stream terminator"), "{err}");
    }
}
