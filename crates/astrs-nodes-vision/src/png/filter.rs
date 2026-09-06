//! PNG scanline filtering (spec §6): the five per-byte predictors
//! (`None`/`Sub`/`Up`/`Average`/`Paeth`) that turn a raw scanline into
//! something DEFLATE compresses well, and back.
//!
//! Every predictor works at a distance of `bpp` — the byte width of one
//! *complete pixel* (`1` for grey, `3` for RGB, `4` for RGBA at 8 bits per
//! sample; never the row's total byte width) — not `1`. Getting that
//! distance wrong is the classic PNG filter bug: it silently still decodes
//! *something*, just the wrong image.
//!
//! All arithmetic is modulo 256 (`wrapping_add`/`wrapping_sub`); the filtered
//! byte stream is not a signed quantity, it is a checksum-free difference
//! encoding that only makes sense unwrapped back through the same modulus.

/// One PNG filter type byte (spec §6.2), the value that prefixes every
/// scanline in the decompressed `IDAT` stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FilterType {
    /// The raw byte, unchanged.
    None,
    /// `raw[x] - raw[x - bpp]`.
    Sub,
    /// `raw[x] - prior[x]`.
    Up,
    /// `raw[x] - floor((raw[x - bpp] + prior[x]) / 2)`.
    Average,
    /// `raw[x] - paeth(raw[x - bpp], prior[x], prior[x - bpp])`.
    Paeth,
}

impl FilterType {
    /// Every filter type, in their spec-assigned byte-value order — the
    /// order [`choose`] tries them in.
    const ALL: [Self; 5] = [Self::None, Self::Sub, Self::Up, Self::Average, Self::Paeth];

    /// The filter type a scanline's leading byte names.
    pub(crate) const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::None),
            1 => Some(Self::Sub),
            2 => Some(Self::Up),
            3 => Some(Self::Average),
            4 => Some(Self::Paeth),
            _ => None,
        }
    }

    /// This filter's spec-assigned byte value.
    pub(crate) const fn as_byte(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Sub => 1,
            Self::Up => 2,
            Self::Average => 3,
            Self::Paeth => 4,
        }
    }
}

/// Paeth's predictor (spec §6.6): picks whichever of `a` (left), `b`
/// (above), `c` (above-left) is closest to `a + b - c`, ties broken in
/// favour of `a`, then `b`, then `c` — the exact rule the spec's own
/// reference pseudocode uses, which a "closest wins, ties unspecified"
/// paraphrase gets wrong for inputs the spec's conformance suite actually
/// exercises.
fn paeth_predictor(a: u8, b: u8, c: u8) -> u8 {
    let (a, b, c) = (i32::from(a), i32::from(b), i32::from(c));
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    let predicted = if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    };
    // `predicted` is one of `a`/`b`/`c`, each already a valid `u8` widened
    // to `i32`, so it narrows back exactly.
    predicted as u8
}

/// The three neighbours a byte at position `x` (`0`-based, within a
/// scanline of complete pixels `bpp` bytes wide) needs to filter or
/// unfilter: left (`a`), above (`b`), above-left (`c`). `0` when there is no
/// such neighbour (spec §6.3: the edges of the image behave as if filled
/// with zero bytes).
///
/// `current` means different things in [`filter_scanline`] and
/// [`unfilter_scanline`]: filtering reads `a`/`c` from the untouched raw
/// scanline, while unfiltering reads them from the *output* scanline being
/// overwritten in place — valid because, left to right, every position
/// before `x` already holds its recovered raw value by the time position
/// `x` is reached. Either way, "the already-known-raw byte `bpp` to the
/// left" is the same read.
const fn neighbors(current: &[u8], previous: &[u8], bpp: usize, x: usize) -> (u8, u8, u8) {
    let a = if x >= bpp { current[x - bpp] } else { 0 };
    let b = if previous.is_empty() { 0 } else { previous[x] };
    let c = if x >= bpp && !previous.is_empty() {
        previous[x - bpp]
    } else {
        0
    };
    (a, b, c)
}

/// Undoes one scanline's filter in place: `scanline` holds the filtered
/// bytes on entry and the raw pixel bytes on exit.
///
/// `previous` is the *already-unfiltered* scanline above (empty for the
/// image's first row — spec §6.3 treats a missing row the same as a missing
/// column, as all zero bytes). Processed left to right, since each
/// recovered byte becomes the next byte's `a`.
///
/// # Panics
///
/// Never on any input this module's own callers pass ([`decode`](super::decode)
/// always supplies a `previous` scanline the same length as `scanline`, or
/// empty) — but see the module doc: this is an internal invariant, not part
/// of a public contract.
pub(crate) fn unfilter_scanline(
    filter: FilterType,
    scanline: &mut [u8],
    previous: &[u8],
    bpp: usize,
) {
    for x in 0..scanline.len() {
        let (a, b, c) = neighbors(scanline, previous, bpp, x);
        let predicted = match filter {
            FilterType::None => 0,
            FilterType::Sub => a,
            FilterType::Up => b,
            FilterType::Average => average_floor(a, b),
            FilterType::Paeth => paeth_predictor(a, b, c),
        };
        scanline[x] = scanline[x].wrapping_add(predicted);
    }
}

