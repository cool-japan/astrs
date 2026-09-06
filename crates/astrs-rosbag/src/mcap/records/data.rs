//! The record types that can appear directly in an mcap Data section:
//! `Header` (0x01), `Footer` (0x02), `Schema` (0x03), `Channel` (0x04),
//! `Message` (0x05), `Chunk` (0x06), `DataEnd` (0x0F).
//!
//! Every `read` here takes an already-length-bounded
//! [`Cursor`] — [`crate::mcap::reader`]
//! is what turns a file offset into that bounded slice in the first
//! place, checked against the real file size before any of these methods
//! ever runs.

use std::collections::BTreeMap;

use crate::error::RosbagError;
use crate::mcap::primitives::Cursor;

/// `Header` (op=0x01) — spec table:
///
/// | Bytes | Name | Type |
/// |---|---|---|
/// | 4+N | profile | String |
/// | 4+N | library | String |
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Header {
    /// If non-empty and one of the [well-known
    /// profiles](https://mcap.dev/spec/registry#well-known-profiles)
    /// (`"ros1"`, `"ros2"`), the file should conform to it — in
    /// particular a `"ros2"` profile's channels use `message_encoding =
    /// "cdr"` and carry `offered_qos_profiles` in their metadata (see
    /// [`Channel`]).
    pub profile: String,
    /// Free-form writer name/version string.
    pub library: String,
}

impl Header {
    /// Reads a `Header` record body.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`]/[`RosbagError::Malformed`] as
    /// [`Cursor::read_string`].
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self, RosbagError> {
        Ok(Self {
            profile: cursor.read_string()?,
            library: cursor.read_string()?,
        })
    }
}

/// `Footer` (op=0x02) — spec table:
///
/// | Bytes | Name | Type |
/// |---|---|---|
/// | 8 | summary_start | uint64 |
/// | 8 | summary_offset_start | uint64 |
/// | 4 | summary_crc | uint32 |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Footer {
    /// Byte offset of the first summary-section record, or 0 if there is
    /// no summary section.
    pub summary_start: u64,
    /// Byte offset of the first `SummaryOffset` record, or 0 if there are
    /// none.
    pub summary_offset_start: u64,
    /// CRC-32 of the summary section through the end of
    /// `summary_offset_start`; 0 means "not available".
    pub summary_crc: u32,
}

impl Footer {
    /// Reads a `Footer` record body.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`] if the body is short.
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self, RosbagError> {
        Ok(Self {
            summary_start: cursor.read_u64()?,
            summary_offset_start: cursor.read_u64()?,
            summary_crc: cursor.read_u32()?,
        })
    }
}

/// `Schema` (op=0x03) — spec table:
///
/// | Bytes | Name | Type |
/// |---|---|---|
/// | 2 | id | uint16 (must not be zero) |
/// | 4+N | name | String |
/// | 4+N | encoding | String |
/// | 4+N | data | uint32 length-prefixed Bytes |
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Schema {
    /// Unique within the file; must not be zero (a zero-id `Schema`
    /// record is invalid and this crate, per the spec, ignores it rather
    /// than erroring — see [`crate::mcap::reader`]).
    pub id: u16,
    /// An identifier for the schema (e.g.
    /// `sensor_msgs/msg/LaserScan` for `encoding: "ros2msg"`).
    pub name: String,
    /// The schema language, e.g. `"ros2msg"`, `"ros2idl"`, `"protobuf"`.
    /// Empty means "no schema available", in which case `data` must be
    /// empty too.
    pub encoding: String,
    /// The schema's own encoded content, per `encoding`.
    pub data: Vec<u8>,
}

impl Schema {
    /// Reads a `Schema` record body.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`]/[`RosbagError::Malformed`] as
    /// [`Cursor::read_string`]/[`Cursor::read_bytes_u32`].
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self, RosbagError> {
        Ok(Self {
            id: cursor.read_u16()?,
            name: cursor.read_string()?,
            encoding: cursor.read_string()?,
            data: cursor.read_bytes_u32()?,
        })
    }
}

