//! [`Metadata`] — the struct that rides beside every payload.
//!
//! Blueprint §6.1: *"Metadata rides beside payloads as an oxicode-encoded
//! struct (never inside the Arrow stream): `{version: u16, timestamp: Hlc,
//! parameters: BTreeMap<Key, Parameter>}` with well-known keys `request_id,
//! goal_id, goal_status, session_id, segment_id, seq, fin, flush` and internal
//! keys `_schema_hash`, stripped before user delivery."*
//!
//! Three consequences of that one sentence shape this module:
//!
//! - **Beside, not inside.** Metadata is a separate oxicode struct, so a
//!   receiver can read the correlation keys without touching the columnar
//!   payload — which is what lets the scheduler grant queue-eviction immunity
//!   (§11.2) before anything is decoded.
//! - **`BTreeMap`, not `HashMap`.** Ordered iteration makes the encoding
//!   deterministic, which is what makes recordings replayable byte-for-byte
//!   (§14) and the protocol snapshot stable.
//! - **Internal keys are stripped.** [`Metadata::strip_internal`] removes the
//!   `_`-prefixed plumbing before an event reaches user code.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{GoalStatus, Metadata, Parameter};
//! use astrs_time::HlcTimestamp;
//!
//! let mut meta = Metadata::new(HlcTimestamp::new(1_700_000_000_000_000_000, 0));
//! meta.set_request_id("req-42");
//! meta.set_goal_status(GoalStatus::Executing);
//! meta.insert_schema_hash("d0f9a1c3");
//!
//! assert!(meta.is_correlated());
//! assert_eq!(meta.request_id(), Some("req-42"));
//! assert_eq!(meta.goal_status(), Some(GoalStatus::Executing));
//!
//! // What user code sees: no `_`-prefixed plumbing.
//! let delivered = meta.stripped();
//! assert_eq!(delivered.schema_hash(), None);
//! assert_eq!(delivered.request_id(), Some("req-42"));
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

pub mod keys;
pub mod parameter;

use std::collections::BTreeMap;
use std::collections::btree_map::{Entry, Iter, Keys, Values};

use astrs_time::HlcTimestamp;
use oxicode::de::{Decode, Decoder};
use oxicode::enc::{Encode, Encoder};
use serde::{Deserialize, Serialize};

use crate::error::codec_invalid;
use crate::ids::ParamKey;

pub use keys::GoalStatus;
pub use parameter::Parameter;

/// The metadata layout version this build writes.
///
/// Carried in every [`Metadata`] so a receiver can tell an old sender's
/// metadata from a new one's without consulting the connection's protocol
/// version — metadata outlives connections, because it is written into
/// recordings (§14).
pub const METADATA_VERSION: u16 = 1;

/// The largest number of parameters accepted off the wire.
///
/// Metadata is a small correlation record, not a document store. The frame
/// limit already bounds the total bytes; this bounds the *shape*, so a peer
/// cannot force a receiver to build a million-entry map inside one legal
/// frame.
pub const MAX_PARAMETERS: usize = 1024;

/// The metadata accompanying one message.
///
/// Deliberately **not** `#[non_exhaustive]`: `astrs-daemon`, `astrs-node-api`
/// and the recorder all build these with struct literals, and the field set is
/// pinned by the blueprint.
///
/// # Examples
///
/// ```
/// use astrs_wire::{Metadata, Parameter};
/// use astrs_time::HlcTimestamp;
///
/// let meta = Metadata::new(HlcTimestamp::new(10, 0))
///     .with("seq", Parameter::Integer(3))?
///     .with("fin", Parameter::Bool(true))?;
/// assert_eq!(meta.seq(), Some(3));
/// assert_eq!(meta.fin(), Some(true));
/// assert_eq!(meta.len(), 2);
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    /// The metadata layout version — [`METADATA_VERSION`] for anything this
    /// build produces.
    pub version: u16,
    /// The hybrid-logical-clock timestamp of the event this metadata
    /// describes (blueprint §4.3).
    pub timestamp: HlcTimestamp,
    /// The key/value parameters, in key order.
    pub parameters: BTreeMap<ParamKey, Parameter>,
}

