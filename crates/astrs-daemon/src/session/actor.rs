//! [`SessionActor`] — one task per connected node.
//!
//! The transport half of the daemon↔node leg. It owns exactly one socket and
//! does exactly four things with it:
//!
//! 1. for a real socket (UDS/TCP), run the standard §7.2 greeting —
//!    `Hello` → `Welcome`/`Refused` — against the daemon's own [`AuthToken`],
//!    accepting only [`astrs_wire::Role::Node`];
//! 2. read [`astrs_wire::NodeRequest`]s and forward them to the event loop as
//!    [`crate::session::DaemonEvent::Request`];
//! 3. write [`astrs_wire::NodeEvent`]s the event loop hands it through a
//!    per-session outbox;
//! 4. report the connection ending, exactly once, however it ended.
//!
//! ```text
//!            ┌──────────── SessionActor (one task) ────────────┐
//!   socket ──┤ Hello/Welcome (§7.2, real sockets only)         │
//!            │ reader: NodeRequest ──► DaemonHandle            │
//!            │ writer: outbox ──► NodeEvent ──► socket         ├── socket
//!            └────────────────────────────────────────────────┘
//!                              ▲
//!                SessionSink ──┘  held by the event loop
//! ```
//!
//! # Admission: handshake vs. trusted
//!
//! [`SessionActor::new`] requires the greeting — this is what
//! [`crate::server::Daemon::attach`] uses for every connection that arrived
//! over a real [`crate::server::listener::ConnectionOrigin::Uds`] or
//! [`crate::server::listener::ConnectionOrigin::Tcp`] socket, and it is what
//! makes the §16 auth token actually checked on this leg (it previously was
//! not: nothing on the accept path ran [`astrs_transport::accept_with`] at
//! all, so any process that could reach the socket could register as a node
//! regardless of token — see this module's tests for the coverage this now
//! has).
//!
//! [`SessionActor::new_trusted`] skips the greeting entirely.
//! [`crate::server::Daemon::attach`] uses it only for
//! [`crate::server::listener::ConnectionOrigin::InProcess`] — the in-process
//! attach seam a same-process embedder or (today, exclusively) this crate's
//! own test suite uses, which is not reachable from outside the process at
//! all, so the token check has nothing meaningful to guard.
//!
//! # It holds no state
//!
//! Deliberately: the conversation's state is [`super::protocol`]'s and the
//! daemon's is [`crate::state`]'s. An actor that also tracked registration
//! would be a second copy of the truth, and the two copies would disagree the
//! first time a node reconnected. What the actor owns is the *socket* — which
//! is exactly what the event loop cannot own, because it must not block on it.
//!
//! # Generic over the stream
//!
//! [`SessionActor::serve`] takes any framed duplex, which is what lets the
//! tests drive a whole conversation over [`tokio::io::duplex`] with no real
//! socket and no ports — see this module's own tests and
//! `tests/session_conversation.rs`.

use std::time::Duration;

