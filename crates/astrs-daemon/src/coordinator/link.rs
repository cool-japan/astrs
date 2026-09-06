//! The daemon → coordinator uplink: dial, register, pump, reconnect
//! (blueprint §4.2, §7.2, §7.3, §12).
//!
//! ```text
//!            ┌──────────────── uplink task ────────────────┐
//!  dial ────►│ TcpStream::connect  →  Hello/Welcome (§7.2) │
//!            │        ↓                                    │
//!            │ DaemonEvent::Register{machine, labels,       │
//!            │        address, running_nodes, catch_up_seq} │
//!            │        ↓                                     │
//!            │  ┌ reader loop ─► DaemonEvent::CoordinatorFrame ──► event loop
//!            │  └ writer loop ◄─ UplinkSink ◄── ReportSink ◄────── event loop
//!            └───────────────┬─────────────────────────────┘
//!                            │ socket ends
//!                            ▼
//!             CoordinatorLost ──► degraded-autonomous (§12)
//!                            ▼
//!               backoff ×2, capped ──► dial again
//! ```
//!
//! # What survives a reconnect
//!
//! Everything the daemon is *doing*. Nodes keep running, rings keep carrying
//! payloads, peer routes keep delivering: the coordinator is the owner of the
//! cluster's plan, not of its execution (§4.2). What the uplink carries across
//! the gap is the two numbers that let the two ends re-converge without
//! stopping anything:
//!
//! - `running_nodes`, so a coordinator that restarted underneath the daemon
//!   knows this daemon is not idle;
//! - `catch_up_seq`, the highest [`astrs_wire::StateEntry`] sequence the
//!   daemon has applied, so the replay starts *after* what it already has
//!   rather than from the beginning of time (§12).
//!
//! Both live in [`UplinkState`], which the event loop writes and the uplink
//! task reads — the one piece of shared mutable state in this module, and
//! atomics rather than a lock precisely because the two sides never need to
//! see each other's writes atomically *together*.
//!
//! # Examples
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use astrs_daemon::coordinator::UplinkConfig;
//! use astrs_daemon::{Daemon, DaemonConfig};
//! use astrs_wire::AuthToken;
//!
//! let mut daemon = Daemon::new(DaemonConfig::from_env())?;
//! daemon.bind().await?;
//! let uplink = UplinkConfig::new("127.0.0.1:7407".parse()?, AuthToken::ZERO);
//! daemon.connect_coordinator(uplink).await?;
//! daemon.run().await;
//! # Ok(()) }
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use astrs_transport::{
    ConnectionCounters, FramedReader, FramedStream, FramedWriter, HandshakeParams, LocalIdentity,
    initiate,
};
use astrs_wire::{
    CoordinatorEvent, DaemonEvent as WireDaemonEvent, DaemonId, DaemonRegistration, FeatureFlags,
    Role,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::coordinator::config::UplinkConfig;
use crate::coordinator::sink::UplinkSink;
use crate::health::ReportSink;
use crate::session::{DaemonEvent, DaemonHandle};

/// The facts the event loop and the uplink task share.
///
/// Every field is a counter or a cursor — never a structure — which is what
/// makes plain atomics sufficient: no reader ever needs two of them to agree
/// with each other at one instant.
#[derive(Debug, Default)]
pub struct UplinkState {
    /// How many times the uplink has registered successfully.
    epoch: AtomicU64,
    /// How many dials failed since the last success.
    failures: AtomicU64,
    /// The highest `StateCatchUp` sequence the daemon has applied (§12).
    catch_up_seq: AtomicU64,
    /// How many nodes this daemon is running, for a resumed registration.
    running_nodes: AtomicU32,
    /// Whether a shutdown was asked for.
    stopping: AtomicBool,
}

impl UplinkState {
    /// A cold state: never connected, nothing applied.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            catch_up_seq: AtomicU64::new(0),
            running_nodes: AtomicU32::new(0),
            stopping: AtomicBool::new(false),
        }
    }

    /// How many times the uplink has registered successfully.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }

    /// How many dials have failed since the last success.
    #[must_use]
    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    /// The highest catch-up sequence applied (§12).
    #[must_use]
    pub fn catch_up_seq(&self) -> u64 {
        self.catch_up_seq.load(Ordering::Relaxed)
    }

    /// Advances the catch-up cursor, never backwards.
    ///
    /// Monotone on purpose: batches can be re-sent (a coordinator that
    /// restarted, an ack lost with a socket), and a cursor that moved back
    /// would ask for a replay the daemon has already applied.
    pub fn observe_catch_up(&self, seq: u64) {
        self.catch_up_seq.fetch_max(seq, Ordering::Relaxed);
    }

    /// Records how many nodes are running, for the next registration.
    pub fn set_running_nodes(&self, count: u32) {
        self.running_nodes.store(count, Ordering::Relaxed);
    }

    /// How many nodes were running at the last sample.
    #[must_use]
    pub fn running_nodes(&self) -> u32 {
        self.running_nodes.load(Ordering::Relaxed)
    }

    /// Whether a shutdown was asked for.
    #[must_use]
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Relaxed)
    }
}

