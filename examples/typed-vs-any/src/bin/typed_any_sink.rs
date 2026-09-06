//! `typed-any-sink` — decodes both the typed and the raw port and asserts
//! the two sequences agree (blueprint §9.1, §9.2).
//!
//! ```text
//!   [source] ──typed──► (typed_in) ┐
//!                                  ├──► [sink] ──► ComparisonReport (JSON)
//!            ──raw────► (raw_in)  ┘
//! ```

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use typed_vs_any::{
    ComparisonReport, PortTally, RAW_PORT, TYPED_PORT, TypedValue, raw_value_of, report_path,
};

fn main() -> ExitCode {
    match compare() {
        Ok(report) => {
            println!(
                "typed-any-sink: typed={} raw={} disagreements={}",
                report.typed.values.len(),
                report.raw.values.len(),
                report.disagreements.len()
            );
            if report.is_clean() {
                ExitCode::SUCCESS
            } else {
                eprintln!("typed-any-sink: the two ports did not agree: {report:?}");
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("typed-any-sink: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Reads both inputs to completion and compares what each delivered.
fn compare() -> Result<ComparisonReport, Box<dyn std::error::Error>> {
    let (node, mut events) = Node::init_from_env()?;
    node.log_info("sink up: comparing `typed` against `raw`");

    let mut typed = PortTally::default();
    let mut raw = PortTally::default();

    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, data, .. } if id.as_str() == TYPED_PORT => {
                let value = data.view::<TypedValue>()?.into_inner();
                typed.push(value);
            }
            Event::Input { id, data, .. } if id.as_str() == RAW_PORT => {
                if let Some(value) = raw_value_of(data.bytes()) {
                    raw.push(value);
                } else {
                    node.log_warn(format!(
                        "a raw payload of {} bytes was undecodable",
                        data.len()
                    ));
                }
            }
            Event::InputClosed { id, .. } if id.as_str() == TYPED_PORT => typed.closed = true,
            Event::InputClosed { id, .. } if id.as_str() == RAW_PORT => raw.closed = true,
            Event::AllInputsClosed => break,
            Event::Stop(cause) => {
                node.log_info(format!("stopping early: {cause}"));
                break;
            }
            _ => {}
        }
    }

    let report = ComparisonReport::compare(typed, raw);
    let path = report_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, report.to_json()?)?;
    node.log_info(format!(
        "wrote the comparison to {} ({} disagreements)",
        path.display(),
        report.disagreements.len()
    ));
    Ok(report)
}
