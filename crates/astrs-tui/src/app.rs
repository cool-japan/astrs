//! [`App`]: the tab set, the input state machine, and the top-level frame
//! render that ties a [`crate::ClusterView`] to a terminal.
//!
//! [`UiState`] and [`handle_key`] are deliberately pure — no I/O, no
//! terminal, no clock — so the input handling state machine is testable by
//! constructing a state, feeding it key events, and asserting on the
//! result, exactly as the crate's test suite does.

use astrs_wire::{LogLevel, NodeId};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Tabs};

use crate::event::TuiEvent;
use crate::theme;
use crate::view::{ClusterSnapshot, ClusterView, ConnectionStatus};

/// The four tabs `astrs top` shows (blueprint §13, §17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    /// List + per-node status/restarts/CPU/RSS/queue depths.
    Dataflows,
    /// The layered ASCII topology with plane badges.
    Graph,
    /// Live log tail with level/node filters.
    Logs,
    /// HLC-ordered recent events.
    Timeline,
}

impl Tab {
    /// Every tab, in the order the tab bar renders them.
    pub const ALL: [Self; 4] = [Self::Dataflows, Self::Graph, Self::Logs, Self::Timeline];

    /// The tab's title, as printed in the tab bar.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Dataflows => "Dataflows",
            Self::Graph => "Graph",
            Self::Logs => "Logs",
            Self::Timeline => "Timeline",
        }
    }

    /// The zero-based index into [`Tab::ALL`].
    #[must_use]
    const fn index(self) -> usize {
        match self {
            Self::Dataflows => 0,
            Self::Graph => 1,
            Self::Logs => 2,
            Self::Timeline => 3,
        }
    }

    /// The tab after this one, wrapping.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Dataflows => Self::Graph,
            Self::Graph => Self::Logs,
            Self::Logs => Self::Timeline,
            Self::Timeline => Self::Dataflows,
        }
    }

    /// The tab before this one, wrapping.
    #[must_use]
    pub const fn prev(self) -> Self {
        match self {
            Self::Dataflows => Self::Timeline,
            Self::Graph => Self::Dataflows,
            Self::Logs => Self::Graph,
            Self::Timeline => Self::Logs,
        }
    }

    /// The tab named by a `1`-`4` key press, if any.
    #[must_use]
    pub const fn from_digit(digit: char) -> Option<Self> {
        match digit {
            '1' => Some(Self::Dataflows),
            '2' => Some(Self::Graph),
            '3' => Some(Self::Logs),
            '4' => Some(Self::Timeline),
            _ => None,
        }
    }
}

/// Every piece of input state a tab render needs, besides the snapshot
/// itself.
#[derive(Debug, Clone, PartialEq)]
pub struct UiState {
    /// The tab currently shown.
    pub active_tab: Tab,
    /// The selected dataflow's index into
    /// [`ClusterSnapshot::dataflows`](crate::view::ClusterSnapshot) —
    /// which dataflow the Dataflows tab highlights and the Graph tab
    /// draws.
    pub selected_dataflow: usize,
    /// The selected node's index into the selected dataflow's node list
    /// (Dataflows tab detail).
    pub selected_node: usize,
    /// The vertical scroll offset for a tab whose content can exceed one
    /// screen (Graph, Logs, Timeline).
    pub scroll: u16,
    /// The minimum level the Logs tab shows.
    pub log_level_filter: LogLevel,
    /// The node the Logs tab is restricted to, if any.
    pub log_node_filter: Option<NodeId>,
    /// Set once the user has asked to quit.
    pub should_quit: bool,
}

impl UiState {
    /// The state a session starts in: the Dataflows tab, nothing selected,
    /// every log line shown.
    #[must_use]
    pub fn new() -> Self {
        Self {
            active_tab: Tab::Dataflows,
            selected_dataflow: 0,
            selected_node: 0,
            scroll: 0,
            log_level_filter: LogLevel::Trace,
            log_node_filter: None,
            should_quit: false,
        }
    }

