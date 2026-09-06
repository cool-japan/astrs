//! Routes: one producer-output → consumer-input delivery path, and the
//! transport plane it runs on.
//!
//! Blueprint glossary: *"**Route** — one producer-output→consumer-input
//! delivery path with a chosen plane (SHM/UDS/QUIC). **Plane** — a transport
//! substrate."*
//!
//! Routes start on the reliable daemon path and are *upgraded* once the daemon
//! knows every consumer has attached (§6.3 slow-start handshake). That is why
//! [`RouteSpec`] carries a plane at all: the same logical edge changes plane
//! over its lifetime, and both ends have to be told.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{Compression, DataflowId, Plane, RouteKey, RouteSpec};
//!
//! let key = RouteKey::new(
//!     DataflowId::from_u128(1),
//!     "camera/image".parse()?,
//!     "detector/frames".parse()?,
//! );
//! let route = RouteSpec::new(key).with_plane(Plane::Shm);
//!
//! assert!(route.plane.is_zero_copy());
//! assert_eq!(route.compression, Compression::None);
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use core::fmt;

use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::common::node::QueuePolicy;
use crate::frame::Compression;
use crate::ids::{DataflowId, PortRef};

/// The payload size at or above which route compression is applied
/// (blueprint §6.4).
pub const COMPRESSION_THRESHOLD_BYTES: u64 = 16 * 1024;

/// The transport substrate a route runs over.
///
/// # Examples
///
/// ```
/// use astrs_wire::Plane;
///
/// assert!(Plane::Shm.is_zero_copy());
/// assert!(Plane::Quic.is_remote());
/// assert!(!Plane::Uds.is_remote());
/// assert!(Plane::Uds.needs_crc() == false);
/// ```
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Encode,
    Decode,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Plane {
    /// The reliable daemon path over a Unix domain socket — where every route
    /// starts (§6.3). The default.
    #[default]
    #[oxicode(variant = 0)]
    Uds,
    /// A shared-memory ring: same host, zero copy (§6.2).
    #[oxicode(variant = 1)]
    Shm,
    /// A TCP connection: the fallback for QUIC-hostile networks (§6.4).
    #[oxicode(variant = 2)]
    Tcp,
    /// A QUIC stream: the default cross-host transport (§6.4).
    #[oxicode(variant = 3)]
    Quic,
}

impl Plane {
    /// Every plane, in variant order.
    pub const ALL: &'static [Self] = &[Self::Uds, Self::Shm, Self::Tcp, Self::Quic];

    /// A stable, lower-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Uds => "uds",
            Self::Shm => "shm",
            Self::Tcp => "tcp",
            Self::Quic => "quic",
        }
    }

    /// Whether a payload crosses this plane without being copied.
    #[must_use]
    pub const fn is_zero_copy(self) -> bool {
        matches!(self, Self::Shm)
    }

    /// Whether this plane crosses a machine boundary.
    #[must_use]
    pub const fn is_remote(self) -> bool {
        matches!(self, Self::Tcp | Self::Quic)
    }

    /// Whether frames on this plane must carry a CRC-32C trailer.
    ///
    /// Blueprint §7.1: mandatory on network legs, optional on UDS. Shared
    /// memory carries no frames at all.
    #[must_use]
    pub const fn needs_crc(self) -> bool {
        self.is_remote()
    }

    /// Whether route compression may be applied on this plane.
    ///
    /// Compressing into a shared-memory ring would defeat the point of zero
    /// copy; compressing over a local socket costs more CPU than it saves.
    #[must_use]
    pub const fn allows_compression(self) -> bool {
        self.is_remote()
    }
}

impl fmt::Display for Plane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The identity of one route: which edge, in which dataflow.
///
/// # Examples
///
/// ```
/// use astrs_wire::{DataflowId, RouteKey};
///
/// let key = RouteKey::new(
///     DataflowId::from_u128(1),
///     "camera/image".parse()?,
///     "detector/frames".parse()?,
/// );
/// assert_eq!(key.to_string(), format!("{}:camera/image->detector/frames", DataflowId::from_u128(1)));
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Encode, Decode,
)]
pub struct RouteKey {
    /// The dataflow the edge belongs to.
    pub dataflow: DataflowId,
    /// The producing output port.
    pub producer: PortRef,
    /// The consuming input port.
    pub consumer: PortRef,
}

