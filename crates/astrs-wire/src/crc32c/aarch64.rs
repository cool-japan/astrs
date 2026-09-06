//! aarch64 hardware CRC-32C via the ARMv8 CRC32C instructions
//! (`crc32cx`/`crc32cb`, exposed as `__crc32cd`/`__crc32cb`).
//!
//! These compute exactly the Castagnoli CRC this crate uses — the ARMv8-A
//! CRC extension's "C" instructions are specifically CRC-32C, distinct from
//! the plain `crc32*` instructions that implement CRC-32/ISO-HDLC.
//! [`is_available`] runtime-detects the extension once
//! ([`std::arch::is_aarch64_feature_detected`]) and every call site funnels
//! through the single `unsafe` boundary at [`update_state_hw`]; the kernels
//! underneath it share that same feature guarantee via `#[target_feature]`
//! and so need no further `unsafe` blocks of their own (calling a
//! `#[target_feature]` function from another with a covering feature set is
//! safe by construction — "target feature 1.1", stable since Rust 1.61).
//!
//! Stability note (blueprint §20.1 W6 SIMD policy): `__crc32cb`, `__crc32ch`,
//! `__crc32cw`, `__crc32cd` and `is_aarch64_feature_detected!("crc")` were
//! confirmed callable on stable rustc 1.95 (this crate's MSRV) by a probe
//! compile-and-run before writing this module — no nightly feature gate is
//! needed for any of them.
//!
//! # The threshold, measured on this machine (aarch64 Apple Silicon)
//!
//! `benches/crc32c.rs`, run natively, drove [`THREE_WAY_THRESHOLD`]:
//!
//! - A first version of [`super::combine`] made every `interleaved` call pay
//!   for two from-scratch GF(2) matrix-exponentiation passes — `O(log n)`
//!   32×32 matrix *squarings*, not just applications. At a few KiB that cost
//!   *exceeded* the time to hash the buffer, so the first measurement showed
//!   `interleaved` losing to [`single_stream`] everywhere tested, by as much
//!   as 40× at 4 KiB. That is a bug in the combine implementation, not a
//!   verdict on interleaving — see [`super::combine`]'s module doc comment
//!   for the fix (a cached doubling chain).
//! - With that fixed, `interleaved` clearly wins at 64 KiB and above,
//!   roughly **2–3× versus [`single_stream`]** in the cleanest pairing
//!   available (64 KiB: 2.6 µs interleaved against 6.5 µs single-stream;
//!   1 MiB: 35.8 µs against 105.9 µs) — but the two figures come from two
//!   separate benchmark runs, not one paired run: this is a shared
//!   development machine, and this pair's own sub-3072-byte control points
//!   (identical `single_stream` code path in both runs) disagree by
//!   4–5× between the runs, so treat 2–3× as an estimated range, not a
//!   precise measurement. The *direction* is not in doubt — every attempt,
//!   clean or noisy, showed interleaved ahead at 64 KiB and 1 MiB — and it
//!   matches literature expectations for hiding `crc32cx`'s issue-to-result
//!   latency behind three independent chains. Still loses to
//!   [`single_stream`] at 4 KiB in every attempt: the fixed combine cost is
//!   far smaller than before, but not zero, and three independent loop
//!   cursors plus the interleaved loop's own bookkeeping still need a
//!   large-enough buffer to amortise against.
//! - This is a shared development machine with other agents' cargo
//!   processes running concurrently; several later re-measurement attempts,
//!   taken to pin the exact crossover point down further than "somewhere
//!   between 4 KiB and 64 KiB", landed under enough contention (confidence
//!   intervals several times wider, occasionally even reordering which
//!   kernel looked faster) that criterion itself reported the difference as
//!   not statistically significant. Rather than report a specific crossover
//!   number chased under those conditions, [`THREE_WAY_THRESHOLD`] is set
//!   inside the bracket the *clean* measurement establishes (a clear loss at
//!   4 KiB, a clear win at 64 KiB), on the conservative side of it.
//!
//! [`super::x86`]'s three-way kernel mirrors this one and shares this same
//! `combine`, but is tuned to the same threshold for a different reason —
//! see that module's doc comment.

