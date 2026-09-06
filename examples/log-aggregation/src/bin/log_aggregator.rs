//! `log-aggregator` — collects every marker-tagged record off the
//! unfiltered `astrs/logs` virtual source and tallies it per node and level
//! (blueprint §8.4, §13).
//!
//! ```text
//!   [sensor-a] ──┐
//!                ├──► astrs/logs ──► [aggregator] ──► LogTally (JSON)
//!   [sensor-b] ──┘
//! ```
//!
//! Every payload on `astrs/logs` is a JSON [`astrs_log::LogRecord`] — the
//! daemon's own records and every node's captured stdout/stderr included,
//! not only this example's own marker lines (§8.4). [`LogTally::accept`] is
//! what tells a marker apart from the rest; this node's job is only to
//! decode every payload and hand it over.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use astrs_log::LogRecord;
use astrs_node_api::{Event, Node};
use log_aggregation::{DEFAULT_RECORDS_PER_WORKER, LogTally, WORKER_IDS, report_path};

/// How long one receive waits before the loop checks its deadline.
const RECV_SLICE: Duration = Duration::from_millis(200);
/// How long the aggregator waits for every expected marker before giving up
/// and reporting whatever it has — generous relative to the workers' own
/// tick cadences (30-45 ms for six records apiece), the same "loose bound"
/// stance `benchmark-latency` documents for its own sanity ceiling.
const DEADLINE: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    match aggregate() {
        Ok(tally) => {
            println!(
                "log-aggregator: {} marker records tallied ({} decode problems)",
                tally.total_matched,
                tally.decode_problems.len()
            );
            if tally.is_complete(WORKER_IDS, DEFAULT_RECORDS_PER_WORKER) {
                ExitCode::SUCCESS
            } else {
                eprintln!(
                    "log-aggregator: incomplete — missing {:?}",
                    tally.missing(WORKER_IDS, DEFAULT_RECORDS_PER_WORKER)
                );
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("log-aggregator: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Collects markers until every worker's budget is met or the deadline
/// passes, then writes the tally.
fn aggregate() -> Result<LogTally, Box<dyn std::error::Error>> {
    let expected_total = WORKER_IDS.len() as u64 * DEFAULT_RECORDS_PER_WORKER;
    let (node, mut events) = Node::init_from_env()?;
    node.log_info(format!(
        "aggregator up: expecting {expected_total} marker records from {WORKER_IDS:?}"
    ));

    let mut tally = LogTally::new();
    let deadline = Instant::now() + DEADLINE;
    while tally.total_matched < expected_total {
        match events.recv_timeout(RECV_SLICE)? {
            Some(Event::Input { data, .. }) => {
                match serde_json::from_slice::<LogRecord>(data.bytes()) {
                    Ok(record) => {
                        let _matched = tally.accept(
                            record.node.as_deref(),
                            record.level.as_str(),
                            &record.message,
                        );
                    }
                    Err(error) => tally.decode_problem(error.to_string()),
                }
            }
            Some(Event::Stop(_)) => break,
            Some(_) => {}
            None if events.session_ended() => break,
            None => {
                if Instant::now() >= deadline {
                    break;
                }
            }
        }
        if Instant::now() >= deadline {
            break;
        }
    }

    let path = report_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, tally.to_json()?)?;
    node.log_info(format!(
        "wrote {} marker records ({} problems) to {}",
        tally.total_matched,
        tally.decode_problems.len(),
        path.display()
    ));
    Ok(tally)
}
