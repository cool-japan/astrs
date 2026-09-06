//! [`SchemaHash`] — a 64-bit fingerprint of a [`Schema`]'s columnar shape.
//!
//! Blueprint §6.1: *"`SchemaHash` (xxh3, in `astrs-data`) stamps metadata so
//! receivers can cache decoded schemas and detect type drift cheaply."* A
//! receiver that has already decoded a schema for hash `H` can skip decoding
//! the next message's schema entirely as long as its stamped hash is also
//! `H`, and knows to re-decode the moment it changes — that is the whole
//! mechanism, and it only works if the hash is a function of exactly the
//! thing that would break a cached decode: the columns.
//!
//! # Canonical form
//!
//! [`canonical_bytes`] serialises a schema's **field list only** — name,
//! nullability and a recursive [`DataType`] encoding, in field order.
//! [`Schema::metadata`] is deliberately excluded, for two reasons that are
//! really one reason: `_schema_hash` itself lives in that map (hashing it
//! would be circular), and every *other* metadata entry (a producer tag, a
//! session id, …) can legitimately vary message to message without the
//! columns changing at all — including it would make the cache thrash on
//! changes that never touch a decoder. **A cache keyed by [`SchemaHash`]
//! must not assume metadata equality**: two schemas with the same hash can
//! carry different metadata, by design.
//!
//! The wire encoding, little-endian throughout:
//!
//! ```text
//! canonical(schema)  = 0x01 ‖ u32(field_count) ‖ field*
//! field              = u32(name_len) ‖ name (UTF-8) ‖ u8(nullable) ‖ data_type
//! data_type          = u8(tag) ‖ payload
//! ```
//!
//! | Tag | Type | Payload |
//! |---:|---|---|
//! | 0 | `Null` | — |
//! | 1 | `Bool` | — |
//! | 2..=5 | `Int8`/`Int16`/`Int32`/`Int64` | — |
//! | 6..=9 | `UInt8`/`UInt16`/`UInt32`/`UInt64` | — |
//! | 10..=12 | `Float16`/`Float32`/`Float64` | — |
//! | 13 | `Binary` | — |
//! | 14 | `LargeBinary` | — |
//! | 15 | `Utf8` | — |
//! | 16 | `LargeUtf8` | — |
//! | 17 | `FixedSizeBinary` | `i32(size)` |
//! | 18 | `FixedSizeList` | `field(child)` ‖ `i32(size)` |
//! | 19 | `List` | `field(child)` |
//! | 20 | `Struct` | `u32(count)` ‖ `field*` |
//! | 21 | `Timestamp` | — |
//! | 22 | `Duration` | — |
//!
//! The leading `0x01` is a canonical-form version, bumped only if this
//! encoding itself ever changes shape — append-only, the same rule the wire
//! protocol follows (blueprint §3.4).
//!
//! ```
//! use astrs_data::{DataType, Field, Schema, SchemaHash};
//!
//! let a = Schema::new(vec![Field::required("x", DataType::Int32)]);
//! let b = Schema::new(vec![Field::required("x", DataType::Int32)])
//!     .with_metadata_entry("producer", "camera-1"); // metadata differs…
//! assert_eq!(SchemaHash::of(&a), SchemaHash::of(&b)); // …hash does not.
//!
//! let renamed = Schema::new(vec![Field::required("y", DataType::Int32)]);
//! assert_ne!(SchemaHash::of(&a), SchemaHash::of(&renamed));
//! ```

use std::fmt;

use crate::datatype::{DataType, Field, Schema};
use crate::hash::xxh3::xxh3_64;

/// Canonical-form version byte — see the [module documentation](self).
const CANONICAL_FORM_VERSION: u8 = 1;

/// The metadata key a stamped schema carries its [`SchemaHash`] under
/// (blueprint §6.1), stripped before user delivery like every `_`-prefixed
/// key.
pub const SCHEMA_HASH_METADATA_KEY: &str = "_schema_hash";

fn write_len(buf: &mut Vec<u8>, len: usize) {
    buf.extend_from_slice(&(len as u32).to_le_bytes());
}

fn write_field(buf: &mut Vec<u8>, field: &Field) {
    let name = field.name().as_bytes();
    write_len(buf, name.len());
    buf.extend_from_slice(name);
    buf.push(u8::from(field.is_nullable()));
    write_data_type(buf, field.data_type());
}

