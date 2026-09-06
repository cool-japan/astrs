//! CRC-32C (Castagnoli) — the frame integrity check of blueprint §7.1.
//!
//! `oxicrypto` exposes hashes, AEADs, MACs, signatures, KEX, KDFs and a
//! CSPRNG, but no CRC; `oxicode`'s optional `checksum` feature pulls in
//! `crc32fast`, which is CRC-32/ISO-HDLC and not the Castagnoli polynomial
//! the frame format specifies. So AstRS owns this one (blueprint §3.2:
//! "if a capability is core to the product, AstRS owns the implementation").
//!
//! # Backends (blueprint §20.1 W6 SIMD policy)
//!
//! Every entry point below ([`crc32c`], [`crc32c_append`], [`Crc32c`])
//! folds bytes through this module's private `dispatch::update_state`,
//! which picks the fastest kernel the running CPU actually supports,
//! checked once per process and cached. (The backend modules named below
//! are deliberately private — implementation detail, not public API — so
//! they are named here as plain text rather than as doc links.)
//!
//! | Module | Backend | Runtime gate |
//! |---|---|---|
//! | `x86` | SSE4.2 `crc32` instructions, 3-way interleaved above a size threshold | `is_x86_feature_detected!("sse4.2")` |
//! | `aarch64` | ARMv8 CRC32C instructions, 3-way interleaved above a size threshold | `is_aarch64_feature_detected!("crc")` |
//! | `scalar` | Portable slice-by-8 tables (the universal fallback) | always available |
//!
//! Dispatch changes *which code runs*, never *what it computes*: every
//! hardware kernel is checked against `scalar::update_state_scalar` by
//! its own exhaustive/property test suite, and `combine` is what makes
//! the 3-way interleaved kernels' split-then-stitch strategy exact rather
//! than approximate. The public API in this module — including which
//! functions are `const fn` — is unaffected by any of this; only the
//! wall-clock cost changes.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::crc32c::{Crc32c, crc32c};
//!
//! // The canonical check value.
//! assert_eq!(crc32c(b"123456789"), 0xE306_9283);
//!
//! // Streaming gives the same answer as the one-shot form.
//! let mut hasher = Crc32c::new();
//! hasher.update(b"12345");
//! hasher.update(b"6789");
//! assert_eq!(hasher.finalize(), crc32c(b"123456789"));
//! ```

mod combine;
mod dispatch;
mod scalar;

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "x86_64")]
mod x86;

use dispatch::update_state;

/// The reflected CRC-32C (Castagnoli) generator polynomial.
///
/// The normal-form polynomial is `0x1EDC_6F41`; reflected for the
/// least-significant-bit-first algorithm used here it becomes `0x82F6_3B78`.
pub const CRC32C_POLYNOMIAL: u32 = 0x82F6_3B78;

/// The CRC-32C check value for the ASCII string `"123456789"`.
///
/// This is the standard "check" constant every CRC catalogue quotes for
/// CRC-32/ISCSI, and the cheapest possible self-test of a build.
pub const CRC32C_CHECK: u32 = 0xE306_9283;

/// Computes the CRC-32C of `data`.
///
/// # Examples
///
/// ```
/// use astrs_wire::crc32c::crc32c;
///
/// assert_eq!(crc32c(b""), 0);
/// assert_eq!(crc32c(b"123456789"), 0xE306_9283);
/// ```
#[must_use]
#[inline]
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c_append(0, data)
}

/// Continues a CRC-32C over `data`, starting from the finalised checksum
/// `previous`.
///
/// This is the primitive that makes a frame's checksum computable without
/// concatenating the header and the payload into one buffer:
/// `crc32c_append(crc32c(header), payload)`.
///
/// # Examples
///
/// ```
/// use astrs_wire::crc32c::{crc32c, crc32c_append};
///
/// let split = crc32c_append(crc32c(b"1234"), b"56789");
/// assert_eq!(split, crc32c(b"123456789"));
/// ```
#[must_use]
#[inline]
pub fn crc32c_append(previous: u32, data: &[u8]) -> u32 {
    !update_state(!previous, data)
}

