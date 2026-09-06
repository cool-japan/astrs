//! The three handshake messages: [`Hello`], [`Welcome`], [`Refused`].
//!
//! Blueprint §7.2, in full:
//!
//! > First frame on every connection is `Hello { protocol: u16, astrs_version:
//! > semver, role: Cli|Daemon|Node|Peer, auth: Token, features: BitFlags }` →
//! > `Welcome { protocol, limits, session_id }`. Incompatible protocol → typed
//! > `Refused` with the highest mutually supported version.
//!
//! Two additions to that sketch, both forced by the same requirement — that
//! negotiation be *symmetric*, so both ends compute the same answer from the
//! same inputs:
//!
//! - `Hello` carries the initiator's own [`NegotiatedLimits`]. Without it the
//!   acceptor would be dictating rather than negotiating, and an embedded node
//!   with a 1 MiB budget could be handed a 64 MiB frame.
//! - `Welcome` echoes the agreed [`FeatureFlags`]. The initiator must learn
//!   which of the capabilities it offered were actually taken up; recomputing
//!   the intersection locally is impossible, because it never sees the
//!   acceptor's set.
//!
//! # The carrier frames
//!
//! The handshake is leg-independent, so it does not get a [`crate::FrameKind`]
//! of its own: `Hello` travels as [`crate::ControlRequest::Hello`] and the two
//! answers as [`crate::ControlReply::Welcome`] / [`crate::ControlReply::Refused`],
//! whichever leg is being opened. The `role` field is what tells the acceptor
//! which family will follow once the handshake completes.

use core::fmt;

use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::auth::AuthToken;
use crate::handshake::features::FeatureFlags;
use crate::handshake::limits::NegotiatedLimits;
use crate::handshake::role::Role;
use crate::ids::SessionId;
use crate::version::{AstrsVersion, MIN_SUPPORTED_PROTOCOL, PROTOCOL_VERSION};

/// The first message on every connection (§7.2).
///
/// # Examples
///
/// ```
/// use astrs_wire::{AuthToken, FeatureFlags, Hello, Role, PROTOCOL_VERSION};
///
/// let hello = Hello::new(Role::Node, AuthToken::ZERO)
///     .with_features(FeatureFlags::SHM_ZERO_COPY);
///
/// assert_eq!(hello.protocol, PROTOCOL_VERSION);
/// assert_eq!(hello.role, Role::Node);
/// assert!(hello.features.contains(FeatureFlags::SHM_ZERO_COPY));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct Hello {
    /// The highest protocol version the initiator speaks.
    pub protocol: u16,
    /// The initiator's AstRS release, for diagnostics and compatibility
    /// warnings. Never used to gate the connection — that is `protocol`'s job.
    pub astrs_version: AstrsVersion,
    /// What kind of endpoint is calling, and therefore which message family
    /// will follow.
    pub role: Role,
    /// The cluster auth token (§16), compared in constant time by the
    /// acceptor.
    pub auth: AuthToken,
    /// The optional capabilities the initiator offers.
    pub features: FeatureFlags,
    /// The budget the initiator is willing to work within.
    pub limits: NegotiatedLimits,
    /// A session to resume, for a reconnecting daemon that wants its state
    /// catch-up log rather than a cold start (§24.1 `StateCatchUp`).
    pub resume: Option<SessionId>,
    /// A human-readable name for the initiator (`camera`, `robot-01`), used in
    /// logs and `astrs list`. Never used for authorisation.
    pub label: Option<String>,
}

