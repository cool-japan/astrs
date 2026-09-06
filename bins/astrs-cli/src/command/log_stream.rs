//! Streaming a running dataflow's output to a terminal (blueprint §17:
//! `astrs run` "streams logs to the terminal"; `astrs logs -f` does the
//! same over the coordinator link).
//!
//! ```text
//!   embedded daemon ──DaemonEvent──► TerminalLogSink ──bounded──► printer task
//!   (event loop)                     (never blocks,               (owns stdout)
//!                                     counts drops)
//!
//!   coordinator ──LogFrame──────────────────────────────────────► same printer
//! ```
//!
//! # The sink must never block the event loop
//!
//! [`astrs_daemon::health::ReportSink::report`] is called from *inside* the
//! daemon's merged event loop, and its contract says so: *"reporting must
//! never block the event loop and must never fail it"*. A sink that took a
//! lock around a `write!` to a terminal would stall every node on the
//! machine behind a slow pipe. [`TerminalLogSink`] therefore does the one
//! thing that cannot stall: a `try_send` into a bounded channel, with a
//! counter for what did not fit. A separate task owns the sink and does the
//! formatting and the writing.
//!
//! Bounded rather than unbounded on purpose: an unbounded channel in front
//! of a terminal that has stopped being read is an out-of-memory bug with
//! extra steps. Dropping the oldest news under sustained overload — and
//! saying how much was dropped when the stream ends — is the behavior a
//! log tail should have.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use astrs_daemon::health::ReportSink;
use astrs_wire::{DaemonEvent, LogLevel, LogRecord, NodeExitCause, NodeId, SpawnOutcome};
use tokio::sync::mpsc;

/// How many items may be queued between the daemon's event loop and the
/// printer before the oldest news starts being dropped.
///
/// Sized for a burst, not a backlog: a node that logs a thousand lines in
/// one scheduler tick should not lose any, while a terminal that has been
/// wedged for a minute should not cost the daemon a megabyte of memory.
pub const STREAM_CAPACITY: usize = 4096;

/// One thing worth printing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamItem {
    /// A log record from a node (captured stdout/stderr, or a structured
    /// record the node emitted).
    Record(Box<LogRecord>),
    /// Something the CLI itself observed about the run: a node started, a
    /// node exited, a restart. Not a node's own output, and marked so.
    Notice {
        /// How serious it is.
        level: LogLevel,
        /// The node it concerns, when it concerns one.
        node: Option<NodeId>,
        /// The text.
        message: String,
    },
}

impl StreamItem {
    /// This item's severity, whatever shape it has.
    #[must_use]
    pub fn level(&self) -> LogLevel {
        match self {
            Self::Record(record) => record.level,
            Self::Notice { level, .. } => *level,
        }
    }

    /// The node this item is about, when it is about one.
    #[must_use]
    pub fn node(&self) -> Option<&NodeId> {
        match self {
            Self::Record(record) => record.node.as_ref(),
            Self::Notice { node, .. } => node.as_ref(),
        }
    }

    /// The text to print.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Record(record) => &record.message,
            Self::Notice { message, .. } => message,
        }
    }
}

/// Which items reach the terminal.
#[derive(Debug, Clone)]
pub struct LogFilter {
    /// Hide anything less severe than this.
    pub min_level: LogLevel,
    /// Show only this node's items, when set.
    pub node: Option<NodeId>,
}

impl Default for LogFilter {
    fn default() -> Self {
        Self {
            min_level: LogLevel::Trace,
            node: None,
        }
    }
}

impl LogFilter {
    /// A filter that hides nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Hides anything below `level`.
    #[must_use]
    pub const fn with_min_level(mut self, level: LogLevel) -> Self {
        self.min_level = level;
        self
    }

    /// Restricts the stream to one node.
    #[must_use]
    pub fn with_node(mut self, node: Option<NodeId>) -> Self {
        self.node = node;
        self
    }

    /// Whether `item` should be printed.
    ///
    /// A [`StreamItem::Notice`] with no node survives a `--node` filter:
    /// "the dataflow failed" is not one node's news, and hiding it because
    /// the user asked to watch one node would hide the reason the stream
    /// ended.
    #[must_use]
    pub fn accepts(&self, item: &StreamItem) -> bool {
        if !level_at_least(item.level(), self.min_level) {
            return false;
        }
        match (&self.node, item.node()) {
            (Some(wanted), Some(actual)) => wanted == actual,
            (Some(_), None) => matches!(item, StreamItem::Notice { .. }),
            (None, _) => true,
        }
    }
}

