//! An async facade over [`CoordinatorStore`], pushing every blocking
//! backend operation through [`tokio::task::spawn_blocking`].

use std::sync::Arc;

use astrs_wire::{DaemonId, DaemonInfo, DataflowId, DataflowStatus, NodeId, NodeInfo, ParamKey};
use oxistore_core::KvStore;
use oxistore_kv_redb::RedbStore;

use crate::error::Result;
use crate::record::{
    BuildCacheEntry, BuildCacheKey, CatchUpBatch, DaemonRecord, DataflowMeta, DataflowSnapshot,
    MutationRecord, MutationSeq, NodeStatusRecord, ParamRecord,
};
use crate::store::CoordinatorStore;

/// [`AsyncCoordinatorStore`] over the real-deployment [`RedbStore`] backend
/// — the type the coordinator process actually uses.
pub type AsyncStore = AsyncCoordinatorStore<RedbStore>;

/// An async facade over [`CoordinatorStore`].
///
/// [`CoordinatorStore`]'s methods are synchronous — the backend (`redb`) is
/// a blocking embedded database — so every method here runs the
/// corresponding sync call inside [`tokio::task::spawn_blocking`], keeping
/// the coordinator's async event loop (blueprint §4.3: one merged
/// `tokio::select!` loop over peer connections, node messages and timers)
/// from ever blocking on store I/O.
///
/// Cloning is cheap: [`CoordinatorStore`] is itself an `Arc`-backed handle,
/// and this wrapper clones one more `Arc` around it, so every clone shares
/// the same backend, clock and write gate.
#[derive(Clone)]
pub struct AsyncCoordinatorStore<S: KvStore + Clone> {
    inner: Arc<CoordinatorStore<S>>,
}

