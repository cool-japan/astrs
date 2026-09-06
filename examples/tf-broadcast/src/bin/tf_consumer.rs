//! `tf-consumer` — buffers both frames and performs an interpolated,
//! time-travel lookup (blueprint §10.6).
//!
//! ```text
//!   [tf-broadcaster] ──map_odom, odom_base──► [tf-consumer] ──► TF_CONSUMER_REPORT (JSON)
//! ```
//!
//! Every received transform goes into this node's own
//! [`astrs_tf::TransformBuffer`], exactly as a real tf2 listener's would.
//! Once both inputs have closed, [`tf_broadcast::build_report`] looks
//! `map`->`base_link` up at a timestamp exactly between two consecutive
//! dynamic samples and checks the answer against what a linear motion at
//! that exact midpoint *time* must equal.

use std::collections::BTreeSet;
use std::process::ExitCode;

use astrs_node_api::message::Transform;
use astrs_node_api::{Event, Node};
use astrs_tf::TfStamp;
use tf_broadcast::{MAP_ODOM_PORT, ODOM_BASE_PORT, build_report, report_path};

fn main() -> ExitCode {
    match consume() {
        Ok(report) => {
            println!(
                "tf-consumer: {} dynamic samples, interpolation matches: {}",
                report.dynamic_samples, report.interpolation_matches
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("tf-consumer: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Buffers both inputs until they close, then looks up the interpolated
/// transform and writes the report.
fn consume() -> Result<tf_broadcast::ConsumerReport, Box<dyn std::error::Error>> {
    let path = report_path();
    let (node, mut events) = Node::init_from_env()?;
    node.log_info(format!("tf-consumer up; report goes to {}", path.display()));

    let mut static_transform: Option<Transform> = None;
    let mut dynamic_samples: Vec<(TfStamp, Transform)> = Vec::new();
    let mut closed: BTreeSet<String> = BTreeSet::new();

    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, data, .. } if id.as_str() == MAP_ODOM_PORT => {
                static_transform = Some(data.view()?);
            }
            Event::Input { id, meta, data } if id.as_str() == ODOM_BASE_PORT => {
                dynamic_samples.push((TfStamp::from(meta.timestamp), data.view()?));
            }
            // Both producers ride the same node, so both ports close
            // together — tracked per-port regardless, matching every other
            // multi-input sink in this estate.
            Event::InputClosed { id, .. } => {
                closed.insert(id.as_str().to_owned());
                if closed.len() >= 2 {
                    break;
                }
            }
            Event::AllInputsClosed => break,
            Event::Stop(cause) => {
                node.log_info(format!("stopping early: {cause}"));
                break;
            }
            _ => {}
        }
    }

    // The control lane pre-empts queued data (§11.2): drain whatever is
    // still queued before computing the answer, or a sample sent just
    // before the close could go missing from the buffer.
    while let Some(event) = events.try_recv() {
        match event {
            Event::Input { id, data, .. } if id.as_str() == MAP_ODOM_PORT => {
                static_transform = Some(data.view()?);
            }
            Event::Input { id, meta, data } if id.as_str() == ODOM_BASE_PORT => {
                dynamic_samples.push((TfStamp::from(meta.timestamp), data.view()?));
            }
            _ => {}
        }
    }

    let static_transform =
        static_transform.ok_or("tf-consumer: the static map->odom transform was never received")?;
    let report = build_report(&static_transform, &dynamic_samples)?
        .ok_or("tf-consumer: fewer than two dynamic samples arrived; nothing to interpolate")?;

    std::fs::write(&path, report.to_json()?)?;
    node.log_info(format!(
        "{} dynamic samples buffered, interpolation matches: {}",
        report.dynamic_samples, report.interpolation_matches
    ));
    Ok(report)
}
