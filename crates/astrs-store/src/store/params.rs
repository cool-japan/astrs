//! The parameters bucket: dataflow-scoped key → JSON value, with hybrid
//! logical clock timestamps and a per-key revision counter.

use astrs_wire::{DataflowId, ParamKey};
use oxistore_core::KvStore;

use crate::error::{Error, Result};
use crate::keys;
use crate::record::{MutationOp, MutationSeq, ParamRecord};
use crate::store::{CoordinatorStore, from_json, to_json};

impl<S: KvStore + Clone> CoordinatorStore<S> {
    /// Creates or updates one dataflow-scoped parameter.
    ///
    /// The first write to a `(dataflow, key)` pair starts
    /// [`ParamRecord::revision`] at `1`; every later write to the same pair
    /// increments it, and [`ParamRecord::created_at`] is carried forward
    /// from the first write. See [`ParamRecord`]'s docs for why the value
    /// is kept as JSON text internally rather than as a
    /// [`serde_json::Value`].
    ///
    /// # Errors
    ///
    /// [`Error::JsonEncode`] if `value` cannot be serialized (unreachable
    /// for a well-formed [`serde_json::Value`], which cannot contain a
    /// non-finite number). [`Error::JsonDecode`] if a prior record at this
    /// key is on-disk corrupted. [`Error::Backend`] if the underlying write
    /// fails.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_store::CoordinatorStore;
    /// use astrs_wire::{DataflowId, ParamKey};
    ///
    /// let store = CoordinatorStore::open_in_memory()?;
    /// let dataflow = DataflowId::generate();
    /// let key = ParamKey::new("gain")?;
    ///
    /// store.set_param(dataflow, key.clone(), serde_json::json!(1.0))?;
    /// store.set_param(dataflow, key.clone(), serde_json::json!(2.0))?;
    ///
    /// let record = store.get_param(dataflow, &key)?.expect("just written");
    /// assert_eq!(record.revision, 2);
    /// assert_eq!(record.value()?, serde_json::json!(2.0));
    /// # Ok::<(), astrs_store::Error>(())
    /// ```
    pub fn set_param(
        &self,
        dataflow: DataflowId,
        key: ParamKey,
        value: serde_json::Value,
    ) -> Result<MutationSeq> {
        let value_json = serde_json::to_string(&value).map_err(|source| Error::JsonEncode {
            what: "param value",
            source,
        })?;
        let bucket_key = keys::param_key(dataflow, &key);
        self.apply_upsert(bucket_key, move |existing, ts| {
            let (revision, created_at) = match existing {
                Some(bytes) => {
                    let old: ParamRecord =
                        serde_json::from_slice(&bytes).map_err(|source| Error::JsonDecode {
                            what: "param record",
                            key_preview: format!("{dataflow}/{key}"),
                            source,
                        })?;
                    (old.revision + 1, old.created_at)
                }
                None => (1, ts),
            };
            let record = ParamRecord {
                value_json,
                revision,
                created_at,
                updated_at: ts,
            };
            let bytes = to_json("param record", &record)?;
            let op = MutationOp::ParamPut {
                dataflow,
                key,
                record,
            };
            Ok((bytes, op))
        })
    }

    /// Reads one dataflow-scoped parameter's current record, if it exists.
    ///
    /// # Errors
    ///
    /// [`Error::JsonDecode`] if the stored bytes are not a valid
    /// [`ParamRecord`] (on-disk corruption; every record this crate writes
    /// round-trips). [`Error::Backend`] if the read fails.
    pub fn get_param(&self, dataflow: DataflowId, key: &ParamKey) -> Result<Option<ParamRecord>> {
        let bucket_key = keys::param_key(dataflow, key);
        match self.backend.get(&bucket_key)? {
            Some(bytes) => Ok(Some(from_json("param record", &bucket_key, &bytes)?)),
            None => Ok(None),
        }
    }

