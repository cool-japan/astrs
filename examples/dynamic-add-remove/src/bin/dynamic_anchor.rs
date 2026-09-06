//! `dynamic-anchor` — the always-on node `dataflow.yml` declares, and the
//! producer a dynamically-added `dynamic-worker` reads from (blueprint
//! §8.3, §17).
//!
//! ```text
//!   astrs/timer/millis/50 ──► tick ──► [anchor] ──beats──► (nobody, until `astrs node add`)
//! ```
//!
//! Runs for a while on purpose (2000 beats at 50 ms, a little over a
//! minute by default) so a reader has room to run the `astrs node add`/
//! `astrs node remove` recipe in this example's README against a cluster
//! that is genuinely still up.

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use dynamic_add_remove::{TICK_PORT, anchor_beat_budget, beat_payload};

fn main() -> ExitCode {
    match publish() {
        Ok(published) => {
            println!("dynamic-anchor: published {published} beats");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("dynamic-anchor: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Publishes the beat budget, one per tick, then closes the port.
fn publish() -> Result<u64, Box<dyn std::error::Error>> {
    let budget = anchor_beat_budget();
    let (mut node, mut events) = Node::init_from_env()?;
    let mut beats = node.raw_output("beats")?;
    node.log_info(format!("anchor up: {budget} beats"));

    let mut published = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, .. } if id.as_str() == TICK_PORT => {
                beats.send_bytes(beat_payload(published), meta.follow())?;
                published += 1;
                if published >= budget {
                    break;
                }
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {published} beats: {cause}"));
                break;
            }
            _ => {}
        }
    }

    beats.close()?;
    node.log_info(format!("published {published} beats"));
    Ok(published)
}
