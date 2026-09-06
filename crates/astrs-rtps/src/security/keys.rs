//! Pre-shared keys, the material derived from them, and per-session keys.
//!
//! # The chain, in one picture
//!
//! ```text
//!   Psk (>= 16 octets, operator-supplied)
//!     │  HKDF-SHA-256-Extract(salt = DOMAIN_SALT, ikm = psk || 0x00 || context)
//!     ▼
//!   prk (32)
//!     │  HKDF-Expand(prk, "…master key")  ──▶ master_key  (32)
//!     │  HKDF-Expand(prk, "…master salt") ──▶ master_salt (32)
//!     │  HKDF-Expand(prk, "…key id")      ──▶ key_id      (u32, on the wire)
//!     ▼
//!   KeyMaterial
//!     │  HKDF-Extract(salt = master_salt, ikm = master_key)
//!     │  HKDF-Expand(…, kind ‖ key_id ‖ session_id) ──▶ session key (16 or 32)
//!     ▼
//!   SessionKey — used by exactly one (transformation kind, session id)
//! ```
//!
//! Three properties this buys, and each is the reason for one arrow:
//!
//! 1. **The pre-shared key never touches an AEAD.** An operator may supply a
//!    long-lived secret; what reaches AES-GCM is a key that is used for at
//!    most [`SESSION_MESSAGE_LIMIT`] submessages.
//! 2. **The key id is derived, not configured.** Both peers compute the same
//!    four octets from the same key, so a receiver can find the right key
//!    material before running any cryptography — and a peer holding a
//!    *different* key produces a different id and is rejected by a map lookup
//!    rather than by a tag failure. That is the cheap rejection path, and it
//!    leaks nothing the attacker did not already supply.
//! 3. **Context separates endpoints.** Two topics configured with the same
//!    pre-shared key derive different material, because the topic and type
//!    name go into the extract step. Sharing a key across a deployment does
//!    not mean sharing a key across its topics.
//!
//! # What is *not* here
//!
//! No handshake, no certificate, no permissions document. [`KeyMaterial`] is
//! reachable two ways: derived from a [`Psk`], and built field by field with
//! [`KeyMaterial::from_parts`]. The second exists so that when an
//! authenticated key agreement is added, it has somewhere to put its output
//! without any of this changing — see the [module documentation](super).

use core::fmt;

use oxicrypto::{Zeroize, ct_eq};

use crate::security::error::{SecurityError, SecurityResult, crypto_reason};
use crate::security::kind::TransformationKind;

/// Shortest pre-shared key this crate accepts, in octets.
///
/// Sixteen octets is 128 bits of key, which is the floor below which the
/// derived AES-256 session key would have less entropy than its length
/// suggests. A shorter key is refused rather than stretched: stretching would
/// hide the weakness behind a KDF that cannot create entropy.
pub const PSK_MIN_LEN: usize = 16;

/// Octets a freshly generated pre-shared key carries.
pub const PSK_GENERATED_LEN: usize = 32;

/// Octets of derived master key.
pub const MASTER_KEY_LEN: usize = 32;

/// Octets of derived master salt.
pub const MASTER_SALT_LEN: usize = 32;

/// Longest session key any transformation kind needs.
pub const MAX_SESSION_KEY_LEN: usize = 32;

/// Submessages one session key protects before the session rolls.
///
/// AES-GCM's birthday bound on a 96-bit nonce is far above this; the limit is
/// low on purpose, so that a compromised session key exposes a bounded window
/// and so that the roll path is exercised by ordinary traffic rather than only
/// by a test. At a thousand samples a second a session lasts a little under
/// twelve days.
pub const SESSION_MESSAGE_LIMIT: u64 = 1 << 30;

/// The domain separator every derivation from a [`Psk`] starts with.
const DOMAIN_SALT: &[u8] = b"astrs-rtps/dds-security/v1";

/// The `info` string of the master-key expansion.
const INFO_MASTER_KEY: &[u8] = b"astrs-rtps/dds-security/v1 master key";

