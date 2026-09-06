//! What can go wrong protecting or unprotecting a submessage.
//!
//! Every variant names one decision, and each of them is a *rejection* rather
//! than a diagnosis: a submessage that reaches any of these is discarded. That
//! is the point — a cryptographic transform has exactly one safe failure mode,
//! and the value of a fine-grained taxonomy here is the log line, never a
//! recovery path.
//!
//! Two rules the enum follows:
//!
//! 1. **No variant carries key material, plaintext, or a tag.** A log line
//!    that printed the expected MAC would hand an attacker the oracle the
//!    constant-time comparison exists to deny.
//! 2. **[`SecurityError::AuthenticationFailed`] does not say why.** A tampered
//!    body, a substituted key of the right id and a spliced crypto header all
//!    fail the same way and report the same thing.

use crate::error::RtpsError;

/// The result of a security transform.
pub type SecurityResult<T> = Result<T, SecurityError>;

/// Why a submessage could not be protected, or would not be accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SecurityError {
    /// A pre-shared key shorter than [`PSK_MIN_LEN`](super::PSK_MIN_LEN).
    #[error("a pre-shared key of {len} octets is below the {minimum}-octet minimum")]
    KeyTooShort {
        /// Octets supplied.
        len: usize,
        /// Octets required.
        minimum: usize,
    },

    /// A hexadecimal pre-shared key that is not hexadecimal, or is odd-length.
    #[error("the pre-shared key is not an even-length run of hexadecimal digits")]
    MalformedHexKey,

    /// The platform CSPRNG would not produce entropy.
    ///
    /// There is deliberately no fallback: a predictable key is worse than no
    /// key, because it looks like one.
    #[error("the platform CSPRNG is unavailable: {reason}")]
    RandomnessUnavailable {
        /// What the CSPRNG reported.
        reason: String,
    },

    /// A key derivation step failed.
    ///
    /// Only reachable if HKDF is asked for an output longer than
    /// `255 * HashLen`, which the constants in this module make impossible.
    #[error("key derivation failed: {reason}")]
    KeyDerivation {
        /// What the KDF reported.
        reason: String,
    },

    /// The AEAD refused to seal.
    ///
    /// Only reachable through a key or nonce of the wrong length, which the
    /// derivation in [`keys`](super::keys) makes impossible — so this is a
    /// bug in this crate rather than anything a peer did. It is separate from
    /// [`SecurityError::AuthenticationFailed`] for exactly that reason: one
    /// is traffic, the other is a defect.
    #[error("the cipher refused to seal: {reason}")]
    Cipher {
        /// What the AEAD reported.
        reason: String,
    },

    /// The `transformation_kind` octets name something this build does not
    /// implement.
    #[error("transformation kind {kind:02x?} is not one of the five DDS-Security 1.1 kinds")]
    UnsupportedTransformation {
        /// The four octets as they arrived.
        kind: [u8; 4],
    },

    /// A protected submessage arrived under a key id nothing is configured
    /// for.
    ///
    /// The ordinary outcome of a peer using a different pre-shared key: the
    /// key id is derived from the key, so a different key is a different id
    /// and the rejection happens before any cryptography runs.
    #[error("no key material is registered for key id 0x{key_id:08x}")]
    UnknownKeyId {
        /// The id the crypto header named.
        key_id: u32,
    },

    /// The tag did not verify.
    ///
    /// Tampering, a substituted key that happens to claim the right id, or a
    /// crypto header spliced from another session. Which one it was is not
    /// reported, and not knowable.
    #[error("the authentication tag did not verify")]
    AuthenticationFailed,

    /// A session counter that has already been accepted, or that has fallen
    /// out of the replay window.
    #[error(
        "session counter {counter} is a replay or is older than the {window}-deep window \
         (highest accepted: {highest})"
    )]
    ReplayedCounter {
        /// The counter the crypto header carried.
        counter: u64,
        /// The highest counter accepted for this session.
        highest: u64,
        /// How far back the window reaches.
        window: u64,
    },

    /// A `SEC_PREFIX` was not followed by the submessages the transformation
    /// kind requires.
    #[error("malformed secure submessage: {reason}")]
    Malformed {
        /// Which structural rule was broken.
        reason: &'static str,
    },

    /// A crypto header, footer or secure body whose octets do not parse.
    #[error("a secure submessage body is {len} octets, which cannot hold {what}")]
    Truncated {
        /// Octets present.
        len: usize,
        /// What was being read.
        what: &'static str,
    },

    /// A protected submessage arrived for an endpoint configured to expect
    /// no protection, or a plaintext one for an endpoint that requires it.
    ///
    /// Both directions matter. Accepting plaintext on a protected endpoint is
    /// a downgrade attack; accepting ciphertext on an unprotected one is a
    /// resource-exhaustion invitation.
    #[error("protection mismatch: the endpoint requires {expected} and the submessage was {found}")]
    ProtectionMismatch {
        /// What the local endpoint's policy demands.
        expected: &'static str,
        /// What arrived.
        found: &'static str,
    },

    /// A single submessage could not be protected inside the datagram budget.
    #[error(
        "a protected submessage of {len} octets does not fit the {budget}-octet datagram budget"
    )]
    OverBudget {
        /// Octets the protected submessage needs.
        len: usize,
        /// Octets a datagram may occupy.
        budget: usize,
    },

    /// The message model refused to encode or decode.
    #[error("the wire format rejected a secure submessage: {0}")]
    Wire(#[from] RtpsError),
}

