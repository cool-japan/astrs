//! The dataflow registry: one [`DataflowMeta`] row per dataflow plus one
//! [`NodeStatusRecord`] row per `(dataflow, node)`, composed into a
//! [`DataflowSnapshot`] on request.

use std::collections::BTreeMap;

use astrs_time::HlcTimestamp;
use astrs_wire::{DaemonId, DataflowId, DataflowStatus, NodeId, NodeInfo};
use oxistore_core::KvStore;

use crate::error::{Error, Result};
use crate::keys;
use crate::record::{DataflowMeta, DataflowSnapshot, MutationOp, MutationSeq, NodeStatusRecord};
use crate::store::{CoordinatorStore, from_json, to_json};

impl<S: KvStore + Clone> CoordinatorStore<S> {
    /// Registers a dataflow, or fully replaces an existing registration's
    /// manifest snapshot.
    ///
    /// Resets [`DataflowMeta::status`] to [`DataflowStatus::Pending`],
    /// clears `daemons` and `started_at`: a manifest replace means the
    /// dataflow's placement and lifecycle start over, matching `astrs
    /// start` registering a dataflow for the first time. Use
    /// [`CoordinatorStore::set_dataflow_status`] and
    /// [`CoordinatorStore::set_dataflow_daemons`] for the FSM transitions
    /// and placement updates that follow.
    ///
    /// # Errors
    ///
    /// [`Error::JsonDecode`] if an existing record at this id is on-disk
    /// corrupted. [`Error::Backend`] if the underlying write fails.
    pub fn upsert_dataflow(
        &self,
        id: DataflowId,
        name: Option<String>,
        manifest_json: String,
        node_count: u32,
    ) -> Result<MutationSeq> {
        let bucket_key = keys::dataflow_meta_key(id);
        self.apply_upsert(bucket_key, move |existing, ts| {
            let revision = match existing {
                Some(bytes) => decode_meta(&bytes, id)?.revision + 1,
                None => 1,
            };
            let record = DataflowMeta {
                id,
                name,
                manifest_json,
                status: DataflowStatus::Pending,
                daemons: Vec::new(),
                node_count,
                started_at: None,
                revision,
                updated_at: ts,
            };
            let bytes = to_json("dataflow meta", &record)?;
            Ok((bytes, MutationOp::DataflowMetaPut { record }))
        })
    }

    /// Transitions a registered dataflow's FSM status.
    ///
    /// `started_at`, when `Some`, overwrites [`DataflowMeta::started_at`]
    /// (the caller passes it when transitioning into
    /// [`DataflowStatus::Running`]); `None` leaves the previously recorded
    /// value untouched, which is what every other transition wants.
    ///
    /// Returns `Ok(None)` if `id` is not registered — see
    /// `CoordinatorStore::apply_update_if_present` (crate-private).
    ///
    /// # Errors
    ///
    /// [`Error::JsonDecode`] if the existing record is on-disk corrupted.
    /// [`Error::Backend`] if the underlying write fails.
    pub fn set_dataflow_status(
        &self,
        id: DataflowId,
        status: DataflowStatus,
        started_at: Option<HlcTimestamp>,
    ) -> Result<Option<MutationSeq>> {
        let bucket_key = keys::dataflow_meta_key(id);
        self.apply_update_if_present(bucket_key, move |bytes, ts| {
            let mut record = decode_meta(&bytes, id)?;
            record.status = status;
            if let Some(started_at) = started_at {
                record.started_at = Some(started_at);
            }
            record.revision += 1;
            record.updated_at = ts;
            let bytes = to_json("dataflow meta", &record)?;
            Ok((bytes, MutationOp::DataflowMetaPut { record }))
        })
    }

