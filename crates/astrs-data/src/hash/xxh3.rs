//! XXH3-64, implemented in-crate against the upstream algorithm.
//!
//! `oxicrypto` 0.3 (the workspace's retained crypto dependency, blueprint
//! §19.1) exposes BLAKE2/BLAKE3/SHA-2/SHA-3 in `oxicrypto-hash` but no
//! xxHash variant — checked directly against its published source before
//! writing a line of this file — so [`crate::SchemaHash`] needs its own
//! XXH3. This module is that implementation: a direct, function-for-function
//! port of the reference algorithm's **scalar** code path (every SIMD
//! variant in the reference is a bit-identical optimisation of the same
//! math, so the scalar path alone is spec-complete), validated against rows
//! taken verbatim from the upstream project's own sanity-check table
//! (`tests/sanity_test_vectors.h` in `Cyan4973/xxHash`) — see the tests
//! below.
//!
//! # Layout
//!
//! The one-shot functions ([`xxh3_64`], [`xxh3_64_with_seed`]) dispatch on
//! length into six branches, each named after its counterpart in the
//! reference:
//!
//! | Length | Function | Shape |
//! |---|---:|---|
//! | 0 | inline in `xxh3_64_0to16` | one read of the secret, seeded |
//! | 1..=3 | `xxh3_64_1to3` | pack 3 bytes + length into one `avalanche` |
//! | 4..=8 | `xxh3_64_4to8` | two 32-bit reads, `strong_avalanche` |
//! | 9..=16 | `xxh3_64_9to16` | two 64-bit reads, `avalanche` |
//! | 17..=128 | `xxh3_64_17to128` | 2, 4, 6 or 8 `mix16` calls by size |
//! | 129..=240 | `xxh3_64_129to240` | 8 fixed `mix16` rounds + a tail |
//! | 241.. | `hash_long` | the accumulator: `accumulate_512`/`scramble_acc` over 64-byte stripes, `merge_accs` at the end |
//!
//! [`Xxh3Hasher`] is the streaming face. It does not reproduce the
//! reference's O(1)-auxiliary-memory stripe bookkeeping (the highest
//! bug-density part of the algorithm, and the part this port has no
//! independent oracle for); instead it buffers every byte `update` is
//! called with and runs [`xxh3_64_with_seed`] over the concatenation at
//! `digest`. XXH3's streaming contract *is* "produces the digest of the
//! concatenated input" — the reference's own sanity check cross-validates
//! its incremental path against the one-shot one for exactly that reason —
//! so this is a legitimate, spec-accurate implementation of that contract,
//! not an approximation of it; it trades the reference's bounded working
//! set for an implementation an order of magnitude smaller and easier to
//! audit, which is the right trade for [`crate::SchemaHash`]'s actual input
//! (a few kilobytes of canonical schema bytes, not a multi-gigabyte stream).

use std::hash::Hasher;

/// `XXH_PRIME32_1`.
const PRIME32_1: u64 = 0x9E37_79B1;
/// `XXH_PRIME32_2`.
const PRIME32_2: u64 = 0x85EB_CA77;
/// `XXH_PRIME32_3`.
const PRIME32_3: u64 = 0xC2B2_AE3D;
/// `XXH_PRIME64_1`.
const PRIME64_1: u64 = 0x9E37_79B1_85EB_CA87;
/// `XXH_PRIME64_2`.
const PRIME64_2: u64 = 0xC2B2_AE3D_27D4_EB4F;
/// `XXH_PRIME64_3`.
const PRIME64_3: u64 = 0x1656_67B1_9E37_79F9;

/// The multiplier XXH3's own `avalanche` uses. **Not** [`PRIME64_3`] — the
/// two differ in one hex digit (`91` vs `B1`) and mixing them up silently
/// produces a different, wrong hash for every input longer than 8 bytes.
const XXH3_AVALANCHE_MUL: u64 = 0x1656_6791_9E37_79F9;
/// The multiplier XXH3's `strong_avalanche` uses, applied twice.
const XXH3_STRONG_AVALANCHE_MUL: u64 = 0x9FB2_1C65_1E98_DF25;

/// Bytes in one accumulator stripe.
const STRIPE_LEN: usize = 64;
/// `u64` lanes per stripe / per accumulator.
const ACC_LANES: usize = STRIPE_LEN / 8;
/// Bytes the per-stripe secret window advances by.
const SECRET_CONSUME_RATE: usize = 8;
/// Byte offset into the secret where `merge_accs` starts reading.
const SECRET_MERGEACCS_START: usize = 11;
/// Bytes back from the secret's end where the final stripe's window starts.
const SECRET_LASTACC_START: usize = 7;
/// Inputs at or below this length use the mid-size path; above it, `hash_long`.
const MID_SIZE_MAX: usize = 240;
/// The smallest secret this algorithm accepts (only relevant to a custom
/// secret; the built-in [`DEFAULT_SECRET`] is always [`DEFAULT_SECRET_SIZE`]).
const SECRET_SIZE_MIN: usize = 136;
/// Length of [`DEFAULT_SECRET`].
const DEFAULT_SECRET_SIZE: usize = 192;

