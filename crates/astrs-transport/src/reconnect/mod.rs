//! Reconnect with backoff, epochs and in-flight buffering (blueprint §12).
//!
//! A robot's network is not a datacentre's. A daemon loses its coordinator
//! because a switch rebooted, because the coordinator was upgraded, because the
//! robot drove behind a pillar. None of those is an error the application
//! should handle by exiting; all of them are handled here.
//!
//! [`ReconnectingConnection`] wraps a *factory* — anything that can produce a
//! fresh [`StreamConnection`] — and runs a supervisor task that keeps one alive:
//!
//! ```text
//!            ┌──────────┐  dial ok   ┌────────────┐
//!   start ──▶│ Dialling │───────────▶│ Connected  │
//!            └────┬─────┘            └─────┬──────┘
//!                 │ dial failed            │ closed
//!                 ▼                        │
//!            ┌──────────┐                  │
//!            │ Backoff  │◀─────────────────┘
//!            └────┬─────┘
//!                 │ attempts exhausted
//!                 ▼
//!            ┌──────────┐
//!            │ GaveUp   │
//!            └──────────┘
//! ```
//!
//! # Three things survive a reconnect
//!
//! 1. **The handles.** [`ReconnectingConnection::send_control`] and the
//!    inbound receivers in [`ReconnectChannels`] are stable across
//!    incarnations. An application never re-acquires them.
//! 2. **The in-flight buffer.** Frames sent while the link is down are queued
//!    (bounded, with a typed overflow — see [`ReconnectBuffer`]) and flushed in
//!    order the instant a new connection comes up.
//! 3. **The epoch.** A counter incremented on every successful
//!    re-establishment, carried on every [`ConnectionEvent`], and available
//!    from [`ReconnectingConnection::epoch`].
//!
//! # What does *not* survive: routes
//!
//! Routes are deliberately **not** re-opened automatically. A route is a
//! producer-output→consumer-input path with a generation stamp and a
//! type-checked contract (§6.3); silently re-creating one after the peer
//! restarted would attach to whatever now holds that handle, which is exactly
//! the class of bug the generation stamp exists to prevent. The epoch is the
//! signal: a subscriber that sees [`ConnectionEvent::Up`] with a new epoch
//! knows to re-open the routes it wants, on terms it re-negotiates.
//!
//! # Examples
//!
//! ```no_run
//! use astrs_transport::{ConnectionEvent, ReconnectingConnection, TransportConfig};
//! use astrs_wire::{FrameKind, SessionId};
//!
//! # type Dial = std::pin::Pin<Box<dyn std::future::Future<Output =
//! #     astrs_transport::TransportResult<(astrs_transport::StreamConnection,
//! #     astrs_transport::MuxChannels)>> + Send>>;
//! # async fn example(
//! #     factory: impl Fn(Option<SessionId>) -> Dial + Send + Sync + 'static,
//! # ) -> Result<(), astrs_transport::TransportError> {
//! let (link, mut channels) = ReconnectingConnection::spawn(factory, TransportConfig::new());
//! let mut events = link.subscribe();
//!
//! // Sends are queued while the link is down and flushed when it returns.
//! link.send_control(FrameKind::DaemonEvent, b"heartbeat")?;
//!
//! while let Ok(event) = events.recv().await {
//!     match event {
//!         ConnectionEvent::Up { epoch, .. } => println!("link up, epoch {epoch}"),
//!         ConnectionEvent::Down { reason, .. } => println!("link down: {reason}"),
//!         other => println!("{other:?}"),
//!     }
//! }
//! # let _ = channels.control.recv().await;
//! # Ok(())
//! # }
//! ```

pub mod backoff;
pub mod buffer;

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use astrs_wire::{Frame, FrameFlags, FrameKind, RouteId, SessionId};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::config::TransportConfig;
use crate::conn::{BoxFuture, Connection, StreamConnection};
use crate::error::{CloseReason, TransportError, TransportResult};
use crate::mux::{MuxChannels, RouteStream};
use crate::stats::TransportSnapshot;

pub use backoff::Backoff;
pub use buffer::{OverflowPolicy, ReconnectBuffer};

/// How many events a subscriber may fall behind before it starts missing them.
///
/// Transitions are rare — a link that flaps more than a few times a second has
/// a bigger problem than a full channel — so a small buffer is plenty, and a
/// lagging subscriber learns it lagged rather than stalling the supervisor.
pub const EVENT_CHANNEL_CAPACITY: usize = 64;

/// How often a waiter re-reads the link state rather than waiting on an event.
///
/// Transitions are delivered as events; this poll only exists so that a state
/// change which happened *before* a waiter subscribed cannot strand it. It is
/// far below any timescale a reconnect cares about.
const STATE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);

