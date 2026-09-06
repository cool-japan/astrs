//! Terminal lifecycle: entering/leaving the alternate screen, restoring on
//! every exit path (normal, error, or panic), and the render loop that
//! ties an [`App`] to a real terminal.
//!
//! # Restore guarantees
//!
//! Two independent mechanisms restore the terminal, because the two kinds
//! of abnormal exit need different ones:
//!
//! - [`TerminalGuard`]'s `Drop` impl restores on any *ordinary* unwind
//!   path — an `Err` returned from [`run`], or a `?` propagating through
//!   it.
//! - [`install_panic_hook`] chains onto whatever hook was previously
//!   installed and restores the terminal *before* that hook runs, so a
//!   panic's message prints on a normal screen instead of being smeared
//!   across whatever was left in the alternate buffer. It is installed
//!   once per process ([`std::sync::Once`]) and always calls the previous
//!   hook afterward, so a second `run` call (as a test harness might do)
//!   never loses another panic hook already in place.
//!
//! Both are idempotent: leaving twice, or a panic after a clean exit, does
//! nothing on the second call.

use std::io::{self, IsTerminal};
use std::panic;
use std::sync::Once;
use std::time::Duration;

use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};

use crate::app::App;
use crate::error::TuiError;
use crate::event::{EventLoop, TuiEvent};

static PANIC_HOOK_INSTALLED: Once = Once::new();

/// Chains a terminal-restoring step onto the process's panic hook, once.
fn install_panic_hook() {
    PANIC_HOOK_INSTALLED.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            previous(info);
        }));
    });
}

/// RAII guard for the alternate screen + raw mode: entered by
/// [`TerminalGuard::enter`], left by `Drop` (idempotently — see this
/// module's docs).
struct TerminalGuard {
    left: bool,
}

impl TerminalGuard {
    /// Enables raw mode, enters the alternate screen, and installs the
    /// panic hook.
    ///
    /// # Errors
    ///
    /// [`TuiError::Terminal`] if either terminal call fails.
    fn enter() -> Result<Self, TuiError> {
        enable_raw_mode().map_err(TuiError::Terminal)?;
        execute!(io::stdout(), EnterAlternateScreen).map_err(TuiError::Terminal)?;
        install_panic_hook();
        Ok(Self { left: false })
    }

    /// Restores the terminal, unless it already has been.
    fn leave(&mut self) {
        if self.left {
            return;
        }
        self.left = true;
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.leave();
    }
}

/// Runs `app` against the real terminal until it quits or the input
/// source ends.
///
/// # Errors
///
/// - [`TuiError::NotATerminal`] if stdout is not an interactive terminal —
///   checked *before* anything is written, so a pipe or a redirect to a
///   file never sees an escape sequence.
/// - [`TuiError::Terminal`] if entering the alternate screen, building the
///   backend, or spawning the input thread fails.
pub fn run(mut app: App, tick_rate: Duration) -> Result<(), TuiError> {
    if !io::stdout().is_terminal() {
        return Err(TuiError::NotATerminal);
    }
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).map_err(TuiError::Terminal)?;
    let events = EventLoop::spawn(tick_rate).map_err(TuiError::Terminal)?;

    run_loop(&mut app, &mut terminal, || events.recv()).map_err(TuiError::Terminal)
}

