//! [`DiscoveryError`] — the single error type this crate returns.
//!
//! Following the same discipline as `astrs-wire`'s `WireError` (blueprint
//! §3.8: nothing panics on hostile input, nothing reports a fault as a bare
//! string): every variant here names the exact thing that went wrong, with
//! enough attached context to act on it, and callers never need to reach
//! into `astrs-wire`, `oxicode` or `oxicrypto` themselves to understand a
//! failure this crate produced.

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::beacon::BeaconRole;

/// Everything that can go wrong while building, sending, receiving or
/// configuring AstRS peer discovery.
///
/// # Examples
///
/// ```
/// use astrs_discovery::DiscoveryError;
///
/// let err = DiscoveryError::Oversized { len: 4096, max: 2048 };
/// assert!(err.to_string().contains("2048"));
/// ```
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DiscoveryError {
    /// A beacon datagram was not a valid encoding of the [`crate::Beacon`]
    /// shape — truncated, corrupt, carrying trailing bytes, or produced by
    /// an unrelated protocol sharing the multicast group.
    ///
    /// This is the outcome for a byte stream that fails to parse *at all*;
    /// a well-formed beacon signed by a different cluster is
    /// [`DiscoveryError::AuthRejected`] instead, not this variant.
    #[error("beacon payload is not valid: {0}")]
    Wire(#[from] astrs_wire::WireError),

    /// A beacon decoded successfully but its `auth_tag` did not match the
    /// locally configured cluster token.
    ///
    /// This is the expected, silent-to-the-network outcome for a foreign or
    /// rogue beacon (blueprint's discovery requirement: reject *without*
    /// treating it as a decode error). Callers should drop the packet and
    /// continue; it does not indicate the local socket, configuration or
    /// the sender's protocol version are broken.
    #[error(
        "beacon from {machine_id} (role {role:?}) failed auth-tag verification: wrong cluster token"
    )]
    AuthRejected {
        /// The role the (unverified) beacon claimed.
        role: BeaconRole,
        /// The peer identity the (unverified) beacon claimed.
        machine_id: astrs_wire::DaemonId,
    },

    /// An inbound datagram was at or above [`crate::defaults::RECV_BUFFER_LEN`]
    /// and was rejected before any decode was attempted.
    ///
    /// UDP silently truncates a datagram larger than the receiver's buffer,
    /// so a beacon this large cannot be trusted to have arrived intact even
    /// if it happened to decode; rejecting on size alone avoids ever acting
    /// on a truncated payload.
    #[error("beacon datagram of {len} bytes reached the {max}-byte receive-buffer cap")]
    Oversized {
        /// The number of bytes the socket reported.
        len: usize,
        /// The configured receive-buffer size.
        max: usize,
    },

    /// Binding the UDP socket used to send and receive beacons failed
    /// outright (as opposed to the multicast *join* on that socket failing,
    /// which degrades gracefully — see [`crate::socket::MulticastStatus`]).
    ///
    /// Unlike a failed multicast join, there is no meaningful fallback for
    /// "the requested local address could not be bound at all" — the
    /// caller's configuration needs to change.
    #[error("failed to bind the discovery socket on {addr}: {source}")]
    SocketBind {
        /// The address that could not be bound.
        addr: SocketAddr,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// Reading a [`crate::peer_book::PeerBook`] configuration file failed.
    #[error("failed to read peer book file {path:?}: {source}")]
    ConfigRead {
        /// The file that could not be read.
        path: PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// A [`crate::peer_book::PeerBook`] configuration file was not valid
    /// JSON, or did not match the expected shape.
    #[error("failed to parse peer book file {path:?}: {source}")]
    ConfigParse {
        /// The file that failed to parse.
        path: PathBuf,
        /// The underlying JSON error.
        #[source]
        source: serde_json::Error,
    },

    /// An environment variable consulted by
    /// [`crate::peer_book::PeerBook::from_env`] was set but could not be
    /// parsed under its documented grammar.
    #[error("environment variable {var} has an invalid value {value:?}: {reason}")]
    EnvVar {
        /// The variable's name (one of the `ENV_*` constants in
        /// [`crate::defaults`]).
        var: &'static str,
        /// The value actually read from the environment.
        value: String,
        /// A short explanation of what about it was invalid.
        reason: String,
    },

    /// A machine label failed AstRS's name-shaped identifier grammar
    /// (`[A-Za-z0-9_.-]+`, ≤255 bytes — see [`astrs_wire::MachineName`]).
    #[error("invalid machine label: {0}")]
    InvalidMachineName(#[from] astrs_wire::IdError),

    /// The underlying network or filesystem I/O failed in a way not covered
    /// by a more specific variant above (a send or receive on an already
    /// established socket, most commonly).
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// An `oxicrypto` operation failed for a reason other than "the tag did
    /// not match" (which is [`DiscoveryError::AuthRejected`], not this).
    ///
    /// Every call site in this crate uses fixed-size buffers and a
    /// truncation length this crate itself controls, so in practice this
    /// should be unreachable; it is surfaced rather than unwrapped per
    /// policy (blueprint §20.1).
    #[error("cryptographic operation failed: {0:?}")]
    Mac(#[from] oxicrypto::CryptoError),
}

/// Convenience alias for fallible discovery operations.
pub type DiscoveryResult<T> = Result<T, DiscoveryError>;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn errors_render_useful_messages() {
        let err = DiscoveryError::Oversized {
            len: 4096,
            max: 2048,
        };
        assert!(err.to_string().contains("4096"));
        assert!(err.to_string().contains("2048"));

        let err = DiscoveryError::EnvVar {
            var: "ASTRS_COORDINATOR_ADDR",
            value: "not-an-addr".to_owned(),
            reason: "missing port".to_owned(),
        };
        let text = err.to_string();
        assert!(text.contains("ASTRS_COORDINATOR_ADDR"));
        assert!(text.contains("not-an-addr"));
        assert!(text.contains("missing port"));
    }

    #[test]
    fn discovery_error_stays_small_enough_for_result_returns() {
        assert!(
            core::mem::size_of::<DiscoveryError>() <= 128,
            "DiscoveryError grew to {} bytes",
            core::mem::size_of::<DiscoveryError>()
        );
    }

    #[test]
    fn is_non_exhaustive_and_implements_std_error() {
        fn assert_error<E: std::error::Error>() {}
        assert_error::<DiscoveryError>();
    }
}