impl Metadata {
    /// An empty metadata record stamped at `timestamp`.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{METADATA_VERSION, Metadata};
    /// use astrs_time::HlcTimestamp;
    ///
    /// let meta = Metadata::new(HlcTimestamp::new(1, 2));
    /// assert_eq!(meta.version, METADATA_VERSION);
    /// assert!(meta.is_empty());
    /// ```
    #[must_use]
    pub fn new(timestamp: HlcTimestamp) -> Self {
        Self {
            version: METADATA_VERSION,
            timestamp,
            parameters: BTreeMap::new(),
        }
    }

    /// Builder form of [`Metadata::insert`].
    ///
    /// # Errors
    ///
    /// [`crate::IdError`] if `key` is not a valid [`ParamKey`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Metadata, Parameter};
    /// use astrs_time::HlcTimestamp;
    ///
    /// let meta = Metadata::new(HlcTimestamp::EPOCH).with("k", Parameter::Bool(true))?;
    /// assert_eq!(meta.get("k").and_then(Parameter::as_bool), Some(true));
    /// assert!(Metadata::new(HlcTimestamp::EPOCH).with("bad key", Parameter::Bool(true)).is_err());
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    pub fn with(mut self, key: &str, value: impl Into<Parameter>) -> Result<Self, crate::IdError> {
        self.insert(key, value)?;
        Ok(self)
    }

    /// Inserts a parameter, returning the value it replaced.
    ///
    /// # Errors
    ///
    /// [`crate::IdError`] if `key` is not a valid [`ParamKey`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Metadata, Parameter};
    /// use astrs_time::HlcTimestamp;
    ///
    /// let mut meta = Metadata::new(HlcTimestamp::EPOCH);
    /// assert_eq!(meta.insert("seq", Parameter::Integer(1))?, None);
    /// assert_eq!(meta.insert("seq", Parameter::Integer(2))?, Some(Parameter::Integer(1)));
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    pub fn insert(
        &mut self,
        key: &str,
        value: impl Into<Parameter>,
    ) -> Result<Option<Parameter>, crate::IdError> {
        Ok(self.parameters.insert(ParamKey::new(key)?, value.into()))
    }

    /// Inserts a parameter under an already-validated key.
    ///
    /// The infallible counterpart of [`Metadata::insert`], for call sites that
    /// hold a [`ParamKey`].
    pub fn insert_key(&mut self, key: ParamKey, value: impl Into<Parameter>) -> Option<Parameter> {
        self.parameters.insert(key, value.into())
    }

    /// Looks up a parameter by key.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Metadata, Parameter};
    /// use astrs_time::HlcTimestamp;
    ///
    /// let meta = Metadata::new(HlcTimestamp::EPOCH).with("a", Parameter::Integer(1))?;
    /// assert!(meta.get("a").is_some());
    /// assert!(meta.get("b").is_none());
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Parameter> {
        self.parameters.get(key)
    }

    /// Removes a parameter, returning it.
    pub fn remove(&mut self, key: &str) -> Option<Parameter> {
        self.parameters.remove(key)
    }

