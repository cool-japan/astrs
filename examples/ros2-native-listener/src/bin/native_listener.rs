//! `native-listener` — subscribes to `/chatter` through a self-hosted
//! [`Ros2Node`] and tallies what arrives, with no `ros2:` bridge sugar
//! (blueprint §10.4).
//!
//! ```text
//!   [native-talker] ──/chatter (real DDS/RTPS)──► [native-listener] ──► NATIVE_LISTENER_REPORT (JSON)
//! ```

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use astrs_node_api::{Event, Node};
use astrs_ros2::msg::std_msgs::String as ChatMessage;
use astrs_ros2::node::{ContextOptions, NodeOptions, Ros2Context, Ros2Node};
use astrs_ros2::qos::QosProfile;
use ros2_native_listener::{
    DOMAIN_ID, ENV_LISTENER_MESSAGES, LISTEN_DEADLINE, ListenerReport, TOPIC, message_budget,
    report_path,
};

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("native-listener: failed to start the tokio runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(listen()) {
        Ok(report) if report.received > 0 => {
            println!(
                "native-listener: received {} messages (budget met: {})",
                report.received, report.met_budget
            );
            ExitCode::SUCCESS
        }
        Ok(report) => {
            // Zero messages within the deadline is a real, actionable
            // failure — most likely a discovery join that this sandbox
            // refused (see this crate's README) — not a quiet success.
            eprintln!(
                "native-listener: received no messages within {LISTEN_DEADLINE:?}; {report:?}"
            );
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("native-listener: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Subscribes and tallies until the budget is met or [`LISTEN_DEADLINE`]
/// runs out, staying responsive to [`Event::Stop`] throughout.
async fn listen() -> Result<ListenerReport, Box<dyn std::error::Error>> {
    let budget = message_budget(std::env::var(ENV_LISTENER_MESSAGES).ok().as_deref());
    let path = report_path();
    let (node, mut events) = Node::init_from_env()?;
    node.log_info(format!(
        "native-listener up: budget {budget} on {TOPIC}, report goes to {}",
        path.display()
    ));

    let context = Ros2Context::new(ContextOptions::new(DOMAIN_ID)).await?;
    let ros_node = Ros2Node::new(
        Arc::clone(&context),
        "astrs_native_listener",
        NodeOptions::default().with_parameter_services(false),
    )
    .await?;
    let subscription = ros_node
        .create_subscription::<ChatMessage>(TOPIC, QosProfile::default())
        .await?;

    let deadline = Instant::now() + LISTEN_DEADLINE;
    let mut report = ListenerReport::default();
    loop {
        if report.received >= budget {
            report.met_budget = true;
            break;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        tokio::select! {
            event = events.recv_async() => match event {
                Some(Event::Stop(cause)) => {
                    node.log_info(format!("stopping after {} messages: {cause}", report.received));
                    break;
                }
                None => break,
                _ => {}
            },
            received = subscription.recv() => {
                let (message, _info) = received?;
                report.received += 1;
                report.texts.push(message.data);
            }
            () = tokio::time::sleep(remaining) => break,
        }
    }

    context.shutdown().await;
    std::fs::write(&path, report.to_json()?)?;
    node.log_info(format!(
        "received {} messages (budget met: {})",
        report.received, report.met_budget
    ));
    Ok(report)
}
