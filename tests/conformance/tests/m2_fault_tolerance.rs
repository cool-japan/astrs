//! **Milestone M2**, the §5.4 fault-tolerance pair: `restart-policies` and
//! `error-propagation` (blueprint §12).
//!
//! Two claims, each with a committed example and a durable artefact:
//!
//! | Claim (§12) | Example | Artefact |
//! |---|---|---|
//! | *budget `max_restarts`* | `restart-policies` | one JSON line per **start**, appended before each incarnation does anything else |
//! | *a failing node emits `NodeFailed` to graph peers* | `error-propagation` | the watcher's report of the events it received, with the **typed** cause |
//!
//! ```text
//!   restart-policies      start ─fail─► start ─fail─► start ─work─►  exit 0
//!                         budget-exhausted.yml: exactly 1 + max_restarts starts
//!
//!   error-propagation     [source] ─exit 23─► NodeFailed{peer,cause} ─► [watcher]
//! ```
//!
//! # Nothing here measures time
//!
//! Backoff is `restart_delay × 2ⁿ`, and a test that measured it would be
//! measuring the machine it ran on. What is asserted is the **count** of
//! starts, the generations, the typed exit causes, and which node the
//! `DataflowResult` blames.
//!
//! # Mapping onto §12's 16-scenario conformance zoo
//!
//! Only one test below is itself a numbered zoo scenario:
//! `an_always_failing_node_exhausts_its_budget_and_stops` is scenario 7/16
//! (restart-budget exhaustion), exercised here through a real `astrs run`
//! process rather than the library-level harness that already covers it in
//! `crates/astrs-daemon/tests/node_roles.rs::restart_policy`. The other three
//! tests are real, valuable coverage of §12's prose (the restart-budget
//! *happy path*, and error propagation) but are not themselves one of the
//! sixteen named scenarios — see §12 for the full list and
//! `crates/astrs-daemon/tests/node_roles.rs` for where the ten dora-flavored
//! ones live.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::time::Duration;

use astrs_cli::command::run::{EXIT_OK, RunArgs, RunReport, run};
use astrs_conformance::{
    Fixture, StageOptions, example_dir, result_file, stage_example, stage_manifest_with,
};
use astrs_wire::{DataflowStatus, NodeExitCause};
use error_propagation::FaultReport;
use restart_policies::{FAILURE_EXIT_CODE, Incarnation, RestartSummary};

/// The ceiling on any one graph in this file.
const RUN_TIMEOUT: Duration = Duration::from_secs(90);

/// Runs a staged fixture through `astrs run`.
fn run_fixture(fixture: &Fixture) -> (RunReport, String) {
    let mut args = RunArgs::new(&fixture.manifest);
    args.skip_build = true;
    args.working_dir = Some(fixture.dir.clone());
    args.runtime_dir = Some(fixture.dir.clone());
    args.timeout = Some(RUN_TIMEOUT);
    args.grace = Some(Duration::from_millis(500));

    let mut terminal: Vec<u8> = Vec::new();
    let report = run(&mut terminal, &args).expect("astrs run returned a report");
    let text = String::from_utf8_lossy(&terminal).into_owned();
    (report, text)
}

/// Every incarnation the worker recorded, in the order it started them.
fn incarnations(log: &Path) -> Vec<Incarnation> {
    let text = std::fs::read_to_string(log)
        .unwrap_or_else(|error| panic!("no incarnation log at {}: {error}", log.display()));
    Incarnation::parse_log(&text)
}

/// Asserts a log's incarnations are a complete, ordered run of starts.
fn assert_well_formed(log: &[Incarnation], starts: usize, text: &str) {
    assert_eq!(
        log.len(),
        starts,
        "the supervisor started the node {} time(s), not {starts}:\n{text}",
        log.len()
    );
    for (index, entry) in log.iter().enumerate() {
        assert_eq!(
            entry.restart_count, index as u64,
            "start {index} reported restart_count {}:\n{text}",
            entry.restart_count
        );
    }
    // A new incarnation is a new generation (§12): the stamps are distinct and
    // increasing, which is what lets a stale message be rejected.
    let generations: Vec<u64> = log.iter().map(|entry| entry.generation).collect();
    let mut sorted = generations.clone();
    sorted.dedup();
    assert_eq!(sorted.len(), generations.len(), "{generations:?}\n{text}");
    assert!(
        generations.windows(2).all(|pair| pair[0] < pair[1]),
        "generations must increase with each incarnation: {generations:?}\n{text}"
    );
}

