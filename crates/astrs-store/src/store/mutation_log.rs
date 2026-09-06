//! The mutation log's read side: paging through catch-up history
//! ([`CoordinatorStore::mutations_since`]) and reclaiming old history
//! ([`CoordinatorStore::compact`]).

use std::sync::PoisonError;

use astrs_wire::WireDecode;
use oxistore_core::KvStore;

use crate::error::{Error, Result};
use crate::keys;
use crate::record::{CatchUpBatch, MutationRecord, MutationSeq};
use crate::store::CoordinatorStore;

/// The hard cap on the page size [`CoordinatorStore::mutations_since`] will
/// return, regardless of the `max_entries` a caller requests.
///
/// Protects the coordinator from an accidental unbounded catch-up response
/// (and a reconnecting daemon that has been offline a very long time from
/// receiving one enormous frame instead of several bounded ones).
///
/// This bounds **entry count**, not serialized byte size. Every entry this
/// crate defines is small except [`crate::record::MutationOp::DataflowMetaPut`], which
/// embeds a whole [`crate::record::DataflowMeta::manifest_json`] snapshot —
/// in principle unbounded, since `astrs-store` (Layer 1) does not itself
/// impose a manifest-size limit. A caller that packs a full
/// [`crate::record::CatchUpBatch`] into one wire frame (the
/// coordinator, forwarding it as blueprint §24.1's
/// `CoordinatorEvent::StateCatchUp{seq, entries}`) must additionally respect
/// §7.1's 64 MiB frame cap — e.g. by chunking on encoded size, not only on
/// [`MAX_CATCH_UP_PAGE`] — if large manifests make that a real risk for its
/// deployment.
pub const MAX_CATCH_UP_PAGE: usize = 4096;

impl<S: KvStore + Clone> CoordinatorStore<S> {
    /// Returns up to `max_entries` mutation-log records logged strictly
    /// after `after`, in ascending sequence order, as a bounded, pageable
    /// [`CatchUpBatch`].
    ///
    /// Pass [`MutationSeq::ZERO`] for `after` to fetch from the very start
    /// of the log — what a daemon that has never connected before should
    /// do. `max_entries` is silently clamped to [`MAX_CATCH_UP_PAGE`].
    /// When [`CatchUpBatch::caught_up`] is `false`, call this again with
    /// [`CatchUpBatch::next_seq`] to continue.
    ///
    /// # Errors
    ///
    /// [`Error::MutationHistoryCompacted`] if `after` is older than
    /// [`CoordinatorStore::compact`] has retained — the caller must fall
    /// back to a full state resend rather than a catch-up delta.
    /// [`Error::Backend`] on a read failure; [`Error::Wire`] if a stored
    /// record fails to decode (on-disk corruption).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_store::CoordinatorStore;
    /// use astrs_store::record::MutationSeq;
    /// use astrs_wire::{DataflowId, ParamKey};
    ///
    /// let store = CoordinatorStore::open_in_memory()?;
    /// let dataflow = DataflowId::generate();
    /// store.set_param(dataflow, ParamKey::new("a")?, serde_json::json!(1))?;
    /// store.set_param(dataflow, ParamKey::new("b")?, serde_json::json!(2))?;
    ///
    /// let first_page = store.mutations_since(MutationSeq::ZERO, 1)?;
    /// assert_eq!(first_page.entries.len(), 1);
    /// assert!(!first_page.caught_up, "one more record remains");
    ///
    /// let rest = store.mutations_since(first_page.next_seq, 10)?;
    /// assert_eq!(rest.entries.len(), 1);
    /// assert!(rest.caught_up);
    /// # Ok::<(), astrs_store::Error>(())
    /// ```
    pub fn mutations_since(&self, after: MutationSeq, max_entries: usize) -> Result<CatchUpBatch> {
        let compacted_before = self.compacted_before()?;
        if after < compacted_before {
            return Err(Error::MutationHistoryCompacted {
                requested: after,
                earliest_available: compacted_before,
            });
        }

        let max_entries = max_entries.min(MAX_CATCH_UP_PAGE);
        let (lo, hi) = keys::mutation_log_window(after, max_entries);
        let mut entries = Vec::new();
        for item in self.backend.range(&lo, &hi)? {
            let (_, raw_value) = item?;
            entries.push(MutationRecord::decode_exact(&raw_value)?);
        }

        let caught_up = entries.len() <= max_entries;
        if !caught_up {
            entries.truncate(max_entries);
        }
        let next_seq = entries.last().map_or(after, |record| record.seq);
        Ok(CatchUpBatch {
            entries,
            next_seq,
            caught_up,
        })
    }

