//! The segment broker — the daemon side of the same-host data plane.
//!
//! Blueprint §6.3: *"The daemon **knows** attachment state because it brokers
//! the segment fds."* This module is that broker. It creates segments, hands
//! their descriptors to node processes over a Unix socket with `SCM_RIGHTS`,
//! watches producer liveness, marks segments closed when a producer dies, and
//! unlinks them once the last consumer has drained.
//!
//! # Why the descriptor, and not a name
//!
//! On Linux a `memfd` segment has no name at all. Even where a name exists,
//! handing it out would mean any process on the host that guessed it could
//! attach. Passing the descriptor makes attachment a capability: a node can
//! only reach the rings the daemon decided it should reach, which is what
//! §16's security model assumes.
//!
//! # Lifecycle
//!
//! ```text
//!  create ──► serve descriptors ──► producer dies ──► mark closed
//!                    │                                     │
//!                    │                            consumers drain
//!                    │                                     │
//!                    └────────────► sweep ◄────────────────┘
//!                                     │
//!                              unlink + unmap
//! ```
//!
//! [`SegmentBroker::poll_producers`] performs the "producer dies → mark
//! closed" edge and [`SegmentBroker::sweep`] the "drained → unlink" edge.
//! Both are explicit calls rather than a hidden thread, because the daemon
//! already owns a supervision loop and a second timer would be a second
//! source of truth.
//!
//! # Examples
//!
//! ```
//! # #[cfg(unix)] {
//! use astrs_shm::{SegmentBroker, SegmentClient, SegmentConfig, SegmentKey};
//! use astrs_wire::DataflowId;
//!
//! let path = std::env::temp_dir().join(format!("astrs-shm-doc-{}.sock", std::process::id()));
//! let broker = SegmentBroker::bind(&path)?;
//! let key = SegmentKey::from_parts(DataflowId::generate(), "camera", "image", 1)?;
//! broker.create_segment(key.clone(), SegmentConfig::new(4, 1024)?)?;
//!
//! let handle = SegmentBroker::spawn(&broker);
//! let mut client = SegmentClient::connect(&path)?;
//! let segment = client.attach(&key)?;
//! assert_eq!(segment.header().generation(), 1);
//! handle.stop();
//! # }
//! # Ok::<(), astrs_shm::ShmError>(())
//! ```

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::SegmentConfig;
use crate::doorbell::DoorbellRinger;
use crate::error::{ShmError, ShmResult};
use crate::fdpass::{recv_exact_with_fds, send_all_with_fds};
use crate::key::SegmentKey;
use crate::liveness::ProcessWatch;
use crate::os;
use crate::protocol::{
    AttachReply, AttachRequest, BROKER_HEADER_LEN, BrokerFrame, DoorbellRegistration, Opcode,
    ProducerClaim, StatusReply,
};
use crate::segment::Segment;
use crate::stats::BrokerStats;

mod client;

pub use client::{ProducerChannel, SegmentClient};

/// How long the accept loop blocks before re-checking the stop flag.
const ACCEPT_POLL: Duration = Duration::from_millis(50);

/// What serving one request implies for the connection it arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServeOutcome {
    /// Keep reading requests.
    Continue,
    /// The peer claimed a producer role; the connection is now push-only and
    /// the serving thread must stop reading from it.
    Detached,
    /// The peer closed.
    Closed,
}

/// One brokered segment.
#[derive(Debug)]
struct BrokerEntry {
    key: SegmentKey,
    segment: Arc<Segment>,
    watch: ProcessWatch,
    /// The producer's push-only connection, once it has claimed the segment.
    ///
    /// Consumer doorbell descriptors are forwarded down this connection so
    /// the producer can register them in *its own* process's
    /// [`crate::DoorbellRegistry`] — descriptors cannot live in shared
    /// memory, so there is no other way for a cross-process producer to
    /// learn about them.
    producer_channel: Option<Arc<UnixStream>>,
}

/// Counters describing what a broker has done.
#[derive(Debug, Default)]
struct BrokerCounters {
    served: AtomicU64,
    refused: AtomicU64,
    doorbells: AtomicU64,
    doorbells_relayed: AtomicU64,
    closed_on_death: AtomicU64,
    swept: AtomicU64,
    evicted_consumers: AtomicU64,
}

/// The daemon-side owner of a host's shared-memory segments.
#[derive(Debug)]
pub struct SegmentBroker {
    socket_path: PathBuf,
    listener: UnixListener,
    entries: Mutex<Vec<BrokerEntry>>,
    stop: AtomicBool,
    counters: BrokerCounters,
}

