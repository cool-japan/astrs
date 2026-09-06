//! [`TransformBuffer`] — the tf2 frame tree.

use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;

use crate::error::{FrameKind, TfError};
use crate::frame::{FrameId, FrameRegistry};
use crate::math::Isometry3;
use crate::time::{TfStamp, TimePoint};

use super::record::FrameStore;

/// The default transform history window: ten seconds, matching tf2's own
/// `tf2::BufferCore::DEFAULT_CACHE_TIME`.
pub const DEFAULT_MAX_HISTORY: Duration = Duration::from_secs(10);

/// The tf2-compatible frame tree: static and dynamic frames, interpolated
/// time-travel lookup, and frame-graph validation (blueprint §10.6).
///
/// # Storage
///
/// One `record::FrameStore` (crate-private) per *child* frame — matching
/// tf2's own `BufferCore` design, which keys its `TimeCache`s by child
/// frame id, not by `(parent, child)` pair. This is what makes the frame
/// graph a forest by construction: a child frame has at most one store, so
/// it can only ever answer "who is my parent" one way at a time (though
/// *which* parent that is may change across a dynamic frame's history —
/// see [`crate::error`]'s module docs).
///
/// [`FrameRegistry`] interns names into dense [`FrameId`]s, and `frames`
/// (a `Vec<Option<FrameStore>>`, indexed by `FrameId`) is kept in lockstep
/// with it by `TransformBuffer::intern` (private), the only place either
/// grows.
///
/// # Two-layer cycle protection
///
/// [`TransformBuffer::set_transform`] runs a **proactive** check
/// (`would_create_cycle`, private) before an insertion that would become a
/// frame's *latest* answer, using each frame's current most-recent parent
/// — cheap, and catches the overwhelmingly common mistake (publishing an
/// edge that immediately closes a loop) at the moment it happens. It is
/// deliberately *not* exhaustive: a dynamic frame's parent can differ
/// across its history, so "does the graph have a cycle" is really a
/// per-query-time question, and checking it for every historical instant
/// on every insert would be both expensive and not actually what a
/// live-topology check should promise.
///
/// The **reactive** guard inside `walk_to_root` (private; a
/// `HashSet<FrameId>` of frames already visited in the current walk) is
/// what actually *guarantees* no lookup can loop forever, for any query
/// time, regardless of whether the proactive check ran or missed a
/// historical case. [`TransformBuffer::validate`] runs the same
/// latest-snapshot check as the proactive path, but over the whole graph
/// at once, for a caller that wants to audit connectivity without waiting
/// for a lookup to hit it.
///
/// # Time consistency for `Latest` lookups
///
/// A [`crate::time::TimePoint::Latest`] lookup that crosses more than one
/// dynamic edge does not let each edge report its own independently
/// freshest sample — see [`TimePoint::Latest`]'s own docs for why that
/// would risk composing a transform that never existed at any single real
/// instant. `latest_common_time` (private) resolves it to one shared
/// instant — the minimum, over every dynamic edge the walk touches, of
/// that edge's own newest sample, `tf2::BufferCore::getLatestCommonTime`'s
/// own algorithm — before `walk_to_root` runs at all, so every dynamic
/// edge a multi-hop `Latest` lookup touches is evaluated at that one
/// shared moment. This is the same best-effort, latest-parent-snapshot
/// topology resolution the proactive cycle check above uses, and carries
/// the identical caveat about a racing reparent.
#[derive(Debug, Clone)]
pub struct TransformBuffer {
    registry: FrameRegistry,
    frames: Vec<Option<FrameStore>>,
    max_history: Duration,
}

