//! The coordinator hub: the shared state every connection's session task
//! and the background watchdogs operate on (blueprint §4.2, §4.3).
//!
//! [`Coordinator`] is a cheap-to-clone handle — every field is `Arc`-backed
//! — so each accepted connection gets its own clone and reaches the same
//! registries, the same store and the same clock as every other one. The
//! three live registries are guarded by a plain [`std::sync::Mutex`] rather
//! than an async one: every critical section that touches one is a few
//! lines of synchronous map/set logic with no `.await` inside it (a
//! `WaitForBuild`-style blocking call registers a `oneshot` receiver and
//! *then* awaits it, outside the lock — see [`crate::handlers::lifecycle`])
//! so a plain mutex is both correct and lighter than the async
//! alternative.

use std::sync::{Arc, Mutex};

use astrs_store::AsyncStore;
use astrs_time::{HlcClock, SystemClock};

use crate::config::CoordinatorConfig;
use crate::registry::{DaemonRegistry, DataflowRegistry, PendingLogFetches, SubscriptionRegistry};
use crate::trace::TraceBuffer;

/// The coordinator's shared state.
#[derive(Clone)]
pub struct Coordinator {
    /// The tunable policy this coordinator was started with.
    pub config: Arc<CoordinatorConfig>,
    /// Durable state: params, the dataflow/daemon registries, the build
    /// cache and the mutation catch-up log.
    pub store: AsyncStore,
    /// The cluster's hybrid logical clock.
    pub clock: Arc<HlcClock<SystemClock>>,
    /// Connected daemons and their outbound channels.
    ///
    /// Private, unlike `config`/`store`/`clock` above: every one of this
    /// crate's own call sites reaches it through
    /// [`Coordinator::daemons`], never the field directly, and keeping it
    /// that way is also what keeps this type's own registry types
    /// (defined in the private `registry` module) out of this
    /// `pub`-exported struct's public field list.
    daemons: Arc<Mutex<DaemonRegistry>>,
    /// Open log/topic subscriptions. See the note on
    /// [`Coordinator::daemons`].
    subscriptions: Arc<Mutex<SubscriptionRegistry>>,
    /// Dataflows currently building, starting, running or stopping. See
    /// the note on [`Coordinator::daemons`].
    dataflows: Arc<Mutex<DataflowRegistry>>,
    /// One-shot `Logs` fetches currently fanned out to daemons, keyed by
    /// their correlation id. See the note on [`Coordinator::daemons`].
    pending_logs: Arc<Mutex<PendingLogFetches>>,
    /// This coordinator's own finished request spans, backing `GetTraces`
    /// (blueprint §13). See the note on [`Coordinator::daemons`].
    traces: Arc<Mutex<TraceBuffer>>,
    /// The replicated-registry handle, when this coordinator was started as
    /// one of a Raft set (blueprint §22). `None` — and, without the `ha`
    /// feature, absent entirely — for the ordinary single-coordinator
    /// deployment, in which case every path behaves exactly as it always
    /// has.
    #[cfg(feature = "ha")]
    ha: Option<Arc<crate::ha::HaHandle>>,
    /// The next `request` correlation id for a one-shot `Logs` fan-out to
    /// daemons (blueprint §7.3), and the id [`crate::handlers::dispatch`]
    /// stamps on every span it records — a plain counter, since both uses
    /// correlate things *within one coordinator process's lifetime* only,
    /// never persisted.
    next_request_id: Arc<std::sync::atomic::AtomicU64>,
}

