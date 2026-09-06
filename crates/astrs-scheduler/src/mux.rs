//! [`EventMux`] — the prioritized, fair, multi-input merged event loop
//! (blueprint §11.3, §4.3).
//!
//! One `EventMux` multiplexes every [`crate::InputQueue`] a node (or
//! operator) owns into the single stream its merged event loop actually
//! reads from. Two rules, applied in order:
//!
//! 1. **The control lane strictly pre-empts the data lane.** As long as any
//!    input registered on [`PriorityLane::Control`] has a message queued,
//!    it is delivered before anything from [`PriorityLane::Data`] — no
//!    matter how large the data backlog is. This is what keeps a `Stop` or
//!    a parameter update from getting stuck behind a queue of camera
//!    frames.
//! 2. **Within a lane, delivery is fair round robin across inputs.** No
//!    single hot input can starve a quieter one: the mux remembers which
//!    input it served last and resumes scanning immediately after it, so
//!    every input in a lane is visited at least once per full pass over
//!    that lane (see [`EventMux::try_recv`]'s docs for the exact bound).
//!
//! # Eviction immunity is not delivery priority
//!
//! These are independent knobs. A message's
//! [`MetadataView::is_evict_immune`] only protects it from being *dropped*
//! while queued (blueprint §11.2, enforced by [`crate::InputQueue`]
//! itself); it says nothing about *when* the mux delivers it. A `Stop`
//! registered on the data lane is never lost, but it still waits its turn
//! behind that lane's ordinary backlog. A caller that needs a control-class
//! message *seen promptly*, not merely *kept*, must register that input
//! with `lane: `[`PriorityLane::Control`], not rely on immunity alone.
//!
//! # Producers bypass the mux's own lock
//!
//! [`EventMux::register_input`] returns an [`InputHandle`] holding its own
//! `Arc` to the underlying queue. A producer task pushes through that
//! handle directly — it never touches the mux's internal registry lock, so
//! many producers and the single consumer calling
//! [`recv`](EventMux::recv)/[`try_recv`](EventMux::try_recv) contend only
//! on each input's own small per-queue lock, never on each other.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use astrs_wire::{DataId, InputSpec, PriorityLane, QueuePolicy};
use tokio::sync::Notify;

use crate::error::{Result, SchedulerError};
use crate::metadata_view::MetadataView;
use crate::queue::{InputQueue, PushReport, QueueSnapshot};
use crate::sync_util::lock;

/// One priority lane's registration order and round-robin position.
#[derive(Default)]
struct Lane {
    /// Registered input ids, in registration order. Membership only
    /// changes on [`EventMux::register_input`]/`unregister_input`, which
    /// are rare compared to `recv`/`try_recv`.
    ids: Vec<DataId>,
    /// The id served by the most recent successful scan of this lane, if
    /// any. Anchoring rotation to an id — rather than a numeric index —
    /// means an `unregister_input` that shifts the `ids` list can never
    /// desynchronize the rotation position: if the remembered id is gone,
    /// the next scan simply starts from the front.
    last_served: Option<DataId>,
}

impl Lane {
    /// Scans this lane starting just after [`Lane::last_served`], returning
    /// the first input with a ready message.
    ///
    /// Visits at most `self.ids.len()` inputs, so a full call always
    /// terminates in `O(n)` regardless of how many are empty.
    fn scan<T: MetadataView>(
        &mut self,
        queues: &HashMap<DataId, Arc<InputQueue<T>>>,
    ) -> Option<(DataId, T)> {
        let n = self.ids.len();
        if n == 0 {
            return None;
        }
        let start = match &self.last_served {
            Some(id) => self
                .ids
                .iter()
                .position(|candidate| candidate == id)
                .map_or(0, |position| (position + 1) % n),
            None => 0,
        };
        for offset in 0..n {
            let idx = (start + offset) % n;
            let id = &self.ids[idx];
            if let Some(queue) = queues.get(id)
                && let Some(item) = queue.pop()
            {
                let id = id.clone();
                self.last_served = Some(id.clone());
                return Some((id, item));
            }
        }
        None
    }
}

/// The mutex-guarded registry: which inputs exist, which lane each is on,
/// and their queues.
struct Registry<T> {
    control: Lane,
    data: Lane,
    queues: HashMap<DataId, Arc<InputQueue<T>>>,
}

