//! Read/mutate token scopes (blueprint §16, §22).
//!
//! > "Capability posture: coordinator API distinguishes read verbs
//! > (list/logs/topic) from mutating verbs (start/stop/param) — token
//! > scopes are 0.2; the enum split lands now so it's not a breaking
//! > change later." — blueprint §16
//!
//! [`astrs_wire::ControlRequest::scope`] is that enum split, already landed
//! and exhaustive (its own doc comment walks through why). This module is
//! the 0.2 half: a way to hand out a [`RequestScope::Read`]-only credential
//! without changing anything on the wire — no new `Hello` field, no new
//! frame, no new [`astrs_wire::ControlRequest`] variant, exactly as §22
//! asks for ("token scopes" listed as a non-breaking 0.2 addition).
//!
//! # Why derivation, not a second stored secret
//!
//! The obvious alternative — mint an independent random token and store it
//! next to the root one — works, but it adds state two processes must
//! agree on: the coordinator's configured root token and every `astrs
//! token mint --scope read` invocation would both need to read the *same*
//! second secret from *somewhere*, and a coordinator started from a config
//! that has not caught up on a rotation would refuse a token minted after
//! it started.
//!
//! Deriving the read-scope credential from the root token with HKDF (RFC
//! 5869) instead needs no second secret and no coordination: anyone who can
//! read the root token — an operator with filesystem access to
//! `.astrs-token`, exactly the bar blueprint §16 already sets for minting
//! *anything* — can compute the read-scope token locally with
//! [`derive_read_token`], and a coordinator configured with that same root
//! token always reaches the identical 32 bytes, with no restart or
//! redistribution step. The derivation is one-way: recovering `root` from
//! [`derive_read_token`]'s output is computationally infeasible (it is an
//! HKDF pseudorandom-function output, not an invertible transform), so a
//! read-scope credential holder gains nothing towards forging a mutate one.
//!
//! # Where this is actually checked
//!
//! Not here. [`AuthToken`] is exactly 32 bytes on the wire
//! (`astrs_wire::Hello::auth`), and `astrs_wire`'s handshake negotiation
//! checks it with one constant-time equality against one expected value —
//! by design, that crate is not this task's to extend (the 0.2 pull-forward
//! is explicit: token scopes land "WITHOUT wire-protocol enum changes"). This
//! crate's connection acceptor (`crate::server`) is where a presented token
//! is actually classified for real: it negotiates against a short list of
//! candidate tokens in order — the root token first, then (for a CLI
//! connection only) [`derive_read_token`]'s output — and keeps the first
//! [`astrs_wire::HandshakeOutcome::Accepted`] it gets, tagged with that
//! candidate's scope. A root-token holder, and anyone genuinely
//! unauthenticated, are unaffected either way: the former accepts on the
//! first candidate, and the latter's *first* refusal — the root candidate's
//! own — is what the connection actually receives, so a legacy client's
//! error path never changes shape. [`classify`] below is that same decision
//! restated as a plain function — not on the connection acceptor's own hot
//! path, but the single place tests and tooling (`astrs token mint`) can ask
//! "what scope is this token?" without a handshake.

use astrs_wire::{AuthToken, RequestScope};

#[cfg(test)]
use astrs_wire::AUTH_TOKEN_LEN;

/// HMAC-SHA256's message parameter, keyed by the root token (RFC 2104):
/// domain-separates this derivation from any other sub-token a future scope
/// might add from the same root. Versioned (`v1`) so an incompatible future
/// derivation scheme can pick a new label without colliding with tokens
/// already handed out under this one.
const READ_SCOPE_LABEL: &[u8] = b"astrs/token-scope/read/v1";

/// Derives the cluster's read-scope credential from its root token.
///
/// Computed as one HMAC-SHA256 application keyed by `root` over a fixed,
/// versioned label (`READ_SCOPE_LABEL`, private to this module) —
/// [`oxicrypto::hkdf_sha256_extract`]'s `(salt, ikm)`
/// parameters are exactly `(key, message)` in HMAC's own terms (RFC 5869
/// §2.2 defines `PRK = HMAC-Hash(salt, IKM)`), so passing `root`'s bytes as
/// `salt` and the label as `ikm` computes `HMAC-SHA256(root, LABEL)`
/// directly — a standard keyed-PRF subkey derivation, and (unlike a
/// two-step HKDF-Extract-then-Expand) one with no output-length ceiling to
/// violate, hence no `Result` to have a failure branch for. `root` already
/// carries 256 bits of entropy end to end (`AuthToken::generate`'s CSPRNG,
/// blueprint §16), so extract-only is exactly as sound here as running
/// Expand afterwards would be, per RFC 5869 §3.3's own guidance for a
/// single-block, high-entropy-IKM derivation.
///
/// Pure and deterministic: the same `root` always derives the same 32
/// bytes, on the coordinator and on every `astrs token mint --scope read`
/// invocation alike, with no state to distribute besides `root` itself (see
/// the module docs above for why that matters). One-way: recovering `root`
/// from this output is computationally infeasible (HMAC's own security
/// property), so a read-scope holder cannot forge a mutate credential.
///
/// # Examples
///
/// ```
/// use astrs_coordinator::auth::derive_read_token;
/// use astrs_wire::AuthToken;
///
/// let root = AuthToken::from_bytes([7; 32]);
/// let read = derive_read_token(&root);
/// assert_ne!(read, root, "the derived credential must differ from the root");
/// assert_eq!(read, derive_read_token(&root), "derivation is deterministic");
/// ```
#[must_use]
pub fn derive_read_token(root: &AuthToken) -> AuthToken {
    AuthToken::from_bytes(oxicrypto::hkdf_sha256_extract(
        root.reveal_bytes(),
        READ_SCOPE_LABEL,
    ))
}