    /// Deletes one dataflow-scoped parameter.
    ///
    /// Returns `Ok(None)` if the key was already absent — see
    /// `CoordinatorStore::apply_delete_if_present` (crate-private) — otherwise the
    /// sequence number the deletion was logged under.
    ///
    /// # Errors
    ///
    /// [`Error::Backend`] if the underlying write fails.
    pub fn delete_param(
        &self,
        dataflow: DataflowId,
        key: &ParamKey,
    ) -> Result<Option<MutationSeq>> {
        let bucket_key = keys::param_key(dataflow, key);
        self.apply_delete_if_present(
            &bucket_key,
            MutationOp::ParamDelete {
                dataflow,
                key: key.clone(),
            },
        )
    }

    /// Lists every parameter currently set for `dataflow`, ordered by key.
    ///
    /// # Errors
    ///
    /// [`Error::CorruptKey`] if a stored key is malformed.
    /// [`Error::JsonDecode`] if a stored value is malformed.
    /// [`Error::Backend`] if the scan fails.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_store::CoordinatorStore;
    /// use astrs_wire::{DataflowId, ParamKey};
    ///
    /// let store = CoordinatorStore::open_in_memory()?;
    /// let dataflow = DataflowId::generate();
    /// store.set_param(dataflow, ParamKey::new("a")?, serde_json::json!(1))?;
    /// store.set_param(dataflow, ParamKey::new("b")?, serde_json::json!(2))?;
    /// store.set_param(DataflowId::generate(), ParamKey::new("a")?, serde_json::json!(9))?;
    ///
    /// let listed = store.list_params(dataflow)?;
    /// assert_eq!(listed.len(), 2, "only this dataflow's params");
    /// # Ok::<(), astrs_store::Error>(())
    /// ```
    pub fn list_params(&self, dataflow: DataflowId) -> Result<Vec<(ParamKey, ParamRecord)>> {
        let prefix = keys::param_prefix(dataflow);
        let mut out = Vec::new();
        for item in self.backend.prefix_scan(&prefix)? {
            let (raw_key, raw_value) = item?;
            let (_, key) = keys::decode_param_key(&raw_key)?;
            let record = from_json("param record", &raw_key, &raw_value)?;
            out.push((key, record));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::store::CoordinatorStore;

    fn store() -> CoordinatorStore<oxistore_kv_redb::RedbStore> {
        CoordinatorStore::open_in_memory().unwrap()
    }

    #[test]
    fn set_then_get_round_trips_the_value() {
        let store = store();
        let dataflow = DataflowId::generate();
        let key = ParamKey::new("gain").unwrap();
        store
            .set_param(dataflow, key.clone(), serde_json::json!({"db": 3}))
            .unwrap();
        let record = store.get_param(dataflow, &key).unwrap().unwrap();
        assert_eq!(record.value().unwrap(), serde_json::json!({"db": 3}));
        assert_eq!(record.revision, 1);
    }

    #[test]
    fn get_on_an_unset_key_is_none() {
        let store = store();
        let dataflow = DataflowId::generate();
        let key = ParamKey::new("missing").unwrap();
        assert!(store.get_param(dataflow, &key).unwrap().is_none());
    }

    #[test]
    fn revision_increments_and_created_at_is_pinned_to_the_first_write() {
        let store = store();
        let dataflow = DataflowId::generate();
        let key = ParamKey::new("gain").unwrap();

        store
            .set_param(dataflow, key.clone(), serde_json::json!(1))
            .unwrap();
        let first = store.get_param(dataflow, &key).unwrap().unwrap();

        store
            .set_param(dataflow, key.clone(), serde_json::json!(2))
            .unwrap();
        let second = store.get_param(dataflow, &key).unwrap().unwrap();

        assert_eq!(first.revision, 1);
        assert_eq!(second.revision, 2);
        assert_eq!(first.created_at, second.created_at);
        // `astrs_time::HlcClock::now()` guarantees a *strictly* increasing
        // timestamp on every call, regardless of wall-clock behavior (see
        // its docs) — `>=` would also pass for a clock that stalled, which
        // is exactly the bug this test exists to catch.
        assert!(
            second.updated_at > first.updated_at,
            "HLC timestamps must be strictly increasing across writes to the same key"
        );
    }

    #[test]
    fn delete_removes_the_key_and_resets_future_revisions() {
        let store = store();
        let dataflow = DataflowId::generate();
        let key = ParamKey::new("gain").unwrap();

        store
            .set_param(dataflow, key.clone(), serde_json::json!(1))
            .unwrap();
        assert!(store.delete_param(dataflow, &key).unwrap().is_some());
        assert!(store.get_param(dataflow, &key).unwrap().is_none());

        store
            .set_param(dataflow, key.clone(), serde_json::json!(1))
            .unwrap();
        assert_eq!(
            store.get_param(dataflow, &key).unwrap().unwrap().revision,
            1
        );
    }

    #[test]
    fn delete_on_an_absent_key_is_a_harmless_no_op() {
        let store = store();
        let dataflow = DataflowId::generate();
        let key = ParamKey::new("gain").unwrap();
        assert_eq!(store.delete_param(dataflow, &key).unwrap(), None);
    }

    #[test]
    fn list_params_is_scoped_to_its_dataflow_and_sorted_by_key() {
        let store = store();
        let dataflow = DataflowId::generate();
        let other = DataflowId::generate();
        store
            .set_param(dataflow, ParamKey::new("b").unwrap(), serde_json::json!(2))
            .unwrap();
        store
            .set_param(dataflow, ParamKey::new("a").unwrap(), serde_json::json!(1))
            .unwrap();
        store
            .set_param(other, ParamKey::new("a").unwrap(), serde_json::json!(9))
            .unwrap();

        let listed = store.list_params(dataflow).unwrap();
        let keys: Vec<String> = listed.iter().map(|(k, _)| k.to_string()).collect();
        assert_eq!(keys, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn list_params_on_a_dataflow_with_no_params_is_empty() {
        let store = store();
        assert!(
            store
                .list_params(DataflowId::generate())
                .unwrap()
                .is_empty()
        );
    }

    /// `backend` is a private field precisely so nothing outside this crate
    /// can write a bucket row that bypasses the mutation log — see
    /// `CoordinatorStore::apply_upsert`'s docs. That is also exactly why
    /// this test has to live here, inside the crate, rather than as an
    /// integration test under `tests/`: it is the only place that can
    /// simulate on-disk corruption (or a foreign write) to prove the public
    /// read path survives it.
    #[test]
    fn a_corrupted_param_row_is_a_typed_error_not_a_panic() {
        let store = store();
        let dataflow = DataflowId::generate();
        let key = ParamKey::new("gain").unwrap();
        store
            .backend
            .put(&keys::param_key(dataflow, &key), b"not json")
            .unwrap();

        let err = store.get_param(dataflow, &key).unwrap_err();
        assert!(matches!(
            err,
            Error::JsonDecode {
                what: "param record",
                ..
            }
        ));
    }

    #[test]
    fn list_params_surfaces_a_corrupt_row_as_an_error_instead_of_skipping_it() {
        let store = store();
        let dataflow = DataflowId::generate();
        store
            .set_param(
                dataflow,
                ParamKey::new("good").unwrap(),
                serde_json::json!(1),
            )
            .unwrap();
        store
            .backend
            .put(
                &keys::param_key(dataflow, &ParamKey::new("bad").unwrap()),
                b"not json",
            )
            .unwrap();

        // A scan that quietly dropped the undecodable row would hide
        // corruption from the coordinator forever; it must error instead.
        let err = store.list_params(dataflow).unwrap_err();
        assert!(matches!(err, Error::JsonDecode { .. }));
    }
}
