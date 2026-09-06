//! The three secure submessages, octet for octet.
//!
//! DDS-Security 1.1 §7.3.6 adds five submessage ids to the RTPS grammar; this
//! module implements the three that submessage protection uses, and the
//! [`SubmessageId`] variants for the other two exist so a log line can name
//! them.
//!
//! ```text
//!  Signed (GMAC) — the submessage stays readable
//! +-----------------------------------------------+
//! | SEC_PREFIX  0x31 | flags | octetsToNextHeader |  4
//! | SecureDataHeader                              | 20
//! +-----------------------------------------------+
//! | the original submessage, in the clear         |  4 + body
//! +-----------------------------------------------+
//! | SEC_POSTFIX 0x32 | flags | octetsToNextHeader |  4
//! | SecureDataTag                                 | 20 + 20·n
//! +-----------------------------------------------+
//!
//!  Encrypted (GCM) — the submessage becomes ciphertext
//! +-----------------------------------------------+
//! | SEC_PREFIX  0x31 | flags | octetsToNextHeader |  4
//! | SecureDataHeader                              | 20
//! +-----------------------------------------------+
//! | SEC_BODY    0x30 | flags | octetsToNextHeader |  4
//! | long secure_data_length                       |  4
//! | octet secure_data[length]  (+ padding to 4)   |
//! +-----------------------------------------------+
//! | SEC_POSTFIX 0x32 | flags | octetsToNextHeader |  4
//! | SecureDataTag                                 | 20 + 20·n
//! +-----------------------------------------------+
//! ```
//!
//! # Byte order
//!
//! [`CryptoHeader`] is deliberately endian-neutral: the specification types
//! every one of its fields as `octet[]`, so the key id, the session id and
//! the initialisation-vector suffix are big-endian octet strings and not
//! integers whose layout a flag could change. That matters more here than
//! elsewhere, because the header is what the AAD binds and a field whose
//! octets depended on a flag would let an attacker change the plaintext of
//! the AAD without changing the header's meaning.
//!
//! The two `long` counts — the secure body's length and the tag's
//! receiver-specific count — are ordinary CDR and follow the submessage's
//! `EndiannessFlag`, as every other length in RTPS does.
//!
//! # The initialisation vector
//!
//! §9.5.3.3.1 builds the twelve-octet AES-GCM nonce as the session id
//! followed by the eight-octet suffix. This crate puts a monotonically
//! increasing per-session counter in the suffix, which gives three things at
//! once: a nonce that cannot repeat inside a session, a value the replay
//! window can order, and — because the counter also bounds the session — a
//! rekey trigger that needs no clock.

use astrs_cdr::{CdrReader, CdrWriter, Endianness};

use crate::messages::{
    BodyExtent, Opaque, SUBMESSAGE_HEADER_LEN, Submessage, SubmessageFlags, SubmessageHeader,
    SubmessageId, body_encoding,
};
use crate::security::error::{SecurityError, SecurityResult};
use crate::security::kind::{TRANSFORMATION_KIND_LEN, TransformationKind};

/// Octets a `SecureDataHeader` occupies (§9.5.2.2).
pub const CRYPTO_HEADER_LEN: usize = TRANSFORMATION_KIND_LEN + 4 + 4 + INIT_VECTOR_SUFFIX_LEN;

/// Octets of initialisation-vector suffix (§9.5.3.3.1).
pub const INIT_VECTOR_SUFFIX_LEN: usize = 8;

/// Octets of AES-GCM nonce: the session id then the suffix.
pub const NONCE_LEN: usize = 4 + INIT_VECTOR_SUFFIX_LEN;

/// Octets of AES-GCM authentication tag.
pub const COMMON_MAC_LEN: usize = 16;

/// Octets one receiver-specific MAC entry occupies.
pub const RECEIVER_MAC_LEN: usize = 4 + COMMON_MAC_LEN;

/// Octets a `SecureDataTag` with no receiver-specific MACs occupies.
pub const CRYPTO_FOOTER_MIN_LEN: usize = COMMON_MAC_LEN + 4;

/// Most receiver-specific MACs a footer may declare.
///
/// A bound checked before anything is allocated, in the same spirit as
/// [`MAX_SUBMESSAGES`](crate::messages::MAX_SUBMESSAGES): a hostile count of
/// four billion must not become a four-billion-element `Vec`.
pub const MAX_RECEIVER_MACS: usize = 1_024;

