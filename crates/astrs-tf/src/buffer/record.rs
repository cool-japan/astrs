//! [`FrameStore`] — one child frame's stored relationship to its parent:
//! either a single time-independent [`StaticEntry`] or a timestamped
//! [`DynamicEntry`] history.

use std::collections::VecDeque;
use std::time::Duration;

use crate::error::{ExtrapolationDirection, FrameKind, TfError};
use crate::frame::FrameId;
use crate::math::Isometry3;
use crate::time::{TfStamp, TimePoint};

/// A static child frame's single, time-independent parent/transform pair.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StaticEntry {
    pub(crate) parent: FrameId,
    pub(crate) transform: Isometry3,
    pub(crate) stamp: TfStamp,
}

/// One sample in a dynamic child frame's history.
///
/// Carries its own `parent`, not just a transform — a dynamic frame's
/// parent is allowed to change across its history (see [`crate::error`]'s
/// module docs, "what is deliberately not a validation failure"), so each
/// sample must be self-describing rather than trusting a single
/// buffer-wide parent field.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DynamicEntry {
    pub(crate) parent: FrameId,
    pub(crate) transform: Isometry3,
    pub(crate) stamp: TfStamp,
}

/// A child frame's stored relationship to its parent: static (one value,
/// any query time) or dynamic (a timestamped history, interpolated on
/// lookup).
///
/// Deliberately mutually exclusive — a frame is *either* static *or*
/// dynamic for as long as it is registered (see
/// [`crate::error::TfError::FrameKindConflict`]) — because the two answer
/// "what was the transform at time T" in incompatible ways: a static
/// entry ignores `T` entirely, a dynamic history brackets and interpolates
/// around it. Mixing them for one child frame would leave "what does a
/// query between the two mean" undefined.
#[derive(Debug, Clone)]
pub(crate) enum FrameStore {
    Static(StaticEntry),
    Dynamic(VecDeque<DynamicEntry>),
}

impl FrameStore {
    /// A fresh static store.
    pub(crate) fn new_static(parent: FrameId, transform: Isometry3, stamp: TfStamp) -> Self {
        Self::Static(StaticEntry {
            parent,
            transform,
            stamp,
        })
    }

    /// A fresh dynamic store holding one sample.
    pub(crate) fn new_dynamic(parent: FrameId, transform: Isometry3, stamp: TfStamp) -> Self {
        let mut deque = VecDeque::with_capacity(1);
        deque.push_back(DynamicEntry {
            parent,
            transform,
            stamp,
        });
        Self::Dynamic(deque)
    }

    /// This store's [`FrameKind`].
    pub(crate) fn kind(&self) -> FrameKind {
        match self {
            Self::Static(_) => FrameKind::Static,
            Self::Dynamic(_) => FrameKind::Dynamic,
        }
    }

    /// The parent a [`TimePoint::Latest`] query would resolve to right now.
    ///
    /// Used only by [`crate::buffer::TransformBuffer`]'s *proactive*,
    /// best-effort cycle check at insert time — the *reactive* guard inside
    /// [`FrameStore::resolve_hop`]'s caller (the walk itself) is what
    /// actually guarantees no lookup can loop forever, for any query time,
    /// regardless of what this returns.
    pub(crate) fn latest_parent(&self) -> Option<FrameId> {
        match self {
            Self::Static(entry) => Some(entry.parent),
            Self::Dynamic(deque) => deque.back().map(|entry| entry.parent),
        }
    }

    /// The timestamp a [`TimePoint::Latest`] query would resolve to right
    /// now — a static store's single stamp, or a dynamic store's newest
    /// sample. Used by [`crate::buffer::TransformBuffer::set_transform`]
    /// to decide whether a new dynamic sample would become the latest
    /// answer (and therefore needs the proactive cycle check).
    pub(crate) fn latest_stamp(&self) -> Option<TfStamp> {
        match self {
            Self::Static(entry) => Some(entry.stamp),
            Self::Dynamic(deque) => deque.back().map(|entry| entry.stamp),
        }
    }

    /// The `(oldest, newest)` buffered timestamps for a dynamic store — the
    /// exact range [`FrameStore::resolve_hop`] can answer with an
    /// interpolated or exact result, `TimePoint::At` outside it always
    /// being [`TfError::Extrapolation`]. `None` for a static store: it
    /// answers every query time identically, so no such range constrains
    /// it. Used by [`crate::buffer::TransformBuffer::history_bounds`].
    pub(crate) fn history_bounds(&self) -> Option<(TfStamp, TfStamp)> {
        match self {
            Self::Static(_) => None,
            Self::Dynamic(deque) => {
                let oldest = deque.front()?.stamp;
                let newest = deque.back()?.stamp;
                Some((oldest, newest))
            }
        }
    }

