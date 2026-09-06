//! The coordinator's network front door (blueprint §4.2, §7.2, §7.3, §12,
//! §16).
//!
//! Every other module in this crate is reachable from a test without a real
//! socket: [`crate::handlers`] answers a [`astrs_wire::ControlRequest`]
//! against a bare [`Coordinator`], and [`crate::session`] runs a connection
//! actor over an already-`Framed` pair of halves. This module is the thin
//! layer that turns an actual TCP accept loop into those calls:
//!
//! 1. **Bind** — [`CoordinatorServer::bind`] listens on
//!    [`crate::config::CoordinatorConfig::bind_addr`].
//! 2. **Admit** — every accepted socket is checked against
//!    [`crate::config::CoordinatorConfig::connection_limit`] and
//!    [`crate::config::CoordinatorConfig::per_ip_connection_limit`] before it
//!    is allowed to spend any CPU on a handshake (dora hardcoded a
//!    256-connection cap; this one is configured and actually enforced).
//! 3. **Greet** — [`accept_scoped`] runs the `Hello`→`Welcome`/`Refused`
//!    exchange (§7.2) against an [`astrs_wire::Acceptor`] scoped to
//!    [`astrs_wire::RoleSet::COORDINATOR`] — the only two roles a
//!    coordinator's port ever serves — and additionally decides which
//!    [`astrs_wire::RequestScope`] the connection is granted (§16, §22; see
//!    [`crate::auth`] for how a token's scope is derived and classified).
//! 4. **Dispatch** — the negotiated [`astrs_wire::Role`] decides whether the
//!    connection becomes a [`crate::session::cli`] or [`crate::session::daemon`]
//!    actor; a CLI session carries its granted scope with it, enforced per
//!    request by [`crate::handlers::dispatch`].
//! 5. **Watch** — a background loop sends every connected daemon a fresh
//!    [`astrs_wire::CoordinatorEvent::Heartbeat`] every
//!    [`crate::config::CoordinatorConfig::heartbeat_interval`] and sweeps for
//!    ones that have gone silent, cascading a daemon declared lost through
//!    exactly the same path a closed socket would (§12).
//!
//! # Example
//!
//! ```no_run
//! use astrs_coordinator::{Coordinator, CoordinatorConfig, CoordinatorServer};
//! use astrs_wire::AuthToken;
//!
//! # async fn example() -> Result<(), astrs_coordinator::CoordinatorError> {
//! let config = CoordinatorConfig::new(AuthToken::from_bytes([7; 32])).with_port(0);
//! let coordinator = Coordinator::open_in_memory(config)?;
//! let server = CoordinatorServer::bind(coordinator).await?;
//! println!("listening on {}", server.local_addr()?);
//! let handle = server.handle();
//! # handle.shutdown();
//! server.serve().await?;
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use astrs_transport::{ConnectionCounters, FramedStream, TransportError, pre_handshake_limits};
use astrs_wire::{
    Acceptor, AuthToken, ControlReply, ControlRequest, CoordinatorEvent, FrameKind,
    HandshakeOutcome, Hello, NegotiatedSession, RefusalReason, Refused, RequestScope, Role,
    RoleSet, SessionAssignment, SessionId, WireMessage, negotiate,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::config::CoordinatorConfig;
use crate::coordinator::Coordinator;
use crate::error::Result;
use crate::handlers::lifecycle;
use crate::session;

/// A brief pause after an `accept` failure, before trying again.
///
/// `accept` itself failing (as opposed to one connection's handshake
/// failing, which is ordinary and does not reach this constant) means
/// something is wrong with the listening socket or the process's file
/// descriptor budget — conditions that usually clear on their own. Retrying
/// immediately in a hot loop would spend CPU finding that out; this is the
/// entire reason the constant exists.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// How long [`refuse_capacity`] waits for a `Hello` before giving up on a
/// peer that will be told no either way.
///
/// Deliberately short and fixed rather than
/// [`CoordinatorConfig::handshake_timeout`]: a connection over the ceiling
/// holds no [`ConnectionLimiter`] slot (there was none to give it), so
/// waiting the full, often much longer, configured handshake budget here
/// would let a burst of over-ceiling dials each tie up a socket for that
/// long — an unbounded-by-the-limiter resource use on the very path meant
/// to bound resource use.
const CAPACITY_REFUSAL_TIMEOUT: Duration = Duration::from_secs(2);

/// A coordinator's listening socket, bound but not yet accepting
/// connections.
///
/// Split from [`CoordinatorServer::serve`] so a caller — a test, or `astrs
/// up`'s embedded server — can read [`CoordinatorServer::local_addr`] (the
/// actual port, when [`CoordinatorConfig::bind_addr`] asked for an
/// ephemeral one) and obtain a [`ServerHandle`] before a single connection
/// is accepted.
pub struct CoordinatorServer {
    coordinator: Coordinator,
    listener: TcpListener,
    limiter: Arc<ConnectionLimiter>,
    shutdown_tx: Arc<watch::Sender<bool>>,
    shutdown_rx: watch::Receiver<bool>,
}

impl CoordinatorServer {
    /// Binds `coordinator`'s configured address.
    ///
    /// # Errors
    ///
    /// [`crate::CoordinatorError::Io`] if the bind fails (address already in
    /// use, insufficient privilege for the port, ...).
    pub async fn bind(coordinator: Coordinator) -> Result<Self> {
        let listener = TcpListener::bind(coordinator.config.bind_addr()).await?;
        let limiter = Arc::new(ConnectionLimiter::new(
            coordinator.config.connection_limit,
            coordinator.config.per_ip_connection_limit,
        ));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Ok(Self {
            coordinator,
            listener,
            limiter,
            shutdown_tx: Arc::new(shutdown_tx),
            shutdown_rx,
        })
    }

    /// The address actually bound — the resolved port, when
    /// [`CoordinatorConfig::bind_addr`] asked for an ephemeral one.
    ///
    /// # Errors
    ///
    /// [`crate::CoordinatorError::Io`] if the socket cannot report it.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// The coordinator hub this server accepts connections into — the same
    /// handle a caller already holds, returned so a caller that only kept
    /// the server can still reach it (registries, store, clock) without
    /// threading a second clone through construction.
    #[must_use]
    pub fn coordinator(&self) -> &Coordinator {
        &self.coordinator
    }

    /// A clonable handle that can ask a running [`CoordinatorServer::serve`]
    /// to stop.
    #[must_use]
    pub fn handle(&self) -> ServerHandle {
        ServerHandle {
            shutdown_tx: Arc::clone(&self.shutdown_tx),
        }
    }

    /// Runs the accept loop and the heartbeat watchdog until a
    /// [`ServerHandle::shutdown`] call resolves them both.
    ///
    /// A failed handshake, or a session actor that returns (the peer
    /// disconnected, or sent something malformed) is one connection ending,
    /// never a reason to stop serving everyone else.
    ///
    /// # Errors
    ///
    /// This method currently always returns `Ok(())` on a requested
    /// shutdown — the `Result` return exists so a future revision that adds
    /// a genuinely fatal listener condition does not need a signature
    /// change to report it.
    pub async fn serve(mut self) -> Result<()> {
        let watchdog = tokio::spawn(heartbeat_watchdog_loop(
            self.coordinator.clone(),
            self.shutdown_rx.clone(),
        ));

        loop {
            tokio::select! {
                changed = self.shutdown_rx.changed() => {
                    if changed.is_err() || *self.shutdown_rx.borrow() {
                        break;
                    }
                }
                accepted = self.listener.accept() => {
                    match accepted {
                        Ok((stream, peer_addr)) => {
                            spawn_connection(
                                self.coordinator.clone(),
                                Arc::clone(&self.limiter),
                                stream,
                                peer_addr,
                            );
                        }
                        Err(err) => {
                            tracing::warn!(%err, "coordinator accept failed");
                            tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                        }
                    }
                }
            }
        }

        watchdog.abort();
        let _ = watchdog.await;
        Ok(())
    }
}

/// A clonable request to stop a running [`CoordinatorServer::serve`].
///
/// Safe to call before `serve` even starts running: the underlying
/// [`watch`] channel's receiver is created in [`CoordinatorServer::bind`],
/// strictly before any [`ServerHandle`] can be obtained from it, so a
/// [`ServerHandle::shutdown`] call is never missed regardless of how the
/// two tasks happen to get scheduled.
#[derive(Clone)]
pub struct ServerHandle {
    shutdown_tx: Arc<watch::Sender<bool>>,
}

impl ServerHandle {
    /// Asks the server to stop accepting connections and return from
    /// [`CoordinatorServer::serve`].
    ///
    /// Does not close connections already being served — those run to
    /// their own completion (the peer disconnecting, or a later process
    /// shutdown) exactly as they would without a shutdown request.
    pub fn shutdown(&self) {
        // The only failure mode is every receiver already dropped, which
        // only happens if `serve` already returned — nothing left to tell.
        let _ = self.shutdown_tx.send(true);
    }
}

/// Spawns the task that greets, negotiates and then runs one accepted
/// connection.
///
/// A free function rather than a method: the spawned task must be
/// `'static`, so every input is either owned or an `Arc` clone rather than
/// a borrow of the [`CoordinatorServer`] that is still looping.
fn spawn_connection(
    coordinator: Coordinator,
    limiter: Arc<ConnectionLimiter>,
    stream: TcpStream,
    peer_addr: SocketAddr,
) {
    tokio::spawn(async move {
        let _ = stream.set_nodelay(true);
        let config = Arc::clone(&coordinator.config);
        match limiter.acquire(peer_addr.ip()) {
            Ok(guard) => {
                handle_connection(coordinator, stream, &config).await;
                drop(guard);
            }
            Err(reason) => refuse_capacity(stream, &config, reason).await,
        }
    });
}

/// Runs one admitted connection's handshake and, on success, its session
/// actor — [`crate::session::cli::run`] or [`crate::session::daemon::run`],
/// chosen by the role [`astrs_wire::Acceptor::negotiate`] accepted.
async fn handle_connection(
    coordinator: Coordinator,
    stream: TcpStream,
    config: &CoordinatorConfig,
) {
    let counters = ConnectionCounters::shared();
    let mut framed = FramedStream::new(stream, config.frame_limits(), counters);

    // Every connection gets a fresh session: the coordinator's own
    // `StateCatchUp` (blueprint §12) resumes a *daemon's* place in the
    // mutation log by the sequence number its `Register` carries (see
    // `crate::catchup`), not by resuming a dropped transport session —
    // so a resume request has nothing to grant here.
    let assignment = |_hello: &Hello| SessionAssignment::Fresh(SessionId::generate());
    let (session, scope) = match accept_scoped(&mut framed, config, assignment).await {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::warn!(%err, "coordinator handshake failed");
            return;
        }
    };

    let role = session.role;
    let session_id = session.session_id;
    let (reader, writer) = framed.into_halves();
    match role {
        Role::Cli => session::cli::run(coordinator, reader, writer, scope).await,
        Role::Daemon => session::daemon::run(coordinator, reader, writer, session_id).await,
        // `RoleSet::COORDINATOR` only ever admits `Cli`/`Daemon` — a third
        // role reaching here would mean the acceptor's role check regressed,
        // not a case this build has any session actor for.
        other => tracing::warn!(
            ?other,
            "accepted a role this coordinator has no session actor for"
        ),
    }
}

