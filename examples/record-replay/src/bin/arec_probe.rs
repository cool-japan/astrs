//! `arec-probe` — the replay graph's assertion sink (blueprint §14).
//!
//! Consumes `frames` and writes down every payload it saw, hex-encoded, in
//! arrival order. It is the node that stands where the detector stood:
//!
//! ```text
//!   live:    [sensor] ─frames─► [detector] ─detections─► [recorder]
//!   replay:  [sensor := astrs-replay-node] ─frames─► [probe] ─► sequence.json
//! ```
//!
//! Everything it measures is a *byte* fact — count, order, content — because
//! that is exactly what §14 promises replay reproduces. It deliberately
//! measures no timestamp: `astrs-replay-node` re-stamps every republished
//! message with a fresh HLC from its own clock, so a probe that compared
//! timestamps would be failing on the one thing replay is documented not to
//! preserve.
//!
//! The same binary works in a live graph too — it is an ordinary consumer of
//! an ordinary port — which is what lets the conformance suite compare a live
//! observation against a replayed one without changing the observer.

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use record_replay::{FRAMES_PORT, PayloadSequence, sequence_path};

fn main() -> ExitCode {
    match observe() {
        Ok(sequence) => {
            println!(
                "arec-probe: {} payloads, {} bytes, digest {}",
                sequence.count,
                sequence.total_bytes,
                sequence.digest()
            );
            if sequence.problems.is_empty() {
                ExitCode::SUCCESS
            } else {
                for problem in &sequence.problems {
                    eprintln!("arec-probe: {problem}");
                }
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("arec-probe: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Records every payload that arrives on `frames`, then writes the sequence.
fn observe() -> Result<PayloadSequence, Box<dyn std::error::Error>> {
    let path = sequence_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let (node, mut events) = Node::init_from_env()?;
    let mut sequence = PayloadSequence::new(node.id().as_str(), FRAMES_PORT);
    node.log_info(format!("probe up; sequence goes to {}", path.display()));

    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, data, .. } if id.as_str() == FRAMES_PORT => {
                sequence.push(&data.to_vec());
            }
            Event::InputClosed { .. } | Event::AllInputsClosed => {
                sequence.inputs_closed = true;
                break;
            }
            Event::Error(message) => node.log_warn(message),
            Event::Stop(cause) => {
                node.log_info(format!(
                    "stopping after {} payloads: {cause}",
                    sequence.count
                ));
                break;
            }
            _ => {}
        }
    }

    // The control lane pre-empts queued data (§11.2): the notice that ended
    // the loop can overtake payloads its producer sent earlier. A probe that
    // left now would report a short sequence and blame replay for it.
    while let Some(event) = events.try_recv() {
        if let Event::Input { id, data, .. } = event
            && id.as_str() == FRAMES_PORT
        {
            sequence.push(&data.to_vec());
        }
    }

    std::fs::write(&path, sequence.to_json()?)?;
    node.log_info(format!(
        "observed {} payloads, digest {}",
        sequence.count,
        sequence.digest()
    ));
    Ok(sequence)
}
