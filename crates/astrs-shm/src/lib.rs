//! **astrs-shm** — the zero-copy same-host data plane of AstRS.
//!
//! One shared-memory ring per `(producer node, output port, generation)`,
//! written by exactly one producer and read by any number of consumers, with
//! no copy on the fast path and a reclamation protocol that survives any
//! participant being `kill -9`'d.
//!
//! This crate implements blueprint §6.2 in full: the segment layout, the SPMC
//! ring, generation stamps, the drop-token reclamation protocol, doorbell
//! wakeups, producer/consumer liveness, and the descriptor broker the daemon
//! uses to hand segments out.
//!
//! # Contents
//!
//! | Module | Role | Platform |
//! |---|---|---|
//! | [`key`] | Segment identity, the 128-bit digest and the short hashed OS name | all |
//! | [`config`] | Ring geometry and the overflow policy | all |
//! | [`layout`] | Byte offsets, alignment, and hostile-header validation | all |
//! | [`header`] | The 128-byte segment header | all |
//! | [`slot`] | The per-slot control block and the pin / reclaim protocol | all |
//! | [`consumer_table`] | Per-consumer cursors, heartbeats and eviction | all |
//! | [`attach`] | Where a consumer starts reading, and how | all |
//! | [`stats`] | Producer / consumer / broker counters | all |
//! | [`backoff`] | The bounded spin → yield → sleep schedule | all |
//! | [`error`] | The typed error surface | all |
//! | [`segment`] | Mapping, attaching, generation checks, teardown | Unix, Windows |
//! | [`producer`] | Write windows, commit, reclamation | Unix |
//! | [`consumer`] | Read views, lag reporting, blocking receives | Unix |
//! | [`doorbell`] | eventfd/pipe wakeups, sync and async | Unix |
//! | [`liveness`] | pidfd / kqueue / poll process watches | Unix |
//! | [`fdpass`] | `SCM_RIGHTS` descriptor passing | Unix |
//! | [`protocol`] | The broker's request/reply framing | Unix |
//! | [`broker`] | The daemon-side segment broker and its client | Unix |
//! | [`os`] | The platform layer: one file per divergent syscall set | Unix, Windows |
//!
//! The "all" half is where the ring's *logic* lives — geometry, validation,
//! the slot state machine, the consumer table — and it compiles and is
//! unit-tested on every platform, Windows included. Only the syscalls are
//! gated, so a Windows port is three primitives away rather than a rewrite.
//!
//! # The segment, in one picture
//!
//! ```text
//! ┌───────────── 128 B header ──────────────────────────────────────────┐
//! │ "ASHM" · layout_ver · generation · key digest · producer_pid ·      │
//! │ slot_count · payload/meta capacity · offsets ‖ write_seq:AtomicU64 ·│
//! │ closed:AtomicU8 · reclaimed_seq · heartbeat · fallback_total        │
//! ├───────────── slot table (64 B per slot) ────────────────────────────┤
//! │ state:AtomicU8 (FREE/WRITING/READY) · refcount:AtomicU32 (the gate) │
//! │ seq:u64 · len:u32 · meta_len:u32 · commit_ns:u64                    │
//! ├───────────── consumer table (64 B per consumer) ────────────────────┤
//! │ state · drop token · cursor · heartbeat · pid · lag/received counts │
//! ├───────────── data slots (128 B aligned) ────────────────────────────┤
//! │ payload region ‖ metadata region, per slot                          │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Full byte offsets are documented in [`layout`], [`header`], [`slot`] and
//! [`consumer_table`]; each of those modules carries a `const _: () =
//! assert!(offset_of!(…))` block, so the documentation and the code cannot
//! drift apart.
//!
//! # Memory ordering, in one place
//!
//! Every ordering choice in the crate is one of five, and each is justified
//! where it is written. The summary:
//!
//! | Location | Ordering | Why |
//! |---|---|---|
//! | header magic: store at creation / load at attach | Release / Acquire | The **publication fence**. A reader that sees the magic sees a fully initialised header; a reader that maps a half-built segment sees zeroes and gets [`ShmError::BadMagic`]. |
//! | slot `refcount` (the gate): producer opens it on commit | Release | Publishes the payload bytes, `len`, `seq` and `state` written before it. |
//! | slot `refcount`: consumer pins with `compare_exchange_weak` | AcqRel / Acquire | Acquire synchronises with the commit; Release keeps this RMW inside the release sequence so a *second* consumer pinning on top still synchronises with the producer. |
//! | slot `refcount`: consumer unpins | Release | Orders the read against the producer's later acquiring reclaim. |
//! | slot `refcount`: producer reclaims with `compare_exchange(0, SENTINEL)` | AcqRel | Fails while any reader holds a pin — the property that makes zero-copy sound. |
//! | `write_seq`: publish / observe | Release / Acquire | The consumer's availability gate, stored *after* the slot's own release so an observed sequence is always pinnable. |
//! | `closed` | Release / Acquire | A consumer that observes `closed` also observes every message committed before it, so the drain cannot lose a tail. |
//! | consumer `cursor` | Release / Acquire | The producer reads cursors to decide what it may overwrite; a cursor must never appear to advance before the reads that justified it. |
//! | consumer entry `state` (claim / release) | AcqRel / Release | Transfers ownership of a table entry, and carries the cleared cursor to the next claimer. |
//! | counters (`fallback_total`, `pin_total`, statistics) | Relaxed | Monotone diagnostics; no algorithm branches on them. |
//! | cold header fields (`slot_count`, offsets, …) | Relaxed | Immutable after publication, but declared atomic because a peer process could in principle write them — see [`header`]. |
//!
//! There is **no `SeqCst` anywhere**, and that is a deliberate design
//! outcome: the ownership protocol routes every transfer through a single
//! memory location, so no store-then-load-of-another-location (Dekker)
//! pattern exists to need a full barrier. [`slot`] explains the alternative
//! that was rejected and why.
//!
//! # Trust model and soundness boundary
//!
//! [`Sample::payload`] returns a `&[u8]` pointing into a mapping that another
//! *process* can physically write. Rust's memory model has no way to prove
//! that peer well-behaved, so this must be stated rather than assumed:
//!
//! - **What the crate guarantees.** Against any peer that follows the slot
//!   protocol, a live [`Sample`] pins its slot, and a pinned slot is never
//!   reclaimed, overwritten, or re-entered by the producer — under either
//!   overflow policy. All bookkeeping is atomic, so concurrent access to the
//!   header, slot table and consumer table is well-defined. Every mapping is
//!   validated before a single pointer is formed into it ([`layout`]), so a
//!   corrupt or hostile *header* produces a typed error, never an
//!   out-of-bounds access.
//! - **What it does not.** A peer that ignores the protocol — writes into a
//!   slot it does not own, or forges the slot table — can change bytes under
//!   a live sample. That is the same trust a program places in any mapped
//!   file format, and the mitigation is architectural: a segment is only
//!   shared with processes of the same dataflow, the descriptor is brokered
//!   by the daemon (§6.3), and the mode is `0600` with an unguessable name.
//! - **The escape hatch.** [`Sample::to_vec`] copies out of shared memory for
//!   callers that would rather pay a copy than rely on peer discipline — the
//!   path recording (§14) takes, since a recorded frame must outlive the ring
//!   anyway.
//!
//! Consumers map read-**write**, not read-only. §6.2's "attach read-only" is
//! a statement about payloads, not page protection: the reclamation protocol
//! lives in shared atomics, so a reader must be able to write its cursor and
//! the slot refcounts. Payload immutability is enforced by the API surface —
//! [`Sample`] hands out `&[u8]` and nothing else.
//!
//! # Platform support
//!
//! Linux and macOS are P0. On Windows (§22, 0.2.0), [`Segment`] itself is
//! real — creation, attach-by-name, generation checking and teardown all
//! work, on top of a named section object (`os::windows`'s module docs —
//! a Windows-only module, so not linkable from every host platform's
//! rendered docs — spell out exactly how the crash-reclamation story maps
//! across). What is
//! not yet ported is [`Producer`], [`Consumer`], [`Doorbell`] and
//! [`SegmentBroker`]: their constructors return [`ShmError::Unsupported`]
//! there today (the `crate::unsupported` stubs, same as on a target with no
//! plane at all), so a caller degrades to the reliable daemon path (§6.3)
//! without a `cfg` in sight for *those* four types specifically. See [`os`]
//! for the per-platform syscall table.
//!
//! # A complete round trip
//!
//! ```
//! # #[cfg(unix)] {
//! use std::sync::Arc;
//! use astrs_shm::{AttachOptions, Consumer, OverflowPolicy, Producer, Segment, SegmentConfig, SegmentKey};
//! use astrs_wire::DataflowId;
//!
//! let key = SegmentKey::from_parts(DataflowId::generate(), "camera", "image", 1)?;
//! let config = SegmentConfig::new(16, 64 * 1024)?.with_overflow(OverflowPolicy::Block);
//! let segment = Segment::create_shared(key, config)?;
//!
//! let mut producer = Producer::new(Arc::clone(&segment))?;
//! let mut consumer = Consumer::attach(Arc::clone(&segment), AttachOptions::default())?;
//!
//! let mut window = producer.allocate(6)?;
//! window.as_mut_slice().copy_from_slice(b"frame0");
//! window.commit(b"hlc")?;
//!
//! let sample = consumer.try_next()?;
//! assert_eq!(sample.payload(), b"frame0");
//! assert_eq!(sample.payload_address() % 128, 0);
//! # }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod attach;
pub mod backoff;
pub mod config;
pub mod consumer_table;
pub mod error;
pub mod header;
pub mod key;
pub mod layout;
pub mod slot;
pub mod stats;

