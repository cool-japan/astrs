//! [`ReplaySource`]: a [`crate::ClusterView`] fed by an `.arec` recording
//! (blueprint §14), for `astrs top --replay`.
//!
//! # What a replay can and cannot show
//!
//! A recording captures *data messages* — `{node, output, hlc, meta,
//! payload}` — not daemon-side lifecycle events. So [`ReplaySource`] is
//! deliberately reduced-fidelity next to a live
//! [`crate::view::coordinator::CoordinatorSource`]:
//!
//! - **Dataflows/Graph** come from the header's embedded, already-expanded
//!   manifest ([`astrs_recording::Header::manifest_yaml`]) — exactly the
//!   same parse path [`crate::view::coordinator`] uses for
//!   `GetManifest`'s YAML, just read from a file instead of the wire. Node
//!   rows show no live process state (there is none to show): state is
//!   always [`astrs_wire::NodeRunState::Finished`] and metrics are always
//!   `None`.
//! - **Logs** stays empty. A `astrs/logs/*` virtual source is just another
//!   recorded port with no documented payload convention this crate can
//!   decode yet — showing nothing is honest; guessing a format would not
//!   be.
//! - **Timeline** is populated from the recording itself: every entry
//!   [`ReplaySource::refresh`] advances past becomes one
//!   [`TimelineCategory::Replay`] event, in the file's own HLC order.
//!
//! # Pace
//!
//! [`ReplaySource::refresh`] delivers exactly one entry per call — it does
//! not attempt to reproduce the recording's original inter-arrival timing
//! against wall-clock time. `astrs replay` (blueprint §14) is the tool for
//! byte- and timing-accurate replay into a live graph; this view exists to
//! browse a recording's shape at the render loop's own tick rate, and
//! keeping the two decoupled is also what makes this module's tests run in
//! milliseconds rather than needing to wait out a played-back session.

use std::collections::BTreeMap;
use std::path::Path;

use astrs_recording::{IndexEntry, Reader, RecordingError};
use astrs_wire::{
    DaemonId, DataId, DataflowStatus, DataflowSummary, NodeId, NodeInfo, NodeRunState,
};

use crate::view::{
    ClusterSnapshot, ClusterView, ConnectionStatus, DataflowRow, NodeRow, TimelineCategory,
    TimelineEvent, ViewError, graph_info_from_manifest, parse_manifest,
};

/// Why a [`ReplaySource`] could not be opened.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReplaySourceError {
    /// The `.arec` file could not be opened or read at all.
    #[error("cannot open recording: {0}")]
    Recording(#[from] RecordingError),
}

/// A [`ClusterView`] that plays an `.arec` recording back, one entry at a
/// time, with no coordinator involved.
pub struct ReplaySource {
    reader: Reader,
    /// The recording's entries, sorted into the same HLC order (with the
    /// same node/output/position tie-break)
    /// [`astrs_recording::Reader::iter_all`] uses — precomputed once so
    /// [`ReplaySource::refresh`] is a single seek-and-decode rather than
    /// re-sorting the whole index every tick.
    order: Vec<IndexEntry>,
    cursor: usize,
    path_display: String,
    snapshot: ClusterSnapshot,
}

impl ReplaySource {
    /// Opens `path`, falling back to a full scan
    /// ([`astrs_recording::Reader::open_or_recover`]) if the file's
    /// trailer is missing or untrustworthy — a monitoring tool should not
    /// refuse to show a recording that a killed recorder process left
    /// without a clean footer.
    ///
    /// # Errors
    ///
    /// [`ReplaySourceError::Recording`] if the file cannot be read at all.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ReplaySourceError> {
        let path = path.as_ref();
        let (reader, _report) = Reader::open_or_recover(path)?;
        let order = hlc_order(reader.index());
        let dataflow_row = build_dataflow_row(&reader);
        let mut snapshot = ClusterSnapshot::empty();
        snapshot.dataflows.push(dataflow_row);
        let total = order.len();
        snapshot.connection = ConnectionStatus::Replay {
            path: path.display().to_string(),
            position: 0,
            total: Some(total),
        };
        Ok(Self {
            reader,
            order,
            cursor: 0,
            path_display: path.display().to_string(),
            snapshot,
        })
    }

    /// How many of the recording's entries have been delivered so far.
    #[must_use]
    pub const fn position(&self) -> usize {
        self.cursor
    }

    /// The recording's total entry count.
    #[must_use]
    pub fn total(&self) -> usize {
        self.order.len()
    }

    /// Whether every entry has been delivered.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.cursor >= self.order.len()
    }
}

