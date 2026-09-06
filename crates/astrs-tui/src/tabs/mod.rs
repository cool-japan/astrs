//! The four tab renderers (blueprint §13, §17) and the dispatcher that
//! picks one by [`crate::app::UiState::active_tab`].
//!
//! Every renderer follows the same split: a pure function building the
//! widget content from a `&ClusterSnapshot`/`&UiState` (unit-testable with
//! no terminal at all), and a thin `render` wrapper that hands the result
//! to a real [`ratatui::Frame`] (golden-testable with
//! [`ratatui::backend::TestBackend`]).

pub mod dataflows;
pub mod graph;
pub mod logs;
pub mod timeline;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Span;

use crate::app::{Tab, UiState};
use crate::view::{ClusterSnapshot, PlaneBadge};

/// Renders whichever tab [`UiState::active_tab`] names into `area`.
pub fn render_active(frame: &mut Frame, area: Rect, snapshot: &ClusterSnapshot, state: &UiState) {
    match state.active_tab {
        Tab::Dataflows => dataflows::render(frame, area, snapshot, state),
        Tab::Graph => graph::render(frame, area, snapshot, state),
        Tab::Logs => logs::render(frame, area, snapshot, state),
        Tab::Timeline => timeline::render(frame, area, snapshot, state),
    }
}

/// A `[plane]` badge span, styled per [`crate::theme::plane_style`].
#[must_use]
pub fn plane_span(plane: PlaneBadge) -> Span<'static> {
    Span::styled(
        format!("[{}]", plane.label()),
        crate::theme::plane_style(plane),
    )
}

/// The style for a placeholder line shown when a tab has nothing to
/// render yet ("no dataflows", "graph not fetched", …).
#[must_use]
pub fn placeholder_style() -> Style {
    Style::new().fg(Color::DarkGray)
}

/// Flattens a [`ratatui::buffer::Buffer`] to plain text, row by row, for
/// substring assertions — shared by every tab's `TestBackend` golden
/// tests.
#[cfg(test)]
pub(crate) fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plane_span_carries_the_badge_label() {
        for plane in [PlaneBadge::Shm, PlaneBadge::Net, PlaneBadge::Virtual] {
            let span = plane_span(plane);
            assert!(span.content.contains(plane.label()));
        }
    }
}
