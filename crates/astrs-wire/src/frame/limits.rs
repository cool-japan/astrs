//! Frame size and integrity limits.
//!
//! Blueprint §7.1: *"Max frame 64 MiB (config), CRC mandatory on network legs,
//! optional on UDS."* [`FrameLimits`] is that configuration, and it is applied
//! symmetrically — an oversize frame is refused when **encoding** as well as
//! when decoding, so a local bug cannot put a frame on the wire that the peer
//! is contractually obliged to reject.
//!
//! The decode path checks the declared length *before sizing any buffer*: a
//! ten-byte header can never provoke a large allocation.

use crate::error::{WireError, WireResult};
use crate::frame::header::{CRC_LEN, HEADER_LEN};

/// The default maximum payload size: 64 MiB (blueprint §24.2).
pub const DEFAULT_MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

/// The largest payload the `len:u32` header field can describe.
///
/// A [`FrameLimits`] can never be configured above this, whatever the caller
/// asks for.
pub const MAX_SUPPORTED_PAYLOAD_BYTES: usize = u32::MAX as usize;

/// The smallest useful payload cap.
///
/// Zero-length payloads are legal (a `Ping` carries nothing), so the floor is
/// zero; this constant exists so callers can name the degenerate case in
/// tests without a magic literal.
pub const MIN_MAX_PAYLOAD_BYTES: usize = 0;

/// Size and integrity policy for one connection.
///
/// # Examples
///
/// ```
/// use astrs_wire::FrameLimits;
///
/// // The default: 64 MiB payloads, checksum optional.
/// let limits = FrameLimits::default();
/// assert_eq!(limits.max_payload_bytes(), 64 * 1024 * 1024);
/// assert!(!limits.require_crc());
///
/// // A network leg insists on the checksum.
/// let network = FrameLimits::network();
/// assert!(network.require_crc());
///
/// // Limits are clamped to what the header can express.
/// let clamped = FrameLimits::default().with_max_payload_bytes(usize::MAX);
/// assert_eq!(clamped.max_payload_bytes(), u32::MAX as usize);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameLimits {
    /// Largest payload accepted or produced, in bytes.
    max_payload_bytes: usize,
    /// Whether a frame without a CRC-32C trailer is refused.
    require_crc: bool,
}

impl FrameLimits {
    /// The default policy: [`DEFAULT_MAX_PAYLOAD_BYTES`], CRC optional.
    ///
    /// This is what [`Default`] returns; it is a `const fn` so it can be used
    /// in constant position.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_payload_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
            require_crc: false,
        }
    }

    /// The policy for network legs (TCP/QUIC): CRC mandatory.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FrameLimits;
    ///
    /// assert!(FrameLimits::network().require_crc());
    /// ```
    #[must_use]
    pub const fn network() -> Self {
        Self {
            max_payload_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
            require_crc: true,
        }
    }

    /// The policy for Unix-domain-socket legs: CRC optional.
    ///
    /// The kernel already guarantees integrity across a UDS, so the checksum
    /// is pure overhead on the node↔daemon hot path.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FrameLimits;
    ///
    /// assert!(!FrameLimits::uds().require_crc());
    /// ```
    #[must_use]
    pub const fn uds() -> Self {
        Self::new()
    }

    /// Replaces the payload cap, clamped to [`MAX_SUPPORTED_PAYLOAD_BYTES`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FrameLimits;
    ///
    /// let tiny = FrameLimits::default().with_max_payload_bytes(1024);
    /// assert_eq!(tiny.max_payload_bytes(), 1024);
    /// ```
    #[must_use]
    pub const fn with_max_payload_bytes(mut self, bytes: usize) -> Self {
        self.max_payload_bytes = if bytes > MAX_SUPPORTED_PAYLOAD_BYTES {
            MAX_SUPPORTED_PAYLOAD_BYTES
        } else {
            bytes
        };
        self
    }

    /// Replaces the CRC requirement.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FrameLimits;
    ///
    /// assert!(FrameLimits::default().with_require_crc(true).require_crc());
    /// ```
    #[must_use]
    pub const fn with_require_crc(mut self, required: bool) -> Self {
        self.require_crc = required;
        self
    }

    /// The configured payload cap in bytes.
    #[must_use]
    pub const fn max_payload_bytes(&self) -> usize {
        self.max_payload_bytes
    }

    /// Whether frames must carry a CRC-32C trailer.
    #[must_use]
    pub const fn require_crc(&self) -> bool {
        self.require_crc
    }

    /// The largest complete frame this policy admits: header + payload + a
    /// checksum trailer.
    ///
    /// Useful for sizing a read buffer once, up front.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameLimits, HEADER_LEN};
    ///
    /// let limits = FrameLimits::default().with_max_payload_bytes(100);
    /// assert_eq!(limits.max_frame_bytes(), HEADER_LEN + 100 + 4);
    /// ```
    #[must_use]
    pub const fn max_frame_bytes(&self) -> usize {
        HEADER_LEN + self.max_payload_bytes + CRC_LEN
    }

    /// Checks a payload length against the cap.
    ///
    /// Called on the decode path with the *declared* length, before any
    /// allocation, and on the encode path with the *measured* length.
    ///
    /// # Errors
    ///
    /// [`WireError::FrameTooLarge`] when `len` exceeds the cap.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameLimits, WireError};
    ///
    /// let limits = FrameLimits::default().with_max_payload_bytes(16);
    /// assert!(limits.check_payload_len(16).is_ok());
    /// assert!(matches!(
    ///     limits.check_payload_len(17),
    ///     Err(WireError::FrameTooLarge { len: 17, max: 16 })
    /// ));
    /// ```
    pub const fn check_payload_len(&self, len: usize) -> WireResult<()> {
        if len > self.max_payload_bytes {
            return Err(WireError::FrameTooLarge {
                len,
                max: self.max_payload_bytes,
            });
        }
        Ok(())
    }

    /// Narrows `self` to the stricter of `self` and `other` on every axis.
    ///
    /// This is the rule the handshake uses: the negotiated limit for a
    /// connection is the minimum of what each side offered, and CRC becomes
    /// mandatory if *either* side requires it.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FrameLimits;
    ///
    /// let ours = FrameLimits::default().with_max_payload_bytes(1_000);
    /// let theirs = FrameLimits::network().with_max_payload_bytes(500);
    /// let agreed = ours.intersect(theirs);
    /// assert_eq!(agreed.max_payload_bytes(), 500);
    /// assert!(agreed.require_crc());
    /// ```
    #[must_use]
    pub const fn intersect(self, other: Self) -> Self {
        let max_payload_bytes = if self.max_payload_bytes < other.max_payload_bytes {
            self.max_payload_bytes
        } else {
            other.max_payload_bytes
        };
        Self {
            max_payload_bytes,
            require_crc: self.require_crc || other.require_crc,
        }
    }
}

