//! **Milestone M1**, proved through the program an adopter installs: the
//! `astrs` binary, spawned as a child process (blueprint §21, §17).
//!
//! `m1_single_machine.rs` calls the run verb in process. That proves the
//! dataflow machinery. It does not prove the thing a reader actually does,
//! which is type a command. Between a shell prompt and that function sit
//! clap's parsing, `main`'s exit-code mapping, and the fact that nodes are
//! spawned by an installed program rather than by a test binary that happens
//! to link the same crate. Everything here goes through the binary.
//!
//! | Test | M1 evidence |
//! |---|---|
//! | `the_committed_hello_timer_manifest_runs_verbatim` | the command in the README, on the committed file, with its committed `path:` |
//! | `the_astrs_binary_runs_the_canonical_typed_pipeline` | §8.1's shape driven from a shell: three processes, two typed edges |
//! | `the_astrs_binary_reports_an_engaged_zero_copy_plane` | §21's zero-copy verification, read from the probe's own stdout and JSON |
//! | `the_zero_copy_plane_engages_at_a_second_frame_size` | the plane is not a fixture of one size: 1 MiB frames engage too |
//! | `the_astrs_binary_answers_every_service_call` | §9.4 request/response correlation, end to end |
//! | `the_run_report_is_machine_readable` | `--json`: the report a CI job parses |
//! | `a_missing_node_binary_fails_with_a_readable_diagnostic` | the negative control — a broken graph *reports*, and exits non-zero |
//! | `the_canonical_typed_pipeline_is_repeatable` | the tally is the same twice running |
//! | `astrs_validate_accepts_every_committed_example` | §17's `validate` verb on all four committed manifests |
//! | `astrs_graph_renders_the_canonical_typed_pipeline` | §17's `graph` verb: the typed edges appear in the rendering |
//!
//! # Preconditions
//!
//! The suite compiles nothing. Build the binaries first:
//!
//! ```bash
//! cargo build -p astrs-cli -p hello-timer -p rust-pipeline \
//!             -p service-roundtrip -p shm-zero-copy-probe
//! ```
//!
//! A missing one fails here with that exact line rather than skipping.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use astrs_conformance::{
    AstrsCli, CliOutcome, CliRunReport, CliValidateReport, EXAMPLES, Fixture, StageOptions,
    committed_node_binaries, example_manifest, example_manifest_relative,
    expected_unconsumed_output, make_scratch_dir, result_file, stage_example, workspace_root,
};

/// The ceiling on any one graph here, in seconds.
///
/// Passed to `astrs run --timeout` as well as enforced by the harness, so a
/// wedged graph is stopped by the program under test first and killed by the
/// suite only if that fails too.
const RUN_TIMEOUT_SECS: u64 = 90;

/// The harness ceiling, comfortably above the graph's own.
const HARNESS_TIMEOUT: Duration = Duration::from_secs(120);

/// The built `astrs` binary, or a failure naming how to build it.
fn cli() -> AstrsCli {
    AstrsCli::discover()
        .unwrap_or_else(|error| panic!("{error}"))
        .timeout(HARNESS_TIMEOUT)
}

/// Runs a staged fixture through the `astrs` binary.
///
/// `--working-dir` and `--runtime-dir` both point at the fixture's own
/// directory: the graph is then entirely inside one temporary directory, so
/// two of these can run at once without sharing a daemon socket.
fn run_fixture(fixture: &Fixture, extra: &[&str]) -> CliOutcome {
    let manifest = fixture.manifest.display().to_string();
    let dir = fixture.dir.display().to_string();
    let timeout = RUN_TIMEOUT_SECS.to_string();
    let mut args = vec![
        "run",
        "--color",
        "never",
        "--skip-build",
        "--working-dir",
        &dir,
        "--runtime-dir",
        &dir,
        "--timeout",
        &timeout,
        "--grace",
        "1",
    ];
    args.extend_from_slice(extra);
    args.push(&manifest);
    cli().run(&args).unwrap_or_else(|error| panic!("{error}"))
}

/// Asserts a run finished cleanly, printing the whole invocation if not.
fn assert_clean(outcome: &CliOutcome, nodes: &[&str]) {
    assert!(outcome.succeeded(), "{}", outcome.describe());
    assert!(
        outcome.says(&format!("finished ({} node(s), 0 failed)", nodes.len())),
        "{}",
        outcome.describe()
    );
    for node in nodes {
        assert!(
            outcome.says(&format!("[{node}")),
            "no output from `{node}`:\n{}",
            outcome.describe()
        );
    }
}

