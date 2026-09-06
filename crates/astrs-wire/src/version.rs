//! Protocol and build versions, and the rule that reconciles two peers.
//!
//! Blueprint §3.4 and §7.2: *"A single `PROTOCOL_VERSION` is negotiated at
//! handshake on every leg."* Two numbers travel in [`crate::Hello`]:
//!
//! - [`PROTOCOL_VERSION`] — the *wire contract* version. It advances only when
//!   the message families change in a way peers must agree about, and it is
//!   the number that is negotiated.
//! - [`AstrsVersion`] — the *build* version (`CARGO_PKG_VERSION`). It is
//!   informational: reported in `astrs list`, logged on connect, never used to
//!   gate compatibility. Two builds of AstRS that speak the same protocol
//!   interoperate, whatever their patch levels.
//!
//! # Negotiation
//!
//! Each side offers the highest protocol it implements and the lowest it still
//! accepts. The agreed version is the highest both support; if the ranges do
//! not overlap, the connection is refused with a typed
//! [`crate::Refused`] carrying the responder's maximum, so the initiator can
//! report something better than "handshake failed".
//!
//! # Examples
//!
//! ```
//! use astrs_wire::version::{MIN_SUPPORTED_PROTOCOL, PROTOCOL_VERSION, negotiate_protocol};
//!
//! // Same version on both sides.
//! assert_eq!(negotiate_protocol(PROTOCOL_VERSION)?, PROTOCOL_VERSION);
//!
//! // A newer peer is met at our maximum.
//! assert_eq!(negotiate_protocol(PROTOCOL_VERSION + 5)?, PROTOCOL_VERSION);
//!
//! // A peer below our floor is refused.
//! if MIN_SUPPORTED_PROTOCOL > 0 {
//!     assert!(negotiate_protocol(MIN_SUPPORTED_PROTOCOL - 1).is_err());
//! }
//! # Ok::<(), astrs_wire::version::ProtocolMismatch>(())
//! ```

use core::fmt;
use core::str::FromStr;

use oxicode::de::{Decode, Decoder};
use oxicode::enc::{Encode, Encoder};
use semver::Version;
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::codec_invalid;

/// The wire protocol version this build implements.
///
/// Bumping this is a deliberate, recorded change: it means a peer speaking the
/// previous version can no longer be understood without a compatibility shim.
/// Adding a variant to the tail of a `#[non_exhaustive]` message enum does
/// *not* require a bump — that is what append-only evolution buys.
pub const PROTOCOL_VERSION: u16 = 1;

/// The oldest protocol version this build still accepts.
///
/// Equal to [`PROTOCOL_VERSION`] for the initial release: there is nothing
/// older to be compatible with.
pub const MIN_SUPPORTED_PROTOCOL: u16 = 1;

// The two protocol constants are pure compile-time facts, so their coherence is
// enforced at compile time rather than by a test that can only ever pass.
const _: () = assert!(
    MIN_SUPPORTED_PROTOCOL <= PROTOCOL_VERSION,
    "the oldest accepted protocol cannot be newer than the one we speak"
);
const _: () = assert!(
    PROTOCOL_VERSION >= 1,
    "protocol 0 is not a released version"
);

/// The build version string, from `CARGO_PKG_VERSION`.
pub const ASTRS_VERSION_STR: &str = env!("CARGO_PKG_VERSION");

/// Why two peers could not agree on a protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProtocolMismatch {
    /// The peer speaks a protocol older than [`MIN_SUPPORTED_PROTOCOL`].
    #[error("peer speaks protocol {peer}, below the minimum supported {minimum}")]
    TooOld {
        /// The version the peer offered.
        peer: u16,
        /// This build's floor.
        minimum: u16,
    },
}

/// Reconciles this build's protocol range with a peer's offer.
///
/// Returns the version both sides will use: the lower of the peer's offer and
/// [`PROTOCOL_VERSION`].
///
/// # Errors
///
/// [`ProtocolMismatch::TooOld`] when the peer's offer is below
/// [`MIN_SUPPORTED_PROTOCOL`].
///
/// # Examples
///
/// ```
/// use astrs_wire::version::{PROTOCOL_VERSION, negotiate_protocol};
///
/// assert_eq!(negotiate_protocol(u16::MAX)?, PROTOCOL_VERSION);
/// assert!(negotiate_protocol(0).is_err());
/// # Ok::<(), astrs_wire::version::ProtocolMismatch>(())
/// ```
pub const fn negotiate_protocol(peer: u16) -> Result<u16, ProtocolMismatch> {
    if peer < MIN_SUPPORTED_PROTOCOL {
        return Err(ProtocolMismatch::TooOld {
            peer,
            minimum: MIN_SUPPORTED_PROTOCOL,
        });
    }
    Ok(if peer < PROTOCOL_VERSION {
        peer
    } else {
        PROTOCOL_VERSION
    })
}