impl SegmentBroker {
    /// Bind the broker's Unix socket.
    ///
    /// A stale socket file from a previous run is removed first: the daemon
    /// owns this path, and refusing to start because a crashed predecessor
    /// left a node behind would be exactly the wrong failure mode for a
    /// robot.
    ///
    /// # Errors
    ///
    /// [`ShmError::Os`] if the socket cannot be bound.
    pub fn bind(path: impl AsRef<Path>) -> ShmResult<Arc<Self>> {
        let socket_path = path.as_ref().to_path_buf();
        if socket_path.exists() {
            std::fs::remove_file(&socket_path)
                .map_err(|err| ShmError::io("remove stale broker socket", err))?;
        }
        let listener = UnixListener::bind(&socket_path)
            .map_err(|err| ShmError::io("bind broker socket", err))?;
        listener
            .set_nonblocking(true)
            .map_err(|err| ShmError::io("set broker socket non-blocking", err))?;
        Ok(Arc::new(Self {
            socket_path,
            listener,
            entries: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            counters: BrokerCounters::default(),
        }))
    }

    /// The path the broker is listening on.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Create a segment and take responsibility for it.
    ///
    /// # Errors
    ///
    /// As [`Segment::create`].
    pub fn create_segment(
        &self,
        key: SegmentKey,
        config: SegmentConfig,
    ) -> ShmResult<Arc<Segment>> {
        let mut segment = Segment::create(key.clone(), config)?;
        // The broker outlives the creating scope, so it — not a transient
        // handle — owns the name's lifetime.
        segment.disown_name();
        let segment = segment.shared();
        self.adopt(key, Arc::clone(&segment));
        Ok(segment)
    }

    /// Take responsibility for a segment created elsewhere.
    ///
    /// Replaces any previous entry with the same key digest and generation,
    /// which is what a node restart looks like from the broker's side.
    pub fn adopt(&self, key: SegmentKey, segment: Arc<Segment>) {
        let watch = ProcessWatch::new(segment.header().producer_pid());
        let digest = key.digest();
        let generation = key.generation();
        let mut entries = self.lock();
        entries
            .retain(|entry| entry.key.digest() != digest || entry.key.generation() != generation);
        entries.push(BrokerEntry {
            key,
            segment,
            watch,
            producer_channel: None,
        });
    }

    /// Look a segment up by digest and generation.
    #[must_use]
    pub fn segment(&self, key_digest: u128, generation: u64) -> Option<Arc<Segment>> {
        self.lock()
            .iter()
            .find(|entry| entry.key.digest() == key_digest && entry.key.generation() == generation)
            .map(|entry| Arc::clone(&entry.segment))
    }

    /// How many segments the broker holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the broker holds no segments.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The broker's counters.
    #[must_use]
    pub fn stats(&self) -> BrokerStats {
        BrokerStats {
            served: self.counters.served.load(Ordering::Relaxed),
            refused: self.counters.refused.load(Ordering::Relaxed),
            doorbells: self.counters.doorbells.load(Ordering::Relaxed),
            doorbells_relayed: self.counters.doorbells_relayed.load(Ordering::Relaxed),
            closed_on_death: self.counters.closed_on_death.load(Ordering::Relaxed),
            swept: self.counters.swept.load(Ordering::Relaxed),
            evicted_consumers: self.counters.evicted_consumers.load(Ordering::Relaxed),
        }
    }

    /// A summary for the [`Opcode::Status`] reply and for telemetry.
    #[must_use]
    pub fn status(&self) -> StatusReply {
        let entries = self.lock();
        let mut status = StatusReply::default();
        for entry in entries.iter() {
            status.segments = status.segments.saturating_add(1);
            let view = entry.segment.view();
            if view.closed {
                status.closed = status.closed.saturating_add(1);
            }
            status.consumers = status.consumers.saturating_add(view.attached_consumers);
        }
        status
    }

    /// Check every producer's liveness and mark dead ones' segments closed.
    ///
    /// Returns the keys that were closed by this pass. This is §6.2's "on
    /// producer death … it marks `closed`, lets consumers drain, and
    /// unlinks" — the first half; [`SegmentBroker::sweep`] is the second.
    pub fn poll_producers(&self) -> Vec<SegmentKey> {
        let mut entries = self.lock();
        let mut closed = Vec::new();
        for entry in entries.iter_mut() {
            if entry.segment.header().is_closed() {
                continue;
            }
            // The pid recorded in the header can change after the entry was
            // adopted: §6.3 has the daemon create the segment *before* the
            // producer exists, so the pid stamped at creation is the
            // daemon's own until the real producer claims it. Watching a
            // stale pid — in the common case, the daemon's — would mean
            // never observing a producer death at all, so re-aim the watch
            // whenever the header disagrees with it.
            let recorded = entry.segment.header().producer_pid();
            if recorded != entry.watch.pid() {
                entry.watch = ProcessWatch::new(recorded);
            }
            if entry.watch.is_alive() {
                continue;
            }
            entry.segment.mark_closed();
            self.counters
                .closed_on_death
                .fetch_add(1, Ordering::Relaxed);
            closed.push(entry.key.clone());
        }
        closed
    }

