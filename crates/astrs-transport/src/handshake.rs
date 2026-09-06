//! Driving the greeting over a live socket (blueprint §7.2).
//!
//! > First frame on every connection is `Hello { protocol, astrs_version, role,
//! > auth, features }` → `Welcome { protocol, limits, session_id }`.
//! > Incompatible protocol → typed `Refused` with the highest mutually
//! > supported version.
//!
//! `astrs-wire` owns the *decision* — [`astrs_wire::negotiate()`] is a pure
//! function of a greeting and an [`Acceptor`] — and this module owns the *I/O*
//! around it: who speaks first, what happens when the wrong frame arrives, and
//! the buffer-sizing dance that makes an unauthenticated socket safe.
//!
//! # The buffer-sizing dance
//!
//! A peer that has not yet proved who it is may not make this process allocate
//! 64 MiB. Every handshake here therefore runs in three steps:
//!
//! 1. **Narrow.** The stream opens with a
//!    [`PRE_HANDSHAKE_MAX_PAYLOAD_BYTES`]
//!    ceiling — enough for a greeting, and nothing more.
//! 2. **Exchange.** `Hello` out, `Welcome` or `Refused` back (or the mirror,
//!    for an acceptor).
//! 3. **Widen, symmetrically.** Both directions move to
//!    [`NegotiatedSession::frame_limits`], so the ceiling this end enforces on
//!    receive is exactly the one it honours on send.
//!
//! Step 3 is the one worth stating twice: an endpoint that widened only its
//! reader would happily emit frames its peer is obliged to refuse.
//!
//! # Examples
//!
//! ```
//! use astrs_transport::{
//!     ConnectionCounters, FramedStream, HandshakeParams, LocalIdentity, accept, initiate,
//! };
//! use astrs_wire::{
//!     Acceptor, AuthToken, FrameLimits, NegotiatedLimits, Role, RoleSet, SessionAssignment,
//!     SessionId,
//! };
//!
//! # fn main() {
//! # tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async {
//! let token = AuthToken::from_bytes([9; 32]);
//! let (client_io, server_io) = tokio::io::duplex(64 * 1024);
//!
//! let mut client = FramedStream::new(client_io, FrameLimits::uds(), ConnectionCounters::shared());
//! let mut server = FramedStream::new(server_io, FrameLimits::uds(), ConnectionCounters::shared());
//!
//! let params = HandshakeParams::new(LocalIdentity::new(Role::Node), token.clone())
//!     .with_limits(NegotiatedLimits::uds());
//! let acceptor = Acceptor::new(token).with_accepted_roles(RoleSet::NODES);
//!
//! let server_task = tokio::spawn(async move {
//!     let outcome = accept(
//!         &mut server,
//!         &acceptor,
//!         SessionAssignment::Fresh(SessionId::from_u128(1)),
//!         std::time::Duration::from_secs(5),
//!     )
//!     .await;
//!     outcome.map(|outcome| outcome.session.session_id)
//! });
//!
//! let session = initiate(&mut client, &params, std::time::Duration::from_secs(5))
//!     .await
//!     .expect("handshake");
//! assert_eq!(session.session.role, Role::Node);
//! assert_eq!(server_task.await.unwrap().unwrap(), session.session.session_id);
//! # });
//! # }
//! ```

use std::time::Duration;

use astrs_wire::{
    Acceptor, AuthToken, ControlReply, ControlRequest, FeatureFlags, FrameKind, FrameLimits,
    HandshakeOutcome, Hello, NegotiatedLimits, NegotiatedSession, RefusalReason, Refused, Role,
    SessionAssignment, SessionId, Welcome,
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::config::{LocalIdentity, PRE_HANDSHAKE_MAX_PAYLOAD_BYTES, TransportConfig};
use crate::error::{TransportError, TransportResult};
use crate::framed::FramedDuplex;

/// What an endpoint offers in its greeting.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HandshakeParams {
    /// The role and label this endpoint claims.
    pub identity: LocalIdentity,
    /// The cluster token (§16).
    pub auth: AuthToken,
    /// The budget this endpoint proposes.
    pub limits: NegotiatedLimits,
    /// The capabilities this endpoint advertises.
    pub features: FeatureFlags,
    /// A session to resume, for a reconnecting peer.
    pub resume: Option<SessionId>,
}