/// Reads a JSON artefact a node wrote, or explains what the run printed.
fn read_artefact<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
    outcome: &CliOutcome,
) -> T {
    let json = std::fs::read_to_string(path).unwrap_or_else(|error| {
        panic!(
            "nothing was written to {}: {error}\n{}",
            path.display(),
            outcome.describe()
        )
    });
    serde_json::from_str(&json).unwrap_or_else(|error| {
        panic!(
            "{} is not readable: {error}\n{json}\n{}",
            path.display(),
            outcome.describe()
        )
    })
}

/// The committed manifest, run exactly as the README prints it — same relative
/// manifest path, same committed `path:` entries, no staging at all.
///
/// This is the only test that proves the file a reader copies actually works.
/// Every other test here rewrites `path:` to an absolute location first, which
/// is necessary for isolation and would happily keep passing if the committed
/// relative path had rotted.
#[test]
fn the_committed_hello_timer_manifest_runs_verbatim() {
    // The committed path is `../../target/debug/hello-timer`. If it is not
    // there, say so as a build precondition rather than letting the daemon
    // report a spawn failure that looks like a product bug.
    for binary in committed_node_binaries("hello-timer").expect("the manifest parses") {
        assert!(
            binary.resolved.is_file(),
            "the committed path `{}` for node `{}` resolves to {}, which does not exist.\n\
             run: cargo build -p hello-timer\n\
             (this test runs the manifest verbatim, so it needs the default target directory)",
            binary.declared,
            binary.node,
            binary.resolved.display()
        );
    }

    // A short name, because the daemon's node socket lives in here and a Unix
    // socket path may be 103 bytes (see `stage::scratch_dir`).
    let runtime = make_scratch_dir().expect("a runtime directory");
    let manifest = example_manifest_relative("hello-timer");
    let outcome = cli()
        .current_dir(workspace_root())
        .run(&[
            "run",
            "--color",
            "never",
            "--skip-build",
            "--runtime-dir",
            &runtime.display().to_string(),
            "--timeout",
            &RUN_TIMEOUT_SECS.to_string(),
            &manifest.display().to_string(),
        ])
        .unwrap_or_else(|error| panic!("{error}"));

    assert_clean(&outcome, &["greeter"]);
    assert!(outcome.says("tick 1 at hlc"), "{}", outcome.describe());
    assert!(outcome.says("tick 10 at hlc"), "{}", outcome.describe());
    assert!(
        outcome.says("done after 10 ticks"),
        "{}",
        outcome.describe()
    );
    let _ = std::fs::remove_dir_all(&runtime);
}

/// §8.1's canonical typed graph, driven from a shell: three OS processes, two
/// typed edges, a tally on disk.
#[test]
fn the_astrs_binary_runs_the_canonical_typed_pipeline() {
    let summary_path = result_file("cli-pipeline.json");
    let _ = std::fs::remove_file(&summary_path);
    let options = StageOptions::new("rust-pipeline")
        .with_env_path(rust_pipeline::ENV_SUMMARY_PATH, &summary_path);
    let fixture = stage_example("rust-pipeline", &options).expect("staged");

    let outcome = run_fixture(&fixture, &[]);
    assert_clean(&outcome, &["camera-sim", "detector-sim", "recorder-sim"]);

    let summary: rust_pipeline::PipelineSummary = read_artefact(&summary_path, &outcome);
    assert_eq!(
        summary.frames,
        rust_pipeline::DEFAULT_FRAMES,
        "the recorder was not given every frame: {summary:?}\n{}",
        outcome.describe()
    );
    assert!(summary.detections >= 1, "{summary:?}");
    assert!(summary.detections <= summary.frames, "{summary:?}");
    assert!(summary.boxes >= summary.detections, "{summary:?}");
    assert!(summary.inputs_closed, "{summary:?}");

    // The payload really decoded as the declared `std` type rather than as
    // bytes: the recorder prints the URN it read.
    assert!(
        outcome.says("std/media/v1/Image[pixel=rgb8]"),
        "{}",
        outcome.describe()
    );

    let _ = std::fs::remove_file(&summary_path);
    fixture.clean();
}

