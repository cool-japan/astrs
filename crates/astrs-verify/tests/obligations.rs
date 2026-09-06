//! End-to-end obligation tests: real manifests, the real solver, the real
//! report.
//!
//! Every obligation appears here **twice** — once on a graph that should
//! satisfy it and once on a graph that should not. A single test per
//! obligation would pass just as happily with the polarity reversed, which
//! is the one bug in a verifier that nothing else catches.

#![cfg(feature = "verify")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_graph::DataflowGraph;
use astrs_manifest::Manifest;
use astrs_verify::{
    Discharge, ObligationKind, Profile, ProveOptions, RenderOptions, Strength, VerificationReport,
    prove, render_human,
};

fn graph_of(yaml: &str) -> DataflowGraph {
    let manifest = Manifest::from_yaml_str(yaml).expect("fixture manifest parses");
    manifest.validate().expect("fixture manifest validates");
    let (graph, _) = DataflowGraph::from_manifest(&manifest).expect("fixture graph builds");
    graph
}

fn prove_yaml(yaml: &str) -> VerificationReport {
    prove(&graph_of(yaml), &ProveOptions::default()).expect("model builds")
}

fn prove_with_profile(manifest_yaml: &str, profile_yaml: &str) -> VerificationReport {
    let profile = Profile::from_yaml_str(profile_yaml).expect("fixture profile parses");
    prove(
        &graph_of(manifest_yaml),
        &ProveOptions::with_profile(profile),
    )
    .expect("model builds")
}

/// Assert that every obligation of `kind` in `report` holds.
fn assert_all_hold(report: &VerificationReport, kind: ObligationKind) {
    let outcomes: Vec<_> = report.of_kind(kind).collect();
    assert!(!outcomes.is_empty(), "{kind} produced no obligation");
    for outcome in outcomes {
        assert!(
            outcome.discharge.holds(),
            "{kind} ({}) should hold, got {:?}",
            outcome.obligation.id,
            outcome.discharge
        );
    }
}

/// Assert that at least one obligation of `kind` is violated, and return it.
fn assert_violated(
    report: &VerificationReport,
    kind: ObligationKind,
) -> &astrs_verify::Counterexample {
    let violated = report
        .of_kind(kind)
        .find(|outcome| outcome.discharge.is_violated())
        .unwrap_or_else(|| {
            panic!(
                "{kind} should be violated; outcomes were {:?}",
                report
                    .of_kind(kind)
                    .map(|o| (&o.obligation.id, o.discharge.status()))
                    .collect::<Vec<_>>()
            )
        });
    violated
        .discharge
        .counterexample()
        .expect("a violated obligation carries a counterexample")
}

// ---------------------------------------------------------------------------
// The sound reference graph
// ---------------------------------------------------------------------------

