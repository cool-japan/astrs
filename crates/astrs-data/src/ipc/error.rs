//! [`IpcError`] — everything that can go wrong encoding or decoding an Arrow
//! IPC stream.
//!
//! The IPC layer keeps its own error type rather than extending
//! [`crate::DataError`]: a malformed *stream* and an invalid
//! *array* are different failures with different recovery paths, and the
//! decoder wants to name the exact flatbuffer field it choked on. Array
//! construction errors surfacing from the columnar core are wrapped in
//! [`IpcError::Data`].
//!
//! Every decode-side variant carries enough context to locate the problem in
//! a hex dump: a byte position, a table and field name, or the offending
//! numeric code.
//!
//! ```
//! use astrs_data::ipc::{IpcError, IpcStreamReader};
//!
//! // A stream that stops in the middle of the metadata length prefix.
//! let err = IpcStreamReader::from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0x10]).unwrap_err();
//! assert!(matches!(err, IpcError::UnexpectedEndOfStream { .. }));
//! ```

use std::fmt;

use crate::error::DataError;

/// The result type of the IPC layer.
pub type Result<T, E = IpcError> = std::result::Result<T, E>;

/// A failure encoding or decoding an Arrow IPC stream.
///
/// `#[non_exhaustive]`: the append-only evolution rule (blueprint §3.4)
/// applies to error enums too.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum IpcError {
    /// The underlying reader or writer failed.
    #[error("io error while {context}: {source}")]
    Io {
        /// What the layer was doing when the I/O failed.
        context: &'static str,
        /// The originating error.
        #[source]
        source: std::io::Error,
    },

    /// An array could not be rebuilt from its decoded buffers.
    #[error(transparent)]
    Data(#[from] DataError),

    /// The stream ended in the middle of a message.
    #[error(
        "stream ended after {available} byte(s) while {expected} more were required ({context})"
    )]
    UnexpectedEndOfStream {
        /// What the decoder was reading.
        context: &'static str,
        /// Bytes still available.
        available: usize,
        /// Bytes the decoder needed.
        expected: usize,
    },

    /// The 4-byte word introducing a message was neither the continuation
    /// marker nor a plausible legacy length.
    #[error("invalid message prefix {marker:#010x} at byte {position}")]
    InvalidContinuation {
        /// The word that was read.
        marker: u32,
        /// Byte offset of the word within the stream.
        position: usize,
    },

    /// The encapsulated-message header declared a negative metadata length.
    #[error("negative metadata length {length} at byte {position}")]
    NegativeMetadataLength {
        /// The declared length.
        length: i32,
        /// Byte offset of the length prefix.
        position: usize,
    },

    /// A declared length exceeded the configured cap.
    #[error("{what} of {} exceeds the {} cap", ByteCount(*length), ByteCount(*cap))]
    TooLarge {
        /// Which length was too large ("metadata", "body", "payload").
        what: &'static str,
        /// The declared length.
        length: u64,
        /// The cap that was exceeded.
        cap: u64,
    },

    /// The flatbuffer metadata is structurally invalid.
    #[error("malformed flatbuffer at byte {position}: {context}")]
    MalformedFlatbuffer {
        /// What the reader was trying to do.
        context: &'static str,
        /// Byte offset within the metadata block.
        position: usize,
    },

    /// A required flatbuffer field was absent.
    #[error("missing required field {field} of table {table}")]
    MissingField {
        /// The flatbuffer table name, as in `Schema.fbs`/`Message.fbs`.
        table: &'static str,
        /// The absent field name.
        field: &'static str,
    },

    /// `Message.version` named a metadata version this decoder does not read.
    #[error("unsupported Arrow metadata version {version} (V4 and V5 are supported)")]
    UnsupportedMetadataVersion {
        /// The raw enum value.
        version: i16,
    },

    /// `Schema.endianness` was big-endian.
    #[error("unsupported endianness {endianness} (only little-endian streams are read)")]
    UnsupportedEndianness {
        /// The raw enum value.
        endianness: i16,
    },

    /// `Message.header_type` named a message kind AstRS does not implement.
    #[error("unsupported message header {header} ({name})")]
    UnsupportedMessageHeader {
        /// The raw union discriminant.
        header: u8,
        /// A human-readable name for the discriminant.
        name: &'static str,
    },

    /// A message of the wrong kind turned up where another was required.
    #[error("expected a {expected} message, found {found}")]
    UnexpectedMessage {
        /// The message the decoder needed.
        expected: &'static str,
        /// The message it found.
        found: &'static str,
    },

    /// The stream carried no schema message.
    #[error("stream is empty: no schema message")]
    EmptyStream,

    /// `Field.type` named a type outside the closed P0 set (blueprint §6.1).
    #[error("unsupported Arrow type {code} ({name}) for field {field:?}")]
    UnsupportedType {
        /// The raw `Type` union discriminant.
        code: u8,
        /// A human-readable name for the discriminant.
        name: &'static str,
        /// The field that carried it.
        field: String,
    },

    /// A supported type family carried an unsupported parameter — a 96-bit
    /// integer, a microsecond timestamp, a time zone.
    #[error("unsupported {family} parameters for field {field:?}: {detail}")]
    UnsupportedTypeParameters {
        /// The type family (`Int`, `Timestamp`, ...).
        family: &'static str,
        /// The field that carried it.
        field: String,
        /// What exactly is unsupported.
        detail: String,
    },

    /// A nested type declared the wrong number of children.
    #[error(
        "field {field:?} of type {family} declares {actual} child field(s), {expected} expected"
    )]
    ChildCountMismatch {
        /// The field name.
        field: String,
        /// The type family.
        family: &'static str,
        /// Children the type requires.
        expected: usize,
        /// Children the message declared.
        actual: usize,
    },

    /// The record batch declared fewer field nodes or buffers than the schema
    /// needs, or left some unconsumed.
    #[error("record batch declares {actual} {what}, the schema needs {expected}")]
    LayoutCountMismatch {
        /// `"field node(s)"` or `"buffer(s)"`.
        what: &'static str,
        /// What the schema implies.
        expected: usize,
        /// What the message declared.
        actual: usize,
    },

    /// A `Buffer` entry pointed outside the message body.
    #[error("buffer {index} at ({offset}, {length}) leaves the {body_len}-byte body")]
    BufferOutOfBounds {
        /// Index within the `buffers` vector.
        index: usize,
        /// Declared body-relative offset.
        offset: i64,
        /// Declared length.
        length: i64,
        /// Actual body length.
        body_len: usize,
    },

    /// A buffer was shorter than the field node's row count requires.
    #[error(
        "buffer {index} ({role}) holds {actual} byte(s), {required} required for {rows} row(s)"
    )]
    BufferTooShort {
        /// Index within the `buffers` vector.
        index: usize,
        /// `"validity"`, `"offsets"` or `"values"`.
        role: &'static str,
        /// Bytes the buffer holds.
        actual: usize,
        /// Bytes the layout requires.
        required: usize,
        /// Row count from the field node.
        rows: usize,
    },

    /// A `FieldNode` declared a negative or absurd length.
    #[error("field node {index} declares length {length} and null count {null_count}")]
    InvalidFieldNode {
        /// Index within the `nodes` vector.
        index: usize,
        /// Declared length.
        length: i64,
        /// Declared null count.
        null_count: i64,
    },

    /// Columns of a record batch disagreed with `RecordBatch.length`.
    #[error("record batch declares {declared} row(s) but column {column} holds {actual}")]
    RowCountMismatch {
        /// `RecordBatch.length`.
        declared: usize,
        /// The offending column index.
        column: usize,
        /// The column's own length.
        actual: usize,
    },

    /// The body carried a compression codec; AstRS negotiates compression at
    /// the route level instead (blueprint §6.4).
    #[error("body compression (codec {codec}) is not supported in AstRS payloads")]
    UnsupportedCompression {
        /// The raw `CompressionType` value.
        codec: i8,
    },

    /// A dictionary-encoded field or a `DictionaryBatch` message turned up.
    #[error("dictionary encoding is not part of the AstRS 0.1.0 type set")]
    UnsupportedDictionary,

    /// [`decode_payload`](crate::ipc::decode_payload) requires exactly one
    /// record batch.
    #[error("payload holds {count} record batch(es), exactly one is required")]
    NotASinglePayload {
        /// Batches found in the stream.
        count: usize,
    },

    /// A type nested deeper than the codec's recursion limit.
    ///
    /// Both codecs recurse, so an unbounded nesting depth is a stack overflow
    /// waiting for a hostile stream; the limit is
    /// [`MAX_NESTING_DEPTH`](crate::ipc::layout::MAX_NESTING_DEPTH).
    #[error("type nests {depth} level(s) deep, the limit is {limit}")]
    NestingTooDeep {
        /// The depth reached.
        depth: usize,
        /// The configured limit.
        limit: usize,
    },

    /// [`write_ipc_stream`](crate::ipc::write_ipc_stream) was handed no
    /// batches, so it has no schema to open the stream with.
    #[error("cannot write a stream without a schema: no record batches were given")]
    MissingSchema,

    /// A batch's schema differs from the one the stream opened with.
    ///
    /// The streaming format has exactly one schema message, so every batch
    /// must match it. Use one stream per schema, or cast the batch first.
    #[error("record batch {index} does not match the schema the stream opened with")]
    SchemaMismatch {
        /// Position of the offending batch within the stream.
        index: usize,
    },
}