/// Applies one filter, writing the filtered bytes of `raw` into `out`
/// (`out.len() == raw.len()` is the caller's responsibility, checked by the
/// slice-indexing panic otherwise — [`encode`](super::encode) always sizes
/// `out` to match).
pub(crate) fn filter_scanline(
    filter: FilterType,
    raw: &[u8],
    previous: &[u8],
    bpp: usize,
    out: &mut [u8],
) {
    for x in 0..raw.len() {
        let (a, b, c) = neighbors(raw, previous, bpp, x);
        let predicted = match filter {
            FilterType::None => 0,
            FilterType::Sub => a,
            FilterType::Up => b,
            FilterType::Average => average_floor(a, b),
            FilterType::Paeth => paeth_predictor(a, b, c),
        };
        out[x] = raw[x].wrapping_sub(predicted);
    }
}

/// `floor((a + b) / 2)`, computed with enough headroom (`u16`) that the sum
/// never wraps the way `u8::wrapping_add` would.
const fn average_floor(a: u8, b: u8) -> u8 {
    ((a as u16 + b as u16) / 2) as u8
}

/// Picks the filter type spec-appendix-recommended "minimum sum of absolute
/// differences" heuristic prefers for one scanline: filter it all five
/// ways, treat each filtered byte as signed (`i8 as i32`, so a byte near
/// `256` counts as "small negative" rather than "huge positive" — the
/// heuristic's whole point is favouring a filtered stream with lots of
/// bytes near zero either side), and keep the smallest sum.
///
/// Ties break toward the earlier entry in [`FilterType::ALL`] (`None` first)
/// — an arbitrary but deterministic choice, matching this module's own
/// "encode output is reproducible" goal for [`super::encode::encode_with`].
pub(crate) fn choose_filter(raw: &[u8], previous: &[u8], bpp: usize) -> FilterType {
    let mut scratch = vec![0u8; raw.len()];
    let mut best = FilterType::None;
    let mut best_cost = u64::MAX;
    for candidate in FilterType::ALL {
        filter_scanline(candidate, raw, previous, bpp, &mut scratch);
        let cost: u64 = scratch
            .iter()
            .map(|&byte| u64::from(i32::from(byte as i8).unsigned_abs()))
            .sum();
        if cost < best_cost {
            best_cost = cost;
            best = candidate;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn filter_type_byte_values_match_the_spec() {
        assert_eq!(FilterType::None.as_byte(), 0);
        assert_eq!(FilterType::Sub.as_byte(), 1);
        assert_eq!(FilterType::Up.as_byte(), 2);
        assert_eq!(FilterType::Average.as_byte(), 3);
        assert_eq!(FilterType::Paeth.as_byte(), 4);
        for filter in FilterType::ALL {
            assert_eq!(FilterType::from_byte(filter.as_byte()), Some(filter));
        }
        assert_eq!(FilterType::from_byte(5), None);
    }

    #[test]
    fn paeth_prefers_a_then_b_then_c_on_a_tie() {
        // a=b=c=10: p = 10+10-10 = 10, pa=pb=pc=0 -> ties all the way, `a` wins.
        assert_eq!(paeth_predictor(10, 10, 10), 10);
        // a=5, b=15, c=10: p = 5+15-10 = 10; pa=|10-5|=5, pb=|10-15|=5,
        // pc=|10-10|=0 -> c is the unique closest.
        assert_eq!(paeth_predictor(5, 15, 10), 10);
        // a=0, b=0, c=1: p = 0+0-1 = -1; pa=1, pb=1, pc=2 -> pa<=pb ties, `a` wins.
        assert_eq!(paeth_predictor(0, 0, 1), 0);
    }

    #[test]
    fn average_floor_matches_a_hand_worked_table() {
        assert_eq!(average_floor(0, 0), 0);
        assert_eq!(average_floor(3, 4), 3); // floor(3.5)
        assert_eq!(average_floor(255, 255), 255);
        assert_eq!(average_floor(255, 0), 127); // floor(127.5), not a wrapped u8 add
    }

    /// The filter layer, checked with hand-written filtered bytes and a
    /// hand-computed expected raw output for each of the five types — a
    /// check that stays meaningful even if `filter_scanline` and
    /// `unfilter_scanline` shared the exact same bug, since it never calls
    /// `filter_scanline` at all (see the module's own "encode/decode share
    /// a bug" risk, called out in the crate's PNG round-trip test).
    #[test]
    fn unfilter_recovers_hand_computed_raw_bytes_for_every_filter_type() {
        let bpp = 1;
        // Raw scanlines chosen so hand-filtering them is easy to check:
        // row0 raw = [10, 20, 30]; row1 raw = [12, 18, 33].
        let previous_raw = [10u8, 20, 30];

        // Filter 0 (None): filtered == raw.
        let mut none_scanline = [12u8, 18, 33];
        unfilter_scanline(FilterType::None, &mut none_scanline, &previous_raw, bpp);
        assert_eq!(none_scanline, [12, 18, 33]);

        // Filter 1 (Sub): filtered[x] = raw[x] - raw[x-1] (0 for x=0).
        // raw = [12, 18, 33] -> filtered = [12, 6, 15].
        let mut sub_scanline = [12u8, 6, 15];
        unfilter_scanline(FilterType::Sub, &mut sub_scanline, &[], bpp);
        assert_eq!(sub_scanline, [12, 18, 33]);

        // Filter 2 (Up): filtered[x] = raw[x] - previous_raw[x].
        // raw = [12, 18, 33], previous = [10, 20, 30] -> filtered = [2, 254, 3]
        // (18 - 20 wraps to 254).
        let mut up_scanline = [2u8, 254, 3];
        unfilter_scanline(FilterType::Up, &mut up_scanline, &previous_raw, bpp);
        assert_eq!(up_scanline, [12, 18, 33]);

        // Filter 3 (Average): filtered[x] = raw[x] - floor((a + b) / 2).
        // x=0: a=0, b=10 -> floor(5) = 5; 12-5 = 7.
        // x=1: a=raw[0]=12, b=20 -> floor(16) = 16; 18-16 = 2.
        // x=2: a=raw[1]=18, b=30 -> floor(24) = 24; 33-24 = 9.
        let mut average_scanline = [7u8, 2, 9];
        unfilter_scanline(
            FilterType::Average,
            &mut average_scanline,
            &previous_raw,
            bpp,
        );
        assert_eq!(average_scanline, [12, 18, 33]);

        // Filter 4 (Paeth): x=0: a=0,b=10,c=0 -> paeth(0,10,0): p=10, pa=10,
        // pb=0, pc=10 -> picks b=10; 12-10=2.
        // x=1: a=raw[0]=12, b=20, c=previous[0]=10 -> p=12+20-10=22; pa=10,
        // pb=2, pc=12 -> picks b=20; 18-20 wraps to 254.
        // x=2: a=raw[1]=18, b=30, c=previous[1]=20 -> p=18+30-20=28; pa=10,
        // pb=2, pc=8 -> picks b=30; 33-30=3.
        let mut paeth_scanline = [2u8, 254, 3];
        unfilter_scanline(FilterType::Paeth, &mut paeth_scanline, &previous_raw, bpp);
        assert_eq!(paeth_scanline, [12, 18, 33]);
    }

    #[test]
    fn filter_then_unfilter_round_trips_for_every_type_and_bpp() {
        let previous = [5u8, 250, 0, 128];
        let raw = [200u8, 1, 255, 64];
        for bpp in [1usize, 2, 4] {
            for filter in FilterType::ALL {
                let mut filtered = vec![0u8; raw.len()];
                filter_scanline(filter, &raw, &previous, bpp, &mut filtered);
                unfilter_scanline(filter, &mut filtered, &previous, bpp);
                assert_eq!(filtered, raw, "{filter:?} bpp={bpp}");
            }
        }
    }

    #[test]
    fn choosing_up_is_optimal_when_the_row_matches_the_one_above() {
        // A scanline that already equals `previous` exactly: `Up` filters
        // every byte to zero, strictly beating `None`'s untouched bytes.
        let previous = [10u8, 10, 10, 10];
        let raw = previous;
        assert_eq!(choose_filter(&raw, &previous, 1), FilterType::Up);
    }

    #[test]
    fn choose_filter_never_panics_on_the_first_scanline() {
        let raw = [1u8, 2, 3, 250, 251, 252];
        let chosen = choose_filter(&raw, &[], 3);
        // Just needs to not panic and to be one of the five; the heuristic
        // itself is exercised by the round-trip test above.
        assert!(FilterType::ALL.contains(&chosen));
    }
}
