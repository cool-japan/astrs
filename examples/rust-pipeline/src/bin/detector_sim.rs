//! `detector-sim` — the middle of the canonical pipeline (blueprint §8.1,
//! §9.1).
//!
//! Reads `std/media/v1/Image[pixel=rgb8]` frames, publishes
//! `std/vision/v1/Detections` — the shape §9.1's flagship snippet has:
//!
//! ```text
//!   [camera-sim] ──frames──► [detector-sim] ──detections──► [recorder-sim]
//! ```
//!
//! This is the node that shows the *typed* output handle:
//! `node.output::<Detections>("detections")` hands back an
//! [`astrs_node_api::Output`] that takes the struct itself. Encoding, framing
//! and plane selection are the handle's problem, not the node's.
//!
//! # Backpressure is declared, not coded
//!
//! The manifest gives this node's input `queue_size: 2` and
//! `queue_policy: drop_oldest` (§11.2). A detector that falls behind therefore
//! *loses old frames* rather than growing a queue — the right answer for a
//! perception stage, and one that takes no code here at all.

use std::process::ExitCode;

use astrs_node_api::message::Image;
use astrs_node_api::{Event, Node};
use rust_pipeline::{DETECTIONS_PORT, Detections, FRAMES_PORT, detections_for};

fn main() -> ExitCode {
    match detect() {
        Ok(count) => {
            println!("detector-sim: published {count} detection messages");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("detector-sim: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Turns every frame into detections until the camera's port closes.
fn detect() -> Result<u64, Box<dyn std::error::Error>> {
    let (mut node, mut events) = Node::init_from_env()?;
    // The typed handle: `Output<Detections>` will only accept `Detections`,
    // and the manifest's `output_types:` entry for this port is checked
    // against `Detections::URN` at startup (§9.2, `ASTRS_TYPE_CHECK`).
    let mut detections = node.output::<Detections>(DETECTIONS_PORT)?;
    node.log_info("detector up");

    let mut published = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, data, meta } if id.as_str() == FRAMES_PORT => {
                // Zero copy where the plane allows it: `view` decodes out of
                // the payload's buffer, which is the shared-memory slot itself
                // once the route has been upgraded (§6.3).
                let frame: Image = data.view()?;
                let found = detections_for(published, frame.width, frame.height);
                detections.send(found, meta.follow())?;
                published += 1;
            }
            // The camera closed its port: there will be no more frames, so
            // this stage is done. A node that ignored this would hang forever
            // on a graph that has finished.
            Event::InputClosed { id, .. } if id.as_str() == FRAMES_PORT => {
                // The control lane pre-empts queued data (§11.2), so frames
                // published before the closure may still be waiting in this
                // node's queue. Drain them before leaving, or the last frames
                // of every run vanish.
                while let Some(pending) = events.try_recv() {
                    if let Event::Input { id, data, meta } = pending
                        && id.as_str() == FRAMES_PORT
                    {
                        let frame: Image = data.view()?;
                        let found = detections_for(published, frame.width, frame.height);
                        detections.send(found, meta.follow())?;
                        published += 1;
                    }
                }
                node.log_info("frames closed; finishing");
                break;
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {published} messages: {cause}"));
                break;
            }
            _ => {}
        }
    }

    detections.close()?;
    node.log_info(format!("published {published} detection messages"));
    Ok(published)
}
