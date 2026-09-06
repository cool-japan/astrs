//! Generic byte-level fuzz generation: unstructured random bytes, and
//! structure-aware mutation of a valid seed (blueprint §15).
//!
//! Nothing here knows about frames, CDR or RTPS — every operator works on a
//! plain `&[u8]` / `Vec<u8>`, so each surface in [`crate::surfaces`] reuses
//! the same small operator set over its own seed corpus.
//!
//! Every function here is bounded by a caller-supplied `max_len`: the
//! byte-length cap the task requires, so a generated or mutated case can
//! never itself be the reason a fuzz iteration allocates more than a fixed,
//! small amount of memory.

use crate::support::rng::Rng;

/// One mutation this module applies. Kept as data (rather than inlined
/// closures) so [`mutate`] can pick uniformly from a fixed set.
#[derive(Debug, Clone, Copy)]
enum Op {
    FlipBit,
    SetByte,
    Truncate,
    DropRange,
    InsertRandom,
    DuplicateRange,
    AppendTail,
    ZeroRange,
}

/// Every operator [`mutate`] may apply.
const OPS: &[Op] = &[
    Op::FlipBit,
    Op::SetByte,
    Op::Truncate,
    Op::DropRange,
    Op::InsertRandom,
    Op::DuplicateRange,
    Op::AppendTail,
    Op::ZeroRange,
];

/// `len` unstructured pseudo-random bytes, `0 <= len <= max_len`.
///
/// This is the "arbitrary bytes" half of the generator the task describes;
/// [`mutate`] and [`splice`] are the "structure-aware mutation" half.
#[must_use]
pub fn random_bytes(rng: &mut Rng, max_len: usize) -> Vec<u8> {
    let len = rng.gen_range(0, max_len.saturating_add(1));
    rng.bytes(len)
}

/// Applies one to three random operators to a copy of `seed`, clamped to
/// `max_len`.
///
/// `seed` is expected to be a valid encoding produced by the target crate's
/// own encoder (see each surface's `seeds()`), so the result starts from a
/// structurally plausible input and perturbs it — the flips, truncations and
/// splices land on real field boundaries far more often than unstructured
/// random bytes would.
#[must_use]
pub fn mutate(rng: &mut Rng, seed: &[u8], max_len: usize) -> Vec<u8> {
    let mut buf = seed.to_vec();
    let rounds = 1 + rng.gen_range(0, 3);
    for _ in 0..rounds {
        let Some(op) = rng.pick(OPS).copied() else {
            break;
        };
        apply(rng, &mut buf, op, max_len);
        if buf.len() > max_len {
            buf.truncate(max_len);
        }
    }
    buf
}

/// The prefix of `a` up to a random cut, followed by the suffix of `b` from
/// a random cut — a crude but effective way to recombine two valid encodings
/// (e.g. two different frame kinds, two different submessage mixes) into
/// something that starts well-formed and goes wrong partway through.
#[must_use]
pub fn splice(rng: &mut Rng, a: &[u8], b: &[u8], max_len: usize) -> Vec<u8> {
    let cut_a = rng.gen_range(0, a.len().saturating_add(1));
    let cut_b = rng.gen_range(0, b.len().saturating_add(1));
    let mut out = Vec::with_capacity(max_len.min(cut_a + b.len().saturating_sub(cut_b)));
    out.extend_from_slice(&a[..cut_a]);
    out.extend_from_slice(&b[cut_b..]);
    out.truncate(max_len);
    out
}

fn apply(rng: &mut Rng, buf: &mut Vec<u8>, op: Op, max_len: usize) {
    match op {
        Op::FlipBit => {
            if let Some(index) = rng.index(buf.len()) {
                let bit = rng.gen_range(0, 8);
                if let Some(byte) = buf.get_mut(index) {
                    *byte ^= 1 << bit;
                }
            }
        }
        Op::SetByte => {
            if let Some(index) = rng.index(buf.len())
                && let Some(byte) = buf.get_mut(index)
            {
                *byte = rng.gen_byte();
            }
        }
        Op::Truncate => {
            if !buf.is_empty() {
                let cut = rng.gen_range(0, buf.len());
                buf.truncate(cut);
            }
        }
        Op::DropRange => {
            if let Some((start, end)) = range(rng, buf.len()) {
                buf.drain(start..end);
            }
        }
        Op::InsertRandom => insert_random(rng, buf, max_len),
        Op::DuplicateRange => duplicate_range(rng, buf, max_len),
        Op::AppendTail => append_tail(rng, buf, max_len),
        Op::ZeroRange => {
            if let Some((start, end)) = range(rng, buf.len())
                && let Some(slice) = buf.get_mut(start..end)
            {
                slice.fill(0);
            }
        }
    }
}

