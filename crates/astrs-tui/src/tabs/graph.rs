//! The Graph tab: a compact layered ASCII rendering of the selected
//! dataflow's topology, with plane badges (blueprint §5.2, §6.2/§6.4).
//!
//! See [`crate::layout`] for the layering algorithm and
//! [`crate::view::PlaneBadge`] for exactly what the badges do (and do not)
//! claim about a live route's actual transport.

use astrs_graph::EdgeSource;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::UiState;
use crate::layout::GraphLayout;
use crate::view::{ClusterSnapshot, GraphInfo};

/// Renders the Graph tab into `area`.
pub fn render(frame: &mut Frame, area: Rect, snapshot: &ClusterSnapshot, state: &UiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let Some(row) = state.selected_dataflow_row(snapshot) else {
        render_placeholder(frame, chunks[0], "no dataflow selected");
        return;
    };
    let title = format!("Graph — {}", row.summary.display_name());

    let Some(info) = &row.graph else {
        render_placeholder(
            frame,
            chunks[0],
            "topology not fetched yet for this dataflow",
        );
        return;
    };

    let lines = graph_lines(info);
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((state.scroll, 0));
    frame.render_widget(paragraph, chunks[0]);

    frame.render_widget(
        Paragraph::new("planes are planned from placement, not live-negotiated")
            .style(super::placeholder_style()),
        chunks[1],
    );
}

fn render_placeholder(frame: &mut Frame, area: Rect, message: &str) {
    frame.render_widget(
        Paragraph::new(message)
            .style(super::placeholder_style())
            .block(Block::default().borders(Borders::ALL).title("Graph")),
        area,
    );
}

/// Builds the layered text body: one block per layer, one line per node,
/// followed by its outgoing edges (and, for a node fed directly by a
/// virtual source, an incoming line for that source — see this function's
/// body for why virtual sources cannot be listed from a producer's side).
#[must_use]
pub fn graph_lines(info: &GraphInfo) -> Vec<Line<'static>> {
    let layout = GraphLayout::compute(&info.graph);
    let mut lines = Vec::new();

    for (layer_index, layer) in layout.layers.iter().enumerate() {
        if layer_index > 0 {
            lines.push(Line::default());
        }
        for node_id in layer {
            lines.push(Line::from(Span::styled(
                format!("[{node_id}]"),
                Style::new().add_modifier(Modifier::BOLD),
            )));

            // Virtual-source inputs have no producer node, so they can
            // only ever be rendered from the consumer's side.
            for (key, edge) in info.graph.edges_into(node_id) {
                if let EdgeSource::Virtual(source) = &edge.from {
                    lines.push(Line::from(vec![
                        Span::raw("  ◀─"),
                        super::plane_span(crate::view::PlaneBadge::Virtual),
                        Span::raw(format!("─ {source} → {}", key.input)),
                    ]));
                }
            }

            // Outgoing node-to-node edges: every edge with a producer
            // appears exactly once, from its producer's row.
            for (key, edge) in info.graph.edges_from(node_id) {
                if let EdgeSource::NodeOutput { .. } = &edge.from {
                    let plane = info.plane_of(key).unwrap_or(crate::view::PlaneBadge::Shm);
                    lines.push(Line::from(vec![
                        Span::raw("  └─"),
                        super::plane_span(plane),
                        Span::raw(format!("─▶ {}/{}", key.consumer, key.input)),
                    ]));
                }
            }
        }
    }

    if lines.is_empty() {
        lines.push(Line::from("(empty graph)"));
    }
    lines
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_manifest::{Input, Manifest, Node};
    use astrs_wire::{DataflowId, DataflowSummary};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::view::DataflowRow;

    fn two_node_graph_info() -> GraphInfo {
        let mut camera = Node::with_path("camera", "./camera");
        camera.outputs = vec!["frames".to_owned()];
        let mut detector = Node::with_path("detector", "./detector");
        detector
            .inputs
            .insert("frames".to_owned(), Input::from_source("camera/frames"));
        detector
            .inputs
            .insert("tick".to_owned(), Input::from_source("astrs/timer/hz/50"));
        let manifest = Manifest {
            nodes: vec![camera, detector],
            ..Manifest::default()
        };
        let (graph, _) = astrs_graph::DataflowGraph::from_manifest(&manifest).unwrap();
        GraphInfo::new(graph)
    }

    #[test]
    fn graph_lines_show_nodes_the_plane_badge_and_the_virtual_source() {
        let info = two_node_graph_info();
        let text: String = graph_lines(&info)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("[camera]"));
        assert!(text.contains("[detector]"));
        assert!(text.contains("[shm]"));
        assert!(text.contains("detector/frames"));
        assert!(text.contains("astrs/timer/hz/50"));
        assert!(text.contains("[timer]"));
    }

    #[test]
    fn an_empty_graph_renders_a_placeholder_line() {
        let (graph, _) = astrs_graph::DataflowGraph::from_manifest(&Manifest::default()).unwrap();
        let info = GraphInfo::new(graph);
        let lines = graph_lines(&info);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].to_string().contains("empty"));
    }

    #[test]
    fn the_graph_tab_renders_a_fetched_topology() {
        let mut snapshot = ClusterSnapshot::empty();
        let mut row = DataflowRow::new(DataflowSummary::pending(DataflowId::from_u128(1), 2));
        row.graph = Some(two_node_graph_info());
        snapshot.dataflows.push(row);
        let state = UiState::new();

        let backend = TestBackend::new(50, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &snapshot, &state))
            .unwrap();
        let rendered = super::super::buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("camera"));
        assert!(rendered.contains("shm"));
        assert!(rendered.contains("planned"));
    }

    #[test]
    fn the_graph_tab_shows_a_placeholder_before_the_topology_arrives() {
        let mut snapshot = ClusterSnapshot::empty();
        snapshot
            .dataflows
            .push(DataflowRow::new(DataflowSummary::pending(
                DataflowId::from_u128(1),
                0,
            )));
        let state = UiState::new();

        let backend = TestBackend::new(50, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &snapshot, &state))
            .unwrap();
        let rendered = super::super::buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("not fetched"));
    }
}