impl Hello {
    /// The greeting this build sends: current protocol, current version, the
    /// blueprint's default limits, no features.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{AuthToken, Hello, Role, PROTOCOL_VERSION};
    ///
    /// let hello = Hello::new(Role::Cli, AuthToken::ZERO);
    /// assert_eq!(hello.protocol, PROTOCOL_VERSION);
    /// assert!(hello.resume.is_none());
    /// ```
    #[must_use]
    pub fn new(role: Role, auth: AuthToken) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            astrs_version: AstrsVersion::current(),
            role,
            auth,
            features: FeatureFlags::EMPTY,
            limits: NegotiatedLimits::new(),
            resume: None,
            label: None,
        }
    }

    /// Offers a feature set.
    #[must_use]
    pub const fn with_features(mut self, features: FeatureFlags) -> Self {
        self.features = features;
        self
    }

    /// Proposes a budget.
    #[must_use]
    pub const fn with_limits(mut self, limits: NegotiatedLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Asks to resume an existing session.
    #[must_use]
    pub const fn with_resume(mut self, session: SessionId) -> Self {
        self.resume = Some(session);
        self
    }

    /// Attaches a human-readable label.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Overrides the protocol version — for a client deliberately speaking an
    /// older protocol, and for tests of the refusal path.
    #[must_use]
    pub const fn with_protocol(mut self, protocol: u16) -> Self {
        self.protocol = protocol;
        self
    }

    /// Whether this greeting asks to resume a session.
    #[must_use]
    pub const fn is_resume(&self) -> bool {
        self.resume.is_some()
    }

    /// The label if one was given, otherwise the role name.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{AuthToken, Hello, Role};
    ///
    /// assert_eq!(Hello::new(Role::Daemon, AuthToken::ZERO).display_name(), "daemon");
    /// assert_eq!(
    ///     Hello::new(Role::Daemon, AuthToken::ZERO).with_label("robot-01").display_name(),
    ///     "robot-01"
    /// );
    /// ```
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.label.as_deref().unwrap_or_else(|| self.role.as_str())
    }
}

impl fmt::Display for Hello {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "hello from {} ({}, protocol {}, astrs {}, features {})",
            self.display_name(),
            self.role,
            self.protocol,
            self.astrs_version,
            self.features
        )
    }
}

/// The acceptance of a [`Hello`] (§7.2).
///
/// # Examples
///
/// ```
/// use astrs_wire::{FeatureFlags, NegotiatedLimits, Role, SessionId, Welcome};
///
/// let welcome = Welcome::new(
///     1,
///     NegotiatedLimits::network(),
///     SessionId::from_u128(1),
///     Role::Daemon,
/// );
/// assert_eq!(welcome.protocol, 1);
/// assert_eq!(welcome.features, FeatureFlags::EMPTY);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct Welcome {
    /// The protocol version both ends will use — never above the initiator's.
    pub protocol: u16,
    /// The agreed budget: the stricter of the two proposals, field by field.
    pub limits: NegotiatedLimits,
    /// The session this connection belongs to. A reconnecting peer that asked
    /// to resume gets the same id back; anything else gets a fresh one.
    pub session_id: SessionId,
    /// The features both ends offered — the intersection, never a superset of
    /// what the initiator asked for.
    pub features: FeatureFlags,
    /// The role the acceptor recognised this connection as — always an echo
    /// of [`Hello::role`].
    ///
    /// An initiator that dialled the wrong socket finds out here rather than
    /// after its first request: an acceptor that does not serve the role it
    /// was offered refuses with
    /// [`RefusalReason::RoleNotPermitted`] instead of echoing it.
    pub peer_role: Role,
    /// The acceptor's AstRS release, for diagnostics.
    pub astrs_version: AstrsVersion,
    /// Whether the session was resumed rather than created. A daemon that sees
    /// `false` after asking to resume must replay its state from scratch.
    pub resumed: bool,
}

impl Welcome {
    /// A welcome with no features and a fresh session.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{NegotiatedLimits, Role, SessionId, Welcome};
    ///
    /// let welcome = Welcome::new(1, NegotiatedLimits::uds(), SessionId::NIL, Role::Daemon);
    /// assert!(!welcome.resumed);
    /// ```
    #[must_use]
    pub fn new(
        protocol: u16,
        limits: NegotiatedLimits,
        session_id: SessionId,
        peer_role: Role,
    ) -> Self {
        Self {
            protocol,
            limits,
            session_id,
            features: FeatureFlags::EMPTY,
            peer_role,
            astrs_version: AstrsVersion::current(),
            resumed: false,
        }
    }

