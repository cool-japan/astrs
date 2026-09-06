//! **Milestone M3** (blueprint §21): `astrs run --deterministic` is
//! reproducible to the message.
//!
//! §14 promises one sentence, and this file is its assertion:
//!
//! > *Determinism mode: `astrs run --deterministic` fixes the timer wheel to
//! > the recording's HLC stream and delivers inputs in recorded order, making
//! > single-host replays reproducible to the message level.*
//!
//! Three real process graphs, one recording:
//!
//! ```text
//!   1. record   astrs/timer/millis/20 ─► [sensor] ─frames─► [recorder] ─► source.arec
//!
//!   2. replay   source.arec ═╗ (the daemon's clock AND its frames producer)
//!                            ╠═► [sensor := stood down] ─frames─► [frames-tap] ─► frames-A.arec
//!                            ╚═► astrs/timer/millis/20 ──────────► [tick-tap]   ─► ticks-A.arec
//!
//!   3. replay   the same, into frames-B.arec / ticks-B.arec
//! ```
//!
//! and then the comparison that is the point: **A and B are the same list of
//! entries, in the same order, with the same HLC stamps** — and the frames
//! half also equals what the *original* run recorded, timestamps included.
//!
//! | Test | M3 evidence |
//! |---|---|
//! | `two_deterministic_replays_produce_identical_message_logs` | the acceptance bar: two runs, byte-identical ordered logs |
//! | `a_deterministic_replay_preserves_the_recorded_timestamps` | the daemon replays entries, so stamps survive — what a replay *node* cannot do |
//! | `timer_ticks_land_only_on_recorded_hlc_points` | the wheel really is fixed to the recording's stream |
//! | `pacing_changes_how_long_a_run_takes_and_nothing_else` | `--speed` is pacing, not semantics |
//! | `determinism_without_a_recording_is_refused` | the flag is never honoured as a label |
//!
//! # Why the artefacts are `.arec` files and not terminal output
//!
//! Everything compared here has to be a function of the recording alone.
//! Terminal output carries elapsed-time prefixes, a freshly generated
//! `DataflowId` and pids; an `.arec` *header* carries a dataflow id and an
//! epoch too, which is why the comparison decodes the entries and compares
//! those rather than hashing the files. The entries are exactly the message
//! log §14 talks about: producer, port, HLC, payload.
//!
//! # Why the taps are two nodes and not one
//!
//! A single recorder with both inputs would interleave two queues, and the
//! order it drains them in is the *node API's* merge policy, not the daemon's
//! delivery order — a different mechanism from the one under test. One node
//! per stream keeps every artefact single-sourced, so what is compared is
//! what the daemon delivered.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use astrs_cli::command::run::{EXIT_OK, RunArgs, RunReport, run};
use astrs_conformance::{binary, make_scratch_dir};
use astrs_recording::{Entry, Reader};
use astrs_time::HlcTimestamp;
use astrs_wire::DataflowStatus;
use record_replay::{DEFAULT_FRAMES, ENV_FRAMES, ENV_SESSION_PATH};

/// The ceiling on any one graph in this file.
///
/// Generous: these are real process graphs on a shared machine, and every
/// assertion is about an outcome rather than about how long it took. A
/// deterministic replay that hits this has hung, and the assertion that
/// follows says so.
const RUN_TIMEOUT: Duration = Duration::from_secs(90);

/// The timer period both the recorded graph and the replays subscribe to.
const TIMER_SOURCE: &str = "astrs/timer/millis/20";

/// One message log: what the daemon delivered, in the order it delivered it.
///
/// The comparable projection of an `.arec` entry — deliberately *not* the
/// file's bytes, whose header carries a per-run dataflow id and epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Message {
    /// The producing node.
    node: String,
    /// The producing port.
    output: String,
    /// The stamp the message carried.
    hlc: HlcTimestamp,
    /// The payload bytes.
    payload: Vec<u8>,
}

impl Message {
    /// The projection of one decoded entry.
    fn of(entry: &Entry) -> Self {
        Self {
            node: entry.node.as_str().to_owned(),
            output: entry.output.as_str().to_owned(),
            hlc: entry.hlc(),
            payload: entry.payload.clone(),
        }
    }
}

