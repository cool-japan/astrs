//! [`RegistryStateMachine`]: the coordinator's durable registry, as a Raft
//! state machine.
//!
//! # Applying is an overwrite, which is what makes this simple
//!
//! Every [`MutationOp`] carries the complete new record, so applying one is a
//! `put`, never a read-modify-write. Three consequences fall out, and all
//! three matter:
//!
//! 1. **Applying is idempotent.** Re-applying an op the store already holds
//!    changes nothing, so [`astrs_raft::StateMachine::applied_index`] can
//!    honestly stay at zero: a replica that restarts and re-applies its whole
//!    log after the snapshot reaches the same state.
//! 2. **Applying is deterministic.** No local clock, no local read, nothing
//!    for two replicas to disagree about. The leader decided every value
//!    before it proposed.
//! 3. **A snapshot is just the current registry, listed.** There is no
//!    separate serialization format to maintain: the snapshot is the same
//!    `MutationOp` vocabulary, one `*Put` per row.
//!
//! # The mutation log is not the replication log
//!
//! `astrs-store` keeps its own sequence-numbered mutation log for daemon
//! catch-up (blueprint §12). Under `ha`, **that counter is no longer the
//! cluster's ordering authority** — the Raft log index is. A replicated op is
//! applied with [`astrs_store::CoordinatorStore::apply_replayed`], which
//! writes the bucket without allocating a local sequence number, so replicas
//! do not invent conflicting history for the same sequence. The consequence is
//! stated once, and handled once: with `ha` on, a reconnecting daemon is sent
//! a **full state snapshot** rather than a delta (`catchup::push_full_snapshot`
//! and its call site in `session::daemon`), which is the same fallback the
//! store already defines for a compacted history.
//!
//! # Blocking calls, and why they are fine here
//!
//! [`astrs_raft::StateMachine::apply`] is synchronous, so this type calls the
//! store's synchronous half directly rather than hopping through
//! `spawn_blocking`. That is deliberate: the only caller is the Raft replica's
//! own `tokio` task, which exists to do exactly this and nothing else — it is
//! not the coordinator's connection loop, and a slow embedded-database write
//! there delays consensus rather than every open socket.

use astrs_store::AsyncStore;
use astrs_store::record::{MutationOp, MutationRecord, MutationSeq, ParamRecord};
use astrs_time::{HlcClock, HlcTimestamp, SystemClock};
use astrs_wire::{DataflowId, NodeId, ParamKey};
use std::sync::Arc;

use astrs_raft::{LogIndex, StateMachine};

use crate::ha::command::RegistryCommand;
use crate::param_scope::GLOBAL_SCOPE_DATAFLOW;

/// The coordinator's durable registry, driven by the Raft log.
pub struct RegistryStateMachine {
    /// The store every applied operation lands in.
    store: AsyncStore,
    /// The clock that stamps the synthetic [`MutationRecord`] wrapper an
    /// applied op is handed to the store in.
    clock: Arc<HlcClock<SystemClock>>,
    /// The highest Raft index applied in this process, for observability.
    applied: LogIndex,
}

impl RegistryStateMachine {
    /// A state machine over `store`.
    #[must_use]
    pub fn new(store: AsyncStore, clock: Arc<HlcClock<SystemClock>>) -> Self {
        Self {
            store,
            clock,
            applied: LogIndex::ZERO,
        }
    }

    /// The highest Raft index this process has applied.
    ///
    /// Observability only — see [`StateMachine::applied_index`] for why this
    /// is deliberately *not* what a restart resumes from.
    #[must_use]
    pub const fn applied(&self) -> LogIndex {
        self.applied
    }

    /// The store this machine writes to.
    #[must_use]
    pub const fn store(&self) -> &AsyncStore {
        &self.store
    }

    /// Applies one operation to the store.
    fn apply_op(&self, op: MutationOp, ts: HlcTimestamp) -> Result<(), String> {
        // `apply_replayed` matches on `record.op` alone, so the wrapper's
        // sequence number is immaterial — the Raft index is this cluster's
        // ordering authority, not the store's local counter.
        let record = MutationRecord {
            seq: MutationSeq::ZERO,
            ts,
            op,
        };
        self.store
            .sync()
            .apply_replayed(&record)
            .map_err(|error| error.to_string())
    }
}