impl ClusterView for ReplaySource {
    fn refresh(&mut self) -> Result<(), ViewError> {
        let Some(target) = self.order.get(self.cursor).cloned() else {
            return Ok(());
        };
        let entry = self
            .reader
            .read_at(&target)
            .map_err(|error| ViewError::Replay(error.to_string()))?;
        self.cursor += 1;

        let dataflow = self.reader.header().dataflow;
        let message = format!(
            "replayed {}/{} ({} byte payload)",
            entry.node,
            entry.output,
            entry.payload.len()
        );
        let mut event = TimelineEvent::new(entry.hlc(), TimelineCategory::Replay, message)
            .with_dataflow(dataflow);
        event.node = Some(entry.node);
        self.snapshot.push_timeline(event);

        self.snapshot.connection = ConnectionStatus::Replay {
            path: self.path_display.clone(),
            position: self.cursor,
            total: Some(self.order.len()),
        };
        Ok(())
    }

    fn snapshot(&self) -> &ClusterSnapshot {
        &self.snapshot
    }
}

/// Sorts `index` into the same HLC order (ties broken by node, then
/// output, then original on-disk position) that
/// [`astrs_recording::Reader::iter_all`] delivers — duplicated rather than
/// reused because that ordering is a private helper of `astrs-recording`'s
/// [`Reader`]; [`Reader::index`] is the public seam this crate is allowed
/// to build on.
fn hlc_order(index: &[IndexEntry]) -> Vec<IndexEntry> {
    let mut entries: Vec<(usize, IndexEntry)> = index.iter().cloned().enumerate().collect();
    entries.sort_by(|(position_a, a), (position_b, b)| {
        a.hlc
            .cmp(&b.hlc)
            .then_with(|| a.node.as_str().cmp(b.node.as_str()))
            .then_with(|| a.output.as_str().cmp(b.output.as_str()))
            .then_with(|| position_a.cmp(position_b))
    });
    entries.into_iter().map(|(_, entry)| entry).collect()
}

/// Builds the single [`DataflowRow`] a replay shows, from the recording's
/// header alone.
fn build_dataflow_row(reader: &Reader) -> DataflowRow {
    let header = reader.header();
    let manifest = parse_manifest(&header.manifest_yaml);

    let node_count = manifest.as_ref().map_or(0, |manifest| manifest.nodes.len());
    let summary = DataflowSummary {
        id: header.dataflow,
        name: manifest.as_ref().and_then(|manifest| manifest.name.clone()),
        status: DataflowStatus::Finished,
        daemons: Vec::new(),
        node_count: u32::try_from(node_count).unwrap_or(u32::MAX),
        running_nodes: 0,
        started_at: Some(header.hlc_epoch),
    };
    let mut row = DataflowRow::new(summary);

    let Some(manifest) = manifest else {
        return row;
    };

    row.nodes = manifest
        .nodes
        .iter()
        .filter_map(|node| node_row_from_manifest(&header.dataflow, node, header.hlc_epoch))
        .collect();
    row.graph = graph_info_from_manifest(&manifest);
    row
}