impl HandshakeParams {
    /// Parameters for `identity`, authenticating with `auth`.
    #[must_use]
    pub fn new(identity: LocalIdentity, auth: AuthToken) -> Self {
        Self {
            identity,
            auth,
            limits: NegotiatedLimits::network(),
            features: FeatureFlags::daemon_defaults(),
            resume: None,
        }
    }

    /// Parameters taken from a [`TransportConfig`], with the plane's checksum
    /// policy applied.
    ///
    /// This is the constructor a backend uses: it keeps the greeting's limits
    /// and features in step with the connection policy without the caller
    /// restating either.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::{HandshakeParams, LocalIdentity, TransportConfig};
    /// use astrs_wire::{AuthToken, Role};
    ///
    /// let params = HandshakeParams::from_config(
    ///     &TransportConfig::uds(),
    ///     LocalIdentity::new(Role::Node),
    ///     AuthToken::ZERO,
    ///     false,
    /// );
    /// assert!(!params.limits.require_crc);
    /// ```
    #[must_use]
    pub fn from_config(
        config: &TransportConfig,
        identity: LocalIdentity,
        auth: AuthToken,
        plane_requires_crc: bool,
    ) -> Self {
        Self {
            identity,
            auth,
            limits: config.proposed_limits(plane_requires_crc),
            features: config.advertised_features(),
            resume: None,
        }
    }

    /// Replaces the proposed limits.
    #[must_use]
    pub const fn with_limits(mut self, limits: NegotiatedLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Replaces the advertised features.
    #[must_use]
    pub const fn with_features(mut self, features: FeatureFlags) -> Self {
        self.features = features;
        self
    }

    /// Asks the peer to resume an existing session.
    #[must_use]
    pub const fn with_resume(mut self, session: Option<SessionId>) -> Self {
        self.resume = session;
        self
    }

    /// The greeting these parameters describe.
    #[must_use]
    pub fn to_hello(&self) -> Hello {
        let mut hello = Hello::new(self.identity.role, self.auth.clone())
            .with_features(self.features)
            .with_limits(self.limits);
        if let Some(label) = &self.identity.label {
            hello = hello.with_label(label.clone());
        }
        if let Some(session) = self.resume {
            hello = hello.with_resume(session);
        }
        hello
    }
}

/// The result of a completed handshake, from the initiator's side.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct InitiatedHandshake {
    /// What both ends agreed to.
    pub session: NegotiatedSession,
    /// The acceptor's answer, kept for its `peer_role` and version.
    pub welcome: Welcome,
}

/// The result of a completed handshake, from the acceptor's side.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AcceptedHandshake {
    /// What both ends agreed to.
    pub session: NegotiatedSession,
    /// The greeting that was accepted, kept for its label and role.
    pub hello: Hello,
}

impl AcceptedHandshake {
    /// The peer's self-declared label, if it sent one.
    #[must_use]
    pub fn peer_label(&self) -> Option<&str> {
        self.hello.label.as_deref()
    }
}

/// The narrow policy a connection uses until the greeting completes.
///
/// Derived from the caller's policy rather than invented, so that a leg which
/// does not checksum keeps not checksumming during the handshake.
///
/// # Examples
///
/// ```
/// use astrs_transport::pre_handshake_limits;
/// use astrs_wire::FrameLimits;
///
/// let narrow = pre_handshake_limits(&FrameLimits::network());
/// assert!(narrow.require_crc);
/// assert!(narrow.max_payload_bytes < FrameLimits::network().max_payload_bytes() as u64);
/// ```
#[must_use]
pub fn pre_handshake_limits(limits: &FrameLimits) -> NegotiatedLimits {
    let ceiling = limits
        .max_payload_bytes()
        .min(PRE_HANDSHAKE_MAX_PAYLOAD_BYTES);
    NegotiatedLimits::from_frame_limits(limits).with_max_payload_bytes(ceiling as u64)
}