/// The `info` string of the master-salt expansion.
const INFO_MASTER_SALT: &[u8] = b"astrs-rtps/dds-security/v1 master salt";

/// The `info` string of the key-id expansion.
const INFO_KEY_ID: &[u8] = b"astrs-rtps/dds-security/v1 key id";

/// The `info` prefix of every session-key expansion.
const INFO_SESSION_KEY: &[u8] = b"astrs-rtps/dds-security/v1 session key";

/// The key id reserved for "no key" (§9.5.2.1.2 uses zero for the null
/// transformation), never handed out by [`KeyMaterial::from_psk`].
pub const KEY_ID_NONE: u32 = 0;

/// An operator-supplied pre-shared key.
///
/// Opaque on purpose. `Debug` prints its length and nothing else, `PartialEq`
/// compares in constant time, and the octets are zeroed when the value is
/// dropped — so a key that reaches a log line, a panic message or a core dump
/// does not reach an attacker.
#[derive(Clone)]
pub struct Psk {
    bytes: Vec<u8>,
}

impl Psk {
    /// Adopt caller-supplied octets.
    ///
    /// # Errors
    ///
    /// [`SecurityError::KeyTooShort`] below [`PSK_MIN_LEN`].
    pub fn new(bytes: impl Into<Vec<u8>>) -> SecurityResult<Self> {
        let bytes = bytes.into();
        if bytes.len() < PSK_MIN_LEN {
            return Err(SecurityError::KeyTooShort {
                len: bytes.len(),
                minimum: PSK_MIN_LEN,
            });
        }
        Ok(Self { bytes })
    }

    /// Mint a fresh key from the platform CSPRNG.
    ///
    /// # Errors
    ///
    /// [`SecurityError::RandomnessUnavailable`] when the CSPRNG fails. There
    /// is no fallback to a weaker generator, for the reason
    /// [`astrs_wire::AuthToken`](https://docs.rs/astrs-wire) gives: a
    /// predictable key is worse than no key, because it looks like one.
    pub fn generate() -> SecurityResult<Self> {
        let bytes = oxicrypto::random_bytes(PSK_GENERATED_LEN).map_err(|error| {
            SecurityError::RandomnessUnavailable {
                reason: crypto_reason(&error),
            }
        })?;
        Self::new(bytes)
    }

    /// Parse a lower- or upper-case hexadecimal key, as a configuration file
    /// would carry it.
    ///
    /// # Errors
    ///
    /// [`SecurityError::MalformedHexKey`] for an odd length or a non-hex
    /// digit, and [`SecurityError::KeyTooShort`] for a key below
    /// [`PSK_MIN_LEN`] once decoded.
    pub fn from_hex(text: &str) -> SecurityResult<Self> {
        if !text.len().is_multiple_of(2) {
            return Err(SecurityError::MalformedHexKey);
        }
        let digits = text.as_bytes();
        let mut bytes = Vec::with_capacity(digits.len() / 2);
        // The even-length check above leaves `as_chunks::<2>` no remainder,
        // so every digit is consumed and the pair destructures infallibly.
        for &[high, low] in digits.as_chunks::<2>().0 {
            bytes.push((hex_digit(high)? << 4) | hex_digit(low)?);
        }
        Self::new(bytes)
    }

    /// The key as hexadecimal.
    ///
    /// Named `reveal_` because calling it is a decision: the return value is
    /// the secret in a form that will be copied, logged and pasted.
    #[must_use]
    pub fn reveal_hex(&self) -> String {
        let mut text = String::with_capacity(self.bytes.len() * 2);
        for byte in &self.bytes {
            text.push_str(&format!("{byte:02x}"));
        }
        text
    }