/// A transition a subscriber can observe.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConnectionEvent {
    /// A connection was established.
    Up {
        /// The new incarnation number.
        epoch: u64,
        /// The session the handshake assigned.
        session: SessionId,
        /// Frames flushed from the in-flight buffer on the way up.
        flushed: usize,
        /// Frames the in-flight buffer has dropped since the last time this
        /// was reported.
        ///
        /// Cumulative-since-last-report rather than "during the outage just
        /// ended": a flush that itself fails part-way re-queues its remainder,
        /// and anything the buffer's policy refuses at *that* point is counted
        /// against the next incarnation. The running total is always
        /// [`ReconnectingConnection::dropped_frames`].
        dropped: usize,
    },
    /// An established connection ended.
    Down {
        /// The incarnation that ended.
        epoch: u64,
        /// Why it ended.
        reason: CloseReason,
    },
    /// A connection was re-established onto the same session (§7.2 `resume`).
    ///
    /// Distinct from [`ConnectionEvent::Up`] because a resumed session means
    /// the peer still holds this endpoint's state: a coordinator that resumed
    /// a daemon's session will catch it up rather than treat it as new
    /// (§12 state catch-up).
    Resumed {
        /// The new incarnation number.
        epoch: u64,
        /// The session that was resumed.
        session: SessionId,
        /// Frames flushed from the in-flight buffer on the way up.
        flushed: usize,
    },
    /// A dial failed and the supervisor is backing off.
    Retrying {
        /// How many consecutive attempts have failed.
        attempt: u32,
        /// How long the supervisor will wait before the next one.
        delay: std::time::Duration,
        /// Why the last attempt failed.
        error: String,
    },
    /// The supervisor reached its attempt ceiling and stopped.
    GaveUp {
        /// How many attempts were made.
        attempts: u32,
        /// Why the last one failed.
        error: String,
    },
    /// The supervisor was shut down by its owner.
    Stopped,
}

impl ConnectionEvent {
    /// The incarnation this event belongs to, where it has one.
    #[must_use]
    pub const fn epoch(&self) -> Option<u64> {
        match self {
            Self::Up { epoch, .. } | Self::Down { epoch, .. } | Self::Resumed { epoch, .. } => {
                Some(*epoch)
            }
            _ => None,
        }
    }

    /// Whether this event means the link is usable.
    #[must_use]
    pub const fn is_up(&self) -> bool {
        matches!(self, Self::Up { .. } | Self::Resumed { .. })
    }

    /// Whether this event means the supervisor will not try again.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::GaveUp { .. } | Self::Stopped)
    }

    /// A stable label for metrics.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Up { .. } => "up",
            Self::Down { .. } => "down",
            Self::Resumed { .. } => "resumed",
            Self::Retrying { .. } => "retrying",
            Self::GaveUp { .. } => "gave_up",
            Self::Stopped => "stopped",
        }
    }
}

impl fmt::Display for ConnectionEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Up { epoch, session, .. } => write!(f, "up (epoch {epoch}, session {session})"),
            Self::Down { epoch, reason } => write!(f, "down (epoch {epoch}): {reason}"),
            Self::Resumed { epoch, session, .. } => {
                write!(f, "resumed (epoch {epoch}, session {session})")
            }
            Self::Retrying {
                attempt,
                delay,
                error,
            } => write!(
                f,
                "retrying (attempt {attempt}, in {}ms): {error}",
                delay.as_millis()
            ),
            Self::GaveUp { attempts, error } => {
                write!(f, "gave up after {attempts} attempts: {error}")
            }
            Self::Stopped => f.write_str("stopped"),
        }
    }
}

/// The link's current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum LinkState {
    /// No connection yet, and the first dial has not finished.
    #[default]
    Dialling,
    /// A connection is live.
    Connected,
    /// Waiting to redial.
    Backoff,
    /// The supervisor has stopped for good.
    Stopped,
}

impl LinkState {
    /// A stable label for metrics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Dialling => "dialling",
            Self::Connected => "connected",
            Self::Backoff => "backoff",
            Self::Stopped => "stopped",
        }
    }

    /// Whether the link can carry a frame right now.
    #[must_use]
    pub const fn is_connected(self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// Anything that can produce a fresh connection.
///
/// Implemented for every `Fn(Option<SessionId>) -> Future<…>`, so a caller
/// normally passes a closure:
///
/// ```ignore
/// let factory = move |resume: Option<SessionId>| {
///     let config = config.clone();
///     let mut params = params.clone();
///     async move {
///         // Ask the peer to continue the session we had, if we had one.
///         params = params.with_resume(resume);
///         tcp::connect(addr, &config, &params).await
///     }
/// };
/// ```
///
/// # The `resume` argument
///
/// The supervisor passes the session id of the incarnation that just ended, or
/// [`None`] for the first dial. A factory that forwards it into
/// [`HandshakeParams::with_resume`](crate::HandshakeParams::with_resume) gives
/// the peer the chance to answer
/// `SessionAssignment::Resumed`, which is what makes
/// [`ConnectionEvent::Resumed`] fire and what a coordinator's state catch-up
/// keys on (§12). A factory that ignores it gets a fresh session every time,
/// which is correct but loses that signal — so the argument is offered rather
/// than imposed.
pub trait ConnectionFactory: Send + Sync + 'static {
    /// Dials once, optionally asking to resume `resume`.
    fn connect(
        &self,
        resume: Option<SessionId>,
    ) -> BoxFuture<'static, TransportResult<(StreamConnection, MuxChannels)>>;
}

impl<F, Fut> ConnectionFactory for F
where
    F: Fn(Option<SessionId>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = TransportResult<(StreamConnection, MuxChannels)>> + Send + 'static,
{
    fn connect(
        &self,
        resume: Option<SessionId>,
    ) -> BoxFuture<'static, TransportResult<(StreamConnection, MuxChannels)>> {
        Box::pin(self(resume))
    }
}

/// The inbound halves that survive every reconnect.
#[derive(Debug)]
#[non_exhaustive]
pub struct ReconnectChannels {
    /// Control-plane frames from whichever connection is live.
    pub control: mpsc::Receiver<Frame>,
    /// Datagrams from whichever connection is live.
    pub datagrams: mpsc::Receiver<Frame>,
    /// Routes the peer opened, tagged with the epoch they arrived on.
    pub accepts: mpsc::UnboundedReceiver<AcceptedRoute>,
}

