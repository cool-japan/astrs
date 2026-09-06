//! `.mcap` — read-only (blueprint §10.6): chunked and unchunked layouts,
//! lz4/zstd chunk decompression, and index-assisted (chunk index/message
//! index) random access when the file carries a summary section.
//!
//! No writer exists in this build — `.arec -> .mcap` is not a supported
//! [`crate::convert`] direction (see
//! [`crate::error::RosbagError::McapWriteUnsupported`]).
//!
//! [`records`] holds every record type's exact byte layout (verified
//! against the official specification — see that module's docs for
//! provenance); [`primitives`] is the bounds-checked byte-level reading
//! every record type is built from; [`Reader`] is the file-level API:
//! open, walk the summary section (or fall back to a full linear scan for
//! an unindexed file), decompress chunks under a safety ceiling, and
//! iterate messages.

pub mod primitives;
pub mod reader;
pub mod records;

pub use reader::{MAGIC, MAX_CHUNK_UNCOMPRESSED_BYTES, Reader};
