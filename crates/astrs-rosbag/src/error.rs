//! [`RosbagError`] — everything that can go wrong reading or writing a
//! rosbag2 `.db3`/`.mcap` file, or converting to/from `.arec` (blueprint
//! §10.6).
//!
//! Two philosophies this crate mirrors from `astrs-recording`
//! ([`astrs_recording::RecordingError`]) rather than reinventing:
//!
//! - **Fatal vs. best-effort is a real distinction, not a shortcut.**
//!   A missing/corrupt `metadata.yaml` sidecar, an un-decodable
//!   `offered_qos_profiles` string, or a payload whose CDR encapsulation
//!   header does not parse are never fatal here — they degrade to a
//!   warning string in the caller's report (`db3::ReadWarnings`,
//!   [`crate::convert::ConversionReport`]) instead of aborting. Only a
//!   structurally broken container (no `messages` table, a corrupt mcap
//!   record, a file that will not open) reaches this enum.
//! - **No `.unwrap()`/`.expect()`/`panic!` on untrusted input, ever.**
//!   Every `.db3`/`.mcap` byte on disk is adversarial-input-shaped the
//!   moment it did not come from this crate's own writer.

use std::io;
use std::path::PathBuf;

/// Errors raised by [`crate::db3`], [`crate::mcap`], and [`crate::convert`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RosbagError {
    /// A filesystem operation on `path` failed.
    #[error("failed to access `{path}`: {source}")]
    Io {
        /// The path that could not be accessed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// `path` has no file extension, so [`crate::convert::convert_bag`]
    /// cannot infer a container format.
    #[error("`{path}` has no file extension; expected .arec, .db3, or .mcap")]
    UnknownExtension {
        /// The path with no extension.
        path: PathBuf,
    },

    /// `path`'s extension is not one this crate reads or writes.
    #[error(
        "`{path}` has extension `.{extension}`, which astrs-rosbag does not read or write \
         in this build (expected .arec, .db3, or .mcap)"
    )]
    UnsupportedExtension {
        /// The offending path.
        path: PathBuf,
        /// The extension that was found, lowercased.
        extension: String,
    },

    /// [`crate::convert::convert_bag`] was asked to write `.mcap`.
    ///
    /// astrs-rosbag implements `.mcap` **read** only (blueprint §10.6): no
    /// writer exists to convert an `.arec` (or a `.db3`) into one. Convert
    /// to `.arec` or `.db3` instead.
    #[error(
        "cannot write `.mcap` in this build: astrs-rosbag implements .mcap READ only \
         (blueprint §10.6); convert to `.arec` or `.db3` instead"
    )]
    McapWriteUnsupported,

    /// [`crate::convert::convert_bag`] was asked to convert directly
    /// between two shapes this crate does not bridge without going
    /// through `.arec` — `.mcap` → `.db3` (this crate has no `.mcap`
    /// writer; see [`Self::McapWriteUnsupported`]), or the same format on
    /// both sides (not a conversion at all).
    #[error(
        "astrs-rosbag does not convert `.{from}` directly to `.{to}`; convert via `.arec` \
         instead (`.{from}` → `.arec`, then `.arec` → `.{to}`)"
    )]
    UnsupportedConversion {
        /// The input format's extension, lowercased (e.g. `"mcap"`).
        from: &'static str,
        /// The output format's extension, lowercased (e.g. `"db3"`).
        to: &'static str,
    },

    /// An `.arec` identifier ([`astrs_wire::NodeId`], [`astrs_wire::DataId`],
    /// [`astrs_wire::ParamKey`], …) could not be built.
    #[error("an .arec identifier could not be built: {0}")]
    Id(#[from] astrs_wire::IdError),

    /// The `.arec` container itself ([`astrs_recording::Reader`]/
    /// [`astrs_recording::Writer`]) reported an error.
    #[error(".arec recording error: {0}")]
    Recording(#[from] astrs_recording::RecordingError),

    /// A `.db3` SQL operation failed (open, DDL, or a query/statement).
    #[error("sqlite error on `{path}`: {source}")]
    Sql {
        /// The `.db3` file the connection was opened against.
        path: PathBuf,
        /// The underlying OxiSQL error.
        #[source]
        source: oxisql_core::OxiSqlError,
    },

    /// A `.db3` file is missing a table or column this crate cannot
    /// function without (as opposed to an optional, version-gated one —
    /// see the module docs' best-effort philosophy).
    #[error("`{path}` is missing the `{table}` table required by every rosbag2 schema version")]
    MissingTable {
        /// The `.db3` file.
        path: PathBuf,
        /// The table name that was not found.
        table: &'static str,
    },

    /// A `.db3` row held a value of the wrong SQL type for its column —
    /// structural corruption, not a version difference (those degrade
    /// gracefully; see the module docs).
    #[error(
        "`{path}`: row {row} of `{table}`.`{column}` has the wrong type: expected {expected}, \
         found {found}"
    )]
    ColumnTypeMismatch {
        /// The `.db3` file.
        path: PathBuf,
        /// The table the row came from.
        table: &'static str,
        /// The column with the unexpected type.
        column: &'static str,
        /// The SQL type expected.
        expected: &'static str,
        /// The SQL type (or "absent") actually found.
        found: &'static str,
        /// The row's primary key, for locating it.
        row: i64,
    },

    /// `path` does not begin with the `.mcap` magic bytes.
    #[error("`{path}` is not a valid .mcap file: {reason}")]
    BadMagic {
        /// The file that was opened.
        path: PathBuf,
        /// What was wrong with the magic.
        reason: &'static str,
    },

    /// An `.mcap` record at `offset` is structurally invalid — a bad
    /// opcode-specific field, an out-of-range channel/schema id reference,
    /// or similar.
    #[error("`{path}`: record at byte offset {offset} is malformed: {reason}")]
    Malformed {
        /// The `.mcap` file.
        path: PathBuf,
        /// The record's absolute byte offset.
        offset: u64,
        /// What was wrong.
        reason: String,
    },

    /// An `.mcap` record's declared length runs past the bytes actually
    /// available — the truncation signature, checked *before* any
    /// length-sized allocation (mirrors
    /// [`astrs_recording::RecordingError::Incomplete`]).
    #[error(
        "`{path}`: record at byte offset {offset} is truncated: needed {needed} more bytes, \
         {available} were available"
    )]
    Truncated {
        /// The `.mcap` file.
        path: PathBuf,
        /// Where the truncated record starts.
        offset: u64,
        /// How many more bytes the record's own framing said would follow.
        needed: usize,
        /// How many bytes were actually available from `offset` onward.
        available: usize,
    },

    /// A `Chunk` record at `offset` declares an `uncompressed_size` over
    /// [`crate::mcap::MAX_CHUNK_UNCOMPRESSED_BYTES`] — refused *before*
    /// decompression is attempted, so a 40-byte record cannot demand a
    /// multi-gigabyte allocation (blueprint §18's decompression-bomb
    /// discipline, the same one `astrs-transport::compress` documents).
    #[error(
        "`{path}`: chunk at byte offset {offset} declares an uncompressed size of {declared} \
         bytes, over the {limit}-byte safety ceiling"
    )]
    ChunkTooLarge {
        /// The `.mcap` file.
        path: PathBuf,
        /// The chunk record's absolute byte offset.
        offset: u64,
        /// The declared `uncompressed_size`.
        declared: u64,
        /// The ceiling that was exceeded.
        limit: u64,
    },

    /// A `Chunk` record's decompressed `records` bytes contain another
    /// `Chunk` record — never valid (mirrors the reference Go reader's
    /// `ErrNestedChunk`).
    #[error(
        "`{path}`: chunk at byte offset {offset} contains a nested chunk, which is not permitted"
    )]
    NestedChunk {
        /// The `.mcap` file.
        path: PathBuf,
        /// The outer chunk record's absolute byte offset.
        offset: u64,
    },

    /// A `Chunk` record names a compression algorithm this crate does not
    /// implement (only `""`, `"lz4"`, and `"zstd"` are — the MCAP registry's
    /// well-known set, §10.6's own scope).
    #[error(
        "`{path}`: chunk at byte offset {offset} uses an unrecognized compression `{compression}` \
         (expected \"\", \"lz4\", or \"zstd\")"
    )]
    UnknownCompression {
        /// The `.mcap` file.
        path: PathBuf,
        /// The chunk record's absolute byte offset.
        offset: u64,
        /// The compression string that was found.
        compression: String,
    },

    /// The lz4 or zstd codec rejected a chunk's compressed bytes.
    #[error("`{path}`: chunk at byte offset {offset} failed to decompress: {reason}")]
    Codec {
        /// The `.mcap` file.
        path: PathBuf,
        /// The chunk record's absolute byte offset.
        offset: u64,
        /// The codec's own error text.
        reason: String,
    },

    /// A chunk decompressed to a different length than its own
    /// `uncompressed_size` declared — corruption, caught immediately
    /// rather than propagated into a mis-parsed sub-record.
    #[error(
        "`{path}`: chunk at byte offset {offset} decompressed to {actual} bytes, not the \
         declared {declared}"
    )]
    ChunkSizeMismatch {
        /// The `.mcap` file.
        path: PathBuf,
        /// The chunk record's absolute byte offset.
        offset: u64,
        /// The declared `uncompressed_size`.
        declared: u64,
        /// The number of bytes decompression actually produced.
        actual: usize,
    },

    /// A defensive check failed that this crate's own logic should have
    /// made impossible — reported as a typed error rather than an index
    /// panic or a silently wrong answer, per the workspace's no-`panic!`
    /// policy. Seeing this is always a bug report.
    #[error("internal invariant violated in astrs-rosbag: {0}")]
    Internal(String),
}

