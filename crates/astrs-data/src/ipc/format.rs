//! The Arrow IPC format constants — every enum value and vtable slot index
//! this crate encodes or decodes.
//!
//! These mirror `Message.fbs` and `Schema.fbs` from the Arrow columnar
//! specification. Each value was taken from the arrow-rs-generated golden
//! vectors committed under `tests/golden/arrow/`.
//!
//! **Unverified.** No test reads those vectors, and this module is not
//! declared in `lib.rs`, so nothing here is compiled or exercised. A wrong
//! constant would not fail any test today.
//!
//! # Slot numbering
//!
//! A FlatBuffers union occupies **two** vtable slots: the discriminant byte
//! and the value offset. That is why, in [`message`], `header_type` is slot 1
//! and `header` is slot 2 even though `Message.fbs` declares a single
//! `header: MessageHeader` field.
//!
//! ```
//! use astrs_data::ipc::format::{MetadataVersion, message_header, type_code};
//!
//! assert_eq!(MetadataVersion::V5 as i16, 4);
//! assert_eq!(message_header::RECORD_BATCH, 3);
//! assert_eq!(type_code::LARGE_UTF8, 20);
//! ```

/// `MetadataVersion` from `Schema.fbs`.
///
/// AstRS writes V5 and reads V4 and V5. V1–V3 predate the current buffer
/// layout and are rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum MetadataVersion {
    /// Arrow 0.8 and later, pre-1.0 union layout.
    V4 = 3,
    /// Arrow 1.0 and later. What AstRS writes.
    V5 = 4,
}

impl MetadataVersion {
    /// Maps the raw flatbuffer value.
    ///
    /// ```
    /// use astrs_data::ipc::format::MetadataVersion;
    ///
    /// assert_eq!(MetadataVersion::from_raw(4), Some(MetadataVersion::V5));
    /// assert_eq!(MetadataVersion::from_raw(1), None);
    /// ```
    #[must_use]
    pub const fn from_raw(value: i16) -> Option<Self> {
        match value {
            3 => Some(Self::V4),
            4 => Some(Self::V5),
            _ => None,
        }
    }

    /// The raw flatbuffer value.
    #[must_use]
    pub const fn as_raw(self) -> i16 {
        self as i16
    }
}

/// `Endianness` from `Schema.fbs`. AstRS is little-endian only.
pub mod endianness {
    /// Little-endian, the flatbuffer default and the only value AstRS writes.
    pub const LITTLE: i16 = 0;
    /// Big-endian; rejected on read.
    pub const BIG: i16 = 1;
}

/// `MessageHeader` union discriminants from `Message.fbs`.
pub mod message_header {
    /// No header — an invalid message.
    pub const NONE: u8 = 0;
    /// `Schema`.
    pub const SCHEMA: u8 = 1;
    /// `DictionaryBatch`; outside the AstRS 0.1.0 type set.
    pub const DICTIONARY_BATCH: u8 = 2;
    /// `RecordBatch`.
    pub const RECORD_BATCH: u8 = 3;
    /// `Tensor`; not part of the streaming format.
    pub const TENSOR: u8 = 4;
    /// `SparseTensor`; not part of the streaming format.
    pub const SPARSE_TENSOR: u8 = 5;

    /// A human-readable name for a discriminant, for error messages.
    ///
    /// ```
    /// use astrs_data::ipc::format::message_header;
    ///
    /// assert_eq!(message_header::name(3), "RecordBatch");
    /// assert_eq!(message_header::name(200), "unknown");
    /// ```
    #[must_use]
    pub const fn name(header: u8) -> &'static str {
        match header {
            NONE => "none",
            SCHEMA => "Schema",
            DICTIONARY_BATCH => "DictionaryBatch",
            RECORD_BATCH => "RecordBatch",
            TENSOR => "Tensor",
            SPARSE_TENSOR => "SparseTensor",
            _ => "unknown",
        }
    }
}

