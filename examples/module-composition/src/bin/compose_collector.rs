//! `compose-collector` — verifies the composed pipeline's output (blueprint
//! §9.3).
//!
//! ```text
//!   [compose] ──result──► [collector] ──► CollectorReport (JSON)
//! ```

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use module_composition::{CollectorReport, RESULT_PORT, report_path, tick_budget, value_of};

fn main() -> ExitCode {
    match collect() {
        Ok(report) => {
            let budget = tick_budget();
            println!(
                "compose-collector: {}/{budget} values, {} mismatches",
                report.values.len(),
                report.mismatches.len()
            );
            if report.is_clean(budget) {
                ExitCode::SUCCESS
            } else {
                eprintln!("compose-collector: not clean: {report:?}");
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("compose-collector: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Reads `result` until the budget is met or the port closes, verifying
/// every value against `composed_value` as it arrives.
fn collect() -> Result<CollectorReport, Box<dyn std::error::Error>> {
    let budget = tick_budget();
    let (node, mut events) = Node::init_from_env()?;
    node.log_info(format!("collector up: expecting {budget} composed values"));

    let mut report = CollectorReport::default();
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, data, .. } if id.as_str() == RESULT_PORT => {
                match value_of(data.bytes()) {
                    Some(value) => report.push(value),
                    None => node.log_warn(format!("undecodable payload ({} bytes)", data.len())),
                }
                if report.values.len() as u64 >= budget {
                    break;
                }
            }
            Event::InputClosed { id, .. } if id.as_str() == RESULT_PORT => {
                node.log_info("compose is done; finishing");
                break;
            }
            Event::Stop(cause) => {
                node.log_info(format!(
                    "stopping after {} values: {cause}",
                    report.values.len()
                ));
                break;
            }
            _ => {}
        }
    }

    let path = report_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, report.to_json()?)?;
    node.log_info(format!(
        "wrote {} values to {}",
        report.values.len(),
        path.display()
    ));
    Ok(report)
}