/// Runs the initiator's half of the greeting.
///
/// Sends `Hello`, waits for `Welcome` or `Refused`, checks the answer against
/// what was offered, and widens both directions to the negotiated budget.
///
/// # Errors
///
/// - [`TransportError::Timeout`] if the exchange does not finish in `timeout`.
/// - [`TransportError::Refused`] if the peer refused, carrying its typed reason
///   and the highest protocol it speaks.
/// - [`TransportError::Handshake`] if the answer is inconsistent with the offer
///   — a role it never claimed, features it never advertised, a budget larger
///   than it proposed.
/// - [`TransportError::UnexpectedFrame`] if the peer answered with something
///   other than a `ControlReply`.
pub async fn initiate<R, W>(
    stream: &mut FramedDuplex<R, W>,
    params: &HandshakeParams,
    timeout: Duration,
) -> TransportResult<InitiatedHandshake>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let target_limits = *stream.limits();
    stream.set_limits(pre_handshake_limits(&target_limits).to_frame_limits());

    let hello = params.to_hello();
    let exchange = async {
        stream
            .send_message(&ControlRequest::Hello(hello.clone()))
            .await?;
        stream
            .expect_message::<ControlReply>(FrameKind::ControlReply)
            .await
    };

    let reply = with_timeout("handshake", timeout, exchange).await?;
    let welcome = match reply {
        ControlReply::Welcome(welcome) => welcome,
        ControlReply::Refused(refused) => return Err(TransportError::Refused(Box::new(refused))),
        other => {
            return Err(TransportError::Configuration(format!(
                "peer answered a greeting with {other:?}"
            )));
        }
    };

    let session =
        astrs_wire::accept_welcome(&hello, &welcome).map_err(TransportError::Handshake)?;
    stream.set_limits(session.frame_limits());
    Ok(InitiatedHandshake { session, welcome })
}

/// Runs the acceptor's half of the greeting.
///
/// Waits for `Hello`, negotiates it against `acceptor`, answers with `Welcome`
/// or `Refused`, and widens both directions on success.
///
/// A refusal is *sent* before it is returned: a peer that is told why it was
/// refused can pick a supported protocol and retry, which is the entire point
/// of the typed [`Refused`] (§7.2).
///
/// # Errors
///
/// - [`TransportError::Timeout`] if the exchange does not finish in `timeout`.
/// - [`TransportError::Refused`] carrying the refusal this end just sent.
/// - [`TransportError::UnexpectedFrame`] if the first frame is not a greeting.
pub async fn accept<R, W>(
    stream: &mut FramedDuplex<R, W>,
    acceptor: &Acceptor,
    assignment: SessionAssignment,
    timeout: Duration,
) -> TransportResult<AcceptedHandshake>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    accept_with(stream, acceptor, timeout, |_| assignment).await
}

/// Runs the acceptor's half of the greeting, choosing the session per peer.
///
/// The closure sees the greeting before the answer is composed, which is how a
/// coordinator decides whether a `resume` request names a session it still
/// holds (§7.2, and the state catch-up of §12).
///
/// # Errors
///
/// As [`accept`].
pub async fn accept_with<R, W, F>(
    stream: &mut FramedDuplex<R, W>,
    acceptor: &Acceptor,
    timeout: Duration,
    assign: F,
) -> TransportResult<AcceptedHandshake>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: FnOnce(&Hello) -> SessionAssignment,
{
    let target_limits = *stream.limits();
    stream.set_limits(pre_handshake_limits(&target_limits).to_frame_limits());

    let request = with_timeout(
        "handshake",
        timeout,
        stream.expect_message::<ControlRequest>(FrameKind::Control),
    )
    .await?;

    let hello = match request {
        ControlRequest::Hello(hello) => hello,
        other => {
            // The peer opened with something other than a greeting. Answer with
            // a refusal so it learns why, then fail.
            let refused = acceptor.refuse(RefusalReason::Internal {
                message: "the first frame on a connection must be a greeting".into(),
            });
            let _ = stream
                .send_message(&ControlReply::Refused(refused.clone()))
                .await;
            return Err(TransportError::Configuration(format!(
                "peer opened with {other:?} instead of a greeting"
            )));
        }
    };

    match acceptor.negotiate(&hello, assign(&hello)) {
        HandshakeOutcome::Accepted { welcome, session } => {
            with_timeout(
                "handshake",
                timeout,
                stream.send_message(&ControlReply::Welcome(welcome)),
            )
            .await?;
            stream.set_limits(session.frame_limits());
            Ok(AcceptedHandshake { session, hello })
        }
        HandshakeOutcome::Refused(refused) => {
            // Best effort: the peer may already be gone, and the refusal is
            // still the right error to return either way.
            let _ = stream
                .send_message(&ControlReply::Refused(refused.clone()))
                .await;
            Err(TransportError::Refused(Box::new(refused)))
        }
    }
}

