//! `showcase-detector` — turns each frame into a one-byte detection count
//! (blueprint §8.1's canonical camera/detector pair).
//!
//! ```text
//!   [camera] ──frames──► [detector] ──detections──► (nobody; a sink in this example)
//! ```

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use tui_showcase::{FRAMES_PORT, frame_index_of};

fn main() -> ExitCode {
    match run() {
        Ok(detected) => {
            println!("showcase-detector: processed {detected} frames");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("showcase-detector: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Turns every frame into a detection count until `camera`'s port closes.
fn run() -> Result<u64, Box<dyn std::error::Error>> {
    let (mut node, mut events) = Node::init_from_env()?;
    let mut detections = node.raw_output("detections")?;
    node.log_info("detector up");

    let mut processed = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, data } if id.as_str() == FRAMES_PORT => {
                let Some(index) = frame_index_of(data.bytes()) else {
                    node.log_warn("an undecodable frame arrived");
                    continue;
                };
                // A trivial "detection": how many set bits the frame index
                // has, encoded as a single byte — enough to be a real,
                // deterministic per-frame output without needing any real
                // computer vision for a showcase example.
                let count = index.count_ones() as u8;
                detections.send_bytes([count], meta.follow())?;
                processed += 1;
            }
            Event::InputClosed { id, .. } if id.as_str() == FRAMES_PORT => {
                node.log_info("camera is done; finishing");
                break;
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {processed} frames: {cause}"));
                break;
            }
            _ => {}
        }
    }

    detections.close()?;
    node.log_info(format!("processed {processed} frames"));
    Ok(processed)
}