/// A producer-side handle to one registered input.
///
/// Returned by [`EventMux::register_input`]. Pushing through this handle —
/// rather than reaching back into the mux by id — is what lets many
/// producer tasks feed the mux concurrently without contending on its
/// registry lock (see the module docs).
pub struct InputHandle<T> {
    id: DataId,
    queue: Arc<InputQueue<T>>,
    notify: Arc<Notify>,
}

// See `InputQueue`'s own `Debug` impl for why this is hand-written: no `T:
// Debug` bound, and a summary (id + the underlying queue's own `Debug`)
// rather than an attempt to print queued messages.
impl<T> std::fmt::Debug for InputHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputHandle")
            .field("id", &self.id)
            .field("queue", &self.queue)
            .finish_non_exhaustive()
    }
}

// Hand-written rather than derived: `#[derive(Clone)]` would add a `T: Clone`
// bound, and a handle holds nothing of `T` — only the input's id and two
// `Arc`s. Cloning is what makes the module documentation's "many producer
// tasks feed the mux concurrently" actually expressible: each task keeps its
// own handle to the same queue.
impl<T> Clone for InputHandle<T> {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            queue: Arc::clone(&self.queue),
            notify: Arc::clone(&self.notify),
        }
    }
}

impl<T> InputHandle<T> {
    /// The input's id.
    #[must_use]
    pub fn id(&self) -> &DataId {
        &self.id
    }

    /// A lock-free snapshot of this input's queue counters.
    #[must_use]
    pub fn snapshot(&self) -> QueueSnapshot {
        self.queue.snapshot()
    }
}

impl<T: MetadataView> InputHandle<T> {
    /// Pushes a message onto this input's queue and wakes a waiting
    /// [`EventMux::recv`], if any.
    pub fn push(&self, item: T) -> PushReport {
        let report = self.queue.push(item);
        self.notify.notify_one();
        report
    }
}

/// A prioritized, fair, multi-input event mux (blueprint §11.3).
///
/// See the module docs for the priority and fairness rules. Generic over
/// the message type `T`; registration and inspection do not require `T:
/// `[`MetadataView`], but delivery ([`recv`](EventMux::recv)/
/// [`try_recv`](EventMux::try_recv)) does, since honoring eviction immunity
/// on the way out of a queue requires being able to ask a message whether
/// it is immune.
///
/// # Lock ordering
///
/// `try_recv` holds the mux's own registry lock only for the duration of
/// its scan, and acquires at most one input queue's internal lock at a
/// time within that scan (never two at once). [`InputHandle::push`] never
/// touches the registry lock at all. Any code extending this module must
/// preserve that "registry lock, then at most one queue lock, never the
/// reverse" order to keep the mux deadlock-free.
///
/// # Examples
///
/// ```
/// use astrs_scheduler::{Envelope, EventMux};
/// use astrs_wire::{DataId, PriorityLane, QueuePolicy};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let mux: EventMux<Envelope<u32>> = EventMux::new();
/// let data = mux.register_input(DataId::new("frames")?, 4, QueuePolicy::DropOldest, PriorityLane::Data)?;
/// let control = mux.register_input(DataId::new("ctrl")?, 4, QueuePolicy::DropOldest, PriorityLane::Control)?;
///
/// data.push(Envelope::new(1));
/// control.push(Envelope::new(2));
///
/// // The control lane is served first, even though `data` was pushed first.
/// let (first, _) = mux.try_recv().expect("something is queued");
/// assert_eq!(first, DataId::new("ctrl")?);
/// # Ok(())
/// # }
/// ```
pub struct EventMux<T> {
    registry: Mutex<Registry<T>>,
    notify: Arc<Notify>,
}

// Hand-written for the same reason as `InputHandle`'s: no `T: Debug` bound
// (the payload is the caller's, and a mux full of camera frames is not
// something a log line should try to render), and a summary — which inputs
// are registered on which lane, and what each queue's counters say — rather
// than the messages themselves. A daemon holding one of these inside its own
// `#[derive(Debug)]` state is the intended caller.
impl<T> std::fmt::Debug for EventMux<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = f.debug_struct("EventMux");
        match self.registry.lock() {
            Ok(registry) => out
                .field("control", &registry.control.ids)
                .field("data", &registry.data.ids)
                .field("queues", &registry.queues.len())
                .finish(),
            // A poisoned registry still has to render: `Debug` is what a
            // caller reaches for *while* diagnosing the panic that poisoned
            // it, so refusing to print would be exactly backwards.
            Err(_) => out.field("registry", &"<poisoned>").finish_non_exhaustive(),
        }
    }
}