impl RouteKey {
    /// Builds a route key.
    #[must_use]
    pub const fn new(dataflow: DataflowId, producer: PortRef, consumer: PortRef) -> Self {
        Self {
            dataflow,
            producer,
            consumer,
        }
    }
}

impl fmt::Display for RouteKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}->{}", self.dataflow, self.producer, self.consumer)
    }
}

/// The full description of one route, as negotiated at setup.
///
/// # Examples
///
/// ```
/// use astrs_wire::{Compression, DataflowId, Plane, RouteKey, RouteSpec};
///
/// let key = RouteKey::new(
///     DataflowId::NIL,
///     "a/out".parse()?,
///     "b/in".parse()?,
/// );
/// let route = RouteSpec::new(key)
///     .with_plane(Plane::Quic)
///     .with_compression(Compression::Zstd);
///
/// assert!(route.compression_applies_to(32_768));
/// assert!(!route.compression_applies_to(1_024));
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct RouteSpec {
    /// Which edge this route serves.
    pub key: RouteKey,
    /// The transport substrate currently in use.
    pub plane: Plane,
    /// The negotiated compression codec.
    pub compression: Compression,
    /// The consumer's queue depth, so the producer can size its ring.
    pub queue_size: u32,
    /// The consumer's queue policy.
    pub queue_policy: QueuePolicy,
    /// The shared-memory segment backing this route, when
    /// `plane == Plane::Shm`.
    pub segment: Option<String>,
}

impl RouteSpec {
    /// A route on the default (reliable daemon) plane, uncompressed.
    #[must_use]
    pub fn new(key: RouteKey) -> Self {
        Self {
            key,
            plane: Plane::Uds,
            compression: Compression::None,
            queue_size: crate::common::node::DEFAULT_QUEUE_SIZE,
            queue_policy: QueuePolicy::DropOldest,
            segment: None,
        }
    }

    /// Sets the transport plane.
    #[must_use]
    pub const fn with_plane(mut self, plane: Plane) -> Self {
        self.plane = plane;
        self
    }

    /// Sets the compression codec.
    #[must_use]
    pub const fn with_compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    /// Names the shared-memory segment backing this route.
    #[must_use]
    pub fn with_segment(mut self, segment: impl Into<String>) -> Self {
        self.segment = Some(segment.into());
        self
    }

    /// Whether a payload of `len` bytes should be compressed on this route.
    ///
    /// Blueprint §6.4: compression applies to payloads at or above
    /// [`COMPRESSION_THRESHOLD_BYTES`], and never on a plane that does not
    /// allow it.
    #[must_use]
    pub const fn compression_applies_to(&self, len: u64) -> bool {
        self.compression.is_enabled()
            && self.plane.allows_compression()
            && len >= COMPRESSION_THRESHOLD_BYTES
    }

    /// Whether this route is the upgraded, zero-copy form (§6.3).
    #[must_use]
    pub const fn is_upgraded(&self) -> bool {
        self.plane.is_zero_copy()
    }
}

/// The answer to a route-setup request (§6.3, §7.3).
///
/// Setup is a two-message exchange, not a fire-and-forget announcement: the
/// accepting side is the one that knows whether the consumer still exists, what
/// planes it can serve and what payload size it will tolerate. Folding the
/// refusal into the same reply — rather than adding a `RouteRejected` variant —
/// keeps the peer family at the six variants §24.1 froze.
///
/// # Examples
///
/// ```
/// use astrs_wire::{Compression, Plane, RouteAcceptance, RouteRejection};
///
/// let accepted = RouteAcceptance::accepted(Plane::Quic, Compression::Zstd, 1 << 20);
/// assert!(accepted.is_accepted());
///
/// let refused = RouteAcceptance::Rejected {
///     reason: RouteRejection::ShuttingDown,
/// };
/// assert!(!refused.is_accepted());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RouteAcceptance {
    /// The route is open; these are the terms the acceptor agreed to.
    #[oxicode(variant = 0)]
    Accepted {
        /// The plane the route will actually run on, which may be weaker than
        /// the plane requested (a QUIC request answered with `Tcp`, say).
        plane: Plane,
        /// The compression the acceptor will decompress, `None` if it refuses
        /// compression on this route.
        compression: Compression,
        /// The largest payload the acceptor will accept on this route, in
        /// bytes. Never larger than the connection's negotiated frame limit.
        max_payload_bytes: u64,
    },
    /// The route is refused, with a machine-readable cause.
    #[oxicode(variant = 1)]
    Rejected {
        /// Why the route was refused.
        reason: RouteRejection,
    },
}