/// A route the peer opened, and the incarnation it arrived on.
#[derive(Debug)]
#[non_exhaustive]
pub struct AcceptedRoute {
    /// The incarnation this route belongs to.
    ///
    /// A route from an older epoch is stale: the connection it lived on is
    /// gone, and its frames will never arrive.
    pub epoch: u64,
    /// The route itself.
    pub stream: RouteStream,
}

/// The supervisor's shared state.
#[derive(Debug)]
struct ReconnectShared {
    /// The current incarnation, or `None` while the link is down.
    current: Mutex<Option<Arc<StreamConnection>>>,
    /// Frames queued while the link is down.
    buffer: Mutex<ReconnectBuffer>,
    /// The link's state.
    state: Mutex<LinkState>,
    /// The current incarnation number.
    epoch: AtomicU64,
    /// Whether the owner asked the supervisor to stop.
    stopped: AtomicBool,
    /// Transition notifications.
    events: broadcast::Sender<ConnectionEvent>,
    /// The last snapshot taken from a live connection, so statistics survive
    /// the gap between incarnations.
    last_stats: Mutex<TransportSnapshot>,
    /// The session of the most recent incarnation, offered to the factory so
    /// it can ask the peer to resume.
    last_session: Mutex<Option<SessionId>>,
}

/// A connection that re-establishes itself.
#[derive(Debug)]
pub struct ReconnectingConnection {
    /// Shared state.
    shared: Arc<ReconnectShared>,
    /// The supervisor task, taken by [`ReconnectingConnection::shutdown`] so
    /// that it can be awaited without moving out of a `Drop` type.
    task: Mutex<Option<JoinHandle<()>>>,
}

impl ReconnectingConnection {
    /// Starts a supervisor over `factory`.
    ///
    /// The first dial happens on the supervisor task, so this returns
    /// immediately and the link comes up asynchronously. A caller that must
    /// wait uses [`ReconnectingConnection::wait_until_connected`].
    #[must_use]
    pub fn spawn(
        factory: impl ConnectionFactory,
        config: TransportConfig,
    ) -> (Self, ReconnectChannels) {
        Self::spawn_with_policy(factory, config, OverflowPolicy::default())
    }

    /// Starts a supervisor with a chosen in-flight overflow policy.
    #[must_use]
    pub fn spawn_with_policy(
        factory: impl ConnectionFactory,
        config: TransportConfig,
        policy: OverflowPolicy,
    ) -> (Self, ReconnectChannels) {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (control_tx, control_rx) = mpsc::channel(config.mux.control_queue_depth.max(1));
        let (datagram_tx, datagram_rx) = mpsc::channel(config.mux.datagram_queue_depth.max(1));
        let (accept_tx, accept_rx) = mpsc::unbounded_channel();

        let shared = Arc::new(ReconnectShared {
            current: Mutex::new(None),
            buffer: Mutex::new(ReconnectBuffer::from_config(&config.backoff, policy)),
            state: Mutex::new(LinkState::Dialling),
            epoch: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            events,
            last_stats: Mutex::new(TransportSnapshot::new(Default::default())),
            last_session: Mutex::new(None),
        });

        let task = tokio::spawn(supervise(
            Arc::clone(&shared),
            factory,
            config,
            Forwarders {
                control: control_tx,
                datagrams: datagram_tx,
                accepts: accept_tx,
            },
        ));

        (
            Self {
                shared,
                task: Mutex::new(Some(task)),
            },
            ReconnectChannels {
                control: control_rx,
                datagrams: datagram_rx,
                accepts: accept_rx,
            },
        )
    }