    /// Replaces the set of daemons hosting a registered dataflow's nodes.
    ///
    /// Returns `Ok(None)` if `id` is not registered.
    ///
    /// # Errors
    ///
    /// As [`CoordinatorStore::set_dataflow_status`].
    pub fn set_dataflow_daemons(
        &self,
        id: DataflowId,
        daemons: Vec<DaemonId>,
    ) -> Result<Option<MutationSeq>> {
        let bucket_key = keys::dataflow_meta_key(id);
        self.apply_update_if_present(bucket_key, move |bytes, ts| {
            let mut record = decode_meta(&bytes, id)?;
            record.daemons = daemons;
            record.revision += 1;
            record.updated_at = ts;
            let bytes = to_json("dataflow meta", &record)?;
            Ok((bytes, MutationOp::DataflowMetaPut { record }))
        })
    }

    /// Reads a dataflow's registration record, if it exists.
    ///
    /// # Errors
    ///
    /// [`Error::JsonDecode`] if the stored record is on-disk corrupted.
    /// [`Error::Backend`] if the read fails.
    pub fn get_dataflow_meta(&self, id: DataflowId) -> Result<Option<DataflowMeta>> {
        let bucket_key = keys::dataflow_meta_key(id);
        match self.backend.get(&bucket_key)? {
            Some(bytes) => Ok(Some(from_json("dataflow meta", &bucket_key, &bytes)?)),
            None => Ok(None),
        }
    }

    /// Removes a dataflow's registration record.
    ///
    /// This removes only the [`DataflowMeta`] row, not the dataflow's
    /// [`NodeStatusRecord`] rows — cascading that cleanup (typically once
    /// the coordinator has confirmed every node has actually stopped) is
    /// the caller's responsibility via
    /// [`CoordinatorStore::list_node_status`] +
    /// [`CoordinatorStore::remove_node_status`]. Keeping this a
    /// single-row change keeps its mutation-log record a single
    /// [`MutationOp::DataflowMetaDelete`], consistent with every other
    /// mutating call in this crate changing exactly one bucket row.
    ///
    /// Returns `Ok(None)` if `id` was not registered.
    ///
    /// # Errors
    ///
    /// [`Error::Backend`] if the underlying write fails.
    pub fn remove_dataflow(&self, id: DataflowId) -> Result<Option<MutationSeq>> {
        let bucket_key = keys::dataflow_meta_key(id);
        self.apply_delete_if_present(&bucket_key, MutationOp::DataflowMetaDelete { dataflow: id })
    }

    /// Lists every registered dataflow's metadata.
    ///
    /// # Errors
    ///
    /// [`Error::JsonDecode`] if a stored record is on-disk corrupted.
    /// [`Error::Backend`] if the scan fails.
    pub fn list_dataflows(&self) -> Result<Vec<DataflowMeta>> {
        let prefix = keys::dataflow_meta_bucket_prefix();
        let mut out = Vec::new();
        for item in self.backend.prefix_scan(&prefix)? {
            let (raw_key, raw_value) = item?;
            out.push(from_json("dataflow meta", &raw_key, &raw_value)?);
        }
        Ok(out)
    }

    /// Creates or updates one node's status.
    ///
    /// The `(dataflow, node)` key is taken from `info.dataflow` /
    /// `info.node`.
    ///
    /// # Errors
    ///
    /// [`Error::JsonDecode`] if an existing record at this key is on-disk
    /// corrupted. [`Error::Backend`] if the underlying write fails.
    pub fn set_node_status(&self, info: NodeInfo) -> Result<MutationSeq> {
        let dataflow = info.dataflow;
        let node = info.node.clone();
        let bucket_key = keys::node_status_key(dataflow, &node);
        self.apply_upsert(bucket_key, move |existing, ts| {
            let revision = match existing {
                Some(bytes) => decode_node_status(&bytes, dataflow, &node)?.revision + 1,
                None => 1,
            };
            let record = NodeStatusRecord {
                info,
                revision,
                updated_at: ts,
            };
            let bytes = to_json("node status", &record)?;
            Ok((bytes, MutationOp::NodeStatusPut { record }))
        })
    }