/// Octets a protected submessage costs beyond the submessage itself.
///
/// The `SEC_PREFIX` (4 + 20), the `SEC_POSTFIX` (4 + 20), the `SEC_BODY`
/// header and length (4 + 4), the AEAD tag inside the body (16) and up to
/// three octets of padding, rounded up to the four-octet boundary a
/// submessage header must start on. Signing costs less than this; encrypting
/// costs exactly it, which is why a budget computed from it is never wrong in
/// the dangerous direction.
pub const PROTECTION_OVERHEAD: usize =
    (4 + CRYPTO_HEADER_LEN) + (4 + CRYPTO_FOOTER_MIN_LEN) + (4 + 4) + COMMON_MAC_LEN + 4;

/// The `SecureDataHeader` of §9.5.2.2: which key, which session, which nonce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CryptoHeader {
    /// Which transformation the protected submessage used.
    pub kind: TransformationKind,
    /// Which key material the receiver must look up.
    pub key_id: u32,
    /// Which session of that key.
    pub session_id: u32,
    /// The per-submessage half of the nonce, a big-endian counter here.
    pub init_vector_suffix: [u8; INIT_VECTOR_SUFFIX_LEN],
}

impl CryptoHeader {
    /// Build a header from its parts.
    #[must_use]
    pub const fn new(kind: TransformationKind, key_id: u32, session_id: u32, counter: u64) -> Self {
        Self {
            kind,
            key_id,
            session_id,
            init_vector_suffix: counter.to_be_bytes(),
        }
    }

    /// The counter the suffix carries.
    #[must_use]
    pub const fn counter(&self) -> u64 {
        u64::from_be_bytes(self.init_vector_suffix)
    }

    /// The twelve-octet AES-GCM nonce (§9.5.3.3.1).
    #[must_use]
    pub const fn nonce(&self) -> [u8; NONCE_LEN] {
        let session = self.session_id.to_be_bytes();
        let suffix = self.init_vector_suffix;
        [
            session[0], session[1], session[2], session[3], suffix[0], suffix[1], suffix[2],
            suffix[3], suffix[4], suffix[5], suffix[6], suffix[7],
        ]
    }

    /// The twenty octets of a `SEC_PREFIX` body.
    #[must_use]
    pub const fn to_octets(&self) -> [u8; CRYPTO_HEADER_LEN] {
        let kind = self.kind.to_octets();
        let key = self.key_id.to_be_bytes();
        let session = self.session_id.to_be_bytes();
        let suffix = self.init_vector_suffix;
        [
            kind[0], kind[1], kind[2], kind[3], key[0], key[1], key[2], key[3], session[0],
            session[1], session[2], session[3], suffix[0], suffix[1], suffix[2], suffix[3],
            suffix[4], suffix[5], suffix[6], suffix[7],
        ]
    }

    /// Read a `SEC_PREFIX` body.
    ///
    /// # Errors
    ///
    /// [`SecurityError::Truncated`] below twenty octets, and
    /// [`SecurityError::UnsupportedTransformation`] for an unassigned
    /// transformation kind.
    pub fn from_octets(body: &[u8]) -> SecurityResult<Self> {
        let octets: [u8; CRYPTO_HEADER_LEN] = body
            .get(..CRYPTO_HEADER_LEN)
            .and_then(|slice| slice.try_into().ok())
            .ok_or(SecurityError::Truncated {
                len: body.len(),
                what: "a crypto header",
            })?;
        let mut kind = [0_u8; TRANSFORMATION_KIND_LEN];
        kind.copy_from_slice(&octets[0..4]);
        let mut key = [0_u8; 4];
        key.copy_from_slice(&octets[4..8]);
        let mut session = [0_u8; 4];
        session.copy_from_slice(&octets[8..12]);
        let mut suffix = [0_u8; INIT_VECTOR_SUFFIX_LEN];
        suffix.copy_from_slice(&octets[12..CRYPTO_HEADER_LEN]);
        Ok(Self {
            kind: TransformationKind::from_octets(kind)?,
            key_id: u32::from_be_bytes(key),
            session_id: u32::from_be_bytes(session),
            init_vector_suffix: suffix,
        })
    }

    /// The `SEC_PREFIX` submessage carrying this header.
    #[must_use]
    pub fn to_submessage(self) -> Submessage<'static> {
        Submessage::Opaque(Opaque::new(
            SubmessageId::SecurePrefix,
            SubmessageFlags::LITTLE_ENDIAN,
            self.to_octets().to_vec(),
        ))
    }
}

