//! The internal event channel — how everything reaches the event loop.
//!
//! The daemon's merged loop (§4.3) selects over four sources: node listener
//! accepts, per-node session traffic, timer ticks, and *this* — one
//! multi-producer channel every background task hands its news to. A child
//! exiting, a log line arriving on a captured pipe, a backoff elapsing: each
//! runs in its own task, and each ends up here as one [`DaemonEvent`],
//! [`astrs_time::Stamped`] with the HLC timestamp the loop assigns on receipt.
//!
//! ```text
//!   waiter task ──ProcessExited{node, gen, status}──┐
//!   log pump    ──NodeOutput{node, stream, line}────┤
//!   backoff     ──RestartDue{node, gen}─────────────┼──► DaemonHandle ──► event loop
//!   session     ──Request{session, request}─────────┤
//!   session     ──SessionClosed{session}────────────┤
//!   uplink      ──CoordinatorFrame{event}───────────┘
//! ```
//!
//! # Why an unbounded channel
//!
//! The producers are the daemon's own tasks, and every one of them is
//! *reporting a fact that has already happened*: a process that exited cannot
//! un-exit while waiting for capacity. A bounded channel would put back
//! pressure on a waiter task, which would delay reaping, which would delay the
//! restart — turning a queueing problem into a liveness problem. The data
//! plane is bounded (that is [`crate::local::NodeMailbox`]'s job); the control
//! plane is not.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::session::{DaemonEvent, event_channel};
//! use astrs_wire::SessionId;
//!
//! # #[tokio::main] async fn main() {
//! let (handle, mut events) = event_channel();
//! handle.send(DaemonEvent::SessionClosed { session: SessionId::from_u128(1) });
//!
//! let received = events.recv().await.expect("the handle is alive");
//! assert_eq!(received.kind_name(), "session_closed");
//! # }
//! ```

use std::process::ExitStatus;

use astrs_log::StdioStream;
use astrs_wire::{DaemonId, DataflowId, NodeId, NodeRequest, PeerEvent, SessionId};
use tokio::sync::mpsc;

use crate::error::DaemonError;

/// Something that happened, on its way to the event loop.
#[derive(Debug)]
#[non_exhaustive]
pub enum DaemonEvent {
    /// A node sent a request on its session.
    Request {
        /// The session it came in on.
        session: SessionId,
        /// The request.
        request: Box<NodeRequest>,
    },
    /// A node emitted a log record on its session (§9.1 `Node::log_*`).
    ///
    /// A separate variant rather than a [`NodeRequest`] because it arrives as
    /// a separate *frame family* (`kind = Log`, §7.1/§7.3) — the same framing
    /// a log subscription rides — and the loop routes it to the log fan-out
    /// rather than to the request handlers.
    NodeLog {
        /// The session it came in on.
        session: SessionId,
        /// The record, with the subscription id the node stamped on it.
        frame: Box<astrs_wire::LogFrame>,
    },
    /// A node's connection ended.
    ///
    /// The crash path and the clean-exit path arrive identically here, which
    /// is deliberate: the daemon must reclaim the same state either way.
    SessionClosed {
        /// The session that ended.
        session: SessionId,
    },
    /// A session could not be served.
    SessionFailed {
        /// The session that failed, if it got far enough to have one.
        session: Option<SessionId>,
        /// Why.
        error: Box<DaemonError>,
    },
    /// A spawned child exited and was reaped.
    ProcessExited {
        /// The dataflow it belonged to.
        dataflow: DataflowId,
        /// The node it was.
        node: NodeId,
        /// The incarnation that exited (§12).
        generation: u64,
        /// How it exited, or the I/O error that prevented finding out.
        status: Result<ExitStatus, String>,
    },
    /// A line arrived on a captured child stream (§13).
    NodeOutput {
        /// The dataflow it belonged to.
        dataflow: DataflowId,
        /// The node that wrote it.
        node: NodeId,
        /// Which stream.
        stream: StdioStream,
        /// The line, without its terminator.
        line: String,
    },
    /// A captured child stream reached end of file.
    NodeOutputClosed {
        /// The dataflow it belonged to.
        dataflow: DataflowId,
        /// The node whose stream closed.
        node: NodeId,
        /// Which stream.
        stream: StdioStream,
    },
    /// A restart backoff elapsed; the node may be respawned.
    RestartDue {
        /// The dataflow it belongs to.
        dataflow: DataflowId,
        /// The node to respawn.
        node: NodeId,
        /// The incarnation that failed — the respawn takes the next one.
        generation: u64,
    },
    /// A peer daemon connection was established, in either direction (§6.4).
    ///
    /// Carried as an event rather than installed directly because the dial and
    /// the accept both happen in their own tasks, and the link table — like
    /// every other mutable fact — has exactly one owner: the event loop.
    PeerAttached {
        /// The established link.
        link: Box<crate::peer::PeerLink>,
    },
    /// A frame arrived from a peer daemon (§7.3 `PeerEvent`).
    PeerFrame {
        /// The peer that sent it.
        daemon: DaemonId,
        /// The frame.
        event: Box<PeerEvent>,
    },
    /// A peer daemon connection ended (§12 peer partition).
    PeerLost {
        /// The peer that went away.
        daemon: DaemonId,
        /// What the transport reported.
        reason: String,
    },
    /// The coordinator uplink completed a greeting and a registration (§4.2).
    ///
    /// The same shape as [`Self::PeerAttached`], and for the same reason: the
    /// dial happens in its own task, and the link's consequences — clearing
    /// degraded-autonomous mode, re-announcing this daemon's dataflows — are
    /// applied by the one owner of every mutable fact.
    CoordinatorConnected {
        /// The session the coordinator assigned this connection.
        session: SessionId,
        /// How many times the uplink has connected, this one included, so a
        /// late frame from a previous connection can be told apart from a
        /// current one.
        epoch: u64,
    },
    /// An instruction arrived from the coordinator (§7.3 `CoordinatorEvent`).
    CoordinatorFrame {
        /// The instruction.
        event: Box<astrs_wire::CoordinatorEvent>,
    },
    /// The coordinator uplink dropped; the daemon runs autonomously (§12).
    CoordinatorLost {
        /// What the transport reported.
        reason: String,
    },
    /// The daemon was asked to shut down.
    Shutdown,
}