use astrs_transport::{ConnectionCounters, FramedDuplex, FramedReader, FramedWriter, accept_with};
use astrs_wire::{
    Acceptor, AuthToken, FrameFlags, FrameKind, FrameLimits, LogFrame, NegotiatedLimits, NodeEvent,
    NodeRequest, RoleSet, SessionAssignment, SessionId, WireMessage,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::error::DaemonError;
use crate::session::channel::{DaemonEvent, DaemonHandle};

/// How many events may be queued for one node's socket before the event loop
/// waits.
///
/// Bounded, unlike the internal event channel: this queue holds *data* on its
/// way to a node, and a node that has stopped reading must eventually apply
/// back pressure rather than growing the daemon's heap without limit. When it
/// fills, the sender's [`SessionSink::try_send`] fails and the caller decides
/// — usually by dropping the message and metering it, exactly as a full input
/// queue does.
pub const SESSION_OUTBOX_DEPTH: usize = 256;

/// How long the §7.2 greeting has to complete before this leg gives up.
///
/// Matches `astrs-node-api`'s own `REGISTRATION_TIMEOUT` and
/// `astrs-coordinator`'s `DEFAULT_HANDSHAKE_TIMEOUT`: the greeting is a small
/// prefix of the overall spawn deadline (§12), not a budget of its own that
/// needs separate tuning.
pub const DEFAULT_NODE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The event loop's end of one session's outbound queue.
#[derive(Debug, Clone)]
pub struct SessionSink {
    /// The session this writes to.
    session: SessionId,
    /// The queue into the writer half.
    sender: mpsc::Sender<NodeEvent>,
}

impl SessionSink {
    /// The session this sink addresses.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Queues an event without waiting.
    ///
    /// # Errors
    ///
    /// - [`DaemonError::ChannelClosed`] if the session has ended.
    /// - [`DaemonError::Timeout`] if the outbox is full — the node is not
    ///   reading, and the caller must decide what to drop.
    pub fn try_send(&self, event: NodeEvent) -> Result<(), DaemonError> {
        self.sender.try_send(event).map_err(|error| match error {
            mpsc::error::TrySendError::Closed(_) => DaemonError::ChannelClosed {
                what: "session outbox",
            },
            mpsc::error::TrySendError::Full(_) => DaemonError::Timeout {
                operation: "session outbox",
                millis: 0,
            },
        })
    }

    /// Queues an event, waiting for room.
    ///
    /// # Errors
    ///
    /// [`DaemonError::ChannelClosed`] if the session has ended.
    pub async fn send(&self, event: NodeEvent) -> Result<(), DaemonError> {
        self.sender
            .send(event)
            .await
            .map_err(|_| DaemonError::ChannelClosed {
                what: "session outbox",
            })
    }

    /// Whether the session is still connected.
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.sender.is_closed()
    }

    /// How many events may still be queued without waiting.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.sender.capacity()
    }
}

/// Whether a connection must present the §7.2 greeting before anything else
/// is read from it — see the module docs for exactly which origin gets
/// which.
#[derive(Debug, Clone)]
enum Admission {
    /// A real socket: negotiate `Hello`/`Welcome` against `token`, accepting
    /// only [`astrs_wire::Role::Node`].
    Handshake {
        /// The cluster/local token to check the greeting against (§16).
        token: AuthToken,
        /// How long the exchange may take.
        timeout: Duration,
    },
    /// The in-process attach seam: proceed straight to the ordinary
    /// conversation.
    Trusted,
}

/// One connected node's socket.
#[derive(Debug)]
pub struct SessionActor {
    /// The session this actor serves.
    session: SessionId,
    /// Where requests go.
    handle: DaemonHandle,
    /// Where outbound events come from.
    outbox: mpsc::Receiver<NodeEvent>,
    /// Whether (and how) this connection must greet before anything else.
    admission: Admission,
}

impl SessionActor {
    /// Builds an actor that requires the standard §7.2 handshake — for a
    /// connection that arrived over a real socket (UDS or TCP).
    ///
    /// `token` is the value every presented `Hello` is checked against
    /// (constant-time; see [`AuthToken`]'s own docs). Only
    /// [`astrs_wire::Role::Node`] is accepted — this leg is the daemon's node
    /// listener, never its peer or CLI-facing surface.
    #[must_use]
    pub fn new(session: SessionId, handle: DaemonHandle, token: AuthToken) -> (Self, SessionSink) {
        Self::build(
            session,
            handle,
            Admission::Handshake {
                token,
                timeout: DEFAULT_NODE_HANDSHAKE_TIMEOUT,
            },
        )
    }

    /// Builds an actor that skips the greeting entirely — for the in-process
    /// attach seam ([`crate::server::listener::NodeListeners::in_process`])
    /// only. Never use this for a real socket: it is what makes the §16
    /// token check meaningless for the connection it serves.
    #[must_use]
    pub fn new_trusted(session: SessionId, handle: DaemonHandle) -> (Self, SessionSink) {
        Self::build(session, handle, Admission::Trusted)
    }

    /// Shared constructor.
    fn build(
        session: SessionId,
        handle: DaemonHandle,
        admission: Admission,
    ) -> (Self, SessionSink) {
        let (sender, outbox) = mpsc::channel(SESSION_OUTBOX_DEPTH);
        (
            Self {
                session,
                handle,
                outbox,
                admission,
            },
            SessionSink { session, sender },
        )
    }

