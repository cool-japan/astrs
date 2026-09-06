//! `cluster-sensor` — the graph's source, on machine `robot-a`
//! (blueprint §4.2, §8.3).
//!
//! Publishes one [`Reading`] per timer tick, each stamped with the machine
//! this process was placed on. That stamp is what makes the crossing provable
//! downstream: a consumer cannot tell which plane carried a message, but it
//! can read where the message says it came from.
//!
//! ```text
//!   astrs/timer/millis/20 ──► tick ──► [sensor @ robot-a] ──readings──► (planner @ robot-b)
//! ```

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use multi_daemon_cluster::{READINGS_PORT, Reading, TICK_PORT, machine_of, reading_budget};

fn main() -> ExitCode {
    match publish() {
        Ok(readings) => {
            println!("cluster-sensor: published {readings} readings");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("cluster-sensor: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Publishes the reading budget, one per tick, then finishes.
fn publish() -> Result<u64, Box<dyn std::error::Error>> {
    let budget = reading_budget();
    let (mut node, mut events) = Node::init_from_env()?;
    let machine = machine_of(&node);
    let mut readings = node.raw_output(READINGS_PORT)?;
    node.log_info(format!("sensor up on {machine}: {budget} readings"));

    let mut published = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, .. } if id.as_str() == TICK_PORT => {
                let reading = Reading {
                    seq: published,
                    origin: machine.clone(),
                };
                // `meta.follow()` keeps the causal chain across the machine
                // boundary: the HLC in the metadata is what makes `astrs
                // trace` and the merged log ordering work cluster-wide (§4.3).
                readings.send_bytes(reading.to_bytes()?, meta.follow())?;
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

    // Closing is what lets the planner — on the *other* machine — see an
    // orderly end of stream rather than a peer that went quiet (§12).
    readings.close()?;
    node.log_info(format!("published {published} readings from {machine}"));
    Ok(published)
}
