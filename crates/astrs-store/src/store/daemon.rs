//! The daemon registry: one [`DaemonRecord`] row per [`DaemonId`].

use astrs_wire::{DaemonId, DaemonInfo};
use oxistore_core::KvStore;

use crate::error::{Error, Result};
use crate::keys;
use crate::record::{DaemonRecord, MutationOp, MutationSeq};
use crate::store::{CoordinatorStore, from_json, to_json};

impl<S: KvStore + Clone> CoordinatorStore<S> {
    /// Registers a daemon, or fully replaces an existing registration.
    ///
    /// [`DaemonRecord::last_heartbeat`] is set to this store's current HLC
    /// time — a freshly (re-)registered daemon counts as heartbeating now.
    ///
    /// # Errors
    ///
    /// [`Error::JsonDecode`] if an existing record at `info.id` is on-disk
    /// corrupted. [`Error::Backend`] if the underlying write fails.
    pub fn upsert_daemon(&self, info: DaemonInfo) -> Result<MutationSeq> {
        let daemon = info.id.clone();
        let bucket_key = keys::daemon_key(&daemon);
        self.apply_upsert(bucket_key, move |existing, ts| {
            let revision = match existing {
                Some(bytes) => decode_daemon(&bytes, &daemon)?.revision + 1,
                None => 1,
            };
            let record = DaemonRecord {
                info,
                last_heartbeat: ts,
                revision,
            };
            let bytes = to_json("daemon record", &record)?;
            Ok((bytes, MutationOp::DaemonPut { record }))
        })
    }

    /// Records a heartbeat from an already-registered daemon: bumps
    /// [`DaemonRecord::last_heartbeat`] to now and marks
    /// [`astrs_wire::DaemonInfo::reachable`].
    ///
    /// Returns `Ok(None)` without writing anything if `daemon` was never
    /// registered — blueprint §12's heartbeat loop assumes `Register`
    /// happens once, before any heartbeat.
    ///
    /// # Errors
    ///
    /// As [`CoordinatorStore::upsert_daemon`].
    pub fn record_heartbeat(&self, daemon: &DaemonId) -> Result<Option<MutationSeq>> {
        let bucket_key = keys::daemon_key(daemon);
        let daemon = daemon.clone();
        self.apply_update_if_present(bucket_key, move |bytes, ts| {
            let mut record = decode_daemon(&bytes, &daemon)?;
            record.last_heartbeat = ts;
            record.info.reachable = true;
            record.revision += 1;
            let bytes = to_json("daemon record", &record)?;
            Ok((bytes, MutationOp::DaemonPut { record }))
        })
    }

    /// Marks a registered daemon reachable or unreachable, without touching
    /// [`DaemonRecord::last_heartbeat`].
    ///
    /// The coordinator's heartbeat-timeout logic (blueprint §12:
    /// *degraded-autonomous* detection after 20s of silence) calls this
    /// with `false` on timeout and [`CoordinatorStore::record_heartbeat`]
    /// on the next successful heartbeat, which sets it back to `true`.
    ///
    /// Returns `Ok(None)` if `daemon` was never registered.
    ///
    /// # Errors
    ///
    /// As [`CoordinatorStore::upsert_daemon`].
    pub fn set_daemon_reachable(
        &self,
        daemon: &DaemonId,
        reachable: bool,
    ) -> Result<Option<MutationSeq>> {
        let bucket_key = keys::daemon_key(daemon);
        let daemon = daemon.clone();
        self.apply_update_if_present(bucket_key, move |bytes, _ts| {
            let mut record = decode_daemon(&bytes, &daemon)?;
            record.info.reachable = reachable;
            record.revision += 1;
            let bytes = to_json("daemon record", &record)?;
            Ok((bytes, MutationOp::DaemonPut { record }))
        })
    }

    /// Reads a daemon's registration record, if it exists.
    ///
    /// # Errors
    ///
    /// [`Error::JsonDecode`] if the stored record is on-disk corrupted.
    /// [`Error::Backend`] if the read fails.
    pub fn get_daemon(&self, daemon: &DaemonId) -> Result<Option<DaemonRecord>> {
        let bucket_key = keys::daemon_key(daemon);
        match self.backend.get(&bucket_key)? {
            Some(bytes) => Ok(Some(from_json("daemon record", &bucket_key, &bytes)?)),
            None => Ok(None),
        }
    }

    /// Removes a daemon's registration record.
    ///
    /// Returns `Ok(None)` if `daemon` was not registered.
    ///
    /// # Errors
    ///
    /// [`Error::Backend`] if the underlying write fails.
    pub fn remove_daemon(&self, daemon: &DaemonId) -> Result<Option<MutationSeq>> {
        let bucket_key = keys::daemon_key(daemon);
        self.apply_delete_if_present(
            &bucket_key,
            MutationOp::DaemonDelete {
                daemon: daemon.clone(),
            },
        )
    }

