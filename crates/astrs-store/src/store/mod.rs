//! The [`CoordinatorStore`] core: construction, the write gate, and the one
//! atomic-write path every bucket module funnels through.
//!
//! Each bucket gets its own `impl<S: KvStore + Clone> CoordinatorStore<S>`
//! block in its own file — [`params`], [`dataflow`], [`daemon`],
//! [`build_cache`], [`mutation_log`] — but all of them call
//! `CoordinatorStore::apply_put` / `CoordinatorStore::apply_delete_if_present`
//! defined here to actually write. See those methods' docs for why that
//! matters.

pub mod build_cache;
pub mod daemon;
pub mod dataflow;
pub mod mutation_log;
pub mod node_params;
pub mod params;
pub mod topology;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use astrs_time::{HlcClock, HlcTimestamp, SystemClock};
use astrs_wire::WireEncode;
use oxistore_core::{KvStore, KvTxn};
use oxistore_kv_redb::RedbStore;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{Error, Result};
use crate::keys;
use crate::record::{MutationOp, MutationRecord, MutationSeq};
use crate::schema;

/// Durable coordinator state: parameters, the dataflow registry, the daemon
/// registry, the build-cache index and the mutation catch-up log, all
/// backed by one [`oxistore_core::KvStore`] implementation.
///
/// Generic over the backend so tests (and `astrs run`'s embedded
/// coordinator) can use [`RedbStore::open_in_memory`] while a real
/// deployment uses [`RedbStore::open`] — both produce the same `RedbStore`
/// type with the same trait surface, so [`CoordinatorStore::open`] and
/// [`CoordinatorStore::open_in_memory`] are the only two places that name it
/// directly; every bucket method in this crate is written against the
/// [`KvStore`] trait alone.
///
/// Every method takes `&self`. Interior mutability lives in the backend
/// (`RedbStore` wraps its `redb::Database` in an `Arc`) and in this type's
/// own write gate, which is what makes a `CoordinatorStore` cheap to clone
/// and safe to share across the coordinator's async tasks — see
/// [`crate::AsyncCoordinatorStore`] for the `spawn_blocking` wrapper that
/// actually does so.
#[derive(Clone)]
pub struct CoordinatorStore<S: KvStore + Clone> {
    backend: S,
    clock: Arc<HlcClock<SystemClock>>,
    /// Serializes the allocate-seq / write-bucket / append-log / commit
    /// critical section across every mutating call on this handle — and
    /// every clone of it, since the `Arc` is shared.
    ///
    /// `oxistore_kv_redb::RedbStore::transaction` already serializes actual
    /// commits (redb allows exactly one writer at a time), but that
    /// guarantee lives in a specific backend this type is generic over, not
    /// in the [`KvStore`] trait contract. Holding this gate for the whole
    /// critical section makes "sequence numbers never gap or duplicate"
    /// true by construction for *any* `S: KvStore`, rather than true today
    /// only because of how one particular backend happens to serialize
    /// writers.
    write_gate: Arc<Mutex<()>>,
    display_path: Arc<PathBuf>,
}

impl<S: KvStore + Clone> fmt::Debug for CoordinatorStore<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoordinatorStore")
            .field("path", &self.display_path)
            .finish_non_exhaustive()
    }
}

impl CoordinatorStore<RedbStore> {
    /// Opens (or creates) a redb-file-backed store at `path`.
    ///
    /// `path` is caller-supplied and never defaulted or hardcoded by this
    /// crate — the coordinator process decides where its state lives (e.g.
    /// under `$XDG_RUNTIME_DIR/astrs` or a configured data directory).
    ///
    /// # Errors
    ///
    /// [`Error::Backend`] if the file cannot be opened or created.
    /// [`Error::SchemaMismatch`] if the file already exists with an
    /// incompatible [`crate::schema::STORE_SCHEMA_VERSION`] — see
    /// [`crate::schema::recreate_store`] for the recovery path.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_store::CoordinatorStore;
    ///
    /// let path = std::env::temp_dir()
    ///     .join(format!("astrs-store-open-doctest-{}.redb", std::process::id()));
    /// # std::fs::remove_file(&path).ok();
    /// let store = CoordinatorStore::open(&path)?;
    /// assert_eq!(store.display_path(), path.as_path());
    /// # drop(store);
    /// # std::fs::remove_file(&path).ok();
    /// # Ok::<(), astrs_store::Error>(())
    /// ```
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let backend = RedbStore::open(&path)?;
        Self::from_backend_at(backend, path)
    }

    /// Opens a fresh in-memory redb-backed store.
    ///
    /// Used by tests and by `astrs run`'s embedded single-process
    /// coordinator, which has no on-disk state to persist across restarts.
    ///
    /// # Errors
    ///
    /// [`Error::Backend`] if the in-memory backend cannot be constructed.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_store::CoordinatorStore;
    ///
    /// let store = CoordinatorStore::open_in_memory()?;
    /// assert!(store.list_daemons()?.is_empty());
    /// # Ok::<(), astrs_store::Error>(())
    /// ```
    pub fn open_in_memory() -> Result<Self> {
        let backend = RedbStore::open_in_memory()?;
        Self::from_backend_at(backend, PathBuf::from("<in-memory>"))
    }
}

