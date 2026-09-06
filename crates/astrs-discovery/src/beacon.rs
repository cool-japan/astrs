//! [`Beacon`] — the periodic UDP announcement a coordinator or daemon sends
//! so the rest of the cluster can find it (blueprint §6.4, §4.2).
//!
//! # Construction
//!
//! A beacon carries the sender's role, its per-process identity, the
//! addresses it can be reached at, the protocol version it speaks, and an
//! HLC timestamp — plus an `auth_tag` that lets a receiver ignore beacons
//! from a foreign or rogue cluster *without* treating them as a decode
//! error (a receiver sharing the multicast group with an unrelated AstRS
//! deployment, or with nothing related to AstRS at all, is expected, not
//! exceptional).
//!
//! `auth_tag` is a **truncated HMAC-SHA-384**, computed as:
//!
//! ```text
//! auth_tag = HMAC-SHA-384(key = cluster_token, msg = MAC_CONTEXT || fields)[..16]
//! ```
//!
//! where `fields` is `role || machine_id || len(listen_addrs) ||
//! listen_addrs[..] || protocol || hlc`, each encoded with the same
//! little-endian-varint `oxicode` configuration as the rest of the AstRS
//! wire (blueprint §7.1), and `MAC_CONTEXT` is the fixed label
//! [`crate::defaults::BEACON_MAC_CONTEXT`].
//!
//! HMAC-SHA-384 (rather than a plain hash of `token || fields`) is chosen
//! because it is the one keyed-MAC construction `oxicrypto`'s facade
//! re-exports with a built-in truncation helper
//! ([`HmacSha384::mac_truncated`]/[`HmacSha384::verify_truncated`], which
//! also compares in constant time); the RFC 2104 HMAC construction accepts
//! a key shorter than the hash's block size without any special-casing (the
//! key is zero-padded internally), so AstRS's 32-byte
//! [`astrs_wire::AuthToken`] is used directly as the HMAC key. The domain
//! label guards against the same token also being the seed for the QUIC PSK
//! (blueprint §16): a value derived from the token for one purpose can
//! never be replayed as if it were derived for another.
//!
//! **Canonicalization.** [`Beacon::signed`] and [`Beacon::verify`] both
//! build the MAC input through the single private `mac_message` function
//! — there is no second, hand-inlined copy of the field encoding to drift
//! out of sync with the first. `listen_addrs` is never reordered,
//! deduplicated or otherwise normalized between signing and sending: wire
//! order is canonical order, by construction, because the exact `Vec` that
//! was signed is the one that gets encoded.
//!
//! # Identity and dedup
//!
//! `machine_id` is meant to identify one **process incarnation**, not a
//! persistent machine: callers must mint a fresh [`astrs_wire::DaemonId`]
//! with [`astrs_wire::DaemonId::generate`] once per process lifetime, never
//! reuse one across a restart. [`crate::watcher::PeerTable`] depends on
//! this — it is what lets a restarted daemon be recognized as a new peer
//! (a fresh `Discovered`) rather than colliding with stale state from
//! before the restart. As a second line of defense,
//! [`crate::watcher::PeerTable::observe`] also emits a fresh `Refreshed`
//! whenever a known peer's advertised role or addresses change, even if its
//! HLC does not strictly advance — so a hypothetical future caller that
//! *does* reuse a `DaemonId` across restarts still converges, rather than
//! being wedged behind the older, larger HLC.

use std::fmt;
use std::net::SocketAddr;

use astrs_time::HlcTimestamp;
use astrs_wire::{AuthToken, DaemonId, PROTOCOL_VERSION, WireDecode, WireEncode};
use oxicode::de::Decoder;
use oxicode::enc::Encoder;
use oxicode::{Decode, Encode};
use oxicrypto::{CryptoError, HmacSha384};
use serde::{Deserialize, Serialize};

use crate::defaults::{AUTH_TAG_LEN, BEACON_MAC_CONTEXT, MAX_BEACON_LISTEN_ADDRS};
use crate::error::{DiscoveryError, DiscoveryResult};

