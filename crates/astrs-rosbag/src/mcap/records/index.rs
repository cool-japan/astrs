//! The record types that appear only in an mcap's Summary/Summary Offset
//! sections: `MessageIndex` (0x07), `ChunkIndex` (0x08), `Attachment`
//! (0x09, technically Data-section but grouped here — see its own docs),
//! `AttachmentIndex` (0x0A), `Statistics` (0x0B), `Metadata` (0x0C),
//! `MetadataIndex` (0x0D), `SummaryOffset` (0x0E).

use std::collections::BTreeMap;

use crate::error::RosbagError;
use crate::mcap::primitives::Cursor;

/// One `(log_time, offset)` pair inside a [`MessageIndex`]. `offset` is
/// relative to the start of the *uncompressed* chunk data, not the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageIndexEntry {
    /// The indexed message's `log_time`.
    pub log_time: u64,
    /// Its byte offset within the chunk's decompressed `records` bytes.
    pub offset: u64,
}

/// `MessageIndex` (op=0x07) — spec table:
///
/// | Bytes | Name | Type |
/// |---|---|---|
/// | 2 | channel_id | uint16 |
/// | 4+N | records | `Array<Tuple<Timestamp, uint64>>` |
///
/// A sequence of these immediately follows each `Chunk` in an indexed
/// file: exactly one per channel that has a message inside that chunk.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MessageIndex {
    /// The channel this index covers.
    pub channel_id: u16,
    /// `(log_time, offset)` pairs, in the order the writer emitted them
    /// (the spec does not require log-time order).
    pub records: Vec<MessageIndexEntry>,
}

impl MessageIndex {
    /// Reads a `MessageIndex` record body.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`] if the body is short.
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self, RosbagError> {
        let channel_id = cursor.read_u16()?;
        let mut sub = cursor.sub_cursor()?;
        let mut records = Vec::new();
        while !sub.is_empty() {
            let log_time = sub.read_u64()?;
            let offset = sub.read_u64()?;
            records.push(MessageIndexEntry { log_time, offset });
        }
        Ok(Self {
            channel_id,
            records,
        })
    }
}

/// `ChunkIndex` (op=0x08) — spec table:
///
/// | Bytes | Name | Type |
/// |---|---|---|
/// | 8 | message_start_time | Timestamp |
/// | 8 | message_end_time | Timestamp |
/// | 8 | chunk_start_offset | uint64 |
/// | 8 | chunk_length | uint64 |
/// | 4+N | message_index_offsets | `Map<uint16, uint64>` |
/// | 8 | message_index_length | uint64 |
/// | 4+N | compression | String |
/// | 8 | compressed_size | uint64 |
/// | 8 | uncompressed_size | uint64 |
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChunkIndex {
    /// Earliest message `log_time` inside the chunk.
    pub message_start_time: u64,
    /// Latest message `log_time` inside the chunk.
    pub message_end_time: u64,
    /// The `Chunk` record's own absolute byte offset from the start of
    /// the file.
    pub chunk_start_offset: u64,
    /// The `Chunk` record's total byte length, opcode and length prefix
    /// included.
    pub chunk_length: u64,
    /// Channel id -> the absolute byte offset of that channel's
    /// `MessageIndex` record after the chunk. Empty means "no message
    /// indexing available for this chunk".
    pub message_index_offsets: BTreeMap<u16, u64>,
    /// Total byte length of every `MessageIndex` record after the chunk.
    pub message_index_length: u64,
    /// Should match the corresponding `Chunk` record's own `compression`.
    pub compression: String,
    /// The size of the `Chunk` record's `records` field on disk
    /// (compressed, if `compression` is non-empty).
    pub compressed_size: u64,
    /// Should match the corresponding `Chunk` record's own
    /// `uncompressed_size`.
    pub uncompressed_size: u64,
}

impl ChunkIndex {
    /// Reads a `ChunkIndex` record body.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`]/[`RosbagError::Malformed`] as
    /// [`Cursor::read_string`]/[`Cursor::read_map_u16_u64`].
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self, RosbagError> {
        Ok(Self {
            message_start_time: cursor.read_u64()?,
            message_end_time: cursor.read_u64()?,
            chunk_start_offset: cursor.read_u64()?,
            chunk_length: cursor.read_u64()?,
            message_index_offsets: cursor.read_map_u16_u64()?,
            message_index_length: cursor.read_u64()?,
            compression: cursor.read_string()?,
            compressed_size: cursor.read_u64()?,
            uncompressed_size: cursor.read_u64()?,
        })
    }
}