impl IpcError {
    /// Wraps an I/O error with the operation that produced it.
    #[must_use]
    pub fn io(context: &'static str, source: std::io::Error) -> Self {
        Self::Io { context, source }
    }

    /// Builds an [`IpcError::MalformedFlatbuffer`].
    #[must_use]
    pub const fn malformed(context: &'static str, position: usize) -> Self {
        Self::MalformedFlatbuffer { context, position }
    }

    /// Builds an [`IpcError::MissingField`].
    #[must_use]
    pub const fn missing(table: &'static str, field: &'static str) -> Self {
        Self::MissingField { table, field }
    }

    /// Returns `true` when the error means "the input bytes are not a valid
    /// Arrow IPC stream", as opposed to an I/O failure or an unsupported but
    /// well-formed feature.
    ///
    /// Route supervisors use this to decide between retrying a leg and
    /// declaring a producer broken.
    ///
    /// ```
    /// use astrs_data::ipc::IpcError;
    ///
    /// let malformed = IpcError::malformed("reading a vtable", 12);
    /// assert!(malformed.is_malformed());
    /// assert!(!IpcError::UnsupportedDictionary.is_malformed());
    /// ```
    #[must_use]
    pub const fn is_malformed(&self) -> bool {
        matches!(
            self,
            Self::UnexpectedEndOfStream { .. }
                | Self::InvalidContinuation { .. }
                | Self::NegativeMetadataLength { .. }
                | Self::MalformedFlatbuffer { .. }
                | Self::MissingField { .. }
                | Self::LayoutCountMismatch { .. }
                | Self::BufferOutOfBounds { .. }
                | Self::BufferTooShort { .. }
                | Self::InvalidFieldNode { .. }
                | Self::RowCountMismatch { .. }
                | Self::ChildCountMismatch { .. }
                | Self::NestingTooDeep { .. }
        )
    }