    /// The currently selected dataflow's row, if any dataflow is known.
    #[must_use]
    pub fn selected_dataflow_row<'a>(
        &self,
        snapshot: &'a ClusterSnapshot,
    ) -> Option<&'a crate::view::DataflowRow> {
        snapshot.dataflows.get(self.selected_dataflow)
    }

    /// Clamps [`UiState::selected_dataflow`]/[`UiState::selected_node`] to
    /// what `snapshot` actually has, so a dataflow finishing and dropping
    /// off the end of the list never leaves the cursor pointing past the
    /// end, and clamps [`UiState::scroll`] the same way (see this impl's
    /// private `clamp_scroll`).
    pub fn clamp_to(&mut self, snapshot: &ClusterSnapshot) {
        if snapshot.dataflows.is_empty() {
            self.selected_dataflow = 0;
        } else if self.selected_dataflow >= snapshot.dataflows.len() {
            self.selected_dataflow = snapshot.dataflows.len() - 1;
        }
        let node_count = self
            .selected_dataflow_row(snapshot)
            .map_or(0, |row| row.nodes.len());
        if node_count == 0 {
            self.selected_node = 0;
        } else if self.selected_node >= node_count {
            self.selected_node = node_count - 1;
        }
        self.clamp_scroll(snapshot);
    }

    /// Coarsely bounds [`UiState::scroll`] to the number of lines the
    /// active tab's content actually has, so holding a scroll key can
    /// never run it away to a value many key presses (or a tab switch)
    /// would be needed to recover from.
    ///
    /// This is a *state-level* guard only — it bounds `scroll` to
    /// `[0, content_len - 1]` using each tab's own public line-building
    /// function, so the bound can never drift from what will actually be
    /// drawn. The *default* reading position within that bound is each
    /// tab's own rendering concern: the Graph tab's `0` already means "the
    /// top of the graph," and the Timeline tab's `0` already means "the
    /// newest event" (its lines are built newest-first), so neither needs
    /// anything beyond this coarse bound. The Logs tab is different — its
    /// lines stay in true chronological order (so
    /// [`astrs_wire::LogRecord`]'s `Display` output reads the same as
    /// `astrs logs`'), so *where* `scroll == 0` points depends on the
    /// real viewport height, which this method — called with no `Rect` —
    /// cannot know; see `tabs::logs::tail_offset` for the precise,
    /// render-time half of that tab's bound.
    ///
    /// [`Tab::Dataflows`] does not scroll any of its tables at all, so its
    /// `scroll` field (unused by that tab's renderer) is left untouched.
    fn clamp_scroll(&mut self, snapshot: &ClusterSnapshot) {
        let line_count = match self.active_tab {
            Tab::Dataflows => return,
            Tab::Graph => self
                .selected_dataflow_row(snapshot)
                .and_then(|row| row.graph.as_ref())
                .map_or(0, |info| crate::tabs::graph::graph_lines(info).len()),
            Tab::Logs => crate::tabs::logs::filtered_lines(snapshot, self).len(),
            Tab::Timeline => crate::tabs::timeline::timeline_lines(snapshot).len(),
        };
        let max_scroll = u16::try_from(line_count.saturating_sub(1)).unwrap_or(u16::MAX);
        self.scroll = self.scroll.min(max_scroll);
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

/// The pure input state machine: applies one key press to `state`.
///
/// Every binding is documented on the footer line this crate draws for
/// the active tab; this function is the single place that implements
/// them, so the two can never drift.
///
/// # Examples
///
/// ```
/// use astrs_tui::{ClusterSnapshot, Tab, UiState, handle_key};
/// use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
///
/// fn key(code: KeyCode) -> KeyEvent {
///     KeyEvent::new(code, KeyModifiers::NONE)
/// }
///
/// let snapshot = ClusterSnapshot::empty();
/// let mut state = UiState::new();
/// handle_key(&mut state, &snapshot, key(KeyCode::Char('2')));
/// assert_eq!(state.active_tab, Tab::Graph);
///
/// handle_key(&mut state, &snapshot, key(KeyCode::Char('q')));
/// assert!(state.should_quit);
/// ```
pub fn handle_key(state: &mut UiState, snapshot: &ClusterSnapshot, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => state.should_quit = true,
        // Terminals report Shift+Tab as its own `BackTab` keycode rather
        // than `Tab` with a shift modifier — `Tab`/`BackTab` are the only
        // two keys that switch tabs, so `Left`/`Right` stay free to mean
        // "move the selected dataflow" on every tab, uniformly.
        KeyCode::Tab => {
            state.active_tab = state.active_tab.next();
            state.scroll = 0;
        }
        KeyCode::BackTab => {
            state.active_tab = state.active_tab.prev();
            state.scroll = 0;
        }
        KeyCode::Char(digit @ '1'..='4') => {
            if let Some(tab) = Tab::from_digit(digit) {
                state.active_tab = tab;
                state.scroll = 0;
            }
        }
        KeyCode::Up | KeyCode::Char('k') => move_up(state, snapshot),
        KeyCode::Down | KeyCode::Char('j') => move_down(state, snapshot),
        KeyCode::Left | KeyCode::Char('h') => move_dataflow(state, snapshot, -1),
        KeyCode::Right | KeyCode::Char('l') => move_dataflow(state, snapshot, 1),
        KeyCode::PageUp => state.scroll = state.scroll.saturating_sub(10),
        KeyCode::PageDown => state.scroll = state.scroll.saturating_add(10),
        KeyCode::Char('e') if state.active_tab == Tab::Logs => {
            state.log_level_filter = LogLevel::Error;
        }
        KeyCode::Char('w') if state.active_tab == Tab::Logs => {
            state.log_level_filter = LogLevel::Warn;
        }
        KeyCode::Char('i') if state.active_tab == Tab::Logs => {
            state.log_level_filter = LogLevel::Info;
        }
        KeyCode::Char('d') if state.active_tab == Tab::Logs => {
            state.log_level_filter = LogLevel::Debug;
        }
        KeyCode::Char('t') if state.active_tab == Tab::Logs => {
            state.log_level_filter = LogLevel::Trace;
        }
        KeyCode::Char('n') if state.active_tab == Tab::Logs => cycle_node_filter(state, snapshot),
        KeyCode::Char('c') if state.active_tab == Tab::Logs => state.log_node_filter = None,
        _ => {}
    }
    // Every key that can move `scroll` (PageUp/PageDown/Up/Down/j/k) must
    // leave it inside what the active tab will actually render — see
    // `UiState::clamp_scroll` — rather than waiting for the next
    // `TuiEvent::Tick` (up to one tick interval away, and every key event
    // redraws immediately: without this, holding a scroll key shows a
    // blank pane for as long as it is held).
    state.clamp_scroll(snapshot);
}