/// `Channel` (op=0x04) — spec table:
///
/// | Bytes | Name | Type |
/// |---|---|---|
/// | 2 | id | uint16 |
/// | 2 | schema_id | uint16 (0 = no schema) |
/// | 4+N | topic | String |
/// | 4+N | message_encoding | String |
/// | 4+N | metadata | `Map<string, string>` |
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Channel {
    /// Unique within the file.
    pub id: u16,
    /// The [`Schema`] this channel's messages conform to; `0` means "no
    /// schema".
    pub schema_id: u16,
    /// The ROS/pub-sub topic name, e.g. `/scan`.
    pub topic: String,
    /// The message encoding, e.g. `"cdr"` for ROS 2 (mcap registry's
    /// `ros2` profile), `"protobuf"`, `"json"`.
    pub message_encoding: String,
    /// Free-form per-channel metadata. The `"ros2"` profile stores
    /// `offered_qos_profiles` (a YAML-formatted sequence of
    /// [`crate::QosProfile`] blocks) here.
    pub metadata: BTreeMap<String, String>,
}

impl Channel {
    /// Reads a `Channel` record body.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`]/[`RosbagError::Malformed`] as
    /// [`Cursor::read_string`]/[`Cursor::read_map_str_str`].
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self, RosbagError> {
        Ok(Self {
            id: cursor.read_u16()?,
            schema_id: cursor.read_u16()?,
            topic: cursor.read_string()?,
            message_encoding: cursor.read_string()?,
            metadata: cursor.read_map_str_str()?,
        })
    }
}

/// `Message` (op=0x05) — spec table:
///
/// | Bytes | Name | Type |
/// |---|---|---|
/// | 2 | channel_id | uint16 |
/// | 4 | sequence | uint32 |
/// | 8 | log_time | Timestamp |
/// | 8 | publish_time | Timestamp |
/// | N | data | Bytes (rest of the record — no length prefix of its own) |
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Message {
    /// The [`Channel`] this message was published on.
    pub channel_id: u16,
    /// Optional gap-detection counter; `0` if unused.
    pub sequence: u32,
    /// When the message was recorded, nanoseconds since a
    /// writer-chosen epoch.
    pub log_time: u64,
    /// When the message was published; equals `log_time` if the
    /// publish time was not available.
    pub publish_time: u64,
    /// The serialized message, per its channel's `message_encoding` —
    /// carried opaquely: this crate never decodes it (blueprint §10.6).
    pub data: Vec<u8>,
}

impl Message {
    /// Reads a `Message` record body — `data` is whatever remains after
    /// the four fixed fields (the record's own outer length already
    /// bounds it; there is no separate length prefix for `data` itself).
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`] if the fixed prefix is short.
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self, RosbagError> {
        let channel_id = cursor.read_u16()?;
        let sequence = cursor.read_u32()?;
        let log_time = cursor.read_u64()?;
        let publish_time = cursor.read_u64()?;
        let data = cursor.read_remaining().to_vec();
        Ok(Self {
            channel_id,
            sequence,
            log_time,
            publish_time,
            data,
        })
    }
}

/// `Chunk` (op=0x06) — spec table:
///
/// | Bytes | Name | Type |
/// |---|---|---|
/// | 8 | message_start_time | Timestamp |
/// | 8 | message_end_time | Timestamp |
/// | 8 | uncompressed_size | uint64 |
/// | 4 | uncompressed_crc | uint32 |
/// | 4+N | compression | String (`""`, `"lz4"`, or `"zstd"`) |
/// | 8+N | records | uint64 length-prefixed Bytes |
///
/// `records` is returned exactly as it sat on disk — still compressed, if
/// `compression` is non-empty. Decompressing it (with the safety ceiling
/// and nested-chunk rejection that requires) is
/// [`crate::mcap::reader`]'s job, not this type's: a record struct should
/// be able to describe what is on disk without also owning the policy for
/// how much of it is safe to inflate.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Chunk {
    /// Earliest message `log_time` inside the chunk; `0` if empty.
    pub message_start_time: u64,
    /// Latest message `log_time` inside the chunk; `0` if empty.
    pub message_end_time: u64,
    /// The decompressed size of `records`, in bytes.
    pub uncompressed_size: u64,
    /// CRC-32 of the *decompressed* `records` bytes; `0` means "not
    /// available".
    pub uncompressed_crc: u32,
    /// `""` (no compression), `"lz4"`, or `"zstd"` — the mcap registry's
    /// well-known compression identifiers, the only ones this crate reads
    /// (blueprint §10.6, §19.1's `oxiarc-lz4`/`oxiarc-zstd`).
    pub compression: String,
    /// The (possibly compressed) inner records, as a single opaque byte
    /// string.
    pub records: Vec<u8>,
}

impl Chunk {
    /// Reads a `Chunk` record body — everything except decompressing
    /// `records`.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`]/[`RosbagError::Malformed`] as
    /// [`Cursor::read_string`]/[`Cursor::read_bytes_u64`].
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self, RosbagError> {
        Ok(Self {
            message_start_time: cursor.read_u64()?,
            message_end_time: cursor.read_u64()?,
            uncompressed_size: cursor.read_u64()?,
            uncompressed_crc: cursor.read_u32()?,
            compression: cursor.read_string()?,
            records: cursor.read_bytes_u64()?,
        })
    }
}