/// A running uplink, and the way to stop it.
#[derive(Debug)]
pub struct UplinkHandle {
    /// The shared cursors.
    state: Arc<UplinkState>,
    /// The outbox the event loop reports through.
    sink: Arc<UplinkSink>,
    /// Flipped to `true` to stop the task.
    shutdown: watch::Sender<bool>,
    /// The task itself, so a caller can await its wind-down.
    task: Option<JoinHandle<()>>,
    /// Where the coordinator is, for diagnostics.
    address: std::net::SocketAddr,
}

impl UplinkHandle {
    /// The shared cursors the event loop writes.
    #[must_use]
    pub fn state(&self) -> &Arc<UplinkState> {
        &self.state
    }

    /// The outbox, so the loop can install it as its report sink.
    #[must_use]
    pub fn sink(&self) -> &Arc<UplinkSink> {
        &self.sink
    }

    /// Where the coordinator is.
    #[must_use]
    pub const fn address(&self) -> std::net::SocketAddr {
        self.address
    }

    /// Whether a connection is up right now.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.sink.is_connected()
    }

    /// How many events are buffered because the link is down (§12).
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.sink.len()
    }

    /// Asks the uplink to stop, without waiting for it.
    pub fn shutdown(&self) {
        self.state.stopping.store(true, Ordering::Relaxed);
        // The only failure is every receiver already gone, which means the
        // task has already returned — nothing left to tell.
        let _ = self.shutdown.send(true);
    }

    /// Asks the uplink to stop and waits for the task to end.
    pub async fn stop(mut self) {
        self.shutdown();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for UplinkHandle {
    fn drop(&mut self) {
        self.state.stopping.store(true, Ordering::Relaxed);
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Starts the uplink task for `daemon`.
///
/// Returns immediately: the first dial happens in the task, so a daemon whose
/// coordinator is not up yet starts anyway and joins the cluster when it is
/// (§12 — the daemon is the autonomous half of the pair).
#[must_use]
pub fn spawn_uplink(daemon: DaemonId, config: UplinkConfig, handle: DaemonHandle) -> UplinkHandle {
    let state = Arc::new(UplinkState::new());
    let sink = Arc::new(UplinkSink::new(config.buffer_capacity()));
    let (shutdown, shutdown_rx) = watch::channel(false);
    let address = config.address();
    let task = tokio::spawn(uplink_loop(
        daemon,
        config,
        handle,
        Arc::clone(&state),
        Arc::clone(&sink),
        shutdown_rx,
    ));
    UplinkHandle {
        state,
        sink,
        shutdown,
        task: Some(task),
        address,
    }
}

/// The reconnect ladder (§12): dial, serve, report the loss, wait, repeat.
async fn uplink_loop(
    daemon: DaemonId,
    config: UplinkConfig,
    handle: DaemonHandle,
    state: Arc<UplinkState>,
    sink: Arc<UplinkSink>,
    mut shutdown: watch::Receiver<bool>,
) {
    while !*shutdown.borrow() && handle.is_open() {
        let outcome = serve_once(&daemon, &config, &handle, &state, &sink, &mut shutdown).await;
        sink.set_connected(false);
        match outcome {
            Ok(reason) => {
                state.failures.store(0, Ordering::Relaxed);
                if *shutdown.borrow() {
                    break;
                }
                tracing::warn!(coordinator = %config.address(), %reason, "coordinator link ended");
                handle.send(DaemonEvent::CoordinatorLost { reason });
            }
            Err(reason) => {
                let failures = state.failures.fetch_add(1, Ordering::Relaxed);
                if failures == 0 {
                    tracing::warn!(
                        coordinator = %config.address(),
                        %reason,
                        "coordinator unreachable; running autonomously"
                    );
                } else {
                    tracing::debug!(
                        coordinator = %config.address(),
                        attempt = failures + 1,
                        %reason,
                        "coordinator dial failed"
                    );
                }
            }
        }
        if *shutdown.borrow() {
            break;
        }
        let attempt = u32::try_from(state.failures()).unwrap_or(u32::MAX);
        let backoff = config.backoff_for(attempt.saturating_sub(1));
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            () = tokio::time::sleep(backoff) => {}
        }
    }
    sink.close();
}

/// One connection, from dial to close.
///
/// `Ok(reason)` means the link was established and then ended; `Err(reason)`
/// means it never came up, which is the case the backoff ladder counts.
async fn serve_once(
    daemon: &DaemonId,
    config: &UplinkConfig,
    handle: &DaemonHandle,
    state: &Arc<UplinkState>,
    sink: &Arc<UplinkSink>,
    shutdown: &mut watch::Receiver<bool>,
) -> std::result::Result<String, String> {
    let stream = tokio::time::timeout(config.dial_timeout(), TcpStream::connect(config.address()))
        .await
        .map_err(|_| "the dial timed out".to_owned())?
        .map_err(|err| err.to_string())?;
    let _ = stream.set_nodelay(true);

    let mut framed = FramedStream::new(stream, config.limits(), ConnectionCounters::shared());
    let params = HandshakeParams::new(
        LocalIdentity::new(Role::Daemon).with_label(daemon.to_string()),
        config.auth().clone(),
    )
    .with_features(FeatureFlags::EMPTY);
    let handshake = initiate(&mut framed, &params, config.handshake_timeout())
        .await
        .map_err(|err| err.to_string())?;

    let session = handshake.session.session_id;
    let registration = registration_for(daemon, config, state, session);
    let (reader, mut writer) = framed.into_halves();
    writer
        .send_message(&WireDaemonEvent::Register(registration))
        .await
        .map_err(|err| err.to_string())?;

    let epoch = state.epoch.fetch_add(1, Ordering::Relaxed) + 1;
    state.failures.store(0, Ordering::Relaxed);
    sink.set_connected(true);
    tracing::info!(
        coordinator = %config.address(),
        %session,
        epoch,
        "registered with the coordinator"
    );
    handle.send(DaemonEvent::CoordinatorConnected { session, epoch });

    let (down_tx, mut down_rx) = watch::channel(false);
    let writer_task = tokio::spawn(write_loop(writer, Arc::clone(sink), down_tx));

    let reason = tokio::select! {
        reason = read_loop(reader, handle.clone(), Arc::clone(state)) => reason,
        changed = down_rx.changed() => {
            if changed.is_err() {
                "the coordinator writer stopped".to_owned()
            } else {
                down_rx.borrow().then(|| "the coordinator write half failed".to_owned())
                    .unwrap_or_else(|| "the coordinator writer stopped".to_owned())
            }
        }
        changed = shutdown.changed() => {
            let _ = changed;
            "the daemon is shutting down".to_owned()
        }
    };

    writer_task.abort();
    let _ = writer_task.await;
    Ok(reason)
}

/// The registration this daemon presents, including its resume cursors (§12).
fn registration_for(
    daemon: &DaemonId,
    config: &UplinkConfig,
    state: &Arc<UplinkState>,
    session: astrs_wire::SessionId,
) -> DaemonRegistration {
    let address = config
        .peer_address()
        .map_or_else(|| config.address().to_string(), str::to_owned);
    let mut registration = DaemonRegistration::new(daemon.clone(), address, session)
        .with_resume_state(state.running_nodes(), state.catch_up_seq());
    if let Some(machine) = config.machine() {
        registration = registration.with_machine(machine.clone());
    }
    for (key, value) in config.labels() {
        registration = registration.with_label(key.clone(), value.clone());
    }
    registration
}

/// Decodes [`CoordinatorEvent`]s and hands each to the event loop.
///
/// Returns the reason the link ended — always a `String`, because there is
/// nothing a caller could do with a typed error here that it does not already
/// do with every ending: reconnect.
async fn read_loop<R>(
    mut reader: FramedReader<R>,
    handle: DaemonHandle,
    state: Arc<UplinkState>,
) -> String
where
    R: AsyncRead + Unpin,
{
    loop {
        match reader.recv_frame().await {
            Ok(Some(frame)) => match frame.decode::<CoordinatorEvent>() {
                Ok(event) => {
                    if let Some(seq) = event.catch_up_high_water() {
                        // Recorded here as well as in the event loop so the
                        // cursor is already correct if the socket dies
                        // between the frame arriving and the loop applying
                        // it — a replay that repeats an applied entry is
                        // wasteful, one that skips an unapplied entry is
                        // wrong, and `observe_catch_up` is monotone either
                        // way.
                        state.observe_catch_up(seq);
                    }
                    if !handle.send(DaemonEvent::CoordinatorFrame {
                        event: Box::new(event),
                    }) {
                        return "the daemon event loop closed".to_owned();
                    }
                }
                Err(err) => return format!("a malformed coordinator event: {err}"),
            },
            Ok(None) => return "the coordinator closed the connection".to_owned(),
            Err(err) => return err.to_string(),
        }
    }
}

/// Drains the outbox onto the socket, one event at a time.
///
/// One at a time, and requeued on failure: a lifecycle fact that was popped
/// and then lost to a broken socket would leave the coordinator's dataflow FSM
/// permanently wrong, and the reconnect that follows is exactly the moment it
/// must still be there to send.
async fn write_loop<W>(
    mut writer: FramedWriter<W>,
    sink: Arc<UplinkSink>,
    down: watch::Sender<bool>,
) where
    W: AsyncWrite + Unpin,
{
    loop {
        let Some(event) = sink.pop() else {
            if !sink.is_open() {
                let _ = down.send(true);
                return;
            }
            sink.wait().await;
            continue;
        };
        if let Err(err) = writer.send_message(&event).await {
            tracing::debug!(%err, "the coordinator write half failed");
            sink.requeue(event);
            let _ = down.send(true);
            return;
        }
        sink.record_forwarded();
    }
}

/// How long a caller polls for the uplink to come up, in tests and in
/// `astrs daemon`'s start-up announcement.
pub const CONNECT_POLL: Duration = Duration::from_millis(10);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::health::ReportSink;
    use astrs_wire::{AuthToken, DaemonStats, MachineName};

    fn state() -> Arc<UplinkState> {
        Arc::new(UplinkState::new())
    }

    fn config() -> UplinkConfig {
        UplinkConfig::new(
            "127.0.0.1:7407".parse().expect("a literal address"),
            AuthToken::from_bytes([5; 32]),
        )
    }

    #[test]
    fn the_catch_up_cursor_only_moves_forward() {
        let state = state();
        assert_eq!(state.catch_up_seq(), 0);
        state.observe_catch_up(41);
        state.observe_catch_up(12);
        assert_eq!(state.catch_up_seq(), 41);
        state.observe_catch_up(43);
        assert_eq!(state.catch_up_seq(), 43);
    }

    #[test]
    fn a_registration_carries_the_resume_cursors_and_the_deployment_facts() {
        let state = state();
        state.observe_catch_up(41);
        state.set_running_nodes(3);
        let config = config()
            .with_machine(MachineName::new("robot-01").unwrap())
            .with_label("zone", "front")
            .with_peer_address("tcp:10.0.0.4:7409");

        let daemon = DaemonId::generate(None);
        let registration = registration_for(
            &daemon,
            &config,
            &state,
            astrs_wire::SessionId::from_u128(7),
        );
        assert_eq!(registration.daemon, daemon);
        assert_eq!(registration.address, "tcp:10.0.0.4:7409");
        assert_eq!(registration.catch_up_seq, 41);
        assert_eq!(registration.running_nodes, 3);
        assert_eq!(registration.label("zone"), Some("front"));
        assert!(registration.is_resuming());
    }

    #[test]
    fn a_registration_without_a_peer_address_falls_back_to_the_control_address() {
        let daemon = DaemonId::generate(None);
        let registration = registration_for(
            &daemon,
            &config(),
            &state(),
            astrs_wire::SessionId::from_u128(1),
        );
        assert_eq!(registration.address, "127.0.0.1:7407");
        assert!(!registration.is_resuming());
    }

    #[tokio::test]
    async fn an_uplink_with_no_coordinator_keeps_retrying_and_buffers_meanwhile() {
        // Port 0 on loopback never has a listener, so every dial fails —
        // which is the point: the daemon must keep running and keep
        // buffering (§12).
        let config = UplinkConfig::new(
            "127.0.0.1:1".parse().unwrap(),
            AuthToken::from_bytes([5; 32]),
        )
        .with_backoff(Duration::from_millis(5), Duration::from_millis(10))
        .with_dial_timeout(Duration::from_millis(200))
        .with_buffer_capacity(4);

        let (handle, _events) = crate::session::event_channel();
        let uplink = spawn_uplink(DaemonId::generate(None), config, handle);
        assert!(!uplink.is_connected());

        uplink.sink().report(WireDaemonEvent::Heartbeat {
            seq: 1,
            sent_at: Default::default(),
            stats: DaemonStats::default(),
        });
        assert_eq!(uplink.buffered(), 1, "nothing is written with no socket");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while uplink.state().failures() == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            uplink.state().failures() > 0,
            "a dial that cannot succeed must be counted, not retried silently forever"
        );
        assert_eq!(uplink.state().epoch(), 0, "nothing ever registered");
        uplink.stop().await;
    }

    #[tokio::test]
    async fn stopping_an_uplink_closes_its_outbox() {
        let (handle, _events) = crate::session::event_channel();
        let uplink = spawn_uplink(
            DaemonId::generate(None),
            config()
                .with_dial_timeout(Duration::from_millis(50))
                .with_backoff(Duration::from_millis(5), Duration::from_millis(5)),
            handle,
        );
        let sink = Arc::clone(uplink.sink());
        uplink.stop().await;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while sink.is_open() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!sink.is_open());
    }
}
