//! The on-disk schema-version marker and the explicit recreate-store escape
//! hatch.

use std::path::Path;

use oxistore_core::KvStore;

use crate::error::{Error, Result};
use crate::keys;

/// The on-disk schema version this build writes and expects to read.
///
/// Bump this whenever a bucket's key layout or record shape changes in a way
/// that is not forward-compatible with an older `astrs-store` build reading
/// the same file. `check_or_initialize` (crate-private) then rejects an on-disk store from
/// before the bump with a typed [`Error::SchemaMismatch`] instead of
/// misreading it.
pub const STORE_SCHEMA_VERSION: u32 = 1;

/// Checks a freshly opened backend's schema marker, writing one if the store
/// is new (its marker key is absent).
///
/// # Errors
///
/// [`Error::SchemaMismatch`] if the store already has a marker and it does
/// not equal [`STORE_SCHEMA_VERSION`]. [`Error::Backend`] if the marker
/// read or write itself fails. [`Error::CorruptKey`] if the marker exists
/// but is not four bytes (on-disk corruption or a foreign write).
pub(crate) fn check_or_initialize(backend: &dyn KvStore, display_path: &Path) -> Result<()> {
    let key = keys::schema_marker_key();
    match backend.get(&key)? {
        None => {
            backend.put(&key, &STORE_SCHEMA_VERSION.to_be_bytes())?;
            Ok(())
        }
        Some(bytes) => {
            let found = decode_marker(&bytes)?;
            if found == STORE_SCHEMA_VERSION {
                Ok(())
            } else {
                Err(Error::SchemaMismatch {
                    path: display_path.to_path_buf(),
                    found,
                    expected: STORE_SCHEMA_VERSION,
                })
            }
        }
    }
}

fn decode_marker(bytes: &[u8]) -> Result<u32> {
    let array: [u8; 4] = bytes.try_into().map_err(|_| Error::CorruptKey {
        bucket: "schema",
        key_preview: "<schema marker>".to_owned(),
        reason: format!("marker is {} bytes, expected 4", bytes.len()),
    })?;
    Ok(u32::from_be_bytes(array))
}

/// Deletes a redb-backed store file so it can be recreated from scratch.
///
/// This is the escape hatch for [`Error::SchemaMismatch`] (blueprint §2.1,
/// the dora coordinator-store lesson: a coordinator that cannot recover from
/// a stale on-disk schema without a human deleting files by hand turns a
/// routine upgrade into an outage). It is deliberately **not** called
/// automatically by [`crate::CoordinatorStore::open`] on a mismatch —
/// wiping a coordinator's parameter and dataflow state is a decision an
/// operator makes, never one this crate makes silently on their behalf.
///
/// # Preconditions
///
/// Every [`crate::CoordinatorStore`] handle on `path` must be dropped
/// first. `redb` holds an exclusive file lock for as long as a `Database`
/// handle is open; removing the file out from under a live handle leaves
/// that handle's further writes going to an unlinked inode that a
/// subsequent `open` of the same path will never see, rather than actually
/// failing loudly.
///
/// # Errors
///
/// [`Error::Io`] if the file exists and cannot be removed (permissions, or
/// still held open on a platform that disallows unlinking open files). A
/// file that is already absent is treated as success: recreating an
/// already-gone store is a no-op, not an error.
///
/// # Examples
///
/// ```
/// use astrs_store::CoordinatorStore;
///
/// let path = std::env::temp_dir()
///     .join(format!("astrs-store-schema-doctest-{}.redb", std::process::id()));
/// # std::fs::remove_file(&path).ok();
/// let store = CoordinatorStore::open(&path)?;
/// drop(store);
///
/// astrs_store::schema::recreate_store(&path)?;
/// let _reopened = CoordinatorStore::open(&path)?;
/// # std::fs::remove_file(&path).ok();
/// # Ok::<(), astrs_store::Error>(())
/// ```
pub fn recreate_store(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use oxistore_kv_redb::RedbStore;

    #[test]
    fn a_fresh_backend_gets_a_marker_written() {
        let backend = RedbStore::open_in_memory().unwrap();
        assert!(backend.get(&keys::schema_marker_key()).unwrap().is_none());
        check_or_initialize(&backend, Path::new("<memory>")).unwrap();
        let marker = backend.get(&keys::schema_marker_key()).unwrap().unwrap();
        assert_eq!(decode_marker(&marker).unwrap(), STORE_SCHEMA_VERSION);
    }

    #[test]
    fn a_matching_marker_is_accepted_without_being_rewritten() {
        let backend = RedbStore::open_in_memory().unwrap();
        check_or_initialize(&backend, Path::new("<memory>")).unwrap();
        // Calling it again on the same backend must still succeed.
        check_or_initialize(&backend, Path::new("<memory>")).unwrap();
    }

    #[test]
    fn a_mismatched_marker_is_a_typed_schema_mismatch() {
        let backend = RedbStore::open_in_memory().unwrap();
        backend
            .put(&keys::schema_marker_key(), &99u32.to_be_bytes())
            .unwrap();
        let err = check_or_initialize(&backend, Path::new("store.redb")).unwrap_err();
        match err {
            Error::SchemaMismatch {
                found, expected, ..
            } => {
                assert_eq!(found, 99);
                assert_eq!(expected, STORE_SCHEMA_VERSION);
            }
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_truncated_marker_is_a_corrupt_key_not_a_panic() {
        let backend = RedbStore::open_in_memory().unwrap();
        backend.put(&keys::schema_marker_key(), &[1, 2]).unwrap();
        assert!(check_or_initialize(&backend, Path::new("store.redb")).is_err());
    }

    #[test]
    fn recreate_store_on_a_missing_file_is_not_an_error() {
        let path = std::env::temp_dir().join(format!(
            "astrs-store-recreate-missing-{}-{}.redb",
            std::process::id(),
            line!()
        ));
        assert!(!path.exists());
        recreate_store(&path).unwrap();
    }

    #[test]
    fn recreate_store_removes_an_existing_file() {
        let path = std::env::temp_dir().join(format!(
            "astrs-store-recreate-existing-{}-{}.redb",
            std::process::id(),
            line!()
        ));
        {
            let backend = RedbStore::open(&path).unwrap();
            drop(backend);
        }
        assert!(path.exists());
        recreate_store(&path).unwrap();
        assert!(!path.exists());
    }
}
