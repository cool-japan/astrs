//! [`Stamped<T>`] — the universal HLC-timestamped event wrapper.

use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::timestamp::HlcTimestamp;

/// A value paired with the [`HlcTimestamp`] it occurred at.
///
/// Per the blueprint (§4.3), every event in every AstRS merged event
/// loop — peer connections, local node messages, timers, internal
/// channels — is wrapped in a `Stamped<T>` as it is updated on receive,
/// giving cluster-wide causal ordering that the recorder (§14) and the TUI
/// timeline rely on.
///
/// # Ordering
///
/// `Stamped<T>`'s derived [`Ord`] (when `T: Ord`) compares `ts` first and
/// `inner` second, since `ts` is declared first — so `.sort()` on a
/// `Vec<Stamped<T>>` produces timestamp order, with `inner` only breaking
/// ties between events that share an identical timestamp. When `T` does not
/// implement `Ord`, sort by timestamp alone with
/// `.sort_by_key(|s| s.ts)` — cheap, since [`HlcTimestamp`] is `Copy`.
///
/// A timestamp-only comparison is deliberately *not* exposed as this type's
/// [`Ord`] implementation: doing so would make `Ord` inconsistent with the
/// derived [`Eq`] (two values with equal `ts` but different `inner` would
/// compare as both "equal" under `Ord` and "not equal" under `Eq`), which
/// silently breaks the documented contract of ordered collections like
/// `BTreeSet`.
///
/// # Examples
///
/// ```
/// use astrs_time::{HlcTimestamp, Stamped};
///
/// let event = Stamped::new(HlcTimestamp::new(1_000, 0), "sensor-frame");
/// assert_eq!(event.inner, "sensor-frame");
///
/// let mapped = event.map(str::len);
/// assert_eq!(mapped.inner, "sensor-frame".len());
/// assert_eq!(mapped.ts, HlcTimestamp::new(1_000, 0));
/// ```
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Encode, Decode,
)]
pub struct Stamped<T> {
    /// The event's hybrid logical clock timestamp.
    pub ts: HlcTimestamp,
    /// The event payload.
    pub inner: T,
}

impl<T> Stamped<T> {
    /// Pairs `inner` with `ts`.
    #[must_use]
    pub const fn new(ts: HlcTimestamp, inner: T) -> Self {
        Self { ts, inner }
    }