impl RouteAcceptance {
    /// Builds an acceptance on the given terms.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Compression, Plane, RouteAcceptance};
    ///
    /// let terms = RouteAcceptance::accepted(Plane::Tcp, Compression::None, 4096);
    /// assert_eq!(terms.plane(), Some(Plane::Tcp));
    /// assert_eq!(terms.max_payload_bytes(), Some(4096));
    /// ```
    #[must_use]
    pub const fn accepted(plane: Plane, compression: Compression, max_payload_bytes: u64) -> Self {
        Self::Accepted {
            plane,
            compression,
            max_payload_bytes,
        }
    }

    /// Whether the route was accepted.
    #[must_use]
    pub const fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted { .. })
    }

    /// The agreed plane, if the route was accepted.
    #[must_use]
    pub const fn plane(&self) -> Option<Plane> {
        match self {
            Self::Accepted { plane, .. } => Some(*plane),
            Self::Rejected { .. } => None,
        }
    }

    /// The agreed compression, if the route was accepted.
    #[must_use]
    pub const fn compression(&self) -> Option<Compression> {
        match self {
            Self::Accepted { compression, .. } => Some(*compression),
            Self::Rejected { .. } => None,
        }
    }

    /// The agreed payload ceiling, if the route was accepted.
    #[must_use]
    pub const fn max_payload_bytes(&self) -> Option<u64> {
        match self {
            Self::Accepted {
                max_payload_bytes, ..
            } => Some(*max_payload_bytes),
            Self::Rejected { .. } => None,
        }
    }

    /// The refusal cause, if the route was rejected.
    #[must_use]
    pub const fn rejection(&self) -> Option<&RouteRejection> {
        match self {
            Self::Accepted { .. } => None,
            Self::Rejected { reason } => Some(reason),
        }
    }
}

impl fmt::Display for RouteAcceptance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Accepted {
                plane,
                compression,
                max_payload_bytes,
            } => write!(
                f,
                "accepted on {plane} (compression {compression}, ≤ {max_payload_bytes} B)"
            ),
            Self::Rejected { reason } => write!(f, "rejected: {reason}"),
        }
    }
}

/// Why a peer refused to open a route.
///
/// # Examples
///
/// ```
/// use astrs_wire::RouteRejection;
///
/// assert!(RouteRejection::ShuttingDown.is_transient());
/// assert!(!RouteRejection::Unauthorized.is_transient());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RouteRejection {
    /// The acceptor has never heard of this dataflow.
    #[oxicode(variant = 0)]
    UnknownDataflow,
    /// The consumer port named by the route does not exist here.
    #[oxicode(variant = 1)]
    UnknownPort {
        /// The port that could not be resolved.
        port: PortRef,
    },
    /// The producer's declared type does not match the consumer's.
    #[oxicode(variant = 2)]
    TypeMismatch {
        /// What the consumer expects, `None` for an untyped port.
        expected: Option<crate::ids::TypeUrn>,
        /// What the producer offered, `None` for an untyped port.
        found: Option<crate::ids::TypeUrn>,
    },
    /// The requested plane is not available at the acceptor.
    #[oxicode(variant = 3)]
    PlaneUnavailable {
        /// The plane that was requested.
        plane: Plane,
    },
    /// The route's payload ceiling exceeds what this peer accepts.
    #[oxicode(variant = 4)]
    PayloadTooLarge {
        /// The acceptor's own ceiling, in bytes.
        limit: u64,
    },
    /// The connection is not authorised for this dataflow.
    #[oxicode(variant = 5)]
    Unauthorized,
    /// The acceptor is shutting down and opens no new routes.
    #[oxicode(variant = 6)]
    ShuttingDown,
    /// A route for the same key is already open under this handle.
    #[oxicode(variant = 7)]
    Duplicate {
        /// The handle of the route that already exists.
        route_id: crate::ids::RouteId,
    },
    /// A cause this build has no dedicated variant for.
    #[oxicode(variant = 8)]
    Other {
        /// A human-readable explanation.
        message: String,
    },
}