#[cfg(unix)]
pub mod broker;
#[cfg(unix)]
pub mod consumer;
#[cfg(unix)]
pub mod doorbell;
#[cfg(unix)]
pub mod fdpass;
#[cfg(unix)]
pub mod liveness;
#[cfg(unix)]
pub mod producer;
#[cfg(unix)]
pub mod protocol;

// [`segment`] is portable to Windows (§22, 0.2.0): a named section object
// stands in for `shm_open`/`mmap`, so segment creation, attach-by-name,
// generation checking and teardown all work there today. [`os`] — the
// platform layer it is built on — follows it onto the same two targets. The
// broker, doorbell, producer, consumer and liveness-watch modules above stay
// Unix-only; see `os::windows`'s module docs for exactly why.
#[cfg(any(unix, windows))]
pub mod os;
#[cfg(any(unix, windows))]
pub mod segment;

// Compiled (and unit-tested) on every platform, not just the ones that need
// it: a stub that only builds on Windows is a stub that silently rots. It is
// only *exported* where there is no real implementation to shadow.
#[cfg(any(not(unix), test))]
pub mod unsupported;

pub use attach::{AttachOptions, StartPosition};
pub use backoff::Backoff;
pub use config::{
    Backing, DEFAULT_CONSUMER_STALE_AFTER, DEFAULT_MAX_CONSUMERS, DEFAULT_META_CAPACITY,
    DEFAULT_POOL_SIZE, DEFAULT_SLOT_COUNT, DEFAULT_ZERO_COPY_THRESHOLD, ENV_POOL_SIZE,
    ENV_ZERO_COPY_THRESHOLD, MAX_MAX_CONSUMERS, MAX_META_CAPACITY, MAX_PAYLOAD_CAPACITY,
    MAX_SLOT_COUNT, OverflowPolicy, SegmentConfig, zero_copy_threshold,
};
pub use consumer_table::{
    CONSUMER_ACTIVE, CONSUMER_CLAIMING, CONSUMER_EMPTY, CONSUMER_EVICTED, CONSUMER_FLAG_DOORBELL,
    ConsumerEntry, ConsumerRegistration, ConsumerSnapshot,
};
pub use error::{CorruptReason, RecvError, ShmError, ShmResult};
pub use header::{MAGIC_WORD, SegmentHeader, SegmentHeaderView};
pub use key::{
    MACOS_SHM_NAME_MAX, SEGMENT_NAME_DIGEST_CHARS, SEGMENT_NAME_LEN, SEGMENT_NAME_PREFIX,
    SegmentKey, SegmentName,
};
pub use layout::{
    CONSUMER_ENTRY_LEN, HEADER_LEN, HeaderFields, LAYOUT_VERSION, MAX_SEGMENT_LEN, REGION_ALIGN,
    SEGMENT_MAGIC, SLOT_ENTRY_LEN, SegmentLayout, align_up,
};
pub use slot::{PinFailure, PinnedFacts, RECLAIM_SENTINEL, SlotHeader, SlotSnapshot, SlotState};
pub use stats::{BrokerStats, ConsumerStats, ProducerStats};

