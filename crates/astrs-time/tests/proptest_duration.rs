//! Property tests: human duration parse/format round-trip.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use astrs_time::{format_duration, parse_duration};
use proptest::prelude::*;

proptest! {
    /// `parse_duration(&format_duration(d)) == Ok(d)` for every
    /// representable `Duration` — the exactness guarantee documented on
    /// [`astrs_time::format_duration`], checked across the full `u64`
    /// seconds range (not just "reasonable" small values).
    #[test]
    fn format_then_parse_round_trips(secs in any::<u64>(), nanos in 0u32..1_000_000_000u32) {
        let d = Duration::new(secs, nanos);
        let formatted = format_duration(d);
        let reparsed = parse_duration(&formatted);
        prop_assert_eq!(reparsed, Ok(d));
    }

    /// `parse_duration` must never panic on arbitrary input, regardless of
    /// whether the result is `Ok` or a typed `Err`.
    #[test]
    fn parse_never_panics_on_arbitrary_input(s in ".{0,80}") {
        let _ = parse_duration(&s);
    }

    /// Every `<value><unit>` combination the grammar documents parses
    /// successfully and recovers the exact intended magnitude.
    #[test]
    fn parses_every_documented_unit(value in 0u64..1_000_000u64, unit_idx in 0usize..7) {
        const UNITS: [(&str, u128); 7] = [
            ("ns", 1),
            ("us", 1_000),
            ("ms", 1_000_000),
            ("s", 1_000_000_000),
            ("m", 60_000_000_000),
            ("h", 3_600_000_000_000),
            ("d", 86_400_000_000_000),
        ];
        let (unit, unit_ns) = UNITS[unit_idx];
        let s = format!("{value}{unit}");
        let parsed = parse_duration(&s).unwrap();
        let expected_ns = u128::from(value) * unit_ns;
        prop_assert_eq!(parsed.as_nanos(), expected_ns);
    }

    /// A leading `-` is always rejected, never silently accepted or
    /// misparsed as a valid (positive) duration.
    #[test]
    fn negative_numbers_are_always_rejected(value in 1u64..1_000_000u64, unit_idx in 0usize..7) {
        const UNITS: [&str; 7] = ["ns", "us", "ms", "s", "m", "h", "d"];
        let s = format!("-{value}{}", UNITS[unit_idx]);
        prop_assert!(parse_duration(&s).is_err());
    }

    /// Compound durations (`"1h30m"`-style, two tokens summed) equal the
    /// sum of parsing each token alone.
    #[test]
    fn compound_durations_sum_their_tokens(
        a_value in 0u64..1_000u64,
        b_value in 0u64..1_000u64,
        a_unit_idx in 0usize..7,
        b_unit_idx in 0usize..7,
    ) {
        const UNITS: [&str; 7] = ["ns", "us", "ms", "s", "m", "h", "d"];
        let a_str = format!("{a_value}{}", UNITS[a_unit_idx]);
        let b_str = format!("{b_value}{}", UNITS[b_unit_idx]);
        let combined = format!("{a_str}{b_str}");

        let a = parse_duration(&a_str).unwrap();
        let b = parse_duration(&b_str).unwrap();
        let sum = parse_duration(&combined).unwrap();

        prop_assert_eq!(sum, a + b);
    }
}
