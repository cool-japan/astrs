//! `fault-source` — the node that fails (blueprint §12).
//!
//! Publishes a few readings so the graph has *done* something, then exits with
//! [`FAILURE_EXIT_CODE`] without closing its output. The abrupt end is the
//! point: the watcher downstream must learn about it from the daemon rather
//! than from a goodbye this node never sent.
//!
//! ```text
//!   astrs/timer/millis/20 ──► [source] ──readings──► [watcher]
//!                                 └─ exit 23 ──► NodeFailed { peer, cause }
//! ```
//!
//! `restart_policy: never` in the manifest, deliberately: a restarted producer
//! would send `Restarted` to its peers instead — which is `restart-policies`'
//! story, not this one.

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use error_propagation::{FAILURE_EXIT_CODE, READINGS_PORT, TICK_PORT, reading_budget};

fn main() -> ExitCode {
    match publish() {
        // The failure this example exists to propagate: an ordinary non-zero
        // exit, which the daemon classifies as `NodeExitCause::ExitCode`.
        Ok(published) => {
            eprintln!("fault-source: failing on purpose after {published} readings");
            ExitCode::from(FAILURE_EXIT_CODE)
        }
        Err(error) => {
            eprintln!("fault-source: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Publishes the reading budget, then returns so `main` can fail.
fn publish() -> Result<u64, Box<dyn std::error::Error>> {
    let budget = reading_budget();
    let (mut node, mut events) = Node::init_from_env()?;
    let mut readings = node.raw_output(READINGS_PORT)?;
    node.log_info(format!(
        "source up: {budget} readings, then exit {FAILURE_EXIT_CODE}"
    ));

    let mut published = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, .. } if id.as_str() == TICK_PORT => {
                readings.send_bytes(published.to_be_bytes(), meta.follow())?;
                published += 1;
                if published >= budget {
                    break;
                }
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {published} readings: {cause}"));
                break;
            }
            _ => {}
        }
    }

    // Deliberately **not** `readings.close()`: that is the explicit spelling,
    // a statement by a node that goes on running, and the daemon answers it
    // immediately with `ProducerFinished` (§12). This node is about to fail,
    // so saying that would be a lie.
    //
    // The handle is still dropped on the way out — that is what makes the
    // consumer see a closure at all rather than a stream that simply stopped
    // — and a dropped handle sends the *teardown* spelling instead. The
    // daemon holds it until the reap, then delivers `ProducerCrashed` because
    // the exit below was non-zero. Nothing here had to know that; the
    // difference is entirely in which frame a drop sends.
    node.log_warn(format!("published {published} readings, now failing"));
    Ok(published)
}