    /// Lists every registered daemon.
    ///
    /// # Errors
    ///
    /// [`Error::JsonDecode`] if a stored record is on-disk corrupted.
    /// [`Error::Backend`] if the scan fails.
    pub fn list_daemons(&self) -> Result<Vec<DaemonRecord>> {
        let prefix = keys::daemon_bucket_prefix();
        let mut out = Vec::new();
        for item in self.backend.prefix_scan(&prefix)? {
            let (raw_key, raw_value) = item?;
            out.push(from_json("daemon record", &raw_key, &raw_value)?);
        }
        Ok(out)
    }
}

fn decode_daemon(bytes: &[u8], daemon: &DaemonId) -> Result<DaemonRecord> {
    serde_json::from_slice(bytes).map_err(|source| Error::JsonDecode {
        what: "daemon record",
        key_preview: daemon.to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_time::HlcTimestamp;
    use astrs_wire::AstrsVersion;
    use std::collections::BTreeMap;

    fn store() -> CoordinatorStore<oxistore_kv_redb::RedbStore> {
        CoordinatorStore::open_in_memory().unwrap()
    }

    fn sample_info(daemon: DaemonId) -> DaemonInfo {
        DaemonInfo {
            id: daemon,
            version: AstrsVersion::current(),
            address: "127.0.0.1:7408".to_owned(),
            connected_at: HlcTimestamp::new(1, 0),
            node_count: 0,
            labels: BTreeMap::new(),
            reachable: true,
        }
    }

    #[test]
    fn upsert_then_get_round_trips() {
        let store = store();
        let daemon = DaemonId::generate(None);
        store.upsert_daemon(sample_info(daemon.clone())).unwrap();
        let record = store.get_daemon(&daemon).unwrap().unwrap();
        assert_eq!(record.revision, 1);
        assert_eq!(record.info.id, daemon);
    }

    #[test]
    fn re_upserting_bumps_revision() {
        let store = store();
        let daemon = DaemonId::generate(None);
        store.upsert_daemon(sample_info(daemon.clone())).unwrap();
        store.upsert_daemon(sample_info(daemon.clone())).unwrap();
        assert_eq!(store.get_daemon(&daemon).unwrap().unwrap().revision, 2);
    }

    #[test]
    fn heartbeat_updates_timestamp_and_reachability() {
        let store = store();
        let daemon = DaemonId::generate(None);
        store.upsert_daemon(sample_info(daemon.clone())).unwrap();
        store.set_daemon_reachable(&daemon, false).unwrap();
        assert!(!store.get_daemon(&daemon).unwrap().unwrap().info.reachable);

        let seq = store.record_heartbeat(&daemon).unwrap();
        assert!(seq.is_some());
        let record = store.get_daemon(&daemon).unwrap().unwrap();
        assert!(record.info.reachable);
        assert_eq!(record.revision, 3);
    }

    #[test]
    fn heartbeat_on_an_unregistered_daemon_is_none() {
        let store = store();
        let daemon = DaemonId::generate(None);
        assert!(store.record_heartbeat(&daemon).unwrap().is_none());
    }

    #[test]
    fn set_reachable_does_not_disturb_last_heartbeat() {
        let store = store();
        let daemon = DaemonId::generate(None);
        store.upsert_daemon(sample_info(daemon.clone())).unwrap();
        let before = store.get_daemon(&daemon).unwrap().unwrap().last_heartbeat;
        store.set_daemon_reachable(&daemon, false).unwrap();
        let after = store.get_daemon(&daemon).unwrap().unwrap().last_heartbeat;
        assert_eq!(before, after);
    }

    #[test]
    fn remove_then_list_reflects_the_removal() {
        let store = store();
        let a = DaemonId::generate(None);
        let b = DaemonId::generate(None);
        store.upsert_daemon(sample_info(a.clone())).unwrap();
        store.upsert_daemon(sample_info(b.clone())).unwrap();
        assert_eq!(store.list_daemons().unwrap().len(), 2);

        assert!(store.remove_daemon(&a).unwrap().is_some());
        assert!(store.remove_daemon(&a).unwrap().is_none(), "already gone");
        assert_eq!(store.list_daemons().unwrap().len(), 1);
    }

    /// The daemon bucket's key is a [`DaemonId`]'s display text (variable
    /// length), unlike the params bucket's fixed-width-prefix key — worth
    /// its own corruption check rather than trusting the params-bucket
    /// version (`store::params::tests`) to stand in for every bucket's
    /// decode path.
    #[test]
    fn a_corrupted_daemon_row_is_a_typed_error_not_a_panic() {
        let store = store();
        let daemon = DaemonId::generate(None);
        store
            .backend
            .put(&keys::daemon_key(&daemon), b"not json")
            .unwrap();

        let err = store.get_daemon(&daemon).unwrap_err();
        assert!(matches!(
            err,
            Error::JsonDecode {
                what: "daemon record",
                ..
            }
        ));
    }

    #[test]
    fn list_daemons_surfaces_a_corrupt_row_as_an_error_instead_of_skipping_it() {
        let store = store();
        store
            .upsert_daemon(sample_info(DaemonId::generate(None)))
            .unwrap();
        store
            .backend
            .put(&keys::daemon_key(&DaemonId::generate(None)), b"not json")
            .unwrap();

        let err = store.list_daemons().unwrap_err();
        assert!(matches!(err, Error::JsonDecode { .. }));
    }
}
