//! [`SegmentRegistry`] — the daemon's side of the shared-memory plane (§6.2).
//!
//! > *The daemon holds every segment fd; on producer death it marks `closed`,
//! > lets consumers drain, and unlinks. A restarted node gets a new generation
//! > — stale mappings are detectable by every reader.*
//!
//! [`astrs_shm::SegmentBroker`] already implements all of that. This module is
//! the *bookkeeping* around it: the mapping from a daemon-level
//! [`OutputKey`] to the segment that serves it, the
//! [`astrs_wire::ShmSegmentSpec`] the producer is told about, and the
//! translation of the broker's segment-key answers back into node ids the
//! event loop can act on.
//!
//! # Descriptors, not names
//!
//! Nodes never learn a shared-memory object name. They connect to the broker's
//! Unix socket — the path travels in [`astrs_wire::NodeConfig::shm_broker`] —
//! and are handed a *descriptor* over `SCM_RIGHTS`
//! ([`astrs_shm::fdpass`]). Attachment is therefore a capability the daemon
//! grants, not a name anything on the host can guess, which is the §16
//! assumption the whole plane rests on.
//!
//! # No second timer
//!
//! `astrs-shm`'s broker documentation is explicit that
//! [`astrs_shm::SegmentBroker::poll_producers`] and
//! [`astrs_shm::SegmentBroker::sweep`] are *calls*, not a hidden thread,
//! "because the daemon already owns a supervision loop and a second timer
//! would be a second source of truth". [`SegmentRegistry::maintain`] is where
//! the daemon's loop makes those calls.
//!
//! The one thread this module does own is the broker's accept loop, which must
//! block in `accept(2)` and has nothing to do with time.
//!
//! # Examples
//!
//! ```
//! # #[cfg(unix)] {
//! use astrs_daemon::shm::{OutputKey, SegmentRegistry};
//! use astrs_shm::SegmentConfig;
//! use astrs_wire::{DataId, DataflowId, NodeId};
//!
//! let socket = std::env::temp_dir().join(format!("astrs-doc-{}.sock", std::process::id()));
//! let mut registry = SegmentRegistry::bind(&socket)?;
//! assert!(registry.is_enabled());
//!
//! let key = OutputKey::new(
//!     DataflowId::from_u128(1),
//!     NodeId::new("camera")?,
//!     DataId::new("image")?,
//! );
//! let spec = registry.create(key.clone(), 1, SegmentConfig::new(4, 1024)?)?;
//! assert_eq!(spec.generation, 1);
//! assert_eq!(registry.open_count(), 1);
//! # }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use astrs_shm::{BrokerHandle, BrokerStats, Segment, SegmentBroker, SegmentConfig, SegmentKey};
use astrs_wire::{DataflowId, NodeId, ShmSegmentSpec};

use crate::error::{DaemonError, DaemonResult};
use crate::shm::keys::OutputKey;

/// How long a consumer entry may go without a heartbeat before the broker
/// evicts it (§6.2, the drop-token protocol's liveness half).
///
/// Matches [`astrs_shm::DEFAULT_CONSUMER_STALE_AFTER`], restated here because
/// the daemon passes it explicitly on every maintenance pass rather than
/// relying on a default it does not own.
pub const CONSUMER_STALE_AFTER: Duration = astrs_shm::DEFAULT_CONSUMER_STALE_AFTER;

/// One brokered segment, as the daemon sees it.
#[derive(Debug, Clone)]
pub struct SegmentRecord {
    /// The output it serves.
    pub key: OutputKey,
    /// The producer incarnation that owns it (§12).
    pub generation: u64,
    /// The shared-memory key, whose digest names the segment on the wire.
    pub segment_key: SegmentKey,
    /// The specification handed to the producer.
    pub spec: ShmSegmentSpec,
    /// The mapping itself, kept so the daemon can read the consumer table.
    segment: Arc<Segment>,
}

