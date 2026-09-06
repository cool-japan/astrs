//! The transport error taxonomy.
//!
//! Every failure the connection abstraction can produce is one variant of
//! [`TransportError`], and every variant answers three questions a caller
//! actually asks:
//!
//! 1. **Is the byte stream still trustworthy?** — [`TransportError::is_fatal`].
//!    A CRC mismatch or a framing violation means the stream lost sync at an
//!    unknown offset; there is nothing to recover and the connection must be
//!    dropped. A refused route or a full queue leaves the connection perfectly
//!    healthy.
//! 2. **Would reconnecting plausibly help?** — [`TransportError::is_retryable`].
//!    [`ReconnectingConnection`](crate::ReconnectingConnection) consults this
//!    to decide between backing off and giving up: a peer that refused the
//!    protocol version will refuse it again in 250 ms, a peer whose process is
//!    restarting will not.
//! 3. **Whose fault is it?** — [`TransportError::is_peer_fault`]. Used by the
//!    daemon's metrics to separate "our network is bad" from "that peer is
//!    misbehaving", which are very different pages in an incident.
//!
//! # Examples
//!
//! ```
//! use astrs_transport::TransportError;
//!
//! let overflow = TransportError::SendQueueFull {
//!     route: astrs_wire::RouteId::new(3),
//!     capacity: 64,
//! };
//! assert!(!overflow.is_fatal());
//! assert!(!overflow.is_retryable());
//! ```

use std::fmt;
use std::io;

use astrs_wire::{Compression, FrameKind, HandshakeError, Refused, RouteId, WireError};

/// Convenience alias for fallible transport operations.
pub type TransportResult<T> = Result<T, TransportError>;