impl<S: KvStore + Clone> CoordinatorStore<S> {
    /// Wraps an already-constructed backend, checking (and initializing) its
    /// schema marker.
    ///
    /// The generic entry point for a caller supplying its own [`KvStore`]
    /// implementation; [`CoordinatorStore::open`] and
    /// [`CoordinatorStore::open_in_memory`] are convenience wrappers around
    /// it for the [`RedbStore`] backend.
    ///
    /// # Errors
    ///
    /// As [`CoordinatorStore::open`].
    pub fn from_backend(backend: S) -> Result<Self> {
        Self::from_backend_at(backend, PathBuf::from("<unknown>"))
    }

    fn from_backend_at(backend: S, display_path: PathBuf) -> Result<Self> {
        schema::check_or_initialize(&backend, &display_path)?;
        Ok(Self {
            backend,
            clock: Arc::new(HlcClock::system()),
            write_gate: Arc::new(Mutex::new(())),
            display_path: Arc::new(display_path),
        })
    }

    /// The path this store was opened from (`CoordinatorStore::open`), or a
    /// placeholder (`"<in-memory>"` / `"<unknown>"`) for a backend that was
    /// not opened from a file.
    #[must_use]
    pub fn display_path(&self) -> &Path {
        &self.display_path
    }

    /// Reads the last mutation-log sequence number issued.
    ///
    /// [`MutationSeq::ZERO`] if no mutating call has ever been made on this
    /// store.
    ///
    /// # Errors
    ///
    /// [`Error::Backend`] if the read fails; [`Error::CorruptKey`] if the
    /// stored counter is not eight bytes (on-disk corruption).
    pub fn last_seq(&self) -> Result<MutationSeq> {
        match self.backend.get(&keys::next_seq_key())? {
            None => Ok(MutationSeq::ZERO),
            Some(bytes) => Ok(MutationSeq::new(decode_seq(&bytes)?)),
        }
    }

    /// Reads the compaction watermark: the highest sequence number
    /// [`CoordinatorStore::compact`] has removed history up to.
    ///
    /// [`MutationSeq::ZERO`] if compaction has never run.
    ///
    /// # Errors
    ///
    /// As [`CoordinatorStore::last_seq`].
    pub fn compacted_before(&self) -> Result<MutationSeq> {
        match self.backend.get(&keys::compacted_before_key())? {
            None => Ok(MutationSeq::ZERO),
            Some(bytes) => Ok(MutationSeq::new(decode_seq(&bytes)?)),
        }
    }

    /// Allocates the next mutation-log sequence number inside an
    /// already-open transaction: reads the counter through the
    /// transaction's own (read-your-writes) view, increments it, and stages
    /// the new value back — all as part of the caller's not-yet-committed
    /// transaction, so a crash or rollback before commit leaves the counter
    /// untouched rather than skipping a number.
    fn allocate_seq_locked(txn: &mut dyn KvTxn) -> Result<MutationSeq> {
        let current = match txn.get(&keys::next_seq_key())? {
            Some(bytes) => decode_seq(&bytes)?,
            None => 0,
        };
        let seq = MutationSeq::new(current + 1);
        txn.put(&keys::next_seq_key(), &seq.get().to_be_bytes())?;
        Ok(seq)
    }