/// Every message a recording holds, in its canonical (HLC, then port) order.
fn messages(path: &Path) -> Vec<Message> {
    let mut reader = Reader::open(path)
        .unwrap_or_else(|error| panic!("{} is not readable: {error}", path.display()));
    reader
        .iter_all()
        .map(|entry| {
            Message::of(&entry.unwrap_or_else(|error| {
                panic!("{} holds an unreadable entry: {error}", path.display())
            }))
        })
        .collect()
}

/// One of `record-replay`'s built binaries, or the `cargo build` line that
/// would produce it.
///
/// The suite compiles nothing (see `astrs_conformance`'s own docs), so a
/// missing binary is a precondition failure and has to read like one.
fn example_binary(name: &str) -> PathBuf {
    binary(name, "record-replay").unwrap_or_else(|error| panic!("{error}"))
}

/// The graph that *produces* the recording: a timer-driven sensor and a tap.
///
/// Deliberately the same shape as the replay graph below, minus the tick tap:
/// the recorded stream has to be a timer-driven one for the replay's wheel to
/// have recorded points to land on.
fn source_manifest(session: &Path) -> String {
    format!(
        "astrs: \"1\"\n\
         name: m3-determinism-source\n\
         exit_when_nodes_finish: true\n\
         nodes:\n\
         \x20 - id: sensor\n\
         \x20   path: {sensor}\n\
         \x20   inputs:\n\
         \x20     tick: {TIMER_SOURCE}\n\
         \x20   outputs: [frames]\n\
         \x20   env:\n\
         \x20     {ENV_FRAMES}: \"{frames}\"\n\
         \x20 - id: frames-tap\n\
         \x20   path: {writer}\n\
         \x20   inputs:\n\
         \x20     frames:\n\
         \x20       source: sensor/frames\n\
         \x20       queue_size: 256\n\
         \x20       queue_policy: backpressure\n\
         \x20   env:\n\
         \x20     {ENV_SESSION_PATH}: {session}\n",
        sensor = example_binary("arec-sensor").display(),
        writer = example_binary("arec-writer").display(),
        frames = DEFAULT_FRAMES,
        session = session.display(),
    )
}

/// The graph a deterministic replay runs: the same sensor (which the run
/// stands down), one tap per stream.
///
/// Both taps are the shipped `arec-writer`, which records every input by its
/// *source* port — so the tick tap's artefact is the daemon's own
/// `astrs/timer/*` deliveries, stamps and all. Both queues are deep and
/// lossless (§11.2): a tap that dropped a message would accuse the replay of
/// losing it.
fn replay_manifest(frames_session: &Path, ticks_session: &Path) -> String {
    format!(
        "astrs: \"1\"\n\
         name: m3-determinism-replay\n\
         exit_when_nodes_finish: true\n\
         nodes:\n\
         \x20 - id: sensor\n\
         \x20   path: {sensor}\n\
         \x20   inputs:\n\
         \x20     tick: {TIMER_SOURCE}\n\
         \x20   outputs: [frames]\n\
         \x20   env:\n\
         \x20     {ENV_FRAMES}: \"{frames}\"\n\
         \x20 - id: frames-tap\n\
         \x20   path: {writer}\n\
         \x20   inputs:\n\
         \x20     frames:\n\
         \x20       source: sensor/frames\n\
         \x20       queue_size: 256\n\
         \x20       queue_policy: backpressure\n\
         \x20   env:\n\
         \x20     {ENV_SESSION_PATH}: {frames_session}\n\
         \x20 - id: tick-tap\n\
         \x20   path: {writer}\n\
         \x20   inputs:\n\
         \x20     tick:\n\
         \x20       source: {TIMER_SOURCE}\n\
         \x20       queue_size: 256\n\
         \x20       queue_policy: backpressure\n\
         \x20   env:\n\
         \x20     {ENV_SESSION_PATH}: {ticks_session}\n",
        sensor = example_binary("arec-sensor").display(),
        writer = example_binary("arec-writer").display(),
        frames = DEFAULT_FRAMES,
        frames_session = frames_session.display(),
        ticks_session = ticks_session.display(),
    )
}