/// The reference implementation's default secret, verbatim.
///
/// A fixed, unremarkable-looking byte string with no meaning beyond being
/// good high-entropy filler for the mixing steps below — every AstRS build
/// uses the exact bytes the upstream project ships, so a hash computed here
/// matches one computed by the C library, `xxhash-rust`, or any other
/// faithful port, seed for seed.
#[rustfmt::skip]
const DEFAULT_SECRET: [u8; DEFAULT_SECRET_SIZE] = [
    0xb8, 0xfe, 0x6c, 0x39, 0x23, 0xa4, 0x4b, 0xbe, 0x7c, 0x01, 0x81, 0x2c, 0xf7, 0x21, 0xad, 0x1c,
    0xde, 0xd4, 0x6d, 0xe9, 0x83, 0x90, 0x97, 0xdb, 0x72, 0x40, 0xa4, 0xa4, 0xb7, 0xb3, 0x67, 0x1f,
    0xcb, 0x79, 0xe6, 0x4e, 0xcc, 0xc0, 0xe5, 0x78, 0x82, 0x5a, 0xd0, 0x7d, 0xcc, 0xff, 0x72, 0x21,
    0xb8, 0x08, 0x46, 0x74, 0xf7, 0x43, 0x24, 0x8e, 0xe0, 0x35, 0x90, 0xe6, 0x81, 0x3a, 0x26, 0x4c,
    0x3c, 0x28, 0x52, 0xbb, 0x91, 0xc3, 0x00, 0xcb, 0x88, 0xd0, 0x65, 0x8b, 0x1b, 0x53, 0x2e, 0xa3,
    0x71, 0x64, 0x48, 0x97, 0xa2, 0x0d, 0xf9, 0x4e, 0x38, 0x19, 0xef, 0x46, 0xa9, 0xde, 0xac, 0xd8,
    0xa8, 0xfa, 0x76, 0x3f, 0xe3, 0x9c, 0x34, 0x3f, 0xf9, 0xdc, 0xbb, 0xc7, 0xc7, 0x0b, 0x4f, 0x1d,
    0x8a, 0x51, 0xe0, 0x4b, 0xcd, 0xb4, 0x59, 0x31, 0xc8, 0x9f, 0x7e, 0xc9, 0xd9, 0x78, 0x73, 0x64,
    0xea, 0xc5, 0xac, 0x83, 0x34, 0xd3, 0xeb, 0xc3, 0xc5, 0x81, 0xa0, 0xff, 0xfa, 0x13, 0x63, 0xeb,
    0x17, 0x0d, 0xdd, 0x51, 0xb7, 0xf0, 0xda, 0x49, 0xd3, 0x16, 0x55, 0x26, 0x29, 0xd4, 0x68, 0x9e,
    0x2b, 0x16, 0xbe, 0x58, 0x7d, 0x47, 0xa1, 0xfc, 0x8f, 0xf8, 0xb8, 0xd1, 0x7a, 0xd0, 0x31, 0xce,
    0x45, 0xcb, 0x3a, 0x8f, 0x95, 0x16, 0x04, 0x28, 0xaf, 0xd7, 0xfb, 0xca, 0xbb, 0x4b, 0x40, 0x7e,
];

/// The eight-lane initial accumulator state, from the reference's
/// `XXH3_INIT_ACC`.
const INITIAL_ACC: [u64; ACC_LANES] = [
    PRIME32_3, PRIME64_1, PRIME64_2, PRIME64_3, PRIME64_4, PRIME32_2, PRIME64_5, PRIME32_1,
];
/// `XXH_PRIME64_4`.
const PRIME64_4: u64 = 0x85EB_CA77_C2B2_AE63;
/// `XXH_PRIME64_5`.
const PRIME64_5: u64 = 0x27D4_EB2F_1656_67C5;

/// Reads a little-endian `u32` at `offset`, treating a window that runs past
/// the end of `data` as zero-padded rather than panicking.
///
/// Every call site in this module has already sized `data` so the read is in
/// range; the zero-pad fallback exists so a future refactor that gets an
/// offset wrong fails as a wrong *hash*, not a panic — matching this crate's
/// crate-wide "never panic on data-dependent input" policy even inside an
/// algorithm this fiddly.
#[inline]
fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    let mut buf = [0u8; 4];
    if let Some(chunk) = data.get(offset..offset + 4) {
        buf.copy_from_slice(chunk);
    }
    u32::from_le_bytes(buf)
}