/// Everything that can go wrong on an AstRS transport connection.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransportError {
    /// The underlying socket failed.
    #[error("transport i/o error: {0}")]
    Io(#[from] io::Error),

    /// The framed codec rejected what came off the wire, or refused to encode
    /// what was handed to it.
    #[error("wire error: {0}")]
    Wire(#[from] WireError),

    /// The handshake did not complete (blueprint §7.2).
    #[error("handshake failed: {0}")]
    Handshake(#[from] HandshakeError),

    /// The peer refused the connection outright, with a typed reason.
    ///
    /// Kept separate from [`TransportError::Handshake`] so that a caller can
    /// read `max_protocol` without matching two levels of enum.
    #[error("peer refused the connection: {0}")]
    Refused(Box<Refused>),

    /// The connection was closed — by us, by the peer, or by the driver task
    /// exiting.
    #[error("connection closed: {reason}")]
    Closed {
        /// Why the connection ended.
        reason: CloseReason,
    },

    /// A frame arrived on a connection whose driver has already stopped, or a
    /// send was attempted after close.
    #[error("connection is no longer usable")]
    NotConnected,

    /// The peer sent a frame larger than the limit both ends negotiated.
    ///
    /// Enforced symmetrically: the same ceiling rejects an oversized *send*
    /// before it reaches the socket.
    #[error("frame payload of {actual} B exceeds the negotiated ceiling of {limit} B")]
    FrameTooLarge {
        /// The payload size that was refused, in bytes.
        actual: usize,
        /// The negotiated ceiling, in bytes.
        limit: usize,
    },

    /// A payload declared an uncompressed size the connection cannot honour.
    ///
    /// This is the decompression-bomb guard: the container's declared original
    /// length is checked against the negotiated ceiling *before* any codec
    /// runs.
    #[error("compressed payload declares {declared} B, above the {limit} B ceiling")]
    DecompressedTooLarge {
        /// The size the container claimed the payload expands to.
        declared: u64,
        /// The negotiated ceiling, in bytes.
        limit: usize,
    },

    /// A compressed payload did not expand to the size its container declared.
    #[error("compressed payload expanded to {actual} B, not the declared {declared} B")]
    DecompressedLengthMismatch {
        /// What the container declared.
        declared: usize,
        /// What the codec actually produced.
        actual: usize,
    },

    /// The compression container was too short to hold its own header.
    #[error("compressed payload is {len} B, too short for the {needed} B container header")]
    TruncatedCompressionHeader {
        /// The payload length that was found.
        len: usize,
        /// The minimum length a container needs.
        needed: usize,
    },

    /// A codec failed to compress or decompress a payload.
    #[error("{codec} codec failed: {message}")]
    Codec {
        /// Which codec failed.
        codec: Compression,
        /// The codec's own description of the failure.
        message: String,
    },

    /// A frame arrived compressed with a codec this connection never
    /// negotiated (blueprint §7.1 flag bits).
    #[error("peer used {codec} compression, which this connection did not negotiate")]
    CompressionNotNegotiated {
        /// The codec the peer used.
        codec: Compression,
    },

    /// The mux mini-protocol header was malformed.
    #[error("malformed route-mux header: {0}")]
    MalformedMux(#[from] MuxHeaderError),

    /// The bounded send queue for a route is full and the caller asked not to
    /// wait.
    ///
    /// This is the typed overflow error the reconnect buffer and the
    /// non-blocking send paths raise instead of growing without bound.
    #[error("send queue for route {route} is full ({capacity} frames)")]
    SendQueueFull {
        /// The route whose queue overflowed.
        route: RouteId,
        /// The queue's capacity, in frames.
        capacity: usize,
    },

    /// The reconnect buffer overflowed while the connection was down.
    #[error("reconnect buffer overflowed: {dropped} frame(s) dropped, capacity {capacity}")]
    ReconnectBufferOverflow {
        /// How many frames were dropped to make room, or refused.
        dropped: usize,
        /// The buffer's capacity, in frames.
        capacity: usize,
    },

    /// The peer sent more frames on a route than its flow-control window
    /// allowed.
    #[error("route {route} overran its flow-control window ({window} frames)")]
    FlowControlViolation {
        /// The offending route.
        route: RouteId,
        /// The window the peer was granted, in frames.
        window: u32,
    },

    /// A route handle was used after the route closed, or was never opened.
    #[error("route {route} is not open on this connection")]
    UnknownRoute {
        /// The route that was addressed.
        route: RouteId,
    },

    /// A route was opened twice with the same handle.
    #[error("route {route} is already open on this connection")]
    DuplicateRoute {
        /// The handle that collided.
        route: RouteId,
    },

    /// The connection has as many routes open as the handshake permits.
    #[error("route limit reached: {limit} routes already open")]
    RouteLimitReached {
        /// The negotiated ceiling.
        limit: u32,
    },

    /// The peer refused a route open.
    #[error("peer refused route {route}: {message}")]
    RouteRefused {
        /// The route that was refused.
        route: RouteId,
        /// The peer's stated cause.
        message: String,
    },

    /// A frame of an unexpected family arrived where a specific one was
    /// required — a `Welcome` was awaited and a `DaemonEvent` arrived, say.
    #[error("expected a {expected} frame, got {found}")]
    UnexpectedFrame {
        /// The family the caller required.
        expected: FrameKind,
        /// The family that arrived.
        found: FrameKind,
    },

    /// The peer closed the stream before sending a frame that was required.
    #[error("peer closed the connection while a {expected} frame was expected")]
    UnexpectedEof {
        /// What was still owed.
        expected: FrameKind,
    },

    /// An operation did not finish inside its deadline.
    #[error("{operation} timed out after {}ms", timeout.as_millis())]
    Timeout {
        /// What was being attempted.
        operation: &'static str,
        /// How long it was given.
        timeout: std::time::Duration,
    },

    /// The datagram capability was used on a connection that does not offer it
    /// and could not emulate it.
    #[error("datagrams are not available on this connection")]
    DatagramsUnavailable,

    /// A datagram exceeded the path's datagram ceiling.
    #[error("datagram of {actual} B exceeds the {limit} B datagram ceiling")]
    DatagramTooLarge {
        /// The datagram size that was refused.
        actual: usize,
        /// The ceiling.
        limit: usize,
    },

    /// The address could not be parsed or does not name a supported plane.
    #[error("invalid transport address: {0}")]
    Address(#[from] AddressError),

    /// The backend refused the configuration it was given.
    #[error("invalid transport configuration: {0}")]
    Configuration(String),

    /// The QUIC backend failed.
    ///
    /// Carries the backend's message rather than the backend's error type so
    /// that this enum stays identical whether or not the `quic` feature is on.
    #[error("quic backend error: {0}")]
    Quic(String),

    /// A capability the blueprint assumes is missing from the backend build.
    #[error("{capability} is not available in this build: {detail}")]
    Unsupported {
        /// The capability that was asked for.
        capability: &'static str,
        /// Why it is missing, and what would restore it.
        detail: &'static str,
    },
}

impl TransportError {
    /// Builds a codec failure from any error that can describe itself.
    pub fn codec(codec: Compression, error: impl fmt::Display) -> Self {
        Self::Codec {
            codec,
            message: error.to_string(),
        }
    }

    /// Builds a QUIC backend failure from any error that can describe itself.
    pub fn quic(error: impl fmt::Display) -> Self {
        Self::Quic(error.to_string())
    }

    /// Whether the byte stream is no longer trustworthy and the connection
    /// must be torn down.
    ///
    /// A framing or checksum failure means the reader has lost track of where
    /// frames begin; there is no resynchronisation point in the AstRS frame
    /// format by design (§7.1), so the only correct action is to close.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::TransportError;
    /// use astrs_wire::{RouteId, WireError};
    ///
    /// let corrupt = TransportError::Wire(WireError::CrcMismatch {
    ///     expected: 1,
    ///     computed: 2,
    /// });
    /// assert!(corrupt.is_fatal());
    ///
    /// let busy = TransportError::SendQueueFull {
    ///     route: RouteId::FIRST,
    ///     capacity: 8,
    /// };
    /// assert!(!busy.is_fatal());
    /// ```
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        match self {
            Self::Wire(err) => err.is_protocol_violation(),
            Self::Io(_)
            | Self::Handshake(_)
            | Self::Refused(_)
            | Self::Closed { .. }
            | Self::NotConnected
            | Self::MalformedMux(_)
            | Self::FlowControlViolation { .. }
            | Self::CompressionNotNegotiated { .. }
            | Self::UnexpectedFrame { .. }
            | Self::UnexpectedEof { .. }
            | Self::DecompressedTooLarge { .. }
            | Self::DecompressedLengthMismatch { .. }
            | Self::TruncatedCompressionHeader { .. } => true,
            Self::FrameTooLarge { .. }
            | Self::Codec { .. }
            | Self::SendQueueFull { .. }
            | Self::ReconnectBufferOverflow { .. }
            | Self::UnknownRoute { .. }
            | Self::DuplicateRoute { .. }
            | Self::RouteLimitReached { .. }
            | Self::RouteRefused { .. }
            | Self::Timeout { .. }
            | Self::DatagramsUnavailable
            | Self::DatagramTooLarge { .. }
            | Self::Address(_)
            | Self::Configuration(_)
            | Self::Quic(_)
            | Self::Unsupported { .. } => false,
        }
    }

    /// Whether opening a fresh connection could plausibly succeed.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::TransportError;
    ///
    /// let dropped = TransportError::Io(std::io::ErrorKind::ConnectionReset.into());
    /// assert!(dropped.is_retryable());
    ///
    /// let misconfigured = TransportError::Configuration("no such socket dir".into());
    /// assert!(!misconfigured.is_retryable());
    /// ```
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Io(err) => !matches!(
                err.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::InvalidInput
                    | io::ErrorKind::Unsupported
            ),
            Self::Handshake(err) => err.is_retryable(),
            Self::Refused(refused) => refused.reason.is_retryable(),
            Self::Closed { reason } => reason.is_retryable(),
            Self::NotConnected | Self::Timeout { .. } | Self::Quic(_) => true,
            // A peer that violated framing may simply be a stale process that
            // a restart replaces; retry, but the connection itself is gone.
            Self::Wire(err) => !err.is_protocol_violation(),
            Self::MalformedMux(_)
            | Self::FlowControlViolation { .. }
            | Self::CompressionNotNegotiated { .. }
            | Self::UnexpectedFrame { .. }
            | Self::UnexpectedEof { .. } => true,
            Self::FrameTooLarge { .. }
            | Self::DecompressedTooLarge { .. }
            | Self::DecompressedLengthMismatch { .. }
            | Self::TruncatedCompressionHeader { .. }
            | Self::Codec { .. }
            | Self::SendQueueFull { .. }
            | Self::ReconnectBufferOverflow { .. }
            | Self::UnknownRoute { .. }
            | Self::DuplicateRoute { .. }
            | Self::RouteLimitReached { .. }
            | Self::RouteRefused { .. }
            | Self::DatagramsUnavailable
            | Self::DatagramTooLarge { .. }
            | Self::Address(_)
            | Self::Configuration(_)
            | Self::Unsupported { .. } => false,
        }
    }

    /// Whether the peer, rather than the local endpoint or the network, is
    /// responsible.
    #[must_use]
    pub fn is_peer_fault(&self) -> bool {
        match self {
            Self::Wire(err) => err.is_protocol_violation(),
            Self::MalformedMux(_)
            | Self::FlowControlViolation { .. }
            | Self::CompressionNotNegotiated { .. }
            | Self::UnexpectedFrame { .. }
            | Self::DecompressedTooLarge { .. }
            | Self::DecompressedLengthMismatch { .. }
            | Self::TruncatedCompressionHeader { .. }
            | Self::Refused(_)
            | Self::DuplicateRoute { .. } => true,
            _ => false,
        }
    }

    /// A short, stable label for metrics dimensions.
    ///
    /// Metric cardinality is bounded by construction: the label set is exactly
    /// the variant set, never a formatted message.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::TransportError;
    ///
    /// assert_eq!(TransportError::NotConnected.label(), "not_connected");
    /// ```
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Io(_) => "io",
            Self::Wire(_) => "wire",
            Self::Handshake(_) => "handshake",
            Self::Refused(_) => "refused",
            Self::Closed { .. } => "closed",
            Self::NotConnected => "not_connected",
            Self::FrameTooLarge { .. } => "frame_too_large",
            Self::DecompressedTooLarge { .. } => "decompressed_too_large",
            Self::DecompressedLengthMismatch { .. } => "decompressed_length_mismatch",
            Self::TruncatedCompressionHeader { .. } => "truncated_compression_header",
            Self::Codec { .. } => "codec",
            Self::CompressionNotNegotiated { .. } => "compression_not_negotiated",
            Self::MalformedMux(_) => "malformed_mux",
            Self::SendQueueFull { .. } => "send_queue_full",
            Self::ReconnectBufferOverflow { .. } => "reconnect_buffer_overflow",
            Self::FlowControlViolation { .. } => "flow_control_violation",
            Self::UnknownRoute { .. } => "unknown_route",
            Self::DuplicateRoute { .. } => "duplicate_route",
            Self::RouteLimitReached { .. } => "route_limit_reached",
            Self::RouteRefused { .. } => "route_refused",
            Self::UnexpectedFrame { .. } => "unexpected_frame",
            Self::UnexpectedEof { .. } => "unexpected_eof",
            Self::Timeout { .. } => "timeout",
            Self::DatagramsUnavailable => "datagrams_unavailable",
            Self::DatagramTooLarge { .. } => "datagram_too_large",
            Self::Address(_) => "address",
            Self::Configuration(_) => "configuration",
            Self::Quic(_) => "quic",
            Self::Unsupported { .. } => "unsupported",
        }
    }
}

