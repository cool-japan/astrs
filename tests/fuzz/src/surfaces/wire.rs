//! Deep-fuzz harness for `astrs-wire`'s framed codec (blueprint §7.1, §15).
//!
//! Attack surface: [`decode_frame`] and [`decode_frame_prefix`] — the wire's
//! front door, which must turn arbitrary octets into either a `FrameView` or
//! a typed `WireError`, never a panic, and never touch a byte the declared
//! `len` field did not already justify (checked *before* any buffer is
//! sized — see the frame module's own docs).
//!
//! The payload is opaque at this layer (`decode_frame` never interprets
//! it), so a valid seed only needs a well-formed header, flags and trailer —
//! no message-family knowledge is required to build one.

use astrs_wire::{
    Compression, FrameFlags, FrameKind, FrameLimits, decode_frame, decode_frame_prefix,
    encode_frame,
};

use crate::support::mutate;
use crate::support::rng::Rng;

/// Per-iteration cap on generated input length — comfortably past
/// [`HEADER_LEN`](astrs_wire::HEADER_LEN) plus a checksum, small enough that
/// millions of nightly-lane iterations stay fast.
pub const MAX_INPUT_LEN: usize = 8 * 1024;

/// Where the committed regression corpus for this surface lives.
pub const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/corpus/wire");

/// A small tight cap, so a good fraction of cases exercise the
/// "declared length rejected before allocation" path documented on
/// [`FrameLimits::check_payload_len`].
fn tight_limits() -> FrameLimits {
    FrameLimits::new().with_max_payload_bytes(48)
}

/// The [`FrameLimits`] configurations every case is checked against.
const LIMITS: &[fn() -> FrameLimits] = &[
    FrameLimits::new,
    FrameLimits::network,
    FrameLimits::uds,
    tight_limits,
];

/// Payload shapes varied enough to exercise short, empty, and multi-KB
/// bodies without needing to understand any payload's contents.
fn sample_payloads() -> Vec<Vec<u8>> {
    vec![
        Vec::new(),
        b"a".to_vec(),
        b"astrs-fuzz wire seed payload".to_vec(),
        vec![0u8; 256],
        vec![0xA5u8; 1024],
        vec![0xFFu8; 4096],
    ]
}

/// Valid encodings from [`encode_frame`], spanning every [`FrameKind`], both
/// CRC settings, and a couple of compression-flagged frames (whose payload
/// is opaque at this layer and so still decodes at the framing level).
#[must_use]
pub fn seeds() -> Vec<Vec<u8>> {
    let mut seeds = Vec::new();
    let limits = FrameLimits::new();
    for &kind in FrameKind::ALL {
        for flags in [FrameFlags::EMPTY, FrameFlags::CRC] {
            for payload in sample_payloads() {
                if let Ok(bytes) = encode_frame(kind, flags, &payload, &limits) {
                    seeds.push(bytes);
                }
            }
        }
    }
    for compression in [Compression::Lz4, Compression::Zstd] {
        let flags = FrameFlags::CRC.with_compression(compression);
        if let Ok(bytes) = encode_frame(FrameKind::Data, flags, b"opaque", &limits) {
            seeds.push(bytes);
        }
    }
    seeds
}

/// Unstructured bytes about a quarter of the time; a structure-aware
/// mutation of a randomly chosen seed otherwise.
#[must_use]
pub fn generate(rng: &mut Rng, seeds: &[Vec<u8>]) -> Vec<u8> {
    if rng.one_in(4) {
        return mutate::random_bytes(rng, MAX_INPUT_LEN);
    }
    match rng.pick(seeds) {
        Some(seed) => mutate::mutate(rng, seed, MAX_INPUT_LEN),
        None => mutate::random_bytes(rng, MAX_INPUT_LEN),
    }
}

/// The invariant: whatever `bytes` are, under whatever [`FrameLimits`],
/// [`decode_frame`] and [`decode_frame_prefix`] return a value or a typed
/// error — never a panic, and a prefix view never claims to have consumed
/// more than was actually present.
pub fn check(bytes: &[u8]) {
    for limits_fn in LIMITS {
        let limits = limits_fn();
        if let Ok(view) = decode_frame_prefix(bytes, &limits) {
            assert!(
                view.total_len() <= bytes.len(),
                "a prefix view consumed more than the input"
            );
            assert!(
                view.payload().len() <= limits.max_payload_bytes(),
                "a decoded payload exceeded the configured cap"
            );
        }
        // Whenever the exact-length entry point accepts `bytes`, the prefix
        // entry point must accept the same bytes and agree with it byte for
        // byte. This is the direction worth asserting: `decode_frame_prefix`
        // succeeds far more often than `decode_frame` does (it tolerates
        // trailing bytes, and every case here is checked under `tight_limits`
        // too), so the converse -- "prefix implies exact, when there is no
        // trailing data" -- is true but fires too rarely under the tight cap
        // to be a meaningful check on its own.
        if let Ok(exact_view) = decode_frame(bytes, &limits) {
            let prefix_result = decode_frame_prefix(bytes, &limits);
            assert!(
                prefix_result.is_ok(),
                "decode_frame accepted an input decode_frame_prefix rejected: {prefix_result:?}"
            );
            if let Ok(prefix_view) = prefix_result {
                assert_eq!(
                    prefix_view, exact_view,
                    "decode_frame and decode_frame_prefix disagreed on the same accepted input"
                );
                assert_eq!(
                    prefix_view.total_len(),
                    bytes.len(),
                    "decode_frame must consume every input byte"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn every_seed_actually_decodes() {
        let seeds = seeds();
        assert!(!seeds.is_empty());
        let limits = FrameLimits::new();
        for seed in &seeds {
            assert!(
                decode_frame(seed, &limits).is_ok(),
                "a wire seed did not decode under FrameLimits::new(): {seed:?}"
            );
        }
    }

    #[test]
    fn generate_never_exceeds_the_cap() {
        let seeds = seeds();
        let mut rng = Rng::new(123);
        for _ in 0..500 {
            assert!(generate(&mut rng, &seeds).len() <= MAX_INPUT_LEN);
        }
    }

    #[test]
    fn check_never_panics_on_a_handful_of_hand_picked_edge_cases() {
        for case in [
            Vec::new(),
            vec![0u8; 1],
            vec![0u8; 9],
            vec![0xFFu8; 10],
            vec![0xFFu8; 64],
        ] {
            check(&case);
        }
    }
}