/// The same graph twice: the tally is a function of the manifest, not of the
/// machine's mood.
#[test]
fn the_canonical_typed_pipeline_is_repeatable() {
    let mut tallies = Vec::new();
    for round in 0..2 {
        let summary_path = result_file(&format!("cli-repeat-{round}.json"));
        let _ = std::fs::remove_file(&summary_path);
        let options = StageOptions::new("rust-pipeline")
            .with_env_path(rust_pipeline::ENV_SUMMARY_PATH, &summary_path);
        let fixture = stage_example("rust-pipeline", &options).expect("staged");

        let outcome = run_fixture(&fixture, &[]);
        assert_clean(&outcome, &["camera-sim", "detector-sim", "recorder-sim"]);
        let summary: rust_pipeline::PipelineSummary = read_artefact(&summary_path, &outcome);
        tallies.push(summary);

        let _ = std::fs::remove_file(&summary_path);
        fixture.clean();
    }

    // The frame count and the closure flag are contractual. The detector's
    // own input is `drop_oldest`, so its *detection* count is allowed to
    // differ between runs (§11.2) — asserting it were equal would be
    // asserting the scheduler is deterministic, which the manifest does not
    // claim.
    assert_eq!(tallies[0].frames, tallies[1].frames, "{tallies:?}");
    assert_eq!(tallies[0].frames, rust_pipeline::DEFAULT_FRAMES);
    assert!(
        tallies.iter().all(|tally| tally.inputs_closed),
        "{tallies:?}"
    );
}

/// §21's zero-copy verification, read out of the probe's own stdout and its
/// JSON report.
#[test]
fn the_astrs_binary_reports_an_engaged_zero_copy_plane() {
    let report_path = result_file("cli-probe.json");
    let _ = std::fs::remove_file(&report_path);
    let options = StageOptions::new("shm-zero-copy-probe")
        .with_env_path(shm_zero_copy_probe::ENV_REPORT_PATH, &report_path);
    let fixture = stage_example("shm-zero-copy-probe", &options).expect("staged");

    let outcome = run_fixture(&fixture, &[]);
    assert_clean(&outcome, &["producer", "consumer"]);

    let probe: shm_zero_copy_probe::ProbeReport = read_artefact(&report_path, &outcome);
    assert_zero_copy(
        &probe,
        shm_zero_copy_probe::DEFAULT_FRAME_BYTES as u64,
        &outcome,
    );
    assert_eq!(
        probe.frames_verified,
        shm_zero_copy_probe::DEFAULT_FRAMES,
        "{probe:?}"
    );

    // The producer announced the §6.3 upgrade, and the consumer printed the
    // stats table. Both are what the milestone report quotes.
    assert!(
        outcome.says("zero-copy plane engaged"),
        "{}",
        outcome.describe()
    );
    assert!(outcome.says("zero-copy: ENGAGED"), "{}", outcome.describe());
    assert!(outcome.says("addresses:"), "{}", outcome.describe());
    assert!(outcome.says("identity:"), "{}", outcome.describe());
    assert!(outcome.says("delivery:"), "{}", outcome.describe());

    let _ = std::fs::remove_file(&report_path);
    fixture.clean();
}

/// The plane is a property of the transport, not a fixture of one frame size:
/// 1 MiB frames engage it too, and the addresses still cycle through the ring.
#[test]
fn the_zero_copy_plane_engages_at_a_second_frame_size() {
    const SMALL_FRAME_BYTES: u64 = 1024 * 1024;
    const SMALL_FRAMES: u64 = 32;

    let report_path = result_file("cli-probe-small.json");
    let _ = std::fs::remove_file(&report_path);
    let options = StageOptions::new("shm-zero-copy-probe")
        .with_env_path(shm_zero_copy_probe::ENV_REPORT_PATH, &report_path)
        .with_env(
            shm_zero_copy_probe::ENV_FRAME_BYTES,
            SMALL_FRAME_BYTES.to_string(),
        )
        .with_env(shm_zero_copy_probe::ENV_FRAMES, SMALL_FRAMES.to_string());
    let fixture = stage_example("shm-zero-copy-probe", &options).expect("staged");

    let outcome = run_fixture(&fixture, &[]);
    assert_clean(&outcome, &["producer", "consumer"]);

    let probe: shm_zero_copy_probe::ProbeReport = read_artefact(&report_path, &outcome);
    assert_zero_copy(&probe, SMALL_FRAME_BYTES, &outcome);
    assert_eq!(probe.frames_verified, SMALL_FRAMES, "{probe:?}");
    // More frames than slots, so the ring must have wrapped: the addresses
    // cannot be all-distinct unless something handed out fresh buffers.
    assert!(
        SMALL_FRAMES > u64::from(probe.slot_count),
        "the sweep no longer forces a wrap: {probe:?}"
    );

    let _ = std::fs::remove_file(&report_path);
    fixture.clean();
}

