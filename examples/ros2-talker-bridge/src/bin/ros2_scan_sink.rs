//! `ros2-scan-sink` — decodes and tallies every scan `bridge.yml`'s
//! `lidar-in` bridges from `ros2-talker` (blueprint §10.5).
//!
//! ```text
//!   [lidar-in: ros2:] ──scan──► [ros2-scan-sink] ──► ROS2_SCAN_SINK_REPORT (JSON)
//! ```

use std::process::ExitCode;

use astrs_data::AstrsMessage;
use astrs_node_api::{Event, Node};
use astrs_ros2::msg::sensor_msgs::LaserScan;
use ros2_talker_bridge::{SinkSummary, report_path};

/// The input port `bridge.yml` wires `lidar-in/scan` to.
const SCAN_PORT: &str = "scan";

fn main() -> ExitCode {
    match sink() {
        Ok(summary) => {
            println!("ros2-scan-sink: tallied {} scans", summary.scans);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("ros2-scan-sink: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Tallies every bridged scan for as long as the graph runs — `lidar-in`
/// has no natural finish, so unlike every other sink in this estate this
/// one writes its running tally back on *every* scan rather than only once
/// at the end (see this crate's README for why `bridge.yml` runs until
/// stopped).
fn sink() -> Result<SinkSummary, Box<dyn std::error::Error>> {
    let path = report_path();
    let (node, mut events) = Node::init_from_env()?;
    node.log_info(format!(
        "ros2-scan-sink up; report goes to {}",
        path.display()
    ));

    let mut summary = SinkSummary::default();

    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, data, .. } if id.as_str() == SCAN_PORT => {
                // `LaserScan` is `astrs-idl`'s generated type: neither it
                // nor `astrs_node_api::message::FromPayload` is local to
                // this crate, so the orphan rule rules out a `FromPayload`
                // bridge. `AstrsMessage::from_record_batch` is the same
                // decode `.view()` would perform, called directly.
                let scan = LaserScan::from_record_batch(data.batch()?)?;
                summary.scans += 1;
                summary.last_ranges = scan.ranges;
                std::fs::write(&path, summary.to_json()?)?;
            }
            Event::InputClosed { .. } | Event::AllInputsClosed => break,
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {} scans: {cause}", summary.scans));
                break;
            }
            _ => {}
        }
    }

    node.log_info(format!("tallied {} scans", summary.scans));
    Ok(summary)
}