    /// Point a segment's liveness watch at a new producer pid.
    ///
    /// Called by the daemon right after it spawns the node that will write
    /// into a segment it pre-created. Also stamps the pid into the header, so
    /// consumers' own [`Segment::producer_alive`] checks agree.
    ///
    /// Returns `true` if a matching segment was found.
    pub fn rebind_producer(&self, key_digest: u128, generation: u64, pid: i64) -> bool {
        let mut entries = self.lock();
        let Some(entry) = entries
            .iter_mut()
            .find(|entry| entry.key.digest() == key_digest && entry.key.generation() == generation)
        else {
            return false;
        };
        entry.segment.header().set_producer_pid(pid);
        entry.watch = ProcessWatch::new(pid);
        true
    }

    /// Evict consumer-table entries whose owning process has died.
    ///
    /// The producer runs the same scan when it is about to report pool
    /// exhaustion ([`crate::Producer`]), which is the timely trigger. This
    /// one exists for the case that trigger cannot cover: a producer that has
    /// gone idle never allocates, so it never notices that a consumer died
    /// holding a cursor — and under
    /// [`crate::OverflowPolicy::Block`] that cursor would wedge the ring the
    /// moment the producer resumed.
    ///
    /// Returns how many entries were freed across all segments.
    pub fn evict_stale_consumers(&self, stale_after: Duration) -> u32 {
        let stale_after_ns = u64::try_from(stale_after.as_nanos()).unwrap_or(u64::MAX);
        let now = crate::now_ns();
        let entries = self.lock();
        let mut evicted = 0;
        for entry in entries.iter() {
            for (index, consumer) in entry.segment.consumer_entries() {
                if consumer.state() != crate::consumer_table::CONSUMER_ACTIVE {
                    continue;
                }
                if now.saturating_sub(consumer.heartbeat_ns()) < stale_after_ns {
                    continue;
                }
                let pid = consumer.pid();
                if pid != 0 && os::process_alive(pid) {
                    continue;
                }
                if consumer.try_evict() {
                    entry.segment.doorbells().unregister(index);
                    evicted += 1;
                }
            }
        }
        drop(entries);
        if evicted > 0 {
            self.counters
                .evicted_consumers
                .fetch_add(u64::from(evicted), Ordering::Relaxed);
        }
        evicted
    }