/// Every invariant that makes "zero copy" a measurement rather than a claim.
fn assert_zero_copy(
    probe: &shm_zero_copy_probe::ProbeReport,
    frame_bytes: u64,
    outcome: &CliOutcome,
) {
    assert!(
        probe.zero_copy_engaged,
        "the zero-copy plane did not engage: {probe:?}\n{}",
        outcome.describe()
    );
    assert_eq!(probe.frame_bytes, frame_bytes, "{probe:?}");
    assert!(probe.addresses_match_layout, "{probe:?}");
    assert!(probe.addresses_inside_mapping, "{probe:?}");
    assert!(probe.sequences_match, "{probe:?}");
    assert!(probe.fingerprints_match, "{probe:?}");
    assert!(
        probe.distinct_addresses <= u64::from(probe.slot_count),
        "the payload addresses did not cycle through the ring: {probe:?}"
    );
    assert!(
        probe.bytes_moved >= probe.frames_verified * probe.frame_bytes,
        "{probe:?}"
    );
    assert!(probe.problems.is_empty(), "{probe:?}");
    // The latency figures are the §21 "latency budget" half of the probe;
    // a zero mean would mean the clock never moved, which would make the
    // throughput figure meaningless too.
    assert!(probe.mean_delivery_us > 0.0, "{probe:?}");
    assert!(probe.max_delivery_us >= probe.mean_delivery_us, "{probe:?}");
    assert!(probe.throughput_mib_s > 0.0, "{probe:?}");
}

/// §9.4: request/response over ordinary edges, correlated by metadata.
#[test]
fn the_astrs_binary_answers_every_service_call() {
    let result_path = result_file("cli-service.json");
    let _ = std::fs::remove_file(&result_path);
    let options = StageOptions::new("service-roundtrip")
        .with_env_path(service_roundtrip::ENV_RESULT_PATH, &result_path);
    let fixture = stage_example("service-roundtrip", &options).expect("staged");

    let outcome = run_fixture(&fixture, &[]);
    assert_clean(&outcome, &["client", "server"]);

    let result: service_roundtrip::RoundTripResult = read_artefact(&result_path, &outcome);
    assert!(
        result.is_clean(),
        "the round trip was not clean: {result:?}\n{}",
        outcome.describe()
    );
    assert_eq!(result.requests, service_roundtrip::DEFAULT_CALLS);
    assert_eq!(result.responses, result.requests);
    assert_eq!(result.correct, result.requests);
    assert_eq!(result.uncorrelated, 0);
    assert!(
        outcome.says(&format!(
            "answered {} requests",
            service_roundtrip::DEFAULT_CALLS
        )),
        "{}",
        outcome.describe()
    );

    let _ = std::fs::remove_file(&result_path);
    fixture.clean();
}

/// `--json`: the report a CI job parses, rather than the text a human reads.
#[test]
fn the_run_report_is_machine_readable() {
    let options = StageOptions::new("hello-timer").with_env("HELLO_TIMER_TICKS", "3");
    let fixture = stage_example("hello-timer", &options).expect("staged");

    let outcome = run_fixture(&fixture, &["--json"]);
    assert!(outcome.succeeded(), "{}", outcome.describe());

    let report = CliRunReport::parse(&outcome.command, &outcome.stdout)
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(report.is_clean(), "{report:?}\n{}", outcome.describe());
    assert_eq!(report.status, "finished");
    assert_eq!(report.exit_code, 0);
    assert_eq!(report.nodes.len(), 1);
    let greeter = report
        .nodes
        .get("greeter")
        .expect("the greeter is reported");
    assert!(!greeter.failed, "{greeter:?}");
    assert!(!report.dataflow.is_empty());
    // The streamed lines really went through the CLI's own printer.
    assert!(report.printed_lines > 0, "{report:?}");

    fixture.clean();
}

