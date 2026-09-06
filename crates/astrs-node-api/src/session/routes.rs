//! The slow-start route handshake, node side (blueprint §6.3).
//!
//! > Every output starts on the **reliable daemon path**. When the daemon has
//! > confirmed that *all* static same-host consumers attached to the ring […]
//! > it issues `RouteUpgrade` to the producer, which switches to direct SHM
//! > publishing. Any consumer crash/downgrade flips the route back.
//!
//! Two things have to be true for that to be safe, and this module is where
//! both are arranged:
//!
//! 1. **No message may fall between the planes.** The daemon keeps brokering
//!    the route until it sees `RouteUpgradeAck`, so the node acknowledges
//!    *after* it has opened the segment, never before.
//! 2. **A producer belongs to exactly one writer.** `astrs-shm` enforces one
//!    [`astrs_shm::Producer`] per mapping, and its `allocate` needs
//!    `&mut`. So the producer is *handed off* through a [`RouteSlot`] to the
//!    output handle that owns it, rather than shared behind a lock — which is
//!    also what makes `allocate(len) -> SampleMut` expressible at all (§9.1).
//!
//! ```text
//!   reader task            RouteSlot                RawOutput
//!   ───────────            ─────────                ─────────
//!   RouteUpgrade  ──►  Upgrade(Producer)  ──►  takes it, publishes zero-copy
//!   RouteDowngrade──►  Downgrade          ──►  drops it, back to the daemon
//! ```
//!
//! # This is the producer end; [`super::inputs`] is the other one
//!
//! Everything here is the **producer** side: the upgrade is accepted or
//! refused, the producer is handed off, the downgrade takes the output back.
//!
//! The consumer side used to have no counterpart, because the frozen §24.1
//! family had no message for it — [`astrs_wire::NodeEvent::RouteUpgrade`] names
//! an `output: DataId`, which a consumer cannot act on, since what moves for a
//! consumer is an **input**. §7.2's append-only rule made the fix a tail
//! append rather than a break, and
//! [`astrs_wire::NodeEvent::InputRouteUpgrade`] /
//! [`astrs_wire::NodeEvent::InputRouteDowngrade`] are it. [`super::inputs`]
//! acts on them: it attaches the segment, reads samples in place and detaches
//! on any fault, so a consumer needs no more code than a producer does.
//!
//! The two ends are sequenced, and the order matters: the consumer is told
//! first, because §6.3 only offers the producer its upgrade once the daemon has
//! *observed* every consumer in the segment's consumer table — which cannot
//! happen before one attaches.
//!
//! # How the mapping is obtained
//!
//! Through the daemon's **segment broker**, whose socket travels in
//! [`astrs_wire::NodeConfig::shm_broker`] and reaches this module as
//! [`crate::session::SessionShared::shm_broker`]. §6.2 puts the daemon in that
//! position deliberately ("the daemon *knows* attachment state because it
//! brokers the segment fds"), and it is also the only path that works
//! everywhere: a segment created with the default
//! [`astrs_shm::Backing::Auto`] is an anonymous `memfd` on Linux, with no name
//! for [`astrs_shm::Segment::open_named`] to open. Opening by name remains the
//! fallback for a node whose daemon published no broker — the in-process test
//! harness, and any embedder that disabled the plane socket.
//!
//! The name the daemon sends is the segment's **canonical** identity
//! (`{dataflow}/{node}/{output}/{generation}` — what
//! [`astrs_wire::ShmSegmentSpec::name`] documents itself to carry); the hashed
//! short name `shm_open` takes is derived from the key here, where it is
//! needed, and never travels on the wire.
//!
//! # When the segment cannot be opened
//!
//! The node answers `RouteUpgradeAck { accepted: false, reason }` and stays on
//! the daemon path. That is the honest outcome — a node that cannot map the
//! segment (a permissions problem, a stale generation, a platform without the
//! plane) must not pretend otherwise, and §6.2's rule is *never sleep-retry*.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use astrs_shm::{Producer, Segment, SegmentClient, SegmentKey};
use astrs_wire::{DataId, DataflowId, NodeId, PortRef, RouteDowngradeReason, ShmSegmentSpec};

use crate::error::{NodeError, Result};

