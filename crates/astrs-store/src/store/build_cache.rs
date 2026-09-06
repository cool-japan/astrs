//! The build-cache index: one [`BuildCacheEntry`] row per
//! [`BuildCacheKey`] (a build's source-hash identity).

use oxistore_core::KvStore;

use crate::error::{Error, Result};
use crate::keys;
use crate::record::{BuildCacheEntry, BuildCacheKey, MutationOp, MutationSeq};
use crate::store::{CoordinatorStore, from_json, to_json};

impl<S: KvStore + Clone> CoordinatorStore<S> {
    /// Records a freshly built artifact, or fully replaces an existing
    /// entry for the same hash (a rebuild that reused the same source
    /// hash — the artifact path or size may still have changed).
    ///
    /// Sets both [`BuildCacheEntry::built_at`] and
    /// [`BuildCacheEntry::last_used_at`] to now; use
    /// [`CoordinatorStore::touch_build_cache`] when reusing an existing
    /// artifact without rebuilding it.
    ///
    /// # Errors
    ///
    /// [`Error::JsonDecode`] if an existing record at `hash` is on-disk
    /// corrupted. [`Error::Backend`] if the underlying write fails.
    pub fn record_build(
        &self,
        hash: BuildCacheKey,
        artifact_path: impl Into<String>,
        size_bytes: Option<u64>,
    ) -> Result<MutationSeq> {
        let artifact_path = artifact_path.into();
        let bucket_key = keys::build_cache_key(&hash);
        self.apply_upsert(bucket_key, move |existing, ts| {
            let revision = match existing {
                Some(bytes) => decode_entry(&bytes, &hash)?.revision + 1,
                None => 1,
            };
            let record = BuildCacheEntry {
                hash,
                artifact_path,
                built_at: ts,
                last_used_at: ts,
                size_bytes,
                revision,
            };
            let bytes = to_json("build cache entry", &record)?;
            Ok((bytes, MutationOp::BuildCachePut { record }))
        })
    }

    /// Marks a cached artifact reused now, without rebuilding it: bumps
    /// [`BuildCacheEntry::last_used_at`] and its revision, leaving
    /// [`BuildCacheEntry::built_at`] untouched.
    ///
    /// Returns `Ok(None)` if `hash` has no cached entry.
    ///
    /// # Errors
    ///
    /// As [`CoordinatorStore::record_build`].
    pub fn touch_build_cache(&self, hash: &BuildCacheKey) -> Result<Option<MutationSeq>> {
        let bucket_key = keys::build_cache_key(hash);
        let hash = hash.clone();
        self.apply_update_if_present(bucket_key, move |bytes, ts| {
            let mut record = decode_entry(&bytes, &hash)?;
            record.last_used_at = ts;
            record.revision += 1;
            let bytes = to_json("build cache entry", &record)?;
            Ok((bytes, MutationOp::BuildCachePut { record }))
        })
    }

    /// Reads a build-cache entry, if one exists for `hash`.
    ///
    /// # Errors
    ///
    /// [`Error::JsonDecode`] if the stored record is on-disk corrupted.
    /// [`Error::Backend`] if the read fails.
    pub fn get_build_cache(&self, hash: &BuildCacheKey) -> Result<Option<BuildCacheEntry>> {
        let bucket_key = keys::build_cache_key(hash);
        match self.backend.get(&bucket_key)? {
            Some(bytes) => Ok(Some(from_json("build cache entry", &bucket_key, &bytes)?)),
            None => Ok(None),
        }
    }

    /// Removes a build-cache entry.
    ///
    /// Returns `Ok(None)` if `hash` had no cached entry.
    ///
    /// # Errors
    ///
    /// [`Error::Backend`] if the underlying write fails.
    pub fn remove_build_cache(&self, hash: &BuildCacheKey) -> Result<Option<MutationSeq>> {
        let bucket_key = keys::build_cache_key(hash);
        self.apply_delete_if_present(
            &bucket_key,
            MutationOp::BuildCacheDelete { hash: hash.clone() },
        )
    }

    /// Lists every build-cache entry.
    ///
    /// # Errors
    ///
    /// [`Error::JsonDecode`] if a stored record is on-disk corrupted.
    /// [`Error::Backend`] if the scan fails.
    pub fn list_build_cache(&self) -> Result<Vec<BuildCacheEntry>> {
        let prefix = keys::build_cache_bucket_prefix();
        let mut out = Vec::new();
        for item in self.backend.prefix_scan(&prefix)? {
            let (raw_key, raw_value) = item?;
            out.push(from_json("build cache entry", &raw_key, &raw_value)?);
        }
        Ok(out)
    }
}

fn decode_entry(bytes: &[u8], hash: &BuildCacheKey) -> Result<BuildCacheEntry> {
    serde_json::from_slice(bytes).map_err(|source| Error::JsonDecode {
        what: "build cache entry",
        key_preview: hash.to_hex(),
        source,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn store() -> CoordinatorStore<oxistore_kv_redb::RedbStore> {
        CoordinatorStore::open_in_memory().unwrap()
    }

    #[test]
    fn record_then_get_round_trips() {
        let store = store();
        let hash = BuildCacheKey::new(vec![1, 2, 3]);
        store
            .record_build(hash.clone(), "/tmp/artifacts/a.so", Some(1024))
            .unwrap();
        let entry = store.get_build_cache(&hash).unwrap().unwrap();
        assert_eq!(entry.artifact_path, "/tmp/artifacts/a.so");
        assert_eq!(entry.size_bytes, Some(1024));
        assert_eq!(entry.revision, 1);
        assert_eq!(entry.built_at, entry.last_used_at);
    }

    #[test]
    fn touch_bumps_last_used_but_not_built_at() {
        let store = store();
        let hash = BuildCacheKey::new(vec![1]);
        store.record_build(hash.clone(), "a", None).unwrap();
        let first = store.get_build_cache(&hash).unwrap().unwrap();

        store.touch_build_cache(&hash).unwrap();
        let second = store.get_build_cache(&hash).unwrap().unwrap();

        assert_eq!(second.built_at, first.built_at);
        assert!(second.last_used_at >= first.last_used_at);
        assert_eq!(second.revision, 2);
    }

    #[test]
    fn touch_on_a_missing_hash_is_none() {
        let store = store();
        assert!(
            store
                .touch_build_cache(&BuildCacheKey::new(vec![9]))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn re_recording_the_same_hash_replaces_the_entry() {
        let store = store();
        let hash = BuildCacheKey::new(vec![1]);
        store.record_build(hash.clone(), "a", Some(1)).unwrap();
        store.record_build(hash.clone(), "b", Some(2)).unwrap();
        let entry = store.get_build_cache(&hash).unwrap().unwrap();
        assert_eq!(entry.artifact_path, "b");
        assert_eq!(entry.size_bytes, Some(2));
        assert_eq!(entry.revision, 2);
    }

    #[test]
    fn remove_then_list_reflects_the_removal() {
        let store = store();
        let a = BuildCacheKey::new(vec![1]);
        let b = BuildCacheKey::new(vec![2]);
        store.record_build(a.clone(), "a", None).unwrap();
        store.record_build(b, "b", None).unwrap();
        assert_eq!(store.list_build_cache().unwrap().len(), 2);

        assert!(store.remove_build_cache(&a).unwrap().is_some());
        assert_eq!(store.list_build_cache().unwrap().len(), 1);
    }
}
