//! Version, feature and limit negotiation — pure, testable, no I/O.
//!
//! Blueprint §7.2 requires that an incompatible protocol produce a *typed*
//! refusal carrying the highest mutually supported version, rather than a
//! dropped connection or a generic error string. That decision — accept or
//! refuse, and on what terms — is a pure function of two greetings, so it lives
//! here as one: [`negotiate`] takes a [`Hello`] and an [`Acceptor`] and returns
//! a [`HandshakeOutcome`]. No socket, no clock, no allocation beyond the
//! message it builds.
//!
//! The initiator runs the mirror check with [`accept_welcome`], which refuses a
//! `Welcome` that grants more than was offered — a peer must not be able to
//! talk a small device into buffering a frame it cannot hold.
//!
//! Both sides end up with the same [`NegotiatedSession`], which is the value
//! the rest of the stack reads: it carries the agreed protocol, features,
//! limits and session id.
//!
//! ```text
//!   initiator                                   acceptor
//!      │  Hello { protocol, role, auth, ... }      │
//!      ├──────────────────────────────────────────►│  negotiate(&hello, &acceptor, …)
//!      │            Welcome { protocol, limits }   │
//!      │◄──────────────────────────────────────────┤
//!   accept_welcome(&hello, &welcome)               │
//!      │                                           │
//!      │            Refused { max_protocol, … }    │
//!      │◄──────────────────────────────────────────┤  (the other branch)
//! ```
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{
//!     accept_welcome, negotiate, Acceptor, AuthToken, FeatureFlags, HandshakeOutcome, Hello,
//!     Role, RoleSet, SessionAssignment, SessionId,
//! };
//!
//! let token = AuthToken::from_bytes([1; 32]);
//! let hello = Hello::new(Role::Node, token.clone())
//!     .with_features(FeatureFlags::SHM_ZERO_COPY | FeatureFlags::RECORDING);
//!
//! let acceptor = Acceptor::new(token)
//!     .with_accepted_roles(RoleSet::NODES)
//!     .with_features(FeatureFlags::SHM_ZERO_COPY | FeatureFlags::TRACING);
//!
//! let outcome = negotiate(
//!     &hello,
//!     &acceptor,
//!     SessionAssignment::Fresh(SessionId::from_u128(1)),
//! );
//!
//! let welcome = match outcome {
//!     HandshakeOutcome::Accepted { welcome, .. } => welcome,
//!     HandshakeOutcome::Refused(refused) => unreachable!("{refused}"),
//! };
//!
//! // Only the capability both ends offered survives.
//! assert_eq!(welcome.features, FeatureFlags::SHM_ZERO_COPY);
//!
//! // And the initiator reaches the same conclusion.
//! let session = accept_welcome(&hello, &welcome)?;
//! assert_eq!(session.features, FeatureFlags::SHM_ZERO_COPY);
//! # Ok::<(), astrs_wire::HandshakeError>(())
//! ```

use core::fmt;

use crate::auth::AuthToken;
use crate::frame::{Compression, FrameLimits};
use crate::handshake::features::FeatureFlags;
use crate::handshake::limits::NegotiatedLimits;
use crate::handshake::messages::{Hello, RefusalReason, Refused, Welcome};
use crate::handshake::role::{Role, RoleSet};
use crate::ids::SessionId;
use crate::version::{AstrsVersion, MIN_SUPPORTED_PROTOCOL, PROTOCOL_VERSION, negotiate_protocol};

/// The smallest payload ceiling a connection may agree to.
///
/// A peer that proposes less than this cannot carry the control messages the
/// protocol itself needs — a `NodeSpawnSpec` for a graph of any size, a state
/// catch-up batch — so the handshake refuses rather than opening a connection
/// that will fail on its first real message.
pub const MIN_USABLE_PAYLOAD_BYTES: u64 = 64 * 1024;

/// What session the acceptor is granting, decided by the caller.
///
/// Session bookkeeping is state, and [`negotiate`] is pure, so the caller looks
/// its session table up first and passes the answer in.
///
/// # Examples
///
/// ```
/// use astrs_wire::{SessionAssignment, SessionId};
///
/// let fresh = SessionAssignment::Fresh(SessionId::from_u128(1));
/// assert!(!fresh.is_resumed());
/// assert_eq!(fresh.session_id(), Some(SessionId::from_u128(1)));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionAssignment {
    /// A brand-new session with this id.
    Fresh(SessionId),
    /// The session the initiator asked to resume is available, and this is it.
    Resumed(SessionId),
    /// The initiator asked to resume a session this acceptor does not have.
    ///
    /// Negotiation refuses with [`RefusalReason::UnknownSession`]; the
    /// initiator is expected to retry without `resume`.
    ResumeUnavailable,
}

impl SessionAssignment {
    /// The granted session id, if one was granted.
    #[must_use]
    pub const fn session_id(&self) -> Option<SessionId> {
        match self {
            Self::Fresh(id) | Self::Resumed(id) => Some(*id),
            Self::ResumeUnavailable => None,
        }
    }

    /// Whether this assignment continues an existing session.
    #[must_use]
    pub const fn is_resumed(&self) -> bool {
        matches!(self, Self::Resumed(_))
    }
}