/// An incremental CRC-32C hasher.
///
/// Use this when the bytes to checksum are produced piecewise — a framed
/// writer, for example, checksums the header it just formatted and then the
/// payload it is about to copy, without materialising the concatenation.
///
/// # Examples
///
/// ```
/// use astrs_wire::crc32c::{Crc32c, crc32c};
///
/// let mut hasher = Crc32c::new();
/// for chunk in [b"1234".as_slice(), b"5678".as_slice(), b"9".as_slice()] {
///     hasher.update(chunk);
/// }
/// assert_eq!(hasher.finalize(), crc32c(b"123456789"));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Crc32c {
    /// The non-finalised CRC register (i.e. the bitwise complement of the
    /// checksum published so far).
    state: u32,
}

impl Crc32c {
    /// Creates a hasher in its initial state.
    #[must_use]
    #[inline]
    pub const fn new() -> Self {
        Self { state: u32::MAX }
    }

    /// Resumes hashing from an already-finalised checksum.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::crc32c::{Crc32c, crc32c};
    ///
    /// let head = crc32c(b"1234");
    /// let mut hasher = Crc32c::resume(head);
    /// hasher.update(b"56789");
    /// assert_eq!(hasher.finalize(), crc32c(b"123456789"));
    /// ```
    #[must_use]
    #[inline]
    pub const fn resume(checksum: u32) -> Self {
        Self { state: !checksum }
    }

    /// Folds `data` into the hasher.
    #[inline]
    pub fn update(&mut self, data: &[u8]) {
        self.state = update_state(self.state, data);
    }

    /// Folds `data` in and returns the hasher, for chaining.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::crc32c::{Crc32c, crc32c};
    ///
    /// let sum = Crc32c::new().chain(b"1234").chain(b"56789").finalize();
    /// assert_eq!(sum, crc32c(b"123456789"));
    /// ```
    #[must_use]
    #[inline]
    pub fn chain(mut self, data: &[u8]) -> Self {
        self.update(data);
        self
    }

    /// Returns the checksum accumulated so far.
    ///
    /// The hasher is `Copy`, so this can be called on an intermediate state
    /// without ending the stream.
    #[must_use]
    #[inline]
    pub const fn finalize(&self) -> u32 {
        !self.state
    }
}

impl Default for Crc32c {
    fn default() -> Self {
        Self::new()
    }
}

impl core::hash::Hasher for Crc32c {
    /// Returns the CRC-32C widened to 64 bits.
    ///
    /// CRC-32C is *not* a good general-purpose hash for hash maps — it is
    /// provided here only so streaming adapters that expect a
    /// [`core::hash::Hasher`] can drive the frame checksum.
    fn finish(&self) -> u64 {
        u64::from(self.finalize())
    }