fn insert_random(rng: &mut Rng, buf: &mut Vec<u8>, max_len: usize) {
    let at = rng.gen_range(0, buf.len().saturating_add(1));
    let want = rng.gen_range(1, 17);
    let extra = rng.bytes(want);
    let room = max_len.saturating_sub(buf.len());
    let take = extra.len().min(room);
    if take > 0 {
        buf.splice(at..at, extra[..take].iter().copied());
    }
}

fn duplicate_range(rng: &mut Rng, buf: &mut Vec<u8>, max_len: usize) {
    let Some((start, end)) = range(rng, buf.len()) else {
        return;
    };
    let room = max_len.saturating_sub(buf.len());
    let Some(chunk) = buf.get(start..end) else {
        return;
    };
    let chunk = chunk.to_vec();
    let take = chunk.len().min(room);
    if take > 0 {
        let at = rng.gen_range(0, buf.len().saturating_add(1));
        buf.splice(at..at, chunk[..take].iter().copied());
    }
}

fn append_tail(rng: &mut Rng, buf: &mut Vec<u8>, max_len: usize) {
    let room = max_len.saturating_sub(buf.len());
    if room == 0 {
        return;
    }
    let want = rng.gen_range(1, room.min(64) + 1);
    let tail = rng.bytes(want);
    buf.extend_from_slice(&tail);
}

/// A pseudo-random, half-open `(start, end)` range within `0..len`, or
/// `None` when `len == 0`.
fn range(rng: &mut Rng, len: usize) -> Option<(usize, usize)> {
    if len == 0 {
        return None;
    }
    let a = rng.gen_range(0, len);
    let b = rng.gen_range(0, len);
    Some((a.min(b), (a.max(b) + 1).min(len)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn random_bytes_never_exceeds_the_cap() {
        let mut rng = Rng::new(1);
        for _ in 0..500 {
            assert!(random_bytes(&mut rng, 32).len() <= 32);
        }
    }

    #[test]
    fn mutate_never_exceeds_the_cap() {
        let mut rng = Rng::new(2);
        let seed = vec![0xAB; 40];
        for _ in 0..2000 {
            assert!(mutate(&mut rng, &seed, 64).len() <= 64);
        }
    }

    #[test]
    fn mutate_of_an_empty_seed_never_panics_and_stays_capped() {
        let mut rng = Rng::new(3);
        for _ in 0..500 {
            assert!(mutate(&mut rng, &[], 16).len() <= 16);
        }
    }

    #[test]
    fn splice_never_exceeds_the_cap() {
        let mut rng = Rng::new(4);
        let a = vec![1u8; 20];
        let b = vec![2u8; 30];
        for _ in 0..500 {
            assert!(splice(&mut rng, &a, &b, 16).len() <= 16);
        }
        assert!(splice(&mut rng, &[], &[], 16).is_empty());
    }

    #[test]
    fn every_operator_runs_without_panicking_on_tiny_buffers() {
        // Each operator is exercised directly (rather than only through the
        // random `mutate` dispatch) so a single rarely-picked op with an
        // off-by-one cannot hide behind the others' coverage.
        let mut rng = Rng::new(5);
        for &op in OPS {
            for seed in [&b""[..], &b"x"[..], &b"ab"[..], &[0u8; 9][..]] {
                let mut buf = seed.to_vec();
                apply(&mut rng, &mut buf, op, 32);
                assert!(buf.len() <= 32 + 16, "op {op:?} grew past any sane bound");
            }
        }
    }
}