    /// Removes mutation-log entries with sequence numbers in
    /// `(compacted_before, retain_after_seq]` and advances the compaction
    /// watermark to `retain_after_seq`.
    ///
    /// Returns the number of entries removed. A `retain_after_seq` at or
    /// below the current watermark is a no-op returning `Ok(0)` — compacting
    /// up to a point that is already compacted is redundant, not a caller
    /// mistake.
    ///
    /// Compaction does **not** itself append a mutation-log record: it
    /// changes which history is *retained*, not the coordinator's
    /// observable current state, so there is nothing here for a
    /// reconnecting daemon to catch up on.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidCompactionTarget`] if `retain_after_seq` is beyond
    /// the highest sequence number actually issued so far.
    /// [`Error::Backend`] on a read or write failure.
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
    ///
    /// let removed = store.compact(store.last_seq()?)?;
    /// assert_eq!(removed, 2);
    /// assert!(store.mutations_since(Default::default(), 10).is_err());
    /// # Ok::<(), astrs_store::Error>(())
    /// ```
    pub fn compact(&self, retain_after_seq: MutationSeq) -> Result<u64> {
        // Held for the whole read-check-delete-commit sequence, exactly
        // like `apply_upsert` / `apply_delete_if_present`: while this gate
        // is held, no concurrent mutating call (and no concurrent
        // `compact`) can be advancing `last_seq` or `compacted_before`
        // underneath this one, so reading them via the plain (non-`KvTxn`)
        // accessors below is already consistent — a transaction is still
        // used for the deletes themselves, so a crash mid-compaction cannot
        // leave the watermark advanced past history that was not actually
        // removed (or vice versa).
        let _gate = self
            .write_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        let current_max = self.last_seq()?;
        if retain_after_seq > current_max {
            return Err(Error::InvalidCompactionTarget {
                requested: retain_after_seq,
                current_max,
            });
        }

        let already = self.compacted_before()?;
        if retain_after_seq <= already {
            return Ok(0);
        }

        let mut txn = self.backend.transaction()?;
        let mut removed = 0u64;
        let mut seq = already.saturating_add(1);
        loop {
            if seq > retain_after_seq {
                break;
            }
            txn.delete(&keys::mutation_log_key(seq))?;
            removed += 1;
            if seq.get() == u64::MAX {
                break;
            }
            seq = seq.saturating_add(1);
        }
        txn.put(
            &keys::compacted_before_key(),
            &retain_after_seq.get().to_be_bytes(),
        )?;
        txn.commit()?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::{DataflowId, ParamKey};

    fn store() -> CoordinatorStore<oxistore_kv_redb::RedbStore> {
        CoordinatorStore::open_in_memory().unwrap()
    }

    fn seed(store: &CoordinatorStore<oxistore_kv_redb::RedbStore>, count: u64) {
        let dataflow = DataflowId::generate();
        for i in 0..count {
            store
                .set_param(
                    dataflow,
                    ParamKey::new(format!("k{i}")).unwrap(),
                    serde_json::json!(i),
                )
                .unwrap();
        }
    }

    #[test]
    fn a_fresh_store_has_no_history_and_that_is_legal() {
        let store = store();
        let batch = store.mutations_since(MutationSeq::ZERO, 10).unwrap();
        assert!(batch.entries.is_empty());
        assert!(batch.caught_up);
        assert_eq!(batch.next_seq, MutationSeq::ZERO);
    }

    #[test]
    fn pages_are_bounded_and_ordered() {
        let store = store();
        seed(&store, 5);

        let mut seen = Vec::new();
        let mut after = MutationSeq::ZERO;
        loop {
            let batch = store.mutations_since(after, 2).unwrap();
            seen.extend(batch.entries.iter().map(|r| r.seq));
            after = batch.next_seq;
            if batch.caught_up {
                break;
            }
        }
        let expected: Vec<MutationSeq> = (1..=5).map(MutationSeq::new).collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn max_entries_zero_still_reports_caught_up_correctly() {
        let store = store();
        seed(&store, 1);
        let batch = store.mutations_since(MutationSeq::ZERO, 0).unwrap();
        assert!(batch.entries.is_empty());
        assert!(!batch.caught_up);
        assert_eq!(batch.next_seq, MutationSeq::ZERO);

        let empty_store = self::store();
        let batch = empty_store.mutations_since(MutationSeq::ZERO, 0).unwrap();
        assert!(batch.caught_up);
    }

    #[test]
    fn max_entries_is_clamped_to_the_hard_cap() {
        let store = store();
        seed(&store, 3);
        // Requesting far more than exists must not error or hang.
        let batch = store
            .mutations_since(MutationSeq::ZERO, usize::MAX)
            .unwrap();
        assert_eq!(batch.entries.len(), 3);
        assert!(batch.caught_up);
    }

    #[test]
    fn after_equal_to_last_seq_yields_an_empty_caught_up_batch() {
        let store = store();
        seed(&store, 2);
        let last = store.last_seq().unwrap();
        let batch = store.mutations_since(last, 10).unwrap();
        assert!(batch.entries.is_empty());
        assert!(batch.caught_up);
        assert_eq!(batch.next_seq, last);
    }

    #[test]
    fn compact_is_a_no_op_below_or_at_the_current_watermark() {
        let store = store();
        seed(&store, 3);
        assert_eq!(store.compact(MutationSeq::ZERO).unwrap(), 0);
        assert_eq!(store.compacted_before().unwrap(), MutationSeq::ZERO);
    }

    #[test]
    fn compact_rejects_a_target_beyond_the_current_max() {
        let store = store();
        seed(&store, 2);
        let err = store.compact(MutationSeq::new(99)).unwrap_err();
        assert!(matches!(err, Error::InvalidCompactionTarget { .. }));
    }

    #[test]
    fn compact_removes_history_and_moves_the_watermark() {
        let store = store();
        seed(&store, 5);
        let removed = store.compact(MutationSeq::new(3)).unwrap();
        assert_eq!(removed, 3);
        assert_eq!(store.compacted_before().unwrap(), MutationSeq::new(3));

        // Exactly at the watermark is still legal ...
        let batch = store.mutations_since(MutationSeq::new(3), 10).unwrap();
        assert_eq!(batch.entries.len(), 2);
        assert!(batch.caught_up);

        // ... but anything older is rejected, not silently truncated.
        let err = store.mutations_since(MutationSeq::new(2), 10).unwrap_err();
        match err {
            Error::MutationHistoryCompacted {
                requested,
                earliest_available,
            } => {
                assert_eq!(requested, MutationSeq::new(2));
                assert_eq!(earliest_available, MutationSeq::new(3));
            }
            other => panic!("expected MutationHistoryCompacted, got {other:?}"),
        }
    }

    #[test]
    fn compacting_twice_only_removes_the_newly_covered_range() {
        let store = store();
        seed(&store, 6);
        assert_eq!(store.compact(MutationSeq::new(2)).unwrap(), 2);
        assert_eq!(store.compact(MutationSeq::new(4)).unwrap(), 2);
        assert_eq!(store.compacted_before().unwrap(), MutationSeq::new(4));
    }
}