    /// Reads one node's status, if it exists.
    ///
    /// # Errors
    ///
    /// [`Error::JsonDecode`] if the stored record is on-disk corrupted.
    /// [`Error::Backend`] if the read fails.
    pub fn get_node_status(
        &self,
        dataflow: DataflowId,
        node: &NodeId,
    ) -> Result<Option<NodeStatusRecord>> {
        let bucket_key = keys::node_status_key(dataflow, node);
        match self.backend.get(&bucket_key)? {
            Some(bytes) => Ok(Some(from_json("node status", &bucket_key, &bytes)?)),
            None => Ok(None),
        }
    }

    /// Removes one node's status.
    ///
    /// Returns `Ok(None)` if the node had no recorded status.
    ///
    /// # Errors
    ///
    /// [`Error::Backend`] if the underlying write fails.
    pub fn remove_node_status(
        &self,
        dataflow: DataflowId,
        node: &NodeId,
    ) -> Result<Option<MutationSeq>> {
        let bucket_key = keys::node_status_key(dataflow, node);
        self.apply_delete_if_present(
            &bucket_key,
            MutationOp::NodeStatusDelete {
                dataflow,
                node: node.clone(),
            },
        )
    }

    /// Lists every node's status within `dataflow`, ordered by node id.
    ///
    /// # Errors
    ///
    /// [`Error::JsonDecode`] if a stored record is on-disk corrupted.
    /// [`Error::Backend`] if the scan fails.
    pub fn list_node_status(&self, dataflow: DataflowId) -> Result<Vec<NodeStatusRecord>> {
        let prefix = keys::node_status_prefix(dataflow);
        let mut out = Vec::new();
        for item in self.backend.prefix_scan(&prefix)? {
            let (raw_key, raw_value) = item?;
            out.push(from_json("node status", &raw_key, &raw_value)?);
        }
        Ok(out)
    }

    /// Composes a dataflow's metadata and every node's status into one
    /// read-only [`DataflowSnapshot`], captured from a single
    /// [`oxistore_core::KvSnapshot`] so the two halves reflect the same
    /// instant even under concurrent writers.
    ///
    /// Returns `Ok(None)` if `dataflow` is not registered (regardless of
    /// whether it happens to still have node-status rows — see
    /// [`CoordinatorStore::remove_dataflow`]).
    ///
    /// # Errors
    ///
    /// [`Error::JsonDecode`] if a stored record is on-disk corrupted.
    /// [`Error::Backend`] if the read fails.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_store::CoordinatorStore;
    /// use astrs_wire::{DataflowId, DataflowStatus};
    ///
    /// let store = CoordinatorStore::open_in_memory()?;
    /// let dataflow = DataflowId::generate();
    /// assert!(store.dataflow_snapshot(dataflow)?.is_none());
    ///
    /// store.upsert_dataflow(dataflow, Some("demo".into()), "{}".into(), 0)?;
    /// let snapshot = store.dataflow_snapshot(dataflow)?.expect("just registered");
    /// assert_eq!(snapshot.meta.status, DataflowStatus::Pending);
    /// assert!(snapshot.nodes.is_empty());
    /// # Ok::<(), astrs_store::Error>(())
    /// ```
    pub fn dataflow_snapshot(&self, dataflow: DataflowId) -> Result<Option<DataflowSnapshot>> {
        let snapshot = self.backend.snapshot()?;
        let meta_key = keys::dataflow_meta_key(dataflow);
        let meta = match snapshot.get(&meta_key)? {
            Some(bytes) => from_json("dataflow meta", &meta_key, &bytes)?,
            None => return Ok(None),
        };
        let prefix = keys::node_status_prefix(dataflow);
        let mut nodes = BTreeMap::new();
        for item in snapshot.prefix_scan(&prefix)? {
            let (raw_key, raw_value) = item?;
            let record: NodeStatusRecord = from_json("node status", &raw_key, &raw_value)?;
            nodes.insert(record.info.node.clone(), record);
        }
        Ok(Some(DataflowSnapshot { meta, nodes }))
    }
}