/// Reads a little-endian `u64` at `offset`. See [`read_u32_le`] for the
/// zero-pad convention.
#[inline]
fn read_u64_le(data: &[u8], offset: usize) -> u64 {
    let mut buf = [0u8; 8];
    if let Some(chunk) = data.get(offset..offset + 8) {
        buf.copy_from_slice(chunk);
    }
    u64::from_le_bytes(buf)
}

#[inline]
const fn xorshift64(value: u64, shift: u32) -> u64 {
    value ^ (value >> shift)
}

/// XXH3's own avalanche — used by every branch except the empty-input case
/// and `xxh3_64_1to3`, which use [`xxh64_avalanche`] instead.
#[inline]
fn avalanche(value: u64) -> u64 {
    let value = xorshift64(value, 37).wrapping_mul(XXH3_AVALANCHE_MUL);
    xorshift64(value, 32)
}

/// XXH64's avalanche (not XXH3's — a different multiplier chain), used only
/// where the reference reuses it: the empty input and the 1..=3 byte case.
#[inline]
fn xxh64_avalanche(value: u64) -> u64 {
    let value = xorshift64(value, 33).wrapping_mul(PRIME64_2);
    let value = xorshift64(value, 29).wrapping_mul(PRIME64_3);
    xorshift64(value, 32)
}

/// The heavier avalanche `xxh3_64_4to8` uses.
#[inline]
fn strong_avalanche(value: u64, len: u64) -> u64 {
    let value = value ^ (value.rotate_left(49) ^ value.rotate_left(24));
    let value = value.wrapping_mul(XXH3_STRONG_AVALANCHE_MUL);
    let value = value ^ ((value >> 35).wrapping_add(len));
    let value = value.wrapping_mul(XXH3_STRONG_AVALANCHE_MUL);
    xorshift64(value, 28)
}

/// `(low 64, high 64)` of the full 128-bit product `left * right`.
#[inline]
fn mul64_to128(left: u64, right: u64) -> (u64, u64) {
    let product = u128::from(left) * u128::from(right);
    (product as u64, (product >> 64) as u64)
}

/// The 128-bit product of `left` and `right`, folded into 64 bits by XOR.
#[inline]
fn mul128_fold64(left: u64, right: u64) -> u64 {
    let (low, high) = mul64_to128(left, right);
    low ^ high
}

/// One 16-byte mixing step: reads 16 bytes of `input` at `input_off` and 16
/// bytes of `secret` at `secret_off`, folds them with `seed`.
///
/// The shared primitive behind every branch from 17 bytes up.
#[inline]
fn mix16(input: &[u8], input_off: usize, secret: &[u8], secret_off: usize, seed: u64) -> u64 {
    let input_lo =
        read_u64_le(input, input_off) ^ read_u64_le(secret, secret_off).wrapping_add(seed);
    let input_hi =
        read_u64_le(input, input_off + 8) ^ read_u64_le(secret, secret_off + 8).wrapping_sub(seed);
    mul128_fold64(input_lo, input_hi)
}

/// Derives a seeded secret from [`DEFAULT_SECRET`], for the long path when
/// `seed != 0` (the short and mid-size paths always use [`DEFAULT_SECRET`]
/// unmodified — only the accumulator path needs a full 192-byte secret keyed
/// by the seed).
fn custom_secret(seed: u64) -> [u8; DEFAULT_SECRET_SIZE] {
    let mut secret = [0u8; DEFAULT_SECRET_SIZE];
    for round in 0..(DEFAULT_SECRET_SIZE / 16) {
        let lo = read_u64_le(&DEFAULT_SECRET, round * 16).wrapping_add(seed);
        let hi = read_u64_le(&DEFAULT_SECRET, round * 16 + 8).wrapping_sub(seed);
        secret[round * 16..round * 16 + 8].copy_from_slice(&lo.to_le_bytes());
        secret[round * 16 + 8..round * 16 + 16].copy_from_slice(&hi.to_le_bytes());
    }
    secret
}

/// One 64-byte stripe folded into the eight-lane accumulator.
///
/// `input[input_off..input_off + 64]` is the stripe; `secret[secret_off
/// .. secret_off + 64]` is this stripe's key window.
#[inline]
fn accumulate_512(
    acc: &mut [u64; ACC_LANES],
    input: &[u8],
    input_off: usize,
    secret: &[u8],
    secret_off: usize,
) {
    for lane in 0..ACC_LANES {
        let value = read_u64_le(input, input_off + lane * 8);
        let key = read_u64_le(secret, secret_off + lane * 8);
        let keyed = value ^ key;
        acc[lane ^ 1] = acc[lane ^ 1].wrapping_add(value);
        acc[lane] = acc[lane].wrapping_add((keyed & 0xFFFF_FFFF).wrapping_mul(keyed >> 32));
    }
}