/// Runs the coordinator's half of the greeting, additionally deciding which
/// [`RequestScope`] the connection is granted (blueprint §16, §22).
///
/// Behaves exactly like [`astrs_transport::accept_with`] — which this
/// inlines rather than calls — for a connection presenting the root token or
/// an invalid one: the same pre-handshake limit narrowing, the same
/// [`CoordinatorConfig::handshake_timeout`], the same [`negotiate`] call, the
/// same [`RefusalReason::BadAuth`] on failure. `accept_with` cannot be reused
/// unmodified because it is built around exactly one [`Acceptor`], chosen
/// *before* `Hello` arrives, and a token's scope is a decision that needs
/// `Hello.auth` to make — and `astrs-wire`/`astrs-transport` are not this
/// task's to extend (blueprint §22 pull-forward: token scopes land "WITHOUT
/// wire-protocol enum changes").
///
/// The one addition: [`scope_candidates`] builds an ordered list of
/// `(token, scope)` pairs for the greeting's role, and this tries
/// [`negotiate`] against each in turn, keeping the *first*
/// [`astrs_wire::HandshakeOutcome::Accepted`] it gets — tagged with that
/// candidate's scope — or, if every candidate is refused, the *first*
/// candidate's refusal (never a later one), so a legacy client seeing only
/// the root candidate observes the exact refusal shape `accept_with` always
/// produced. [`negotiate`] is a pure function of its inputs (its own module
/// docs), so trying more than one changes nothing about what any individual
/// call decides on its own. Every role tries exactly the same number of
/// candidates every time — [`scope_candidates`] returns a list whose length
/// depends only on `hello.role`, never on how many of them would have
/// accepted — so which candidate a real client happens to hold changes
/// nothing about how many [`negotiate`] calls (and therefore how long this
/// function runs) a given role's connection costs.
async fn accept_scoped(
    framed: &mut FramedStream<TcpStream>,
    config: &CoordinatorConfig,
    assign: impl FnOnce(&Hello) -> SessionAssignment,
) -> std::result::Result<(NegotiatedSession, RequestScope), ScopedHandshakeError> {
    let target_limits = *framed.limits();
    framed.set_limits(pre_handshake_limits(&target_limits).to_frame_limits());

    let request = tokio::time::timeout(
        config.handshake_timeout,
        framed.expect_message::<ControlRequest>(FrameKind::Control),
    )
    .await
    .map_err(|_elapsed| ScopedHandshakeError::Timeout)?
    .map_err(ScopedHandshakeError::Transport)?;

    let hello = match request {
        ControlRequest::Hello(hello) => hello,
        other => {
            let acceptor = Acceptor::new(config.token.clone())
                .with_accepted_roles(RoleSet::COORDINATOR)
                .with_limits(config.limits);
            let refused = acceptor.refuse(RefusalReason::Internal {
                message: "the first frame on a connection must be a greeting".into(),
            });
            let _ = framed.send_message(&ControlReply::Refused(refused)).await;
            return Err(ScopedHandshakeError::UnexpectedFrame(other.variant_name()));
        }
    };

    let session = assign(&hello);
    let mut first_refusal = None;
    let mut accepted = None;
    for (token, scope) in scope_candidates(&config.token, hello.role) {
        let acceptor = Acceptor::new(token)
            .with_accepted_roles(RoleSet::COORDINATOR)
            .with_limits(config.limits);
        match negotiate(&hello, &acceptor, session) {
            HandshakeOutcome::Accepted { welcome, session } => {
                accepted = Some((welcome, session, scope));
                break;
            }
            HandshakeOutcome::Refused(refused) => {
                first_refusal.get_or_insert(refused);
            }
        }
    }

    match accepted {
        Some((welcome, session, scope)) => {
            tokio::time::timeout(
                config.handshake_timeout,
                framed.send_message(&ControlReply::Welcome(welcome)),
            )
            .await
            .map_err(|_elapsed| ScopedHandshakeError::Timeout)?
            .map_err(ScopedHandshakeError::Transport)?;
            framed.set_limits(session.frame_limits());
            Ok((session, scope))
        }
        None => {
            // `scope_candidates` never returns an empty list (its own docs),
            // so a refusal was recorded on every iteration of the loop above.
            let refused = first_refusal.unwrap_or_else(|| {
                Acceptor::new(config.token.clone())
                    .with_accepted_roles(RoleSet::COORDINATOR)
                    .with_limits(config.limits)
                    .refuse(RefusalReason::Internal {
                        message: "no scope candidate was tried".to_owned(),
                    })
            });
            let _ = framed
                .send_message(&ControlReply::Refused(refused.clone()))
                .await;
            Err(ScopedHandshakeError::Refused(refused))
        }
    }
}