    /// Records the agreed feature set.
    #[must_use]
    pub const fn with_features(mut self, features: FeatureFlags) -> Self {
        self.features = features;
        self
    }

    /// Marks the session as resumed.
    #[must_use]
    pub const fn with_resumed(mut self, resumed: bool) -> Self {
        self.resumed = resumed;
        self
    }

    /// Whether the agreed protocol is one this build can actually speak.
    ///
    /// A peer that answers with a protocol *above* what it was offered is
    /// malfunctioning; this is the check that catches it.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{NegotiatedLimits, Role, SessionId, Welcome};
    ///
    /// let welcome = Welcome::new(1, NegotiatedLimits::uds(), SessionId::NIL, Role::Daemon);
    /// assert!(welcome.protocol_is_supported());
    /// ```
    #[must_use]
    pub const fn protocol_is_supported(&self) -> bool {
        self.protocol >= MIN_SUPPORTED_PROTOCOL && self.protocol <= PROTOCOL_VERSION
    }
}

impl fmt::Display for Welcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "welcome to session {} as {} (protocol {}, features {}{})",
            self.session_id,
            self.peer_role,
            self.protocol,
            self.features,
            if self.resumed { ", resumed" } else { "" }
        )
    }
}

/// The refusal of a [`Hello`] (§7.2, §24.1 `ControlReply::Refused`).
///
/// A refusal always carries the acceptor's supported protocol *range*, not
/// merely the reason: the initiator's error message is the first thing an
/// operator reads when a rolling upgrade goes wrong, and "this daemon speaks
/// protocol 2–3, you speak 1" is the sentence that ends the investigation.
///
/// # Examples
///
/// ```
/// use astrs_wire::{RefusalReason, Refused};
///
/// let refused = Refused::protocol_too_old(1, 2, 3);
/// assert_eq!(refused.max_protocol, 3);
/// assert!(matches!(refused.reason, RefusalReason::ProtocolTooOld { .. }));
/// assert!(refused.to_string().contains("protocol"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct Refused {
    /// The highest protocol the acceptor speaks.
    pub max_protocol: u16,
    /// The lowest protocol the acceptor still accepts.
    pub min_protocol: u16,
    /// Why the connection was refused.
    pub reason: RefusalReason,
}

impl Refused {
    /// A refusal with the given reason and this build's protocol range.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{RefusalReason, Refused, PROTOCOL_VERSION};
    ///
    /// let refused = Refused::new(RefusalReason::BadAuth);
    /// assert_eq!(refused.max_protocol, PROTOCOL_VERSION);
    /// ```
    #[must_use]
    pub const fn new(reason: RefusalReason) -> Self {
        Self {
            max_protocol: PROTOCOL_VERSION,
            min_protocol: MIN_SUPPORTED_PROTOCOL,
            reason,
        }
    }

    /// A refusal for an initiator whose protocol is below the acceptor's floor.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::Refused;
    ///
    /// let refused = Refused::protocol_too_old(1, 2, 4);
    /// assert_eq!(refused.min_protocol, 2);
    /// ```
    #[must_use]
    pub const fn protocol_too_old(peer: u16, min_protocol: u16, max_protocol: u16) -> Self {
        Self {
            max_protocol,
            min_protocol,
            reason: RefusalReason::ProtocolTooOld {
                peer,
                minimum: min_protocol,
            },
        }
    }

    /// Whether retrying with a different protocol version could succeed.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{RefusalReason, Refused};
    ///
    /// assert!(Refused::protocol_too_old(1, 2, 3).is_version_problem());
    /// assert!(!Refused::new(RefusalReason::BadAuth).is_version_problem());
    /// ```
    #[must_use]
    pub const fn is_version_problem(&self) -> bool {
        matches!(self.reason, RefusalReason::ProtocolTooOld { .. })
    }