    /// Octets in the key.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Always false: a [`Psk`] cannot be constructed empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The octets, for a derivation step that needs them.
    #[must_use]
    pub(crate) fn expose(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for Psk {
    /// Prints the length and nothing else.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Psk({} octets, redacted)", self.bytes.len())
    }
}

impl PartialEq for Psk {
    /// Constant time in the octets, so comparing two keys does not time-leak
    /// how long a common prefix they share.
    fn eq(&self, other: &Self) -> bool {
        self.bytes.len() == other.bytes.len() && ct_eq(&self.bytes, &other.bytes)
    }
}

impl Eq for Psk {}

impl Drop for Psk {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

/// Turn one hexadecimal digit into its value.
const fn hex_digit(byte: u8) -> SecurityResult<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(SecurityError::MalformedHexKey),
    }
}

/// A session key, zeroed when it goes out of scope.
#[derive(Clone)]
pub struct SessionKey {
    bytes: Vec<u8>,
}

impl SessionKey {
    /// The key octets, for the AEAD call that consumes them.
    #[must_use]
    pub(crate) fn expose(&self) -> &[u8] {
        &self.bytes
    }

    /// Octets in the key.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Always false: a session key is derived at a fixed nonzero length.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionKey({} octets, redacted)", self.bytes.len())
    }
}

impl Drop for SessionKey {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

/// Everything a peer needs to protect and verify traffic under one key.
///
/// The DDS-Security `KeyMaterial_AES_GCM_GMAC` of §9.5.2.1.2, minus the
/// receiver-specific half this release does not use: a master key, a master
/// salt, and the id that names them on the wire.
#[derive(Clone)]
pub struct KeyMaterial {
    key_id: u32,
    master_key: [u8; MASTER_KEY_LEN],
    master_salt: [u8; MASTER_SALT_LEN],
}

impl KeyMaterial {
    /// Derive material from a pre-shared key and a context string.
    ///
    /// `context` separates endpoints that share a key — pass the topic and
    /// type name, or an empty string when there is nothing to separate. Both
    /// peers must pass the same string, which is why it is the topic rather
    /// than anything either side chooses locally.
    ///
    /// # Errors
    ///
    /// [`SecurityError::KeyDerivation`] if HKDF refuses, which the fixed
    /// output lengths here make unreachable.
    pub fn from_psk(psk: &Psk, context: &str) -> SecurityResult<Self> {
        let mut ikm = Vec::with_capacity(psk.len() + 1 + context.len());
        ikm.extend_from_slice(psk.expose());
        ikm.push(0);
        ikm.extend_from_slice(context.as_bytes());
        let prk = oxicrypto::hkdf_sha256_extract(DOMAIN_SALT, &ikm);
        ikm.zeroize();

        let mut master_key = [0_u8; MASTER_KEY_LEN];
        expand(&prk, INFO_MASTER_KEY, &mut master_key)?;
        let mut master_salt = [0_u8; MASTER_SALT_LEN];
        expand(&prk, INFO_MASTER_SALT, &mut master_salt)?;
        let mut id_octets = [0_u8; 4];
        expand(&prk, INFO_KEY_ID, &mut id_octets)?;

        let derived = u32::from_be_bytes(id_octets);
        // Zero means "no key" on the wire, so the one derivation in four
        // billion that lands there is nudged rather than handed out.
        let key_id = if derived == KEY_ID_NONE { 1 } else { derived };

        Ok(Self {
            key_id,
            master_key,
            master_salt,
        })
    }

    /// Build material from parts, bypassing the pre-shared-key derivation.
    ///
    /// The seam an authenticated key agreement plugs into: a handshake that
    /// produces a master key, a master salt and an agreed key id has
    /// somewhere to put them, and everything downstream — session derivation,
    /// the wire format, the replay window — is unchanged.
    ///
    /// It is also how a test builds two peers that agree on a key id and
    /// disagree on the key, which is the only way to reach the tag-failure
    /// rejection path rather than the key-id-lookup one.
    #[must_use]
    pub const fn from_parts(
        key_id: u32,
        master_key: [u8; MASTER_KEY_LEN],
        master_salt: [u8; MASTER_SALT_LEN],
    ) -> Self {
        Self {
            key_id,
            master_key,
            master_salt,
        }
    }

    /// The id that names this material on the wire.
    #[must_use]
    pub const fn key_id(&self) -> u32 {
        self.key_id
    }