    /// Reads the current value at `bucket_key` (if any), lets `build`
    /// compute the replacement bytes and the [`MutationOp`] describing the
    /// change, then writes both and appends the matching mutation-log
    /// record — atomically, holding [`CoordinatorStore::write_gate`] for
    /// the whole read-modify-write.
    ///
    /// This is the **only** place in this crate that performs a bucket
    /// *put*: every `set_*` / `upsert_*` / `record_*` method across
    /// [`params`], [`dataflow`], [`daemon`] and [`build_cache`] funnels
    /// through it, so a bucket write and its log record can never drift
    /// apart. It is a read-modify-write, not a blind write, because every
    /// record type this crate defines carries a `revision` counter that
    /// must be read and incremented under the same lock as the write —
    /// computing "current revision + 1" *before* acquiring the write gate
    /// would let two concurrent writers to the same key both read the same
    /// old revision and both write the same new one, silently
    /// under-counting writes.
    ///
    /// # Errors
    ///
    /// Whatever `build` returns, plus [`Error::Backend`] if the transaction
    /// fails to open, write or commit, and [`Error::Wire`] if the resulting
    /// [`MutationRecord`] fails to oxicode-encode (unreachable for the
    /// record types this crate defines).
    pub(crate) fn apply_upsert(
        &self,
        bucket_key: Vec<u8>,
        build: impl FnOnce(Option<Vec<u8>>, HlcTimestamp) -> Result<(Vec<u8>, MutationOp)>,
    ) -> Result<MutationSeq> {
        let _gate = self
            .write_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        let mut txn = self.backend.transaction()?;
        let existing = txn.get(&bucket_key)?;
        let ts = self.clock.now();
        let (new_bytes, op) = build(existing, ts)?;
        txn.put(&bucket_key, &new_bytes)?;
        let seq = Self::allocate_seq_locked(&mut *txn)?;
        let record = MutationRecord { seq, ts, op };
        txn.put(&keys::mutation_log_key(seq), &record.encode_to_vec()?)?;
        txn.commit()?;
        Ok(seq)
    }

    /// As [`CoordinatorStore::apply_upsert`], but a no-op — no write, no log
    /// record, `Ok(None)` — when `bucket_key` is not already present.
    ///
    /// Used for partial updates that only make sense against an existing
    /// record (a daemon heartbeat with no prior registration, a build-cache
    /// "last used" touch for a hash that was never recorded): there is no
    /// sensible record for `build` to produce from nothing, so this never
    /// calls it in that case, unlike [`CoordinatorStore::apply_upsert`]
    /// which always calls `build` (with `None`) to let it decide whether
    /// "absent" means "create".
    ///
    /// # Errors
    ///
    /// As [`CoordinatorStore::apply_upsert`].
    pub(crate) fn apply_update_if_present(
        &self,
        bucket_key: Vec<u8>,
        build: impl FnOnce(Vec<u8>, HlcTimestamp) -> Result<(Vec<u8>, MutationOp)>,
    ) -> Result<Option<MutationSeq>> {
        let _gate = self
            .write_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        let mut txn = self.backend.transaction()?;
        let existing = match txn.get(&bucket_key)? {
            Some(bytes) => bytes,
            None => {
                txn.rollback()?;
                return Ok(None);
            }
        };
        let ts = self.clock.now();
        let (new_bytes, op) = build(existing, ts)?;
        txn.put(&bucket_key, &new_bytes)?;
        let seq = Self::allocate_seq_locked(&mut *txn)?;
        let record = MutationRecord { seq, ts, op };
        txn.put(&keys::mutation_log_key(seq), &record.encode_to_vec()?)?;
        txn.commit()?;
        Ok(Some(seq))
    }

    /// Deletes one bucket key — if it is present — and appends the matching
    /// mutation-log record, atomically, holding
    /// [`CoordinatorStore::write_gate`] for the duration.
    ///
    /// If `bucket_key` is already absent, this is a no-op: no bucket write,
    /// no log record, and `Ok(None)`. A delete of something that was never
    /// there is not a state change a reconnecting daemon needs to hear
    /// about, and logging it anyway would let a caller retrying deletes
    /// grow the log without bound for no observable effect.
    ///
    /// # Errors
    ///
    /// As [`CoordinatorStore::apply_put`].
    pub(crate) fn apply_delete_if_present(
        &self,
        bucket_key: &[u8],
        op: MutationOp,
    ) -> Result<Option<MutationSeq>> {
        let _gate = self
            .write_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        let mut txn = self.backend.transaction()?;
        if txn.get(bucket_key)?.is_none() {
            txn.rollback()?;
            return Ok(None);
        }
        txn.delete(bucket_key)?;
        let seq = Self::allocate_seq_locked(&mut *txn)?;
        let record = MutationRecord {
            seq,
            ts: self.clock.now(),
            op,
        };
        txn.put(&keys::mutation_log_key(seq), &record.encode_to_vec()?)?;
        txn.commit()?;
        Ok(Some(seq))
    }

