//! Trace/span id generation and hex encoding.
//!
//! OTLP identifies a trace by a 16-byte id and a span by an 8-byte id,
//! both rendered as lowercase hex in JSON (blueprint §13: "traceId/spanId
//! hex"). Generating them only needs *uniqueness*, not
//! cryptographic unpredictability, so this module avoids adding a `rand`
//! dependency (not on the retained list, §18.1) and instead mixes
//! [`std::collections::hash_map::RandomState`] — which reseeds itself from
//! OS entropy per call, exactly the primitive `HashMap`'s own DoS
//! resistance relies on — with a per-process counter and the current
//! time.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-process tie-breaker mixed into every generated id, so two ids
/// generated on the same thread in the same nanosecond still differ.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A 64-bit value with enough entropy to make collisions practically
/// impossible for id generation, but with no claim of cryptographic
/// unpredictability.
fn random_u64() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hasher.write_u128(nanos);
    hasher.write_u32(std::process::id());
    hasher.finish()
}

/// Generates a new 128-bit trace id.
///
/// Never returns the all-zero id: the W3C Trace Context spec (and OTLP)
/// treat an all-zero trace id as invalid/absent, so a defensive rewrite
/// (setting the low bit) guarantees a usable id even in the
/// astronomically unlikely case two `random_u64` calls both land on
/// zero.
#[must_use]
pub fn new_trace_id() -> [u8; 16] {
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&random_u64().to_be_bytes());
    id[8..].copy_from_slice(&random_u64().to_be_bytes());
    if id == [0u8; 16] {
        id[15] = 1;
    }
    id
}

/// Generates a new 64-bit span id. See [`new_trace_id`] for why the
/// all-zero value is excluded.
#[must_use]
pub fn new_span_id() -> [u8; 8] {
    let mut id = random_u64().to_be_bytes();
    if id == [0u8; 8] {
        id[7] = 1;
    }
    id
}

/// Renders `bytes` as lowercase hex, two characters per byte.
///
/// Crate-private: [`crate::propagation::SpanContext`] is the public,
/// documented-with-examples surface that uses this; see its
/// `trace_id_hex`/`span_id_hex` methods.
#[must_use]
pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Parses exactly `2 * N` lowercase-or-uppercase hex characters into a
/// `[u8; N]`. Returns `None` on the wrong length or a non-hex character,
/// rather than panicking — this is the entry point for parsing a
/// `traceparent` header a peer sent us, which this process must never
/// trust to be well-formed.
#[must_use]
pub(crate) fn decode_hex_exact<const N: usize>(s: &str) -> Option<[u8; N]> {
    let bytes = s.as_bytes();
    if bytes.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for i in 0..N {
        let hi = hex_nibble(bytes[i * 2])?;
        let lo = hex_nibble(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

/// The 4-bit value of one ASCII hex digit, or `None` if it is not one.
const fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn generated_trace_and_span_ids_are_not_all_zero() {
        for _ in 0..64 {
            assert_ne!(new_trace_id(), [0u8; 16]);
            assert_ne!(new_span_id(), [0u8; 8]);
        }
    }

    #[test]
    fn generated_ids_are_practically_unique() {
        let mut trace_ids = std::collections::BTreeSet::new();
        let mut span_ids = std::collections::BTreeSet::new();
        for _ in 0..1_000 {
            assert!(trace_ids.insert(new_trace_id()));
            assert!(span_ids.insert(new_span_id()));
        }
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0x00, 0x0f, 0xab, 0xff];
        let hex = encode_hex(&bytes);
        assert_eq!(hex, "000fabff");
        assert_eq!(decode_hex_exact::<4>(&hex), Some(bytes));
    }

    #[test]
    fn hex_decode_accepts_uppercase() {
        assert_eq!(decode_hex_exact::<2>("AB12"), Some([0xab, 0x12]));
    }

    #[test]
    fn hex_decode_rejects_wrong_length() {
        assert_eq!(decode_hex_exact::<4>("00"), None);
        assert_eq!(decode_hex_exact::<4>("00000000000"), None);
    }

    #[test]
    fn hex_decode_rejects_non_hex_characters() {
        assert_eq!(decode_hex_exact::<2>("zz12"), None);
        assert_eq!(decode_hex_exact::<2>("12 3"), None);
    }

    #[test]
    fn encode_of_empty_is_empty() {
        assert_eq!(encode_hex(&[]), "");
        assert_eq!(decode_hex_exact::<0>(""), Some([]));
    }
}
