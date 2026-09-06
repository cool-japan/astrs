//! `recorder-sim` — the sink of the canonical pipeline (blueprint §8.1).
//!
//! Subscribes to both edges, counts what arrives, and writes a JSON tally when
//! every input has closed:
//!
//! ```text
//!   [camera-sim] ──frames─────┐
//!                             ├──► [recorder-sim] ──► PIPELINE_SUMMARY (JSON)
//!   [detector-sim] ──detections──┘
//! ```
//!
//! # `record:` sugar versus a real node
//!
//! §8.1's own recorder is one line — `record: [camera/frames,
//! detector/detections]` — which the manifest expands into
//! `astrs-record-node` and a `.arec` file (§14). That path belongs to the
//! recording wave; this example keeps the *shape* of §8.1 with an ordinary
//! `path:` node, so the graph is three real processes and the tally is
//! something a test can read today. The manifest says so too.
//!
//! # Why the exit is driven by `AllInputsClosed`
//!
//! A sink cannot know how many messages are coming — that is the producer's
//! business — so it waits to be told the graph is done. When both producers
//! close their ports the daemon delivers [`Event::InputClosed`] for each and
//! then [`Event::AllInputsClosed`], which is this node's cue to write its
//! tally and finish. With `exit_when_nodes_finish: true` in the manifest, that
//! is also what ends the whole run.

use std::process::ExitCode;

use astrs_node_api::message::Image;
use astrs_node_api::{Event, Node};
use rust_pipeline::{
    Detections, PipelineSummary, RECORDER_DETECTIONS_PORT, RECORDER_FRAMES_PORT, summary_path,
};

fn main() -> ExitCode {
    match record() {
        Ok(summary) => {
            println!(
                "recorder-sim: {} frames, {} detection messages, {} boxes",
                summary.frames, summary.detections, summary.boxes
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("recorder-sim: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Counts everything both inputs deliver, then writes the tally.
fn record() -> Result<PipelineSummary, Box<dyn std::error::Error>> {
    let path = summary_path();
    let (node, mut events) = Node::init_from_env()?;
    node.log_info(format!("recorder up; tally goes to {}", path.display()));

    let mut closed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut summary = PipelineSummary {
        frames: 0,
        detections: 0,
        boxes: 0,
        inputs_closed: false,
    };

    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, data, .. } if id.as_str() == RECORDER_FRAMES_PORT => {
                // Decoded rather than merely counted: a recorder that never
                // looked at its payloads would not prove the pipeline carried
                // anything decodable.
                let frame: Image = data.view()?;
                if summary.frames == 0 {
                    node.log_info(format!(
                        "first frame: {}x{} {}",
                        frame.width,
                        frame.height,
                        frame.urn()
                    ));
                }
                summary.frames += 1;
            }
            Event::Input { id, data, .. } if id.as_str() == RECORDER_DETECTIONS_PORT => {
                let found: Detections = data.view()?;
                summary.detections += 1;
                summary.boxes += found.len() as u64;
            }
            // Both producers are done. Counted here rather than waiting for
            // `AllInputsClosed` alone: a sink knows which inputs it declared,
            // and closing is per-edge information it can act on without a
            // second announcement.
            Event::InputClosed { id, .. } => {
                closed.insert(id.as_str().to_owned());
                if closed.len() >= 2 {
                    summary.inputs_closed = true;
                    break;
                }
            }
            Event::Error(message) => node.log_warn(message),
            Event::AllInputsClosed => {
                summary.inputs_closed = true;
                break;
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping early: {cause}"));
                break;
            }
            _ => {}
        }
    }

    // The control lane pre-empts queued data (§11.2), so `InputClosed` can
    // arrive while frames its producer sent *earlier* are still in this
    // node's own queue. Leaving now would discard them — and a tally that
    // silently dropped the tail would be worse than no tally. So the stream
    // is drained to empty before the summary is written.
    while let Some(event) = events.try_recv() {
        match event {
            Event::Input { id, data, .. } if id.as_str() == RECORDER_FRAMES_PORT => {
                let _: Image = data.view()?;
                summary.frames += 1;
            }
            Event::Input { id, data, .. } if id.as_str() == RECORDER_DETECTIONS_PORT => {
                let found: Detections = data.view()?;
                summary.detections += 1;
                summary.boxes += found.len() as u64;
            }
            _ => {}
        }
    }

    std::fs::write(&path, summary.to_json()?)?;
    node.log_info(format!(
        "recorded {} frames and {} detection messages",
        summary.frames, summary.detections
    ));
    Ok(summary)
}