/// `Type` union discriminants from `Schema.fbs`.
///
/// The full Arrow list is kept — including the types AstRS does not support —
/// so [`type_code::name`] can report exactly what a foreign producer sent.
pub mod type_code {
    /// No type.
    pub const NONE: u8 = 0;
    /// `Null`.
    pub const NULL: u8 = 1;
    /// `Int`.
    pub const INT: u8 = 2;
    /// `FloatingPoint`.
    pub const FLOATING_POINT: u8 = 3;
    /// `Binary`.
    pub const BINARY: u8 = 4;
    /// `Utf8`.
    pub const UTF8: u8 = 5;
    /// `Bool`.
    pub const BOOL: u8 = 6;
    /// `Decimal`.
    pub const DECIMAL: u8 = 7;
    /// `Date`.
    pub const DATE: u8 = 8;
    /// `Time`.
    pub const TIME: u8 = 9;
    /// `Timestamp`.
    pub const TIMESTAMP: u8 = 10;
    /// `Interval`.
    pub const INTERVAL: u8 = 11;
    /// `List`.
    pub const LIST: u8 = 12;
    /// `Struct_`.
    pub const STRUCT: u8 = 13;
    /// `Union`.
    pub const UNION: u8 = 14;
    /// `FixedSizeBinary`.
    pub const FIXED_SIZE_BINARY: u8 = 15;
    /// `FixedSizeList`.
    pub const FIXED_SIZE_LIST: u8 = 16;
    /// `Map`.
    pub const MAP: u8 = 17;
    /// `Duration`.
    pub const DURATION: u8 = 18;
    /// `LargeBinary`.
    pub const LARGE_BINARY: u8 = 19;
    /// `LargeUtf8`.
    pub const LARGE_UTF8: u8 = 20;
    /// `LargeList`.
    pub const LARGE_LIST: u8 = 21;
    /// `RunEndEncoded`.
    pub const RUN_END_ENCODED: u8 = 22;
    /// `BinaryView`.
    pub const BINARY_VIEW: u8 = 23;
    /// `Utf8View`.
    pub const UTF8_VIEW: u8 = 24;
    /// `ListView`.
    pub const LIST_VIEW: u8 = 25;
    /// `LargeListView`.
    pub const LARGE_LIST_VIEW: u8 = 26;

    /// A human-readable name for a discriminant, for error messages.
    ///
    /// ```
    /// use astrs_data::ipc::format::type_code;
    ///
    /// assert_eq!(type_code::name(type_code::MAP), "Map");
    /// assert_eq!(type_code::name(99), "unknown");
    /// ```
    #[must_use]
    pub const fn name(code: u8) -> &'static str {
        match code {
            NONE => "none",
            NULL => "Null",
            INT => "Int",
            FLOATING_POINT => "FloatingPoint",
            BINARY => "Binary",
            UTF8 => "Utf8",
            BOOL => "Bool",
            DECIMAL => "Decimal",
            DATE => "Date",
            TIME => "Time",
            TIMESTAMP => "Timestamp",
            INTERVAL => "Interval",
            LIST => "List",
            STRUCT => "Struct_",
            UNION => "Union",
            FIXED_SIZE_BINARY => "FixedSizeBinary",
            FIXED_SIZE_LIST => "FixedSizeList",
            MAP => "Map",
            DURATION => "Duration",
            LARGE_BINARY => "LargeBinary",
            LARGE_UTF8 => "LargeUtf8",
            LARGE_LIST => "LargeList",
            RUN_END_ENCODED => "RunEndEncoded",
            BINARY_VIEW => "BinaryView",
            UTF8_VIEW => "Utf8View",
            LIST_VIEW => "ListView",
            LARGE_LIST_VIEW => "LargeListView",
            _ => "unknown",
        }
    }
}

/// `Precision` from `Schema.fbs`. The flatbuffer default is `HALF`, so a
/// `Float16` field may omit the slot entirely.
pub mod precision {
    /// IEEE 754 binary16.
    pub const HALF: i16 = 0;
    /// IEEE 754 binary32.
    pub const SINGLE: i16 = 1;
    /// IEEE 754 binary64.
    pub const DOUBLE: i16 = 2;
}