/// `Up`/`k`: on the Dataflows tab, moves the node selection; elsewhere,
/// scrolls up.
fn move_up(state: &mut UiState, snapshot: &ClusterSnapshot) {
    if state.active_tab == Tab::Dataflows {
        state.selected_node = state.selected_node.saturating_sub(1);
        let _ = snapshot;
    } else {
        state.scroll = state.scroll.saturating_sub(1);
    }
}

/// `Down`/`j`: on the Dataflows tab, moves the node selection (clamped);
/// elsewhere, scrolls down.
fn move_down(state: &mut UiState, snapshot: &ClusterSnapshot) {
    if state.active_tab == Tab::Dataflows {
        let node_count = state
            .selected_dataflow_row(snapshot)
            .map_or(0, |row| row.nodes.len());
        if node_count > 0 && state.selected_node + 1 < node_count {
            state.selected_node += 1;
        }
    } else {
        state.scroll = state.scroll.saturating_add(1);
    }
}

/// `Left`/`Right`/`h`/`l`: moves the selected dataflow by `delta`, clamped
/// to the known dataflow list, resetting the node selection.
fn move_dataflow(state: &mut UiState, snapshot: &ClusterSnapshot, delta: i32) {
    if snapshot.dataflows.is_empty() {
        return;
    }
    let len = snapshot.dataflows.len();
    #[expect(
        clippy::cast_possible_wrap,
        reason = "a dataflow list has nowhere near i32::MAX entries"
    )]
    let current = state.selected_dataflow as i32;
    let moved = (current + delta).clamp(0, len as i32 - 1);
    #[expect(
        clippy::cast_sign_loss,
        reason = "moved is clamped non-negative on the line above"
    )]
    let new_index = moved as usize;
    if new_index != state.selected_dataflow {
        state.selected_dataflow = new_index;
        state.selected_node = 0;
    }
}