impl Coordinator {
    /// Builds a coordinator hub over an already-opened store.
    #[must_use]
    pub fn new(config: CoordinatorConfig, store: AsyncStore) -> Self {
        Self {
            config: Arc::new(config),
            store,
            clock: Arc::new(HlcClock::system()),
            daemons: Arc::new(Mutex::new(DaemonRegistry::new())),
            subscriptions: Arc::new(Mutex::new(SubscriptionRegistry::new())),
            dataflows: Arc::new(Mutex::new(DataflowRegistry::new())),
            pending_logs: Arc::new(Mutex::new(PendingLogFetches::new())),
            traces: Arc::new(Mutex::new(TraceBuffer::new())),
            #[cfg(feature = "ha")]
            ha: None,
            next_request_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    /// Attaches a running replicated-registry handle to this coordinator.
    ///
    /// Consuming rather than mutating, because the handle has to be started
    /// *from* a coordinator (it replicates that coordinator's store) and then
    /// attached back to it — one line at startup, with no window in which a
    /// clone of this hub exists without the handle its dispatch path checks
    /// for.
    #[cfg(feature = "ha")]
    #[must_use]
    pub fn with_ha(mut self, handle: Arc<crate::ha::HaHandle>) -> Self {
        self.ha = Some(handle);
        self
    }

    /// This coordinator's replicated-registry handle, if it has one.
    ///
    /// `None` means an ordinary single-coordinator deployment: every mutating
    /// verb is accepted locally, exactly as without the feature.
    #[cfg(feature = "ha")]
    #[must_use]
    pub fn ha(&self) -> Option<Arc<crate::ha::HaHandle>> {
        self.ha.clone()
    }

    /// A coordinator over a fresh in-memory store — for tests and `astrs
    /// run`'s embedded single-process coordinator.
    ///
    /// # Errors
    ///
    /// [`crate::CoordinatorError::Store`] if the in-memory backend cannot
    /// be constructed (never actually fails for `redb`'s in-memory mode).
    pub fn open_in_memory(config: CoordinatorConfig) -> crate::error::Result<Self> {
        let store = astrs_store::CoordinatorStore::open_in_memory()?;
        Ok(Self::new(config, AsyncStore::new(store)))
    }

    /// The next correlation id for a fan-out request.
    pub fn next_request_id(&self) -> u64 {
        self.next_request_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Locks the daemon registry for a short, synchronous critical
    /// section.
    ///
    /// Recovers the guard even if the lock is poisoned (a prior critical
    /// section panicked while holding it) rather than panicking here too:
    /// every critical section this crate writes is a few lines of
    /// map/set logic with no fallible I/O, so the registry a poisoned
    /// lock hands back is, in practice, still a valid (if possibly
    /// slightly stale) map — and a coordinator that keeps running for
    /// every other dataflow is a better outcome than one that takes the
    /// whole cluster's control plane down over one panicked critical
    /// section (blueprint principle 5: crash-first design is about
    /// resources having a reclamation path, not about the coordinator
    /// process itself being fragile).
    pub fn daemons(&self) -> std::sync::MutexGuard<'_, DaemonRegistry> {
        self.daemons
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// As [`Coordinator::daemons`], for the subscription registry.
    pub fn subscriptions(&self) -> std::sync::MutexGuard<'_, SubscriptionRegistry> {
        self.subscriptions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// As [`Coordinator::daemons`], for the dataflow registry.
    pub fn dataflows(&self) -> std::sync::MutexGuard<'_, DataflowRegistry> {
        self.dataflows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// As [`Coordinator::daemons`], for in-flight `Logs` fan-outs.
    pub fn pending_logs(&self) -> std::sync::MutexGuard<'_, PendingLogFetches> {
        self.pending_logs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// As [`Coordinator::daemons`], for this coordinator's own request-span
    /// buffer (`GetTraces`'s backing store — see this crate's `trace` module).
    pub fn traces(&self) -> std::sync::MutexGuard<'_, TraceBuffer> {
        self.traces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A cloneable sink that pushes into this coordinator's own
    /// `TraceBuffer` (this crate's private `trace` module) — the seam a
    /// real process wires `astrs_telemetry::subscriber::init_telemetry_with_spans`
    /// through (see `bins/astrs-cli::command::serve::coordinator`) so that
    /// `GetTraces` genuinely answers from astrs-telemetry's own
    /// finished-span collection (blueprint §13), not only from
    /// `handlers::dispatch`'s own per-request bookkeeping.
    ///
    /// Independent of `self` once returned (it holds a clone of the same
    /// `Arc` `self.traces` does), so it is safe to hand to a
    /// `'static` subscriber that outlives the borrow of `self` that
    /// produced it.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_coordinator::{Coordinator, CoordinatorConfig};
    /// use astrs_time::HlcTimestamp;
    /// use astrs_wire::{AuthToken, SpanStatus, TraceSpan};
    ///
    /// let coordinator =
    ///     Coordinator::open_in_memory(CoordinatorConfig::new(AuthToken::from_bytes([1; 32])))?;
    /// let sink = coordinator.trace_sink();
    /// let span = TraceSpan::new("t", "s", "example", HlcTimestamp::EPOCH).with_status(SpanStatus::Ok);
    /// sink(span);
    /// assert_eq!(coordinator.traces().len(), 1);
    /// # Ok::<(), astrs_coordinator::CoordinatorError>(())
    /// ```
    pub fn trace_sink(&self) -> impl Fn(astrs_wire::TraceSpan) + Send + Sync + 'static {
        let traces = Arc::clone(&self.traces);
        move |span| {
            traces
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(span);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::AuthToken;

    fn coordinator() -> Coordinator {
        Coordinator::open_in_memory(
            CoordinatorConfig::new(AuthToken::from_bytes([1; 32])).with_port(0),
        )
        .unwrap()
    }

    #[test]
    fn request_ids_are_distinct_and_increasing() {
        let coordinator = coordinator();
        let a = coordinator.next_request_id();
        let b = coordinator.next_request_id();
        assert!(b > a);
    }

    #[test]
    fn clones_share_the_same_registries() {
        let coordinator = coordinator();
        let clone = coordinator.clone();
        let id = astrs_wire::DaemonId::generate(None);
        {
            let mut daemons = coordinator.daemons();
            let (tx, _rx) = tokio::sync::mpsc::channel(1);
            daemons.insert(crate::registry::DaemonHandle::new(
                id.clone(),
                None,
                astrs_wire::SessionId::generate(),
                tx,
                astrs_time::HlcTimestamp::EPOCH,
            ));
        }
        assert!(clone.daemons().is_connected(&id));
    }

    #[test]
    fn open_in_memory_starts_with_an_empty_cluster() {
        let coordinator = coordinator();
        assert!(coordinator.daemons().is_empty());
        assert!(coordinator.dataflows().is_empty());
        assert!(coordinator.subscriptions().is_empty());
    }
}
