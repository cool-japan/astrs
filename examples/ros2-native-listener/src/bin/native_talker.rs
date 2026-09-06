//! `native-talker` — publishes `std_msgs/msg/String` on `/chatter` through
//! a self-hosted [`Ros2Node`], with no `ros2:` bridge sugar (blueprint
//! §10.4).
//!
//! ```text
//!   [native-talker] ──/chatter (real DDS/RTPS)──► [native-listener]
//! ```
//!
//! Registers with the daemon exactly like any other node in this estate
//! (logging, health checks, `Event::Stop`), but declares no AstRS
//! `inputs:`/`outputs:` at all — everything this node says goes out over a
//! real RTPS participant it creates itself.

use std::process::ExitCode;
use std::sync::Arc;

use astrs_node_api::{Event, Node};
use astrs_ros2::msg::std_msgs::String as ChatMessage;
use astrs_ros2::node::{ContextOptions, NodeOptions, Ros2Context, Ros2Node};
use astrs_ros2::qos::QosProfile;
use ros2_native_listener::{
    DOMAIN_ID, ENV_TALKER_MESSAGES, PUBLISH_INTERVAL, TOPIC, chat_text, message_budget,
};

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("native-talker: failed to start the tokio runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(talk()) {
        Ok(sent) => {
            println!("native-talker: sent {sent} messages");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("native-talker: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Publishes `native-talker`'s message budget over real RTPS, pacing
/// itself against [`PUBLISH_INTERVAL`] and staying responsive to
/// [`Event::Stop`] throughout — the same `select!`-over-two-async-sources
/// shape `bins/astrs-ros2-bridge-node`'s own event loop uses, minus the
/// second, AstRS-side source this node has none of.
async fn talk() -> Result<u64, Box<dyn std::error::Error>> {
    let budget = message_budget(std::env::var(ENV_TALKER_MESSAGES).ok().as_deref());
    let (node, mut events) = Node::init_from_env()?;
    node.log_info(format!("native-talker up: {budget} messages on {TOPIC}"));

    let context = Ros2Context::new(ContextOptions::new(DOMAIN_ID)).await?;
    let ros_node = Ros2Node::new(
        Arc::clone(&context),
        "astrs_native_talker",
        NodeOptions::default().with_parameter_services(false),
    )
    .await?;
    let publisher = ros_node
        .create_publisher::<ChatMessage>(TOPIC, QosProfile::default())
        .await?;

    let mut interval = tokio::time::interval(PUBLISH_INTERVAL);
    let mut sent = 0_u64;
    loop {
        tokio::select! {
            event = events.recv_async() => match event {
                Some(Event::Stop(cause)) => {
                    node.log_info(format!("stopping after {sent} messages: {cause}"));
                    break;
                }
                None => break,
                _ => {}
            },
            _ = interval.tick() => {
                let message = ChatMessage { data: chat_text(sent) };
                publisher.publish(&message).await?;
                sent += 1;
                if sent >= budget {
                    break;
                }
            }
        }
    }

    context.shutdown().await;
    node.log_info(format!("sent {sent} messages"));
    Ok(sent)
}
