//! The Logs tab: a live-tailing view of [`ClusterSnapshot::logs`], filtered
//! by level and node (blueprint §13, §17 `astrs logs`).
//!
//! Rendering reuses [`astrs_wire::LogRecord`]'s own [`std::fmt::Display`]
//! so a line here reads identically to `astrs logs`' own output — one
//! formatting rule, not two that can drift apart. [`filtered_lines`]
//! keeps that same chronological order (oldest first), and the pane
//! itself stays anchored to the newest record by default — see this
//! module's private `tail_offset` for exactly how, and why that is a
//! scroll offset rather than a reordering.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::UiState;
use crate::theme;
use crate::view::ClusterSnapshot;

/// Renders the Logs tab into `area`.
pub fn render(frame: &mut Frame, area: Rect, snapshot: &ClusterSnapshot, state: &UiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let lines = filtered_lines(snapshot, state);
    let title = filter_title(state);
    let offset = tail_offset(chunks[0], lines.len(), state.scroll);
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((offset, 0));
    frame.render_widget(paragraph, chunks[0]);

    frame.render_widget(
        Paragraph::new(format!(
            "showing {} of {} record(s)",
            visible_count(snapshot, state),
            snapshot.logs.len()
        ))
        .style(theme::footer_style()),
        chunks[1],
    );
}

/// Whether `record` passes the tab's current level/node filter.
#[must_use]
fn passes(record: &astrs_wire::LogRecord, state: &UiState) -> bool {
    if !record.level.is_enabled_at(state.log_level_filter) {
        return false;
    }
    match &state.log_node_filter {
        Some(node) => record.node.as_ref() == Some(node),
        None => true,
    }
}

/// The styled lines for every record currently passing the filter, oldest
/// first — the same order `astrs logs -f` prints in.
#[must_use]
pub fn filtered_lines(snapshot: &ClusterSnapshot, state: &UiState) -> Vec<Line<'static>> {
    let lines: Vec<Line<'static>> = snapshot
        .logs
        .iter()
        .filter(|record| passes(record, state))
        .map(|record| Line::styled(record.to_string(), theme::log_level_style(record.level)))
        .collect();
    if lines.is_empty() {
        vec![Line::from("(no log records match the current filter)")]
    } else {
        lines
    }
}

/// How many records currently pass the filter.
#[must_use]
fn visible_count(snapshot: &ClusterSnapshot, state: &UiState) -> usize {
    snapshot
        .logs
        .iter()
        .filter(|record| passes(record, state))
        .count()
}

/// The tab's title, naming the active filter.
#[must_use]
fn filter_title(state: &UiState) -> String {
    match &state.log_node_filter {
        Some(node) => format!("Logs — ≥{} — {node}", state.log_level_filter.as_str()),
        None => format!("Logs — ≥{}", state.log_level_filter.as_str()),
    }
}