impl DaemonEvent {
    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Request { .. } => "request",
            Self::NodeLog { .. } => "node_log",
            Self::SessionClosed { .. } => "session_closed",
            Self::SessionFailed { .. } => "session_failed",
            Self::ProcessExited { .. } => "process_exited",
            Self::NodeOutput { .. } => "node_output",
            Self::NodeOutputClosed { .. } => "node_output_closed",
            Self::RestartDue { .. } => "restart_due",
            Self::PeerAttached { .. } => "peer_attached",
            Self::PeerFrame { .. } => "peer_frame",
            Self::PeerLost { .. } => "peer_lost",
            Self::CoordinatorConnected { .. } => "coordinator_connected",
            Self::CoordinatorFrame { .. } => "coordinator_frame",
            Self::CoordinatorLost { .. } => "coordinator_lost",
            Self::Shutdown => "shutdown",
        }
    }

    /// The node this event concerns, when it concerns one.
    #[must_use]
    pub const fn node(&self) -> Option<&NodeId> {
        match self {
            Self::ProcessExited { node, .. }
            | Self::NodeOutput { node, .. }
            | Self::NodeOutputClosed { node, .. }
            | Self::RestartDue { node, .. } => Some(node),
            _ => None,
        }
    }

    /// The dataflow this event concerns, when it concerns one.
    #[must_use]
    pub const fn dataflow(&self) -> Option<DataflowId> {
        match self {
            Self::ProcessExited { dataflow, .. }
            | Self::NodeOutput { dataflow, .. }
            | Self::NodeOutputClosed { dataflow, .. }
            | Self::RestartDue { dataflow, .. } => Some(*dataflow),
            _ => None,
        }
    }

    /// The session this event concerns, when it concerns one.
    #[must_use]
    pub const fn session(&self) -> Option<SessionId> {
        match self {
            Self::Request { session, .. }
            | Self::SessionClosed { session }
            | Self::CoordinatorConnected { session, .. } => Some(*session),
            Self::SessionFailed { session, .. } => *session,
            _ => None,
        }
    }

    /// The peer daemon this event concerns, when it concerns one.
    #[must_use]
    pub fn peer(&self) -> Option<&DaemonId> {
        match self {
            Self::PeerFrame { daemon, .. } | Self::PeerLost { daemon, .. } => Some(daemon),
            Self::PeerAttached { link } => Some(link.daemon()),
            _ => None,
        }
    }

    /// Whether this event asks the loop to stop.
    #[must_use]
    pub const fn is_shutdown(&self) -> bool {
        matches!(self, Self::Shutdown)
    }
}

/// A cloneable sender every background task keeps.
#[derive(Debug, Clone)]
pub struct DaemonHandle {
    /// The channel into the event loop.
    sender: mpsc::UnboundedSender<DaemonEvent>,
}

impl DaemonHandle {
    /// Sends an event, returning whether the loop is still listening.
    ///
    /// A closed channel means the daemon is gone, which for a background task
    /// is not an error to propagate but a signal to stop — hence a `bool`
    /// rather than a `Result` nobody could act on.
    pub fn send(&self, event: DaemonEvent) -> bool {
        self.sender.send(event).is_ok()
    }

    /// Whether the event loop is still running.
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.sender.is_closed()
    }

    /// Asks the daemon to shut down.
    pub fn shutdown(&self) -> bool {
        self.send(DaemonEvent::Shutdown)
    }
}

/// The receiving end, owned by the event loop alone.
#[derive(Debug)]
pub struct DaemonEvents {
    /// The channel out of the background tasks.
    receiver: mpsc::UnboundedReceiver<DaemonEvent>,
}

impl DaemonEvents {
    /// Waits for the next event, or [`None`] once every handle is gone.
    pub async fn recv(&mut self) -> Option<DaemonEvent> {
        self.receiver.recv().await
    }

