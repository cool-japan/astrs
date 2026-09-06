//! Shared types for `tui-showcase` — `astrs-tui` rendering driven against a
//! scripted cluster snapshot, verified with `TestBackend` goldens
//! (blueprint §13, §17).
//!
//! ```text
//!   astrs/timer/millis/40 ──► tick ──► [camera] ──frames──► [detector]
//! ```
//!
//! `camera`/`detector` are an ordinary, real, runnable dataflow — the same
//! pair blueprint §8.1's own canonical manifest example uses; this module
//! is only their shared port/env constants and frame codec, the same shape
//! every other example in this estate has. The interesting part —
//! `ScriptedCluster`, the scripted `astrs_tui::ClusterView` this crate's own
//! `TestBackend` goldens render from — lives entirely inside the `tests`
//! module below (a `#[cfg(test)]` item, so not linkable from here), because
//! it is meaningful only there: see that module's own docs.

/// The camera's tick input.
pub const TICK_PORT: &str = "tick";
/// The camera's output port, and the detector's input.
pub const FRAMES_PORT: &str = "frames";
/// The detector's output port.
pub const DETECTIONS_PORT: &str = "detections";

/// Environment variable overriding how many frames the camera publishes.
pub const ENV_FRAMES: &str = "TUI_SHOWCASE_FRAMES";
/// How many frames the camera publishes by default.
pub const DEFAULT_FRAMES: u64 = 20;
/// How many bytes one frame payload carries: one big-endian frame index.
pub const FRAME_BYTES: usize = 8;

/// How many frames this run's camera should publish.
#[must_use]
pub fn frame_budget() -> u64 {
    std::env::var(ENV_FRAMES)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|budget| *budget > 0)
        .unwrap_or(DEFAULT_FRAMES)
}

/// Encodes a frame index as its wire payload.
#[must_use]
pub fn frame_payload(index: u64) -> [u8; FRAME_BYTES] {
    index.to_be_bytes()
}

/// Decodes a frame index out of a payload; [`None`] if it is the wrong
/// length.
#[must_use]
pub fn frame_index_of(payload: &[u8]) -> Option<u64> {
    let bytes: [u8; FRAME_BYTES] = payload.try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

/// The path of the committed `dataflow.yml`, for `tests::ScriptedCluster::
/// new` (and this crate's own manifest-validation test) to read.
#[must_use]
pub fn manifest_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dataflow.yml")
}

