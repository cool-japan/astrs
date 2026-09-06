//! The Dataflows tab: the dataflow list, and the selected dataflow's
//! per-node status/restarts/CPU/RSS/queue-depth table (blueprint §13).

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

use crate::app::UiState;
use crate::theme;
use crate::view::{ClusterSnapshot, DataflowRow, NodeRow};

/// Renders the dataflow list (top), the selected dataflow's node table
/// (middle), and the selected node's full queue-depth breakdown (bottom
/// strip — blueprint §13's "queue depths," plural: the node table's own
/// "DEEPEST QUEUE" column names only the bottleneck input, so every input's
/// depth lives here instead of being lost).
pub fn render(frame: &mut Frame, area: Rect, snapshot: &ClusterSnapshot, state: &UiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(35),
            Constraint::Percentage(55),
            Constraint::Length(1),
        ])
        .split(area);

    render_dataflow_table(frame, chunks[0], snapshot, state);
    render_node_table(frame, chunks[1], snapshot, state);
    render_selected_node_queues(frame, chunks[2], snapshot, state);
}

/// The top table: one row per known dataflow.
fn render_dataflow_table(
    frame: &mut Frame,
    area: Rect,
    snapshot: &ClusterSnapshot,
    state: &UiState,
) {
    let header = Row::new(["NAME", "STATUS", "NODES", "RUNNING", "DAEMONS"])
        .style(Style::new().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = snapshot
        .dataflows
        .iter()
        .enumerate()
        .map(|(index, row)| dataflow_row(row, index == state.selected_dataflow))
        .collect();

    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new("no dataflows (waiting for `astrs list`)")
                .style(super::placeholder_style())
                .block(Block::default().borders(Borders::ALL).title("Dataflows")),
            area,
        );
        return;
    }

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(30),
            Constraint::Percentage(15),
            Constraint::Percentage(10),
            Constraint::Percentage(10),
            Constraint::Percentage(35),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title("Dataflows"));
    frame.render_widget(table, area);
}

/// One dataflow's table row.
fn dataflow_row(row: &DataflowRow, selected: bool) -> Row<'static> {
    let daemons = if row.summary.daemons.is_empty() {
        "-".to_owned()
    } else {
        row.summary
            .daemons
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    let cells = [
        Cell::from(row.summary.display_name()),
        Cell::from(row.summary.status.as_str())
            .style(theme::dataflow_status_style(row.summary.status)),
        Cell::from(row.summary.node_count.to_string()),
        Cell::from(row.summary.running_nodes.to_string()),
        Cell::from(daemons),
    ];
    let base = Row::new(cells);
    if selected {
        base.style(Style::new().add_modifier(Modifier::REVERSED))
    } else {
        base
    }
}

/// The bottom table: one row per node of the selected dataflow.
fn render_node_table(frame: &mut Frame, area: Rect, snapshot: &ClusterSnapshot, state: &UiState) {
    let Some(row) = state.selected_dataflow_row(snapshot) else {
        frame.render_widget(
            Paragraph::new("no dataflow selected")
                .style(super::placeholder_style())
                .block(Block::default().borders(Borders::ALL).title("Nodes")),
            area,
        );
        return;
    };

    let header = Row::new([
        "NODE",
        "STATE",
        "RESTARTS",
        "CPU%",
        "RSS",
        "BANDWIDTH",
        "SHM",
        "DEEPEST QUEUE",
    ])
    .style(Style::new().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = row
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| node_row(node, index == state.selected_node))
        .collect();

    let title = format!("Nodes — {}", row.summary.display_name());
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new("no node detail yet")
                .style(super::placeholder_style())
                .block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
        return;
    }

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(17),
            Constraint::Percentage(10),
            Constraint::Percentage(8),
            Constraint::Percentage(6),
            Constraint::Percentage(10),
            Constraint::Percentage(23),
            Constraint::Percentage(8),
            Constraint::Percentage(18),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(table, area);
}

