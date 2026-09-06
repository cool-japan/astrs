//! Persisting dynamic-topology ops (blueprint §8, §17 — `astrs node
//! add/remove/replace/connect/disconnect`).
//!
//! Every other bucket module in this crate pairs a domain write with its
//! mutation-log record through `CoordinatorStore::apply_upsert` /
//! `CoordinatorStore::apply_delete_if_present` (both `pub(crate)`, hence
//! plain code spans rather than doc links here — see [`super`]'s own
//! docs, where the same names *are* linkable from another private item).
//! A topology op has no domain bucket of its own: the *current* graph is
//! not stored as a value anyone reads back directly; it is derived by
//! rebuilding the base graph from a dataflow's stored manifest and
//! replaying every [`MutationOp::TopologyOpApplied`] logged for it, in
//! order, through `astrs_graph::apply` (a Layer 2 operation this crate
//! does not depend on and so cannot link to — see that variant's own docs
//! for why the replay itself is `astrs-coordinator`'s job, not this
//! crate's). [`CoordinatorStore::record_topology_op`] is therefore the
//! one bucket-write path in this crate that writes *only* to the
//! mutation log.

use std::sync::PoisonError;

use astrs_wire::{DataflowId, WireEncode};
use oxistore_core::KvStore;

use crate::error::Result;
use crate::keys;
use crate::record::{MutationOp, MutationRecord, MutationSeq};
use crate::store::CoordinatorStore;

impl<S: KvStore + Clone> CoordinatorStore<S> {
    /// Appends one applied `astrs_graph::TopologyOp` (already rendered
    /// as JSON by the caller — see [`MutationOp::TopologyOpApplied`]'s own
    /// docs for why this crate never names that type directly, nor links
    /// to it: `astrs-store` has no dependency on `astrs-graph` to resolve
    /// it against) to the mutation log, without touching any domain
    /// bucket.
    ///
    /// Holds `CoordinatorStore::write_gate` for the duration, exactly
    /// like `CoordinatorStore::apply_upsert`, so a concurrent topology
    /// op and an ordinary bucket write can never interleave their sequence
    /// numbers.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Backend`] if the transaction fails to open, write
    /// or commit.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_store::CoordinatorStore;
    /// use astrs_wire::DataflowId;
    ///
    /// let store = CoordinatorStore::open_in_memory()?;
    /// let dataflow = DataflowId::generate();
    /// let seq = store.record_topology_op(dataflow, "{\"RemoveNode\":{\"id\":\"a\"}}".to_owned())?;
    /// let batch = store.mutations_since(Default::default(), 10)?;
    /// assert_eq!(batch.entries.len(), 1);
    /// assert_eq!(batch.entries[0].seq, seq);
    /// # Ok::<(), astrs_store::Error>(())
    /// ```
    pub fn record_topology_op(&self, dataflow: DataflowId, op_json: String) -> Result<MutationSeq> {
        let _gate = self
            .write_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        let mut txn = self.backend.transaction()?;
        let ts = self.clock.now();
        let seq = Self::allocate_seq_locked(&mut *txn)?;
        let record = MutationRecord {
            seq,
            ts,
            op: MutationOp::TopologyOpApplied { dataflow, op_json },
        };
        txn.put(&keys::mutation_log_key(seq), &record.encode_to_vec()?)?;
        txn.commit()?;
        Ok(seq)
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
    fn recording_a_topology_op_appends_only_to_the_log() {
        let store = store();
        let dataflow = DataflowId::generate();
        let seq = store
            .record_topology_op(dataflow, "{\"AddNode\":{}}".to_owned())
            .unwrap();
        assert_eq!(seq, MutationSeq::new(1));

        let batch = store.mutations_since(MutationSeq::ZERO, 10).unwrap();
        assert_eq!(batch.entries.len(), 1);
        match &batch.entries[0].op {
            MutationOp::TopologyOpApplied {
                dataflow: got,
                op_json,
            } => {
                assert_eq!(*got, dataflow);
                assert_eq!(op_json, "{\"AddNode\":{}}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn topology_ops_share_the_sequence_counter_with_ordinary_mutations() {
        let store = store();
        let dataflow = DataflowId::generate();
        store
            .set_param(
                dataflow,
                astrs_wire::ParamKey::new("gain").unwrap(),
                serde_json::json!(1),
            )
            .unwrap();
        let seq = store.record_topology_op(dataflow, "{}".to_owned()).unwrap();
        assert_eq!(
            seq,
            MutationSeq::new(2),
            "the counter is shared, not scoped"
        );
    }

    #[test]
    fn recording_several_ops_preserves_their_order() {
        let store = store();
        let dataflow = DataflowId::generate();
        store
            .record_topology_op(dataflow, "\"first\"".to_owned())
            .unwrap();
        store
            .record_topology_op(dataflow, "\"second\"".to_owned())
            .unwrap();

        let batch = store.mutations_since(MutationSeq::ZERO, 10).unwrap();
        let ops: Vec<&str> = batch
            .entries
            .iter()
            .map(|entry| match &entry.op {
                MutationOp::TopologyOpApplied { op_json, .. } => op_json.as_str(),
                _ => panic!("unexpected op"),
            })
            .collect();
        assert_eq!(ops, ["\"first\"", "\"second\""]);
    }
}