impl From<Refused> for TransportError {
    fn from(value: Refused) -> Self {
        Self::Refused(Box::new(value))
    }
}

/// Why a connection ended.
///
/// Reported to reconnect subscribers so that a supervisor can tell "the peer
/// said goodbye" from "the wire went quiet" without parsing a message.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CloseReason {
    /// The local end asked for the close.
    Local {
        /// A short operator-facing explanation.
        detail: String,
    },
    /// The peer asked for the close, cleanly.
    Peer {
        /// The peer's stated reason, if it sent one.
        detail: String,
    },
    /// The stream ended without a goodbye.
    Eof,
    /// The connection was torn down because the stream lost integrity.
    Protocol {
        /// What was wrong.
        detail: String,
    },
    /// The transport failed underneath.
    Transport {
        /// The failure description.
        detail: String,
    },
    /// The keepalive deadline expired with no traffic.
    KeepaliveTimeout,
    /// The owning handle was dropped.
    Dropped,
}

impl CloseReason {
    /// A local close with a short explanation.
    #[must_use]
    pub fn local(detail: impl Into<String>) -> Self {
        Self::Local {
            detail: detail.into(),
        }
    }

    /// A peer-initiated close with the peer's explanation.
    #[must_use]
    pub fn peer(detail: impl Into<String>) -> Self {
        Self::Peer {
            detail: detail.into(),
        }
    }

