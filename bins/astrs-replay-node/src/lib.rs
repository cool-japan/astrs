//! The AstRS replay node.
//!
//! Reads an `.arec` session and re-injects the recorded outputs as
//! sources, replacing any subset of the original nodes — so a new
//! planner can be tested against last week's sensor data byte for byte,
//! at a configurable speed or in lockstep (blueprint §14).
//!
//! # Taking over a producer's identity
//!
//! `astrs replay <file> --into <manifest>` (the CLI verb this binary
//! serves) rewrites the target manifest **in place**: a replaced node
//! keeps its own `id` and its own declared `outputs`, only its `path:`
//! becomes `astrs-replay-node` and its `args:` gain `--only
//! <that node's id>/<output>` for each output it had. Every sibling
//! node's `inputs:` therefore still reads `camera/frames` — nothing
//! about the graph's wiring changes, only what actually produces it.
//!
//! Consequently, this binary publishes on **its own declared output
//! names** (`Node::descriptor().outputs`), not on the entry's *recorded*
//! producer node id — an entry recorded as `camera/frames` republishes
//! on whichever output name this process itself declares as `frames`,
//! which for the in-place rewrite above is always the same name the
//! original producer used.
//!
//! # Timing modes
//!
//! - [`TimingMode::AsFastAsPossible`] — no pacing at all.
//! - [`TimingMode::RealTime`] — sleeps the recorded HLC delta between
//!   consecutive entries, scaled by `--speed` (`2.0` plays twice as
//!   fast; `0.5` half as fast).
//! - [`TimingMode::FixedRate`] — sleeps a constant `1 / --rate` seconds
//!   between every entry, ignoring recorded deltas entirely.
//!
//! Every republished message carries a **fresh** HLC timestamp from this
//! node's own clock — replay is a new live event, not a literal replica
//! of history — while every other metadata parameter (`seq`,
//! correlation keys, …) is carried through unchanged. This is what makes
//! the payload bytes exactly reproducible while the timestamps stay
//! monotone on their own new timeline, regardless of what the recording's
//! original clock did.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry as MapEntry;
use std::path::PathBuf;
use std::time::Duration;

use astrs_node_api::{Event, EventStream, Node, NodeError, RawOutput};
use astrs_recording::{Entry as RecordedEntry, Reader, RecordingError};
use astrs_time::HlcTimestamp;
use astrs_wire::{DataId, IdError, PortRef};
use clap::Parser;

/// How [`replay`] paces re-emitted entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum TimingMode {
    /// Emit every entry back to back, with no delay.
    AsFastAsPossible,
    /// Sleep the recorded HLC delta between entries, scaled by `--speed`.
    RealTime,
    /// Sleep a constant period between entries, from `--rate` (Hz).
    FixedRate,
}

/// The default fixed-rate emission frequency, when `--mode fixed-rate` is
/// asked for without `--rate`.
pub const DEFAULT_FIXED_RATE_HZ: f64 = 10.0;

/// `astrs-replay-node` command-line arguments.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "astrs-replay-node",
    about = "Re-injects an .arec recording's entries as this node's outputs (blueprint §14)."
)]
pub struct ReplayArgs {
    /// The `.arec` file to replay.
    pub input: PathBuf,
    /// The timing mode entries are paced with.
    #[arg(long, value_enum, default_value = "real-time")]
    pub mode: TimingMode,
    /// The speed multiplier applied to recorded HLC deltas in `real-time`
    /// mode (`2.0` is twice as fast).
    #[arg(long, default_value_t = 1.0)]
    pub speed: f64,
    /// The fixed emission rate, in Hz, for `fixed-rate` mode. Defaults to
    /// [`DEFAULT_FIXED_RATE_HZ`] when the mode is `fixed-rate` and this
    /// is unset.
    #[arg(long)]
    pub rate: Option<f64>,
    /// Restart from the beginning once every entry has been replayed.
    #[arg(long)]
    pub r#loop: bool,
    /// Restrict replay to these recorded `node/output` ports (repeatable
    /// — an empty list replays everything the file holds).
    #[arg(long = "only")]
    pub only: Vec<String>,
}