impl RouteRejection {
    /// Whether retrying later could succeed.
    ///
    /// A type mismatch will never fix itself; a peer that is shutting down or
    /// has not yet learned about the dataflow may accept the same request a
    /// second later. Callers use this to decide between backing off and giving
    /// up.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::UnknownDataflow | Self::ShuttingDown | Self::UnknownPort { .. }
        )
    }

    /// A stable, lower-case name for logs and metrics labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::UnknownDataflow => "unknown_dataflow",
            Self::UnknownPort { .. } => "unknown_port",
            Self::TypeMismatch { .. } => "type_mismatch",
            Self::PlaneUnavailable { .. } => "plane_unavailable",
            Self::PayloadTooLarge { .. } => "payload_too_large",
            Self::Unauthorized => "unauthorized",
            Self::ShuttingDown => "shutting_down",
            Self::Duplicate { .. } => "duplicate",
            Self::Other { .. } => "other",
        }
    }
}

impl fmt::Display for RouteRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownDataflow => f.write_str("unknown dataflow"),
            Self::UnknownPort { port } => write!(f, "unknown port {port}"),
            Self::TypeMismatch { expected, found } => {
                let expected = expected.as_ref().map_or("any", |urn| urn.as_str());
                let found = found.as_ref().map_or("any", |urn| urn.as_str());
                write!(f, "type mismatch: expected {expected}, found {found}")
            }
            Self::PlaneUnavailable { plane } => write!(f, "plane {plane} unavailable"),
            Self::PayloadTooLarge { limit } => {
                write!(f, "payload ceiling above the peer's {limit} B limit")
            }
            Self::Unauthorized => f.write_str("not authorised for this dataflow"),
            Self::ShuttingDown => f.write_str("peer is shutting down"),
            Self::Duplicate { route_id } => write!(f, "route {route_id} already open"),
            Self::Other { message } => f.write_str(message),
        }
    }
}

/// Why an established route stopped carrying data.
///
/// Shared by [`crate::PeerEvent::RouteTeardown`],
/// [`crate::PeerEvent::OutputClosed`] and the node-facing
/// [`crate::NodeEvent::InputClosed`], so a consumer sees the same vocabulary
/// whichever leg the news arrives on.
///
/// # Examples
///
/// ```
/// use astrs_wire::RouteCloseReason;
///
/// assert!(RouteCloseReason::ProducerFinished.is_expected());
/// assert!(!RouteCloseReason::ProducerCrashed { generation: 3 }.is_expected());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RouteCloseReason {
    /// The producer closed the output deliberately (`OutputDone`).
    #[oxicode(variant = 0)]
    ProducerFinished,
    /// The producer process ended without closing the output.
    #[oxicode(variant = 1)]
    ProducerCrashed {
        /// The generation of the incarnation that died (§6.2).
        generation: u64,
    },
    /// The consumer is gone, so there is nobody left to deliver to.
    #[oxicode(variant = 2)]
    ConsumerGone,
    /// The whole dataflow was stopped or destroyed.
    #[oxicode(variant = 3)]
    DataflowStopped,
    /// The transport under the route failed.
    #[oxicode(variant = 4)]
    PlaneFailed {
        /// The plane that failed.
        plane: Plane,
        /// What the transport reported.
        message: String,
    },
    /// A newer route replaced this one — typically a restart with a fresh
    /// generation, or an upgrade to a different plane.
    #[oxicode(variant = 5)]
    Superseded {
        /// The handle that took over.
        route_id: crate::ids::RouteId,
    },
    /// The daemon owning one end is shutting down.
    #[oxicode(variant = 6)]
    DaemonShutdown,
    /// A cause this build has no dedicated variant for.
    #[oxicode(variant = 7)]
    Error {
        /// A human-readable explanation.
        message: String,
    },
    /// An operator removed this edge (blueprint §8, §17 `astrs node
    /// disconnect`) — a tail append beyond the original eight variants.
    ///
    /// Distinct from [`Self::ProducerFinished`]/[`Self::ConsumerGone`]:
    /// neither end of the route failed or left, an operator simply asked
    /// for the wiring itself to go away, which is exactly the distinction
    /// [`Self::is_expected`] exists to preserve for a caller deciding
    /// whether a closure is worth a warning.
    #[oxicode(variant = 8)]
    Disconnected,
}