/// Re-mixes the accumulator between blocks, using the secret's final 64
/// bytes.
#[inline]
fn scramble_acc(acc: &mut [u64; ACC_LANES], secret: &[u8], secret_off: usize) {
    for (lane, slot) in acc.iter_mut().enumerate() {
        let key = read_u64_le(secret, secret_off + lane * 8);
        let value = xorshift64(*slot, 47) ^ key;
        *slot = value.wrapping_mul(PRIME32_1);
    }
}

/// Folds `nb_stripes` consecutive 64-byte stripes starting at `input_off`
/// into the accumulator, sliding the secret window by
/// [`SECRET_CONSUME_RATE`] bytes per stripe.
#[inline]
fn accumulate_loop(
    acc: &mut [u64; ACC_LANES],
    input: &[u8],
    input_off: usize,
    secret: &[u8],
    secret_off: usize,
    nb_stripes: usize,
) {
    for stripe in 0..nb_stripes {
        accumulate_512(
            acc,
            input,
            input_off + stripe * STRIPE_LEN,
            secret,
            secret_off + stripe * SECRET_CONSUME_RATE,
        );
    }
}

/// The accumulator sweep over the whole (>240-byte) input: full blocks with a
/// scramble between them, the partial trailing block, then one more overlap
/// with the final stripe.
fn hash_long_internal_loop(acc: &mut [u64; ACC_LANES], input: &[u8], secret: &[u8]) {
    let nb_stripes_per_block = (secret.len() - STRIPE_LEN) / SECRET_CONSUME_RATE;
    let block_len = STRIPE_LEN * nb_stripes_per_block;
    let nb_blocks = (input.len() - 1) / block_len;

    for block in 0..nb_blocks {
        accumulate_loop(
            acc,
            input,
            block * block_len,
            secret,
            0,
            nb_stripes_per_block,
        );
        scramble_acc(acc, secret, secret.len() - STRIPE_LEN);
    }

    let last_block_off = nb_blocks * block_len;
    let remaining_stripes = ((input.len() - 1) - last_block_off) / STRIPE_LEN;
    accumulate_loop(acc, input, last_block_off, secret, 0, remaining_stripes);

    // The final stripe is always the input's last 64 bytes, keyed by a
    // dedicated window near (not at) the secret's end.
    accumulate_512(
        acc,
        input,
        input.len() - STRIPE_LEN,
        secret,
        secret.len() - STRIPE_LEN - SECRET_LASTACC_START,
    );
}

/// Folds two adjacent accumulator lanes with one 16-byte secret window into
/// a single `u64`, the primitive [`merge_accs`] calls four times.
#[inline]
fn mix_two_accs(acc: &[u64; ACC_LANES], lane: usize, secret: &[u8], secret_off: usize) -> u64 {
    mul128_fold64(
        acc[lane] ^ read_u64_le(secret, secret_off),
        acc[lane + 1] ^ read_u64_le(secret, secret_off + 8),
    )
}

/// Collapses the eight-lane accumulator into the final 64-bit digest.
fn merge_accs(acc: &[u64; ACC_LANES], secret: &[u8], secret_off: usize, seed: u64) -> u64 {
    let mut result = seed;
    result = result.wrapping_add(mix_two_accs(acc, 0, secret, secret_off));
    result = result.wrapping_add(mix_two_accs(acc, 2, secret, secret_off + 16));
    result = result.wrapping_add(mix_two_accs(acc, 4, secret, secret_off + 32));
    result = result.wrapping_add(mix_two_accs(acc, 6, secret, secret_off + 48));
    avalanche(result)
}

/// The `> 240`-byte path: sweep the accumulator over the whole input, then
/// merge it down to one `u64`.
fn hash_long(input: &[u8], secret: &[u8]) -> u64 {
    let mut acc = INITIAL_ACC;
    hash_long_internal_loop(&mut acc, input, secret);
    merge_accs(
        &acc,
        secret,
        SECRET_MERGEACCS_START,
        (input.len() as u64).wrapping_mul(PRIME64_1),
    )
}

/// 1..=3 bytes: pack the first, middle and last byte plus the length into
/// one 32-bit word, avalanche with XXH64's mix.
#[inline]
fn xxh3_64_1to3(input: &[u8], seed: u64, secret: &[u8]) -> u64 {
    let len = input.len();
    let c1 = u32::from(input[0]);
    let c2 = u32::from(input[len >> 1]);
    let c3 = u32::from(input[len - 1]);
    let combined = (c1 << 16) | (c2 << 24) | c3 | ((len as u32) << 8);
    let flip = u64::from(read_u32_le(secret, 0) ^ read_u32_le(secret, 4)).wrapping_add(seed);
    xxh64_avalanche(u64::from(combined) ^ flip)
}