    /// Whether a parameter with this key is present.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.parameters.contains_key(key)
    }

    /// The entry API for the parameter map, for read-modify-write updates.
    pub fn entry(&mut self, key: ParamKey) -> Entry<'_, ParamKey, Parameter> {
        self.parameters.entry(key)
    }

    /// The number of parameters.
    #[must_use]
    pub fn len(&self) -> usize {
        self.parameters.len()
    }

    /// Whether there are no parameters.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parameters.is_empty()
    }

    /// Iterates the parameters in key order.
    pub fn iter(&self) -> Iter<'_, ParamKey, Parameter> {
        self.parameters.iter()
    }

    /// Iterates the keys in order.
    pub fn keys(&self) -> Keys<'_, ParamKey, Parameter> {
        self.parameters.keys()
    }

    /// Iterates the values in key order.
    pub fn values(&self) -> Values<'_, ParamKey, Parameter> {
        self.parameters.values()
    }

    /// Removes every AstRS-internal (`_`-prefixed) parameter in place.
    ///
    /// Called by the node API immediately before an `NodeEvent::Input`
    /// is handed to user code, so application code never sees the plumbing
    /// (blueprint §6.1).
    ///
    /// Returns the number of parameters removed.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Metadata, Parameter};
    /// use astrs_time::HlcTimestamp;
    ///
    /// let mut meta = Metadata::new(HlcTimestamp::EPOCH)
    ///     .with("seq", Parameter::Integer(1))?
    ///     .with("_schema_hash", Parameter::String("abc".into()))?
    ///     .with("_trace", Parameter::String("t".into()))?;
    ///
    /// assert_eq!(meta.strip_internal(), 2);
    /// assert_eq!(meta.len(), 1);
    /// assert!(meta.contains_key("seq"));
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    pub fn strip_internal(&mut self) -> usize {
        let before = self.parameters.len();
        self.parameters.retain(|key, _| !key.is_internal());
        before - self.parameters.len()
    }

    /// A copy with the internal parameters removed.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Metadata, Parameter};
    /// use astrs_time::HlcTimestamp;
    ///
    /// let meta = Metadata::new(HlcTimestamp::EPOCH)
    ///     .with("_schema_hash", Parameter::String("abc".into()))?;
    /// assert!(meta.stripped().is_empty());
    /// assert_eq!(meta.len(), 1, "the original is untouched");
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn stripped(&self) -> Self {
        let mut copy = self.clone();
        copy.strip_internal();
        copy
    }

    /// Whether this message is *correlated* — carries a `request_id`,
    /// `goal_id` or `goal_status`.
    ///
    /// Blueprint §11.2: correlated messages are immune to queue eviction,
    /// because dropping a service response or a goal-status update would wedge
    /// a client forever.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Metadata, Parameter};
    /// use astrs_time::HlcTimestamp;
    ///
    /// let plain = Metadata::new(HlcTimestamp::EPOCH).with("seq", Parameter::Integer(1))?;
    /// assert!(!plain.is_correlated());
    ///
    /// let correlated = plain.clone().with("request_id", Parameter::String("r".into()))?;
    /// assert!(correlated.is_correlated());
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn is_correlated(&self) -> bool {
        keys::CORRELATION
            .iter()
            .any(|key| self.parameters.contains_key(*key))
    }

    /// Whether this message is part of a stream (`session_id` present).
    #[must_use]
    pub fn is_stream_chunk(&self) -> bool {
        self.contains_key(keys::SESSION_ID)
    }

    /// Derives the metadata for a message produced *in response to* this one.
    ///
    /// This is the `meta.follow()` of the node API example in blueprint §9.1.
    /// It preserves exactly the keys that must survive a hop — the correlation
    /// keys and the stream keys — and drops everything else, including all
    /// internal plumbing, which the sender will re-stamp for its own payload.
    /// The timestamp is inherited so that causality is visible in a recording;
    /// callers with a live clock should overwrite it with a fresh reading.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Metadata, Parameter};
    /// use astrs_time::HlcTimestamp;
    ///
    /// let incoming = Metadata::new(HlcTimestamp::new(5, 0))
    ///     .with("request_id", Parameter::String("r-1".into()))?
    ///     .with("seq", Parameter::Integer(9))?
    ///     .with("user_note", Parameter::String("ignored".into()))?
    ///     .with("_schema_hash", Parameter::String("h".into()))?;
    ///
    /// let outgoing = incoming.follow();
    /// assert_eq!(outgoing.request_id(), Some("r-1"));
    /// assert_eq!(outgoing.seq(), Some(9));
    /// assert!(!outgoing.contains_key("user_note"));
    /// assert!(!outgoing.contains_key("_schema_hash"));
    /// assert_eq!(outgoing.timestamp, incoming.timestamp);
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn follow(&self) -> Self {
        let parameters = self
            .parameters
            .iter()
            .filter(|(key, _)| {
                keys::is_correlation_key(key.as_str()) || keys::is_stream_key(key.as_str())
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Self {
            version: METADATA_VERSION,
            timestamp: self.timestamp,
            parameters,
        }
    }

    /// The `request_id`, if present.
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        self.get(keys::REQUEST_ID).and_then(Parameter::as_str)
    }

    /// Sets the `request_id`.
    pub fn set_request_id(&mut self, id: impl Into<String>) {
        self.set_known(keys::REQUEST_ID, Parameter::String(id.into()));
    }

    /// The `goal_id`, if present.
    #[must_use]
    pub fn goal_id(&self) -> Option<&str> {
        self.get(keys::GOAL_ID).and_then(Parameter::as_str)
    }

    /// Sets the `goal_id`.
    pub fn set_goal_id(&mut self, id: impl Into<String>) {
        self.set_known(keys::GOAL_ID, Parameter::String(id.into()));
    }

    /// The `goal_status`, if present and recognised.
    ///
    /// A status code this build does not know yields `None`; the raw integer
    /// is still readable through [`Metadata::get`].
    #[must_use]
    pub fn goal_status(&self) -> Option<GoalStatus> {
        self.get(keys::GOAL_STATUS)
            .and_then(Parameter::as_integer)
            .and_then(GoalStatus::from_i64)
    }

    /// Sets the `goal_status`.
    pub fn set_goal_status(&mut self, status: GoalStatus) {
        self.set_known(keys::GOAL_STATUS, Parameter::Integer(status.as_i64()));
    }

    /// The `session_id`, if present.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.get(keys::SESSION_ID).and_then(Parameter::as_str)
    }

    /// Sets the `session_id`.
    pub fn set_session_id(&mut self, id: impl Into<String>) {
        self.set_known(keys::SESSION_ID, Parameter::String(id.into()));
    }

    /// The `segment_id`, if present.
    #[must_use]
    pub fn segment_id(&self) -> Option<i64> {
        self.get(keys::SEGMENT_ID).and_then(Parameter::as_integer)
    }

    /// Sets the `segment_id`.
    pub fn set_segment_id(&mut self, segment: i64) {
        self.set_known(keys::SEGMENT_ID, Parameter::Integer(segment));
    }

    /// The `seq`, if present.
    #[must_use]
    pub fn seq(&self) -> Option<i64> {
        self.get(keys::SEQ).and_then(Parameter::as_integer)
    }

    /// Sets the `seq`.
    pub fn set_seq(&mut self, seq: i64) {
        self.set_known(keys::SEQ, Parameter::Integer(seq));
    }

    /// The `fin` flag, if present.
    #[must_use]
    pub fn fin(&self) -> Option<bool> {
        self.get(keys::FIN).and_then(Parameter::as_bool)
    }

    /// Sets the `fin` flag.
    pub fn set_fin(&mut self, fin: bool) {
        self.set_known(keys::FIN, Parameter::Bool(fin));
    }

    /// The `flush` flag, if present.
    #[must_use]
    pub fn flush(&self) -> Option<bool> {
        self.get(keys::FLUSH).and_then(Parameter::as_bool)
    }

    /// Sets the `flush` flag.
    pub fn set_flush(&mut self, flush: bool) {
        self.set_known(keys::FLUSH, Parameter::Bool(flush));
    }

    /// The internal `_schema_hash`, if present.
    #[must_use]
    pub fn schema_hash(&self) -> Option<&str> {
        self.get(keys::SCHEMA_HASH).and_then(Parameter::as_str)
    }

    /// Stamps the internal `_schema_hash`.
    ///
    /// Set by the sending side from `astrs-data`'s `SchemaHash` so receivers
    /// can cache decoded schemas and detect type drift cheaply (§6.1).
    pub fn insert_schema_hash(&mut self, hash: impl Into<String>) {
        self.set_known(keys::SCHEMA_HASH, Parameter::String(hash.into()));
    }

    /// Inserts under a well-known key.
    ///
    /// Every constant in [`keys`] is a valid [`ParamKey`] by construction —
    /// asserted by a test in that module — so this cannot fail; the `if let`
    /// keeps the function total without a panic path.
    fn set_known(&mut self, key: &str, value: Parameter) {
        if let Ok(key) = ParamKey::new(key) {
            self.parameters.insert(key, value);
        }
    }

    /// Bitwise equality, so that metadata carrying a `NaN` float compares
    /// equal to itself.
    ///
    /// See [`Parameter::bitwise_eq`].
    #[must_use]
    pub fn bitwise_eq(&self, other: &Self) -> bool {
        self.version == other.version
            && self.timestamp == other.timestamp
            && self.parameters.len() == other.parameters.len()
            && self
                .parameters
                .iter()
                .zip(other.parameters.iter())
                .all(|((lk, lv), (rk, rv))| lk == rk && lv.bitwise_eq(rv))
    }
}

