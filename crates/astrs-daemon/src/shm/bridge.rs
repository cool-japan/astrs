//! [`SegmentBridge`] — the daemon reading a ring it also brokers (§6.3).
//!
//! An upgraded route is one the daemon has stepped out of: the producer writes
//! into the ring and the consumers read it, and no byte passes through the
//! event loop. That is the whole point of §6.2, and for the lifetime of an
//! ordinary route it is the end of the story.
//!
//! Two situations break it, and both are transitions rather than steady
//! states:
//!
//! | Situation | Why the daemon needs the bytes |
//! |---|---|
//! | A consumer appears that cannot map the ring — remote (§6.4), dynamic, or a debug tap (§13) | It must be served from somewhere, and the ring is where the data is |
//! | A downgrade is in flight | The producer has been told to go back to the daemon path but may still have committed into the ring first |
//!
//! In both, the producer signals with
//! [`astrs_wire::OutputPayload::Shm`] — a slot reference rather than bytes —
//! and this module turns that reference into bytes by draining the daemon's
//! own [`astrs_shm::Consumer`] on the segment.
//!
//! # Drain on notify, not on a timer
//!
//! The bridge never polls. The `SendMessage` carrying the slot reference *is*
//! the wakeup, so the daemon reads exactly when there is something to read and
//! holds no cursor open across idle periods longer than one event.
//!
//! # The cursor the daemon holds
//!
//! An attached bridge occupies a consumer-table entry like any other reader,
//! which means it participates in reclamation: a slot is not reusable until
//! the bridge has passed it. That is why the bridge attaches at
//! [`astrs_shm::StartPosition::Latest`] (it wants the messages from *now*, not
//! a backlog it was never responsible for) and why it is detached the moment
//! the route no longer needs it.
//!
//! The daemon's own pid is not any node's pid, so the entry it claims is
//! ignored by [`crate::shm::AttachmentLedger`] — an unexpected attacher is not
//! tracked, which is exactly right here.
//!
//! # Examples
//!
//! ```
//! # #[cfg(unix)] {
//! use astrs_daemon::shm::{OutputKey, SegmentBridge, SegmentRegistry};
//! use astrs_shm::{Producer, SegmentConfig};
//! use astrs_wire::{DataId, DataflowId, NodeId};
//!
//! let socket = std::env::temp_dir().join(format!("astrs-bridge-doc-{}.sock", std::process::id()));
//! let mut registry = SegmentRegistry::bind(&socket)?;
//! let key = OutputKey::new(
//!     DataflowId::from_u128(1),
//!     NodeId::new("camera")?,
//!     DataId::new("image")?,
//! );
//! registry.create(key.clone(), 1, SegmentConfig::new(4, 1024)?)?;
//!
//! let record = registry.record(&key).expect("just created");
//! let mut bridge = SegmentBridge::new();
//! bridge.attach(key.clone(), 1, record.segment())?;
//!
//! let mut producer = Producer::new(std::sync::Arc::clone(record.segment()))?;
//! producer.send(b"frame", b"meta")?;
//!
//! let drained = bridge.drain(&key, 8);
//! assert_eq!(drained.len(), 1);
//! assert_eq!(drained[0].payload, b"frame");
//! # }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;

use astrs_shm::{AttachOptions, Consumer, RecvError, Segment, StartPosition};
use astrs_wire::{DataflowId, NodeId};

use crate::error::{DaemonError, DaemonResult};
use crate::shm::keys::OutputKey;

/// How many messages one drain reads before yielding back to the event loop.
///
/// A bound, not a target: the loop must not spend an unbounded slice of one
/// event draining a ring a fast producer keeps refilling, because everything
/// else the daemon does — node exits, health deadlines — waits behind it.
pub const DEFAULT_DRAIN_BATCH: usize = 64;

/// One message read out of a ring by the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgedMessage {
    /// The ring sequence number it carried.
    pub seq: u64,
    /// The payload bytes — an Arrow IPC stream (§6.1), opaque here.
    pub payload: Vec<u8>,
    /// The metadata bytes that rode beside it.
    pub metadata: Vec<u8>,
}

impl BridgedMessage {
    /// How many payload bytes this message carries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.payload.len()
    }

    /// Whether the payload is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }
}

/// The daemon's reader on one ring.
#[derive(Debug)]
struct BridgeConsumer {
    /// The producer incarnation this reader belongs to (§12).
    generation: u64,
    /// The reader itself.
    consumer: Consumer,
}

