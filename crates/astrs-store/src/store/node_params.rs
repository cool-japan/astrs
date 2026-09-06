//! The node-scoped parameters bucket: `(dataflow, node, key) -> JSON value`.
//!
//! The mirror of [`crate::store::params`] one level further down the
//! `ParamScope` hierarchy (`astrs_wire::ParamScope::Node`): a coordinator
//! serving `astrs param set --node camera exposure 12` needs a bucket keyed
//! by node as well as by dataflow, which the two-component params bucket
//! cannot express — see `crate::keys`'s `node_param_*` functions for the
//! on-disk key layout this builds on.

use astrs_wire::{DataflowId, NodeId, ParamKey};
use oxistore_core::KvStore;

use crate::error::{Error, Result};
use crate::keys;
use crate::record::{MutationOp, MutationSeq, ParamRecord};
use crate::store::{CoordinatorStore, from_json, to_json};

impl<S: KvStore + Clone> CoordinatorStore<S> {
    /// Creates or updates one node-scoped parameter.
    ///
    /// As [`CoordinatorStore::set_param`], but keyed by `(dataflow, node,
    /// key)` rather than `(dataflow, key)`: the first write to a triple
    /// starts [`ParamRecord::revision`] at `1`; every later write to the
    /// same triple increments it, and [`ParamRecord::created_at`] is
    /// carried forward from the first write.
    ///
    /// # Errors
    ///
    /// [`Error::JsonEncode`] if `value` cannot be serialized (unreachable
    /// for a well-formed [`serde_json::Value`]). [`Error::JsonDecode`] if a
    /// prior record at this key is on-disk corrupted. [`Error::Backend`] if
    /// the underlying write fails.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_store::CoordinatorStore;
    /// use astrs_wire::{DataflowId, NodeId, ParamKey};
    ///
    /// let store = CoordinatorStore::open_in_memory()?;
    /// let dataflow = DataflowId::generate();
    /// let node = NodeId::new("camera")?;
    /// let key = ParamKey::new("exposure")?;
    ///
    /// store.set_node_param(dataflow, node.clone(), key.clone(), serde_json::json!(12))?;
    /// let record = store.get_node_param(dataflow, &node, &key)?.expect("just written");
    /// assert_eq!(record.value()?, serde_json::json!(12));
    /// # Ok::<(), astrs_store::Error>(())
    /// ```
    pub fn set_node_param(
        &self,
        dataflow: DataflowId,
        node: NodeId,
        key: ParamKey,
        value: serde_json::Value,
    ) -> Result<MutationSeq> {
        let value_json = serde_json::to_string(&value).map_err(|source| Error::JsonEncode {
            what: "node param value",
            source,
        })?;
        let bucket_key = keys::node_param_key(dataflow, &node, &key);
        self.apply_upsert(bucket_key, move |existing, ts| {
            let (revision, created_at) = match existing {
                Some(bytes) => {
                    let old: ParamRecord =
                        serde_json::from_slice(&bytes).map_err(|source| Error::JsonDecode {
                            what: "node param record",
                            key_preview: format!("{dataflow}/{node}/{key}"),
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
            let bytes = to_json("node param record", &record)?;
            let op = MutationOp::NodeParamPut {
                dataflow,
                node,
                key,
                record,
            };
            Ok((bytes, op))
        })
    }

    /// Reads one node-scoped parameter's current record, if it exists.
    ///
    /// # Errors
    ///
    /// [`Error::JsonDecode`] if the stored bytes are not a valid
    /// [`ParamRecord`] (on-disk corruption). [`Error::Backend`] if the read
    /// fails.
    pub fn get_node_param(
        &self,
        dataflow: DataflowId,
        node: &NodeId,
        key: &ParamKey,
    ) -> Result<Option<ParamRecord>> {
        let bucket_key = keys::node_param_key(dataflow, node, key);
        match self.backend.get(&bucket_key)? {
            Some(bytes) => Ok(Some(from_json("node param record", &bucket_key, &bytes)?)),
            None => Ok(None),
        }
    }

    /// Deletes one node-scoped parameter.
    ///
    /// Returns `Ok(None)` if the key was already absent.
    ///
    /// # Errors
    ///
    /// [`Error::Backend`] if the underlying write fails.
    pub fn delete_node_param(
        &self,
        dataflow: DataflowId,
        node: &NodeId,
        key: &ParamKey,
    ) -> Result<Option<MutationSeq>> {
        let bucket_key = keys::node_param_key(dataflow, node, key);
        self.apply_delete_if_present(
            &bucket_key,
            MutationOp::NodeParamDelete {
                dataflow,
                node: node.clone(),
                key: key.clone(),
            },
        )
    }

    /// Lists every parameter currently set for `(dataflow, node)`, ordered
    /// by key.
    ///
    /// # Errors
    ///
    /// [`Error::CorruptKey`] if a stored key is malformed.
    /// [`Error::JsonDecode`] if a stored value is malformed.
    /// [`Error::Backend`] if the scan fails.
    pub fn list_node_params(
        &self,
        dataflow: DataflowId,
        node: &NodeId,
    ) -> Result<Vec<(ParamKey, ParamRecord)>> {
        let prefix = keys::node_param_prefix(dataflow, node);
        let mut out = Vec::new();
        for item in self.backend.prefix_scan(&prefix)? {
            let (raw_key, raw_value) = item?;
            let (_, _, key) = keys::decode_node_param_key(&raw_key)?;
            let record = from_json("node param record", &raw_key, &raw_value)?;
            out.push((key, record));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn store() -> CoordinatorStore<oxistore_kv_redb::RedbStore> {
        CoordinatorStore::open_in_memory().unwrap()
    }

    #[test]
    fn set_then_get_round_trips_the_value() {
        let store = store();
        let dataflow = DataflowId::generate();
        let node = NodeId::new("camera").unwrap();
        let key = ParamKey::new("exposure").unwrap();
        store
            .set_node_param(dataflow, node.clone(), key.clone(), serde_json::json!(12))
            .unwrap();
        let record = store
            .get_node_param(dataflow, &node, &key)
            .unwrap()
            .unwrap();
        assert_eq!(record.value().unwrap(), serde_json::json!(12));
        assert_eq!(record.revision, 1);
    }

    #[test]
    fn get_on_an_unset_key_is_none() {
        let store = store();
        let dataflow = DataflowId::generate();
        let node = NodeId::new("camera").unwrap();
        let key = ParamKey::new("missing").unwrap();
        assert!(
            store
                .get_node_param(dataflow, &node, &key)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn revision_increments_and_created_at_is_pinned_to_the_first_write() {
        let store = store();
        let dataflow = DataflowId::generate();
        let node = NodeId::new("camera").unwrap();
        let key = ParamKey::new("exposure").unwrap();

        store
            .set_node_param(dataflow, node.clone(), key.clone(), serde_json::json!(1))
            .unwrap();
        let first = store
            .get_node_param(dataflow, &node, &key)
            .unwrap()
            .unwrap();

        store
            .set_node_param(dataflow, node.clone(), key.clone(), serde_json::json!(2))
            .unwrap();
        let second = store
            .get_node_param(dataflow, &node, &key)
            .unwrap()
            .unwrap();

        assert_eq!(first.revision, 1);
        assert_eq!(second.revision, 2);
        assert_eq!(first.created_at, second.created_at);
        assert!(second.updated_at > first.updated_at);
    }

    #[test]
    fn delete_removes_the_key_and_resets_future_revisions() {
        let store = store();
        let dataflow = DataflowId::generate();
        let node = NodeId::new("camera").unwrap();
        let key = ParamKey::new("exposure").unwrap();

        store
            .set_node_param(dataflow, node.clone(), key.clone(), serde_json::json!(1))
            .unwrap();
        assert!(
            store
                .delete_node_param(dataflow, &node, &key)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .get_node_param(dataflow, &node, &key)
                .unwrap()
                .is_none()
        );

        store
            .set_node_param(dataflow, node.clone(), key.clone(), serde_json::json!(1))
            .unwrap();
        assert_eq!(
            store
                .get_node_param(dataflow, &node, &key)
                .unwrap()
                .unwrap()
                .revision,
            1
        );
    }

    #[test]
    fn delete_on_an_absent_key_is_a_harmless_no_op() {
        let store = store();
        let dataflow = DataflowId::generate();
        let node = NodeId::new("camera").unwrap();
        let key = ParamKey::new("exposure").unwrap();
        assert_eq!(
            store.delete_node_param(dataflow, &node, &key).unwrap(),
            None
        );
    }

    #[test]
    fn params_are_scoped_per_node_within_the_same_dataflow() {
        let store = store();
        let dataflow = DataflowId::generate();
        let camera = NodeId::new("camera").unwrap();
        let lidar = NodeId::new("lidar").unwrap();
        let key = ParamKey::new("rate").unwrap();

        store
            .set_node_param(dataflow, camera.clone(), key.clone(), serde_json::json!(30))
            .unwrap();
        store
            .set_node_param(dataflow, lidar.clone(), key.clone(), serde_json::json!(10))
            .unwrap();

        assert_eq!(
            store
                .get_node_param(dataflow, &camera, &key)
                .unwrap()
                .unwrap()
                .value()
                .unwrap(),
            serde_json::json!(30)
        );
        assert_eq!(
            store
                .get_node_param(dataflow, &lidar, &key)
                .unwrap()
                .unwrap()
                .value()
                .unwrap(),
            serde_json::json!(10)
        );
    }

    #[test]
    fn list_node_params_is_scoped_and_sorted() {
        let store = store();
        let dataflow = DataflowId::generate();
        let other_dataflow = DataflowId::generate();
        let camera = NodeId::new("camera").unwrap();
        let lidar = NodeId::new("lidar").unwrap();

        store
            .set_node_param(
                dataflow,
                camera.clone(),
                ParamKey::new("b").unwrap(),
                serde_json::json!(2),
            )
            .unwrap();
        store
            .set_node_param(
                dataflow,
                camera.clone(),
                ParamKey::new("a").unwrap(),
                serde_json::json!(1),
            )
            .unwrap();
        store
            .set_node_param(
                dataflow,
                lidar,
                ParamKey::new("a").unwrap(),
                serde_json::json!(9),
            )
            .unwrap();
        store
            .set_node_param(
                other_dataflow,
                camera.clone(),
                ParamKey::new("a").unwrap(),
                serde_json::json!(9),
            )
            .unwrap();

        let listed = store.list_node_params(dataflow, &camera).unwrap();
        let keys: Vec<String> = listed.iter().map(|(k, _)| k.to_string()).collect();
        assert_eq!(keys, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn list_node_params_on_a_node_with_no_params_is_empty() {
        let store = store();
        assert!(
            store
                .list_node_params(DataflowId::generate(), &NodeId::new("ghost").unwrap())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn node_scoped_and_dataflow_scoped_params_do_not_collide() {
        // The same `(dataflow, key)` text, one written through the plain
        // dataflow-scoped bucket and one through the node-scoped bucket,
        // must land in different rows.
        let store = store();
        let dataflow = DataflowId::generate();
        let key = ParamKey::new("gain").unwrap();
        let node = NodeId::new("camera").unwrap();

        store
            .set_param(dataflow, key.clone(), serde_json::json!("dataflow"))
            .unwrap();
        store
            .set_node_param(
                dataflow,
                node.clone(),
                key.clone(),
                serde_json::json!("node"),
            )
            .unwrap();

        assert_eq!(
            store
                .get_param(dataflow, &key)
                .unwrap()
                .unwrap()
                .value()
                .unwrap(),
            serde_json::json!("dataflow")
        );
        assert_eq!(
            store
                .get_node_param(dataflow, &node, &key)
                .unwrap()
                .unwrap()
                .value()
                .unwrap(),
            serde_json::json!("node")
        );
    }

    #[test]
    fn a_corrupted_node_param_row_is_a_typed_error_not_a_panic() {
        let store = store();
        let dataflow = DataflowId::generate();
        let node = NodeId::new("camera").unwrap();
        let key = ParamKey::new("gain").unwrap();
        store
            .backend
            .put(&keys::node_param_key(dataflow, &node, &key), b"not json")
            .unwrap();

        let err = store.get_node_param(dataflow, &node, &key).unwrap_err();
        assert!(matches!(
            err,
            Error::JsonDecode {
                what: "node param record",
                ..
            }
        ));
    }
}