/// One end's negotiating position: what it accepts, offers and requires.
///
/// Held by a listener and reused for every incoming connection, so it is
/// `Clone` and carries no per-connection state beyond the counters the caller
/// updates.
///
/// # Examples
///
/// ```
/// use astrs_wire::{Acceptor, AuthToken, FeatureFlags, NegotiatedLimits, RoleSet};
///
/// let acceptor = Acceptor::new(AuthToken::from_bytes([9; 32]))
///     .with_accepted_roles(RoleSet::COORDINATOR)
///     .with_features(FeatureFlags::daemon_defaults())
///     .with_limits(NegotiatedLimits::network())
///     .with_connection_limit(Some(64));
///
/// assert!(acceptor.accepts_role(astrs_wire::Role::Cli));
/// assert!(!acceptor.accepts_role(astrs_wire::Role::Node));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acceptor {
    /// The cluster token an initiator must present (§16).
    expected_auth: AuthToken,
    /// Whether the token is checked at all. `false` only for a socket the
    /// operating system already protects (a UDS with peer-cred checks).
    require_auth: bool,
    /// The capabilities this end offers.
    features: FeatureFlags,
    /// The budget this end is willing to work within.
    limits: NegotiatedLimits,
    /// Which initiator roles this endpoint serves.
    accepted_roles: RoleSet,
    /// The concurrent-connection ceiling, if any.
    connection_limit: Option<u32>,
    /// How many connections are already open.
    connections_open: u32,
    /// Whether this end is shutting down and should refuse everything.
    shutting_down: bool,
    /// The oldest protocol this end accepts.
    min_protocol: u16,
    /// The newest protocol this end speaks.
    max_protocol: u16,
}

impl Acceptor {
    /// An acceptor that requires `expected_auth`, serves every role, offers no
    /// optional features and proposes the default limits.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Acceptor, AuthToken, PROTOCOL_VERSION};
    ///
    /// let acceptor = Acceptor::new(AuthToken::ZERO);
    /// assert_eq!(acceptor.max_protocol(), PROTOCOL_VERSION);
    /// assert!(acceptor.requires_auth());
    /// ```
    #[must_use]
    pub fn new(expected_auth: AuthToken) -> Self {
        Self {
            expected_auth,
            require_auth: true,
            features: FeatureFlags::EMPTY,
            limits: NegotiatedLimits::new(),
            accepted_roles: RoleSet::ALL,
            connection_limit: None,
            connections_open: 0,
            shutting_down: false,
            min_protocol: MIN_SUPPORTED_PROTOCOL,
            max_protocol: PROTOCOL_VERSION,
        }
    }

    /// An acceptor that performs no token check.
    ///
    /// Only appropriate where the operating system already authenticates the
    /// peer — a UDS in a 0700 runtime directory with a `SO_PEERCRED` check
    /// (§16). Never appropriate on a network socket.
    #[must_use]
    pub fn without_auth() -> Self {
        Self {
            require_auth: false,
            ..Self::new(AuthToken::ZERO)
        }
    }

    /// Sets the capabilities this end offers.
    #[must_use]
    pub const fn with_features(mut self, features: FeatureFlags) -> Self {
        self.features = features;
        self
    }

