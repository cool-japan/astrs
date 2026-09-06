//! An insertion-ordered YAML mapping.
//!
//! A manifest's key order is part of its meaning to a human reading the
//! diff, so [`Mapping`] is a *sequence* of pairs with a hash index bolted on
//! the side rather than a `HashMap`. Iteration yields keys in the order the
//! document wrote them, and the emitter writes them back out in that same
//! order — which is what makes `parse(to_string(v)) == v` a statement about
//! bytes and not just about contents.
//!
//! # Why the index exists
//!
//! The parser must reject duplicate keys, so it performs one lookup per key
//! it reads. A plain `Vec` would make that O(n²) — a hostile document with
//! a few million keys in one mapping would take hours. Above
//! [`Mapping::LINEAR_SCAN_MAX`] entries the mapping therefore maintains an
//! open-addressed bucket table keyed by a per-mapping [`RandomState`], so
//! lookups are O(1) and an attacker cannot precompute collisions.
//!
//! Below that threshold — which is every mapping in every manifest in this
//! workspace — no table is allocated at all and lookups are a linear scan,
//! which is faster than hashing for a handful of short keys.
//!
//! # Equality is order-sensitive
//!
//! `{a: 1, b: 2}` and `{b: 2, a: 1}` are **not** equal. This is deliberate
//! and differs from `serde_yaml`, whose `Mapping` wraps an `IndexMap` and
//! compares as an unordered map. Order-sensitivity is what lets the
//! round-trip property test prove the emitter preserves order rather than
//! merely preserving contents.

use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hash, Hasher};

use serde::{Deserialize, Serialize};

use crate::value::Value;

/// Sentinel for an unused bucket slot.
const EMPTY: usize = usize::MAX;

/// An insertion-ordered mapping from [`Value`] to [`Value`].
///
/// # Examples
///
/// ```
/// use astrs_yaml::{Mapping, Value};
///
/// let mut mapping = Mapping::new();
/// mapping.insert(Value::from("name"), Value::from("perception"));
/// mapping.insert(Value::from("nodes"), Value::Sequence(vec![]));
///
/// assert_eq!(mapping.len(), 2);
/// assert_eq!(mapping.get_str("name"), Some(&Value::from("perception")));
/// assert_eq!(
///     mapping.keys().collect::<Vec<_>>(),
///     vec![&Value::from("name"), &Value::from("nodes")],
/// );
/// ```
#[derive(Clone)]
pub struct Mapping {
    entries: Vec<(Value, Value)>,
    buckets: Vec<usize>,
    state: RandomState,
}

impl Mapping {
    /// Entry count below which no hash table is built and lookups are a
    /// linear scan.
    pub const LINEAR_SCAN_MAX: usize = 8;