/// Whether this build can speak `protocol` at all.
///
/// # Examples
///
/// ```
/// use astrs_wire::version::{PROTOCOL_VERSION, supports_protocol};
///
/// assert!(supports_protocol(PROTOCOL_VERSION));
/// assert!(!supports_protocol(0));
/// ```
#[must_use]
pub const fn supports_protocol(protocol: u16) -> bool {
    protocol >= MIN_SUPPORTED_PROTOCOL && protocol <= PROTOCOL_VERSION
}

/// An AstRS build version — a `semver::Version` that travels on the wire.
///
/// Encoded as its **string form**, not as a `(major, minor, patch)` triple:
/// the triple silently drops pre-release and build metadata, so
/// `0.2.0-rc.1` and `0.2.0` would become indistinguishable exactly when the
/// difference matters most.
///
/// # Examples
///
/// ```
/// use astrs_wire::AstrsVersion;
///
/// let version: AstrsVersion = "0.1.0-rc.2+build.7".parse()?;
/// assert_eq!(version.major(), 0);
/// assert_eq!(version.minor(), 1);
/// assert!(version.is_prerelease());
/// assert_eq!(version.to_string(), "0.1.0-rc.2+build.7");
/// # Ok::<(), semver::Error>(())
/// ```
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct AstrsVersion(Version);

impl AstrsVersion {
    /// This build's version.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::AstrsVersion;
    ///
    /// assert_eq!(AstrsVersion::current().to_string(), env!("CARGO_PKG_VERSION"));
    /// ```
    #[must_use]
    pub fn current() -> Self {
        // `CARGO_PKG_VERSION` is validated as semver by Cargo itself, so the
        // fallback is unreachable in any build Cargo produced. It exists only
        // so this function is total.
        Self(Version::parse(ASTRS_VERSION_STR).unwrap_or_else(|_| Version::new(0, 0, 0)))
    }

    /// Wraps an existing [`Version`].
    #[must_use]
    pub const fn new(version: Version) -> Self {
        Self(version)
    }

    /// Builds a version from its three numeric components.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::AstrsVersion;
    ///
    /// assert_eq!(AstrsVersion::from_parts(1, 2, 3).to_string(), "1.2.3");
    /// ```
    #[must_use]
    pub fn from_parts(major: u64, minor: u64, patch: u64) -> Self {
        Self(Version::new(major, minor, patch))
    }

    /// Parses a semver string.
    ///
    /// # Errors
    ///
    /// [`semver::Error`] if the string is not valid semver.
    pub fn parse(text: &str) -> Result<Self, semver::Error> {
        Ok(Self(Version::parse(text)?))
    }

    /// The underlying [`Version`].
    #[must_use]
    pub const fn as_version(&self) -> &Version {
        &self.0
    }

    /// Consumes the wrapper and returns the underlying [`Version`].
    #[must_use]
    pub fn into_version(self) -> Version {
        self.0
    }

    /// The major component.
    #[must_use]
    pub const fn major(&self) -> u64 {
        self.0.major
    }

    /// The minor component.
    #[must_use]
    pub const fn minor(&self) -> u64 {
        self.0.minor
    }

    /// The patch component.
    #[must_use]
    pub const fn patch(&self) -> u64 {
        self.0.patch
    }

    /// Whether this version carries a pre-release tag.
    #[must_use]
    pub fn is_prerelease(&self) -> bool {
        !self.0.pre.is_empty()
    }