// The two export lists below are name-for-name identical for every type
// except `Segment`: everything else the data plane exposes is Unix-only
// today, so a downstream crate never needs a `cfg` to name it (blueprint
// §6.2 — the plane is an optimisation, not a compile-time fork). `Segment`
// breaks the symmetry on purpose (§22, 0.2.0): it is real on Windows too
// (see `segment`'s and `os::windows`'s module docs), so it is exported
// separately below, gated on `any(unix, windows)` rather than plain `unix`.
#[cfg(unix)]
pub use broker::{BrokerHandle, ProducerChannel, SegmentBroker, SegmentClient};
#[cfg(unix)]
pub use consumer::{Consumer, Sample};
#[cfg(unix)]
pub use doorbell::{Doorbell, DoorbellFanout, DoorbellRegistry, DoorbellRinger};
#[cfg(unix)]
pub use liveness::{DEFAULT_POLL_INTERVAL, ProcessWatch};
#[cfg(unix)]
pub use producer::{Producer, SampleMut};

#[cfg(not(unix))]
pub use unsupported::{
    BrokerHandle, Consumer, DEFAULT_POLL_INTERVAL, Doorbell, DoorbellFanout, DoorbellRegistry,
    DoorbellRinger, ProcessWatch, Producer, ProducerChannel, Sample, SampleMut, SegmentBroker,
    SegmentClient,
};