impl ReplayArgs {
    /// Arguments to replay `input` in real-time mode at normal speed,
    /// with no filter and no looping — the common case for a direct
    /// caller (tests, an embedder) that does not need `clap`.
    #[must_use]
    pub fn new(input: impl Into<PathBuf>) -> Self {
        Self {
            input: input.into(),
            mode: TimingMode::RealTime,
            speed: 1.0,
            rate: None,
            r#loop: false,
            only: Vec::new(),
        }
    }

    /// Sets the timing mode.
    #[must_use]
    pub const fn with_mode(mut self, mode: TimingMode) -> Self {
        self.mode = mode;
        self
    }

    /// Sets the real-time speed multiplier.
    #[must_use]
    pub const fn with_speed(mut self, speed: f64) -> Self {
        self.speed = speed;
        self
    }

    /// Sets the fixed-rate emission frequency.
    #[must_use]
    pub const fn with_rate(mut self, rate: f64) -> Self {
        self.rate = Some(rate);
        self
    }

    /// Enables looping.
    #[must_use]
    pub const fn looping(mut self) -> Self {
        self.r#loop = true;
        self
    }

    /// Restricts replay to `node/output`.
    #[must_use]
    pub fn only(mut self, port: impl Into<String>) -> Self {
        self.only.push(port.into());
        self
    }

    /// Parses [`ReplayArgs::only`] into [`PortRef`]s.
    ///
    /// # Errors
    ///
    /// [`IdError`] if any entry is not a well-formed `node/output` string.
    pub fn parsed_filters(&self) -> Result<Vec<PortRef>, IdError> {
        self.only.iter().map(|text| text.parse()).collect()
    }
}