impl TransformBuffer {
    /// A new, empty buffer with the default ten-second history window.
    ///
    /// ```
    /// use astrs_tf::TransformBuffer;
    ///
    /// let buffer = TransformBuffer::new();
    /// assert!(!buffer.is_known_frame("map"));
    /// assert_eq!(buffer.max_history(), astrs_tf::buffer::DEFAULT_MAX_HISTORY);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::with_max_history(DEFAULT_MAX_HISTORY)
    }

    /// A new, empty buffer with a custom dynamic-frame history window.
    #[must_use]
    pub fn with_max_history(max_history: Duration) -> Self {
        Self {
            registry: FrameRegistry::new(),
            frames: Vec::new(),
            max_history,
        }
    }

    /// This buffer's configured history window for dynamic frames.
    #[must_use]
    pub fn max_history(&self) -> Duration {
        self.max_history
    }

    /// Interns `name`, growing `frames` in lockstep with the registry so
    /// the two never drift out of sync (see this module's own docs).
    fn intern(&mut self, name: &str) -> FrameId {
        let id = self.registry.intern(name);
        if self.frames.len() <= id.index() {
            self.frames.resize(id.index() + 1, None);
        }
        id
    }

    fn store(&self, id: FrameId) -> Option<&FrameStore> {
        self.frames.get(id.index()).and_then(Option::as_ref)
    }

    fn name_of(&self, id: FrameId) -> &str {
        self.registry.name(id).unwrap_or("<unknown>")
    }

    fn names_of(&self, ids: &[FrameId]) -> Vec<String> {
        ids.iter().map(|&id| self.name_of(id).to_owned()).collect()
    }

    /// Registers the transform from `parent` to `child`: `transform` maps a
    /// point expressed in `child`'s frame to the same point expressed in
    /// `parent`'s frame (tf2's own convention — see
    /// [`crate::math::isometry::Isometry3`]'s "Convention" docs).
    ///
    /// `transform.rotation` is normalized before storing (correcting the
    /// mild drift real publishers accumulate); `stamp` is ignored for a
    /// static frame beyond being recorded for introspection, and used for
    /// time-bracketing on a dynamic one.
    ///
    /// # Errors
    ///
    /// - [`TfError::NonFiniteTranslation`] / [`TfError::DegenerateQuaternion`]
    ///   for a transform that is not a legal rigid-body transform.
    /// - [`TfError::FrameKindConflict`] when `child` is already registered
    ///   as the other kind (static vs. dynamic) — see
    ///   [`TransformBuffer::remove_frame`] to deliberately re-kind a frame.
    /// - [`TfError::Cycle`] when this edge would immediately close a loop
    ///   under the current (latest) topology snapshot — see this struct's
    ///   "Two-layer cycle protection" docs for what this does and does not
    ///   catch.
    ///
    /// ```
    /// use astrs_tf::{TfStamp, TransformBuffer};
    /// use astrs_tf::math::Isometry3;
    ///
    /// # fn main() -> Result<(), astrs_tf::TfError> {
    /// let mut buffer = TransformBuffer::new();
    /// buffer.set_transform(
    ///     "map", "odom",
    ///     Isometry3::IDENTITY,
    ///     TfStamp::from_nanos(0),
    ///     true, // static: valid at any query time
    /// )?;
    /// assert!(buffer.is_known_frame("odom"));
    /// # Ok(())
    /// # }
    /// ```
    pub fn set_transform(
        &mut self,
        parent: &str,
        child: &str,
        transform: Isometry3,
        stamp: TfStamp,
        is_static: bool,
    ) -> Result<(), TfError> {
        if !transform.translation.is_finite() {
            return Err(TfError::NonFiniteTranslation {
                child: child.to_owned(),
            });
        }
        let rotation =
            transform
                .rotation
                .normalize()
                .ok_or_else(|| TfError::DegenerateQuaternion {
                    child: child.to_owned(),
                })?;
        let transform = Isometry3::new(transform.translation, rotation);

        let parent_id = self.intern(parent);
        let child_id = self.intern(child);
        let requested_kind = if is_static {
            FrameKind::Static
        } else {
            FrameKind::Dynamic
        };

        if let Some(existing) = self.store(child_id) {
            let existing_kind = existing.kind();
            if existing_kind != requested_kind {
                return Err(TfError::FrameKindConflict {
                    frame: child.to_owned(),
                    existing: existing_kind,
                    requested: requested_kind,
                });
            }
        }

        let becomes_latest = is_static
            || self
                .store(child_id)
                .and_then(FrameStore::latest_stamp)
                .is_none_or(|latest| stamp >= latest);
        if becomes_latest && let Some(cycle) = self.would_create_cycle(child_id, parent_id) {
            return Err(TfError::Cycle {
                path: self.names_of(&cycle),
            });
        }

        match self.frames[child_id.index()].as_mut() {
            None => {
                self.frames[child_id.index()] = Some(if is_static {
                    FrameStore::new_static(parent_id, transform, stamp)
                } else {
                    FrameStore::new_dynamic(parent_id, transform, stamp)
                });
            }
            Some(existing) if is_static => existing.overwrite_static(parent_id, transform, stamp),
            Some(existing) => {
                existing.insert_dynamic(parent_id, transform, stamp, self.max_history)
            }
        }
        Ok(())
    }

    /// Best-effort proactive cycle check: would giving `child_id` the
    /// parent `new_parent_id` close a loop, walking each frame's *current*
    /// latest parent? Returns the cyclic path (starting and ending at
    /// `child_id`) when it would.
    ///
    /// See this struct's "Two-layer cycle protection" docs for why this is
    /// deliberately not the only guard.
    fn would_create_cycle(
        &self,
        child_id: FrameId,
        new_parent_id: FrameId,
    ) -> Option<Vec<FrameId>> {
        let mut path = vec![child_id];
        let mut visited: HashSet<FrameId> = HashSet::from([child_id]);
        let mut current = new_parent_id;
        loop {
            path.push(current);
            if current == child_id {
                return Some(path);
            }
            if !visited.insert(current) {
                // Walked into some other, unrelated existing loop that does
                // not involve `child_id` — not this check's concern (it was
                // already a bug when *that* edge was inserted, and the
                // reactive guard in `walk_to_root` still protects any
                // lookup that reaches it).
                return None;
            }
            // Reaching a root frame (`?` returns `None`) means no cycle.
            current = self.store(current).and_then(FrameStore::latest_parent)?;
        }
    }

    /// Removes `name`'s record entirely (both static and dynamic), so a
    /// subsequent [`TransformBuffer::set_transform`] may register it as a
    /// different kind or under a different root — the escape hatch
    /// [`TfError::FrameKindConflict`]'s message refers to.
    ///
    /// `name` itself stays known to the registry (an already-interned
    /// [`FrameId`] is never reclaimed), so
    /// [`TransformBuffer::lookup_transform`] with `name` as an endpoint
    /// still resolves the name — it is simply rootless (like any other
    /// never-parented frame) until `set_transform` gives it a new record.
    ///
    /// Returns `true` if `name` had a record to remove.
    pub fn remove_frame(&mut self, name: &str) -> bool {
        let Some(id) = self.registry.get(name) else {
            return false;
        };
        match self.frames.get_mut(id.index()) {
            Some(slot) => slot.take().is_some(),
            None => false,
        }
    }

    /// Walks from `start` up to its root, accumulating the transform from
    /// each visited ancestor back to `start` at query time `time`.
    ///
    /// Returns `path[0] == (start, Isometry3::IDENTITY)`, and each
    /// subsequent `(ancestor, transform)` where `transform` maps a point in
    /// `start`'s frame to a point in `ancestor`'s frame — i.e.
    /// `path[k].1` is `T_{path[k].0}_{start}`.
    ///
    /// # Errors
    ///
    /// [`TfError::Cycle`] if the walk revisits a frame (the *reactive*
    /// guard — see this struct's own docs), or whatever
    /// [`super::record::FrameStore::resolve_hop`] returns for the hop that
    /// failed (naming that specific frame, not `start`).
    fn walk_to_root(
        &self,
        start: FrameId,
        time: TimePoint,
    ) -> Result<Vec<(FrameId, Isometry3)>, TfError> {
        let mut path = vec![(start, Isometry3::IDENTITY)];
        let mut visited: HashSet<FrameId> = HashSet::from([start]);
        let mut accumulated = Isometry3::IDENTITY;
        let mut current = start;
        loop {
            let Some(store) = self.store(current) else {
                return Ok(path); // `current` is a root: no parent record.
            };
            let (parent_id, hop) = store.resolve_hop(time, self.name_of(current))?;
            accumulated = hop.compose(accumulated);
            if !visited.insert(parent_id) {
                let mut ids: Vec<FrameId> = path.iter().map(|(id, _)| *id).collect();
                ids.push(parent_id);
                let first = ids.iter().position(|&id| id == parent_id).unwrap_or(0);
                return Err(TfError::Cycle {
                    path: self.names_of(&ids[first..]),
                });
            }
            path.push((parent_id, accumulated));
            current = parent_id;
        }
    }

    /// Looks up the transform from `source` to `target` at `time`:
    /// applying the result to a point expressed in `source`'s frame gives
    /// that point expressed in `target`'s frame — tf2's own
    /// `lookupTransform(target_frame, source_frame, time)` convention.
    ///
    /// `target == source` (as strings) always returns
    /// [`Isometry3::IDENTITY`] immediately, without even checking either
    /// name is registered — asking "where is X relative to X" is trivially
    /// true regardless.
    ///
    /// # Algorithm
    ///
    /// [`TimePoint::Latest`] is first resolved to a single shared instant —
    /// see `latest_common_time` (private) and this struct's "Time
    /// consistency for `Latest` lookups" docs — so every dynamic edge the
    /// walk touches is evaluated at the *same* moment rather than each
    /// reporting its own independently freshest sample. From there (and for
    /// any [`TimePoint::At`] query, which is already a single instant),
    /// this walks both `target` and `source` to their roots (`walk_to_root`,
    /// private) at that one time, then finds the lowest common ancestor by
    /// scanning `target`'s root-ward path for the first frame also present
    /// on `source`'s: `T_target_source = T_lca_target.inverse_unit() ∘
    /// T_lca_source`.
    ///
    /// # Errors
    ///
    /// - [`TfError::UnknownFrame`] when `target` or `source` was never
    ///   registered.
    /// - [`TfError::Disconnected`] when both are known but no common
    ///   ancestor exists (more than one root in the graph).
    /// - [`TfError::Cycle`] from the walk (`walk_to_root`, private).
    /// - [`TfError::Extrapolation`] from the walk, for *either*
    ///   [`TimePoint::At`] or [`TimePoint::Latest`] — a `Latest` query
    ///   spanning two or more dynamic edges with different "own latest"
    ///   bounds can still extrapolate if the shared instant it resolves to
    ///   falls before some edge's oldest buffered sample (matching
    ///   `tf2::BufferCore::getLatestCommonTime`'s own documented
    ///   limitation, not a new one introduced here). A `Latest` query
    ///   touching at most one dynamic edge never extrapolates, exactly as
    ///   before.
    ///
    /// ```
    /// use astrs_tf::{TfStamp, TimePoint, TransformBuffer};
    /// use astrs_tf::math::{Isometry3, Vector3};
    ///
    /// # fn main() -> Result<(), astrs_tf::TfError> {
    /// let mut buffer = TransformBuffer::new();
    /// buffer.set_transform(
    ///     "map", "odom",
    ///     Isometry3::from_translation(Vector3::new(10.0, 0.0, 0.0)),
    ///     TfStamp::from_nanos(0),
    ///     true,
    /// )?;
    ///
    /// // "odom" expressed in "map" is 10m on X; the reverse is -10m.
    /// let map_from_odom = buffer.lookup_transform("map", "odom", TimePoint::Latest)?;
    /// let odom_from_map = buffer.lookup_transform("odom", "map", TimePoint::Latest)?;
    /// assert_eq!(map_from_odom.translation, Vector3::new(10.0, 0.0, 0.0));
    /// assert_eq!(odom_from_map.translation, Vector3::new(-10.0, 0.0, 0.0));
    /// # Ok(())
    /// # }
    /// ```
    pub fn lookup_transform(
        &self,
        target: &str,
        source: &str,
        time: TimePoint,
    ) -> Result<Isometry3, TfError> {
        if target == source {
            return Ok(Isometry3::IDENTITY);
        }
        let target_id = self
            .registry
            .get(target)
            .ok_or_else(|| TfError::UnknownFrame {
                frame: target.to_owned(),
            })?;
        let source_id = self
            .registry
            .get(source)
            .ok_or_else(|| TfError::UnknownFrame {
                frame: source.to_owned(),
            })?;

        let time = match time {
            TimePoint::At(_) => time,
            TimePoint::Latest => self
                .latest_common_time(target_id, source_id)
                .map_or(TimePoint::Latest, TimePoint::At),
        };

        let source_chain = self.walk_to_root(source_id, time)?;
        let target_chain = self.walk_to_root(target_id, time)?;

        let source_map: HashMap<FrameId, Isometry3> = source_chain.into_iter().collect();
        for (ancestor_id, t_ancestor_target) in target_chain {
            if let Some(&t_ancestor_source) = source_map.get(&ancestor_id) {
                return Ok(t_ancestor_target.inverse_unit().compose(t_ancestor_source));
            }
        }
        Err(TfError::Disconnected {
            target_frame: target.to_owned(),
            source_frame: source.to_owned(),
        })
    }

    /// The shared instant a [`TimePoint::Latest`] lookup between `target_id`
    /// and `source_id` should resolve to — `tf2::BufferCore::
    /// getLatestCommonTime`'s own algorithm: the minimum, over every
    /// *dynamic* edge on the two frames' paths up to their lowest common
    /// ancestor, of that edge's own newest buffered sample. A static edge
    /// never constrains this (verified against `tf2::StaticCache::
    /// getLatestTimestamp`, which unconditionally reports "unconstrained"
    /// rather than its stored sample's timestamp — a static transform is
    /// valid at any time, so it cannot be the reason a shared instant needs
    /// to be held back).
    ///
    /// Returns `None` when no dynamic edge lies on the path (nothing
    /// constrains "latest" — an all-static chain, or `target`/`source`
    /// directly ancestor-related through only static edges — in which case
    /// every hop already answers identically at any instant, so
    /// [`TimePoint::Latest`] needs no resolution at all) **and** when
    /// `target_id`/`source_id` share no common ancestor at all (the walk
    /// that follows will report [`TfError::Disconnected`] on its own; this
    /// function does not need to anticipate that).
    ///
    /// Topology (which frames lie on the path, and where the two chains
    /// meet) is resolved via each frame's *current* latest parent — the
    /// same best-effort snapshot [`TransformBuffer::would_create_cycle`]
    /// uses, with the identical caveat: a reparent racing this computation
    /// could in principle pick a path slightly different from the one the
    /// subsequent time-aware walk actually takes. This mirrors a known,
    /// accepted limitation of real tf2's own `getLatestCommonTime` (which
    /// resolves topology via each `TimeCache`'s current parent the same
    /// way), not a shortcut invented here. Only edges strictly between an
    /// endpoint and the lowest common ancestor are considered — an edge
    /// beyond it (on the shared trunk toward the ultimate root) is never
    /// part of the transform this lookup actually composes, so it must not
    /// be allowed to hold the shared instant back either.
    fn latest_common_time(&self, target_id: FrameId, source_id: FrameId) -> Option<TfStamp> {
        let target_chain = self.latest_parent_chain(target_id);
        let source_chain = self.latest_parent_chain(source_id);
        let source_positions: HashMap<FrameId, usize> = source_chain
            .iter()
            .enumerate()
            .map(|(index, &id)| (id, index))
            .collect();
        let (target_lca_index, source_lca_index) = target_chain
            .iter()
            .enumerate()
            .find_map(|(index, id)| source_positions.get(id).map(|&other| (index, other)))?;

        let mut common_time: Option<TfStamp> = None;
        let mut fold_in = |id: FrameId| {
            let Some(store @ FrameStore::Dynamic(_)) = self.store(id) else {
                return;
            };
            if let Some(stamp) = store.latest_stamp() {
                common_time = Some(common_time.map_or(stamp, |current| current.min(stamp)));
            }
        };
        for &id in &target_chain[..target_lca_index] {
            fold_in(id);
        }
        for &id in &source_chain[..source_lca_index] {
            fold_in(id);
        }
        common_time
    }

    /// `start`'s ancestor chain (inclusive of `start` itself, as index `0`)
    /// followed via each frame's *current* latest parent, stopping at a
    /// root (no stored parent) or upon revisiting a frame already on the
    /// chain (a latent cycle — defensive only; [`TransformBuffer::
    /// walk_to_root`]'s reactive guard is what actually protects a real
    /// lookup from one, exactly as for [`TransformBuffer::would_create_cycle`]).
    /// Used only by [`TransformBuffer::latest_common_time`], which needs
    /// topology but not any particular resolved transform.
    fn latest_parent_chain(&self, start: FrameId) -> Vec<FrameId> {
        let mut chain = vec![start];
        let mut visited: HashSet<FrameId> = HashSet::from([start]);
        let mut current = start;
        while let Some(parent) = self.store(current).and_then(FrameStore::latest_parent) {
            if !visited.insert(parent) {
                break;
            }
            chain.push(parent);
            current = parent;
        }
        chain
    }

    /// `true` if [`TransformBuffer::lookup_transform`] would succeed —
    /// tf2's own `canTransform`.
    ///
    /// ```
    /// use astrs_tf::{TimePoint, TransformBuffer};
    ///
    /// let buffer = TransformBuffer::new();
    /// assert!(!buffer.can_transform("map", "odom", TimePoint::Latest));
    /// ```
    #[must_use]
    pub fn can_transform(&self, target: &str, source: &str, time: TimePoint) -> bool {
        self.lookup_transform(target, source, time).is_ok()
    }

    /// `true` when `name` has been registered by some
    /// [`TransformBuffer::set_transform`] call, as either a parent or a
    /// child.
    #[must_use]
    pub fn is_known_frame(&self, name: &str) -> bool {
        self.registry.get(name).is_some()
    }

    /// Every registered frame name, in registration order.
    pub fn frame_names(&self) -> impl Iterator<Item = &str> {
        self.registry.iter().map(|(_, name)| name)
    }

    /// `name`'s current parent frame, if it has one — `None` for an
    /// unregistered name *or* a registered root frame (a name only ever
    /// seen as somebody else's parent).
    #[must_use]
    pub fn parent_of(&self, name: &str) -> Option<&str> {
        let id = self.registry.get(name)?;
        let parent_id = self.store(id)?.latest_parent()?;
        self.registry.name(parent_id)
    }

    /// Every currently-registered frame whose latest parent is `name`.
    ///
    /// Returns an empty `Vec` for an unregistered `name` rather than
    /// [`TfError::UnknownFrame`] — this is an introspection query, not a
    /// lookup, and "no children" is a perfectly good answer for a frame
    /// nobody has claimed as a parent yet.
    #[must_use]
    pub fn children_of(&self, name: &str) -> Vec<&str> {
        let Some(parent_id) = self.registry.get(name) else {
            return Vec::new();
        };
        self.registry
            .iter()
            .filter(|&(id, _)| {
                self.store(id).and_then(FrameStore::latest_parent) == Some(parent_id)
            })
            .map(|(_, child_name)| child_name)
            .collect()
    }

    /// `name`'s [`FrameKind`] (static or dynamic), or `None` if it is
    /// unregistered or a rootless root frame.
    #[must_use]
    pub fn frame_kind(&self, name: &str) -> Option<FrameKind> {
        let id = self.registry.get(name)?;
        self.store(id).map(FrameStore::kind)
    }

    /// The `(oldest, newest)` buffered timestamps `name` can currently be
    /// queried at without [`TfError::Extrapolation`], if `name` is a
    /// registered *dynamic* frame with at least one sample.
    ///
    /// `None` for an unregistered name, a static frame (no bounded range —
    /// see [`TransformBuffer::set_transform`]'s docs), or a root frame.
    /// Lets a caller pick a query time it already knows is in range instead
    /// of discovering the buffer's bounds by trial and error against
    /// [`TransformBuffer::lookup_transform`].
    #[must_use]
    pub fn history_bounds(&self, name: &str) -> Option<(TfStamp, TfStamp)> {
        let id = self.registry.get(name)?;
        self.store(id)?.history_bounds()
    }

    /// Every registered frame with no record of its own — either never
    /// given a parent, or [`TransformBuffer::remove_frame`]d — in
    /// registration order.
    ///
    /// A frame forest may have more than one of these (see
    /// [`TransformBuffer::validate`]'s docs on why that is not itself an
    /// error); [`TransformBuffer::lookup_transform`] only fails when
    /// `target` and `source` land under *different* ones.
    pub fn root_frames(&self) -> impl Iterator<Item = &str> {
        self.registry
            .iter()
            .filter(|&(id, _)| self.store(id).is_none())
            .map(|(_, name)| name)
    }

    /// Validates the whole frame graph for cycles under the current
    /// (latest-parent) topology snapshot — the same check
    /// [`TransformBuffer::set_transform`] runs proactively per insertion,
    /// applied once to every registered frame rather than only the one
    /// just inserted.
    ///
    /// # Errors
    ///
    /// [`TfError::Cycle`] naming the cyclic path. Disconnection is not an
    /// error here — a frame forest with more than one root is completely
    /// normal (e.g. an unattached sensor frame awaiting calibration); only
    /// [`TransformBuffer::lookup_transform`] treats "no common ancestor"
    /// as a failure, because only a lookup actually needs one.
    pub fn validate(&self) -> Result<(), TfError> {
        let mut visited: HashSet<FrameId> = HashSet::new();
        let ids: Vec<FrameId> = self.registry.iter().map(|(id, _)| id).collect();
        for start in ids {
            if visited.contains(&start) {
                continue;
            }
            let mut on_path: Vec<FrameId> = Vec::new();
            self.validate_visit(start, &mut visited, &mut on_path)?;
        }
        Ok(())
    }

    fn validate_visit(
        &self,
        id: FrameId,
        visited: &mut HashSet<FrameId>,
        on_path: &mut Vec<FrameId>,
    ) -> Result<(), TfError> {
        if let Some(start_index) = on_path.iter().position(|&visited_id| visited_id == id) {
            let mut names = self.names_of(&on_path[start_index..]);
            names.push(self.name_of(id).to_owned());
            return Err(TfError::Cycle { path: names });
        }
        if visited.contains(&id) {
            return Ok(());
        }
        on_path.push(id);
        if let Some(parent) = self.store(id).and_then(FrameStore::latest_parent) {
            self.validate_visit(parent, visited, on_path)?;
        }
        on_path.pop();
        visited.insert(id);
        Ok(())
    }

    /// Renders the frame graph's current (latest-parent) topology snapshot
    /// as a Mermaid `flowchart` — this crate's answer to tf2's own
    /// `tf2_tools view_frames`, which renders the same tree as a PDF.
    /// Static edges render as solid arrows, dynamic ones as dashed with the
    /// buffered sample count labeled (`n=<count>`) — the two facts about an
    /// edge the buffer can state with certainty without picking a
    /// particular query time.
    ///
    /// A frame with a name that is not a bare identifier is rendered with
    /// its name as a quoted Mermaid node label rather than the node id
    /// itself, so arbitrary ROS frame names (which may contain `/`, as
    /// tf1-style namespaced frames do) never produce invalid Mermaid
    /// syntax.
    #[must_use]
    pub fn to_mermaid(&self) -> String {
        let mut out = String::from("flowchart TB\n");
        for (id, name) in self.registry.iter() {
            let Some(store) = self.store(id) else {
                continue;
            };
            let Some(parent_id) = store.latest_parent() else {
                continue;
            };
            let parent_node = mermaid_node(parent_id, self.name_of(parent_id));
            let child_node = mermaid_node(id, name);
            match store {
                FrameStore::Static(_) => {
                    out.push_str(&format!("    {parent_node} --> {child_node}\n"));
                }
                FrameStore::Dynamic(deque) => {
                    out.push_str(&format!(
                        "    {parent_node} -. \"n={}\" .-> {child_node}\n",
                        deque.len()
                    ));
                }
            }
        }
        out
    }
}