    /// Sets the budget this end proposes.
    #[must_use]
    pub const fn with_limits(mut self, limits: NegotiatedLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets which initiator roles this endpoint serves.
    #[must_use]
    pub const fn with_accepted_roles(mut self, roles: RoleSet) -> Self {
        self.accepted_roles = roles;
        self
    }

    /// Sets the concurrent-connection ceiling.
    #[must_use]
    pub const fn with_connection_limit(mut self, limit: Option<u32>) -> Self {
        self.connection_limit = limit;
        self
    }

    /// Sets how many connections are already open.
    #[must_use]
    pub const fn with_connections_open(mut self, open: u32) -> Self {
        self.connections_open = open;
        self
    }

    /// Marks this end as shutting down, so every greeting is refused.
    #[must_use]
    pub const fn with_shutting_down(mut self, shutting_down: bool) -> Self {
        self.shutting_down = shutting_down;
        self
    }

    /// Narrows the protocol range this end accepts.
    ///
    /// The range is clamped to what this build actually implements, so a
    /// misconfiguration cannot make an endpoint claim to speak a protocol that
    /// does not exist.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Acceptor, AuthToken, PROTOCOL_VERSION};
    ///
    /// let acceptor = Acceptor::new(AuthToken::ZERO).with_protocol_range(0, 9_999);
    /// assert_eq!(acceptor.max_protocol(), PROTOCOL_VERSION);
    /// ```
    #[must_use]
    pub const fn with_protocol_range(mut self, min: u16, max: u16) -> Self {
        self.min_protocol = if min < MIN_SUPPORTED_PROTOCOL {
            MIN_SUPPORTED_PROTOCOL
        } else {
            min
        };
        self.max_protocol = if max > PROTOCOL_VERSION {
            PROTOCOL_VERSION
        } else {
            max
        };
        if self.min_protocol > self.max_protocol {
            self.min_protocol = self.max_protocol;
        }
        self
    }

    /// The capabilities this end offers.
    #[must_use]
    pub const fn features(&self) -> FeatureFlags {
        self.features
    }

    /// The budget this end proposes.
    #[must_use]
    pub const fn limits(&self) -> NegotiatedLimits {
        self.limits
    }

    /// The roles this endpoint serves.
    #[must_use]
    pub const fn accepted_roles(&self) -> RoleSet {
        self.accepted_roles
    }

    /// Whether this endpoint serves `role`.
    #[must_use]
    pub const fn accepts_role(&self, role: Role) -> bool {
        self.accepted_roles.contains(role)
    }

    /// Whether a token check is performed.
    #[must_use]
    pub const fn requires_auth(&self) -> bool {
        self.require_auth
    }

    /// The oldest protocol this end accepts.
    #[must_use]
    pub const fn min_protocol(&self) -> u16 {
        self.min_protocol
    }

    /// The newest protocol this end speaks.
    #[must_use]
    pub const fn max_protocol(&self) -> u16 {
        self.max_protocol
    }

    /// Whether this end has room for another connection.
    #[must_use]
    pub const fn has_capacity(&self) -> bool {
        match self.connection_limit {
            Some(limit) => self.connections_open < limit,
            None => true,
        }
    }

    /// A refusal built with this end's protocol range.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Acceptor, AuthToken, RefusalReason};
    ///
    /// let refused = Acceptor::new(AuthToken::ZERO).refuse(RefusalReason::ShuttingDown);
    /// assert_eq!(refused.reason, RefusalReason::ShuttingDown);
    /// ```
    #[must_use]
    pub const fn refuse(&self, reason: RefusalReason) -> Refused {
        Refused {
            max_protocol: self.max_protocol,
            min_protocol: self.min_protocol,
            reason,
        }
    }

    /// Runs the negotiation for one greeting — see [`negotiate`].
    #[must_use]
    pub fn negotiate(&self, hello: &Hello, session: SessionAssignment) -> HandshakeOutcome {
        negotiate(hello, self, session)
    }
}

impl Default for Acceptor {
    /// An acceptor with the all-zero token, which no real deployment uses.
    ///
    /// It exists so `Acceptor` composes into `#[derive(Default)]` configs; a
    /// deployment always calls [`Acceptor::new`] with the cluster token.
    fn default() -> Self {
        Self::new(AuthToken::ZERO)
    }
}

/// What a connection agreed to, as both ends see it.
///
/// The acceptor gets one from [`negotiate`], the initiator from
/// [`accept_welcome`], and the two are equal field for field except
/// [`NegotiatedSession::peer_version`], which necessarily names the *other*
/// end.
///
/// # Examples
///
/// ```
/// use astrs_wire::{FeatureFlags, NegotiatedLimits, NegotiatedSession, Role, SessionId};
///
/// let session = NegotiatedSession {
///     protocol: 1,
///     session_id: SessionId::from_u128(2),
///     features: FeatureFlags::COMPRESSION_ZSTD,
///     limits: NegotiatedLimits::network(),
///     role: Role::Peer,
///     peer_version: Default::default(),
///     resumed: false,
/// };
/// assert!(session.supports(FeatureFlags::COMPRESSION_ZSTD));
/// assert!(session.frame_limits().require_crc());
/// ```
// Deliberately *not* `#[non_exhaustive]`: this is a local result type, never a
// wire type. The append-only rule (§3, principle 4) governs what crosses the
// wire; forcing every caller to write a wildcard arm — or forbidding them from
// building one in a test — would buy nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedSession {
    /// The protocol version both ends will use.
    pub protocol: u16,
    /// The session this connection belongs to.
    pub session_id: SessionId,
    /// The capabilities both ends offered.
    pub features: FeatureFlags,
    /// The budget both ends agreed to.
    pub limits: NegotiatedLimits,
    /// The role of the *initiating* end — the same value on both sides.
    pub role: Role,
    /// The other end's AstRS release.
    pub peer_version: AstrsVersion,
    /// Whether an existing session was resumed rather than created.
    pub resumed: bool,
}

impl NegotiatedSession {
    /// The frame policy this session implies.
    #[must_use]
    pub const fn frame_limits(&self) -> FrameLimits {
        self.limits.to_frame_limits()
    }

    /// Whether every capability in `features` is live on this connection.
    #[must_use]
    pub const fn supports(&self, features: FeatureFlags) -> bool {
        self.features.contains(features)
    }

    /// The best compression both ends support.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{
    ///     Compression, FeatureFlags, NegotiatedLimits, NegotiatedSession, Role, SessionId,
    /// };
    ///
    /// let session = NegotiatedSession {
    ///     protocol: 1,
    ///     session_id: SessionId::NIL,
    ///     features: FeatureFlags::COMPRESSION_LZ4,
    ///     limits: NegotiatedLimits::uds(),
    ///     role: Role::Node,
    ///     peer_version: Default::default(),
    ///     resumed: false,
    /// };
    /// assert_eq!(session.compression(), Compression::Lz4);
    /// ```
    #[must_use]
    pub const fn compression(&self) -> Compression {
        self.features.best_compression()
    }
}

impl fmt::Display for NegotiatedSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "session {} with a {} (protocol {}, astrs {}, features {})",
            self.session_id, self.role, self.protocol, self.peer_version, self.features
        )
    }
}

/// The result of negotiating one greeting.
///
/// # Examples
///
/// ```
/// use astrs_wire::{HandshakeOutcome, RefusalReason, Refused};
///
/// let outcome = HandshakeOutcome::Refused(Refused::new(RefusalReason::ShuttingDown));
/// assert!(!outcome.is_accepted());
/// assert!(outcome.welcome().is_none());
/// ```
// Not `#[non_exhaustive]`, for the reason given on [`NegotiatedSession`]:
// accept and refuse are the only two answers a handshake can have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeOutcome {
    /// The connection is open on these terms.
    Accepted {
        /// The message to send back to the initiator.
        welcome: Welcome,
        /// What the acceptor should remember about the connection.
        session: NegotiatedSession,
    },
    /// The connection is refused; send this and close.
    Refused(Refused),
}