impl RouteCloseReason {
    /// Whether this closure is part of normal operation rather than a fault.
    ///
    /// Consumers use it to decide whether closing an input is worth a warning.
    #[must_use]
    pub const fn is_expected(&self) -> bool {
        matches!(
            self,
            Self::ProducerFinished
                | Self::DataflowStopped
                | Self::DaemonShutdown
                | Self::Disconnected
        )
    }

    /// A stable, lower-case name for logs and metrics labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::ProducerFinished => "producer_finished",
            Self::ProducerCrashed { .. } => "producer_crashed",
            Self::ConsumerGone => "consumer_gone",
            Self::DataflowStopped => "dataflow_stopped",
            Self::PlaneFailed { .. } => "plane_failed",
            Self::Superseded { .. } => "superseded",
            Self::DaemonShutdown => "daemon_shutdown",
            Self::Error { .. } => "error",
            Self::Disconnected => "disconnected",
        }
    }
}

impl fmt::Display for RouteCloseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProducerFinished => f.write_str("producer finished the output"),
            Self::ProducerCrashed { generation } => {
                write!(f, "producer generation {generation} crashed")
            }
            Self::ConsumerGone => f.write_str("consumer is gone"),
            Self::DataflowStopped => f.write_str("dataflow stopped"),
            Self::PlaneFailed { plane, message } => write!(f, "{plane} plane failed: {message}"),
            Self::Superseded { route_id } => write!(f, "superseded by route {route_id}"),
            Self::DaemonShutdown => f.write_str("daemon shutting down"),
            Self::Error { message } => f.write_str(message),
            Self::Disconnected => f.write_str("edge removed by an operator"),
        }
    }
}

/// Why the daemon pulled a producer back off the zero-copy plane (§6.3).
///
/// The slow-start handshake upgrades a route to SHM once *every* consumer has
/// attached; anything that invalidates that fact downgrades it again. The
/// producer needs the cause, not just the instruction, because a pool
/// exhaustion is a metric worth surfacing (`shm_fallback_total`) while a plain
/// consumer detach is routine.
///
/// # Examples
///
/// ```
/// use astrs_wire::RouteDowngradeReason;
///
/// assert!(RouteDowngradeReason::PoolExhausted.is_capacity_pressure());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RouteDowngradeReason {
    /// A consumer detached from the ring in an orderly fashion.
    #[oxicode(variant = 0)]
    ConsumerDetached {
        /// The consumer that left.
        consumer: PortRef,
    },
    /// A consumer died while attached, so its cursor can no longer advance.
    #[oxicode(variant = 1)]
    ConsumerCrashed {
        /// The consumer that died.
        consumer: PortRef,
    },
    /// A new consumer appeared that cannot use the shared segment (a remote
    /// one, or a dynamic node on another host).
    #[oxicode(variant = 2)]
    RemoteConsumerAdded {
        /// The consumer that forced the downgrade.
        consumer: PortRef,
    },
    /// The segment was closed — usually because the producer's generation was
    /// reclaimed after a crash.
    #[oxicode(variant = 3)]
    SegmentClosed {
        /// The generation whose segment went away.
        generation: u64,
    },
    /// The pool ran out of free slots. Blueprint §6.2: never sleep-retry —
    /// fall back to the reliable daemon path and count it.
    #[oxicode(variant = 4)]
    PoolExhausted,
    /// The daemon asked for the downgrade for a reason of its own (a rolling
    /// reconfiguration, say).
    #[oxicode(variant = 5)]
    DaemonRequest {
        /// A human-readable explanation.
        message: String,
    },
}

