//! The transformation kinds, the protection levels, and the AAD profile.
//!
//! # Transformation kinds
//!
//! DDS-Security 1.1 §9.5.2.1.1 defines `CryptoTransformKind` as four octets
//! and assigns five values. The last octet carries the value and the first
//! three are zero, which is why [`TransformationKind::to_octets`] writes them
//! big-endian and never as a native integer.
//!
//! | Octets | Name | AES key | What it does |
//! |---|---|---|---|
//! | `00 00 00 00` | `NONE` | — | nothing; the submessage is in the clear |
//! | `00 00 00 01` | `AES128_GMAC` | 16 | authenticates, does not encrypt |
//! | `00 00 00 02` | `AES128_GCM` | 16 | encrypts and authenticates |
//! | `00 00 00 03` | `AES256_GMAC` | 32 | authenticates, does not encrypt |
//! | `00 00 00 04` | `AES256_GCM` | 32 | encrypts and authenticates |
//!
//! GMAC is not a separate primitive here and is not implemented by hand: it
//! is AES-GCM run with an empty plaintext and the octets to be authenticated
//! passed as additional authenticated data, which is the definition of GMAC
//! (NIST SP 800-38D §3). Both modes therefore reach `oxicrypto` through the
//! same `Aead` implementation, and neither this crate nor any test contains a
//! block cipher, a hash or a MAC.
//!
//! # The AAD profile
//!
//! [`AadBinding`] exists because the specification and good practice disagree,
//! and the disagreement is worth being explicit about rather than burying in
//! a `seal` call:
//!
//! - [`AadBinding::SpecEmpty`] is what DDS-Security prescribes for the GCM
//!   kinds: the plaintext is the submessage and the AAD is empty, so the
//!   crypto header — the key id, the session id and the initialisation
//!   vector — is *not* authenticated.
//! - [`AadBinding::HeaderBound`] additionally passes the twenty-octet crypto
//!   header as AAD, so a header spliced from another session or another key
//!   makes the tag fail rather than being silently accepted.
//!
//! This release defaults to [`AadBinding::HeaderBound`]. See the
//! [module documentation](super) for what that means for interoperability and
//! for the one line that changes it back.

use core::fmt;

use crate::security::error::{SecurityError, SecurityResult};

/// Octets a `CryptoTransformKind` occupies (§9.5.2.1.1).
pub const TRANSFORMATION_KIND_LEN: usize = 4;

/// Which AES-GCM transformation protects a submessage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[non_exhaustive]
pub enum TransformationKind {
    /// `CRYPTO_TRANSFORMATION_KIND_NONE`.
    #[default]
    None,
    /// `CRYPTO_TRANSFORMATION_KIND_AES128_GMAC`.
    Aes128Gmac,
    /// `CRYPTO_TRANSFORMATION_KIND_AES128_GCM`.
    Aes128Gcm,
    /// `CRYPTO_TRANSFORMATION_KIND_AES256_GMAC`.
    Aes256Gmac,
    /// `CRYPTO_TRANSFORMATION_KIND_AES256_GCM`.
    Aes256Gcm,
}

impl TransformationKind {
    /// Every kind, in the order §9.5.2.1.1 assigns them.
    pub const ALL: [Self; 5] = [
        Self::None,
        Self::Aes128Gmac,
        Self::Aes128Gcm,
        Self::Aes256Gmac,
        Self::Aes256Gcm,
    ];