impl HandshakeOutcome {
    /// Whether the connection was accepted.
    #[must_use]
    pub const fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted { .. })
    }

    /// The welcome to send, if the connection was accepted.
    #[must_use]
    pub const fn welcome(&self) -> Option<&Welcome> {
        match self {
            Self::Accepted { welcome, .. } => Some(welcome),
            Self::Refused(_) => None,
        }
    }

    /// The negotiated session, if the connection was accepted.
    #[must_use]
    pub const fn session(&self) -> Option<&NegotiatedSession> {
        match self {
            Self::Accepted { session, .. } => Some(session),
            Self::Refused(_) => None,
        }
    }

    /// The refusal to send, if the connection was refused.
    #[must_use]
    pub const fn refused(&self) -> Option<&Refused> {
        match self {
            Self::Accepted { .. } => None,
            Self::Refused(refused) => Some(refused),
        }
    }
}

/// Everything that can go wrong for the *initiator* after it sent a `Hello`.
///
/// The acceptor never produces one of these — its failures are typed
/// [`Refused`] messages, which is the whole point of §7.2.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HandshakeError {
    /// The peer refused the connection.
    #[error("connection refused: {0}")]
    Refused(Refused),
    /// The peer answered with a protocol this build does not implement.
    #[error("peer chose protocol {chosen}, which this build does not speak ({minimum}–{maximum})")]
    ProtocolNotSupported {
        /// What the peer chose.
        chosen: u16,
        /// The oldest protocol this build speaks.
        minimum: u16,
        /// The newest protocol this build speaks.
        maximum: u16,
    },
    /// The peer chose a protocol *above* the one offered, which no correct
    /// implementation does.
    #[error("peer chose protocol {chosen}, above the offered {offered}")]
    ProtocolAboveOffer {
        /// What the peer chose.
        chosen: u16,
        /// What the initiator offered.
        offered: u16,
    },
    /// The peer echoed a role other than the one offered.
    #[error("peer answered as a {echoed} connection, but a {offered} connection was opened")]
    RoleMismatch {
        /// The role the initiator sent.
        offered: Role,
        /// The role the peer echoed.
        echoed: Role,
    },
    /// The peer claimed capabilities the initiator never offered.
    #[error("peer enabled features that were not offered: {unexpected}")]
    FeaturesNotOffered {
        /// The bits the peer added.
        unexpected: FeatureFlags,
    },
    /// The peer granted a budget larger than the initiator offered.
    #[error("peer granted limits beyond what was offered ({granted} > {offered})")]
    LimitsExceedOffer {
        /// What the initiator offered.
        offered: Box<NegotiatedLimits>,
        /// What the peer granted.
        granted: Box<NegotiatedLimits>,
    },
    /// The peer resumed a session that was never requested.
    #[error("peer resumed session {session}, which was not requested")]
    UnexpectedResume {
        /// The session the peer claimed to resume.
        session: SessionId,
    },
}

impl HandshakeError {
    /// Whether retrying the connection could plausibly succeed.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{HandshakeError, RefusalReason, Refused};
    ///
    /// let busy = HandshakeError::Refused(Refused::new(RefusalReason::TooManyConnections {
    ///     limit: 4,
    /// }));
    /// assert!(busy.is_retryable());
    /// ```
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Refused(refused) => refused.reason.is_retryable(),
            _ => false,
        }
    }

    /// The refusal the peer sent, if the failure was a refusal.
    #[must_use]
    pub const fn refusal(&self) -> Option<&Refused> {
        match self {
            Self::Refused(refused) => Some(refused),
            _ => None,
        }
    }
}