/// The scroll offset that keeps this tab anchored to the newest record by
/// default — a genuine live tail, not a pane frozen on whatever was
/// oldest when it first had more lines than room to show them.
///
/// `state.scroll == 0` renders the *tail*: [`crate::app::handle_key`]'s
/// `Down`/`j` increases `scroll` and `Up`/`k` decreases it (see that
/// module's `move_up`/`move_down`), so on this tab that reads as "`Down`
/// looks further back into history, `Up` returns toward what is live" —
/// exactly the Timeline tab's own "most recent first, `Down` goes further
/// back" convention. Timeline reaches that reading by reordering its
/// lines (fine for a discrete event list read in either direction);
/// [`filtered_lines`] deliberately does *not* reorder, because this
/// tab's whole point is that a multi-record view reads identically to
/// `astrs logs`' own chronological output, so the anchor is a scroll
/// *offset* instead.
///
/// Only a coarse, snapshot-length bound on `scroll` itself lives on
/// [`crate::app::UiState`] (its private `clamp_scroll`, called with no
/// [`Rect`] and so unable to know a viewport height) — this function is
/// the precise other half, computed here where `area` is real.
#[must_use]
fn tail_offset(area: Rect, line_count: usize, scroll: u16) -> u16 {
    // The block this pane renders into always has a one-line border on
    // top and bottom (see `render`); a viewport shorter than that has no
    // interior at all, and `saturating_sub` leaves `inner_height` at `0`
    // rather than panicking.
    let inner_height = area.height.saturating_sub(2);
    let total = u16::try_from(line_count).unwrap_or(u16::MAX);
    let tail = total.saturating_sub(inner_height);
    tail.saturating_sub(scroll)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_time::HlcTimestamp;
    use astrs_wire::{LogLevel, LogRecord, NodeId};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn snapshot_with_mixed_levels() -> ClusterSnapshot {
        let mut snapshot = ClusterSnapshot::empty();
        snapshot.push_log(
            LogRecord::new(HlcTimestamp::new(1, 0), LogLevel::Info, "camera up")
                .with_node(NodeId::new("camera").unwrap()),
        );
        snapshot.push_log(
            LogRecord::new(HlcTimestamp::new(2, 0), LogLevel::Error, "detector crashed")
                .with_node(NodeId::new("detector").unwrap()),
        );
        snapshot
    }

    #[test]
    fn the_default_filter_shows_every_record() {
        let snapshot = snapshot_with_mixed_levels();
        let state = UiState::new();
        assert_eq!(visible_count(&snapshot, &state), 2);
    }

    #[test]
    fn a_level_filter_hides_less_severe_records() {
        let snapshot = snapshot_with_mixed_levels();
        let mut state = UiState::new();
        state.log_level_filter = LogLevel::Error;
        assert_eq!(visible_count(&snapshot, &state), 1);
        let lines = filtered_lines(&snapshot, &state);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].to_string().contains("detector crashed"));
    }

    #[test]
    fn a_node_filter_keeps_only_that_nodes_records() {
        let snapshot = snapshot_with_mixed_levels();
        let mut state = UiState::new();
        state.log_node_filter = Some(NodeId::new("camera").unwrap());
        assert_eq!(visible_count(&snapshot, &state), 1);
        let lines = filtered_lines(&snapshot, &state);
        assert!(lines[0].to_string().contains("camera up"));
    }

    #[test]
    fn an_unmatched_filter_says_so_rather_than_rendering_nothing() {
        let snapshot = snapshot_with_mixed_levels();
        let mut state = UiState::new();
        state.log_node_filter = Some(NodeId::new("nope").unwrap());
        let lines = filtered_lines(&snapshot, &state);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].to_string().contains("no log records"));
    }

    #[test]
    fn the_logs_tab_renders_visible_records_and_a_count_footer() {
        let snapshot = snapshot_with_mixed_levels();
        let state = UiState::new();
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &snapshot, &state))
            .unwrap();
        let rendered = super::super::buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("camera up"));
        assert!(rendered.contains("detector crashed"));
        assert!(rendered.contains("showing 2 of 2"));
    }

    #[test]
    fn tail_offset_shows_everything_from_the_top_when_it_all_fits() {
        let area = Rect::new(0, 0, 20, 8); // inner height 6
        assert_eq!(tail_offset(area, 3, 0), 0);
    }

    #[test]
    fn tail_offset_anchors_to_the_newest_lines_by_default() {
        let area = Rect::new(0, 0, 20, 7); // inner height 5
        assert_eq!(tail_offset(area, 10, 0), 5);
    }

    #[test]
    fn tail_offset_scrolls_back_from_the_tail_and_floors_at_zero() {
        let area = Rect::new(0, 0, 20, 7); // inner height 5, tail = 5
        assert_eq!(tail_offset(area, 10, 3), 2);
        assert_eq!(tail_offset(area, 10, 100), 0, "never scrolls past the top");
    }

    #[test]
    fn tail_offset_never_panics_on_a_viewport_too_short_for_even_its_own_borders() {
        assert_eq!(tail_offset(Rect::new(0, 0, 20, 1), 10, 0), 10);
        assert_eq!(tail_offset(Rect::new(0, 0, 20, 0), 0, 0), 0);
    }

    fn many_lines(count: u64) -> ClusterSnapshot {
        let mut snapshot = ClusterSnapshot::empty();
        for index in 0..count {
            snapshot.push_log(LogRecord::new(
                HlcTimestamp::new(index, 0),
                LogLevel::Info,
                format!("line {index}"),
            ));
        }
        snapshot
    }

    #[test]
    fn the_logs_tab_shows_the_newest_record_by_default_when_content_overflows() {
        let snapshot = many_lines(10);
        let state = UiState::new();
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &snapshot, &state))
            .unwrap();
        let rendered = super::super::buffer_text(terminal.backend().buffer());
        assert!(
            rendered.contains("line 9"),
            "the newest record must be visible without scrolling:\n{rendered}"
        );
        assert!(
            !rendered.contains("line 0"),
            "the oldest record must be scrolled out of view by default:\n{rendered}"
        );
    }

    #[test]
    fn scrolling_the_logs_tab_reaches_the_oldest_record_in_chronological_order() {
        let snapshot = many_lines(10);
        let mut state = UiState::new();
        state.scroll = 5;
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &snapshot, &state))
            .unwrap();
        let rendered = super::super::buffer_text(terminal.backend().buffer());
        assert!(
            rendered.contains("line 0"),
            "scrolling back far enough must reach the oldest record:\n{rendered}"
        );
    }
}