    /// A close forced by a protocol violation.
    #[must_use]
    pub fn protocol(detail: impl Into<String>) -> Self {
        Self::Protocol {
            detail: detail.into(),
        }
    }

    /// A close forced by the transport underneath.
    #[must_use]
    pub fn transport(detail: impl Into<String>) -> Self {
        Self::Transport {
            detail: detail.into(),
        }
    }

    /// Whether reconnecting after this close makes sense.
    ///
    /// A deliberate local close does not reconnect; everything else might.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::CloseReason;
    ///
    /// assert!(!CloseReason::local("shutting down").is_retryable());
    /// assert!(CloseReason::Eof.is_retryable());
    /// ```
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        !matches!(self, Self::Local { .. } | Self::Dropped)
    }

    /// A short, stable label for metrics dimensions.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Local { .. } => "local",
            Self::Peer { .. } => "peer",
            Self::Eof => "eof",
            Self::Protocol { .. } => "protocol",
            Self::Transport { .. } => "transport",
            Self::KeepaliveTimeout => "keepalive_timeout",
            Self::Dropped => "dropped",
        }
    }
}

impl fmt::Display for CloseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local { detail } => write!(f, "closed locally ({detail})"),
            Self::Peer { detail } => write!(f, "closed by peer ({detail})"),
            Self::Eof => f.write_str("peer stopped sending"),
            Self::Protocol { detail } => write!(f, "protocol violation ({detail})"),
            Self::Transport { detail } => write!(f, "transport failure ({detail})"),
            Self::KeepaliveTimeout => f.write_str("keepalive timed out"),
            Self::Dropped => f.write_str("handle dropped"),
        }
    }
}

