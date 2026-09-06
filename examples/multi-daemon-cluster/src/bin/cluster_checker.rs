//! `cluster-checker` — the assertion node, back on machine `robot-a`
//! (blueprint §4.2, §6.4, §12).
//!
//! Reads every [`Command`] the planner produced and writes a verdict:
//!
//! * every command arrived, in sequence order, with no gaps;
//! * every command agrees about where the sensor and the planner were;
//! * the three machine names — the sensor's, the planner's, and this node's
//!   own — describe a route that crossed a machine boundary **twice**.
//!
//! ```text
//!   (planner @ robot-b) ──commands──► [checker @ robot-a] ──► $CLUSTER_REPORT (JSON)
//! ```
//!
//! # Why the verdict is a file
//!
//! It is what `tests/conformance/tests/m2_multi_daemon.rs` reads. A test that
//! grepped this node's log lines would be asserting on formatting; a JSON
//! artefact survives being written on one machine and read on another, which
//! is the shape a real fleet check has anyway.
//!
//! # Why a single-host run still exits zero
//!
//! `astrs run` embeds one daemon and never consults `deploy:`, so the same
//! manifest runs on one machine with every placement reported as `unplaced`.
//! That is a legitimate way to run this graph — it is how a reader tries it
//! before setting up a cluster — so the checker reports
//! `crossed_twice: false` and finishes cleanly. Only the *cluster* test
//! demands the crossing, because only it arranged for one.

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use multi_daemon_cluster::{
    COMMANDS_PORT, Command, CrossingReport, labels_of, machine_of, reading_budget, report_path,
};

fn main() -> ExitCode {
    match check() {
        Ok(report) => {
            println!("cluster-checker: {}", report.summary());
            if report.is_clean() {
                ExitCode::SUCCESS
            } else {
                for problem in &report.problems {
                    eprintln!("cluster-checker: {problem}");
                }
                eprintln!(
                    "cluster-checker: {}/{} commands, inputs_closed={}",
                    report.received, report.expected, report.inputs_closed
                );
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("cluster-checker: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Collects every command, then writes the verdict.
fn check() -> Result<CrossingReport, Box<dyn std::error::Error>> {
    let path = report_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let (node, mut events) = Node::init_from_env()?;
    let machine = machine_of(&node);
    let mut report = CrossingReport::new(&machine, labels_of(&node), reading_budget());
    node.log_info(format!(
        "checker up on {machine}; verdict goes to {}",
        path.display()
    ));

    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, data, .. } if id.as_str() == COMMANDS_PORT => {
                report.observe(&Command::from_bytes(&data.to_vec())?);
            }
            Event::InputClosed { .. } | Event::AllInputsClosed => {
                report.inputs_closed = true;
                break;
            }
            // §12: a failing peer reaches this node as an ordinary event, so a
            // verdict written after one says *why* it is short.
            Event::NodeFailed { peer, cause } => {
                report.problems.push(format!("{peer} failed: {cause}"));
            }
            Event::Error(message) => node.log_warn(message),
            Event::Stop(cause) => {
                node.log_info(format!(
                    "stopping after {} commands: {cause}",
                    report.received
                ));
                break;
            }
            _ => {}
        }
    }

    // The control lane pre-empts queued data (§11.2): the notice that ended
    // the loop can have overtaken commands its producer sent earlier, and a
    // checker that left now would accuse the peer route of losing them.
    while let Some(event) = events.try_recv() {
        if let Event::Input { id, data, .. } = event
            && id.as_str() == COMMANDS_PORT
        {
            report.observe(&Command::from_bytes(&data.to_vec())?);
        }
    }

    std::fs::write(&path, report.to_json()?)?;
    node.log_info(format!("verdict: {}", report.summary()));
    Ok(report)
}