/// 4..=8 bytes: two 32-bit reads folded through `strong_avalanche`.
#[inline]
fn xxh3_64_4to8(input: &[u8], seed: u64, secret: &[u8]) -> u64 {
    let len = input.len();
    let seed = seed ^ (u64::from((seed as u32).swap_bytes()) << 32);
    let input_lo = read_u32_le(input, 0);
    let input_hi = read_u32_le(input, len - 4);
    let flip = (read_u64_le(secret, 8) ^ read_u64_le(secret, 16)).wrapping_sub(seed);
    let combined = u64::from(input_hi).wrapping_add(u64::from(input_lo) << 32);
    strong_avalanche(combined ^ flip, len as u64)
}

/// 9..=16 bytes: two 64-bit reads, mixed and avalanched.
#[inline]
fn xxh3_64_9to16(input: &[u8], seed: u64, secret: &[u8]) -> u64 {
    let len = input.len();
    let flip1 = (read_u64_le(secret, 24) ^ read_u64_le(secret, 32)).wrapping_add(seed);
    let flip2 = (read_u64_le(secret, 40) ^ read_u64_le(secret, 48)).wrapping_sub(seed);
    let input_lo = read_u64_le(input, 0) ^ flip1;
    let input_hi = read_u64_le(input, len - 8) ^ flip2;
    let acc = (len as u64)
        .wrapping_add(input_lo.swap_bytes())
        .wrapping_add(input_hi)
        .wrapping_add(mul128_fold64(input_lo, input_hi));
    avalanche(acc)
}

/// 0..=16 bytes: dispatches to the three sub-ranges above, or the direct
/// one-read formula for an empty input.
fn xxh3_64_0to16(input: &[u8], seed: u64, secret: &[u8]) -> u64 {
    let len = input.len();
    if len > 8 {
        xxh3_64_9to16(input, seed, secret)
    } else if len >= 4 {
        xxh3_64_4to8(input, seed, secret)
    } else if len > 0 {
        xxh3_64_1to3(input, seed, secret)
    } else {
        xxh64_avalanche(seed ^ (read_u64_le(secret, 56) ^ read_u64_le(secret, 64)))
    }
}

/// 17..=128 bytes: 2, 4, 6 or 8 `mix16` calls, nested by size threshold
/// (>32, >64, >96), all folded into one running sum before the avalanche.
fn xxh3_64_17to128(input: &[u8], seed: u64, secret: &[u8]) -> u64 {
    let len = input.len();
    let mut acc = (len as u64).wrapping_mul(PRIME64_1);

    if len > 32 {
        if len > 64 {
            if len > 96 {
                acc = acc.wrapping_add(mix16(input, 48, secret, 96, seed));
                acc = acc.wrapping_add(mix16(input, len - 64, secret, 112, seed));
            }
            acc = acc.wrapping_add(mix16(input, 32, secret, 64, seed));
            acc = acc.wrapping_add(mix16(input, len - 48, secret, 80, seed));
        }
        acc = acc.wrapping_add(mix16(input, 16, secret, 32, seed));
        acc = acc.wrapping_add(mix16(input, len - 32, secret, 48, seed));
    }
    acc = acc.wrapping_add(mix16(input, 0, secret, 0, seed));
    acc = acc.wrapping_add(mix16(input, len - 16, secret, 16, seed));
    avalanche(acc)
}

/// 129..=240 bytes: eight fixed `mix16` rounds (avalanched partway through),
/// then one round per remaining 16-byte group, then a final 16-byte tail
/// read from the input's end against a dedicated secret window.
fn xxh3_64_129to240(input: &[u8], seed: u64, secret: &[u8]) -> u64 {
    const SECOND_HALF_SECRET_OFFSET: usize = 3;
    const TAIL_SECRET_BACK_OFFSET: usize = 17;

    let len = input.len();
    let mut acc = (len as u64).wrapping_mul(PRIME64_1);
    let nb_rounds = len / 16;

    for round in 0..8 {
        acc = acc.wrapping_add(mix16(input, 16 * round, secret, 16 * round, seed));
    }
    acc = avalanche(acc);

    for round in 8..nb_rounds {
        acc = acc.wrapping_add(mix16(
            input,
            16 * round,
            secret,
            16 * (round - 8) + SECOND_HALF_SECRET_OFFSET,
            seed,
        ));
    }

    acc = acc.wrapping_add(mix16(
        input,
        len - 16,
        secret,
        SECRET_SIZE_MIN - TAIL_SECRET_BACK_OFFSET,
        seed,
    ));
    avalanche(acc)
}

/// The 64-bit XXH3 digest of `input`, unseeded.
///
/// Identical to `xxh3_64_with_seed(input, 0)` — kept as a separate entry
/// point because that is the pair the reference implementation and its own
/// sanity check expose, and because most callers (including
/// [`crate::SchemaHash`]) never need a seed at all.
///
/// ```
/// use astrs_data::hash::xxh3::{xxh3_64, xxh3_64_with_seed};
///
/// assert_eq!(xxh3_64(b""), 0x2D06_8005_38D3_94C2);
/// assert_eq!(xxh3_64(b"a"), xxh3_64_with_seed(b"a", 0));
/// ```
#[must_use]
pub fn xxh3_64(input: &[u8]) -> u64 {
    xxh3_64_with_seed(input, 0)
}