/// One receiver-specific MAC of a `SecureDataTag` (§9.5.2.3).
///
/// Never emitted by this release — a pre-shared key has one recipient set and
/// nothing to distinguish inside it — but parsed, so a peer that sends them
/// is understood rather than rejected, and so the field a future
/// per-receiver keying step needs is already on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReceiverMac {
    /// The receiver-specific key this MAC was computed under.
    pub key_id: u32,
    /// The MAC itself.
    pub mac: [u8; COMMON_MAC_LEN],
}

/// The `SecureDataTag` of §9.5.2.3.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CryptoFooter {
    /// The AEAD tag over the protected submessage.
    pub common_mac: [u8; COMMON_MAC_LEN],
    /// Per-receiver MACs, empty in this release.
    pub receiver_specific: Vec<ReceiverMac>,
}

impl CryptoFooter {
    /// A footer carrying only the common MAC.
    #[must_use]
    pub const fn new(common_mac: [u8; COMMON_MAC_LEN]) -> Self {
        Self {
            common_mac,
            receiver_specific: Vec::new(),
        }
    }

    /// Octets the footer occupies.
    #[must_use]
    pub fn body_len(&self) -> usize {
        CRYPTO_FOOTER_MIN_LEN + self.receiver_specific.len() * RECEIVER_MAC_LEN
    }

    /// The octets of a `SEC_POSTFIX` body.
    ///
    /// # Errors
    ///
    /// [`SecurityError::Wire`] if the CDR writer refuses the count, which the
    /// [`MAX_RECEIVER_MACS`] bound makes unreachable.
    pub fn to_octets(&self, endianness: Endianness) -> SecurityResult<Vec<u8>> {
        let mut writer = CdrWriter::headerless(body_encoding(endianness));
        writer.write_octets(&self.common_mac);
        let count = u32::try_from(self.receiver_specific.len()).unwrap_or(u32::MAX);
        writer
            .write_u32(count)
            .map_err(crate::error::RtpsError::from)?;
        for entry in &self.receiver_specific {
            writer.write_octets(&entry.key_id.to_be_bytes());
            writer.write_octets(&entry.mac);
        }
        Ok(writer.finish())
    }

    /// Read a `SEC_POSTFIX` body.
    ///
    /// # Errors
    ///
    /// [`SecurityError::Truncated`] when the octets run out, and
    /// [`SecurityError::Malformed`] for a receiver-MAC count above
    /// [`MAX_RECEIVER_MACS`] — checked against the count *and* against what
    /// the body could possibly hold, before anything is allocated.
    pub fn from_octets(body: &[u8], endianness: Endianness) -> SecurityResult<Self> {
        if body.len() < CRYPTO_FOOTER_MIN_LEN {
            return Err(SecurityError::Truncated {
                len: body.len(),
                what: "a crypto footer",
            });
        }
        let mut reader = CdrReader::with_encoding(body, body_encoding(endianness));
        let mut common_mac = [0_u8; COMMON_MAC_LEN];
        common_mac.copy_from_slice(
            reader
                .read_octets(COMMON_MAC_LEN)
                .map_err(crate::error::RtpsError::from)?,
        );
        let declared = reader.read_u32().map_err(crate::error::RtpsError::from)?;
        let count = usize::try_from(declared).unwrap_or(usize::MAX);
        let available = (body.len() - CRYPTO_FOOTER_MIN_LEN) / RECEIVER_MAC_LEN;
        if count > MAX_RECEIVER_MACS || count > available {
            return Err(SecurityError::Malformed {
                reason: "a crypto footer declares more receiver-specific MACs than it carries",
            });
        }
        let mut receiver_specific = Vec::with_capacity(count);
        for _ in 0..count {
            let octets = reader
                .read_octets(RECEIVER_MAC_LEN)
                .map_err(crate::error::RtpsError::from)?;
            let mut key = [0_u8; 4];
            key.copy_from_slice(&octets[0..4]);
            let mut mac = [0_u8; COMMON_MAC_LEN];
            mac.copy_from_slice(&octets[4..RECEIVER_MAC_LEN]);
            receiver_specific.push(ReceiverMac {
                key_id: u32::from_be_bytes(key),
                mac,
            });
        }
        Ok(Self {
            common_mac,
            receiver_specific,
        })
    }

    /// The `SEC_POSTFIX` submessage carrying this footer.
    ///
    /// # Errors
    ///
    /// As [`to_octets`](Self::to_octets).
    pub fn to_submessage(&self) -> SecurityResult<Submessage<'static>> {
        Ok(Submessage::Opaque(Opaque::new(
            SubmessageId::SecurePostfix,
            SubmessageFlags::LITTLE_ENDIAN,
            self.to_octets(Endianness::Little)?,
        )))
    }
}

