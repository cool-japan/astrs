//! [`RouteId`]: the per-connection handle that identifies an established
//! route.
//!
//! A route is fully described by a [`crate::RouteKey`] — dataflow id, producer
//! port, consumer port — but that description costs sixteen bytes plus four
//! strings on the wire. Repeating it on every payload frame of a 30 Hz camera
//! topic is exactly the kind of per-message overhead blueprint §3 (principle 7,
//! "performance is a feature of the design") rules out.
//!
//! So the two daemons agree on a short handle once, during route setup
//! ([`crate::PeerEvent::RouteSetup`] → [`crate::PeerEvent::RouteAccept`]), and
//! every subsequent [`crate::PeerEvent::Output`] carries the handle instead of
//! the key. A `RouteId` is meaningful **only** on the connection that minted
//! it: the same numeric value on two different daemon links refers to two
//! different routes, exactly like a file descriptor.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::RouteId;
//!
//! let first = RouteId::FIRST;
//! assert_eq!(first.get(), 1);
//! assert_eq!(first.next().get(), 2);
//! assert!(RouteId::NONE.is_none());
//! ```

use core::fmt;
use core::str::FromStr;

use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// A connection-scoped handle for one established route.
///
/// Zero ([`RouteId::NONE`]) is reserved and never names a live route, so a
/// zeroed struct cannot accidentally address one.
///
/// # Examples
///
/// ```
/// use astrs_wire::RouteId;
///
/// let mut next = RouteId::FIRST;
/// let camera = next;
/// next = next.next();
/// assert_ne!(camera, next);
/// assert_eq!(camera.to_string(), "1");
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
#[serde(transparent)]
pub struct RouteId(u64);

impl RouteId {
    /// The reserved "no route" value.
    pub const NONE: Self = Self(0);

    /// The first handle an allocator hands out.
    pub const FIRST: Self = Self(1);

    /// Wraps a raw handle value.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::RouteId;
    ///
    /// assert_eq!(RouteId::new(7).get(), 7);
    /// ```
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw handle value.
    #[must_use]
    pub const fn get(&self) -> u64 {
        self.0
    }

    /// Whether this is the reserved "no route" value.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::RouteId;
    ///
    /// assert!(RouteId::NONE.is_none());
    /// assert!(!RouteId::FIRST.is_none());
    /// ```
    #[must_use]
    pub const fn is_none(&self) -> bool {
        self.0 == 0
    }

    /// The next handle in sequence.
    ///
    /// Saturates at [`u64::MAX`] rather than wrapping: a reused handle would
    /// deliver a camera frame to whichever route inherited the number, which
    /// is worse than refusing to open route 2^64 on a single connection.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::RouteId;
    ///
    /// assert_eq!(RouteId::new(1).next().get(), 2);
    /// assert_eq!(RouteId::new(u64::MAX).next().get(), u64::MAX);
    /// ```
    #[must_use]
    pub const fn next(&self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for RouteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl From<u64> for RouteId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<RouteId> for u64 {
    fn from(id: RouteId) -> Self {
        id.0
    }
}

impl FromStr for RouteId {
    type Err = core::num::ParseIntError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(value.parse()?))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireDecode, WireEncode};

    #[test]
    fn reserved_values_are_what_the_docs_claim() {
        assert_eq!(RouteId::NONE.get(), 0);
        assert_eq!(RouteId::FIRST.get(), 1);
        assert!(RouteId::NONE.is_none());
        assert!(!RouteId::FIRST.is_none());
        assert_eq!(RouteId::default(), RouteId::NONE);
    }

    #[test]
    fn allocation_is_monotone_and_saturating() {
        let mut id = RouteId::FIRST;
        for expected in 1..64u64 {
            assert_eq!(id.get(), expected);
            id = id.next();
        }
        assert_eq!(RouteId::new(u64::MAX).next(), RouteId::new(u64::MAX));
    }

    #[test]
    fn ordering_follows_the_raw_value() {
        assert!(RouteId::new(1) < RouteId::new(2));
        assert!(RouteId::new(u64::MAX) > RouteId::new(0));
    }

    #[test]
    fn text_and_numeric_conversions_round_trip() {
        for raw in [0u64, 1, 42, u64::MAX] {
            let id = RouteId::from(raw);
            assert_eq!(u64::from(id), raw);
            assert_eq!(id.to_string(), raw.to_string());
            assert_eq!(id.to_string().parse::<RouteId>().unwrap(), id);
        }
        assert!("not-a-number".parse::<RouteId>().is_err());
    }

    #[test]
    fn wire_encoding_is_a_varint() {
        // Small handles — the common case on a link with a handful of routes —
        // cost a single byte.
        assert_eq!(RouteId::FIRST.encode_to_vec().unwrap(), vec![1]);
        for raw in [0u64, 1, 250, 251, u64::MAX] {
            let id = RouteId::new(raw);
            let bytes = id.encode_to_vec().unwrap();
            assert_eq!(bytes.len(), id.encode_size_hint().unwrap());
            assert_eq!(RouteId::decode_exact(&bytes).unwrap(), id);
        }
    }

    #[test]
    fn serde_is_transparent() {
        let json = serde_json::to_string(&RouteId::new(9)).unwrap();
        assert_eq!(json, "9");
        assert_eq!(
            serde_json::from_str::<RouteId>(&json).unwrap(),
            RouteId::new(9)
        );
    }
}