/// The 64-bit XXH3 digest of `input`, seeded.
///
/// `seed = 0` is defined to equal [`xxh3_64`] — the reference implementation
/// asserts exactly this in its own sanity check, and this port's tests do
/// the same.
///
/// ```
/// use astrs_data::hash::xxh3::{xxh3_64, xxh3_64_with_seed};
///
/// assert_eq!(xxh3_64_with_seed(b"hello", 0), xxh3_64(b"hello"));
/// assert_ne!(xxh3_64_with_seed(b"hello", 1), xxh3_64(b"hello"));
/// ```
#[must_use]
pub fn xxh3_64_with_seed(input: &[u8], seed: u64) -> u64 {
    let len = input.len();
    if len <= 16 {
        xxh3_64_0to16(input, seed, &DEFAULT_SECRET)
    } else if len <= 128 {
        xxh3_64_17to128(input, seed, &DEFAULT_SECRET)
    } else if len <= MID_SIZE_MAX {
        xxh3_64_129to240(input, seed, &DEFAULT_SECRET)
    } else if seed == 0 {
        hash_long(input, &DEFAULT_SECRET)
    } else {
        hash_long(input, &custom_secret(seed))
    }
}

/// A streaming XXH3-64 computation.
///
/// Buffers every byte handed to [`Xxh3Hasher::update`] (or the
/// [`std::hash::Hasher::write`] it is built on) and runs [`xxh3_64_with_seed`]
/// over the concatenation when asked to finish — see the [module
/// documentation](self) for why that is a spec-accurate implementation of
/// XXH3's streaming contract, not a shortcut around it.
///
/// ```
/// use astrs_data::hash::xxh3::{xxh3_64, Xxh3Hasher};
///
/// let mut hasher = Xxh3Hasher::new();
/// hasher.update(b"hello, ");
/// hasher.update(b"world");
/// assert_eq!(hasher.digest(), xxh3_64(b"hello, world"));
/// ```
#[derive(Debug, Clone, Default)]
pub struct Xxh3Hasher {
    /// The seed passed to every digest.
    seed: u64,
    /// Every byte seen since construction or the last [`Xxh3Hasher::reset`].
    buffer: Vec<u8>,
}

impl Xxh3Hasher {
    /// An unseeded hasher.
    #[must_use]
    pub fn new() -> Self {
        Self::with_seed(0)
    }

    /// A hasher that digests as `xxh3_64_with_seed(_, seed)`.
    #[must_use]
    pub const fn with_seed(seed: u64) -> Self {
        Self {
            seed,
            buffer: Vec::new(),
        }
    }

    /// Feeds more bytes into the computation.
    #[inline]
    pub fn update(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The digest of every byte seen so far, without consuming the hasher —
    /// `update` may continue afterwards.
    #[must_use]
    pub fn digest(&self) -> u64 {
        xxh3_64_with_seed(&self.buffer, self.seed)
    }

    /// Bytes buffered since construction or the last [`Xxh3Hasher::reset`].
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Returns `true` when nothing has been fed in yet.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Drops every buffered byte, keeping the seed.
    pub fn reset(&mut self) {
        self.buffer.clear();
    }
}

impl Hasher for Xxh3Hasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.digest()
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Ports `fillTestBuffer` from the upstream project's
    /// `tests/sanity_test.c`, byte for byte: it is the pseudorandom buffer
    /// the official vectors below were computed over, so any deviation here
    /// would silently invalidate every one of them.
    fn sanity_buffer(len: usize) -> Vec<u8> {
        const PRIME32: u64 = 2_654_435_761;
        const PRIME64: u64 = 11_400_714_785_074_694_797;
        let mut byte_gen = PRIME32;
        let mut buffer = Vec::with_capacity(len);
        for _ in 0..len {
            buffer.push((byte_gen >> 56) as u8);
            byte_gen = byte_gen.wrapping_mul(PRIME64);
        }
        buffer
    }

    /// The second seed the official table uses (`PRIME64` above, reused as a
    /// seed — the upstream table's own choice, not a coincidence this port
    /// introduces).
    const SEED2: u64 = 0x9E37_79B1_85EB_CA8D;