/// `Attachment` (op=0x09) — spec table:
///
/// | Bytes | Name | Type |
/// |---|---|---|
/// | 8 | log_time | Timestamp |
/// | 8 | create_time | Timestamp |
/// | 4+N | name | String |
/// | 4+N | media_type | String |
/// | 8+N | data | uint64 length-prefixed Bytes |
/// | 4 | crc | uint32 |
///
/// Lives in the Data section (never inside a `Chunk` — the spec
/// explicitly forbids that), but is grouped here with the other
/// less-central records purely for this module's own file-size split;
/// [`crate::mcap::reader`] treats it like any other Data-section record.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Attachment {
    /// When the attachment was recorded.
    pub log_time: u64,
    /// When the attachment was created; `0` if unknown.
    pub create_time: u64,
    /// The attachment's file name, e.g. `"calibration.yaml"`.
    pub name: String,
    /// A [media type](https://en.wikipedia.org/wiki/Media_type), e.g.
    /// `"text/plain"`.
    pub media_type: String,
    /// The raw attachment bytes.
    pub data: Vec<u8>,
    /// CRC-32 of every preceding field in this record; `0` means "not
    /// available". Not verified by this crate (see [`super::DataEnd`]'s
    /// own note on the same convention).
    pub crc: u32,
}

impl Attachment {
    /// Reads an `Attachment` record body.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`]/[`RosbagError::Malformed`] as
    /// [`Cursor::read_string`]/[`Cursor::read_bytes_u64`].
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self, RosbagError> {
        Ok(Self {
            log_time: cursor.read_u64()?,
            create_time: cursor.read_u64()?,
            name: cursor.read_string()?,
            media_type: cursor.read_string()?,
            data: cursor.read_bytes_u64()?,
            crc: cursor.read_u32()?,
        })
    }
}

/// `AttachmentIndex` (op=0x0A) — spec table:
///
/// | Bytes | Name | Type |
/// |---|---|---|
/// | 8 | offset | uint64 |
/// | 8 | length | uint64 |
/// | 8 | log_time | Timestamp |
/// | 8 | create_time | Timestamp |
/// | 8 | data_size | uint64 |
/// | 4+N | name | String |
/// | 4+N | media_type | String |
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AttachmentIndex {
    /// The `Attachment` record's absolute byte offset from the start of
    /// the file.
    pub offset: u64,
    /// The `Attachment` record's total byte length, opcode and length
    /// prefix included.
    pub length: u64,
    /// Should match the `Attachment` record's own `log_time`.
    pub log_time: u64,
    /// Should match the `Attachment` record's own `create_time`.
    pub create_time: u64,
    /// The size of the attachment's `data` field alone.
    pub data_size: u64,
    /// Should match the `Attachment` record's own `name`.
    pub name: String,
    /// Should match the `Attachment` record's own `media_type`.
    pub media_type: String,
}

impl AttachmentIndex {
    /// Reads an `AttachmentIndex` record body.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`]/[`RosbagError::Malformed`] as
    /// [`Cursor::read_string`].
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self, RosbagError> {
        Ok(Self {
            offset: cursor.read_u64()?,
            length: cursor.read_u64()?,
            log_time: cursor.read_u64()?,
            create_time: cursor.read_u64()?,
            data_size: cursor.read_u64()?,
            name: cursor.read_string()?,
            media_type: cursor.read_string()?,
        })
    }
}

/// `Statistics` (op=0x0B) — spec table:
///
/// | Bytes | Name | Type |
/// |---|---|---|
/// | 8 | message_count | uint64 |
/// | 2 | schema_count | uint16 |
/// | 4 | channel_count | uint32 |
/// | 4 | attachment_count | uint32 |
/// | 4 | metadata_count | uint32 |
/// | 4 | chunk_count | uint32 |
/// | 8 | message_start_time | Timestamp |
/// | 8 | message_end_time | Timestamp |
/// | 4+N | channel_message_counts | `Map<uint16, uint64>` |
///
/// At most one per file, in the summary section.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Statistics {
    /// Total `Message` records in the file (chunked and unchunked).
    pub message_count: u64,
    /// Distinct non-zero schema ids.
    pub schema_count: u16,
    /// Distinct channel ids.
    pub channel_count: u32,
    /// Total `Attachment` records.
    pub attachment_count: u32,
    /// Total `Metadata` records.
    pub metadata_count: u32,
    /// Total `Chunk` records.
    pub chunk_count: u32,
    /// Earliest message `log_time` in the file; `0` if the file has no
    /// messages.
    pub message_start_time: u64,
    /// Latest message `log_time` in the file; `0` if the file has no
    /// messages.
    pub message_end_time: u64,
    /// Channel id -> total message count on that channel. Empty means
    /// "not available"; when non-empty, the spec requires the summary
    /// section to also carry every `Channel` record.
    pub channel_message_counts: BTreeMap<u16, u64>,
}