    /// Folds a previously logged [`MutationRecord`] directly into this
    /// store's buckets, without appending a new mutation-log record of its
    /// own.
    ///
    /// This is what a reconnecting daemon's local shadow of coordinator
    /// state — or this crate's own tests — use to turn a
    /// [`crate::record::CatchUpBatch`] from
    /// [`CoordinatorStore::mutations_since`] into bucket state: replaying
    /// every record it contains, in order, reproduces the source store's
    /// bucket contents exactly, because every `*Put` [`MutationOp`] carries
    /// the complete new record rather than a delta (see the [`MutationOp`]
    /// docs).
    ///
    /// # Errors
    ///
    /// [`Error::JsonEncode`] if re-serializing the embedded record to the
    /// bucket's JSON form fails (unreachable for a record decoded from this
    /// crate's own oxicode form). [`Error::Backend`] if the underlying
    /// write fails.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_store::CoordinatorStore;
    /// use astrs_wire::{DataflowId, ParamKey};
    ///
    /// let source = CoordinatorStore::open_in_memory()?;
    /// let dataflow = DataflowId::generate();
    /// let key = ParamKey::new("gain")?;
    /// source.set_param(dataflow, key.clone(), serde_json::json!(1.5))?;
    ///
    /// let shadow = CoordinatorStore::open_in_memory()?;
    /// let batch = source.mutations_since(Default::default(), 16)?;
    /// for record in &batch.entries {
    ///     shadow.apply_replayed(record)?;
    /// }
    ///
    /// assert_eq!(
    ///     shadow.get_param(dataflow, &key)?.map(|r| r.value_json),
    ///     source.get_param(dataflow, &key)?.map(|r| r.value_json),
    /// );
    /// # Ok::<(), astrs_store::Error>(())
    /// ```
    pub fn apply_replayed(&self, record: &MutationRecord) -> Result<()> {
        match &record.op {
            MutationOp::ParamPut {
                dataflow,
                key,
                record,
            } => {
                let bytes = to_json("param record", record)?;
                self.backend.put(&keys::param_key(*dataflow, key), &bytes)?;
            }
            MutationOp::ParamDelete { dataflow, key } => {
                self.backend.delete(&keys::param_key(*dataflow, key))?;
            }
            MutationOp::DataflowMetaPut { record } => {
                let bytes = to_json("dataflow meta", record)?;
                self.backend
                    .put(&keys::dataflow_meta_key(record.id), &bytes)?;
            }
            MutationOp::DataflowMetaDelete { dataflow } => {
                self.backend.delete(&keys::dataflow_meta_key(*dataflow))?;
            }
            MutationOp::NodeStatusPut { record } => {
                let bytes = to_json("node status", record)?;
                self.backend.put(
                    &keys::node_status_key(record.info.dataflow, &record.info.node),
                    &bytes,
                )?;
            }
            MutationOp::NodeStatusDelete { dataflow, node } => {
                self.backend
                    .delete(&keys::node_status_key(*dataflow, node))?;
            }
            MutationOp::DaemonPut { record } => {
                let bytes = to_json("daemon record", record)?;
                self.backend
                    .put(&keys::daemon_key(&record.info.id), &bytes)?;
            }
            MutationOp::DaemonDelete { daemon } => {
                self.backend.delete(&keys::daemon_key(daemon))?;
            }
            MutationOp::BuildCachePut { record } => {
                let bytes = to_json("build cache entry", record)?;
                self.backend
                    .put(&keys::build_cache_key(&record.hash), &bytes)?;
            }
            MutationOp::BuildCacheDelete { hash } => {
                self.backend.delete(&keys::build_cache_key(hash))?;
            }
            MutationOp::NodeParamPut {
                dataflow,
                node,
                key,
                record,
            } => {
                let bytes = to_json("node param record", record)?;
                self.backend
                    .put(&keys::node_param_key(*dataflow, node, key), &bytes)?;
            }
            MutationOp::NodeParamDelete {
                dataflow,
                node,
                key,
            } => {
                self.backend
                    .delete(&keys::node_param_key(*dataflow, node, key))?;
            }
            MutationOp::TopologyOpApplied { .. } => {
                // No bucket write: a dynamic-topology op has no domain
                // bucket of its own (see that variant's docs) — the log
                // entry itself is the durable record, and a shadow store
                // built by replaying it already has everything a later
                // `astrs_graph::apply` replay needs.
            }
        }
        Ok(())
    }
}

