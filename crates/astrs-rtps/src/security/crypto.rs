//! The two AEAD calls, and nothing else.
//!
//! Every cryptographic operation this crate performs goes through
//! [`protect`] and [`unprotect`], and both are thin: they choose the
//! `oxicrypto` AEAD the transformation kind names, assemble the additional
//! authenticated data, and call `seal` or `open`. There is no primitive here.
//! There is no fallback, no "if the AEAD is unavailable" branch, and no place
//! a future edit could introduce one without deleting a comment that says so.
//!
//! # GMAC is GCM with an empty plaintext
//!
//! NIST SP 800-38D §3 defines GMAC as the authentication-only mode of GCM:
//! the message to authenticate is passed as additional authenticated data,
//! the plaintext is empty, and the output is the tag. That is exactly what
//! [`protect`] does for the two `_GMAC` kinds, so the signing path and the
//! encrypting path share one implementation and one dependency, and the
//! signing path cannot rot while the encrypting path is exercised.
//!
//! # The additional authenticated data
//!
//! | Kind | AAD | Plaintext | Output |
//! |---|---|---|---|
//! | `AES*_GCM`, [`AadBinding::HeaderBound`] | crypto header (20) | submessage | ciphertext ‖ tag |
//! | `AES*_GCM`, [`AadBinding::SpecEmpty`] | *(empty)* | submessage | ciphertext ‖ tag |
//! | `AES*_GMAC`, [`AadBinding::HeaderBound`] | crypto header ‖ submessage | *(empty)* | tag |
//! | `AES*_GMAC`, [`AadBinding::SpecEmpty`] | submessage | *(empty)* | tag |
//!
//! The GMAC rows have no `SpecEmpty` alternative for the submessage itself —
//! it has to be in the AAD or nothing would be authenticated — so the profile
//! only decides whether the crypto header joins it.

use crate::security::error::{SecurityError, SecurityResult, crypto_reason};
use crate::security::keys::SessionKey;
use crate::security::kind::{AadBinding, TransformationKind};
use crate::security::wire::{COMMON_MAC_LEN, NONCE_LEN};

/// What one protected submessage carries: the payload and the tag.
///
/// `payload` is the ciphertext under a GCM kind and empty under a GMAC one,
/// which is the whole difference between the two wire shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    /// The ciphertext, or empty for an authentication-only transformation.
    pub payload: Vec<u8>,
    /// The AEAD tag.
    pub tag: [u8; COMMON_MAC_LEN],
}

/// Seal `plaintext` under `kind`.
///
/// # Errors
///
/// [`SecurityError::UnsupportedTransformation`] for
/// [`TransformationKind::None`], and [`SecurityError::Cipher`] if the AEAD
/// refuses — which at this layer means a key or nonce of the wrong length,
/// i.e. a bug here rather than hostile input.
pub fn protect(
    kind: TransformationKind,
    key: &SessionKey,
    nonce: &[u8; NONCE_LEN],
    header: &[u8],
    binding: AadBinding,
    plaintext: &[u8],
) -> SecurityResult<Sealed> {
    let algo = kind
        .aead()
        .ok_or(SecurityError::UnsupportedTransformation {
            kind: kind.to_octets(),
        })?;
    let aead = oxicrypto::aead_impl(algo);
    let bound = if binding.binds_header() { header } else { &[] };

    let (aad, message): (Vec<u8>, &[u8]) = if kind.is_encrypting() {
        (bound.to_vec(), plaintext)
    } else {
        let mut aad = Vec::with_capacity(bound.len() + plaintext.len());
        aad.extend_from_slice(bound);
        aad.extend_from_slice(plaintext);
        (aad, &[])
    };

    let mut sealed = aead
        .seal_to_vec(key.expose(), nonce, &aad, message)
        .map_err(|error| SecurityError::Cipher {
            reason: crypto_reason(&error),
        })?;
    if sealed.len() < COMMON_MAC_LEN {
        return Err(SecurityError::Cipher {
            reason: format!(
                "the AEAD produced {} octets, fewer than a tag",
                sealed.len()
            ),
        });
    }
    let tag_at = sealed.len() - COMMON_MAC_LEN;
    let mut tag = [0_u8; COMMON_MAC_LEN];
    tag.copy_from_slice(&sealed[tag_at..]);
    sealed.truncate(tag_at);
    Ok(Sealed {
        payload: sealed,
        tag,
    })
}

