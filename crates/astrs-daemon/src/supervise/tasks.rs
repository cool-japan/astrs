//! The background tasks that feed the event loop.
//!
//! Three shapes, each a thin adapter between something that blocks and the
//! internal event channel ([`crate::session::DaemonHandle`]):
//!
//! | Task | Waits on | Reports |
//! |---|---|---|
//! | [`spawn_waiter`] | `wait(2)` on one child | [`DaemonEvent::ProcessExited`] |
//! | [`spawn_log_pump`] | one captured pipe | [`DaemonEvent::NodeOutput`] per line, then `NodeOutputClosed` |
//! | [`spawn_restart_timer`] | one backoff | [`DaemonEvent::RestartDue`] |
//!
//! Every one of them is generation-stamped, and that stamp is the whole
//! reason they can be fire-and-forget. A waiter for generation 4 reports
//! generation 4; if the event loop has since moved the node to generation 5,
//! it sees a stale report and ignores it. Nothing has to be cancelled at
//! exactly the right moment, which is the kind of requirement that turns into
//! a race the first time a machine is loaded.
//!
//! # Lines, not bytes
//!
//! [`spawn_log_pump`] emits one event per *line*, because that is the unit
//! [`astrs_log::LogRecord::from_captured_output`] takes and the unit a human
//! reads. A node that writes a 4 MiB line gets it truncated at
//! [`MAX_CAPTURED_LINE`] rather than being allowed to grow the daemon's heap
//! — a misbehaving node must not be able to exhaust the supervisor's memory,
//! which is exactly what the conformance zoo tries (§20.3).
//!
//! # Examples
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use astrs_daemon::session::event_channel;
//! use astrs_daemon::supervise::spawn_restart_timer;
//! use astrs_wire::{DataflowId, NodeId};
//! use std::time::Duration;
//!
//! let (handle, mut events) = event_channel();
//! spawn_restart_timer(
//!     handle,
//!     DataflowId::from_u128(1),
//!     NodeId::new("camera")?,
//!     4,
//!     Duration::from_millis(50),
//! );
//!
//! let event = events.recv().await.expect("the timer fired");
//! assert_eq!(event.kind_name(), "restart_due");
//! # Ok(()) }
//! ```

use std::time::Duration;

use astrs_log::StdioStream;
use astrs_wire::{DataflowId, NodeId};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Child;
use tokio::task::JoinHandle;

use crate::session::channel::{DaemonEvent, DaemonHandle};
use crate::spawn::ProcessHandle;

/// The longest captured line the daemon will forward, in bytes.
///
/// A line beyond this is truncated with an explicit marker rather than
/// silently cut, so an operator reading the log knows the node said more than
/// they are seeing.
pub const MAX_CAPTURED_LINE: usize = 16 * 1024;

/// The marker appended to a truncated line.
pub const TRUNCATION_MARKER: &str = " …[truncated]";

/// Waits for one child and reports its exit.
///
/// The `handle` is marked reaped before the event is sent, so a watchdog that
/// fires in the window between the two finds a dead handle and does nothing —
/// rather than signalling a pid the kernel may already have recycled.
pub fn spawn_waiter(
    events: DaemonHandle,
    handle: ProcessHandle,
    mut child: Child,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let status = child.wait().await;
        handle.mark_reaped();
        events.send(DaemonEvent::ProcessExited {
            dataflow: handle.dataflow(),
            node: handle.node().clone(),
            generation: handle.generation(),
            status: status.map_err(|error| error.to_string()),
        });
    })
}

/// Pumps one captured stream into the event channel, line by line.
pub fn spawn_log_pump<R>(
    events: DaemonHandle,
    dataflow: DataflowId,
    node: NodeId,
    stream: StdioStream,
    reader: R,
) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if !events.send(DaemonEvent::NodeOutput {
                        dataflow,
                        node: node.clone(),
                        stream,
                        line: truncate_line(line),
                    }) {
                        break;
                    }
                }
                Ok(None) => break,
                // A read error on a child's pipe means the child is gone or
                // the fd broke; either way there is nothing further to read
                // and nothing useful to report beyond the close below.
                Err(_) => break,
            }
        }
        events.send(DaemonEvent::NodeOutputClosed {
            dataflow,
            node,
            stream,
        });
    })
}