/// The negative control. A conformance suite that can only observe success has
/// not shown that its assertions can fail: point one node at a binary that is
/// not there and the run must *report* it, name it, and exit non-zero.
#[test]
fn a_missing_node_binary_fails_with_a_readable_diagnostic() {
    let missing = std::env::temp_dir().join("astrs-conformance-no-such-node");
    let _ = std::fs::remove_file(&missing);
    let options = StageOptions::new("hello-timer")
        .with_node_path("greeter", missing.display().to_string())
        .with_env("HELLO_TIMER_TICKS", "2");
    let fixture = stage_example("hello-timer", &options).expect("staged");

    let outcome = run_fixture(&fixture, &["--json"]);
    assert!(
        !outcome.succeeded(),
        "a graph with a missing binary exited zero:\n{}",
        outcome.describe()
    );

    let report = CliRunReport::parse(&outcome.command, &outcome.stdout)
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(report.failed, "{report:?}\n{}", outcome.describe());
    assert_eq!(report.status, "failed", "{report:?}");
    assert_ne!(report.exit_code, 0, "{report:?}");
    assert_eq!(report.failed_nodes(), vec!["greeter"], "{report:?}");

    // And the human-readable form names both the node and the path, which is
    // the difference between a diagnosable failure and an exit code.
    assert!(outcome.says("greeter"), "{}", outcome.describe());
    assert!(
        outcome.says(&missing.display().to_string()),
        "{}",
        outcome.describe()
    );

    fixture.clean();
}

/// §17's `validate` verb over every committed example manifest — the
/// *committed* files, not the staged copies, because what a reader
/// validates by hand is what is asserted here.
#[test]
fn astrs_validate_accepts_every_committed_example() {
    for example in EXAMPLES {
        let manifest = example_manifest(example);
        let outcome = cli()
            .timeout(Duration::from_secs(60))
            .run(&[
                "validate",
                "--json",
                "--color",
                "never",
                &manifest.display().to_string(),
            ])
            .unwrap_or_else(|error| panic!("{example}: {error}"));

        let report = CliValidateReport::parse(&outcome.command, &outcome.stdout)
            .unwrap_or_else(|error| panic!("{example}: {error}"));
        match expected_unconsumed_output(example) {
            Some((node, output)) => {
                assert_eq!(
                    report.diagnostics.len(),
                    1,
                    "{example}: expected exactly the documented unconsumed-`{output}` \
                     diagnostic:\n{}\n{}",
                    report.rendered(),
                    outcome.describe()
                );
                let diagnostic = &report.diagnostics[0];
                assert_eq!(
                    diagnostic.severity,
                    "info",
                    "{example}: {}",
                    report.rendered()
                );
                assert!(
                    diagnostic.message.contains(output) && diagnostic.message.contains(node),
                    "{example}: unexpected diagnostic: {}",
                    report.rendered()
                );
            }
            None => assert!(
                report.is_clean(),
                "{example} is not clean:\n{}\n{}",
                report.rendered(),
                outcome.describe()
            ),
        }
        assert!(outcome.succeeded(), "{example}: {}", outcome.describe());
    }
}

/// §17's `graph` verb: the typed edges the manifest declares are the edges the
/// rendering shows, which is what makes the picture worth trusting.
#[test]
fn astrs_graph_renders_the_canonical_typed_pipeline() {
    let manifest = example_manifest("rust-pipeline");
    let outcome = cli()
        .timeout(Duration::from_secs(60))
        .run(&[
            "graph",
            "--format",
            "dot",
            "--color",
            "never",
            &manifest.display().to_string(),
        ])
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(outcome.succeeded(), "{}", outcome.describe());

    for expected in [
        "digraph astrs",
        "camera-sim",
        "detector-sim",
        "recorder-sim",
        // The virtual timer source is drawn as a source, not hidden.
        "astrs/timer/hz/20",
        // Both typed edges carry their URN into the rendering.
        "frames: std/media/v1/Image[pixel=rgb8]",
        "detections: std/vision/v1/Detections",
    ] {
        assert!(
            outcome.stdout.contains(expected),
            "the rendering is missing `{expected}`:\n{}",
            outcome.describe()
        );
    }

    // Mermaid is the default format, and must render the same graph.
    let mermaid = cli()
        .timeout(Duration::from_secs(60))
        .run(&["graph", "--color", "never", &manifest.display().to_string()])
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(mermaid.succeeded(), "{}", mermaid.describe());
    assert!(
        mermaid.stdout.contains("camera-sim"),
        "{}",
        mermaid.describe()
    );
    assert!(
        mermaid.stdout.contains("detector-sim"),
        "{}",
        mermaid.describe()
    );
}