/// The octets a protected submessage arrived as.
///
/// A type rather than two slices, because the choice is not the caller's: an
/// encrypting kind *must* bring ciphertext and an authenticating one *must*
/// bring cleartext, and a mismatch is a malformed envelope rather than a
/// combination to handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protected<'a> {
    /// A `SEC_BODY`'s ciphertext, under one of the GCM kinds.
    Ciphertext(&'a [u8]),
    /// The submessage as it travelled, under one of the GMAC kinds.
    Cleartext(&'a [u8]),
}

/// Open what [`protect`] produced, returning the recovered submessage octets.
///
/// Under a GMAC kind the return value is a copy of the cleartext that was
/// authenticated, so the caller has one code path whichever kind arrived.
///
/// # Errors
///
/// [`SecurityError::UnsupportedTransformation`] for
/// [`TransformationKind::None`], [`SecurityError::Malformed`] when the
/// transformation kind and the [`Protected`] shape disagree, and
/// [`SecurityError::AuthenticationFailed`] for every cryptographic failure —
/// a tampered payload, a wrong key, a spliced header. Those are deliberately
/// not distinguished; see [`SecurityError`].
pub fn unprotect(
    kind: TransformationKind,
    key: &SessionKey,
    nonce: &[u8; NONCE_LEN],
    header: &[u8],
    binding: AadBinding,
    protected: Protected<'_>,
    tag: &[u8; COMMON_MAC_LEN],
) -> SecurityResult<Vec<u8>> {
    let algo = kind
        .aead()
        .ok_or(SecurityError::UnsupportedTransformation {
            kind: kind.to_octets(),
        })?;
    let aead = oxicrypto::aead_impl(algo);
    let bound = if binding.binds_header() { header } else { &[] };

    let (aad, sealed, recovered): (Vec<u8>, Vec<u8>, Option<Vec<u8>>) =
        match (kind.is_encrypting(), protected) {
            (true, Protected::Ciphertext(ciphertext)) => {
                let mut sealed = Vec::with_capacity(ciphertext.len() + COMMON_MAC_LEN);
                sealed.extend_from_slice(ciphertext);
                sealed.extend_from_slice(tag);
                (bound.to_vec(), sealed, None)
            }
            (false, Protected::Cleartext(cleartext)) => {
                let mut aad = Vec::with_capacity(bound.len() + cleartext.len());
                aad.extend_from_slice(bound);
                aad.extend_from_slice(cleartext);
                (aad, tag.to_vec(), Some(cleartext.to_vec()))
            }
            (true, Protected::Cleartext(_)) => {
                return Err(SecurityError::Malformed {
                    reason: "an encrypting transformation arrived without a SEC_BODY",
                });
            }
            (false, Protected::Ciphertext(_)) => {
                return Err(SecurityError::Malformed {
                    reason: "an authentication-only transformation arrived as ciphertext",
                });
            }
        };

    let opened = aead
        .open_to_vec(key.expose(), nonce, &aad, &sealed)
        .map_err(|_| SecurityError::AuthenticationFailed)?;
    Ok(recovered.unwrap_or(opened))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::security::keys::{KeyMaterial, Psk};

    fn material(seed: u8) -> KeyMaterial {
        let psk = Psk::new(vec![seed; 32]).expect("valid");
        KeyMaterial::from_psk(&psk, "rt/chatter").expect("derive")
    }

    fn key(kind: TransformationKind, seed: u8) -> SessionKey {
        material(seed).session_key(kind, 0).expect("derive")
    }

    const NONCE: [u8; NONCE_LEN] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
    const HEADER: [u8; 20] = [0xab; 20];

    fn round_trip(kind: TransformationKind, binding: AadBinding) {
        let key = key(kind, 0x11);
        let plaintext = b"the whole submessage, header included".as_slice();
        let sealed = protect(kind, &key, &NONCE, &HEADER, binding, plaintext).expect("seal");

        if kind.is_encrypting() {
            assert_eq!(sealed.payload.len(), plaintext.len(), "{kind} {binding}");
            assert_ne!(
                sealed.payload.as_slice(),
                plaintext,
                "{kind}: a GCM kind must not leave the plaintext readable"
            );
        } else {
            assert!(
                sealed.payload.is_empty(),
                "{kind}: a GMAC kind produces a tag and nothing else"
            );
        }

        let protected = if kind.is_encrypting() {
            Protected::Ciphertext(&sealed.payload)
        } else {
            Protected::Cleartext(plaintext)
        };
        let opened =
            unprotect(kind, &key, &NONCE, &HEADER, binding, protected, &sealed.tag).expect("open");
        assert_eq!(opened, plaintext, "{kind} {binding}");
    }

    #[test]
    fn every_kind_and_profile_round_trips() {
        for kind in [
            TransformationKind::Aes128Gmac,
            TransformationKind::Aes128Gcm,
            TransformationKind::Aes256Gmac,
            TransformationKind::Aes256Gcm,
        ] {
            for binding in AadBinding::ALL {
                round_trip(kind, binding);
            }
        }
    }

    #[test]
    fn the_null_transformation_has_no_aead() {
        let key = key(TransformationKind::Aes256Gcm, 0x11);
        assert!(matches!(
            protect(
                TransformationKind::None,
                &key,
                &NONCE,
                &HEADER,
                AadBinding::HeaderBound,
                b"x"
            ),
            Err(SecurityError::UnsupportedTransformation { .. })
        ));
        assert!(matches!(
            unprotect(
                TransformationKind::None,
                &key,
                &NONCE,
                &HEADER,
                AadBinding::HeaderBound,
                Protected::Cleartext(b"x"),
                &[0; COMMON_MAC_LEN]
            ),
            Err(SecurityError::UnsupportedTransformation { .. })
        ));
    }

    #[test]
    fn a_tampered_tag_is_refused_under_every_kind() {
        for kind in [
            TransformationKind::Aes256Gmac,
            TransformationKind::Aes256Gcm,
            TransformationKind::Aes128Gcm,
        ] {
            let key = key(kind, 0x11);
            let plaintext = b"payload".as_slice();
            let mut sealed = protect(
                kind,
                &key,
                &NONCE,
                &HEADER,
                AadBinding::HeaderBound,
                plaintext,
            )
            .expect("seal");
            sealed.tag[0] ^= 0x01;
            assert_eq!(
                unprotect(
                    kind,
                    &key,
                    &NONCE,
                    &HEADER,
                    AadBinding::HeaderBound,
                    if kind.is_encrypting() {
                        Protected::Ciphertext(&sealed.payload)
                    } else {
                        Protected::Cleartext(plaintext)
                    },
                    &sealed.tag,
                ),
                Err(SecurityError::AuthenticationFailed),
                "{kind}"
            );
        }
    }

    #[test]
    fn a_tampered_ciphertext_is_refused() {
        let kind = TransformationKind::Aes256Gcm;
        let key = key(kind, 0x11);
        let mut sealed = protect(
            kind,
            &key,
            &NONCE,
            &HEADER,
            AadBinding::HeaderBound,
            b"payload",
        )
        .expect("seal");
        sealed.payload[0] ^= 0xff;
        assert_eq!(
            unprotect(
                kind,
                &key,
                &NONCE,
                &HEADER,
                AadBinding::HeaderBound,
                Protected::Ciphertext(&sealed.payload),
                &sealed.tag,
            ),
            Err(SecurityError::AuthenticationFailed)
        );
    }

    #[test]
    fn a_tampered_signed_submessage_is_refused() {
        let kind = TransformationKind::Aes256Gmac;
        let key = key(kind, 0x11);
        let plaintext = b"readable submessage".as_slice();
        let sealed = protect(
            kind,
            &key,
            &NONCE,
            &HEADER,
            AadBinding::HeaderBound,
            plaintext,
        )
        .expect("seal");
        let mut altered = plaintext.to_vec();
        altered[0] ^= 0x20;
        assert_eq!(
            unprotect(
                kind,
                &key,
                &NONCE,
                &HEADER,
                AadBinding::HeaderBound,
                Protected::Cleartext(&altered),
                &sealed.tag,
            ),
            Err(SecurityError::AuthenticationFailed),
            "signing leaves the octets readable, not editable"
        );
    }

    #[test]
    fn a_different_key_is_refused() {
        let kind = TransformationKind::Aes256Gcm;
        let sealed = protect(
            kind,
            &key(kind, 0x11),
            &NONCE,
            &HEADER,
            AadBinding::HeaderBound,
            b"payload",
        )
        .expect("seal");
        assert_eq!(
            unprotect(
                kind,
                &key(kind, 0x22),
                &NONCE,
                &HEADER,
                AadBinding::HeaderBound,
                Protected::Ciphertext(&sealed.payload),
                &sealed.tag,
            ),
            Err(SecurityError::AuthenticationFailed)
        );
    }

    #[test]
    fn a_different_nonce_is_refused() {
        let kind = TransformationKind::Aes256Gcm;
        let key = key(kind, 0x11);
        let sealed = protect(
            kind,
            &key,
            &NONCE,
            &HEADER,
            AadBinding::HeaderBound,
            b"payload",
        )
        .expect("seal");
        let mut nonce = NONCE;
        nonce[11] ^= 0x01;
        assert_eq!(
            unprotect(
                kind,
                &key,
                &nonce,
                &HEADER,
                AadBinding::HeaderBound,
                Protected::Ciphertext(&sealed.payload),
                &sealed.tag,
            ),
            Err(SecurityError::AuthenticationFailed)
        );
    }

    #[test]
    fn the_header_binding_is_what_makes_a_spliced_header_fail() {
        // The reason `AadBinding` is a choice rather than a constant: under
        // the specification's empty-AAD profile the crypto header is not
        // authenticated, and swapping it changes nothing the tag can see.
        let kind = TransformationKind::Aes256Gcm;
        let key = key(kind, 0x11);
        let other_header = [0xcd_u8; 20];

        let bound = protect(
            kind,
            &key,
            &NONCE,
            &HEADER,
            AadBinding::HeaderBound,
            b"payload",
        )
        .expect("seal");
        assert_eq!(
            unprotect(
                kind,
                &key,
                &NONCE,
                &other_header,
                AadBinding::HeaderBound,
                Protected::Ciphertext(&bound.payload),
                &bound.tag,
            ),
            Err(SecurityError::AuthenticationFailed),
            "header-bound: a spliced header must fail"
        );

        let loose = protect(
            kind,
            &key,
            &NONCE,
            &HEADER,
            AadBinding::SpecEmpty,
            b"payload",
        )
        .expect("seal");
        assert_eq!(
            unprotect(
                kind,
                &key,
                &NONCE,
                &other_header,
                AadBinding::SpecEmpty,
                Protected::Ciphertext(&loose.payload),
                &loose.tag,
            )
            .expect("spec-empty accepts it"),
            b"payload".to_vec(),
            "spec-empty: the header is outside the tag, and this is why we do not use it"
        );
    }

    #[test]
    fn the_kind_and_the_envelope_shape_must_agree() {
        let gmac = TransformationKind::Aes256Gmac;
        assert!(matches!(
            unprotect(
                gmac,
                &key(gmac, 0x11),
                &NONCE,
                &HEADER,
                AadBinding::HeaderBound,
                Protected::Ciphertext(b"unexpected ciphertext"),
                &[0; COMMON_MAC_LEN],
            ),
            Err(SecurityError::Malformed { .. })
        ));

        let gcm = TransformationKind::Aes256Gcm;
        assert!(matches!(
            unprotect(
                gcm,
                &key(gcm, 0x11),
                &NONCE,
                &HEADER,
                AadBinding::HeaderBound,
                Protected::Cleartext(b"unexpected cleartext"),
                &[0; COMMON_MAC_LEN],
            ),
            Err(SecurityError::Malformed { .. })
        ));
    }

    #[test]
    fn an_empty_plaintext_still_produces_a_tag() {
        let kind = TransformationKind::Aes256Gcm;
        let key = key(kind, 0x11);
        let sealed =
            protect(kind, &key, &NONCE, &HEADER, AadBinding::HeaderBound, &[]).expect("seal");
        assert!(sealed.payload.is_empty());
        assert_eq!(
            unprotect(
                kind,
                &key,
                &NONCE,
                &HEADER,
                AadBinding::HeaderBound,
                Protected::Ciphertext(&[]),
                &sealed.tag,
            )
            .expect("open"),
            Vec::<u8>::new()
        );
    }
}