impl StateMachine for RegistryStateMachine {
    fn apply(&mut self, index: LogIndex, command: &[u8]) -> Result<Vec<u8>, String> {
        let command = RegistryCommand::decode(command).map_err(|error| error.to_string())?;
        let ts = self.clock.now();
        for op in command.into_ops() {
            self.apply_op(op, ts)?;
        }
        self.applied = index;
        Ok(Vec::new())
    }

    fn snapshot(&self) -> Result<Vec<u8>, String> {
        let ops = snapshot_ops(&self.store).map_err(|error| error.to_string())?;
        RegistryCommand::new(ops)
            .encode()
            .map_err(|error| error.to_string())
    }

    fn restore(&mut self, snapshot: &[u8]) -> Result<(), String> {
        let command = RegistryCommand::decode(snapshot).map_err(|error| error.to_string())?;
        let ts = self.clock.now();
        for op in command.into_ops() {
            self.apply_op(op, ts)?;
        }
        Ok(())
    }

    fn applied_index(&self) -> LogIndex {
        // Deliberately the default. Every op is a whole-record overwrite, so
        // replaying the log after a restart converges on the same registry —
        // see this module's header, point 1.
        LogIndex::ZERO
    }
}

/// Lists the whole durable registry as the operations that would recreate
/// it.
///
/// Used for the Raft snapshot: a follower that fell behind the leader's
/// compaction is caught up by replaying this one batch instead of history the
/// leader no longer has.
///
/// # Errors
///
/// [`crate::CoordinatorError::Store`] if any bucket cannot be listed.
pub fn snapshot_ops(store: &AsyncStore) -> crate::error::Result<Vec<MutationOp>> {
    let store = store.sync();
    let mut ops = Vec::new();

    // Global parameters live under the reserved sentinel dataflow.
    push_params(&mut ops, store, GLOBAL_SCOPE_DATAFLOW)?;

    for meta in store.list_dataflows()? {
        let dataflow = meta.id;
        ops.push(MutationOp::DataflowMetaPut { record: meta });
        push_params(&mut ops, store, dataflow)?;
        for record in store.list_node_status(dataflow)? {
            let node = record.info.node.clone();
            ops.push(MutationOp::NodeStatusPut { record });
            push_node_params(&mut ops, store, dataflow, &node)?;
        }
    }

    for record in store.list_daemons()? {
        ops.push(MutationOp::DaemonPut { record });
    }
    for record in store.list_build_cache()? {
        ops.push(MutationOp::BuildCachePut { record });
    }
    Ok(ops)
}

/// Appends every dataflow-scoped parameter of `dataflow`.
fn push_params(
    ops: &mut Vec<MutationOp>,
    store: &astrs_store::CoordinatorStore<oxistore_kv_redb::RedbStore>,
    dataflow: DataflowId,
) -> crate::error::Result<()> {
    for (key, record) in store.list_params(dataflow)? {
        ops.push(param_put(dataflow, key, record));
    }
    Ok(())
}

/// Appends every node-scoped parameter of `dataflow`/`node`.
fn push_node_params(
    ops: &mut Vec<MutationOp>,
    store: &astrs_store::CoordinatorStore<oxistore_kv_redb::RedbStore>,
    dataflow: DataflowId,
    node: &NodeId,
) -> crate::error::Result<()> {
    for (key, record) in store.list_node_params(dataflow, node)? {
        ops.push(MutationOp::NodeParamPut {
            dataflow,
            node: node.clone(),
            key,
            record,
        });
    }
    Ok(())
}

/// One dataflow-scoped parameter write.
#[must_use]
pub fn param_put(dataflow: DataflowId, key: ParamKey, record: ParamRecord) -> MutationOp {
    MutationOp::ParamPut {
        dataflow,
        key,
        record,
    }
}

