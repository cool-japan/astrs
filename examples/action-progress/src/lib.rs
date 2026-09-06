//! Shared types for `action-progress` — a long-running action goal with
//! progress feedback over the `goal_id`/`goal_status` FSM (blueprint §9.4,
//! `astrs_node_api::patterns::action`).
//!
//! ```text
//!   [client] ──goal────► [server]
//!      ▲                     │
//!      └──────status─────────┘
//! ```
//!
//! `client` submits a small run of goals, one at a time, over
//! [`astrs_node_api::Node::goal`]. `server` executes each in four steps —
//! three `Executing` feedback updates carrying a fractional progress value,
//! then one terminal update — over
//! [`astrs_node_api::Node::goal_status`]. There is no separate feedback
//! topic: it is the same `status` edge, the same `goal_id`, and the queue
//! immunity §11.2 already grants a correlated message.
//!
//! # Why one goal at a time
//!
//! [`astrs_node_api::GoalTracker`] can track any number of goals at once,
//! but `client` submits its goals sequentially — waiting for one to reach a
//! terminal status before the next is sent — because this example's claim
//! (feedback arrives in increasing order and ends in the right terminal
//! status) does not need concurrent goals to be checkable, and a
//! sequential run is easier to read start to finish.
//!
//! # Why one goal aborts
//!
//! [`GOAL_TARGETS`] includes a negative target on purpose. A demo where
//! every goal succeeds cannot tell a reader whether `Aborted` was ever
//! actually wired up — the same reason `restart-policies` ships a
//! budget-exhausted manifest alongside its happy path.

use astrs_wire::GoalStatus;
use serde::{Deserialize, Serialize};

/// The client's output port carrying each goal, and the server's input.
pub const GOAL_PORT: &str = "goal";
/// The server's output port carrying every status update (feedback and
/// terminal alike), and the client's input.
pub const STATUS_PORT: &str = "status";

/// Environment variable naming the JSON file the client writes its
/// [`ActionRunReport`] to.
pub const ENV_REPORT_PATH: &str = "ACTION_PROGRESS_REPORT";

/// The goal targets `client` submits, in order. The last one is negative on
/// purpose — see the crate docs' "Why one goal aborts".
pub const GOAL_TARGETS: &[i64] = &[5, 8, -3];

/// The fractional progress `server` reports as `Executing` feedback, in
/// order, before the terminal update.
pub const FEEDBACK_FRACTIONS: &[f64] = &[0.25, 0.5, 0.75];

/// The fractional progress a `Succeeded` terminal update carries.
pub const SUCCEEDED_FRACTION: f64 = 1.0;
/// The fractional progress an `Aborted` terminal update carries — negative,
/// so it can never be confused with real feedback even by a reader who has
/// not checked the status field.
pub const ABORTED_FRACTION: f64 = -1.0;

/// Where the client writes its report when the manifest names no path.
#[must_use]
pub fn default_report_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-action-progress-report.json")
}

/// The JSON file this run's client writes its report to.
#[must_use]
pub fn report_path() -> std::path::PathBuf {
    std::env::var(ENV_REPORT_PATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default_report_path, std::path::PathBuf::from)
}

/// The terminal status `server` reports for `target`: `Aborted` for a
/// negative target, `Succeeded` otherwise.
///
/// A pure function so both binaries and this crate's own tests can predict
/// a run's outcome without executing anything.
#[must_use]
pub const fn expected_status_for(target: i64) -> GoalStatus {
    if target < 0 {
        GoalStatus::Aborted
    } else {
        GoalStatus::Succeeded
    }
}

/// Whether a run of fractional feedback values is strictly increasing —
/// what real progress toward a goal looks like, as opposed to feedback that
/// repeated, went backwards, or arrived out of order.
#[must_use]
pub fn is_strictly_increasing(values: &[f64]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

/// One goal's whole life cycle, as `client` observed it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoalRun {
    /// The target `client` asked for.
    pub target: i64,
    /// The goal id `Node::goal` generated.
    pub goal_id: String,
    /// Every `Executing` feedback fraction, in arrival order.
    pub feedback: Vec<f64>,
    /// The terminal status observed, if the run reached one before the
    /// session ended.
    pub final_status: Option<GoalStatus>,
    /// The fractional value the terminal update itself carried.
    pub final_fraction: Option<f64>,
}

impl GoalRun {
    /// A run with no observations yet.
    #[must_use]
    pub fn new(target: i64, goal_id: impl Into<String>) -> Self {
        Self {
            target,
            goal_id: goal_id.into(),
            feedback: Vec::new(),
            final_status: None,
            final_fraction: None,
        }
    }

    /// Whether this run reached the terminal status [`expected_status_for`]
    /// predicts for its own target, with feedback that was real progress
    /// (non-empty and strictly increasing).
    #[must_use]
    pub fn is_correct(&self) -> bool {
        self.final_status == Some(expected_status_for(self.target))
            && !self.feedback.is_empty()
            && is_strictly_increasing(&self.feedback)
    }
}

/// The client's whole session: every goal it ran, in submission order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ActionRunReport {
    /// One entry per goal submitted.
    pub runs: Vec<GoalRun>,
}

impl ActionRunReport {
    /// Builds a report from completed runs.
    #[must_use]
    pub const fn new(runs: Vec<GoalRun>) -> Self {
        Self { runs }
    }

    /// Whether every submitted goal ran to completion and
    /// [`GoalRun::is_correct`].
    #[must_use]
    pub fn all_correct(&self, expected_goals: usize) -> bool {
        self.runs.len() == expected_goals && self.runs.iter().all(GoalRun::is_correct)
    }

