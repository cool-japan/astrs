//! `bag-sink` — tallies every message `astrs-rosbag` fed into the pipeline
//! (blueprint §10.6).
//!
//! ```text
//!   [bag-feeder] ──feed──► [bag-sink] ──► ROSBAG_READER_REPORT (JSON)
//! ```

use std::process::ExitCode;

use astrs_data::AstrsMessage;
use astrs_idl::generated::std_msgs::String as StdString;
use astrs_node_api::{Event, Node};
use rosbag_reader::{BagSummary, FEED_PORT, report_path};

fn main() -> ExitCode {
    match sink() {
        Ok(summary) => {
            println!("bag-sink: tallied {} messages", summary.messages);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("bag-sink: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Tallies everything `feed` delivers, then writes the tally once it closes.
fn sink() -> Result<BagSummary, Box<dyn std::error::Error>> {
    let path = report_path();
    let (node, mut events) = Node::init_from_env()?;
    node.log_info(format!("bag-sink up; report goes to {}", path.display()));

    let mut summary = BagSummary::default();

    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, data, .. } if id.as_str() == FEED_PORT => {
                // `StdString` is `astrs-idl`'s generated type: neither it nor
                // `astrs_node_api::message::FromPayload` is local to this
                // crate, so the orphan rule rules out a `FromPayload` bridge
                // (unlike `rust_pipeline::Detections`, defined in that
                // example's own crate). `AstrsMessage::from_record_batch` is
                // the same decode `.view()` would perform, called directly.
                let message = StdString::from_record_batch(data.batch()?)?;
                summary.messages += 1;
                summary.texts.push(message.data);
            }
            Event::InputClosed { .. } | Event::AllInputsClosed => break,
            Event::Stop(cause) => {
                node.log_info(format!("stopping early: {cause}"));
                break;
            }
            _ => {}
        }
    }

    // The control lane pre-empts queued data (§11.2): drain whatever is
    // still queued before writing the tally.
    while let Some(event) = events.try_recv() {
        if let Event::Input { id, data, .. } = event
            && id.as_str() == FEED_PORT
        {
            let message = StdString::from_record_batch(data.batch()?)?;
            summary.messages += 1;
            summary.texts.push(message.data);
        }
    }

    std::fs::write(&path, summary.to_json()?)?;
    node.log_info(format!("tallied {} messages", summary.messages));
    Ok(summary)
}
