//! The AstRS recorder node.
//!
//! A normal graph node (also reachable through `record:` manifest sugar
//! and `astrs record start`) that subscribes to the selected outputs and
//! streams them into an `.arec` session through `astrs-recording` —
//! HLC-ordered, zstd-framed and seekable (blueprint §14).
//!
//! # Convention-agnostic by design
//!
//! This binary is spawned two different ways, and each names this node's
//! own inputs differently:
//!
//! - `record:` manifest sugar lowers to inputs named `_record_0`,
//!   `_record_1`, … (`astrs_manifest::Node::record_sugar_inputs`).
//! - `astrs record start`'s dynamic-node path names them `in0`, `in1`, …
//!   (the coordinator's own synthesis).
//!
//! [`record`] never looks at its own input names at all. For every
//! [`Event::Input`], [`Node::input_source`] answers "which producer port
//! did this actually come from" — a property of the wiring, not of the
//! message — and that answer is what becomes the entry's `node`/`output`
//! fields. The same binary therefore serves both spawn paths (and any
//! future one) without caring which it was.
//!
//! # Usage
//!
//! ```text
//! astrs-record-node <output.arec> [--rotate-bytes N] [--rotate-seconds N]
//! ```
//!
//! The daemon supplies `<output.arec>` as this process's first argument
//! (see `astrs-daemon`'s `command_for`); `--rotate-*` are not set by
//! either spawn path today, but are here for a caller (a future manifest
//! field, direct invocation) that wants bounded file sizes.

use std::path::PathBuf;
use std::time::Duration;

use astrs_node_api::{Event, EventStream, Node, NodeError};
use astrs_recording::{RecordingError, RotationPolicy, Writer, WriterOptions};
use astrs_wire::{DataId, NodeId, PortRef};
use clap::Parser;

/// `astrs-record-node` command-line arguments.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "astrs-record-node",
    about = "Writes a dataflow's inputs into an .arec recording (blueprint §14)."
)]
pub struct RecordArgs {
    /// The `.arec` file to write (numbered segments if rotation is set —
    /// see [`astrs_recording::Writer`]).
    pub output: PathBuf,
    /// Rotate to a new file once the current one would exceed this many
    /// bytes.
    #[arg(long = "rotate-bytes")]
    pub rotate_bytes: Option<u64>,
    /// Rotate to a new file once the current one has been open this many
    /// seconds.
    #[arg(long = "rotate-seconds")]
    pub rotate_seconds: Option<u64>,
}

impl RecordArgs {
    /// Arguments for `output`, with no rotation — the common case for a
    /// direct caller (tests, an embedder) that does not need `clap`.
    #[must_use]
    pub fn new(output: impl Into<PathBuf>) -> Self {
        Self {
            output: output.into(),
            rotate_bytes: None,
            rotate_seconds: None,
        }
    }

    /// Sets the byte-count rotation bound.
    #[must_use]
    pub const fn with_rotate_bytes(mut self, bytes: u64) -> Self {
        self.rotate_bytes = Some(bytes);
        self
    }

    /// Sets the wall-clock rotation bound.
    #[must_use]
    pub const fn with_rotate_seconds(mut self, seconds: u64) -> Self {
        self.rotate_seconds = Some(seconds);
        self
    }

    /// This session's [`RotationPolicy`], derived from the `--rotate-*`
    /// flags.
    #[must_use]
    pub fn rotation_policy(&self) -> RotationPolicy {
        RotationPolicy {
            max_bytes: self.rotate_bytes,
            max_duration: self.rotate_seconds.map(Duration::from_secs),
        }
    }
}

