//! The data source abstraction every tab renders from:
//! [`ClusterView`] and the [`ClusterSnapshot`] it maintains.
//!
//! Two implementations exist: [`coordinator::CoordinatorSource`] polls a
//! live coordinator over the same framing every other client verb uses
//! (blueprint §17), and [`replay::ReplaySource`] plays back an `.arec`
//! recording (§14) with no coordinator at all. [`crate::App`] and every
//! tab renderer hold only a `Box<dyn ClusterView>` — neither knows, or
//! needs to know, which of the two it has.

mod snapshot;

pub mod coordinator;
pub mod replay;

pub use snapshot::{
    ClusterSnapshot, ConnectionStatus, DataflowRow, GraphInfo, MAX_LOG_LINES, MAX_TIMELINE_EVENTS,
    NodeRow, PlaneBadge, TimelineCategory, TimelineEvent,
};

/// Parses and validates a manifest's YAML, returning `None` rather than an
/// error: both [`coordinator::CoordinatorSource`] (a `GetManifest` reply
/// that failed to parse) and [`replay::ReplaySource`] (a recording header
/// with no or a corrupt embedded manifest) treat that the same way — keep
/// showing the dataflow row, just with no Graph tab content.
///
/// Shared so the two views can never interpret the same YAML two
/// different ways.
#[must_use]
pub(crate) fn parse_manifest(yaml: &str) -> Option<astrs_manifest::Manifest> {
    if yaml.trim().is_empty() {
        return None;
    }
    let manifest = astrs_manifest::Manifest::from_yaml_str(yaml).ok()?;
    manifest.validate().ok()?;
    Some(manifest)
}

/// Builds a [`GraphInfo`] from an already-parsed, already-validated
/// manifest — `None` only if the manifest's own cross-references were
/// never validated (see [`astrs_graph::DataflowGraph::from_manifest`]'s
/// docs), which [`parse_manifest`] already ruled out for every caller of
/// this function.
#[must_use]
pub(crate) fn graph_info_from_manifest(manifest: &astrs_manifest::Manifest) -> Option<GraphInfo> {
    let (graph, _construction_diagnostics) =
        astrs_graph::DataflowGraph::from_manifest(manifest).ok()?;
    Some(GraphInfo::new(graph))
}

/// Why a [`ClusterView::refresh`] could not update the snapshot.
///
/// A refresh failure is never fatal to the TUI — [`crate::App`] downgrades
/// [`ConnectionStatus`] and keeps rendering the last snapshot that
/// succeeded (blueprint: a monitor that freezes or exits the moment a
/// coordinator hiccups is worse than one showing slightly stale data with
/// an honest status line).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ViewError {
    /// The coordinator connection is down (refused, reset, or never
    /// established).
    #[error("lost connection to the coordinator: {0}")]
    Disconnected(String),
    /// The recording ended, or could not be read further.
    #[error("replay source cannot continue: {0}")]
    Replay(String),
}

/// A source of cluster state a `astrs top` session renders from.
///
/// # Why `refresh` + `snapshot`, not a single blocking call
///
/// Splitting "pull whatever is newly available" from "hand back what you
/// have" is what lets a background-task-backed implementation (the
/// coordinator source runs its network I/O on a Tokio task and drains an
/// `mpsc` channel on `refresh`) coexist with a purely synchronous one (the
/// replay source just advances a cursor) behind the same trait, and lets
/// [`ClusterView::snapshot`] return a plain `&ClusterSnapshot` rather than
/// a value that has to be reconstructed, cloned, or read through a lock
/// guard on every redraw. See [`ClusterSnapshot`]'s own docs for why it is
/// owned rather than borrowed.
///
/// # Object safety
///
/// Deliberately object-safe (no generics, no `async fn`) so
/// [`crate::App`] can hold a `Box<dyn ClusterView>` chosen at runtime by
/// `astrs top --replay`, and so a test can hand it a small fixture
/// implementation with no coordinator and no file at all.
///
/// # Examples
///
/// The smallest possible implementation — no coordinator, no file, no
/// clock — which is exactly what makes every renderer in this crate
/// testable without a terminal:
///
/// ```
/// use astrs_tui::{ClusterSnapshot, ClusterView, ViewError};
///
/// struct Fixture(ClusterSnapshot);
///
/// impl ClusterView for Fixture {
///     fn refresh(&mut self) -> Result<(), ViewError> {
///         Ok(())
///     }
///
///     fn snapshot(&self) -> &ClusterSnapshot {
///         &self.0
///     }
/// }
///
/// let mut view: Box<dyn ClusterView> = Box::new(Fixture(ClusterSnapshot::empty()));
/// assert!(view.refresh().is_ok());
/// assert!(view.snapshot().dataflows.is_empty());
/// ```
pub trait ClusterView {
    /// Pulls whatever new state is available and folds it into the
    /// snapshot [`ClusterView::snapshot`] will return next.
    ///
    /// Called once per redraw tick (blueprint: no busy-poll — the caller
    /// owns the tick rate, not this method). Must not block on network or
    /// disk I/O for longer than a redraw tick can tolerate; the coordinator
    /// source achieves this by never doing its own I/O inline here — see
    /// that module's docs.
    ///
    /// # Errors
    ///
    /// [`ViewError`] when nothing new could be obtained. The snapshot is
    /// left exactly as it was; the caller decides how to reflect the
    /// failure (typically: update [`ConnectionStatus`] and keep rendering).
    fn refresh(&mut self) -> Result<(), ViewError>;

    /// The current snapshot — a plain reference into state this view
    /// already owns.
    fn snapshot(&self) -> &ClusterSnapshot;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A trivial fixture proving [`ClusterView`] is object-safe and that a
    /// test can drive one with no coordinator, no file and no clock.
    struct FixtureView {
        snapshot: ClusterSnapshot,
        fail_next: bool,
    }

    impl ClusterView for FixtureView {
        fn refresh(&mut self) -> Result<(), ViewError> {
            if self.fail_next {
                self.fail_next = false;
                return Err(ViewError::Disconnected("test".to_owned()));
            }
            Ok(())
        }

        fn snapshot(&self) -> &ClusterSnapshot {
            &self.snapshot
        }
    }

    #[test]
    fn a_boxed_cluster_view_is_usable_through_the_trait() {
        let mut view: Box<dyn ClusterView> = Box::new(FixtureView {
            snapshot: ClusterSnapshot::empty(),
            fail_next: false,
        });
        assert!(view.refresh().is_ok());
        assert!(view.snapshot().dataflows.is_empty());
    }

    #[test]
    fn a_refresh_failure_leaves_the_snapshot_untouched() {
        let mut view = FixtureView {
            snapshot: ClusterSnapshot::empty(),
            fail_next: true,
        };
        let error = view.refresh().unwrap_err();
        assert!(matches!(error, ViewError::Disconnected(_)));
        assert!(view.snapshot().dataflows.is_empty());
        assert!(error.to_string().contains("coordinator"));
    }
}