/// One node's table row.
fn node_row(node: &NodeRow, selected: bool) -> Row<'static> {
    let (cpu, rss, queue) = node.metrics.as_ref().map_or_else(
        || ("-".to_owned(), "-".to_owned(), "-".to_owned()),
        |sample| {
            (
                format!("{:.1}", sample.cpu_percent),
                format_bytes(sample.rss_bytes),
                deepest_queue_text(sample),
            )
        },
    );
    let cells = [
        Cell::from(node.info.node.as_str().to_owned()),
        Cell::from(node.info.state.as_str()).style(theme::node_state_style(node.info.state)),
        Cell::from(node.info.restart_count.to_string()),
        Cell::from(cpu),
        Cell::from(rss),
        Cell::from(bandwidth_text(node)),
        Cell::from(shm_text(node)),
        Cell::from(queue),
    ];
    let base = Row::new(cells);
    if selected {
        base.style(Style::new().add_modifier(Modifier::REVERSED))
    } else {
        base
    }
}

/// Renders a byte count as a human-scaled string, e.g. `32.0 MiB`.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// One node's throughput as `^out v in`, or `-` until two samples exist
/// (§13).
///
/// Both directions in one cell rather than two columns: a node's egress and
/// its ingress are read *together* — a consumer whose ingress collapsed while
/// its producer's egress held is the shape of a backed-up queue — and side by
/// side is what makes the comparison free. The arrows are ASCII on purpose:
/// this cell is rendered into whatever terminal an operator has, including
/// the serial console on a robot.
#[must_use]
fn bandwidth_text(node: &NodeRow) -> String {
    node.throughput().map_or_else(
        || "-".to_owned(),
        |delta| {
            format!(
                "^{} v{}",
                format_rate(delta.sent_bytes_per_second()),
                format_rate(delta.received_bytes_per_second())
            )
        },
    )
}

/// A byte *rate*, human-scaled, e.g. `1.5 MiB/s`.
///
/// Sub-byte rates render as `0 B/s` rather than `0.0009 B/s`: a graph that
/// published one message ten minutes ago is idle, and saying so in four
/// characters leaves the column readable.
#[must_use]
pub fn format_rate(bytes_per_second: f64) -> String {
    if !bytes_per_second.is_finite() || bytes_per_second < 1.0 {
        return "0 B/s".to_owned();
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "guarded above: finite and >= 1.0, and the value is a display figure"
    )]
    let whole = bytes_per_second as u64;
    format!("{}/s", format_bytes(whole))
}

/// One node's shared-memory occupancy as `in_use/total`, or `-` when it
/// produces no rings at all (§6.2).
///
/// `-` and `0/8` are different facts — the first means this node's routes are
/// on the daemon path, the second means they are on the shared-memory plane
/// and idle — and collapsing them would hide the moment a route was
/// downgraded off the fast plane, which is exactly what an operator watching
/// this column is looking for.
#[must_use]
fn shm_text(node: &NodeRow) -> String {
    match node.io.as_ref() {
        Some(io) if io.shm_slots_total > 0 => {
            format!("{}/{}", io.shm_slots_in_use, io.shm_slots_total)
        }
        _ => "-".to_owned(),
    }
}

/// The deepest queue's `input:depth` text, or `-` if the sample has no
/// queues (or reports no depth at all).
#[must_use]
fn deepest_queue_text(sample: &astrs_wire::NodeMetricsSample) -> String {
    sample
        .deepest_queue()
        .map_or_else(|| "-".to_owned(), |(id, depth)| format!("{id}:{depth}"))
}

/// The one-line detail strip: every one of the selected node's queue
/// depths, not just the deepest one the table column names.
///
/// `astrs_wire::NodeMetricsSample::queue_depths` is a
/// `BTreeMap<DataId, u64>` keyed by input name, so iterating it in key
/// order (rather than depth order) is what keeps this line's layout
/// stable frame to frame — an operator watching for "which input backed
/// up" benefits far more from each name staying in the same place than
/// from the busiest one floating to the front.
fn render_selected_node_queues(
    frame: &mut Frame,
    area: Rect,
    snapshot: &ClusterSnapshot,
    state: &UiState,
) {
    let text = state
        .selected_dataflow_row(snapshot)
        .and_then(|row| row.nodes.get(state.selected_node))
        .map_or_else(
            || "select a node to see its full queue-depth breakdown".to_owned(),
            queue_detail_text,
        );
    frame.render_widget(Paragraph::new(text).style(super::placeholder_style()), area);
}