/// Whether `level` is at least as severe as `floor`.
///
/// [`LogLevel`]'s own ordering runs `Trace` (least) to `Error` (most), and
/// this is the one place that fact is relied on — a comparison spelled out
/// here rather than repeated at each call site.
#[must_use]
pub fn level_at_least(level: LogLevel, floor: LogLevel) -> bool {
    severity(level) >= severity(floor)
}

/// A total order over severities, most severe highest.
const fn severity(level: LogLevel) -> u8 {
    match level {
        LogLevel::Trace => 0,
        LogLevel::Debug => 1,
        LogLevel::Info => 2,
        LogLevel::Warn => 3,
        LogLevel::Error => 4,
        // `LogLevel` is `#[non_exhaustive]` (the protocol may grow a
        // severity); an unknown one is treated as the most severe rather
        // than silently filtered out, because news the CLI cannot classify
        // is exactly the news a user must not miss.
        _ => u8::MAX,
    }
}

/// Parses a `--level` argument, accepting the spellings a user types.
///
/// # Errors
///
/// [`crate::error::CliError::BadArgument`] naming the accepted values.
pub fn parse_level(flag: &'static str, text: &str) -> Result<LogLevel, crate::error::CliError> {
    match text.trim().to_ascii_lowercase().as_str() {
        "trace" => Ok(LogLevel::Trace),
        "debug" => Ok(LogLevel::Debug),
        "info" => Ok(LogLevel::Info),
        "warn" | "warning" => Ok(LogLevel::Warn),
        "error" | "err" => Ok(LogLevel::Error),
        _ => Err(crate::error::CliError::BadArgument {
            flag,
            value: text.to_owned(),
            reason: "expected one of trace, debug, info, warn, error".to_owned(),
        }),
    }
}

/// The [`ReportSink`] `astrs run` installs on its embedded daemon.
///
/// Turns the daemon's upward traffic into [`StreamItem`]s a printer task
/// consumes, without ever blocking or failing the event loop.
#[derive(Debug)]
pub struct TerminalLogSink {
    /// The bounded queue into the printer.
    sender: mpsc::Sender<StreamItem>,
    /// How many items did not fit.
    dropped: AtomicU64,
}

impl TerminalLogSink {
    /// Builds a sink and the receiver a printer task drains.
    #[must_use]
    pub fn new() -> (Self, mpsc::Receiver<StreamItem>) {
        let (sender, receiver) = mpsc::channel(STREAM_CAPACITY);
        (
            Self {
                sender,
                dropped: AtomicU64::new(0),
            },
            receiver,
        )
    }