/// The role the sender of a [`Beacon`] plays in the cluster (blueprint
/// §4.2).
///
/// `#[non_exhaustive]` and encoded by stable variant index
/// ([`#[oxicode(variant = N)]`][oxicode-variant]), per the blueprint's
/// append-only wire evolution rule (§3.4): a future role is added at the
/// tail with a new index, never by renumbering these two.
///
/// [oxicode-variant]: https://docs.rs/oxicode
///
/// # Examples
///
/// ```
/// use astrs_discovery::BeaconRole;
///
/// assert_eq!(BeaconRole::Coordinator.as_str(), "coordinator");
/// assert_ne!(BeaconRole::Coordinator, BeaconRole::Daemon);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BeaconRole {
    /// The cluster's single coordinator (blueprint §4.2): owns dataflow
    /// lifecycle, the daemon registry and the parameter store.
    #[oxicode(variant = 0)]
    Coordinator,
    /// One per-machine daemon (blueprint §4.2): spawns and supervises
    /// nodes, brokers the local SHM plane, bridges remote routes.
    #[oxicode(variant = 1)]
    Daemon,
}

impl BeaconRole {
    /// Every role this build knows, in discriminant order.
    pub const ALL: &'static [Self] = &[Self::Coordinator, Self::Daemon];

    /// The lower-case noun used in logs and `--json` output.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_discovery::BeaconRole;
    ///
    /// assert_eq!(BeaconRole::Daemon.as_str(), "daemon");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Coordinator => "coordinator",
            Self::Daemon => "daemon",
        }
    }
}

impl fmt::Display for BeaconRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Builds the exact byte sequence [`Beacon::auth_tag`] authenticates.
///
/// Used identically by [`Beacon::signed`] (to compute the tag) and
/// [`Beacon::verify`] (to recompute it for comparison) — see the module
/// docs' "Construction" section for the wire shape. `listen_addrs` is
/// walked and encoded one element at a time (rather than delegated to
/// `Vec<SocketAddr>`'s own `Encode`) purely so this function has no
/// dependency on how that impl happens to be shaped; the two encodings are
/// not required to match each other, only to be internally deterministic,
/// which an explicit length prefix plus a fixed per-element encoding
/// guarantees regardless.
fn mac_message(
    role: BeaconRole,
    machine_id: &DaemonId,
    listen_addrs: &[SocketAddr],
    protocol: u16,
    hlc: HlcTimestamp,
) -> DiscoveryResult<Vec<u8>> {
    let mut msg = Vec::with_capacity(BEACON_MAC_CONTEXT.len() + 64);
    msg.extend_from_slice(BEACON_MAC_CONTEXT);
    role.encode_presized(&mut msg)?;
    machine_id.encode_presized(&mut msg)?;
    let addr_count = u64::try_from(listen_addrs.len()).unwrap_or(u64::MAX);
    addr_count.encode_presized(&mut msg)?;
    for addr in listen_addrs {
        addr.encode_presized(&mut msg)?;
    }
    protocol.encode_presized(&mut msg)?;
    hlc.encode_presized(&mut msg)?;
    Ok(msg)
}

/// A periodic AstRS discovery announcement (blueprint §6.4).
///
/// See the module docs for the `auth_tag` construction and the identity
/// contract `machine_id` carries. `role`/`listen_addrs`/`protocol`/`hlc`
/// are exactly what `astrs doctor` or a TUI peer view would want to render;
/// they are public so a caller who has already verified a beacon (or who
/// is only using this type as a plain data holder — for `astrs run`'s
/// single-process mode, say) is never forced through `oxicrypto` just to
/// read a field.
///
/// # Examples
///
/// ```
/// use astrs_discovery::{Beacon, BeaconRole};
/// use astrs_time::HlcTimestamp;
/// use astrs_wire::{AuthToken, DaemonId, MachineName};
///
/// let token = AuthToken::from_bytes([7; 32]);
/// let machine_id = DaemonId::generate(Some(MachineName::new("robot-01")?));
/// let beacon = Beacon::signed(
///     BeaconRole::Daemon,
///     machine_id,
///     vec!["10.0.0.5:7408".parse()?],
///     HlcTimestamp::new(1_000, 0),
///     &token,
/// )?;
///
/// let bytes = beacon.to_bytes()?;
/// let decoded = Beacon::decode_and_verify(&bytes, &token)?;
/// assert_eq!(decoded, beacon);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Beacon {
    /// Whether the sender is the cluster coordinator or a daemon.
    pub role: BeaconRole,
    /// The sender's per-process identity. See the module docs' "Identity
    /// and dedup" section for why this must be fresh per process start.
    pub machine_id: DaemonId,
    /// Addresses the sender can be reached at for its `role` (the
    /// coordinator's TCP 7407, or a daemon's TCP 7408 / UDS path rendered
    /// as a placeholder loopback address — the daemon owns that mapping).
    ///
    /// Capped at [`MAX_BEACON_LISTEN_ADDRS`]
    /// entries; see [`Beacon`]'s `Decode` impl.
    pub listen_addrs: Vec<SocketAddr>,
    /// The wire protocol version the sender speaks
    /// ([`astrs_wire::PROTOCOL_VERSION`] at signing time).
    ///
    /// A beacon is never rejected for carrying a different value than the
    /// receiver's own — actual protocol negotiation happens later, at the
    /// `Hello`/`Welcome` handshake (blueprint §7.2). This field exists so a
    /// caller (`astrs doctor`, the TUI) can flag a version-mismatched peer
    /// before a connection is even attempted.
    pub protocol: u16,
    /// The sender's HLC timestamp at the moment this beacon was built.
    ///
    /// Strictly increasing across successive beacons from the same
    /// `machine_id` under normal operation; [`crate::watcher::PeerTable`]
    /// uses it to dedup and order announcements.
    pub hlc: HlcTimestamp,
    /// A truncated HMAC-SHA-384 over the fields above, keyed by the
    /// cluster's [`AuthToken`]. See the module docs for the exact
    /// construction.
    pub auth_tag: [u8; AUTH_TAG_LEN],
}

