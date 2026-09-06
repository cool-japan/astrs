//! Shared types for `multi-daemon-cluster` — one dataflow, two machines, and
//! the crossing asserted from *inside* the graph (blueprint §4.2, §6.4).
//!
//! ```text
//!   machine robot-a                        machine robot-b
//!   ┌────────────────────┐                 ┌────────────────────┐
//!   │ [sensor] ─readings─┼════ peer TCP ══►│ [planner]          │
//!   │ [checker] ◄────────┼◄═══ peer TCP ═══┼─ commands          │
//!   └────────────────────┘                 └────────────────────┘
//!            daemon A ──────► coordinator ◄────── daemon B
//! ```
//!
//! # How a node can prove it crossed a machine boundary
//!
//! It cannot, on its own. A consumer sees an ordinary [`astrs_node_api::Event::Input`]
//! whichever plane carried it; that is the point of the design, and
//! `Node::input_plane` only distinguishes *this host's* daemon path from its
//! shared-memory ring.
//!
//! So each node stamps **its own placement** into every message it publishes.
//! A node knows where it is running — the daemon that spawned it put the
//! answer in its spawn spec, and [`machine_of`] reads it back — so a
//! [`Command`] carries both the planner's machine and the machine the reading
//! it answers came from. The checker then compares three names it did not
//! choose: the sensor's, the planner's, and its own. Two adjacent names that
//! differ *are* a machine crossing, witnessed by the application rather than
//! inferred from a log line.
//!
//! Under `astrs run` there is one embedded daemon and no placement at all, so
//! every name is [`UNPLACED`] and [`CrossingReport::crossed_twice`] is false.
//! That is not a failure: the same manifest is meant to run both ways, and the
//! report says which way it ran.
//!
//! # Payloads are JSON on an untyped edge
//!
//! What travels here is a *placement claim*, not sensor data, and the point of
//! the example is where the bytes went rather than what they meant. JSON keeps
//! the artefact legible in `astrs topic echo` and in a failure message; the
//! typed-URN story is `rust-pipeline`'s.

use std::collections::BTreeSet;

use astrs_node_api::Node;
use serde::{Deserialize, Serialize};

/// The sensor's output port, and the planner's input.
pub const READINGS_PORT: &str = "readings";

/// The planner's output port, and the checker's input.
pub const COMMANDS_PORT: &str = "commands";

/// The sensor's tick input.
pub const TICK_PORT: &str = "tick";

/// Environment variable naming the JSON file the checker writes its verdict to.
pub const ENV_REPORT_PATH: &str = "CLUSTER_REPORT";

/// Environment variable overriding how many readings the sensor publishes.
pub const ENV_READINGS: &str = "CLUSTER_READINGS";

/// How many readings the sensor publishes by default.
pub const DEFAULT_READINGS: u64 = 12;

/// The machine name reported by a node the placement planner never placed.
///
/// `astrs run` embeds a single daemon and never consults `deploy:`, so every
/// node in a single-process run reports this.
pub const UNPLACED: &str = "unplaced";

/// Where the checker writes its verdict when the manifest names no path.
#[must_use]
pub fn default_report_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-multi-daemon-cluster-report.json")
}

/// The JSON file this run's checker writes.
#[must_use]
pub fn report_path() -> std::path::PathBuf {
    std::env::var(ENV_REPORT_PATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default_report_path, std::path::PathBuf::from)
}

/// How many readings this run should publish.
#[must_use]
pub fn reading_budget() -> u64 {
    std::env::var(ENV_READINGS)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_READINGS)
}

/// The machine a node is running on, as the daemon that spawned it recorded.
///
/// Reads `deploy.machine` out of this node's own spawn spec (§8.3), which the
/// coordinator resolved from the manifest before dispatching the spawn.
/// [`UNPLACED`] when there is no placement — the single-process `astrs run`
/// case.
#[must_use]
pub fn machine_of(node: &Node) -> String {
    node.descriptor()
        .deploy
        .machine
        .as_ref()
        .map_or_else(|| UNPLACED.to_owned(), |machine| machine.to_string())
}

/// The placement labels a node was deployed with (§8.3 `deploy.labels`).
///
/// Carried through the spawn spec beside the machine name. Placement itself
/// resolves on `machine:` in this build — labels are declared, advertised by
/// each daemon at registration, and delivered to the node, which is what lets
/// an example assert they survived the trip.
#[must_use]
pub fn labels_of(node: &Node) -> std::collections::BTreeMap<String, String> {
    node.descriptor().deploy.labels.clone()
}