impl Default for TransformBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// A Mermaid flowchart node reference for `name`, used by
/// [`TransformBuffer::to_mermaid`].
///
/// `name` is used directly as the node id when it is already safe Mermaid
/// syntax (non-empty, ASCII alphanumeric/underscore); otherwise this emits
/// `id_<n>["<name>"]`, an id synthesized from `id`'s own dense index (never
/// a hash — `FrameId` already *is* a stable, unique small integer per
/// frame, so reusing it needs no extra machinery) paired with `name` as a
/// quoted display label, so a ROS frame name containing `/` or other
/// Mermaid-significant punctuation (legal in `.msg` `string` fields, even
/// if unusual in practice) still renders as valid Mermaid syntax. The `id_`
/// prefix keeps `n` from ever colliding with a *different* frame whose
/// literal name happens to be a bare decimal number.
fn mermaid_node(id: FrameId, name: &str) -> String {
    let is_safe_bare_id =
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if is_safe_bare_id {
        name.to_owned()
    } else {
        let escaped = name.replace('"', "'").replace(['[', ']'], "");
        format!("id_{}[\"{escaped}\"]", id.index())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::math::{Quaternion, Vector3};
    use proptest::prelude::*;

    fn translate(x: f64) -> Isometry3 {
        Isometry3::from_translation(Vector3::new(x, 0.0, 0.0))
    }

    fn at(nanos: i64) -> TimePoint {
        TimePoint::At(TfStamp::from_nanos(nanos))
    }

    #[test]
    fn identity_lookup_needs_no_registration() {
        let buffer = TransformBuffer::new();
        assert_eq!(
            buffer.lookup_transform("anything", "anything", TimePoint::Latest),
            Ok(Isometry3::IDENTITY)
        );
    }

    #[test]
    fn unknown_frame_is_a_typed_error() {
        let buffer = TransformBuffer::new();
        assert_eq!(
            buffer.lookup_transform("map", "odom", TimePoint::Latest),
            Err(TfError::UnknownFrame {
                frame: "map".to_owned()
            })
        );
    }

    #[test]
    fn static_frame_overrides_correctly() {
        // Mandated property (hand-written half): a static transform is
        // returned for any query time, including one before the frame was
        // ever set and one far in the future.
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform(
                "map",
                "odom",
                translate(5.0),
                TfStamp::from_nanos(1_000),
                true,
            )
            .unwrap();
        assert_eq!(
            buffer.lookup_transform("map", "odom", at(0)).unwrap(),
            translate(5.0)
        );
        assert_eq!(
            buffer
                .lookup_transform("map", "odom", at(i64::MAX))
                .unwrap(),
            translate(5.0)
        );
    }

    #[test]
    fn dynamic_frame_interpolates() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(0.0),
                TfStamp::from_nanos(0),
                false,
            )
            .unwrap();
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(10.0),
                TfStamp::from_nanos(1_000_000_000),
                false,
            )
            .unwrap();
        let mid = buffer
            .lookup_transform("odom", "base_link", at(500_000_000))
            .unwrap();
        assert_eq!(mid, translate(5.0));
    }

    #[test]
    fn lookup_at_exact_stamps_returns_exact_transforms() {
        // Mandated property (hand-written half).
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(1.0),
                TfStamp::from_nanos(0),
                false,
            )
            .unwrap();
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(2.0),
                TfStamp::from_nanos(1_000_000_000),
                false,
            )
            .unwrap();
        assert_eq!(
            buffer.lookup_transform("odom", "base_link", at(0)).unwrap(),
            translate(1.0)
        );
        assert_eq!(
            buffer
                .lookup_transform("odom", "base_link", at(1_000_000_000))
                .unwrap(),
            translate(2.0)
        );
    }

    #[test]
    fn extrapolation_outside_the_buffered_range_is_an_error() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(0.0),
                TfStamp::from_nanos(1_000),
                false,
            )
            .unwrap();
        assert!(matches!(
            buffer.lookup_transform("odom", "base_link", at(0)),
            Err(TfError::Extrapolation { .. })
        ));
    }

    #[test]
    fn two_hop_chain_composes() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform("map", "odom", translate(10.0), TfStamp::from_nanos(0), true)
            .unwrap();
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(5.0),
                TfStamp::from_nanos(0),
                true,
            )
            .unwrap();
        let result = buffer
            .lookup_transform("map", "base_link", TimePoint::Latest)
            .unwrap();
        assert_eq!(result, translate(15.0));
    }

    #[test]
    fn three_hop_chain_composes() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform("map", "odom", translate(1.0), TfStamp::from_nanos(0), true)
            .unwrap();
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(2.0),
                TfStamp::from_nanos(0),
                true,
            )
            .unwrap();
        buffer
            .set_transform(
                "base_link",
                "sensor",
                translate(4.0),
                TfStamp::from_nanos(0),
                true,
            )
            .unwrap();
        let result = buffer
            .lookup_transform("map", "sensor", TimePoint::Latest)
            .unwrap();
        assert_eq!(result, translate(7.0));
    }

    #[test]
    fn lookup_is_correctly_inverted() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform("map", "odom", translate(10.0), TfStamp::from_nanos(0), true)
            .unwrap();
        let forward = buffer
            .lookup_transform("odom", "map", TimePoint::Latest)
            .unwrap();
        assert_eq!(forward, translate(-10.0));
    }

    #[test]
    fn sibling_lookup_through_a_common_ancestor() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform("map", "left", translate(-1.0), TfStamp::from_nanos(0), true)
            .unwrap();
        buffer
            .set_transform("map", "right", translate(1.0), TfStamp::from_nanos(0), true)
            .unwrap();
        // left's origin sits at map-x = -1; right's origin sits at
        // map-x = +1. Expressed in "right", a point at left's origin
        // (map-x = -1) is at right-x = -1 - 1 = -2.
        let result = buffer
            .lookup_transform("right", "left", TimePoint::Latest)
            .unwrap();
        assert_eq!(result, translate(-2.0));
    }

    #[test]
    fn disconnected_trees_are_a_typed_error() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform("map", "odom", translate(0.0), TfStamp::from_nanos(0), true)
            .unwrap();
        buffer
            .set_transform(
                "other_root",
                "satellite",
                translate(0.0),
                TfStamp::from_nanos(0),
                true,
            )
            .unwrap();
        assert_eq!(
            buffer.lookup_transform("odom", "satellite", TimePoint::Latest),
            Err(TfError::Disconnected {
                target_frame: "odom".to_owned(),
                source_frame: "satellite".to_owned(),
            })
        );
    }

    #[test]
    fn a_direct_self_cycle_is_rejected_at_insert_time() {
        let mut buffer = TransformBuffer::new();
        let err = buffer
            .set_transform("a", "a", Isometry3::IDENTITY, TfStamp::from_nanos(0), true)
            .unwrap_err();
        assert!(matches!(err, TfError::Cycle { .. }));
    }

    #[test]
    fn a_two_frame_cycle_is_rejected_at_insert_time() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform("a", "b", translate(1.0), TfStamp::from_nanos(0), true)
            .unwrap();
        let err = buffer
            .set_transform("b", "a", translate(1.0), TfStamp::from_nanos(0), true)
            .unwrap_err();
        assert!(matches!(err, TfError::Cycle { .. }));
    }

    #[test]
    fn an_out_of_order_historical_cycle_is_still_caught_reactively() {
        // The proactive check only runs when an insertion becomes a
        // frame's *latest* answer. Give both "a" and "b" an established
        // history first (so a later insert into either is *not* latest and
        // skips the proactive check), then close a loop between them with
        // an old sample at a shared exact stamp (`ts(500)`) both sides can
        // resolve without needing interpolation or extrapolation — proving
        // the loop, not some other failure, is what `walk_to_root`'s
        // reactive guard catches.
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform(
                "root",
                "a",
                translate(1.0),
                TfStamp::from_nanos(5_000),
                false,
            )
            .unwrap();
        buffer
            .set_transform("a", "b", translate(1.0), TfStamp::from_nanos(5_000), false)
            .unwrap();
        buffer
            .set_transform("a", "b", translate(1.0), TfStamp::from_nanos(500), false)
            .unwrap();
        // Neither insertion above is a's latest (still ts(5_000), parent
        // "root"), so this one skips the proactive check.
        buffer
            .set_transform("b", "a", translate(1.0), TfStamp::from_nanos(500), false)
            .unwrap();
        let err = buffer.lookup_transform("root", "a", at(500));
        assert!(
            matches!(err, Err(TfError::Cycle { .. })),
            "expected a Cycle error, got {err:?}"
        );
    }

    #[test]
    fn frame_kind_conflict_is_rejected() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform("map", "odom", translate(0.0), TfStamp::from_nanos(0), true)
            .unwrap();
        let err = buffer
            .set_transform("map", "odom", translate(0.0), TfStamp::from_nanos(0), false)
            .unwrap_err();
        assert_eq!(
            err,
            TfError::FrameKindConflict {
                frame: "odom".to_owned(),
                existing: FrameKind::Static,
                requested: FrameKind::Dynamic,
            }
        );
    }

    #[test]
    fn remove_frame_clears_the_kind_conflict() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform("map", "odom", translate(0.0), TfStamp::from_nanos(0), true)
            .unwrap();
        assert!(buffer.remove_frame("odom"));
        buffer
            .set_transform("map", "odom", translate(1.0), TfStamp::from_nanos(0), false)
            .unwrap();
        assert_eq!(buffer.frame_kind("odom"), Some(FrameKind::Dynamic));
    }

    #[test]
    fn remove_frame_reports_whether_anything_was_removed() {
        let mut buffer = TransformBuffer::new();
        assert!(!buffer.remove_frame("nonexistent"));
        buffer
            .set_transform("map", "odom", translate(0.0), TfStamp::from_nanos(0), true)
            .unwrap();
        assert!(buffer.remove_frame("odom"));
        assert!(!buffer.remove_frame("odom"));
    }

    #[test]
    fn non_finite_translation_is_rejected() {
        let mut buffer = TransformBuffer::new();
        let bad = Isometry3::from_translation(Vector3::new(f64::NAN, 0.0, 0.0));
        assert_eq!(
            buffer.set_transform("map", "odom", bad, TfStamp::from_nanos(0), true),
            Err(TfError::NonFiniteTranslation {
                child: "odom".to_owned()
            })
        );
    }

    #[test]
    fn degenerate_quaternion_is_rejected() {
        let mut buffer = TransformBuffer::new();
        let bad = Isometry3::from_rotation(Quaternion::new(0.0, 0.0, 0.0, 0.0));
        assert_eq!(
            buffer.set_transform("map", "odom", bad, TfStamp::from_nanos(0), true),
            Err(TfError::DegenerateQuaternion {
                child: "odom".to_owned()
            })
        );
    }

    #[test]
    fn a_mildly_denormalized_quaternion_is_corrected_not_rejected() {
        let mut buffer = TransformBuffer::new();
        let drifted = Isometry3::from_rotation(Quaternion::new(0.0, 0.0, 0.0, 1.000_001));
        assert!(
            buffer
                .set_transform("map", "odom", drifted, TfStamp::from_nanos(0), true)
                .is_ok()
        );
    }

    #[test]
    fn can_transform_mirrors_lookup_success() {
        let mut buffer = TransformBuffer::new();
        assert!(!buffer.can_transform("map", "odom", TimePoint::Latest));
        buffer
            .set_transform("map", "odom", translate(0.0), TfStamp::from_nanos(0), true)
            .unwrap();
        assert!(buffer.can_transform("map", "odom", TimePoint::Latest));
    }

    #[test]
    fn introspection_reports_names_kinds_and_relations() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform("map", "odom", translate(0.0), TfStamp::from_nanos(0), true)
            .unwrap();
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(0.0),
                TfStamp::from_nanos(0),
                false,
            )
            .unwrap();

        assert!(buffer.is_known_frame("map"));
        assert!(!buffer.is_known_frame("nonexistent"));

        let mut names: Vec<&str> = buffer.frame_names().collect();
        names.sort_unstable();
        assert_eq!(names, ["base_link", "map", "odom"]);

        assert_eq!(buffer.parent_of("odom"), Some("map"));
        assert_eq!(buffer.parent_of("map"), None); // root
        assert_eq!(buffer.parent_of("nonexistent"), None);

        assert_eq!(buffer.children_of("map"), vec!["odom"]);
        assert!(buffer.children_of("nonexistent").is_empty());

        // "map" is only ever referenced as a *parent* — it has no store of
        // its own (this buffer keys storage by child frame; see this
        // module's docs), so it reports no kind, the same as any other
        // root frame.
        assert_eq!(buffer.frame_kind("map"), None);
        assert_eq!(buffer.frame_kind("odom"), Some(FrameKind::Static));
        assert_eq!(buffer.frame_kind("base_link"), Some(FrameKind::Dynamic));
    }

    #[test]
    fn history_bounds_is_none_for_unregistered_static_and_root_frames() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform("map", "odom", translate(0.0), TfStamp::from_nanos(0), true)
            .unwrap();
        assert_eq!(buffer.history_bounds("nonexistent"), None);
        assert_eq!(buffer.history_bounds("odom"), None); // static
        assert_eq!(buffer.history_bounds("map"), None); // root
    }

    #[test]
    fn history_bounds_matches_the_buffered_dynamic_range() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(0.0),
                TfStamp::from_nanos(0),
                false,
            )
            .unwrap();
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(1.0),
                TfStamp::from_nanos(1_000_000_000),
                false,
            )
            .unwrap();
        let (oldest, newest) = buffer.history_bounds("base_link").unwrap();
        assert_eq!(oldest, TfStamp::from_nanos(0));
        assert_eq!(newest, TfStamp::from_nanos(1_000_000_000));

        // Both bounds are queryable exactly; one step outside either is not.
        assert!(buffer.can_transform("odom", "base_link", TimePoint::At(oldest)));
        assert!(buffer.can_transform("odom", "base_link", TimePoint::At(newest)));
        assert!(!buffer.can_transform(
            "odom",
            "base_link",
            TimePoint::At(TfStamp::from_nanos(oldest.as_nanos() - 1))
        ));
    }

    #[test]
    fn root_frames_lists_every_frame_with_no_record_of_its_own() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform("map", "odom", translate(0.0), TfStamp::from_nanos(0), true)
            .unwrap();
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(0.0),
                TfStamp::from_nanos(0),
                true,
            )
            .unwrap();
        let mut roots: Vec<&str> = buffer.root_frames().collect();
        roots.sort_unstable();
        assert_eq!(roots, ["map"]);
    }

    #[test]
    fn root_frames_reports_every_disconnected_root() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform("map", "odom", translate(0.0), TfStamp::from_nanos(0), true)
            .unwrap();
        buffer
            .set_transform(
                "other_root",
                "satellite",
                translate(0.0),
                TfStamp::from_nanos(0),
                true,
            )
            .unwrap();
        let mut roots: Vec<&str> = buffer.root_frames().collect();
        roots.sort_unstable();
        assert_eq!(roots, ["map", "other_root"]);
    }

    #[test]
    fn root_frames_reflects_a_removed_frame() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform("map", "odom", translate(0.0), TfStamp::from_nanos(0), true)
            .unwrap();
        assert!(buffer.remove_frame("odom"));
        let roots: Vec<&str> = buffer.root_frames().collect();
        assert!(roots.contains(&"odom"));
    }

    #[test]
    fn to_mermaid_renders_static_and_dynamic_edges_distinctly() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform("map", "odom", translate(0.0), TfStamp::from_nanos(0), true)
            .unwrap();
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(0.0),
                TfStamp::from_nanos(0),
                false,
            )
            .unwrap();
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(1.0),
                TfStamp::from_nanos(1_000),
                false,
            )
            .unwrap();
        let mermaid = buffer.to_mermaid();
        assert!(mermaid.starts_with("flowchart TB\n"));
        assert!(mermaid.contains("map --> odom"));
        assert!(mermaid.contains("odom -. \"n=2\" .-> base_link"));
    }

    #[test]
    fn to_mermaid_quotes_a_frame_name_with_unsafe_characters() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform(
                "map",
                "laser/scan",
                translate(0.0),
                TfStamp::from_nanos(0),
                true,
            )
            .unwrap();
        // Referenced a second time, as a *parent* this time — proves the
        // same synthesized node id is reused rather than minted fresh per
        // occurrence, which would silently split it into two disconnected
        // Mermaid nodes for what is really one frame.
        buffer
            .set_transform(
                "laser/scan",
                "reflector",
                translate(0.0),
                TfStamp::from_nanos(0),
                true,
            )
            .unwrap();
        let mermaid = buffer.to_mermaid();
        // Interning order: "map" (id 0), "laser/scan" (id 1, first seen as
        // the first edge's child), "reflector" (id 2) — so "laser/scan"'s
        // synthesized node is deterministically `id_1["laser/scan"]`,
        // reused for both the edge that targets it and the edge it is the
        // source of.
        assert_eq!(
            mermaid.matches("id_1[\"laser/scan\"]").count(),
            2,
            "the same node id must appear once per edge endpoint: {mermaid}"
        );
    }

    #[test]
    fn to_mermaid_of_an_empty_buffer_is_just_the_header() {
        let buffer = TransformBuffer::new();
        assert_eq!(buffer.to_mermaid(), "flowchart TB\n");
    }

    #[test]
    fn validate_passes_for_a_tree() {
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform("map", "odom", translate(0.0), TfStamp::from_nanos(0), true)
            .unwrap();
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(0.0),
                TfStamp::from_nanos(0),
                true,
            )
            .unwrap();
        assert_eq!(buffer.validate(), Ok(()));
    }

    #[test]
    fn validate_passes_for_a_disconnected_forest() {
        // Multiple roots are not a validation failure — only a lookup that
        // needs a common ancestor cares.
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform("map", "odom", translate(0.0), TfStamp::from_nanos(0), true)
            .unwrap();
        buffer
            .set_transform(
                "other_root",
                "satellite",
                translate(0.0),
                TfStamp::from_nanos(0),
                true,
            )
            .unwrap();
        assert_eq!(buffer.validate(), Ok(()));
    }

    #[test]
    fn default_matches_new() {
        assert_eq!(
            TransformBuffer::default().max_history(),
            TransformBuffer::new().max_history()
        );
    }

    #[test]
    fn latest_lookup_across_two_dynamic_hops_uses_one_shared_instant() {
        // Mandated property: a `TimePoint::Latest` lookup that crosses more
        // than one dynamic edge must evaluate every dynamic edge at one
        // shared "latest common time" (the minimum of each edge's own
        // newest sample along the walked path) — `tf2::BufferCore::
        // getLatestCommonTime`'s real behavior — rather than letting each
        // hop independently report its own freshest sample, which can
        // compose transforms that never coexisted at any single instant.
        //
        // `odom -> base_link` has samples at t=0s and t=10s (latest: 10s).
        // `base_link -> sensor` has samples at t=0s and t=5s (latest: 5s).
        // The shared instant is min(10s, 5s) = 5s: `base_link` must be
        // interpolated *back* to 5s (it is not that hop's own latest),
        // while `sensor` lands exactly on its own latest sample.
        let mut buffer = TransformBuffer::new();
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(0.0),
                TfStamp::from_nanos(0),
                false,
            )
            .unwrap();
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(10.0),
                TfStamp::from_nanos(10_000_000_000),
                false,
            )
            .unwrap();
        buffer
            .set_transform(
                "base_link",
                "sensor",
                translate(0.0),
                TfStamp::from_nanos(0),
                false,
            )
            .unwrap();
        buffer
            .set_transform(
                "base_link",
                "sensor",
                translate(1.0),
                TfStamp::from_nanos(5_000_000_000),
                false,
            )
            .unwrap();

        let via_latest = buffer
            .lookup_transform("odom", "sensor", TimePoint::Latest)
            .unwrap();
        let via_explicit_shared_instant = buffer
            .lookup_transform("odom", "sensor", at(5_000_000_000))
            .unwrap();
        assert_eq!(via_latest, via_explicit_shared_instant);
        // Pinned to the expected value directly too, so this test does not
        // pass merely because both sides share one (possibly still wrong)
        // bug: base_link interpolated to 5s is translate(5.0), composed
        // with sensor's own exact-5s sample translate(1.0) -> 6.0.
        assert!((via_latest.translation.x - 6.0).abs() < 1e-9);
    }

    #[test]
    fn custom_history_window_is_honored() {
        let mut buffer = TransformBuffer::with_max_history(Duration::from_millis(500));
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(0.0),
                TfStamp::from_nanos(0),
                false,
            )
            .unwrap();
        buffer
            .set_transform(
                "odom",
                "base_link",
                translate(1.0),
                TfStamp::from_nanos(1_000_000_000), // 1s later: prunes ts(0).
                false,
            )
            .unwrap();
        assert!(matches!(
            buffer.lookup_transform("odom", "base_link", at(0)),
            Err(TfError::Extrapolation { .. })
        ));
    }

    proptest! {
        /// Looking up any frame to itself always yields identity, whether
        /// or not it has ever been registered.
        #[test]
        fn prop_self_lookup_is_always_identity(frame in "[a-z]{1,12}") {
            let buffer = TransformBuffer::new();
            prop_assert_eq!(
                buffer.lookup_transform(&frame, &frame, TimePoint::Latest),
                Ok(Isometry3::IDENTITY)
            );
        }

        /// A static frame returns the same transform at any query time,
        /// including timestamps nowhere near when it was set — the
        /// property-test half of "static frames override correctly."
        #[test]
        fn prop_static_frame_is_time_invariant(
            tx in -1000f64..1000.0, ty in -1000f64..1000.0, tz in -1000f64..1000.0,
            set_at in 0i64..1_000_000_000_000,
            query_at in 0i64..1_000_000_000_000,
        ) {
            let mut buffer = TransformBuffer::new();
            let transform = Isometry3::from_translation(Vector3::new(tx, ty, tz));
            buffer
                .set_transform("map", "odom", transform, TfStamp::from_nanos(set_at), true)
                .unwrap();
            let looked_up = buffer
                .lookup_transform("map", "odom", TimePoint::At(TfStamp::from_nanos(query_at)))
                .unwrap();
            prop_assert_eq!(looked_up, transform);
        }

        /// Interpolated lookups never leave the bracketing pair's
        /// translation interval — the buffer-level half of "interpolation
        /// stays inside its bracketing interval" (the math-level half
        /// lives in `math::isometry`'s own property test).
        #[test]
        fn prop_interpolated_lookup_stays_within_the_bracketing_interval(
            x0 in -100f64..100.0, x1 in -100f64..100.0,
            t_query in 1i64..999_999_999,
        ) {
            let mut buffer = TransformBuffer::new();
            buffer
                .set_transform("odom", "base_link", translate(x0), TfStamp::from_nanos(0), false)
                .unwrap();
            buffer
                .set_transform(
                    "odom",
                    "base_link",
                    translate(x1),
                    TfStamp::from_nanos(1_000_000_000),
                    false,
                )
                .unwrap();
            let mid = buffer
                .lookup_transform("odom", "base_link", at(t_query))
                .unwrap();
            let (lo, hi) = (x0.min(x1), x0.max(x1));
            prop_assert!(mid.translation.x >= lo - 1e-9 && mid.translation.x <= hi + 1e-9);
        }

        /// Exact-stamp lookups are exact, for any pair of distinct stamps
        /// and any translation values — the general form of the
        /// hand-written `lookup_at_exact_stamps_returns_exact_transforms`
        /// test above.
        #[test]
        fn prop_lookup_at_exact_stamps_is_exact(
            x0 in -1000f64..1000.0, x1 in -1000f64..1000.0,
            t0 in 0i64..500_000_000_000, gap in 1i64..500_000_000_000,
        ) {
            // This property is about exactness, not the history window —
            // `gap` ranges up to 500s, well past the default 10s buffer,
            // so pruning must not be allowed to interfere with it.
            let mut buffer = TransformBuffer::with_max_history(Duration::MAX);
            let t1 = t0 + gap;
            buffer
                .set_transform("odom", "base_link", translate(x0), TfStamp::from_nanos(t0), false)
                .unwrap();
            buffer
                .set_transform("odom", "base_link", translate(x1), TfStamp::from_nanos(t1), false)
                .unwrap();
            prop_assert_eq!(
                buffer.lookup_transform("odom", "base_link", TimePoint::At(TfStamp::from_nanos(t0))).unwrap(),
                translate(x0)
            );
            prop_assert_eq!(
                buffer.lookup_transform("odom", "base_link", TimePoint::At(TfStamp::from_nanos(t1))).unwrap(),
                translate(x1)
            );
        }
    }
}