    /// The protocol version an initiator should retry with, if any.
    ///
    /// Returns `None` when no version could work — because the refusal was not
    /// about versions, or because the acceptor's whole range sits above what
    /// this build speaks.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Refused, PROTOCOL_VERSION};
    ///
    /// // The acceptor is newer but still accepts what we speak.
    /// let refused = Refused::protocol_too_old(0, PROTOCOL_VERSION, PROTOCOL_VERSION + 3);
    /// assert_eq!(refused.retry_protocol(), Some(PROTOCOL_VERSION));
    ///
    /// // The acceptor dropped support for everything we know.
    /// let hopeless = Refused::protocol_too_old(1, PROTOCOL_VERSION + 1, PROTOCOL_VERSION + 2);
    /// assert_eq!(hopeless.retry_protocol(), None);
    /// ```
    #[must_use]
    pub const fn retry_protocol(&self) -> Option<u16> {
        if !self.is_version_problem() {
            return None;
        }
        // The best we can do is the highest version both sides know: capped by
        // what we speak, floored by what they still accept.
        let candidate = if self.max_protocol < PROTOCOL_VERSION {
            self.max_protocol
        } else {
            PROTOCOL_VERSION
        };
        if candidate < self.min_protocol || candidate < MIN_SUPPORTED_PROTOCOL {
            None
        } else {
            Some(candidate)
        }
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (peer speaks protocol {}–{})",
            self.reason, self.min_protocol, self.max_protocol
        )
    }
}

/// Why a connection was refused.
///
/// # Examples
///
/// ```
/// use astrs_wire::RefusalReason;
///
/// assert!(RefusalReason::TooManyConnections { limit: 8 }.is_retryable());
/// assert!(!RefusalReason::BadAuth.is_retryable());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RefusalReason {
    /// The initiator speaks a protocol older than the acceptor's floor.
    #[oxicode(variant = 0)]
    ProtocolTooOld {
        /// The version the initiator offered.
        peer: u16,
        /// The oldest version the acceptor accepts.
        minimum: u16,
    },
    /// The auth token did not match (§16).
    ///
    /// Deliberately carries no detail: a refusal that explains *how* the token
    /// was wrong is an oracle.
    #[oxicode(variant = 1)]
    BadAuth,
    /// This endpoint does not accept connections from that role — a node
    /// dialling the coordinator's port, say.
    #[oxicode(variant = 2)]
    RoleNotPermitted {
        /// The role that was refused.
        role: Role,
    },
    /// The acceptor is already at its connection ceiling.
    #[oxicode(variant = 3)]
    TooManyConnections {
        /// The ceiling.
        limit: u32,
    },
    /// The acceptor is shutting down.
    #[oxicode(variant = 4)]
    ShuttingDown,
    /// The session the initiator asked to resume is unknown or expired. The
    /// initiator should retry without `resume`.
    #[oxicode(variant = 5)]
    UnknownSession {
        /// The session that could not be resumed.
        session: SessionId,
    },
    /// The initiator's proposed limits cannot be met — a payload ceiling below
    /// what the acceptor needs for its own control messages, for instance.
    #[oxicode(variant = 6)]
    LimitsUnacceptable {
        /// What was wrong with them.
        message: String,
    },
    /// The acceptor failed for a reason of its own.
    #[oxicode(variant = 7)]
    Internal {
        /// A human-readable explanation.
        message: String,
    },
}

impl RefusalReason {
    /// Whether an identical retry could plausibly succeed later.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::TooManyConnections { .. } | Self::ShuttingDown | Self::Internal { .. }
        )
    }

    /// A stable, lower-case name for logs and metrics labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::ProtocolTooOld { .. } => "protocol_too_old",
            Self::BadAuth => "bad_auth",
            Self::RoleNotPermitted { .. } => "role_not_permitted",
            Self::TooManyConnections { .. } => "too_many_connections",
            Self::ShuttingDown => "shutting_down",
            Self::UnknownSession { .. } => "unknown_session",
            Self::LimitsUnacceptable { .. } => "limits_unacceptable",
            Self::Internal { .. } => "internal",
        }
    }
}

