//! **Milestone M3** pipeline chain (blueprint §14, §10.6, §21 M3, §20.3):
//! `astrs run` records a live graph to `.arec` → `astrs bag convert` to
//! `.db3` → `astrs bag convert` back to `.arec` → `astrs replay --into` cuts
//! a fresh graph over to the round-tripped recording and runs it.
//!
//! Four real hops, four commands, one content check that must survive all of
//! them:
//!
//! ```text
//!   record_replay::frame_payload(0..12)   the sensor's own generator
//!            ║ hop 1: astrs run (record: sugar)
//!   session.arec                          what the live graph carried
//!            ║ hop 2: astrs bag convert session.arec session.db3
//!   session.db3                           rosbag2, sidecar-carrying custom_data
//!            ║ hop 3: astrs bag convert session.db3 roundtrip.arec
//!   roundtrip.arec                        node/output/dataflow id recovered from the sidecar
//!            ║ hop 4: astrs replay --into replay.yml, then astrs run
//!   probe's PayloadSequence.payloads      what the replayed graph delivered
//! ```
//!
//! Each hop gets its own assertion rather than one equality at the end, so a
//! regression in the middle of the chain (a lossy `.db3` conversion, a
//! sidecar that failed to round-trip the node/output naming) is attributed to
//! the hop that caused it instead of surfacing as "the last assertion
//! failed".
//!
//! This is the two-command chain the M3 audit found untested — the
//! individual `.arec ⇄ .db3` round-trip is already proven at the unit level
//! by `astrs-rosbag`'s own
//! `arec_to_db3_to_arec_preserves_dataflow_epoch_and_every_entry`
//! (`crates/astrs-rosbag/src/convert/arec_to_db3.rs`); what had no coverage
//! anywhere was driving the same two conversions through the real `astrs
//! bag convert` CLI verb, between a **real recorded graph run** and a
//! **real live replay** — the shape an operator actually uses (§17: `astrs
//! bag convert in.arec out.db3` and reverse).
//!
//! # What is deliberately not compared
//!
//! Timestamps, exactly as `m2_record_replay.rs` documents: `astrs-replay-node`
//! re-stamps every republished message with a fresh HLC, so only payload
//! bytes (and, at the `.arec` hops, the dataflow id) are asserted equal.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::time::Duration;

use astrs_cli::cli::ReplayTimingMode;
use astrs_cli::command::bag::{self, ConvertArgs};
use astrs_cli::command::replay::{ReplayIntoArgs, ReplayIntoReport};
use astrs_cli::command::run::{EXIT_OK, RunArgs, RunReport, run};
use astrs_conformance::{
    Fixture, StageOptions, binary, example_dir, result_file, stage_example, stage_manifest_with,
};
use astrs_recording::{Entry, Reader as ArecReader};
use astrs_rosbag::db3::Reader as Db3Reader;
use astrs_wire::DataflowStatus;
use record_replay::{
    DEFAULT_FRAMES, ENV_SEQUENCE_PATH, ENV_SESSION_PATH, PayloadSequence, frame_payload, hex,
};

/// The ceiling on any one graph in this file.
///
/// Generous: these are real process graphs on a shared machine, and every
/// assertion is about an outcome rather than about how long it took.
const RUN_TIMEOUT: Duration = Duration::from_secs(90);

/// The example whose committed files this file drives — the same
/// `record-replay` graph `m2_record_replay.rs` uses for hops 1 and 4, so the
/// only new ground here is the `.db3` detour in between.
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

/// The payloads the sensor generates, hex-encoded — the sequence every hop
/// below must still agree with once it has passed through the whole chain.
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