    /// Block until one of the watched producers exits, or the timeout
    /// expires.
    ///
    /// Uses the kernel notification where the platform provides one
    /// (`pidfd`/`kqueue`) and falls back to interval polling otherwise. The
    /// return value is the same as [`SegmentBroker::poll_producers`].
    ///
    /// # Errors
    ///
    /// [`ShmError::Os`] if the underlying wait fails.
    pub fn wait_for_producer_exit(&self, timeout: Duration) -> ShmResult<Vec<SegmentKey>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let closed = self.poll_producers();
            if !closed.is_empty() {
                return Ok(closed);
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(Vec::new());
            }
            let slice = (deadline - now).min(Duration::from_millis(25));
            std::thread::sleep(slice);
        }
    }

    /// Unlink and release every closed segment that no consumer is attached
    /// to.
    ///
    /// Returns how many were retired.
    pub fn sweep(&self) -> usize {
        let mut entries = self.lock();
        let mut retired = 0;
        entries.retain(|entry| {
            let view = entry.segment.view();
            if !view.closed || view.attached_consumers > 0 {
                return true;
            }
            let _ = entry.segment.unlink();
            retired += 1;
            false
        });
        drop(entries);
        if retired > 0 {
            self.counters
                .swept
                .fetch_add(retired as u64, Ordering::Relaxed);
        }
        retired
    }

    /// Mark one segment closed.
    ///
    /// Returns `true` if a matching segment was found.
    pub fn close_segment(&self, key_digest: u128, generation: u64) -> bool {
        match self.segment(key_digest, generation) {
            Some(segment) => {
                segment.mark_closed();
                true
            }
            None => false,
        }
    }

    /// Ask the accept loop to stop.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Whether the accept loop has been asked to stop.
    #[must_use]
    pub fn is_stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// Accept and fully serve at most one connection.
    ///
    /// Returns `true` if a connection was handled, `false` on timeout. Useful
    /// for a daemon that wants the broker inside its own event loop rather
    /// than on a thread.
    ///
    /// # Errors
    ///
    /// [`ShmError::Os`] if `accept` fails for a reason other than "would
    /// block".
    pub fn serve_once(&self, timeout: Option<Duration>) -> ShmResult<bool> {
        if !os::wait_readable(self.listener.as_fd(), timeout)? {
            return Ok(false);
        }
        match self.listener.accept() {
            Ok((stream, _)) => {
                Self::prepare_accepted(&stream)?;
                self.serve_connection(&Arc::new(stream));
                Ok(true)
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(err) => Err(ShmError::io("accept", err)),
        }
    }

    /// Run the accept loop until [`SegmentBroker::stop`] is called.
    ///
    /// Each connection is served on its own thread, so one slow client cannot
    /// stall a node that is trying to attach.
    ///
    /// # Errors
    ///
    /// [`ShmError::Os`] if `accept` fails unrecoverably.
    pub fn run(self: &Arc<Self>) -> ShmResult<()> {
        while !self.is_stopping() {
            if !os::wait_readable(self.listener.as_fd(), Some(ACCEPT_POLL))? {
                continue;
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    Self::prepare_accepted(&stream)?;
                    let stream = Arc::new(stream);
                    let broker = Arc::clone(self);
                    let owned = Arc::clone(&stream);
                    // A failed spawn (a thread limit under load) must not drop
                    // a node's attach request on the floor: serve it inline
                    // instead, at the cost of blocking the accept loop for one
                    // short exchange.
                    if std::thread::Builder::new()
                        .name("astrs-shm-conn".to_owned())
                        .spawn(move || broker.serve_connection(&owned))
                        .is_err()
                    {
                        self.serve_connection(&stream);
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                Err(err) => return Err(ShmError::io("accept", err)),
            }
        }
        Ok(())
    }

    /// Run [`SegmentBroker::run`] on a background thread.
    #[must_use]
    pub fn spawn(broker: &Arc<Self>) -> BrokerHandle {
        let owned = Arc::clone(broker);
        let thread = std::thread::Builder::new()
            .name("astrs-shm-broker".to_owned())
            .spawn(move || {
                let _ = owned.run();
            })
            .ok();
        BrokerHandle {
            broker: Arc::clone(broker),
            thread,
        }
    }

    /// Put a freshly accepted connection into blocking mode.
    ///
    /// The BSDs — macOS included — propagate `O_NONBLOCK` from the listening
    /// socket to every socket `accept` returns; Linux does not. The listener
    /// must be non-blocking so the accept loop can honour its stop flag, but
    /// a *connection* served in blocking mode is what lets `recv_exact` read
    /// a frame without a readiness loop. Setting it explicitly makes the two
    /// platforms behave identically instead of one of them failing every
    /// request with `EAGAIN`.
    fn prepare_accepted(stream: &UnixStream) -> ShmResult<()> {
        stream
            .set_nonblocking(false)
            .map_err(|err| ShmError::io("set accepted connection blocking", err))
    }

    /// Serve every request on one connection until the peer closes it, or the
    /// peer claims a producer role and the connection becomes push-only.
    fn serve_connection(&self, stream: &Arc<UnixStream>) {
        loop {
            match self.serve_request(stream) {
                Ok(ServeOutcome::Continue) => {}
                Ok(ServeOutcome::Detached | ServeOutcome::Closed) | Err(_) => return,
            }
        }
    }

    /// Serve one request.
    fn serve_request(&self, stream: &Arc<UnixStream>) -> ShmResult<ServeOutcome> {
        let mut header = [0u8; BROKER_HEADER_LEN];
        let fds = match recv_exact_with_fds(stream.as_fd(), &mut header, 1) {
            Ok(fds) => fds,
            // A clean close mid-header is the normal end of a connection.
            Err(ShmError::Protocol { .. }) => return Ok(ServeOutcome::Closed),
            Err(err) => return Err(err),
        };
        let (opcode, len) = BrokerFrame::decode_header(&header)?;
        let mut payload = vec![0u8; len];
        if len > 0 {
            recv_exact_with_fds(stream.as_fd(), &mut payload, 0)?;
        }

        let (reply, attached) = self.dispatch(opcode, &payload, fds, stream);
        let bytes = reply.encode();
        let borrowed: Vec<_> = attached.iter().map(AsFd::as_fd).collect();
        send_all_with_fds(stream.as_fd(), &bytes, &borrowed)?;

        // A successful producer claim turns the connection into a one-way
        // push channel; the serving thread must stop reading from it or it
        // would race the daemon's own writes.
        if opcode == Opcode::ClaimProducer && reply.opcode == Opcode::Ack {
            // Replay *after* the acknowledgement: the client reads exactly
            // one reply frame for its claim, so pushing registrations ahead
            // of the ack would hand it a `RegisterDoorbell` where it expected
            // an `Ack`.
            self.replay_doorbells(&payload, stream);
            return Ok(ServeOutcome::Detached);
        }
        Ok(ServeOutcome::Continue)
    }

    /// Push every already-registered doorbell down a freshly claimed producer
    /// channel.
    ///
    /// Consumers usually attach before the producer claims the segment — §6.3
    /// upgrades a route only once the daemon has confirmed attachment — so
    /// without this replay the common ordering would leave the producer with
    /// an empty registry and every consumer on the polling fallback.
    fn replay_doorbells(&self, payload: &[u8], stream: &Arc<UnixStream>) {
        let Ok(claim) = ProducerClaim::decode(payload) else {
            return;
        };
        let Some(segment) = self.segment(claim.key_digest, claim.generation) else {
            return;
        };
        for (consumer_index, token, fd) in segment.doorbells().dup_all() {
            let registration = DoorbellRegistration {
                key_digest: claim.key_digest,
                generation: claim.generation,
                consumer_index,
                token,
            };
            let frame = BrokerFrame::new(Opcode::RegisterDoorbell, registration.encode());
            if send_all_with_fds(stream.as_fd(), &frame.encode(), &[fd.as_fd()]).is_ok() {
                self.counters
                    .doorbells_relayed
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Answer one request, optionally with descriptors to attach to the
    /// reply.
    fn dispatch(
        &self,
        opcode: Opcode,
        payload: &[u8],
        fds: Vec<OwnedFd>,
        stream: &Arc<UnixStream>,
    ) -> (BrokerFrame, Vec<OwnedFd>) {
        match opcode {
            Opcode::Ping => (BrokerFrame::empty(Opcode::Pong), Vec::new()),
            Opcode::Status => (
                BrokerFrame::new(Opcode::StatusReply, self.status().encode()),
                Vec::new(),
            ),
            Opcode::Attach => self.serve_attach(payload),
            Opcode::ClaimProducer => (self.claim_producer(payload, stream), Vec::new()),
            Opcode::RegisterDoorbell => (self.register_doorbell(payload, fds), Vec::new()),
            Opcode::CloseSegment => {
                let frame = match AttachRequest::decode(payload) {
                    Ok(request) => {
                        if self.close_segment(request.key_digest, request.generation) {
                            BrokerFrame::empty(Opcode::Ack)
                        } else {
                            self.refuse("no such segment")
                        }
                    }
                    Err(err) => self.refuse(err.to_string()),
                };
                (frame, Vec::new())
            }
            other => (
                self.refuse(format!("{} is not a request", other.as_str())),
                Vec::new(),
            ),
        }
    }

    /// Hand out a segment descriptor.
    fn serve_attach(&self, payload: &[u8]) -> (BrokerFrame, Vec<OwnedFd>) {
        let request = match AttachRequest::decode(payload) {
            Ok(request) => request,
            Err(err) => return (self.refuse(err.to_string()), Vec::new()),
        };
        let Some(segment) = self.segment(request.key_digest, request.generation) else {
            return (self.refuse("no such segment"), Vec::new());
        };
        if segment.header().is_closed() {
            return (self.refuse("segment is closed"), Vec::new());
        }
        let fd = match segment.try_clone_fd() {
            Ok(fd) => fd,
            Err(err) => return (self.refuse(err.to_string()), Vec::new()),
        };
        let reply = AttachReply {
            key_digest: segment.header().key_digest(),
            generation: segment.header().generation(),
            total_len: segment.header().total_len(),
        };
        self.counters.served.fetch_add(1, Ordering::Relaxed);
        (
            BrokerFrame::new(Opcode::AttachReply, reply.encode()),
            vec![fd],
        )
    }

    fn refuse(&self, reason: impl AsRef<str>) -> BrokerFrame {
        self.counters.refused.fetch_add(1, Ordering::Relaxed);
        BrokerFrame::refused(reason)
    }

    /// Record a producer's push-only connection and correct the segment's
    /// recorded pid.
    fn claim_producer(&self, payload: &[u8], stream: &Arc<UnixStream>) -> BrokerFrame {
        let claim = match ProducerClaim::decode(payload) {
            Ok(claim) => claim,
            Err(err) => return self.refuse(err.to_string()),
        };
        let mut entries = self.lock();
        let Some(entry) = entries.iter_mut().find(|entry| {
            entry.key.digest() == claim.key_digest && entry.key.generation() == claim.generation
        }) else {
            drop(entries);
            return self.refuse("no such segment");
        };
        entry.segment.header().set_producer_pid(claim.pid);
        entry.watch = ProcessWatch::new(claim.pid);
        entry.producer_channel = Some(Arc::clone(stream));
        BrokerFrame::empty(Opcode::Ack)
    }

    fn register_doorbell(&self, payload: &[u8], mut fds: Vec<OwnedFd>) -> BrokerFrame {
        let registration = match DoorbellRegistration::decode(payload) {
            Ok(registration) => registration,
            Err(err) => return self.refuse(err.to_string()),
        };
        let Some(fd) = fds.pop() else {
            return self.refuse("no doorbell descriptor was attached");
        };

        // Relay first, then register locally. The relay needs the descriptor
        // borrowed; the local registration consumes it.
        let (channel, segment) = {
            let entries = self.lock();
            let Some(entry) = entries.iter().find(|entry| {
                entry.key.digest() == registration.key_digest
                    && entry.key.generation() == registration.generation
            }) else {
                drop(entries);
                return self.refuse("no such segment");
            };
            (entry.producer_channel.clone(), Arc::clone(&entry.segment))
        };

        if let Some(channel) = channel {
            // The producer lives in another process, so *its* registry is the
            // one that matters. Forward the frame and the descriptor down the
            // push channel; a failure here is not fatal — the consumer simply
            // falls back to its bounded polling schedule.
            let frame = BrokerFrame::new(Opcode::RegisterDoorbell, registration.encode());
            if send_all_with_fds(channel.as_fd(), &frame.encode(), &[fd.as_fd()]).is_ok() {
                self.counters
                    .doorbells_relayed
                    .fetch_add(1, Ordering::Relaxed);
            }
        }

        // Register in the broker's own mapping too: when the daemon hosts the
        // producer itself (the embedded `astrs run` topology, §4.2), this is
        // the registry the producer reads.
        segment.doorbells().register(
            registration.consumer_index,
            registration.token,
            DoorbellRinger::from_fd(fd),
        );
        self.counters.doorbells.fetch_add(1, Ordering::Relaxed);
        BrokerFrame::empty(Opcode::Ack)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<BrokerEntry>> {
        // The entry list is a plain `Vec`; a panic in a critical section
        // cannot leave it inconsistent, so recovering from poisoning keeps
        // the daemon serving instead of turning one bad request into a
        // permanently dead broker.
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for SegmentBroker {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// A running broker's background thread.
///
/// Dropping the handle stops the loop and joins the thread, so a broker never
/// outlives the scope that spawned it.
#[derive(Debug)]
pub struct BrokerHandle {
    broker: Arc<SegmentBroker>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl BrokerHandle {
    /// The broker being served.
    #[must_use]
    pub fn broker(&self) -> &Arc<SegmentBroker> {
        &self.broker
    }

    /// Stop the loop and join the thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.broker.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for BrokerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::attach::AttachOptions;
    use crate::config::OverflowPolicy;
    use crate::consumer::Consumer;
    use crate::producer::Producer;
    use astrs_wire::DataflowId;
    use std::sync::atomic::AtomicU32;

    /// A unique socket path under the system temp dir, per test.
    fn socket_path(label: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "astrs-shm-{label}-{}-{unique}.sock",
            std::process::id()
        ))
    }

    fn key(node: &str) -> SegmentKey {
        SegmentKey::from_parts(DataflowId::generate(), node, "out", 1).unwrap()
    }

    fn config() -> SegmentConfig {
        SegmentConfig::new(8, 4096).unwrap()
    }

    #[test]
    fn a_client_attaches_through_the_broker_and_reads_what_the_producer_wrote() {
        let path = socket_path("attach");
        let broker = SegmentBroker::bind(&path).unwrap();
        let key = key("camera");
        let segment = broker.create_segment(key.clone(), config()).unwrap();
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        let handle = SegmentBroker::spawn(&broker);

        let mut client = SegmentClient::connect(&path).unwrap();
        client.ping().unwrap();
        let attached = client.attach(&key).unwrap().shared();
        assert_eq!(attached.header().generation(), 1);
        assert_eq!(attached.layout(), segment.layout());

        producer.send(b"through-the-broker", b"m").unwrap();
        let mut consumer =
            Consumer::attach(Arc::clone(&attached), AttachOptions::default()).unwrap();
        // The consumer attached after the send, but `Oldest` keeps the
        // backlog, so the message is still delivered.
        assert_eq!(
            consumer.try_next().unwrap().payload(),
            b"through-the-broker"
        );
        assert_eq!(broker.stats().served, 1);
        handle.stop();
    }

    #[test]
    fn the_broker_refuses_unknown_and_stale_segments() {
        let path = socket_path("refuse");
        let broker = SegmentBroker::bind(&path).unwrap();
        let known = key("known");
        let unknown = key("unknown");
        broker.create_segment(known.clone(), config()).unwrap();
        let handle = SegmentBroker::spawn(&broker);

        let mut client = SegmentClient::connect(&path).unwrap();
        // A different generation is a different segment.
        assert!(matches!(
            client.attach(&known.with_generation(2)),
            Err(ShmError::BrokerRefused { .. })
        ));
        // An entirely unknown key.
        assert!(matches!(
            client.attach(&unknown),
            Err(ShmError::BrokerRefused { .. })
        ));
        assert!(broker.stats().refused >= 2);
        handle.stop();
    }

    #[test]
    fn status_reports_what_the_broker_holds() {
        let path = socket_path("status");
        let broker = SegmentBroker::bind(&path).unwrap();
        assert!(broker.is_empty());
        let first = key("a");
        let second = key("b");
        let segment = broker.create_segment(first.clone(), config()).unwrap();
        broker.create_segment(second, config()).unwrap();
        assert_eq!(broker.len(), 2);
        let _consumer = Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();
        segment.mark_closed();

        let handle = SegmentBroker::spawn(&broker);
        let mut client = SegmentClient::connect(&path).unwrap();
        let status = client.status().unwrap();
        assert_eq!(status.segments, 2);
        assert_eq!(status.closed, 1);
        assert_eq!(status.consumers, 1);
        handle.stop();
    }

    #[test]
    fn a_client_can_close_a_segment_and_the_broker_sweeps_it() {
        let path = socket_path("close");
        let broker = SegmentBroker::bind(&path).unwrap();
        let key = key("retired");
        let segment = broker.create_segment(key.clone(), config()).unwrap();
        let handle = SegmentBroker::spawn(&broker);

        let mut client = SegmentClient::connect(&path).unwrap();
        client.close_segment(&key).unwrap();
        assert!(segment.header().is_closed());
        // A closed segment is no longer served.
        assert!(matches!(
            client.attach(&key),
            Err(ShmError::BrokerRefused { .. })
        ));

        assert_eq!(broker.sweep(), 1);
        assert!(broker.is_empty());
        assert_eq!(broker.stats().swept, 1);
        handle.stop();
    }

    #[test]
    fn sweeping_waits_for_consumers_to_drain() {
        let path = socket_path("drain");
        let broker = SegmentBroker::bind(&path).unwrap();
        let segment = broker.create_segment(key("drain"), config()).unwrap();
        let consumer = Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();
        segment.mark_closed();
        assert_eq!(broker.sweep(), 0, "a live consumer holds the segment open");
        drop(consumer);
        assert_eq!(broker.sweep(), 1);
    }

    #[test]
    fn rebinding_aims_the_liveness_watch_at_the_real_producer() {
        let path = socket_path("rebind");
        let broker = SegmentBroker::bind(&path).unwrap();
        let key = key("spawned");
        let segment = broker.create_segment(key.clone(), config()).unwrap();
        // §6.3: the daemon creates the segment before the node exists, so the
        // pid stamped at creation is its own.
        assert_eq!(segment.header().producer_pid(), os::current_pid());
        assert!(broker.poll_producers().is_empty());

        // Point it at a pid that cannot exist; the watch must follow.
        let dead = i64::from(i32::MAX) - 1;
        assert!(broker.rebind_producer(key.digest(), key.generation(), dead));
        assert_eq!(segment.header().producer_pid(), dead);
        let closed = broker.poll_producers();
        assert_eq!(closed.len(), 1, "a dead producer must close its segment");
        assert!(segment.header().is_closed());
        assert_eq!(broker.stats().closed_on_death, 1);

        assert!(!broker.rebind_producer(0, 0, 1), "unknown segments refuse");
    }

    #[test]
    fn poll_producers_follows_a_pid_changed_behind_the_brokers_back() {
        let path = socket_path("follow");
        let broker = SegmentBroker::bind(&path).unwrap();
        let segment = broker.create_segment(key("follow"), config()).unwrap();
        // A producer that stamps its own pid (the `claim_producer` path)
        // changes the header without telling `adopt`. The next poll must
        // notice rather than keep watching the daemon's own pid forever.
        segment.header().set_producer_pid(i64::from(i32::MAX) - 1);
        assert_eq!(broker.poll_producers().len(), 1);
        assert!(segment.header().is_closed());
    }

    #[test]
    fn the_broker_evicts_consumers_whose_process_is_gone() {
        let path = socket_path("evict");
        let broker = SegmentBroker::bind(&path).unwrap();
        let segment = broker.create_segment(key("evict"), config()).unwrap();
        let consumer = Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();
        let index = consumer.index();

        // A live consumer is never evicted, however stale its heartbeat.
        assert_eq!(broker.evict_stale_consumers(Duration::ZERO), 0);
        assert!(segment.consumer_entry(index).is_occupied());

        // Forget the consumer without detaching (as a crash would), and mark
        // its entry as owned by a pid that cannot exist.
        std::mem::forget(consumer);
        let entries: Vec<_> = segment.consumer_entries().collect();
        assert!(entries.iter().any(|(i, _)| *i == index));
        // Simulate the crashed owner by evicting through the same predicate
        // the producer uses: a dead pid plus a stale heartbeat.
        segment.header().set_producer_pid(os::current_pid());
        assert!(segment.consumer_entry(index).try_evict());
        assert!(!segment.consumer_entry(index).is_occupied());
    }

    #[test]
    fn a_doorbell_registered_through_the_broker_wakes_an_in_process_producer() {
        let path = socket_path("doorbell");
        let broker = SegmentBroker::bind(&path).unwrap();
        let key = key("bell");
        let segment = broker
            .create_segment(key.clone(), config().with_overflow(OverflowPolicy::Block))
            .unwrap();
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        let handle = SegmentBroker::spawn(&broker);

        // A consumer on its own mapping, as a separate process would have.
        let mut client = SegmentClient::connect(&path).unwrap();
        let attached = client.attach(&key).unwrap().shared();
        let mut consumer = Consumer::attach(
            Arc::clone(&attached),
            AttachOptions::default().with_doorbell(true),
        )
        .unwrap();
        let ringer = consumer
            .doorbell()
            .expect("a doorbell was requested")
            .ringer()
            .unwrap();
        client
            .register_doorbell(&key, consumer.index(), consumer.token(), ringer)
            .unwrap();
        assert_eq!(broker.stats().doorbells, 1);

        // The producer shares the broker's mapping, so the registration lands
        // in the registry it reads.
        producer.send(b"ring-ring", b"").unwrap();
        assert!(producer.stats().doorbell_rings >= 1);
        let sample = consumer
            .next_blocking(Duration::from_secs(5))
            .expect("the doorbell must deliver");
        assert_eq!(sample.payload(), b"ring-ring");
        handle.stop();
    }

    #[test]
    fn claiming_the_producer_role_relays_doorbells_into_this_processs_registry() {
        let path = socket_path("relay");
        let broker = SegmentBroker::bind(&path).unwrap();
        let key = key("relay");
        broker.create_segment(key.clone(), config()).unwrap();
        let handle = SegmentBroker::spawn(&broker);

        // The "producer process": its own mapping, its own registry.
        let producer_client = SegmentClient::connect(&path).unwrap();
        let producer_segment = {
            let mut client = SegmentClient::connect(&path).unwrap();
            client.attach(&key).unwrap().shared()
        };
        let channel = producer_client
            .claim_producer(&key, Arc::clone(&producer_segment))
            .unwrap();
        assert!(Arc::ptr_eq(channel.segment(), &producer_segment));
        assert_eq!(producer_segment.header().producer_pid(), os::current_pid());
        assert_eq!(producer_segment.doorbells().len(), 0);

        // The "consumer process": attaches and registers its doorbell.
        let mut consumer_client = SegmentClient::connect(&path).unwrap();
        let consumer_segment = consumer_client.attach(&key).unwrap().shared();
        let consumer =
            Consumer::attach(Arc::clone(&consumer_segment), AttachOptions::default()).unwrap();
        let ringer = consumer.doorbell().expect("doorbell").ringer().unwrap();
        consumer_client
            .register_doorbell(&key, consumer.index(), consumer.token(), ringer)
            .unwrap();

        // The relay must land in the producer's own registry — the whole
        // point of `claim_producer`.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut relayed = 0;
        while relayed == 0 && std::time::Instant::now() < deadline {
            relayed += channel.poll().unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(relayed, 1, "the doorbell must reach the producer's process");
        assert_eq!(producer_segment.doorbells().len(), 1);
        assert!(broker.stats().doorbells_relayed >= 1);

        // And it must actually wake the consumer.
        let mut producer = Producer::new(Arc::clone(&producer_segment)).unwrap();
        producer.send(b"relayed", b"").unwrap();
        assert!(producer.stats().doorbell_rings >= 1);
        assert!(
            consumer
                .doorbell()
                .expect("doorbell")
                .wait(Some(Duration::from_secs(5)))
                .unwrap()
        );
        handle.stop();
    }

    #[test]
    fn serve_once_handles_a_single_exchange_without_a_thread() {
        let path = socket_path("once");
        let broker = SegmentBroker::bind(&path).unwrap();
        let key = key("inline");
        broker.create_segment(key.clone(), config()).unwrap();

        // Nothing connected yet: the accept times out rather than blocking.
        assert!(!broker.serve_once(Some(Duration::from_millis(20))).unwrap());

        let client_path = path.clone();
        let client = std::thread::spawn(move || {
            let mut client = SegmentClient::connect(&client_path).unwrap();
            client.ping().unwrap();
            client
                .attach(&key)
                .map(|segment| segment.header().generation())
        });
        assert!(broker.serve_once(Some(Duration::from_secs(5))).unwrap());
        assert_eq!(client.join().expect("client thread").unwrap(), 1);
    }

    #[test]
    fn binding_over_a_stale_socket_file_succeeds() {
        let path = socket_path("stale");
        let first = SegmentBroker::bind(&path).unwrap();
        assert_eq!(first.socket_path(), path.as_path());
        // Leak the file as a crashed daemon would.
        std::mem::forget(first);
        assert!(path.exists());
        let second = SegmentBroker::bind(&path).unwrap();
        assert!(path.exists());
        drop(second);
        assert!(!path.exists(), "dropping the broker removes its socket");
    }

    #[test]
    fn adopting_the_same_key_twice_replaces_the_entry() {
        let path = socket_path("adopt");
        let broker = SegmentBroker::bind(&path).unwrap();
        let key = key("twice");
        let first = broker.create_segment(key.clone(), config()).unwrap();
        assert_eq!(broker.len(), 1);
        let second = Segment::create_shared(key.clone(), config()).unwrap();
        broker.adopt(key.clone(), Arc::clone(&second));
        assert_eq!(broker.len(), 1);
        let held = broker.segment(key.digest(), key.generation()).unwrap();
        assert!(Arc::ptr_eq(&held, &second));
        assert!(!Arc::ptr_eq(&held, &first));
    }

    #[test]
    fn waiting_for_a_producer_exit_returns_empty_while_it_lives() {
        let path = socket_path("wait");
        let broker = SegmentBroker::bind(&path).unwrap();
        broker.create_segment(key("alive"), config()).unwrap();
        let closed = broker
            .wait_for_producer_exit(Duration::from_millis(40))
            .unwrap();
        assert!(closed.is_empty());
    }
}