/// What one deterministic replay produced.
struct Replay {
    /// The messages the stood-down `sensor` delivered.
    frames: Vec<Message>,
    /// The `astrs/timer/*` ticks the wheel delivered.
    ticks: Vec<Message>,
}

/// Runs a manifest through the real `astrs run` verb, asserting it finished.
fn run_graph(dir: &Path, yaml: &str, name: &str, deterministic: Option<(&Path, Option<f64>)>) {
    let manifest = dir.join(format!("{name}.yml"));
    std::fs::write(&manifest, yaml).unwrap();

    let mut args = RunArgs::new(&manifest);
    // The suite compiles nothing: cargo already built these binaries, and the
    // manifests above name them by absolute path.
    args.skip_build = true;
    args.working_dir = Some(dir.to_path_buf());
    args.runtime_dir = Some(dir.to_path_buf());
    args.timeout = Some(RUN_TIMEOUT);
    args.grace = Some(Duration::from_millis(500));
    if let Some((recording, speed)) = deterministic {
        args.deterministic = true;
        args.from_recording = Some(recording.to_path_buf());
        args.speed = speed;
    }

    let mut terminal: Vec<u8> = Vec::new();
    let report: RunReport = run(&mut terminal, &args).expect("astrs run returned a report");
    let text = String::from_utf8_lossy(&terminal).into_owned();
    assert_eq!(
        report.exit_code(),
        EXIT_OK,
        "{name} exited non-zero:\n{text}"
    );
    assert_eq!(
        report.result.status,
        DataflowStatus::Finished,
        "{name} did not finish (a deterministic run that hangs has run out of \
         recorded points without closing anything):\n{text}"
    );
    assert!(
        !report.result.has_failures(),
        "{name}: a node failed:\n{text}"
    );
}

/// Records the source session, once, into `dir`.
///
/// The fixture precondition is checked *here*, on the live run that produced
/// it, so a machine so loaded that the recording graph lost a frame is
/// diagnosed as that rather than as a replay defect. Every comparison in this
/// file is between replays of **one** recording, so the recorded stream's own
/// wall-clock jitter — which decides how many ticks the replay's wheel lands —
/// never enters an equality assertion.
fn record_source(dir: &Path) -> PathBuf {
    let session = dir.join("source.arec");
    run_graph(dir, &source_manifest(&session), "source", None);
    assert!(
        session.is_file(),
        "the recording graph wrote no session at {}",
        session.display()
    );
    assert_eq!(
        messages(&session).len(),
        usize::try_from(DEFAULT_FRAMES).unwrap(),
        "the *source* graph did not record every frame it published, so there \
         is nothing sound to replay (see {})",
        session.display()
    );
    session
}

/// Replays `session` deterministically once, returning both message logs.
fn replay_once(dir: &Path, session: &Path, label: &str, speed: Option<f64>) -> Replay {
    let frames_session = dir.join(format!("frames-{label}.arec"));
    let ticks_session = dir.join(format!("ticks-{label}.arec"));
    run_graph(
        dir,
        &replay_manifest(&frames_session, &ticks_session),
        &format!("replay-{label}"),
        Some((session, speed)),
    );
    Replay {
        frames: messages(&frames_session),
        ticks: messages(&ticks_session),
    }
}

/// The whole fixture: a source recording plus two deterministic replays of it.
fn record_then_replay_twice() -> (PathBuf, PathBuf, Replay, Replay) {
    let dir = make_scratch_dir().unwrap();
    let session = record_source(&dir);
    let first = replay_once(&dir, &session, "a", None);
    let second = replay_once(&dir, &session, "b", None);
    (dir, session, first, second)
}