impl Default for Metadata {
    /// Empty metadata stamped at [`HlcTimestamp::EPOCH`].
    fn default() -> Self {
        Self::new(HlcTimestamp::EPOCH)
    }
}

impl<'a> IntoIterator for &'a Metadata {
    type Item = (&'a ParamKey, &'a Parameter);
    type IntoIter = Iter<'a, ParamKey, Parameter>;

    fn into_iter(self) -> Self::IntoIter {
        self.parameters.iter()
    }
}

impl Encode for Metadata {
    fn encode<E: Encoder>(&self, encoder: &mut E) -> Result<(), oxicode::error::Error> {
        self.version.encode(encoder)?;
        self.timestamp.encode(encoder)?;
        self.parameters.encode(encoder)
    }
}

impl Decode for Metadata {
    /// Not generic over `oxicode`'s decode context: the fields this reads
    /// (`HlcTimestamp`, `Parameter`) are `#[derive(Decode)]` types, and the
    /// derive emits context-free implementations.
    fn decode<D: Decoder<Context = ()>>(decoder: &mut D) -> Result<Self, oxicode::error::Error> {
        let version = u16::decode(decoder)?;
        let timestamp = HlcTimestamp::decode(decoder)?;
        let parameters = BTreeMap::<ParamKey, Parameter>::decode(decoder)?;
        if parameters.len() > MAX_PARAMETERS {
            return Err(codec_invalid(format!(
                "metadata carries {} parameters, over the {MAX_PARAMETERS} limit",
                parameters.len()
            )));
        }
        Ok(Self {
            version,
            timestamp,
            parameters,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireDecode, WireEncode};

    fn sample() -> Metadata {
        let mut meta = Metadata::new(HlcTimestamp::new(1_700_000_000_000_000_000, 3));
        meta.set_request_id("req-1");
        meta.set_goal_id("goal-1");
        meta.set_goal_status(GoalStatus::Executing);
        meta.set_session_id("sess-1");
        meta.set_segment_id(2);
        meta.set_seq(7);
        meta.set_fin(false);
        meta.set_flush(true);
        meta.insert_schema_hash("0123abcd");
        meta
    }

    #[test]
    fn every_well_known_key_is_a_valid_param_key() {
        for key in keys::WELL_KNOWN {
            assert!(ParamKey::new(*key).is_ok(), "{key} must be a valid key");
        }
    }

    #[test]
    fn typed_accessors_round_trip_through_the_map() {
        let meta = sample();
        assert_eq!(meta.request_id(), Some("req-1"));
        assert_eq!(meta.goal_id(), Some("goal-1"));
        assert_eq!(meta.goal_status(), Some(GoalStatus::Executing));
        assert_eq!(meta.session_id(), Some("sess-1"));
        assert_eq!(meta.segment_id(), Some(2));
        assert_eq!(meta.seq(), Some(7));
        assert_eq!(meta.fin(), Some(false));
        assert_eq!(meta.flush(), Some(true));
        assert_eq!(meta.schema_hash(), Some("0123abcd"));
        assert_eq!(meta.len(), keys::WELL_KNOWN.len());
    }

    #[test]
    fn absent_keys_read_as_none() {
        let meta = Metadata::default();
        assert_eq!(meta.request_id(), None);
        assert_eq!(meta.goal_id(), None);
        assert_eq!(meta.goal_status(), None);
        assert_eq!(meta.session_id(), None);
        assert_eq!(meta.segment_id(), None);
        assert_eq!(meta.seq(), None);
        assert_eq!(meta.fin(), None);
        assert_eq!(meta.flush(), None);
        assert_eq!(meta.schema_hash(), None);
        assert!(meta.is_empty());
        assert_eq!(meta.timestamp, HlcTimestamp::EPOCH);
    }

    #[test]
    fn a_wrongly_typed_well_known_key_reads_as_none() {
        let mut meta = Metadata::default();
        meta.insert(keys::SEQ, Parameter::String("three".into()))
            .unwrap();
        assert_eq!(meta.seq(), None, "a string seq must not coerce");
        assert!(meta.contains_key(keys::SEQ));
    }

    #[test]
    fn an_unknown_goal_status_code_reads_as_none() {
        let mut meta = Metadata::default();
        meta.insert(keys::GOAL_STATUS, Parameter::Integer(99))
            .unwrap();
        assert_eq!(meta.goal_status(), None);
        assert!(meta.is_correlated(), "the key is still a correlation key");
    }

    #[test]
    fn insert_replaces_and_reports_the_old_value() {
        let mut meta = Metadata::default();
        assert_eq!(meta.insert("k", Parameter::Integer(1)).unwrap(), None);
        assert_eq!(
            meta.insert("k", Parameter::Integer(2)).unwrap(),
            Some(Parameter::Integer(1))
        );
        assert_eq!(meta.get("k"), Some(&Parameter::Integer(2)));
        assert_eq!(meta.remove("k"), Some(Parameter::Integer(2)));
        assert_eq!(meta.remove("k"), None);
    }

    #[test]
    fn invalid_keys_are_refused() {
        let mut meta = Metadata::default();
        assert!(meta.insert("bad key", Parameter::Bool(true)).is_err());
        assert!(meta.insert("", Parameter::Bool(true)).is_err());
        assert!(meta.is_empty());
        assert!(
            Metadata::default()
                .with("also/bad", Parameter::Bool(true))
                .is_err()
        );
    }

    #[test]
    fn insert_key_takes_a_validated_key() {
        let mut meta = Metadata::default();
        let key = ParamKey::new("k").unwrap();
        assert_eq!(meta.insert_key(key.clone(), Parameter::Bool(true)), None);
        assert_eq!(
            meta.insert_key(key, Parameter::Bool(false)),
            Some(Parameter::Bool(true))
        );
    }

    #[test]
    fn strip_internal_removes_only_underscore_keys() {
        let mut meta = sample();
        meta.insert("_trace", Parameter::String("t".into()))
            .unwrap();
        let before = meta.len();
        let removed = meta.strip_internal();
        assert_eq!(removed, 2, "_schema_hash and _trace");
        assert_eq!(meta.len(), before - 2);
        assert!(meta.keys().all(|key| !key.is_internal()));
        assert_eq!(meta.request_id(), Some("req-1"));
        assert_eq!(meta.strip_internal(), 0, "stripping again is a no-op");
    }

    #[test]
    fn stripped_leaves_the_original_alone() {
        let meta = sample();
        let stripped = meta.stripped();
        assert!(stripped.schema_hash().is_none());
        assert!(meta.schema_hash().is_some());
        assert_eq!(stripped.len() + 1, meta.len());
    }

    #[test]
    fn correlation_detection_matches_the_key_set() {
        assert!(!Metadata::default().is_correlated());
        for key in keys::CORRELATION {
            let mut meta = Metadata::default();
            meta.insert(key, Parameter::String("x".into())).unwrap();
            assert!(meta.is_correlated(), "{key} must make a message correlated");
        }
        for key in keys::STREAM {
            let mut meta = Metadata::default();
            meta.insert(key, Parameter::Integer(1)).unwrap();
            assert!(!meta.is_correlated(), "{key} must not");
        }
    }

    #[test]
    fn stream_chunk_detection() {
        assert!(!Metadata::default().is_stream_chunk());
        let mut meta = Metadata::default();
        meta.set_session_id("s");
        assert!(meta.is_stream_chunk());
    }

    #[test]
    fn follow_keeps_correlation_and_stream_keys_only() {
        let mut source = sample();
        source.insert("user", Parameter::Integer(1)).unwrap();
        let derived = source.follow();

        for key in keys::CORRELATION.iter().chain(keys::STREAM.iter()) {
            assert!(derived.contains_key(key), "{key} must survive follow()");
        }
        assert!(!derived.contains_key("user"));
        assert!(!derived.contains_key(keys::SCHEMA_HASH));
        assert_eq!(derived.timestamp, source.timestamp);
        assert_eq!(derived.version, METADATA_VERSION);
    }

    #[test]
    fn follow_of_empty_metadata_is_empty() {
        let derived = Metadata::new(HlcTimestamp::new(4, 1)).follow();
        assert!(derived.is_empty());
        assert_eq!(derived.timestamp, HlcTimestamp::new(4, 1));
    }

    #[test]
    fn codec_round_trips_the_full_sample() {
        let meta = sample();
        let bytes = meta.encode_to_vec().unwrap();
        assert_eq!(bytes.len(), meta.encode_size_hint().unwrap());
        assert_eq!(Metadata::decode_exact(&bytes).unwrap(), meta);
    }

    #[test]
    fn codec_round_trips_empty_metadata_cheaply() {
        let meta = Metadata::default();
        let bytes = meta.encode_to_vec().unwrap();
        // version (varint 1) + timestamp + empty-map length.
        assert!(
            bytes.len() <= 8,
            "empty metadata cost {} bytes",
            bytes.len()
        );
        assert_eq!(Metadata::decode_exact(&bytes).unwrap(), meta);
    }

    #[test]
    fn encoding_is_deterministic_regardless_of_insertion_order() {
        let mut forwards = Metadata::new(HlcTimestamp::new(1, 1));
        let mut backwards = Metadata::new(HlcTimestamp::new(1, 1));
        for key in ["a", "b", "c", "d"] {
            forwards.insert(key, Parameter::String(key.into())).unwrap();
        }
        for key in ["d", "c", "b", "a"] {
            backwards
                .insert(key, Parameter::String(key.into()))
                .unwrap();
        }
        assert_eq!(
            forwards.encode_to_vec().unwrap(),
            backwards.encode_to_vec().unwrap()
        );
    }

    #[test]
    fn decoding_rejects_an_invalid_parameter_key() {
        let forged = (
            METADATA_VERSION,
            HlcTimestamp::EPOCH,
            BTreeMap::from([("bad key".to_owned(), Parameter::Bool(true))]),
        )
            .encode_to_vec()
            .unwrap();
        assert!(Metadata::decode_exact(&forged).is_err());
    }

    #[test]
    fn decoding_rejects_too_many_parameters() {
        let mut oversized = BTreeMap::new();
        for index in 0..=MAX_PARAMETERS {
            oversized.insert(format!("k{index}"), Parameter::Integer(index as i64));
        }
        let forged = (METADATA_VERSION, HlcTimestamp::EPOCH, oversized)
            .encode_to_vec()
            .unwrap();
        assert!(Metadata::decode_exact(&forged).is_err());
    }

    #[test]
    fn a_future_metadata_version_still_decodes() {
        // Forward compatibility: an unknown layout version is carried, not
        // rejected, so a newer sender's metadata survives an old recorder.
        let forged = (
            METADATA_VERSION + 7,
            HlcTimestamp::new(9, 0),
            BTreeMap::<ParamKey, Parameter>::new(),
        )
            .encode_to_vec()
            .unwrap();
        let decoded = Metadata::decode_exact(&forged).unwrap();
        assert_eq!(decoded.version, METADATA_VERSION + 7);
    }

    #[test]
    fn bitwise_equality_covers_nan_parameters() {
        let mut left = Metadata::new(HlcTimestamp::new(1, 0));
        left.insert("f", Parameter::Float(f64::NAN)).unwrap();
        let right = left.clone();
        assert_ne!(left, right, "PartialEq cannot see NaN as equal");
        assert!(left.bitwise_eq(&right));

        let mut other = Metadata::new(HlcTimestamp::new(2, 0));
        other.insert("f", Parameter::Float(f64::NAN)).unwrap();
        assert!(!left.bitwise_eq(&other), "timestamps differ");
    }

    #[test]
    fn iteration_is_in_key_order() {
        let mut meta = Metadata::default();
        for key in ["z", "a", "m"] {
            meta.insert(key, Parameter::Integer(0)).unwrap();
        }
        let order: Vec<_> = meta.keys().map(ParamKey::as_str).collect();
        assert_eq!(order, vec!["a", "m", "z"]);
        assert_eq!(meta.values().count(), 3);
        assert_eq!(meta.iter().count(), 3);
        assert_eq!((&meta).into_iter().count(), 3);
    }

    #[test]
    fn entry_supports_read_modify_write() {
        let mut meta = Metadata::default();
        let key = ParamKey::new("counter").unwrap();
        meta.entry(key.clone()).or_insert(Parameter::Integer(0));
        if let Some(Parameter::Integer(value)) = meta.parameters.get_mut(&key) {
            *value += 5;
        }
        assert_eq!(meta.get("counter").and_then(Parameter::as_integer), Some(5));
    }

    #[test]
    fn serde_round_trips() {
        let meta = sample();
        let json = serde_json::to_string(&meta).unwrap();
        assert_eq!(serde_json::from_str::<Metadata>(&json).unwrap(), meta);
        assert!(json.contains("\"version\""));
        assert!(json.contains("\"timestamp\""));
        assert!(json.contains("\"parameters\""));
    }
}