/// Errors `astrs-replay-node` can report.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReplayNodeError {
    /// Joining the dataflow failed.
    #[error("node initialization failed: {0}")]
    Node(#[from] NodeError),
    /// Reading the `.arec` session failed.
    #[error(".arec read failed: {0}")]
    Recording(#[from] RecordingError),
    /// A `--only` filter was not a well-formed `node/output` string.
    #[error("--only {value:?} is not a well-formed node/output filter: {source}")]
    BadFilter {
        /// The offending filter text.
        value: String,
        /// Why it was rejected.
        #[source]
        source: IdError,
    },
}

/// Connects as an ordinary node (§9.1's [`Node::init_from_env`]) and
/// replays `args.input` until told to stop.
///
/// # Errors
///
/// As [`replay`].
pub fn run(args: &ReplayArgs) -> Result<(), ReplayNodeError> {
    let (mut node, mut events) = Node::init_from_env()?;
    replay(&mut node, &mut events, args)
}

/// The replay loop itself, parameterized over an already-connected node
/// — the seam [`astrs_node_api::testing::MockDaemon`]-driven tests use.
///
/// Opens `args.input` once, republishes every entry that survives the
/// `--only` filter (all of them, if none was given) in HLC order,
/// pacing each one per [`ReplayArgs::mode`], and either stops after one
/// full pass or restarts it under [`ReplayArgs::loop`]. A [`Event::Stop`]
/// observed between entries ends the loop early, however far through a
/// pass it is.
///
/// # Errors
///
/// [`ReplayNodeError::Recording`] if the file cannot be opened (falling
/// back to [`Reader::open_or_recover`] for one with no footer) or an
/// entry cannot be decoded; [`ReplayNodeError::Node`] if publishing
/// fails; [`ReplayNodeError::BadFilter`] if `--only` is malformed.
pub fn replay(
    node: &mut Node,
    events: &mut EventStream,
    args: &ReplayArgs,
) -> Result<(), ReplayNodeError> {
    let filters = args
        .parsed_filters()
        .map_err(|source| ReplayNodeError::BadFilter {
            value: args.only.join(", "),
            source,
        })?;
    let (mut reader, _report) = Reader::open_or_recover(&args.input)?;
    let mut outputs: BTreeMap<DataId, RawOutput> = BTreeMap::new();

    loop {
        let mut previous_hlc: Option<HlcTimestamp> = None;
        let mut emitted_any = false;
        let mut stop_requested = false;

        for candidate in reader.iter_all() {
            if matches!(events.try_recv(), Some(Event::Stop(_))) {
                stop_requested = true;
                break;
            }
            let entry = candidate?;
            if !passes_filter(&filters, &entry) {
                continue;
            }
            pace(args, &mut previous_hlc, entry.hlc());
            publish(node, &mut outputs, entry)?;
            emitted_any = true;
        }

        if stop_requested || !args.r#loop {
            break;
        }
        if !emitted_any {
            // Nothing matched (an empty file, or a filter that excludes
            // everything): looping forever with zero work would just spin
            // the CPU. A short, bounded pause between empty passes keeps
            // this responsive to `Stop` without doing that.
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    for output in outputs.values_mut() {
        output.close()?;
    }
    Ok(())
}

/// Whether `entry` was recorded from a port `filters` names — every
/// filter passes when `filters` is empty.
fn passes_filter(filters: &[PortRef], entry: &RecordedEntry) -> bool {
    filters.is_empty()
        || filters
            .iter()
            .any(|port| port.node() == &entry.node && port.port() == &entry.output)
}

/// Sleeps however long `args.mode` says to wait before publishing
/// `current`, given the previously published entry's timestamp (`None`
/// for the very first entry of a pass, which always publishes at once).
fn pace(args: &ReplayArgs, previous: &mut Option<HlcTimestamp>, current: HlcTimestamp) {
    match args.mode {
        TimingMode::AsFastAsPossible => {}
        TimingMode::RealTime => {
            if let Some(previous_hlc) = *previous {
                let delta_ns = current
                    .physical_ns()
                    .saturating_sub(previous_hlc.physical_ns());
                let speed = if args.speed.is_finite() && args.speed > 0.0 {
                    args.speed
                } else {
                    1.0
                };
                let scaled_ns = (delta_ns as f64 / speed).round().max(0.0);
                if scaled_ns > 0.0 {
                    std::thread::sleep(Duration::from_secs_f64(scaled_ns / 1_000_000_000.0));
                }
            }
        }
        TimingMode::FixedRate => {
            let hz = args
                .rate
                .filter(|rate| rate.is_finite() && *rate > 0.0)
                .unwrap_or(DEFAULT_FIXED_RATE_HZ);
            std::thread::sleep(Duration::from_secs_f64(1.0 / hz));
        }
    }
    *previous = Some(current);
}

/// Republishes one recorded entry on this node's own output of the same
/// name (see the module docs for why the entry's *recorded* producer id
/// is irrelevant here), claiming the output handle the first time that
/// name is seen.
///
/// The republished metadata keeps every parameter the recording carried
/// (`seq`, correlation keys, …) but overwrites the timestamp with this
/// node's own live clock — a fresh, monotone reading, not the recorded
/// one (see the module docs).
fn publish(
    node: &mut Node,
    outputs: &mut BTreeMap<DataId, RawOutput>,
    entry: RecordedEntry,
) -> Result<(), ReplayNodeError> {
    let handle = match outputs.entry(entry.output.clone()) {
        MapEntry::Occupied(existing) => existing.into_mut(),
        MapEntry::Vacant(vacant) => {
            let raw = node.raw_output(entry.output.as_str())?;
            vacant.insert(raw)
        }
    };
    let mut meta = entry.meta;
    meta.timestamp = node.hlc_now();
    handle.send_bytes(entry.payload, meta)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use astrs_node_api::testing::MockDaemon;
    use astrs_recording::{Writer, WriterOptions};
    use astrs_wire::{
        DataflowId, InputSpec, Metadata, NodeId, NodeSource, NodeSpawnSpec, OutputSpec, StopCause,
    };

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "astrs-replay-node-test-{}-{}-{label}.arec",
            std::process::id(),
            uniq()
        ))
    }

    /// Writes a small `.arec` recording with one producer, two outputs.
    fn sample_recording(path: &PathBuf) {
        let options = WriterOptions::new(DataflowId::from_u128(1), HlcTimestamp::EPOCH);
        let mut writer = Writer::create(path, options).unwrap();
        writer
            .append_parts(
                NodeId::new("camera").unwrap(),
                DataId::new("frames").unwrap(),
                Metadata::new(HlcTimestamp::new(1_000, 0)),
                vec![1, 2, 3],
            )
            .unwrap();
        writer
            .append_parts(
                NodeId::new("camera").unwrap(),
                DataId::new("meta").unwrap(),
                Metadata::new(HlcTimestamp::new(1_001, 0)),
                vec![9],
            )
            .unwrap();
        writer
            .append_parts(
                NodeId::new("other").unwrap(),
                DataId::new("x").unwrap(),
                Metadata::new(HlcTimestamp::new(1_002, 0)),
                vec![7],
            )
            .unwrap();
        writer.finish().unwrap();
    }

    /// A replay-node spec taking over `id`'s declared `outputs`.
    fn replay_spec(daemon: &MockDaemon, id: &str, outputs: &[&str]) -> NodeSpawnSpec {
        let mut spec = NodeSpawnSpec::new(
            daemon.dataflow(),
            NodeId::new(id).unwrap(),
            0,
            NodeSource::Dynamic,
        );
        for output in outputs {
            spec = spec.with_output(OutputSpec::new(DataId::new(*output).unwrap()));
        }
        spec
    }

    #[test]
    fn as_fast_as_possible_replays_the_filtered_subset_in_hlc_order() {
        let path = temp_path("subset");
        sample_recording(&path);

        let daemon = MockDaemon::start().unwrap();
        let spec = replay_spec(&daemon, "camera", &["frames", "meta"]);
        let (mut node, mut events) = daemon.connect_node(spec).unwrap();

        let args = ReplayArgs::new(&path)
            .with_mode(TimingMode::AsFastAsPossible)
            .only("camera/frames")
            .only("camera/meta");
        replay(&mut node, &mut events, &args).unwrap();

        // `replay` returning does not guarantee the node's writer task has
        // already flushed every queued send over the (mock) wire —
        // `wait_for_sends` is the daemon-side synchronization for that.
        let camera = NodeId::new("camera").unwrap();
        daemon
            .wait_for_sends(
                &camera,
                &DataId::new("frames").unwrap(),
                1,
                Duration::from_secs(5),
            )
            .unwrap();
        daemon
            .wait_for_sends(
                &camera,
                &DataId::new("meta").unwrap(),
                1,
                Duration::from_secs(5),
            )
            .unwrap();

        let sends = daemon.sends();
        assert_eq!(sends.len(), 2, "the `other/x` entry must be filtered out");
        assert_eq!(sends[0].output.as_str(), "frames");
        assert_eq!(sends[0].bytes(), Some(&[1, 2, 3][..]));
        assert_eq!(sends[1].output.as_str(), "meta");
        assert_eq!(sends[1].bytes(), Some(&[9][..]));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn no_filter_replays_only_this_nodes_own_declared_outputs() {
        // Recording has entries from both "camera" and "other", but this
        // replay-node only declares "frames"/"meta" — asking `raw_output`
        // for "x" (which "other/x" would need) is refused, so an empty
        // filter here still only ever republishes what this node can.
        let path = temp_path("own-outputs-only");
        sample_recording(&path);

        let daemon = MockDaemon::start().unwrap();
        let spec = replay_spec(&daemon, "camera", &["frames", "meta"]);
        let (mut node, mut events) = daemon.connect_node(spec).unwrap();

        let args = ReplayArgs::new(&path).with_mode(TimingMode::AsFastAsPossible);
        let error = replay(&mut node, &mut events, &args).unwrap_err();
        assert!(
            matches!(error, ReplayNodeError::Node(_)),
            "publishing the unfiltered `other/x` entry on an undeclared output must fail: {error:?}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn byte_equality_and_hlc_monotonicity_hold_across_a_record_replay_round_trip() {
        // Live pipeline: a producer feeds a record-node.
        let daemon = MockDaemon::start().unwrap();
        let producer_id = NodeId::new("camera").unwrap();
        let producer_spec = NodeSpawnSpec::new(
            daemon.dataflow(),
            producer_id.clone(),
            0,
            NodeSource::Dynamic,
        )
        .with_output(OutputSpec::new(DataId::new("frames").unwrap()));
        let (mut producer, _producer_events) = daemon.connect_node(producer_spec).unwrap();

        let recorder_spec = NodeSpawnSpec::new(
            daemon.dataflow(),
            NodeId::new("recorder").unwrap(),
            0,
            NodeSource::Dynamic,
        )
        .with_input(InputSpec::new(
            DataId::new("_record_0").unwrap(),
            PortRef::from_parts("camera", "frames").unwrap(),
        ));
        let (recorder_node, recorder_events) = daemon.connect_node(recorder_spec).unwrap();

        let recorded_path = temp_path("round-trip");
        let recorder_args = astrs_record_node::RecordArgs::new(&recorded_path);
        let recorder_handle = std::thread::spawn({
            let mut recorder_node = recorder_node;
            let mut recorder_events = recorder_events;
            move || {
                astrs_record_node::record(&mut recorder_node, &mut recorder_events, &recorder_args)
            }
        });

        let payloads: Vec<Vec<u8>> = (0..5u8).map(|seq| vec![seq; 4]).collect();
        let mut output = producer.raw_output("frames").unwrap();
        for payload in &payloads {
            output
                .send_bytes(payload.clone(), producer.metadata())
                .unwrap();
        }

        assert_eq!(
            astrs_replay_node_test_support::wait_for_recovered_count(
                &recorded_path,
                payloads.len(),
                Duration::from_secs(5)
            ),
            payloads.len(),
            "the recorder must have durably captured every published frame"
        );
        daemon
            .stop(&NodeId::new("recorder").unwrap(), StopCause::Requested)
            .unwrap();
        recorder_handle.join().unwrap().unwrap();

        // `wait_for_recovered_count` above only guarantees the *file* has
        // every entry; `handle_request`'s `SendMessage` arm records into
        // `daemon.inner.sends` strictly before it routes the input on to
        // the recorder (whose write is what that wait actually observed),
        // so the daemon's own bookkeeping for the original producer's
        // sends is already complete too — safe to snapshot as a baseline.
        let camera = NodeId::new("camera").unwrap();
        let frames = DataId::new("frames").unwrap();
        let baseline = daemon.sends_on(&camera, &frames).len();
        assert_eq!(baseline, payloads.len(), "the live producer's own sends");

        // Replay: a fresh node takes over "camera"'s identity and
        // publishes into a counting sink — the same (node, output) the
        // original producer used, so the assertions below only look at
        // what replay added *after* the baseline.
        let replay_spec =
            NodeSpawnSpec::new(daemon.dataflow(), producer_id, 1, NodeSource::Dynamic)
                .with_output(OutputSpec::new(DataId::new("frames").unwrap()));
        let (mut replay_node, mut replay_events) = daemon.connect_node(replay_spec).unwrap();
        let replay_args = ReplayArgs::new(&recorded_path).with_mode(TimingMode::AsFastAsPossible);
        replay(&mut replay_node, &mut replay_events, &replay_args).unwrap();

        daemon
            .wait_for_sends(
                &camera,
                &frames,
                baseline + payloads.len(),
                Duration::from_secs(5),
            )
            .unwrap();
        let sends = daemon.sends_on(&camera, &frames);
        assert_eq!(sends.len(), baseline + payloads.len());
        let replayed = &sends[baseline..];
        assert_eq!(
            replayed.len(),
            payloads.len(),
            "the counting sink's send count"
        );
        for (sent, expected) in replayed.iter().zip(payloads.iter()) {
            assert_eq!(
                sent.bytes(),
                Some(expected.as_slice()),
                "byte-equality of payloads"
            );
        }
        let timestamps: Vec<HlcTimestamp> = replayed
            .iter()
            .map(|send| send.metadata.timestamp)
            .collect();
        for window in timestamps.windows(2) {
            assert!(
                window[0] < window[1],
                "HLC monotonicity across the replayed sequence"
            );
        }

        let _ = std::fs::remove_file(&recorded_path);
    }

    #[test]
    fn looping_replays_more_than_once_until_stopped() {
        let path = temp_path("looping");
        sample_recording(&path);

        let daemon = MockDaemon::start().unwrap();
        let spec = replay_spec(&daemon, "camera", &["frames", "meta"]);
        let recorder_id = NodeId::new("camera").unwrap();
        let (node, events) = daemon.connect_node(spec).unwrap();

        let args = ReplayArgs::new(&path)
            .with_mode(TimingMode::AsFastAsPossible)
            .only("camera/frames")
            .only("camera/meta")
            .looping();
        let handle = std::thread::spawn({
            let mut node = node;
            let mut events = events;
            move || replay(&mut node, &mut events, &args)
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while daemon.sends().len() < 5 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            daemon.sends().len() >= 5,
            "looping must republish past one full pass (2 matching entries)"
        );

        daemon.stop(&recorder_id, StopCause::Requested).unwrap();
        handle.join().unwrap().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fixed_rate_defaults_when_no_rate_is_given() {
        let mut previous = None;
        let args = ReplayArgs::new("unused.arec").with_mode(TimingMode::FixedRate);
        let start = std::time::Instant::now();
        pace(&args, &mut previous, HlcTimestamp::new(1, 0));
        assert!(start.elapsed() >= Duration::from_secs_f64(1.0 / DEFAULT_FIXED_RATE_HZ) / 2);
    }

    #[test]
    fn as_fast_as_possible_never_sleeps() {
        let mut previous = Some(HlcTimestamp::new(1, 0));
        let args = ReplayArgs::new("unused.arec").with_mode(TimingMode::AsFastAsPossible);
        let start = std::time::Instant::now();
        pace(
            &args,
            &mut previous,
            HlcTimestamp::new(1_000_000_000_000, 0),
        );
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn cli_parsing_accepts_every_flag() {
        let args = ReplayArgs::try_parse_from([
            "astrs-replay-node",
            "session.arec",
            "--mode",
            "fixed-rate",
            "--rate",
            "30",
            "--loop",
            "--only",
            "camera/frames",
        ])
        .unwrap();
        assert_eq!(args.input, PathBuf::from("session.arec"));
        assert_eq!(args.mode, TimingMode::FixedRate);
        assert_eq!(args.rate, Some(30.0));
        assert!(args.r#loop);
        assert_eq!(args.only, vec!["camera/frames".to_string()]);
    }

    #[test]
    fn a_malformed_filter_is_reported_not_panicked() {
        let daemon = MockDaemon::start().unwrap();
        let spec = replay_spec(&daemon, "camera", &["frames"]);
        let (mut node, mut events) = daemon.connect_node(spec).unwrap();
        let args = ReplayArgs::new("unused.arec").only("not a valid port");
        let error = replay(&mut node, &mut events, &args).unwrap_err();
        assert!(matches!(error, ReplayNodeError::BadFilter { .. }));
    }
}

/// Test-only helpers shared by this crate's own integration test above.
///
/// A separate module (rather than inlining) purely so the round-trip
/// test's setup reads as a sequence of named steps.
#[cfg(test)]
mod astrs_replay_node_test_support {
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// Polls a still-being-written `.arec` file's scan-recoverable entry
    /// count until it reaches `expected` or `timeout` elapses.
    pub fn wait_for_recovered_count(path: &Path, expected: usize, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        loop {
            let count = astrs_recording::recover::scan(path)
                .map(|report| report.index.len())
                .unwrap_or(0);
            if count >= expected || Instant::now() >= deadline {
                return count;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
