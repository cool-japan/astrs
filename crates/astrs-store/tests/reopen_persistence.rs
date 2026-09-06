//! Integration tests: state survives a process restart (dropping every
//! handle, then reopening the same redb file).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_store::CoordinatorStore;
use astrs_store::record::MutationSeq;
use astrs_wire::{DataflowId, ParamKey};

/// A fresh, collision-resistant path under `std::env::temp_dir()`.
///
/// `name` distinguishes tests within this file; `std::process::id()`
/// distinguishes separate `cargo test` invocations that might otherwise
/// leave a stale file from a previous, non-cleaned-up run.
fn temp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "astrs-store-reopen-{name}-{}.redb",
        std::process::id()
    ))
}

#[test]
fn params_and_dataflow_meta_survive_a_reopen() {
    let path = temp_path("basic");
    let _ = std::fs::remove_file(&path);

    let dataflow = DataflowId::generate();
    let key = ParamKey::new("gain").unwrap();
    {
        let store = CoordinatorStore::open(&path).unwrap();
        store
            .set_param(dataflow, key.clone(), serde_json::json!(2.5))
            .unwrap();
        store
            .upsert_dataflow(dataflow, Some("demo".to_owned()), "{}".to_owned(), 1)
            .unwrap();
        // `store` drops here, releasing redb's exclusive file lock before
        // the next `CoordinatorStore::open` call on the same path.
    }

    let reopened = CoordinatorStore::open(&path).unwrap();
    let record = reopened.get_param(dataflow, &key).unwrap().unwrap();
    assert_eq!(record.value().unwrap(), serde_json::json!(2.5));
    let meta = reopened.get_dataflow_meta(dataflow).unwrap().unwrap();
    assert_eq!(meta.name.as_deref(), Some("demo"));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_mutation_log_and_its_sequence_counter_survive_a_reopen() {
    let path = temp_path("log");
    let _ = std::fs::remove_file(&path);

    let dataflow = DataflowId::generate();
    {
        let store = CoordinatorStore::open(&path).unwrap();
        for i in 0..3u32 {
            store
                .set_param(
                    dataflow,
                    ParamKey::new(format!("k{i}")).unwrap(),
                    serde_json::json!(i),
                )
                .unwrap();
        }
        assert_eq!(store.last_seq().unwrap(), MutationSeq::new(3));
    }

    let reopened = CoordinatorStore::open(&path).unwrap();
    assert_eq!(reopened.last_seq().unwrap(), MutationSeq::new(3));
    let batch = reopened.mutations_since(MutationSeq::ZERO, 10).unwrap();
    assert_eq!(batch.entries.len(), 3);
    assert!(batch.caught_up);

    // A write after reopening must continue the sequence, not restart it —
    // the counter itself, not just the log rows, has to be durable.
    let dataflow2 = DataflowId::generate();
    reopened
        .set_param(dataflow2, ParamKey::new("x").unwrap(), serde_json::json!(1))
        .unwrap();
    assert_eq!(reopened.last_seq().unwrap(), MutationSeq::new(4));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_compaction_watermark_survives_a_reopen() {
    let path = temp_path("watermark");
    let _ = std::fs::remove_file(&path);

    let dataflow = DataflowId::generate();
    {
        let store = CoordinatorStore::open(&path).unwrap();
        for i in 0..4u32 {
            store
                .set_param(
                    dataflow,
                    ParamKey::new(format!("k{i}")).unwrap(),
                    serde_json::json!(i),
                )
                .unwrap();
        }
        store.compact(MutationSeq::new(2)).unwrap();
    }

    let reopened = CoordinatorStore::open(&path).unwrap();
    assert_eq!(reopened.compacted_before().unwrap(), MutationSeq::new(2));
    assert!(reopened.mutations_since(MutationSeq::new(1), 10).is_err());
    let batch = reopened.mutations_since(MutationSeq::new(2), 10).unwrap();
    assert_eq!(batch.entries.len(), 2);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn reopening_a_store_this_crate_created_is_never_a_schema_mismatch() {
    let path = temp_path("schema-ok");
    let _ = std::fs::remove_file(&path);

    CoordinatorStore::open(&path).unwrap();
    CoordinatorStore::open(&path).unwrap();
    CoordinatorStore::open(&path).unwrap();

    let _ = std::fs::remove_file(&path);
}

#[test]
fn recreate_store_then_reopen_starts_completely_fresh() {
    let path = temp_path("recreate");
    let _ = std::fs::remove_file(&path);

    let dataflow = DataflowId::generate();
    let key = ParamKey::new("gain").unwrap();
    {
        let store = CoordinatorStore::open(&path).unwrap();
        store
            .set_param(dataflow, key.clone(), serde_json::json!(1))
            .unwrap();
    }

    astrs_store::schema::recreate_store(&path).unwrap();
    let fresh = CoordinatorStore::open(&path).unwrap();
    assert!(fresh.get_param(dataflow, &key).unwrap().is_none());
    assert_eq!(fresh.last_seq().unwrap(), MutationSeq::ZERO);

    let _ = std::fs::remove_file(&path);
}
