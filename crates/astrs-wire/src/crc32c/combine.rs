//! CRC combine — folding two independently-computed CRC registers into the
//! register a single sequential pass would have produced, without
//! re-reading either stream. This is what makes 3-way interleaving
//! ([`super::x86`], [`super::aarch64`]) possible: three chunks of one buffer
//! are hashed as three independent streams (for instruction-level
//! parallelism across three CRC dependency chains, each of which otherwise
//! serialises on the hardware instruction's own latency), then stitched
//! back into one register with this module.
//!
//! # The maths, in the vocabulary this crate already uses
//!
//! [`super::scalar::update_state_scalar`]'s byte step,
//! `state' = (state >> 8) ^ TABLES[0][(state ^ byte) & 0xFF]`, is GF(2)-affine
//! in `state`: expanding it shows `state' = M(state) ^ f(byte)` for a fixed
//! 32×32 GF(2)-linear map `M` (independent of `byte`) and a fixed map `f`
//! (independent of `state`). Folding `n` bytes is `M` applied `n` times
//! composed with the `f(byte)` contributions, and since `f(0) = 0` (`f` is
//! linear), folding `n` *zero* bytes reduces to exactly `M^n`, the classic
//! CRC "shift by N zero bytes" operator (`gf2_matrix`-style, as in zlib's
//! `crc32_combine` — same operator, computed at the byte granularity this
//! crate folds at rather than zlib's bit granularity, so no separate
//! bit-level polynomial constant is needed).
//!
//! For two streams `A` (processed from register `head`) and `B` (processed
//! independently from a zero register, giving `tail_from_zero`), this module
//! computes the register `update_state_scalar(head, A)` would reach after
//! also folding `B`:
//!
//! ```text
//! combine(head, tail_from_zero, len(B)) == update_state_scalar(head, A ++ B)
//! ```
//!
//! `M`'s 32 basis images are read straight off [`super::scalar`] itself
//! (`M(e_i) = update_state_scalar(1 << i, &[0u8])`) rather than re-derived
//! from the polynomial's bit representation — bootstrapping off code that is
//! already tested against the bitwise reference and the RFC 3720 vectors
//! removes an entire class of "got the reflected/normal polynomial direction
//! backwards" bugs that a from-scratch bit-level derivation would risk.
//!
//! # Why a cached doubling chain, not squaring per call
//!
//! A first version of [`shift_zero_bytes`] computed `M^n` by binary
//! exponentiation *from scratch on every call*: `O(log n)` iterations, each
//! squaring the running power (`square`, a 32×32 GF(2) matrix composed with
//! itself — 32 calls to [`apply`], ~1024 elementary XOR-if-bit-set steps in
//! total). Benchmarking the three-way interleaved kernels
//! (`benches/crc32c.rs`) against single-stream hardware CRC exposed this as
//! a real cost, not a rounding error: at a few KiB, `interleaved`'s two
//! `combine` calls (four to six thousand elementary steps between them,
//! `third` typically 10–12 bits wide) took *longer than hashing the buffer*,
//! turning a technique meant to speed things up into a double-digit
//! regression. Squaring is exactly the part of binary exponentiation that
//! does not depend on the input register — `M^(2^k)` is the same operator
//! every time `shift_zero_bytes` is asked to shift by a length whose bit
//! pattern includes bit `k` — so [`doubling_chain`] computes the whole
//! `M^1, M^2, M^4, …, M^(2^63)` chain once, lazily, and caches it: the
//! one-time cost is ~63 squarings ever, and every call after that is
//! `O(popcount(n))` calls to [`apply`] alone (~32 elementary steps each),
//! not `O(log n)` squarings. That fix alone took the interleaved kernels
//! from losing to single-stream everywhere tested to winning by 2.5–3× at
//! ≥64 KiB (aarch64, measured — see `aarch64`'s module doc comment).

use std::sync::OnceLock;

use super::scalar::update_state_scalar;