    /// Queues one item, counting it as dropped if there is no room.
    fn offer(&self, item: StreamItem) {
        if self.sender.try_send(item).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl ReportSink for TerminalLogSink {
    fn report(&self, event: DaemonEvent) {
        match event {
            DaemonEvent::Log { records, .. } => {
                for record in records {
                    self.offer(StreamItem::Record(Box::new(record)));
                }
            }
            DaemonEvent::SpawnResult { node, outcome, .. } => {
                if let Some(message) = spawn_failure_text(&outcome) {
                    self.offer(StreamItem::Notice {
                        level: LogLevel::Error,
                        node: Some(node),
                        message,
                    });
                }
            }
            DaemonEvent::NodeStopped {
                node,
                cause,
                restarting,
                ..
            } => {
                self.offer(StreamItem::Notice {
                    level: exit_level(&cause),
                    node: Some(node),
                    message: exit_text(&cause, restarting),
                });
            }
            // Heartbeats, metric batches, tapped topic frames, catch-up
            // acknowledgements: real events, but not *news a terminal wants*.
            // Deliberately dropped here rather than filtered downstream, so
            // a 2 s metrics tick never costs a channel slot a log line could
            // have used.
            _ => {}
        }
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn is_open(&self) -> bool {
        !self.sender.is_closed()
    }
}

/// The message for a spawn that did not produce a running node, or `None`
/// when it did.
fn spawn_failure_text(outcome: &SpawnOutcome) -> Option<String> {
    match outcome {
        SpawnOutcome::Spawned { .. } | SpawnOutcome::AwaitingDynamic => None,
        SpawnOutcome::Failed { message, errno } => Some(match errno {
            Some(number) => format!("failed to spawn: {message} (errno {number})"),
            None => format!("failed to spawn: {message}"),
        }),
        SpawnOutcome::Cancelled => Some("spawn cancelled: the dataflow stopped first".to_owned()),
        other => Some(format!("spawn did not start the node: {other:?}")),
    }
}

/// How severe a node's exit is.
fn exit_level(cause: &NodeExitCause) -> LogLevel {
    if cause.is_failure() {
        LogLevel::Error
    } else {
        LogLevel::Info
    }
}

/// The one-line summary of a node's exit.
fn exit_text(cause: &NodeExitCause, restarting: bool) -> String {
    if restarting {
        format!("exited ({cause}); restarting")
    } else {
        format!("exited ({cause})")
    }
}

/// How the terminal is painted.
#[derive(Debug, Clone, Copy)]
pub struct LogStyle {
    /// Whether to emit ANSI SGR codes.
    pub color: bool,
    /// How wide the `[node]` column is, so messages line up.
    pub prefix_width: usize,
    /// Whether to prefix each line with the seconds since the stream
    /// started.
    pub elapsed: bool,
}

impl Default for LogStyle {
    fn default() -> Self {
        Self {
            color: false,
            prefix_width: 0,
            elapsed: true,
        }
    }
}

/// Formats one item as the line a terminal shows.
///
/// `elapsed` is the time since the stream started, used only when
/// [`LogStyle::elapsed`] is set — the wall clock a user cares about during a
/// run is "how long has this been going", not a date, and deriving it here
/// keeps the formatter free of any dependency on how the caller measures
/// time.
#[must_use]
pub fn render(item: &StreamItem, style: LogStyle, elapsed: std::time::Duration) -> String {
    let mut line = String::with_capacity(item.message().len() + 32);
    if style.elapsed {
        line.push_str(&format!("{:>8.3}s ", elapsed.as_secs_f64()));
    }
    let name = item
        .node()
        .map_or_else(|| "astrs".to_owned(), |node| node.as_str().to_owned());
    let padded = if name.len() >= style.prefix_width {
        name.clone()
    } else {
        format!("{name:<width$}", width = style.prefix_width)
    };
    if style.color {
        line.push_str(&format!("{}[{padded}]{} ", node_color(&name), ansi::RESET));
    } else {
        line.push_str(&format!("[{padded}] "));
    }
    let level = item.level();
    if level_at_least(level, LogLevel::Warn) {
        if style.color {
            line.push_str(&format!(
                "{}{}{} ",
                level_color(level),
                level_tag(level),
                ansi::RESET
            ));
        } else {
            line.push_str(&format!("{} ", level_tag(level)));
        }
    }
    line.push_str(item.message());
    line
}

/// The uppercase tag shown for a warning or an error.
const fn level_tag(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Trace => "TRACE",
        LogLevel::Debug => "DEBUG",
        LogLevel::Info => "INFO",
        LogLevel::Warn => "WARN",
        LogLevel::Error => "ERROR",
        _ => "LOG",
    }
}

/// Plain ANSI SGR codes — the same handful [`crate::diagnostic`] uses, kept
/// here rather than shared because these are *foreground palette* entries
/// for node names, not severity colors.
mod ansi {
    /// Ends any styling.
    pub const RESET: &str = "\x1b[0m";
    /// The per-node palette, chosen for legibility on both light and dark
    /// terminals (no bright yellow, no pure blue).
    pub const PALETTE: &[&str] = &[
        "\x1b[36m", // cyan
        "\x1b[35m", // magenta
        "\x1b[32m", // green
        "\x1b[34m", // blue
        "\x1b[33m", // yellow
        "\x1b[31m", // red
    ];
    /// Warnings.
    pub const YELLOW: &str = "\x1b[33m";
    /// Errors.
    pub const RED: &str = "\x1b[31m";
}

/// A stable color for a node name.
///
/// Stable across the whole run *and* across runs: the same graph always
/// paints `camera` the same color, so a user reading two terminals side by
/// side can match them up. A hash rather than an assignment counter is what
/// makes that true even though nodes register in a nondeterministic order.
fn node_color(name: &str) -> &'static str {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let index = usize::try_from(hash % ansi::PALETTE.len() as u64).unwrap_or(0);
    ansi::PALETTE.get(index).copied().unwrap_or(ansi::RESET)
}

/// The color a severity tag is painted.
const fn level_color(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => ansi::RED,
        _ => ansi::YELLOW,
    }
}

