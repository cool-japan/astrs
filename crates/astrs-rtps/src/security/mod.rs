//! DDS-Security submessage protection, on pre-shared keys.
//!
//! # Scope, stated plainly
//!
//! DDS-Security 1.1 is five plugins. This module implements **one half of
//! one** of them, and the honest summary of what that buys is short:
//!
//! | Part of DDS-Security | Here? |
//! |---|---|
//! | Cryptographic plugin — submessage protection (`SEC_PREFIX`, `SEC_BODY`, `SEC_POSTFIX`; `AES128`/`AES256` `GMAC`/`GCM`) | **yes** |
//! | Cryptographic plugin — whole-message protection (`SRTPS_PREFIX`/`SRTPS_POSTFIX`) | no; the submessage ids are named, the transform is not implemented |
//! | Cryptographic plugin — serialized-payload protection | no |
//! | Cryptographic plugin — receiver-specific MACs | parsed, never emitted |
//! | Authentication plugin — the PKI handshake, identity certificates | **no** |
//! | Access-control plugin — governance and permissions XML, signed | **no** |
//! | Logging and Data Tagging plugins | **no** |
//!
//! Keying is a pre-shared key an operator configures on both endpoints. There
//! is no key exchange, no identity, and no authorisation: every peer holding
//! the key is equally entitled to write to the topic and to read it.
//!
//! **This does not interoperate with a PKI-based DDS-Security stack.** Not
//! "not yet", and not "except for the handshake": a Fast-DDS or Connext
//! participant with security enabled derives its keys from a handshake that
//! does not happen here, so nothing either side sends will verify at the
//! other. The wire *format* is the specification's, which is the part that
//! makes the missing half addable rather than the part that makes it
//! unnecessary — see [What slots in later](#what-slots-in-later).
//!
//! What it *does* buy, on a robot, today: a `DATA` on `/cmd_vel` that an
//! attacker on the same network cannot forge, alter, replay, or (under
//! `encrypt`) read.
//!
//! # The pieces
//!
//! | Module | Contents |
//! |---|---|
//! | [`kind`] | [`TransformationKind`], [`ProtectionKind`], [`AadBinding`] |
//! | [`keys`] | [`Psk`], [`KeyMaterial`], [`SessionKey`], [`SessionSender`] |
//! | [`replay`] | [`ReplayWindow`] — the sliding anti-replay window |
//! | [`wire`] | [`CryptoHeader`], [`CryptoFooter`], the `SEC_*` submessage bodies |
//! | [`crypto`] | the two AEAD calls, and nothing else |
//! | [`transform`] | [`EndpointSecurity`] and [`SecurityContext`] |
//! | [`error`] | [`SecurityError`] |
//!
//! # Cryptography
//!
//! Every primitive comes from `oxicrypto` and nothing is implemented here:
//! AES-GCM through its `Aead` trait, GMAC as that same AEAD with an empty
//! plaintext (NIST SP 800-38D §3), HKDF-SHA-256 for every derivation, its
//! CSPRNG for [`Psk::generate`], and its `ct_eq` and `Zeroize` for handling
//! the results. `crypto.rs` is the only file that calls a cipher, and it is
//! two functions long so that staying true stays checkable.
//!
//! One composition is worth flagging because it is a choice rather than a
//! lookup: DDS-Security's key derivation is specified in terms of an
//! HMAC-SHA-256 construction, and this module uses **HKDF-SHA-256**
//! (RFC 5869) — extract-then-expand over the same hash — because that is what
//! `oxicrypto` exposes as a KDF and because it is the stronger of the two for
//! the job. Nothing about the wire format depends on the choice: the key id,
//! the session id and the nonce are all on the wire, and a peer that derived
//! its keys some other way would still parse every octet.
//!
//! # Two deliberate deviations
//!
//! Both are named, both are reversible, and both exist because the
//! specification's choice is weaker:
//!
//! 1. **The AAD binds the crypto header.** DDS-Security passes empty
//!    additional authenticated data for the GCM kinds, which leaves the key
//!    id, session id and initialisation vector unauthenticated.
//!    [`AadBinding::HeaderBound`] is the default here and
//!    [`AadBinding::SpecEmpty`] is one field away; both are implemented and
//!    both are tested, including the test that shows what `SpecEmpty` lets
//!    through.
//! 2. **Key material is context-separated.** Two topics configured with the
//!    same pre-shared key derive different keys and different key ids,
//!    because the topic and type name go into the extraction step. Sharing a
//!    key across a deployment does not silently mean sharing one across its
//!    topics.
//!
//! # What the tag does not cover
//!
//! Three limits that are real, bounded, and better written down than
//! discovered. None is a missing feature; each is a consequence of protecting
//! *submessages* rather than whole messages.
//!
//! 1. **The RTPS header is outside the AAD.** [`AadBinding::HeaderBound`]
//!    binds the twenty-octet crypto header, not the datagram's own header, so
//!    the sending participant's `guidPrefix` is authenticated by nothing.
//!    Anti-replay windows are per-sender (they must be — see
//!    [`SecurityContext`]), so a replay under a *spoofed* prefix lands in a
//!    fresh window and is accepted by this layer. It is then dropped one
//!    layer up: the sample carries the original writer's GUID and sequence
//!    number, and a reader that has already seen that sequence number
//!    discards it. The cost of the attack is therefore a duplicate that the
//!    reliability protocol was already built to absorb. Closing it properly
//!    means binding `guidPrefix ‖ crypto_header` as AAD —
//!    [`crypto::protect`] takes `header: &[u8]` rather than
//!    a fixed array precisely so that this needs no signature change — at the
//!    price of a third deviation from the wire specification. It was left
//!    out of this release deliberately.
//! 2. **A plaintext broadcast from a user endpoint is refused rather than
//!    routed.** A submessage addressed to `ENTITYID_UNKNOWN` names no
//!    endpoint, so no endpoint's policy can be consulted — and waving it
//!    through would be a downgrade reachable by clearing a field rather than
//!    by omitting the transform. On a participant with any protected
//!    endpoint, such a submessage is refused when a *user* endpoint sent it.
//!    Metatraffic is exempt, and has to be: a builtin writer's broadcast is
//!    discovery, which is never protected (limit 3), so refusing it would
//!    break SPDP on any participant that happened to secure one topic.
//!    The residual cost is narrow: on a participant that *mixes* protected
//!    and unprotected user endpoints, a plaintext broadcast meant for one of
//!    the unprotected ones is refused too. This crate never emits
//!    `ENTITYID_UNKNOWN` in an entity submessage, so only a third-party peer
//!    can provoke it.
//! 3. **Metatraffic is in the clear.** SPDP and SEDP are never protected —
//!    see [`ParticipantConfig::security`](crate::behavior::ParticipantConfig).
//!    Topic names, type names and QoS are therefore visible to anyone on the
//!    network, and only the user data on those topics is protected. Hiding
//!    discovery is what `SRTPS_PREFIX` whole-message protection is for, and
//!    that is out of scope above.
//!
//! # What slots in later
//!
//! The seam is [`KeyMaterial::from_parts`] and
//! [`SecurityContext::register_material`]: an authenticated key agreement
//! that produces a master key, a master salt and an agreed key id has
//! somewhere to put them, and every layer above — session derivation, the
//! twenty-octet crypto header, the replay window, the submessage framing — is
//! unchanged. Adding the handshake is adding a source of [`KeyMaterial`], not
//! a rewrite of this module.
//!
//! # Example
//!
//! ```
//! use astrs_rtps::security::{EndpointSecurity, Psk, SecurityContext};
//! use astrs_rtps::messages::{Data, DataPayload, Message, SerializedPayload, SubmessageId};
//! use astrs_rtps::structure::{EntityId, EntityKind, GuidPrefix, SequenceNumber};
//!
//! let psk = Psk::new(vec![0x5a; 32])?;
//! let writer = EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY);
//! let reader = EntityId::user_defined(2, EntityKind::USER_READER_NO_KEY);
//!
//! // The publisher protects; the subscriber verifies. Same key, same topic.
//! let mut publisher = SecurityContext::new(1_400);
//! publisher.register(writer, &EndpointSecurity::encrypted(psk.clone()), "rt/chatter")?;
//! let mut subscriber = SecurityContext::new(1_400);
//! subscriber.register(reader, &EndpointSecurity::encrypted(psk), "rt/chatter")?;
//!
//! let message = Message::from_participant(GuidPrefix::new([1; 12])).with(Data::new(
//!     reader,
//!     writer,
//!     SequenceNumber::FIRST,
//!     DataPayload::Data(SerializedPayload::from_cdr(&7_u32)?),
//! ));
//!
//! let protected = publisher.protect(&message)?;
//! assert_eq!(protected.len(), 1);
//! let ids: Vec<SubmessageId> = protected[0].iter().map(|s| s.id()).collect();
//! assert_eq!(ids, vec![
//!     SubmessageId::SecurePrefix,
//!     SubmessageId::SecureBody,
//!     SubmessageId::SecurePostfix,
//! ]);
//!
//! assert_eq!(subscriber.unprotect(&protected[0])?, message);
//! # Ok::<(), astrs_rtps::security::SecurityError>(())
//! ```

pub mod crypto;
pub mod error;
pub mod keys;
pub mod kind;
pub mod replay;
pub mod transform;
pub mod wire;

pub use error::{SecurityError, SecurityResult};
pub use keys::{
    KEY_ID_NONE, KeyMaterial, MASTER_KEY_LEN, MASTER_SALT_LEN, MAX_SESSION_KEY_LEN,
    PSK_GENERATED_LEN, PSK_MIN_LEN, Psk, SESSION_MESSAGE_LIMIT, SessionKey, SessionSender,
};
pub use kind::{AadBinding, ProtectionKind, TRANSFORMATION_KIND_LEN, TransformationKind};
pub use replay::{REPLAY_WINDOW, ReplayWindow};
pub use transform::{EndpointSecurity, SecurityContext, destination_entity, source_entity};
pub use wire::{
    COMMON_MAC_LEN, CRYPTO_FOOTER_MIN_LEN, CRYPTO_HEADER_LEN, CryptoFooter, CryptoHeader,
    INIT_VECTOR_SUFFIX_LEN, MAX_RECEIVER_MACS, NONCE_LEN, PROTECTION_OVERHEAD, RECEIVER_MAC_LEN,
    ReceiverMac,
};
