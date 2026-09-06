//! Property-based round-trip tests for the [`Beacon`] wire codec.
//!
//! Complements `beacon.rs`'s hand-written rejection matrix and single-byte
//! mutation sweep with broad, generated coverage of the field space: any
//! role, any (validly-shaped) peer identity, up to
//! [`astrs_discovery::defaults::MAX_BEACON_LISTEN_ADDRS`] addresses of
//! either IP family, and any HLC value must sign, encode, decode and
//! verify back to the exact same value (blueprint §5.2's test-estate
//! discipline: "Protocol snapshot + property tests (proptest) across
//! wire/data/cdr").

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use proptest::prelude::*;

use astrs_discovery::defaults::MAX_BEACON_LISTEN_ADDRS;
use astrs_discovery::{Beacon, BeaconRole};
use astrs_time::HlcTimestamp;
use astrs_wire::{AuthToken, DaemonId};

fn arb_role() -> impl Strategy<Value = BeaconRole> {
    prop_oneof![Just(BeaconRole::Coordinator), Just(BeaconRole::Daemon)]
}

fn arb_socket_addr() -> impl Strategy<Value = SocketAddr> {
    prop_oneof![
        (
            any::<u8>(),
            any::<u8>(),
            any::<u8>(),
            any::<u8>(),
            any::<u16>()
        )
            .prop_map(|(a, b, c, d, port)| SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(a, b, c, d)),
                port
            )),
        (any::<u128>(), any::<u16>())
            .prop_map(|(bits, port)| SocketAddr::new(IpAddr::V6(Ipv6Addr::from(bits)), port)),
    ]
}

/// A [`DaemonId`], generated through its own canonical text form so every
/// draw is guaranteed to be a value the real type could actually produce
/// (an optional lowercase-alphanumeric-and-hyphen label, plus a
/// syntactically valid UUID tail — see `astrs_wire::DaemonId`'s docs for
/// why the split is unambiguous).
fn arb_daemon_id() -> impl Strategy<Value = DaemonId> {
    (proptest::option::of("[a-z][a-z0-9-]{0,20}"), any::<u128>()).prop_map(|(label, uuid_bits)| {
        let uuid_text = format!(
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            (uuid_bits >> 96) as u32,
            (uuid_bits >> 80) as u16,
            (uuid_bits >> 64) as u16,
            (uuid_bits >> 48) as u16,
            uuid_bits & 0xFFFF_FFFF_FFFF,
        );
        let text = match label {
            Some(label) => format!("{label}-{uuid_text}"),
            None => uuid_text,
        };
        text.parse()
            .unwrap_or_else(|e| panic!("generated an invalid DaemonId text form {text:?}: {e}"))
    })
}

fn arb_hlc() -> impl Strategy<Value = HlcTimestamp> {
    (any::<u64>(), any::<u32>())
        .prop_map(|(physical, logical)| HlcTimestamp::new(physical, logical))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Sign -> encode -> decode -> verify round-trips exactly, for any
    /// combination of fields the real API can produce.
    #[test]
    fn beacon_round_trips_for_arbitrary_fields(
        role in arb_role(),
        machine_id in arb_daemon_id(),
        listen_addrs in proptest::collection::vec(arb_socket_addr(), 0..=MAX_BEACON_LISTEN_ADDRS),
        hlc in arb_hlc(),
        token_bytes in any::<[u8; 32]>(),
    ) {
        let token = AuthToken::from_bytes(token_bytes);
        let beacon = Beacon::signed(role, machine_id, listen_addrs, hlc, &token).unwrap();

        let bytes = beacon.to_bytes().unwrap();
        let decoded = Beacon::decode_and_verify(&bytes, &token).unwrap();

        prop_assert_eq!(decoded, beacon);
    }

    /// A beacon signed with one token never verifies against a different
    /// one — the whole point of `auth_tag`.
    #[test]
    fn a_beacon_never_verifies_against_a_different_token(
        role in arb_role(),
        machine_id in arb_daemon_id(),
        hlc in arb_hlc(),
        token_a in any::<[u8; 32]>(),
        token_b in any::<[u8; 32]>(),
    ) {
        prop_assume!(token_a != token_b);

        let beacon = Beacon::signed(role, machine_id, vec![], hlc, &AuthToken::from_bytes(token_a)).unwrap();
        let bytes = beacon.to_bytes().unwrap();

        let result = Beacon::decode_and_verify(&bytes, &AuthToken::from_bytes(token_b));
        prop_assert!(result.is_err());
    }
}