    /// Whether two builds are API-compatible under Cargo's semver rules.
    ///
    /// This is *not* what gates a connection — [`PROTOCOL_VERSION`] is — but
    /// the CLI reports it in `astrs doctor` so a mixed-version cluster is
    /// visible before it becomes confusing.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::AstrsVersion;
    ///
    /// let a = AstrsVersion::from_parts(0, 1, 0);
    /// let b = AstrsVersion::from_parts(0, 1, 5);
    /// let c = AstrsVersion::from_parts(0, 2, 0);
    /// assert!(a.is_compatible_with(&b));
    /// assert!(!a.is_compatible_with(&c));
    ///
    /// let one = AstrsVersion::from_parts(1, 0, 0);
    /// let one_three = AstrsVersion::from_parts(1, 3, 0);
    /// assert!(one.is_compatible_with(&one_three));
    /// ```
    #[must_use]
    pub fn is_compatible_with(&self, other: &Self) -> bool {
        if self.major() != other.major() {
            return false;
        }
        // Below 1.0.0 the minor component carries the breaking changes.
        self.major() != 0 || self.minor() == other.minor()
    }
}

impl fmt::Display for AstrsVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Debug for AstrsVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AstrsVersion({})", self.0)
    }
}

impl Default for AstrsVersion {
    fn default() -> Self {
        Self::current()
    }
}

impl FromStr for AstrsVersion {
    type Err = semver::Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl From<Version> for AstrsVersion {
    fn from(version: Version) -> Self {
        Self(version)
    }
}

impl From<AstrsVersion> for Version {
    fn from(version: AstrsVersion) -> Self {
        version.0
    }
}

impl<'de> Deserialize<'de> for AstrsVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(Version::deserialize(deserializer)?))
    }
}

impl Encode for AstrsVersion {
    /// Encodes the full semver string, pre-release and build metadata
    /// included.
    fn encode<E: Encoder>(&self, encoder: &mut E) -> Result<(), oxicode::error::Error> {
        self.0.to_string().encode(encoder)
    }
}

impl<Context> Decode<Context> for AstrsVersion {
    fn decode<D: Decoder<Context = Context>>(
        decoder: &mut D,
    ) -> Result<Self, oxicode::error::Error> {
        let text = String::decode(decoder)?;
        // Cap the string before parsing: a semver string is short, and a peer
        // that sends a megabyte of digits should be rejected, not parsed.
        if text.len() > MAX_VERSION_TEXT_LEN {
            return Err(codec_invalid(format!(
                "version string of {} bytes exceeds the {MAX_VERSION_TEXT_LEN}-byte limit",
                text.len()
            )));
        }
        Version::parse(&text)
            .map(Self)
            .map_err(|err| codec_invalid(format!("invalid semver `{text}`: {err}")))
    }
}