impl RosbagError {
    /// Wraps an [`io::Error`] with the path that was being accessed.
    #[must_use]
    pub fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Wraps an [`oxisql_core::OxiSqlError`] with the `.db3` path the
    /// connection was opened against.
    #[must_use]
    pub fn sql(path: impl Into<PathBuf>, source: oxisql_core::OxiSqlError) -> Self {
        Self::Sql {
            path: path.into(),
            source,
        }
    }

    /// True for the truncation family ([`Self::Truncated`]) — the signal a
    /// caller streaming a still-being-written file (mirroring
    /// `astrs-recording`'s scan-recovery use case) would want to treat as
    /// "not yet, not corrupt".
    #[must_use]
    pub const fn is_truncation(&self) -> bool {
        matches!(self, Self::Truncated { .. })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn io_helper_carries_the_path() {
        let err = RosbagError::io(
            "/tmp/nope.db3",
            io::Error::new(io::ErrorKind::NotFound, "nope"),
        );
        assert!(err.to_string().contains("/tmp/nope.db3"));
    }

    #[test]
    fn is_truncation_identifies_only_the_truncated_variant() {
        let truncated = RosbagError::Truncated {
            path: PathBuf::from("a.mcap"),
            offset: 0,
            needed: 4,
            available: 1,
        };
        assert!(truncated.is_truncation());
        let malformed = RosbagError::Malformed {
            path: PathBuf::from("a.mcap"),
            offset: 0,
            reason: "x".to_owned(),
        };
        assert!(!malformed.is_truncation());
    }

    #[test]
    fn id_error_converts_via_from() {
        let source = astrs_wire::NodeId::new("bad name").unwrap_err();
        let err: RosbagError = source.into();
        assert!(matches!(err, RosbagError::Id(_)));
    }

    #[test]
    fn every_variant_has_a_non_empty_display() {
        // A cheap guard against a future variant with a blank `#[error("")]`.
        let samples: Vec<RosbagError> = vec![
            RosbagError::io("p", io::Error::other("x")),
            RosbagError::UnknownExtension {
                path: PathBuf::from("p"),
            },
            RosbagError::UnsupportedExtension {
                path: PathBuf::from("p"),
                extension: "txt".to_owned(),
            },
            RosbagError::McapWriteUnsupported,
            RosbagError::UnsupportedConversion {
                from: "mcap",
                to: "db3",
            },
            RosbagError::MissingTable {
                path: PathBuf::from("p"),
                table: "messages",
            },
            RosbagError::BadMagic {
                path: PathBuf::from("p"),
                reason: "short",
            },
            RosbagError::Internal("x".to_owned()),
        ];
        for err in samples {
            assert!(!err.to_string().is_empty());
        }
    }

    #[test]
    fn unsupported_conversion_names_the_arec_workaround() {
        let err = RosbagError::UnsupportedConversion {
            from: "mcap",
            to: "db3",
        };
        let text = err.to_string();
        assert!(text.contains(".mcap"), "{text}");
        assert!(text.contains(".db3"), "{text}");
        assert!(text.contains(".arec"), "{text}");
    }
}