/// Builds one [`NodeRow`] from a manifest node — best-effort: a port name
/// that fails [`DataId`]'s validation is skipped rather than failing the
/// whole row, since a manifest that reached a recording already passed
/// `astrs-manifest`'s own validation once.
fn node_row_from_manifest(
    dataflow: &astrs_wire::DataflowId,
    node: &astrs_manifest::Node,
    started_at: astrs_time::HlcTimestamp,
) -> Option<NodeRow> {
    let node_id = NodeId::new(node.id.clone()).ok()?;
    let inputs: BTreeMap<DataId, Option<astrs_wire::TypeUrn>> = node
        .inputs
        .keys()
        .filter_map(|name| DataId::new(name.clone()).ok())
        .map(|id| {
            let urn = node
                .input_types
                .get(id.as_str())
                .and_then(|urn| astrs_wire::TypeUrn::new(urn.as_str()).ok());
            (id, urn)
        })
        .collect();
    let outputs: BTreeMap<DataId, Option<astrs_wire::TypeUrn>> = node
        .outputs
        .iter()
        .filter_map(|name| DataId::new(name.clone()).ok())
        .map(|id| {
            let urn = node
                .output_types
                .get(id.as_str())
                .and_then(|urn| astrs_wire::TypeUrn::new(urn.as_str()).ok());
            (id, urn)
        })
        .collect();

    Some(NodeRow {
        info: NodeInfo {
            dataflow: *dataflow,
            node: node_id,
            daemon: DaemonId::generate(None),
            state: NodeRunState::Finished,
            pid: None,
            generation: 1,
            restart_count: 0,
            inputs,
            outputs,
            started_at: Some(started_at),
            exit_cause: None,
        },
        metrics: None,
        io: None,
        previous_io: None,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use astrs_recording::{Entry, Writer, WriterOptions};
    use astrs_time::HlcTimestamp;
    use astrs_wire::{DataflowId, Metadata};

    use super::*;

    fn temp_path(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        std::env::temp_dir().join(format!(
            "astrs-tui-replay-test-{}-{}-{label}.arec",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    const MANIFEST_YAML: &str = "\
name: perception-demo
nodes:
  - id: camera
    path: ./camera
    outputs: [frames]
  - id: detector
    path: ./detector
    inputs:
      frames: camera/frames
";

    fn write_sample(path: &Path, entries: &[(&str, &str, u64)]) {
        let options = WriterOptions::new(DataflowId::from_u128(1), HlcTimestamp::new(1, 0))
            .with_manifest_yaml(MANIFEST_YAML);
        let mut writer = Writer::create(path, options).unwrap();
        for (node, output, hlc) in entries {
            writer
                .append(Entry::new(
                    NodeId::new(*node).unwrap(),
                    DataId::new(*output).unwrap(),
                    Metadata::new(HlcTimestamp::new(*hlc, 0)),
                    vec![1, 2, 3],
                ))
                .unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn opening_a_replay_builds_the_dataflow_and_graph_from_the_header() {
        let path = temp_path("open");
        write_sample(&path, &[("camera", "frames", 10)]);

        let source = ReplaySource::open(&path).unwrap();
        let snapshot = source.snapshot();
        assert_eq!(snapshot.dataflows.len(), 1);
        let row = &snapshot.dataflows[0];
        assert_eq!(row.summary.name.as_deref(), Some("perception-demo"));
        assert_eq!(row.summary.status, DataflowStatus::Finished);
        assert_eq!(row.nodes.len(), 2);
        assert!(row.graph.is_some());
        assert!(matches!(
            snapshot.connection,
            ConnectionStatus::Replay { position: 0, .. }
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn refresh_delivers_one_entry_per_call_in_hlc_order() {
        let path = temp_path("order");
        write_sample(
            &path,
            &[
                ("camera", "frames", 30),
                ("camera", "frames", 10),
                ("detector", "boxes", 20),
            ],
        );
        let mut source = ReplaySource::open(&path).unwrap();
        assert_eq!(source.total(), 3);
        assert!(!source.is_finished());

        source.refresh().unwrap();
        assert_eq!(source.position(), 1);
        assert_eq!(source.snapshot().timeline.len(), 1);
        assert_eq!(
            source.snapshot().timeline[0].timestamp,
            HlcTimestamp::new(10, 0),
            "the earliest HLC entry must come first regardless of write order"
        );

        source.refresh().unwrap();
        source.refresh().unwrap();
        assert_eq!(source.position(), 3);
        assert!(source.is_finished());
        let stamps: Vec<u64> = source
            .snapshot()
            .timeline
            .iter()
            .map(|event| event.timestamp.physical_ns())
            .collect();
        assert_eq!(stamps, vec![10, 20, 30]);

        // Once exhausted, refreshing again is a harmless no-op.
        source.refresh().unwrap();
        assert_eq!(source.position(), 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn refresh_updates_the_connection_status_position() {
        let path = temp_path("status");
        write_sample(&path, &[("a", "x", 1), ("a", "x", 2)]);
        let mut source = ReplaySource::open(&path).unwrap();
        source.refresh().unwrap();
        match &source.snapshot().connection {
            ConnectionStatus::Replay {
                position, total, ..
            } => {
                assert_eq!(*position, 1);
                assert_eq!(*total, Some(2));
            }
            other => panic!("expected Replay status, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_empty_recording_opens_with_nothing_to_replay() {
        let path = temp_path("empty");
        let options = WriterOptions::new(DataflowId::from_u128(2), HlcTimestamp::EPOCH);
        let mut writer = Writer::create(&path, options).unwrap();
        writer.finish().unwrap();

        let mut source = ReplaySource::open(&path).unwrap();
        assert!(source.is_finished());
        source.refresh().unwrap();
        assert!(source.snapshot().timeline.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_recording_with_no_manifest_still_opens_with_a_bare_dataflow_row() {
        let path = temp_path("no-manifest");
        let options = WriterOptions::new(DataflowId::from_u128(3), HlcTimestamp::EPOCH);
        let mut writer = Writer::create(&path, options).unwrap();
        writer.finish().unwrap();

        let source = ReplaySource::open(&path).unwrap();
        let row = &source.snapshot().dataflows[0];
        assert_eq!(row.summary.id, DataflowId::from_u128(3));
        assert!(row.nodes.is_empty());
        assert!(row.graph.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn opening_a_missing_file_reports_a_recording_error() {
        // `ReplaySource` is not `Debug` (it holds `astrs_recording::Reader`,
        // which is not either), so this matches on the `Result` directly
        // rather than via `unwrap_err`.
        match ReplaySource::open(temp_path("does-not-exist")) {
            Err(error) => {
                assert!(matches!(error, ReplaySourceError::Recording(_)));
                assert!(error.to_string().contains("recording"));
            }
            Ok(_) => panic!("expected opening a missing file to fail"),
        }
    }
}