/// Which plane an output publishes on right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RoutePlane {
    /// The reliable path: payloads ride the daemon connection (§6.3's
    /// starting state).
    Daemon,
    /// The zero-copy path: payloads are written straight into a ring slot.
    Shm,
}

impl RoutePlane {
    /// A stable name for metrics labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Daemon => "daemon",
            Self::Shm => "shm",
        }
    }

    /// Whether publishing on this plane copies nothing.
    #[must_use]
    pub const fn is_zero_copy(self) -> bool {
        matches!(self, Self::Shm)
    }
}

impl core::fmt::Display for RoutePlane {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A route change waiting to be picked up by the output handle that owns the
/// route.
#[derive(Debug)]
pub enum RouteUpdate {
    /// Nothing has changed since the handle last looked.
    Idle,
    /// Switch to the zero-copy plane with this producer (§6.3).
    Upgrade {
        /// The producer, already opened and verified.
        producer: Box<Producer>,
        /// The segment it writes into.
        segment: ShmSegmentSpec,
        /// The consumers the daemon says are attached.
        consumers: Vec<PortRef>,
    },
    /// Go back to the daemon path.
    Downgrade {
        /// Why.
        reason: RouteDowngradeReason,
    },
}

impl RouteUpdate {
    /// Whether this update leaves the plane unchanged.
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }
}

/// The hand-off point for one output's route.
///
/// The reader task writes; the output handle reads and takes. A slot never
/// holds more than one pending update — a downgrade that lands on top of an
/// unclaimed upgrade replaces it, which is correct: the daemon's latest word
/// is the one that matters.
#[derive(Debug)]
pub struct RouteSlot {
    /// The pending update.
    pending: Mutex<RouteUpdate>,
    /// The plane the handle is currently publishing on.
    plane: Mutex<RoutePlane>,
    /// How many upgrades this route has seen.
    upgrades: AtomicU64,
    /// How many downgrades this route has seen.
    downgrades: AtomicU64,
}

impl Default for RouteSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl RouteSlot {
    /// A slot on the daemon path with nothing pending.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(RouteUpdate::Idle),
            plane: Mutex::new(RoutePlane::Daemon),
            upgrades: AtomicU64::new(0),
            downgrades: AtomicU64::new(0),
        }
    }

    /// Publishes an upgrade for the owning handle to pick up.
    pub fn offer_upgrade(
        &self,
        producer: Producer,
        segment: ShmSegmentSpec,
        consumers: Vec<PortRef>,
    ) {
        *lock(&self.pending) = RouteUpdate::Upgrade {
            producer: Box::new(producer),
            segment,
            consumers,
        };
        self.upgrades.fetch_add(1, Ordering::Relaxed);
    }

    /// Publishes a downgrade for the owning handle to pick up.
    pub fn offer_downgrade(&self, reason: RouteDowngradeReason) {
        *lock(&self.pending) = RouteUpdate::Downgrade { reason };
        self.downgrades.fetch_add(1, Ordering::Relaxed);
    }

    /// Takes whatever is pending, leaving the slot idle.
    #[must_use]
    pub fn take(&self) -> RouteUpdate {
        core::mem::replace(&mut lock(&self.pending), RouteUpdate::Idle)
    }

    /// Whether an update is waiting.
    #[must_use]
    pub fn has_update(&self) -> bool {
        !lock(&self.pending).is_idle()
    }

    /// The plane the owning handle reports it is publishing on.
    #[must_use]
    pub fn plane(&self) -> RoutePlane {
        *lock(&self.plane)
    }

    /// Records the plane the owning handle switched to.
    pub fn set_plane(&self, plane: RoutePlane) {
        *lock(&self.plane) = plane;
    }

    /// How many upgrades and downgrades this route has seen.
    #[must_use]
    pub fn transitions(&self) -> (u64, u64) {
        (
            self.upgrades.load(Ordering::Relaxed),
            self.downgrades.load(Ordering::Relaxed),
        )
    }
}

/// Every output's route slot, keyed by output id.
#[derive(Debug, Default)]
pub struct RouteTable {
    /// The slots.
    slots: Mutex<HashMap<DataId, Arc<RouteSlot>>>,
}

