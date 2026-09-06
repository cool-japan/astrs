//! Color and style mapping shared by every tab, kept in one place so
//! "what does red mean in this TUI" has exactly one answer.

use astrs_wire::{DataflowStatus, LogLevel, NodeRunState};
use ratatui::style::{Color, Modifier, Style};

use crate::view::{PlaneBadge, TimelineCategory};

/// The style for the active tab's title.
#[must_use]
pub fn tab_title_style(active: bool) -> Style {
    if active {
        Style::new()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Gray)
    }
}

/// The style for a [`DataflowStatus`] cell.
#[must_use]
pub const fn dataflow_status_style(status: DataflowStatus) -> Style {
    match status {
        DataflowStatus::Running => Style::new().fg(Color::Green),
        DataflowStatus::Starting | DataflowStatus::Building | DataflowStatus::Ready => {
            Style::new().fg(Color::Yellow)
        }
        DataflowStatus::Stopping | DataflowStatus::Pending => Style::new().fg(Color::Gray),
        DataflowStatus::Finished => Style::new().fg(Color::Blue),
        DataflowStatus::Failed => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        // `DataflowStatus` is `#[non_exhaustive]`; an unknown status added
        // later renders plainly rather than failing to compile here.
        _ => Style::new(),
    }
}

/// The style for a [`NodeRunState`] cell.
#[must_use]
pub const fn node_state_style(state: NodeRunState) -> Style {
    match state {
        NodeRunState::Running => Style::new().fg(Color::Green),
        NodeRunState::Spawning | NodeRunState::Restarting | NodeRunState::Pending => {
            Style::new().fg(Color::Yellow)
        }
        NodeRunState::Stopping => Style::new().fg(Color::Gray),
        NodeRunState::Finished => Style::new().fg(Color::Blue),
        NodeRunState::Failed => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        _ => Style::new(),
    }
}

/// The style for a [`PlaneBadge`].
#[must_use]
pub const fn plane_style(plane: PlaneBadge) -> Style {
    match plane {
        PlaneBadge::Shm => Style::new().fg(Color::Green),
        PlaneBadge::Net => Style::new().fg(Color::Magenta),
        PlaneBadge::Virtual => Style::new().fg(Color::Gray),
    }
}

/// The style for a [`LogLevel`] cell, matching `astrs logs`' own coloring
/// intent (error loudest, trace quietest).
#[must_use]
pub const fn log_level_style(level: LogLevel) -> Style {
    match level {
        LogLevel::Error => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        LogLevel::Warn => Style::new().fg(Color::Yellow),
        LogLevel::Info => Style::new().fg(Color::White),
        LogLevel::Debug => Style::new().fg(Color::Cyan),
        LogLevel::Trace => Style::new().fg(Color::DarkGray),
        _ => Style::new(),
    }
}

/// The style for a [`TimelineCategory`] badge.
#[must_use]
pub const fn timeline_category_style(category: TimelineCategory) -> Style {
    match category {
        TimelineCategory::Spawn => Style::new().fg(Color::Green),
        TimelineCategory::Restart => Style::new().fg(Color::Yellow),
        TimelineCategory::Status => Style::new().fg(Color::Blue),
        TimelineCategory::Violation => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        TimelineCategory::Replay => Style::new().fg(Color::Magenta),
    }
}

/// The style for the footer's key-hint text.
#[must_use]
pub fn footer_style() -> Style {
    Style::new().fg(Color::DarkGray)
}

/// The style for a degraded/disconnected status line.
#[must_use]
pub fn warning_style() -> Style {
    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_dataflow_status_has_a_style_without_panicking() {
        for status in DataflowStatus::ALL {
            let _ = dataflow_status_style(*status);
        }
    }

    #[test]
    fn every_node_run_state_has_a_style_without_panicking() {
        for state in NodeRunState::ALL {
            let _ = node_state_style(*state);
        }
    }

    #[test]
    fn every_log_level_has_a_style_without_panicking() {
        for level in LogLevel::ALL {
            let _ = log_level_style(*level);
        }
    }

    #[test]
    fn every_timeline_category_has_a_distinct_style() {
        let mut seen = std::collections::BTreeSet::new();
        for category in TimelineCategory::ALL {
            let style = timeline_category_style(*category);
            seen.insert(format!("{style:?}"));
        }
        assert_eq!(seen.len(), TimelineCategory::ALL.len());
    }

    #[test]
    fn active_and_inactive_tab_titles_differ() {
        assert_ne!(tab_title_style(true), tab_title_style(false));
    }
}