impl Default for FrameLimits {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn defaults_match_the_blueprint() {
        let limits = FrameLimits::default();
        assert_eq!(limits.max_payload_bytes(), 64 * 1024 * 1024);
        assert_eq!(limits.max_payload_bytes(), DEFAULT_MAX_PAYLOAD_BYTES);
        assert!(!limits.require_crc());
        assert_eq!(FrameLimits::new(), limits);
        assert_eq!(FrameLimits::uds(), limits);
    }

    #[test]
    fn network_requires_crc_but_keeps_the_size_cap() {
        let network = FrameLimits::network();
        assert!(network.require_crc());
        assert_eq!(network.max_payload_bytes(), DEFAULT_MAX_PAYLOAD_BYTES);
    }

    #[test]
    fn builders_are_independent() {
        let limits = FrameLimits::default()
            .with_max_payload_bytes(4096)
            .with_require_crc(true);
        assert_eq!(limits.max_payload_bytes(), 4096);
        assert!(limits.require_crc());

        let relaxed = limits.with_require_crc(false);
        assert_eq!(relaxed.max_payload_bytes(), 4096);
        assert!(!relaxed.require_crc());
    }

    #[test]
    fn cap_is_clamped_to_the_header_field_width() {
        for request in [usize::MAX, MAX_SUPPORTED_PAYLOAD_BYTES + 1] {
            let limits = FrameLimits::default().with_max_payload_bytes(request);
            assert_eq!(limits.max_payload_bytes(), MAX_SUPPORTED_PAYLOAD_BYTES);
        }
        let exact = FrameLimits::default().with_max_payload_bytes(MAX_SUPPORTED_PAYLOAD_BYTES);
        assert_eq!(exact.max_payload_bytes(), MAX_SUPPORTED_PAYLOAD_BYTES);
    }

    #[test]
    fn zero_cap_admits_only_empty_payloads() {
        let limits = FrameLimits::default().with_max_payload_bytes(MIN_MAX_PAYLOAD_BYTES);
        assert!(limits.check_payload_len(0).is_ok());
        assert!(limits.check_payload_len(1).is_err());
    }

    #[test]
    fn boundary_lengths_are_inclusive() {
        let limits = FrameLimits::default().with_max_payload_bytes(10);
        assert!(limits.check_payload_len(9).is_ok());
        assert!(limits.check_payload_len(10).is_ok());
        match limits.check_payload_len(11) {
            Err(WireError::FrameTooLarge { len, max }) => {
                assert_eq!(len, 11);
                assert_eq!(max, 10);
            }
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn max_frame_bytes_accounts_for_header_and_trailer() {
        let limits = FrameLimits::default().with_max_payload_bytes(1000);
        assert_eq!(limits.max_frame_bytes(), HEADER_LEN + 1000 + CRC_LEN);
    }

    #[test]
    fn intersect_takes_the_stricter_side() {
        let a = FrameLimits::default().with_max_payload_bytes(100);
        let b = FrameLimits::default()
            .with_max_payload_bytes(200)
            .with_require_crc(true);

        let ab = a.intersect(b);
        let ba = b.intersect(a);
        assert_eq!(ab, ba, "intersection is commutative");
        assert_eq!(ab.max_payload_bytes(), 100);
        assert!(ab.require_crc());
    }

    #[test]
    fn intersect_is_idempotent() {
        let limits = FrameLimits::network().with_max_payload_bytes(77);
        assert_eq!(limits.intersect(limits), limits);
    }
}