    /// Takes the next event if one is already waiting.
    pub fn try_recv(&mut self) -> Option<DaemonEvent> {
        self.receiver.try_recv().ok()
    }

    /// Closes the channel: queued events still drain, new ones are refused.
    pub fn close(&mut self) {
        self.receiver.close();
    }
}

/// Builds the internal event channel.
#[must_use]
pub fn event_channel() -> (DaemonHandle, DaemonEvents) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (DaemonHandle { sender }, DaemonEvents { receiver })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn node() -> NodeId {
        NodeId::new("camera").unwrap()
    }

    fn dataflow() -> DataflowId {
        DataflowId::from_u128(1)
    }

    fn every_event() -> Vec<DaemonEvent> {
        vec![
            DaemonEvent::Request {
                session: SessionId::from_u128(1),
                request: Box::new(NodeRequest::EventStreamDropped),
            },
            DaemonEvent::SessionClosed {
                session: SessionId::from_u128(1),
            },
            DaemonEvent::SessionFailed {
                session: None,
                error: Box::new(DaemonError::ShuttingDown),
            },
            DaemonEvent::ProcessExited {
                dataflow: dataflow(),
                node: node(),
                generation: 2,
                status: Err("no such process".into()),
            },
            DaemonEvent::NodeOutput {
                dataflow: dataflow(),
                node: node(),
                stream: StdioStream::Stdout,
                line: "hello".into(),
            },
            DaemonEvent::NodeOutputClosed {
                dataflow: dataflow(),
                node: node(),
                stream: StdioStream::Stderr,
            },
            DaemonEvent::RestartDue {
                dataflow: dataflow(),
                node: node(),
                generation: 2,
            },
            DaemonEvent::PeerFrame {
                daemon: DaemonId::generate(None),
                event: Box::new(PeerEvent::ping(1, Default::default())),
            },
            DaemonEvent::PeerLost {
                daemon: DaemonId::generate(None),
                reason: "connection reset".into(),
            },
            DaemonEvent::CoordinatorConnected {
                session: SessionId::from_u128(9),
                epoch: 1,
            },
            DaemonEvent::CoordinatorFrame {
                event: Box::new(astrs_wire::CoordinatorEvent::Heartbeat {
                    seq: 1,
                    sent_at: Default::default(),
                }),
            },
            DaemonEvent::CoordinatorLost {
                reason: "connection reset".into(),
            },
            DaemonEvent::Shutdown,
        ]
    }

    #[test]
    fn every_event_has_a_distinct_label() {
        let mut names: Vec<&str> = every_event().iter().map(DaemonEvent::kind_name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "labels collide");
    }

    #[test]
    fn accessors_report_what_the_variant_carries() {
        for event in every_event() {
            match event.kind_name() {
                "request" | "session_closed" => {
                    assert!(event.session().is_some());
                    assert!(event.node().is_none());
                }
                "process_exited" | "node_output" | "node_output_closed" | "restart_due" => {
                    assert!(event.node().is_some());
                    assert_eq!(event.dataflow(), Some(dataflow()));
                    assert!(event.session().is_none());
                }
                "shutdown" => assert!(event.is_shutdown()),
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn events_arrive_in_the_order_they_were_sent() {
        let (handle, mut events) = event_channel();
        for index in 0..4u128 {
            assert!(handle.send(DaemonEvent::SessionClosed {
                session: SessionId::from_u128(index)
            }));
        }
        for index in 0..4u128 {
            let event = events.recv().await.expect("sent");
            assert_eq!(event.session(), Some(SessionId::from_u128(index)));
        }
    }

    #[tokio::test]
    async fn many_producers_share_one_handle() {
        let (handle, mut events) = event_channel();
        let mut tasks = Vec::new();
        for index in 0..8u128 {
            let handle = handle.clone();
            tasks.push(tokio::spawn(async move {
                handle.send(DaemonEvent::SessionClosed {
                    session: SessionId::from_u128(index),
                })
            }));
        }
        for task in tasks {
            assert!(task.await.expect("task"));
        }
        let mut seen = 0;
        while events.try_recv().is_some() {
            seen += 1;
        }
        assert_eq!(seen, 8);
    }

    #[tokio::test]
    async fn a_dropped_receiver_makes_sends_fail_rather_than_block() {
        let (handle, events) = event_channel();
        drop(events);
        assert!(!handle.is_open());
        assert!(!handle.send(DaemonEvent::Shutdown));
    }

    #[tokio::test]
    async fn closing_lets_queued_events_drain() {
        let (handle, mut events) = event_channel();
        handle.send(DaemonEvent::Shutdown);
        events.close();
        assert!(events.recv().await.is_some(), "the queued event survived");
        assert!(events.recv().await.is_none());
    }

    #[tokio::test]
    async fn shutdown_is_a_one_call_convenience() {
        let (handle, mut events) = event_channel();
        assert!(handle.shutdown());
        assert!(events.recv().await.expect("sent").is_shutdown());
    }

    #[tokio::test]
    async fn recv_ends_when_every_handle_is_gone() {
        let (handle, mut events) = event_channel();
        drop(handle);
        assert!(events.recv().await.is_none());
    }
}