impl RouteDowngradeReason {
    /// Whether this downgrade signals resource pressure worth a metric.
    #[must_use]
    pub const fn is_capacity_pressure(&self) -> bool {
        matches!(self, Self::PoolExhausted)
    }

    /// A stable, lower-case name for logs and metrics labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::ConsumerDetached { .. } => "consumer_detached",
            Self::ConsumerCrashed { .. } => "consumer_crashed",
            Self::RemoteConsumerAdded { .. } => "remote_consumer_added",
            Self::SegmentClosed { .. } => "segment_closed",
            Self::PoolExhausted => "pool_exhausted",
            Self::DaemonRequest { .. } => "daemon_request",
        }
    }
}

impl fmt::Display for RouteDowngradeReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConsumerDetached { consumer } => write!(f, "consumer {consumer} detached"),
            Self::ConsumerCrashed { consumer } => write!(f, "consumer {consumer} crashed"),
            Self::RemoteConsumerAdded { consumer } => {
                write!(f, "remote consumer {consumer} joined")
            }
            Self::SegmentClosed { generation } => {
                write!(f, "segment for generation {generation} closed")
            }
            Self::PoolExhausted => f.write_str("shm pool exhausted"),
            Self::DaemonRequest { message } => f.write_str(message),
        }
    }
}

/// The shared-memory segment a producer publishes into after an upgrade
/// (§6.2, §6.3).
///
/// The daemon brokers every segment fd, so it is the daemon — not the producer
/// — that decides the geometry. This is the message that hands it over.
///
/// # Examples
///
/// ```
/// use astrs_wire::ShmSegmentSpec;
///
/// let segment = ShmSegmentSpec::new("astrs/df/camera/7/image", 7, 32, 1 << 20);
/// assert_eq!(segment.capacity_bytes(), 32 * (1 << 20));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct ShmSegmentSpec {
    /// The segment name, embedding `{dataflow_id}/{node_id}/{generation}` so a
    /// stale mapping is detectable by every reader (§6.2).
    pub name: String,
    /// The producer incarnation that owns the segment.
    pub generation: u64,
    /// How many slots the ring holds.
    pub slot_count: u32,
    /// The usable payload bytes per slot.
    pub slot_size: u64,
}

impl ShmSegmentSpec {
    /// Describes a segment.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::ShmSegmentSpec;
    ///
    /// let segment = ShmSegmentSpec::new("astrs/df/camera/1/image", 1, 8, 4096);
    /// assert_eq!(segment.slot_count, 8);
    /// ```
    #[must_use]
    pub fn new(name: impl Into<String>, generation: u64, slot_count: u32, slot_size: u64) -> Self {
        Self {
            name: name.into(),
            generation,
            slot_count,
            slot_size,
        }
    }

