//! Property test: for any sequence of parameter writes/deletes, the
//! mutation log is a gap-free, duplicate-free, ascending run of sequence
//! numbers, and replaying it into a fresh store reproduces the source
//! store's state exactly.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_store::CoordinatorStore;
use astrs_store::record::MutationSeq;
use astrs_wire::{DataflowId, ParamKey};
use proptest::prelude::*;

/// A random op against one of a small, fixed universe of
/// `(dataflow, key)` pairs — small enough that puts and deletes to the
/// *same* key collide often, which is what exercises revision bookkeeping
/// and the "delete of an absent key logs nothing" rule.
#[derive(Debug, Clone)]
enum Op {
    Set {
        dataflow_idx: usize,
        key_idx: usize,
        value: i32,
    },
    Delete {
        dataflow_idx: usize,
        key_idx: usize,
    },
}

const DATAFLOW_COUNT: usize = 3;
const KEY_COUNT: usize = 4;

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..DATAFLOW_COUNT, 0..KEY_COUNT, any::<i32>()).prop_map(
            |(dataflow_idx, key_idx, value)| {
                Op::Set {
                    dataflow_idx,
                    key_idx,
                    value,
                }
            }
        ),
        (0..DATAFLOW_COUNT, 0..KEY_COUNT).prop_map(|(dataflow_idx, key_idx)| Op::Delete {
            dataflow_idx,
            key_idx,
        }),
    ]
}

/// Pages through the whole mutation log from the start, collecting every
/// record in sequence order. Exercises the same pagination API a
/// reconnecting daemon would use, with a deliberately small page size so a
/// non-trivial op sequence spans several pages.
fn drain_log(
    store: &CoordinatorStore<oxistore_kv_redb::RedbStore>,
) -> Vec<astrs_store::MutationRecord> {
    let mut all = Vec::new();
    let mut after = MutationSeq::ZERO;
    loop {
        let batch = store.mutations_since(after, 7).unwrap();
        after = batch.next_seq;
        let caught_up = batch.caught_up;
        all.extend(batch.entries);
        if caught_up {
            break;
        }
    }
    all
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn mutation_log_has_no_gaps_and_replay_reproduces_final_state(
        ops in proptest::collection::vec(op_strategy(), 0..200)
    ) {
        let dataflows: Vec<DataflowId> = (0..DATAFLOW_COUNT).map(|_| DataflowId::generate()).collect();
        let keys: Vec<ParamKey> = (0..KEY_COUNT)
            .map(|i| ParamKey::new(format!("k{i}")).unwrap())
            .collect();

        let source = CoordinatorStore::open_in_memory().unwrap();
        for op in &ops {
            match *op {
                Op::Set { dataflow_idx, key_idx, value } => {
                    source
                        .set_param(dataflows[dataflow_idx], keys[key_idx].clone(), serde_json::json!(value))
                        .unwrap();
                }
                Op::Delete { dataflow_idx, key_idx } => {
                    source.delete_param(dataflows[dataflow_idx], &keys[key_idx]).unwrap();
                }
            }
        }

        // --- Property 1: the log is exactly {1, ..., last_seq}, in order,
        // with no gaps and no duplicates, however many pages it took.
        let last = source.last_seq().unwrap();
        let entries = drain_log(&source);
        let seen: Vec<u64> = entries.iter().map(|r| r.seq.get()).collect();
        let expected: Vec<u64> = (1..=last.get()).collect();
        prop_assert_eq!(seen, expected);

        // --- Property 2: replaying the log into a fresh store reproduces
        // the source store's observable state exactly, key by key — proof
        // that every `*Put` op really does carry a complete record rather
        // than a delta that would need prior state to interpret.
        let shadow = CoordinatorStore::open_in_memory().unwrap();
        for record in &entries {
            shadow.apply_replayed(record).unwrap();
        }
        for &dataflow in &dataflows {
            for key in &keys {
                let source_value = source.get_param(dataflow, key).unwrap();
                let shadow_value = shadow.get_param(dataflow, key).unwrap();
                prop_assert_eq!(source_value, shadow_value);
            }
        }

        // --- Property 3: replaying twice is idempotent (every op is a
        // last-writer-wins overwrite, never an increment relative to
        // whatever the shadow already had).
        for record in &entries {
            shadow.apply_replayed(record).unwrap();
        }
        for &dataflow in &dataflows {
            for key in &keys {
                prop_assert_eq!(
                    source.get_param(dataflow, key).unwrap(),
                    shadow.get_param(dataflow, key).unwrap()
                );
            }
        }
    }
}
