//! [`NegotiatedLimits`]: the per-connection budget both ends agree to.
//!
//! Blueprint §7.2 has `Welcome` carry `limits`, and §7.1 caps a frame at 64 MiB
//! "(config)". A limit that only one end knows is not a limit: the sender must
//! refuse to *build* an oversize frame, and the receiver must refuse to *buffer*
//! one, and both need the same number to do so. That agreement is this type.
//!
//! Negotiation is `min` field by field ([`NegotiatedLimits::negotiate`]): the
//! stricter side always wins, so an embedded node with a 1 MiB budget is never
//! handed a 64 MiB frame by a well-provisioned daemon.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{DurationMs, NegotiatedLimits};
//!
//! let daemon = NegotiatedLimits::default();
//! let embedded = NegotiatedLimits::default()
//!     .with_max_payload_bytes(1 << 20)
//!     .with_heartbeat_interval(DurationMs::from_secs(1));
//!
//! let agreed = daemon.negotiate(&embedded);
//! assert_eq!(agreed.max_payload_bytes, 1 << 20);
//! assert_eq!(agreed.heartbeat_interval, DurationMs::from_secs(1));
//! ```

use core::fmt;

use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::common::duration::DurationMs;
use crate::frame::{DEFAULT_MAX_PAYLOAD_BYTES, FrameLimits, MAX_SUPPORTED_PAYLOAD_BYTES};

/// The default number of routes one connection may carry at once.
///
/// Sized for the worst realistic single-link case — every output of a large
/// graph crossing one daemon pair — with room to spare, while still bounding
/// the route table a peer can force a daemon to allocate.
pub const DEFAULT_MAX_ROUTES: u32 = 4_096;

/// The default number of frames a sender may have in flight before it must
/// wait for the peer to make progress.
pub const DEFAULT_MAX_INFLIGHT_FRAMES: u32 = 1_024;

/// The default number of concurrent subscriptions (log tails, topic taps) one
/// connection may open.
pub const DEFAULT_MAX_SUBSCRIPTIONS: u32 = 256;

/// The budget a connection runs under, agreed during the handshake.
///
/// # Examples
///
/// ```
/// use astrs_wire::{FrameLimits, NegotiatedLimits};
///
/// let limits = NegotiatedLimits::default().with_require_crc(true);
/// let frame_limits: FrameLimits = limits.to_frame_limits();
/// assert!(frame_limits.require_crc());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct NegotiatedLimits {
    /// The largest frame payload either end may send, in bytes.
    pub max_payload_bytes: u64,
    /// The largest number of routes this connection may carry at once.
    pub max_routes: u32,
    /// How many frames a sender may have unacknowledged before it must wait.
    pub max_inflight_frames: u32,
    /// How many subscriptions (log tails, topic taps) may be open at once.
    pub max_subscriptions: u32,
    /// How often each end sends a heartbeat (§24.2: 5 s).
    pub heartbeat_interval: DurationMs,
    /// How long an end waits for peer traffic before declaring the connection
    /// dead. Always at least twice the heartbeat interval after negotiation.
    pub keepalive_timeout: DurationMs,
    /// Whether every frame on this connection must carry a crc32c trailer
    /// (§7.1: mandatory on network legs, optional on UDS).
    pub require_crc: bool,
}