fn decode_meta(bytes: &[u8], id: DataflowId) -> Result<DataflowMeta> {
    serde_json::from_slice(bytes).map_err(|source| Error::JsonDecode {
        what: "dataflow meta",
        key_preview: id.to_string(),
        source,
    })
}

fn decode_node_status(
    bytes: &[u8],
    dataflow: DataflowId,
    node: &NodeId,
) -> Result<NodeStatusRecord> {
    serde_json::from_slice(bytes).map_err(|source| Error::JsonDecode {
        what: "node status",
        key_preview: format!("{dataflow}/{node}"),
        source,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::{NodeExitCause, NodeRunState};

    fn store() -> CoordinatorStore<oxistore_kv_redb::RedbStore> {
        CoordinatorStore::open_in_memory().unwrap()
    }

    fn node_info(dataflow: DataflowId, node: &str, daemon: DaemonId) -> NodeInfo {
        NodeInfo {
            dataflow,
            node: NodeId::new(node).unwrap(),
            daemon,
            state: NodeRunState::Spawning,
            pid: None,
            generation: 1,
            restart_count: 0,
            inputs: BTreeMap::new(),
            outputs: BTreeMap::new(),
            started_at: None,
            exit_cause: None::<NodeExitCause>,
        }
    }

    #[test]
    fn upsert_registers_a_pending_dataflow() {
        let store = store();
        let id = DataflowId::generate();
        store
            .upsert_dataflow(id, Some("demo".to_owned()), "{}".to_owned(), 2)
            .unwrap();
        let meta = store.get_dataflow_meta(id).unwrap().unwrap();
        assert_eq!(meta.status, DataflowStatus::Pending);
        assert_eq!(meta.node_count, 2);
        assert_eq!(meta.revision, 1);
        assert!(meta.daemons.is_empty());
        assert!(meta.started_at.is_none());
    }

    #[test]
    fn re_upserting_bumps_revision_and_resets_lifecycle_fields() {
        let store = store();
        let id = DataflowId::generate();
        store.upsert_dataflow(id, None, "{}".to_owned(), 1).unwrap();
        store
            .set_dataflow_status(id, DataflowStatus::Running, Some(HlcTimestamp::new(1, 0)))
            .unwrap();

        store
            .upsert_dataflow(id, None, "{\"v\":2}".to_owned(), 3)
            .unwrap();
        let meta = store.get_dataflow_meta(id).unwrap().unwrap();
        // Revision keeps climbing across *every* mutating call on this
        // dataflow — upsert (1), the status change (2), this upsert (3) —
        // it is not reset by re-registering.
        assert_eq!(meta.revision, 3);
        assert_eq!(meta.status, DataflowStatus::Pending);
        assert!(meta.started_at.is_none());
        assert_eq!(meta.node_count, 3);
    }

    #[test]
    fn set_status_updates_started_at_only_when_given() {
        let store = store();
        let id = DataflowId::generate();
        store.upsert_dataflow(id, None, "{}".to_owned(), 1).unwrap();

        let ts = HlcTimestamp::new(42, 0);
        store
            .set_dataflow_status(id, DataflowStatus::Running, Some(ts))
            .unwrap();
        assert_eq!(
            store.get_dataflow_meta(id).unwrap().unwrap().started_at,
            Some(ts)
        );

        store
            .set_dataflow_status(id, DataflowStatus::Stopping, None)
            .unwrap();
        let meta = store.get_dataflow_meta(id).unwrap().unwrap();
        assert_eq!(meta.status, DataflowStatus::Stopping);
        assert_eq!(meta.started_at, Some(ts), "unrelated transition keeps it");
    }

    #[test]
    fn set_status_on_an_unregistered_dataflow_is_none() {
        let store = store();
        let result = store
            .set_dataflow_status(DataflowId::generate(), DataflowStatus::Running, None)
            .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn set_daemons_replaces_the_placement_list() {
        let store = store();
        let id = DataflowId::generate();
        store.upsert_dataflow(id, None, "{}".to_owned(), 1).unwrap();
        let daemon = DaemonId::generate(None);
        store
            .set_dataflow_daemons(id, vec![daemon.clone()])
            .unwrap();
        assert_eq!(
            store.get_dataflow_meta(id).unwrap().unwrap().daemons,
            vec![daemon]
        );
    }

    #[test]
    fn remove_dataflow_deletes_only_the_meta_row() {
        let store = store();
        let id = DataflowId::generate();
        let daemon = DaemonId::generate(None);
        store.upsert_dataflow(id, None, "{}".to_owned(), 1).unwrap();
        store.set_node_status(node_info(id, "cam", daemon)).unwrap();

        assert!(store.remove_dataflow(id).unwrap().is_some());
        assert!(store.get_dataflow_meta(id).unwrap().is_none());
        assert_eq!(
            store.list_node_status(id).unwrap().len(),
            1,
            "nodes survive"
        );
    }

    #[test]
    fn list_dataflows_returns_every_registration() {
        let store = store();
        store
            .upsert_dataflow(DataflowId::generate(), None, "{}".to_owned(), 1)
            .unwrap();
        store
            .upsert_dataflow(DataflowId::generate(), None, "{}".to_owned(), 2)
            .unwrap();
        assert_eq!(store.list_dataflows().unwrap().len(), 2);
    }

    #[test]
    fn node_status_round_trips_and_bumps_revision() {
        let store = store();
        let id = DataflowId::generate();
        let daemon = DaemonId::generate(None);

        store
            .set_node_status(node_info(id, "cam", daemon.clone()))
            .unwrap();
        let first = store
            .get_node_status(id, &NodeId::new("cam").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(first.revision, 1);

        let mut updated = node_info(id, "cam", daemon);
        updated.state = NodeRunState::Running;
        updated.pid = Some(123);
        store.set_node_status(updated).unwrap();
        let second = store
            .get_node_status(id, &NodeId::new("cam").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(second.revision, 2);
        assert_eq!(second.info.state, NodeRunState::Running);
    }

    #[test]
    fn remove_node_status_is_none_when_absent() {
        let store = store();
        let id = DataflowId::generate();
        assert!(
            store
                .remove_node_status(id, &NodeId::new("ghost").unwrap())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn list_node_status_is_scoped_and_sorted() {
        let store = store();
        let id = DataflowId::generate();
        let other = DataflowId::generate();
        let daemon = DaemonId::generate(None);
        store
            .set_node_status(node_info(id, "b", daemon.clone()))
            .unwrap();
        store
            .set_node_status(node_info(id, "a", daemon.clone()))
            .unwrap();
        store
            .set_node_status(node_info(other, "a", daemon))
            .unwrap();

        let listed = store.list_node_status(id).unwrap();
        let names: Vec<String> = listed.iter().map(|r| r.info.node.to_string()).collect();
        assert_eq!(names, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn dataflow_snapshot_is_none_when_unregistered() {
        let store = store();
        assert!(
            store
                .dataflow_snapshot(DataflowId::generate())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn dataflow_snapshot_composes_meta_and_nodes() {
        let store = store();
        let id = DataflowId::generate();
        let daemon = DaemonId::generate(None);
        store
            .upsert_dataflow(id, Some("demo".to_owned()), "{}".to_owned(), 1)
            .unwrap();
        store.set_node_status(node_info(id, "cam", daemon)).unwrap();

        let snapshot = store.dataflow_snapshot(id).unwrap().unwrap();
        assert_eq!(snapshot.meta.name.as_deref(), Some("demo"));
        assert_eq!(snapshot.nodes.len(), 1);
        assert!(snapshot.nodes.contains_key(&NodeId::new("cam").unwrap()));
    }
}