/// `n` on the Logs tab: cycles the node filter forward through the
/// selected dataflow's nodes, then back to "no filter".
fn cycle_node_filter(state: &mut UiState, snapshot: &ClusterSnapshot) {
    let Some(row) = state.selected_dataflow_row(snapshot) else {
        return;
    };
    if row.nodes.is_empty() {
        state.log_node_filter = None;
        return;
    }
    let next = match &state.log_node_filter {
        None => Some(row.nodes[0].info.node.clone()),
        Some(current) => {
            let position = row.nodes.iter().position(|n| &n.info.node == current);
            match position {
                Some(index) if index + 1 < row.nodes.len() => {
                    Some(row.nodes[index + 1].info.node.clone())
                }
                _ => None,
            }
        }
    };
    state.log_node_filter = next;
}

/// Applies one [`TuiEvent`] to `state`, refreshing `view` on every
/// [`TuiEvent::Tick`].
///
/// The one place [`crate::ClusterView::refresh`] is called from — every
/// other piece of input handling is the pure [`handle_key`] above.
///
/// # Examples
///
/// ```
/// use astrs_tui::app::on_event;
/// use astrs_tui::{ClusterSnapshot, ClusterView, TuiEvent, UiState, ViewError};
/// use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
///
/// struct Fixture(ClusterSnapshot);
/// impl ClusterView for Fixture {
///     fn refresh(&mut self) -> Result<(), ViewError> {
///         Ok(())
///     }
///     fn snapshot(&self) -> &ClusterSnapshot {
///         &self.0
///     }
/// }
///
/// let mut view = Fixture(ClusterSnapshot::empty());
/// let mut state = UiState::new();
/// on_event(&mut state, &mut view, TuiEvent::Tick);
/// on_event(
///     &mut state,
///     &mut view,
///     TuiEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
/// );
/// assert!(state.should_quit);
/// ```
pub fn on_event(state: &mut UiState, view: &mut dyn ClusterView, event: TuiEvent) {
    match event {
        TuiEvent::Key(key) => handle_key(state, view.snapshot(), key),
        TuiEvent::Resize(_, _) => {}
        TuiEvent::Tick => {
            let _ = view.refresh();
            state.clamp_to(view.snapshot());
        }
    }
}

/// Renders one whole frame: the tab bar, the active tab's body, and the
/// footer.
pub fn render(frame: &mut Frame, snapshot: &ClusterSnapshot, state: &UiState) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    render_tab_bar(frame, chunks[0], state.active_tab, snapshot);
    crate::tabs::render_active(frame, chunks[1], snapshot, state);
    render_footer(frame, chunks[2], state);
}

/// The top tab bar: the four tab titles plus a connection indicator.
fn render_tab_bar(frame: &mut Frame, area: Rect, active: Tab, snapshot: &ClusterSnapshot) {
    let titles: Vec<Line> = Tab::ALL
        .iter()
        .map(|tab| {
            Line::from(Span::styled(
                tab.title(),
                theme::tab_title_style(*tab == active),
            ))
        })
        .collect();
    let tabs = Tabs::new(titles)
        .select(active.index())
        .divider(" ")
        .highlight_style(theme::tab_title_style(true));
    let connection_area = Rect {
        x: area.x
            + area
                .width
                .saturating_sub(connection_label(snapshot).len() as u16),
        y: area.y,
        width: connection_label(snapshot).len() as u16,
        height: 1,
    };
    frame.render_widget(tabs, area);
    frame.render_widget(
        Paragraph::new(connection_label(snapshot)).style(connection_style(snapshot)),
        connection_area,
    );
}

