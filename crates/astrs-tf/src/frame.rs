//! [`FrameId`] and [`FrameRegistry`] — the frame-name string interner
//! [`crate::buffer::TransformBuffer`] is built on.
//!
//! tf2's own C++ implementation (`tf2::BufferCore`) interns frame names into
//! a `CompactFrameID` the same way: every buffer operation after the very
//! first mention of a frame name compares and indexes `u32`s rather than
//! hashing/comparing strings, which matters for a crate blueprint §11
//! places under real-time accommodations.

use std::collections::HashMap;

/// An interned frame name — a dense index into a [`FrameRegistry`].
///
/// Cheap to copy, compare and hash; never constructed directly by a
/// caller — [`FrameRegistry::intern`] and [`FrameRegistry::get`] are the
/// only ways to obtain one, so a `FrameId` is always valid for the
/// [`FrameRegistry`] that produced it (never for a different one — nothing
/// prevents mixing them, so callers holding more than one buffer should
/// treat `FrameId`s as buffer-scoped).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameId(u32);

impl FrameId {
    /// This id's dense index — the position [`FrameRegistry::name`] reads
    /// it back from.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// A bidirectional frame-name interner: `String ⇄ `[`FrameId`].
///
/// [`crate::buffer::TransformBuffer`] owns one and keeps a `Vec` of
/// per-frame records indexed in lockstep with it — every
/// [`FrameRegistry::intern`] call is immediately followed by pushing that
/// frame's record slot, so the two never drift out of sync (see
/// `TransformBuffer`'s own module docs).
#[derive(Debug, Clone, Default)]
pub struct FrameRegistry {
    /// Dense name storage, indexed by `FrameId::index()`.
    names: Vec<Box<str>>,
    /// The reverse lookup.
    ids: HashMap<Box<str>, FrameId>,
}

impl FrameRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            names: Vec::new(),
            ids: HashMap::new(),
        }
    }

    /// Returns `name`'s [`FrameId`], minting a new one if this is the first
    /// time `name` has been seen.
    ///
    /// # Panics-free overflow handling
    ///
    /// A registry holding `u32::MAX` frames already (over four billion —
    /// not a realistic frame-tree size for any physical robot) would have
    /// its `(u32::MAX)`th new name collide with index `u32::MAX` on the
    /// next call rather than panicking; this mirrors
    /// `astrs_idl::codegen::fields::fixed_size_i32`'s identical "saturate
    /// rather than fail, because there is no meaningful error to return
    /// and the bound is never approached in practice" reasoning.
    pub fn intern(&mut self, name: &str) -> FrameId {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = FrameId(u32::try_from(self.names.len()).unwrap_or(u32::MAX));
        self.names.push(Box::from(name));
        self.ids.insert(Box::from(name), id);
        id
    }

    /// Looks up `name`'s [`FrameId`] without interning it.
    ///
    /// This is the read-only half [`crate::buffer::TransformBuffer::lookup_transform`]
    /// uses: a lookup for a frame name that was never `intern`ed is a typo
    /// or a genuine unknown frame, and should report
    /// [`crate::error::TfError::UnknownFrame`] rather than silently
    /// minting a rootless frame that would make the typo look like a
    /// successful (if useless) query.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<FrameId> {
        self.ids.get(name).copied()
    }

    /// The name `id` was interned under.
    #[must_use]
    pub fn name(&self, id: FrameId) -> Option<&str> {
        self.names.get(id.index()).map(Box::as_ref)
    }

    /// The number of distinct frame names interned so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// `true` when no frame has been interned yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Every interned `(id, name)` pair, in interning order.
    pub fn iter(&self) -> impl Iterator<Item = (FrameId, &str)> {
        self.names
            .iter()
            .enumerate()
            .map(|(index, name)| (FrameId(index as u32), name.as_ref()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn interning_the_same_name_twice_returns_the_same_id() {
        let mut registry = FrameRegistry::new();
        let a = registry.intern("map");
        let b = registry.intern("map");
        assert_eq!(a, b);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn distinct_names_get_distinct_ids() {
        let mut registry = FrameRegistry::new();
        let map = registry.intern("map");
        let odom = registry.intern("odom");
        assert_ne!(map, odom);
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn get_does_not_intern() {
        let registry = FrameRegistry::new();
        assert_eq!(registry.get("map"), None);
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn name_reads_back_what_was_interned() {
        let mut registry = FrameRegistry::new();
        let id = registry.intern("base_link");
        assert_eq!(registry.name(id), Some("base_link"));
    }

    #[test]
    fn name_of_an_id_from_another_registry_is_out_of_range() {
        let mut a = FrameRegistry::new();
        let mut b = FrameRegistry::new();
        a.intern("map");
        let id_from_a = a.intern("odom");
        b.intern("only_frame");
        // `b` only has one frame; `id_from_a`'s index (1) is out of range.
        assert_eq!(b.name(id_from_a), None);
    }

    #[test]
    fn empty_registry_reports_empty() {
        let registry = FrameRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(FrameRegistry::default().len(), 0);
    }

    #[test]
    fn iter_yields_every_frame_in_interning_order() {
        let mut registry = FrameRegistry::new();
        registry.intern("map");
        registry.intern("odom");
        registry.intern("base_link");
        let names: Vec<&str> = registry.iter().map(|(_, name)| name).collect();
        assert_eq!(names, ["map", "odom", "base_link"]);
    }

    #[test]
    fn iter_pairs_match_get() {
        let mut registry = FrameRegistry::new();
        registry.intern("map");
        registry.intern("odom");
        for (id, name) in registry.iter() {
            assert_eq!(registry.get(name), Some(id));
        }
    }
}
