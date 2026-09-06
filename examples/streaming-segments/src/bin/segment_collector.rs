//! `segment-collector` — reassembles every segment and verifies its bytes
//! (blueprint §9.4).
//!
//! ```text
//!   [streamer] ──chunks──► [collector] ──► StreamTally (JSON)
//! ```
//!
//! Every event this node's loop sees is offered to a single
//! [`StreamAssembler`]: a chunk is absorbed, a completed segment is verified
//! against `streaming_segments::segment_payload` and tallied, and anything
//! that is not a stream chunk at all (`Stop`, `InputClosed`, …) is handed
//! back unchanged for the loop's own `match` to act on.

use std::process::ExitCode;

use astrs_node_api::{Event, Node, StreamAssembler};
use streaming_segments::{StreamTally, report_path, segment_count};

fn main() -> ExitCode {
    match collect() {
        Ok(tally) => {
            println!(
                "segment-collector: {} segments completed ({} problems)",
                tally.completed,
                tally.problems.len()
            );
            let budget = segment_count();
            if tally.is_clean(budget) {
                ExitCode::SUCCESS
            } else {
                eprintln!("segment-collector: not clean against a budget of {budget}: {tally:?}");
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("segment-collector: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Reassembles and verifies every segment until the streamer's port closes.
fn collect() -> Result<StreamTally, Box<dyn std::error::Error>> {
    let budget = segment_count();
    let (node, mut events) = Node::init_from_env()?;
    node.log_info(format!("collector up: expecting {budget} segments"));

    let mut assembler = StreamAssembler::new();
    let mut tally = StreamTally::new();

    while let Some(event) = events.recv() {
        match assembler.accept_event(event) {
            Ok(Ok(Some(segment))) => {
                // `StreamTally::accept` verifies the reassembled bytes
                // against the same pure generator the streamer used — a
                // byte the wire dropped, duplicated or reordered fails that
                // comparison even though the chunk count matched.
                tally.accept(segment.segment, segment.chunks, &segment.bytes);
                if tally.completed >= budget {
                    break;
                }
            }
            Ok(Ok(None)) => {} // one more chunk absorbed into an open segment
            Ok(Err(handed_back)) => match *handed_back {
                Event::Stop(cause) => {
                    node.log_info(format!(
                        "stopping after {} of {budget} segments: {cause}",
                        tally.completed
                    ));
                    break;
                }
                Event::InputClosed { .. } | Event::AllInputsClosed => {
                    node.log_info("the streamer is done; finishing");
                    break;
                }
                _ => {}
            },
            // A reported gap: `StreamAssembler` has already abandoned the
            // broken segment (see its own docs), so this run continues
            // rather than trying to salvage it.
            Err(error) => tally.problem(error.to_string()),
        }
    }

    let path = report_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, tally.to_json()?)?;
    node.log_info(format!(
        "wrote {} completed segments to {}",
        tally.completed,
        path.display()
    ));
    Ok(tally)
}