/// Decodes an 8-byte big-endian counter value.
fn decode_seq(bytes: &[u8]) -> Result<u64> {
    let array: [u8; 8] = bytes.try_into().map_err(|_| Error::CorruptKey {
        bucket: "meta",
        key_preview: crate::keys::preview(bytes),
        reason: format!("counter is {} bytes, expected 8", bytes.len()),
    })?;
    Ok(u64::from_be_bytes(array))
}

/// Serializes `value` to compact JSON, wrapping a failure as a typed error
/// tagged with `what` (a short label such as `"param record"`).
pub(crate) fn to_json<T: Serialize>(what: &'static str, value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|source| Error::JsonEncode { what, source })
}

/// Deserializes `bytes` as JSON, wrapping a failure as a typed error tagged
/// with `what` and a preview of the key `bytes` was read from.
pub(crate) fn from_json<T: DeserializeOwned>(
    what: &'static str,
    key: &[u8],
    bytes: &[u8],
) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|source| Error::JsonDecode {
        what,
        key_preview: crate::keys::preview(key),
        source,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::{DataflowId, ParamKey};

    #[test]
    fn open_in_memory_starts_with_every_bucket_empty() {
        let store = CoordinatorStore::open_in_memory().unwrap();
        assert!(store.list_daemons().unwrap().is_empty());
        assert_eq!(store.last_seq().unwrap(), MutationSeq::ZERO);
        assert_eq!(store.compacted_before().unwrap(), MutationSeq::ZERO);
    }

    #[test]
    fn apply_upsert_and_apply_delete_if_present_share_seq_allocation() {
        let store = CoordinatorStore::open_in_memory().unwrap();
        let dataflow = DataflowId::from_u128(1);
        let key = ParamKey::new("gain").unwrap();

        let seq1 = store
            .set_param(dataflow, key.clone(), serde_json::json!(1))
            .unwrap();
        assert_eq!(seq1.get(), 1);

        // Deleting an absent key allocates no sequence number.
        let other = ParamKey::new("absent").unwrap();
        assert!(store.delete_param(dataflow, &other).unwrap().is_none());
        assert_eq!(store.last_seq().unwrap(), MutationSeq::new(1));

        let deleted = store.delete_param(dataflow, &key).unwrap();
        assert_eq!(deleted, Some(MutationSeq::new(2)));
    }

    #[test]
    fn apply_replayed_reproduces_a_param_write() {
        let source = CoordinatorStore::open_in_memory().unwrap();
        let dataflow = DataflowId::generate();
        let key = ParamKey::new("gain").unwrap();
        source
            .set_param(dataflow, key.clone(), serde_json::json!(1.5))
            .unwrap();

        let shadow = CoordinatorStore::open_in_memory().unwrap();
        let batch = source.mutations_since(MutationSeq::ZERO, 16).unwrap();
        for record in &batch.entries {
            shadow.apply_replayed(record).unwrap();
        }

        assert_eq!(
            shadow
                .get_param(dataflow, &key)
                .unwrap()
                .map(|r| r.value_json),
            source
                .get_param(dataflow, &key)
                .unwrap()
                .map(|r| r.value_json),
        );
    }

    #[test]
    fn apply_replayed_reproduces_a_node_param_write_and_delete() {
        use astrs_wire::NodeId;

        let source = CoordinatorStore::open_in_memory().unwrap();
        let dataflow = DataflowId::generate();
        let node = NodeId::new("camera").unwrap();
        let key = ParamKey::new("exposure").unwrap();
        source
            .set_node_param(dataflow, node.clone(), key.clone(), serde_json::json!(12))
            .unwrap();

        let shadow = CoordinatorStore::open_in_memory().unwrap();
        let batch = source.mutations_since(MutationSeq::ZERO, 16).unwrap();
        for record in &batch.entries {
            shadow.apply_replayed(record).unwrap();
        }
        assert_eq!(
            shadow.get_node_param(dataflow, &node, &key).unwrap(),
            source.get_node_param(dataflow, &node, &key).unwrap(),
        );

        source.delete_node_param(dataflow, &node, &key).unwrap();
        let batch = source.mutations_since(MutationSeq::new(1), 16).unwrap();
        for record in &batch.entries {
            shadow.apply_replayed(record).unwrap();
        }
        assert!(
            shadow
                .get_node_param(dataflow, &node, &key)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn debug_does_not_require_the_backend_to_implement_debug() {
        let store = CoordinatorStore::open_in_memory().unwrap();
        let text = format!("{store:?}");
        assert!(text.contains("CoordinatorStore"));
    }
}