/// Errors `astrs-record-node` can report.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RecordNodeError {
    /// Joining the dataflow failed.
    #[error("node initialization failed: {0}")]
    Node(#[from] NodeError),
    /// Writing the `.arec` session failed.
    #[error(".arec write failed: {0}")]
    Recording(#[from] RecordingError),
}

/// Connects as an ordinary node (§9.1's [`Node::init_from_env`]) and
/// records every input until told to stop.
///
/// # Errors
///
/// [`RecordNodeError::Node`] if the daemon handshake fails, or
/// [`RecordNodeError::Recording`] if the `.arec` file cannot be created
/// or written.
pub fn run(args: &RecordArgs) -> Result<(), RecordNodeError> {
    let (mut node, mut events) = Node::init_from_env()?;
    record(&mut node, &mut events, args)
}

/// The recording loop itself, parameterized over an already-connected
/// node — the seam [`astrs_node_api::testing::MockDaemon`]-driven tests
/// use to exercise this without a real daemon or a real file-descriptor
/// handshake.
///
/// Every [`Event::Input`] is appended as one `.arec` entry, its `node`
/// and `output` taken from [`Node::input_source`] rather than from this
/// node's own input id (see the module docs). The loop ends — and the
/// file is finalized with its footer and trailer — on [`Event::Stop`] or
/// [`Event::AllInputsClosed`], or when the event stream itself ends
/// (`recv` returning [`None`], e.g. the daemon connection closing).
/// Every other event is ignored: a recorder has nothing useful to do
/// with a parameter update or a peer's restart.
///
/// # Errors
///
/// [`RecordNodeError::Recording`] if the file cannot be created or a
/// write fails. A write failure partway through still leaves every
/// entry appended before it durable — see [`astrs_recording::Writer`]'s
/// own crash-tolerance docs — so this is never a total loss.
pub fn record(
    node: &mut Node,
    events: &mut EventStream,
    args: &RecordArgs,
) -> Result<(), RecordNodeError> {
    let options = WriterOptions::new(node.dataflow_id(), node.hlc_now())
        .with_rotation(args.rotation_policy());
    let mut writer = Writer::create(&args.output, options)?;

    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, data } => {
                let (source_node, source_output) = source_of(node, &id);
                writer.append_parts(source_node, source_output, meta, data.to_vec())?;
            }
            Event::Stop(_) | Event::AllInputsClosed => break,
            _ => {}
        }
    }

    writer.finish()?;
    Ok(())
}