    /// The value the last octet carries.
    #[must_use]
    pub const fn value(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Aes128Gmac => 1,
            Self::Aes128Gcm => 2,
            Self::Aes256Gmac => 3,
            Self::Aes256Gcm => 4,
        }
    }

    /// The four octets as they appear in a crypto header.
    #[must_use]
    pub const fn to_octets(self) -> [u8; TRANSFORMATION_KIND_LEN] {
        [0, 0, 0, self.value()]
    }

    /// Classify four octets from the wire.
    ///
    /// # Errors
    ///
    /// [`SecurityError::UnsupportedTransformation`] for anything outside the
    /// five assigned values, including a value in the last octet with a
    /// nonzero octet before it.
    pub const fn from_octets(octets: [u8; TRANSFORMATION_KIND_LEN]) -> SecurityResult<Self> {
        match octets {
            [0, 0, 0, 0] => Ok(Self::None),
            [0, 0, 0, 1] => Ok(Self::Aes128Gmac),
            [0, 0, 0, 2] => Ok(Self::Aes128Gcm),
            [0, 0, 0, 3] => Ok(Self::Aes256Gmac),
            [0, 0, 0, 4] => Ok(Self::Aes256Gcm),
            kind => Err(SecurityError::UnsupportedTransformation { kind }),
        }
    }

    /// True when the transformation hides the submessage rather than only
    /// authenticating it.
    #[must_use]
    pub const fn is_encrypting(self) -> bool {
        matches!(self, Self::Aes128Gcm | Self::Aes256Gcm)
    }

    /// True when the transformation produces a tag at all.
    #[must_use]
    pub const fn is_protecting(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Octets of AES key the transformation needs.
    ///
    /// Zero for [`TransformationKind::None`], which has no key.
    #[must_use]
    pub const fn key_len(self) -> usize {
        match self {
            Self::None => 0,
            Self::Aes128Gmac | Self::Aes128Gcm => 16,
            Self::Aes256Gmac | Self::Aes256Gcm => 32,
        }
    }

    /// The `oxicrypto` AEAD that implements it.
    ///
    /// Both the GCM and the GMAC kind of a given key length map to the same
    /// AEAD: GMAC is that AEAD with an empty plaintext. Returns `None` for
    /// [`TransformationKind::None`].
    #[must_use]
    pub const fn aead(self) -> Option<oxicrypto::AeadAlgo> {
        match self {
            Self::None => None,
            Self::Aes128Gmac | Self::Aes128Gcm => Some(oxicrypto::AeadAlgo::Aes128Gcm),
            Self::Aes256Gmac | Self::Aes256Gcm => Some(oxicrypto::AeadAlgo::Aes256Gcm),
        }
    }

    /// The specification's name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "CRYPTO_TRANSFORMATION_KIND_NONE",
            Self::Aes128Gmac => "CRYPTO_TRANSFORMATION_KIND_AES128_GMAC",
            Self::Aes128Gcm => "CRYPTO_TRANSFORMATION_KIND_AES128_GCM",
            Self::Aes256Gmac => "CRYPTO_TRANSFORMATION_KIND_AES256_GMAC",
            Self::Aes256Gcm => "CRYPTO_TRANSFORMATION_KIND_AES256_GCM",
        }
    }
}

impl fmt::Display for TransformationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How much protection an endpoint asks for.
///
/// The configuration-facing spelling: `none`, `sign` or `encrypt`. Each maps
/// to one [`TransformationKind`], and the mapping is what
/// [`ProtectionKind::transformation`] fixes — AES-256 in both protecting
/// cases, because there is no reason to offer a weaker default and the key
/// schedule cost is not what dominates a datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[non_exhaustive]
pub enum ProtectionKind {
    /// No transform at all. The submessage goes out exactly as an
    /// unprotected participant would send it.
    #[default]
    None,
    /// Authenticate the submessage; leave it readable.
    ///
    /// What a deployment wants when the payload is not secret but its origin
    /// and integrity are: an injected or altered `DATA` is rejected, and a
    /// packet capture still shows what the robot published.
    Sign,
    /// Encrypt and authenticate the submessage.
    Encrypt,
}

impl ProtectionKind {
    /// Every level, weakest first.
    pub const ALL: [Self; 3] = [Self::None, Self::Sign, Self::Encrypt];

    /// The transformation this level uses.
    #[must_use]
    pub const fn transformation(self) -> TransformationKind {
        match self {
            Self::None => TransformationKind::None,
            Self::Sign => TransformationKind::Aes256Gmac,
            Self::Encrypt => TransformationKind::Aes256Gcm,
        }
    }

    /// True when a submessage under this level is transformed at all.
    #[must_use]
    pub const fn is_protecting(self) -> bool {
        !matches!(self, Self::None)
    }

    /// The configuration spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Sign => "sign",
            Self::Encrypt => "encrypt",
        }
    }

    /// Parse the configuration spelling.
    ///
    /// # Errors
    ///
    /// [`SecurityError::Malformed`] for anything but `none`, `sign` or
    /// `encrypt`.
    pub fn parse(text: &str) -> SecurityResult<Self> {
        match text {
            "none" => Ok(Self::None),
            "sign" => Ok(Self::Sign),
            "encrypt" => Ok(Self::Encrypt),
            _ => Err(SecurityError::Malformed {
                reason: "a protection level must be none, sign or encrypt",
            }),
        }
    }
}

impl fmt::Display for ProtectionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the AEAD authenticates besides the submessage.
///
/// See the [module documentation](self) for why this is a choice rather than
/// a constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[non_exhaustive]
pub enum AadBinding {
    /// Bind the crypto header into the additional authenticated data.
    ///
    /// The default, and the stronger option: a crypto header lifted from
    /// another session or another key cannot be spliced onto this ciphertext.
    #[default]
    HeaderBound,
    /// Exactly what DDS-Security 1.1 prescribes: no additional authenticated
    /// data for the GCM kinds.
    ///
    /// Kept because it is the choice a future interoperability mode needs,
    /// and because leaving it out would have made the deviation invisible.
    SpecEmpty,
}