/// A graph nothing is wrong with: timer-driven, typed end to end, shallow
/// queues, a timeout the producer comfortably meets.
const SOUND: &str = "
strict_types: true
nodes:
  - id: camera
    path: ./camera
    inputs:
      tick:
        source: astrs/timer/hz/10
        queue_size: 1
    outputs: [frames]
    output_types: { frames: \"std/media/v1/Image\" }
  - id: detector
    path: ./detector
    inputs:
      frames:
        source: camera/frames
        queue_size: 2
        timeout: 1.0
    input_types: { frames: \"std/media/v1/Image\" }
";

/// The service times the sound graph's nodes are declared to take.
const SOUND_PROFILE: &str = "
nodes:
  camera:
    wcet: 0.002
  detector:
    wcet: { min: 0.001, max: 0.020 }
";

#[test]
fn a_sound_manifest_passes_every_obligation() {
    let report = prove_with_profile(SOUND, SOUND_PROFILE);
    assert!(
        !report.has_violations(),
        "{}",
        render_human(&report, &RenderOptions::default())
    );
    assert!(
        report.everything_discharged(),
        "every obligation must reach a verdict:\n{}",
        render_human(&report, &RenderOptions::default())
    );
    for kind in ObligationKind::ALL {
        assert_all_hold(&report, kind);
    }
    let summary = report.summary();
    assert_eq!(summary.violated, 0);
    assert_eq!(summary.not_attempted, 0);
    assert_eq!(summary.inconclusive, 0);
    assert!(summary.holds >= 5, "{summary}");
}

#[test]
fn the_sound_graph_without_a_profile_reports_rather_than_guesses() {
    let report = prove_yaml(SOUND);
    assert!(!report.has_violations());
    assert!(
        !report.everything_discharged(),
        "no service times means boundedness cannot be decided"
    );
    for outcome in report.of_kind(ObligationKind::QueueBoundedness) {
        assert!(
            matches!(
                outcome.discharge,
                Discharge::NotAttempted {
                    reason: astrs_verify::NotAttempted::NoServiceTime { .. }
                }
            ),
            "{:?}",
            outcome.discharge
        );
    }
    // The other four still decide without any profile at all.
    assert_all_hold(&report, ObligationKind::DeadlockFreedom);
    assert_all_hold(&report, ObligationKind::RateConsistency);
    assert_all_hold(&report, ObligationKind::LatencyBudget);
    assert_all_hold(&report, ObligationKind::TypeConsistency);
}

// ---------------------------------------------------------------------------
// Deadlock freedom
// ---------------------------------------------------------------------------

/// Two nodes each waiting on the other, with nothing to start either.
const DEADLOCK: &str = "
nodes:
  - id: planner
    path: ./planner
    inputs: { pose: localizer/pose }
    outputs: [plan]
  - id: localizer
    path: ./localizer
    inputs: { plan: planner/plan }
    outputs: [pose]
";

#[test]
fn a_deadlocking_manifest_is_proven_to_deadlock() {
    let report = prove_yaml(DEADLOCK);
    let counterexample = assert_violated(&report, ObligationKind::DeadlockFreedom);
    assert_eq!(
        counterexample.strength,
        Strength::Proof,
        "an initially-empty siphon is a proof, not a suspicion"
    );
    assert!(
        counterexample.cross_checked,
        "the assignment must survive this crate's own replay"
    );
    assert!(
        counterexample.headline.contains("can never fire"),
        "{}",
        counterexample.headline
    );
    assert!(
        counterexample.facts.iter().any(
            |fact| fact.value.contains("planner.pose") || fact.value.contains("localizer.plan")
        ),
        "{:?}",
        counterexample.facts
    );
    assert!(!counterexample.trace.is_empty());
    assert!(counterexample.remedy.is_some());
}

#[test]
fn the_deadlock_counterexample_renders_for_a_human() {
    let report = prove_yaml(DEADLOCK);
    let text = render_human(&report, &RenderOptions::default());
    assert!(text.contains("VIOLATED"), "{text}");
    assert!(text.contains("deadlock freedom"), "{text}");
    assert!(text.contains("can never fire"), "{text}");
    assert!(text.contains("trace"), "{text}");
    assert!(text.contains("fix:"), "{text}");
    assert!(text.contains("(proved)"), "{text}");
    assert!(
        !text.contains('\u{1b}'),
        "plain rendering has no ANSI codes"
    );
}

#[test]
fn one_timer_is_enough_to_prove_the_same_cycle_live() {
    let report = prove_yaml(
        "
nodes:
  - id: planner
    path: ./planner
    inputs:
      tick:
        source: astrs/timer/hz/10
        queue_size: 1
      pose: localizer/pose
    outputs: [plan]
  - id: localizer
    path: ./localizer
    inputs: { plan: planner/plan }
    outputs: [pose]
",
    );
    assert_all_hold(&report, ObligationKind::DeadlockFreedom);
}

#[test]
fn an_unwired_service_correlation_wedges_its_client() {
    let report = prove_yaml(
        "
nodes:
  - id: client
    path: ./client
    pattern: service-client
    inputs:
      tick:
        source: astrs/timer/hz/1
        queue_size: 1
    outputs: [request]
  - id: server
    path: ./server
    pattern: service-server
    inputs: { request: client/request }
    outputs: [response]
",
    );
    let counterexample = assert_violated(&report, ObligationKind::DeadlockFreedom);
    assert!(
        counterexample.headline.contains("waits forever"),
        "{}",
        counterexample.headline
    );
}

#[test]
fn a_fully_wired_service_correlation_is_deadlock_free() {
    let report = prove_yaml(
        "
nodes:
  - id: client
    path: ./client
    pattern: service-client
    inputs:
      tick:
        source: astrs/timer/hz/1
        queue_size: 1
      response: server/response
    outputs: [request]
  - id: server
    path: ./server
    pattern: service-server
    inputs: { request: client/request }
    outputs: [response]
",
    );
    assert_all_hold(&report, ObligationKind::DeadlockFreedom);
}

#[test]
fn a_shallow_action_status_queue_is_a_capacity_deadlock() {
    let report = prove_yaml(
        "
nodes:
  - id: client
    path: ./client
    pattern: action-client
    inputs:
      tick:
        source: astrs/timer/hz/1
        queue_size: 1
      status:
        source: server/status
        queue_size: 1
    outputs: [goal]
  - id: server
    path: ./server
    pattern: action-server
    inputs: { goal: client/goal }
    outputs: [status]
",
    );
    let counterexample = assert_violated(&report, ObligationKind::DeadlockFreedom);
    assert!(
        counterexample.headline.contains("goal status"),
        "{}",
        counterexample.headline
    );
    assert!(
        counterexample
            .remedy
            .as_deref()
            .is_some_and(|remedy| remedy.contains("queue_size")),
        "{:?}",
        counterexample.remedy
    );
}

#[test]
fn a_deep_enough_action_status_queue_is_deadlock_free() {
    let report = prove_yaml(
        "
nodes:
  - id: client
    path: ./client
    pattern: action-client
    inputs:
      tick:
        source: astrs/timer/hz/1
        queue_size: 1
      status:
        source: server/status
        queue_size: 4
    outputs: [goal]
  - id: server
    path: ./server
    pattern: action-server
    inputs: { goal: client/goal }
    outputs: [status]
",
    );
    assert_all_hold(&report, ObligationKind::DeadlockFreedom);
}

// ---------------------------------------------------------------------------
// Rate consistency
// ---------------------------------------------------------------------------

fn timeout_graph(period: &str, timeout: &str) -> String {
    format!(
        "
nodes:
  - id: camera
    path: ./camera
    inputs:
      tick:
        source: {period}
        queue_size: 1
    outputs: [frames]
  - id: detector
    path: ./detector
    inputs:
      frames:
        source: camera/frames
        queue_size: 1
        timeout: {timeout}
"
    )
}

#[test]
fn a_timeout_the_producer_cannot_meet_is_caught() {
    // A 0.5 Hz producer feeding an input that expects one every 500ms.
    let report = prove_yaml(&timeout_graph("astrs/timer/secs/2", "0.5"));
    let counterexample = assert_violated(&report, ObligationKind::RateConsistency);
    assert_eq!(counterexample.strength, Strength::Proof);
    assert!(counterexample.cross_checked);
    assert!(
        counterexample.headline.contains("times out"),
        "{}",
        counterexample.headline
    );
    let facts: Vec<&str> = counterexample
        .facts
        .iter()
        .map(|fact| fact.value.as_str())
        .collect();
    assert!(facts.contains(&"1/2 Hz"), "{facts:?}");
    assert!(facts.contains(&"500ms"), "{facts:?}");
    assert!(facts.contains(&"2s"), "{facts:?}");
}

#[test]
fn a_timeout_the_producer_meets_holds() {
    let report = prove_yaml(&timeout_graph("astrs/timer/secs/2", "5.0"));
    assert_all_hold(&report, ObligationKind::RateConsistency);
}

#[test]
fn a_declared_rate_the_wiring_contradicts_is_caught() {
    let report = prove_with_profile(
        "
nodes:
  - id: camera
    path: ./camera
    inputs:
      tick:
        source: astrs/timer/hz/50
        queue_size: 1
    outputs: [frames]
  - id: detector
    path: ./detector
    inputs: { frames: camera/frames }
",
        "nodes:\n  detector:\n    rate: 30\n",
    );
    let counterexample = assert_violated(&report, ObligationKind::RateConsistency);
    assert!(
        counterexample.headline.contains("declared rate"),
        "{}",
        counterexample.headline
    );
    let facts: Vec<&str> = counterexample
        .facts
        .iter()
        .map(|fact| fact.value.as_str())
        .collect();
    assert!(facts.contains(&"30 Hz"), "{facts:?}");
    assert!(facts.contains(&"50 Hz"), "{facts:?}");
}

#[test]
fn a_declared_rate_the_wiring_agrees_with_holds() {
    let report = prove_with_profile(
        "
nodes:
  - id: camera
    path: ./camera
    inputs:
      tick:
        source: astrs/timer/hz/50
        queue_size: 1
    outputs: [frames]
  - id: detector
    path: ./detector
    inputs: { frames: camera/frames }
",
        "nodes:\n  detector:\n    rate: 50\n",
    );
    assert_all_hold(&report, ObligationKind::RateConsistency);
}

// ---------------------------------------------------------------------------
// Latency budgets
// ---------------------------------------------------------------------------

fn latency_chain(queue: u32, budget: &str) -> String {
    format!(
        "
nodes:
  - id: camera
    path: ./camera
    inputs:
      tick:
        source: astrs/timer/hz/10
        queue_size: 1
    outputs: [frames]
  - id: stage1
    path: ./stage1
    inputs:
      frames:
        source: camera/frames
        queue_size: {queue}
    outputs: [out]
  - id: stage2
    path: ./stage2
    inputs:
      data:
        source: stage1/out
        queue_size: {queue}
        timeout: {budget}
"
    )
}

#[test]
fn a_latency_budget_the_queues_blow_is_caught() {
    // 10 Hz, two queues of 8: 100ms + 800ms + 800ms = 1.7s against 200ms.
    let report = prove_yaml(&latency_chain(8, "0.2"));
    let counterexample = assert_violated(&report, ObligationKind::LatencyBudget);
    assert!(counterexample.cross_checked);
    assert!(
        counterexample.headline.contains("later than"),
        "{}",
        counterexample.headline
    );
    let worst = counterexample
        .facts
        .iter()
        .find(|fact| fact.label == "worst-case delivery")
        .expect("the worst case is reported");
    assert_eq!(worst.value, "1.7s", "{:?}", counterexample.facts);
    assert!(
        counterexample
            .trace
            .iter()
            .any(|step| step.actor == "stage2.data"),
        "{:?}",
        counterexample.trace
    );
}

#[test]
fn a_latency_budget_the_queues_meet_holds() {
    // 10 Hz, two queues of 1: 100ms + 100ms + 100ms = 300ms against 500ms.
    let report = prove_yaml(&latency_chain(1, "0.5"));
    assert_all_hold(&report, ObligationKind::LatencyBudget);
}

#[test]
fn a_profile_declared_path_budget_is_checked_too() {
    let manifest = "
nodes:
  - id: camera
    path: ./camera
    inputs:
      tick:
        source: astrs/timer/hz/10
        queue_size: 1
    outputs: [frames]
  - id: planner
    path: ./planner
    inputs:
      frames:
        source: camera/frames
        queue_size: 4
";
    let tight = prove_with_profile(
        manifest,
        "paths:\n  - { name: shutter-to-plan, target: planner/frames, budget: 0.05 }\n",
    );
    let counterexample = assert_violated(&tight, ObligationKind::LatencyBudget);
    assert!(
        counterexample
            .facts
            .iter()
            .any(|fact| fact.value == "shutter-to-plan"),
        "{:?}",
        counterexample.facts
    );

    let generous = prove_with_profile(
        manifest,
        "paths:\n  - { name: shutter-to-plan, target: planner/frames, budget: 1.0 }\n",
    );
    assert_all_hold(&generous, ObligationKind::LatencyBudget);
}

// ---------------------------------------------------------------------------
// Queue boundedness
// ---------------------------------------------------------------------------

fn boundedness_graph(service: &str) -> (String, String) {
    (
        "
nodes:
  - id: camera
    path: ./camera
    inputs:
      tick:
        source: astrs/timer/hz/50
        queue_size: 1
    outputs: [frames]
  - id: detector
    path: ./detector
    inputs:
      frames:
        source: camera/frames
        queue_size: 4
"
        .to_string(),
        format!("nodes:\n  camera:\n    wcet: 0.0001\n  detector:\n    wcet: {service}\n"),
    )
}

#[test]
fn a_consumer_that_cannot_keep_up_is_caught() {
    // 50 events per second at 30ms each is 1.5s of work per second.
    let (manifest, profile) = boundedness_graph("0.030");
    let report = prove_with_profile(&manifest, &profile);
    let counterexample = assert_violated(&report, ObligationKind::QueueBoundedness);
    assert_eq!(
        counterexample.strength,
        Strength::Proof,
        "an exact declared service time makes overload a proof"
    );
    assert!(
        counterexample.headline.contains("cannot keep up"),
        "{}",
        counterexample.headline
    );
    let facts: Vec<(&str, &str)> = counterexample
        .facts
        .iter()
        .map(|fact| (fact.label.as_str(), fact.value.as_str()))
        .collect();
    assert!(facts.contains(&("work per window", "1.5s")), "{facts:?}");
    assert!(facts.contains(&("window", "1s")), "{facts:?}");
}

#[test]
fn a_consumer_that_keeps_up_holds() {
    // 50 events per second at 10ms each is 500ms of work per second.
    let (manifest, profile) = boundedness_graph("0.010");
    let report = prove_with_profile(&manifest, &profile);
    assert_all_hold(&report, ObligationKind::QueueBoundedness);
}

#[test]
fn an_interval_service_time_weakens_the_counterexample_honestly() {
    let (manifest, _) = boundedness_graph("0");
    let report = prove_with_profile(
        &manifest,
        "nodes:\n  camera:\n    wcet: 0.0001\n  detector:\n    wcet: { min: 0.001, max: 0.030 }\n",
    );
    let counterexample = assert_violated(&report, ObligationKind::QueueBoundedness);
    assert_eq!(
        counterexample.strength,
        Strength::UnderAssumptions,
        "the node is only overloaded at the slow end of its declared range"
    );
}

// ---------------------------------------------------------------------------
// Type-rule consistency
// ---------------------------------------------------------------------------

fn typed_graph(produced: &str, consumed: &str, rules: &str) -> String {
    format!(
        "
strict_types: true
{rules}nodes:
  - id: camera
    path: ./camera
    inputs:
      tick:
        source: astrs/timer/hz/1
        queue_size: 1
    outputs: [frames]
    output_types: {{ frames: \"{produced}\" }}
  - id: detector
    path: ./detector
    inputs:
      frames:
        source: camera/frames
        queue_size: 1
    input_types: {{ frames: \"{consumed}\" }}
"
    )
}

#[test]
fn an_uncoercible_edge_is_caught() {
    let report = prove_yaml(&typed_graph(
        "std/media/v1/Image",
        "std/vision/v1/Detections",
        "",
    ));
    let counterexample = assert_violated(&report, ObligationKind::TypeConsistency);
    assert_eq!(counterexample.strength, Strength::Proof);
    assert!(
        counterexample
            .headline
            .contains("no chain of type rules reaches"),
        "{}",
        counterexample.headline
    );
    assert!(
        counterexample
            .facts
            .iter()
            .any(|fact| fact.value.contains("strict_types")),
        "{:?}",
        counterexample.facts
    );
}

#[test]
fn a_transitively_coercible_edge_holds() {
    let rules = concat!(
        "type_rules:\n",
        "  - { from: \"std/media/v1/Image\", to: \"std/media/v2/Image\" }\n",
        "  - { from: \"std/media/v2/Image\", to: \"std/media/v3/Image\" }\n"
    );
    let report = prove_yaml(&typed_graph(
        "std/media/v1/Image",
        "std/media/v3/Image",
        rules,
    ));
    assert_all_hold(&report, ObligationKind::TypeConsistency);
}

#[test]
fn a_rule_pointing_the_wrong_way_does_not_help() {
    let rules = "type_rules:\n  - { from: \"std/media/v2/Image\", to: \"std/media/v1/Image\" }\n";
    let report = prove_yaml(&typed_graph(
        "std/media/v1/Image",
        "std/media/v2/Image",
        rules,
    ));
    assert_violated(&report, ObligationKind::TypeConsistency);
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

#[test]
fn solver_runs_are_deterministic() {
    for yaml in [
        SOUND,
        DEADLOCK,
        &timeout_graph("astrs/timer/secs/2", "0.5"),
        &latency_chain(8, "0.2"),
        &typed_graph("a/b/v1/A", "a/b/v1/Z", ""),
    ] {
        let first = prove_yaml(yaml);
        let second = prove_yaml(yaml);
        assert_eq!(first, second, "reports must be identical across runs");
        assert_eq!(
            render_human(&first, &RenderOptions::default()),
            render_human(&second, &RenderOptions::default()),
            "rendered reports must be byte-identical across runs"
        );
    }
}

#[test]
fn a_profiled_run_is_deterministic_too() {
    let first = prove_with_profile(SOUND, SOUND_PROFILE);
    let second = prove_with_profile(SOUND, SOUND_PROFILE);
    assert_eq!(first, second);
    let json_first = serde_json::to_string(&first).expect("serializable");
    let json_second = serde_json::to_string(&second).expect("serializable");
    assert_eq!(json_first, json_second);
}

#[test]
fn repeated_runs_agree_over_many_iterations() {
    let baseline = prove_yaml(DEADLOCK);
    for _ in 0..16 {
        assert_eq!(prove_yaml(DEADLOCK), baseline);
    }
}

// ---------------------------------------------------------------------------
// Report plumbing
// ---------------------------------------------------------------------------

#[test]
fn every_obligation_carries_the_system_it_solved() {
    let report = prove_with_profile(SOUND, SOUND_PROFILE);
    for outcome in &report.obligations {
        assert!(
            outcome.encoded_system.is_some(),
            "{} decided without recording its system",
            outcome.obligation.id
        );
        let system = outcome.encoded_system.as_deref().unwrap_or_default();
        assert!(system.starts_with("(set-logic QF_LIA)"), "{system}");
        assert!(system.ends_with("(check-sat)\n"), "{system}");
    }
}

#[test]
fn reports_round_trip_through_json() {
    let report = prove_yaml(DEADLOCK);
    let json = serde_json::to_string_pretty(&report).expect("serializable");
    let back: VerificationReport = serde_json::from_str(&json).expect("round trips");
    assert_eq!(back, report);
    assert!(json.contains("deadlock_freedom"), "{json}");
}

#[test]
fn an_empty_graph_has_nothing_to_prove_and_says_exactly_that() {
    let report = prove_yaml("nodes: []\n");
    assert!(!report.has_violations());
    assert_eq!(report.node_count, 0);
    for outcome in &report.obligations {
        assert!(
            matches!(
                outcome.discharge,
                Discharge::NotAttempted {
                    reason: astrs_verify::NotAttempted::NothingToProve
                }
            ),
            "{}: {:?}",
            outcome.obligation.id,
            outcome.discharge
        );
    }
    assert_eq!(report.gap_count(), 0, "a vacuous obligation is not a hole");
    assert!(report.everything_discharged());
}

#[test]
fn a_missing_declaration_is_a_hole_but_a_missing_instance_is_not() {
    // No timeout, no types, no profile: rate, latency and typing have
    // nothing to prove, while boundedness has no service time.
    let report = prove_yaml(
        "
nodes:
  - id: camera
    path: ./camera
    inputs:
      tick:
        source: astrs/timer/hz/10
        queue_size: 1
    outputs: [frames]
  - id: sink
    path: ./sink
    inputs:
      frames:
        source: camera/frames
        queue_size: 1
",
    );
    assert!(!report.has_violations());
    assert_eq!(
        report.gap_count(),
        2,
        "only the two boundedness obligations lack a declaration: {:?}",
        report
            .obligations
            .iter()
            .map(|o| (&o.obligation.id, o.discharge.status()))
            .collect::<Vec<_>>()
    );
    assert!(!report.everything_discharged());
}
