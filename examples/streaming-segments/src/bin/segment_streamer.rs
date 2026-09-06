//! `segment-streamer` — publishes deterministic large payloads in
//! fixed-size chunks (blueprint §9.4).
//!
//! ```text
//!   astrs/timer/millis/50 ──► tick ──► [streamer] ──chunks──► (collector)
//! ```
//!
//! One [`StreamWriter`] lives for the whole run: each tick hands it a whole
//! [`segment_payload`] via [`Node::stream_segment`], which does its own
//! `session_id`/`segment_id`/`seq`/`fin` bookkeeping (§9.4) — this binary
//! never numbers a chunk by hand. Reusing the same writer across every
//! segment is what makes the writer's own segment index line up with this
//! example's own segment index: `fin` advances it by exactly one, in order,
//! same as the loop below.

use std::process::ExitCode;

use astrs_node_api::{Event, Node, StreamWriter};
use streaming_segments::{
    CHUNK_BYTES, CHUNKS_PORT, SEGMENT_BYTES, TICK_PORT, segment_count, segment_payload,
};

fn main() -> ExitCode {
    match publish() {
        Ok(segments) => {
            println!("segment-streamer: published {segments} segments");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("segment-streamer: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Publishes the segment budget, one per tick, then closes the port.
fn publish() -> Result<u64, Box<dyn std::error::Error>> {
    let budget = segment_count();
    let (mut node, mut events) = Node::init_from_env()?;
    let mut chunks = node.raw_output(CHUNKS_PORT)?;
    let mut writer = StreamWriter::new();
    node.log_info(format!(
        "streamer up: {budget} segments of {SEGMENT_BYTES} B in {CHUNK_BYTES}-B chunks"
    ));

    let mut published = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, .. } if id.as_str() == TICK_PORT => {
                let payload = segment_payload(published, SEGMENT_BYTES);
                let sent = node.stream_segment(&mut chunks, &mut writer, &payload, CHUNK_BYTES)?;
                node.log_debug(format!("segment {published}: {sent} chunks"));
                published += 1;
                if published >= budget {
                    break;
                }
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {published} segments: {cause}"));
                break;
            }
            _ => {}
        }
    }

    // Closing tells the collector the stream is over, so it can finish
    // rather than waiting on a segment that will never arrive.
    chunks.close()?;
    node.log_info(format!("published {published} segments"));
    Ok(published)
}