/// The status-line text for the current [`ConnectionStatus`].
fn connection_label(snapshot: &ClusterSnapshot) -> String {
    match &snapshot.connection {
        ConnectionStatus::Connecting => "connecting…".to_owned(),
        ConnectionStatus::Live { endpoint } => format!("live: {endpoint}"),
        ConnectionStatus::Degraded { endpoint, reason } => {
            format!("degraded: {endpoint} ({reason})")
        }
        ConnectionStatus::Replay {
            path,
            position,
            total,
        } => match total {
            Some(total) => format!("replay: {path} ({position}/{total})"),
            None => format!("replay: {path} ({position})"),
        },
    }
}

/// The style for the connection status label.
fn connection_style(snapshot: &ClusterSnapshot) -> Style {
    match &snapshot.connection {
        ConnectionStatus::Live { .. } => Style::new().fg(Color::Green),
        ConnectionStatus::Connecting => Style::new().fg(Color::Yellow),
        ConnectionStatus::Degraded { .. } => theme::warning_style(),
        ConnectionStatus::Replay { .. } => Style::new().fg(Color::Cyan),
    }
}

/// The bottom key-hint footer, specific to the active tab.
fn render_footer(frame: &mut Frame, area: Rect, state: &UiState) {
    let hint = match state.active_tab {
        Tab::Dataflows => "q quit  Tab/1-4 tab  ←→ dataflow  ↑↓ node",
        Tab::Graph => "q quit  Tab/1-4 tab  ←→ dataflow  ↑↓ scroll",
        Tab::Logs => "q quit  Tab/1-4 tab  e/w/i/d/t level  n node  c clear  ↑↓ scroll",
        Tab::Timeline => "q quit  Tab/1-4 tab  ↑↓ scroll",
    };
    frame.render_widget(Paragraph::new(hint).style(theme::footer_style()), area);
}

/// A convenience wrapper combining a [`ClusterView`] with its [`UiState`]
/// — what `astrs top`'s run loop actually owns.
///
/// # Examples
///
/// ```
/// use astrs_tui::{App, ClusterSnapshot, ClusterView, ViewError};
///
/// struct Fixture(ClusterSnapshot);
/// impl ClusterView for Fixture {
///     fn refresh(&mut self) -> Result<(), ViewError> {
///         Ok(())
///     }
///     fn snapshot(&self) -> &ClusterSnapshot {
///         &self.0
///     }
/// }
///
/// let app = App::new(Box::new(Fixture(ClusterSnapshot::empty())));
/// assert!(!app.should_quit());
/// assert!(app.snapshot().dataflows.is_empty());
/// ```
pub struct App {
    view: Box<dyn ClusterView>,
    /// The input state — public so a caller (or a test) can seed it before
    /// the first render.
    pub state: UiState,
}

impl App {
    /// Wraps `view` with a fresh [`UiState`].
    #[must_use]
    pub fn new(view: Box<dyn ClusterView>) -> Self {
        Self {
            view,
            state: UiState::new(),
        }
    }

    /// Applies one event.
    pub fn on_event(&mut self, event: TuiEvent) {
        on_event(&mut self.state, self.view.as_mut(), event);
    }

    /// Whether the app has been asked to quit.
    #[must_use]
    pub const fn should_quit(&self) -> bool {
        self.state.should_quit
    }

    /// The current snapshot, for a caller that wants to render directly
    /// with [`render`].
    #[must_use]
    pub fn snapshot(&self) -> &ClusterSnapshot {
        self.view.snapshot()
    }

