//! **Milestone M1** (blueprint §21): `astrs run` executes the canonical typed
//! pipeline on one machine, with zero copy verified by a probe.
//!
//! Every test here drives the real verb — `astrs_cli::command::run::run`, the
//! function the `astrs` binary's `run` arm calls — over a *committed example
//! manifest*, staged onto freshly built binaries by [`astrs_conformance`].
//! Nothing is compiled here, nothing is mocked, and no node is played by the
//! test binary: these are the same three-process graphs a reader gets from
//! `astrs run examples/…/dataflow.yml`.
//!
//! | Test | M1 evidence |
//! |---|---|
//! | `hello_timer_runs_and_exits_zero` | the smallest graph: a virtual timer source drives a node to completion |
//! | `the_canonical_typed_pipeline_runs_end_to_end` | §8.1's shape: three processes, two typed edges, a tally on disk |
//! | `the_zero_copy_probe_reports_an_engaged_plane` | §6.2/§6.3: 4 MiB frames read out of the producer's ring, uncopied |
//! | `the_service_round_trip_answers_every_call` | §9.4: request/response correlation over ordinary edges |
//!
//! # Reading a failure
//!
//! Every assertion prints the run's captured terminal output, which is every
//! node's stdout/stderr plus the daemon's own summary line. A failure here is
//! therefore readable without re-running anything by hand.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::time::Duration;

use astrs_cli::command::run::{EXIT_OK, RunArgs, RunReport, run};
use astrs_conformance::{Fixture, example_manifest, expected_unconsumed_output, stage_manifest};
use astrs_wire::DataflowStatus;

/// The ceiling on any one graph in this suite.
///
/// Generous: these are real process graphs on a shared machine, and the tests
/// assert on *outcomes*, never on how long they took. A run that needs this
/// much has failed at something the assertions below will name.
const RUN_TIMEOUT: Duration = Duration::from_secs(90);

/// Runs a staged fixture through `astrs run`, returning the report and the
/// terminal output it produced.
fn run_fixture(fixture: &Fixture) -> (RunReport, String) {
    let mut args = RunArgs::new(&fixture.manifest);
    // The suite builds nothing: cargo already did, and `stage_manifest` has
    // rewritten every `path:` to the binary it produced.
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

/// Asserts a run finished cleanly, with every node exiting successfully.
fn assert_clean(report: &RunReport, text: &str, nodes: usize) {
    assert_eq!(report.exit_code(), EXIT_OK, "non-zero exit:\n{text}");
    assert_eq!(
        report.result.status,
        DataflowStatus::Finished,
        "the dataflow did not finish:\n{text}"
    );
    assert!(!report.result.has_failures(), "a node failed:\n{text}");
    assert_eq!(
        report.result.node_results.len(),
        nodes,
        "unexpected node count:\n{text}"
    );
}

/// A per-run temporary file a node writes its result to.
fn result_file(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("astrs-m1-{}-{name}", std::process::id()))
}

/// The smallest dataflow there is: one node, driven by a virtual timer source
/// (§8.4), logging through the node API and finishing on its own.
#[test]
fn hello_timer_runs_and_exits_zero() {
    let ticks_path = result_file("hello-timer.txt");
    let _ = std::fs::remove_file(&ticks_path);
    let env = BTreeMap::from([(
        "HELLO_TIMER_RESULT".to_owned(),
        ticks_path.display().to_string(),
    )]);
    let fixture =
        stage_manifest(&example_manifest("hello-timer"), "hello-timer", &env).expect("staged");

    let (report, text) = run_fixture(&fixture);
    assert_clean(&report, &text, 1);

    // The timer really drove the node to completion, asserted from the file
    // the node wrote.
    let ticks = std::fs::read_to_string(&ticks_path)
        .unwrap_or_else(|error| panic!("the greeter wrote no result: {error}\n{text}"));
    assert_eq!(ticks.trim(), "10", "unexpected tick count:\n{text}");

    // The node's own log records really reached the terminal `astrs run`
    // streams, node-prefixed (§13, §17).
    assert!(
        text.contains("[greeter"),
        "no node-prefixed output:\n{text}"
    );
    assert!(text.contains("tick 1 at hlc"), "no first tick:\n{text}");
    assert!(text.contains("tick 10 at hlc"), "no last tick:\n{text}");
    // The *last* record a node emits before exiting, asserted directly rather
    // than worked around. This used to be unreliable: a node's writer task
    // was abandoned when its runtime shut down, so its final records raced
    // the process exit and lost. `Node::drop` now waits for that writer.
    assert!(
        text.contains("done after 10 ticks"),
        "the greeter's last record never reached the terminal:\n{text}"
    );
    let _ = std::fs::remove_file(&ticks_path);
    fixture.clean();
}