    /// Overwrites a static store's single entry in place.
    ///
    /// The caller (`TransformBuffer::set_transform`) guarantees `self` is
    /// already [`FrameStore::Static`] — mismatched-kind calls are rejected
    /// before reaching here — so this silently does nothing on the (dead,
    /// but non-panicking) alternative rather than asserting.
    pub(crate) fn overwrite_static(
        &mut self,
        parent: FrameId,
        transform: Isometry3,
        stamp: TfStamp,
    ) {
        if let Self::Static(entry) = self {
            *entry = StaticEntry {
                parent,
                transform,
                stamp,
            };
        }
    }

    /// Inserts a new dynamic sample in timestamp order (binary-search
    /// insert; an exact-timestamp duplicate replaces the existing entry
    /// rather than growing the history with an ambiguous same-instant
    /// pair), then prunes samples older than `newest_stamp - max_history`.
    ///
    /// Same non-panicking no-op-on-mismatch contract as
    /// [`FrameStore::overwrite_static`].
    pub(crate) fn insert_dynamic(
        &mut self,
        parent: FrameId,
        transform: Isometry3,
        stamp: TfStamp,
        max_history: Duration,
    ) {
        let Self::Dynamic(deque) = self else {
            return;
        };
        let entry = DynamicEntry {
            parent,
            transform,
            stamp,
        };
        match deque.binary_search_by_key(&stamp, |existing| existing.stamp) {
            Ok(index) => deque[index] = entry,
            Err(index) => deque.insert(index, entry),
        }
        // The newest entry always survives (`cutoff <= newest_stamp`), so a
        // dynamic store's history is never emptied by pruning — see
        // `crate::error::TfError::EmptyHistory`'s docs.
        if let Some(newest_stamp) = deque.back().map(|entry| entry.stamp) {
            let cutoff = newest_stamp.saturating_sub(max_history);
            while deque.front().is_some_and(|entry| entry.stamp < cutoff) {
                deque.pop_front();
            }
        }
    }

    /// Resolves `(parent, transform)` at `time`, for a store belonging to
    /// the frame named `frame_name` (used only to label an error).
    ///
    /// `time == TimePoint::Latest` here always means *this store's own*
    /// newest sample, taken independently of whatever any other store in
    /// the same walk resolves to — correct for a single hop considered
    /// alone (which is all this type ever sees). A multi-hop lookup that
    /// needs every dynamic edge to agree on one shared instant does not
    /// call this with `Latest` directly: `crate::buffer::TransformBuffer::
    /// lookup_transform` pre-resolves `Latest` to a single `TimePoint::At`
    /// (the "latest common time" — see [`TimePoint::Latest`]'s own docs)
    /// before any `FrameStore` in the walk is touched, so `Latest` only
    /// ever reaches here for a lookup with at most one dynamic edge, where
    /// per-store independence and the shared-instant contract coincide.
    ///
    /// # Errors
    ///
    /// [`TfError::EmptyHistory`] (practically unreachable — see
    /// [`FrameStore::insert_dynamic`]) or [`TfError::Extrapolation`] when
    /// `time` falls outside a dynamic store's buffered range.
    pub(crate) fn resolve_hop(
        &self,
        time: TimePoint,
        frame_name: &str,
    ) -> Result<(FrameId, Isometry3), TfError> {
        match self {
            Self::Static(entry) => Ok((entry.parent, entry.transform)),
            Self::Dynamic(deque) => resolve_dynamic_hop(deque, time, frame_name),
        }
    }
}

