//! `ros2-talker` — publishes `sensor_msgs/msg/LaserScan` on `/scan` through
//! a self-hosted [`Ros2Node`] (blueprint §10.5).
//!
//! ```text
//!   [ros2-talker] ──/scan (real DDS/RTPS)──► (bridge.yml's lidar-in, run separately)
//! ```
//!
//! The only node in `dataflow.yml` — see that file's header comment for why
//! the declarative bridge itself lives in a separate manifest,
//! `bridge.yml`, run separately (see this crate's README).

use std::process::ExitCode;
use std::sync::Arc;

use astrs_node_api::{Event, Node};
use astrs_ros2::msg::sensor_msgs::LaserScan;
use astrs_ros2::node::{ContextOptions, NodeOptions, Ros2Context, Ros2Node};
use astrs_ros2::qos::QosProfile;
use ros2_talker_bridge::{DOMAIN_ID, ENV_SCANS, PUBLISH_INTERVAL, TOPIC, scan_budget, scan_for};

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("ros2-talker: failed to start the tokio runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(talk()) {
        Ok(sent) => {
            println!("ros2-talker: published {sent} scans");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("ros2-talker: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Publishes `ros2-talker`'s scan budget over real RTPS, pacing itself
/// against [`PUBLISH_INTERVAL`] and staying responsive to [`Event::Stop`]
/// throughout — the same `select!`-over-two-async-sources shape
/// `bins/astrs-ros2-bridge-node`'s own event loop uses.
async fn talk() -> Result<u64, Box<dyn std::error::Error>> {
    let budget = scan_budget(std::env::var(ENV_SCANS).ok().as_deref());
    let (node, mut events) = Node::init_from_env()?;
    node.log_info(format!("ros2-talker up: {budget} scans on {TOPIC}"));

    let context = Ros2Context::new(ContextOptions::new(DOMAIN_ID)).await?;
    let ros_node = Ros2Node::new(
        Arc::clone(&context),
        "astrs_ros2_talker",
        NodeOptions::default().with_parameter_services(false),
    )
    .await?;
    let publisher = ros_node
        .create_publisher::<LaserScan>(TOPIC, QosProfile::default())
        .await?;

    let mut interval = tokio::time::interval(PUBLISH_INTERVAL);
    let mut sent = 0_u64;
    loop {
        tokio::select! {
            event = events.recv_async() => match event {
                Some(Event::Stop(cause)) => {
                    node.log_info(format!("stopping after {sent} scans: {cause}"));
                    break;
                }
                None => break,
                _ => {}
            },
            _ = interval.tick() => {
                publisher.publish(&scan_for(sent)).await?;
                sent += 1;
                if sent >= budget {
                    break;
                }
            }
        }
    }

    context.shutdown().await;
    node.log_info(format!("published {sent} scans"));
    Ok(sent)
}