impl Beacon {
    /// Builds and signs a beacon.
    ///
    /// # Errors
    ///
    /// [`DiscoveryError::Wire`] if a field cannot be encoded (unreachable
    /// for any value actually constructible through this crate's public
    /// API); [`DiscoveryError::Mac`] if the underlying HMAC implementation
    /// fails for a reason other than a tag mismatch (also expected to be
    /// unreachable — see [`DiscoveryError::Mac`]'s docs).
    ///
    /// # Examples
    ///
    /// See the [`Beacon`] type docs.
    pub fn signed(
        role: BeaconRole,
        machine_id: DaemonId,
        listen_addrs: Vec<SocketAddr>,
        hlc: HlcTimestamp,
        token: &AuthToken,
    ) -> DiscoveryResult<Self> {
        let protocol = PROTOCOL_VERSION;
        let message = mac_message(role, &machine_id, &listen_addrs, protocol, hlc)?;
        let mut auth_tag = [0u8; AUTH_TAG_LEN];
        HmacSha384.mac_truncated(token.reveal_bytes(), &message, &mut auth_tag)?;
        Ok(Self {
            role,
            machine_id,
            listen_addrs,
            protocol,
            hlc,
            auth_tag,
        })
    }

    /// Verifies this beacon's `auth_tag` against `token`, in constant time.
    ///
    /// # Errors
    ///
    /// [`DiscoveryError::AuthRejected`] if the tag does not match — the
    /// expected outcome for a beacon from a different cluster, not a bug.
    /// [`DiscoveryError::Wire`]/[`DiscoveryError::Mac`] only in the
    /// unreachable cases documented on [`Beacon::signed`].
    ///
    /// # Known limitation: no replay window
    ///
    /// This checks only that `auth_tag` binds the fields it was computed
    /// over to *some* holder of `token` — it does not bind them to *time*.
    /// A verbatim byte-for-byte capture of a legitimate beacon, replayed
    /// after the original sender has already aged out via
    /// [`crate::watcher::PeerTable::sweep`], decodes and verifies exactly
    /// as it did the first time and produces a fresh
    /// [`crate::watcher::BeaconEvent::Discovered`]. Blueprint §16 scopes
    /// this crate's authentication to "foreign/rogue beacons are ignored"
    /// (the actual, tested threat model here — see the rejection matrix in
    /// this module's tests), not to a replay-resistant liveness protocol;
    /// DDS-Security-grade replay protection is explicitly out of scope for
    /// 0.1.0 (§16, §1.3). [`crate::rendezvous::discover_coordinator`]
    /// inherits the same property: it applies no HLC-freshness check of
    /// its own to the beacons it collects.
    pub fn verify(&self, token: &AuthToken) -> DiscoveryResult<()> {
        let message = mac_message(
            self.role,
            &self.machine_id,
            &self.listen_addrs,
            self.protocol,
            self.hlc,
        )?;
        match HmacSha384.verify_truncated(token.reveal_bytes(), &message, &self.auth_tag) {
            Ok(()) => Ok(()),
            Err(CryptoError::InvalidTag) => Err(DiscoveryError::AuthRejected {
                role: self.role,
                machine_id: self.machine_id.clone(),
            }),
            Err(other) => Err(DiscoveryError::Mac(other)),
        }
    }