impl NegotiatedLimits {
    /// The blueprint defaults (§7.1, §24.2): a 64 MiB payload cap, a 5 s
    /// heartbeat, no mandatory checksum.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::NegotiatedLimits;
    ///
    /// let limits = NegotiatedLimits::new();
    /// assert_eq!(limits.max_payload_bytes, 64 * 1024 * 1024);
    /// assert_eq!(limits.heartbeat_interval.as_millis(), 5_000);
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_payload_bytes: DEFAULT_MAX_PAYLOAD_BYTES as u64,
            max_routes: DEFAULT_MAX_ROUTES,
            max_inflight_frames: DEFAULT_MAX_INFLIGHT_FRAMES,
            max_subscriptions: DEFAULT_MAX_SUBSCRIPTIONS,
            heartbeat_interval: DurationMs::HEARTBEAT,
            keepalive_timeout: DurationMs::new(15_000),
            require_crc: false,
        }
    }

    /// The defaults for a network leg: checksums are mandatory (§7.1).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::NegotiatedLimits;
    ///
    /// assert!(NegotiatedLimits::network().require_crc);
    /// assert!(!NegotiatedLimits::uds().require_crc);
    /// ```
    #[must_use]
    pub const fn network() -> Self {
        Self {
            require_crc: true,
            ..Self::new()
        }
    }

    /// The defaults for a UDS leg: checksums are optional, since the kernel
    /// already guarantees the bytes.
    #[must_use]
    pub const fn uds() -> Self {
        Self::new()
    }

    /// Sets the payload ceiling, clamped to what the `len` header field can
    /// describe.
    #[must_use]
    pub const fn with_max_payload_bytes(mut self, bytes: u64) -> Self {
        let ceiling = MAX_SUPPORTED_PAYLOAD_BYTES as u64;
        self.max_payload_bytes = if bytes > ceiling { ceiling } else { bytes };
        self
    }

    /// Sets the route ceiling.
    #[must_use]
    pub const fn with_max_routes(mut self, routes: u32) -> Self {
        self.max_routes = routes;
        self
    }

    /// Sets the in-flight frame ceiling.
    #[must_use]
    pub const fn with_max_inflight_frames(mut self, frames: u32) -> Self {
        self.max_inflight_frames = frames;
        self
    }

    /// Sets the subscription ceiling.
    #[must_use]
    pub const fn with_max_subscriptions(mut self, subscriptions: u32) -> Self {
        self.max_subscriptions = subscriptions;
        self
    }

    /// Sets the heartbeat interval.
    #[must_use]
    pub const fn with_heartbeat_interval(mut self, interval: DurationMs) -> Self {
        self.heartbeat_interval = interval;
        self
    }

    /// Sets the keepalive timeout.
    #[must_use]
    pub const fn with_keepalive_timeout(mut self, timeout: DurationMs) -> Self {
        self.keepalive_timeout = timeout;
        self
    }

    /// Sets whether a checksum is mandatory.
    #[must_use]
    pub const fn with_require_crc(mut self, required: bool) -> Self {
        self.require_crc = required;
        self
    }

    /// The stricter of two proposals, field by field.
    ///
    /// `require_crc` is the exception to "smaller wins": a checksum requirement
    /// is a *safety* property, so it is the logical `OR` — if either end needs
    /// integrity, both provide it.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::NegotiatedLimits;
    ///
    /// let strict = NegotiatedLimits::network().with_max_routes(8);
    /// let loose = NegotiatedLimits::uds();
    /// let agreed = strict.negotiate(&loose);
    /// assert_eq!(agreed.max_routes, 8);
    /// assert!(agreed.require_crc, "the stricter integrity policy wins");
    /// ```
    #[must_use]
    pub const fn negotiate(&self, other: &Self) -> Self {
        let agreed = Self {
            max_payload_bytes: min_u64(self.max_payload_bytes, other.max_payload_bytes),
            max_routes: min_u32(self.max_routes, other.max_routes),
            max_inflight_frames: min_u32(self.max_inflight_frames, other.max_inflight_frames),
            max_subscriptions: min_u32(self.max_subscriptions, other.max_subscriptions),
            heartbeat_interval: self.heartbeat_interval.min(other.heartbeat_interval),
            keepalive_timeout: self.keepalive_timeout.min(other.keepalive_timeout),
            require_crc: self.require_crc || other.require_crc,
        };
        agreed.repaired()
    }

    /// This budget with any internally inconsistent field repaired.
    ///
    /// A peer may propose a keepalive timeout shorter than its own heartbeat
    /// interval — deliberately, to force disconnects, or by accident. Either
    /// way the connection would die on the first quiet second, so the timeout
    /// is raised to twice the heartbeat, and a zero heartbeat is read as "never
    /// send one" rather than "send them continuously".
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{DurationMs, NegotiatedLimits};
    ///
    /// let hostile = NegotiatedLimits::new()
    ///     .with_heartbeat_interval(DurationMs::from_secs(5))
    ///     .with_keepalive_timeout(DurationMs::new(1));
    /// assert_eq!(hostile.repaired().keepalive_timeout.as_millis(), 10_000);
    /// ```
    #[must_use]
    pub const fn repaired(mut self) -> Self {
        if self.heartbeat_interval.is_zero() {
            self.keepalive_timeout = DurationMs::ZERO;
            return self;
        }
        let floor = match self.heartbeat_interval.checked_mul(2) {
            Some(floor) => floor,
            None => DurationMs::MAX,
        };
        if self.keepalive_timeout.as_millis() < floor.as_millis() {
            self.keepalive_timeout = floor;
        }
        self
    }

    /// Whether this budget is at least as permissive as `other` in every
    /// dimension.
    ///
    /// The initiator uses it to check that the acceptor's `Welcome` did not
    /// grant *more* than the initiator offered, which would let a peer talk a
    /// small device into buffering a frame it cannot hold.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::NegotiatedLimits;
    ///
    /// let offered = NegotiatedLimits::new();
    /// let granted = NegotiatedLimits::new().with_max_payload_bytes(1 << 20);
    /// assert!(offered.permits(&granted));
    /// assert!(!granted.permits(&offered));
    /// ```
    #[must_use]
    pub const fn permits(&self, other: &Self) -> bool {
        self.max_payload_bytes >= other.max_payload_bytes
            && self.max_routes >= other.max_routes
            && self.max_inflight_frames >= other.max_inflight_frames
            && self.max_subscriptions >= other.max_subscriptions
    }

    /// The frame-level policy this budget implies.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::NegotiatedLimits;
    ///
    /// let limits = NegotiatedLimits::new().with_max_payload_bytes(4096);
    /// assert_eq!(limits.to_frame_limits().max_payload_bytes(), 4096);
    /// ```
    #[must_use]
    pub const fn to_frame_limits(&self) -> FrameLimits {
        let bytes = if self.max_payload_bytes > MAX_SUPPORTED_PAYLOAD_BYTES as u64 {
            MAX_SUPPORTED_PAYLOAD_BYTES
        } else {
            self.max_payload_bytes as usize
        };
        FrameLimits::new()
            .with_max_payload_bytes(bytes)
            .with_require_crc(self.require_crc)
    }

    /// The budget implied by an existing frame policy, with the remaining
    /// fields left at their defaults.
    #[must_use]
    pub const fn from_frame_limits(limits: &FrameLimits) -> Self {
        Self::new()
            .with_max_payload_bytes(limits.max_payload_bytes() as u64)
            .with_require_crc(limits.require_crc())
    }
}

