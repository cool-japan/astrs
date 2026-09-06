//! `fault-watcher` — the peer that is told (blueprint §12, §8.4).
//!
//! Consumes `readings` from the source and `astrs/status`, the reserved
//! lifecycle stream, and writes down every [`Event::NodeFailed`] and
//! [`Event::InputClosed`] it received before finishing.
//!
//! ```text
//!   [source] ──readings──► [watcher] ──► $FAULT_REPORT (JSON)
//!   astrs/status ────────►
//! ```
//!
//! # `astrs/status` is declared, and the delivery does not depend on it
//!
//! §8.4 makes `astrs/status` an input like any other, and declaring it is how
//! a supervisor-shaped node says *I react to my peers* — `astrs validate`
//! sees it, `astrs graph` draws it, and a reader of the manifest knows why
//! this node has a `NodeFailed` arm. The daemon serves it from the supervisor
//! directly (its route table already knows this node is downstream of the one
//! that failed), so the arm below would fire either way. Both facts are worth
//! knowing, and the manifest states the intent rather than relying on the
//! coincidence.
//!
//! # Why `astrs/status` changes how this node ends
//!
//! `astrs/status` is a virtual source the daemon serves for the life of the
//! dataflow: it never closes. A node that declares it and waits for
//! [`Event::AllInputsClosed`] therefore waits for ever — the graph is over,
//! its producer is gone, and one input is still nominally live. A
//! supervisor-shaped node ends on the closure of the input it actually
//! processes, which is what the loop below does.
//!
//! # Why it finishes rather than failing
//!
//! Its producer died; its input closed. That is a *reported* condition, not
//! this node's error — §12's whole point is that a peer's failure reaches the
//! application as an ordinary event it decides what to do about. So the
//! watcher writes its report and exits zero, and the dataflow's own result
//! carries the failure of the node that actually failed.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use astrs_node_api::{Event, Node};
use error_propagation::{FaultReport, PeerFailure, READINGS_PORT, report_path};

/// How long to wait, after `readings` closes, for the daemon's verdict on the
/// producer that stopped feeding it.
///
/// Generous: the reap normally lands within milliseconds of the closure, so
/// this only ever elapses when the verdict is genuinely missing — which is a
/// failure worth reporting, not worth hanging on.
const VERDICT_WAIT: Duration = Duration::from_secs(10);

fn main() -> ExitCode {
    match watch() {
        Ok(report) => {
            println!(
                "fault-watcher: {} reading(s), {} peer failure(s)",
                report.readings,
                report.failures.len()
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("fault-watcher: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Records every reading and every lifecycle event, then writes the report.
fn watch() -> Result<FaultReport, Box<dyn std::error::Error>> {
    let path = report_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let (node, mut events) = Node::init_from_env()?;
    let mut report = FaultReport::new();
    node.log_info(format!("watcher up; report goes to {}", path.display()));

    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, .. } if id.as_str() == READINGS_PORT => {
                report.readings += 1;
            }
            // §12's error propagation, arriving as an ordinary event with a
            // **typed** cause rather than a string.
            Event::NodeFailed { peer, cause } => {
                node.log_warn(format!("peer {peer} failed: {cause}"));
                report.failures.push(PeerFailure::new(&peer, &cause));
            }
            // The readings input closing is this node's cue to finish, and
            // `AllInputsClosed` is *not*: `astrs/status` is a virtual source
            // the daemon serves for the life of the dataflow and never closes,
            // so a node that declares it and waits to be told every input is
            // done waits for ever. A supervisor-shaped node ends on the
            // closure of the input it actually processes.
            Event::InputClosed { id, reason, .. } => {
                report.closed_inputs.push(format!("{id}: {reason}"));
                if id.as_str() == READINGS_PORT {
                    report.finished_cleanly = true;
                    break;
                }
            }
            Event::AllInputsClosed => {
                report.finished_cleanly = true;
                break;
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping: {cause}"));
                report.finished_cleanly = true;
                break;
            }
            _ => {}
        }
    }

    // Wait, bounded, for the daemon's verdict on the producer.
    //
    // The closure and the failure are two *different* facts arriving at two
    // different times. `readings` closed because the producer's output handle
    // was dropped on the way out of its process; its exit **status** is
    // something the daemon only learns once it has reaped that process, a
    // moment later. Draining whatever happened to be queued therefore raced
    // the reaper: on a fast machine this node wrote its report, said "0 peer
    // failure(s)" and exited before the `NodeFailed` was ever sent — an
    // intermittent failure of the very promise the example demonstrates.
    //
    // Waiting is not a workaround for a missing event; it is what a
    // supervisor-shaped node has to do to observe a fact that is still in
    // flight. The deadline exists so a genuinely absent verdict fails the
    // example loudly instead of hanging it.
    let deadline = Instant::now() + VERDICT_WAIT;
    while report.failures.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            node.log_warn(format!(
                "no peer verdict within {VERDICT_WAIT:?}; reporting what was seen"
            ));
            break;
        }
        // `Ok(None)` is the deadline, a fused stream or an ended session —
        // in every one of them there is nothing further to wait for.
        let Some(event) = events.recv_timeout(remaining)? else {
            break;
        };
        match event {
            Event::Input { id, .. } if id.as_str() == READINGS_PORT => report.readings += 1,
            Event::NodeFailed { peer, cause } => {
                node.log_warn(format!("peer {peer} failed: {cause}"));
                report.failures.push(PeerFailure::new(&peer, &cause));
            }
            Event::InputClosed { id, reason, .. } => {
                report.closed_inputs.push(format!("{id}: {reason}"));
            }
            _ => {}
        }
    }

    // The control lane pre-empts queued data (§11.2), and a `NodeFailed` is a
    // control-lane event: leaving on the first one would abandon readings its
    // producer sent *before* it died, and this report is about both.
    while let Some(event) = events.try_recv() {
        match event {
            Event::Input { id, .. } if id.as_str() == READINGS_PORT => report.readings += 1,
            Event::NodeFailed { peer, cause } => {
                report.failures.push(PeerFailure::new(&peer, &cause));
            }
            Event::InputClosed { id, reason, .. } => {
                report.closed_inputs.push(format!("{id}: {reason}"));
            }
            _ => {}
        }
    }

    std::fs::write(&path, report.to_json()?)?;
    node.log_info(format!(
        "report written: {} reading(s), {} failure(s)",
        report.readings,
        report.failures.len()
    ));
    Ok(report)
}