fn write_data_type(buf: &mut Vec<u8>, data_type: &DataType) {
    match data_type {
        DataType::Null => buf.push(0),
        DataType::Bool => buf.push(1),
        DataType::Int8 => buf.push(2),
        DataType::Int16 => buf.push(3),
        DataType::Int32 => buf.push(4),
        DataType::Int64 => buf.push(5),
        DataType::UInt8 => buf.push(6),
        DataType::UInt16 => buf.push(7),
        DataType::UInt32 => buf.push(8),
        DataType::UInt64 => buf.push(9),
        DataType::Float16 => buf.push(10),
        DataType::Float32 => buf.push(11),
        DataType::Float64 => buf.push(12),
        DataType::Binary => buf.push(13),
        DataType::LargeBinary => buf.push(14),
        DataType::Utf8 => buf.push(15),
        DataType::LargeUtf8 => buf.push(16),
        DataType::FixedSizeBinary(size) => {
            buf.push(17);
            buf.extend_from_slice(&size.to_le_bytes());
        }
        DataType::FixedSizeList(child, size) => {
            buf.push(18);
            write_field(buf, child);
            buf.extend_from_slice(&size.to_le_bytes());
        }
        DataType::List(child) => {
            buf.push(19);
            write_field(buf, child);
        }
        DataType::Struct(fields) => {
            buf.push(20);
            write_len(buf, fields.len());
            for field in fields {
                write_field(buf, field);
            }
        }
        DataType::Timestamp => buf.push(21),
        DataType::Duration => buf.push(22),
    }
}

/// The canonical byte encoding [`SchemaHash::of`] hashes — see the [module
/// documentation](self) for the exact layout.
///
/// Exposed so a caller can hash a schema with a different algorithm, diff
/// two schemas byte-for-byte, or write a golden-vector test against this
/// crate's own encoding without going through XXH3 at all.
///
/// ```
/// use astrs_data::hash::schema_hash::canonical_bytes;
/// use astrs_data::{DataType, Field, Schema};
///
/// let a = canonical_bytes(&Schema::new(vec![Field::required("x", DataType::Int32)]));
/// let b = canonical_bytes(&Schema::new(vec![Field::required("x", DataType::Int32)]));
/// assert_eq!(a, b);
/// ```
#[must_use]
pub fn canonical_bytes(schema: &Schema) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16 + schema.len() * 24);
    buf.push(CANONICAL_FORM_VERSION);
    write_len(&mut buf, schema.len());
    for field in schema.fields() {
        write_field(&mut buf, field);
    }
    buf
}

/// A 64-bit XXH3 fingerprint of a [`Schema`]'s field list.
///
/// See the [module documentation](self) for the canonical form it hashes and
/// — importantly — what it deliberately leaves out.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct SchemaHash(u64);

impl SchemaHash {
    /// Hashes `schema`'s canonical field-list encoding.
    ///
    /// ```
    /// use astrs_data::{DataType, Field, Schema, SchemaHash};
    ///
    /// let schema = Schema::new(vec![Field::required("x", DataType::Int32)]);
    /// assert_eq!(SchemaHash::of(&schema), SchemaHash::of(&schema));
    /// ```
    #[must_use]
    pub fn of(schema: &Schema) -> Self {
        Self(xxh3_64(&canonical_bytes(schema)))
    }

    /// Wraps a raw 64-bit value, for a hash that arrived over the wire
    /// (already parsed from its hex metadata string) rather than one this
    /// process computed.
    #[inline]
    #[must_use]
    pub const fn from_u64(value: u64) -> Self {
        Self(value)
    }

    /// The raw 64-bit value.
    #[inline]
    #[must_use]
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    /// The fixed-width lowercase hex form this crate stores in schema
    /// metadata (`_schema_hash`) and prints in [`fmt::Display`].
    ///
    /// ```
    /// use astrs_data::SchemaHash;
    ///
    /// let hash = SchemaHash::from_u64(0x1234_5678_9abc_def0);
    /// assert_eq!(hash.to_hex(), "123456789abcdef0");
    /// assert_eq!(SchemaHash::parse_hex(&hash.to_hex()), Some(hash));
    /// ```
    #[must_use]
    pub fn to_hex(&self) -> String {
        format!("{:016x}", self.0)
    }