/// §12's happy path: a node that fails twice is restarted twice and then runs
/// to completion, and the run finishes clean.
///
/// Not itself one of §12's 16 numbered zoo scenarios (it is the *recovery*
/// half of the restart-policy story; scenario 7/16 is the *exhaustion* half,
/// below).
#[test]
fn a_flaky_node_recovers_inside_its_restart_budget() {
    let log_path = result_file("restart-log.txt");
    let summary_path = result_file("restart-summary.json");
    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_file(&summary_path);

    let options = StageOptions::new("restart-policies")
        .with_env_path(restart_policies::ENV_RESTART_LOG, &log_path)
        .with_env_path(restart_policies::ENV_SUMMARY_PATH, &summary_path);
    let fixture = stage_example("restart-policies", &options).expect("staged");

    let (report, text) = run_fixture(&fixture);
    assert_eq!(report.exit_code(), EXIT_OK, "non-zero exit:\n{text}");
    assert_eq!(report.result.status, DataflowStatus::Finished, "{text}");
    assert!(
        !report.result.has_failures(),
        "a node that recovered must not leave the dataflow failed:\n{text}"
    );

    // Three starts: the original plus two restarts. The two failures are the
    // manifest's `RESTART_FAILURES: 2`, well inside its `max_restarts: 5`.
    let log = incarnations(&log_path);
    assert_well_formed(&log, 3, &text);
    assert_eq!(
        log.iter().filter(|entry| entry.will_fail).count(),
        2,
        "{log:?}\n{text}"
    );
    assert!(!log[2].will_fail, "{log:?}\n{text}");

    // The incarnation that survived agrees with the log about which one it
    // was — the node API's `restart_count`/`generation` and the file are two
    // independent views of the same fact.
    let summary: RestartSummary =
        serde_json::from_str(&std::fs::read_to_string(&summary_path).expect("a summary"))
            .expect("a readable summary");
    assert_eq!(summary.restart_count, 2, "{summary:?}\n{text}");
    assert_eq!(summary.generation, log[2].generation, "{summary:?}\n{text}");
    assert_eq!(summary.ticks, 3, "{summary:?}\n{text}");

    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_file(&summary_path);
    fixture.clean();
}

/// §12 conformance zoo, scenario 7/16 (restart-budget exhaustion) — the CLI
/// counterpart to `crates/astrs-daemon/tests/node_roles.rs::restart_policy`,
/// through a real `astrs run` process rather than the library harness.
///
/// §12's other half: `max_restarts` is a **ceiling**. The supervisor starts
/// the node exactly `1 + max_restarts` times and then stops, and the dataflow
/// carries the node's typed exit cause.
#[test]
fn an_always_failing_node_exhausts_its_budget_and_stops() {
    let log_path = result_file("restart-budget-log.txt");
    let _ = std::fs::remove_file(&log_path);

    let options = StageOptions::new("restart-policies")
        .with_env_path(restart_policies::ENV_RESTART_LOG, &log_path);
    let source = example_dir("restart-policies").join("budget-exhausted.yml");
    let fixture = stage_manifest_with(&source, &options).expect("staged");

    let (report, text) = run_fixture(&fixture);
    assert_ne!(
        report.exit_code(),
        EXIT_OK,
        "a graph whose node exhausted its budget has failed:\n{text}"
    );
    assert!(report.result.has_failures(), "{text}");

    // `max_restarts: 2` in the committed manifest, so three starts: the
    // original plus two. Exactly — not "at least", which is what makes this a
    // budget rather than a suggestion.
    let log = incarnations(&log_path);
    assert_well_formed(&log, 3, &text);
    assert!(
        log.iter().all(|entry| entry.will_fail),
        "every incarnation was told to fail: {log:?}\n{text}"
    );

    // The cause is typed, and says *why the supervisor gave up* rather than
    // merely repeating the last exit code — §12's "typed causes, not strings"
    // is worth the most exactly here, where the interesting fact is the
    // budget, not the symptom. The count it carries is the manifest's
    // `max_restarts`, and the window its `restart_window` default.
    let (node, cause) = report
        .result
        .failed_nodes()
        .next()
        .unwrap_or_else(|| panic!("the result names no failed node:\n{text}"));
    assert_eq!(node.as_str(), "worker", "{text}");
    match cause {
        NodeExitCause::RestartBudgetExhausted { restarts, .. } => assert_eq!(
            *restarts, 2,
            "the cause must carry the budget that was spent:\n{text}"
        ),
        other => panic!("expected a budget-exhausted cause, got {other:?}:\n{text}"),
    }
    // And the last incarnation really did exit the way the worker says it
    // does, which is what the supervisor was counting.
    assert!(
        text.contains(&format!("exited with code {FAILURE_EXIT_CODE}")),
        "{text}"
    );

    let _ = std::fs::remove_file(&log_path);
    fixture.clean();
}