    /// Renders the report as pretty JSON.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if it cannot be serialised, which its field
    /// types make impossible in practice.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A negative target aborts; anything else succeeds.
    #[test]
    fn expected_status_follows_the_targets_sign() {
        assert_eq!(expected_status_for(5), GoalStatus::Succeeded);
        assert_eq!(expected_status_for(0), GoalStatus::Succeeded);
        assert_eq!(expected_status_for(-1), GoalStatus::Aborted);
        // Every configured target actually gets exercised by the manifest's
        // own budget: at least one succeeds and at least one aborts.
        assert!(
            GOAL_TARGETS
                .iter()
                .any(|target| expected_status_for(*target) == GoalStatus::Succeeded)
        );
        assert!(
            GOAL_TARGETS
                .iter()
                .any(|target| expected_status_for(*target) == GoalStatus::Aborted)
        );
    }

    /// Strictly increasing feedback passes; anything that repeats, stalls
    /// or goes backwards does not.
    #[test]
    fn strictly_increasing_feedback_is_recognised() {
        assert!(is_strictly_increasing(&[0.25, 0.5, 0.75]));
        assert!(is_strictly_increasing(&[0.1]));
        assert!(is_strictly_increasing(&[]));
        assert!(!is_strictly_increasing(&[0.5, 0.5, 0.75]), "a repeat");
        assert!(!is_strictly_increasing(&[0.5, 0.25, 0.75]), "a regression");
    }

    /// A run whose feedback matches [`FEEDBACK_FRACTIONS`] and whose
    /// terminal status matches its target is correct.
    #[test]
    fn a_well_formed_succeeded_run_is_correct() {
        let mut run = GoalRun::new(5, "g1");
        run.feedback = FEEDBACK_FRACTIONS.to_vec();
        run.final_status = Some(GoalStatus::Succeeded);
        run.final_fraction = Some(SUCCEEDED_FRACTION);
        assert!(run.is_correct(), "{run:?}");
    }

    /// The same shape, but for the aborting target, is correct too — a
    /// "correct" run is not synonymous with "succeeded".
    #[test]
    fn a_well_formed_aborted_run_is_correct() {
        let mut run = GoalRun::new(-3, "g2");
        run.feedback = FEEDBACK_FRACTIONS.to_vec();
        run.final_status = Some(GoalStatus::Aborted);
        run.final_fraction = Some(ABORTED_FRACTION);
        assert!(run.is_correct(), "{run:?}");
    }

    /// A run whose terminal status disagrees with its own target's sign is
    /// not correct.
    #[test]
    fn a_wrong_terminal_status_is_not_correct() {
        let mut run = GoalRun::new(5, "g3");
        run.feedback = FEEDBACK_FRACTIONS.to_vec();
        run.final_status = Some(GoalStatus::Aborted); // target was positive
        assert!(!run.is_correct());
    }

    /// A run with no feedback at all is not correct, even with the right
    /// terminal status — "it finished" is not "it reported progress".
    #[test]
    fn a_run_with_no_feedback_is_not_correct() {
        let mut run = GoalRun::new(5, "g4");
        run.final_status = Some(GoalStatus::Succeeded);
        assert!(!run.is_correct());
    }

    /// A run still in flight (`final_status: None`) is never correct.
    #[test]
    fn an_unfinished_run_is_not_correct() {
        let mut run = GoalRun::new(5, "g5");
        run.feedback = FEEDBACK_FRACTIONS.to_vec();
        assert!(!run.is_correct());
    }

    /// A report is `all_correct` only when every configured goal ran and
    /// every one of them was individually correct.
    #[test]
    fn a_report_is_all_correct_only_when_every_run_is() {
        let mut good = GoalRun::new(5, "a");
        good.feedback = FEEDBACK_FRACTIONS.to_vec();
        good.final_status = Some(GoalStatus::Succeeded);
        let mut bad = GoalRun::new(8, "b");
        bad.feedback = FEEDBACK_FRACTIONS.to_vec();
        bad.final_status = Some(GoalStatus::Aborted); // wrong for a positive target

        let all_good = ActionRunReport::new(vec![good.clone(), good.clone()]);
        assert!(all_good.all_correct(2));

        let one_bad = ActionRunReport::new(vec![good.clone(), bad]);
        assert!(!one_bad.all_correct(2));

        let too_few = ActionRunReport::new(vec![good]);
        assert!(!too_few.all_correct(2), "fewer runs than expected");
    }

    /// The report round-trips as JSON, `GoalStatus` included.
    #[test]
    fn a_report_round_trips_as_json() {
        let mut run = GoalRun::new(5, "g6");
        run.feedback = FEEDBACK_FRACTIONS.to_vec();
        run.final_status = Some(GoalStatus::Succeeded);
        run.final_fraction = Some(SUCCEEDED_FRACTION);
        let report = ActionRunReport::new(vec![run]);
        let json = report.to_json().unwrap();
        assert!(json.contains("succeeded"), "{json}");
        let parsed: ActionRunReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }

    /// The report path defaults under the temporary directory.
    #[test]
    fn the_report_path_defaults_to_the_temp_dir() {
        assert!(default_report_path().starts_with(std::env::temp_dir()));
    }

    /// The committed manifest parses, validates, declares the
    /// `action-client`/`action-server` pattern pair, and names this
    /// dataflow.
    #[test]
    fn the_committed_manifest_parses_and_validates() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dataflow.yml");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("pattern: action-client"), "{text}");
        assert!(text.contains("pattern: action-server"), "{text}");
        let manifest = astrs_manifest::Manifest::from_yaml_file(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        manifest
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(manifest.name.as_deref(), Some("action-progress"));
        assert_eq!(manifest.nodes.len(), 2);
    }
}