impl<T> Default for EventMux<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> EventMux<T> {
    /// Creates an empty mux with no registered inputs.
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: Mutex::new(Registry {
                control: Lane::default(),
                data: Lane::default(),
                queues: HashMap::new(),
            }),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Registers a new input with the given queue configuration and lane,
    /// returning the producer-side handle for it.
    ///
    /// # Errors
    ///
    /// [`SchedulerError::ZeroCapacity`] if `capacity` is zero.
    /// [`SchedulerError::DuplicateInput`] if `id` is already registered.
    pub fn register_input(
        &self,
        id: DataId,
        capacity: u32,
        policy: QueuePolicy,
        lane: PriorityLane,
    ) -> Result<InputHandle<T>> {
        let queue = Arc::new(InputQueue::new(capacity, policy)?);
        let mut registry = lock(&self.registry);
        if registry.queues.contains_key(&id) {
            return Err(SchedulerError::DuplicateInput(id));
        }
        let lane_list = match lane {
            PriorityLane::Control => &mut registry.control,
            PriorityLane::Data => &mut registry.data,
            // `PriorityLane` is `#[non_exhaustive]` upstream. A future
            // variant this build does not know how to prioritize is placed
            // on the data lane: the conservative choice is to never assume
            // an unrecognized lane should pre-empt anything.
            _ => &mut registry.data,
        };
        lane_list.ids.push(id.clone());
        registry.queues.insert(id.clone(), Arc::clone(&queue));
        drop(registry);
        Ok(InputHandle {
            id,
            queue,
            notify: Arc::clone(&self.notify),
        })
    }

    /// Registers a new input directly from a manifest-resolved
    /// [`InputSpec`], reusing its `id`, `queue_size`, `queue_policy`, and
    /// `priority_lane` fields — the mux-level counterpart of
    /// [`InputQueue::from_spec`], so a daemon or the node API wiring up a
    /// node's `inputs:` list does not need to unpack each field by hand.
    ///
    /// # Errors
    ///
    /// [`SchedulerError::ZeroCapacity`] if `spec.queue_size` is zero.
    /// [`SchedulerError::DuplicateInput`] if `spec.id` is already
    /// registered.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_scheduler::{Envelope, EventMux};
    /// use astrs_wire::{DataId, InputSpec, PortRef, PriorityLane, QueuePolicy};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mux: EventMux<Envelope<u32>> = EventMux::new();
    /// let mut spec = InputSpec::new(DataId::new("frames")?, PortRef::from_parts("camera", "image")?)
    ///     .with_queue(4, QueuePolicy::DropOldest);
    /// spec.priority_lane = PriorityLane::Control;
    ///
    /// let handle = mux.register_from_spec(&spec)?;
    /// assert_eq!(handle.id(), &spec.id);
    /// assert_eq!(mux.control_inputs(), vec![spec.id.clone()]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn register_from_spec(&self, spec: &InputSpec) -> Result<InputHandle<T>> {
        self.register_input(
            spec.id.clone(),
            spec.queue_size,
            spec.queue_policy,
            spec.priority_lane,
        )
    }

    /// Removes an input, returning whether it was present.
    ///
    /// Draining or forwarding whatever was still queued for `id` is the
    /// caller's responsibility, done before calling this if it matters —
    /// `unregister_input` simply drops the queue.
    pub fn unregister_input(&self, id: &DataId) -> bool {
        let mut registry = lock(&self.registry);
        let removed = registry.queues.remove(id).is_some();
        if removed {
            registry.control.ids.retain(|existing| existing != id);
            registry.data.ids.retain(|existing| existing != id);
        }
        removed
    }

    /// The ids currently registered on the control lane, in registration
    /// order.
    #[must_use]
    pub fn control_inputs(&self) -> Vec<DataId> {
        lock(&self.registry).control.ids.clone()
    }

    /// The ids currently registered on the data lane, in registration
    /// order.
    #[must_use]
    pub fn data_inputs(&self) -> Vec<DataId> {
        lock(&self.registry).data.ids.clone()
    }

    /// A lock-free snapshot of one input's queue counters, if it is
    /// registered.
    #[must_use]
    pub fn queue_snapshot(&self, id: &DataId) -> Option<QueueSnapshot> {
        lock(&self.registry)
            .queues
            .get(id)
            .map(|queue| queue.snapshot())
    }

    /// A lock-free snapshot of every registered input's queue counters, one
    /// entry per input, in no particular order.
    ///
    /// The single call a blueprint §13 2-second metrics sampler needs, in
    /// place of concatenating [`EventMux::control_inputs`] and
    /// [`EventMux::data_inputs`] and then calling
    /// [`EventMux::queue_snapshot`] once per id. Each per-queue snapshot is
    /// still taken independently (see [`QueueSnapshot`]'s own docs on what
    /// that does and does not guarantee under concurrent pushes/pops); this
    /// only saves the registry lookups, not the inherent per-queue
    /// point-in-time nature of each entry.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_scheduler::{Envelope, EventMux};
    /// use astrs_wire::{DataId, PriorityLane, QueuePolicy};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mux: EventMux<Envelope<u32>> = EventMux::new();
    /// let a = mux.register_input(DataId::new("a")?, 4, QueuePolicy::DropOldest, PriorityLane::Data)?;
    /// let b = mux.register_input(DataId::new("b")?, 4, QueuePolicy::DropOldest, PriorityLane::Control)?;
    /// a.push(Envelope::new(1));
    ///
    /// let mut snapshots = mux.snapshot_all();
    /// snapshots.sort_by(|(id, _), (other, _)| id.as_str().cmp(other.as_str()));
    /// assert_eq!(snapshots.len(), 2);
    /// assert_eq!(snapshots[0].0, DataId::new("a")?);
    /// assert_eq!(snapshots[0].1.depth, 1);
    /// assert_eq!(snapshots[1].1.depth, 0);
    /// # let _ = b;
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn snapshot_all(&self) -> Vec<(DataId, QueueSnapshot)> {
        lock(&self.registry)
            .queues
            .iter()
            .map(|(id, queue)| (id.clone(), queue.snapshot()))
            .collect()
    }
}