#[test]
fn two_deterministic_replays_produce_identical_message_logs() {
    let (dir, _session, first, second) = record_then_replay_twice();

    // Guarded before the equality: two empty logs compare equal, and that is
    // how a determinism test lies. `record_source` has already checked the
    // recording itself holds every frame, so a mismatch here is the replay's.
    assert_eq!(
        first.frames.len(),
        usize::try_from(DEFAULT_FRAMES).unwrap(),
        "the replay delivered the wrong number of recorded messages"
    );
    assert!(
        !first.ticks.is_empty(),
        "the recorded clock produced no ticks at all, so nothing about the \
         timer wheel is under test"
    );

    assert_eq!(
        first.frames, second.frames,
        "two deterministic replays of one recording must deliver the same \
         messages, in the same order, with the same stamps"
    );
    assert_eq!(
        first.ticks, second.ticks,
        "two deterministic replays of one recording must produce the same \
         timer ticks, in the same order, with the same stamps"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_deterministic_replay_preserves_the_recorded_timestamps() {
    // The claim `astrs replay` deliberately does *not* make (its replay nodes
    // re-stamp every message from their own clock), and the reason
    // determinism mode replays from inside the daemon instead.
    let (dir, session, first, _second) = record_then_replay_twice();
    let recorded = messages(&session);

    assert_eq!(
        recorded.len(),
        usize::try_from(DEFAULT_FRAMES).unwrap(),
        "the source graph recorded the wrong number of frames"
    );
    assert_eq!(
        first.frames, recorded,
        "a deterministic replay must deliver the recorded messages unchanged \
         — producer, port, payload *and* HLC"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn timer_ticks_land_only_on_recorded_hlc_points() {
    // §14's actual sentence: the wheel is fixed to the recording's HLC
    // stream. Every tick therefore carries the physical time of some recorded
    // point — never a reading of this machine's clock.
    let (dir, session, first, _second) = record_then_replay_twice();
    let points: BTreeSet<u64> = messages(&session)
        .iter()
        .map(|message| message.hlc.physical_ns())
        .collect();

    assert!(!first.ticks.is_empty(), "no ticks to check");
    for tick in &first.ticks {
        assert_eq!(
            tick.node, "astrs",
            "a tick comes from the reserved virtual producer (§8.4)"
        );
        assert!(
            tick.output.starts_with("timer."),
            "unexpected virtual port {}",
            tick.output
        );
        assert!(
            points.contains(&tick.hlc.physical_ns()),
            "tick at {:?} is not on any recorded point",
            tick.hlc
        );
    }

    let mut previous: Option<HlcTimestamp> = None;
    for tick in &first.ticks {
        if let Some(previous) = previous {
            assert!(previous < tick.hlc, "ticks must be strictly ordered");
        }
        previous = Some(tick.hlc);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pacing_changes_how_long_a_run_takes_and_nothing_else() {
    // `--speed` scales the wall-clock walk through the recorded stream. It
    // must not touch the virtual timeline, so the artefacts are the same
    // ones the unpaced replay produced.
    let dir = make_scratch_dir().unwrap();
    let session = record_source(&dir);
    let unpaced = replay_once(&dir, &session, "fast", None);
    let paced = replay_once(&dir, &session, "paced", Some(4.0));

    assert_eq!(
        unpaced.frames.len(),
        usize::try_from(DEFAULT_FRAMES).unwrap()
    );
    assert_eq!(unpaced.frames, paced.frames, "pacing changed the messages");
    assert_eq!(unpaced.ticks, paced.ticks, "pacing changed the tick grid");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn determinism_without_a_recording_is_refused() {
    // A run whose wheel still reads the wall clock is not reproducible,
    // whatever the flag says — so the flag is refused before anything is
    // built or spawned rather than honoured as a label.
    let dir = make_scratch_dir().unwrap();
    let manifest = dir.join("plain.yml");
    std::fs::write(
        &manifest,
        "astrs: \"1\"\nnodes:\n  - id: a\n    path: /usr/bin/true\n",
    )
    .unwrap();

    let mut args = RunArgs::new(&manifest);
    args.skip_build = true;
    args.deterministic = true;
    let error = run(&mut Vec::new(), &args).expect_err("determinism needs a recording");
    let text = error.to_string();
    assert!(
        text.contains("--from-recording"),
        "the refusal must say what is missing: {text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