impl SegmentRecord {
    /// The mapped segment.
    #[must_use]
    pub fn segment(&self) -> &Arc<Segment> {
        &self.segment
    }

    /// How many bytes the mapping occupies.
    #[must_use]
    pub fn mapped_bytes(&self) -> u64 {
        self.segment.mapped_len() as u64
    }

    /// Whether the segment has been marked closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.segment.view().closed
    }

    /// The pids currently occupying the segment's consumer table.
    ///
    /// This is the observation [`crate::shm::AttachmentLedger`] folds in: a
    /// consumer that mapped the ring claimed an entry and wrote its pid there,
    /// and one that died had its entry cleared by the eviction sweep.
    #[must_use]
    pub fn attached_pids(&self) -> Vec<u32> {
        self.segment
            .consumer_entries()
            .filter(|(_, entry)| entry.is_occupied())
            .filter_map(|(_, entry)| u32::try_from(entry.pid()).ok())
            .collect()
    }

    /// How many consumers the segment header reports.
    #[must_use]
    pub fn attached_count(&self) -> u32 {
        self.segment.view().attached_consumers
    }

    /// The `shm_fallback_total` the producer has recorded in the header (§6.2).
    #[must_use]
    pub fn producer_fallbacks(&self) -> u64 {
        self.segment.view().fallback_total
    }
}

/// Every segment this daemon brokers.
#[derive(Debug)]
pub struct SegmentRegistry {
    /// The broker, absent when the plane is off or unsupported.
    broker: Option<Arc<SegmentBroker>>,
    /// The broker's accept thread, stopped on drop.
    handle: Option<BrokerHandle>,
    /// Where nodes dial to attach.
    socket_path: Option<PathBuf>,
    /// One record per output with a live segment.
    records: BTreeMap<OutputKey, SegmentRecord>,
    /// How many segments have been created.
    created: u64,
    /// How many were closed.
    closed: u64,
    /// How many creations failed.
    failures: u64,
}