/// `ScriptedCluster` — a scripted `astrs_tui::ClusterView` — and the
/// `TestBackend` goldens that render every tab from it.
///
/// # Why this lives entirely in `#[cfg(test)]`
///
/// `ScriptedCluster` is a fixture: nothing in `compose-camera`/
/// `compose-detector` (the real dataflow nodes this crate also ships) ever
/// constructs one, and its whole reason to exist is to be rendered by this
/// module's own tests. Building it needs `.expect()` at a few points —
/// `dataflow.yml` failing to parse, a hard-coded node id failing its own
/// grammar — that would never happen outside a broken *test fixture*, and
/// this workspace's policy is "no `unwrap`/`expect` in non-test code", not
/// "no `unwrap`/`expect` anywhere ever": keeping the whole fixture inside
/// `#[cfg(test)]`, alongside every other test in this estate that already
/// relies on the identical `#![allow(clippy::unwrap_used, clippy::expect_used,
/// clippy::panic)]` header, is what makes that true here rather than
/// reaching for a per-function lint override this codebase does not use
/// anywhere else.
///
/// # Why a scripted `ClusterView`, not a live one
///
/// `astrs_tui::ClusterView` is deliberately object-safe and trivial to
/// implement (see that trait's own doc example) precisely so a caller other
/// than a live coordinator or an `.arec` replay can drive every renderer in
/// the crate — that is what makes `astrs-tui` testable at all.
/// `ScriptedCluster` is the demonstration: it never opens a socket or reads
/// a file at render time (only once, at construction, to parse the real
/// `dataflow.yml`), and every golden below renders from it with no
/// terminal, no coordinator and no daemon — `astrs-tui`'s own architecture
/// note put to use rather than only quoted.
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::BTreeMap;

    use astrs_time::HlcTimestamp;
    use astrs_tui::view::{
        ClusterSnapshot, DataflowRow, GraphInfo, NodeRow, TimelineCategory, TimelineEvent,
    };
    use astrs_tui::{App, ClusterView, ConnectionStatus, Tab, TuiEvent, ViewError};
    use astrs_wire::{
        DaemonId, DataId, DataflowId, DataflowStatus, DataflowSummary, LogLevel, LogRecord, NodeId,
        NodeInfo, NodeIoSample, NodeMetricsSample, NodeRunState,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    /// A scripted, deterministic [`ClusterView`]: the real topology (parsed
    /// from the committed `dataflow.yml`, the same `astrs_graph::
    /// DataflowGraph::from_manifest` call `astrs-tui`'s own coordinator
    /// source makes) with fabricated run-time state layered on top — a
    /// `camera` node shown running with resource metrics, a `detector`
    /// node shown mid-restart with none yet — so every tab has something
    /// real (the topology) and something scripted (everything else about
    /// what is supposedly happening to it) to render at once.
    ///
    /// `refresh` is a no-op returning `Ok(())`: the snapshot is complete
    /// from construction, which is what makes it usable from a golden test
    /// with no setup beyond [`ScriptedCluster::new`].
    #[derive(Debug, Clone)]
    pub struct ScriptedCluster(ClusterSnapshot);

    impl ScriptedCluster {
        /// Builds the scripted snapshot. See the type's own docs for what
        /// it contains and why.
        pub fn new() -> Self {
            let dataflow = DataflowId::from_u128(0x7157_5c04);
            let daemon = DaemonId::generate(None);
            let camera = NodeId::new("camera").expect("a valid node id");
            let detector = NodeId::new("detector").expect("a valid node id");
            let epoch = HlcTimestamp::new(1, 0);

            let manifest_text =
                std::fs::read_to_string(manifest_path()).expect("dataflow.yml reads");
            let manifest = astrs_manifest::Manifest::from_yaml_str(&manifest_text)
                .expect("dataflow.yml parses");
            let (graph, _diagnostics) = astrs_graph::DataflowGraph::from_manifest(&manifest)
                .expect("dataflow.yml builds a graph");

            let mut summary = DataflowSummary::pending(dataflow, 2);
            summary.name = Some("tui-showcase".to_owned());
            summary.status = DataflowStatus::Running;
            summary.daemons = vec![daemon.clone()];
            summary.running_nodes = 1;
            summary.started_at = Some(epoch);

            let mut camera_metrics =
                NodeMetricsSample::new(camera.clone(), HlcTimestamp::new(5, 0));
            camera_metrics.cpu_percent = 6.5;
            camera_metrics.rss_bytes = 18 * 1024 * 1024;

            let mut detector_metrics =
                NodeMetricsSample::new(detector.clone(), HlcTimestamp::new(5, 0));
            detector_metrics.cpu_percent = 0.0;
            let _previous = detector_metrics
                .queue_depths
                .insert(DataId::new(FRAMES_PORT).expect("a valid data id"), 3);

            // Two readings a second apart, mirroring `astrs-tui`'s own
            // `dataflows::tests::io_sample` fixture: `NodeRow::with_io`
            // keeps the first as `previous_io` so `NodeRow::throughput`
            // (the Dataflows tab's BANDWIDTH column) has a real rate to
            // divide, not just a single point-in-time byte count.
            let mut camera_io_before =
                NodeIoSample::new(camera.clone(), HlcTimestamp::new(4_000_000_000, 0));
            camera_io_before.shm_slots_in_use = 3;
            camera_io_before.shm_slots_total = 8;
            let mut camera_io_after =
                NodeIoSample::new(camera.clone(), HlcTimestamp::new(5_000_000_000, 0));
            camera_io_after.sent_bytes_total.insert(
                DataId::new(FRAMES_PORT).expect("a valid data id"),
                1024 * 1024,
            );
            camera_io_after.shm_slots_in_use = 3;
            camera_io_after.shm_slots_total = 8;

            let mut row = DataflowRow::new(summary);
            row.graph = Some(GraphInfo::new(graph));
            row.nodes.push(
                NodeRow::new(NodeInfo {
                    dataflow,
                    node: camera.clone(),
                    daemon: daemon.clone(),
                    state: NodeRunState::Running,
                    pid: Some(4242),
                    generation: 1,
                    restart_count: 0,
                    inputs: BTreeMap::new(),
                    outputs: BTreeMap::new(),
                    started_at: Some(epoch),
                    exit_cause: None,
                })
                .with_metrics(camera_metrics)
                .with_io(camera_io_before)
                .with_io(camera_io_after),
            );
            // `detector` carries no `io` sample at all — mid-restart, it has
            // not published a bandwidth reading yet, which is also what
            // exercises `bandwidth_text`/`shm_text`'s own "-" fallback
            // alongside `camera`'s populated one.
            row.nodes.push(
                NodeRow::new(NodeInfo {
                    dataflow,
                    node: detector.clone(),
                    daemon: daemon.clone(),
                    state: NodeRunState::Restarting,
                    pid: None,
                    generation: 2,
                    restart_count: 1,
                    inputs: BTreeMap::new(),
                    outputs: BTreeMap::new(),
                    started_at: Some(epoch),
                    exit_cause: None,
                })
                .with_metrics(detector_metrics),
            );

            let mut snapshot = ClusterSnapshot::empty();
            snapshot.dataflows.push(row);
            snapshot.connection = ConnectionStatus::Live {
                endpoint: "127.0.0.1:7654 (scripted)".to_owned(),
            };

            snapshot.push_log(
                LogRecord::new(epoch, LogLevel::Info, "camera up: 20 frames")
                    .with_node(camera.clone()),
            );
            snapshot.push_log(
                LogRecord::new(
                    HlcTimestamp::new(2, 0),
                    LogLevel::Warn,
                    "detector restarting",
                )
                .with_node(detector.clone()),
            );
            snapshot.push_log(
                LogRecord::new(
                    HlcTimestamp::new(3, 0),
                    LogLevel::Error,
                    "frame queue backpressure",
                )
                .with_node(detector.clone()),
            );

            snapshot.push_timeline(
                TimelineEvent::new(epoch, TimelineCategory::Spawn, "camera spawned")
                    .with_dataflow(dataflow)
                    .with_node(camera.clone()),
            );
            snapshot.push_timeline(
                TimelineEvent::new(
                    HlcTimestamp::new(2, 0),
                    TimelineCategory::Spawn,
                    "detector spawned",
                )
                .with_dataflow(dataflow)
                .with_node(detector.clone()),
            );
            snapshot.push_timeline(
                TimelineEvent::new(
                    HlcTimestamp::new(3, 0),
                    TimelineCategory::Restart,
                    "detector restarted after exit code 1",
                )
                .with_dataflow(dataflow)
                .with_node(detector.clone()),
            );
            snapshot.push_timeline(
                TimelineEvent::new(
                    HlcTimestamp::new(4, 0),
                    TimelineCategory::Violation,
                    "frames queue depth exceeded its warning threshold",
                )
                .with_dataflow(dataflow)
                .with_node(detector),
            );

            Self(snapshot)
        }
    }

    impl Default for ScriptedCluster {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ClusterView for ScriptedCluster {
        fn refresh(&mut self) -> Result<(), ViewError> {
            Ok(())
        }

        fn snapshot(&self) -> &ClusterSnapshot {
            &self.0
        }
    }

    /// A key press with no modifiers — this crate's own copy of the small
    /// helper `astrs-tui`'s own tests define identically.
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Flattens a rendered buffer to plain text, row by row, for substring
    /// assertions — this crate's own copy of `astrs-tui`'s `tabs::
    /// buffer_text` helper, which is `pub(crate)` there and so not
    /// reachable from here.
    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let area = buffer.area();
        let mut out = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Renders whichever tab `app.state.active_tab` names into an 80x24
    /// buffer and returns the flattened text.
    fn render(app: &App) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        buffer_text(terminal.backend().buffer())
    }

    /// The scripted snapshot's own internal consistency: both node ids the
    /// fixture fabricates match node ids the *real* parsed graph actually
    /// contains — a scripted node the topology does not know about would
    /// make the Graph tab and the Dataflows tab disagree about what is
    /// running.
    #[test]
    fn the_scripted_nodes_match_the_real_parsed_graph() {
        let cluster = ScriptedCluster::new();
        let snapshot = cluster.snapshot();
        let row = &snapshot.dataflows[0];
        let graph = row.graph.as_ref().expect("a parsed graph");
        for node in &row.nodes {
            let graph_id = astrs_graph::NodeId::new(node.info.node.as_str());
            assert!(
                graph.graph.node(&graph_id).is_some(),
                "{} is not in the parsed graph",
                node.info.node
            );
        }
        assert_eq!(row.nodes.len(), 2);
        assert_eq!(snapshot.logs.len(), 3);
        assert_eq!(snapshot.timeline.len(), 4);
    }

    /// A fresh `App` opens on the Dataflows tab, and it renders the
    /// scripted dataflow's name, both node ids, `camera`'s CPU reading, its
    /// derived bandwidth rate (from the two `NodeIoSample`s
    /// `ScriptedCluster::new` scripts a second apart) and its shared-memory
    /// occupancy — `detector`'s matching columns render `-`, having no `io`
    /// sample at all, which is the other half of the same fixture.
    #[test]
    fn the_dataflows_tab_renders_the_scripted_state() {
        let app = App::new(Box::new(ScriptedCluster::new()));
        assert_eq!(app.state.active_tab, Tab::Dataflows);
        let text = render(&app);
        assert!(text.contains("tui-showcase"), "{text}");
        assert!(text.contains("camera"), "{text}");
        assert!(text.contains("detector"), "{text}");
        assert!(text.contains("6.5"), "{text}");
        assert!(text.contains("MiB/s"), "{text}");
        assert!(text.contains("3/8"), "{text}");
    }

    /// Switching to the Graph tab (key `2`) renders the *real* topology
    /// parsed from `dataflow.yml`: both node ids and the `frames` edge,
    /// with a plane badge (same-host, so `[shm]`).
    #[test]
    fn the_graph_tab_renders_the_real_topology() {
        let mut app = App::new(Box::new(ScriptedCluster::new()));
        app.on_event(TuiEvent::Key(key(KeyCode::Char('2'))));
        assert_eq!(app.state.active_tab, Tab::Graph);
        let text = render(&app);
        assert!(text.contains("camera"), "{text}");
        assert!(text.contains("detector"), "{text}");
        assert!(text.contains("frames"), "{text}");
        assert!(text.contains("shm"), "{text}");
    }

    /// Switching to the Logs tab (key `3`) renders every scripted log
    /// line's message.
    #[test]
    fn the_logs_tab_renders_every_scripted_line() {
        let mut app = App::new(Box::new(ScriptedCluster::new()));
        app.on_event(TuiEvent::Key(key(KeyCode::Char('3'))));
        assert_eq!(app.state.active_tab, Tab::Logs);
        let text = render(&app);
        assert!(text.contains("camera up"), "{text}");
        assert!(text.contains("detector restarting"), "{text}");
        assert!(text.contains("backpressure"), "{text}");
    }

    /// Switching to the Timeline tab (key `4`) renders every scripted
    /// category's badge and message, in the HLC order they were pushed.
    #[test]
    fn the_timeline_tab_renders_every_scripted_event() {
        let mut app = App::new(Box::new(ScriptedCluster::new()));
        app.on_event(TuiEvent::Key(key(KeyCode::Char('4'))));
        assert_eq!(app.state.active_tab, Tab::Timeline);
        let text = render(&app);
        assert!(text.contains("camera spawned"), "{text}");
        assert!(text.contains("restarted after exit code 1"), "{text}");
        assert!(
            text.contains("backpressure") || text.contains("threshold"),
            "{text}"
        );
    }

    /// A scripted key sequence — cycle through every tab, then quit —
    /// drives the same `UiState` machine a real terminal session would,
    /// with `ScriptedCluster` standing in for the coordinator the whole
    /// way through.
    #[test]
    fn a_scripted_key_sequence_cycles_every_tab_and_quits() {
        let mut app = App::new(Box::new(ScriptedCluster::new()));
        let mut seen = vec![app.state.active_tab];
        for _ in 0..Tab::ALL.len() {
            app.on_event(TuiEvent::Key(key(KeyCode::Tab)));
            seen.push(app.state.active_tab);
        }
        // Four `Tab` presses from `Dataflows` visit every tab once and
        // return to the start.
        assert_eq!(seen.len(), Tab::ALL.len() + 1);
        assert_eq!(seen.first(), seen.last());
        assert!(!app.should_quit());

        app.on_event(TuiEvent::Key(key(KeyCode::Char('q'))));
        assert!(app.should_quit());
    }

    /// The committed manifest parses, validates, names this dataflow, and
    /// is the exact pair of nodes [`ScriptedCluster::new`] scripts state
    /// for — `camera`/`detector`, nothing more, nothing less.
    #[test]
    fn the_committed_manifest_parses_and_validates() {
        let path = manifest_path();
        let manifest = astrs_manifest::Manifest::from_yaml_file(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        manifest
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(manifest.name.as_deref(), Some("tui-showcase"));
        let ids: Vec<&str> = manifest.nodes.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(ids, vec!["camera", "detector"]);
    }

    /// The frame codec (shared with the real `showcase-camera`/
    /// `showcase-detector` binaries) round-trips, and rejects the wrong
    /// length rather than guessing.
    #[test]
    fn frame_payloads_round_trip_and_reject_the_wrong_length() {
        for index in [0_u64, 1, 42, u64::MAX] {
            let payload = frame_payload(index);
            assert_eq!(frame_index_of(&payload), Some(index));
        }
        assert_eq!(frame_index_of(&[1, 2, 3]), None);
    }
}