/// A GF(2)-linear operator on the 32-bit CRC register, represented as the
/// images of its 32 basis vectors: `matrix[i]` is the operator applied to
/// the register with only bit `i` set.
type Matrix = [u32; 32];

/// Length of the doubling chain — one entry per bit of a `u64` shift length.
const CHAIN_LEN: usize = u64::BITS as usize;

/// The "fold one zero byte" operator `M`.
///
/// `M(e_i) = update_state_scalar(1 << i, &[0])` for each bit `i` — see the
/// module doc comment for why reading this off the scalar kernel is exact,
/// not an approximation.
fn one_zero_byte_operator() -> Matrix {
    let mut matrix = [0u32; 32];
    for (bit, image) in matrix.iter_mut().enumerate() {
        *image = update_state_scalar(1u32 << bit, &[0u8]);
    }
    matrix
}

/// Applies the GF(2)-linear operator `matrix` to `vector`.
///
/// This is ordinary matrix-vector multiplication over GF(2): XOR together
/// the basis images for every set bit of `vector`.
#[inline]
fn apply(matrix: &Matrix, vector: u32) -> u32 {
    let mut sum = 0u32;
    let mut remaining = vector;
    let mut bit = 0usize;
    while remaining != 0 {
        if remaining & 1 != 0 {
            sum ^= matrix[bit];
        }
        remaining >>= 1;
        bit += 1;
    }
    sum
}

/// Composes `matrix` with itself, i.e. computes the operator for applying
/// `matrix` twice.
#[inline]
fn square(matrix: &Matrix) -> Matrix {
    let mut result = [0u32; 32];
    for (bit, image) in result.iter_mut().enumerate() {
        *image = apply(matrix, matrix[bit]);
    }
    result
}

/// The doubling chain `M^(2^0), M^(2^1), …, M^(2^63)`, computed once (lazily)
/// and cached for the lifetime of the process.
///
/// See the module doc comment: this is what turns [`shift_zero_bytes`] from
/// an `O(log n)`-squarings-per-call operation into an
/// `O(popcount(n))`-applications-per-call one.
fn doubling_chain() -> &'static [Matrix; CHAIN_LEN] {
    static CHAIN: OnceLock<[Matrix; CHAIN_LEN]> = OnceLock::new();
    CHAIN.get_or_init(|| {
        let mut chain = [[0u32; 32]; CHAIN_LEN];
        chain[0] = one_zero_byte_operator();
        for previous in 1..CHAIN_LEN {
            chain[previous] = square(&chain[previous - 1]);
        }
        chain
    })
}

/// Computes the register that folding `n` zero bytes into `reg` would reach,
/// without touching any bytes — `M^n` applied to `reg`, read off the cached
/// [`doubling_chain`] one set bit of `n` at a time.
///
/// This is the operator [`combine`] uses to advance the head register past
/// the tail chunk's length before folding in the tail's own contribution.
pub(crate) fn shift_zero_bytes(reg: u32, mut n: u64) -> u32 {
    let chain = doubling_chain();
    let mut result = reg;
    let mut bit = 0usize;
    while n != 0 {
        if n & 1 != 0 {
            result = apply(&chain[bit], result);
        }
        n >>= 1;
        bit += 1;
    }
    result
}

