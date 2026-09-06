//! The crate's unified error type.

use std::path::PathBuf;

use thiserror::Error;

use crate::record::MutationSeq;

/// Everything that can go wrong in `astrs-store`: backend I/O, JSON/oxicode
/// (de)serialization of a bucket record, schema-version mismatches on open,
/// and catch-up log errors (compacted history, an out-of-range compaction
/// target).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The underlying [`oxistore_core::KvStore`] backend failed.
    #[error("store backend error: {0}")]
    Backend(#[from] oxistore_core::StoreError),

    /// An id (e.g. [`astrs_wire::ParamKey`], [`astrs_wire::NodeId`]) failed
    /// its own validation while being constructed for a call into this
    /// crate.
    ///
    /// A `From` conversion (rather than requiring every caller to map it by
    /// hand) so building, say, a [`astrs_wire::ParamKey`] and passing it to
    /// [`crate::CoordinatorStore::set_param`] can be chained with `?`
    /// inside a function already returning [`Result`].
    #[error(transparent)]
    Id(#[from] astrs_wire::IdError),

    /// A JSON value failed to parse or serialize outside the context of any
    /// specific bucket record — for instance,
    /// [`crate::record::ParamRecord::value`] decoding a parameter's stored
    /// value back to a [`serde_json::Value`].
    ///
    /// Contrast [`Error::JsonEncode`] / [`Error::JsonDecode`], which carry
    /// the bucket and key a *record* was being read from or written to;
    /// this variant is for JSON errors with no such context to attach.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// A bucket record failed to serialize to JSON on write.
    ///
    /// In practice this is effectively unreachable for the record types this
    /// crate defines (every field type serializes infallibly), but
    /// `serde_json::to_vec` still returns a `Result`, and this crate does not
    /// `unwrap()`/`expect()` it away.
    #[error("failed to encode {what} as JSON: {source}")]
    JsonEncode {
        /// A short label for what was being encoded (e.g. `"param record"`).
        what: &'static str,
        /// The underlying JSON encode failure.
        #[source]
        source: serde_json::Error,
    },

    /// A value read back from a bucket was not valid JSON for its record
    /// type.
    ///
    /// This indicates on-disk corruption or a foreign write to the store
    /// file — every value this crate itself writes round-trips.
    #[error("stored {what} at key {key_preview} is not valid JSON: {source}")]
    JsonDecode {
        /// A short label for what was being decoded (e.g. `"param record"`).
        what: &'static str,
        /// A bounded, human-readable preview of the offending key.
        key_preview: String,
        /// The underlying JSON decode failure.
        #[source]
        source: serde_json::Error,
    },

    /// A [`crate::record::MutationRecord`] failed to encode or decode
    /// through the oxicode wire codec ([`astrs_wire::WireEncode`] /
    /// [`astrs_wire::WireDecode`]).
    #[error("mutation log codec error: {0}")]
    Wire(#[from] astrs_wire::WireError),

    /// A stored key or value was structurally malformed — too short to
    /// contain its fixed-width fields, or containing bytes that are not a
    /// legal id in the family they claim to be.
    ///
    /// Like [`Error::JsonDecode`], this indicates on-disk corruption or a
    /// foreign write; every key this crate itself writes is well-formed by
    /// construction.
    #[error("corrupt {bucket} key {key_preview}: {reason}")]
    CorruptKey {
        /// The bucket the malformed key was read from.
        bucket: &'static str,
        /// A bounded, human-readable preview of the offending key.
        key_preview: String,
        /// What was wrong with it.
        reason: String,
    },

    /// [`crate::schema::STORE_SCHEMA_VERSION`] on disk does not match the
    /// version this build expects.
    ///
    /// Blueprint §2.1 (the dora coordinator-store lesson): a coordinator
    /// that refuses to start on a stale schema, with no way forward except
    /// deleting files by hand, turns a routine upgrade into an outage. This
    /// variant is deliberately fatal to [`crate::CoordinatorStore::open`] —
    /// the escape hatch is the explicit, opt-in
    /// [`crate::schema::recreate_store`], never an automatic wipe.
    #[error(
        "store schema mismatch at {path}: found version {found}, this build expects {expected}; \
         call astrs_store::schema::recreate_store() to reset it"
    )]
    SchemaMismatch {
        /// The store file (or `"<in-memory>"`) the marker was read from.
        path: PathBuf,
        /// The version recorded in the store.
        found: u32,
        /// The version this build was compiled against.
        expected: u32,
    },

    /// [`crate::CoordinatorStore::mutations_since`] was asked for history
    /// older than [`crate::CoordinatorStore::compact`] has retained.
    ///
    /// The caller (typically the coordinator resyncing a reconnecting
    /// daemon, blueprint §12) must fall back to a full state resend rather
    /// than a catch-up delta.
    #[error(
        "mutation history requested from seq {requested} but the log was compacted before {earliest_available}"
    )]
    MutationHistoryCompacted {
        /// The sequence number the caller asked to resume from.
        requested: MutationSeq,
        /// The oldest sequence number still retained.
        earliest_available: MutationSeq,
    },

    /// [`crate::CoordinatorStore::compact`] was asked to retain up to a
    /// sequence number that has not been issued yet.
    #[error("cannot compact up to seq {requested}: only {current_max} entries have been logged")]
    InvalidCompactionTarget {
        /// The requested retention boundary.
        requested: MutationSeq,
        /// The highest sequence number actually issued so far.
        current_max: MutationSeq,
    },

    /// A filesystem operation on the store file (e.g.
    /// [`crate::schema::recreate_store`] removing it) failed.
    #[error("store file I/O error at {path}: {source}")]
    Io {
        /// The file the operation was acting on.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// An [`crate::AsyncCoordinatorStore`] blocking task panicked or was
    /// cancelled before it could return.
    #[error("store task failed to complete: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// Result alias used throughout this crate.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn backend_error_converts_via_from() {
        let err: Error = oxistore_core::StoreError::NotFound.into();
        assert!(matches!(err, Error::Backend(_)));
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn schema_mismatch_message_names_both_versions() {
        let err = Error::SchemaMismatch {
            path: PathBuf::from("store.redb"),
            found: 1,
            expected: 2,
        };
        let text = err.to_string();
        assert!(text.contains("found version 1"));
        assert!(text.contains("expects 2"));
    }

    #[test]
    fn mutation_history_compacted_names_both_seqs() {
        let err = Error::MutationHistoryCompacted {
            requested: MutationSeq::new(3),
            earliest_available: MutationSeq::new(10),
        };
        let text = err.to_string();
        assert!(text.contains('3'));
        assert!(text.contains("10"));
    }

    #[test]
    fn json_decode_error_carries_a_key_preview() {
        let source = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let err = Error::JsonDecode {
            what: "param record",
            key_preview: "01deadbeef".to_owned(),
            source,
        };
        assert!(err.to_string().contains("01deadbeef"));
    }
}