/// Negotiates one greeting — the acceptor's half of §7.2.
///
/// The checks run in the order that leaks the least: a connection that is
/// refused for shutdown, protocol or authentication never reveals whether the
/// *other* checks would have passed.
///
/// 1. shutting down → [`RefusalReason::ShuttingDown`];
/// 2. protocol below the acceptor's floor → [`RefusalReason::ProtocolTooOld`]
///    with the acceptor's range;
/// 3. bad token (constant-time compare) → [`RefusalReason::BadAuth`];
/// 4. role not served here → [`RefusalReason::RoleNotPermitted`];
/// 5. no connection capacity → [`RefusalReason::TooManyConnections`];
/// 6. unresumable session → [`RefusalReason::UnknownSession`];
/// 7. limits that cannot carry the protocol →
///    [`RefusalReason::LimitsUnacceptable`];
/// 8. otherwise accepted, with the intersected features and limits.
///
/// # Examples
///
/// ```
/// use astrs_wire::{
///     negotiate, Acceptor, AuthToken, HandshakeOutcome, Hello, RefusalReason, Role, RoleSet,
///     SessionAssignment, SessionId,
/// };
///
/// let acceptor = Acceptor::new(AuthToken::from_bytes([1; 32]))
///     .with_accepted_roles(RoleSet::NODES);
///
/// // A CLI dialling the node socket is refused, not accepted-then-confused.
/// let outcome = negotiate(
///     &Hello::new(Role::Cli, AuthToken::from_bytes([1; 32])),
///     &acceptor,
///     SessionAssignment::Fresh(SessionId::from_u128(1)),
/// );
/// assert!(matches!(
///     outcome.refused().map(|refused| &refused.reason),
///     Some(RefusalReason::RoleNotPermitted { .. })
/// ));
/// ```
#[must_use]
pub fn negotiate(
    hello: &Hello,
    acceptor: &Acceptor,
    session: SessionAssignment,
) -> HandshakeOutcome {
    if acceptor.shutting_down {
        return HandshakeOutcome::Refused(acceptor.refuse(RefusalReason::ShuttingDown));
    }

    let protocol = match negotiate_protocol(hello.protocol) {
        Ok(protocol) => protocol.min(acceptor.max_protocol),
        Err(_) => {
            return HandshakeOutcome::Refused(acceptor.refuse(RefusalReason::ProtocolTooOld {
                peer: hello.protocol,
                minimum: acceptor.min_protocol,
            }));
        }
    };
    if protocol < acceptor.min_protocol {
        return HandshakeOutcome::Refused(acceptor.refuse(RefusalReason::ProtocolTooOld {
            peer: hello.protocol,
            minimum: acceptor.min_protocol,
        }));
    }

    if acceptor.require_auth && !acceptor.expected_auth.verify(&hello.auth) {
        return HandshakeOutcome::Refused(acceptor.refuse(RefusalReason::BadAuth));
    }

    if !acceptor.accepts_role(hello.role) {
        return HandshakeOutcome::Refused(
            acceptor.refuse(RefusalReason::RoleNotPermitted { role: hello.role }),
        );
    }

    if !acceptor.has_capacity() {
        let limit = acceptor.connection_limit.unwrap_or(u32::MAX);
        return HandshakeOutcome::Refused(
            acceptor.refuse(RefusalReason::TooManyConnections { limit }),
        );
    }

    let (session_id, resumed) = match (session, hello.resume) {
        (SessionAssignment::Resumed(id), Some(_)) => (id, true),
        // A session offered as "resumed" for a greeting that never asked to
        // resume is simply this connection's id.
        (SessionAssignment::Resumed(id) | SessionAssignment::Fresh(id), _) => (id, false),
        (SessionAssignment::ResumeUnavailable, Some(requested)) => {
            return HandshakeOutcome::Refused(
                acceptor.refuse(RefusalReason::UnknownSession { session: requested }),
            );
        }
        (SessionAssignment::ResumeUnavailable, None) => {
            return HandshakeOutcome::Refused(acceptor.refuse(RefusalReason::Internal {
                message: "no session was assigned to this connection".to_owned(),
            }));
        }
    };

    let limits = acceptor.limits.negotiate(&hello.limits);
    if limits.max_payload_bytes < MIN_USABLE_PAYLOAD_BYTES {
        return HandshakeOutcome::Refused(acceptor.refuse(RefusalReason::LimitsUnacceptable {
            message: format!(
                "an agreed payload ceiling of {} B is below the {MIN_USABLE_PAYLOAD_BYTES} B \
                 the protocol needs",
                limits.max_payload_bytes
            ),
        }));
    }

    let features = acceptor.features.negotiate(hello.features).known();

    let welcome = Welcome {
        protocol,
        limits,
        session_id,
        features,
        peer_role: hello.role,
        astrs_version: AstrsVersion::current(),
        resumed,
    };
    let session = NegotiatedSession {
        protocol,
        session_id,
        features,
        limits,
        role: hello.role,
        peer_version: hello.astrs_version.clone(),
        resumed,
    };
    HandshakeOutcome::Accepted { welcome, session }
}