/// Builds an [`Acceptor`] from a connection policy.
///
/// Keeps the acceptor's advertised limits and features in step with the
/// [`TransportConfig`] the same endpoint dials out with, so that a daemon does
/// not accept on terms it would never propose.
///
/// # Examples
///
/// ```
/// use astrs_transport::{TransportConfig, acceptor_from_config};
/// use astrs_wire::{AuthToken, Role, RoleSet};
///
/// let acceptor = acceptor_from_config(
///     &TransportConfig::uds(),
///     AuthToken::from_bytes([1; 32]),
///     RoleSet::NODES,
///     false,
/// );
/// assert!(acceptor.accepts_role(Role::Node));
/// assert!(!acceptor.accepts_role(Role::Cli));
/// ```
#[must_use]
pub fn acceptor_from_config(
    config: &TransportConfig,
    auth: AuthToken,
    roles: astrs_wire::RoleSet,
    plane_requires_crc: bool,
) -> Acceptor {
    Acceptor::new(auth)
        .with_accepted_roles(roles)
        .with_features(config.advertised_features())
        .with_limits(config.proposed_limits(plane_requires_crc))
}

/// The roles an endpoint serving `role` normally accepts.
///
/// A node socket serves nodes, a coordinator port serves CLIs and daemons, a
/// daemon's peer port serves peers. Getting this wrong is how a CLI ends up
/// half-connected to a node socket, so the mapping lives in one place.
///
/// # Examples
///
/// ```
/// use astrs_transport::default_accepted_roles;
/// use astrs_wire::{Role, RoleSet};
///
/// assert_eq!(default_accepted_roles(Role::Daemon), RoleSet::NODES.with(Role::Peer));
/// assert_eq!(default_accepted_roles(Role::Cli), RoleSet::EMPTY);
/// ```
#[must_use]
pub const fn default_accepted_roles(local: Role) -> astrs_wire::RoleSet {
    match local {
        // A coordinator is dialled by the CLI and by daemons.
        Role::Cli => astrs_wire::RoleSet::EMPTY,
        Role::Daemon => astrs_wire::RoleSet::NODES.with(Role::Peer),
        Role::Node => astrs_wire::RoleSet::EMPTY,
        Role::Peer => astrs_wire::RoleSet::PEERS,
        // `Role` is `#[non_exhaustive]`: a role added later serves nobody until
        // this table says otherwise, which fails closed.
        _ => astrs_wire::RoleSet::EMPTY,
    }
}

/// Applies a deadline to a handshake step.
async fn with_timeout<T>(
    operation: &'static str,
    timeout: Duration,
    future: impl Future<Output = TransportResult<T>>,
) -> TransportResult<T> {
    match tokio::time::timeout(timeout, future).await {
        Ok(result) => result,
        Err(_) => Err(TransportError::Timeout { operation, timeout }),
    }
}

use std::future::Future;

