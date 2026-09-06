//! A tiny in-crate PRNG.
//!
//! `rand` is not a workspace dependency (blueprint §18.1's closed list), and
//! adding it for a test-only fuzz estate is not what this crate's task
//! authorizes, so this is a from-scratch xorshift64* generator (Marsaglia
//! 2003, multiplier from Vigna's `xorshift64star`): not cryptographic, not
//! even statistically rigorous (`next_u64`'s use in [`Rng::gen_range`] has
//! the usual small modulo bias), but exactly what generating and mutating
//! fuzz inputs needs.

/// A seeded xorshift64* generator.
///
/// Two generators constructed from the same seed produce the same stream —
/// this is what makes a default fuzz run reproducible (blueprint §15.1's
/// determinism principle, applied here to the harness rather than the
/// solver).
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// A generator seeded with `seed`.
    ///
    /// xorshift requires a nonzero state; a zero seed is remapped to a fixed
    /// nonzero constant so every `u64` is an accepted seed.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        let state = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state }
    }

    /// The next pseudo-random `u64`.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// The next pseudo-random `u32`, taken from the high bits (the higher
    /// bits of an xorshift stream are the better-distributed half).
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// A pseudo-random byte.
    pub fn gen_byte(&mut self) -> u8 {
        (self.next_u64() & 0xFF) as u8
    }

    /// A pseudo-random value in `[low, high)`.
    ///
    /// Returns `low` when the range is empty (`high <= low`), so callers
    /// never need to special-case a degenerate span themselves.
    pub fn gen_range(&mut self, low: usize, high: usize) -> usize {
        if high <= low {
            return low;
        }
        let span = (high - low) as u64;
        low + (self.next_u64() % span) as usize
    }

    /// True with probability `1 / one_in` (always false when `one_in == 0`).
    pub fn one_in(&mut self, one_in: u32) -> bool {
        one_in != 0 && self.gen_range(0, one_in as usize) == 0
    }

    /// A pseudo-random index into a slice of length `len`, or `None` when
    /// `len == 0`.
    pub fn index(&mut self, len: usize) -> Option<usize> {
        if len == 0 {
            None
        } else {
            Some(self.gen_range(0, len))
        }
    }

    /// A reference to a pseudo-randomly chosen element of `items`, or `None`
    /// when `items` is empty.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        self.index(items.len()).and_then(|index| items.get(index))
    }

    /// `len` pseudo-random bytes.
    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push(self.gen_byte());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_same_seed_reproduces_the_same_stream() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..64 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn a_zero_seed_does_not_degenerate() {
        let mut rng = Rng::new(0);
        // xorshift's fixed point is all-zero state; a zero seed must not
        // land there, or every draw would be zero.
        assert!((0..16).any(|_| rng.next_u64() != 0));
    }

    #[test]
    fn gen_range_stays_in_bounds() {
        let mut rng = Rng::new(7);
        for _ in 0..1000 {
            let value = rng.gen_range(3, 9);
            assert!((3..9).contains(&value));
        }
    }

    #[test]
    fn gen_range_on_an_empty_span_returns_low() {
        let mut rng = Rng::new(7);
        assert_eq!(rng.gen_range(5, 5), 5);
        assert_eq!(rng.gen_range(5, 2), 5);
    }

    #[test]
    fn pick_and_index_are_none_on_empty_input() {
        let mut rng = Rng::new(1);
        let empty: [u8; 0] = [];
        assert_eq!(rng.index(0), None);
        assert_eq!(rng.pick(&empty), None);
    }

    #[test]
    fn pick_stays_within_the_slice() {
        let mut rng = Rng::new(99);
        let items = [10, 20, 30, 40];
        for _ in 0..200 {
            let picked = rng.pick(&items);
            assert!(picked.is_some_and(|value| items.contains(value)));
        }
    }

    #[test]
    fn bytes_has_the_requested_length() {
        let mut rng = Rng::new(5);
        assert_eq!(rng.bytes(37).len(), 37);
        assert_eq!(rng.bytes(0).len(), 0);
    }

    #[test]
    fn one_in_zero_is_always_false() {
        let mut rng = Rng::new(3);
        assert!((0..100).all(|_| !rng.one_in(0)));
    }
}