    /// Decodes a beacon from wire bytes and verifies its `auth_tag` in one
    /// step — the sanctioned way to turn an inbound UDP datagram's payload
    /// into a trusted [`Beacon`].
    ///
    /// A structurally invalid datagram and a well-formed-but-wrong-cluster
    /// one are distinguished by the returned error variant
    /// ([`DiscoveryError::Wire`] vs. [`DiscoveryError::AuthRejected`]), but
    /// both are equally safe for a caller to treat the same way: log at low
    /// severity, drop the packet, keep listening. Neither ever panics on
    /// attacker-controlled input. A decode failure rejects the *entire*
    /// input, including trailing bytes after an otherwise-valid beacon
    /// (blueprint §7.1's "reject trailing bytes" rule).
    ///
    /// # Errors
    ///
    /// See [`Beacon::verify`] and [`Beacon`]'s `Decode` impl.
    ///
    /// # Examples
    ///
    /// See the [`Beacon`] type docs.
    pub fn decode_and_verify(bytes: &[u8], token: &AuthToken) -> DiscoveryResult<Self> {
        let beacon: Self = WireDecode::decode_exact(bytes)?;
        beacon.verify(token)?;
        Ok(beacon)
    }

    /// Encodes this beacon to its wire form (the exact bytes a
    /// [`crate::socket::DiscoverySocket`] sends).
    ///
    /// # Errors
    ///
    /// [`DiscoveryError::Wire`] if encoding fails — unreachable for any
    /// value actually constructible through this crate's public API.
    ///
    /// # Examples
    ///
    /// See the [`Beacon`] type docs.
    pub fn to_bytes(&self) -> DiscoveryResult<Vec<u8>> {
        Ok(WireEncode::encode_to_vec(self)?)
    }

    /// Whether [`Beacon::protocol`] matches this build's
    /// [`astrs_wire::PROTOCOL_VERSION`].
    ///
    /// A `false` result is informational, not a reason to drop the beacon
    /// — see [`Beacon::protocol`]'s docs.
    #[must_use]
    pub const fn speaks_our_protocol(&self) -> bool {
        self.protocol == PROTOCOL_VERSION
    }
}

impl Encode for Beacon {
    fn encode<E: Encoder>(&self, encoder: &mut E) -> Result<(), oxicode::error::Error> {
        self.role.encode(encoder)?;
        self.machine_id.encode(encoder)?;
        self.listen_addrs.encode(encoder)?;
        self.protocol.encode(encoder)?;
        self.hlc.encode(encoder)?;
        self.auth_tag.encode(encoder)
    }
}

