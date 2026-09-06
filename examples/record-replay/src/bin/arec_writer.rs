//! `arec-writer` — the live graph's recorder (blueprint §14).
//!
//! An ordinary graph node that streams everything its inputs deliver into an
//! `.arec` session through [`astrs_recording::Writer`]: HLC-ordered,
//! zstd-framed, with a seekable footer index.
//!
//! ```text
//!   [sensor] ──frames──────┐
//!                          ├──► [recorder] ──► $RECORD_REPLAY_SESSION (.arec)
//!   [detector] ──detections┘
//! ```
//!
//! # Why an example node rather than `astrs-record-node`
//!
//! The production path for this is one line of manifest sugar — `record:
//! [sensor/frames, detector/detections]` — which lowers to the shipped
//! `astrs-record-node` binary (§8.3, §14). That binary is looked up on
//! `PATH`, because a manifest that named a build directory would not survive
//! being deployed; and its destination `.arec` comes from the sugar rather
//! than from the environment. Neither fits an example that must run straight
//! out of a `cargo build` and must not write into the checkout.
//!
//! So this node does the same job over the same public API — `astrs-recording`
//! for the container, [`Node::input_source`] for the entry names — with the
//! destination read from `$RECORD_REPLAY_SESSION` (default: a file under the
//! system temporary directory). The file it produces is an ordinary `.arec`:
//! `astrs bag info`, `astrs replay --into` and `astrs-replay-node` all read it
//! without knowing which writer produced it, and the conformance suite proves
//! exactly that by replaying it.
//!
//! # Naming entries by their *source*
//!
//! An entry's `node`/`output` are the **producer's**, never this node's own
//! input id — a recording is a record of what crossed an edge, and the same
//! edge recorded by two different recorders must produce the same entry.
//! [`Node::input_source`] answers that question from the wiring, so this
//! binary works unchanged whatever its inputs happen to be called.

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use astrs_recording::{Writer, WriterOptions};
use astrs_wire::{DataId, NodeId, PortRef};
use record_replay::session_path;

fn main() -> ExitCode {
    match record() {
        Ok((entries, path)) => {
            println!(
                "arec-writer: recorded {entries} entries to {}",
                path.display()
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("arec-writer: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Records every input into an `.arec` session until the graph is done.
fn record() -> Result<(u64, std::path::PathBuf), Box<dyn std::error::Error>> {
    let path = session_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let (node, mut events) = Node::init_from_env()?;

    // The header carries the dataflow id and this run's HLC epoch, so a
    // recording identifies the execution it came from (§14).
    let mut writer = Writer::create(
        &path,
        WriterOptions::new(node.dataflow_id(), node.hlc_now()),
    )?;
    node.log_info(format!("recording to {}", path.display()));

    let mut entries = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, data } => {
                let (source_node, source_output) = source_of(&node, &id);
                writer.append_parts(source_node, source_output, meta, data.to_vec())?;
                entries += 1;
            }
            Event::Stop(_) | Event::AllInputsClosed => break,
            _ => {}
        }
    }

    // Same reason every sink in this estate drains: the control lane
    // pre-empts queued data (§11.2), so the notice that ended the loop above
    // may have overtaken payloads still sitting in this node's queues. A
    // recording that silently lost its tail would be worse than none.
    while let Some(event) = events.try_recv() {
        if let Event::Input { id, meta, data } = event {
            let (source_node, source_output) = source_of(&node, &id);
            writer.append_parts(source_node, source_output, meta, data.to_vec())?;
            entries += 1;
        }
    }

    // `finish` is what writes the footer index and the trailer that makes the
    // file seekable. Without it the session is still *readable* — through
    // `Reader::open_or_recover`'s scan path — but `astrs replay --into` could
    // not tell which nodes it covers without reading every entry.
    writer.finish()?;
    node.log_info(format!("recorded {entries} entries"));
    Ok((entries, path))
}

/// The producer `(node, output)` behind one of this node's inputs.
///
/// The fallback (this node's own input id, doubling as both halves) is
/// defensive only: every input the daemon delivers an [`Event::Input`] for has
/// a declared source in this node's spawn spec.
fn source_of(node: &Node, input: &DataId) -> (NodeId, DataId) {
    node.input_source(input.as_str())
        .map(PortRef::into_parts)
        .unwrap_or_else(|| (NodeId::sanitized(input.as_str()), input.clone()))
}
