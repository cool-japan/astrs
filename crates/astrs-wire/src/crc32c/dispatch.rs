//! One-time runtime dispatch between the hardware CRC32C backends
//! ([`super::x86`], [`super::aarch64`]) and the portable scalar fallback
//! ([`super::scalar`]).
//!
//! # Cached bool, not a cached function pointer
//!
//! Two dispatch shapes were measured before choosing one: a
//! [`std::sync::OnceLock<bool>`] read every call, branching directly to
//! whichever backend function that resolves to, versus a
//! [`std::sync::OnceLock`] holding a resolved `fn(u32, &[u8]) -> u32`,
//! called indirectly every call. A release-profile (`opt-level = 3`,
//! `lto = true`, `codegen-units = 1`) microbenchmark of both, dispatching to
//! one of two `#[inline(never)]` backends on a 10-byte input (the size where
//! per-call dispatch overhead is largest relative to the work done — this
//! crate's own frame header is exactly 10 bytes, see `benches/crc32c.rs`),
//! measured the cached-bool branch consistently faster: ~6.2–6.5 ns/iter
//! against ~6.7–6.8 ns/iter for the function-pointer form, stable across
//! repeated rounds. This matches the general expectation for a two-way,
//! almost-always-same-outcome branch: a direct conditional branch is
//! trivial for the branch predictor after warmup, while an indirect call
//! through a function pointer cannot be inlined or spoken for by the same
//! prediction machinery. [`super::x86::is_available`] and
//! [`super::aarch64::is_available`] each cache their own detection result
//! the same way, for the same reason.

use super::scalar::update_state_scalar;

#[cfg(target_arch = "aarch64")]
use super::aarch64;
#[cfg(target_arch = "x86_64")]
use super::x86;

/// Folds `data` into the running, non-finalised CRC register `state`,
/// picking the fastest backend the current CPU supports: hardware CRC32C
/// (SSE4.2 on x86_64, the ARMv8 CRC extension on aarch64) when the runtime
/// feature check passes, the portable slice-by-8 table kernel otherwise.
///
/// This is the sole entry point [`super::Crc32c`]/[`super::crc32c`]/
/// [`super::crc32c_append`] use — the public API's behaviour (including its
/// `const`-ness where already `const`) is unchanged by which backend
/// actually runs; only the wall-clock cost differs.
#[inline]
pub(crate) fn update_state(state: u32, data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        if x86::is_available() {
            // SAFETY: `is_available()` just confirmed SSE4.2 is present,
            // which is exactly `update_state_hw`'s documented precondition.
            return unsafe { x86::update_state_hw(state, data) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if aarch64::is_available() {
            // SAFETY: `is_available()` just confirmed the ARMv8 CRC
            // extension is present, which is exactly `update_state_hw`'s
            // documented precondition.
            return unsafe { aarch64::update_state_hw(state, data) };
        }
    }
    update_state_scalar(state, data)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn dispatch_matches_scalar_on_the_published_check_value() {
        // Whichever backend this process's CPU selects, it must agree with
        // the portable reference — the per-backend test modules already
        // check this exhaustively when their hardware is present; this is
        // the end-to-end smoke test through the actual dispatch function
        // the public API calls.
        let expected = update_state_scalar(u32::MAX, b"123456789");
        assert_eq!(update_state(u32::MAX, b"123456789"), expected);
    }

    #[test]
    fn dispatch_matches_scalar_across_small_lengths() {
        let data: Vec<u8> = (0..64u32)
            .map(|i| (i.wrapping_mul(31) & 0xFF) as u8)
            .collect();
        for len in 0..=data.len() {
            let slice = &data[..len];
            assert_eq!(
                update_state(0x1234_5678, slice),
                update_state_scalar(0x1234_5678, slice),
                "len {len}"
            );
        }
    }
}