use std::arch::aarch64::{__crc32cb, __crc32cd};
use std::sync::OnceLock;

use super::combine::combine;
use super::scalar::split_chunk;

/// Buffer length above which the three-way interleaved kernel is used
/// instead of a single sequential CRC32C stream.
///
/// See the module doc comment: this is a measured choice, tuned against
/// `benches/crc32c.rs` run natively on this machine.
const THREE_WAY_THRESHOLD: usize = 32 * 1024;

/// Reports whether the ARMv8 CRC32C instructions are usable on this CPU.
///
/// Checked once per process ([`OnceLock`]) — `is_aarch64_feature_detected!`
/// is itself already cached by `std`, but caching the `bool` here avoids
/// re-deriving it (and re-touching the detection machinery) on every call.
#[inline]
pub(crate) fn is_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| std::arch::is_aarch64_feature_detected!("crc"))
}

/// Folds `data` into the running, non-finalised CRC register `state`, using
/// the ARMv8 hardware CRC32C instructions.
///
/// # Safety
///
/// The caller must ensure [`is_available`] has returned `true` — this
/// executes the CRC32C instructions directly, which trap as an undefined
/// instruction on a core without the ARMv8 CRC extension.
#[target_feature(enable = "crc")]
pub(crate) unsafe fn update_state_hw(state: u32, data: &[u8]) -> u32 {
    if data.len() >= THREE_WAY_THRESHOLD {
        interleaved(state, data)
    } else {
        single_stream(state, data)
    }
}

/// Single sequential CRC32C stream, eight bytes per hardware instruction.
///
/// Not itself `unsafe fn`: like the intrinsics it calls, its safety
/// obligation is entirely expressed by `#[target_feature]` — callable
/// without an `unsafe` block from another function with a covering feature
/// set (such as [`update_state_hw`]), and rejected at compile time from
/// anywhere else.
#[target_feature(enable = "crc")]
fn single_stream(mut state: u32, data: &[u8]) -> u32 {
    let mut rest = data;
    while let Some((chunk, tail)) = split_chunk(rest) {
        let word = u64::from_le_bytes(*chunk);
        state = __crc32cd(state, word);
        rest = tail;
    }
    for &byte in rest {
        state = __crc32cb(state, byte);
    }
    state
}