/// The longest version string accepted off the wire.
///
/// Semver allows arbitrarily long pre-release and build metadata; 128 bytes is
/// far beyond anything a real release uses and bounds the parse.
pub const MAX_VERSION_TEXT_LEN: usize = 128;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireDecode, WireEncode};

    #[test]
    fn protocol_constants_are_coherent() {
        // The ordering and lower-bound invariants are enforced at compile time
        // by the `const _` assertions beside the constants. What is worth
        // checking here is that the runtime predicate agrees with them.
        assert!(supports_protocol(MIN_SUPPORTED_PROTOCOL));
        assert!(supports_protocol(PROTOCOL_VERSION));
        assert!(!supports_protocol(PROTOCOL_VERSION + 1));
    }

    #[test]
    fn negotiation_meets_at_the_lower_version() {
        assert_eq!(
            negotiate_protocol(PROTOCOL_VERSION).unwrap(),
            PROTOCOL_VERSION
        );
        assert_eq!(negotiate_protocol(u16::MAX).unwrap(), PROTOCOL_VERSION);
        assert_eq!(
            negotiate_protocol(MIN_SUPPORTED_PROTOCOL).unwrap(),
            MIN_SUPPORTED_PROTOCOL
        );
    }

    #[test]
    fn negotiation_refuses_versions_below_the_floor() {
        for peer in 0..MIN_SUPPORTED_PROTOCOL {
            match negotiate_protocol(peer) {
                Err(ProtocolMismatch::TooOld { peer: p, minimum }) => {
                    assert_eq!(p, peer);
                    assert_eq!(minimum, MIN_SUPPORTED_PROTOCOL);
                }
                other => panic!("protocol {peer} should be refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn support_predicate_matches_negotiation() {
        for protocol in 0..=(PROTOCOL_VERSION + 3) {
            let negotiable = negotiate_protocol(protocol)
                .map(|agreed| agreed == protocol)
                .unwrap_or(false);
            assert_eq!(
                supports_protocol(protocol),
                negotiable,
                "protocol {protocol}"
            );
        }
    }

    #[test]
    fn current_matches_the_package_version() {
        assert_eq!(AstrsVersion::current().to_string(), ASTRS_VERSION_STR);
        assert_eq!(AstrsVersion::default(), AstrsVersion::current());
        assert!(AstrsVersion::current() > AstrsVersion::from_parts(0, 0, 0));
    }

    #[test]
    fn components_are_exposed() {
        let version = AstrsVersion::parse("2.3.4-rc.1+meta").unwrap();
        assert_eq!(version.major(), 2);
        assert_eq!(version.minor(), 3);
        assert_eq!(version.patch(), 4);
        assert!(version.is_prerelease());
        assert!(!AstrsVersion::from_parts(2, 3, 4).is_prerelease());
        assert_eq!(version.as_version().major, 2);
        assert_eq!(Version::from(version.clone()), version.into_version());
    }

    #[test]
    fn compatibility_follows_cargo_semver_rules() {
        let zero_one = AstrsVersion::from_parts(0, 1, 0);
        assert!(zero_one.is_compatible_with(&AstrsVersion::from_parts(0, 1, 9)));
        assert!(!zero_one.is_compatible_with(&AstrsVersion::from_parts(0, 2, 0)));
        assert!(!zero_one.is_compatible_with(&AstrsVersion::from_parts(1, 1, 0)));

        let one_two = AstrsVersion::from_parts(1, 2, 0);
        assert!(one_two.is_compatible_with(&AstrsVersion::from_parts(1, 9, 9)));
        assert!(!one_two.is_compatible_with(&AstrsVersion::from_parts(2, 0, 0)));
    }

    #[test]
    fn codec_preserves_prerelease_and_build_metadata() {
        for text in [
            "0.1.0",
            "0.1.0-rc.1",
            "0.1.0+build.7",
            "1.0.0-alpha.1+sha.abcdef",
            "18446744073709551615.0.0",
        ] {
            let version = AstrsVersion::parse(text).unwrap();
            let bytes = version.encode_to_vec().unwrap();
            let decoded = AstrsVersion::decode_exact(&bytes).unwrap();
            assert_eq!(decoded, version);
            assert_eq!(decoded.to_string(), text);
        }
    }

    #[test]
    fn decoding_rejects_a_non_semver_string() {
        let hostile = "not.a.version".to_owned().encode_to_vec().unwrap();
        assert!(AstrsVersion::decode_exact(&hostile).is_err());
    }

    #[test]
    fn decoding_rejects_an_oversize_version_string() {
        let hostile = format!("1.0.0+{}", "a".repeat(MAX_VERSION_TEXT_LEN))
            .encode_to_vec()
            .unwrap();
        assert!(AstrsVersion::decode_exact(&hostile).is_err());
    }

    #[test]
    fn ordering_follows_semver_precedence() {
        let mut versions = [
            AstrsVersion::parse("1.0.0").unwrap(),
            AstrsVersion::parse("1.0.0-rc.1").unwrap(),
            AstrsVersion::parse("0.9.0").unwrap(),
        ];
        versions.sort();
        assert_eq!(versions[0].to_string(), "0.9.0");
        assert_eq!(versions[1].to_string(), "1.0.0-rc.1");
        assert_eq!(versions[2].to_string(), "1.0.0");
    }

    #[test]
    fn serde_uses_the_string_form() {
        let version = AstrsVersion::parse("0.1.0-rc.2").unwrap();
        let json = serde_json::to_string(&version).unwrap();
        assert_eq!(json, "\"0.1.0-rc.2\"");
        assert_eq!(
            serde_json::from_str::<AstrsVersion>(&json).unwrap(),
            version
        );
    }

    #[test]
    fn conversions_and_formatting() {
        let version = AstrsVersion::from(Version::new(1, 2, 3));
        assert_eq!(version, AstrsVersion::new(Version::new(1, 2, 3)));
        assert_eq!("1.2.3".parse::<AstrsVersion>().unwrap(), version);
        assert_eq!(format!("{version:?}"), "AstrsVersion(1.2.3)");
        assert!(AstrsVersion::parse("nope").is_err());
    }
}