/// One sensor reading, stamped with where it was produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reading {
    /// Its position in the stream, from zero.
    pub seq: u64,
    /// The machine the sensor was placed on.
    pub origin: String,
}

impl Reading {
    /// Encodes the reading as the bytes that go on the wire.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if it cannot be serialised, which its field types
    /// make impossible in practice.
    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Decodes a reading from the wire.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if the payload is not a reading.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

/// One planner command, stamped with where it was produced *and* where the
/// reading behind it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    /// The sequence number of the reading this answers.
    pub seq: u64,
    /// The machine the planner was placed on.
    pub planner: String,
    /// The machine the reading behind it was produced on.
    pub upstream: String,
}

impl Command {
    /// Encodes the command as the bytes that go on the wire.
    ///
    /// # Errors
    ///
    /// As [`Reading::to_bytes`].
    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Decodes a command from the wire.
    ///
    /// # Errors
    ///
    /// As [`Reading::from_bytes`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

/// What the checker saw, written as JSON when its input closes.
///
/// A file rather than a log line because it is what the conformance suite
/// reads: a test that grepped the terminal would be asserting on formatting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossingReport {
    /// The machine the sensor claimed, unanimously.
    pub sensor_machine: String,
    /// The machine the planner claimed, unanimously.
    pub planner_machine: String,
    /// The machine this checker is running on.
    pub checker_machine: String,
    /// The placement labels this checker was deployed with.
    pub checker_labels: std::collections::BTreeMap<String, String>,
    /// How many commands were expected.
    pub expected: u64,
    /// How many arrived.
    pub received: u64,
    /// Whether they arrived in sequence order with no gaps.
    pub in_order: bool,
    /// Whether the checker finished because its input closed, rather than
    /// because it was stopped mid-stream.
    pub inputs_closed: bool,
    /// Everything that did not add up, each named.
    pub problems: Vec<String>,
}

impl CrossingReport {
    /// A report for a checker on `machine` expecting `expected` commands.
    #[must_use]
    pub fn new(
        machine: impl Into<String>,
        labels: std::collections::BTreeMap<String, String>,
        expected: u64,
    ) -> Self {
        Self {
            sensor_machine: String::new(),
            planner_machine: String::new(),
            checker_machine: machine.into(),
            checker_labels: labels,
            expected,
            received: 0,
            in_order: true,
            inputs_closed: false,
            problems: Vec::new(),
        }
    }

    /// Records one arriving command, checking its order and its placement
    /// claims against everything seen so far.
    pub fn observe(&mut self, command: &Command) {
        if command.seq != self.received {
            self.in_order = false;
            self.problems.push(format!(
                "command {} arrived in position {}",
                command.seq, self.received
            ));
        }
        Self::note(
            &mut self.planner_machine,
            &mut self.problems,
            "planner",
            &command.planner,
        );
        Self::note(
            &mut self.sensor_machine,
            &mut self.problems,
            "sensor",
            &command.upstream,
        );
        self.received += 1;
    }

    /// Records a placement claim, or reports it if it contradicts an earlier
    /// one — a graph whose planner answered from two different machines is a
    /// placement bug, not a routing detail.
    fn note(seen: &mut String, problems: &mut Vec<String>, role: &str, claimed: &str) {
        if seen.is_empty() {
            seen.push_str(claimed);
        } else if seen != claimed {
            problems.push(format!("{role} claimed {seen} and then {claimed}"));
        }
    }

    /// Whether both hops crossed a machine boundary: sensor → planner, and
    /// planner → checker.
    ///
    /// False under `astrs run`, where every name is [`UNPLACED`].
    #[must_use]
    pub fn crossed_twice(&self) -> bool {
        self.sensor_machine != self.planner_machine
            && self.planner_machine != self.checker_machine
            && !self.sensor_machine.is_empty()
            && !self.planner_machine.is_empty()
    }

    /// Every distinct machine name this run touched.
    #[must_use]
    pub fn machines(&self) -> BTreeSet<String> {
        [
            self.sensor_machine.clone(),
            self.planner_machine.clone(),
            self.checker_machine.clone(),
        ]
        .into_iter()
        .filter(|name| !name.is_empty())
        .collect()
    }