/// Combines a `head` register (the state after folding some prefix `A`)
/// with the from-zero register of the `tail_len`-byte block `B` that
/// immediately follows it, producing the register
/// `update_state_scalar(head, A)` would reach after also folding `B` — i.e.
/// `update_state_scalar(head, [A, B].concat())`.
///
/// `tail_from_zero` must be `update_state_scalar(0, B)`: `B`'s own register
/// computed independently, starting from a zero register rather than from
/// `head` or any finalisation value. This is exactly what an independent
/// hardware CRC pass over `B` produces when seeded with `0`.
#[inline]
pub(crate) fn combine(head: u32, tail_from_zero: u32, tail_len: u64) -> u32 {
    shift_zero_bytes(head, tail_len) ^ tail_from_zero
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use proptest::prelude::*;

    /// A tiny xorshift generator for the u32 register values these tests
    /// exercise `shift_zero_bytes`/`combine` with — the retained-crate list
    /// (blueprint §18.1) has no RNG in it, and every use here just needs a
    /// non-degenerate `u32`/`u8`, not statistical quality.
    fn next_u32(state: &mut u64) -> u32 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (*state >> 32) as u32
    }

    #[test]
    fn shift_by_zero_is_the_identity() {
        assert_eq!(shift_zero_bytes(0x1234_5678, 0), 0x1234_5678);
        assert_eq!(shift_zero_bytes(0, 0), 0);
        assert_eq!(shift_zero_bytes(u32::MAX, 0), u32::MAX);
    }

    #[test]
    fn shift_matches_folding_explicit_zero_bytes_exhaustively_for_small_n() {
        let mut seed = 0xC0FF_EEEEu64;
        for _ in 0..8 {
            let reg = next_u32(&mut seed);
            for n in 0u64..=64 {
                let zeros = vec![0u8; n as usize];
                let expected = update_state_scalar(reg, &zeros);
                assert_eq!(shift_zero_bytes(reg, n), expected, "reg {reg:#010x} n {n}");
            }
        }
    }

    #[test]
    fn shift_matches_folding_explicit_zero_bytes_at_power_of_two_boundaries() {
        // Binary exponentiation's own edge cases: exactly on, one below, and
        // one above every power-of-two bit position up to 2^20 bytes (1 MiB
        // of zero bytes is still a sub-millisecond `vec!` allocation).
        let mut seed = 0xFEED_FACEu64;
        for shift in 0u32..=20 {
            let base = 1u64 << shift;
            for n in [base.saturating_sub(1), base, base + 1] {
                let reg = next_u32(&mut seed);
                let zeros = vec![0u8; n as usize];
                let expected = update_state_scalar(reg, &zeros);
                assert_eq!(shift_zero_bytes(reg, n), expected, "reg {reg:#010x} n {n}");
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Random `(reg, n)` pairs up to 1 MiB of implied zero bytes — the
        /// reference builds one plain `vec![0u8; n]` per case (a memset, not
        /// a proptest-shrinkable strategy), so this stays fast even at the
        /// top of the range.
        #[test]
        fn shift_matches_folding_explicit_zero_bytes_randomly(
            reg in any::<u32>(),
            n in 0u64..=1_048_576,
        ) {
            let zeros = vec![0u8; n as usize];
            let expected = update_state_scalar(reg, &zeros);
            prop_assert_eq!(shift_zero_bytes(reg, n), expected);
        }

        /// `combine` must agree with sequentially folding the concatenation,
        /// for arbitrary prefix/suffix byte content (not just zero bytes) —
        /// this is the property the 3-way interleave kernels actually rely
        /// on.
        #[test]
        fn combine_matches_sequential_folding(
            initial in any::<u32>(),
            prefix_seed in any::<u64>(),
            prefix_len in 0usize..=2048,
            suffix_seed in any::<u64>(),
            suffix_len in 0usize..=2048,
        ) {
            let mut ps = prefix_seed | 1;
            let prefix: Vec<u8> = (0..prefix_len).map(|_| next_u32(&mut ps) as u8).collect();
            let mut ss = suffix_seed | 1;
            let suffix: Vec<u8> = (0..suffix_len).map(|_| next_u32(&mut ss) as u8).collect();

            let head = update_state_scalar(initial, &prefix);
            let tail_from_zero = update_state_scalar(0, &suffix);
            let combined = combine(head, tail_from_zero, suffix.len() as u64);

            let mut concatenated = prefix.clone();
            concatenated.extend_from_slice(&suffix);
            let sequential = update_state_scalar(initial, &concatenated);

            prop_assert_eq!(combined, sequential);
        }
    }
}