/// The canonical §8.1 graph: `camera-sim → detector-sim → recorder-sim`, three
/// OS processes, two typed edges, `std` URNs on every port and a
/// `#[derive(AstrsMessage)]` type in the middle.
#[test]
fn the_canonical_typed_pipeline_runs_end_to_end() {
    let summary_path = result_file("pipeline-summary.json");
    let _ = std::fs::remove_file(&summary_path);
    let env = BTreeMap::from([(
        rust_pipeline::ENV_SUMMARY_PATH.to_owned(),
        summary_path.display().to_string(),
    )]);
    let fixture =
        stage_manifest(&example_manifest("rust-pipeline"), "rust-pipeline", &env).expect("staged");

    let (report, text) = run_fixture(&fixture);
    assert_clean(&report, &text, 3);

    let json = std::fs::read_to_string(&summary_path)
        .unwrap_or_else(|error| panic!("the recorder wrote no tally: {error}\n{text}"));
    let summary: rust_pipeline::PipelineSummary =
        serde_json::from_str(&json).expect("a readable tally");

    // The camera published `DEFAULT_FRAMES` and the recorder's queues are
    // declared deep enough to hold them all, so every one of them must be
    // accounted for — exactly, not as a range. This was a range while the
    // daemon let an `AllInputsClosed` notice overtake messages still queued
    // behind it, which cost the recorder the tail of its stream on roughly
    // one run in three; `NodeMailbox::try_recv` now holds that notice until
    // the queues it speaks for are empty.
    assert_eq!(
        summary.frames,
        rust_pipeline::DEFAULT_FRAMES,
        "the recorder was not given every frame: {summary:?}\n{text}"
    );
    // The detector's own input is `queue_size: 2, drop_oldest`, so *it* may
    // legitimately skip frames under load (§11.2). What must hold is that it
    // ran, produced typed detections, and never produced more than it was
    // given.
    assert!(
        summary.detections >= 1,
        "no detections: {summary:?}\n{text}"
    );
    assert!(
        summary.detections <= summary.frames,
        "more detections than frames: {summary:?}\n{text}"
    );
    assert!(
        summary.boxes >= summary.detections,
        "a detection with no boxes: {summary:?}\n{text}"
    );
    assert!(
        summary.inputs_closed,
        "the recorder was cut short rather than finishing: {summary:?}\n{text}"
    );

    // The payloads really decoded as the declared std type, not as bytes.
    assert!(
        text.contains("std/media/v1/Image[pixel=rgb8]"),
        "the recorder never decoded a frame:\n{text}"
    );
    let _ = std::fs::remove_file(&summary_path);
    fixture.clean();
}

/// §21's zero-copy verification: 4 MiB frames allocated in the producer's ring
/// and read in place by the consumer, with the §6.3 slow-start upgrade having
/// actually fired.
#[test]
fn the_zero_copy_probe_reports_an_engaged_plane() {
    let report_path = result_file("probe.json");
    let _ = std::fs::remove_file(&report_path);
    let env = BTreeMap::from([(
        shm_zero_copy_probe::ENV_REPORT_PATH.to_owned(),
        report_path.display().to_string(),
    )]);
    let fixture = stage_manifest(
        &example_manifest("shm-zero-copy-probe"),
        "shm-zero-copy-probe",
        &env,
    )
    .expect("staged");

    let (report, text) = run_fixture(&fixture);
    assert_clean(&report, &text, 2);

    let json = std::fs::read_to_string(&report_path)
        .unwrap_or_else(|error| panic!("the probe wrote no report: {error}\n{text}"));
    let probe: shm_zero_copy_probe::ProbeReport =
        serde_json::from_str(&json).expect("a readable probe report");

    assert!(
        probe.zero_copy_engaged,
        "the zero-copy plane did not engage: {probe:?}\n{text}"
    );
    assert_eq!(
        probe.frames_verified,
        shm_zero_copy_probe::DEFAULT_FRAMES,
        "not every frame arrived: {probe:?}\n{text}"
    );
    assert_eq!(
        probe.frame_bytes,
        shm_zero_copy_probe::DEFAULT_FRAME_BYTES as u64,
        "the probe did not move 4 MiB frames: {probe:?}"
    );

    // The four invariants that make "zero copy" a measurement rather than a
    // claim (see `shm_zero_copy_probe`'s crate docs).
    assert!(probe.addresses_match_layout, "{probe:?}\n{text}");
    assert!(probe.addresses_inside_mapping, "{probe:?}\n{text}");
    assert!(probe.sequences_match, "{probe:?}\n{text}");
    assert!(probe.fingerprints_match, "{probe:?}\n{text}");
    assert!(
        probe.distinct_addresses <= u64::from(probe.slot_count),
        "the payload addresses did not cycle through the ring: {probe:?}"
    );
    assert!(
        probe.bytes_moved >= probe.frames_verified * probe.frame_bytes,
        "{probe:?}"
    );
    assert!(probe.problems.is_empty(), "{probe:?}\n{text}");

    // And the producer really took the upgrade rather than falling back.
    assert!(
        text.contains("zero-copy plane engaged"),
        "the producer never reported the upgrade:\n{text}"
    );
    let _ = std::fs::remove_file(&report_path);
    fixture.clean();
}