    /// Subscribes to transition events.
    ///
    /// Every subscriber sees every transition from the moment it subscribes;
    /// one that falls more than [`EVENT_CHANNEL_CAPACITY`] behind receives a
    /// lag error rather than blocking the supervisor.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ConnectionEvent> {
        self.shared.events.subscribe()
    }

    /// The current incarnation number.
    ///
    /// Zero until the first connection comes up, then incremented on every
    /// successful re-establishment.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.shared.epoch.load(Ordering::Acquire)
    }

    /// The link's state.
    #[must_use]
    pub fn state(&self) -> LinkState {
        *lock(&self.shared.state)
    }

    /// Whether a connection is live right now.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.state().is_connected()
    }

    /// The session of the most recent incarnation, if there has been one.
    ///
    /// This is what the supervisor offers the factory as its `resume`
    /// argument; a caller that builds its own dial loop can use it for the
    /// same purpose.
    #[must_use]
    pub fn last_session(&self) -> Option<SessionId> {
        *lock(&self.shared.last_session)
    }

    /// The live connection, if there is one.
    ///
    /// Handed out as an [`Arc`] so a caller can open routes on it without
    /// holding a lock; the handle becomes inert (every send fails) once that
    /// incarnation closes.
    #[must_use]
    pub fn current(&self) -> Option<Arc<StreamConnection>> {
        lock(&self.shared.current).clone()
    }

    /// Sends a control frame, queueing it if the link is down.
    ///
    /// # Errors
    ///
    /// - [`TransportError::ReconnectBufferOverflow`] if the link is down and
    ///   the buffer is full under [`OverflowPolicy::Reject`].
    /// - [`TransportError::NotConnected`] once the supervisor has stopped.
    pub fn send_control(&self, kind: FrameKind, payload: &[u8]) -> TransportResult<()> {
        if self.shared.stopped.load(Ordering::Acquire) {
            return Err(TransportError::NotConnected);
        }
        if let Some(connection) = self.current() {
            match connection.open_control().try_send(kind, payload) {
                Ok(()) => return Ok(()),
                // The link died between the check and the send, or its queue
                // is full. Either way the frame belongs in the buffer.
                Err(TransportError::SendQueueFull { .. } | TransportError::Closed { .. }) => {}
                Err(err) => return Err(err),
            }
        }
        let frame =
            Frame::new(kind, FrameFlags::EMPTY, payload.to_vec()).map_err(TransportError::Wire)?;
        lock(&self.shared.buffer).push(frame)
    }

    /// Queues a frame for the next connection without trying the current one.
    ///
    /// # Errors
    ///
    /// As [`ReconnectingConnection::send_control`].
    pub fn queue_control(&self, frame: Frame) -> TransportResult<()> {
        lock(&self.shared.buffer).push(frame)
    }

    /// How many frames are waiting for the link to come back.
    #[must_use]
    pub fn queued_frames(&self) -> usize {
        lock(&self.shared.buffer).len()
    }

    /// How many frames the in-flight buffer has dropped, ever.
    #[must_use]
    pub fn dropped_frames(&self) -> usize {
        lock(&self.shared.buffer).dropped()
    }

    /// Opens a route on the live connection.
    ///
    /// # Errors
    ///
    /// [`TransportError::NotConnected`] while the link is down — routes are
    /// never queued, because a route opened against a connection that does not
    /// exist has no peer to accept it (see the module documentation).
    pub fn open_route(&self, descriptor: &[u8]) -> TransportResult<RouteStream> {
        let connection = self.current().ok_or(TransportError::NotConnected)?;
        connection.open_route(descriptor)
    }

    /// Opens a route with a caller-chosen handle on the live connection.
    ///
    /// # Errors
    ///
    /// As [`ReconnectingConnection::open_route`].
    pub fn open_route_stream(
        &self,
        route: RouteId,
        descriptor: &[u8],
    ) -> TransportResult<RouteStream> {
        let connection = self.current().ok_or(TransportError::NotConnected)?;
        connection.open_route_stream(route, descriptor)
    }

    /// The live connection's counters, or the last incarnation's if the link
    /// is down.
    ///
    /// Statistics that reset to zero every time a robot drives behind a pillar
    /// would be useless, so the last reading is kept.
    #[must_use]
    pub fn stats(&self) -> TransportSnapshot {
        match self.current() {
            Some(connection) => {
                let snapshot = connection.stats();
                *lock(&self.shared.last_stats) = snapshot.clone();
                snapshot
            }
            None => lock(&self.shared.last_stats).clone(),
        }
    }

    /// Waits until a connection is live, or the supervisor gives up.
    ///
    /// # Errors
    ///
    /// [`TransportError::NotConnected`] if the supervisor stopped or gave up
    /// before a connection came up.
    pub async fn wait_until_connected(&self) -> TransportResult<u64> {
        let mut events = self.subscribe();
        loop {
            // Polled as well as awaited, because a supervisor that stopped
            // *before* this call subscribed has already sent its terminal
            // event: waiting on the channel alone would wait forever.
            if self.is_connected() {
                return Ok(self.epoch());
            }
            if matches!(self.state(), LinkState::Stopped) {
                return Err(TransportError::NotConnected);
            }

            tokio::select! {
                received = events.recv() => match received {
                    Ok(event) if event.is_up() => {
                        return Ok(event.epoch().unwrap_or_else(|| self.epoch()));
                    }
                    Ok(event) if event.is_terminal() => {
                        return Err(TransportError::NotConnected);
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(TransportError::NotConnected);
                    }
                },
                () = tokio::time::sleep(STATE_POLL_INTERVAL) => {}
            }
        }
    }

    /// Stops the supervisor and closes the live connection.
    ///
    /// Waits for the supervisor task to wind down, so that a caller tearing a
    /// process down knows no socket is still being polled.
    pub async fn shutdown(&self) {
        self.shared.stopped.store(true, Ordering::Release);
        if let Some(connection) = self.current() {
            let _ = connection
                .close(CloseReason::local("reconnect shut down"))
                .await;
        }
        *lock(&self.shared.state) = LinkState::Stopped;
        let _ = self.shared.events.send(ConnectionEvent::Stopped);

        let task = lock(&self.task).take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for ReconnectingConnection {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        if let Some(task) = lock(&self.task).take() {
            task.abort();
        }
    }
}

/// The stable inbound endpoints the supervisor forwards into.
struct Forwarders {
    /// Control-plane frames.
    control: mpsc::Sender<Frame>,
    /// Datagrams.
    datagrams: mpsc::Sender<Frame>,
    /// Routes the peer opened.
    accepts: mpsc::UnboundedSender<AcceptedRoute>,
}