/// Every entry a finished `.arec` session holds, in file order.
fn read_entries(session: &Path) -> Vec<Entry> {
    let mut reader = ArecReader::open(session)
        .unwrap_or_else(|error| panic!("no readable recording at {}: {error}", session.display()));
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

/// Rewrites the committed `replay.yml` through the real `astrs replay` verb,
/// against whichever `.arec` `session` names — the original recording for
/// `m2_record_replay.rs`, the round-tripped one here.
fn rewrite_replay_manifest(session: &Path) -> ReplayIntoReport {
    let args = ReplayIntoArgs {
        input: session.to_path_buf(),
        into: example_dir(EXAMPLE).join("replay.yml"),
        mode: ReplayTimingMode::RealTime,
        speed: None,
        rate: None,
        r#loop: false,
        replace: Vec::new(),
    };
    let mut out: Vec<u8> = Vec::new();
    astrs_cli::command::replay::run(&mut out, &args).expect("astrs replay --into")
}

/// Converts `input` to `output` through the real `astrs bag convert` verb,
/// panicking with the paths on failure so a chain break names its own hop.
fn bag_convert(input: &Path, output: &Path) -> astrs_rosbag::convert::ConversionReport {
    let _ = std::fs::remove_file(output);
    let mut out: Vec<u8> = Vec::new();
    bag::convert(
        &mut out,
        &ConvertArgs {
            input: input.to_path_buf(),
            output: output.to_path_buf(),
            json: false,
        },
    )
    .unwrap_or_else(|error| {
        panic!(
            "astrs bag convert {} {}: {error}",
            input.display(),
            output.display()
        )
    })
}

/// The full M3 chain: record a live run, convert it to `.db3` and back, then
/// replay the round-tripped recording into a fresh live dataflow — content
/// preserved at every hop.
#[test]
fn a_recording_survives_a_db3_round_trip_and_replays_byte_for_byte() {
    let generated = generated_payloads();

    // ---- hop 1: astrs run, recording to .arec --------------------------
    let session = result_file("m3-pipeline-session.arec");
    let (report, text, live) = record_live_session(&session);
    assert_clean(&report, &text, 3);
    assert!(
        text.contains("recorded 24 entries"),
        "the recorder never reported a finished session:\n{text}"
    );

    let original_entries = read_entries(&session);
    assert_eq!(
        original_entries.len(),
        2 * DEFAULT_FRAMES as usize,
        "sensor/frames + detector/detections, {} frames each",
        DEFAULT_FRAMES
    );
    let recorded = recorded_payloads(&original_entries, "sensor", "frames");
    assert_eq!(
        recorded, generated,
        "hop 1 (astrs run, recording) did not carry what the sensor published"
    );

    // ---- hop 2: astrs bag convert session.arec session.db3 -------------
    let db3_path = result_file("m3-pipeline-session.db3");
    let to_db3 = bag_convert(&session, &db3_path);
    assert_eq!(to_db3.direction, "arec -> db3");
    assert_eq!(
        to_db3.messages,
        original_entries.len() as u64,
        "hop 2 (bag convert to .db3) dropped or duplicated messages"
    );
    assert_eq!(
        to_db3.topics, 2,
        "hop 2 (bag convert to .db3) must keep sensor/frames and \
         detector/detections as two distinct topics"
    );

    // Read the .db3 back independently of the conversion report, so this
    // assertion cannot pass merely because the report lied about itself.
    let db3_reader = Db3Reader::open(&db3_path).expect("the .db3 opens");
    let sensor_topic_id = db3_reader
        .topics()
        .iter()
        .find(|(_, record)| record.topic.ends_with("sensor/frames"))
        .map(|(id, _)| *id)
        .unwrap_or_else(|| {
            panic!(
                "no topic named .../sensor/frames among {:?}",
                db3_reader
                    .topics()
                    .iter()
                    .map(|(_, record)| record.topic.clone())
                    .collect::<Vec<_>>()
            )
        });
    let db3_sensor_payloads: Vec<String> = db3_reader
        .iter_messages()
        .collect::<Result<Vec<_>, _>>()
        .expect("every .db3 row decodes")
        .into_iter()
        .filter(|message| message.topic_id == sensor_topic_id)
        .map(|message| hex(&message.data))
        .collect();
    assert_eq!(
        db3_sensor_payloads, generated,
        "hop 2 (bag convert to .db3) changed sensor/frames' payload bytes"
    );

    // ---- hop 3: astrs bag convert session.db3 roundtrip.arec -----------
    let roundtrip_path = result_file("m3-pipeline-roundtrip.arec");
    let back = bag_convert(&db3_path, &roundtrip_path);
    assert_eq!(back.direction, "db3 -> arec");
    assert_eq!(
        back.messages,
        original_entries.len() as u64,
        "hop 3 (bag convert back to .arec) dropped or duplicated messages"
    );

    let roundtrip_entries = read_entries(&roundtrip_path);
    assert_eq!(roundtrip_entries.len(), original_entries.len());
    let roundtrip_recorded = recorded_payloads(&roundtrip_entries, "sensor", "frames");
    assert_eq!(
        roundtrip_recorded, generated,
        "hop 3 (bag convert back to .arec) changed sensor/frames' payload bytes"
    );

    // The `.db3`'s embedded sidecar (blueprint §10.6) is what lets this hop
    // recover the *original* node/output/dataflow identity instead of
    // synthesizing fresh ones from topic names — assert that identity
    // actually came back, not just that *some* node published *some* bytes.
    let (original_reader, _) =
        ArecReader::open_or_recover(&session).expect("the original .arec opens");
    let (roundtrip_reader, _) =
        ArecReader::open_or_recover(&roundtrip_path).expect("the round-tripped .arec opens");
    assert_eq!(
        roundtrip_reader.header().dataflow,
        original_reader.header().dataflow,
        "the round trip must recover the original dataflow id from the .db3 sidecar, \
         not synthesize a fresh one"
    );

    // ---- hop 4: astrs replay --into, then astrs run ---------------------
    // The load-bearing check: the *round-tripped* recording — not the
    // original — is what gets replayed, so this hop can only pass if hops
    // 2 and 3 preserved everything `astrs replay --into` depends on (the
    // node id `sensor` and its `frames` output).
    let rewritten = rewrite_replay_manifest(&roundtrip_path);
    assert_eq!(
        rewritten.replaced,
        vec!["sensor".to_owned()],
        "the round-tripped recording must still name `sensor` as a replayable producer"
    );

    let sequence_path = result_file("m3-pipeline-sequence.json");
    let _ = std::fs::remove_file(&sequence_path);
    let source = live.dir.join("replayed.yml");
    std::fs::write(&source, &rewritten.yaml).expect("the rewritten manifest is writable");

    let replay_binary = binary("astrs-replay-node", "astrs-replay-node")
        .expect("astrs-replay-node is built")
        .display()
        .to_string();
    let options = StageOptions::new(EXAMPLE)
        .with_node_path("sensor", replay_binary)
        .with_env_path(ENV_SEQUENCE_PATH, &sequence_path);
    let replayed = stage_manifest_with(&source, &options).expect("the replay graph stages");

    let (report, text) = run_fixture(&replayed);
    assert_clean(&report, &text, 2);

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
        "hop 4 (replay) delivered a different number of payloads:\n{text}"
    );

    // The chain's whole point: what the original sensor generated is what a
    // live dataflow receives after two bag conversions and a replay.
    assert_eq!(
        observed.payloads, generated,
        "the full chain (record -> db3 -> arec -> replay) did not reproduce \
         the original content byte for byte:\n{text}"
    );
    assert!(observed.matches_generated(DEFAULT_FRAMES), "{observed:?}");

    let _ = std::fs::remove_file(&session);
    let _ = std::fs::remove_file(&db3_path);
    let _ = std::fs::remove_file(&roundtrip_path);
    let _ = std::fs::remove_file(&sequence_path);
    live.clean();
    replayed.clean();
}