/// §12's error propagation: a failing node's `NodeFailed` reaches its graph
/// peer as an ordinary event, carrying a **typed** cause — and the dataflow's
/// result blames the node that failed, not the one that was told.
///
/// Not one of §12's 16 numbered zoo scenarios — error propagation is a
/// separate, general property in §12's prose, not an entry in the
/// misbehaving-node list.
#[test]
fn a_node_failure_reaches_its_peer_with_a_typed_cause() {
    let report_path = result_file("fault-report.json");
    let _ = std::fs::remove_file(&report_path);

    let options = StageOptions::new("error-propagation")
        .with_env_path(error_propagation::ENV_REPORT_PATH, &report_path);
    let fixture = stage_example("error-propagation", &options).expect("staged");

    let (run_report, text) = run_fixture(&fixture);

    // The graph *does* fail: its source fails on purpose. What matters is
    // which node is blamed, and with what.
    assert!(run_report.result.has_failures(), "{text}");
    assert_eq!(
        run_report.result.node_results.get(&node_id("source")),
        Some(&NodeExitCause::ExitCode {
            code: i32::from(error_propagation::FAILURE_EXIT_CODE)
        }),
        "the failing node's cause must be typed and its own:\n{text}"
    );
    assert_eq!(
        run_report.result.node_results.get(&node_id("watcher")),
        Some(&NodeExitCause::Success),
        "the peer that was merely *told* must not be blamed:\n{text}"
    );

    // And the peer really received it, as an event, in its own process.
    let report: FaultReport =
        serde_json::from_str(&std::fs::read_to_string(&report_path).expect("a report"))
            .expect("a readable report");
    assert!(
        report.saw_failure_of("source"),
        "the watcher was never told: {report:?}\n{text}"
    );
    let failure = report.failure_of("source").expect("the recorded failure");
    assert_eq!(
        failure.cause_kind, "exit_code",
        "a typed cause, not a string: {failure:?}\n{text}"
    );
    assert_eq!(
        failure.exit_code,
        Some(i32::from(error_propagation::FAILURE_EXIT_CODE)),
        "{failure:?}\n{text}"
    );

    // Everything the source published before it died still arrived: a peer
    // failure must not cost the messages that preceded it.
    assert_eq!(
        report.readings,
        error_propagation::DEFAULT_READINGS,
        "{report:?}\n{text}"
    );
    assert!(
        report
            .closed_inputs
            .iter()
            .any(|closed| closed.starts_with("readings")),
        "the watcher's input must close: {report:?}\n{text}"
    );
    assert!(report.finished_cleanly, "{report:?}\n{text}");

    let _ = std::fs::remove_file(&report_path);
    fixture.clean();
}

/// Both committed manifests of `restart-policies` declare a bounded budget.
///
/// The drift guard behind the two tests above: an unbounded `max_restarts`
/// would make `budget-exhausted.yml` loop instead of failing, and the test
/// that waits for its dataflow would hang rather than report.
#[test]
fn every_restart_manifest_declares_a_bounded_budget() {
    for name in ["dataflow.yml", "budget-exhausted.yml"] {
        let path = example_dir("restart-policies").join(name);
        let manifest = astrs_manifest::Manifest::from_yaml_file(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        for node in &manifest.nodes {
            assert_eq!(
                node.restart_policy,
                Some(astrs_manifest::RestartPolicy::OnFailure),
                "{name}/{}",
                node.id
            );
            let budget = node
                .max_restarts
                .unwrap_or_else(|| panic!("{name}/{} declares no budget", node.id));
            assert!(budget > 0 && budget <= 8, "{name}/{}: {budget}", node.id);
        }
    }
}

/// A node id, for looking one up in a [`astrs_wire::DataflowResult`].
fn node_id(name: &str) -> astrs_wire::NodeId {
    astrs_wire::NodeId::new(name).expect("a legal node id")
}