    /// Parses the sixteen-hex-digit form [`SchemaHash::to_hex`] produces.
    ///
    /// Returns `None` for anything else — the wrong length, or a
    /// non-hex-digit byte — rather than accepting a short or padded form,
    /// so a truncated metadata value is a decode gap the caller notices
    /// instead of a hash that silently collides with a shorter one's.
    ///
    /// ```
    /// use astrs_data::SchemaHash;
    ///
    /// assert!(SchemaHash::parse_hex("00000000deadbeef").is_some());
    /// assert!(SchemaHash::parse_hex("deadbeef").is_none(), "too short");
    /// assert!(SchemaHash::parse_hex("not hex at all!!").is_none());
    /// ```
    #[must_use]
    pub fn parse_hex(text: &str) -> Option<Self> {
        if text.len() != 16 || !text.is_ascii() {
            return None;
        }
        u64::from_str_radix(text, 16).ok().map(Self)
    }
}

impl fmt::Display for SchemaHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// Returns a copy of `schema` carrying its own [`SchemaHash`] under
/// [`SCHEMA_HASH_METADATA_KEY`] (blueprint §6.1: *"stamps metadata"*).
///
/// The hash is computed **before** stamping, from the schema as given —
/// stamping a schema that is already stamped recomputes and overwrites the
/// entry rather than hashing the old one in.
///
/// ```
/// use astrs_data::hash::schema_hash::{stamp, SCHEMA_HASH_METADATA_KEY};
/// use astrs_data::{DataType, Field, Schema, SchemaHash};
///
/// let schema = Schema::new(vec![Field::required("x", DataType::Int32)]);
/// let expected = SchemaHash::of(&schema).to_hex();
/// let stamped = stamp(schema);
/// assert_eq!(stamped.metadata_value(SCHEMA_HASH_METADATA_KEY), Some(expected.as_str()));
/// ```
#[must_use]
pub fn stamp(schema: Schema) -> Schema {
    let hash = SchemaHash::of(&schema).to_hex();
    schema.with_metadata_entry(SCHEMA_HASH_METADATA_KEY, hash)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn payload_schema(data_type: DataType) -> Schema {
        Schema::new(vec![Field::required("data", data_type)])
    }

    #[test]
    fn identical_schemas_hash_identically() {
        let a = payload_schema(DataType::Int32);
        let b = payload_schema(DataType::Int32);
        assert_eq!(SchemaHash::of(&a), SchemaHash::of(&b));
        assert_eq!(canonical_bytes(&a), canonical_bytes(&b));
    }

    #[test]
    fn a_renamed_field_changes_the_hash() {
        let a = Schema::new(vec![Field::required("x", DataType::Int32)]);
        let b = Schema::new(vec![Field::required("y", DataType::Int32)]);
        assert_ne!(SchemaHash::of(&a), SchemaHash::of(&b));
    }

    #[test]
    fn a_retyped_field_changes_the_hash() {
        assert_ne!(
            SchemaHash::of(&payload_schema(DataType::Int32)),
            SchemaHash::of(&payload_schema(DataType::Int64))
        );
    }

    #[test]
    fn a_nullability_flip_changes_the_hash() {
        let required = Schema::new(vec![Field::required("x", DataType::Int32)]);
        let nullable = Schema::new(vec![Field::nullable("x", DataType::Int32)]);
        assert_ne!(SchemaHash::of(&required), SchemaHash::of(&nullable));
    }

    #[test]
    fn metadata_never_affects_the_hash() {
        let bare = payload_schema(DataType::Utf8);
        let decorated = bare
            .clone()
            .with_metadata_entry("producer", "camera-1")
            .with_metadata_entry("session_id", "abc-123");
        assert_eq!(SchemaHash::of(&bare), SchemaHash::of(&decorated));

        // Including a `_schema_hash` entry that is already there (e.g. a
        // schema decoded off the wire) still does not feed back into the
        // hash — the metadata map is excluded wholesale, not filtered key
        // by key.
        let already_stamped = stamp(bare.clone());
        assert_eq!(SchemaHash::of(&bare), SchemaHash::of(&already_stamped));
    }

    #[test]
    fn field_order_matters() {
        let ab = Schema::new(vec![
            Field::required("a", DataType::Int8),
            Field::required("b", DataType::Int8),
        ]);
        let ba = Schema::new(vec![
            Field::required("b", DataType::Int8),
            Field::required("a", DataType::Int8),
        ]);
        assert_ne!(SchemaHash::of(&ab), SchemaHash::of(&ba));
    }

    #[test]
    fn nested_layouts_are_distinguished_structurally() {
        let list = payload_schema(DataType::list(Field::nullable("item", DataType::Int32)));
        let fixed = payload_schema(DataType::fixed_size_list(
            Field::nullable("item", DataType::Int32),
            3,
        ));
        let strukt = payload_schema(DataType::strukt([Field::nullable("item", DataType::Int32)]));
        let hashes = [
            SchemaHash::of(&list),
            SchemaHash::of(&fixed),
            SchemaHash::of(&strukt),
        ];
        assert_ne!(hashes[0], hashes[1]);
        assert_ne!(hashes[1], hashes[2]);
        assert_ne!(hashes[0], hashes[2]);
    }

    #[test]
    fn a_fixed_size_list_size_change_is_visible() {
        let three = payload_schema(DataType::fixed_size_list(
            Field::nullable("item", DataType::Float32),
            3,
        ));
        let four = payload_schema(DataType::fixed_size_list(
            Field::nullable("item", DataType::Float32),
            4,
        ));
        assert_ne!(SchemaHash::of(&three), SchemaHash::of(&four));
    }

    #[test]
    fn every_closed_data_type_encodes_without_colliding() {
        let every_type = [
            DataType::Null,
            DataType::Bool,
            DataType::Int8,
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::UInt8,
            DataType::UInt16,
            DataType::UInt32,
            DataType::UInt64,
            DataType::Float16,
            DataType::Float32,
            DataType::Float64,
            DataType::Binary,
            DataType::LargeBinary,
            DataType::Utf8,
            DataType::LargeUtf8,
            DataType::FixedSizeBinary(4),
            DataType::Timestamp,
            DataType::Duration,
        ];
        let mut seen = std::collections::BTreeSet::new();
        for data_type in every_type {
            let hash = SchemaHash::of(&payload_schema(data_type));
            assert!(seen.insert(hash), "collision within the closed scalar set");
        }
    }

    #[test]
    fn hex_round_trips_and_rejects_malformed_input() {
        let hash = SchemaHash::of(&payload_schema(DataType::Float64));
        assert_eq!(SchemaHash::parse_hex(&hash.to_hex()), Some(hash));
        assert_eq!(hash.to_string(), hash.to_hex());
        assert!(SchemaHash::parse_hex("").is_none());
        assert!(SchemaHash::parse_hex("00").is_none());
        assert!(SchemaHash::parse_hex(&"0".repeat(17)).is_none());
        assert!(SchemaHash::parse_hex("zzzzzzzzzzzzzzzz").is_none());
    }

    #[test]
    fn as_u64_and_from_u64_are_inverses() {
        let hash = SchemaHash::of(&payload_schema(DataType::Bool));
        assert_eq!(SchemaHash::from_u64(hash.as_u64()), hash);
    }

    #[test]
    fn stamp_attaches_the_hex_hash_under_the_well_known_key() {
        let schema = payload_schema(DataType::Int64);
        let expected = SchemaHash::of(&schema);
        let stamped = stamp(schema);
        assert_eq!(
            stamped.metadata_value(SCHEMA_HASH_METADATA_KEY),
            Some(expected.to_hex().as_str())
        );
    }

    #[test]
    fn stamping_twice_recomputes_rather_than_compounds() {
        let schema = payload_schema(DataType::Int64);
        let once = stamp(schema.clone());
        let twice = stamp(once.clone());
        assert_eq!(
            once.metadata_value(SCHEMA_HASH_METADATA_KEY),
            twice.metadata_value(SCHEMA_HASH_METADATA_KEY)
        );
    }

    #[test]
    fn empty_schema_still_hashes() {
        let empty = Schema::default();
        assert_eq!(
            canonical_bytes(&empty).len(),
            5,
            "version byte + u32 zero count"
        );
        assert_eq!(SchemaHash::of(&empty), SchemaHash::of(&Schema::default()));
    }

    #[test]
    fn serde_round_trips_as_a_bare_integer() {
        let hash = SchemaHash::from_u64(0xdead_beef);
        let json = serde_json::to_string(&hash).unwrap();
        assert_eq!(json, hash.as_u64().to_string());
        assert_eq!(serde_json::from_str::<SchemaHash>(&json).unwrap(), hash);
    }
}