    /// Whether every command arrived, in order, with a clean finish.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
            && self.in_order
            && self.inputs_closed
            && self.received == self.expected
    }

    /// Renders the report as pretty JSON.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if it cannot be serialised, which its field types
    /// make impossible in practice.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// The one-line summary the checker logs and prints.
    #[must_use]
    pub fn summary(&self) -> String {
        let route = format!(
            "{} -> {} -> {}",
            self.sensor_machine, self.planner_machine, self.checker_machine
        );
        let crossing = if self.crossed_twice() {
            "crossed two machine boundaries"
        } else {
            "stayed on one machine"
        };
        format!(
            "{}/{} commands, {route}, {crossing}",
            self.received, self.expected
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::collections::BTreeMap;

    fn command(seq: u64, planner: &str, upstream: &str) -> Command {
        Command {
            seq,
            planner: planner.to_owned(),
            upstream: upstream.to_owned(),
        }
    }

    /// Both message types survive the wire encoding they use.
    #[test]
    fn messages_round_trip_as_json_bytes() {
        let reading = Reading {
            seq: 3,
            origin: "robot-a".to_owned(),
        };
        assert_eq!(
            Reading::from_bytes(&reading.to_bytes().unwrap()).unwrap(),
            reading
        );

        let command = command(3, "robot-b", "robot-a");
        assert_eq!(
            Command::from_bytes(&command.to_bytes().unwrap()).unwrap(),
            command
        );
    }

    /// A distributed run reports two crossings; a single-host run reports
    /// none, and neither is an error.
    #[test]
    fn a_two_machine_route_is_recognised_and_a_one_machine_route_is_not() {
        let mut distributed = CrossingReport::new("robot-a", BTreeMap::new(), 2);
        distributed.observe(&command(0, "robot-b", "robot-a"));
        distributed.observe(&command(1, "robot-b", "robot-a"));
        distributed.inputs_closed = true;
        assert!(distributed.crossed_twice(), "{distributed:?}");
        assert!(distributed.is_clean());
        assert_eq!(
            distributed.machines(),
            BTreeSet::from(["robot-a".to_owned(), "robot-b".to_owned()])
        );
        assert!(
            distributed
                .summary()
                .contains("robot-a -> robot-b -> robot-a")
        );

        let mut local = CrossingReport::new(UNPLACED, BTreeMap::new(), 1);
        local.observe(&command(0, UNPLACED, UNPLACED));
        local.inputs_closed = true;
        assert!(!local.crossed_twice());
        assert!(local.is_clean(), "a single-host run is still a clean run");
        assert!(local.summary().contains("stayed on one machine"));
    }

    /// A gap in the sequence is reported rather than counted over.
    #[test]
    fn a_missing_command_is_named() {
        let mut report = CrossingReport::new("robot-a", BTreeMap::new(), 3);
        report.observe(&command(0, "robot-b", "robot-a"));
        report.observe(&command(2, "robot-b", "robot-a"));
        assert!(!report.in_order);
        assert_eq!(report.problems.len(), 1, "{report:?}");
        assert!(!report.is_clean());
    }

    /// A planner that answered from two machines contradicts itself, and the
    /// report says so instead of keeping the last claim.
    #[test]
    fn a_contradictory_placement_claim_is_reported() {
        let mut report = CrossingReport::new("robot-a", BTreeMap::new(), 2);
        report.observe(&command(0, "robot-b", "robot-a"));
        report.observe(&command(1, "robot-c", "robot-a"));
        assert!(
            report
                .problems
                .iter()
                .any(|problem| problem.contains("robot-b") && problem.contains("robot-c")),
            "{report:?}"
        );
        assert!(!report.is_clean());
    }

    /// A run cut short is not clean, even with every command accounted for.
    #[test]
    fn an_unfinished_run_is_not_clean() {
        let mut report = CrossingReport::new("robot-a", BTreeMap::new(), 1);
        report.observe(&command(0, "robot-b", "robot-a"));
        assert!(!report.is_clean(), "inputs never closed");
        report.inputs_closed = true;
        assert!(report.is_clean());
    }

    /// The report round-trips as JSON, which is how the suite reads it.
    #[test]
    fn the_report_round_trips_as_json() {
        let mut report = CrossingReport::new(
            "robot-a",
            BTreeMap::from([("role".to_owned(), "sensing".to_owned())]),
            1,
        );
        report.observe(&command(0, "robot-b", "robot-a"));
        report.inputs_closed = true;
        let parsed: CrossingReport = serde_json::from_str(&report.to_json().unwrap()).unwrap();
        assert_eq!(parsed, report);
        assert_eq!(
            parsed.checker_labels.get("role").map(String::as_str),
            Some("sensing")
        );
    }

    /// The verdict path defaults under the temporary directory, so a run never
    /// writes into the checkout.
    #[test]
    fn the_report_path_defaults_to_the_temp_dir() {
        assert!(default_report_path().starts_with(std::env::temp_dir()));
    }
}
