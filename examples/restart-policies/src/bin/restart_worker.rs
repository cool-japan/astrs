//! `restart-worker` — a node that fails a fixed number of times, then works
//! (blueprint §12).
//!
//! Every incarnation does the same three things:
//!
//! 1. appends one [`Incarnation`] line to `$RESTART_LOG`, before anything
//!    else, so a start is recorded even if this process is about to die;
//! 2. decides whether *it* is one of the failures, from
//!    [`Node::restart_count`] and `$RESTART_FAILURES`;
//! 3. either exits with [`FAILURE_EXIT_CODE`], or runs for a few ticks, writes
//!    `$RESTART_SUMMARY` and finishes cleanly.
//!
//! ```text
//!   start 1  restart_count=0  fail  ─┐
//!   start 2  restart_count=1  fail  ─┼─ backoff: restart_delay × 2ⁿ (§12)
//!   start 3  restart_count=2  work  ─┘  → summary written, run finishes
//! ```
//!
//! The same binary drives both of this example's manifests: with
//! `RESTART_FAILURES` larger than the budget, no incarnation ever succeeds and
//! the supervisor gives up after exactly `1 + max_restarts` starts.
//!
//! # Why the decision is `restart_count` and not a counter of its own
//!
//! A node that counted its own restarts would be counting a file. The daemon
//! tells each incarnation how many times it has been restarted (§9.1's
//! introspection surface), and using *that* is what makes this example a test
//! of the supervisor rather than of the example.

use std::io::Write as _;
use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use restart_policies::{
    FAILURE_EXIT_CODE, Incarnation, RestartSummary, TICK_PORT, failure_budget, log_path,
    summary_path,
};

/// How many ticks a surviving incarnation processes before finishing.
const TICKS: u64 = 3;

fn main() -> ExitCode {
    match work() {
        Ok(Some(summary)) => {
            println!(
                "restart-worker: survived after {} restart(s), generation {}",
                summary.restart_count, summary.generation
            );
            ExitCode::SUCCESS
        }
        // A deliberate failure: this incarnation was told to be one of the
        // ones that dies, and says so on the way out.
        Ok(None) => ExitCode::from(FAILURE_EXIT_CODE),
        Err(error) => {
            eprintln!("restart-worker: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Records this incarnation, then either fails on purpose or runs to
/// completion.
fn work() -> Result<Option<RestartSummary>, Box<dyn std::error::Error>> {
    let (node, mut events) = Node::init_from_env()?;
    let restart_count = node.restart_count();
    let will_fail = restart_count < failure_budget();

    // Written first, and flushed: this line is the only durable evidence that
    // this incarnation existed at all.
    append(&Incarnation {
        restart_count,
        generation: node.generation(),
        will_fail,
    })?;

    if will_fail {
        node.log_warn(format!(
            "incarnation {restart_count} failing on purpose with exit code {FAILURE_EXIT_CODE}"
        ));
        // `eprintln!` rather than a log record: the daemon captures a failing
        // node's stderr, and this must be visible even if the session ends
        // before the log record is written.
        eprintln!("restart-worker: incarnation {restart_count} failing on purpose");
        return Ok(None);
    }

    node.log_info(format!(
        "incarnation {restart_count} is the one that works (generation {})",
        node.generation()
    ));

    let mut ticks = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, .. } if id.as_str() == TICK_PORT => {
                ticks += 1;
                if ticks >= TICKS {
                    break;
                }
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {ticks} ticks: {cause}"));
                break;
            }
            _ => {}
        }
    }

    let summary = RestartSummary {
        restart_count,
        generation: node.generation(),
        ticks,
    };
    let path = summary_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, summary.to_json()?)?;
    node.log_info(format!("summary written to {}", path.display()));
    Ok(Some(summary))
}

/// Appends one incarnation line to the log, creating it on the first start.
fn append(incarnation: &Incarnation) -> Result<(), Box<dyn std::error::Error>> {
    let path = log_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(incarnation.to_line()?.as_bytes())?;
    // Flushed explicitly: this process may be about to exit non-zero, and a
    // buffered line would be a start the log never recorded.
    file.flush()?;
    Ok(())
}