    /// Derive the key one session of one transformation kind uses.
    ///
    /// The kind is bound into the derivation, so the same session id under
    /// `AES256_GMAC` and under `AES256_GCM` produces different keys — a
    /// transformation kind an attacker flips in the crypto header therefore
    /// selects a key that cannot verify, whatever the AAD profile is.
    ///
    /// # Errors
    ///
    /// [`SecurityError::KeyDerivation`] if HKDF refuses, and
    /// [`SecurityError::UnsupportedTransformation`] for
    /// [`TransformationKind::None`], which has no key to derive.
    pub fn session_key(
        &self,
        kind: TransformationKind,
        session_id: u32,
    ) -> SecurityResult<SessionKey> {
        let key_len = kind.key_len();
        if key_len == 0 {
            return Err(SecurityError::UnsupportedTransformation {
                kind: kind.to_octets(),
            });
        }
        let prk = oxicrypto::hkdf_sha256_extract(&self.master_salt, &self.master_key);
        let mut info = Vec::with_capacity(INFO_SESSION_KEY.len() + 12);
        info.extend_from_slice(INFO_SESSION_KEY);
        info.extend_from_slice(&kind.to_octets());
        info.extend_from_slice(&self.key_id.to_be_bytes());
        info.extend_from_slice(&session_id.to_be_bytes());

        let mut bytes = vec![0_u8; key_len];
        expand(&prk, &info, &mut bytes)?;
        Ok(SessionKey { bytes })
    }
}

impl fmt::Debug for KeyMaterial {
    /// Prints the key id and nothing else. The id is public — it travels in
    /// every crypto header — and the two secrets never appear.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyMaterial(key id 0x{:08x}, redacted)", self.key_id)
    }
}

impl PartialEq for KeyMaterial {
    fn eq(&self, other: &Self) -> bool {
        self.key_id == other.key_id
            && ct_eq(&self.master_key, &other.master_key)
            && ct_eq(&self.master_salt, &other.master_salt)
    }
}

impl Eq for KeyMaterial {}

impl Drop for KeyMaterial {
    fn drop(&mut self) {
        self.master_key.zeroize();
        self.master_salt.zeroize();
    }
}

/// HKDF-Expand with the error mapped into this module's taxonomy.
fn expand(prk: &[u8], info: &[u8], out: &mut [u8]) -> SecurityResult<()> {
    oxicrypto::hkdf_sha256_expand(prk, info, out).map_err(|error| SecurityError::KeyDerivation {
        reason: crypto_reason(&error),
    })
}

/// The sending half of one key's session state.
///
/// Holds the session id, the counter that becomes the initialisation vector
/// suffix, and the derived key for the current session — so the ordinary path
/// through [`next`](SessionSender::next) is an increment and a comparison,
/// with HKDF running once per session rather than once per submessage.
#[derive(Debug)]
pub struct SessionSender {
    kind: TransformationKind,
    session_id: u32,
    counter: u64,
    key: SessionKey,
}

impl SessionSender {
    /// Start a sender at session zero, counter zero.
    ///
    /// # Errors
    ///
    /// Whatever [`KeyMaterial::session_key`] returns.
    pub fn new(material: &KeyMaterial, kind: TransformationKind) -> SecurityResult<Self> {
        Self::with_state(material, kind, 0, 0)
    }

    /// Start a sender at an explicit session and counter.
    ///
    /// The seam a test uses to park a sender one submessage below
    /// [`SESSION_MESSAGE_LIMIT`] and watch the session roll, and the seam a
    /// future implementation of persistent state would restore through.
    ///
    /// # Errors
    ///
    /// Whatever [`KeyMaterial::session_key`] returns.
    pub fn with_state(
        material: &KeyMaterial,
        kind: TransformationKind,
        session_id: u32,
        counter: u64,
    ) -> SecurityResult<Self> {
        Ok(Self {
            kind,
            session_id,
            counter,
            key: material.session_key(kind, session_id)?,
        })
    }

    /// The session the next submessage will be protected under.
    #[must_use]
    pub const fn session_id(&self) -> u32 {
        self.session_id
    }