/// The tokens `accept_scoped` tries, in order, for a greeting claiming
/// `role` — each paired with the [`RequestScope`] a match against it grants
/// (blueprint §16, §22).
///
/// The root token is always first and always present: back-compat (blueprint
/// §16 — "legacy scope-less tokens = mutate for back-compat") means a
/// root-token holder must always be accepted on the very first candidate,
/// with no dependence on role. [`crate::auth::derive_read_token`]'s output
/// is appended only for [`Role::Cli`]: token scopes are a CLI concept — a
/// daemon or peer leg is always fully trusted or refused outright, never
/// scope-restricted (blueprint §16) — so a non-CLI role's list is always
/// exactly the one root candidate, both to keep that guarantee simple to
/// audit and so every connection of a given role costs the identical number
/// of [`negotiate`] calls regardless of which candidate (if any) a real
/// client happens to hold.
///
/// Never empty: every role has at least the root candidate.
fn scope_candidates(root: &AuthToken, role: Role) -> Vec<(AuthToken, RequestScope)> {
    let mut candidates = vec![(root.clone(), RequestScope::Mutate)];
    if role == Role::Cli {
        candidates.push((crate::auth::derive_read_token(root), RequestScope::Read));
    }
    candidates
}

/// What can go wrong while [`accept_scoped`] negotiates one greeting.
///
/// Never leaves [`handle_connection`], which only logs it and drops the
/// connection — exactly what happened for any
/// [`astrs_transport::TransportError`] that [`astrs_transport::accept_with`]
/// surfaced before [`accept_scoped`] replaced that call here.
#[derive(Debug)]
enum ScopedHandshakeError {
    /// The exchange did not complete within
    /// [`CoordinatorConfig::handshake_timeout`].
    Timeout,
    /// A framing or I/O failure while reading or writing.
    Transport(TransportError),
    /// The first frame on the connection was not a greeting; its variant
    /// name, for the log line.
    UnexpectedFrame(&'static str),
    /// The connection was refused (bad auth, wrong role, over capacity,
    /// ...); the refusal has already been sent.
    Refused(Refused),
}

impl std::fmt::Display for ScopedHandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => f.write_str("handshake timed out"),
            Self::Transport(err) => write!(f, "{err}"),
            Self::UnexpectedFrame(variant) => {
                write!(f, "peer opened with {variant} instead of a greeting")
            }
            Self::Refused(refused) => write!(f, "{refused}"),
        }
    }
}

