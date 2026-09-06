//! x86_64 hardware CRC-32C via SSE4.2's `crc32` instruction family.
//!
//! `_mm_crc32_u64`/`_mm_crc32_u8` compute exactly the Castagnoli CRC this
//! crate uses — SSE4.2 baked the CRC-32C polynomial into silicon because it
//! is the iSCSI/ext4/Btrfs checksum, not a coincidence this module can
//! borrow. [`is_available`] runtime-detects the feature once
//! ([`std::arch::is_x86_feature_detected`]) and every call site funnels
//! through the single `unsafe` boundary at [`update_state_hw`]; the two
//! kernels underneath it share that same feature guarantee via
//! `#[target_feature]` and so need no further `unsafe` blocks of their own
//! (calling a `#[target_feature]` function from another with a covering
//! feature set is safe by construction — "target feature 1.1",
//! stable since Rust 1.61).
//!
//! Above [`THREE_WAY_THRESHOLD`], the buffer is split into three roughly
//! equal, 8-byte-aligned chunks folded through three *independent* CRC
//! register chains in one interleaved loop, then stitched back together with
//! [`super::combine`]. The reason this helps at all: `crc32q` has a few
//! cycles of latency between issue and result, so one dependency chain
//! cannot issue back-to-back every cycle — three independent chains give the
//! core three chains' worth of independent work to fill that latency with,
//! which is exactly the classic "crc32c-by-3" technique (Intel's iSCSI
//! CRC32 instruction whitepaper; used by zlib-ng and others).
//!
//! # The threshold, and why it is not tuned on this machine
//!
//! [`THREE_WAY_THRESHOLD`] is **not** independently measured on x86_64 —
//! this crate has no x86_64 hardware to benchmark on, and this module's own
//! logic was only ever *executed* under Rosetta 2 translation on an aarch64
//! development machine, which proves correctness (translated `crc32q`
//! executes and produces the right answer) but not timing (translation
//! overhead is not representative of native silicon). What *is* known:
//! [`super::combine`] is shared, unmodified, between this module and
//! [`super::aarch64`], so the combine-side cost structure the aarch64
//! threshold was tuned against (see that module's doc comment for the
//! measured numbers and the bug that first threshold-tuning pass found)
//! applies here identically. This module's threshold is therefore set to
//! the same value as the measured aarch64 crossover, on the reasoning that
//! it is a defensible, conservative choice rather than a guess — this
//! module's own first draft used a from-the-literature threshold of 3072
//! bytes (an order of magnitude smaller), which is exactly the kind of
//! number this crate's own combine-cost bug (see `super::combine`'s module
//! doc comment) would have made a plausible-looking regression at 4 KiB
//! frames, had it shipped without the aarch64 data point in hand.

use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
use std::sync::OnceLock;

use super::combine::combine;
use super::scalar::split_chunk;

/// Buffer length above which the three-way interleaved kernel is used
/// instead of a single sequential CRC32C stream. See the module doc comment
/// for why this value is not independently measured on x86_64.
const THREE_WAY_THRESHOLD: usize = 32 * 1024;

/// Reports whether the SSE4.2 `crc32` instructions are usable on this CPU.
///
/// Checked once per process ([`OnceLock`]) — `is_x86_feature_detected!` is
/// itself already cached by `std`, but caching the `bool` here avoids
/// re-deriving it (and re-touching the detection machinery) on every call.
#[inline]
pub(crate) fn is_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| std::arch::is_x86_feature_detected!("sse4.2"))
}

/// Folds `data` into the running, non-finalised CRC register `state`, using
/// the SSE4.2 hardware CRC32C instructions.
///
/// # Safety
///
/// The caller must ensure [`is_available`] has returned `true` — this
/// executes the `crc32` instruction family directly, which is `SIGILL` on a
/// CPU without SSE4.2.
#[target_feature(enable = "sse4.2")]
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
#[target_feature(enable = "sse4.2")]
fn single_stream(mut state: u32, data: &[u8]) -> u32 {
    let mut rest = data;
    while let Some((chunk, tail)) = split_chunk(rest) {
        let word = u64::from_le_bytes(*chunk);
        state = _mm_crc32_u64(u64::from(state), word) as u32;
        rest = tail;
    }
    for &byte in rest {
        state = _mm_crc32_u8(state, byte);
    }
    state
}

/// Three-way interleaved CRC32C: three independent hardware register chains
/// over three roughly-equal thirds of `data`, combined back into one
/// register with [`combine`].
///
/// `state` seeds the *first* third's chain; the second and third thirds are
/// folded from a zero register each (an independent CRC computation, exactly
/// what an independent hardware pass produces when seeded with `0`) and
/// [`combine`]d in afterwards — see `crate::crc32c::combine`'s module docs
/// for why that is the correct composition, not an approximation of one.
#[target_feature(enable = "sse4.2")]
fn interleaved(state: u32, data: &[u8]) -> u32 {
    // Round down to a multiple of 8 so every lockstep chunk is a full
    // doubleword; the shortfall (at most 7 bytes per third, i.e. at most 21
    // bytes total) lands in `c`'s tail and is handled by `single_stream`
    // after the combine below.
    let third = (data.len() / 3) & !7;
    if third == 0 {
        // Defends the threshold above: with `THREE_WAY_THRESHOLD >= 24` this
        // cannot happen, but a smaller threshold must not divide by a
        // zero-length chunk.
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

    // The three chains touch disjoint memory and no chain's state feeds
    // another's within an iteration, so the compiler is free to interleave
    // their `crc32q` issuance — the whole point of splitting the buffer
    // this way.
    while let (Some((chunk_a, tail_a)), Some((chunk_b, tail_b)), Some((chunk_c, tail_c))) =
        (split_chunk(ra), split_chunk(rb), split_chunk(rc))
    {
        let word_a = u64::from_le_bytes(*chunk_a);
        let word_b = u64::from_le_bytes(*chunk_b);
        let word_c = u64::from_le_bytes(*chunk_c);
        state_a = _mm_crc32_u64(u64::from(state_a), word_a) as u32;
        state_b = _mm_crc32_u64(u64::from(state_b), word_b) as u32;
        state_c = _mm_crc32_u64(u64::from(state_c), word_c) as u32;
        ra = tail_a;
        rb = tail_b;
        rc = tail_c;
    }

    let combined = combine(state_a, state_b, third as u64);
    let combined = combine(combined, state_c, third as u64);
    single_stream(combined, c_tail)
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::crc32c::scalar::update_state_scalar;
    use proptest::prelude::*;

    /// Runs `body` only when SSE4.2 is actually present, so this suite
    /// stays green (rather than flaky) on a hypothetical x86_64 CI runner
    /// without it.
    fn with_hw(body: impl FnOnce()) {
        if is_available() {
            body();
        } else {
            eprintln!("skipping: SSE4.2 not detected on this CPU");
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
            // Exercises every residue `third` can land on mod 8 as the
            // buffer length varies, on both sides of the threshold.
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
            // SSE4.2-less x86_64 runner every case would otherwise be
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