/// Why a mux header could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MuxHeaderError {
    /// The frame payload was shorter than the fixed mux header.
    #[error("payload is {len} B, shorter than the {needed} B mux header")]
    Truncated {
        /// What arrived.
        len: usize,
        /// What the header needs.
        needed: usize,
    },
    /// The tag byte is not one this build knows.
    #[error("unknown mux tag {tag:#04x}")]
    UnknownTag {
        /// The byte that was found.
        tag: u8,
    },
    /// A control-plane frame carried a non-zero route, or a route frame
    /// carried the reserved zero route.
    #[error("mux tag {tag} is not valid with route {route}")]
    RouteMismatch {
        /// The tag that was found.
        tag: &'static str,
        /// The route that was found.
        route: RouteId,
    },
    /// A tag's fixed-size body was missing or the wrong length.
    #[error("mux tag {tag} expects a {expected} B body, found {found} B")]
    BadBody {
        /// The tag whose body was wrong.
        tag: &'static str,
        /// How many bytes the tag needs.
        expected: usize,
        /// How many bytes arrived.
        found: usize,
    },
}

/// Why a transport address could not be understood.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AddressError {
    /// The address text was empty.
    #[error("transport address is empty")]
    Empty,
    /// The scheme prefix is not one of `uds:`, `tcp:` or `quic:`.
    #[error("unknown transport scheme {scheme:?} (expected uds, tcp or quic)")]
    UnknownScheme {
        /// The scheme that was found.
        scheme: String,
    },
    /// The address body did not parse for its scheme.
    #[error("{scheme} address {body:?} is malformed: {detail}")]
    Malformed {
        /// The scheme that was being parsed.
        scheme: &'static str,
        /// The text that failed.
        body: String,
        /// Why it failed.
        detail: String,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::{FrameKind, RefusalReason};

    #[test]
    fn framing_failures_are_fatal_and_blamed_on_the_peer() {
        let err = TransportError::Wire(WireError::CrcMismatch {
            expected: 1,
            computed: 2,
        });
        assert!(err.is_fatal());
        assert!(err.is_peer_fault());
        assert!(!err.is_retryable());
    }

    #[test]
    fn io_failures_are_fatal_for_the_connection_but_retryable() {
        let err = TransportError::Io(io::ErrorKind::ConnectionReset.into());
        assert!(err.is_fatal());
        assert!(err.is_retryable());
        assert!(!err.is_peer_fault());
    }

    #[test]
    fn permission_denied_is_not_worth_retrying() {
        let err = TransportError::Io(io::ErrorKind::PermissionDenied.into());
        assert!(!err.is_retryable());
    }

    #[test]
    fn a_full_queue_leaves_the_connection_alone() {
        let err = TransportError::SendQueueFull {
            route: RouteId::FIRST,
            capacity: 4,
        };
        assert!(!err.is_fatal());
        assert!(!err.is_retryable());
        assert_eq!(err.label(), "send_queue_full");
    }

    #[test]
    fn a_transient_refusal_is_retryable() {
        let transient: TransportError = Refused::new(RefusalReason::ShuttingDown).into();
        assert!(transient.is_retryable());
        let permanent: TransportError = Refused::new(RefusalReason::BadAuth).into();
        assert!(!permanent.is_retryable());
    }

    #[test]
    fn every_label_is_snake_case_and_short() {
        let errors = [
            TransportError::Io(io::ErrorKind::Other.into()),
            TransportError::Wire(WireError::MissingCrc),
            TransportError::NotConnected,
            TransportError::Closed {
                reason: CloseReason::Eof,
            },
            TransportError::FrameTooLarge {
                actual: 2,
                limit: 1,
            },
            TransportError::DecompressedTooLarge {
                declared: 2,
                limit: 1,
            },
            TransportError::DecompressedLengthMismatch {
                declared: 2,
                actual: 1,
            },
            TransportError::TruncatedCompressionHeader { len: 1, needed: 4 },
            TransportError::codec(Compression::Lz4, "boom"),
            TransportError::CompressionNotNegotiated {
                codec: Compression::Zstd,
            },
            TransportError::MalformedMux(MuxHeaderError::UnknownTag { tag: 0xff }),
            TransportError::SendQueueFull {
                route: RouteId::FIRST,
                capacity: 1,
            },
            TransportError::ReconnectBufferOverflow {
                dropped: 1,
                capacity: 1,
            },
            TransportError::FlowControlViolation {
                route: RouteId::FIRST,
                window: 1,
            },
            TransportError::UnknownRoute {
                route: RouteId::FIRST,
            },
            TransportError::DuplicateRoute {
                route: RouteId::FIRST,
            },
            TransportError::RouteLimitReached { limit: 1 },
            TransportError::RouteRefused {
                route: RouteId::FIRST,
                message: "no".into(),
            },
            TransportError::UnexpectedFrame {
                expected: FrameKind::Control,
                found: FrameKind::Data,
            },
            TransportError::UnexpectedEof {
                expected: FrameKind::Control,
            },
            TransportError::Timeout {
                operation: "handshake",
                timeout: std::time::Duration::from_millis(1),
            },
            TransportError::DatagramsUnavailable,
            TransportError::DatagramTooLarge {
                actual: 2,
                limit: 1,
            },
            TransportError::Address(AddressError::Empty),
            TransportError::Configuration("bad".into()),
            TransportError::quic("bad"),
            TransportError::Unsupported {
                capability: "x",
                detail: "y",
            },
        ];
        for err in &errors {
            let label = err.label();
            assert!(!label.is_empty());
            assert!(
                label.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_'),
                "label {label:?} is not snake_case"
            );
            // Every variant must render without panicking.
            assert!(!err.to_string().is_empty());
        }
    }

    #[test]
    fn close_reasons_render_and_classify() {
        assert!(!CloseReason::local("bye").is_retryable());
        assert!(!CloseReason::Dropped.is_retryable());
        assert!(CloseReason::peer("restarting").is_retryable());
        assert!(CloseReason::Eof.is_retryable());
        assert!(CloseReason::protocol("bad crc").is_retryable());
        assert!(CloseReason::transport("reset").is_retryable());
        assert!(CloseReason::KeepaliveTimeout.is_retryable());
        assert_eq!(CloseReason::Eof.label(), "eof");
        assert!(CloseReason::local("bye").to_string().contains("bye"));
    }

    #[test]
    fn mux_header_errors_render() {
        let errors = [
            MuxHeaderError::Truncated { len: 1, needed: 9 },
            MuxHeaderError::UnknownTag { tag: 9 },
            MuxHeaderError::RouteMismatch {
                tag: "control",
                route: RouteId::FIRST,
            },
            MuxHeaderError::BadBody {
                tag: "credit",
                expected: 4,
                found: 0,
            },
        ];
        for err in errors {
            assert!(!err.to_string().is_empty());
        }
    }

    #[test]
    fn address_errors_render() {
        assert!(AddressError::Empty.to_string().contains("empty"));
        assert!(
            AddressError::UnknownScheme {
                scheme: "ftp".into()
            }
            .to_string()
            .contains("ftp")
        );
        assert!(
            AddressError::Malformed {
                scheme: "tcp",
                body: "::".into(),
                detail: "no port".into(),
            }
            .to_string()
            .contains("no port")
        );
    }
}