/// Answers an over-capacity peer with a typed
/// [`RefusalReason::TooManyConnections`] instead of a bare disconnect,
/// best-effort: a peer that already gave up waiting, or never sends a
/// `Hello` at all, gets nothing back, but nothing here can hang the
/// listener either way.
async fn refuse_capacity(stream: TcpStream, config: &CoordinatorConfig, reason: RefusalReason) {
    let counters = ConnectionCounters::shared();
    let narrow = pre_handshake_limits(&config.frame_limits()).to_frame_limits();
    let mut framed = FramedStream::new(stream, narrow, counters);
    let greeting = tokio::time::timeout(
        CAPACITY_REFUSAL_TIMEOUT,
        framed.expect_message::<ControlRequest>(FrameKind::Control),
    )
    .await;
    if let Ok(Ok(ControlRequest::Hello(_))) = greeting {
        let _ = framed
            .send_message(&ControlReply::Refused(Refused::new(reason)))
            .await;
    }
}

/// Sends every connected daemon a fresh heartbeat and sweeps for silence,
/// every [`CoordinatorConfig::heartbeat_interval`], until `shutdown`
/// resolves.
async fn heartbeat_watchdog_loop(coordinator: Coordinator, mut shutdown: watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(coordinator.config.heartbeat_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
            _ = ticker.tick() => run_heartbeat_sweep(&coordinator).await,
        }
    }
}

