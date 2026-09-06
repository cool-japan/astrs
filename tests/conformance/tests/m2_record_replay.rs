//! **Milestone M2**, half two (blueprint §21): record → replay round trip.
//!
//! §14 promises that a recording replays *byte-for-byte*:
//!
//! > Replay: `astrs replay session.arec --speed 1.0 --into graph.yml`
//! > re-injects recorded outputs as sources — replacing any subset of nodes
//! > (test a new planner against last week's sensor data byte-for-byte).
//!
//! This file turns that sentence into an assertion. It runs the committed
//! `examples/record-replay` graph for real, reads the `.arec` it wrote,
//! rewrites the committed `replay.yml` through the **real `astrs replay`
//! verb**, runs the result, and compares three byte sequences that must all
//! be the same list in the same order:
//!
//! ```text
//!   record_replay::frame_payload(0..12)      the sensor's own generator
//!            ║
//!   .arec entries for sensor/frames          what the live run carried
//!            ║
//!   probe's PayloadSequence.payloads         what the replay delivered
//! ```
//!
//! | Test | M2 evidence |
//! |---|---|
//! | `a_live_run_replays_byte_for_byte` | the three-way equality above, end to end through two real process graphs |
//! | `the_replay_verb_replaces_only_the_recorded_producer` | `--into` rewrites the source in place and leaves every other node alone |
//! | `a_recorded_session_is_a_well_formed_arec_container` | the session has a seekable footer, both ports, and the payload bytes the run published |
//!
//! # What is deliberately not compared
//!
//! Timestamps. `astrs-replay-node` re-stamps every republished message with a
//! fresh HLC from its own clock and carries the rest of the metadata through
//! unchanged, so payload bytes are reproducible and the clock is not.
//! Comparing HLCs would be a test that fails for the one reason the design
//! documents.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::time::Duration;

use astrs_cli::cli::ReplayTimingMode;
use astrs_cli::command::replay::{ReplayIntoArgs, ReplayIntoReport};
use astrs_cli::command::run::{EXIT_OK, RunArgs, RunReport, run};
use astrs_conformance::{
    Fixture, StageOptions, binary, example_dir, result_file, stage_example, stage_manifest_with,
};
use astrs_recording::{Entry, Reader};
use astrs_wire::DataflowStatus;
use record_replay::{
    DEFAULT_FRAMES, ENV_SEQUENCE_PATH, ENV_SESSION_PATH, PayloadSequence, frame_payload, hex,
};

/// The ceiling on any one graph in this file.
///
/// Generous: these are real process graphs on a shared machine, and every
/// assertion is about an outcome rather than about how long it took.
const RUN_TIMEOUT: Duration = Duration::from_secs(90);

/// The example whose committed files this file drives.
const EXAMPLE: &str = "record-replay";