/// `TimeUnit` from `Schema.fbs`. AstRS pins timestamps and durations to
/// nanoseconds (blueprint §6.1).
pub mod time_unit {
    /// Seconds.
    pub const SECOND: i16 = 0;
    /// Milliseconds. The flatbuffer default for `Duration.unit`.
    pub const MILLISECOND: i16 = 1;
    /// Microseconds.
    pub const MICROSECOND: i16 = 2;
    /// Nanoseconds. The only unit AstRS accepts.
    pub const NANOSECOND: i16 = 3;

    /// A human-readable name, for error messages.
    ///
    /// ```
    /// use astrs_data::ipc::format::time_unit;
    ///
    /// assert_eq!(time_unit::name(time_unit::MICROSECOND), "microsecond");
    /// ```
    #[must_use]
    pub const fn name(unit: i16) -> &'static str {
        match unit {
            SECOND => "second",
            MILLISECOND => "millisecond",
            MICROSECOND => "microsecond",
            NANOSECOND => "nanosecond",
            _ => "unknown",
        }
    }
}

/// Vtable slots of `table Message` (`Message.fbs`).
pub mod message {
    /// `version: MetadataVersion` (short, default `V1` = 0).
    pub const VERSION: usize = 0;
    /// `header_type` — the union discriminant of `header`.
    pub const HEADER_TYPE: usize = 1;
    /// `header` — the union value.
    pub const HEADER: usize = 2;
    /// `bodyLength: long` (default 0).
    pub const BODY_LENGTH: usize = 3;
    /// `custom_metadata: [KeyValue]`.
    pub const CUSTOM_METADATA: usize = 4;
}

/// Vtable slots of `table Schema` (`Schema.fbs`).
pub mod schema {
    /// `endianness: Endianness` (short, default `Little` = 0).
    pub const ENDIANNESS: usize = 0;
    /// `fields: [Field]`.
    pub const FIELDS: usize = 1;
    /// `custom_metadata: [KeyValue]`.
    pub const CUSTOM_METADATA: usize = 2;
    /// `features: [Feature]`.
    pub const FEATURES: usize = 3;
}

/// Vtable slots of `table Field` (`Schema.fbs`).
pub mod field {
    /// `name: string`.
    pub const NAME: usize = 0;
    /// `nullable: bool` (default `false`).
    pub const NULLABLE: usize = 1;
    /// `type_type` — the union discriminant of `type`.
    pub const TYPE_TYPE: usize = 2;
    /// `type` — the union value.
    pub const TYPE: usize = 3;
    /// `dictionary: DictionaryEncoding`.
    pub const DICTIONARY: usize = 4;
    /// `children: [Field]`.
    pub const CHILDREN: usize = 5;
    /// `custom_metadata: [KeyValue]`.
    pub const CUSTOM_METADATA: usize = 6;
}

/// Vtable slots of `table RecordBatch` (`Message.fbs`).
pub mod record_batch {
    /// `length: long` (default 0) — the row count.
    pub const LENGTH: usize = 0;
    /// `nodes: [FieldNode]`.
    pub const NODES: usize = 1;
    /// `buffers: [Buffer]`.
    pub const BUFFERS: usize = 2;
    /// `compression: BodyCompression`.
    pub const COMPRESSION: usize = 3;
    /// `variadicBufferCounts: [long]`.
    pub const VARIADIC_BUFFER_COUNTS: usize = 4;
}

/// Vtable slots of `table KeyValue` (`Schema.fbs`).
pub mod key_value {
    /// `key: string`.
    pub const KEY: usize = 0;
    /// `value: string`.
    pub const VALUE: usize = 1;
}

/// Vtable slots of the `Type` tables AstRS encodes.
pub mod type_table {
    /// `Int.bitWidth: int` (default 0).
    pub const INT_BIT_WIDTH: usize = 0;
    /// `Int.is_signed: bool` (default `false`).
    pub const INT_IS_SIGNED: usize = 1;
    /// `FloatingPoint.precision: Precision` (short, default `HALF` = 0).
    pub const FLOAT_PRECISION: usize = 0;
    /// `FixedSizeBinary.byteWidth: int` (default 0).
    pub const FIXED_SIZE_BINARY_BYTE_WIDTH: usize = 0;
    /// `FixedSizeList.listSize: int` (default 0).
    pub const FIXED_SIZE_LIST_LIST_SIZE: usize = 0;
    /// `Timestamp.unit: TimeUnit` (short, default `SECOND` = 0).
    pub const TIMESTAMP_UNIT: usize = 0;
    /// `Timestamp.timezone: string`.
    pub const TIMESTAMP_TIMEZONE: usize = 1;
    /// `Duration.unit: TimeUnit` (short, default `MILLISECOND` = 1).
    pub const DURATION_UNIT: usize = 0;
}