    /// An empty mapping.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            buckets: Vec::new(),
            state: RandomState::new(),
        }
    }

    /// An empty mapping with room for `capacity` entries.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
            buckets: Vec::new(),
            state: RandomState::new(),
        }
    }

    /// How many key/value pairs this mapping holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the mapping holds no pairs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Remove every pair, keeping the allocated capacity.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.buckets.clear();
    }

    /// Insert `key => value`, returning the previous value for `key`.
    ///
    /// Re-inserting an existing key keeps that key's original *position*,
    /// matching `IndexMap` and every other insertion-ordered map: only the
    /// value changes.
    pub fn insert(&mut self, key: Value, value: Value) -> Option<Value> {
        let hash = self.hash_of(&key);
        if let Some(slot) = self.find(hash, &key) {
            let previous = std::mem::replace(&mut self.entries[slot].1, value);
            return Some(previous);
        }
        let slot = self.entries.len();
        self.entries.push((key, value));
        self.record(hash, slot);
        None
    }

    /// The value stored under `key`.
    #[must_use]
    pub fn get(&self, key: &Value) -> Option<&Value> {
        let hash = self.hash_of(key);
        self.find(hash, key).map(|slot| &self.entries[slot].1)
    }

    /// A mutable reference to the value stored under `key`.
    pub fn get_mut(&mut self, key: &Value) -> Option<&mut Value> {
        let hash = self.hash_of(key);
        self.find(hash, key).map(|slot| &mut self.entries[slot].1)
    }

    /// The value stored under the string key `key`, without allocating a
    /// [`Value`] to look it up with.
    ///
    /// Every key in a dataflow manifest is a string, so this is the lookup
    /// that actually runs in production.
    #[must_use]
    pub fn get_str(&self, key: &str) -> Option<&Value> {
        let hash = self.hash_of_str(key);
        self.find_str(hash, key).map(|slot| &self.entries[slot].1)
    }

    /// True when `key` is present.
    #[must_use]
    pub fn contains_key(&self, key: &Value) -> bool {
        self.get(key).is_some()
    }

    /// Remove `key`, returning its value.
    ///
    /// Order-preserving, and therefore O(n): the remaining entries shift
    /// down and the hash index is rebuilt. Removal is a caller-driven edit,
    /// never something the parser does, so the cost is paid where it is
    /// visible.
    pub fn remove(&mut self, key: &Value) -> Option<Value> {
        let hash = self.hash_of(key);
        let slot = self.find(hash, key)?;
        let (_, value) = self.entries.remove(slot);
        self.rebuild();
        Some(value)
    }

    /// Iterate over `(key, value)` pairs in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&Value, &Value)> {
        self.entries.iter().map(|(key, value)| (key, value))
    }

    /// Iterate over `(key, &mut value)` pairs in insertion order.
    ///
    /// Keys are handed out immutably: mutating one would invalidate the
    /// hash index without the mapping being able to notice.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&Value, &mut Value)> {
        self.entries.iter_mut().map(|(key, value)| (&*key, value))
    }

    /// Iterate over keys in insertion order.
    pub fn keys(&self) -> impl Iterator<Item = &Value> {
        self.entries.iter().map(|(key, _)| key)
    }

    /// Iterate over values in insertion order.
    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.entries.iter().map(|(_, value)| value)
    }

    /// The `index`-th pair in insertion order.
    #[must_use]
    pub fn get_index(&self, index: usize) -> Option<(&Value, &Value)> {
        self.entries.get(index).map(|(key, value)| (key, value))
    }

    fn hash_of(&self, key: &Value) -> u64 {
        self.state.hash_one(key)
    }

    /// The hash a `Value::String(key)` would produce, computed straight from
    /// the `&str` so [`Mapping::get_str`] never allocates.
    fn hash_of_str(&self, key: &str) -> u64 {
        let mut hasher = self.state.build_hasher();
        Value::hash_str_key(key, &mut hasher);
        hasher.finish()
    }

    fn find(&self, hash: u64, key: &Value) -> Option<usize> {
        self.probe(hash, |candidate| candidate == key)
    }

    fn find_str(&self, hash: u64, key: &str) -> Option<usize> {
        self.probe(hash, |candidate| match candidate {
            Value::String(text) => text == key,
            _ => false,
        })
    }

    fn probe(&self, hash: u64, matches: impl Fn(&Value) -> bool) -> Option<usize> {
        if self.buckets.is_empty() {
            return self
                .entries
                .iter()
                .position(|(candidate, _)| matches(candidate));
        }
        let mask = self.buckets.len() - 1;
        let mut bucket = (hash as usize) & mask;
        loop {
            let slot = self.buckets[bucket];
            if slot == EMPTY {
                return None;
            }
            if matches(&self.entries[slot].0) {
                return Some(slot);
            }
            bucket = (bucket + 1) & mask;
        }
    }

    /// Record that `slot` (already pushed onto `entries`) hashes to `hash`,
    /// growing or first building the table when the load factor demands it.
    fn record(&mut self, hash: u64, slot: usize) {
        if self.buckets.is_empty() {
            if self.entries.len() <= Self::LINEAR_SCAN_MAX {
                return;
            }
            self.rebuild();
            return;
        }
        // Keep the table at most half full: linear probing degrades sharply
        // past that, and the table is cheap relative to the entries.
        if self.entries.len() * 2 > self.buckets.len() {
            self.rebuild();
            return;
        }
        self.place(hash, slot);
    }

    fn place(&mut self, hash: u64, slot: usize) {
        let mask = self.buckets.len() - 1;
        let mut bucket = (hash as usize) & mask;
        while self.buckets[bucket] != EMPTY {
            bucket = (bucket + 1) & mask;
        }
        self.buckets[bucket] = slot;
    }

    /// Discard and re-create the bucket table from `entries`.
    fn rebuild(&mut self) {
        if self.entries.len() <= Self::LINEAR_SCAN_MAX {
            self.buckets.clear();
            return;
        }
        let size = (self.entries.len() * 4).next_power_of_two().max(32);
        self.buckets.clear();
        self.buckets.resize(size, EMPTY);
        for slot in 0..self.entries.len() {
            let hash = self.hash_of(&self.entries[slot].0);
            self.place(hash, slot);
        }
    }
}