/// Runs a staged fixture through `astrs run`, returning the report and the
/// terminal output it produced.
fn run_fixture(fixture: &Fixture) -> (RunReport, String) {
    let mut args = RunArgs::new(&fixture.manifest);
    // The suite builds nothing: cargo already did, and staging has rewritten
    // every `path:` to the binary it produced.
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

/// The payloads the sensor generates, hex-encoded — the sequence both other
/// sequences must equal.
fn generated_payloads() -> Vec<String> {
    (0..DEFAULT_FRAMES)
        .map(|index| hex(&frame_payload(index)))
        .collect()
}

/// Runs the committed live graph, writing its session to `session`.
fn record_live_session(session: &Path) -> (RunReport, String, Fixture) {
    let _ = std::fs::remove_file(session);
    let options = StageOptions::new(EXAMPLE).with_env_path(ENV_SESSION_PATH, session);
    let fixture = stage_example(EXAMPLE, &options).expect("the live graph stages");
    let (report, text) = run_fixture(&fixture);
    (report, text, fixture)
}

/// Every entry a finished session holds, in file order.
fn read_entries(session: &Path) -> Vec<Entry> {
    let mut reader = Reader::open(session).unwrap_or_else(|error| {
        panic!(
            "the recorder wrote no readable session at {}: {error}",
            session.display()
        )
    });
    reader
        .iter_all()
        .collect::<Result<Vec<_>, _>>()
        .expect("every entry decodes")
}

/// The hex payloads of one recorded port, in file order.
fn recorded_payloads(entries: &[Entry], node: &str, output: &str) -> Vec<String> {
    entries
        .iter()
        .filter(|entry| entry.node.as_str() == node && entry.output.as_str() == output)
        .map(|entry| hex(&entry.payload))
        .collect()
}

/// Rewrites the committed `replay.yml` through the real `astrs replay` verb.
fn rewrite_replay_manifest(session: &Path) -> ReplayIntoReport {
    let args = ReplayIntoArgs {
        input: session.to_path_buf(),
        into: example_dir(EXAMPLE).join("replay.yml"),
        // The mode the example's README prints, so what is proved here is
        // what a reader runs. Real-time paces entries by their recorded HLC
        // deltas, which for this session is one 20 ms frame period.
        mode: ReplayTimingMode::RealTime,
        speed: None,
        rate: None,
        r#loop: false,
        // Empty on purpose: the auto-detection — intersect the recording's
        // node ids with the manifest's — is the behaviour under test.
        replace: Vec::new(),
    };
    let mut out: Vec<u8> = Vec::new();
    astrs_cli::command::replay::run(&mut out, &args).expect("astrs replay --into")
}

/// The full M2 record → replay proof: two real process graphs, one `.arec`
/// between them, and three byte sequences that must agree exactly.
#[test]
fn a_live_run_replays_byte_for_byte() {
    // ---- 1. record ----------------------------------------------------
    let session = result_file("record-replay-session.arec");
    let (report, text, live) = record_live_session(&session);
    assert_clean(&report, &text, 3);
    assert!(
        text.contains("recorded 24 entries"),
        "the recorder never reported a finished session:\n{text}"
    );

    // ---- 2. what the live run carried ---------------------------------
    let entries = read_entries(&session);
    let recorded = recorded_payloads(&entries, "sensor", "frames");
    let generated = generated_payloads();
    assert_eq!(
        recorded.len(),
        DEFAULT_FRAMES as usize,
        "the session is missing frames — the live graph's edges must be lossless:\n{text}"
    );
    assert_eq!(
        recorded, generated,
        "the recording is not what the sensor published"
    );

    // ---- 3. rewrite the target graph through the real verb -------------
    let rewritten = rewrite_replay_manifest(&session);
    assert_eq!(
        rewritten.replaced,
        vec!["sensor".to_owned()],
        "only the recorded producer present in the target graph is replaced"
    );

    // ---- 4. replay ----------------------------------------------------
    let sequence_path = result_file("record-replay-sequence.json");
    let _ = std::fs::remove_file(&sequence_path);
    let source = live.dir.join("replayed.yml");
    std::fs::write(&source, &rewritten.yaml).expect("the rewritten manifest is writable");

    let replay_binary = binary("astrs-replay-node", "astrs-replay-node")
        .expect("astrs-replay-node is built")
        .display()
        .to_string();
    let options = StageOptions::new(EXAMPLE)
        // `astrs replay --into` writes a bare `path: astrs-replay-node`, a
        // `PATH` lookup, because a manifest naming a build directory would
        // not survive being deployed. The suite resolves it the same way it
        // resolves every other binary instead of mutating this process's
        // environment.
        .with_node_path("sensor", replay_binary)
        .with_env_path(ENV_SEQUENCE_PATH, &sequence_path);
    let replayed = stage_manifest_with(&source, &options).expect("the replay graph stages");

    let (report, text) = run_fixture(&replayed);
    assert_clean(&report, &text, 2);

    // ---- 5. what the replay delivered ---------------------------------
    let json = std::fs::read_to_string(&sequence_path)
        .unwrap_or_else(|error| panic!("the probe wrote no sequence: {error}\n{text}"));
    let observed: PayloadSequence = serde_json::from_str(&json).expect("a readable sequence");

    assert!(
        observed.problems.is_empty(),
        "the probe saw an out-of-order or malformed payload: {:?}\n{text}",
        observed.problems
    );
    assert!(
        observed.inputs_closed,
        "the probe was cut short rather than finishing: {observed:?}\n{text}"
    );
    assert_eq!(
        observed.count, DEFAULT_FRAMES,
        "the replay delivered a different number of payloads:\n{text}"
    );

    // The §14 promise itself: identical payloads, identical order, all three
    // sequences. Compared as whole lists — a count or a digest would let a
    // reordering through.
    assert_eq!(
        observed.payloads, recorded,
        "replay did not reproduce the recording byte for byte:\n{text}"
    );
    assert_eq!(
        observed.payloads, generated,
        "replay did not reproduce what the sensor generated:\n{text}"
    );
    assert!(observed.matches_generated(DEFAULT_FRAMES), "{observed:?}");
    assert_eq!(
        observed.total_bytes,
        DEFAULT_FRAMES * record_replay::FRAME_BYTES as u64
    );

    let _ = std::fs::remove_file(&session);
    let _ = std::fs::remove_file(&sequence_path);
    live.clean();
    replayed.clean();
}

/// `astrs replay --into` rewrites the recorded producer **in place** and
/// leaves every other node exactly as committed — which is what lets the
/// consumer's `frames: sensor/frames` keep resolving after the swap.
#[test]
fn the_replay_verb_replaces_only_the_recorded_producer() {
    let session = result_file("record-replay-rewrite.arec");
    let (report, text, live) = record_live_session(&session);
    assert_clean(&report, &text, 3);

    let rewritten = rewrite_replay_manifest(&session);
    assert_eq!(rewritten.replaced, vec!["sensor".to_owned()]);

    let sensor = rewritten
        .manifest
        .nodes
        .iter()
        .find(|node| node.id == "sensor")
        .expect("the sensor survived the rewrite");
    assert_eq!(sensor.path.as_deref(), Some("astrs-replay-node"));
    assert_eq!(
        sensor.outputs,
        vec!["frames".to_owned()],
        "the replaced node keeps its declared outputs, so nothing is rewired"
    );
    assert!(
        sensor.args.iter().any(|arg| arg == "sensor/frames"),
        "the replay node must be restricted to this node's own port: {:?}",
        sensor.args
    );
    assert!(
        sensor
            .args
            .first()
            .is_some_and(|arg| arg == &session.display().to_string()),
        "the recording is the first argument: {:?}",
        sensor.args
    );
    assert!(sensor.build.is_none(), "a replay node has nothing to build");

    // `detector` is recorded but absent from the target graph, so it is not
    // replayed: the auto-detection intersects, it does not import.
    assert!(
        !rewritten.replaced.iter().any(|id| id == "detector"),
        "a recorded node the target graph does not declare must not appear"
    );

    let probe = rewritten
        .manifest
        .nodes
        .iter()
        .find(|node| node.id == "probe")
        .expect("the probe survived the rewrite");
    assert_eq!(probe.path.as_deref(), Some("../../target/debug/arec-probe"));
    assert_eq!(
        probe.build.as_deref(),
        Some("cargo build -p record-replay"),
        "an untouched node keeps its committed `build:` line"
    );

    let _ = std::fs::remove_file(&session);
    live.clean();
}

/// The session an ordinary graph node wrote is a well-formed `.arec`: a
/// seekable footer index naming both recorded ports, and payload bytes that
/// are exactly what the graph published (§14).
#[test]
fn a_recorded_session_is_a_well_formed_arec_container() {
    let session = result_file("record-replay-container.arec");
    let (report, text, live) = record_live_session(&session);
    assert_clean(&report, &text, 3);

    // `Reader::open` reads the trailer and seeks to the footer; a file
    // without one only opens through the recovery scan, so this call
    // succeeding *is* the "seekable index footer" assertion.
    let reader = Reader::open(&session).expect("a finished, seekable session");
    let ports: std::collections::BTreeSet<String> = reader
        .index()
        .iter()
        .map(|entry| format!("{}/{}", entry.node.as_str(), entry.output.as_str()))
        .collect();
    assert_eq!(
        ports,
        std::collections::BTreeSet::from([
            "sensor/frames".to_owned(),
            "detector/detections".to_owned()
        ]),
        "entries are named by their producer port, not by the recorder's own inputs"
    );
    drop(reader);

    let entries = read_entries(&session);
    assert_eq!(entries.len(), 2 * DEFAULT_FRAMES as usize);
    assert_eq!(
        recorded_payloads(&entries, "sensor", "frames"),
        generated_payloads()
    );

    let detections = recorded_payloads(&entries, "detector", "detections");
    assert_eq!(detections.len(), DEFAULT_FRAMES as usize);
    assert_eq!(
        detections,
        (0..DEFAULT_FRAMES)
            .map(|index| hex(&record_replay::detection_payload(&frame_payload(index))))
            .collect::<Vec<_>>(),
        "the detector's own output is recorded, not recomputed"
    );

    // Every entry carries the HLC it was published with, and they are
    // non-decreasing in file order — the ordering `astrs bag info` and the
    // replay node's real-time pacing both depend on.
    let mut previous = None;
    for entry in &entries {
        let hlc = entry.hlc();
        if let Some(previous) = previous {
            assert!(hlc >= previous, "entries are not HLC-ordered: {entries:?}");
        }
        previous = Some(hlc);
    }

    let _ = std::fs::remove_file(&session);
    live.clean();
}