    /// Rows taken verbatim from `XSUM_XXH3_testdata` in
    /// `Cyan4973/xxHash`'s `tests/sanity_test_vectors.h` (fetched from the
    /// `dev` branch), spanning every branch boundary this module has: the
    /// empty input, each of `1to3`/`4to8`/`9to16`, the 32/64/96/128
    /// thresholds inside `17to128`, `129to240`, and `hash_long` both within
    /// one block (≤ ~1024 bytes with the default secret) and across several.
    const OFFICIAL_VECTORS: &[(usize, u64, u64)] = &[
        (0, 0, 0x2D06_8005_38D3_94C2),
        (0, SEED2, 0xA8A6_B918_B2F0_364A),
        (1, 0, 0xC44B_DFF4_074E_ECDB),
        (1, SEED2, 0x032B_E332_DD76_6EF8),
        (2, 0, 0x7A99_7804_4CB8_A8BB),
        (2, SEED2, 0x764B_35C9_0519_AD88),
        (3, 0, 0x5424_7382_A8D6_B94D),
        (3, SEED2, 0x634B_8990_B497_6373),
        (4, 0, 0xE5DC_74BC_5184_8A51),
        (4, SEED2, 0xAA2E_7ECC_B0C8_F747),
        (5, 0, 0xE424_3F00_7203_06BB),
        (5, SEED2, 0x5A67_C87E_50ED_80ED),
        (7, 0, 0x9941_E000_7F55_5E50),
        (7, SEED2, 0x75BD_AB43_463F_0151),
        (8, 0, 0x24CC_C9AC_AA9F_65E4),
        (8, SEED2, 0x8F97_3410_999B_8F6B),
        (9, 0, 0x14D5_001C_15DD_3F2B),
        (9, SEED2, 0xB3AE_7333_D901_3F60),
        (12, 0, 0xA713_DAF0_DFBB_77E7),
        (12, SEED2, 0xE730_3E1B_2336_DE0E),
        (15, 0, 0x4555_6D4D_6E17_98BC),
        (15, SEED2, 0x710D_D531_8F6F_16D5),
        (16, 0, 0x981B_17D3_6C74_98C9),
        (16, SEED2, 0x663F_2933_3B4D_B6B1),
        (17, 0, 0x796F_5ACD_3A60_F862),
        (17, SEED2, 0xF3EC_5067_F430_6DB3),
        (20, 0, 0x4BC1_8275_FB46_F223),
        (20, SEED2, 0x9331_17EF_BE73_C5FA),
        (32, 0, 0x9FEA_DDBD_BF57_EED3),
        (32, SEED2, 0x2199_FAB1_5348_93D9),
        (33, 0, 0xABFB_2D08_1B40_0A10),
        (33, SEED2, 0xAD56_348D_A574_BB6D),
        (48, 0, 0x397D_A259_ECBA_1F11),
        (48, SEED2, 0xADC2_CBAA_44AC_C616),
        (64, 0, 0x9CB4_8487_720E_C49D),
        (64, SEED2, 0x4FE8_895D_B9B8_C077),
        (65, 0, 0xFD81_AAC4_BEBC_3883),
        (65, SEED2, 0xAD80_AEEC_1FC9_E0A7),
        (96, 0, 0x935A_769A_7F94_776F),
        (96, SEED2, 0x70CF_5193_7E50_0540),
        (97, 0, 0xCA4C_A268_FD3C_3A6C),
        (97, SEED2, 0xEE46_1D3A_DD7E_E6C9),
        (111, 0, 0x678D_BD7C_D8EF_8F5C),
        (111, SEED2, 0x0452_9DFA_D46B_27BF),
        (128, 0, 0xFCFF_2412_6754_D861),
        (128, SEED2, 0x73FD_E752_8064_6649),
        (129, 0, 0x98F1_B0A6_79A2_CA29),
        (129, SEED2, 0x21FF_FDBC_A099_C844),
        (140, 0, 0xCA0D_611F_4C6D_0492),
        (140, SEED2, 0x0EE6_E507_ED38_899C),
        (192, 0, 0xAF9F_58E7_8B8D_3587),
        (192, SEED2, 0x69E0_06AA_2156_C999),
        (235, 0, 0x75F3_FF40_7516_816B),
        (235, SEED2, 0xCAAC_D909_A07B_2CDE),
        (240, 0, 0x81C3_C2B6_7F56_8CCF),
        (240, SEED2, 0xCC0F_58C2_7EF3_D8EE),
        (241, 0, 0xC5A6_39EC_D203_0E5E),
        (241, SEED2, 0xDDA9_B0A1_61D4_829A),
        (242, 0, 0x9673_44E7_8CB4_B723),
        (242, SEED2, 0x5811_A132_DBCB_4C75),
        (300, 0, 0x7F37_1220_466C_4A1A),
        (300, SEED2, 0x1C88_6C66_4C4D_81C4),
        (512, 0, 0x617E_4959_9013_CB6B),
        (512, SEED2, 0x3CE4_57DE_14C2_7708),
        (1000, 0, 0xACA2_DDE0_F195_1B9A),
        (1000, SEED2, 0xF4DD_9ADF_00FF_B410),
        (1024, 0, 0xDD85_C9B5_C110_9C5C),
        (1024, SEED2, 0xEF36_8A8A_2EBA_BAEF),
        (2048, 0, 0xDD59_E2C3_A5F0_38E0),
        (2048, SEED2, 0x66F8_1670_669A_BABC),
        (4096, 0, 0xE912_0642_9D1F_48F9),
        (4096, SEED2, 0x2A3B_BB20_A543_9DCD),
        (4160, 0, 0x4F32_3B15_321E_94E1),
        (4160, SEED2, 0x1BF6_F5FA_F9EE_CABD),
    ];

