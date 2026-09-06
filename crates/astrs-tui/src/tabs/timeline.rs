//! The Timeline tab: recent lifecycle events in HLC order (blueprint §13 —
//! spawns, restarts, status changes and violations across every machine),
//! most recent first.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::UiState;
use crate::theme;
use crate::view::{ClusterSnapshot, TimelineEvent};

/// Renders the Timeline tab into `area`.
pub fn render(frame: &mut Frame, area: Rect, snapshot: &ClusterSnapshot, state: &UiState) {
    let lines = timeline_lines(snapshot);
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Timeline (most recent first)"),
        )
        .scroll((state.scroll, 0));
    frame.render_widget(paragraph, area);
}

/// One event's rendered line: `<hlc> [category] node message`.
#[must_use]
fn event_line(event: &TimelineEvent) -> Line<'static> {
    let mut spans = vec![
        Span::raw(format!("{} ", event.timestamp)),
        Span::styled(
            format!("[{}]", event.category.label()),
            theme::timeline_category_style(event.category),
        ),
        Span::raw(" "),
    ];
    if let Some(node) = &event.node {
        spans.push(Span::raw(format!("{node} ")));
    }
    spans.push(Span::raw(event.message.clone()));
    Line::from(spans)
}

/// The most-recent-first line list for every known event.
#[must_use]
pub fn timeline_lines(snapshot: &ClusterSnapshot) -> Vec<Line<'static>> {
    if snapshot.timeline.is_empty() {
        return vec![Line::from("(no events yet)")];
    }
    snapshot.timeline.iter().rev().map(event_line).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_time::HlcTimestamp;
    use astrs_wire::NodeId;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::view::TimelineCategory;

    fn snapshot_with_events() -> ClusterSnapshot {
        let mut snapshot = ClusterSnapshot::empty();
        snapshot.push_timeline(
            TimelineEvent::new(
                HlcTimestamp::new(1, 0),
                TimelineCategory::Spawn,
                "camera spawned",
            )
            .with_node(NodeId::new("camera").unwrap()),
        );
        snapshot.push_timeline(TimelineEvent::new(
            HlcTimestamp::new(2, 0),
            TimelineCategory::Violation,
            "queue overflow on detector/frames",
        ));
        snapshot
    }

    #[test]
    fn events_render_most_recent_first() {
        let snapshot = snapshot_with_events();
        let lines = timeline_lines(&snapshot);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].to_string().contains("queue overflow"));
        assert!(lines[1].to_string().contains("camera spawned"));
    }

    #[test]
    fn an_empty_timeline_says_so() {
        let snapshot = ClusterSnapshot::empty();
        let lines = timeline_lines(&snapshot);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].to_string().contains("no events"));
    }

    #[test]
    fn the_timeline_tab_renders_events_and_their_category_badge() {
        let snapshot = snapshot_with_events();
        let state = UiState::new();
        let backend = TestBackend::new(60, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &snapshot, &state))
            .unwrap();
        let rendered = super::super::buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("camera spawned"));
        assert!(rendered.contains("violation"));
    }
}