/// The render loop itself, generic over the backend (returning that
/// backend's own error type rather than [`TuiError`], so a test running it
/// against [`ratatui::backend::TestBackend`] — whose `Error` is
/// [`std::convert::Infallible`] — needs no conversion at all) and over how
/// the next event is obtained, which is what lets [`run`]'s production
/// path (a real [`EventLoop`]) and this module's tests (a scripted list)
/// share exactly the same draw/apply/quit-check logic.
pub fn run_loop<B: Backend>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    mut next_event: impl FnMut() -> Option<TuiEvent>,
) -> Result<(), B::Error> {
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        let Some(event) = next_event() else {
            break;
        };
        app.on_event(event);
        if app.should_quit() {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::VecDeque;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::view::{ClusterSnapshot, ClusterView, ViewError};

    struct FixtureView(ClusterSnapshot);
    impl ClusterView for FixtureView {
        fn refresh(&mut self) -> Result<(), ViewError> {
            Ok(())
        }

        fn snapshot(&self) -> &ClusterSnapshot {
            &self.0
        }
    }

    fn fixture_app() -> App {
        App::new(Box::new(FixtureView(ClusterSnapshot::empty())))
    }

    #[test]
    fn run_loop_draws_and_stops_on_a_quit_key() {
        let mut app = fixture_app();
        let mut terminal = Terminal::new(TestBackend::new(20, 5)).unwrap();
        let mut queue = VecDeque::from([
            TuiEvent::Tick,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        ]);
        let result = run_loop(&mut app, &mut terminal, || queue.pop_front());
        assert!(result.is_ok());
        assert!(app.should_quit());
    }

    #[test]
    fn run_loop_stops_cleanly_when_the_event_source_ends() {
        let mut app = fixture_app();
        let mut terminal = Terminal::new(TestBackend::new(20, 5)).unwrap();
        let result = run_loop(&mut app, &mut terminal, || None);
        assert!(result.is_ok());
        assert!(!app.should_quit());
    }

    #[test]
    fn leaving_a_terminal_guard_twice_is_a_no_op_the_second_time() {
        let mut guard = TerminalGuard { left: false };
        guard.leave();
        assert!(guard.left);
        // The second call must not panic, and must not flip the flag back.
        guard.leave();
        assert!(guard.left);
    }

    /// A dataflow with real node/log/timeline content, so a resize test
    /// exercises every tab's actual table/paragraph layout code — not
    /// just the placeholder paths an empty snapshot takes.
    fn populated_snapshot() -> ClusterSnapshot {
        use astrs_time::HlcTimestamp;
        use astrs_wire::{
            DaemonId, DataflowId, DataflowStatus, DataflowSummary, LogLevel, LogRecord, NodeId,
            NodeInfo, NodeRunState,
        };

        use crate::view::{DataflowRow, NodeRow, TimelineCategory, TimelineEvent};

        let mut snapshot = ClusterSnapshot::empty();
        let mut row = DataflowRow::new(DataflowSummary {
            id: DataflowId::from_u128(1),
            name: Some("demo".to_owned()),
            status: DataflowStatus::Running,
            daemons: Vec::new(),
            node_count: 1,
            running_nodes: 1,
            started_at: None,
        });
        row.nodes.push(NodeRow::new(NodeInfo {
            dataflow: DataflowId::from_u128(1),
            node: NodeId::new("camera").unwrap(),
            daemon: DaemonId::generate(None),
            state: NodeRunState::Running,
            pid: Some(1),
            generation: 1,
            restart_count: 0,
            inputs: std::collections::BTreeMap::new(),
            outputs: std::collections::BTreeMap::new(),
            started_at: None,
            exit_cause: None,
        }));
        snapshot.dataflows.push(row);
        snapshot.push_log(LogRecord::new(
            HlcTimestamp::new(1, 0),
            LogLevel::Info,
            "camera up",
        ));
        snapshot.push_timeline(TimelineEvent::new(
            HlcTimestamp::new(1, 0),
            TimelineCategory::Spawn,
            "camera spawned",
        ));
        snapshot
    }

    #[test]
    fn drawing_survives_a_shrink_and_a_grow_on_every_tab() {
        use crate::app::Tab;

        let mut app = App::new(Box::new(FixtureView(populated_snapshot())));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();

        // `(1, 1)` and other extreme sizes are the point: a `Layout`
        // split, a `Table`'s column math, or the tab bar's
        // `saturating_sub` connection-label placement is exactly the kind
        // of code that panics on a too-small `Rect` rather than merely
        // rendering it uglily — so this is a real regression test, not
        // just a smoke test.
        for tab in Tab::ALL {
            app.state.active_tab = tab;
            for &(width, height) in &[(60u16, 20u16), (10, 3), (1, 1), (120, 45), (0, 0)] {
                terminal.backend_mut().resize(width, height);
                app.on_event(TuiEvent::Resize(width, height));
                terminal
                    .draw(|frame| app.draw(frame))
                    .unwrap_or_else(|error| panic!("tab {tab:?} at {width}x{height}: {error}"));
                let area = terminal.backend().buffer().area();
                assert_eq!(area.width, width);
                assert_eq!(area.height, height);
            }
        }
    }

    #[test]
    fn a_resize_event_changes_no_ui_state_the_next_draw_picks_up_the_new_size_on_its_own() {
        let mut app = fixture_app();
        app.state.scroll = 3;
        let before = app.state.clone();
        app.on_event(TuiEvent::Resize(12, 4));
        assert_eq!(
            app.state, before,
            "a resize must not itself change any UI state"
        );
        assert!(!app.should_quit());
    }
}