/// §9.4: a request/response service over two ordinary edges, correlated by
/// metadata, in a graph whose cycle is legal because the pattern covers it.
#[test]
fn the_service_round_trip_answers_every_call() {
    let result_path = result_file("service.json");
    let _ = std::fs::remove_file(&result_path);
    let env = BTreeMap::from([(
        service_roundtrip::ENV_RESULT_PATH.to_owned(),
        result_path.display().to_string(),
    )]);
    let fixture = stage_manifest(
        &example_manifest("service-roundtrip"),
        "service-roundtrip",
        &env,
    )
    .expect("staged");

    let (report, text) = run_fixture(&fixture);
    assert_clean(&report, &text, 2);

    let json = std::fs::read_to_string(&result_path)
        .unwrap_or_else(|error| panic!("the client wrote no result: {error}\n{text}"));
    let result: service_roundtrip::RoundTripResult =
        serde_json::from_str(&json).expect("a readable result");

    assert!(
        result.is_clean(),
        "the round trip was not clean: {result:?}\n{text}"
    );
    assert_eq!(
        result.requests,
        service_roundtrip::DEFAULT_CALLS,
        "{result:?}\n{text}"
    );
    assert_eq!(result.responses, result.requests, "{result:?}\n{text}");
    assert_eq!(result.correct, result.requests, "{result:?}\n{text}");
    assert_eq!(result.uncorrelated, 0, "{result:?}\n{text}");
    assert!(
        text.contains("answered 8 requests"),
        "the server never reported answering:\n{text}"
    );
    let _ = std::fs::remove_file(&result_path);
    fixture.clean();
}

/// Every committed example manifest passes the graph checks `astrs validate`
/// performs — type agreement on every edge, wired pattern pairs, no illegal
/// cycle (§8, §9.4).
///
/// Run against the *committed* files rather than the staged copies: what a
/// reader validates by hand is what is asserted here.
#[test]
fn every_example_manifest_validates_through_the_cli() {
    for example in astrs_conformance::EXAMPLES {
        let path = example_manifest(example);
        let args = astrs_cli::command::validate::ValidateArgs {
            manifest_path: path.clone(),
            prove: false,
            profile: None,
            json: false,
            color: false,
        };
        let mut out: Vec<u8> = Vec::new();
        let report = astrs_cli::command::validate::run(&mut out, &args)
            .unwrap_or_else(|error| panic!("{example}: {error}"));
        let text = String::from_utf8_lossy(&out).into_owned();
        match expected_unconsumed_output(example) {
            // See the identical, identically-reasoned exception in
            // `m1_cli_process::expected_unconsumed_output` — two examples
            // whose whole point is a port with no consumer in the committed
            // manifest alone. Checked precisely (severity, node and port
            // name) rather than exempted wholesale, so a *different*
            // diagnostic on either still fails this test.
            Some((node, output)) => {
                assert_eq!(
                    report.diagnostics.len(),
                    1,
                    "{example}: expected exactly the documented unconsumed-`{output}` \
                     diagnostic: {:?}\n{text}",
                    report.diagnostics
                );
                let diagnostic = &report.diagnostics[0];
                assert_eq!(
                    diagnostic.severity,
                    astrs_cli::diagnostic::Severity::Info,
                    "{example}: {:?}",
                    report.diagnostics
                );
                assert!(
                    diagnostic.message.contains(output) && diagnostic.message.contains(node),
                    "{example}: unexpected diagnostic: {:?}",
                    report.diagnostics
                );
            }
            None => assert!(
                report.diagnostics.is_empty(),
                "{example} is not clean: {:?}\n{text}",
                report.diagnostics
            ),
        }
    }
}