impl RouteTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// The slot for `output`, creating it if this is the first look.
    #[must_use]
    pub fn slot(&self, output: &DataId) -> Arc<RouteSlot> {
        let mut slots = lock(&self.slots);
        Arc::clone(
            slots
                .entry(output.clone())
                .or_insert_with(|| Arc::new(RouteSlot::new())),
        )
    }

    /// The slot for `output`, if one exists.
    #[must_use]
    pub fn existing(&self, output: &DataId) -> Option<Arc<RouteSlot>> {
        lock(&self.slots).get(output).map(Arc::clone)
    }

    /// The plane every known output is publishing on.
    #[must_use]
    pub fn planes(&self) -> Vec<(DataId, RoutePlane)> {
        let mut planes: Vec<(DataId, RoutePlane)> = lock(&self.slots)
            .iter()
            .map(|(id, slot)| (id.clone(), slot.plane()))
            .collect();
        planes.sort_by(|left, right| left.0.cmp(&right.0));
        planes
    }

    /// How many outputs are on the zero-copy plane.
    #[must_use]
    pub fn upgraded_count(&self) -> usize {
        lock(&self.slots)
            .values()
            .filter(|slot| slot.plane().is_zero_copy())
            .count()
    }
}

/// Opens the segment a [`RouteUpgrade`](astrs_wire::NodeEvent::RouteUpgrade)
/// names and claims it as this node's producer.
///
/// The segment's identity is checked twice over: the name the daemon sent must
/// be the one the key derived from *this* node's own
/// `(dataflow, node, output, generation)` describes, and the mapped header's
/// digest and generation must match that key too. A stale segment from a
/// previous incarnation therefore cannot be written into, which is the §6.2
/// crash-safety property from the producer's side.
///
/// # How the mapping is obtained
///
/// Through the daemon's segment broker when there is one — `broker` is the
/// socket path from [`astrs_wire::NodeConfig::shm_broker`] — because that is
/// the path §6.2 specifies ("the daemon **knows** attachment state because it
/// brokers the segment fds") and the only one that works on every platform: a
/// segment created with the default [`astrs_shm::Backing::Auto`] is an
/// anonymous `memfd` on Linux, with no name to open at all. Opening by name is
/// kept as the fallback for a node whose daemon published no broker (the
/// in-process test harness, and any embedder that disabled the plane socket).
///
/// # Errors
///
/// [`NodeError::Shm`] when the segment cannot be opened, verified or claimed,
/// and [`NodeError::Pattern`] when the daemon's name does not describe the key
/// this node would derive.
pub fn open_producer(
    dataflow: DataflowId,
    node: &NodeId,
    output: &DataId,
    segment: &ShmSegmentSpec,
    broker: Option<&Path>,
) -> Result<Producer> {
    let key = SegmentKey::new(dataflow, node.clone(), output.clone(), segment.generation);
    // `ShmSegmentSpec::name` is the *canonical* identity
    // (`{dataflow}/{node}/{output}/{generation}`, its own documentation:
    // "embedding `{dataflow_id}/{node_id}/{generation}`"), not the hashed
    // short name `shm_open` takes — that one is derived from the key here,
    // where it is needed, and never travels on the wire.
    let canonical = key.canonical();
    if canonical != segment.name {
        return Err(NodeError::Pattern(format!(
            "route upgrade for `{output}` names segment `{}`, but this node's own key is `{canonical}`",
            segment.name,
        )));
    }

    let mapped = match broker {
        Some(path) => SegmentClient::connect(path)?.attach(&key)?,
        None => {
            let name = key.os_name();
            Segment::open_named(&name, Some(&key))?
        }
    };
    Producer::new(Arc::new(mapped)).map_err(NodeError::Shm)
}

