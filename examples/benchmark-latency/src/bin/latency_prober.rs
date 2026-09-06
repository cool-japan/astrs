//! `latency-prober` — the timing half of `benchmark-latency` (blueprint
//! §20.4).
//!
//! Holds exactly one round trip in flight: sends `ping`, waits for the
//! matching `pong`, times the gap with its own [`Instant`] clock, and only
//! then sends the next `ping`. See the crate docs for why this closed-loop
//! discipline — and never comparing an `Instant` across processes — is the
//! whole point.
//!
//! ```text
//!   [prober] ──ping──► [reflector]
//!      ▲                    │
//!      └───────pong─────────┘
//! ```

use std::process::ExitCode;
use std::time::Instant;

use astrs_node_api::{Event, Node, RawOutput};
use benchmark_latency::{
    LatencyReport, PONG_PORT, report_path, sample_budget, seq_of, seq_payload,
};

fn main() -> ExitCode {
    match probe() {
        Ok(report) => {
            println!(
                "latency-prober: {} samples, p50={} us p90={} us p99={} us max={} us ({} mismatched)",
                report.samples,
                report.p50_us,
                report.p90_us,
                report.p99_us,
                report.max_us,
                report.mismatched
            );
            if report.sanity_bounds_ok() {
                ExitCode::SUCCESS
            } else {
                eprintln!("latency-prober: the report failed its own sanity bounds: {report:?}");
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("latency-prober: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the closed-loop round trips to completion and writes the report.
fn probe() -> Result<LatencyReport, Box<dyn std::error::Error>> {
    let budget = sample_budget();
    let (mut node, mut events) = Node::init_from_env()?;
    let mut ping = node.raw_output("ping")?;
    node.log_info(format!("prober up: {budget} closed-loop round trips"));

    let mut latencies_us: Vec<u64> = Vec::with_capacity(budget as usize);
    let mut mismatched = 0_u64;
    let mut next_seq = 0_u64;

    // The first ping has no triggering event to wait for — it starts the
    // closed loop rather than reacting to one.
    let mut started = send_ping(&mut ping, &node, next_seq)?;
    next_seq += 1;

    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, data, .. } if id.as_str() == PONG_PORT => {
                let elapsed = started.elapsed();
                let expected = next_seq - 1;
                if seq_of(data.bytes()) == Some(expected) {
                    let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
                    latencies_us.push(micros);
                } else {
                    mismatched += 1;
                }
                if latencies_us.len() as u64 + mismatched >= budget {
                    break;
                }
                started = send_ping(&mut ping, &node, next_seq)?;
                next_seq += 1;
            }
            Event::Stop(cause) => {
                node.log_info(format!(
                    "stopping after {} of {budget} round trips: {cause}",
                    latencies_us.len()
                ));
                break;
            }
            _ => {}
        }
    }

    ping.close()?;
    let report = LatencyReport::from_samples_us(&latencies_us, mismatched);
    let path = report_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, report.to_json()?)?;
    node.log_info(format!(
        "wrote {} samples to {}",
        report.samples,
        path.display()
    ));
    Ok(report)
}

/// Sends one `ping` carrying `seq`, returning the instant it was sent.
fn send_ping(
    ping: &mut RawOutput,
    node: &Node,
    seq: u64,
) -> Result<Instant, Box<dyn std::error::Error>> {
    let metadata = node.metadata();
    // `Instant::now()` is read *before* the send: the round trip starts the
    // moment this process hands the payload off, not after it returns.
    let started = Instant::now();
    ping.send_bytes(seq_payload(seq), metadata)?;
    Ok(started)
}