/// Validates a [`Welcome`] against the [`Hello`] that produced it — the
/// initiator's half of §7.2.
///
/// A `Welcome` is not taken on trust. The acceptor may only *narrow* what was
/// offered: a lower protocol, a subset of the features, a stricter budget.
/// Anything else is a malfunctioning or hostile peer, and gets a typed error
/// rather than an oversized buffer.
///
/// # Errors
///
/// - [`HandshakeError::ProtocolAboveOffer`] / [`HandshakeError::ProtocolNotSupported`]
///   when the chosen protocol is not one both ends can speak.
/// - [`HandshakeError::RoleMismatch`] when the echoed role is not the one sent.
/// - [`HandshakeError::FeaturesNotOffered`] when the peer enabled a capability
///   that was never offered.
/// - [`HandshakeError::LimitsExceedOffer`] when the granted budget is larger
///   than the offered one.
/// - [`HandshakeError::UnexpectedResume`] when a session was resumed without
///   being asked for.
///
/// # Examples
///
/// ```
/// use astrs_wire::{
///     accept_welcome, AuthToken, FeatureFlags, HandshakeError, Hello, NegotiatedLimits, Role,
///     SessionId, Welcome,
/// };
///
/// let hello = Hello::new(Role::Node, AuthToken::ZERO);
/// let greedy = Welcome::new(
///     hello.protocol,
///     NegotiatedLimits::new(),
///     SessionId::from_u128(1),
///     Role::Node,
/// )
/// .with_features(FeatureFlags::QUIC);
///
/// assert!(matches!(
///     accept_welcome(&hello, &greedy),
///     Err(HandshakeError::FeaturesNotOffered { .. })
/// ));
/// ```
pub fn accept_welcome(
    hello: &Hello,
    welcome: &Welcome,
) -> Result<NegotiatedSession, HandshakeError> {
    if welcome.protocol > hello.protocol {
        return Err(HandshakeError::ProtocolAboveOffer {
            chosen: welcome.protocol,
            offered: hello.protocol,
        });
    }
    if !welcome.protocol_is_supported() {
        return Err(HandshakeError::ProtocolNotSupported {
            chosen: welcome.protocol,
            minimum: MIN_SUPPORTED_PROTOCOL,
            maximum: PROTOCOL_VERSION,
        });
    }
    if welcome.peer_role != hello.role {
        return Err(HandshakeError::RoleMismatch {
            offered: hello.role,
            echoed: welcome.peer_role,
        });
    }
    let unexpected = welcome.features.difference(hello.features);
    if !unexpected.is_empty() {
        return Err(HandshakeError::FeaturesNotOffered { unexpected });
    }
    if !hello.limits.permits(&welcome.limits) {
        return Err(HandshakeError::LimitsExceedOffer {
            offered: Box::new(hello.limits),
            granted: Box::new(welcome.limits),
        });
    }
    if welcome.resumed && hello.resume.is_none() {
        return Err(HandshakeError::UnexpectedResume {
            session: welcome.session_id,
        });
    }

    Ok(NegotiatedSession {
        protocol: welcome.protocol,
        session_id: welcome.session_id,
        features: welcome.features,
        limits: welcome.limits,
        role: hello.role,
        peer_version: welcome.astrs_version.clone(),
        resumed: welcome.resumed,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::common::DurationMs;

    fn token() -> AuthToken {
        AuthToken::from_bytes([0xAB; 32])
    }

    fn acceptor() -> Acceptor {
        Acceptor::new(token())
            .with_features(FeatureFlags::daemon_defaults())
            .with_limits(NegotiatedLimits::network())
            .with_accepted_roles(RoleSet::ALL)
    }

    fn hello() -> Hello {
        Hello::new(Role::Node, token())
            .with_features(FeatureFlags::SHM_ZERO_COPY | FeatureFlags::RECORDING)
    }

    fn fresh() -> SessionAssignment {
        SessionAssignment::Fresh(SessionId::from_u128(0x5E55))
    }

    #[test]
    fn a_good_greeting_is_accepted_on_intersected_terms() {
        let outcome = negotiate(&hello(), &acceptor(), fresh());
        let (welcome, session) = match outcome {
            HandshakeOutcome::Accepted { welcome, session } => (welcome, session),
            HandshakeOutcome::Refused(refused) => panic!("unexpected refusal: {refused}"),
        };

        assert_eq!(welcome.protocol, PROTOCOL_VERSION);
        assert_eq!(welcome.session_id, SessionId::from_u128(0x5E55));
        assert_eq!(welcome.peer_role, Role::Node);
        assert!(!welcome.resumed);

        // RECORDING was offered by the node but not by the acceptor.
        assert_eq!(welcome.features, FeatureFlags::SHM_ZERO_COPY);
        assert!(welcome.limits.require_crc, "the stricter policy wins");

        // Both ends agree.
        let mirrored = accept_welcome(&hello(), &welcome).unwrap();
        assert_eq!(mirrored.features, session.features);
        assert_eq!(mirrored.limits, session.limits);
        assert_eq!(mirrored.protocol, session.protocol);
        assert_eq!(mirrored.session_id, session.session_id);
        assert_eq!(mirrored.role, session.role);
    }

    #[test]
    fn negotiation_is_a_pure_function_of_its_inputs() {
        let first = negotiate(&hello(), &acceptor(), fresh());
        let second = negotiate(&hello(), &acceptor(), fresh());
        assert_eq!(first, second);
    }

    #[test]
    fn the_acceptor_method_and_the_free_function_agree() {
        let acceptor = acceptor();
        assert_eq!(
            acceptor.negotiate(&hello(), fresh()),
            negotiate(&hello(), &acceptor, fresh())
        );
    }

    #[test]
    fn a_bad_token_is_refused_without_detail() {
        let hello = Hello::new(Role::Node, AuthToken::from_bytes([0; 32]));
        let refused = negotiate(&hello, &acceptor(), fresh())
            .refused()
            .cloned()
            .expect("a bad token must be refused");
        assert_eq!(refused.reason, RefusalReason::BadAuth);
        assert_eq!(refused.max_protocol, PROTOCOL_VERSION);
    }

    #[test]
    fn an_unauthenticated_acceptor_skips_the_token_check() {
        let acceptor = Acceptor::without_auth();
        assert!(!acceptor.requires_auth());
        let hello = Hello::new(Role::Node, AuthToken::from_bytes([0x11; 32]));
        assert!(negotiate(&hello, &acceptor, fresh()).is_accepted());
    }

    #[test]
    fn a_protocol_below_the_floor_is_refused_with_the_range() {
        let hello = hello().with_protocol(0);
        let refused = negotiate(&hello, &acceptor(), fresh())
            .refused()
            .cloned()
            .expect("protocol 0 is not a release");
        assert!(refused.is_version_problem());
        assert_eq!(refused.min_protocol, MIN_SUPPORTED_PROTOCOL);
        assert_eq!(refused.max_protocol, PROTOCOL_VERSION);
        assert!(matches!(
            refused.reason,
            RefusalReason::ProtocolTooOld { peer: 0, .. }
        ));
    }

    #[test]
    fn a_newer_peer_is_met_at_this_builds_ceiling() {
        let hello = hello().with_protocol(u16::MAX);
        let welcome = negotiate(&hello, &acceptor(), fresh())
            .welcome()
            .cloned()
            .expect("a newer peer is compatible downwards");
        assert_eq!(welcome.protocol, PROTOCOL_VERSION);
    }

    #[test]
    fn an_acceptor_that_dropped_old_protocols_refuses_them() {
        let strict = acceptor().with_protocol_range(PROTOCOL_VERSION, PROTOCOL_VERSION);
        assert_eq!(strict.min_protocol(), PROTOCOL_VERSION);
        // A build that speaks only protocol 1 cannot express this case, so the
        // test asserts the clamping rather than a refusal that cannot happen.
        assert_eq!(strict.max_protocol(), PROTOCOL_VERSION);
        assert!(negotiate(&hello(), &strict, fresh()).is_accepted());
    }

    #[test]
    fn the_protocol_range_is_clamped_to_what_this_build_implements() {
        let absurd = acceptor().with_protocol_range(0, u16::MAX);
        assert_eq!(absurd.min_protocol(), MIN_SUPPORTED_PROTOCOL);
        assert_eq!(absurd.max_protocol(), PROTOCOL_VERSION);

        let inverted = acceptor().with_protocol_range(u16::MAX, MIN_SUPPORTED_PROTOCOL);
        assert!(inverted.min_protocol() <= inverted.max_protocol());
    }

    #[test]
    fn a_role_this_endpoint_does_not_serve_is_refused() {
        let node_socket = acceptor().with_accepted_roles(RoleSet::NODES);
        let refused = negotiate(&Hello::new(Role::Cli, token()), &node_socket, fresh())
            .refused()
            .cloned()
            .expect("a CLI on the node socket is a misdial");
        assert_eq!(
            refused.reason,
            RefusalReason::RoleNotPermitted { role: Role::Cli }
        );
        assert!(negotiate(&hello(), &node_socket, fresh()).is_accepted());
    }

    #[test]
    fn a_full_acceptor_refuses_with_its_limit() {
        let full = acceptor()
            .with_connection_limit(Some(2))
            .with_connections_open(2);
        assert!(!full.has_capacity());
        let refused = negotiate(&hello(), &full, fresh()).refused().cloned();
        assert_eq!(
            refused.map(|refused| refused.reason),
            Some(RefusalReason::TooManyConnections { limit: 2 })
        );
    }

    #[test]
    fn a_shutting_down_acceptor_refuses_everything_first() {
        // Even a greeting that would fail authentication is refused for
        // shutdown, so a draining daemon leaks nothing.
        let draining = acceptor().with_shutting_down(true);
        let bad_token = Hello::new(Role::Node, AuthToken::ZERO);
        assert_eq!(
            negotiate(&bad_token, &draining, fresh())
                .refused()
                .map(|refused| refused.reason.clone()),
            Some(RefusalReason::ShuttingDown)
        );
    }

    #[test]
    fn a_resumed_session_keeps_its_id() {
        let session = SessionId::from_u128(0xBEEF);
        let hello = hello().with_resume(session);
        let welcome = negotiate(&hello, &acceptor(), SessionAssignment::Resumed(session))
            .welcome()
            .cloned()
            .expect("a resumable session is accepted");
        assert_eq!(welcome.session_id, session);
        assert!(welcome.resumed);
        assert!(accept_welcome(&hello, &welcome).unwrap().resumed);
    }

    #[test]
    fn an_unresumable_session_is_refused_by_name() {
        let session = SessionId::from_u128(0xDEAD);
        let hello = hello().with_resume(session);
        let refused = negotiate(&hello, &acceptor(), SessionAssignment::ResumeUnavailable)
            .refused()
            .cloned()
            .expect("an unknown session cannot be resumed");
        assert_eq!(refused.reason, RefusalReason::UnknownSession { session });
    }

    #[test]
    fn a_resume_grant_for_a_cold_greeting_is_treated_as_fresh() {
        let session = SessionId::from_u128(1);
        let welcome = negotiate(&hello(), &acceptor(), SessionAssignment::Resumed(session))
            .welcome()
            .cloned()
            .expect("a cold greeting is still acceptable");
        assert_eq!(welcome.session_id, session);
        assert!(!welcome.resumed);
    }

    #[test]
    fn a_missing_session_assignment_is_an_internal_refusal() {
        let refused = negotiate(&hello(), &acceptor(), SessionAssignment::ResumeUnavailable)
            .refused()
            .cloned()
            .expect("a connection needs a session");
        assert!(matches!(refused.reason, RefusalReason::Internal { .. }));
    }

    #[test]
    fn limits_too_small_to_carry_the_protocol_are_refused() {
        let tiny = hello().with_limits(NegotiatedLimits::uds().with_max_payload_bytes(128));
        let refused = negotiate(&tiny, &acceptor(), fresh())
            .refused()
            .cloned()
            .expect("128 bytes cannot carry a spawn spec");
        assert!(matches!(
            refused.reason,
            RefusalReason::LimitsUnacceptable { .. }
        ));
    }

    #[test]
    fn the_agreed_limits_are_the_stricter_of_the_two() {
        let modest = hello().with_limits(
            NegotiatedLimits::uds()
                .with_max_payload_bytes(1 << 20)
                .with_heartbeat_interval(DurationMs::from_secs(1)),
        );
        let welcome = negotiate(&modest, &acceptor(), fresh())
            .welcome()
            .cloned()
            .expect("modest limits are acceptable");
        assert_eq!(welcome.limits.max_payload_bytes, 1 << 20);
        assert_eq!(welcome.limits.heartbeat_interval, DurationMs::from_secs(1));
        assert!(welcome.limits.require_crc);
        assert_eq!(
            welcome.limits.to_frame_limits().max_payload_bytes(),
            1 << 20
        );
    }

    #[test]
    fn unknown_feature_bits_never_become_live() {
        let futuristic = hello().with_features(FeatureFlags::from_bits(u64::MAX));
        let welcome = negotiate(&futuristic, &acceptor(), fresh())
            .welcome()
            .cloned()
            .expect("an unknown bit is ignored, not fatal");
        assert!(!welcome.features.has_unknown());
        assert_eq!(welcome.features, FeatureFlags::daemon_defaults());
    }

    #[test]
    fn the_initiator_rejects_a_welcome_that_grants_too_much() {
        let hello = hello();

        let above = Welcome::new(
            hello.protocol + 1,
            hello.limits,
            SessionId::from_u128(1),
            hello.role,
        );
        assert!(matches!(
            accept_welcome(&hello, &above),
            Err(HandshakeError::ProtocolAboveOffer { .. })
        ));

        let wrong_role = Welcome::new(hello.protocol, hello.limits, SessionId::NIL, Role::Cli);
        assert!(matches!(
            accept_welcome(&hello, &wrong_role),
            Err(HandshakeError::RoleMismatch { .. })
        ));

        let greedy_features =
            Welcome::new(hello.protocol, hello.limits, SessionId::NIL, hello.role)
                .with_features(FeatureFlags::QUIC);
        assert!(matches!(
            accept_welcome(&hello, &greedy_features),
            Err(HandshakeError::FeaturesNotOffered { .. })
        ));

        let greedy_limits = Welcome::new(
            hello.protocol,
            NegotiatedLimits::new().with_max_payload_bytes(u64::from(u32::MAX)),
            SessionId::NIL,
            hello.role,
        );
        let small = hello
            .clone()
            .with_limits(NegotiatedLimits::uds().with_max_payload_bytes(1 << 20));
        assert!(matches!(
            accept_welcome(&small, &greedy_limits),
            Err(HandshakeError::LimitsExceedOffer { .. })
        ));

        let surprise = Welcome::new(hello.protocol, hello.limits, SessionId::NIL, hello.role)
            .with_resumed(true);
        assert!(matches!(
            accept_welcome(&hello, &surprise),
            Err(HandshakeError::UnexpectedResume { .. })
        ));
    }

    #[test]
    fn the_initiator_rejects_a_protocol_it_cannot_speak() {
        let hello = hello().with_protocol(u16::MAX);
        let unspeakable = Welcome::new(
            PROTOCOL_VERSION + 1,
            hello.limits,
            SessionId::NIL,
            hello.role,
        );
        assert!(matches!(
            accept_welcome(&hello, &unspeakable),
            Err(HandshakeError::ProtocolNotSupported { .. })
        ));
    }

    #[test]
    fn handshake_errors_classify_themselves() {
        let refused = HandshakeError::Refused(Refused::new(RefusalReason::ShuttingDown));
        assert!(refused.is_retryable());
        assert!(refused.refusal().is_some());

        let mismatch = HandshakeError::RoleMismatch {
            offered: Role::Node,
            echoed: Role::Cli,
        };
        assert!(!mismatch.is_retryable());
        assert!(mismatch.refusal().is_none());
        assert!(!mismatch.to_string().is_empty());
    }

    #[test]
    fn session_assignment_reports_what_it_grants() {
        let id = SessionId::from_u128(3);
        assert_eq!(SessionAssignment::Fresh(id).session_id(), Some(id));
        assert!(!SessionAssignment::Fresh(id).is_resumed());
        assert!(SessionAssignment::Resumed(id).is_resumed());
        assert_eq!(SessionAssignment::ResumeUnavailable.session_id(), None);
    }

    #[test]
    fn the_negotiated_session_exposes_the_terms() {
        let session = negotiate(&hello(), &acceptor(), fresh())
            .session()
            .cloned()
            .expect("accepted");
        assert!(session.supports(FeatureFlags::SHM_ZERO_COPY));
        assert!(!session.supports(FeatureFlags::RECORDING));
        assert!(session.frame_limits().require_crc());
        assert_eq!(session.compression(), Compression::None);
        assert!(session.to_string().contains("session"));
    }

    #[test]
    fn compression_follows_the_negotiated_features() {
        let both = Acceptor::new(token())
            .with_features(FeatureFlags::COMPRESSION_LZ4 | FeatureFlags::COMPRESSION_ZSTD);
        let hello = Hello::new(Role::Peer, token()).with_features(FeatureFlags::COMPRESSION_LZ4);
        let session = negotiate(&hello, &both, fresh())
            .session()
            .cloned()
            .expect("accepted");
        assert_eq!(session.compression(), Compression::Lz4);
    }

    #[test]
    fn the_default_acceptor_is_the_zero_token_one() {
        assert_eq!(Acceptor::default(), Acceptor::new(AuthToken::ZERO));
        assert!(Acceptor::default().accepts_role(Role::Peer));
        assert_eq!(Acceptor::default().features(), FeatureFlags::EMPTY);
        assert_eq!(Acceptor::default().limits(), NegotiatedLimits::new());
        assert_eq!(Acceptor::default().accepted_roles(), RoleSet::ALL);
    }
}