fn resolve_dynamic_hop(
    deque: &VecDeque<DynamicEntry>,
    time: TimePoint,
    frame_name: &str,
) -> Result<(FrameId, Isometry3), TfError> {
    let (Some(oldest), Some(newest)) = (deque.front().copied(), deque.back().copied()) else {
        return Err(TfError::EmptyHistory {
            frame: frame_name.to_owned(),
        });
    };
    let stamp = match time {
        TimePoint::Latest => return Ok((newest.parent, newest.transform)),
        TimePoint::At(stamp) => stamp,
    };
    match deque.binary_search_by_key(&stamp, |entry| entry.stamp) {
        // Exact match: the property "lookup at exact stamps returns exact
        // transforms" holds because this branch never interpolates.
        Ok(index) => {
            let entry = deque[index];
            Ok((entry.parent, entry.transform))
        }
        Err(0) => Err(TfError::Extrapolation {
            frame: frame_name.to_owned(),
            requested: stamp,
            bound: oldest.stamp,
            direction: ExtrapolationDirection::Past,
        }),
        Err(index) if index == deque.len() => Err(TfError::Extrapolation {
            frame: frame_name.to_owned(),
            requested: stamp,
            bound: newest.stamp,
            direction: ExtrapolationDirection::Future,
        }),
        Err(index) => {
            let before = deque[index - 1];
            let after = deque[index];
            if before.parent == after.parent {
                let t = fraction(before.stamp, after.stamp, stamp);
                Ok((
                    before.parent,
                    before.transform.interpolate(after.transform, t),
                ))
            } else {
                // The frame was re-parented between `before` and `after`
                // (blueprint: parent may change across a dynamic frame's
                // history — see `crate::error`'s module docs). Blending a
                // transform expressed relative to one parent with one
                // expressed relative to a *different* parent has no
                // well-defined meaning, so this treats `after` as already
                // in effect for any query time at or past the topology
                // change, rather than inventing a meaningless blend.
                Ok((after.parent, after.transform))
            }
        }
    }
}