/// The smaller of two `u64`s, in a `const` context.
const fn min_u64(left: u64, right: u64) -> u64 {
    if left < right { left } else { right }
}

/// The smaller of two `u32`s, in a `const` context.
const fn min_u32(left: u32, right: u32) -> u32 {
    if left < right { left } else { right }
}

impl Default for NegotiatedLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for NegotiatedLimits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "≤{} B/frame, ≤{} routes, ≤{} in flight, ≤{} subscriptions, heartbeat {}, timeout {}, crc {}",
            self.max_payload_bytes,
            self.max_routes,
            self.max_inflight_frames,
            self.max_subscriptions,
            self.heartbeat_interval,
            self.keepalive_timeout,
            if self.require_crc {
                "required"
            } else {
                "optional"
            }
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::round_trip;

    #[test]
    fn defaults_match_the_blueprint_tables() {
        let limits = NegotiatedLimits::default();
        assert_eq!(limits.max_payload_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.heartbeat_interval, DurationMs::HEARTBEAT);
        assert!(!limits.require_crc);
        assert!(NegotiatedLimits::network().require_crc);
        assert_eq!(NegotiatedLimits::uds(), NegotiatedLimits::new());
    }

    #[test]
    fn negotiation_takes_the_stricter_of_each_field() {
        let left = NegotiatedLimits::new()
            .with_max_payload_bytes(1 << 20)
            .with_max_routes(10)
            .with_max_inflight_frames(4)
            .with_max_subscriptions(2)
            .with_heartbeat_interval(DurationMs::from_secs(1))
            .with_keepalive_timeout(DurationMs::from_secs(30));
        let right = NegotiatedLimits::new()
            .with_max_payload_bytes(1 << 24)
            .with_max_routes(4)
            .with_max_inflight_frames(64)
            .with_max_subscriptions(8)
            .with_heartbeat_interval(DurationMs::from_secs(5))
            .with_keepalive_timeout(DurationMs::from_secs(20));

        let agreed = left.negotiate(&right);
        assert_eq!(agreed.max_payload_bytes, 1 << 20);
        assert_eq!(agreed.max_routes, 4);
        assert_eq!(agreed.max_inflight_frames, 4);
        assert_eq!(agreed.max_subscriptions, 2);
        assert_eq!(agreed.heartbeat_interval, DurationMs::from_secs(1));
        assert_eq!(agreed.keepalive_timeout, DurationMs::from_secs(20));
    }

    #[test]
    fn negotiation_is_commutative_and_idempotent() {
        let left = NegotiatedLimits::network().with_max_routes(9);
        let right = NegotiatedLimits::uds().with_max_payload_bytes(4096);
        let agreed = left.negotiate(&right);
        assert_eq!(agreed, right.negotiate(&left));
        assert_eq!(agreed, agreed.negotiate(&agreed));
    }

    #[test]
    fn the_stricter_integrity_policy_wins() {
        let agreed = NegotiatedLimits::uds().negotiate(&NegotiatedLimits::network());
        assert!(agreed.require_crc);
    }

    #[test]
    fn a_timeout_below_two_heartbeats_is_repaired() {
        let hostile = NegotiatedLimits::new()
            .with_heartbeat_interval(DurationMs::from_secs(5))
            .with_keepalive_timeout(DurationMs::new(1));
        assert_eq!(hostile.repaired().keepalive_timeout.as_millis(), 10_000);

        // A zero heartbeat means "never", not "always".
        let quiet = NegotiatedLimits::new()
            .with_heartbeat_interval(DurationMs::ZERO)
            .with_keepalive_timeout(DurationMs::from_secs(3));
        assert_eq!(quiet.repaired().keepalive_timeout, DurationMs::ZERO);

        // Repair cannot overflow on an absurd heartbeat.
        let absurd = NegotiatedLimits::new().with_heartbeat_interval(DurationMs::MAX);
        assert_eq!(absurd.repaired().keepalive_timeout, DurationMs::MAX);
    }

    #[test]
    fn negotiation_repairs_the_result_too() {
        let left = NegotiatedLimits::new()
            .with_heartbeat_interval(DurationMs::from_secs(5))
            .with_keepalive_timeout(DurationMs::from_secs(30));
        let right = NegotiatedLimits::new()
            .with_heartbeat_interval(DurationMs::from_secs(5))
            .with_keepalive_timeout(DurationMs::from_secs(6));
        let agreed = left.negotiate(&right);
        assert!(agreed.keepalive_timeout.as_millis() >= 10_000);
    }

    #[test]
    fn the_payload_ceiling_cannot_exceed_the_header_field() {
        let absurd = NegotiatedLimits::new().with_max_payload_bytes(u64::MAX);
        assert_eq!(
            absurd.max_payload_bytes, MAX_SUPPORTED_PAYLOAD_BYTES as u64,
            "the len field is 32 bits wide"
        );
        assert_eq!(
            absurd.to_frame_limits().max_payload_bytes(),
            MAX_SUPPORTED_PAYLOAD_BYTES
        );
    }

    #[test]
    fn permits_is_a_one_way_comparison() {
        let offered = NegotiatedLimits::new();
        let granted = NegotiatedLimits::new().with_max_payload_bytes(1 << 20);
        assert!(offered.permits(&granted));
        assert!(!granted.permits(&offered));
        assert!(offered.permits(&offered));
    }

    #[test]
    fn frame_limits_convert_both_ways() {
        let limits = NegotiatedLimits::new()
            .with_max_payload_bytes(4096)
            .with_require_crc(true);
        let frame_limits = limits.to_frame_limits();
        assert_eq!(frame_limits.max_payload_bytes(), 4096);
        assert!(frame_limits.require_crc());

        let back = NegotiatedLimits::from_frame_limits(&frame_limits);
        assert_eq!(back.max_payload_bytes, 4096);
        assert!(back.require_crc);
    }

    #[test]
    fn the_wire_form_round_trips() {
        let limits = NegotiatedLimits::network()
            .with_max_routes(7)
            .with_max_subscriptions(3);
        assert_eq!(round_trip(&limits).unwrap(), limits);
        let json = serde_json::to_string(&limits).unwrap();
        assert_eq!(
            serde_json::from_str::<NegotiatedLimits>(&json).unwrap(),
            limits
        );
    }

    #[test]
    fn display_mentions_every_dimension() {
        let text = NegotiatedLimits::network().to_string();
        for needle in ["B/frame", "routes", "in flight", "subscriptions", "crc"] {
            assert!(text.contains(needle), "{text} is missing {needle}");
        }
    }
}
