//! `bag-feeder` — writes a tiny fixture `.db3` bag in code, reads it back,
//! and republishes each message into the graph (blueprint §10.6).
//!
//! ```text
//!   [write .db3] ──► [read .db3] ──► astrs/timer/millis/50 ──► [bag-feeder] ──feed──►
//! ```
//!
//! No checked-in binary fixture and no external ROS install: the bag is
//! written with [`astrs_rosbag::db3::Writer`] and read straight back with
//! [`astrs_rosbag::db3::Reader`], both before the event loop starts, so a
//! spawn failure in either shows up as this node failing to start rather
//! than as a silent empty run.

use std::process::ExitCode;

use astrs_idl::generated::std_msgs::String as StdString;
use astrs_node_api::{Event, Node};
use astrs_rosbag::db3::Reader;
use rosbag_reader::{ENV_BAG_PATH, ENV_MESSAGES, FEED_PORT, TICK_PORT, bag_path, message_budget};

fn main() -> ExitCode {
    match feed() {
        Ok(published) => {
            println!("bag-feeder: republished {published} bag messages");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("bag-feeder: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Writes the fixture bag, reads it back into memory, then republishes one
/// message per tick until the bag is exhausted.
fn feed() -> Result<u64, Box<dyn std::error::Error>> {
    let budget = message_budget(std::env::var(ENV_MESSAGES).ok().as_deref());
    let path = bag_path();
    rosbag_reader::write_fixture_bag(&path, budget)?;

    let reader = Reader::open(&path)?;
    let messages: Vec<StdString> = reader
        .iter_messages()
        .map(|row| {
            let row = row?;
            let decoded: StdString = astrs_cdr::from_bytes_tolerant(&row.data)?;
            Ok::<_, Box<dyn std::error::Error>>(decoded)
        })
        .collect::<Result<_, _>>()?;

    let (mut node, mut events) = Node::init_from_env()?;
    let mut feed = node.output::<StdString>(FEED_PORT)?;
    node.log_info(format!(
        "bag-feeder up: {} messages from {}",
        messages.len(),
        path.display()
    ));

    let mut published = 0_usize;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, .. } if id.as_str() == TICK_PORT => {
                let Some(message) = messages.get(published) else {
                    break;
                };
                feed.send(message.clone(), meta.follow())?;
                published += 1;
                if published >= messages.len() {
                    break;
                }
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {published} messages: {cause}"));
                break;
            }
            _ => {}
        }
    }

    feed.close()?;
    node.log_info(format!(
        "republished {published} of {} bag messages (bag at {}, override {ENV_BAG_PATH} in the manifest's env:)",
        messages.len(),
        path.display()
    ));
    Ok(published as u64)
}