    /// Renders the current state to `frame`.
    pub fn draw(&self, frame: &mut Frame) {
        render(frame, self.view.snapshot(), &self.state);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_time::HlcTimestamp;
    use astrs_wire::{DataflowId, DataflowSummary};

    use crossterm::event::KeyModifiers;

    use super::*;
    use crate::view::{DataflowRow, NodeRow, TimelineCategory, TimelineEvent};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn node_info(id: &str) -> astrs_wire::NodeInfo {
        astrs_wire::NodeInfo {
            dataflow: DataflowId::from_u128(1),
            node: NodeId::new(id).unwrap(),
            daemon: astrs_wire::DaemonId::generate(None),
            state: astrs_wire::NodeRunState::Running,
            pid: None,
            generation: 1,
            restart_count: 0,
            inputs: std::collections::BTreeMap::new(),
            outputs: std::collections::BTreeMap::new(),
            started_at: None,
            exit_cause: None,
        }
    }

    fn snapshot_with_two_dataflows() -> ClusterSnapshot {
        let mut snapshot = ClusterSnapshot::empty();
        for (index, name) in ["alpha", "beta"].into_iter().enumerate() {
            let mut row = DataflowRow::new(DataflowSummary::pending(
                DataflowId::from_u128(index as u128 + 1),
                2,
            ));
            row.summary.name = Some(name.to_owned());
            row.nodes.push(NodeRow::new(node_info("camera")));
            row.nodes.push(NodeRow::new(node_info("detector")));
            snapshot.dataflows.push(row);
        }
        snapshot
    }

    #[test]
    fn q_and_esc_both_quit() {
        let snapshot = ClusterSnapshot::empty();
        let mut state = UiState::new();
        handle_key(&mut state, &snapshot, key(KeyCode::Char('q')));
        assert!(state.should_quit);

        let mut state = UiState::new();
        handle_key(&mut state, &snapshot, key(KeyCode::Esc));
        assert!(state.should_quit);
    }

    #[test]
    fn digit_keys_jump_directly_to_a_tab() {
        let snapshot = ClusterSnapshot::empty();
        let mut state = UiState::new();
        handle_key(&mut state, &snapshot, key(KeyCode::Char('3')));
        assert_eq!(state.active_tab, Tab::Logs);
        handle_key(&mut state, &snapshot, key(KeyCode::Char('1')));
        assert_eq!(state.active_tab, Tab::Dataflows);
    }

    #[test]
    fn tab_cycles_forward_through_every_tab_with_wraparound() {
        let snapshot = ClusterSnapshot::empty();
        let mut state = UiState::new();
        assert_eq!(state.active_tab, Tab::Dataflows);
        for expected in [Tab::Graph, Tab::Logs, Tab::Timeline, Tab::Dataflows] {
            handle_key(&mut state, &snapshot, key(KeyCode::Tab));
            assert_eq!(state.active_tab, expected);
        }
    }

    #[test]
    fn back_tab_cycles_backward_through_every_tab_with_wraparound() {
        let snapshot = ClusterSnapshot::empty();
        let mut state = UiState::new();
        assert_eq!(state.active_tab, Tab::Dataflows);
        for expected in [Tab::Timeline, Tab::Logs, Tab::Graph, Tab::Dataflows] {
            handle_key(&mut state, &snapshot, key(KeyCode::BackTab));
            assert_eq!(state.active_tab, expected);
        }
    }

    #[test]
    fn left_and_right_move_the_dataflow_selection_even_off_the_dataflows_tab() {
        let snapshot = snapshot_with_two_dataflows();
        let mut state = UiState::new();
        state.active_tab = Tab::Graph;
        handle_key(&mut state, &snapshot, key(KeyCode::Right));
        assert_eq!(
            state.selected_dataflow, 1,
            "left/right must not be swallowed by the tab switch, on any tab"
        );
        assert_eq!(state.active_tab, Tab::Graph, "and must not change the tab");
    }

    #[test]
    fn left_and_right_move_the_selected_dataflow_and_reset_the_node() {
        let snapshot = snapshot_with_two_dataflows();
        let mut state = UiState::new();
        state.selected_node = 1;
        handle_key(&mut state, &snapshot, key(KeyCode::Right));
        assert_eq!(state.selected_dataflow, 1);
        assert_eq!(state.selected_node, 0);
        handle_key(&mut state, &snapshot, key(KeyCode::Right));
        assert_eq!(state.selected_dataflow, 1, "clamped at the last dataflow");
        handle_key(&mut state, &snapshot, key(KeyCode::Left));
        assert_eq!(state.selected_dataflow, 0);
        handle_key(&mut state, &snapshot, key(KeyCode::Left));
        assert_eq!(state.selected_dataflow, 0, "clamped at the first dataflow");
    }

    #[test]
    fn up_and_down_move_node_selection_on_the_dataflows_tab_only() {
        let mut snapshot = snapshot_with_two_dataflows();
        let mut state = UiState::new();
        assert_eq!(state.active_tab, Tab::Dataflows);
        handle_key(&mut state, &snapshot, key(KeyCode::Down));
        assert_eq!(state.selected_node, 1);
        handle_key(&mut state, &snapshot, key(KeyCode::Down));
        assert_eq!(state.selected_node, 1, "clamped at the last node");
        handle_key(&mut state, &snapshot, key(KeyCode::Up));
        assert_eq!(state.selected_node, 0);

        // The Timeline tab needs more than one line of content for a
        // `scroll` of `1` to be a meaningful, *unclamped* position — see
        // `UiState::clamp_scroll`, which every `handle_key` call now runs.
        for index in 0..5_u64 {
            snapshot.push_timeline(TimelineEvent::new(
                HlcTimestamp::new(index, 0),
                TimelineCategory::Status,
                format!("event {index}"),
            ));
        }
        state.active_tab = Tab::Timeline;
        state.scroll = 0;
        handle_key(&mut state, &snapshot, key(KeyCode::Down));
        assert_eq!(state.scroll, 1, "elsewhere, up/down scroll instead");
    }

    #[test]
    fn scroll_is_clamped_to_the_active_tabs_own_content_so_holding_a_key_cannot_blank_the_pane() {
        let mut snapshot = ClusterSnapshot::empty();
        for index in 0..3_u64 {
            snapshot.push_timeline(TimelineEvent::new(
                HlcTimestamp::new(index, 0),
                TimelineCategory::Status,
                format!("event {index}"),
            ));
        }
        let mut state = UiState::new();
        state.active_tab = Tab::Timeline;

        // Three events render as three lines; holding `Down` far past
        // that must never move the pane beyond the last one.
        for _ in 0..50 {
            handle_key(&mut state, &snapshot, key(KeyCode::Down));
        }
        assert_eq!(state.scroll, 2, "clamped to the last renderable line");

        // A tab with nothing to show at all (no dataflow selected, so no
        // fetched graph) clamps all the way to zero.
        state.active_tab = Tab::Graph;
        handle_key(&mut state, &snapshot, key(KeyCode::Down));
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn the_dataflows_tabs_own_scroll_field_is_never_clamped_since_it_never_renders_with_it() {
        // `page_up_and_down_scroll_by_ten` already exercises this; this
        // test names *why* it is allowed to reach values well past
        // `ClusterSnapshot::empty()`'s zero-line content: the Dataflows
        // tab's tables do not use `UiState::scroll` at all (see
        // `tabs::dataflows`), so `clamp_scroll` deliberately leaves it be.
        let snapshot = ClusterSnapshot::empty();
        let mut state = UiState::new();
        assert_eq!(state.active_tab, Tab::Dataflows);
        handle_key(&mut state, &snapshot, key(KeyCode::PageDown));
        assert_eq!(state.scroll, 10);
    }

    #[test]
    fn log_level_keys_only_apply_on_the_logs_tab() {
        let snapshot = ClusterSnapshot::empty();
        let mut state = UiState::new();
        handle_key(&mut state, &snapshot, key(KeyCode::Char('e')));
        assert_eq!(
            state.log_level_filter,
            LogLevel::Trace,
            "ignored outside the Logs tab"
        );

        state.active_tab = Tab::Logs;
        handle_key(&mut state, &snapshot, key(KeyCode::Char('e')));
        assert_eq!(state.log_level_filter, LogLevel::Error);
        handle_key(&mut state, &snapshot, key(KeyCode::Char('d')));
        assert_eq!(state.log_level_filter, LogLevel::Debug);
    }

    #[test]
    fn n_cycles_the_node_filter_and_c_clears_it() {
        let snapshot = snapshot_with_two_dataflows();
        let mut state = UiState::new();
        state.active_tab = Tab::Logs;
        assert_eq!(state.log_node_filter, None);

        handle_key(&mut state, &snapshot, key(KeyCode::Char('n')));
        assert_eq!(state.log_node_filter, Some(NodeId::new("camera").unwrap()));
        handle_key(&mut state, &snapshot, key(KeyCode::Char('n')));
        assert_eq!(
            state.log_node_filter,
            Some(NodeId::new("detector").unwrap())
        );
        handle_key(&mut state, &snapshot, key(KeyCode::Char('n')));
        assert_eq!(state.log_node_filter, None, "wraps back to no filter");

        handle_key(&mut state, &snapshot, key(KeyCode::Char('n')));
        assert!(state.log_node_filter.is_some());
        handle_key(&mut state, &snapshot, key(KeyCode::Char('c')));
        assert_eq!(state.log_node_filter, None);
    }

    #[test]
    fn page_up_and_down_scroll_by_ten() {
        let snapshot = ClusterSnapshot::empty();
        let mut state = UiState::new();
        handle_key(&mut state, &snapshot, key(KeyCode::PageDown));
        assert_eq!(state.scroll, 10);
        handle_key(&mut state, &snapshot, key(KeyCode::PageUp));
        assert_eq!(state.scroll, 0);
        handle_key(&mut state, &snapshot, key(KeyCode::PageUp));
        assert_eq!(state.scroll, 0, "does not go negative");
    }

    #[test]
    fn clamp_to_pulls_a_stale_selection_back_into_range() {
        let mut state = UiState::new();
        state.selected_dataflow = 5;
        state.selected_node = 5;
        let snapshot = snapshot_with_two_dataflows();
        state.clamp_to(&snapshot);
        assert_eq!(state.selected_dataflow, 1);
        assert_eq!(state.selected_node, 1);

        let empty = ClusterSnapshot::empty();
        state.clamp_to(&empty);
        assert_eq!(state.selected_dataflow, 0);
        assert_eq!(state.selected_node, 0);
    }

    #[test]
    fn a_default_ui_state_starts_on_dataflows_with_nothing_selected() {
        let state = UiState::default();
        assert_eq!(state.active_tab, Tab::Dataflows);
        assert_eq!(state.selected_dataflow, 0);
        assert!(!state.should_quit);
    }

    struct FixtureView(ClusterSnapshot);
    impl ClusterView for FixtureView {
        fn refresh(&mut self) -> Result<(), crate::view::ViewError> {
            Ok(())
        }

        fn snapshot(&self) -> &ClusterSnapshot {
            &self.0
        }
    }

    #[test]
    fn app_on_event_dispatches_ticks_and_keys() {
        let mut app = App::new(Box::new(FixtureView(snapshot_with_two_dataflows())));
        assert!(!app.should_quit());
        app.on_event(TuiEvent::Key(key(KeyCode::Right)));
        assert_eq!(app.state.selected_dataflow, 1);
        app.on_event(TuiEvent::Tick);
        app.on_event(TuiEvent::Key(key(KeyCode::Char('q'))));
        assert!(app.should_quit());
    }
}