impl Default for Mapping {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Mapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl PartialEq for Mapping {
    /// Order-sensitive: see the module documentation.
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl Eq for Mapping {}

impl Hash for Mapping {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.entries.len().hash(state);
        for entry in &self.entries {
            entry.hash(state);
        }
    }
}

impl FromIterator<(Value, Value)> for Mapping {
    fn from_iter<I: IntoIterator<Item = (Value, Value)>>(iter: I) -> Self {
        let iter = iter.into_iter();
        let (lower, _) = iter.size_hint();
        let mut mapping = Self::with_capacity(lower);
        for (key, value) in iter {
            mapping.insert(key, value);
        }
        mapping
    }
}

impl Extend<(Value, Value)> for Mapping {
    fn extend<I: IntoIterator<Item = (Value, Value)>>(&mut self, iter: I) {
        for (key, value) in iter {
            self.insert(key, value);
        }
    }
}

impl IntoIterator for Mapping {
    type Item = (Value, Value);
    type IntoIter = std::vec::IntoIter<(Value, Value)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<'a> IntoIterator for &'a Mapping {
    type Item = (&'a Value, &'a Value);
    type IntoIter = std::iter::Map<
        std::slice::Iter<'a, (Value, Value)>,
        fn(&'a (Value, Value)) -> (&'a Value, &'a Value),
    >;

    fn into_iter(self) -> Self::IntoIter {
        fn split(entry: &(Value, Value)) -> (&Value, &Value) {
            (&entry.0, &entry.1)
        }
        self.entries.iter().map(split)
    }
}

impl Serialize for Mapping {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.len()))?;
        for (key, value) in self.iter() {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Mapping {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MappingVisitor;

        impl<'de> serde::de::Visitor<'de> for MappingVisitor {
            type Value = Mapping;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a YAML mapping")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut access: A,
            ) -> Result<Mapping, A::Error> {
                let mut mapping = Mapping::with_capacity(access.size_hint().unwrap_or(0));
                while let Some((key, value)) = access.next_entry()? {
                    mapping.insert(key, value);
                }
                Ok(mapping)
            }

            fn visit_unit<E>(self) -> Result<Mapping, E> {
                Ok(Mapping::new())
            }
        }

        deserializer.deserialize_map(MappingVisitor)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn key(text: &str) -> Value {
        Value::from(text)
    }

    #[test]
    fn insertion_order_is_iteration_order() {
        let mut mapping = Mapping::new();
        for name in ["z", "a", "m"] {
            mapping.insert(key(name), Value::from(1u8));
        }
        let seen: Vec<&str> = mapping.keys().filter_map(|value| value.as_str()).collect();
        assert_eq!(seen, vec!["z", "a", "m"]);
    }

    #[test]
    fn reinserting_a_key_keeps_its_position_and_returns_the_old_value() {
        let mut mapping = Mapping::new();
        mapping.insert(key("a"), Value::from(1u8));
        mapping.insert(key("b"), Value::from(2u8));
        let previous = mapping.insert(key("a"), Value::from(9u8));
        assert_eq!(previous, Some(Value::from(1u8)));
        assert_eq!(mapping.len(), 2);
        assert_eq!(mapping.get_index(0).map(|(k, _)| k), Some(&key("a")));
        assert_eq!(mapping.get_str("a"), Some(&Value::from(9u8)));
    }

    #[test]
    fn lookups_work_below_and_above_the_indexing_threshold() {
        let mut mapping = Mapping::new();
        let count = Mapping::LINEAR_SCAN_MAX * 8;
        for i in 0..count {
            mapping.insert(key(&format!("k{i}")), Value::from(i as u64));
        }
        assert_eq!(mapping.len(), count);
        for i in 0..count {
            assert_eq!(
                mapping.get_str(&format!("k{i}")),
                Some(&Value::from(i as u64)),
                "k{i}"
            );
            assert!(mapping.contains_key(&key(&format!("k{i}"))));
        }
        assert_eq!(mapping.get_str("absent"), None);
        assert!(!mapping.buckets.is_empty(), "index should be built");
    }

    #[test]
    fn a_small_mapping_allocates_no_bucket_table() {
        let mut mapping = Mapping::new();
        for i in 0..Mapping::LINEAR_SCAN_MAX {
            mapping.insert(key(&format!("k{i}")), Value::Null);
        }
        assert!(mapping.buckets.is_empty());
        mapping.insert(key("one-more"), Value::Null);
        assert!(!mapping.buckets.is_empty());
    }

    #[test]
    fn get_str_agrees_with_get_on_a_value_key() {
        let mut mapping = Mapping::new();
        for i in 0..40 {
            mapping.insert(key(&format!("k{i}")), Value::from(i as u64));
        }
        for i in 0..40 {
            let name = format!("k{i}");
            assert_eq!(mapping.get_str(&name), mapping.get(&key(&name)));
        }
        // A non-string key must not be found by the string fast path.
        mapping.insert(Value::from(7u8), Value::from("int"));
        assert_eq!(mapping.get_str("7"), None);
        assert_eq!(mapping.get(&Value::from(7u8)), Some(&Value::from("int")));
    }

    #[test]
    fn removal_preserves_order_and_keeps_the_index_valid() {
        let mut mapping = Mapping::new();
        for i in 0..40 {
            mapping.insert(key(&format!("k{i}")), Value::from(i as u64));
        }
        assert_eq!(mapping.remove(&key("k0")), Some(Value::from(0u64)));
        assert_eq!(mapping.remove(&key("k39")), Some(Value::from(39u64)));
        assert_eq!(mapping.remove(&key("k0")), None);
        assert_eq!(mapping.len(), 38);
        assert_eq!(mapping.get_index(0).map(|(k, _)| k), Some(&key("k1")));
        for i in 1..39 {
            assert_eq!(
                mapping.get_str(&format!("k{i}")),
                Some(&Value::from(i as u64))
            );
        }
    }

    #[test]
    fn equality_is_order_sensitive() {
        let forward: Mapping = [(key("a"), Value::from(1u8)), (key("b"), Value::from(2u8))]
            .into_iter()
            .collect();
        let backward: Mapping = [(key("b"), Value::from(2u8)), (key("a"), Value::from(1u8))]
            .into_iter()
            .collect();
        assert_ne!(forward, backward);
        assert_eq!(forward, forward.clone());
    }

    #[test]
    fn a_clone_keeps_a_usable_index() {
        let mut mapping = Mapping::new();
        for i in 0..64 {
            mapping.insert(key(&format!("k{i}")), Value::from(i as u64));
        }
        let clone = mapping.clone();
        for i in 0..64 {
            assert_eq!(
                clone.get_str(&format!("k{i}")),
                Some(&Value::from(i as u64))
            );
        }
    }

    #[test]
    fn mutation_helpers_reach_the_stored_values() {
        let mut mapping = Mapping::new();
        mapping.insert(key("a"), Value::from(1u8));
        if let Some(slot) = mapping.get_mut(&key("a")) {
            *slot = Value::from(2u8);
        }
        assert_eq!(mapping.get_str("a"), Some(&Value::from(2u8)));
        for (_, value) in mapping.iter_mut() {
            *value = Value::Null;
        }
        assert_eq!(mapping.get_str("a"), Some(&Value::Null));
        mapping.clear();
        assert!(mapping.is_empty());
        assert_eq!(mapping.get_str("a"), None);
    }

    #[test]
    fn extend_and_into_iter_round_trip() {
        let mut mapping = Mapping::with_capacity(2);
        mapping.extend([(key("a"), Value::Null), (key("b"), Value::Null)]);
        let pairs: Vec<(Value, Value)> = mapping.clone().into_iter().collect();
        assert_eq!(pairs.len(), 2);
        let borrowed: Vec<(&Value, &Value)> = (&mapping).into_iter().collect();
        assert_eq!(borrowed.len(), 2);
        assert_eq!(mapping.values().count(), 2);
        assert!(format!("{mapping:?}").contains('a'));
    }
}
