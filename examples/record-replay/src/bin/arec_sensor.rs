//! `arec-sensor` — the live source of the record/replay pair (blueprint §14).
//!
//! Publishes one deterministic byte payload per timer tick on `frames`, then
//! closes the port and finishes.
//!
//! ```text
//!   astrs/timer/millis/20 ──► tick ──► [sensor] ──frames──► (detector, recorder)
//! ```
//!
//! This is the node the replay graph *replaces*: `astrs replay session.arec
//! --into replay.yml` keeps the id `sensor` and the output `frames`, and swaps
//! only what produces them for `astrs-replay-node`. Nothing downstream is
//! rewired, which is the whole point of §14's in-place replacement.

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use record_replay::{FRAMES_PORT, TICK_PORT, frame_budget, frame_payload};

fn main() -> ExitCode {
    match publish() {
        Ok(frames) => {
            println!("arec-sensor: published {frames} frames");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("arec-sensor: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Publishes the frame budget, one payload per tick, then finishes.
fn publish() -> Result<u64, Box<dyn std::error::Error>> {
    let budget = frame_budget();
    let (mut node, mut events) = Node::init_from_env()?;
    let mut frames = node.raw_output(FRAMES_PORT)?;
    node.log_info(format!("sensor up: {budget} frames of raw bytes"));

    let mut published = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, .. } if id.as_str() == TICK_PORT => {
                // `meta.follow()` keeps the causal chain: the frame is stamped
                // as *caused by* the tick that produced it (§4.3), and that
                // metadata is what the recorder stores beside the payload.
                frames.send_bytes(frame_payload(published), meta.follow())?;
                published += 1;
                if published >= budget {
                    break;
                }
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {published} frames: {cause}"));
                break;
            }
            _ => {}
        }
    }

    // Closing tells the detector and the recorder that the stream is over, so
    // the recorder finalises its `.arec` footer instead of being killed with
    // an unfinished file behind it.
    frames.close()?;
    node.log_info(format!("published {published} frames"));
    Ok(published)
}