/// Vtable slots of `table BodyCompression` (`Message.fbs`).
pub mod body_compression {
    /// `codec: CompressionType` (byte, default `LZ4_FRAME` = 0).
    pub const CODEC: usize = 0;
    /// `method: BodyCompressionMethod` (byte, default `BUFFER` = 0).
    pub const METHOD: usize = 1;
}

/// The 4-byte word that introduces every non-legacy encapsulated message.
pub const CONTINUATION_MARKER: u32 = 0xFFFF_FFFF;

/// Every encapsulated message starts on a multiple of this many bytes, and the
/// metadata block is padded so the body does too.
///
/// The Arrow specification fixes 8 as the minimum; AstRS writes 64 so that
/// body buffers land on cache lines (blueprint §6.1).
pub const MIN_MESSAGE_ALIGNMENT: usize = 8;

/// The alignment AstRS writes: message boundaries and body buffers both land
/// on 64-byte multiples, matching [`crate::ALIGNMENT`].
pub const DEFAULT_MESSAGE_ALIGNMENT: usize = 64;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn metadata_versions_round_trip() {
        for version in [MetadataVersion::V4, MetadataVersion::V5] {
            assert_eq!(MetadataVersion::from_raw(version.as_raw()), Some(version));
        }
        for raw in [i16::MIN, -1, 0, 1, 2, 5, i16::MAX] {
            assert_eq!(MetadataVersion::from_raw(raw), None, "raw {raw}");
        }
        assert!(MetadataVersion::V4 < MetadataVersion::V5);
    }

    #[test]
    fn type_codes_are_dense_and_named() {
        for code in 0..=26u8 {
            assert_ne!(type_code::name(code), "unknown", "code {code}");
        }
        assert_eq!(type_code::name(27), "unknown");
        // Spot-check the values that decide wire compatibility.
        assert_eq!(type_code::NULL, 1);
        assert_eq!(type_code::INT, 2);
        assert_eq!(type_code::FLOATING_POINT, 3);
        assert_eq!(type_code::BINARY, 4);
        assert_eq!(type_code::UTF8, 5);
        assert_eq!(type_code::BOOL, 6);
        assert_eq!(type_code::TIMESTAMP, 10);
        assert_eq!(type_code::LIST, 12);
        assert_eq!(type_code::STRUCT, 13);
        assert_eq!(type_code::FIXED_SIZE_BINARY, 15);
        assert_eq!(type_code::FIXED_SIZE_LIST, 16);
        assert_eq!(type_code::DURATION, 18);
        assert_eq!(type_code::LARGE_BINARY, 19);
        assert_eq!(type_code::LARGE_UTF8, 20);
    }

    #[test]
    fn message_headers_are_named() {
        for header in 0..=5u8 {
            assert_ne!(message_header::name(header), "unknown");
        }
        assert_eq!(message_header::name(6), "unknown");
    }

    #[test]
    fn time_units_are_named() {
        for unit in 0..=3i16 {
            assert_ne!(time_unit::name(unit), "unknown");
        }
        assert_eq!(time_unit::name(4), "unknown");
    }

    #[test]
    fn union_slots_are_adjacent() {
        assert_eq!(message::HEADER, message::HEADER_TYPE + 1);
        assert_eq!(field::TYPE, field::TYPE_TYPE + 1);
    }

    #[test]
    fn alignments_are_powers_of_two() {
        assert!(MIN_MESSAGE_ALIGNMENT.is_power_of_two());
        assert!(DEFAULT_MESSAGE_ALIGNMENT.is_power_of_two());
        const { assert!(DEFAULT_MESSAGE_ALIGNMENT >= MIN_MESSAGE_ALIGNMENT) };
        assert_eq!(DEFAULT_MESSAGE_ALIGNMENT, crate::ALIGNMENT);
    }
}
