//! Property tests: [`astrs_time::HlcTimestamp`] total ordering, its
//! consistency with the packed `u128` view, and [`astrs_time::Stamped`]'s
//! derived ordering.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_time::{HlcTimestamp, Stamped};
use proptest::prelude::*;

fn hlc_strategy() -> impl Strategy<Value = HlcTimestamp> {
    (any::<u64>(), any::<u32>())
        .prop_map(|(physical_ns, logical)| HlcTimestamp::new(physical_ns, logical))
}

proptest! {
    /// For any two timestamps, exactly one of `<`, `==`, `>` holds — the
    /// defining property of a total order.
    #[test]
    fn ordering_is_total(a in hlc_strategy(), b in hlc_strategy()) {
        let flags = u8::from(a < b) + u8::from(a == b) + u8::from(a > b);
        prop_assert_eq!(flags, 1);
    }

    /// `<=` is transitive.
    #[test]
    fn ordering_is_transitive(a in hlc_strategy(), b in hlc_strategy(), c in hlc_strategy()) {
        if a <= b && b <= c {
            prop_assert!(a <= c);
        }
    }

    /// Comparing two timestamps agrees with comparing their packed `u128`
    /// views — the layout guarantee documented on `HlcTimestamp`.
    #[test]
    fn ordering_matches_u128_packing(a in hlc_strategy(), b in hlc_strategy()) {
        prop_assert_eq!(a.cmp(&b), a.as_u128().cmp(&b.as_u128()));
    }

    /// `as_u128`/`from_u128` round-trip for every timestamp.
    #[test]
    fn packed_round_trip(a in hlc_strategy()) {
        prop_assert_eq!(HlcTimestamp::from_u128(a.as_u128()), a);
    }

    /// Comparing two timestamps agrees with comparing
    /// `(physical_ns, logical)` tuples directly — physical time dominates,
    /// logical only breaks ties.
    #[test]
    fn ordering_agrees_with_physical_then_logical_tuple(a in hlc_strategy(), b in hlc_strategy()) {
        let tuple_order = (a.physical_ns(), a.logical()).cmp(&(b.physical_ns(), b.logical()));
        prop_assert_eq!(a.cmp(&b), tuple_order);
    }

    /// `Stamped<T>`'s derived `Ord` compares `ts` first and `inner` only as
    /// a tiebreaker (documented on `Stamped`).
    #[test]
    fn stamped_ordering_prioritizes_timestamp_over_inner(
        ts_a in hlc_strategy(), val_a in any::<i32>(),
        ts_b in hlc_strategy(), val_b in any::<i32>(),
    ) {
        let a = Stamped::new(ts_a, val_a);
        let b = Stamped::new(ts_b, val_b);
        let expected = if ts_a != ts_b { ts_a.cmp(&ts_b) } else { val_a.cmp(&val_b) };
        prop_assert_eq!(a.cmp(&b), expected);
    }

    /// Sorting a slice of `Stamped<T>` by the `Ord` impl produces the same
    /// order as sorting by `.ts` alone via `sort_by_key`, for payloads that
    /// happen to also be `Ord` (confirms the two documented sorting
    /// strategies do not silently diverge when both are available).
    #[test]
    fn full_ord_sort_and_sort_by_key_agree_on_timestamp_order(
        entries in proptest::collection::vec((hlc_strategy(), any::<i32>()), 0..30),
    ) {
        let mut by_full_ord: Vec<Stamped<i32>> = entries
            .iter()
            .map(|&(ts, v)| Stamped::new(ts, v))
            .collect();
        let mut by_key = by_full_ord.clone();

        by_full_ord.sort();
        by_key.sort_by_key(|s| s.ts);

        let ts_from_full_ord: Vec<HlcTimestamp> = by_full_ord.iter().map(|s| s.ts).collect();
        let ts_from_key: Vec<HlcTimestamp> = by_key.iter().map(|s| s.ts).collect();
        prop_assert_eq!(ts_from_full_ord, ts_from_key);
    }
}