/// One heartbeat tick: pushes [`CoordinatorEvent::Heartbeat`] at every
/// connected daemon, then sweeps the registry for ones that have not been
/// heard from — degrading, then (blueprint §12) declaring lost and
/// cascading through [`lifecycle::handle_daemon_disconnected`], the exact
/// path a closed socket's own cleanup already uses (safe to run for the
/// same daemon from both: every step it takes is idempotent past the first
/// call — see that function's docs).
async fn run_heartbeat_sweep(coordinator: &Coordinator) {
    let now = coordinator.clock.now();
    let interval = coordinator.config.heartbeat_interval;
    let missed_heartbeat_limit = coordinator.config.missed_heartbeat_limit;

    let sweep = {
        let mut daemons = coordinator.daemons();
        for handle in daemons.handles_mut() {
            let seq = handle.next_heartbeat_seq();
            let _ = handle.send(CoordinatorEvent::Heartbeat { seq, sent_at: now });
        }
        daemons.sweep_heartbeats(now, interval, missed_heartbeat_limit)
    };

    for id in &sweep.newly_degraded {
        tracing::warn!(daemon = %id, "daemon degraded: missed heartbeats");
    }
    for id in sweep.lost {
        tracing::error!(daemon = %id, "daemon lost: exceeded the missed-heartbeat budget");
        coordinator.daemons().remove(&id);
        let _ = coordinator
            .store
            .set_daemon_reachable(id.clone(), false)
            .await;
        lifecycle::handle_daemon_disconnected(coordinator, id).await;
    }
}

/// Enforces [`CoordinatorConfig::connection_limit`] and
/// [`CoordinatorConfig::per_ip_connection_limit`] before a connection's
/// handshake spends any CPU — dora's coordinator hardcoded a
/// 256-connection cap; this one is configured and actually checked
/// (blueprint §7.3: "limits in config, enforced symmetrically").
#[derive(Debug)]
struct ConnectionLimiter {
    total_limit: Option<u32>,
    per_ip_limit: Option<u32>,
    state: Mutex<LimiterState>,
}

/// The mutable half of [`ConnectionLimiter`], behind one lock so a total
/// and a per-IP check-and-increment can never observe each other's
/// half-applied update.
#[derive(Debug, Default)]
struct LimiterState {
    total: u32,
    per_ip: HashMap<IpAddr, u32>,
}

impl ConnectionLimiter {
    /// A limiter enforcing `total_limit` concurrent connections overall and
    /// `per_ip_limit` from any one remote address. Either `None` is
    /// unbounded.
    fn new(total_limit: Option<u32>, per_ip_limit: Option<u32>) -> Self {
        Self {
            total_limit,
            per_ip_limit,
            state: Mutex::new(LimiterState::default()),
        }
    }