impl SecurityError {
    /// True when this rejection is one a hostile peer can provoke.
    ///
    /// The distinction a log level wants: an unknown key id or a failed tag is
    /// traffic, and belongs at `debug`; a key derivation failure or an
    /// unavailable CSPRNG is a local fault, and belongs at `error`.
    #[must_use]
    pub const fn is_remote_fault(&self) -> bool {
        matches!(
            self,
            Self::UnsupportedTransformation { .. }
                | Self::UnknownKeyId { .. }
                | Self::AuthenticationFailed
                | Self::ReplayedCounter { .. }
                | Self::Malformed { .. }
                | Self::Truncated { .. }
                | Self::ProtectionMismatch { .. }
                | Self::Wire(_)
        )
    }
}

/// A short, allocation-free name for a `CryptoError` that never leaks state.
pub(crate) fn crypto_reason(error: &oxicrypto::CryptoError) -> String {
    error.to_string()
}

/// The two words [`SecurityError::ProtectionMismatch`] quotes.
pub(crate) const fn protection_word(protected: bool) -> &'static str {
    if protected { "protected" } else { "plaintext" }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_failed_tag_says_nothing_about_why() {
        let message = SecurityError::AuthenticationFailed.to_string();
        assert_eq!(message, "the authentication tag did not verify");
        assert!(!message.contains("key"), "no key material in the message");
    }

    #[test]
    fn remote_faults_and_local_faults_are_separated() {
        assert!(SecurityError::AuthenticationFailed.is_remote_fault());
        assert!(SecurityError::UnknownKeyId { key_id: 7 }.is_remote_fault());
        assert!(
            SecurityError::ReplayedCounter {
                counter: 1,
                highest: 9,
                window: 64,
            }
            .is_remote_fault()
        );
        assert!(
            !SecurityError::RandomnessUnavailable {
                reason: "no entropy".to_owned(),
            }
            .is_remote_fault()
        );
        assert!(
            !SecurityError::Cipher {
                reason: "bad key length".to_owned(),
            }
            .is_remote_fault(),
            "a cipher refusal is a defect here, not traffic"
        );
        assert!(
            !SecurityError::KeyTooShort {
                len: 4,
                minimum: 16,
            }
            .is_remote_fault()
        );
    }

    #[test]
    fn every_message_names_the_thing_that_failed() {
        assert!(
            SecurityError::UnknownKeyId {
                key_id: 0x1234_5678
            }
            .to_string()
            .contains("0x12345678")
        );
        assert!(
            SecurityError::Truncated {
                len: 3,
                what: "a crypto header",
            }
            .to_string()
            .contains("a crypto header")
        );
        assert!(
            SecurityError::OverBudget {
                len: 2_000,
                budget: 1_400,
            }
            .to_string()
            .contains("1400")
        );
    }

    #[test]
    fn the_protection_words_are_the_two_the_mismatch_error_quotes() {
        assert_eq!(protection_word(true), "protected");
        assert_eq!(protection_word(false), "plaintext");
    }
}