/// Drains `receiver` onto `out` until the stream ends, returning how many
/// items were printed.
///
/// Ends when every sender is dropped — which, for `astrs run`, is when the
/// daemon and the sink it held are both gone, i.e. exactly when the run is
/// over and there is nothing left to print.
pub async fn print_stream(
    receiver: &mut mpsc::Receiver<StreamItem>,
    out: &mut (dyn Write + Send),
    filter: &LogFilter,
    style: LogStyle,
    started: std::time::Instant,
) -> usize {
    let mut printed = 0;
    while let Some(item) = receiver.recv().await {
        if !filter.accepts(&item) {
            continue;
        }
        let line = render(&item, style, started.elapsed());
        if writeln!(out, "{line}").is_err() {
            // The terminal went away (a closed pipe). Nothing useful is left
            // to do, and failing the run because `head -5` exited would be
            // absurd; stop printing and let the run finish on its own terms.
            break;
        }
        let _ = out.flush();
        printed += 1;
    }
    printed
}

/// The widest node name in a manifest, so the `[node]` column is sized
/// before the first line is printed rather than jittering as nodes appear.
#[must_use]
pub fn prefix_width<'a>(names: impl IntoIterator<Item = &'a str>) -> usize {
    names
        .into_iter()
        .map(str::len)
        .max()
        .unwrap_or(0)
        .max("astrs".len())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_time::HlcTimestamp;

    fn node(name: &str) -> NodeId {
        NodeId::new(name).unwrap()
    }

    fn record(level: LogLevel, name: &str, message: &str) -> StreamItem {
        StreamItem::Record(Box::new(
            LogRecord::new(HlcTimestamp::new(1, 0), level, message).with_node(node(name)),
        ))
    }

    #[test]
    fn severities_are_ordered_trace_to_error() {
        assert!(level_at_least(LogLevel::Error, LogLevel::Trace));
        assert!(level_at_least(LogLevel::Info, LogLevel::Info));
        assert!(!level_at_least(LogLevel::Debug, LogLevel::Info));
    }

    #[test]
    fn a_level_filter_hides_what_is_below_it() {
        let filter = LogFilter::new().with_min_level(LogLevel::Warn);
        assert!(!filter.accepts(&record(LogLevel::Info, "cam", "hi")));
        assert!(filter.accepts(&record(LogLevel::Warn, "cam", "hi")));
        assert!(filter.accepts(&record(LogLevel::Error, "cam", "hi")));
    }

    #[test]
    fn a_node_filter_keeps_only_that_node_but_never_hides_a_global_notice() {
        let filter = LogFilter::new().with_node(Some(node("cam")));
        assert!(filter.accepts(&record(LogLevel::Info, "cam", "mine")));
        assert!(!filter.accepts(&record(LogLevel::Info, "detect", "theirs")));
        assert!(filter.accepts(&StreamItem::Notice {
            level: LogLevel::Error,
            node: None,
            message: "the dataflow failed".to_owned(),
        }));
    }

    #[test]
    fn parse_level_accepts_the_spellings_a_user_types() {
        assert_eq!(parse_level("level", "WARN").unwrap(), LogLevel::Warn);
        assert_eq!(parse_level("level", "warning").unwrap(), LogLevel::Warn);
        assert_eq!(parse_level("level", " error ").unwrap(), LogLevel::Error);
        let error = parse_level("level", "loud").unwrap_err();
        assert!(error.to_string().contains("trace"), "{error}");
    }

    #[test]
    fn rendering_pads_the_node_column_and_omits_a_tag_below_warn() {
        let style = LogStyle {
            color: false,
            prefix_width: 8,
            elapsed: false,
        };
        let line = render(
            &record(LogLevel::Info, "cam", "frame 1"),
            style,
            std::time::Duration::ZERO,
        );
        assert_eq!(line, "[cam     ] frame 1");

        let line = render(
            &record(LogLevel::Error, "cam", "boom"),
            style,
            std::time::Duration::ZERO,
        );
        assert_eq!(line, "[cam     ] ERROR boom");
    }

    #[test]
    fn rendering_with_color_wraps_the_prefix_and_resets() {
        let style = LogStyle {
            color: true,
            prefix_width: 0,
            elapsed: false,
        };
        let line = render(
            &record(LogLevel::Info, "cam", "hi"),
            style,
            std::time::Duration::ZERO,
        );
        assert!(line.contains("\x1b["), "{line}");
        assert!(line.ends_with("hi"), "{line}");
        assert!(line.contains(ansi::RESET));
    }

    #[test]
    fn elapsed_is_rendered_when_asked_for() {
        let style = LogStyle {
            color: false,
            prefix_width: 0,
            elapsed: true,
        };
        let line = render(
            &record(LogLevel::Info, "cam", "hi"),
            style,
            std::time::Duration::from_millis(1500),
        );
        assert!(line.starts_with("   1.500s "), "{line}");
    }

    #[test]
    fn a_node_keeps_one_color_across_calls() {
        assert_eq!(node_color("camera"), node_color("camera"));
    }

    #[test]
    fn the_prefix_width_never_shrinks_below_the_cli_s_own_name() {
        assert_eq!(prefix_width(["a", "bb"]), "astrs".len());
        assert_eq!(prefix_width(["a-very-long-node"]), "a-very-long-node".len());
        assert_eq!(prefix_width(std::iter::empty()), "astrs".len());
    }

    #[tokio::test]
    async fn the_sink_turns_daemon_log_events_into_stream_items() {
        let (sink, mut receiver) = TerminalLogSink::new();
        sink.report(DaemonEvent::Log {
            request: None,
            records: vec![
                LogRecord::new(HlcTimestamp::new(1, 0), LogLevel::Info, "one")
                    .with_node(node("cam")),
                LogRecord::new(HlcTimestamp::new(2, 0), LogLevel::Error, "two")
                    .with_node(node("cam")),
            ],
            truncated: false,
        });
        assert_eq!(receiver.recv().await.unwrap().message(), "one");
        assert_eq!(receiver.recv().await.unwrap().message(), "two");
        assert_eq!(sink.dropped(), 0);
    }

    #[tokio::test]
    async fn the_sink_summarizes_a_node_exit_and_marks_a_restart() {
        let (sink, mut receiver) = TerminalLogSink::new();
        sink.report(DaemonEvent::NodeStopped {
            dataflow: astrs_wire::DataflowId::from_u128(1),
            node: node("cam"),
            generation: 0,
            cause: NodeExitCause::ExitCode { code: 3 },
            restarting: true,
        });
        let item = receiver.recv().await.unwrap();
        assert_eq!(item.level(), LogLevel::Error);
        assert!(item.message().contains("restarting"), "{item:?}");
    }

    #[tokio::test]
    async fn the_sink_ignores_traffic_a_terminal_does_not_want() {
        let (sink, mut receiver) = TerminalLogSink::new();
        sink.report(DaemonEvent::StateCatchUpAck { seq: 1, applied: 0 });
        assert!(
            receiver.try_recv().is_err(),
            "a catch-up acknowledgement is not news"
        );
    }

    #[tokio::test]
    async fn the_sink_counts_what_does_not_fit_instead_of_blocking() {
        let (sink, receiver) = TerminalLogSink::new();
        for index in 0..(STREAM_CAPACITY + 8) {
            sink.report(DaemonEvent::Log {
                request: None,
                records: vec![LogRecord::new(
                    HlcTimestamp::new(1, 0),
                    LogLevel::Info,
                    format!("line {index}"),
                )],
                truncated: false,
            });
        }
        assert_eq!(sink.dropped(), 8, "the overflow is counted, not queued");
        drop(receiver);
        assert!(!sink.is_open());
    }

    #[tokio::test]
    async fn printing_applies_the_filter_and_stops_when_the_senders_go() {
        let (sink, mut receiver) = TerminalLogSink::new();
        sink.report(DaemonEvent::Log {
            request: None,
            records: vec![
                LogRecord::new(HlcTimestamp::new(1, 0), LogLevel::Debug, "quiet")
                    .with_node(node("cam")),
                LogRecord::new(HlcTimestamp::new(2, 0), LogLevel::Error, "loud")
                    .with_node(node("cam")),
            ],
            truncated: false,
        });
        drop(sink);

        let mut out: Vec<u8> = Vec::new();
        let filter = LogFilter::new().with_min_level(LogLevel::Warn);
        let style = LogStyle {
            color: false,
            prefix_width: 0,
            elapsed: false,
        };
        let printed = print_stream(
            &mut receiver,
            &mut out,
            &filter,
            style,
            std::time::Instant::now(),
        )
        .await;
        assert_eq!(printed, 1);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("loud"), "{text}");
        assert!(!text.contains("quiet"), "{text}");
    }
}