impl AadBinding {
    /// Both profiles.
    pub const ALL: [Self; 2] = [Self::HeaderBound, Self::SpecEmpty];

    /// True when the crypto header is authenticated.
    #[must_use]
    pub const fn binds_header(self) -> bool {
        matches!(self, Self::HeaderBound)
    }

    /// A short name for a log line or a configuration file.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HeaderBound => "header-bound",
            Self::SpecEmpty => "spec-empty",
        }
    }
}

impl fmt::Display for AadBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn every_kind_matches_the_specification_table() {
        // DDS-Security 1.1 §9.5.2.1.1.
        let table = [
            ([0_u8, 0, 0, 0], TransformationKind::None, 0_usize),
            ([0, 0, 0, 1], TransformationKind::Aes128Gmac, 16),
            ([0, 0, 0, 2], TransformationKind::Aes128Gcm, 16),
            ([0, 0, 0, 3], TransformationKind::Aes256Gmac, 32),
            ([0, 0, 0, 4], TransformationKind::Aes256Gcm, 32),
        ];
        assert_eq!(table.len(), TransformationKind::ALL.len());
        for (octets, kind, key_len) in table {
            assert_eq!(TransformationKind::from_octets(octets), Ok(kind));
            assert_eq!(kind.to_octets(), octets);
            assert_eq!(kind.key_len(), key_len);
            assert!(TransformationKind::ALL.contains(&kind));
        }
    }

    #[test]
    fn an_unassigned_kind_is_rejected_rather_than_guessed() {
        for octets in [[0_u8, 0, 0, 5], [0, 0, 0, 0xff], [1, 0, 0, 2], [0, 0, 1, 4]] {
            assert_eq!(
                TransformationKind::from_octets(octets),
                Err(SecurityError::UnsupportedTransformation { kind: octets }),
                "{octets:02x?} must not be resolved to a neighbouring kind"
            );
        }
    }

    #[test]
    fn gmac_and_gcm_of_one_key_length_share_an_aead() {
        assert_eq!(
            TransformationKind::Aes256Gmac.aead(),
            TransformationKind::Aes256Gcm.aead()
        );
        assert_eq!(
            TransformationKind::Aes128Gmac.aead(),
            TransformationKind::Aes128Gcm.aead()
        );
        assert_ne!(
            TransformationKind::Aes128Gcm.aead(),
            TransformationKind::Aes256Gcm.aead()
        );
        assert_eq!(TransformationKind::None.aead(), None);
    }

    #[test]
    fn only_the_gcm_kinds_encrypt() {
        for kind in TransformationKind::ALL {
            assert_eq!(
                kind.is_encrypting(),
                matches!(
                    kind,
                    TransformationKind::Aes128Gcm | TransformationKind::Aes256Gcm
                ),
                "{kind}"
            );
            assert_eq!(kind.is_protecting(), kind != TransformationKind::None);
            assert_eq!(kind.aead().is_some(), kind.is_protecting());
        }
    }

    #[test]
    fn the_protection_levels_map_to_aes_256() {
        assert_eq!(
            ProtectionKind::None.transformation(),
            TransformationKind::None
        );
        assert_eq!(
            ProtectionKind::Sign.transformation(),
            TransformationKind::Aes256Gmac
        );
        assert_eq!(
            ProtectionKind::Encrypt.transformation(),
            TransformationKind::Aes256Gcm
        );
        for level in ProtectionKind::ALL {
            assert_eq!(
                level.is_protecting(),
                level.transformation().is_protecting(),
                "{level}"
            );
            assert_eq!(ProtectionKind::parse(level.as_str()), Ok(level));
            assert_eq!(level.to_string(), level.as_str());
        }
    }

    #[test]
    fn an_unknown_protection_word_is_refused() {
        assert!(ProtectionKind::parse("NONE").is_err());
        assert!(ProtectionKind::parse("").is_err());
        assert!(ProtectionKind::parse("encrypted").is_err());
    }

    #[test]
    fn the_default_aad_profile_binds_the_header() {
        assert_eq!(AadBinding::default(), AadBinding::HeaderBound);
        assert!(AadBinding::HeaderBound.binds_header());
        assert!(!AadBinding::SpecEmpty.binds_header());
        assert_eq!(AadBinding::SpecEmpty.to_string(), "spec-empty");
        assert_eq!(AadBinding::ALL.len(), 2);
    }
}