/// Locks a mutex, recovering from a poisoning panic elsewhere.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn output() -> DataId {
        DataId::new("image").unwrap()
    }

    #[test]
    fn a_fresh_route_is_on_the_daemon_path() {
        let table = RouteTable::new();
        let slot = table.slot(&output());
        assert_eq!(slot.plane(), RoutePlane::Daemon);
        assert!(!slot.has_update());
        assert!(slot.take().is_idle());
        assert_eq!(slot.transitions(), (0, 0));
        assert_eq!(table.upgraded_count(), 0);
    }

    #[test]
    fn the_table_hands_out_one_slot_per_output() {
        let table = RouteTable::new();
        let first = table.slot(&output());
        let second = table.slot(&output());
        assert!(Arc::ptr_eq(&first, &second));
        assert!(table.existing(&output()).is_some());
        assert!(table.existing(&DataId::new("other").unwrap()).is_none());
    }

    #[test]
    fn downgrades_are_offered_and_taken_once() {
        let table = RouteTable::new();
        let slot = table.slot(&output());
        slot.offer_downgrade(RouteDowngradeReason::ConsumerDetached {
            consumer: PortRef::from_parts("detect", "frames").unwrap(),
        });
        assert!(slot.has_update());
        assert_eq!(slot.transitions(), (0, 1));

        let update = slot.take();
        assert!(matches!(update, RouteUpdate::Downgrade { .. }));
        assert!(!slot.has_update(), "an update is delivered once");
        assert!(slot.take().is_idle());
    }

    #[test]
    fn the_plane_a_handle_reports_is_remembered() {
        let table = RouteTable::new();
        let slot = table.slot(&output());
        slot.set_plane(RoutePlane::Shm);
        assert_eq!(slot.plane(), RoutePlane::Shm);
        assert_eq!(table.upgraded_count(), 1);
        assert_eq!(table.planes(), vec![(output(), RoutePlane::Shm)]);
        slot.set_plane(RoutePlane::Daemon);
        assert_eq!(table.upgraded_count(), 0);
    }

    #[test]
    fn planes_describe_themselves() {
        assert_eq!(RoutePlane::Daemon.as_str(), "daemon");
        assert_eq!(RoutePlane::Shm.to_string(), "shm");
        assert!(RoutePlane::Shm.is_zero_copy());
        assert!(!RoutePlane::Daemon.is_zero_copy());
    }

    #[test]
    fn a_segment_name_that_does_not_match_the_key_is_refused() {
        let spec = ShmSegmentSpec::new("astrs-not-our-segment", 3, 8, 4096);
        let error = open_producer(
            DataflowId::from_u128(1),
            &NodeId::new("camera").unwrap(),
            &output(),
            &spec,
            None,
        )
        .unwrap_err();
        assert!(matches!(error, NodeError::Pattern(_)), "{error}");
        assert!(error.to_string().contains("this node's own key is"));
    }

    /// The daemon sends the *canonical* identity, which is what the node
    /// compares against — the hashed `shm_open` name is derived locally and
    /// never travels (see `astrs-daemon`'s `SegmentRegistry`, which fills the
    /// spec from `SegmentKey::canonical`).
    #[test]
    fn the_canonical_name_is_what_the_daemon_sends() {
        let dataflow = DataflowId::from_u128(0x7777);
        let node = NodeId::new("camera").unwrap();
        let key = SegmentKey::new(dataflow, node.clone(), output(), 2);
        assert!(key.canonical().ends_with("/camera/image/2"));

        // The hashed form is refused, because it is not what §6.2's wire
        // vocabulary carries.
        let hashed = ShmSegmentSpec::new(key.os_name().as_str(), 2, 8, 4096);
        assert!(open_producer(dataflow, &node, &output(), &hashed, None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_real_segment_is_opened_verified_and_claimed() {
        use astrs_shm::{Backing, SegmentConfig};

        let dataflow = DataflowId::from_u128(0x5150);
        let node = NodeId::new("camera").unwrap();
        let key = SegmentKey::new(dataflow, node.clone(), output(), 4);
        // Named explicitly: this test exercises the *no broker* fallback,
        // which opens by name, and the default backing is an anonymous
        // `memfd` on Linux with no name to open.
        let config = SegmentConfig::new(4, 1024)
            .unwrap()
            .with_backing(Backing::Named);
        let Ok(segment) = Segment::create(key.clone(), config) else {
            // A sandbox without a usable shared-memory backing is not a test
            // failure; the plane is an optimisation (§6.2).
            return;
        };
        let spec = ShmSegmentSpec::new(key.canonical(), 4, 4, 1024);
        let producer = open_producer(dataflow, &node, &output(), &spec, None);
        assert!(producer.is_ok(), "{:?}", producer.err());

        // A generation the node does not believe in is refused by the name
        // check before any mapping happens.
        let stale = ShmSegmentSpec::new(key.canonical(), 5, 4, 1024);
        assert!(open_producer(dataflow, &node, &output(), &stale, None).is_err());

        drop(producer);
        let _ = segment.unlink();
    }
}
