//! `compose-host` — hosts `ScaleOperator` and `OffsetOperator` composed in
//! one runtime process (blueprint §9.3).
//!
//! ```text
//!   [tick input] ──► ScaleOperator ──out──► OffsetOperator ──► [result output]
//! ```
//!
//! An ordinary `Node::init_from_env` connection handed straight to
//! [`RuntimeHost`] — see `module_composition`'s crate docs for why this
//! binary builds its own [`astrs_runtime::RuntimeConfig`] in Rust rather
//! than reading a manifest `operators:` list, and why that is still a
//! faithful proof of blueprint §9.3's claim.

use std::process::ExitCode;

use astrs_node_api::Node;
use astrs_runtime::RuntimeHost;
use module_composition::runtime_config;

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("compose-host: not every operator finished healthily");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("compose-host: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Connects, hosts both operators until the session ends, and reports
/// whether every one of them finished healthily.
fn run() -> Result<bool, Box<dyn std::error::Error>> {
    let (node, events) = Node::init_from_env()?;
    let config = runtime_config()?;
    let host = RuntimeHost::new(node, events, config)?;
    let report = host.run()?;
    for operator in &report.operators {
        println!("compose-host: {}: {:?}", operator.id, operator.outcome);
    }
    Ok(report.all_healthy())
}