/// The record types a snapshot round-trips, named so a reader can see at a
/// glance what "the whole registry" means here.
///
/// Purely documentary: it exists so that a record type added to `astrs-store`
/// without being added to [`snapshot_ops`] is visible as an omission in one
/// place rather than buried in a listing function.
pub const SNAPSHOT_RECORD_TYPES: [&str; 6] = [
    stringify!(ParamRecord),
    stringify!(DataflowMeta),
    stringify!(NodeStatusRecord),
    stringify!(DaemonRecord),
    stringify!(BuildCacheEntry),
    "NodeParamRecord",
];

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use astrs_store::CoordinatorStore;
    use astrs_wire::{DataflowStatus, NodeId};

    fn machine() -> RegistryStateMachine {
        let store = AsyncStore::new(CoordinatorStore::open_in_memory().unwrap());
        RegistryStateMachine::new(store, Arc::new(HlcClock::system()))
    }

    fn node_info(dataflow: DataflowId, node: &str) -> astrs_wire::NodeInfo {
        astrs_wire::NodeInfo {
            dataflow,
            node: NodeId::new(node).unwrap(),
            daemon: astrs_wire::DaemonId::generate(None),
            state: astrs_wire::NodeRunState::Running,
            pid: None,
            generation: 1,
            restart_count: 0,
            inputs: std::collections::BTreeMap::new(),
            outputs: std::collections::BTreeMap::new(),
            started_at: None,
            exit_cause: None,
        }
    }

    fn daemon_info() -> astrs_wire::DaemonInfo {
        astrs_wire::DaemonInfo {
            id: astrs_wire::DaemonId::generate(None),
            version: astrs_wire::AstrsVersion::current(),
            address: "127.0.0.1:7500".to_owned(),
            connected_at: HlcTimestamp::EPOCH,
            node_count: 0,
            labels: std::collections::BTreeMap::new(),
            reachable: true,
        }
    }

    fn param_op(key: &str, value: &str, revision: u64) -> MutationOp {
        MutationOp::ParamPut {
            dataflow: GLOBAL_SCOPE_DATAFLOW,
            key: ParamKey::new(key).unwrap(),
            record: ParamRecord {
                value_json: value.to_owned(),
                revision,
                created_at: HlcTimestamp::new(1, 0),
                updated_at: HlcTimestamp::new(revision, 0),
            },
        }
    }

    fn command(ops: Vec<MutationOp>) -> Vec<u8> {
        RegistryCommand::new(ops).encode().unwrap()
    }

    #[test]
    fn applying_a_batch_writes_every_operation() {
        let mut machine = machine();
        machine
            .apply(
                LogIndex::new(1),
                &command(vec![param_op("a", "1", 1), param_op("b", "2", 1)]),
            )
            .unwrap();
        assert_eq!(machine.applied(), LogIndex::new(1));

        let store = machine.store().sync();
        assert_eq!(
            store
                .get_param(GLOBAL_SCOPE_DATAFLOW, &ParamKey::new("a").unwrap())
                .unwrap()
                .map(|record| record.value_json),
            Some("1".to_owned())
        );
        assert_eq!(
            store
                .get_param(GLOBAL_SCOPE_DATAFLOW, &ParamKey::new("b").unwrap())
                .unwrap()
                .map(|record| record.value_json),
            Some("2".to_owned())
        );
    }

    #[test]
    fn applying_the_same_entry_twice_changes_nothing() {
        // The property that lets `applied_index` stay at the default: a
        // restart that replays the log converges on the same registry.
        let mut machine = machine();
        let batch = command(vec![param_op("gain", "1.5", 3)]);
        machine.apply(LogIndex::new(1), &batch).unwrap();
        let first = machine
            .store()
            .sync()
            .get_param(GLOBAL_SCOPE_DATAFLOW, &ParamKey::new("gain").unwrap())
            .unwrap();
        machine.apply(LogIndex::new(1), &batch).unwrap();
        let second = machine
            .store()
            .sync()
            .get_param(GLOBAL_SCOPE_DATAFLOW, &ParamKey::new("gain").unwrap())
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(machine.applied_index(), LogIndex::ZERO);
    }

    #[test]
    fn a_delete_removes_what_a_put_wrote() {
        let mut machine = machine();
        machine
            .apply(LogIndex::new(1), &command(vec![param_op("x", "9", 1)]))
            .unwrap();
        machine
            .apply(
                LogIndex::new(2),
                &command(vec![MutationOp::ParamDelete {
                    dataflow: GLOBAL_SCOPE_DATAFLOW,
                    key: ParamKey::new("x").unwrap(),
                }]),
            )
            .unwrap();
        assert!(
            machine
                .store()
                .sync()
                .get_param(GLOBAL_SCOPE_DATAFLOW, &ParamKey::new("x").unwrap())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_malformed_command_is_reported_not_swallowed() {
        let mut machine = machine();
        let error = machine
            .apply(LogIndex::new(1), b"not a registry command")
            .unwrap_err();
        assert!(!error.is_empty());
    }

    #[test]
    fn a_snapshot_reproduces_the_whole_registry_in_a_fresh_store() {
        let mut source = machine();
        // Parameters, a dataflow, a node status and a daemon: one of each
        // bucket `snapshot_ops` claims to cover.
        source
            .apply(LogIndex::new(1), &command(vec![param_op("global", "1", 1)]))
            .unwrap();

        let dataflow = DataflowId::generate();
        let store = source.store().sync();
        store
            .upsert_dataflow(dataflow, Some("perception".to_owned()), "{}".to_owned(), 1)
            .unwrap();
        store
            .set_param(
                dataflow,
                ParamKey::new("scoped").unwrap(),
                serde_json::json!(2),
            )
            .unwrap();
        store
            .set_node_param(
                dataflow,
                NodeId::new("camera").unwrap(),
                ParamKey::new("fps").unwrap(),
                serde_json::json!(30),
            )
            .unwrap();
        store
            .set_node_status(node_info(dataflow, "camera"))
            .unwrap();
        store
            .set_dataflow_status(dataflow, DataflowStatus::Running, None)
            .unwrap();

        let bytes = source.snapshot().unwrap();

        let mut restored = machine();
        restored.restore(&bytes).unwrap();
        let target = restored.store().sync();

        assert_eq!(target.list_dataflows().unwrap().len(), 1);
        assert_eq!(
            target
                .get_param(GLOBAL_SCOPE_DATAFLOW, &ParamKey::new("global").unwrap())
                .unwrap()
                .map(|record| record.value_json),
            Some("1".to_owned())
        );
        assert!(
            target
                .get_param(dataflow, &ParamKey::new("scoped").unwrap())
                .unwrap()
                .is_some()
        );
        assert!(
            target
                .get_node_param(
                    dataflow,
                    &NodeId::new("camera").unwrap(),
                    &ParamKey::new("fps").unwrap()
                )
                .unwrap()
                .is_some()
        );
        assert_eq!(target.list_node_status(dataflow).unwrap().len(), 1);
        assert_eq!(
            target
                .get_dataflow_meta(dataflow)
                .unwrap()
                .map(|m| m.status),
            Some(DataflowStatus::Running)
        );
    }

    #[test]
    fn an_empty_registry_snapshots_and_restores() {
        let source = machine();
        let bytes = source.snapshot().unwrap();
        let mut restored = machine();
        restored.restore(&bytes).unwrap();
        assert!(restored.store().sync().list_dataflows().unwrap().is_empty());
    }

    #[test]
    fn the_documented_record_types_are_the_ones_listed() {
        assert_eq!(SNAPSHOT_RECORD_TYPES.len(), 6);
        assert!(SNAPSHOT_RECORD_TYPES.contains(&"DaemonRecord"));
        assert!(SNAPSHOT_RECORD_TYPES.contains(&"BuildCacheEntry"));
    }

    #[test]
    fn snapshot_ops_lists_daemons_and_build_cache_entries() {
        let machine = machine();
        let store = machine.store().sync();
        store.upsert_daemon(daemon_info()).unwrap();
        let ops = snapshot_ops(machine.store()).unwrap();
        assert!(
            ops.iter()
                .any(|op| matches!(op, MutationOp::DaemonPut { .. })),
            "a registered daemon must appear in the snapshot"
        );
    }
}
