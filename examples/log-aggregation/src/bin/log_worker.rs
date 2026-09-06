//! `log-worker` — emits a deterministic, marker-tagged log line on every
//! tick, cycling through every severity level (blueprint §8.4, §13).
//!
//! One binary, spawned twice under different ids and tick cadences (`env:
//! LOG_WORKER_ID`) — the same reuse `restart-policies` makes of its single
//! worker binary.
//!
//! ```text
//!   astrs/timer/millis/N ──► tick ──► [worker] ──log_*()──► the daemon
//! ```
//!
//! This node declares no `outputs:` at all: the only thing it produces is
//! its own log stream, read back by `log-aggregator` through the virtual
//! `astrs/logs` input rather than through any `node/output` edge.

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use astrs_wire::LogLevel;
use log_aggregation::{TICK_PORT, level_label_for, marker_message, records_per_worker, worker_id};

fn main() -> ExitCode {
    match emit() {
        Ok(sent) => {
            println!("log-worker: emitted {sent} marker records");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("log-worker: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Emits the record budget, one marker per tick, then finishes.
fn emit() -> Result<u64, Box<dyn std::error::Error>> {
    let id = worker_id();
    let budget = records_per_worker();
    let (node, mut events) = Node::init_from_env()?;
    node.log_info(format!("log-worker[{id}] up: {budget} marker records"));

    let mut sent = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id: input, .. } if input.as_str() == TICK_PORT => {
                node.log(wire_level_for(sent), marker_message(sent));
                sent += 1;
                if sent >= budget {
                    break;
                }
            }
            Event::Stop(cause) => {
                node.log_info(format!("log-worker[{id}] stopping after {sent}: {cause}"));
                break;
            }
            _ => {}
        }
    }

    node.log_info(format!("log-worker[{id}] emitted {sent} marker records"));
    Ok(sent)
}

/// The wire-level severity for record `seq`, matching
/// [`log_aggregation::level_label_for`].
fn wire_level_for(seq: u64) -> LogLevel {
    match level_label_for(seq) {
        "trace" => LogLevel::Trace,
        "debug" => LogLevel::Debug,
        "info" => LogLevel::Info,
        "warn" => LogLevel::Warn,
        _ => LogLevel::Error,
    }
}