impl Decode for Beacon {
    /// Decodes a beacon, rejecting one that declares more than
    /// [`MAX_BEACON_LISTEN_ADDRS`]
    /// addresses.
    ///
    /// This runs *in addition to* `oxicode`'s own protection against a
    /// forged length prefix driving an unbounded allocation (every
    /// collection decode claims its declared length against the decoder's
    /// remaining-input budget before allocating); the check here is a
    /// protocol-level sanity bound; a beacon advertising more than a
    /// handful of addresses is nonsensical regardless of whether decoding
    /// it would have been safe.
    ///
    /// Not generic over `oxicode`'s `Decode::Context` (unlike, say,
    /// `astrs_wire::DaemonId`'s hand-written impl): [`BeaconRole`] and
    /// [`HlcTimestamp`] are both `#[derive(Decode)]`, which only
    /// implements the default `Context = ()`, so any type embedding them
    /// is pinned to `()` too. That default is exactly what
    /// `astrs_wire::WireDecode`'s blanket impl (and therefore
    /// [`Beacon::decode_and_verify`]) requires.
    fn decode<D: Decoder<Context = ()>>(decoder: &mut D) -> Result<Self, oxicode::error::Error> {
        let role = BeaconRole::decode(decoder)?;
        let machine_id = DaemonId::decode(decoder)?;
        let listen_addrs = Vec::<SocketAddr>::decode(decoder)?;
        if listen_addrs.len() > MAX_BEACON_LISTEN_ADDRS {
            return Err(oxicode::error::Error::OwnedCustom {
                message: format!(
                    "beacon carries {} listen addresses, over the {MAX_BEACON_LISTEN_ADDRS}-address limit",
                    listen_addrs.len()
                ),
            });
        }
        let protocol = u16::decode(decoder)?;
        let hlc = HlcTimestamp::decode(decoder)?;
        let auth_tag = <[u8; AUTH_TAG_LEN]>::decode(decoder)?;
        Ok(Self {
            role,
            machine_id,
            listen_addrs,
            protocol,
            hlc,
            auth_tag,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use oxicrypto::Mac;

    fn token(byte: u8) -> AuthToken {
        AuthToken::from_bytes([byte; 32])
    }

    fn machine_id(label: &str) -> DaemonId {
        // Avoids a direct `uuid` crate dependency in this crate: `DaemonId`
        // already round-trips through its `<label>-<uuid>` text form, so a
        // fixed, syntactically valid UUID tail is enough for a test fixture.
        format!("{label}-00000000-0000-0000-0000-0000000000ab")
            .parse()
            .unwrap()
    }

    fn sample(token: &AuthToken) -> Beacon {
        Beacon::signed(
            BeaconRole::Daemon,
            machine_id("robot-01"),
            vec![
                "10.0.0.5:7408".parse().unwrap(),
                "[::1]:7408".parse().unwrap(),
            ],
            HlcTimestamp::new(1_000_000, 7),
            token,
        )
        .unwrap()
    }

    #[test]
    fn role_display_and_as_str_agree() {
        for role in BeaconRole::ALL {
            assert_eq!(role.to_string(), role.as_str());
        }
        assert_eq!(BeaconRole::ALL.len(), 2);
    }

    #[test]
    fn hmac_sha384_accepts_a_32_byte_key() {
        // AuthToken is 32 bytes; HmacSha384::min_key_len() advertises 48
        // (the RFC 2104-recommended length equal to the hash output), but
        // HMAC's own construction zero-pads any shorter key internally and
        // never rejects it. Pin that behavior directly, since the whole
        // `auth_tag` construction depends on it.
        let key = [0x11u8; 32];
        let mut tag = [0u8; 48];
        HmacSha384.mac(&key, b"hello", &mut tag).unwrap();
        assert!(HmacSha384.verify(&key, b"hello", &tag).is_ok());
    }

    #[test]
    fn signed_beacon_verifies_against_the_same_token() {
        let token = token(1);
        let beacon = sample(&token);
        assert!(beacon.verify(&token).is_ok());
    }

    #[test]
    fn codec_round_trips() {
        let token = token(2);
        let beacon = sample(&token);
        let bytes = beacon.to_bytes().unwrap();
        let decoded = Beacon::decode_and_verify(&bytes, &token).unwrap();
        assert_eq!(decoded, beacon);
    }

    #[test]
    fn empty_listen_addrs_round_trips() {
        let token = token(3);
        let beacon = Beacon::signed(
            BeaconRole::Coordinator,
            machine_id("coord"),
            Vec::new(),
            HlcTimestamp::EPOCH,
            &token,
        )
        .unwrap();
        let bytes = beacon.to_bytes().unwrap();
        assert_eq!(Beacon::decode_and_verify(&bytes, &token).unwrap(), beacon);
    }

    // --- Rejection matrix -------------------------------------------------
    //
    // Four independent ways a datagram can fail to become a trusted
    // `Beacon`, each dropped without panicking, each leaving the next good
    // beacon free to decode normally afterward (the last assertion in every
    // case below).

    #[test]
    fn rejects_garbage_bytes_as_a_codec_error() {
        let token = token(4);
        let garbage = [0xFFu8; 3];
        let err = Beacon::decode_and_verify(&garbage, &token).unwrap_err();
        assert!(matches!(err, DiscoveryError::Wire(_)));

        // The rejection does not poison anything: a real beacon decodes
        // fine right after.
        let good = sample(&token).to_bytes().unwrap();
        assert!(Beacon::decode_and_verify(&good, &token).is_ok());
    }

    #[test]
    fn rejects_a_well_formed_beacon_signed_with_a_different_token() {
        let ours = token(5);
        let theirs = token(6);
        let foreign = sample(&theirs).to_bytes().unwrap();

        let err = Beacon::decode_and_verify(&foreign, &ours).unwrap_err();
        match err {
            DiscoveryError::AuthRejected { role, machine_id } => {
                assert_eq!(role, BeaconRole::Daemon);
                assert_eq!(machine_id.machine(), Some("robot-01"));
            }
            other => panic!("expected AuthRejected, got {other:?}"),
        }

        let good = sample(&ours).to_bytes().unwrap();
        assert!(Beacon::decode_and_verify(&good, &ours).is_ok());
    }

    #[test]
    fn rejects_an_unknown_role_discriminant_as_a_codec_error() {
        // Hand-encode a `BeaconRole` value this build does not know: a
        // hypothetical variant 2, which a newer peer might one day send.
        // The decoder must reject it rather than panicking on an
        // out-of-range enum discriminant.
        let token = token(7);
        let mut forged = Vec::new();
        2u32.encode_presized(&mut forged).unwrap(); // stand-in variant index
        machine_id("x").encode_presized(&mut forged).unwrap();
        Vec::<SocketAddr>::new()
            .encode_presized(&mut forged)
            .unwrap();
        PROTOCOL_VERSION.encode_presized(&mut forged).unwrap();
        HlcTimestamp::EPOCH.encode_presized(&mut forged).unwrap();
        [0u8; AUTH_TAG_LEN].encode_presized(&mut forged).unwrap();

        let err = Beacon::decode_and_verify(&forged, &token).unwrap_err();
        assert!(matches!(err, DiscoveryError::Wire(_)));

        let good = sample(&token).to_bytes().unwrap();
        assert!(Beacon::decode_and_verify(&good, &token).is_ok());
    }

    #[test]
    fn rejects_more_than_the_maximum_listen_addrs() {
        let token = token(8);
        let too_many: Vec<SocketAddr> = (0..=MAX_BEACON_LISTEN_ADDRS)
            .map(|i| SocketAddr::from(([10, 0, 0, u8::try_from(i % 255).unwrap_or(0)], 7408)))
            .collect();
        assert!(too_many.len() > MAX_BEACON_LISTEN_ADDRS);

        // `Beacon::signed` itself does not enforce the cap (it is a
        // decode-time protocol guard, not a construction-time one): a
        // beacon with too many addresses signs and encodes just fine, and
        // is only ever rejected on the receiving end, during `Decode`.
        let beacon = Beacon::signed(
            BeaconRole::Daemon,
            machine_id("many"),
            too_many,
            HlcTimestamp::new(1, 0),
            &token,
        )
        .unwrap();
        let bytes = beacon.to_bytes().unwrap();

        let err = Beacon::decode_and_verify(&bytes, &token).unwrap_err();
        assert!(matches!(err, DiscoveryError::Wire(_)));

        let good = sample(&token).to_bytes().unwrap();
        assert!(Beacon::decode_and_verify(&good, &token).is_ok());
    }

    #[test]
    fn mutating_any_byte_of_a_signed_beacon_never_verifies_with_the_original_bytes_intact() {
        // A cheap, deterministic fuzz: flipping any single byte of an
        // encoded beacon must never produce another value that both (a)
        // decodes and (b) verifies, since that would mean the tag failed to
        // bind the fields it claims to cover.
        let token = token(9);
        let beacon = sample(&token);
        let original = beacon.to_bytes().unwrap();

        for i in 0..original.len() {
            let mut mutated = original.clone();
            mutated[i] ^= 0xFF;
            if mutated == original {
                continue;
            }
            if let Ok(decoded) = Beacon::decode_and_verify(&mutated, &token) {
                assert_eq!(
                    decoded, beacon,
                    "byte {i} flip decoded+verified to a different beacon"
                );
            }
        }
    }

    #[test]
    fn speaks_our_protocol_reflects_the_protocol_field() {
        let token = token(10);
        let mut beacon = sample(&token);
        assert!(beacon.speaks_our_protocol());
        beacon.protocol = PROTOCOL_VERSION.wrapping_add(1);
        assert!(!beacon.speaks_our_protocol());
    }

    #[test]
    fn serde_json_round_trips_for_debug_json_boundaries() {
        let token = token(11);
        let beacon = sample(&token);
        let json = serde_json::to_string(&beacon).unwrap();
        let decoded: Beacon = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, beacon);
    }
}