    /// The total payload capacity of the ring, in bytes.
    ///
    /// Saturates rather than overflowing: a forged spec claiming
    /// `u32::MAX` slots of `u64::MAX` bytes must not wrap into a small number
    /// that then passes a capacity check.
    #[must_use]
    pub const fn capacity_bytes(&self) -> u64 {
        (self.slot_count as u64).saturating_mul(self.slot_size)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::round_trip;

    fn key() -> RouteKey {
        RouteKey::new(
            DataflowId::from_u128(0x2B),
            "camera/image".parse().unwrap(),
            "detector/frames".parse().unwrap(),
        )
    }

    #[test]
    fn planes_round_trip_and_classify() {
        let mut names = std::collections::BTreeSet::new();
        for plane in Plane::ALL.iter().copied() {
            assert_eq!(round_trip(&plane).unwrap(), plane);
            assert!(names.insert(plane.as_str()));
            assert_eq!(plane.to_string(), plane.as_str());
        }
        assert_eq!(names.len(), 4);
        assert_eq!(Plane::default(), Plane::Uds);
    }

    #[test]
    fn plane_properties_match_the_blueprint() {
        assert!(Plane::Shm.is_zero_copy());
        assert!(!Plane::Uds.is_zero_copy());

        assert!(Plane::Tcp.is_remote());
        assert!(Plane::Quic.is_remote());
        assert!(!Plane::Uds.is_remote());
        assert!(!Plane::Shm.is_remote());

        // CRC mandatory on network legs, optional on UDS.
        assert!(Plane::Quic.needs_crc());
        assert!(Plane::Tcp.needs_crc());
        assert!(!Plane::Uds.needs_crc());
        assert!(!Plane::Shm.needs_crc());

        // Compression is a cross-host concern only.
        for plane in Plane::ALL.iter().copied() {
            assert_eq!(plane.allows_compression(), plane.is_remote());
        }
    }

    #[test]
    fn route_keys_render_and_round_trip() {
        let key = key();
        assert_eq!(
            key.to_string(),
            format!("{}:camera/image->detector/frames", key.dataflow)
        );
        assert_eq!(round_trip(&key).unwrap(), key);
    }

    #[test]
    fn route_keys_order_by_dataflow_then_endpoints() {
        let mut keys = [
            RouteKey::new(
                DataflowId::from_u128(2),
                "a/o".parse().unwrap(),
                "b/i".parse().unwrap(),
            ),
            RouteKey::new(
                DataflowId::from_u128(1),
                "z/o".parse().unwrap(),
                "b/i".parse().unwrap(),
            ),
            RouteKey::new(
                DataflowId::from_u128(1),
                "a/o".parse().unwrap(),
                "b/i".parse().unwrap(),
            ),
        ];
        keys.sort();
        assert_eq!(keys[0].dataflow, DataflowId::from_u128(1));
        assert_eq!(keys[0].producer.node().as_str(), "a");
        assert_eq!(keys[2].dataflow, DataflowId::from_u128(2));
    }

    #[test]
    fn a_new_route_starts_on_the_reliable_path() {
        let route = RouteSpec::new(key());
        assert_eq!(route.plane, Plane::Uds);
        assert_eq!(route.compression, Compression::None);
        assert!(!route.is_upgraded());
        assert!(route.segment.is_none());
        assert_eq!(route.queue_size, crate::common::node::DEFAULT_QUEUE_SIZE);
    }

    #[test]
    fn upgrading_to_shm_marks_the_route_upgraded() {
        let route = RouteSpec::new(key())
            .with_plane(Plane::Shm)
            .with_segment("dataflow/camera/1");
        assert!(route.is_upgraded());
        assert_eq!(route.segment.as_deref(), Some("dataflow/camera/1"));
        assert_eq!(round_trip(&route).unwrap(), route);
    }

    #[test]
    fn compression_needs_a_codec_a_remote_plane_and_the_threshold() {
        let plain = RouteSpec::new(key()).with_plane(Plane::Quic);
        assert!(!plain.compression_applies_to(u64::MAX), "no codec selected");

        let local = RouteSpec::new(key())
            .with_plane(Plane::Shm)
            .with_compression(Compression::Lz4);
        assert!(!local.compression_applies_to(u64::MAX), "shm is zero copy");

        let remote = RouteSpec::new(key())
            .with_plane(Plane::Quic)
            .with_compression(Compression::Lz4);
        assert!(!remote.compression_applies_to(COMPRESSION_THRESHOLD_BYTES - 1));
        assert!(remote.compression_applies_to(COMPRESSION_THRESHOLD_BYTES));
        assert!(remote.compression_applies_to(COMPRESSION_THRESHOLD_BYTES + 1));
    }

    #[test]
    fn the_compression_threshold_matches_the_blueprint() {
        assert_eq!(COMPRESSION_THRESHOLD_BYTES, 16 * 1024);
    }

    #[test]
    fn route_specs_round_trip_on_every_plane() {
        for plane in Plane::ALL.iter().copied() {
            for compression in Compression::ALL.iter().copied() {
                let route = RouteSpec::new(key())
                    .with_plane(plane)
                    .with_compression(compression);
                assert_eq!(round_trip(&route).unwrap(), route);
            }
        }
    }
}