impl<T: MetadataView> EventMux<T> {
    /// Attempts to deliver one message without waiting.
    ///
    /// Scans the control lane first (in fair round-robin order across its
    /// inputs); only if it is entirely empty does this look at the data
    /// lane. Returns `None` if both lanes have nothing ready.
    ///
    /// # Fairness bound
    ///
    /// Within a lane of `n` inputs, if some input has had a message
    /// continuously available since this lane's rotation position last
    /// passed it, that input is served within the next `n` successful
    /// deliveries from this lane — it can be skipped by other inputs at
    /// most `n - 1` times in a row before its turn comes up. A hot input
    /// competing with the lane's other `n - 1` inputs cannot starve any of
    /// them past that bound.
    pub fn try_recv(&self) -> Option<(DataId, T)> {
        let mut registry = lock(&self.registry);
        // Destructured (rather than `registry.control.scan(&registry.queues)`
        // inline) so the borrow checker sees `control`/`data`/`queues` as
        // the disjoint fields they are: a single compound place expression
        // borrowing one field as the method receiver while another is used
        // as an argument does not get that treatment, even though the
        // fields themselves never overlap.
        let Registry {
            control,
            data,
            queues,
        } = &mut *registry;
        if let Some(hit) = control.scan(queues) {
            return Some(hit);
        }
        data.scan(queues)
    }