/// Wrap ciphertext in a `SEC_BODY` submessage.
///
/// The length prefix is what makes the padding recoverable: a submessage body
/// must be a multiple of four for the `SEC_POSTFIX` to follow it, and the
/// ciphertext of an arbitrary submessage is not.
///
/// # Errors
///
/// [`SecurityError::Wire`] if the CDR writer refuses the length.
pub fn secure_body_submessage(ciphertext: &[u8]) -> SecurityResult<Submessage<'static>> {
    let mut writer = CdrWriter::headerless(body_encoding(Endianness::Little));
    let length = u32::try_from(ciphertext.len()).unwrap_or(u32::MAX);
    writer
        .write_u32(length)
        .map_err(crate::error::RtpsError::from)?;
    writer.write_octets(ciphertext);
    let mut body = writer.finish();
    while !body.len().is_multiple_of(4) {
        body.push(0);
    }
    Ok(Submessage::Opaque(Opaque::new(
        SubmessageId::SecureBody,
        SubmessageFlags::LITTLE_ENDIAN,
        body,
    )))
}

/// Recover the ciphertext a `SEC_BODY` carries.
///
/// # Errors
///
/// [`SecurityError::Truncated`] when the body cannot hold its own length
/// prefix, and [`SecurityError::Malformed`] when the declared length exceeds
/// what follows it — checked before any allocation.
pub fn read_secure_body(body: &[u8], endianness: Endianness) -> SecurityResult<Vec<u8>> {
    if body.len() < 4 {
        return Err(SecurityError::Truncated {
            len: body.len(),
            what: "a secure body length",
        });
    }
    let mut reader = CdrReader::with_encoding(body, body_encoding(endianness));
    let declared = reader.read_u32().map_err(crate::error::RtpsError::from)?;
    let length = usize::try_from(declared).unwrap_or(usize::MAX);
    if length > body.len() - 4 {
        return Err(SecurityError::Malformed {
            reason: "a secure body declares more ciphertext than it carries",
        });
    }
    Ok(reader
        .read_octets(length)
        .map_err(crate::error::RtpsError::from)?
        .to_vec())
}

