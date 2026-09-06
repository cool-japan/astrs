//! `showcase-camera` — publishes one deterministic frame per tick (blueprint
//! §8.1's canonical camera/detector pair, reused here as the real topology
//! `tui_showcase::tests::ScriptedCluster` renders).
//!
//! ```text
//!   astrs/timer/millis/40 ──► tick ──► [camera] ──frames──► (detector)
//! ```

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use tui_showcase::{TICK_PORT, frame_budget, frame_payload};

fn main() -> ExitCode {
    match publish() {
        Ok(frames) => {
            println!("showcase-camera: published {frames} frames");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("showcase-camera: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Publishes the frame budget, one per tick, then closes the port.
fn publish() -> Result<u64, Box<dyn std::error::Error>> {
    let budget = frame_budget();
    let (mut node, mut events) = Node::init_from_env()?;
    let mut frames = node.raw_output("frames")?;
    node.log_info(format!("camera up: {budget} frames"));

    let mut published = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, .. } if id.as_str() == TICK_PORT => {
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

    frames.close()?;
    node.log_info(format!("published {published} frames"));
    Ok(published)
}