    /// How many submessages this session has protected.
    #[must_use]
    pub const fn counter(&self) -> u64 {
        self.counter
    }

    /// Take the next (session id, counter, key), rolling the session when the
    /// current one is used up.
    ///
    /// The counter starts at one, never zero: zero is what an uninitialised
    /// replay window holds, and a counter that collides with it would be
    /// rejected as a replay of nothing.
    ///
    /// # Errors
    ///
    /// Whatever [`KeyMaterial::session_key`] returns when the session rolls.
    pub fn next(&mut self, material: &KeyMaterial) -> SecurityResult<(u32, u64, &SessionKey)> {
        if self.counter >= SESSION_MESSAGE_LIMIT {
            self.session_id = self.session_id.wrapping_add(1);
            self.counter = 0;
            self.key = material.session_key(self.kind, self.session_id)?;
        }
        self.counter = self.counter.saturating_add(1);
        Ok((self.session_id, self.counter, &self.key))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn psk() -> Psk {
        Psk::new(vec![0x5a_u8; 32]).expect("32 octets is long enough")
    }

    #[test]
    fn a_short_key_is_refused_rather_than_stretched() {
        let error = Psk::new(vec![1_u8; PSK_MIN_LEN - 1]).expect_err("too short");
        assert_eq!(
            error,
            SecurityError::KeyTooShort {
                len: PSK_MIN_LEN - 1,
                minimum: PSK_MIN_LEN,
            }
        );
        assert!(Psk::new(vec![1_u8; PSK_MIN_LEN]).is_ok(), "the floor is ok");
    }

    #[test]
    fn a_key_never_prints_itself() {
        let key = psk();
        let printed = format!("{key:?}");
        assert_eq!(printed, "Psk(32 octets, redacted)");
        assert!(!printed.contains("5a"), "the octets must not appear");
        assert_eq!(key.len(), 32);
        assert!(!key.is_empty());
    }

    #[test]
    fn hexadecimal_round_trips_and_rejects_what_is_not_hexadecimal() {
        let key = psk();
        let text = key.reveal_hex();
        assert_eq!(text.len(), 64);
        assert_eq!(Psk::from_hex(&text).expect("round trip"), key);
        assert_eq!(
            Psk::from_hex(&text.to_uppercase()).expect("upper case too"),
            key
        );

        assert_eq!(Psk::from_hex("abc"), Err(SecurityError::MalformedHexKey));
        assert_eq!(
            Psk::from_hex(&"zz".repeat(16)),
            Err(SecurityError::MalformedHexKey)
        );
        assert!(matches!(
            Psk::from_hex("00112233"),
            Err(SecurityError::KeyTooShort { .. })
        ));
    }

    #[test]
    fn two_generated_keys_differ() {
        let one = Psk::generate().expect("the CSPRNG must work");
        let two = Psk::generate().expect("the CSPRNG must work");
        assert_ne!(one, two);
        assert_eq!(one.len(), PSK_GENERATED_LEN);
    }

    #[test]
    fn derivation_is_deterministic_and_context_separated() {
        let key = psk();
        let a = KeyMaterial::from_psk(&key, "rt/chatter").expect("derive");
        let again = KeyMaterial::from_psk(&key, "rt/chatter").expect("derive");
        let elsewhere = KeyMaterial::from_psk(&key, "rt/odom").expect("derive");

        assert_eq!(a, again, "the same key and context give the same material");
        assert_ne!(
            a.key_id(),
            elsewhere.key_id(),
            "a different topic, a different key"
        );
        assert_ne!(a, elsewhere);

        let other_key = Psk::new(vec![0xa5_u8; 32]).expect("valid");
        let other = KeyMaterial::from_psk(&other_key, "rt/chatter").expect("derive");
        assert_ne!(
            a.key_id(),
            other.key_id(),
            "a different pre-shared key must be rejected by the id lookup, cheaply"
        );
    }

    #[test]
    fn a_derived_key_id_is_never_the_reserved_zero() {
        // Exhaustive is impossible; what is checkable is that the guard maps
        // zero to one and leaves everything else alone.
        let material = KeyMaterial::from_parts(KEY_ID_NONE, [0; 32], [0; 32]);
        assert_eq!(material.key_id(), KEY_ID_NONE, "from_parts does not nudge");
        for context in ["", "a", "rt/chatter", "rt/tf_static"] {
            let derived = KeyMaterial::from_psk(&psk(), context).expect("derive");
            assert_ne!(derived.key_id(), KEY_ID_NONE, "context {context:?}");
        }
    }

    #[test]
    fn material_never_prints_its_secrets() {
        let material = KeyMaterial::from_parts(0x0102_0304, [0xaa; 32], [0xbb; 32]);
        let printed = format!("{material:?}");
        assert_eq!(printed, "KeyMaterial(key id 0x01020304, redacted)");
        assert!(!printed.contains("aa"));
        assert!(!printed.contains("bb"));
    }

    #[test]
    fn a_session_key_is_a_function_of_kind_and_session_id() {
        let material = KeyMaterial::from_psk(&psk(), "rt/chatter").expect("derive");
        let gcm = material
            .session_key(TransformationKind::Aes256Gcm, 0)
            .expect("derive");
        let gmac = material
            .session_key(TransformationKind::Aes256Gmac, 0)
            .expect("derive");
        let later = material
            .session_key(TransformationKind::Aes256Gcm, 1)
            .expect("derive");

        assert_eq!(gcm.len(), 32);
        assert_ne!(
            gcm.expose(),
            gmac.expose(),
            "flipping the transformation kind must select a different key"
        );
        assert_ne!(gcm.expose(), later.expose(), "and so must the session id");

        let short = material
            .session_key(TransformationKind::Aes128Gcm, 0)
            .expect("derive");
        assert_eq!(short.len(), 16);
        assert!(!short.is_empty());
        assert_eq!(
            format!("{short:?}"),
            "SessionKey(16 octets, redacted)",
            "a session key never prints itself either"
        );
    }

    #[test]
    fn the_null_transformation_has_no_session_key() {
        let material = KeyMaterial::from_psk(&psk(), "").expect("derive");
        assert!(matches!(
            material.session_key(TransformationKind::None, 0),
            Err(SecurityError::UnsupportedTransformation { kind: [0, 0, 0, 0] })
        ));
    }

    #[test]
    fn a_sender_counts_from_one() {
        let material = KeyMaterial::from_psk(&psk(), "").expect("derive");
        let mut sender =
            SessionSender::new(&material, TransformationKind::Aes256Gcm).expect("start");
        assert_eq!(sender.session_id(), 0);
        assert_eq!(sender.counter(), 0);

        for expected in 1..=4_u64 {
            let (session, counter, key) = sender.next(&material).expect("next");
            assert_eq!(session, 0);
            assert_eq!(counter, expected, "counters start at one and never repeat");
            assert_eq!(key.len(), 32);
        }
        assert_eq!(sender.counter(), 4);
    }

    #[test]
    fn a_sender_rolls_the_session_when_its_key_is_used_up() {
        let material = KeyMaterial::from_psk(&psk(), "").expect("derive");
        let mut sender = SessionSender::with_state(
            &material,
            TransformationKind::Aes256Gcm,
            7,
            SESSION_MESSAGE_LIMIT - 1,
        )
        .expect("start");

        let first_key = material
            .session_key(TransformationKind::Aes256Gcm, 7)
            .expect("derive");
        let (session, counter, key) = sender.next(&material).expect("the last of session 7");
        assert_eq!((session, counter), (7, SESSION_MESSAGE_LIMIT));
        assert_eq!(key.expose(), first_key.expose());

        let (session, counter, key) = sender.next(&material).expect("session 8 begins");
        assert_eq!(
            (session, counter),
            (8, 1),
            "a fresh session restarts at one"
        );
        assert_ne!(
            key.expose(),
            first_key.expose(),
            "and brings a different key with it"
        );
        assert_eq!(sender.session_id(), 8);
    }
}