impl SegmentRegistry {
    /// A registry with no plane behind it.
    ///
    /// Every query answers "nothing", every creation fails with
    /// [`DaemonError::ShmUnavailable`], and every route therefore stays on the
    /// reliable daemon path — which is exactly the degradation §6.2 asks for
    /// on a platform without shared memory.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            broker: None,
            handle: None,
            socket_path: None,
            records: BTreeMap::new(),
            created: 0,
            closed: 0,
            failures: 0,
        }
    }

    /// Binds a broker at `socket_path` and starts serving descriptors.
    ///
    /// # Errors
    ///
    /// [`DaemonError::ShmUnavailable`] if the socket cannot be bound — which
    /// on a platform with no shared-memory plane is always, by construction.
    pub fn bind(socket_path: impl AsRef<Path>) -> DaemonResult<Self> {
        let path = socket_path.as_ref().to_path_buf();
        let broker = SegmentBroker::bind(&path).map_err(|error| DaemonError::ShmUnavailable {
            message: error.to_string(),
        })?;
        let handle = SegmentBroker::spawn(&broker);
        Ok(Self {
            broker: Some(broker),
            handle: Some(handle),
            socket_path: Some(path),
            records: BTreeMap::new(),
            created: 0,
            closed: 0,
            failures: 0,
        })
    }

    /// Binds a broker, degrading to [`SegmentRegistry::disabled`] on failure.
    ///
    /// The constructor a daemon uses at start-up: a machine without a
    /// shared-memory plane must still run dataflows.
    #[must_use]
    pub fn bind_or_disabled(socket_path: impl AsRef<Path>) -> Self {
        Self::bind(socket_path).unwrap_or_else(|_| Self::disabled())
    }

    /// Whether a broker is running.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.broker.is_some()
    }

    /// The socket nodes dial to attach (§24.2), if there is one.
    #[must_use]
    pub fn socket_path(&self) -> Option<&Path> {
        self.socket_path.as_deref()
    }

    /// Creates the segment for one output incarnation.
    ///
    /// # Errors
    ///
    /// - [`DaemonError::ShmUnavailable`] when the plane is off.
    /// - [`DaemonError::ShmSegment`] if the segment cannot be created — a pool
    ///   size the geometry refuses, or an out-of-descriptors host.
    pub fn create(
        &mut self,
        key: OutputKey,
        generation: u64,
        config: SegmentConfig,
    ) -> DaemonResult<ShmSegmentSpec> {
        if self.broker.is_none() {
            self.failures = self.failures.saturating_add(1);
            return Err(DaemonError::ShmUnavailable {
                message: "no shared-memory broker is running".into(),
            });
        }
        // A previous incarnation's segment must go first: the key digest
        // differs by generation, so both could otherwise sit in the broker
        // with only one reachable from the daemon's own map.
        self.close(&key);

        let Some(broker) = self.broker.as_ref() else {
            return Err(DaemonError::ShmUnavailable {
                message: "no shared-memory broker is running".into(),
            });
        };
        let segment_key = key.segment_key(generation);
        let segment = broker
            .create_segment(segment_key.clone(), config)
            .map_err(|error| DaemonError::ShmSegment {
                segment: segment_key.canonical(),
                message: error.to_string(),
            })?;

        let spec = ShmSegmentSpec::new(
            segment_key.canonical(),
            generation,
            config.slot_count(),
            u64::from(config.payload_capacity()),
        );
        self.records.insert(
            key.clone(),
            SegmentRecord {
                key,
                generation,
                segment_key,
                spec: spec.clone(),
                segment,
            },
        );
        self.created = self.created.saturating_add(1);
        Ok(spec)
    }

    /// The record for one output, if it has a segment.
    #[must_use]
    pub fn record(&self, key: &OutputKey) -> Option<&SegmentRecord> {
        self.records.get(key)
    }

    /// The specification handed to one output's producer.
    #[must_use]
    pub fn spec(&self, key: &OutputKey) -> Option<ShmSegmentSpec> {
        self.records.get(key).map(|record| record.spec.clone())
    }

    /// Whether one output has a segment.
    #[must_use]
    pub fn has_segment(&self, key: &OutputKey) -> bool {
        self.records.contains_key(key)
    }

    /// The pids attached to one output's ring.
    #[must_use]
    pub fn attached_pids(&self, key: &OutputKey) -> Vec<u32> {
        self.records
            .get(key)
            .map(SegmentRecord::attached_pids)
            .unwrap_or_default()
    }

    /// Marks one output's segment closed and forgets it.
    ///
    /// The segment itself stays alive inside the broker until its consumers
    /// have drained — §6.2's "marks `closed`, lets consumers drain, and
    /// unlinks", of which [`SegmentRegistry::maintain`] performs the last
    /// step.
    pub fn close(&mut self, key: &OutputKey) -> bool {
        let Some(record) = self.records.remove(key) else {
            return false;
        };
        record.segment.mark_closed();
        if let Some(broker) = self.broker.as_ref() {
            broker.close_segment(record.segment_key.digest(), record.generation);
        }
        self.closed = self.closed.saturating_add(1);
        true
    }

    /// Closes every segment one node produces, returning their outputs.
    pub fn close_producer(&mut self, dataflow: DataflowId, node: &NodeId) -> Vec<OutputKey> {
        let keys: Vec<OutputKey> = self
            .records
            .keys()
            .filter(|key| key.dataflow == dataflow && key.node == *node)
            .cloned()
            .collect();
        for key in &keys {
            self.close(key);
        }
        keys
    }

    /// Closes every segment of one dataflow, returning their outputs.
    pub fn close_dataflow(&mut self, dataflow: DataflowId) -> Vec<OutputKey> {
        let keys: Vec<OutputKey> = self
            .records
            .keys()
            .filter(|key| key.dataflow == dataflow)
            .cloned()
            .collect();
        for key in &keys {
            self.close(key);
        }
        keys
    }

    /// One maintenance pass: dead producers detected, stale consumers evicted,
    /// drained segments unlinked (§6.2).
    ///
    /// Returns the outputs whose producers were found dead, so the loop can
    /// downgrade their routes and close their consumers' inputs.
    pub fn maintain(&mut self) -> Vec<OutputKey> {
        let Some(broker) = self.broker.as_ref() else {
            return Vec::new();
        };
        let dead = broker.poll_producers();
        broker.evict_stale_consumers(CONSUMER_STALE_AFTER);
        broker.sweep();

        if dead.is_empty() {
            return Vec::new();
        }
        let digests: Vec<(u128, u64)> = dead
            .iter()
            .map(|key| (key.digest(), key.generation()))
            .collect();
        let orphaned: Vec<OutputKey> = self
            .records
            .iter()
            .filter(|(_, record)| {
                digests.iter().any(|(digest, generation)| {
                    record.segment_key.digest() == *digest && record.generation == *generation
                })
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in &orphaned {
            self.close(key);
        }
        orphaned
    }

    /// How many segments are open.
    #[must_use]
    pub fn open_count(&self) -> usize {
        self.records.len()
    }

    /// How many bytes of shared memory the daemon has mapped (§13).
    #[must_use]
    pub fn mapped_bytes(&self) -> u64 {
        self.records.values().map(SegmentRecord::mapped_bytes).sum()
    }

    /// The `shm_fallback_total` every producer has recorded in its header.
    ///
    /// The producers count their own pool exhaustions in shared memory (§6.2);
    /// the daemon reads them here rather than being told, which means the
    /// figure survives a producer that crashed before it could report.
    #[must_use]
    pub fn producer_fallbacks(&self) -> u64 {
        self.records
            .values()
            .map(SegmentRecord::producer_fallbacks)
            .sum()
    }

    /// How many segments have ever been created.
    #[must_use]
    pub const fn created_total(&self) -> u64 {
        self.created
    }

    /// How many segments have been closed.
    #[must_use]
    pub const fn closed_total(&self) -> u64 {
        self.closed
    }

    /// How many creations failed.
    #[must_use]
    pub const fn failure_total(&self) -> u64 {
        self.failures
    }

    /// The broker's own counters, when there is a broker.
    #[must_use]
    pub fn broker_stats(&self) -> Option<BrokerStats> {
        self.broker.as_ref().map(|broker| broker.stats())
    }

    /// Every open output, in key order.
    pub fn keys(&self) -> impl Iterator<Item = &OutputKey> {
        self.records.keys()
    }

    /// Stops the broker's accept loop and closes every segment.
    pub fn shutdown(&mut self) {
        let keys: Vec<OutputKey> = self.records.keys().cloned().collect();
        for key in &keys {
            self.close(key);
        }
        if let Some(broker) = self.broker.as_ref() {
            broker.sweep();
        }
        if let Some(handle) = self.handle.take() {
            handle.stop();
        }
        self.broker = None;
    }
}

impl Drop for SegmentRegistry {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Default for SegmentRegistry {
    fn default() -> Self {
        Self::disabled()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_wire::DataId;

    use super::*;

    fn key(node: &str, output: &str) -> OutputKey {
        OutputKey::new(
            dataflow(),
            NodeId::new(node).unwrap(),
            DataId::new(output).unwrap(),
        )
    }

    /// A per-test identity.
    ///
    /// A thread-local counter distinguishes tests that share a process. It is
    /// deliberately *not* enough on its own to name anything machine-global —
    /// see `dataflow`, which adds the process identity.
    fn test_id() -> u32 {
        use std::cell::Cell;
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(4096);
        thread_local! {
            static ID: Cell<u32> = const { Cell::new(0) };
        }
        ID.with(|id| {
            if id.get() == 0 {
                id.set(NEXT.fetch_add(1, Ordering::Relaxed));
            }
            id.get()
        })
    }

    /// The dataflow every test in this module works in.
    ///
    /// The POSIX shared-memory name a segment gets is derived from this id,
    /// and that namespace is *machine-global*, not per-process. `cargo
    /// nextest` runs each test in its own process, where a thread-local
    /// counter restarts from the same seed — so on the counter alone every
    /// test in every process picks the same id, and two running concurrently
    /// fail `shm_open` with `EEXIST`. Mixing in the pid makes the id unique
    /// across processes as well as threads, under either test runner.
    fn dataflow() -> DataflowId {
        DataflowId::from_u128((u128::from(std::process::id()) << 32) | u128::from(test_id()))
    }

    fn socket(_name: &str) -> PathBuf {
        // Unix socket paths are capped at `SUN_LEN` (104 bytes on macOS) and
        // the temporary directory already eats most of it, so the name stays
        // short: the per-test id is enough to be unique in this binary.
        std::env::temp_dir().join(format!("as{}-{}.sock", std::process::id(), test_id()))
    }

    fn config() -> SegmentConfig {
        SegmentConfig::new(4, 1024).unwrap()
    }

    #[test]
    fn a_disabled_registry_refuses_everything_without_failing_the_daemon() {
        let mut registry = SegmentRegistry::disabled();
        assert!(!registry.is_enabled());
        assert!(registry.socket_path().is_none());
        assert_eq!(registry.open_count(), 0);
        assert_eq!(registry.mapped_bytes(), 0);
        assert!(registry.broker_stats().is_none());
        assert!(registry.maintain().is_empty());

        let error = registry
            .create(key("camera", "image"), 1, config())
            .expect_err("no plane");
        assert!(matches!(error, DaemonError::ShmUnavailable { .. }));
        assert_eq!(registry.failure_total(), 1);
    }

    #[test]
    fn the_default_registry_is_the_disabled_one() {
        assert!(!SegmentRegistry::default().is_enabled());
    }

    #[cfg(unix)]
    #[test]
    fn a_bound_registry_creates_and_describes_a_segment() {
        let path = socket("create");
        let mut registry = SegmentRegistry::bind(&path).expect("bound");
        assert!(registry.is_enabled());
        assert_eq!(registry.socket_path(), Some(path.as_path()));

        let spec = registry
            .create(key("camera", "image"), 3, config())
            .expect("created");
        assert_eq!(spec.generation, 3);
        assert_eq!(spec.slot_count, 4);
        assert!(spec.name.ends_with("/camera/image/3"), "{}", spec.name);
        assert!(spec.capacity_bytes() >= 4096);

        assert_eq!(registry.open_count(), 1);
        assert_eq!(registry.created_total(), 1);
        assert!(registry.mapped_bytes() > 0);
        assert!(registry.has_segment(&key("camera", "image")));
        assert_eq!(registry.spec(&key("camera", "image")), Some(spec));
    }

    #[cfg(unix)]
    #[test]
    fn a_fresh_segment_has_no_consumers() {
        let mut registry = SegmentRegistry::bind(socket("consumers")).expect("bound");
        registry
            .create(key("camera", "image"), 1, config())
            .expect("created");
        let record = registry.record(&key("camera", "image")).expect("present");

        assert!(record.attached_pids().is_empty());
        assert_eq!(record.attached_count(), 0);
        assert_eq!(record.producer_fallbacks(), 0);
        assert!(!record.is_closed());
        assert_eq!(registry.attached_pids(&key("camera", "image")).len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_new_generation_replaces_the_previous_segment() {
        let mut registry = SegmentRegistry::bind(socket("generation")).expect("bound");
        let first = registry
            .create(key("camera", "image"), 1, config())
            .expect("created");
        let second = registry
            .create(key("camera", "image"), 2, config())
            .expect("created");

        assert_ne!(first.name, second.name, "a new generation, a new segment");
        assert_eq!(registry.open_count(), 1);
        assert_eq!(registry.created_total(), 2);
        assert_eq!(registry.closed_total(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn closing_forgets_the_segment() {
        let mut registry = SegmentRegistry::bind(socket("close")).expect("bound");
        registry
            .create(key("camera", "image"), 1, config())
            .expect("created");

        assert!(registry.close(&key("camera", "image")));
        assert!(!registry.close(&key("camera", "image")));
        assert_eq!(registry.open_count(), 0);
        assert!(registry.spec(&key("camera", "image")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn closing_a_producer_closes_all_of_its_outputs() {
        let mut registry = SegmentRegistry::bind(socket("producer")).expect("bound");
        registry
            .create(key("camera", "image"), 1, config())
            .unwrap();
        registry
            .create(key("camera", "depth"), 1, config())
            .unwrap();
        registry
            .create(key("lidar", "points"), 1, config())
            .unwrap();

        let closed = registry.close_producer(dataflow(), &NodeId::new("camera").unwrap());
        assert_eq!(closed.len(), 2);
        assert_eq!(registry.open_count(), 1);
        assert_eq!(registry.keys().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn closing_a_dataflow_closes_every_segment_in_it() {
        let mut registry = SegmentRegistry::bind(socket("dataflow")).expect("bound");
        registry
            .create(key("camera", "image"), 1, config())
            .unwrap();
        registry
            .create(key("lidar", "points"), 1, config())
            .unwrap();
        registry
            .create(
                OutputKey::new(
                    DataflowId::from_u128(99),
                    NodeId::new("camera").unwrap(),
                    DataId::new("image").unwrap(),
                ),
                1,
                config(),
            )
            .unwrap();

        assert_eq!(registry.close_dataflow(dataflow()).len(), 2);
        assert_eq!(registry.open_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn maintenance_on_a_live_producer_orphans_nothing() {
        let mut registry = SegmentRegistry::bind(socket("maintain")).expect("bound");
        registry
            .create(key("camera", "image"), 1, config())
            .unwrap();
        // The producer pid stamped into a daemon-created segment is the
        // daemon's own, which is by definition alive.
        assert!(registry.maintain().is_empty());
        assert_eq!(registry.open_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn the_broker_reports_its_counters() {
        let mut registry = SegmentRegistry::bind(socket("stats")).expect("bound");
        registry
            .create(key("camera", "image"), 1, config())
            .unwrap();
        let stats = registry.broker_stats().expect("a broker");
        assert_eq!(stats.served, 0, "nobody has attached yet");
    }

    #[cfg(unix)]
    #[test]
    fn a_shutdown_closes_everything_and_is_idempotent() {
        let mut registry = SegmentRegistry::bind(socket("shutdown")).expect("bound");
        registry
            .create(key("camera", "image"), 1, config())
            .unwrap();

        registry.shutdown();
        assert!(!registry.is_enabled());
        assert_eq!(registry.open_count(), 0);
        registry.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn an_impossible_geometry_is_a_typed_error() {
        let mut registry = SegmentRegistry::bind(socket("geometry")).expect("bound");
        // 2 GiB of slots is beyond `MAX_SEGMENT_LEN`; the broker refuses and
        // the daemon reports which segment could not be made.
        let config = SegmentConfig::new(1024, u32::MAX / 2);
        match config {
            Ok(config) => {
                let error = registry
                    .create(key("camera", "image"), 1, config)
                    .expect_err("too large");
                assert!(matches!(error, DaemonError::ShmSegment { .. }));
            }
            Err(_) => {
                // The geometry was refused before it reached the broker, which
                // is the same protection one layer earlier.
            }
        }
    }

    #[test]
    fn binding_an_impossible_path_degrades_rather_than_failing() {
        let registry = SegmentRegistry::bind_or_disabled("/nonexistent-astrs-dir/shm.sock");
        assert!(!registry.is_enabled());
    }
}
