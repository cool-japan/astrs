//! `dynamic-worker` — the node `astrs node add` spawns against a running
//! `dynamic-add-remove` cluster (blueprint §8.3, §17).
//!
//! ```text
//!   [anchor] ──beats──► [worker] ──► WorkerProof (JSON)
//! ```
//!
//! An ordinary spawned node in every respect — `Node::init_from_env` is the
//! same constructor a statically-declared node uses (§8.3's `path: dynamic`
//! sentinel and [`astrs_node_api::Node::init_from_node_id`] are a *different*
//! mechanism, for a node that attaches itself with no process for the
//! daemon to spawn at all; this one is spawned, just later than the rest of
//! the graph). It proves it was really wired to `anchor/beats` — not merely
//! started — by writing down the first and last counter values it actually
//! observed.

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use dynamic_add_remove::{BEATS_PORT, WorkerProof, beat_value_of, proof_path, worker_beat_budget};

fn main() -> ExitCode {
    match watch() {
        Ok(proof) => {
            println!(
                "dynamic-worker: observed {} beats ({:?} .. {:?})",
                proof.beats_seen, proof.first_beat, proof.last_beat
            );
            if proof.is_consistent() {
                ExitCode::SUCCESS
            } else {
                eprintln!("dynamic-worker: the observed beats were not consistent: {proof:?}");
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("dynamic-worker: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Watches `beats` until the budget is met or the session ends, then writes
/// the proof.
fn watch() -> Result<WorkerProof, Box<dyn std::error::Error>> {
    let budget = worker_beat_budget();
    let (node, mut events) = Node::init_from_env()?;
    node.log_info(format!(
        "worker up (generation {}): waiting for {budget} beats",
        node.generation()
    ));

    let mut beats_seen = 0_u64;
    let mut first_beat = None;
    let mut last_beat = None;

    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, data, .. } if id.as_str() == BEATS_PORT => {
                if let Some(value) = beat_value_of(data.bytes()) {
                    first_beat.get_or_insert(value);
                    last_beat = Some(value);
                    beats_seen += 1;
                }
                if beats_seen >= budget {
                    break;
                }
            }
            Event::InputClosed { id, .. } if id.as_str() == BEATS_PORT => {
                node.log_info("anchor finished before the budget was met");
                break;
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {beats_seen} beats: {cause}"));
                break;
            }
            _ => {}
        }
    }

    let proof = WorkerProof {
        is_restart: node.is_restart(),
        pid: std::process::id(),
        beats_seen,
        first_beat,
        last_beat,
    };
    let path = proof_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, proof.to_json()?)?;
    node.log_info(format!(
        "wrote proof of {beats_seen} beats to {}",
        path.display()
    ));
    Ok(proof)
}
