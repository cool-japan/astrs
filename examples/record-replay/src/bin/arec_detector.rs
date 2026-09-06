//! `arec-detector` — the stage the replay graph swaps out (blueprint §14).
//!
//! Consumes `frames`, publishes one `detections` payload per frame, and
//! finishes when its input closes. It exists so that the live graph is a
//! *pipeline* rather than a source and a tape recorder — and so that the
//! `.arec` session holds two ports, which is what gives the replay's
//! `--only sensor/frames` filter something to actually filter.
//!
//! ```text
//!   [sensor] ──frames──► [detector] ──detections──► [recorder]
//! ```
//!
//! In `replay.yml` this node is gone: an assertion sink (`sequence-probe`)
//! reads `sensor/frames` in its place. That substitution is §14's headline —
//! *test a new planner against last week's sensor data* — with the new
//! planner's job reduced to the one thing a test can check exactly.

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use record_replay::{DETECTIONS_PORT, FRAMES_PORT, detection_payload};

fn main() -> ExitCode {
    match detect() {
        Ok(count) => {
            println!("arec-detector: emitted {count} detections");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("arec-detector: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Turns every frame into one detection payload.
fn detect() -> Result<u64, Box<dyn std::error::Error>> {
    let (mut node, mut events) = Node::init_from_env()?;
    let mut detections = node.raw_output(DETECTIONS_PORT)?;
    node.log_info("detector up");

    let mut emitted = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, data } if id.as_str() == FRAMES_PORT => {
                detections.send_bytes(detection_payload(&data.to_vec()), meta.follow())?;
                emitted += 1;
            }
            // The control lane pre-empts queued data (§11.2), so leaving on
            // the first `InputClosed` would abandon frames still queued behind
            // it. The stream is drained below instead.
            Event::InputClosed { .. } | Event::AllInputsClosed => break,
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {emitted} detections: {cause}"));
                break;
            }
            _ => {}
        }
    }

    while let Some(event) = events.try_recv() {
        if let Event::Input { id, meta, data } = event
            && id.as_str() == FRAMES_PORT
        {
            detections.send_bytes(detection_payload(&data.to_vec()), meta.follow())?;
            emitted += 1;
        }
    }
    detections.close()?;
    node.log_info(format!("emitted {emitted} detections"));
    Ok(emitted)
}