    /// Overrides how long the greeting may take (only meaningful for
    /// [`SessionActor::new`]; a no-op when this actor is trusted).
    #[must_use]
    pub fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        if let Admission::Handshake { timeout: slot, .. } = &mut self.admission {
            *slot = timeout;
        }
        self
    }

    /// The session this actor serves.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Serves one connection until either end closes it.
    ///
    /// For [`SessionActor::new`] (a real socket), the very first thing this
    /// does is the §7.2 greeting; a peer that never presents a valid `Hello`
    /// for the configured token, or that claims a role other than
    /// [`astrs_wire::Role::Node`], never reaches the ordinary conversation at
    /// all — the connection is reported failed-then-closed and dropped.
    ///
    /// After admission (immediate for [`SessionActor::new_trusted`]), reads
    /// and writes run concurrently — a node that is publishing while the
    /// daemon delivers to it must not deadlock, which is precisely what a
    /// read-then-write loop would do the moment both directions filled.
    ///
    /// Reports [`DaemonEvent::SessionClosed`] exactly once on every exit
    /// path once admission has happened, so the daemon's reclamation runs
    /// whether the node exited cleanly, crashed, sent something malformed,
    /// or never greeted correctly at all.
    pub async fn serve<R, W>(mut self, reader: R, writer: W)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let session = self.session;
        let handle = self.handle.clone();

        let (mut frame_reader, mut frame_writer) = match self.admission.clone() {
            Admission::Trusted => {
                let limits = FrameLimits::uds();
                (
                    FramedReader::new(reader, limits, ConnectionCounters::shared()),
                    FramedWriter::new(writer, limits, ConnectionCounters::shared()),
                )
            }
            Admission::Handshake { token, timeout } => {
                // Matches the proposal `astrs-node-api`'s own
                // `node_handshake_params` always sends, on every plane this
                // leg supports (§16 auth, not §7.1 checksum policy, is what
                // actually varies here) -- see the module docs.
                let proposed = NegotiatedLimits::uds();
                let mut framed = FramedDuplex::from_halves(
                    reader,
                    writer,
                    proposed.to_frame_limits(),
                    astrs_wire::DEFAULT_BUFFER_CAPACITY,
                    ConnectionCounters::shared(),
                );
                let acceptor = Acceptor::new(token)
                    .with_accepted_roles(RoleSet::NODES)
                    .with_limits(proposed);
                // The session id was already minted (and, for a real socket,
                // already handed to the caller as part of `AcceptedNode`)
                // before this actor existed, so there is nothing to decide
                // per-`Hello` here — every greeting on this leg gets the one
                // session this actor was built for.
                let assignment = SessionAssignment::Fresh(session);
                match accept_with(&mut framed, &acceptor, timeout, |_hello| assignment).await {
                    Ok(_accepted) => framed.into_halves(),
                    Err(err) => {
                        handle.send(DaemonEvent::SessionFailed {
                            session: Some(session),
                            error: Box::new(DaemonError::Transport(err)),
                        });
                        handle.send(DaemonEvent::SessionClosed { session });
                        return;
                    }
                }
            }
        };

        let reader_handle = handle.clone();
        let read_task = tokio::spawn(async move {
            loop {
                match frame_reader.recv_frame().await {
                    Ok(Some(frame)) => {
                        // Dispatch on the family, never on a guess. §7.1 puts
                        // `kind` in the header precisely so "a router can
                        // dispatch without decoding", and a node legitimately
                        // sends two families on this socket: `NodeRequest`
                        // (§7.3) and `Log` — the latter is what every
                        // `Node::log_*` call produces ("Log/topic
                        // subscriptions ride the same framing"). Decoding
                        // every frame as a `NodeRequest` made the second one
                        // look like a corrupt first one, and dropped the
                        // session of any node that logged.
                        match frame.kind() {
                            FrameKind::NodeRequest => {
                                let request = match frame.decode::<NodeRequest>() {
                                    Ok(request) => request,
                                    Err(err) => {
                                        reader_handle.send(DaemonEvent::SessionFailed {
                                            session: Some(session),
                                            error: Box::new(DaemonError::Wire(err)),
                                        });
                                        break;
                                    }
                                };
                                let terminal = matches!(request, NodeRequest::EventStreamDropped);
                                if !reader_handle.send(DaemonEvent::Request {
                                    session,
                                    request: Box::new(request),
                                }) {
                                    break;
                                }
                                if terminal {
                                    break;
                                }
                            }
                            FrameKind::Log => match frame.decode::<LogFrame>() {
                                Ok(log) => {
                                    if !reader_handle.send(DaemonEvent::NodeLog {
                                        session,
                                        frame: Box::new(log),
                                    }) {
                                        break;
                                    }
                                }
                                Err(err) => {
                                    reader_handle.send(DaemonEvent::SessionFailed {
                                        session: Some(session),
                                        error: Box::new(DaemonError::Wire(err)),
                                    });
                                    break;
                                }
                            },
                            // A family this leg does not carry. Reported and
                            // skipped rather than fatal: an unknown frame is
                            // a peer speaking a dialect this build does not
                            // know (§7.2's append-only rule), and tearing the
                            // session down would turn forward compatibility
                            // into an outage.
                            other => {
                                tracing::warn!(
                                    ?session,
                                    kind = other.as_str(),
                                    "ignoring a frame family a node session does not carry"
                                );
                            }
                        }
                    }
                    // A clean end of stream: the node closed its socket.
                    Ok(None) => break,
                    Err(err) => {
                        reader_handle.send(DaemonEvent::SessionFailed {
                            session: Some(session),
                            error: Box::new(DaemonError::Transport(err)),
                        });
                        break;
                    }
                }
            }
        });

        // The writer drains the outbox until one of three things happens: the
        // event loop drops the sink, the socket refuses a write, or the
        // *reader* ends. That last one is why this is a `select!` and not a
        // `while let`: a node that closed its socket is gone, and a daemon
        // still holding a `SessionSink` for it — which the event loop does,
        // until it processes the close it has not been told about yet — would
        // otherwise keep this task parked forever, and the close would never
        // be sent. The deadlock is real and this arm is what closes it.
        let mut read_task = read_task;
        let mut reader_finished = false;
        loop {
            tokio::select! {
                biased;
                // `JoinHandle` panics if polled after completion, so the arm
                // is guarded rather than merely `break`-ing: `select!` may
                // re-enter the loop for the write arm in the same iteration a
                // read finishes, and the guard keeps the completed handle out
                // of the next poll.
                _ = &mut read_task, if !reader_finished => {
                    reader_finished = true;
                    break;
                }
                queued = self.outbox.recv() => match queued {
                    Some(event) => {
                        if frame_writer.send_message(&event).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
        let _ = frame_writer.flush().await;
        let _ = frame_writer.shutdown().await;

        if !reader_finished {
            read_task.abort();
            let _ = read_task.await;
        }

        handle.send(DaemonEvent::SessionClosed { session });
    }

    /// Serves one connection over a single duplex stream.
    ///
    /// The shape a Unix socket, a TCP socket and [`tokio::io::duplex`] all
    /// take; the split happens here so the caller does not have to know.
    pub async fn serve_stream<S>(self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        self.serve(reader, writer).await;
    }
}

/// Frames a [`NodeEvent`] the way this leg does, for a test or a fake node.
///
/// # Errors
///
/// [`astrs_wire::WireError`] if the event exceeds the frame budget.
pub fn encode_event(
    event: &NodeEvent,
    limits: &FrameLimits,
) -> Result<Vec<u8>, astrs_wire::WireError> {
    let flags = if limits.require_crc() {
        FrameFlags::CRC
    } else {
        FrameFlags::EMPTY
    };
    event.to_frame(flags, limits)
}

/// Frames a [`NodeRequest`] the way this leg does, for a test or a fake node.
///
/// # Errors
///
/// [`astrs_wire::WireError`] if the request exceeds the frame budget.
pub fn encode_request(
    request: &NodeRequest,
    limits: &FrameLimits,
) -> Result<Vec<u8>, astrs_wire::WireError> {
    let flags = if limits.require_crc() {
        FrameFlags::CRC
    } else {
        FrameFlags::EMPTY
    };
    request.to_frame(flags, limits)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_wire::{DataflowId, Hello, NodeHandshake, NodeId, Role, StopCause};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::session::channel::event_channel;

    fn session() -> SessionId {
        SessionId::from_u128(1)
    }

    fn limits() -> FrameLimits {
        FrameLimits::uds()
    }

    fn token() -> AuthToken {
        AuthToken::from_bytes([9; 32])
    }

    /// Plays the client's half of the §7.2 greeting directly on `stream`,
    /// bypassing `astrs-transport`'s `initiate` (which would consume the
    /// whole duplex via `tokio::io::split`): every test in this module needs
    /// the *same* `DuplexStream` usable for its own raw reads/writes
    /// afterwards, so this operates through `&mut` exactly as those do.
    async fn client_greet(stream: &mut tokio::io::DuplexStream, token: AuthToken) {
        let hello = Hello::new(Role::Node, token);
        let bytes = astrs_wire::ControlRequest::Hello(hello)
            .to_frame(FrameFlags::EMPTY, &limits())
            .unwrap();
        stream.write_all(&bytes).await.unwrap();

        let mut reader = astrs_wire::io::AsyncFrameReader::new(&mut *stream, limits());
        let reply = reader
            .read_message::<astrs_wire::ControlReply>()
            .await
            .unwrap()
            .expect("a welcome");
        assert!(
            matches!(reply, astrs_wire::ControlReply::Welcome(_)),
            "{reply:?}"
        );
    }

    /// Runs one actor over an in-memory duplex, having already completed the
    /// §7.2 greeting on the fake node's behalf, and returns the fake node's
    /// end ready for the raw `NodeRequest`/`NodeEvent` traffic every test in
    /// this module drives by hand.
    async fn spawn_actor() -> (
        tokio::io::DuplexStream,
        SessionSink,
        crate::session::channel::DaemonEvents,
    ) {
        let (handle, events) = event_channel();
        let (actor, sink) = SessionActor::new(session(), handle, token());
        let (daemon_side, mut node_side) = tokio::io::duplex(64 * 1024);
        tokio::spawn(actor.serve_stream(daemon_side));
        client_greet(&mut node_side, token()).await;
        (node_side, sink, events)
    }

    async fn read_one_event(stream: &mut tokio::io::DuplexStream) -> NodeEvent {
        let mut reader = astrs_wire::io::AsyncFrameReader::new(stream, limits());
        reader
            .read_message::<NodeEvent>()
            .await
            .expect("a readable frame")
            .expect("a frame")
    }

    #[tokio::test]
    async fn a_request_reaches_the_event_loop() {
        let (mut node, _sink, mut events) = spawn_actor().await;
        let request = NodeRequest::Register(NodeHandshake::new(
            DataflowId::from_u128(7),
            NodeId::new("camera").unwrap(),
            0,
        ));
        node.write_all(&encode_request(&request, &limits()).unwrap())
            .await
            .unwrap();

        match events.recv().await.expect("an event") {
            DaemonEvent::Request {
                session: got,
                request: received,
            } => {
                assert_eq!(got, session());
                assert_eq!(*received, request);
            }
            other => panic!("expected a request, got {}", other.kind_name()),
        }
    }

    #[tokio::test]
    async fn an_event_reaches_the_node() {
        let (mut node, sink, _events) = spawn_actor().await;
        let event = NodeEvent::Stop {
            cause: StopCause::Requested,
            grace: None,
        };
        sink.try_send(event.clone()).unwrap();

        let received = read_one_event(&mut node).await;
        assert_eq!(received, event);
    }

    #[tokio::test]
    async fn many_events_arrive_in_order() {
        let (mut node, sink, _events) = spawn_actor().await;
        for index in 0..8u64 {
            sink.send(NodeEvent::Restarted {
                peer: NodeId::new("camera").unwrap(),
                generation: index,
            })
            .await
            .unwrap();
        }
        let mut reader = astrs_wire::io::AsyncFrameReader::new(&mut node, limits());
        for index in 0..8u64 {
            match reader
                .read_message::<NodeEvent>()
                .await
                .unwrap()
                .expect("a frame")
            {
                NodeEvent::Restarted { generation, .. } => assert_eq!(generation, index),
                other => panic!("expected a restart event, got {other}"),
            }
        }
    }

    #[tokio::test]
    async fn a_closed_socket_reports_the_session_once() {
        let (node, _sink, mut events) = spawn_actor().await;
        drop(node);

        let mut closes = 0;
        while let Some(event) = events.recv().await {
            if matches!(event, DaemonEvent::SessionClosed { .. }) {
                closes += 1;
            }
        }
        assert_eq!(closes, 1, "exactly one close, however the socket ended");
    }

    #[tokio::test]
    async fn dropping_the_sink_ends_the_actor() {
        let (mut node, sink, mut events) = spawn_actor().await;
        drop(sink);

        // The writer half finishes, which shuts the socket down.
        let mut buffer = [0u8; 8];
        let read = node.read(&mut buffer).await.unwrap_or(0);
        assert_eq!(read, 0, "the daemon closed its end");

        let mut saw_close = false;
        while let Some(event) = events.recv().await {
            saw_close |= matches!(event, DaemonEvent::SessionClosed { .. });
        }
        assert!(saw_close);
    }

    #[tokio::test]
    async fn a_connection_that_never_greets_is_refused_and_closed() {
        // No `client_greet` here: this is the pre-fix scenario -- a peer that
        // sends an ordinary request instead of a `Hello` -- and it must now
        // be refused during admission rather than accepted as if it had
        // greeted correctly.
        let (handle, mut events) = event_channel();
        let (actor, _sink) = SessionActor::new(session(), handle, token());
        let (daemon_side, mut node_side) = tokio::io::duplex(64 * 1024);
        tokio::spawn(actor.serve_stream(daemon_side));

        node_side
            .write_all(&encode_request(&NodeRequest::EventStreamDropped, &limits()).unwrap())
            .await
            .unwrap();

        let mut saw_failure = false;
        let mut saw_close = false;
        while let Some(event) = events.recv().await {
            match event {
                DaemonEvent::SessionFailed { session: got, .. } => {
                    assert_eq!(got, Some(session()));
                    saw_failure = true;
                }
                DaemonEvent::SessionClosed { .. } => {
                    saw_close = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_failure, "a request instead of a greeting is refused");
        assert!(saw_close, "and the session still closed");
    }

    #[tokio::test]
    async fn a_wrong_token_is_refused_and_closed() {
        let (handle, mut events) = event_channel();
        let (actor, _sink) = SessionActor::new(session(), handle, token());
        let (daemon_side, mut node_side) = tokio::io::duplex(64 * 1024);
        tokio::spawn(actor.serve_stream(daemon_side));

        let wrong = AuthToken::from_bytes([0xFF; 32]);
        let bytes = astrs_wire::ControlRequest::Hello(Hello::new(Role::Node, wrong))
            .to_frame(FrameFlags::EMPTY, &limits())
            .unwrap();
        node_side.write_all(&bytes).await.unwrap();

        let mut saw_close = false;
        while let Some(event) = events.recv().await {
            saw_close |= matches!(event, DaemonEvent::SessionClosed { .. });
        }
        assert!(saw_close, "a bad token still ends the session cleanly");
    }

    #[tokio::test]
    async fn a_non_node_role_is_refused() {
        let (handle, mut events) = event_channel();
        let (actor, _sink) = SessionActor::new(session(), handle, token());
        let (daemon_side, mut node_side) = tokio::io::duplex(64 * 1024);
        tokio::spawn(actor.serve_stream(daemon_side));

        let bytes = astrs_wire::ControlRequest::Hello(Hello::new(Role::Cli, token()))
            .to_frame(FrameFlags::EMPTY, &limits())
            .unwrap();
        node_side.write_all(&bytes).await.unwrap();

        let mut saw_close = false;
        while let Some(event) = events.recv().await {
            saw_close |= matches!(event, DaemonEvent::SessionClosed { .. });
        }
        assert!(saw_close, "a CLI greeting on the node leg is still refused");
    }

    #[tokio::test]
    async fn a_trusted_actor_skips_the_greeting() {
        let (handle, _events) = event_channel();
        let (actor, sink) = SessionActor::new_trusted(session(), handle);
        let (daemon_side, mut node_side) = tokio::io::duplex(64 * 1024);
        tokio::spawn(actor.serve_stream(daemon_side));

        // No `client_greet`: a trusted actor must accept a bare `NodeRequest`
        // as its very first frame, exactly as the in-process attach seam's
        // callers already rely on.
        let request = NodeRequest::EventStreamDropped;
        node_side
            .write_all(&encode_request(&request, &limits()).unwrap())
            .await
            .unwrap();
        drop(sink);
        let _ = node_side.shutdown().await;
    }

    #[tokio::test]
    async fn a_malformed_frame_is_reported_and_ends_the_session() {
        let (mut node, _sink, mut events) = spawn_actor().await;
        node.write_all(b"this is not a frame at all, not even close")
            .await
            .unwrap();

        let mut saw_failure = false;
        let mut saw_close = false;
        while let Some(event) = events.recv().await {
            match event {
                DaemonEvent::SessionFailed { session: got, .. } => {
                    assert_eq!(got, Some(session()));
                    saw_failure = true;
                }
                DaemonEvent::SessionClosed { .. } => {
                    saw_close = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_failure, "the codec error was reported");
        assert!(saw_close, "and the session still closed");
    }

    #[tokio::test]
    async fn an_event_stream_drop_ends_the_read_side() {
        let (mut node, _sink, mut events) = spawn_actor().await;
        node.write_all(&encode_request(&NodeRequest::EventStreamDropped, &limits()).unwrap())
            .await
            .unwrap();

        let mut saw_request = false;
        let mut saw_close = false;
        while let Some(event) = events.recv().await {
            match event {
                DaemonEvent::Request { .. } => saw_request = true,
                DaemonEvent::SessionClosed { .. } => {
                    saw_close = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_request);
        assert!(saw_close);
    }

    #[tokio::test]
    async fn the_sink_reports_its_own_health() {
        let (handle, _events) = event_channel();
        let (actor, sink) = SessionActor::new(session(), handle, token());
        assert_eq!(sink.session(), session());
        assert!(sink.is_open());
        assert_eq!(sink.capacity(), SESSION_OUTBOX_DEPTH);
        assert_eq!(actor.session(), session());

        drop(actor);
        assert!(!sink.is_open());
        assert!(matches!(
            sink.try_send(NodeEvent::AllInputsClosed),
            Err(DaemonError::ChannelClosed { .. })
        ));
    }

    #[tokio::test]
    async fn a_full_outbox_refuses_rather_than_growing() {
        let (handle, _events) = event_channel();
        let (_actor, sink) = SessionActor::new(session(), handle, token());
        // Nothing is draining: the queue fills at exactly its depth.
        for _ in 0..SESSION_OUTBOX_DEPTH {
            sink.try_send(NodeEvent::AllInputsClosed).unwrap();
        }
        assert!(matches!(
            sink.try_send(NodeEvent::AllInputsClosed),
            Err(DaemonError::Timeout { .. })
        ));
        assert_eq!(sink.capacity(), 0);
    }

    #[tokio::test]
    async fn a_round_trip_conversation_works_in_both_directions_at_once() {
        let (mut node, sink, mut events) = spawn_actor().await;

        // The daemon writes while the node writes.
        sink.try_send(NodeEvent::AllInputsClosed).unwrap();
        node.write_all(
            &encode_request(&NodeRequest::Subscribe { inputs: Vec::new() }, &limits()).unwrap(),
        )
        .await
        .unwrap();

        let received = read_one_event(&mut node).await;
        assert!(matches!(received, NodeEvent::AllInputsClosed));

        match events.recv().await.expect("a request") {
            DaemonEvent::Request { request, .. } => {
                assert!(matches!(*request, NodeRequest::Subscribe { .. }));
            }
            other => panic!("expected a request, got {}", other.kind_name()),
        }
    }

    #[test]
    fn the_helpers_frame_what_the_actor_reads() {
        let limits = limits();
        let request = NodeRequest::EventStreamDropped;
        let bytes = encode_request(&request, &limits).unwrap();
        assert_eq!(NodeRequest::from_bytes(&bytes, &limits).unwrap(), request);

        let event = NodeEvent::AllInputsClosed;
        let bytes = encode_event(&event, &limits).unwrap();
        assert_eq!(NodeEvent::from_bytes(&bytes, &limits).unwrap(), event);
    }

    #[test]
    fn a_checksum_requiring_leg_frames_with_the_checksum() {
        let limits = FrameLimits::network();
        assert!(limits.require_crc());
        let bytes = encode_event(&NodeEvent::AllInputsClosed, &limits).unwrap();
        let view = astrs_wire::decode_frame(&bytes, &limits).unwrap();
        assert!(view.flags().contains(FrameFlags::CRC));
    }
}
