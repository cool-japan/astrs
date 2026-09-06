//! The portable **slice-by-8** table kernel — the universal fallback and the
//! correctness reference every hardware backend in this module is checked
//! against.
//!
//! Eight 256-entry tables are derived from the reflected polynomial
//! [`super::CRC32C_POLYNOMIAL`] at compile time, and the hot loop consumes
//! eight bytes per iteration with eight table lookups and eight XORs. There
//! is no `unsafe` and no runtime table initialisation.
//!
//! [`update_state_scalar`] and [`split_chunk`] are also load-bearing for the
//! hardware backends ([`super::x86`], [`super::aarch64`]): the former is how
//! [`super::combine`] derives the GF(2) "zero bytes" operator without
//! re-deriving polynomial bit constants by hand, and the latter's
//! fixed-size-array chunking is exactly what a 64-bit hardware CRC
//! instruction wants for its operand.

use super::CRC32C_POLYNOMIAL;

/// Number of lookup tables used by the slice-by-8 kernel.
pub(crate) const SLICE: usize = 8;

/// Compile-time construction of the eight slice-by-8 lookup tables.
const fn build_tables() -> [[u32; 256]; SLICE] {
    let mut tables = [[0u32; 256]; SLICE];

    // Table 0 is the ordinary byte-wise CRC table.
    let mut index = 0usize;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ CRC32C_POLYNOMIAL
            } else {
                crc >> 1
            };
            bit += 1;
        }
        tables[0][index] = crc;
        index += 1;
    }

    // Table `k` extends table `k - 1` by one further byte position.
    let mut level = 1usize;
    while level < SLICE {
        let mut index = 0usize;
        while index < 256 {
            let previous = tables[level - 1][index];
            tables[level][index] = (previous >> 8) ^ tables[0][(previous & 0xFF) as usize];
            index += 1;
        }
        level += 1;
    }

    tables
}

/// The eight slice-by-8 tables, materialised at compile time.
pub(crate) static TABLES: [[u32; 256]; SLICE] = build_tables();

/// Folds `data` into the running, *non-finalised* CRC register `state`,
/// using only the portable slice-by-8 table method.
///
/// This is the fallback every platform has, and the reference every
/// hardware kernel in this module is tested against — never optimise this
/// function to depend on the answer a hardware path would give.
#[inline]
pub(crate) fn update_state_scalar(mut state: u32, data: &[u8]) -> u32 {
    let mut rest = data;

    // Eight bytes per iteration.
    while let Some((chunk, tail)) = split_chunk(rest) {
        let low = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) ^ state;
        let high = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        state = TABLES[7][(low & 0xFF) as usize]
            ^ TABLES[6][((low >> 8) & 0xFF) as usize]
            ^ TABLES[5][((low >> 16) & 0xFF) as usize]
            ^ TABLES[4][((low >> 24) & 0xFF) as usize]
            ^ TABLES[3][(high & 0xFF) as usize]
            ^ TABLES[2][((high >> 8) & 0xFF) as usize]
            ^ TABLES[1][((high >> 16) & 0xFF) as usize]
            ^ TABLES[0][((high >> 24) & 0xFF) as usize];
        rest = tail;
    }

    // Byte-wise remainder.
    for &byte in rest {
        state = (state >> 8) ^ TABLES[0][((state ^ u32::from(byte)) & 0xFF) as usize];
    }

    state
}

/// Splits an eight-byte chunk off the front of `data`, if one is available.
///
/// Written as a helper so the hot loop indexes a fixed-size array and the
/// bounds checks fold away. Shared with the hardware backends: a 64-bit CRC
/// instruction wants exactly this `&[u8; 8]` chunk as its operand, unaligned
/// reads and all — [`u64::from_le_bytes`] does a byte-wise copy regardless of
/// the slice's base address, so this never assumes 8-byte alignment.
#[inline]
pub(crate) fn split_chunk(data: &[u8]) -> Option<(&[u8; SLICE], &[u8])> {
    if data.len() < SLICE {
        return None;
    }
    let (head, tail) = data.split_at(SLICE);
    let head: &[u8; SLICE] = head.try_into().ok()?;
    Some((head, tail))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn tables_are_internally_consistent() {
        // Every higher table must extend the previous one by one byte.
        for level in 1..SLICE {
            let lower = &TABLES[level - 1];
            let upper = &TABLES[level];
            for (index, (&previous, &actual)) in lower.iter().zip(upper.iter()).enumerate() {
                let expected = (previous >> 8) ^ TABLES[0][(previous & 0xFF) as usize];
                assert_eq!(actual, expected, "table {level} entry {index}");
            }
        }
    }

    #[test]
    fn split_chunk_reports_none_below_eight_bytes() {
        for len in 0..SLICE {
            let data = vec![0u8; len];
            assert!(split_chunk(&data).is_none(), "len {len}");
        }
    }

    #[test]
    fn split_chunk_splits_at_exactly_eight_bytes() {
        let data: Vec<u8> = (0..20u8).collect();
        let (chunk, tail) = split_chunk(&data).expect("20 bytes is at least one chunk");
        assert_eq!(chunk, &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(tail, &data[8..]);
    }
}