/// Every ring the daemon is currently reading.
#[derive(Debug, Default)]
pub struct SegmentBridge {
    /// One reader per bridged output.
    consumers: BTreeMap<OutputKey, BridgeConsumer>,
    /// How many messages have been drained.
    drained: u64,
    /// How many messages were lost to a producer outrunning the bridge.
    lagged: u64,
    /// How many drains found the segment closed.
    closed: u64,
}

impl SegmentBridge {
    /// A bridge reading nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            consumers: BTreeMap::new(),
            drained: 0,
            lagged: 0,
            closed: 0,
        }
    }

    /// Attaches the daemon to one output's ring.
    ///
    /// Idempotent for the same generation: attaching twice keeps the first
    /// reader and its cursor, so a second `SendMessage` cannot silently reset
    /// the daemon's position. A *different* generation replaces the reader,
    /// because the ring itself was replaced.
    ///
    /// # Errors
    ///
    /// [`DaemonError::ShmSegment`] if the segment refuses another consumer —
    /// its table is full, or it has already been closed.
    pub fn attach(
        &mut self,
        key: OutputKey,
        generation: u64,
        segment: &Arc<Segment>,
    ) -> DaemonResult<()> {
        if let Some(existing) = self.consumers.get(&key)
            && existing.generation == generation
        {
            return Ok(());
        }
        let options = AttachOptions::new()
            .with_start(StartPosition::Latest)
            .with_doorbell(false);
        let consumer = Consumer::attach(Arc::clone(segment), options).map_err(|error| {
            DaemonError::ShmSegment {
                segment: key.to_string(),
                message: error.to_string(),
            }
        })?;
        self.consumers.insert(
            key,
            BridgeConsumer {
                generation,
                consumer,
            },
        );
        Ok(())
    }

    /// Whether the daemon is reading one output's ring.
    #[must_use]
    pub fn is_attached(&self, key: &OutputKey) -> bool {
        self.consumers.contains_key(key)
    }

    /// The generation the bridge is reading for one output.
    #[must_use]
    pub fn generation_of(&self, key: &OutputKey) -> Option<u64> {
        self.consumers.get(key).map(|entry| entry.generation)
    }

    /// Reads up to `max` messages from one output's ring.
    ///
    /// A lag report is counted and skipped rather than returned: the messages
    /// it names are gone, and the caller's job is to forward what survived.
    /// A closed segment ends the drain and the reader is dropped, because
    /// nothing further can arrive on it.
    pub fn drain(&mut self, key: &OutputKey, max: usize) -> Vec<BridgedMessage> {
        let Some(entry) = self.consumers.get_mut(key) else {
            return Vec::new();
        };
        let mut drained = Vec::new();
        let mut finished = false;
        while drained.len() < max {
            match entry.consumer.try_next() {
                Ok(sample) => {
                    drained.push(BridgedMessage {
                        seq: sample.seq(),
                        payload: sample.to_vec(),
                        metadata: sample.metadata_to_vec(),
                    });
                }
                Err(RecvError::Empty) => break,
                Err(RecvError::Lagged(count)) => {
                    self.lagged = self.lagged.saturating_add(count);
                }
                Err(RecvError::Closed) => {
                    self.closed = self.closed.saturating_add(1);
                    finished = true;
                    break;
                }
                Err(_) => {
                    // A hard failure on a mapping the daemon owns — or a
                    // variant a later `astrs-shm` adds: stop reading rather
                    // than spinning on the same error, and let the reliable
                    // path carry the route.
                    finished = true;
                    break;
                }
            }
        }
        self.drained = self.drained.saturating_add(drained.len() as u64);
        if finished {
            self.detach(key);
        }
        drained
    }

    /// Reads up to [`DEFAULT_DRAIN_BATCH`] messages.
    pub fn drain_batch(&mut self, key: &OutputKey) -> Vec<BridgedMessage> {
        self.drain(key, DEFAULT_DRAIN_BATCH)
    }

    /// Stops reading one output's ring.
    pub fn detach(&mut self, key: &OutputKey) -> bool {
        match self.consumers.remove(key) {
            Some(mut entry) => {
                entry.consumer.detach();
                true
            }
            None => false,
        }
    }

    /// Stops reading every output one node produces.
    pub fn detach_producer(&mut self, dataflow: DataflowId, node: &NodeId) -> usize {
        let keys: Vec<OutputKey> = self
            .consumers
            .keys()
            .filter(|key| key.dataflow == dataflow && key.node == *node)
            .cloned()
            .collect();
        let count = keys.len();
        for key in keys {
            self.detach(&key);
        }
        count
    }

    /// Stops reading every ring of one dataflow.
    pub fn detach_dataflow(&mut self, dataflow: DataflowId) -> usize {
        let keys: Vec<OutputKey> = self
            .consumers
            .keys()
            .filter(|key| key.dataflow == dataflow)
            .cloned()
            .collect();
        let count = keys.len();
        for key in keys {
            self.detach(&key);
        }
        count
    }

    /// How many rings the daemon is reading.
    #[must_use]
    pub fn len(&self) -> usize {
        self.consumers.len()
    }

    /// Whether the daemon is reading nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.consumers.is_empty()
    }

    /// How many messages the bridge has forwarded.
    #[must_use]
    pub const fn drained_total(&self) -> u64 {
        self.drained
    }

    /// How many messages were lost to a producer outrunning the bridge.
    #[must_use]
    pub const fn lagged_total(&self) -> u64 {
        self.lagged
    }

    /// How many drains found their segment closed.
    #[must_use]
    pub const fn closed_total(&self) -> u64 {
        self.closed
    }

    /// Every bridged output, in key order.
    pub fn keys(&self) -> impl Iterator<Item = &OutputKey> {
        self.consumers.keys()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_wire::DataId;

    use super::*;

    /// A per-test identity.
    ///
    /// A thread-local counter distinguishes tests that share a process. It is
    /// deliberately *not* enough on its own to name anything machine-global —
    /// see `dataflow`, which adds the process identity.
    fn test_id() -> u32 {
        use std::cell::Cell;
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(12288);
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

    fn key(node: &str, output: &str) -> OutputKey {
        OutputKey::new(
            dataflow(),
            NodeId::new(node).unwrap(),
            DataId::new(output).unwrap(),
        )
    }

    #[test]
    fn an_empty_bridge_reads_nothing() {
        let mut bridge = SegmentBridge::new();
        assert!(bridge.is_empty());
        assert_eq!(bridge.len(), 0);
        assert!(!bridge.is_attached(&key("camera", "image")));
        assert!(bridge.drain(&key("camera", "image"), 4).is_empty());
        assert!(!bridge.detach(&key("camera", "image")));
        assert_eq!(bridge.drained_total(), 0);
        assert_eq!(bridge.generation_of(&key("camera", "image")), None);
    }

    #[test]
    fn the_default_bridge_is_empty() {
        assert!(SegmentBridge::default().is_empty());
    }

    #[test]
    fn a_bridged_message_reports_its_size() {
        let message = BridgedMessage {
            seq: 1,
            payload: vec![1, 2, 3],
            metadata: Vec::new(),
        };
        assert_eq!(message.len(), 3);
        assert!(!message.is_empty());
        assert!(
            BridgedMessage {
                seq: 0,
                payload: Vec::new(),
                metadata: Vec::new()
            }
            .is_empty()
        );
    }

    #[cfg(unix)]
    mod with_segments {
        #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

        use astrs_shm::{Producer, SegmentConfig};

        use super::*;
        use crate::shm::SegmentRegistry;

        fn socket(_name: &str) -> std::path::PathBuf {
            // Unix socket paths are capped at `SUN_LEN` (104 bytes on macOS)
            // and the temporary directory already eats most of it, so the name
            // stays short: the per-test id is enough to be unique here.
            std::env::temp_dir().join(format!(
                "as{}-{}.sock",
                std::process::id(),
                super::test_id()
            ))
        }

        fn registry(name: &str) -> (SegmentRegistry, OutputKey) {
            let mut registry = SegmentRegistry::bind(socket(name)).expect("bound");
            let key = key("camera", "image");
            registry
                .create(key.clone(), 1, SegmentConfig::new(8, 4096).unwrap())
                .expect("created");
            (registry, key)
        }

        #[test]
        fn the_bridge_reads_what_the_producer_commits() {
            let (registry, key) = registry("read");
            let record = registry.record(&key).expect("present");
            let mut bridge = SegmentBridge::new();
            bridge
                .attach(key.clone(), 1, record.segment())
                .expect("attached");
            assert!(bridge.is_attached(&key));
            assert_eq!(bridge.generation_of(&key), Some(1));

            let mut producer = Producer::new(Arc::clone(record.segment())).expect("producer");
            producer.send(b"first", b"m1").expect("sent");
            producer.send(b"second", b"m2").expect("sent");

            let drained = bridge.drain_batch(&key);
            assert_eq!(drained.len(), 2);
            assert_eq!(drained[0].payload, b"first");
            assert_eq!(drained[0].metadata, b"m1");
            assert_eq!(drained[1].payload, b"second");
            assert_eq!(bridge.drained_total(), 2);
        }

        #[test]
        fn a_drain_is_bounded_by_its_batch() {
            let (registry, key) = registry("batch");
            let record = registry.record(&key).expect("present");
            let mut bridge = SegmentBridge::new();
            bridge.attach(key.clone(), 1, record.segment()).unwrap();

            let mut producer = Producer::new(Arc::clone(record.segment())).unwrap();
            for index in 0..4u8 {
                producer.send(&[index], b"").unwrap();
            }

            assert_eq!(bridge.drain(&key, 2).len(), 2);
            assert_eq!(bridge.drain(&key, 8).len(), 2, "the rest is still there");
            assert!(bridge.drain(&key, 8).is_empty());
        }

        #[test]
        fn the_bridge_starts_at_the_latest_rather_than_the_backlog() {
            let (registry, key) = registry("latest");
            let record = registry.record(&key).expect("present");
            let mut producer = Producer::new(Arc::clone(record.segment())).unwrap();
            producer.send(b"before", b"").unwrap();

            let mut bridge = SegmentBridge::new();
            bridge.attach(key.clone(), 1, record.segment()).unwrap();
            assert!(
                bridge.drain_batch(&key).is_empty(),
                "the daemon is not responsible for a backlog it never brokered"
            );

            producer.send(b"after", b"").unwrap();
            let drained = bridge.drain_batch(&key);
            assert_eq!(drained.len(), 1);
            assert_eq!(drained[0].payload, b"after");
        }

        #[test]
        fn attaching_twice_for_one_generation_keeps_the_cursor() {
            let (registry, key) = registry("idempotent");
            let record = registry.record(&key).expect("present");
            let mut bridge = SegmentBridge::new();
            bridge.attach(key.clone(), 1, record.segment()).unwrap();

            let mut producer = Producer::new(Arc::clone(record.segment())).unwrap();
            producer.send(b"one", b"").unwrap();

            bridge.attach(key.clone(), 1, record.segment()).unwrap();
            assert_eq!(bridge.len(), 1);
            assert_eq!(
                bridge.drain_batch(&key).len(),
                1,
                "the message committed before the second attach is still pending"
            );
        }

        #[test]
        fn a_new_generation_replaces_the_reader() {
            let (mut registry, key) = registry("generation");
            let mut bridge = SegmentBridge::new();
            bridge
                .attach(
                    key.clone(),
                    1,
                    registry.record(&key).expect("present").segment(),
                )
                .unwrap();

            registry
                .create(key.clone(), 2, SegmentConfig::new(8, 4096).unwrap())
                .unwrap();
            bridge
                .attach(
                    key.clone(),
                    2,
                    registry.record(&key).expect("present").segment(),
                )
                .unwrap();

            assert_eq!(bridge.generation_of(&key), Some(2));
            assert_eq!(bridge.len(), 1);
        }

        #[test]
        fn a_closed_segment_detaches_the_bridge() {
            let (mut registry, key) = registry("closed");
            let record = registry.record(&key).expect("present");
            let mut bridge = SegmentBridge::new();
            bridge.attach(key.clone(), 1, record.segment()).unwrap();

            let mut producer = Producer::new(Arc::clone(record.segment())).unwrap();
            producer.send(b"last", b"").unwrap();
            record.segment().mark_closed();

            let drained = bridge.drain_batch(&key);
            assert_eq!(drained.len(), 1, "the tail is drained before the close");
            assert!(bridge.drain_batch(&key).is_empty());
            assert!(!bridge.is_attached(&key), "a closed ring is let go");
            assert_eq!(bridge.closed_total(), 1);
            registry.close(&key);
        }

        #[test]
        fn detaching_a_producer_releases_all_of_its_rings() {
            let mut registry = SegmentRegistry::bind(socket("detach")).expect("bound");
            let mut bridge = SegmentBridge::new();
            for output in ["image", "depth"] {
                let key = key("camera", output);
                registry
                    .create(key.clone(), 1, SegmentConfig::new(4, 1024).unwrap())
                    .unwrap();
                bridge
                    .attach(key.clone(), 1, registry.record(&key).unwrap().segment())
                    .unwrap();
            }
            let other = key("lidar", "points");
            registry
                .create(other.clone(), 1, SegmentConfig::new(4, 1024).unwrap())
                .unwrap();
            bridge
                .attach(other.clone(), 1, registry.record(&other).unwrap().segment())
                .unwrap();

            assert_eq!(
                bridge.detach_producer(dataflow(), &NodeId::new("camera").unwrap()),
                2
            );
            assert_eq!(bridge.len(), 1);
            assert_eq!(bridge.keys().count(), 1);
            assert_eq!(bridge.detach_dataflow(dataflow()), 1);
            assert!(bridge.is_empty());
        }
    }
}
