//! `hello-timer` — the smallest AstRS dataflow that does something
//! (blueprint §5.4, §8.4, §9.1).
//!
//! One node, one input, no producers: the input is a **virtual source**, the
//! `astrs/timer/millis/N` clock the daemon drives (§8.4), and every tick is an
//! ordinary [`Event::Input`] — there is no timer API to learn, because a timer
//! is just an edge in the graph.
//!
//! ```text
//!   astrs/timer/millis/200 ──► tick ──► [hello-timer] ──► log records ──► `astrs run`
//! ```
//!
//! Two things here are worth copying into a real node:
//!
//! 1. **The loop is the whole program.** `while let Some(event) = events.recv()`
//!    with a `match` is §9.1's canonical shape; a node never sleeps, polls or
//!    spawns a scheduler of its own.
//! 2. **Logging goes through the node, not `println!`.** [`Node::log_info`]
//!    stamps the record with the node's own hybrid logical clock and hands it
//!    to the daemon, which fans it out to `astrs logs`, to any in-graph
//!    `astrs/logs/*` subscriber (§8.4) and to the terminal `astrs run` is
//!    streaming. A `println!` only reaches the last of those, and without the
//!    causal timestamp.
//!
//! Run it:
//!
//! ```text
//! cargo build -p hello-timer
//! astrs run examples/hello-timer/dataflow.yml
//! ```

use std::process::ExitCode;

use astrs_node_api::{Event, Node};

/// The input the timer drives, as named in `dataflow.yml`.
const TICK_PORT: &str = "tick";

/// How many ticks to greet before finishing.
///
/// A bounded example: `exit_when_nodes_finish: true` ends the dataflow when
/// every node has finished, so a node that never finished would leave
/// `astrs run` waiting for a graph that is done.
const DEFAULT_TICKS: u64 = 10;

/// Overrides [`DEFAULT_TICKS`] from the manifest's `env:` block.
const ENV_TICKS: &str = "HELLO_TIMER_TICKS";

/// Names a file the node writes its tick count to, if the manifest asks for
/// one.
///
/// Nothing in the dataflow needs it — the log lines are the output. It exists
/// because a *test* needs a durable fact rather than a scrape of streamed
/// terminal text, and showing how a node hands one over is worth the four
/// lines it costs.
const ENV_RESULT: &str = "HELLO_TIMER_RESULT";

fn main() -> ExitCode {
    match greet() {
        Ok(ticks) => {
            println!("hello-timer: greeted {ticks} ticks");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("hello-timer: {error}");
            ExitCode::FAILURE
        }
    }
}

/// How many ticks this run should greet.
///
/// A value the manifest cannot express — a negative number, a word — falls
/// back to the default rather than failing the node. A node that refused to
/// start because someone typed `HELLO_TIMER_TICKS=lots` would be a worse
/// failure than one that greets ten times and says so.
fn tick_budget(raw: Option<&str>) -> u64 {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|ticks| *ticks > 0)
        .unwrap_or(DEFAULT_TICKS)
}

/// Greets every tick until the budget runs out, then finishes.
fn greet() -> Result<u64, Box<dyn std::error::Error>> {
    let wanted = tick_budget(std::env::var(ENV_TICKS).ok().as_deref());

    // `init_from_env` reads the handshake blob the daemon put in this
    // process's environment, dials the daemon, registers, and subscribes to
    // every input the manifest declared. Nothing else is needed to join a
    // graph.
    let (node, mut events) = Node::init_from_env()?;
    node.log_info(format!("hello from {}, {wanted} ticks to go", node.id()));

    let mut ticks = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, .. } if id.as_str() == TICK_PORT => {
                ticks += 1;
                // The metadata carries the daemon's HLC reading for this tick
                // (§4.3), which is what makes a log line orderable against
                // every other event in the cluster.
                node.log_info(format!("tick {ticks} at hlc {}", meta.timestamp));
                if ticks >= wanted {
                    break;
                }
            }
            // A stop can arrive at any time — Ctrl-C, `astrs stop`, a peer
            // failing. Breaking here is what makes the exit clean.
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {ticks} ticks: {cause}"));
                break;
            }
            _ => {}
        }
    }

    node.log_info(format!("done after {ticks} ticks"));
    if let Ok(path) = std::env::var(ENV_RESULT)
        && !path.is_empty()
    {
        std::fs::write(&path, ticks.to_string())?;
    }
    Ok(ticks)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// The manifest's `env:` block is the knob; the default is what a reader
    /// gets with no knob at all.
    #[test]
    fn the_tick_budget_comes_from_the_environment() {
        assert_eq!(tick_budget(Some("3")), 3);
        assert_eq!(tick_budget(Some("  7  ")), 7);
        assert_eq!(tick_budget(None), DEFAULT_TICKS);
    }

    /// A value the manifest cannot express falls back rather than failing the
    /// node — including zero, which would make `exit_when_nodes_finish` fire
    /// before the graph had done anything.
    #[test]
    fn an_unusable_budget_falls_back_to_the_default() {
        for raw in ["", "lots", "-1", "0", "1.5"] {
            assert_eq!(tick_budget(Some(raw)), DEFAULT_TICKS, "{raw:?}");
        }
    }

    /// The port and variable names the manifest uses are the ones this file
    /// reads. Trivial, and exactly the pair that drifts when a manifest is
    /// edited without its node.
    #[test]
    fn the_names_match_the_manifest() {
        assert_eq!(TICK_PORT, "tick");
        assert_eq!(ENV_TICKS, "HELLO_TIMER_TICKS");
        assert_eq!(ENV_RESULT, "HELLO_TIMER_RESULT");
        assert_eq!(DEFAULT_TICKS, 10);
    }
}