/// Three-way interleaved CRC32C: three independent hardware register chains
/// over three roughly-equal thirds of `data`, combined back into one
/// register with [`combine`]. Mirrors [`super::x86`]'s kernel of the same
/// shape — see its doc comment for the combine direction and why seeding
/// the second/third chains from zero is exact, not an approximation.
#[target_feature(enable = "crc")]
fn interleaved(state: u32, data: &[u8]) -> u32 {
    let third = (data.len() / 3) & !7;
    if third == 0 {
        return single_stream(state, data);
    }

    let (a, rest) = data.split_at(third);
    let (b, rest) = rest.split_at(third);
    let (c_head, c_tail) = rest.split_at(third);

    let mut state_a = state;
    let mut state_b = 0u32;
    let mut state_c = 0u32;

    let mut ra = a;
    let mut rb = b;
    let mut rc = c_head;

    while let (Some((chunk_a, tail_a)), Some((chunk_b, tail_b)), Some((chunk_c, tail_c))) =
        (split_chunk(ra), split_chunk(rb), split_chunk(rc))
    {
        let word_a = u64::from_le_bytes(*chunk_a);
        let word_b = u64::from_le_bytes(*chunk_b);
        let word_c = u64::from_le_bytes(*chunk_c);
        state_a = __crc32cd(state_a, word_a);
        state_b = __crc32cd(state_b, word_b);
        state_c = __crc32cd(state_c, word_c);
        ra = tail_a;
        rb = tail_b;
        rc = tail_c;
    }

    let combined = combine(state_a, state_b, third as u64);
    let combined = combine(combined, state_c, third as u64);
    single_stream(combined, c_tail)
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::crc32c::scalar::update_state_scalar;
    use proptest::prelude::*;

    /// Runs `body` only when the CRC extension is actually present, so this
    /// suite stays green (rather than flaky) on a hypothetical aarch64 CI
    /// runner without it (e.g. an ARMv8.0 core with the optional CRC
    /// extension unimplemented).
    fn with_hw(body: impl FnOnce()) {
        if is_available() {
            body();
        } else {
            eprintln!("skipping: ARMv8 CRC extension not detected on this CPU");
        }
    }

    /// # Safety
    /// Only called after `is_available()` is confirmed, by every caller in
    /// this module.
    unsafe fn hw(state: u32, data: &[u8]) -> u32 {
        unsafe { update_state_hw(state, data) }
    }

    #[test]
    fn matches_scalar_on_the_published_check_value() {
        with_hw(|| {
            let expected = update_state_scalar(u32::MAX, b"123456789");
            assert_eq!(unsafe { hw(u32::MAX, b"123456789") }, expected);
        });
    }

    #[test]
    fn matches_scalar_exhaustively_for_every_length_up_to_256() {
        with_hw(|| {
            let data: Vec<u8> = (0..256u32)
                .map(|i| (i.wrapping_mul(97) & 0xFF) as u8)
                .collect();
            for len in 0..=data.len() {
                let slice = &data[..len];
                let expected = update_state_scalar(0xACE1_5A5A, slice);
                assert_eq!(unsafe { hw(0xACE1_5A5A, slice) }, expected, "len {len}");
            }
        });
    }

    #[test]
    fn matches_scalar_around_the_three_way_threshold_boundary() {
        with_hw(|| {
            for delta in -8i64..=8 {
                let len = (THREE_WAY_THRESHOLD as i64 + delta).max(0) as usize;
                let data: Vec<u8> = (0..len)
                    .map(|i| (i.wrapping_mul(131) & 0xFF) as u8)
                    .collect();
                let expected = update_state_scalar(0x1357_9BDF, &data);
                assert_eq!(unsafe { hw(0x1357_9BDF, &data) }, expected, "len {len}");
            }
        });
    }

    #[test]
    fn matches_scalar_at_every_chunk_multiple_near_the_threshold() {
        with_hw(|| {
            for len in (THREE_WAY_THRESHOLD - 64)..=(THREE_WAY_THRESHOLD + 64) {
                let data: Vec<u8> = (0..len)
                    .map(|i| (i.wrapping_mul(53) & 0xFF) as u8)
                    .collect();
                let expected = update_state_scalar(0x2222_3333, &data);
                assert_eq!(unsafe { hw(0x2222_3333, &data) }, expected, "len {len}");
            }
        });
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Arbitrary buffers up to 1 MiB, at arbitrary alignment offsets:
        /// slicing a larger backing buffer at odd offsets proves the 8-byte
        /// chunking never assumes the slice's base address is
        /// qword-aligned — `u64::from_le_bytes` copies byte-wise regardless,
        /// but this is the test that would catch a regression if a future
        /// change swapped that for an aligned load. Content is filled with
        /// the LCG already used elsewhere in this crate's tests, not a
        /// proptest `Vec` strategy, which would build an expensive
        /// shrinkable tree per byte at this size.
        #[test]
        fn matches_scalar_over_arbitrary_buffers_and_offsets(
            seed in any::<u32>(),
            len in 0usize..=1_048_576,
            offset in 0usize..8,
            initial in any::<u32>(),
        ) {
            // A plain early return (not `prop_assume!`): on a hypothetical
            // CRC-less aarch64 runner every case would otherwise be
            // rejected, and proptest fails a run that rejects too many
            // cases rather than treating it as "nothing to check here".
            if !is_available() {
                return Ok(());
            }
            let mut x = seed | 1;
            let backing: Vec<u8> = (0..len + offset)
                .map(|_| {
                    x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (x >> 24) as u8
                })
                .collect();
            let slice = &backing[offset..];
            let expected = update_state_scalar(initial, slice);
            let actual = unsafe { hw(initial, slice) };
            prop_assert_eq!(actual, expected);
        }
    }
}
