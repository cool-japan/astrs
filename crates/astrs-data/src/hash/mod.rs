//! Hashing: an in-crate XXH3-64 ([`xxh3`]) and the schema fingerprint built
//! on it ([`schema_hash`], re-exported at the crate root as
//! [`crate::SchemaHash`]).
//!
//! Two modules, one dependency:
//!
//! ```text
//!   schema_hash   SchemaHash · canonical_bytes · stamp   (blueprint §6.1)
//!   xxh3          xxh3_64 · xxh3_64_with_seed · Xxh3Hasher
//! ```
//!
//! `xxh3` has no `astrs-data` types in its signature at all — it is a
//! general-purpose hash that happens to live here because `SchemaHash` is
//! its only caller in this crate and the blueprint says to keep it
//! in-crate. Reach for [`crate::SchemaHash`] unless something outside a
//! schema genuinely needs a raw XXH3 digest.

pub mod schema_hash;
pub mod xxh3;

pub use crate::hash::schema_hash::{SCHEMA_HASH_METADATA_KEY, SchemaHash, canonical_bytes, stamp};
pub use crate::hash::xxh3::{Xxh3Hasher, xxh3_64, xxh3_64_with_seed};