/// Classifies a presented token against `root`'s derived family.
///
/// [`RequestScope::Mutate`] for `root` itself — this is what keeps every
/// token minted before 0.2 a full-access one with no migration step
/// (blueprint §16: "legacy scope-less tokens = mutate for back-compat").
/// [`RequestScope::Read`] for [`derive_read_token`]'s output. `None` for
/// anything else — the caller's own [`astrs_wire::RefusalReason::BadAuth`]
/// case.
///
/// # Examples
///
/// ```
/// use astrs_coordinator::auth::{classify, derive_read_token};
/// use astrs_wire::{AuthToken, RequestScope};
///
/// let root = AuthToken::from_bytes([1; 32]);
/// assert_eq!(classify(&root, &root), Some(RequestScope::Mutate));
/// assert_eq!(
///     classify(&root, &derive_read_token(&root)),
///     Some(RequestScope::Read)
/// );
/// assert_eq!(classify(&root, &AuthToken::from_bytes([2; 32])), None);
/// ```
#[must_use]
pub fn classify(root: &AuthToken, presented: &AuthToken) -> Option<RequestScope> {
    if root.verify(presented) {
        Some(RequestScope::Mutate)
    } else if derive_read_token(root).verify(presented) {
        Some(RequestScope::Read)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn root() -> AuthToken {
        AuthToken::from_bytes([0x42; AUTH_TOKEN_LEN])
    }

    #[test]
    fn derivation_is_deterministic() {
        assert_eq!(derive_read_token(&root()), derive_read_token(&root()));
    }

    #[test]
    fn the_derived_token_differs_from_the_root() {
        assert_ne!(derive_read_token(&root()), root());
    }

    #[test]
    fn different_roots_derive_different_read_tokens() {
        let other = AuthToken::from_bytes([0x43; AUTH_TOKEN_LEN]);
        assert_ne!(derive_read_token(&root()), derive_read_token(&other));
    }

    #[test]
    fn the_derivation_is_not_a_bare_reflection_of_the_root_bytes() {
        // Guards against a regression to something like a plain hash of the
        // root with no domain separation, which loses the "cannot be turned
        // back into the root" property this module's docs promise.
        let derived = derive_read_token(&root());
        assert_ne!(derived.reveal_bytes(), root().reveal_bytes());
        assert_ne!(derived, AuthToken::ZERO);
    }

    #[test]
    fn classify_maps_the_root_to_mutate() {
        assert_eq!(classify(&root(), &root()), Some(RequestScope::Mutate));
    }

    #[test]
    fn classify_maps_the_derived_token_to_read() {
        let read = derive_read_token(&root());
        assert_eq!(classify(&root(), &read), Some(RequestScope::Read));
    }

    #[test]
    fn classify_rejects_anything_else() {
        assert_eq!(
            classify(&root(), &AuthToken::from_bytes([9; AUTH_TOKEN_LEN])),
            None
        );
        assert_eq!(classify(&root(), &AuthToken::ZERO), None);
    }

    #[test]
    fn the_zero_root_still_classifies_itself_as_mutate() {
        // `AuthToken::ZERO` is a legal (if discouraged) token value (§16); it
        // must not be special-cased out of the same classification rule.
        assert_eq!(
            classify(&AuthToken::ZERO, &AuthToken::ZERO),
            Some(RequestScope::Mutate)
        );
    }

    #[test]
    fn deriving_from_the_zero_root_still_produces_a_real_non_zero_credential() {
        // `derive_read_token` has no special case for `AuthToken::ZERO` (it
        // is an ordinary, if discouraged, root value per §16) — HMAC of a
        // known all-zero key is exactly as well-defined and one-way as any
        // other key, so this must derive a real 32-byte value rather than
        // ever falling back to `ZERO` itself, which would collapse the
        // read/mutate distinction for a token-less deployment.
        let derived = derive_read_token(&AuthToken::ZERO);
        assert_ne!(derived, AuthToken::ZERO);
        assert_eq!(
            classify(&AuthToken::ZERO, &derived),
            Some(RequestScope::Read)
        );
    }
}