// `Segment` alone: real on Unix *and* Windows ([`segment::Segment`]), and
// only the uninhabited [`unsupported::Segment`] stub on a target with no
// data plane implementation at all.
#[cfg(any(unix, windows))]
pub use segment::Segment;
#[cfg(not(any(unix, windows)))]
pub use unsupported::Segment;

/// The current wall-clock reading in nanoseconds since the UNIX epoch.
///
/// Every heartbeat and commit stamp in the crate goes through this one
/// function, and it reads the same clock `astrs-time` does — so a commit
/// stamp in a slot and an [`astrs_time::HlcTimestamp`] in the metadata beside
/// it are on the same timescale, which is what makes the recording and
/// replay paths (§14) able to correlate them.
///
/// # Examples
///
/// ```
/// let first = astrs_shm::now_ns();
/// let second = astrs_shm::now_ns();
/// assert!(second >= first);
/// ```
#[must_use]
pub fn now_ns() -> u64 {
    use astrs_time::Clock;
    astrs_time::SystemClock.now_wall_ns()
}

/// The layout version, protocol version and platform this build speaks, as a
/// one-line banner for `astrs doctor` (§17).
///
/// # Examples
///
/// ```
/// let banner = astrs_shm::banner();
/// assert!(banner.contains("layout v"));
/// ```
#[must_use]
pub fn banner() -> String {
    format!(
        "astrs-shm layout v{LAYOUT_VERSION} · {} · {} slots default · {} MiB default pool",
        std::env::consts::OS,
        DEFAULT_SLOT_COUNT,
        DEFAULT_POOL_SIZE / (1024 * 1024),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    #[test]
    fn the_clock_is_monotone_enough_for_heartbeats() {
        let first = super::now_ns();
        let second = super::now_ns();
        assert!(second >= first);
        assert!(
            first > 1_600_000_000_000_000_000,
            "the wall clock looks unset"
        );
    }

    #[test]
    fn the_banner_names_the_layout_version() {
        let banner = super::banner();
        assert!(banner.contains("layout v1"), "{banner}");
        assert!(banner.contains(std::env::consts::OS), "{banner}");
    }
}
