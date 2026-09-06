//! §12 conformance zoo, scenario 8/16 (many-nodes contention) — blueprint
//! §12, §20.3.
//!
//! `node_roles.rs::several_nodes` covers this number in name only — three
//! independent nodes is not contention. This file spawns many more at once
//! and gives them something to actually contend over: real OS process
//! creation happening concurrently (the daemon's spawn dispatch fans every
//! node out without waiting for the previous one to finish spawning) under
//! one shared spawn deadline, so a slow fork/exec on a loaded machine
//! competes with every sibling's for the same budget.
//!
//! ```text
//!   40 independent nodes, no edges between them, path: /bin/true
//!            │  daemon dispatches all 40 spawns without serializing them
//!            ▼
//!   every one of the 40 must register and exit inside ONE spawn deadline —
//!   a contention-caused delay on node 37 must not cost node 37 its own
//!   accounting, and must not be masked by the other 39 succeeding
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fmt::Write as _;
use std::time::Duration;

use astrs_daemon::{RunOptions, run_dataflow_with};
use astrs_manifest::Manifest;
use astrs_wire::{DataflowStatus, NodeExitCause, NodeId};

/// How many independent nodes contend for one spawn dispatch at once.
///
/// Large enough that "three nodes all ran" (`several_nodes`) and "forty
/// nodes all ran" are different claims — real OS process creation for this
/// many children at once is genuine load on the spawn path, not a nominal
/// count.
const NODE_COUNT: usize = 40;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("astrs-contention-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

fn manifest_with(count: usize) -> Manifest {
    let mut yaml = String::from("exit_when_nodes_finish: true\nnodes:\n");
    for index in 0..count {
        // `/bin/sh -c 'exit 0'`: a real, independent OS process per node,
        // with no edges between any of them — nothing here serializes their
        // spawns through a shared queue or a graph dependency, so the
        // daemon's own spawn dispatch is the only thing standing between
        // "40 requested" and "40 running". `/bin/sh` rather than
        // `/bin/true`/`/usr/bin/true`: it is the one trivial binary this
        // workspace already relies on being at that exact path on every
        // platform it builds for (see `cluster_m2.rs`'s `CRASHING_PIPELINE`
        // for the same reasoning) — `/bin/true` does not even exist on
        // every machine this suite runs on.
        let _ = writeln!(
            yaml,
            "  - id: n{index}\n    path: /bin/sh\n    args: [\"-c\", \"exit 0\"]"
        );
    }
    Manifest::from_yaml_str(&yaml).expect("a valid manifest")
}

/// A generous but bounded ceiling: forty real `fork`/`exec` calls on a
/// loaded CI machine can be slow, but this scenario's whole point is that
/// none of them should need to be caught by a *false* spawn-deadline
/// failure — genuine contention delays a spawn, it must not lose one.
const SPAWN_DEADLINE: Duration = Duration::from_secs(15);

/// §12 conformance zoo, scenario 8/16: many independent nodes, spawned at
/// once, all accounted for — no false spawn-deadline failure from
/// contention, and no node silently dropped among the others' successes.
#[tokio::test]
async fn many_independent_nodes_all_spawn_and_finish_under_one_deadline() {
    let dir = scratch("many");
    let manifest = manifest_with(NODE_COUNT);
    let options = RunOptions::new()
        .with_runtime_dir(dir.clone())
        .with_working_dir(dir)
        .with_build(false)
        .with_finish_grace(Duration::from_millis(500))
        .with_spawn_deadline(SPAWN_DEADLINE)
        .with_timeout(Duration::from_secs(60));

    let result = run_dataflow_with(&manifest, options)
        .await
        .expect("a result");

    assert_eq!(
        result.node_results.len(),
        NODE_COUNT,
        "every node must be accounted for, not just however many won the race: {result:?}"
    );
    assert_eq!(
        result.status,
        DataflowStatus::Finished,
        "contention must not turn into a dataflow-level failure: {result:?}"
    );
    assert!(
        !result.has_failures(),
        "no node's spawn should be starved into a false failure by its siblings: {result:?}"
    );

    let mut deadline_exceeded = Vec::new();
    let mut spawn_failed = Vec::new();
    let mut other_failure = Vec::new();
    for index in 0..NODE_COUNT {
        let id = NodeId::new(format!("n{index}")).expect("a legal node id");
        match result.node_results.get(&id) {
            Some(NodeExitCause::SpawnDeadlineExceeded { .. }) => deadline_exceeded.push(index),
            Some(NodeExitCause::SpawnFailed { .. }) => spawn_failed.push(index),
            Some(NodeExitCause::Success | NodeExitCause::ExitCode { code: 0 }) => {}
            other => other_failure.push((index, other.cloned())),
        }
    }
    assert!(
        deadline_exceeded.is_empty(),
        "contention among {NODE_COUNT} siblings must not cost any of them a false spawn-deadline \
         failure: nodes {deadline_exceeded:?} were falsely caught by it"
    );
    assert!(
        spawn_failed.is_empty(),
        "no node's spawn should fail outright under contention: {spawn_failed:?}"
    );
    assert!(
        other_failure.is_empty(),
        "every node must exit cleanly: {other_failure:?}"
    );
}