/// The interpolation fraction of `query` between `a` and `b`, assuming
/// `a < query < b` (guaranteed by every call site — the `binary_search`
/// branch this is called from only reaches here for a strict bracket).
/// Guards `span <= 0` anyway: not a panic risk (float division by zero
/// does not panic), but it would silently produce a `NaN`/`±∞` fraction
/// that propagates into [`Isometry3::interpolate`] rather than a clean
/// value, so the guard is about correctness, not safety.
fn fraction(a: TfStamp, b: TfStamp, query: TfStamp) -> f64 {
    let span = b.signed_nanos_since(a);
    if span <= 0 {
        return 0.0;
    }
    query.signed_nanos_since(a) as f64 / span as f64
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::frame::FrameRegistry;
    use crate::math::{Quaternion, Vector3};

    /// Mints frame `n`'s id from a **shared, per-test** registry.
    ///
    /// `FrameId` has no public constructor (see its own docs — it is only
    /// ever minted by `FrameRegistry`), so tests go through a real one
    /// rather than re-deriving its private layout. The registry must be
    /// created once per test and threaded through every `fid` call in that
    /// test (never one fresh `FrameRegistry` per call) — a fresh registry
    /// always interns its first name as index `0`, which would make
    /// `fid(&mut FrameRegistry::new(), 1)` and
    /// `fid(&mut FrameRegistry::new(), 2)` both `FrameId` `0` and silently
    /// erase the distinctness these tests are checking for.
    fn fid(registry: &mut FrameRegistry, n: u32) -> FrameId {
        registry.intern(&format!("frame-{n}"))
    }

    fn ts(nanos: i64) -> TfStamp {
        TfStamp::from_nanos(nanos)
    }

    fn tf(x: f64) -> Isometry3 {
        Isometry3::from_translation(Vector3::new(x, 0.0, 0.0))
    }

    #[test]
    fn static_store_ignores_query_time() {
        let mut registry = FrameRegistry::new();
        let store = FrameStore::new_static(fid(&mut registry, 0), tf(1.0), ts(100));
        let (_, at_zero) = store.resolve_hop(TimePoint::At(ts(0)), "child").unwrap();
        let (_, at_far_future) = store
            .resolve_hop(TimePoint::At(ts(i64::MAX)), "child")
            .unwrap();
        assert_eq!(at_zero, tf(1.0));
        assert_eq!(at_far_future, tf(1.0));
    }

    #[test]
    fn overwrite_static_replaces_the_single_entry() {
        let mut registry = FrameRegistry::new();
        let mut store = FrameStore::new_static(fid(&mut registry, 0), tf(1.0), ts(100));
        let new_parent = fid(&mut registry, 1);
        store.overwrite_static(new_parent, tf(2.0), ts(200));
        let (parent, transform) = store.resolve_hop(TimePoint::Latest, "child").unwrap();
        assert_eq!(parent, new_parent);
        assert_eq!(transform, tf(2.0));
    }

    #[test]
    fn dynamic_exact_stamp_is_not_interpolated() {
        let mut registry = FrameRegistry::new();
        let parent = fid(&mut registry, 0);
        let mut store = FrameStore::new_dynamic(parent, tf(0.0), ts(0));
        store.insert_dynamic(parent, tf(10.0), ts(1_000_000_000), Duration::from_secs(10));
        let (_, exact) = store.resolve_hop(TimePoint::At(ts(0)), "child").unwrap();
        assert_eq!(exact, tf(0.0));
        let (_, exact_newer) = store
            .resolve_hop(TimePoint::At(ts(1_000_000_000)), "child")
            .unwrap();
        assert_eq!(exact_newer, tf(10.0));
    }

    #[test]
    fn dynamic_interpolates_between_brackets() {
        let mut registry = FrameRegistry::new();
        let parent = fid(&mut registry, 0);
        let mut store = FrameStore::new_dynamic(parent, tf(0.0), ts(0));
        store.insert_dynamic(parent, tf(10.0), ts(1_000_000_000), Duration::from_secs(10));
        let (_, mid) = store
            .resolve_hop(TimePoint::At(ts(500_000_000)), "child")
            .unwrap();
        assert_eq!(mid, tf(5.0));
    }

    #[test]
    fn dynamic_extrapolation_past_is_an_error() {
        let mut registry = FrameRegistry::new();
        let store = FrameStore::new_dynamic(fid(&mut registry, 0), tf(0.0), ts(1_000));
        let err = store
            .resolve_hop(TimePoint::At(ts(500)), "child")
            .unwrap_err();
        assert_eq!(
            err,
            TfError::Extrapolation {
                frame: "child".to_owned(),
                requested: ts(500),
                bound: ts(1_000),
                direction: ExtrapolationDirection::Past,
            }
        );
    }

    #[test]
    fn dynamic_extrapolation_future_is_an_error() {
        let mut registry = FrameRegistry::new();
        let store = FrameStore::new_dynamic(fid(&mut registry, 0), tf(0.0), ts(1_000));
        let err = store
            .resolve_hop(TimePoint::At(ts(2_000)), "child")
            .unwrap_err();
        assert_eq!(
            err,
            TfError::Extrapolation {
                frame: "child".to_owned(),
                requested: ts(2_000),
                bound: ts(1_000),
                direction: ExtrapolationDirection::Future,
            }
        );
    }

    #[test]
    fn latest_returns_the_newest_sample_without_interpolating() {
        let mut registry = FrameRegistry::new();
        let parent = fid(&mut registry, 0);
        let mut store = FrameStore::new_dynamic(parent, tf(0.0), ts(0));
        store.insert_dynamic(parent, tf(10.0), ts(1_000_000_000), Duration::from_secs(10));
        let (_, latest) = store.resolve_hop(TimePoint::Latest, "child").unwrap();
        assert_eq!(latest, tf(10.0));
    }

    #[test]
    fn out_of_order_insertion_is_sorted_by_stamp() {
        let mut registry = FrameRegistry::new();
        let parent = fid(&mut registry, 0);
        let mut store = FrameStore::new_dynamic(parent, tf(10.0), ts(1_000_000_000));
        // Arrives late, but timestamped earlier than the first sample.
        store.insert_dynamic(parent, tf(0.0), ts(0), Duration::from_secs(10));
        let (_, mid) = store
            .resolve_hop(TimePoint::At(ts(500_000_000)), "child")
            .unwrap();
        assert_eq!(mid, tf(5.0));
    }

    #[test]
    fn a_duplicate_stamp_replaces_rather_than_duplicates() {
        let mut registry = FrameRegistry::new();
        let parent = fid(&mut registry, 0);
        let mut store = FrameStore::new_dynamic(parent, tf(1.0), ts(100));
        store.insert_dynamic(parent, tf(2.0), ts(100), Duration::from_secs(10));
        let (_, at_100) = store.resolve_hop(TimePoint::At(ts(100)), "child").unwrap();
        assert_eq!(at_100, tf(2.0));
        let FrameStore::Dynamic(deque) = &store else {
            panic!("expected a dynamic store");
        };
        assert_eq!(deque.len(), 1);
    }

    #[test]
    fn pruning_drops_entries_older_than_the_history_window() {
        let mut registry = FrameRegistry::new();
        let parent = fid(&mut registry, 0);
        let mut store = FrameStore::new_dynamic(parent, tf(0.0), ts(0));
        store.insert_dynamic(parent, tf(1.0), ts(5_000_000_000), Duration::from_secs(2));
        // The `ts(0)` sample is now 5s older than the newest (2s window) —
        // pruned. Querying at `ts(0)` should now be a past-extrapolation
        // error, not the pruned exact value.
        let err = store
            .resolve_hop(TimePoint::At(ts(0)), "child")
            .unwrap_err();
        assert!(matches!(
            err,
            TfError::Extrapolation {
                direction: ExtrapolationDirection::Past,
                ..
            }
        ));
    }

    #[test]
    fn a_reparent_boundary_uses_the_newer_topology_without_blending() {
        let mut registry = FrameRegistry::new();
        let old_parent = fid(&mut registry, 1);
        let new_parent = fid(&mut registry, 2);
        assert_ne!(old_parent, new_parent, "test setup: ids must be distinct");
        let mut store = FrameStore::new_dynamic(old_parent, tf(0.0), ts(0));
        // Re-parented from `old_parent` to `new_parent` at ts(1_000_000_000).
        store.insert_dynamic(
            new_parent,
            tf(10.0),
            ts(1_000_000_000),
            Duration::from_secs(10),
        );
        let (parent, transform) = store
            .resolve_hop(TimePoint::At(ts(500_000_000)), "child")
            .unwrap();
        // No blend across the reparent: the newer (`after`) sample wins.
        assert_eq!(parent, new_parent);
        assert_eq!(transform, tf(10.0));
    }

    #[test]
    fn kind_reports_static_or_dynamic() {
        let mut registry = FrameRegistry::new();
        let parent = fid(&mut registry, 0);
        assert_eq!(
            FrameStore::new_static(parent, tf(0.0), ts(0)).kind(),
            FrameKind::Static
        );
        assert_eq!(
            FrameStore::new_dynamic(parent, tf(0.0), ts(0)).kind(),
            FrameKind::Dynamic
        );
    }

    #[test]
    fn latest_parent_reflects_the_most_recent_sample() {
        let mut registry = FrameRegistry::new();
        let first_parent = fid(&mut registry, 1);
        let second_parent = fid(&mut registry, 2);
        assert_ne!(
            first_parent, second_parent,
            "test setup: ids must be distinct"
        );
        let mut store = FrameStore::new_dynamic(first_parent, tf(0.0), ts(0));
        assert_eq!(store.latest_parent(), Some(first_parent));
        store.insert_dynamic(second_parent, tf(1.0), ts(1_000), Duration::from_secs(10));
        assert_eq!(store.latest_parent(), Some(second_parent));
    }

    #[test]
    fn history_bounds_is_none_for_a_static_store() {
        let mut registry = FrameRegistry::new();
        let parent = fid(&mut registry, 0);
        let store = FrameStore::new_static(parent, tf(0.0), ts(1_000));
        assert_eq!(store.history_bounds(), None);
    }

    #[test]
    fn history_bounds_spans_the_oldest_and_newest_dynamic_samples() {
        let mut registry = FrameRegistry::new();
        let parent = fid(&mut registry, 0);
        let mut store = FrameStore::new_dynamic(parent, tf(0.0), ts(1_000));
        assert_eq!(store.history_bounds(), Some((ts(1_000), ts(1_000))));
        store.insert_dynamic(parent, tf(1.0), ts(2_000), Duration::from_secs(10));
        // Out-of-order insert, older than both existing samples.
        store.insert_dynamic(parent, tf(-1.0), ts(500), Duration::from_secs(10));
        assert_eq!(store.history_bounds(), Some((ts(500), ts(2_000))));
    }

    #[test]
    fn history_bounds_reflects_pruning() {
        let mut registry = FrameRegistry::new();
        let parent = fid(&mut registry, 0);
        let mut store = FrameStore::new_dynamic(parent, tf(0.0), ts(0));
        store.insert_dynamic(parent, tf(1.0), ts(5_000_000_000), Duration::from_secs(2));
        // The `ts(0)` sample was pruned (5s old, 2s window); the surviving
        // bound is the newest sample on both ends.
        assert_eq!(
            store.history_bounds(),
            Some((ts(5_000_000_000), ts(5_000_000_000)))
        );
    }

    #[test]
    fn identity_quaternion_rotation_is_unaffected_by_interpolation_direction() {
        // Sanity check that `resolve_hop`'s interpolation path actually
        // calls through to `Isometry3::interpolate` (rotation included),
        // not just the translation.
        let mut registry = FrameRegistry::new();
        let parent = fid(&mut registry, 0);
        let mut store = FrameStore::new_dynamic(
            parent,
            Isometry3::from_rotation(Quaternion::IDENTITY),
            ts(0),
        );
        store.insert_dynamic(
            parent,
            Isometry3::from_rotation(Quaternion::from_axis_angle(
                Vector3::UNIT_Z,
                std::f64::consts::FRAC_PI_2,
            )),
            ts(1_000_000_000),
            Duration::from_secs(10),
        );
        let (_, mid) = store
            .resolve_hop(TimePoint::At(ts(500_000_000)), "child")
            .unwrap();
        let (_, angle) = mid.rotation.to_axis_angle();
        assert!((angle - std::f64::consts::FRAC_PI_4).abs() < 1e-9);
    }
}