    /// Returns `true` when the stream is well-formed Arrow that carries a
    /// feature outside the AstRS 0.1.0 set (blueprint §6.1).
    ///
    /// ```
    /// use astrs_data::ipc::IpcError;
    ///
    /// assert!(IpcError::UnsupportedDictionary.is_unsupported());
    /// ```
    #[must_use]
    pub const fn is_unsupported(&self) -> bool {
        matches!(
            self,
            Self::UnsupportedMetadataVersion { .. }
                | Self::UnsupportedEndianness { .. }
                | Self::UnsupportedMessageHeader { .. }
                | Self::UnsupportedType { .. }
                | Self::UnsupportedTypeParameters { .. }
                | Self::UnsupportedCompression { .. }
                | Self::UnsupportedDictionary
        )
    }
}

/// Formats a byte count with a unit, for the size-cap error messages.
pub(crate) struct ByteCount(pub u64);

impl fmt::Display for ByteCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
        let mut value = self.0 as f64;
        let mut unit = 0usize;
        while value >= 1024.0 && unit + 1 < UNITS.len() {
            value /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            write!(f, "{} {}", self.0, UNITS[unit])
        } else {
            write!(f, "{value:.1} {}", UNITS[unit])
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn io_errors_carry_context() {
        let err = IpcError::io(
            "writing the schema message",
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"),
        );
        let text = err.to_string();
        assert!(text.contains("writing the schema message"), "{text}");
        assert!(text.contains("gone"), "{text}");
    }

    #[test]
    fn malformed_classification_is_exclusive() {
        let malformed = [
            IpcError::malformed("x", 0),
            IpcError::missing("Message", "header"),
            IpcError::UnexpectedEndOfStream {
                context: "metadata",
                available: 1,
                expected: 8,
            },
        ];
        for err in malformed {
            assert!(err.is_malformed(), "{err}");
            assert!(!err.is_unsupported(), "{err}");
        }
        let unsupported = [
            IpcError::UnsupportedDictionary,
            IpcError::UnsupportedMetadataVersion { version: 1 },
            IpcError::UnsupportedCompression { codec: 0 },
        ];
        for err in unsupported {
            assert!(err.is_unsupported(), "{err}");
            assert!(!err.is_malformed(), "{err}");
        }
    }

    #[test]
    fn data_errors_convert() {
        let err: IpcError = DataError::UnknownStructLength.into();
        assert!(matches!(err, IpcError::Data(_)));
        assert!(!err.is_malformed());
    }

    #[test]
    fn byte_count_formats_units() {
        assert_eq!(ByteCount(512).to_string(), "512 B");
        assert_eq!(ByteCount(2048).to_string(), "2.0 KiB");
        assert_eq!(ByteCount(256 * 1024 * 1024).to_string(), "256.0 MiB");
    }

    #[test]
    fn messages_name_the_offending_value() {
        let err = IpcError::UnsupportedType {
            code: 17,
            name: "Map",
            field: "labels".into(),
        };
        let text = err.to_string();
        assert!(text.contains("Map"), "{text}");
        assert!(text.contains("labels"), "{text}");

        let err = IpcError::BufferOutOfBounds {
            index: 3,
            offset: 64,
            length: 4096,
            body_len: 128,
        };
        assert!(err.to_string().contains("128-byte body"));
    }
}
