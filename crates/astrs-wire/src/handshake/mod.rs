//! The handshake: one greeting, one answer, on every leg (blueprint §7.2).
//!
//! > First frame on every connection is `Hello { protocol, astrs_version, role,
//! > auth, features }` → `Welcome { protocol, limits, session_id }`.
//! > Incompatible protocol → typed `Refused` with the highest mutually
//! > supported version.
//!
//! | Module | Contents |
//! |---|---|
//! | [`role`] | [`Role`], [`RoleSet`] — who is calling, and who is served |
//! | [`features`] | [`FeatureFlags`] — the optional capability bitmap |
//! | [`limits`] | [`NegotiatedLimits`] — the per-connection budget |
//! | [`messages`] | [`Hello`], [`Welcome`], [`Refused`], [`RefusalReason`] |
//! | [`mod@negotiate`] | [`Acceptor`], [`negotiate()`], [`accept_welcome`], [`NegotiatedSession`] |
//!
//! # One handshake, four legs
//!
//! The handshake does not have a [`crate::FrameKind`] of its own. It travels as
//! [`crate::ControlRequest::Hello`] and [`crate::ControlReply::Welcome`] /
//! [`crate::ControlReply::Refused`] on *every* leg — CLI↔coordinator,
//! daemon↔coordinator, node↔daemon and daemon↔daemon — and the [`Role`] in the
//! greeting is what tells the acceptor which family will follow. That is why
//! §7.2 gives `Hello` a role at all: one spec per concern (§3, principle 3),
//! not four near-identical greetings.
//!
//! # Negotiation is pure
//!
//! [`negotiate()`] is a function of a [`Hello`] and an [`Acceptor`]: no socket,
//! no clock, no session table. Anything stateful — minting a session id,
//! looking up whether a session can be resumed — is decided by the caller and
//! passed in as a [`SessionAssignment`]. Both ends can therefore run the same
//! logic and reach the same [`NegotiatedSession`], and every branch of it is
//! testable without a network.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{
//!     accept_welcome, negotiate, Acceptor, AuthToken, ControlReply, ControlRequest,
//!     FeatureFlags, FrameFlags, FrameLimits, HandshakeOutcome, Hello, Role, RoleSet,
//!     SessionAssignment, SessionId, WireMessage,
//! };
//!
//! let token = AuthToken::from_bytes([42; 32]);
//! let limits = FrameLimits::uds();
//!
//! // The node greets its daemon.
//! let hello = Hello::new(Role::Node, token.clone())
//!     .with_features(FeatureFlags::SHM_ZERO_COPY)
//!     .with_label("camera");
//! let request = ControlRequest::Hello(hello.clone());
//! let on_the_wire = request.to_frame(FrameFlags::EMPTY, &limits)?;
//!
//! // The daemon decodes it and negotiates.
//! let greeting = match ControlRequest::from_bytes(&on_the_wire, &limits)? {
//!     ControlRequest::Hello(hello) => hello,
//!     other => unreachable!("the first frame is always a greeting: {other:?}"),
//! };
//! let acceptor = Acceptor::new(token)
//!     .with_accepted_roles(RoleSet::NODES)
//!     .with_features(FeatureFlags::daemon_defaults());
//! let reply = match negotiate(
//!     &greeting,
//!     &acceptor,
//!     SessionAssignment::Fresh(SessionId::from_u128(7)),
//! ) {
//!     HandshakeOutcome::Accepted { welcome, .. } => ControlReply::Welcome(welcome),
//!     HandshakeOutcome::Refused(refused) => ControlReply::Refused(refused),
//! };
//!
//! // And the node checks the answer.
//! let welcome = match reply {
//!     ControlReply::Welcome(welcome) => welcome,
//!     other => unreachable!("expected a welcome: {other:?}"),
//! };
//! let session = accept_welcome(&hello, &welcome)?;
//! assert!(session.supports(FeatureFlags::SHM_ZERO_COPY));
//! assert_eq!(session.session_id, SessionId::from_u128(7));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod features;
pub mod limits;
pub mod messages;
pub mod negotiate;
pub mod role;

pub use features::FeatureFlags;
pub use limits::{
    DEFAULT_MAX_INFLIGHT_FRAMES, DEFAULT_MAX_ROUTES, DEFAULT_MAX_SUBSCRIPTIONS, NegotiatedLimits,
};
pub use messages::{Hello, RefusalReason, Refused, Welcome};
pub use negotiate::{
    Acceptor, HandshakeError, HandshakeOutcome, MIN_USABLE_PAYLOAD_BYTES, NegotiatedSession,
    SessionAssignment, accept_welcome, negotiate,
};
pub use role::{Role, RoleSet};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::auth::AuthToken;
    use crate::ids::SessionId;

    /// The whole exchange, end to end, on every leg.
    #[test]
    fn every_leg_completes_the_same_handshake() {
        let token = AuthToken::from_bytes([3; 32]);
        for (index, &role) in Role::ALL.iter().enumerate() {
            let hello = Hello::new(role, token.clone())
                .with_features(FeatureFlags::TRACING)
                .with_label(role.as_str());
            let acceptor = Acceptor::new(token.clone())
                .with_accepted_roles(RoleSet::only(role))
                .with_features(FeatureFlags::TRACING | FeatureFlags::QUIC);

            let session_id = SessionId::from_u128(index as u128 + 1);
            let outcome = negotiate(&hello, &acceptor, SessionAssignment::Fresh(session_id));
            let welcome = outcome.welcome().cloned().expect("accepted");

            let session = accept_welcome(&hello, &welcome).unwrap();
            assert_eq!(session.role, role);
            assert_eq!(session.session_id, session_id);
            assert_eq!(session.features, FeatureFlags::TRACING);
            assert!(!session.resumed);
        }
    }

    #[test]
    fn the_module_re_exports_the_whole_surface() {
        // A compile-time check that the public surface stays complete.
        fn assert_exists<T>() {}
        assert_exists::<Hello>();
        assert_exists::<Welcome>();
        assert_exists::<Refused>();
        assert_exists::<RefusalReason>();
        assert_exists::<Role>();
        assert_exists::<RoleSet>();
        assert_exists::<FeatureFlags>();
        assert_exists::<NegotiatedLimits>();
        assert_exists::<Acceptor>();
        assert_exists::<NegotiatedSession>();
        assert_exists::<HandshakeOutcome>();
        assert_exists::<HandshakeError>();
        assert_exists::<SessionAssignment>();
        const {
            assert!(MIN_USABLE_PAYLOAD_BYTES > 0);
            assert!(DEFAULT_MAX_ROUTES > 0);
            assert!(DEFAULT_MAX_INFLIGHT_FRAMES > 0);
            assert!(DEFAULT_MAX_SUBSCRIPTIONS > 0);
        }
    }
}