/// Encode one submessage on its own — header and body, nothing around it.
///
/// This is what the AEAD sees. A submessage inside an envelope is the last
/// thing in its own little frame, which is why `is_last` is true: a body that
/// does not fit sixteen bits then declares zero and runs to the end, exactly
/// as it would at the end of a datagram.
///
/// # Errors
///
/// [`SecurityError::Wire`] from the submessage's own writer.
pub fn encode_submessage(submessage: &Submessage<'_>) -> SecurityResult<Vec<u8>> {
    let body = submessage.encode_body().map_err(SecurityError::Wire)?;
    let octets = SubmessageHeader::octets_for(submessage.id(), body.len(), true)
        .map_err(SecurityError::Wire)?;
    let header = SubmessageHeader::new(submessage.id(), submessage.flags(), octets);
    let mut out = Vec::with_capacity(SUBMESSAGE_HEADER_LEN + body.len());
    out.extend_from_slice(&header.to_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// The inverse of [`encode_submessage`].
///
/// # Errors
///
/// [`SecurityError::Truncated`] below four octets, and [`SecurityError::Wire`]
/// from the framing or the submessage's own reader.
pub fn decode_submessage(octets: &[u8]) -> SecurityResult<Submessage<'static>> {
    if octets.len() < SUBMESSAGE_HEADER_LEN {
        return Err(SecurityError::Truncated {
            len: octets.len(),
            what: "a recovered submessage header",
        });
    }
    let header = SubmessageHeader::decode(octets).map_err(SecurityError::Wire)?;
    let rest = &octets[SUBMESSAGE_HEADER_LEN..];
    let body = match header
        .body_extent(rest.len())
        .map_err(SecurityError::Wire)?
    {
        BodyExtent::Exact(len) => rest.get(..len).ok_or(SecurityError::Truncated {
            len: rest.len(),
            what: "a recovered submessage body",
        })?,
        BodyExtent::ToEndOfMessage => rest,
    };
    Ok(Submessage::read(&header, body)
        .map_err(SecurityError::Wire)?
        .into_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn header() -> CryptoHeader {
        CryptoHeader::new(
            TransformationKind::Aes256Gcm,
            0x1122_3344,
            0x0000_0007,
            0x0102_0304_0506_0708,
        )
    }

    #[test]
    fn a_crypto_header_is_twenty_octets_in_the_order_the_spec_gives() {
        let octets = header().to_octets();
        assert_eq!(octets.len(), CRYPTO_HEADER_LEN);
        assert_eq!(CRYPTO_HEADER_LEN, 20);
        assert_eq!(
            octets,
            [
                // transformation_kind: AES256_GCM
                0x00, 0x00, 0x00, 0x04, //
                // transformation_key_id, big-endian
                0x11, 0x22, 0x33, 0x44, //
                // session_id, big-endian
                0x00, 0x00, 0x00, 0x07, //
                // init_vector_suffix
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
            ]
        );
    }

    #[test]
    fn a_crypto_header_round_trips() {
        for kind in [
            TransformationKind::Aes128Gmac,
            TransformationKind::Aes128Gcm,
            TransformationKind::Aes256Gmac,
            TransformationKind::Aes256Gcm,
        ] {
            let original = CryptoHeader::new(kind, 0xdead_beef, 3, 4_096);
            let decoded = CryptoHeader::from_octets(&original.to_octets()).expect("round trip");
            assert_eq!(decoded, original, "{kind}");
            assert_eq!(decoded.counter(), 4_096);
        }
    }

    #[test]
    fn the_nonce_is_the_session_id_then_the_suffix() {
        let nonce = header().nonce();
        assert_eq!(nonce.len(), NONCE_LEN);
        assert_eq!(NONCE_LEN, 12, "AES-GCM takes twelve octets");
        assert_eq!(
            nonce,
            [
                0x00, 0x00, 0x00, 0x07, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08
            ]
        );
    }

    #[test]
    fn a_short_or_unassigned_crypto_header_is_refused() {
        assert!(matches!(
            CryptoHeader::from_octets(&[0_u8; CRYPTO_HEADER_LEN - 1]),
            Err(SecurityError::Truncated { .. })
        ));
        assert!(matches!(
            CryptoHeader::from_octets(&[]),
            Err(SecurityError::Truncated { len: 0, .. })
        ));

        let mut octets = header().to_octets();
        octets[3] = 0x09;
        assert!(matches!(
            CryptoHeader::from_octets(&octets),
            Err(SecurityError::UnsupportedTransformation { .. })
        ));
    }

    #[test]
    fn a_crypto_header_ignores_octets_a_later_version_appends() {
        let mut body = header().to_octets().to_vec();
        body.extend_from_slice(&[0xff; 8]);
        assert_eq!(CryptoHeader::from_octets(&body).expect("read"), header());
    }

    #[test]
    fn a_footer_round_trips_in_both_byte_orders() {
        let footer = CryptoFooter {
            common_mac: [0x5a; COMMON_MAC_LEN],
            receiver_specific: vec![
                ReceiverMac {
                    key_id: 1,
                    mac: [0x11; COMMON_MAC_LEN],
                },
                ReceiverMac {
                    key_id: 0x8899_aabb,
                    mac: [0x22; COMMON_MAC_LEN],
                },
            ],
        };
        for endianness in [Endianness::Little, Endianness::Big] {
            let octets = footer.to_octets(endianness).expect("encode");
            assert_eq!(octets.len(), footer.body_len());
            assert_eq!(
                CryptoFooter::from_octets(&octets, endianness).expect("decode"),
                footer,
                "{endianness:?}"
            );
        }
    }

    #[test]
    fn an_empty_footer_is_twenty_octets() {
        let footer = CryptoFooter::new([7; COMMON_MAC_LEN]);
        assert_eq!(footer.body_len(), CRYPTO_FOOTER_MIN_LEN);
        assert_eq!(CRYPTO_FOOTER_MIN_LEN, 20);
        let octets = footer.to_octets(Endianness::Little).expect("encode");
        assert_eq!(octets.len(), 20);
        assert_eq!(&octets[16..], &[0, 0, 0, 0], "a zero count");
        assert!(footer.body_len().is_multiple_of(4), "and it stays aligned");
    }

    #[test]
    fn a_hostile_receiver_mac_count_is_rejected_before_it_allocates() {
        let mut octets = CryptoFooter::new([0; COMMON_MAC_LEN])
            .to_octets(Endianness::Little)
            .expect("encode");
        octets[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            CryptoFooter::from_octets(&octets, Endianness::Little),
            Err(SecurityError::Malformed { .. })
        ));

        octets[16..20].copy_from_slice(&1_u32.to_le_bytes());
        assert!(
            matches!(
                CryptoFooter::from_octets(&octets, Endianness::Little),
                Err(SecurityError::Malformed { .. })
            ),
            "one MAC declared and none present is the same lie, smaller"
        );
    }

    #[test]
    fn a_truncated_footer_is_refused() {
        assert!(matches!(
            CryptoFooter::from_octets(&[0_u8; CRYPTO_FOOTER_MIN_LEN - 1], Endianness::Little),
            Err(SecurityError::Truncated { .. })
        ));
    }

    #[test]
    fn a_secure_body_round_trips_at_every_length_modulo_four() {
        for len in 0..12_usize {
            let ciphertext: Vec<u8> = (0..len).map(|index| index as u8).collect();
            let submessage = secure_body_submessage(&ciphertext).expect("wrap");
            assert_eq!(submessage.id(), SubmessageId::SecureBody);
            assert!(
                submessage.body_len().is_multiple_of(4),
                "a SEC_BODY must be followed by a SEC_POSTFIX, so it pads"
            );
            let body = submessage.encode_body().expect("encode");
            assert_eq!(
                read_secure_body(&body, Endianness::Little).expect("unwrap"),
                ciphertext,
                "length {len}"
            );
        }
    }

    #[test]
    fn a_secure_body_that_lies_about_its_length_is_refused() {
        let mut body = secure_body_submessage(&[1, 2, 3, 4])
            .expect("wrap")
            .encode_body()
            .expect("encode");
        body[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            read_secure_body(&body, Endianness::Little),
            Err(SecurityError::Malformed { .. })
        ));

        assert!(matches!(
            read_secure_body(&[0, 0], Endianness::Little),
            Err(SecurityError::Truncated { .. })
        ));
    }

    #[test]
    fn the_overhead_constant_matches_what_the_submessages_actually_cost() {
        let header = header().to_submessage();
        let footer = CryptoFooter::new([0; COMMON_MAC_LEN])
            .to_submessage()
            .expect("encode");
        // The worst case: a one-octet submessage body, encrypted.
        let plaintext_len = 4 + 1;
        let body =
            secure_body_submessage(&vec![0_u8; plaintext_len + COMMON_MAC_LEN]).expect("wrap");
        let protected = header.serialized_len() + body.serialized_len() + footer.serialized_len();
        assert!(
            protected <= plaintext_len + PROTECTION_OVERHEAD,
            "{protected} octets must fit the {PROTECTION_OVERHEAD}-octet budget allowance"
        );
        assert!(PROTECTION_OVERHEAD.is_multiple_of(4));
    }

    #[test]
    fn a_standalone_submessage_round_trips_at_any_body_length() {
        use crate::messages::{Heartbeat, Pad, SerializedPayload};
        use crate::structure::{EntityId, EntityKind, SequenceNumber};

        let reader = EntityId::user_defined(2, EntityKind::USER_READER_NO_KEY);
        let writer = EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY);
        let heartbeat = Submessage::Heartbeat(Heartbeat::new(
            reader,
            writer,
            SequenceNumber::FIRST,
            SequenceNumber::new(9),
            3,
        ));
        let unaligned = Submessage::Data(crate::messages::Data::new(
            reader,
            writer,
            SequenceNumber::FIRST,
            crate::messages::DataPayload::Data(SerializedPayload::new(vec![7_u8; 5])),
        ));
        let empty = Submessage::Pad(Pad::empty());

        for submessage in [heartbeat, unaligned, empty] {
            let octets = encode_submessage(&submessage).expect("encode");
            assert_eq!(octets.len(), submessage.serialized_len());
            assert_eq!(
                decode_submessage(&octets).expect("decode"),
                submessage,
                "{submessage}"
            );
        }

        assert!(matches!(
            decode_submessage(&[1, 2, 3]),
            Err(SecurityError::Truncated { .. })
        ));
    }

    #[test]
    fn the_prefix_and_postfix_carry_the_right_submessage_ids() {
        assert_eq!(header().to_submessage().id(), SubmessageId::SecurePrefix);
        assert_eq!(header().to_submessage().body_len(), CRYPTO_HEADER_LEN);
        let footer = CryptoFooter::new([0; COMMON_MAC_LEN])
            .to_submessage()
            .expect("encode");
        assert_eq!(footer.id(), SubmessageId::SecurePostfix);
        assert_eq!(footer.body_len(), CRYPTO_FOOTER_MIN_LEN);
    }
}