/// A refusal describing a protocol this build cannot speak.
///
/// Used by tests and by backends that must refuse before an [`Acceptor`]
/// exists — a listener at its connection ceiling, for instance.
#[must_use]
pub fn refuse(reason: RefusalReason) -> Refused {
    Refused::new(reason)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::framed::FramedStream;
    use crate::stats::ConnectionCounters;
    use astrs_wire::{PROTOCOL_VERSION, RoleSet};

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn token() -> AuthToken {
        AuthToken::from_bytes([0xab; 32])
    }

    fn duplex_pair(
        limits: FrameLimits,
    ) -> (
        FramedStream<tokio::io::DuplexStream>,
        FramedStream<tokio::io::DuplexStream>,
    ) {
        let (a, b) = tokio::io::duplex(1 << 20);
        (
            FramedStream::new(a, limits, ConnectionCounters::shared()),
            FramedStream::new(b, limits, ConnectionCounters::shared()),
        )
    }

    /// Runs both halves concurrently and returns both outcomes.
    async fn exchange(
        params: HandshakeParams,
        acceptor: Acceptor,
        limits: FrameLimits,
    ) -> (
        TransportResult<InitiatedHandshake>,
        TransportResult<AcceptedHandshake>,
    ) {
        let (mut client, mut server) = duplex_pair(limits);
        let server_task = tokio::spawn(async move {
            let outcome = accept(
                &mut server,
                &acceptor,
                SessionAssignment::Fresh(SessionId::from_u128(7)),
                TIMEOUT,
            )
            .await;
            (outcome, server)
        });
        let client_outcome = initiate(&mut client, &params, TIMEOUT).await;
        let (server_outcome, _server) = server_task.await.expect("server task");
        (client_outcome, server_outcome)
    }

    #[tokio::test]
    async fn a_matching_pair_agrees_on_everything() {
        let params =
            HandshakeParams::new(LocalIdentity::new(Role::Node).with_label("camera"), token())
                .with_limits(NegotiatedLimits::uds());
        let acceptor = Acceptor::new(token())
            .with_accepted_roles(RoleSet::NODES)
            .with_limits(NegotiatedLimits::uds());

        let (client, server) = exchange(params, acceptor, FrameLimits::uds()).await;
        let client = client.unwrap();
        let server = server.unwrap();

        assert_eq!(client.session.session_id, server.session.session_id);
        assert_eq!(client.session.protocol, PROTOCOL_VERSION);
        assert_eq!(client.session.role, Role::Node);
        assert_eq!(client.session.limits, server.session.limits);
        assert_eq!(client.session.features, server.session.features);
        assert_eq!(server.peer_label(), Some("camera"));
        assert_eq!(client.welcome.peer_role, Role::Node);
    }

    #[tokio::test]
    async fn a_bad_token_is_refused_typed() {
        let params = HandshakeParams::new(
            LocalIdentity::new(Role::Node),
            AuthToken::from_bytes([1; 32]),
        );
        let acceptor = Acceptor::new(token()).with_accepted_roles(RoleSet::NODES);

        let (client, server) = exchange(params, acceptor, FrameLimits::uds()).await;
        match client.unwrap_err() {
            TransportError::Refused(refused) => {
                assert_eq!(refused.reason, RefusalReason::BadAuth);
                assert_eq!(refused.max_protocol, PROTOCOL_VERSION);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(matches!(server.unwrap_err(), TransportError::Refused(_)));
    }

    #[tokio::test]
    async fn a_protocol_below_the_floor_is_refused_with_the_supported_range() {
        // A peer from before the protocol was frozen, speaking version 0.
        //
        // The greeting is built by hand rather than through `HandshakeParams`:
        // `Hello::new` always stamps this build's `PROTOCOL_VERSION`, and
        // `Acceptor::with_protocol_range` clamps its ceiling to the same value,
        // so an *older* peer is the only way to exercise the floor while
        // `PROTOCOL_VERSION` is 1.
        let (mut client, mut server) = duplex_pair(FrameLimits::network());
        let mut ancient = Hello::new(Role::Peer, token());
        ancient.protocol = 0;

        let acceptor = Acceptor::new(token()).with_accepted_roles(RoleSet::PEERS);
        let server_task = tokio::spawn(async move {
            accept(
                &mut server,
                &acceptor,
                SessionAssignment::Fresh(SessionId::from_u128(1)),
                TIMEOUT,
            )
            .await
        });

        client
            .send_message(&ControlRequest::Hello(ancient))
            .await
            .unwrap();
        let reply: ControlReply = client
            .expect_message(FrameKind::ControlReply)
            .await
            .unwrap();

        match reply {
            ControlReply::Refused(refused) => {
                assert!(
                    matches!(
                        refused.reason,
                        RefusalReason::ProtocolTooOld { peer: 0, .. }
                    ),
                    "expected a protocol refusal, got {:?}",
                    refused.reason
                );
                // The refusal names the range the peer should retry within.
                assert_eq!(refused.max_protocol, PROTOCOL_VERSION);
                assert!(refused.min_protocol <= refused.max_protocol);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }

        let err = server_task.await.unwrap().unwrap_err();
        assert!(matches!(err, TransportError::Refused(_)));
        assert!(!err.is_retryable(), "an old peer will still be old");
    }

    #[tokio::test]
    async fn the_protocol_range_never_claims_more_than_this_build_speaks() {
        // A misconfigured acceptor asking for protocols 4..=9 is clamped to
        // what exists, rather than refusing everybody forever.
        let acceptor = Acceptor::new(token())
            .with_accepted_roles(RoleSet::PEERS)
            .with_protocol_range(4, 9);
        assert_eq!(acceptor.max_protocol(), PROTOCOL_VERSION);
        assert!(acceptor.min_protocol() <= PROTOCOL_VERSION);

        let params = HandshakeParams::new(LocalIdentity::new(Role::Peer), token());
        let (client, _server) = exchange(params, acceptor, FrameLimits::network()).await;
        assert_eq!(client.unwrap().session.protocol, PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn the_wrong_role_is_refused_at_the_door() {
        let params = HandshakeParams::new(LocalIdentity::new(Role::Cli), token());
        let acceptor = Acceptor::new(token()).with_accepted_roles(RoleSet::NODES);

        let (client, _server) = exchange(params, acceptor, FrameLimits::uds()).await;
        match client.unwrap_err() {
            TransportError::Refused(refused) => assert!(matches!(
                refused.reason,
                RefusalReason::RoleNotPermitted { role: Role::Cli }
            )),
            other => panic!("expected a role refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_shutting_down_acceptor_refuses_retryably() {
        let params = HandshakeParams::new(LocalIdentity::new(Role::Peer), token());
        let acceptor = Acceptor::new(token())
            .with_accepted_roles(RoleSet::PEERS)
            .with_shutting_down(true);

        let (client, _server) = exchange(params, acceptor, FrameLimits::network()).await;
        let err = client.unwrap_err();
        assert!(err.is_retryable(), "a shutdown refusal is worth retrying");
    }

    #[tokio::test]
    async fn the_negotiated_ceiling_is_the_smaller_of_the_two() {
        let params = HandshakeParams::new(LocalIdentity::new(Role::Peer), token())
            .with_limits(NegotiatedLimits::network().with_max_payload_bytes(8 << 20));
        let acceptor = Acceptor::new(token())
            .with_accepted_roles(RoleSet::PEERS)
            .with_limits(NegotiatedLimits::network().with_max_payload_bytes(1 << 20));

        let (client, server) = exchange(params, acceptor, FrameLimits::network()).await;
        let client = client.unwrap();
        assert_eq!(client.session.limits.max_payload_bytes, 1 << 20);
        assert_eq!(server.unwrap().session.limits.max_payload_bytes, 1 << 20);
    }

    #[tokio::test]
    async fn both_directions_widen_after_the_greeting() {
        let target = FrameLimits::network().with_max_payload_bytes(4 << 20);
        let (mut client, mut server) = duplex_pair(target);

        let params = HandshakeParams::new(LocalIdentity::new(Role::Peer), token())
            .with_limits(NegotiatedLimits::from_frame_limits(&target));
        let acceptor = Acceptor::new(token())
            .with_accepted_roles(RoleSet::PEERS)
            .with_limits(NegotiatedLimits::from_frame_limits(&target));

        let server_task = tokio::spawn(async move {
            let outcome = accept(
                &mut server,
                &acceptor,
                SessionAssignment::Fresh(SessionId::from_u128(3)),
                TIMEOUT,
            )
            .await;
            (outcome, server)
        });
        let outcome = initiate(&mut client, &params, TIMEOUT).await.unwrap();
        let (server_outcome, server) = server_task.await.unwrap();
        server_outcome.unwrap();

        // Both ends left the narrow pre-handshake ceiling behind…
        assert_eq!(client.limits().max_payload_bytes(), 4 << 20);
        assert_eq!(server.limits().max_payload_bytes(), 4 << 20);
        // …and it matches what the session says.
        assert_eq!(outcome.session.frame_limits().max_payload_bytes(), 4 << 20);
    }

    #[tokio::test]
    async fn a_greeting_larger_than_the_pre_handshake_ceiling_is_refused() {
        // A label big enough to exceed the narrow ceiling proves the guard is
        // live *during* the handshake, not merely configured for after it.
        let (mut client, mut server) = duplex_pair(FrameLimits::network());
        let huge_label = "x".repeat(PRE_HANDSHAKE_MAX_PAYLOAD_BYTES + 1_024);
        let params = HandshakeParams::new(
            LocalIdentity::new(Role::Peer).with_label(huge_label),
            token(),
        );
        let acceptor = Acceptor::new(token()).with_accepted_roles(RoleSet::PEERS);

        let server_task = tokio::spawn(async move {
            accept(
                &mut server,
                &acceptor,
                SessionAssignment::Fresh(SessionId::from_u128(1)),
                Duration::from_millis(500),
            )
            .await
        });
        let client_result = initiate(&mut client, &params, Duration::from_millis(500)).await;
        let server_result = server_task.await.unwrap();

        // One end or the other must have refused it; neither may have accepted.
        assert!(
            client_result.is_err() || server_result.is_err(),
            "an oversize greeting must not complete a handshake"
        );
    }

    #[tokio::test]
    async fn a_non_greeting_first_frame_is_refused_and_reported() {
        let (mut client, mut server) = duplex_pair(FrameLimits::uds());
        let acceptor = Acceptor::new(token()).with_accepted_roles(RoleSet::NODES);

        let server_task = tokio::spawn(async move {
            accept(
                &mut server,
                &acceptor,
                SessionAssignment::Fresh(SessionId::from_u128(1)),
                TIMEOUT,
            )
            .await
        });
        client
            .send_message(&ControlRequest::List { all: false })
            .await
            .unwrap();

        let err = server_task.await.unwrap().unwrap_err();
        assert!(matches!(err, TransportError::Configuration(_)));

        // …and the peer was told why.
        let reply: ControlReply = client
            .expect_message(FrameKind::ControlReply)
            .await
            .unwrap();
        assert!(matches!(reply, ControlReply::Refused(_)));
    }

    #[tokio::test]
    async fn an_initiator_that_gets_a_stray_reply_reports_it() {
        let (mut client, mut server) = duplex_pair(FrameLimits::uds());
        let params = HandshakeParams::new(LocalIdentity::new(Role::Node), token());

        let server_task = tokio::spawn(async move {
            let _greeting: ControlRequest =
                server.expect_message(FrameKind::Control).await.unwrap();
            server.send_message(&ControlReply::Ok).await.unwrap();
            server
        });
        let err = initiate(&mut client, &params, TIMEOUT).await.unwrap_err();
        assert!(matches!(err, TransportError::Configuration(_)));
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn a_silent_peer_times_out() {
        let (mut client, _server) = duplex_pair(FrameLimits::uds());
        let params = HandshakeParams::new(LocalIdentity::new(Role::Node), token());
        let err = initiate(&mut client, &params, Duration::from_millis(50))
            .await
            .unwrap_err();
        match err {
            TransportError::Timeout { operation, .. } => assert_eq!(operation, "handshake"),
            other => panic!("expected a timeout, got {other:?}"),
        }
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn a_resume_request_is_visible_to_the_assignment_closure() {
        let resumed = SessionId::from_u128(0xfeed);
        let (mut client, mut server) = duplex_pair(FrameLimits::uds());
        let params = HandshakeParams::new(LocalIdentity::new(Role::Node), token())
            .with_resume(Some(resumed));
        let acceptor = Acceptor::new(token()).with_accepted_roles(RoleSet::NODES);

        let server_task = tokio::spawn(async move {
            accept_with(&mut server, &acceptor, TIMEOUT, |hello| {
                assert_eq!(hello.resume, Some(resumed));
                SessionAssignment::Resumed(resumed)
            })
            .await
        });
        let outcome = initiate(&mut client, &params, TIMEOUT).await.unwrap();
        let accepted = server_task.await.unwrap().unwrap();

        assert!(outcome.session.resumed);
        assert_eq!(outcome.session.session_id, resumed);
        assert!(accepted.session.resumed);
    }

    #[tokio::test]
    async fn an_unresumable_session_is_refused() {
        let (mut client, mut server) = duplex_pair(FrameLimits::uds());
        let params = HandshakeParams::new(LocalIdentity::new(Role::Node), token())
            .with_resume(Some(SessionId::from_u128(1)));
        let acceptor = Acceptor::new(token()).with_accepted_roles(RoleSet::NODES);

        let server_task = tokio::spawn(async move {
            accept_with(&mut server, &acceptor, TIMEOUT, |_| {
                SessionAssignment::ResumeUnavailable
            })
            .await
        });
        let err = initiate(&mut client, &params, TIMEOUT).await.unwrap_err();
        match err {
            TransportError::Refused(refused) => assert!(matches!(
                refused.reason,
                RefusalReason::UnknownSession { .. }
            )),
            other => panic!("expected an unknown-session refusal, got {other:?}"),
        }
        assert!(server_task.await.unwrap().is_err());
    }

    #[test]
    fn parameters_carry_the_config_policy() {
        let params = HandshakeParams::from_config(
            &TransportConfig::uds(),
            LocalIdentity::new(Role::Node).with_label("n"),
            token(),
            false,
        );
        assert!(!params.limits.require_crc);
        let hello = params.to_hello();
        assert_eq!(hello.role, Role::Node);
        assert_eq!(hello.label.as_deref(), Some("n"));
        assert_eq!(hello.resume, None);

        let network = HandshakeParams::from_config(
            &TransportConfig::new(),
            LocalIdentity::new(Role::Peer),
            token(),
            true,
        );
        assert!(network.limits.require_crc);
    }

    #[test]
    fn builders_reach_every_field() {
        let params = HandshakeParams::new(LocalIdentity::new(Role::Peer), token())
            .with_limits(NegotiatedLimits::uds())
            .with_features(FeatureFlags::TRACING)
            .with_resume(Some(SessionId::from_u128(5)));
        assert_eq!(params.features, FeatureFlags::TRACING);
        assert_eq!(params.resume, Some(SessionId::from_u128(5)));
        let hello = params.to_hello();
        assert_eq!(hello.features, FeatureFlags::TRACING);
        assert_eq!(hello.resume, Some(SessionId::from_u128(5)));
    }

    #[test]
    fn the_pre_handshake_ceiling_is_narrow_but_never_wider_than_the_target() {
        let wide = pre_handshake_limits(&FrameLimits::network());
        assert_eq!(
            wide.max_payload_bytes,
            PRE_HANDSHAKE_MAX_PAYLOAD_BYTES as u64
        );
        assert!(wide.require_crc);

        // A policy already narrower than the pre-handshake ceiling keeps its
        // own, tighter value.
        let tiny = FrameLimits::uds().with_max_payload_bytes(1_024);
        let narrow = pre_handshake_limits(&tiny);
        assert_eq!(narrow.max_payload_bytes, 1_024);
        assert!(!narrow.require_crc);
    }

    #[test]
    fn an_acceptor_inherits_the_connection_policy() {
        let acceptor =
            acceptor_from_config(&TransportConfig::uds(), token(), RoleSet::NODES, false);
        assert!(acceptor.accepts_role(Role::Node));
        assert!(!acceptor.accepts_role(Role::Peer));
        assert!(!acceptor.limits().require_crc);
        assert!(acceptor.requires_auth());
    }

    #[test]
    fn the_default_role_table_fails_closed() {
        assert_eq!(default_accepted_roles(Role::Cli), RoleSet::EMPTY);
        assert_eq!(default_accepted_roles(Role::Node), RoleSet::EMPTY);
        assert_eq!(default_accepted_roles(Role::Peer), RoleSet::PEERS);
        let daemon = default_accepted_roles(Role::Daemon);
        assert!(daemon.contains(Role::Node));
        assert!(daemon.contains(Role::Peer));
        assert!(!daemon.contains(Role::Cli));
    }

    #[test]
    fn a_refusal_helper_stamps_this_builds_range() {
        let refused = refuse(RefusalReason::ShuttingDown);
        assert_eq!(refused.max_protocol, PROTOCOL_VERSION);
        assert!(refused.reason.is_retryable());
    }
}