/// The producer `(node, output)` behind one of this node's inputs.
///
/// Resolves through [`Node::input_source`] unconditionally — see the
/// module docs for why. The fallback (this node's own input id, doubling
/// as both halves) is defensive only: every input the daemon actually
/// delivers an [`Event::Input`] for has a declared source in this node's
/// [`astrs_wire::NodeSpawnSpec`], so it should be unreachable in
/// practice.
fn source_of(node: &Node, input: &DataId) -> (NodeId, DataId) {
    node.input_source(input.as_str())
        .map(PortRef::into_parts)
        .unwrap_or_else(|| (NodeId::sanitized(input.as_str()), input.clone()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use astrs_node_api::testing::MockDaemon;
    use astrs_recording::Reader;
    use astrs_time::HlcTimestamp;
    use astrs_wire::{InputSpec, Metadata, NodeSource, NodeSpawnSpec, StopCause};
    use std::path::PathBuf;

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "astrs-record-node-test-{}-{}-{label}.arec",
            std::process::id(),
            uniq()
        ))
    }

    /// A recorder node spec with one input per `(input_name, source_node,
    /// source_output)` triple, mirroring the shape either spawn path
    /// (`record:` sugar's `_record_<i>` or `astrs record start`'s
    /// `in<i>`) actually builds.
    fn recorder_spec(daemon: &MockDaemon, inputs: &[(&str, &str, &str)]) -> NodeSpawnSpec {
        let mut spec = NodeSpawnSpec::new(
            daemon.dataflow(),
            NodeId::new("recorder").unwrap(),
            0,
            NodeSource::Dynamic,
        );
        for (name, node, output) in inputs {
            spec = spec.with_input(InputSpec::new(
                DataId::new(*name).unwrap(),
                PortRef::from_parts(node, output).unwrap(),
            ));
        }
        spec
    }

    /// Runs [`record`] on a background thread, since it blocks until
    /// `Stop`/`AllInputsClosed`.
    ///
    /// A test needs this rather than calling `record` inline because
    /// `EventStream`'s control lane deliberately jumps ahead of already
    /// -queued data (§11.3: "control... delivered before any data") —
    /// sending every input and then `Stop` back-to-back, *before*
    /// anything has started draining the stream, would let `Stop` be
    /// served first and the recorder would capture nothing. Running the
    /// loop concurrently and waiting (via [`wait_for_recovered_count`])
    /// for it to have actually written what was sent so far is what a
    /// real recorder's continuously-draining loop gets for free.
    fn spawn_record(
        mut node: Node,
        mut events: EventStream,
        args: RecordArgs,
    ) -> std::thread::JoinHandle<Result<(), RecordNodeError>> {
        std::thread::spawn(move || record(&mut node, &mut events, &args))
    }

    /// Polls (never sleeping longer than a few milliseconds at a time,
    /// bounded by `timeout`) an in-progress recording's scan-recoverable
    /// entry count until it reaches `expected`.
    ///
    /// Safe to call while a [`Writer`] elsewhere is still appending to
    /// the same path: every [`Writer::append`] flushes durably before
    /// returning, and [`astrs_recording::recover::scan`] tolerates
    /// (indeed, exists for) a file with no footer yet.
    fn wait_for_recovered_count(
        path: &std::path::Path,
        expected: usize,
        timeout: Duration,
    ) -> usize {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let count = astrs_recording::recover::scan(path)
                .map(|report| report.index.len())
                .unwrap_or(0);
            if count >= expected || std::time::Instant::now() >= deadline {
                return count;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn records_inputs_named_however_the_spawn_path_named_them() {
        let daemon = MockDaemon::start().unwrap();
        let spec = recorder_spec(
            &daemon,
            &[
                ("_record_0", "camera", "frames"),
                ("in1", "detector", "boxes"),
            ],
        );
        let recorder_id = NodeId::new("recorder").unwrap();
        let (node, events) = daemon.connect_node(spec).unwrap();

        let path = temp_path("basic");
        let handle = spawn_record(node, events, RecordArgs::new(&path));

        daemon
            .send_input(
                &recorder_id,
                &DataId::new("_record_0").unwrap(),
                Metadata::new(HlcTimestamp::new(10, 0)),
                vec![1, 2, 3],
            )
            .unwrap();
        daemon
            .send_input(
                &recorder_id,
                &DataId::new("in1").unwrap(),
                Metadata::new(HlcTimestamp::new(20, 0)),
                vec![4, 5],
            )
            .unwrap();
        assert_eq!(
            wait_for_recovered_count(&path, 2, Duration::from_secs(5)),
            2,
            "both inputs must be durable before Stop is allowed to end the loop"
        );
        daemon.stop(&recorder_id, StopCause::Requested).unwrap();
        handle.join().unwrap().unwrap();

        let mut reader = Reader::open(&path).unwrap();
        assert_eq!(reader.header().dataflow, daemon.dataflow());
        let entries: Vec<_> = reader.iter_all().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].node.as_str(), "camera");
        assert_eq!(entries[0].output.as_str(), "frames");
        assert_eq!(entries[0].payload, vec![1, 2, 3]);
        assert_eq!(entries[1].node.as_str(), "detector");
        assert_eq!(entries[1].output.as_str(), "boxes");
        assert_eq!(entries[1].payload, vec![4, 5]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn all_inputs_closed_also_finalizes_the_file() {
        let daemon = MockDaemon::start().unwrap();
        let spec = recorder_spec(&daemon, &[("_record_0", "camera", "frames")]);
        let recorder_id = NodeId::new("recorder").unwrap();
        let (node, events) = daemon.connect_node(spec).unwrap();

        let path = temp_path("all-inputs-closed");
        let handle = spawn_record(node, events, RecordArgs::new(&path));

        daemon
            .send_input(
                &recorder_id,
                &DataId::new("_record_0").unwrap(),
                Metadata::new(HlcTimestamp::new(1, 0)),
                vec![9],
            )
            .unwrap();
        assert_eq!(
            wait_for_recovered_count(&path, 1, Duration::from_secs(5)),
            1
        );
        daemon
            .send_event(&recorder_id, astrs_wire::NodeEvent::AllInputsClosed)
            .unwrap();
        handle.join().unwrap().unwrap();

        // A finalized file has a trailer, so a plain `open` (no recovery
        // fallback) must succeed.
        let reader = Reader::open(&path).unwrap();
        assert_eq!(reader.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_empty_recording_still_produces_a_valid_finalized_file() {
        let daemon = MockDaemon::start().unwrap();
        let spec = recorder_spec(&daemon, &[]);
        let recorder_id = NodeId::new("recorder").unwrap();
        let (mut node, mut events) = daemon.connect_node(spec).unwrap();
        daemon.stop(&recorder_id, StopCause::Requested).unwrap();

        let path = temp_path("empty");
        record(&mut node, &mut events, &RecordArgs::new(&path)).unwrap();

        let reader = Reader::open(&path).unwrap();
        assert!(reader.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rotation_flags_produce_numbered_segments() {
        let daemon = MockDaemon::start().unwrap();
        let spec = recorder_spec(&daemon, &[("_record_0", "camera", "frames")]);
        let recorder_id = NodeId::new("recorder").unwrap();
        let (node, events) = daemon.connect_node(spec).unwrap();

        let path = temp_path("rotate");
        let args = RecordArgs::new(&path).with_rotate_bytes(64);
        let handle = spawn_record(node, events, args);

        for seq in 0..20u8 {
            daemon
                .send_input(
                    &recorder_id,
                    &DataId::new("_record_0").unwrap(),
                    Metadata::new(HlcTimestamp::new(u64::from(seq) + 1, 0)),
                    vec![seq; 8],
                )
                .unwrap();
        }
        // Rotation moves entries into numbered segments the base path
        // itself never receives, so wait on the first segment rather
        // than the (never-written-to, under rotation) base path.
        let first_segment = path.with_file_name(format!(
            "{}-0001.{}",
            path.file_stem().unwrap().to_string_lossy(),
            path.extension().unwrap().to_string_lossy()
        ));
        assert!(
            wait_for_recovered_count(&first_segment, 1, Duration::from_secs(5)) >= 1,
            "the first segment must receive at least one entry before Stop"
        );
        daemon.stop(&recorder_id, StopCause::Requested).unwrap();
        handle.join().unwrap().unwrap();

        // The first numbered segment must exist and be finalized.
        assert!(first_segment.exists());
        let reader = Reader::open(&first_segment).unwrap();
        assert!(!reader.is_empty());

        // Clean up every segment this test produced.
        for sequence in 1..=10u32 {
            let segment = path.with_file_name(format!(
                "{}-{sequence:04}.{}",
                path.file_stem().unwrap().to_string_lossy(),
                path.extension().unwrap().to_string_lossy()
            ));
            if segment.exists() {
                let _ = std::fs::remove_file(&segment);
            }
        }
    }

    #[test]
    fn record_args_new_defaults_to_no_rotation() {
        let args = RecordArgs::new(std::env::temp_dir().join("x.arec"));
        assert!(args.rotate_bytes.is_none());
        assert!(args.rotate_seconds.is_none());
        assert!(!args.rotation_policy().is_enabled());
    }

    #[test]
    fn record_args_builders_set_the_expected_fields() {
        let args = RecordArgs::new(std::env::temp_dir().join("x.arec"))
            .with_rotate_bytes(100)
            .with_rotate_seconds(5);
        assert_eq!(args.rotate_bytes, Some(100));
        assert_eq!(args.rotate_seconds, Some(5));
        assert!(args.rotation_policy().is_enabled());
    }

    #[test]
    fn cli_parsing_accepts_the_positional_output_and_rotate_flags() {
        let args = RecordArgs::try_parse_from([
            "astrs-record-node",
            "out.arec",
            "--rotate-bytes",
            "1024",
            "--rotate-seconds",
            "60",
        ])
        .unwrap();
        assert_eq!(args.output, PathBuf::from("out.arec"));
        assert_eq!(args.rotate_bytes, Some(1024));
        assert_eq!(args.rotate_seconds, Some(60));
    }
}