/// `DataEnd` (op=0x0F) — spec table:
///
/// | Bytes | Name | Type |
/// |---|---|---|
/// | 4 | data_section_crc | uint32 |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DataEnd {
    /// CRC-32 of every byte from the start of the file through this
    /// record; `0` means "not available". This crate does not verify it
    /// (the spec makes it optional, and mismatched CRCs on an otherwise
    /// well-formed file are a warning-shaped concern, not this type's).
    pub data_section_crc: u32,
}

impl DataEnd {
    /// Reads a `DataEnd` record body.
    ///
    /// # Errors
    ///
    /// [`RosbagError::Truncated`] if the body is short.
    pub fn read(cursor: &mut Cursor<'_>) -> Result<Self, RosbagError> {
        Ok(Self {
            data_section_crc: cursor.read_u32()?,
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

    /// Byte-exact against the spec table: `profile="ros2"`,
    /// `library="astrs"`.
    #[test]
    fn header_parses_the_spec_byte_layout() {
        let mut bytes = prefixed_string("ros2");
        bytes.extend(prefixed_string("astrs"));
        let header = Header::read(&mut cursor(&bytes)).unwrap();
        assert_eq!(header.profile, "ros2");
        assert_eq!(header.library, "astrs");
    }

    /// Byte-exact against the spec table: three fixed-width fields, no
    /// padding, in `summary_start, summary_offset_start, summary_crc`
    /// order.
    #[test]
    fn footer_parses_the_spec_byte_layout() {
        let mut bytes = 100u64.to_le_bytes().to_vec();
        bytes.extend(200u64.to_le_bytes());
        bytes.extend(0xdead_beefu32.to_le_bytes());
        let footer = Footer::read(&mut cursor(&bytes)).unwrap();
        assert_eq!(footer.summary_start, 100);
        assert_eq!(footer.summary_offset_start, 200);
        assert_eq!(footer.summary_crc, 0xdead_beef);
    }

    /// Byte-exact against the spec table: `id: uint16`, then two Strings,
    /// then a uint32-length-prefixed `data` — the field this crate must
    /// not confuse with the *other* records' uint64-prefixed `Bytes`.
    #[test]
    fn schema_parses_the_spec_byte_layout_with_a_u32_prefixed_data_field() {
        let mut bytes = 7u16.to_le_bytes().to_vec();
        bytes.extend(prefixed_string("sensor_msgs/msg/LaserScan"));
        bytes.extend(prefixed_string("ros2msg"));
        let mut data_field = 3u32.to_le_bytes().to_vec();
        data_field.extend_from_slice(&[0xaa, 0xbb, 0xcc]);
        bytes.extend(data_field);
        let schema = Schema::read(&mut cursor(&bytes)).unwrap();
        assert_eq!(schema.id, 7);
        assert_eq!(schema.name, "sensor_msgs/msg/LaserScan");
        assert_eq!(schema.encoding, "ros2msg");
        assert_eq!(schema.data, vec![0xaa, 0xbb, 0xcc]);
    }

    #[test]
    fn schema_id_zero_is_readable_even_though_the_spec_calls_it_invalid() {
        // Reading must not special-case this — a reader that panics or
        // errors on the exact input the spec says to *ignore* would be
        // worse than one that hands it up for the caller to skip.
        let mut bytes = 0u16.to_le_bytes().to_vec();
        bytes.extend(prefixed_string(""));
        bytes.extend(prefixed_string(""));
        bytes.extend(0u32.to_le_bytes());
        let schema = Schema::read(&mut cursor(&bytes)).unwrap();
        assert_eq!(schema.id, 0);
    }

    /// Byte-exact against the spec table: two uint16s, two Strings, then
    /// a `Map<string,string>` (its own `uint32` total-byte-length prefix,
    /// distinct from a plain string's length prefix).
    #[test]
    fn channel_parses_the_spec_byte_layout_including_its_metadata_map() {
        let mut bytes = 3u16.to_le_bytes().to_vec();
        bytes.extend(7u16.to_le_bytes());
        bytes.extend(prefixed_string("/scan"));
        bytes.extend(prefixed_string("cdr"));
        let mut map_inner = prefixed_string("offered_qos_profiles");
        map_inner.extend(prefixed_string("- history: keep_last\n  depth: 10\n"));
        let mut map_field = (map_inner.len() as u32).to_le_bytes().to_vec();
        map_field.extend(map_inner);
        bytes.extend(map_field);

        let channel = Channel::read(&mut cursor(&bytes)).unwrap();
        assert_eq!(channel.id, 3);
        assert_eq!(channel.schema_id, 7);
        assert_eq!(channel.topic, "/scan");
        assert_eq!(channel.message_encoding, "cdr");
        assert_eq!(
            channel
                .metadata
                .get("offered_qos_profiles")
                .map(String::as_str),
            Some("- history: keep_last\n  depth: 10\n")
        );
    }

    /// Byte-exact against the spec table: `channel_id: uint16`,
    /// `sequence: uint32`, two `Timestamp`s (`uint64`), then `data` with
    /// **no length prefix of its own** — `size-eos`, the record's own
    /// outer framing is the only bound.
    #[test]
    fn message_parses_the_spec_byte_layout_with_size_eos_data() {
        let mut bytes = 3u16.to_le_bytes().to_vec();
        bytes.extend(42u32.to_le_bytes());
        bytes.extend(1_000u64.to_le_bytes());
        bytes.extend(1_001u64.to_le_bytes());
        bytes.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);

        let message = Message::read(&mut cursor(&bytes)).unwrap();
        assert_eq!(message.channel_id, 3);
        assert_eq!(message.sequence, 42);
        assert_eq!(message.log_time, 1_000);
        assert_eq!(message.publish_time, 1_001);
        assert_eq!(message.data, vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn message_data_may_be_empty() {
        let mut bytes = 0u16.to_le_bytes().to_vec();
        bytes.extend(0u32.to_le_bytes());
        bytes.extend(0u64.to_le_bytes());
        bytes.extend(0u64.to_le_bytes());
        let message = Message::read(&mut cursor(&bytes)).unwrap();
        assert!(message.data.is_empty());
    }

    /// Byte-exact against the spec table: three `uint64`/`uint32` fixed
    /// fields, a `compression` String, then `records` as a
    /// **uint64**-length-prefixed `Bytes` — the field this crate must not
    /// confuse with `Schema::data`'s uint32 prefix.
    #[test]
    fn chunk_parses_the_spec_byte_layout_with_a_u64_prefixed_records_field() {
        let mut bytes = 10u64.to_le_bytes().to_vec();
        bytes.extend(20u64.to_le_bytes());
        bytes.extend(6u64.to_le_bytes());
        bytes.extend(0u32.to_le_bytes());
        bytes.extend(prefixed_string(""));
        let mut records_field = 6u64.to_le_bytes().to_vec();
        records_field.extend_from_slice(b"inner!");
        bytes.extend(records_field);

        let chunk = Chunk::read(&mut cursor(&bytes)).unwrap();
        assert_eq!(chunk.message_start_time, 10);
        assert_eq!(chunk.message_end_time, 20);
        assert_eq!(chunk.uncompressed_size, 6);
        assert_eq!(chunk.compression, "");
        assert_eq!(chunk.records, b"inner!");
    }

    /// Byte-exact against the spec table: one `uint32`.
    #[test]
    fn data_end_parses_the_spec_byte_layout() {
        let bytes = 0x1234_5678u32.to_le_bytes();
        let data_end = DataEnd::read(&mut cursor(&bytes)).unwrap();
        assert_eq!(data_end.data_section_crc, 0x1234_5678);
    }

    #[test]
    fn every_record_type_rejects_a_truncated_body_without_panicking() {
        macro_rules! assert_truncates {
            ($ty:ty, $full:expr) => {
                let full: Vec<u8> = $full;
                for cut in 0..full.len() {
                    let result = <$ty>::read(&mut cursor(&full[..cut]));
                    assert!(
                        matches!(result, Err(RosbagError::Truncated { .. })),
                        "{} at cut {cut} of {}: {:?}",
                        stringify!($ty),
                        full.len(),
                        result
                    );
                }
            };
        }
        assert_truncates!(Footer, {
            let mut b = 1u64.to_le_bytes().to_vec();
            b.extend(2u64.to_le_bytes());
            b.extend(3u32.to_le_bytes());
            b
        });
        assert_truncates!(DataEnd, 7u32.to_le_bytes().to_vec());
    }
}