impl Statistics {
    /// Reads a `Statistics` record body.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`]/[`RosbagError::Malformed`] as
    /// [`Cursor::read_map_u16_u64`].
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self, RosbagError> {
        Ok(Self {
            message_count: cursor.read_u64()?,
            schema_count: cursor.read_u16()?,
            channel_count: cursor.read_u32()?,
            attachment_count: cursor.read_u32()?,
            metadata_count: cursor.read_u32()?,
            chunk_count: cursor.read_u32()?,
            message_start_time: cursor.read_u64()?,
            message_end_time: cursor.read_u64()?,
            channel_message_counts: cursor.read_map_u16_u64()?,
        })
    }
}

/// `Metadata` (op=0x0C) — spec table:
///
/// | Bytes | Name | Type |
/// |---|---|---|
/// | 4+N | name | String |
/// | 4+N | metadata | `Map<string, string>` |
///
/// Arbitrary user data, e.g. hardware/calibration info — distinct from
/// [`super::Schema`] (a *message type* description) and [`Channel`
/// metadata](super::Channel::metadata) (per-topic, e.g. QoS).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Metadata {
    /// A label for this metadata record, e.g.
    /// `"my_company_hardware_info"`.
    pub name: String,
    /// Free-form key/value data.
    pub metadata: BTreeMap<String, String>,
}

impl Metadata {
    /// Reads a `Metadata` record body.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`]/[`RosbagError::Malformed`] as
    /// [`Cursor::read_string`]/[`Cursor::read_map_str_str`].
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self, RosbagError> {
        Ok(Self {
            name: cursor.read_string()?,
            metadata: cursor.read_map_str_str()?,
        })
    }
}

/// `MetadataIndex` (op=0x0D) — spec table:
///
/// | Bytes | Name | Type |
/// |---|---|---|
/// | 8 | offset | uint64 |
/// | 8 | length | uint64 |
/// | 4+N | name | String |
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MetadataIndex {
    /// The `Metadata` record's absolute byte offset from the start of the
    /// file.
    pub offset: u64,
    /// The `Metadata` record's total byte length, opcode and length
    /// prefix included.
    pub length: u64,
    /// Should match the `Metadata` record's own `name`.
    pub name: String,
}

impl MetadataIndex {
    /// Reads a `MetadataIndex` record body.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`]/[`RosbagError::Malformed`] as
    /// [`Cursor::read_string`].
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self, RosbagError> {
        Ok(Self {
            offset: cursor.read_u64()?,
            length: cursor.read_u64()?,
            name: cursor.read_string()?,
        })
    }
}

/// `SummaryOffset` (op=0x0E) — spec table:
///
/// | Bytes | Name | Type |
/// |---|---|---|
/// | 1 | group_opcode | uint8 |
/// | 8 | group_start | uint64 |
/// | 8 | group_length | uint64 |
///
/// Points at one contiguous, single-opcode run of records inside the
/// summary section (the spec requires the summary section to be grouped
/// by opcode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SummaryOffset {
    /// The opcode every record in the group shares. Kept as the raw wire
    /// byte (not [`super::Opcode`]) — an unrecognized value here just
    /// means this crate skips the group, the same forward-compatibility
    /// stance [`super::Opcode::from_u8`] documents.
    pub group_opcode: u8,
    /// The group's first record's absolute byte offset.
    pub group_start: u64,
    /// Total byte length of every record in the group.
    pub group_length: u64,
}

