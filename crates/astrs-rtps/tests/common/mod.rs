//! Shared helpers for the golden-packet tests.
//!
//! Every fixture in `tests/` is **spec-derived**: the octets were worked out
//! from the field tables of OMG DDSI-RTPS 2.3 and OMG CDR, field by field,
//! with the derivation written into the test beside them. Nothing here was
//! captured from a C or C++ DDS stack — cross-stack validation lives in a
//! separate out-of-repo project, per blueprint §18.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use astrs_rtps::messages::Message;
use astrs_rtps::structure::GuidPrefix;

/// The participant every fixture is sent from.
///
/// `41 53` is [`VendorId::ASTRS`](astrs_rtps::structure::VendorId::ASTRS) —
/// §9.3.1.5 puts the vendor id in the first two octets of a `guidPrefix` —
/// and the remaining ten are `01..0a` so an offset is readable in a hex dump.
pub const SENDER: GuidPrefix = GuidPrefix::new([
    0x41, 0x53, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a,
]);

/// A second participant, for the fixtures that need a destination.
pub const PEER: GuidPrefix = GuidPrefix::new([
    0x41, 0x53, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14,
]);

/// The twenty octets of a message header from [`SENDER`].
///
/// `"RTPS"`, version `2.3`, vendor `41 53`, then the prefix. None of the four
/// fields is byte-order sensitive (§9.3.1).
pub const SENDER_HEADER: [u8; 20] = [
    0x52, 0x54, 0x50, 0x53, // 'R' 'T' 'P' 'S'
    0x02, 0x03, // protocolVersion 2.3
    0x41, 0x53, // vendorId "AS"
    0x41, 0x53, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, // guidPrefix
];

/// Compare octets, reporting the first offset that differs and its context.
///
/// A plain `assert_eq!` on two hundred octets prints two hundred octets; this
/// prints the one that is wrong, which is the difference between a five-second
/// fix and a five-minute one.
#[track_caller]
pub fn assert_octets(actual: &[u8], expected: &[u8]) {
    if actual == expected {
        return;
    }
    let first = actual
        .iter()
        .zip(expected)
        .position(|(left, right)| left != right)
        .unwrap_or(actual.len().min(expected.len()));
    let from = first.saturating_sub(8);
    let to = (first + 8).min(actual.len().max(expected.len()));
    panic!(
        "octets differ at offset {first} (lengths {} vs {})\n  actual   [{from}..{to}]: {:02x?}\n  expected [{from}..{to}]: {:02x?}",
        actual.len(),
        expected.len(),
        &actual[from.min(actual.len())..to.min(actual.len())],
        &expected[from.min(expected.len())..to.min(expected.len())],
    );
}

/// Assert that `message` encodes to exactly `expected`, and that decoding
/// `expected` reproduces `message`.
///
/// Every golden fixture goes through this, so each one proves both
/// directions rather than only the one the author was thinking about.
#[track_caller]
pub fn assert_golden(message: &Message<'_>, expected: &[u8]) {
    let encoded = message.encode().expect("the fixture must encode");
    assert_octets(&encoded, expected);
    assert_eq!(
        encoded.len(),
        message.serialized_len(),
        "serialized_len disagrees with the octets written"
    );
    let decoded = Message::decode(expected).expect("the fixture must decode");
    assert_eq!(&decoded, message, "decode is not the inverse of encode");
    assert_eq!(
        decoded.encode().expect("re-encode"),
        expected,
        "re-encoding a decoded fixture must reproduce it"
    );
}

/// Concatenate labelled octet groups into one buffer.
///
/// Used where a fixture is more readable as a sequence of named fields than
/// as one flat array.
#[must_use]
pub fn concat(groups: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for group in groups {
        out.extend_from_slice(group);
    }
    out
}