    #[test]
    fn official_vectors_match_one_shot() {
        // Every row indexes the *same* buffer's prefix — the reference's own
        // sanity check reuses one 4161-byte buffer across every length
        // rather than regenerating it per row, so this must too.
        let buffer = sanity_buffer(4161);
        for &(len, seed, expected) in OFFICIAL_VECTORS {
            let actual = xxh3_64_with_seed(&buffer[..len], seed);
            assert_eq!(actual, expected, "len={len} seed={seed:#x}");
        }
    }

    #[test]
    fn unseeded_matches_seed_zero() {
        let buffer = sanity_buffer(600);
        for len in [0, 1, 8, 16, 17, 128, 129, 240, 241, 600] {
            assert_eq!(
                xxh3_64(&buffer[..len]),
                xxh3_64_with_seed(&buffer[..len], 0)
            );
        }
    }

    #[test]
    fn empty_input_is_stable() {
        assert_eq!(xxh3_64(b""), 0x2D06_8005_38D3_94C2);
        assert_eq!(xxh3_64(&[]), xxh3_64(b""));
    }

    #[test]
    fn streaming_matches_one_shot_in_a_single_call() {
        let buffer = sanity_buffer(600);
        for &(len, seed, expected) in OFFICIAL_VECTORS {
            let mut hasher = Xxh3Hasher::with_seed(seed);
            hasher.update(&buffer[..len.min(buffer.len())]);
            if len > buffer.len() {
                continue;
            }
            assert_eq!(hasher.digest(), expected, "len={len} seed={seed:#x}");
        }
    }

    #[test]
    fn streaming_matches_one_shot_across_arbitrary_chunk_boundaries() {
        // Mirrors the upstream sanity check's "random ingestion" mode: the
        // same bytes fed through many small `update` calls must digest
        // identically to one `update` call, whatever the chunk sizes are.
        let buffer = sanity_buffer(4096 + 37);
        let chunk_sizes = [1usize, 3, 7, 8, 16, 63, 64, 65, 127, 128, 129, 1024, 4096];
        for &chunk in &chunk_sizes {
            let mut hasher = Xxh3Hasher::new();
            for window in buffer.chunks(chunk) {
                hasher.update(window);
            }
            assert_eq!(hasher.digest(), xxh3_64(&buffer), "chunk size {chunk}");
        }
    }

    #[test]
    fn streaming_matches_one_shot_byte_by_byte() {
        let buffer = sanity_buffer(300);
        let mut hasher = Xxh3Hasher::with_seed(SEED2);
        for byte in &buffer {
            hasher.update(std::slice::from_ref(byte));
        }
        assert_eq!(hasher.digest(), xxh3_64_with_seed(&buffer, SEED2));
    }

    #[test]
    fn reset_clears_the_buffer_but_keeps_the_seed() {
        let mut hasher = Xxh3Hasher::with_seed(7);
        hasher.update(b"anything");
        assert!(!hasher.is_empty());
        hasher.reset();
        assert!(hasher.is_empty());
        assert_eq!(hasher.digest(), xxh3_64_with_seed(b"", 7));
    }

    #[test]
    fn std_hasher_trait_matches_the_inherent_api() {
        let mut hasher = Xxh3Hasher::new();
        Hasher::write(&mut hasher, b"hello");
        assert_eq!(Hasher::finish(&hasher), xxh3_64(b"hello"));
    }

    #[test]
    fn default_is_the_unseeded_hasher() {
        assert_eq!(Xxh3Hasher::default().digest(), xxh3_64(b""));
    }

    #[test]
    fn distinct_seeds_produce_distinct_digests_for_the_same_bytes() {
        let bytes = sanity_buffer(50);
        assert_ne!(
            xxh3_64_with_seed(&bytes, 1),
            xxh3_64_with_seed(&bytes, 2),
            "collision between two small seeds is astronomically unlikely"
        );
    }

    #[test]
    fn sanity_buffer_matches_the_upstream_generator_by_construction() {
        // A directed check on the generator itself: the first bytes of
        // PRIME32's top byte and the first multiply step, computed by hand.
        let buffer = sanity_buffer(2);
        assert_eq!(buffer[0], (2_654_435_761u64 >> 56) as u8);
        let next = 2_654_435_761u64.wrapping_mul(11_400_714_785_074_694_797);
        assert_eq!(buffer[1], (next >> 56) as u8);
    }
}