/// The full `node: input:depth  input:depth  …` line for one [`NodeRow`].
#[must_use]
fn queue_detail_text(node: &NodeRow) -> String {
    match &node.metrics {
        None => format!("{}: no metrics sample yet", node.info.node),
        Some(sample) if sample.queue_depths.is_empty() => {
            format!("{}: no open input queues", node.info.node)
        }
        Some(sample) => {
            let parts: Vec<String> = sample
                .queue_depths
                .iter()
                .map(|(id, depth)| format!("{id}:{depth}"))
                .collect();
            format!("{} queues — {}", node.info.node, parts.join("  "))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::BTreeMap;

    use astrs_wire::{
        DaemonId, DataId, DataflowId, DataflowSummary, NodeId, NodeInfo, NodeMetricsSample,
        NodeRunState,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::app::UiState;

    #[test]
    fn format_bytes_scales_to_the_nearest_unit() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(32 * 1024 * 1024), "32.0 MiB");
        assert_eq!(format_bytes(1024), "1.0 KiB");
    }

    fn node_info(id: &str, state: NodeRunState, restarts: u32) -> NodeInfo {
        NodeInfo {
            dataflow: DataflowId::from_u128(1),
            node: NodeId::new(id).unwrap(),
            daemon: DaemonId::generate(None),
            state,
            pid: Some(100),
            generation: 1,
            restart_count: restarts,
            inputs: BTreeMap::new(),
            outputs: BTreeMap::new(),
            started_at: None,
            exit_cause: None,
        }
    }

    /// A bandwidth sample for `camera` at `physical_ns`, with the given
    /// cumulative byte totals.
    fn io_sample(physical_ns: u64, sent: u64, received: u64) -> astrs_wire::NodeIoSample {
        let mut io = astrs_wire::NodeIoSample::new(
            NodeId::new("camera").unwrap(),
            astrs_time::HlcTimestamp::new(physical_ns, 0),
        );
        io.sent_bytes_total
            .insert(DataId::new("frames").unwrap(), sent);
        io.received_bytes_total
            .insert(DataId::new("tick").unwrap(), received);
        io.shm_slots_in_use = 3;
        io.shm_slots_total = 8;
        io
    }

    fn sample(cpu: f32, rss: u64) -> NodeMetricsSample {
        let mut sample = NodeMetricsSample::new(
            NodeId::new("camera").unwrap(),
            astrs_time::HlcTimestamp::EPOCH,
        );
        sample.cpu_percent = cpu;
        sample.rss_bytes = rss;
        sample
            .queue_depths
            .insert(DataId::new("frames").unwrap(), 4);
        sample
    }

    #[test]
    fn deepest_queue_text_reports_the_busiest_input() {
        assert_eq!(deepest_queue_text(&sample(1.0, 1)), "frames:4");
        let empty =
            NodeMetricsSample::new(NodeId::new("n").unwrap(), astrs_time::HlcTimestamp::EPOCH);
        assert_eq!(deepest_queue_text(&empty), "-");
    }

    fn fixture_snapshot() -> ClusterSnapshot {
        let mut snapshot = ClusterSnapshot::empty();
        let mut row = DataflowRow::new(DataflowSummary {
            id: DataflowId::from_u128(1),
            name: Some("perception".to_owned()),
            status: astrs_wire::DataflowStatus::Running,
            daemons: Vec::new(),
            node_count: 2,
            running_nodes: 2,
            started_at: None,
        });
        row.nodes.push(
            NodeRow::new(node_info("camera", NodeRunState::Running, 0))
                .with_metrics(sample(12.5, 32 * 1024 * 1024))
                // Two readings a second apart: 1 MiB out, 512 KiB in.
                .with_io(io_sample(0, 0, 0))
                .with_io(io_sample(1_000_000_000, 1024 * 1024, 512 * 1024)),
        );
        row.nodes.push(NodeRow::new(node_info(
            "detector",
            NodeRunState::Restarting,
            3,
        )));
        snapshot.dataflows.push(row);
        snapshot
    }

    #[test]
    fn the_dataflows_tab_renders_the_selected_nodes_metrics() {
        let snapshot = fixture_snapshot();
        let state = UiState::new();
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &snapshot, &state))
            .unwrap();
        let rendered = super::super::buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("perception"));
        assert!(rendered.contains("camera"));
        assert!(rendered.contains("detector"));
        assert!(rendered.contains("12.5"));
        assert!(rendered.contains("frames:4"));
    }

    #[test]
    fn the_bandwidth_column_reports_a_rate_derived_from_two_samples() {
        // 1 MiB out and 512 KiB in over exactly one second of HLC physical
        // time — the rate's denominator is the samples' own clock, never the
        // polling cadence, so a late poll changes nothing here.
        let node = NodeRow::new(node_info("camera", NodeRunState::Running, 0))
            .with_io(io_sample(0, 0, 0))
            .with_io(io_sample(1_000_000_000, 1024 * 1024, 512 * 1024));
        assert_eq!(bandwidth_text(&node), "^1.0 MiB/s v512.0 KiB/s");
        assert_eq!(shm_text(&node), "3/8");
    }

    #[test]
    fn one_sample_is_not_yet_a_rate() {
        let node = NodeRow::new(node_info("camera", NodeRunState::Running, 0)).with_io(io_sample(
            1_000_000_000,
            1024 * 1024,
            0,
        ));
        assert_eq!(
            bandwidth_text(&node),
            "-",
            "a cumulative total on its own says nothing about a rate"
        );
    }

    #[test]
    fn a_node_with_no_rings_is_not_reported_as_an_empty_one() {
        let mut io = io_sample(1_000_000_000, 0, 0);
        io.shm_slots_in_use = 0;
        io.shm_slots_total = 0;
        let node = NodeRow::new(node_info("camera", NodeRunState::Running, 0)).with_io(io);
        assert_eq!(
            shm_text(&node),
            "-",
            "`-` means the daemon path; `0/8` would mean an idle ring, and \
             collapsing the two hides a route being downgraded"
        );
    }

    #[test]
    fn a_sub_byte_rate_reads_as_idle() {
        assert_eq!(format_rate(0.0), "0 B/s");
        assert_eq!(format_rate(0.9), "0 B/s");
        assert_eq!(format_rate(f64::NAN), "0 B/s");
        assert_eq!(format_rate(1_024.0), "1.0 KiB/s");
    }

    #[test]
    fn the_node_table_shows_the_bandwidth_and_shm_columns() {
        let snapshot = fixture_snapshot();
        let state = UiState::new();
        let backend = TestBackend::new(140, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &snapshot, &state))
            .unwrap();
        let rendered = super::super::buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("BANDWIDTH"), "{rendered}");
        assert!(rendered.contains("SHM"), "{rendered}");
        assert!(rendered.contains("1.0 MiB/s"), "{rendered}");
        assert!(rendered.contains("3/8"), "{rendered}");
    }

    #[test]
    fn queue_detail_text_lists_every_input_not_just_the_deepest() {
        let mut sample = NodeMetricsSample::new(
            NodeId::new("detector").unwrap(),
            astrs_time::HlcTimestamp::EPOCH,
        );
        sample
            .queue_depths
            .insert(DataId::new("frames").unwrap(), 4);
        sample.queue_depths.insert(DataId::new("tick").unwrap(), 1);
        let node =
            NodeRow::new(node_info("detector", NodeRunState::Running, 0)).with_metrics(sample);
        let text = queue_detail_text(&node);
        assert!(text.contains("frames:4"), "{text}");
        assert!(text.contains("tick:1"), "{text}");

        let no_metrics = NodeRow::new(node_info("detector", NodeRunState::Running, 0));
        assert!(queue_detail_text(&no_metrics).contains("no metrics"));

        let idle = NodeRow::new(node_info("detector", NodeRunState::Running, 0)).with_metrics(
            NodeMetricsSample::new(
                NodeId::new("detector").unwrap(),
                astrs_time::HlcTimestamp::EPOCH,
            ),
        );
        assert!(queue_detail_text(&idle).contains("no open input queues"));
    }

    #[test]
    fn the_dataflows_tab_shows_the_selected_nodes_full_queue_breakdown() {
        let mut snapshot = fixture_snapshot();
        // Give the selected node (index 0, "camera") a second queued
        // input so a single-queue fixture cannot hide a regression that
        // only shows the first entry.
        snapshot.dataflows[0].nodes[0]
            .metrics
            .as_mut()
            .unwrap()
            .queue_depths
            .insert(DataId::new("config").unwrap(), 1);
        let state = UiState::new();
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &snapshot, &state))
            .unwrap();
        let rendered = super::super::buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("frames:4"), "{rendered}");
        assert!(rendered.contains("config:1"), "{rendered}");
    }

    #[test]
    fn an_empty_snapshot_shows_a_placeholder_not_an_empty_table() {
        let snapshot = ClusterSnapshot::empty();
        let state = UiState::new();
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &snapshot, &state))
            .unwrap();
        let rendered = super::super::buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("no dataflows"));
    }
}