impl<S: KvStore + Clone + 'static> AsyncCoordinatorStore<S> {
    /// Wraps a [`CoordinatorStore`] for use from async code.
    #[must_use]
    pub fn new(inner: CoordinatorStore<S>) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Borrows the synchronous store underneath.
    ///
    /// For callers already on a blocking-safe thread — inside another
    /// `spawn_blocking` closure, a test, or `astrs run`'s embedded
    /// single-process mode — that would rather call it directly than pay
    /// for a task hop.
    #[must_use]
    pub fn sync(&self) -> &CoordinatorStore<S> {
        &self.inner
    }

    /// Runs `f` against the wrapped store on the blocking thread pool.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Join`] if the blocking task panicked or was
    /// cancelled; otherwise whatever `f` returns.
    async fn run<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&CoordinatorStore<S>) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || f(&inner)).await?
    }

    // ---------------------------------------------------------------
    // params — async wrappers for `crate::store::params`
    // ---------------------------------------------------------------

    /// Async wrapper for [`CoordinatorStore::set_param`].
    pub async fn set_param(
        &self,
        dataflow: DataflowId,
        key: ParamKey,
        value: serde_json::Value,
    ) -> Result<MutationSeq> {
        self.run(move |store| store.set_param(dataflow, key, value))
            .await
    }

    /// Async wrapper for [`CoordinatorStore::get_param`].
    pub async fn get_param(
        &self,
        dataflow: DataflowId,
        key: ParamKey,
    ) -> Result<Option<ParamRecord>> {
        self.run(move |store| store.get_param(dataflow, &key)).await
    }

    /// Async wrapper for [`CoordinatorStore::delete_param`].
    pub async fn delete_param(
        &self,
        dataflow: DataflowId,
        key: ParamKey,
    ) -> Result<Option<MutationSeq>> {
        self.run(move |store| store.delete_param(dataflow, &key))
            .await
    }

    /// Async wrapper for [`CoordinatorStore::list_params`].
    pub async fn list_params(&self, dataflow: DataflowId) -> Result<Vec<(ParamKey, ParamRecord)>> {
        self.run(move |store| store.list_params(dataflow)).await
    }

    // ---------------------------------------------------------------
    // node-scoped params — async wrappers for `crate::store::node_params`
    // ---------------------------------------------------------------

    /// Async wrapper for [`CoordinatorStore::set_node_param`].
    pub async fn set_node_param(
        &self,
        dataflow: DataflowId,
        node: NodeId,
        key: ParamKey,
        value: serde_json::Value,
    ) -> Result<MutationSeq> {
        self.run(move |store| store.set_node_param(dataflow, node, key, value))
            .await
    }

    /// Async wrapper for [`CoordinatorStore::get_node_param`].
    pub async fn get_node_param(
        &self,
        dataflow: DataflowId,
        node: NodeId,
        key: ParamKey,
    ) -> Result<Option<ParamRecord>> {
        self.run(move |store| store.get_node_param(dataflow, &node, &key))
            .await
    }

    /// Async wrapper for [`CoordinatorStore::delete_node_param`].
    pub async fn delete_node_param(
        &self,
        dataflow: DataflowId,
        node: NodeId,
        key: ParamKey,
    ) -> Result<Option<MutationSeq>> {
        self.run(move |store| store.delete_node_param(dataflow, &node, &key))
            .await
    }

    /// Async wrapper for [`CoordinatorStore::list_node_params`].
    pub async fn list_node_params(
        &self,
        dataflow: DataflowId,
        node: NodeId,
    ) -> Result<Vec<(ParamKey, ParamRecord)>> {
        self.run(move |store| store.list_node_params(dataflow, &node))
            .await
    }

    // ---------------------------------------------------------------
    // dataflow registry — async wrappers for `crate::store::dataflow`
    // ---------------------------------------------------------------

    /// Async wrapper for [`CoordinatorStore::upsert_dataflow`].
    pub async fn upsert_dataflow(
        &self,
        id: DataflowId,
        name: Option<String>,
        manifest_json: String,
        node_count: u32,
    ) -> Result<MutationSeq> {
        self.run(move |store| store.upsert_dataflow(id, name, manifest_json, node_count))
            .await
    }

    /// Async wrapper for [`CoordinatorStore::set_dataflow_status`].
    pub async fn set_dataflow_status(
        &self,
        id: DataflowId,
        status: DataflowStatus,
        started_at: Option<astrs_time::HlcTimestamp>,
    ) -> Result<Option<MutationSeq>> {
        self.run(move |store| store.set_dataflow_status(id, status, started_at))
            .await
    }

    /// Async wrapper for [`CoordinatorStore::set_dataflow_daemons`].
    pub async fn set_dataflow_daemons(
        &self,
        id: DataflowId,
        daemons: Vec<DaemonId>,
    ) -> Result<Option<MutationSeq>> {
        self.run(move |store| store.set_dataflow_daemons(id, daemons))
            .await
    }

    /// Async wrapper for [`CoordinatorStore::get_dataflow_meta`].
    pub async fn get_dataflow_meta(&self, id: DataflowId) -> Result<Option<DataflowMeta>> {
        self.run(move |store| store.get_dataflow_meta(id)).await
    }

    /// Async wrapper for [`CoordinatorStore::remove_dataflow`].
    pub async fn remove_dataflow(&self, id: DataflowId) -> Result<Option<MutationSeq>> {
        self.run(move |store| store.remove_dataflow(id)).await
    }

    /// Async wrapper for [`CoordinatorStore::list_dataflows`].
    pub async fn list_dataflows(&self) -> Result<Vec<DataflowMeta>> {
        self.run(|store| store.list_dataflows()).await
    }

    /// Async wrapper for [`CoordinatorStore::set_node_status`].
    pub async fn set_node_status(&self, info: NodeInfo) -> Result<MutationSeq> {
        self.run(move |store| store.set_node_status(info)).await
    }

    /// Async wrapper for [`CoordinatorStore::get_node_status`].
    pub async fn get_node_status(
        &self,
        dataflow: DataflowId,
        node: NodeId,
    ) -> Result<Option<NodeStatusRecord>> {
        self.run(move |store| store.get_node_status(dataflow, &node))
            .await
    }

    /// Async wrapper for [`CoordinatorStore::remove_node_status`].
    pub async fn remove_node_status(
        &self,
        dataflow: DataflowId,
        node: NodeId,
    ) -> Result<Option<MutationSeq>> {
        self.run(move |store| store.remove_node_status(dataflow, &node))
            .await
    }

    /// Async wrapper for [`CoordinatorStore::list_node_status`].
    pub async fn list_node_status(&self, dataflow: DataflowId) -> Result<Vec<NodeStatusRecord>> {
        self.run(move |store| store.list_node_status(dataflow))
            .await
    }

    /// Async wrapper for [`CoordinatorStore::dataflow_snapshot`].
    pub async fn dataflow_snapshot(
        &self,
        dataflow: DataflowId,
    ) -> Result<Option<DataflowSnapshot>> {
        self.run(move |store| store.dataflow_snapshot(dataflow))
            .await
    }

    // ---------------------------------------------------------------
    // daemon registry — async wrappers for `crate::store::daemon`
    // ---------------------------------------------------------------

    /// Async wrapper for [`CoordinatorStore::upsert_daemon`].
    pub async fn upsert_daemon(&self, info: DaemonInfo) -> Result<MutationSeq> {
        self.run(move |store| store.upsert_daemon(info)).await
    }

    /// Async wrapper for [`CoordinatorStore::record_heartbeat`].
    pub async fn record_heartbeat(&self, daemon: DaemonId) -> Result<Option<MutationSeq>> {
        self.run(move |store| store.record_heartbeat(&daemon)).await
    }

    /// Async wrapper for [`CoordinatorStore::set_daemon_reachable`].
    pub async fn set_daemon_reachable(
        &self,
        daemon: DaemonId,
        reachable: bool,
    ) -> Result<Option<MutationSeq>> {
        self.run(move |store| store.set_daemon_reachable(&daemon, reachable))
            .await
    }

    /// Async wrapper for [`CoordinatorStore::get_daemon`].
    pub async fn get_daemon(&self, daemon: DaemonId) -> Result<Option<DaemonRecord>> {
        self.run(move |store| store.get_daemon(&daemon)).await
    }

    /// Async wrapper for [`CoordinatorStore::remove_daemon`].
    pub async fn remove_daemon(&self, daemon: DaemonId) -> Result<Option<MutationSeq>> {
        self.run(move |store| store.remove_daemon(&daemon)).await
    }

    /// Async wrapper for [`CoordinatorStore::list_daemons`].
    pub async fn list_daemons(&self) -> Result<Vec<DaemonRecord>> {
        self.run(|store| store.list_daemons()).await
    }

    // ---------------------------------------------------------------
    // build cache — async wrappers for `crate::store::build_cache`
    // ---------------------------------------------------------------

    /// Async wrapper for [`CoordinatorStore::record_build`].
    pub async fn record_build(
        &self,
        hash: BuildCacheKey,
        artifact_path: String,
        size_bytes: Option<u64>,
    ) -> Result<MutationSeq> {
        self.run(move |store| store.record_build(hash, artifact_path, size_bytes))
            .await
    }

    /// Async wrapper for [`CoordinatorStore::touch_build_cache`].
    pub async fn touch_build_cache(&self, hash: BuildCacheKey) -> Result<Option<MutationSeq>> {
        self.run(move |store| store.touch_build_cache(&hash)).await
    }

    /// Async wrapper for [`CoordinatorStore::get_build_cache`].
    pub async fn get_build_cache(&self, hash: BuildCacheKey) -> Result<Option<BuildCacheEntry>> {
        self.run(move |store| store.get_build_cache(&hash)).await
    }

    /// Async wrapper for [`CoordinatorStore::remove_build_cache`].
    pub async fn remove_build_cache(&self, hash: BuildCacheKey) -> Result<Option<MutationSeq>> {
        self.run(move |store| store.remove_build_cache(&hash)).await
    }

    /// Async wrapper for [`CoordinatorStore::list_build_cache`].
    pub async fn list_build_cache(&self) -> Result<Vec<BuildCacheEntry>> {
        self.run(|store| store.list_build_cache()).await
    }

    // ---------------------------------------------------------------
    // mutation log — async wrappers for `crate::store::mutation_log`
    // ---------------------------------------------------------------

    /// Async wrapper for [`CoordinatorStore::last_seq`].
    pub async fn last_seq(&self) -> Result<MutationSeq> {
        self.run(|store| store.last_seq()).await
    }

    /// Async wrapper for [`CoordinatorStore::compacted_before`].
    pub async fn compacted_before(&self) -> Result<MutationSeq> {
        self.run(|store| store.compacted_before()).await
    }

    /// Async wrapper for [`CoordinatorStore::mutations_since`].
    pub async fn mutations_since(
        &self,
        after: MutationSeq,
        max_entries: usize,
    ) -> Result<CatchUpBatch> {
        self.run(move |store| store.mutations_since(after, max_entries))
            .await
    }

    /// Async wrapper for [`CoordinatorStore::compact`].
    pub async fn compact(&self, retain_after_seq: MutationSeq) -> Result<u64> {
        self.run(move |store| store.compact(retain_after_seq)).await
    }

    /// Async wrapper for [`CoordinatorStore::apply_replayed`].
    pub async fn apply_replayed(&self, record: MutationRecord) -> Result<()> {
        self.run(move |store| store.apply_replayed(&record)).await
    }

    // ---------------------------------------------------------------
    // topology — async wrapper for `crate::store::topology`
    // ---------------------------------------------------------------

    /// Async wrapper for [`CoordinatorStore::record_topology_op`].
    pub async fn record_topology_op(
        &self,
        dataflow: DataflowId,
        op_json: String,
    ) -> Result<MutationSeq> {
        self.run(move |store| store.record_topology_op(dataflow, op_json))
            .await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::store::CoordinatorStore;

    fn async_store() -> AsyncStore {
        AsyncCoordinatorStore::new(CoordinatorStore::open_in_memory().unwrap())
    }

    #[tokio::test]
    async fn set_and_get_round_trip_across_a_task_hop() {
        let store = async_store();
        let dataflow = DataflowId::generate();
        let key = ParamKey::new("gain").unwrap();
        store
            .set_param(dataflow, key.clone(), serde_json::json!(3))
            .await
            .unwrap();
        let record = store.get_param(dataflow, key).await.unwrap().unwrap();
        assert_eq!(record.value().unwrap(), serde_json::json!(3));
    }

    #[tokio::test]
    async fn sync_accessor_reaches_the_same_state() {
        let store = async_store();
        let dataflow = DataflowId::generate();
        let key = ParamKey::new("gain").unwrap();
        store
            .set_param(dataflow, key.clone(), serde_json::json!(1))
            .await
            .unwrap();
        assert!(store.sync().get_param(dataflow, &key).unwrap().is_some());
    }

    #[tokio::test]
    async fn clones_share_the_same_backend() {
        let store = async_store();
        let clone = store.clone();
        let dataflow = DataflowId::generate();
        let key = ParamKey::new("gain").unwrap();
        store
            .set_param(dataflow, key.clone(), serde_json::json!(1))
            .await
            .unwrap();
        assert!(clone.get_param(dataflow, key).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn mutation_log_wrappers_page_through_history() {
        let store = async_store();
        let dataflow = DataflowId::generate();
        store
            .set_param(dataflow, ParamKey::new("a").unwrap(), serde_json::json!(1))
            .await
            .unwrap();
        store
            .set_param(dataflow, ParamKey::new("b").unwrap(), serde_json::json!(2))
            .await
            .unwrap();

        let batch = store.mutations_since(MutationSeq::ZERO, 10).await.unwrap();
        assert_eq!(batch.entries.len(), 2);
        assert_eq!(store.last_seq().await.unwrap(), MutationSeq::new(2));

        let removed = store.compact(MutationSeq::new(1)).await.unwrap();
        assert_eq!(removed, 1);
        assert_eq!(store.compacted_before().await.unwrap(), MutationSeq::new(1));
    }

    #[tokio::test]
    async fn apply_replayed_folds_a_record_into_a_second_store() {
        let source = async_store();
        let shadow = async_store();
        let dataflow = DataflowId::generate();
        let key = ParamKey::new("gain").unwrap();
        source
            .set_param(dataflow, key.clone(), serde_json::json!(7))
            .await
            .unwrap();

        let batch = source.mutations_since(MutationSeq::ZERO, 10).await.unwrap();
        for record in batch.entries {
            shadow.apply_replayed(record).await.unwrap();
        }
        assert_eq!(
            shadow
                .get_param(dataflow, key)
                .await
                .unwrap()
                .unwrap()
                .value_json,
            "7"
        );
    }
}