    /// Reserves one connection slot for `ip`, or reports which ceiling
    /// blocked it. The reservation is released when the returned
    /// [`ConnectionGuard`] drops.
    fn acquire(
        self: &Arc<Self>,
        ip: IpAddr,
    ) -> std::result::Result<ConnectionGuard, RefusalReason> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(limit) = self.total_limit
            && state.total >= limit
        {
            return Err(RefusalReason::TooManyConnections { limit });
        }
        if let Some(limit) = self.per_ip_limit {
            let current = state.per_ip.get(&ip).copied().unwrap_or(0);
            if current >= limit {
                return Err(RefusalReason::TooManyConnections { limit });
            }
        }
        state.total += 1;
        *state.per_ip.entry(ip).or_insert(0) += 1;
        drop(state);
        Ok(ConnectionGuard {
            limiter: Arc::clone(self),
            ip,
        })
    }
}

/// Releases the slot [`ConnectionLimiter::acquire`] reserved, once, when
/// dropped — regardless of which of `acquire`'s many callers held it, or
/// whether the connection it guarded ended cleanly or panicked.
#[derive(Debug)]
struct ConnectionGuard {
    limiter: Arc<ConnectionLimiter>,
    ip: IpAddr,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let mut state = self
            .limiter
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.total = state.total.saturating_sub(1);
        if let Some(count) = state.per_ip.get_mut(&self.ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.per_ip.remove(&self.ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::net::{IpAddr, Ipv4Addr};

    use super::*;
    use crate::config::CoordinatorConfig;
    use astrs_transport::LocalIdentity;
    use astrs_wire::{AuthToken, DaemonRegistration, ErrorCode, FeatureFlags, Hello};

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, last))
    }

    #[test]
    fn an_unbounded_limiter_never_refuses() {
        let limiter = Arc::new(ConnectionLimiter::new(None, None));
        let guards: Vec<_> = (0..50).map(|_| limiter.acquire(ip(1)).unwrap()).collect();
        assert_eq!(guards.len(), 50);
    }

    #[test]
    fn a_total_ceiling_refuses_once_reached_and_recovers_on_drop() {
        let limiter = Arc::new(ConnectionLimiter::new(Some(2), None));
        let first = limiter.acquire(ip(1)).unwrap();
        let second = limiter.acquire(ip(2)).unwrap();
        assert_eq!(
            limiter.acquire(ip(3)).unwrap_err(),
            RefusalReason::TooManyConnections { limit: 2 }
        );

        drop(first);
        let third = limiter.acquire(ip(3)).unwrap();
        drop(second);
        drop(third);
    }

    #[test]
    fn a_per_ip_ceiling_is_independent_of_other_ips() {
        let limiter = Arc::new(ConnectionLimiter::new(None, Some(1)));
        let _first = limiter.acquire(ip(1)).unwrap();
        assert_eq!(
            limiter.acquire(ip(1)).unwrap_err(),
            RefusalReason::TooManyConnections { limit: 1 }
        );
        // A different IP is a different bucket.
        let _second = limiter.acquire(ip(2)).unwrap();
    }

    #[test]
    fn dropping_every_guard_for_an_ip_forgets_it() {
        let limiter = Arc::new(ConnectionLimiter::new(None, Some(1)));
        let first = limiter.acquire(ip(1)).unwrap();
        drop(first);
        assert!(limiter.state.lock().unwrap().per_ip.is_empty());
    }

    fn token() -> AuthToken {
        AuthToken::from_bytes([42; 32])
    }

    fn config() -> CoordinatorConfig {
        CoordinatorConfig::new(token())
            .with_port(0)
            .with_heartbeat(Duration::from_millis(30), 2)
    }

    async fn server() -> (CoordinatorServer, SocketAddr) {
        let coordinator = Coordinator::open_in_memory(config()).unwrap();
        let server = CoordinatorServer::bind(coordinator).await.unwrap();
        let addr = server.local_addr().unwrap();
        (server, addr)
    }

    /// A bare, unauthenticated-until-`Hello` client connection — the same
    /// shape a real CLI or daemon dials in with.
    async fn dial(addr: SocketAddr) -> FramedStream<tokio::net::TcpStream> {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        FramedStream::new(
            stream,
            astrs_wire::FrameLimits::network(),
            ConnectionCounters::shared(),
        )
    }

    async fn greet(
        addr: SocketAddr,
        role: Role,
        auth: AuthToken,
    ) -> astrs_transport::TransportResult<(
        FramedStream<tokio::net::TcpStream>,
        astrs_transport::InitiatedHandshake,
    )> {
        let mut stream = dial(addr).await;
        let params = astrs_transport::HandshakeParams::new(LocalIdentity::new(role), auth)
            .with_features(FeatureFlags::EMPTY);
        let outcome = astrs_transport::initiate(&mut stream, &params, Duration::from_secs(5)).await;
        outcome.map(|handshake| (stream, handshake))
    }

    #[tokio::test]
    async fn a_cli_connection_completes_the_handshake_and_answers_a_request() {
        let (server, addr) = server().await;
        let handle = server.handle();
        let serve_task = tokio::spawn(server.serve());

        let (mut stream, handshake) = greet(addr, Role::Cli, token()).await.unwrap();
        assert_eq!(handshake.session.role, Role::Cli);

        stream
            .send_message(&ControlRequest::List { all: true })
            .await
            .unwrap();
        let reply: ControlReply = stream
            .expect_message(FrameKind::ControlReply)
            .await
            .unwrap();
        assert!(matches!(reply, ControlReply::DataflowList { .. }));

        handle.shutdown();
        serve_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_daemon_connection_registers_and_is_visible_to_a_cli() {
        let (server, addr) = server().await;
        let handle = server.handle();
        let serve_task = tokio::spawn(server.serve());

        let (mut daemon_stream, handshake) = greet(addr, Role::Daemon, token()).await.unwrap();
        assert_eq!(handshake.session.role, Role::Daemon);
        let daemon_id = astrs_wire::DaemonId::generate(None);
        daemon_stream
            .send_message(&astrs_wire::DaemonEvent::Register(DaemonRegistration::new(
                daemon_id.clone(),
                "127.0.0.1:7408",
                handshake.session.session_id,
            )))
            .await
            .unwrap();

        let (mut cli_stream, _) = greet(addr, Role::Cli, token()).await.unwrap();
        // The registration is handled by the daemon session's own task;
        // poll rather than assume it has landed the instant `send_message`
        // returns.
        let mut connected = false;
        for _ in 0..200 {
            cli_stream
                .send_message(&ControlRequest::ConnectedDaemons {
                    include_unreachable: false,
                })
                .await
                .unwrap();
            let reply: ControlReply = cli_stream
                .expect_message(FrameKind::ControlReply)
                .await
                .unwrap();
            if let ControlReply::DaemonList { daemons } = reply
                && daemons.iter().any(|d| d.id == daemon_id)
            {
                connected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(connected, "the registered daemon never became visible");

        handle.shutdown();
        serve_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_bad_token_is_refused_and_never_reaches_a_session_actor() {
        let (server, addr) = server().await;
        let handle = server.handle();
        let serve_task = tokio::spawn(server.serve());

        let err = greet(addr, Role::Cli, AuthToken::from_bytes([0; 32]))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            astrs_transport::TransportError::Refused(refused)
                if refused.reason == RefusalReason::BadAuth
        ));

        handle.shutdown();
        serve_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_read_scope_token_answers_a_read_verb_but_is_denied_a_mutating_one() {
        let (server, addr) = server().await;
        let handle = server.handle();
        let serve_task = tokio::spawn(server.serve());

        let read_token = crate::auth::derive_read_token(&token());
        let (mut stream, handshake) = greet(addr, Role::Cli, read_token).await.unwrap();
        assert_eq!(handshake.session.role, Role::Cli);

        stream
            .send_message(&ControlRequest::List { all: true })
            .await
            .unwrap();
        let reply: ControlReply = stream
            .expect_message(FrameKind::ControlReply)
            .await
            .unwrap();
        assert!(
            matches!(reply, ControlReply::DataflowList { .. }),
            "a read verb must still be answered: {reply:?}"
        );

        stream
            .send_message(&ControlRequest::Destroy { force: false })
            .await
            .unwrap();
        let reply: ControlReply = stream
            .expect_message(FrameKind::ControlReply)
            .await
            .unwrap();
        assert_eq!(
            reply.error_code(),
            Some(ErrorCode::PermissionDenied),
            "a mutating verb on a read-scope connection must be denied, not dispatched: {reply:?}"
        );

        handle.shutdown();
        serve_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_root_token_still_completes_a_mutating_verb_unchanged() {
        // The back-compat case (blueprint §16, §22): a token minted before
        // token scopes existed is exactly the root token, and must keep
        // full access with no migration step.
        let (server, addr) = server().await;
        let handle = server.handle();
        let serve_task = tokio::spawn(server.serve());

        let (mut stream, _) = greet(addr, Role::Cli, token()).await.unwrap();
        stream
            .send_message(&ControlRequest::Destroy { force: false })
            .await
            .unwrap();
        let reply: ControlReply = stream
            .expect_message(FrameKind::ControlReply)
            .await
            .unwrap();
        assert_ne!(
            reply.error_code(),
            Some(ErrorCode::PermissionDenied),
            "the root token must never be scope-denied: {reply:?}"
        );

        handle.shutdown();
        serve_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_daemon_cannot_authenticate_with_the_read_scope_token() {
        // Token scopes are a CLI concept (§16): a daemon leg is always fully
        // trusted or refused outright, never scope-restricted, so the
        // derived credential must not open a daemon session either.
        let (server, addr) = server().await;
        let handle = server.handle();
        let serve_task = tokio::spawn(server.serve());

        let read_token = crate::auth::derive_read_token(&token());
        let err = greet(addr, Role::Daemon, read_token).await.unwrap_err();
        assert!(matches!(
            err,
            astrs_transport::TransportError::Refused(refused)
                if refused.reason == RefusalReason::BadAuth
        ));

        handle.shutdown();
        serve_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn an_unrelated_token_is_refused_exactly_as_before_the_retry_existed() {
        // Guards the genuinely-invalid-token branch of `accept_scoped`: it
        // must fail closed with the *original* refusal rather than the
        // read-scope retry ever accepting something it should not.
        let (server, addr) = server().await;
        let handle = server.handle();
        let serve_task = tokio::spawn(server.serve());

        let err = greet(addr, Role::Cli, AuthToken::from_bytes([0xEE; 32]))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            astrs_transport::TransportError::Refused(refused)
                if refused.reason == RefusalReason::BadAuth
        ));

        handle.shutdown();
        serve_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_role_the_coordinator_does_not_serve_is_refused() {
        let (server, addr) = server().await;
        let handle = server.handle();
        let serve_task = tokio::spawn(server.serve());

        let err = greet(addr, Role::Node, token()).await.unwrap_err();
        assert!(matches!(
            err,
            astrs_transport::TransportError::Refused(refused)
                if matches!(refused.reason, RefusalReason::RoleNotPermitted { role: Role::Node })
        ));

        handle.shutdown();
        serve_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_connection_over_the_ceiling_is_refused_with_the_typed_reason() {
        let coordinator =
            Coordinator::open_in_memory(config().with_connection_limit(Some(1))).unwrap();
        let server = CoordinatorServer::bind(coordinator).await.unwrap();
        let addr = server.local_addr().unwrap();
        let handle = server.handle();
        let serve_task = tokio::spawn(server.serve());

        // Hold the one slot open with a connection that never finishes its
        // own handshake, so the second dial is refused deterministically
        // rather than racing the first connection's session actor to close.
        let _first = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut second = dial(addr).await;
        second
            .send_message(&ControlRequest::Hello(Hello::new(Role::Cli, token())))
            .await
            .unwrap();
        let reply: ControlReply = second
            .expect_message(FrameKind::ControlReply)
            .await
            .unwrap();
        assert!(matches!(
            reply,
            ControlReply::Refused(Refused {
                reason: RefusalReason::TooManyConnections { limit: 1 },
                ..
            })
        ));

        handle.shutdown();
        serve_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_silent_daemon_is_declared_lost_and_removed_by_the_watchdog() {
        // `missed_heartbeat_limit` of 2 and a 30 ms interval (see `config`)
        // declares a daemon lost at 4 missed intervals: well under the
        // polling budget below, and short enough not to slow the suite.
        let (server, addr) = server().await;
        let coordinator = server.coordinator().clone();
        let handle = server.handle();
        let serve_task = tokio::spawn(server.serve());

        let (mut daemon_stream, handshake) = greet(addr, Role::Daemon, token()).await.unwrap();
        let daemon_id = astrs_wire::DaemonId::generate(None);
        daemon_stream
            .send_message(&astrs_wire::DaemonEvent::Register(DaemonRegistration::new(
                daemon_id.clone(),
                "127.0.0.1:7408",
                handshake.session.session_id,
            )))
            .await
            .unwrap();

        let mut lost = false;
        for _ in 0..200 {
            if !coordinator.daemons().is_connected(&daemon_id) {
                lost = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            lost,
            "a daemon that never answers a heartbeat must eventually be declared lost"
        );

        handle.shutdown();
        serve_task.await.unwrap().unwrap();
    }
}