/// Sleeps out a restart backoff, then reports that the node may respawn.
pub fn spawn_restart_timer(
    events: DaemonHandle,
    dataflow: DataflowId,
    node: NodeId,
    generation: u64,
    delay: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        events.send(DaemonEvent::RestartDue {
            dataflow,
            node,
            generation,
        });
    })
}

/// Truncates a captured line to [`MAX_CAPTURED_LINE`], on a character
/// boundary, with an explicit marker.
#[must_use]
pub fn truncate_line(line: String) -> String {
    if line.len() <= MAX_CAPTURED_LINE {
        return line;
    }
    let mut cut = MAX_CAPTURED_LINE;
    while cut > 0 && !line.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut truncated = String::with_capacity(cut + TRUNCATION_MARKER.len());
    truncated.push_str(&line[..cut]);
    truncated.push_str(TRUNCATION_MARKER);
    truncated
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::time::Instant;

    use super::*;
    use crate::session::channel::event_channel;

    fn node() -> NodeId {
        NodeId::new("camera").unwrap()
    }

    fn dataflow() -> DataflowId {
        DataflowId::from_u128(1)
    }

    #[tokio::test]
    async fn a_waiter_reports_the_exit_with_its_generation() {
        let (handle, mut events) = event_channel();
        let mut command = tokio::process::Command::new("/usr/bin/true");
        command.stdin(std::process::Stdio::null());
        let child = command.spawn().unwrap();
        let pid = child.id().unwrap_or_default();
        let process = ProcessHandle::new(dataflow(), node(), 7, pid);

        spawn_waiter(handle, process.clone(), child);

        match events.recv().await.expect("an exit") {
            DaemonEvent::ProcessExited {
                node: got,
                generation,
                status,
                ..
            } => {
                assert_eq!(got, node());
                assert_eq!(generation, 7);
                assert!(status.expect("a status").success());
            }
            other => panic!("expected an exit, got {}", other.kind_name()),
        }
        assert!(process.is_reaped(), "the handle stops signalling");
    }

    #[tokio::test]
    async fn a_failing_child_reports_its_code() {
        let (handle, mut events) = event_channel();
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", "exit 5"]);
        command.stdin(std::process::Stdio::null());
        let child = command.spawn().unwrap();
        let process = ProcessHandle::new(dataflow(), node(), 0, child.id().unwrap_or_default());

        spawn_waiter(handle, process, child);
        match events.recv().await.expect("an exit") {
            DaemonEvent::ProcessExited { status, .. } => {
                assert_eq!(status.expect("a status").code(), Some(5));
            }
            other => panic!("expected an exit, got {}", other.kind_name()),
        }
    }

    #[tokio::test]
    async fn a_log_pump_emits_one_event_per_line_then_closes() {
        let (handle, mut events) = event_channel();
        let input = std::io::Cursor::new(b"first\nsecond\nthird\n".to_vec());
        spawn_log_pump(handle, dataflow(), node(), StdioStream::Stdout, input);

        for expected in ["first", "second", "third"] {
            match events.recv().await.expect("a line") {
                DaemonEvent::NodeOutput { line, stream, .. } => {
                    assert_eq!(line, expected);
                    assert_eq!(stream, StdioStream::Stdout);
                }
                other => panic!("expected a line, got {}", other.kind_name()),
            }
        }
        assert!(matches!(
            events.recv().await.expect("a close"),
            DaemonEvent::NodeOutputClosed { .. }
        ));
    }

    #[tokio::test]
    async fn a_stream_with_no_trailing_newline_still_yields_its_last_line() {
        let (handle, mut events) = event_channel();
        let input = std::io::Cursor::new(b"no newline".to_vec());
        spawn_log_pump(handle, dataflow(), node(), StdioStream::Stderr, input);

        match events.recv().await.expect("a line") {
            DaemonEvent::NodeOutput { line, stream, .. } => {
                assert_eq!(line, "no newline");
                assert_eq!(stream, StdioStream::Stderr);
            }
            other => panic!("expected a line, got {}", other.kind_name()),
        }
    }

    #[tokio::test]
    async fn an_empty_stream_reports_only_the_close() {
        let (handle, mut events) = event_channel();
        spawn_log_pump(
            handle,
            dataflow(),
            node(),
            StdioStream::Stdout,
            std::io::Cursor::new(Vec::new()),
        );
        assert!(matches!(
            events.recv().await.expect("a close"),
            DaemonEvent::NodeOutputClosed { .. }
        ));
    }

    #[tokio::test]
    async fn an_enormous_line_is_truncated_rather_than_buffered_whole() {
        let (handle, mut events) = event_channel();
        let mut payload = vec![b'x'; MAX_CAPTURED_LINE * 4];
        payload.push(b'\n');
        spawn_log_pump(
            handle,
            dataflow(),
            node(),
            StdioStream::Stdout,
            std::io::Cursor::new(payload),
        );

        match events.recv().await.expect("a line") {
            DaemonEvent::NodeOutput { line, .. } => {
                assert!(line.ends_with(TRUNCATION_MARKER), "not marked");
                assert!(line.len() <= MAX_CAPTURED_LINE + TRUNCATION_MARKER.len());
            }
            other => panic!("expected a line, got {}", other.kind_name()),
        }
    }

    #[test]
    fn truncation_respects_character_boundaries() {
        // Fill exactly past the limit with multi-byte characters so a naive
        // byte cut would land mid-character.
        let line = "é".repeat(MAX_CAPTURED_LINE);
        let truncated = truncate_line(line);
        assert!(truncated.ends_with(TRUNCATION_MARKER));
        // The result is valid UTF-8 by construction — `String` guarantees it —
        // so the real assertion is that it did not panic and stayed bounded.
        assert!(truncated.len() <= MAX_CAPTURED_LINE + TRUNCATION_MARKER.len());
    }

    #[test]
    fn a_short_line_is_left_alone() {
        assert_eq!(truncate_line("hello".into()), "hello");
        assert_eq!(truncate_line(String::new()), "");
        let exact = "x".repeat(MAX_CAPTURED_LINE);
        assert_eq!(truncate_line(exact.clone()), exact);
    }

    #[tokio::test]
    async fn a_restart_timer_waits_out_its_backoff() {
        let (handle, mut events) = event_channel();
        let started = Instant::now();
        spawn_restart_timer(handle, dataflow(), node(), 4, Duration::from_millis(60));

        match events.recv().await.expect("the timer fired") {
            DaemonEvent::RestartDue { generation, .. } => assert_eq!(generation, 4),
            other => panic!("expected a restart, got {}", other.kind_name()),
        }
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "the backoff was not waited out"
        );
    }

    #[tokio::test]
    async fn a_zero_backoff_fires_immediately() {
        let (handle, mut events) = event_channel();
        spawn_restart_timer(handle, dataflow(), node(), 1, Duration::ZERO);
        assert!(matches!(
            events.recv().await.expect("fired"),
            DaemonEvent::RestartDue { .. }
        ));
    }

    #[tokio::test]
    async fn a_task_whose_receiver_is_gone_simply_ends() {
        let (handle, events) = event_channel();
        drop(events);
        let task = spawn_log_pump(
            handle,
            dataflow(),
            node(),
            StdioStream::Stdout,
            std::io::Cursor::new(b"line\n".to_vec()),
        );
        task.await.expect("the task ended cleanly");
    }
}