    fn write(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A deliberately naive, bit-at-a-time CRC-32C written straight from the
    /// polynomial definition. The dispatched kernel (whichever backend the
    /// running CPU selects) is validated against this, so a table- or
    /// intrinsic-level bug cannot hide behind a matching bug in the test's
    /// expectations.
    fn reference_crc32c(data: &[u8]) -> u32 {
        let mut crc: u32 = u32::MAX;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ CRC32C_POLYNOMIAL
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn matches_the_published_check_value() {
        assert_eq!(crc32c(b"123456789"), CRC32C_CHECK);
    }

    #[test]
    fn empty_input_is_zero() {
        assert_eq!(crc32c(b""), 0);
        assert_eq!(Crc32c::new().finalize(), 0);
    }

    #[test]
    fn matches_rfc3720_appendix_vectors() {
        // iSCSI (RFC 3720) publishes these four 32-byte CRC-32C vectors.
        assert_eq!(crc32c(&[0x00u8; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);

        let ascending: Vec<u8> = (0u8..32).collect();
        assert_eq!(crc32c(&ascending), 0x46DD_794E);

        let descending: Vec<u8> = (0u8..32).rev().collect();
        assert_eq!(crc32c(&descending), 0x113F_DB5C);
    }

    #[test]
    fn matches_the_bitwise_reference_on_every_length_up_to_two_blocks() {
        // Covers the slice-by-8 fast path, the byte-wise tail, and every
        // possible split between them.
        let data: Vec<u8> = (0..64u32)
            .map(|i| (i.wrapping_mul(31) & 0xFF) as u8)
            .collect();
        for len in 0..=data.len() {
            let slice = &data[..len];
            assert_eq!(
                crc32c(slice),
                reference_crc32c(slice),
                "mismatch at length {len}"
            );
        }
    }

    #[test]
    fn matches_the_bitwise_reference_on_every_length_up_to_256() {
        // The exhaustive small-length sweep the W6 SIMD hardening pass
        // added: every length from 0 to 256 bytes, covering the dispatched
        // kernel's tail handling one byte at a time past the slice-by-8/
        // hardware-doubleword fast path on every backend this process
        // actually runs.
        let data: Vec<u8> = (0..256u32)
            .map(|i| (i.wrapping_mul(197).wrapping_add(11) & 0xFF) as u8)
            .collect();
        for len in 0..=data.len() {
            let slice = &data[..len];
            assert_eq!(
                crc32c(slice),
                reference_crc32c(slice),
                "mismatch at length {len}"
            );
        }
    }

    #[test]
    fn matches_the_bitwise_reference_on_a_large_buffer() {
        let mut data = Vec::with_capacity(4096);
        let mut x: u32 = 0x1234_5678;
        for _ in 0..4096 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            data.push((x >> 24) as u8);
        }
        assert_eq!(crc32c(&data), reference_crc32c(&data));
    }

    #[test]
    fn single_byte_values_match_the_reference() {
        for byte in 0u8..=255 {
            assert_eq!(crc32c(&[byte]), reference_crc32c(&[byte]), "byte {byte}");
        }
    }

    #[test]
    fn append_equals_one_shot_at_every_split() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let whole = crc32c(data);
        for split in 0..=data.len() {
            let (head, tail) = data.split_at(split);
            assert_eq!(crc32c_append(crc32c(head), tail), whole, "split {split}");
        }
    }

    #[test]
    fn streaming_equals_one_shot_at_every_split() {
        let data = b"AS\x01\x03\x00\x00\x2a\x00\x00\x00payload-bytes";
        let whole = crc32c(data);
        for split in 0..=data.len() {
            let (head, tail) = data.split_at(split);
            let mut hasher = Crc32c::new();
            hasher.update(head);
            hasher.update(tail);
            assert_eq!(hasher.finalize(), whole, "split {split}");
        }
    }

    #[test]
    fn resume_round_trips_a_finalised_checksum() {
        let head = crc32c(b"header");
        let mut hasher = Crc32c::resume(head);
        hasher.update(b"payload");
        assert_eq!(hasher.finalize(), crc32c(b"headerpayload"));
    }

    #[test]
    fn chain_is_equivalent_to_update() {
        let chained = Crc32c::new().chain(b"ab").chain(b"cd").finalize();
        let mut updated = Crc32c::new();
        updated.update(b"ab");
        updated.update(b"cd");
        assert_eq!(chained, updated.finalize());
        assert_eq!(chained, crc32c(b"abcd"));
    }

    #[test]
    fn a_single_flipped_bit_always_changes_the_checksum() {
        let base = b"astrs frame payload under integrity check".to_vec();
        let expected = crc32c(&base);
        for index in 0..base.len() {
            for bit in 0..8 {
                let mut corrupted = base.clone();
                corrupted[index] ^= 1 << bit;
                assert_ne!(
                    crc32c(&corrupted),
                    expected,
                    "flipping bit {bit} of byte {index} went unnoticed"
                );
            }
        }
    }

    #[test]
    fn hasher_trait_reports_the_widened_checksum() {
        use core::hash::Hasher as _;

        let mut hasher = Crc32c::new();
        hasher.write(b"123456789");
        assert_eq!(hasher.finish(), u64::from(CRC32C_CHECK));
    }

    #[test]
    fn default_matches_new() {
        assert_eq!(Crc32c::default(), Crc32c::new());
    }

    #[test]
    fn large_buffers_match_the_reference_across_a_range_of_odd_alignments() {
        // Proves the dispatched kernel's chunking never depends on the
        // slice's base address being 8-byte aligned: the same backing
        // buffer, sliced at every offset 0..8, must give the same answer a
        // fresh, offset-0 computation of the same bytes would.
        let mut backing = Vec::with_capacity(1 << 16);
        let mut x: u32 = 0x9E37_79B9;
        for _ in 0..(1 << 16) {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            backing.push((x >> 24) as u8);
        }
        for offset in 0..8 {
            let slice = &backing[offset..];
            assert_eq!(crc32c(slice), reference_crc32c(slice), "offset {offset}");
        }
    }
}