/// The supervisor loop.
async fn supervise(
    shared: Arc<ReconnectShared>,
    factory: impl ConnectionFactory,
    config: TransportConfig,
    forwarders: Forwarders,
) {
    let mut backoff = Backoff::new(config.backoff);

    loop {
        if shared.stopped.load(Ordering::Acquire) {
            break;
        }

        *lock(&shared.state) = LinkState::Dialling;
        let resume = *lock(&shared.last_session);
        let established = match factory.connect(resume).await {
            Ok(established) => established,
            Err(err) => {
                *lock(&shared.state) = LinkState::Backoff;
                let Some(delay) = backoff.next_delay() else {
                    *lock(&shared.state) = LinkState::Stopped;
                    let _ = shared.events.send(ConnectionEvent::GaveUp {
                        attempts: backoff.attempts(),
                        error: err.to_string(),
                    });
                    lock(&shared.buffer).clear();
                    break;
                };
                let _ = shared.events.send(ConnectionEvent::Retrying {
                    attempt: backoff.attempts(),
                    delay,
                    error: err.to_string(),
                });
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                continue;
            }
        };

        backoff.reset();
        let (connection, channels) = established;
        let resumed = connection.session().resumed;
        let session = connection.session().session_id;
        let epoch = shared.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        *lock(&shared.last_session) = Some(session);
        let connection = Arc::new(connection.with_epoch(epoch));

        // Flush whatever accumulated while the link was down, in order, before
        // anything new can be queued behind it.
        let (flushed, dropped) = flush_buffer(&shared, &connection).await;

        *lock(&shared.current) = Some(Arc::clone(&connection));
        *lock(&shared.state) = LinkState::Connected;
        let _ = shared.events.send(if resumed {
            ConnectionEvent::Resumed {
                epoch,
                session,
                flushed,
            }
        } else {
            ConnectionEvent::Up {
                epoch,
                session,
                flushed,
                dropped,
            }
        });

        pump(&shared, &connection, channels, &forwarders, epoch).await;

        // The incarnation ended. Snapshot it before it goes, so statistics
        // survive the gap.
        *lock(&shared.last_stats) = connection.stats();
        let reason = connection.close_reason().unwrap_or(CloseReason::Eof);
        *lock(&shared.current) = None;
        drop(connection);
        let _ = shared.events.send(ConnectionEvent::Down { epoch, reason });

        if shared.stopped.load(Ordering::Acquire) {
            break;
        }
        *lock(&shared.state) = LinkState::Backoff;
        let Some(delay) = backoff.next_delay() else {
            *lock(&shared.state) = LinkState::Stopped;
            let _ = shared.events.send(ConnectionEvent::GaveUp {
                attempts: backoff.attempts(),
                error: "connection closed and the attempt ceiling was reached".into(),
            });
            lock(&shared.buffer).clear();
            break;
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }

    *lock(&shared.state) = LinkState::Stopped;
    let _ = shared.events.send(ConnectionEvent::Stopped);
}

/// Empties the in-flight buffer into a freshly established connection.
async fn flush_buffer(shared: &ReconnectShared, connection: &StreamConnection) -> (usize, usize) {
    let (pending, dropped) = {
        let mut buffer = lock(&shared.buffer);
        (buffer.drain(), buffer.reset_dropped())
    };
    let control = connection.open_control();
    let mut flushed = 0usize;
    let mut pending = pending.into_iter();
    for frame in pending.by_ref() {
        if control.send(frame.kind(), frame.payload()).await.is_err() {
            // The link died mid-flush. Put this frame and everything behind it
            // back, so the next incarnation gets them instead of them
            // vanishing between the drain and the failure. Frames the buffer's
            // policy refuses at that point are counted as dropped by `push`
            // itself, so the loss shows up in
            // [`ReconnectingConnection::dropped_frames`] rather than
            // disappearing.
            let mut buffer = lock(&shared.buffer);
            let _ = buffer.push(frame);
            for remaining in pending {
                let _ = buffer.push(remaining);
            }
            break;
        }
        flushed += 1;
    }
    (flushed, dropped)
}

/// Forwards one incarnation's inbound traffic until it closes.
async fn pump(
    shared: &ReconnectShared,
    connection: &Arc<StreamConnection>,
    mut channels: MuxChannels,
    forwarders: &Forwarders,
    epoch: u64,
) {
    loop {
        tokio::select! {
            frame = channels.control.recv() => match frame {
                Some(frame) => {
                    if forwarders.control.send(frame).await.is_err() {
                        shared.stopped.store(true, Ordering::Release);
                        return;
                    }
                }
                // The mux closed its control sink: this incarnation is over.
                None => return,
            },
            frame = channels.datagrams.recv() => {
                if let Some(frame) = frame {
                    // Datagrams are best effort in both directions: a receiver
                    // that is not keeping up loses them rather than stalling
                    // the control plane behind them.
                    let _ = forwarders.datagrams.try_send(frame);
                }
            }
            stream = channels.accepts.accept() => {
                if let Some(stream) = stream
                    && forwarders.accepts.send(AcceptedRoute { epoch, stream }).is_err()
                {
                    shared.stopped.store(true, Ordering::Release);
                    return;
                }
            }
            () = wait_for_close(connection) => return,
        }
    }
}

/// Resolves once `connection` has closed.
///
/// Polls rather than waiting on a notification because [`Connection`] exposes
/// closure as a predicate, not as a future — a deliberate simplification of the
/// trait, since this is its only caller and a 25 ms poll is far below any
/// timescale a reconnect cares about.
async fn wait_for_close(connection: &StreamConnection) {
    while !connection.is_closed() {
        tokio::time::sleep(STATE_POLL_INTERVAL).await;
    }
}

/// Takes a lock, recovering from poisoning rather than propagating a panic.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::backend::tcp::{TcpListener, connect};
    use crate::config::{BackoffConfig, LocalIdentity};
    use crate::handshake::{HandshakeParams, acceptor_from_config};
    use astrs_wire::{AuthToken, Role, RoleSet, SessionAssignment, SessionId};
    use std::net::SocketAddr;
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    fn token() -> AuthToken {
        AuthToken::from_bytes([0x2c; 32])
    }

    fn fast_config() -> TransportConfig {
        TransportConfig::new().with_backoff(
            BackoffConfig::new()
                .with_base(Duration::from_millis(5))
                .with_cap(Duration::from_millis(20))
                .with_jitter_percent(0),
        )
    }

    /// A factory dialling a fixed TCP address, forwarding the resume request.
    fn tcp_factory(addr: SocketAddr, config: TransportConfig) -> impl ConnectionFactory {
        move |resume: Option<SessionId>| {
            let config = config.clone();
            async move {
                let params = HandshakeParams::from_config(
                    &config,
                    LocalIdentity::new(Role::Peer).with_label("reconnector"),
                    token(),
                    true,
                )
                .with_resume(resume);
                connect(addr, &config, &params).await
            }
        }
    }

    /// Serves `rounds` connections on `listener`, then stops.
    fn serve(
        listener: TcpListener,
        config: TransportConfig,
        rounds: usize,
    ) -> tokio::task::JoinHandle<Vec<StreamConnection>> {
        tokio::spawn(async move {
            let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);
            let mut served = Vec::new();
            for index in 0..rounds {
                match listener
                    .accept(
                        &acceptor,
                        SessionAssignment::Fresh(SessionId::from_u128(index as u128 + 1)),
                    )
                    .await
                {
                    Ok((connection, _channels)) => served.push(connection),
                    Err(_) => break,
                }
            }
            served
        })
    }

    #[tokio::test]
    async fn the_link_comes_up_and_reports_its_epoch() {
        let config = fast_config();
        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap(), config.clone())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve(listener, config.clone(), 1);

        let (link, _channels) =
            ReconnectingConnection::spawn(tcp_factory(addr, config.clone()), config);
        let epoch = tokio::time::timeout(Duration::from_secs(10), link.wait_until_connected())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(epoch, 1);
        assert_eq!(link.epoch(), 1);
        assert!(link.is_connected());
        assert_eq!(link.state(), LinkState::Connected);
        assert!(link.current().is_some());

        let served = server.await.unwrap();
        assert_eq!(served.len(), 1);
        link.shutdown().await;
    }

    #[tokio::test]
    async fn a_server_restart_bumps_the_epoch() {
        let config = fast_config();
        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap(), config.clone())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        // Serve three incarnations, dropping each after a moment so the client
        // has to redial.
        let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);
        let server = tokio::spawn(async move {
            for index in 0..3u128 {
                let Ok((connection, _channels)) = listener
                    .accept(
                        &acceptor,
                        SessionAssignment::Fresh(SessionId::from_u128(index + 1)),
                    )
                    .await
                else {
                    break;
                };
                tokio::time::sleep(Duration::from_millis(30)).await;
                drop(connection);
            }
            listener
        });

        let (link, _channels) =
            ReconnectingConnection::spawn(tcp_factory(addr, config.clone()), config);
        let mut events = link.subscribe();

        let mut ups = 0;
        let mut downs = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while ups < 3 && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Ok(ConnectionEvent::Up { epoch, .. })) => {
                    ups += 1;
                    assert_eq!(epoch, ups as u64, "epochs must increment monotonically");
                }
                Ok(Ok(ConnectionEvent::Down { .. })) => downs += 1,
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        assert_eq!(ups, 3, "the client must resume after each restart");
        assert!(downs >= 2, "each ended incarnation must report Down");
        assert!(link.epoch() >= 3);

        let _ = server.await;
        link.shutdown().await;
    }

    #[tokio::test]
    async fn frames_sent_while_down_are_flushed_on_the_way_up() {
        let config = fast_config();
        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap(), config.clone())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let (link, _channels) = ReconnectingConnection::spawn_with_policy(
            tcp_factory(addr, config.clone()),
            config.clone(),
            OverflowPolicy::Reject,
        );

        // Nothing is listening yet: sends queue.
        for index in 0..4u8 {
            link.send_control(FrameKind::DaemonEvent, &[index]).unwrap();
        }
        assert_eq!(link.queued_frames(), 4);

        // Now start a listener at the same address and let the link find it.
        let listener = TcpListener::bind(addr, config.clone()).await.unwrap();
        let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);
        let server = tokio::spawn(async move {
            let (connection, mut channels) = listener
                .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
                .await
                .unwrap();
            let mut seen = Vec::new();
            for _ in 0..4 {
                let frame = tokio::time::timeout(Duration::from_secs(10), channels.control.recv())
                    .await
                    .unwrap()
                    .expect("a buffered frame");
                seen.push(frame.payload()[0]);
            }
            (connection, seen)
        });

        tokio::time::timeout(Duration::from_secs(20), link.wait_until_connected())
            .await
            .unwrap()
            .unwrap();
        let (_connection, seen) = server.await.unwrap();
        assert_eq!(seen, vec![0, 1, 2, 3], "order must be preserved");
        assert_eq!(link.queued_frames(), 0);
        link.shutdown().await;
    }

    #[tokio::test]
    async fn a_full_buffer_is_a_typed_overflow() {
        let config = fast_config().with_backoff(
            BackoffConfig::new()
                .with_base(Duration::from_millis(5))
                .with_cap(Duration::from_millis(20))
                .with_jitter_percent(0)
                .with_buffer_frames(2),
        );
        // A port with nothing on it, so the link never comes up.
        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap(), config.clone())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let (link, _channels) = ReconnectingConnection::spawn_with_policy(
            tcp_factory(addr, config.clone()),
            config,
            OverflowPolicy::Reject,
        );

        link.send_control(FrameKind::DaemonEvent, b"a").unwrap();
        link.send_control(FrameKind::DaemonEvent, b"b").unwrap();
        let err = link.send_control(FrameKind::DaemonEvent, b"c").unwrap_err();
        match err {
            TransportError::ReconnectBufferOverflow { capacity, .. } => assert_eq!(capacity, 2),
            other => panic!("expected an overflow, got {other:?}"),
        }
        assert_eq!(link.dropped_frames(), 1);
        link.shutdown().await;
    }

    #[tokio::test]
    async fn a_lossy_buffer_keeps_the_freshest_frames() {
        let config = fast_config().with_backoff(
            BackoffConfig::new()
                .with_base(Duration::from_millis(5))
                .with_jitter_percent(0)
                .with_buffer_frames(2),
        );
        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap(), config.clone())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let (link, _channels) = ReconnectingConnection::spawn_with_policy(
            tcp_factory(addr, config.clone()),
            config,
            OverflowPolicy::DropOldest,
        );
        for index in 0..8u8 {
            link.send_control(FrameKind::DaemonEvent, &[index]).unwrap();
        }
        assert_eq!(link.queued_frames(), 2);
        assert_eq!(link.dropped_frames(), 6);
        link.shutdown().await;
    }

    #[tokio::test]
    async fn the_supervisor_gives_up_at_its_ceiling() {
        let config = TransportConfig::new().with_backoff(
            BackoffConfig::new()
                .with_base(Duration::from_millis(1))
                .with_cap(Duration::from_millis(2))
                .with_jitter_percent(0)
                .with_max_attempts(Some(2)),
        );
        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap(), config.clone())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let (link, _channels) =
            ReconnectingConnection::spawn(tcp_factory(addr, config.clone()), config);
        let mut events = link.subscribe();

        let mut gave_up = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Ok(event)) => {
                    if matches!(event, ConnectionEvent::GaveUp { .. }) {
                        assert!(event.is_terminal());
                        gave_up = true;
                        break;
                    }
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }
        assert!(gave_up, "the supervisor must stop at its ceiling");

        // …and a caller waiting for a connection is told, rather than hanging.
        assert!(link.wait_until_connected().await.is_err());
        link.shutdown().await;
    }

    #[tokio::test]
    async fn a_failing_factory_reports_every_retry() {
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&attempts);
        let factory = move |_resume: Option<SessionId>| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::Relaxed);
                Err(TransportError::Io(
                    std::io::ErrorKind::ConnectionRefused.into(),
                ))
            }
        };

        let config = TransportConfig::new().with_backoff(
            BackoffConfig::new()
                .with_base(Duration::from_millis(1))
                .with_cap(Duration::from_millis(1))
                .with_jitter_percent(0)
                .with_max_attempts(Some(3)),
        );
        let (link, _channels) = ReconnectingConnection::spawn(factory, config);
        let mut events = link.subscribe();

        let mut retries = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Ok(ConnectionEvent::Retrying { error, .. })) => {
                    assert!(!error.is_empty());
                    retries += 1;
                }
                Ok(Ok(ConnectionEvent::GaveUp { attempts, .. })) => {
                    assert_eq!(attempts, 3);
                    break;
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        assert!(retries >= 1);
        assert!(attempts.load(Ordering::Relaxed) >= 3);
        link.shutdown().await;
    }

    #[tokio::test]
    async fn stats_survive_the_gap_between_incarnations() {
        let config = fast_config();
        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap(), config.clone())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);
        let server = tokio::spawn(async move {
            let (connection, mut channels) = listener
                .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
                .await
                .unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(10), channels.control.recv()).await;
            (connection, listener)
        });

        let (link, _channels) =
            ReconnectingConnection::spawn(tcp_factory(addr, config.clone()), config);
        tokio::time::timeout(Duration::from_secs(10), link.wait_until_connected())
            .await
            .unwrap()
            .unwrap();
        link.send_control(FrameKind::DaemonEvent, b"hello").unwrap();

        // Snapshot while connected…
        let mut sent = 0;
        for _ in 0..200 {
            sent = link.stats().connection.frames_sent;
            if sent > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(sent > 0);

        let (connection, _listener) = server.await.unwrap();
        drop(connection);

        // …and the reading survives the connection going away.
        for _ in 0..200 {
            if !link.is_connected() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(link.stats().connection.frames_sent >= sent);
        link.shutdown().await;
    }

    #[tokio::test]
    async fn routes_are_never_opened_while_the_link_is_down() {
        let config = fast_config();
        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap(), config.clone())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let (link, _channels) =
            ReconnectingConnection::spawn(tcp_factory(addr, config.clone()), config);
        assert!(matches!(
            link.open_route(b"camera").unwrap_err(),
            TransportError::NotConnected
        ));
        assert!(matches!(
            link.open_route_stream(RouteId::FIRST, b"camera")
                .unwrap_err(),
            TransportError::NotConnected
        ));
        link.shutdown().await;
    }

    #[tokio::test]
    async fn a_peer_opened_route_arrives_tagged_with_its_epoch() {
        let config = fast_config();
        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap(), config.clone())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);
        let server = tokio::spawn(async move {
            let (connection, _channels) = listener
                .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
                .await
                .unwrap();
            let route = connection.open_route(b"from-the-server").unwrap();
            (connection, route, listener)
        });

        let (link, mut channels) =
            ReconnectingConnection::spawn(tcp_factory(addr, config.clone()), config);
        tokio::time::timeout(Duration::from_secs(10), link.wait_until_connected())
            .await
            .unwrap()
            .unwrap();

        let accepted = tokio::time::timeout(Duration::from_secs(10), channels.accepts.recv())
            .await
            .unwrap()
            .expect("an inbound route");
        assert_eq!(accepted.epoch, 1);
        assert_eq!(accepted.stream.descriptor(), b"from-the-server");

        let (_connection, _route, _listener) = server.await.unwrap();
        link.shutdown().await;
    }

    #[tokio::test]
    async fn a_resumed_session_is_reported_as_such() {
        // The factory forwards the supervisor's `resume` argument, and the
        // server honours it, so the second incarnation reports `Resumed`
        // rather than `Up` — which is what a coordinator's state catch-up
        // (§12) keys on.
        let config = fast_config();
        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap(), config.clone())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);
        let session = SessionId::from_u128(0xfeed);

        let server = tokio::spawn(async move {
            for _ in 0..2u8 {
                let Ok((connection, _channels)) = listener
                    .accept_with(&acceptor, |hello| match hello.resume {
                        Some(requested) if requested == session => {
                            SessionAssignment::Resumed(requested)
                        }
                        _ => SessionAssignment::Fresh(session),
                    })
                    .await
                else {
                    break;
                };
                tokio::time::sleep(Duration::from_millis(30)).await;
                drop(connection);
            }
            listener
        });

        let (link, _channels) =
            ReconnectingConnection::spawn(tcp_factory(addr, config.clone()), config);
        let mut events = link.subscribe();

        let mut saw_up = false;
        let mut saw_resumed = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !saw_resumed && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Ok(ConnectionEvent::Up {
                    epoch, session: id, ..
                })) => {
                    assert_eq!(epoch, 1, "only the first dial can be fresh");
                    assert_eq!(id, session);
                    saw_up = true;
                }
                Ok(Ok(ConnectionEvent::Resumed {
                    epoch, session: id, ..
                })) => {
                    assert_eq!(epoch, 2);
                    assert_eq!(id, session);
                    saw_resumed = true;
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        assert!(saw_up, "the first incarnation must report Up");
        assert!(saw_resumed, "the second must report Resumed");
        assert_eq!(link.last_session(), Some(session));

        let _ = server.await;
        link.shutdown().await;
    }

    #[tokio::test]
    async fn the_first_dial_asks_to_resume_nothing() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Option<SessionId>>::new()));
        let recorder = Arc::clone(&seen);
        let factory = move |resume: Option<SessionId>| {
            let recorder = Arc::clone(&recorder);
            async move {
                recorder
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(resume);
                Err(TransportError::Io(
                    std::io::ErrorKind::ConnectionRefused.into(),
                ))
            }
        };

        let config = TransportConfig::new().with_backoff(
            BackoffConfig::new()
                .with_base(Duration::from_millis(1))
                .with_cap(Duration::from_millis(1))
                .with_jitter_percent(0)
                .with_max_attempts(Some(2)),
        );
        let (link, _channels) = ReconnectingConnection::spawn(factory, config);
        let _ = link.wait_until_connected().await;

        let recorded: Vec<Option<SessionId>> = seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert!(!recorded.is_empty());
        assert!(
            recorded.iter().all(Option::is_none),
            "with no incarnation behind it, every dial must ask to resume nothing"
        );
        link.shutdown().await;
    }

    #[tokio::test]
    async fn the_epoch_reaches_the_statistics_snapshot() {
        let config = fast_config();
        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap(), config.clone())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve(listener, config.clone(), 1);

        let (link, _channels) =
            ReconnectingConnection::spawn(tcp_factory(addr, config.clone()), config);
        tokio::time::timeout(Duration::from_secs(10), link.wait_until_connected())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(link.epoch(), 1);
        assert_eq!(
            link.stats().connection.epoch,
            1,
            "a metrics consumer reading the snapshot must see the incarnation"
        );

        let _ = server.await;
        link.shutdown().await;
    }

    #[tokio::test]
    async fn shutting_down_stops_the_supervisor() {
        let config = fast_config();
        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap(), config.clone())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let (link, _channels) =
            ReconnectingConnection::spawn(tcp_factory(addr, config.clone()), config);
        link.shutdown().await;
        // A stopped supervisor refuses new work rather than queueing it
        // forever.
    }

    #[test]
    fn events_describe_themselves() {
        let events = [
            ConnectionEvent::Up {
                epoch: 1,
                session: SessionId::from_u128(1),
                flushed: 2,
                dropped: 0,
            },
            ConnectionEvent::Down {
                epoch: 1,
                reason: CloseReason::Eof,
            },
            ConnectionEvent::Resumed {
                epoch: 2,
                session: SessionId::from_u128(1),
                flushed: 0,
            },
            ConnectionEvent::Retrying {
                attempt: 3,
                delay: Duration::from_millis(250),
                error: "refused".into(),
            },
            ConnectionEvent::GaveUp {
                attempts: 5,
                error: "refused".into(),
            },
            ConnectionEvent::Stopped,
        ];
        for event in &events {
            assert!(!event.to_string().is_empty());
            assert!(!event.label().is_empty());
        }
        assert_eq!(events[0].epoch(), Some(1));
        assert_eq!(events[3].epoch(), None);
        assert!(events[0].is_up());
        assert!(events[2].is_up());
        assert!(!events[1].is_up());
        assert!(events[4].is_terminal());
        assert!(events[5].is_terminal());
        assert!(!events[3].is_terminal());
    }

    #[test]
    fn link_states_describe_themselves() {
        assert_eq!(LinkState::default(), LinkState::Dialling);
        assert!(LinkState::Connected.is_connected());
        assert!(!LinkState::Backoff.is_connected());
        for state in [
            LinkState::Dialling,
            LinkState::Connected,
            LinkState::Backoff,
            LinkState::Stopped,
        ] {
            assert!(!state.label().is_empty());
        }
    }
}