    /// Discards the timestamp, returning the wrapped value.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.inner
    }

    /// Applies `f` to the wrapped value, keeping the timestamp unchanged.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_time::{HlcTimestamp, Stamped};
    ///
    /// let event = Stamped::new(HlcTimestamp::new(1_000, 0), 41);
    /// let next = event.map(|n| n + 1);
    /// assert_eq!(next.inner, 42);
    /// ```
    #[must_use]
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Stamped<U> {
        Stamped {
            ts: self.ts,
            inner: f(self.inner),
        }
    }

    /// Borrows the wrapped value without cloning it, keeping the timestamp.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_time::{HlcTimestamp, Stamped};
    ///
    /// let event = Stamped::new(HlcTimestamp::new(1_000, 0), String::from("hi"));
    /// let borrowed: Stamped<&String> = event.as_ref();
    /// assert_eq!(borrowed.inner, "hi");
    /// ```
    #[must_use]
    pub fn as_ref(&self) -> Stamped<&T> {
        Stamped {
            ts: self.ts,
            inner: &self.inner,
        }
    }

    /// Mutably borrows the wrapped value without cloning it, keeping the
    /// timestamp.
    #[must_use]
    pub fn as_mut(&mut self) -> Stamped<&mut T> {
        Stamped {
            ts: self.ts,
            inner: &mut self.inner,
        }
    }

    /// Replaces the timestamp, keeping the wrapped value.
    ///
    /// Used when re-stamping a re-emitted event (e.g. a bridge or operator
    /// forwarding a value under a new timestamp) without needing to
    /// destructure and rebuild the wrapper.
    #[must_use]
    pub fn with_ts(self, ts: HlcTimestamp) -> Self {
        Self {
            ts,
            inner: self.inner,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn new_and_field_access() {
        let ts = HlcTimestamp::new(1_000, 0);
        let s = Stamped::new(ts, 42);
        assert_eq!(s.ts, ts);
        assert_eq!(s.inner, 42);
    }

    #[test]
    fn into_inner_discards_timestamp() {
        let s = Stamped::new(HlcTimestamp::new(1, 0), String::from("payload"));
        assert_eq!(s.into_inner(), "payload");
    }

    #[test]
    fn map_transforms_inner_keeps_ts() {
        let ts = HlcTimestamp::new(1_000, 7);
        let s = Stamped::new(ts, 10);
        let mapped = s.map(|n| n * 2);
        assert_eq!(mapped.inner, 20);
        assert_eq!(mapped.ts, ts);
    }

    #[test]
    fn as_ref_borrows_without_cloning() {
        let s = Stamped::new(HlcTimestamp::new(1, 0), vec![1, 2, 3]);
        let borrowed = s.as_ref();
        assert_eq!(borrowed.inner, &vec![1, 2, 3]);
        // `s` is still usable: `as_ref` did not consume it.
        assert_eq!(s.inner.len(), 3);
    }

    #[test]
    fn as_mut_allows_in_place_mutation() {
        let mut s = Stamped::new(HlcTimestamp::new(1, 0), vec![1, 2, 3]);
        s.as_mut().inner.push(4);
        assert_eq!(s.inner, vec![1, 2, 3, 4]);
    }

    #[test]
    fn with_ts_replaces_only_the_timestamp() {
        let s = Stamped::new(HlcTimestamp::new(1, 0), "payload");
        let restamped = s.with_ts(HlcTimestamp::new(2, 0));
        assert_eq!(restamped.ts, HlcTimestamp::new(2, 0));
        assert_eq!(restamped.inner, "payload");
    }

    #[test]
    fn ordering_is_by_timestamp_then_inner() {
        let a = Stamped::new(HlcTimestamp::new(1, 0), 99);
        let b = Stamped::new(HlcTimestamp::new(2, 0), 1);
        assert!(
            a < b,
            "earlier timestamp sorts first regardless of inner value"
        );

        let c = Stamped::new(HlcTimestamp::new(1, 0), 1);
        let d = Stamped::new(HlcTimestamp::new(1, 0), 2);
        assert!(c < d, "equal timestamps fall back to comparing inner");
    }

    #[test]
    fn sort_by_key_orders_by_timestamp_without_requiring_ord_on_inner() {
        // A payload type that intentionally does not implement `Ord`.
        #[derive(Debug, Clone, PartialEq)]
        struct NotOrd(u32);

        let mut events = [
            Stamped::new(HlcTimestamp::new(3, 0), NotOrd(1)),
            Stamped::new(HlcTimestamp::new(1, 0), NotOrd(2)),
            Stamped::new(HlcTimestamp::new(2, 0), NotOrd(3)),
        ];
        events.sort_by_key(|s| s.ts);
        let physical: Vec<u64> = events.iter().map(|s| s.ts.physical_ns()).collect();
        assert_eq!(physical, vec![1, 2, 3]);
    }

    #[test]
    fn oxicode_round_trip_for_generic_payload() {
        let s = Stamped::new(HlcTimestamp::new(123, 4), String::from("payload"));
        let bytes = oxicode::encode_to_vec(&s).unwrap();
        let (decoded, _len): (Stamped<String>, usize) = oxicode::decode_from_slice(&bytes).unwrap();
        assert_eq!(decoded, s);
    }

    #[test]
    fn serde_json_round_trip_for_generic_payload() {
        let s = Stamped::new(HlcTimestamp::new(123, 4), vec![1u8, 2, 3]);
        let json = serde_json::to_string(&s).unwrap();
        let decoded: Stamped<Vec<u8>> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, s);
    }

    #[test]
    fn copy_when_inner_is_copy() {
        let s = Stamped::new(HlcTimestamp::new(1, 0), 5i32);
        let copy = s;
        // Both usable: `Stamped<i32>` is `Copy` because `i32` is `Copy`.
        assert_eq!(s.inner, copy.inner);
    }
}