impl fmt::Display for RefusalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProtocolTooOld { peer, minimum } => write!(
                f,
                "protocol {peer} is below the minimum supported protocol {minimum}"
            ),
            Self::BadAuth => f.write_str("authentication failed"),
            Self::RoleNotPermitted { role } => write!(f, "role {role} is not accepted here"),
            Self::TooManyConnections { limit } => {
                write!(f, "connection limit of {limit} reached")
            }
            Self::ShuttingDown => f.write_str("peer is shutting down"),
            Self::UnknownSession { session } => write!(f, "session {session} cannot be resumed"),
            Self::LimitsUnacceptable { message } => write!(f, "limits unacceptable: {message}"),
            Self::Internal { message } => write!(f, "internal error: {message}"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireEncode, round_trip};

    fn hello() -> Hello {
        Hello::new(Role::Node, AuthToken::from_bytes([7; 32]))
            .with_features(FeatureFlags::SHM_ZERO_COPY | FeatureFlags::TRACING)
            .with_limits(NegotiatedLimits::uds().with_max_payload_bytes(1 << 20))
            .with_label("camera")
    }

    #[test]
    fn hello_defaults_to_this_builds_protocol_and_version() {
        let hello = Hello::new(Role::Cli, AuthToken::ZERO);
        assert_eq!(hello.protocol, PROTOCOL_VERSION);
        assert_eq!(hello.astrs_version, AstrsVersion::current());
        assert_eq!(hello.features, FeatureFlags::EMPTY);
        assert_eq!(hello.limits, NegotiatedLimits::new());
        assert!(!hello.is_resume());
        assert_eq!(hello.display_name(), "cli");
    }

    #[test]
    fn hello_builders_set_exactly_what_they_name() {
        let session = SessionId::from_u128(9);
        let hello = hello().with_resume(session).with_protocol(3);
        assert_eq!(hello.protocol, 3);
        assert_eq!(hello.resume, Some(session));
        assert!(hello.is_resume());
        assert_eq!(hello.display_name(), "camera");
        assert_eq!(hello.role, Role::Node);
    }

    #[test]
    fn every_handshake_message_round_trips() {
        let hello = hello();
        assert_eq!(round_trip(&hello).unwrap(), hello);

        let welcome = Welcome::new(
            1,
            NegotiatedLimits::network(),
            SessionId::from_u128(4),
            Role::Daemon,
        )
        .with_features(FeatureFlags::SHM_ZERO_COPY)
        .with_resumed(true);
        assert_eq!(round_trip(&welcome).unwrap(), welcome);

        let refused = Refused::protocol_too_old(1, 2, 3);
        assert_eq!(round_trip(&refused).unwrap(), refused);
    }

    #[test]
    fn a_hello_never_reveals_its_token_in_debug_or_display() {
        let hello = hello();
        let debug = format!("{hello:?}");
        let display = hello.to_string();
        let secret = AuthToken::from_bytes([7; 32]).reveal_hex();
        assert!(!debug.contains(&secret), "Debug leaked the token");
        assert!(!display.contains(&secret), "Display leaked the token");
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn refusal_reason_indices_are_frozen() {
        let reasons = [
            RefusalReason::ProtocolTooOld {
                peer: 1,
                minimum: 2,
            },
            RefusalReason::BadAuth,
            RefusalReason::RoleNotPermitted { role: Role::Node },
            RefusalReason::TooManyConnections { limit: 4 },
            RefusalReason::ShuttingDown,
            RefusalReason::UnknownSession {
                session: SessionId::from_u128(2),
            },
            RefusalReason::LimitsUnacceptable {
                message: "too small".to_owned(),
            },
            RefusalReason::Internal {
                message: "boom".to_owned(),
            },
        ];
        for (index, reason) in reasons.into_iter().enumerate() {
            let bytes = reason.encode_to_vec().unwrap();
            assert_eq!(usize::from(bytes[0]), index, "{reason:?} moved");
            assert_eq!(round_trip(&reason).unwrap(), reason);
            assert!(!reason.kind_name().is_empty());
            assert!(!reason.to_string().is_empty());
        }
    }

    #[test]
    fn only_transient_refusals_are_retryable() {
        assert!(RefusalReason::ShuttingDown.is_retryable());
        assert!(RefusalReason::TooManyConnections { limit: 1 }.is_retryable());
        assert!(
            RefusalReason::Internal {
                message: String::new()
            }
            .is_retryable()
        );
        assert!(!RefusalReason::BadAuth.is_retryable());
        assert!(
            !RefusalReason::ProtocolTooOld {
                peer: 1,
                minimum: 2
            }
            .is_retryable()
        );
    }

    #[test]
    fn a_bad_auth_refusal_carries_no_detail() {
        // The variant is a unit: there is nothing an attacker can learn from
        // it beyond "wrong".
        let refused = Refused::new(RefusalReason::BadAuth);
        assert_eq!(refused.reason, RefusalReason::BadAuth);
        assert_eq!(refused.max_protocol, PROTOCOL_VERSION);
        assert_eq!(refused.min_protocol, MIN_SUPPORTED_PROTOCOL);
        assert!(!refused.is_version_problem());
        assert_eq!(refused.retry_protocol(), None);
    }

    #[test]
    fn retry_protocol_picks_the_highest_mutually_known_version() {
        // Peer is newer, still accepts ours.
        let refused = Refused::protocol_too_old(0, PROTOCOL_VERSION, PROTOCOL_VERSION + 5);
        assert_eq!(refused.retry_protocol(), Some(PROTOCOL_VERSION));

        // Peer is older: meet at the peer's ceiling, if we still speak it.
        let older = Refused {
            max_protocol: PROTOCOL_VERSION,
            min_protocol: MIN_SUPPORTED_PROTOCOL,
            reason: RefusalReason::ProtocolTooOld {
                peer: PROTOCOL_VERSION,
                minimum: MIN_SUPPORTED_PROTOCOL,
            },
        };
        assert_eq!(older.retry_protocol(), Some(PROTOCOL_VERSION));

        // Peer dropped every version we know.
        let hopeless = Refused::protocol_too_old(1, PROTOCOL_VERSION + 1, PROTOCOL_VERSION + 2);
        assert_eq!(hopeless.retry_protocol(), None);
    }

    #[test]
    fn welcome_validates_the_protocol_it_reports() {
        let good = Welcome::new(
            PROTOCOL_VERSION,
            NegotiatedLimits::uds(),
            SessionId::NIL,
            Role::Daemon,
        );
        assert!(good.protocol_is_supported());

        let impossible = Welcome::new(
            PROTOCOL_VERSION + 1,
            NegotiatedLimits::uds(),
            SessionId::NIL,
            Role::Daemon,
        );
        assert!(!impossible.protocol_is_supported());
    }

    #[test]
    fn display_forms_are_informative() {
        assert!(hello().to_string().contains("camera"));
        let welcome = Welcome::new(
            1,
            NegotiatedLimits::uds(),
            SessionId::from_u128(1),
            Role::Daemon,
        )
        .with_resumed(true);
        assert!(welcome.to_string().contains("resumed"));
        assert!(
            Refused::protocol_too_old(1, 2, 3)
                .to_string()
                .contains("2–3")
        );
    }

    #[test]
    fn serde_round_trips_the_handshake_for_diagnostics() {
        let json = serde_json::to_string(&hello()).unwrap();
        assert_eq!(serde_json::from_str::<Hello>(&json).unwrap(), hello());
    }
}