    /// Waits for and delivers the next message, honoring the same
    /// priority and fairness rules as [`EventMux::try_recv`].
    ///
    /// Waker-correct: uses the documented
    /// "create-the-notification-future-before-checking-the-condition"
    /// pattern ([`tokio::sync::Notify`]), so a message pushed at any point
    /// after this call starts is guaranteed to be observed, never lost to
    /// a race between checking and sleeping.
    pub async fn recv(&self) -> (DataId, T) {
        loop {
            let notified = self.notify.notified();
            if let Some(hit) = self.try_recv() {
                return hit;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::metadata_view::Envelope;
    use crate::queue::PushOutcome;

    type Mux = EventMux<Envelope<u32>>;

    fn id(name: &str) -> DataId {
        DataId::new(name).unwrap()
    }

    #[test]
    fn the_mux_renders_a_summary_rather_than_its_messages() {
        let mux: EventMux<Envelope<u32>> = EventMux::new();
        let frames = DataId::new("frames").unwrap();
        let ctrl = DataId::new("ctrl").unwrap();
        mux.register_input(
            frames.clone(),
            4,
            QueuePolicy::DropOldest,
            PriorityLane::Data,
        )
        .unwrap()
        .push(Envelope::new(1));
        mux.register_input(
            ctrl.clone(),
            4,
            QueuePolicy::DropOldest,
            PriorityLane::Control,
        )
        .unwrap();

        let rendered = format!("{mux:?}");
        assert!(rendered.starts_with("EventMux"), "{rendered}");
        assert!(rendered.contains("frames"), "{rendered}");
        assert!(rendered.contains("ctrl"), "{rendered}");
        assert!(rendered.contains("queues: 2"), "{rendered}");
    }

    #[test]
    fn handles_clone_onto_the_same_queue() {
        let mux: Mux = EventMux::new();
        let first = mux
            .register_input(id("a"), 4, QueuePolicy::DropOldest, PriorityLane::Data)
            .unwrap();
        let second = first.clone();
        assert_eq!(second.id(), first.id());
        first.push(Envelope::new(1));
        second.push(Envelope::new(2));
        assert_eq!(second.snapshot().depth, 2, "both handles feed one queue");
        assert_eq!(mux.try_recv().unwrap().1.payload, 1);
        assert_eq!(mux.try_recv().unwrap().1.payload, 2);
    }

    #[test]
    fn empty_mux_try_recv_is_none() {
        let mux: Mux = EventMux::new();
        assert!(mux.try_recv().is_none());
    }

    #[test]
    fn single_input_round_trips() {
        let mux: Mux = EventMux::new();
        let handle = mux
            .register_input(id("a"), 4, QueuePolicy::DropOldest, PriorityLane::Data)
            .unwrap();
        handle.push(Envelope::new(1));
        let (recv_id, item) = mux.try_recv().unwrap();
        assert_eq!(recv_id, id("a"));
        assert_eq!(item.payload, 1);
        assert!(mux.try_recv().is_none());
    }

    #[test]
    fn duplicate_registration_is_rejected() {
        let mux: Mux = EventMux::new();
        mux.register_input(id("a"), 4, QueuePolicy::DropOldest, PriorityLane::Data)
            .unwrap();
        let err = mux
            .register_input(id("a"), 4, QueuePolicy::DropOldest, PriorityLane::Data)
            .unwrap_err();
        assert_eq!(err, SchedulerError::DuplicateInput(id("a")));
    }

    #[test]
    fn control_lane_preempts_data_lane_regardless_of_backlog_size() {
        let mux: Mux = EventMux::new();
        let data = mux
            .register_input(id("data"), 16, QueuePolicy::DropOldest, PriorityLane::Data)
            .unwrap();
        let control = mux
            .register_input(
                id("control"),
                4,
                QueuePolicy::DropOldest,
                PriorityLane::Control,
            )
            .unwrap();
        for n in 0..10 {
            data.push(Envelope::new(n));
        }
        control.push(Envelope::new(999));
        let (recv_id, item) = mux.try_recv().unwrap();
        assert_eq!(recv_id, id("control"));
        assert_eq!(item.payload, 999);
    }

    #[test]
    fn round_robin_alternates_between_two_ready_inputs() {
        let mux: Mux = EventMux::new();
        let a = mux
            .register_input(id("a"), 4, QueuePolicy::DropOldest, PriorityLane::Data)
            .unwrap();
        let b = mux
            .register_input(id("b"), 4, QueuePolicy::DropOldest, PriorityLane::Data)
            .unwrap();
        for n in 0..4u32 {
            a.push(Envelope::new(n));
            b.push(Envelope::new(n + 100));
        }
        let mut order = Vec::new();
        while let Some((recv_id, _)) = mux.try_recv() {
            order.push(recv_id);
        }
        assert_eq!(order.len(), 8);
        // Perfectly alternating, in registration order, starting with `a`.
        for (i, recv_id) in order.iter().enumerate() {
            let expected = if i % 2 == 0 { id("a") } else { id("b") };
            assert_eq!(*recv_id, expected, "position {i}");
        }
    }

    #[test]
    fn unregister_removes_the_input_and_scanning_stays_stable() {
        let mux: Mux = EventMux::new();
        let a = mux
            .register_input(id("a"), 4, QueuePolicy::DropOldest, PriorityLane::Data)
            .unwrap();
        let b = mux
            .register_input(id("b"), 4, QueuePolicy::DropOldest, PriorityLane::Data)
            .unwrap();
        a.push(Envelope::new(1));
        let (first, _) = mux.try_recv().unwrap();
        assert_eq!(first, id("a"));

        assert!(mux.unregister_input(&id("a")));
        assert!(!mux.unregister_input(&id("a")), "already removed");

        b.push(Envelope::new(2));
        let (second, item) = mux.try_recv().unwrap();
        assert_eq!(second, id("b"));
        assert_eq!(item.payload, 2);
        assert_eq!(mux.data_inputs(), vec![id("b")]);
    }

    #[test]
    fn queue_snapshot_reflects_pushes_and_is_none_for_unknown_ids() {
        let mux: Mux = EventMux::new();
        let a = mux
            .register_input(id("a"), 4, QueuePolicy::DropOldest, PriorityLane::Data)
            .unwrap();
        a.push(Envelope::new(1));
        assert_eq!(mux.queue_snapshot(&id("a")).unwrap().depth, 1);
        assert!(mux.queue_snapshot(&id("missing")).is_none());
    }

    #[test]
    fn snapshot_all_covers_every_registered_input_across_both_lanes() {
        let mux: Mux = EventMux::new();
        let a = mux
            .register_input(id("a"), 4, QueuePolicy::DropOldest, PriorityLane::Data)
            .unwrap();
        let _b = mux
            .register_input(id("b"), 4, QueuePolicy::DropOldest, PriorityLane::Control)
            .unwrap();
        a.push(Envelope::new(1));
        a.push(Envelope::new(2));

        let mut snapshots = mux.snapshot_all();
        snapshots.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].0, id("a"));
        assert_eq!(snapshots[0].1.depth, 2);
        assert_eq!(snapshots[1].0, id("b"));
        assert_eq!(snapshots[1].1.depth, 0);

        assert!(mux.unregister_input(&id("a")));
        assert_eq!(
            mux.snapshot_all().len(),
            1,
            "unregistering shrinks the snapshot too"
        );
    }

    #[test]
    fn snapshot_all_of_an_empty_mux_is_empty() {
        let mux: Mux = EventMux::new();
        assert!(mux.snapshot_all().is_empty());
    }

    #[test]
    fn register_from_spec_reuses_id_queue_settings_and_lane() {
        let mux: Mux = EventMux::new();
        let mut spec = astrs_wire::InputSpec::new(
            id("frames"),
            astrs_wire::PortRef::new(astrs_wire::NodeId::new("camera").unwrap(), id("image")),
        )
        .with_queue(2, QueuePolicy::Backpressure);
        spec.priority_lane = PriorityLane::Control;

        let handle = mux.register_from_spec(&spec).unwrap();
        assert_eq!(handle.id(), &spec.id);
        assert_eq!(mux.control_inputs(), vec![id("frames")]);
        assert!(mux.data_inputs().is_empty());

        // The queue settings themselves came along too: `Backpressure`
        // buffers 10x `queue_size` (2) before dropping, not `DropOldest`'s
        // default of evicting at capacity.
        for n in 0..20u32 {
            let report = handle.push(Envelope::new(n));
            assert_eq!(report.outcome, PushOutcome::Enqueued, "message {n}");
        }
        assert_eq!(mux.queue_snapshot(&id("frames")).unwrap().depth, 20);
    }

    #[test]
    fn register_from_spec_rejects_a_zero_queue_size() {
        let mux: Mux = EventMux::new();
        let spec = astrs_wire::InputSpec::new(
            id("frames"),
            astrs_wire::PortRef::new(astrs_wire::NodeId::new("camera").unwrap(), id("image")),
        )
        .with_queue(0, QueuePolicy::DropOldest);
        assert_eq!(
            mux.register_from_spec(&spec).unwrap_err(),
            SchedulerError::ZeroCapacity
        );
    }

    #[tokio::test]
    async fn recv_wakes_up_when_a_message_is_pushed_after_it_starts_waiting() {
        let mux: Arc<Mux> = Arc::new(EventMux::new());
        let handle = mux
            .register_input(id("a"), 4, QueuePolicy::DropOldest, PriorityLane::Data)
            .unwrap();

        let waiter = {
            let mux = Arc::clone(&mux);
            tokio::spawn(async move { mux.recv().await })
        };
        // Give the waiter a chance to actually park in `notified().await`
        // before the push, exercising the wakeup path rather than the
        // immediate-`try_recv`-hit path.
        tokio::task::yield_now().await;
        handle.push(Envelope::new(7));

        let (recv_id, item) = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recv_id, id("a"));
        assert_eq!(item.payload, 7);
    }
}
