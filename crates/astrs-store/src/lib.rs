//! Durable coordinator state for AstRS.
//!
//! A thin, schema-versioned persistence layer over `oxistore-core` /
//! `oxistore-kv-redb` (blueprint §5.2, §2.2 "coordinator persistence"
//! row):
//!
//! - [`CoordinatorStore`] — the parameter store backing `astrs param
//!   get/set/list/delete` ([`store::params`]); dataflow lifecycle state, so
//!   a restarted coordinator resumes rather than forgetting the cluster
//!   ([`store::dataflow`]); the daemon registry ([`store::daemon`]); and
//!   the build cache index for artifact reuse across `astrs build` runs
//!   ([`store::build_cache`]).
//! - [`schema`] — a schema-version marker checked on every open, plus the
//!   explicit [`schema::recreate_store`] escape hatch (blueprint §2.1: the
//!   dora coordinator-store lesson).
//! - [`store::mutation_log`] — a seq-numbered mutation log every mutating
//!   call appends to, so a reconnecting daemon can be resynchronised with
//!   `StateCatchUp` (blueprint §12) instead of re-sent the coordinator's
//!   entire state.
//! - [`AsyncCoordinatorStore`] — the same API, with every blocking backend
//!   call pushed through [`tokio::task::spawn_blocking`], for the
//!   coordinator's async event loop.
//!
//! # Two backends, one trait surface
//!
//! [`CoordinatorStore`] is generic over any [`oxistore_core::KvStore`], but
//! in practice this crate has exactly one backend:
//! [`oxistore_kv_redb::RedbStore`], used two ways —
//! [`CoordinatorStore::open`] (a real file, for a real deployment) and
//! [`CoordinatorStore::open_in_memory`] (an ephemeral in-memory database,
//! for tests and `astrs run`'s embedded single-process coordinator). Both
//! constructors return the identical `RedbStore` type with the identical
//! `KvStore` trait surface, so every bucket method in this crate is written
//! once, against the trait, and works identically against either.
//!
//! # Buckets are a key-prefixing convention
//!
//! `oxistore-core`'s [`oxistore_core::KvStore`] is one flat byte-keyed
//! namespace with no native concept of separate tables. This crate's
//! "buckets" — params, the dataflow registry, the daemon registry, the
//! build cache, the mutation log — are a one-byte key-tag convention layered
//! on top of that single namespace, implemented in this crate's private
//! `keys` module (its doc comment has the exact layout and the reasoning
//! behind it).
//!
//! # JSON buckets, an oxicode log
//!
//! Every record type in [`record`] derives both `serde::{Serialize,
//! Deserialize}` and `oxicode::{Encode, Decode}` and is written through
//! **both** codecs, deliberately at different points:
//!
//! - **Buckets store JSON.** A param's value, a dataflow's manifest
//!   snapshot, a node's status — these are exactly the data
//!   `ControlReply::ParamValue` / `astrs list --json` / a future `astrs
//!   store dump` would put on the human boundary anyway (blueprint §7.1:
//!   *"JSON only at the human boundary"*), and JSON is what stays legible
//!   in a raw file dump while debugging a coordinator.
//! - **The mutation log stores oxicode**, via [`astrs_wire::WireEncode`] /
//!   [`astrs_wire::WireDecode`] (this crate's dependency on `astrs-wire` is
//!   for exactly this codec, plus the id vocabulary). The log is never a
//!   human boundary — only [`CoordinatorStore::mutations_since`] and
//!   [`CoordinatorStore::apply_replayed`] ever read a record back — and it
//!   is the one bucket whose write volume can be genuinely high (every
//!   mutating call appends one entry, including daemon heartbeats), so it
//!   gets the compact, varint-friendly, sixteen-bytes-not-thirty-six-per-uuid
//!   encoding instead.
//!
//! `serde_json::Value` has no oxicode encoding, which is *why* values that
//! need to live in both places — a parameter's value, a manifest snapshot —
//! are kept as pre-serialized JSON `String`s inside their record types
//! rather than as `serde_json::Value`: see [`record::ParamRecord`] for the
//! full reasoning. That choice also means a mutation-log replay
//! ([`CoordinatorStore::apply_replayed`]) never re-serializes a caller's
//! original value — the exact JSON text that was stored is the exact JSON
//! text that gets replayed.
//!
//! # Every write is logged, atomically, from one place
//!
//! Every `set_*` / `upsert_*` / `record_*` / `delete_*` method across every
//! bucket module funnels through one of three helpers on
//! [`CoordinatorStore`] — `apply_upsert`, `apply_update_if_present`,
//! `apply_delete_if_present` (crate-private; see `src/store/mod.rs`) — each
//! of which opens one backend transaction, allocates the next mutation-log
//! sequence number *inside* that transaction, writes the bucket row and the
//! log row, and commits. A [`std::sync::Mutex`] held for that whole
//! sequence (not merely relied upon from the backend) is what makes
//! "sequence numbers never gap or duplicate" true for any
//! [`oxistore_core::KvStore`] implementation, not only for `redb`'s own
//! single-writer behavior.

pub mod async_store;
mod error;
mod keys;
pub mod record;
pub mod schema;
pub mod store;

pub use async_store::{AsyncCoordinatorStore, AsyncStore};
pub use error::{Error, Result};
pub use record::{
    BuildCacheEntry, BuildCacheKey, CatchUpBatch, DaemonRecord, DataflowMeta, DataflowSnapshot,
    MutationOp, MutationRecord, MutationSeq, NodeStatusRecord, ParamRecord,
};
pub use store::CoordinatorStore;
pub use store::mutation_log::MAX_CATCH_UP_PAGE;