impl SummaryOffset {
    /// Reads a `SummaryOffset` record body.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`] if the body is short.
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self, RosbagError> {
        Ok(Self {
            group_opcode: cursor.read_u8()?,
            group_start: cursor.read_u64()?,
            group_length: cursor.read_u64()?,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::path::Path;

    fn cursor(bytes: &[u8]) -> Cursor<'_> {
        Cursor::new(bytes, 0, Path::new("test.mcap"))
    }

    fn prefixed_string(s: &str) -> Vec<u8> {
        let mut out = (s.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(s.as_bytes());
        out
    }

    /// Byte-exact against the spec table: `channel_id: uint16`, then an
    /// `Array<Tuple<uint64, uint64>>` (its own `uint32` byte-length
    /// prefix, then flat pairs — no per-element framing).
    #[test]
    fn message_index_parses_the_spec_byte_layout() {
        let mut bytes = 5u16.to_le_bytes().to_vec();
        let mut array_inner = Vec::new();
        for (log_time, offset) in [(10u64, 0u64), (20u64, 16u64)] {
            array_inner.extend(log_time.to_le_bytes());
            array_inner.extend(offset.to_le_bytes());
        }
        bytes.extend((array_inner.len() as u32).to_le_bytes());
        bytes.extend(array_inner);

        let index = MessageIndex::read(&mut cursor(&bytes)).unwrap();
        assert_eq!(index.channel_id, 5);
        assert_eq!(
            index.records,
            vec![
                MessageIndexEntry {
                    log_time: 10,
                    offset: 0
                },
                MessageIndexEntry {
                    log_time: 20,
                    offset: 16
                },
            ]
        );
    }

    #[test]
    fn message_index_may_be_empty() {
        let mut bytes = 1u16.to_le_bytes().to_vec();
        bytes.extend(0u32.to_le_bytes());
        let index = MessageIndex::read(&mut cursor(&bytes)).unwrap();
        assert!(index.records.is_empty());
    }

    /// Byte-exact against the spec table: four `uint64` fields, a
    /// `Map<uint16, uint64>`, a `uint64`, a `String`, and two more
    /// `uint64`s — the record with the most distinct field-type shapes,
    /// deliberately chosen to cross-check ordering.
    #[test]
    fn chunk_index_parses_the_spec_byte_layout() {
        let mut bytes = 1u64.to_le_bytes().to_vec(); // message_start_time
        bytes.extend(2u64.to_le_bytes()); // message_end_time
        bytes.extend(3u64.to_le_bytes()); // chunk_start_offset
        bytes.extend(4u64.to_le_bytes()); // chunk_length
        let mut map_inner = Vec::new();
        map_inner.extend(9u16.to_le_bytes());
        map_inner.extend(99u64.to_le_bytes());
        bytes.extend((map_inner.len() as u32).to_le_bytes());
        bytes.extend(map_inner); // message_index_offsets
        bytes.extend(5u64.to_le_bytes()); // message_index_length
        bytes.extend(prefixed_string("zstd")); // compression
        bytes.extend(6u64.to_le_bytes()); // compressed_size
        bytes.extend(7u64.to_le_bytes()); // uncompressed_size

        let index = ChunkIndex::read(&mut cursor(&bytes)).unwrap();
        assert_eq!(index.message_start_time, 1);
        assert_eq!(index.message_end_time, 2);
        assert_eq!(index.chunk_start_offset, 3);
        assert_eq!(index.chunk_length, 4);
        assert_eq!(index.message_index_offsets.get(&9), Some(&99));
        assert_eq!(index.message_index_length, 5);
        assert_eq!(index.compression, "zstd");
        assert_eq!(index.compressed_size, 6);
        assert_eq!(index.uncompressed_size, 7);
    }

    /// Byte-exact against the spec table: `data` is `uint64`
    /// length-prefixed (unlike `Schema::data`'s `uint32`), and `crc`
    /// trails it.
    #[test]
    fn attachment_parses_the_spec_byte_layout_with_a_u64_prefixed_data_field() {
        let mut bytes = 1_000u64.to_le_bytes().to_vec();
        bytes.extend(2_000u64.to_le_bytes());
        bytes.extend(prefixed_string("calib.yaml"));
        bytes.extend(prefixed_string("text/plain"));
        let mut data_field = 4u64.to_le_bytes().to_vec();
        data_field.extend_from_slice(&[1, 2, 3, 4]);
        bytes.extend(data_field);
        bytes.extend(0xcafeu32.to_le_bytes());

        let attachment = Attachment::read(&mut cursor(&bytes)).unwrap();
        assert_eq!(attachment.log_time, 1_000);
        assert_eq!(attachment.create_time, 2_000);
        assert_eq!(attachment.name, "calib.yaml");
        assert_eq!(attachment.media_type, "text/plain");
        assert_eq!(attachment.data, vec![1, 2, 3, 4]);
        assert_eq!(attachment.crc, 0xcafe);
    }

    #[test]
    fn attachment_index_parses_the_spec_byte_layout() {
        let mut bytes = 1u64.to_le_bytes().to_vec();
        bytes.extend(2u64.to_le_bytes());
        bytes.extend(3u64.to_le_bytes());
        bytes.extend(4u64.to_le_bytes());
        bytes.extend(5u64.to_le_bytes());
        bytes.extend(prefixed_string("calib.yaml"));
        bytes.extend(prefixed_string("text/plain"));

        let index = AttachmentIndex::read(&mut cursor(&bytes)).unwrap();
        assert_eq!(index.offset, 1);
        assert_eq!(index.length, 2);
        assert_eq!(index.log_time, 3);
        assert_eq!(index.create_time, 4);
        assert_eq!(index.data_size, 5);
        assert_eq!(index.name, "calib.yaml");
        assert_eq!(index.media_type, "text/plain");
    }

    /// Byte-exact against the spec table: `uint64, uint16, uint32,
    /// uint32, uint32, uint32, uint64, uint64` then a `Map<uint16,
    /// uint64>` — every fixed-width integer size the format defines, in
    /// one record.
    #[test]
    fn statistics_parses_the_spec_byte_layout_with_every_integer_width() {
        let mut bytes = 1_000u64.to_le_bytes().to_vec(); // message_count
        bytes.extend(3u16.to_le_bytes()); // schema_count
        bytes.extend(4u32.to_le_bytes()); // channel_count
        bytes.extend(5u32.to_le_bytes()); // attachment_count
        bytes.extend(6u32.to_le_bytes()); // metadata_count
        bytes.extend(7u32.to_le_bytes()); // chunk_count
        bytes.extend(10u64.to_le_bytes()); // message_start_time
        bytes.extend(20u64.to_le_bytes()); // message_end_time
        let mut map_inner = Vec::new();
        map_inner.extend(1u16.to_le_bytes());
        map_inner.extend(500u64.to_le_bytes());
        bytes.extend((map_inner.len() as u32).to_le_bytes());
        bytes.extend(map_inner);

        let stats = Statistics::read(&mut cursor(&bytes)).unwrap();
        assert_eq!(stats.message_count, 1_000);
        assert_eq!(stats.schema_count, 3);
        assert_eq!(stats.channel_count, 4);
        assert_eq!(stats.attachment_count, 5);
        assert_eq!(stats.metadata_count, 6);
        assert_eq!(stats.chunk_count, 7);
        assert_eq!(stats.message_start_time, 10);
        assert_eq!(stats.message_end_time, 20);
        assert_eq!(stats.channel_message_counts.get(&1), Some(&500));
    }

    #[test]
    fn metadata_parses_the_spec_byte_layout() {
        let mut bytes = prefixed_string("hardware_info");
        let mut map_inner = prefixed_string("board_revision");
        map_inner.extend(prefixed_string("rev-3"));
        bytes.extend((map_inner.len() as u32).to_le_bytes());
        bytes.extend(map_inner);

        let metadata = Metadata::read(&mut cursor(&bytes)).unwrap();
        assert_eq!(metadata.name, "hardware_info");
        assert_eq!(
            metadata.metadata.get("board_revision").map(String::as_str),
            Some("rev-3")
        );
    }

    #[test]
    fn metadata_index_parses_the_spec_byte_layout() {
        let mut bytes = 1u64.to_le_bytes().to_vec();
        bytes.extend(2u64.to_le_bytes());
        bytes.extend(prefixed_string("hardware_info"));
        let index = MetadataIndex::read(&mut cursor(&bytes)).unwrap();
        assert_eq!(index.offset, 1);
        assert_eq!(index.length, 2);
        assert_eq!(index.name, "hardware_info");
    }

    /// Byte-exact against the spec table: `uint8, uint64, uint64` — the
    /// only record with a one-byte fixed field.
    #[test]
    fn summary_offset_parses_the_spec_byte_layout() {
        let mut bytes = vec![0x05u8]; // group_opcode = Message
        bytes.extend(1_000u64.to_le_bytes());
        bytes.extend(2_000u64.to_le_bytes());
        let offset = SummaryOffset::read(&mut cursor(&bytes)).unwrap();
        assert_eq!(offset.group_opcode, 0x05);
        assert_eq!(offset.group_start, 1_000);
        assert_eq!(offset.group_length, 2_000);
    }
}
