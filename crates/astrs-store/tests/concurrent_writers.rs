//! Integration tests: multiple threads mutating one shared
//! [`CoordinatorStore`] handle concurrently.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::thread;

use astrs_store::CoordinatorStore;
use astrs_store::record::MutationSeq;
use astrs_wire::{DataflowId, ParamKey};

const THREADS: usize = 8;
const WRITES_PER_THREAD: usize = 50;

#[test]
fn concurrent_writers_to_the_same_key_never_lose_a_revision() {
    let store = CoordinatorStore::open_in_memory().unwrap();
    let dataflow = DataflowId::generate();
    let key = ParamKey::new("shared").unwrap();
    store
        .set_param(dataflow, key.clone(), serde_json::json!(0))
        .unwrap();

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let store = store.clone();
            let key = key.clone();
            thread::spawn(move || {
                for i in 0..WRITES_PER_THREAD {
                    store
                        .set_param(dataflow, key.clone(), serde_json::json!(t * 1000 + i))
                        .unwrap();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    let record = store.get_param(dataflow, &key).unwrap().unwrap();
    // One initial write, plus THREADS * WRITES_PER_THREAD concurrent
    // writes to the same key: the per-key revision counter must have
    // counted every single one. A read-modify-write race that computed
    // "current revision + 1" outside the write gate would let two threads
    // both read the same old revision and both write the same new one,
    // silently undercounting — this is exactly what holding the gate
    // across the whole read-decode-modify-encode-write sequence in
    // `apply_upsert` prevents.
    assert_eq!(record.revision, 1 + (THREADS * WRITES_PER_THREAD) as u64);
}

#[test]
fn concurrent_writers_across_many_keys_produce_a_gap_free_contiguous_log() {
    let store = CoordinatorStore::open_in_memory().unwrap();
    let dataflow = DataflowId::generate();

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let store = store.clone();
            thread::spawn(move || {
                for i in 0..WRITES_PER_THREAD {
                    let key = ParamKey::new(format!("t{t}-k{i}")).unwrap();
                    store
                        .set_param(dataflow, key, serde_json::json!(i))
                        .unwrap();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    let total = (THREADS * WRITES_PER_THREAD) as u64;
    assert_eq!(store.last_seq().unwrap().get(), total);

    // Page through the entire log and confirm the sequence numbers are
    // exactly {1, ..., total} with no gaps and no duplicates, regardless of
    // which thread happened to win the race for the write gate at each
    // step — the property the mutation-log proptest also checks, here
    // under real OS-thread contention instead of a single-threaded
    // simulated op sequence.
    let mut seen = BTreeSet::new();
    let mut after = MutationSeq::ZERO;
    loop {
        let batch = store.mutations_since(after, 17).unwrap();
        for record in &batch.entries {
            assert!(
                seen.insert(record.seq.get()),
                "duplicate seq {}",
                record.seq
            );
        }
        after = batch.next_seq;
        if batch.caught_up {
            break;
        }
    }
    let expected: BTreeSet<u64> = (1..=total).collect();
    assert_eq!(seen, expected);

    // Every write actually landed: one row per (thread, index) pair.
    assert_eq!(
        store.list_params(dataflow).unwrap().len(),
        THREADS * WRITES_PER_THREAD
    );
}

#[test]
fn concurrent_mixed_bucket_writers_all_get_logged_atomically_with_their_data() {
    let store = CoordinatorStore::open_in_memory().unwrap();
    let dataflow = DataflowId::generate();
    store
        .upsert_dataflow(dataflow, None, "{}".to_owned(), 0)
        .unwrap();

    let param_handles: Vec<_> = (0..4)
        .map(|t| {
            let store = store.clone();
            thread::spawn(move || {
                for i in 0..25 {
                    let key = ParamKey::new(format!("p{t}-{i}")).unwrap();
                    store
                        .set_param(dataflow, key, serde_json::json!(i))
                        .unwrap();
                }
            })
        })
        .collect();
    let daemon_handles: Vec<_> = (0..4)
        .map(|_| {
            let store = store.clone();
            thread::spawn(move || {
                for _ in 0..25 {
                    let daemon = astrs_wire::DaemonId::generate(None);
                    store
                        .upsert_daemon(astrs_wire::DaemonInfo {
                            id: daemon,
                            version: astrs_wire::AstrsVersion::current(),
                            address: "127.0.0.1:0".to_owned(),
                            connected_at: astrs_time::HlcTimestamp::new(1, 0),
                            node_count: 0,
                            labels: Default::default(),
                            reachable: true,
                        })
                        .unwrap();
                }
            })
        })
        .collect();

    for handle in param_handles.into_iter().chain(daemon_handles) {
        handle.join().unwrap();
    }

    assert_eq!(store.list_params(dataflow).unwrap().len(), 100);
    assert_eq!(store.list_daemons().unwrap().len(), 100);
    // Every one of those 200 bucket writes, plus the initial
    // `upsert_dataflow`, must have logged exactly one mutation record —
    // never more, never fewer, regardless of which bucket or which thread.
    assert_eq!(store.last_seq().unwrap(), MutationSeq::new(201));
}
