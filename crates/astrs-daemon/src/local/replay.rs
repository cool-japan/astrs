//! The recorded clock — `astrs run --deterministic`'s virtual timeline (§14).
//!
//! > *Determinism mode: `astrs run --deterministic` fixes the timer wheel to
//! > the recording's HLC stream and delivers inputs in recorded order, making
//! > single-host replays reproducible to the message level.* — blueprint §14
//!
//! [`ReplaySource`] is that sentence. It is the second of the daemon's two
//! clock sources for the shared `astrs/timer/*` wheel (§11.1):
//!
//! | Source | Virtual now | Ticks land | Reproducible |
//! |---|---|---|---|
//! | wall clock (default) | [`std::time::Instant::now`] | whenever the host got round to it | no |
//! | [`ReplaySource`] | the recorded HLC point the cursor sits on | only at recorded points | yes |
//!
//! # One step, one recorded point
//!
//! The daemon's loop calls [`ReplaySource::advance`] once per iteration. Each
//! call moves the cursor forward by **at most one** recorded entry, and moving
//! it is what moves virtual time — so the set of instants the timer wheel is
//! ever advanced to is exactly the recording's HLC stream, whatever the host
//! is doing:
//!
//! ```text
//!   recorded stream   t0 ──── t1 ──── t2 ──── t3 ──── t4 (exhausted)
//!                      │       │       │       │       │
//!   virtual now ───────●───────●───────●───────●───────●
//!                      │       │       │       │       │
//!   injected entry     e0      e1      e2      e3      —
//!   timer ticks        ticks due at ≤ tk, stamped from tk
//! ```
//!
//! **The ordering rule, stated once**: at recorded point `T` the entry
//! stamped `T` is injected *first*, and the timers due at `≤ T` fire
//! *after* it. Both orders would be deterministic; this one is chosen
//! because it makes a tick the acknowledgement that everything recorded up
//! to that instant has already been delivered.
//!
//! # What the recording supplies, and what it does not
//!
//! Every recorded entry is a **clock point**. An entry is *also* replayed as
//! a message only when its producer is a node the run stood down —
//! `path: dynamic` after `astrs run --deterministic --from-recording`'s
//! rewrite. Entries produced by the reserved virtual node (`astrs/timer/*`,
//! `astrs/logs/*`, `astrs/status` — see [`crate::state::is_virtual_port`])
//! are never injected: the wheel produces those, and replaying them beside it
//! would double every tick. The daemon applies that filter, because only it
//! knows which nodes were stood down; this module carries the ports so it
//! can.
//!
//! # Pacing
//!
//! Virtual time and wall time are different questions. Virtual time is
//! always the recorded timeline, unscaled — that is what makes two runs
//! agree. [`ReplaySource::speed`] only decides how long the daemon *waits*
//! before stepping to the next recorded point: `None` steps as fast as the
//! loop can (no wall sleeps at all), `Some(1.0)` reproduces the original
//! wall-clock spacing, `Some(2.0)` halves it. Pacing changes how long a run
//! takes and nothing about what it produces.
//!
//! # Why the clock is held until the graph is up
//!
//! A wheel armed at a *different* virtual instant produces a different tick
//! grid, and node registration is a wall-clock race. So virtual time does
//! not move until the daemon says the graph is ready
//! ([`ReplaySource::release`]): every subscription is therefore armed at the
//! same virtual instant — the recording's first point — in every run.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::local::ReplaySource;
//! use astrs_recording::{Writer, WriterOptions};
//! use astrs_time::HlcTimestamp;
//! use astrs_wire::{DataId, DataflowId, Metadata, NodeId};
//! use std::time::Instant;
//!
//! let path = std::env::temp_dir().join(format!("astrs-doc-replay-{}.arec", std::process::id()));
//! let mut writer = Writer::create(
//!     &path,
//!     WriterOptions::new(DataflowId::from_u128(1), HlcTimestamp::EPOCH),
//! )?;
//! for physical in [1_000_u64, 2_000, 3_000] {
//!     writer.append_parts(
//!         NodeId::new("camera")?,
//!         DataId::new("frames")?,
//!         Metadata::new(HlcTimestamp::new(physical, 0)),
//!         vec![1],
//!     )?;
//! }
//! writer.finish()?;
//!
//! let origin = Instant::now();
//! let mut replay = ReplaySource::open(&path, None, origin)?;
//! assert_eq!(replay.len(), 3);
//! assert_eq!(replay.virtual_now(), origin, "frozen until the graph is up");
//!
//! replay.release(Instant::now());
//! let first = replay.advance(Instant::now()).expect("the first recorded entry");
//! assert_eq!(first.hlc(), HlcTimestamp::new(1_000, 0));
//! assert_eq!(replay.virtual_now(), origin, "the first point *is* the origin");
//!
//! let second = replay.advance(Instant::now()).expect("the second recorded entry");
//! assert_eq!(second.hlc(), HlcTimestamp::new(2_000, 0));
//! assert_eq!(
//!     replay.virtual_now(),
//!     origin + std::time::Duration::from_nanos(1_000),
//!     "virtual time moved by exactly the recorded gap"
//! );
//! # let _ = std::fs::remove_file(&path);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use astrs_recording::{Entry, Reader, RecordingError};
use astrs_time::HlcTimestamp;
use astrs_wire::{NodeId, PortRef};

/// A recording driving the daemon's virtual-source timer wheel (§14).
///
/// See the module documentation for the timeline, the ordering rule and the
/// pacing model.
pub struct ReplaySource {
    /// The open recording; entries are read one at a time, never all at once.
    reader: Reader,
    /// Positions into the reader's index, in the recording's canonical
    /// order — see [`canonical_order`].
    order: Vec<usize>,
    /// How far through [`Self::order`] the cursor has moved.
    cursor: usize,
    /// The recorded physical nanosecond virtual time is measured from.
    origin_ns: u64,
    /// The monotonic instant that recorded nanosecond maps to.
    origin: Instant,
    /// The recorded point virtual time currently sits on.
    at: HlcTimestamp,
    /// How many timer stamps have been handed out at [`Self::at`].
    stamps: u32,
    /// The pacing factor; [`None`] steps as fast as the loop can.
    speed: Option<f64>,
    /// When pacing started; [`None`] until the graph is up.
    released_at: Option<Instant>,
    /// Every producer port the recording covers.
    ports: BTreeSet<PortRef>,
    /// Every producer node the recording covers.
    producers: BTreeSet<NodeId>,
    /// The read failure that stopped the replay, if one did.
    failure: Option<String>,
    /// Whether the exhaustion consequences have already been applied.
    finalized: bool,
}

/// Written by hand because [`Reader`] holds an open file and is not
/// [`std::fmt::Debug`]: what a diagnostic wants from a replay source is where
/// its cursor is, not the file handle under it.
impl core::fmt::Debug for ReplaySource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReplaySource")
            .field("entries", &self.order.len())
            .field("cursor", &self.cursor)
            .field("at", &self.at)
            .field("speed", &self.speed)
            .field("released", &self.released_at.is_some())
            .field("ports", &self.ports.len())
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}

impl ReplaySource {
    /// Opens `path` as the clock source for a deterministic run.
    ///
    /// `origin` is the monotonic instant the recording's first point maps
    /// to — the daemon passes the instant it was constructed at, so the
    /// timer wheel's own epoch and this timeline share a zero.
    ///
    /// A footerless recording (a run killed before its writer finished) is
    /// recovered by scan, exactly as `astrs bag info` and `astrs replay`
    /// recover one: a determinism mode that refused the recordings a crash
    /// produces would be unusable for the case it exists to debug.
    ///
    /// # Errors
    ///
    /// [`RecordingError`] if the file cannot be opened, is not an `.arec`
    /// container, or cannot be recovered.
    pub fn open(path: &Path, speed: Option<f64>, origin: Instant) -> Result<Self, RecordingError> {
        let (reader, _recovery) = Reader::open_or_recover(path)?;
        let order = canonical_order(&reader);
        let origin_ns = order
            .first()
            .and_then(|position| reader.index().get(*position))
            .map_or(0, |entry| entry.hlc.physical_ns());
        let at = order
            .first()
            .and_then(|position| reader.index().get(*position))
            .map_or(HlcTimestamp::EPOCH, |entry| entry.hlc);
        let mut ports = BTreeSet::new();
        let mut producers = BTreeSet::new();
        for entry in reader.index() {
            ports.insert(PortRef::new(entry.node.clone(), entry.output.clone()));
            producers.insert(entry.node.clone());
        }
        Ok(Self {
            reader,
            order,
            cursor: 0,
            origin_ns,
            origin,
            at,
            stamps: 0,
            // A speed that is not a positive, finite number cannot describe
            // a pacing, so it is read as "no pacing" rather than turned into
            // an error the daemon would have to route somewhere: the CLI
            // refuses a nonsensical `--speed` before the run starts, and this
            // is the backstop for an embedder that did not.
            speed: speed.filter(|factor| factor.is_finite() && *factor > 0.0),
            released_at: None,
            ports,
            producers,
            failure: None,
            finalized: false,
        })
    }

    /// How many recorded entries the timeline has in total.
    #[must_use]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether the recording holds nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The pacing factor in force, if any.
    #[must_use]
    pub const fn speed(&self) -> Option<f64> {
        self.speed
    }

    /// Every producer port the recording covers.
    #[must_use]
    pub const fn ports(&self) -> &BTreeSet<PortRef> {
        &self.ports
    }

    /// Every producer node the recording covers.
    #[must_use]
    pub const fn producers(&self) -> &BTreeSet<NodeId> {
        &self.producers
    }

    /// Whether the recording carries anything for `port`.
    #[must_use]
    pub fn covers(&self, port: &PortRef) -> bool {
        self.ports.contains(port)
    }

    /// The read failure that stopped the replay, if one did.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Whether virtual time has been allowed to move yet.
    #[must_use]
    pub const fn is_released(&self) -> bool {
        self.released_at.is_some()
    }

    /// Lets virtual time start moving, pacing from `wall_now`.
    ///
    /// Idempotent: a second call is ignored, so the daemon may ask on every
    /// loop iteration without having to remember whether it already did.
    pub fn release(&mut self, wall_now: Instant) {
        if self.released_at.is_none() {
            self.released_at = Some(wall_now);
        }
    }

    /// Whether every recorded point has been stepped through (or a read
    /// failure ended the timeline early).
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.cursor >= self.order.len() || self.failure.is_some()
    }

    /// Claims the right to apply the end-of-recording consequences, once.
    ///
    /// Returns `true` exactly one time: on the first call made after the
    /// timeline is both released and exhausted. The daemon uses it to close
    /// the ports the recording stood in for and the timer subscriptions no
    /// further point could ever fire — see
    /// [`crate::server::core::Daemon::tick`].
    pub fn take_finalization(&mut self) -> bool {
        if self.finalized || !self.is_released() || !self.is_exhausted() {
            return false;
        }
        self.finalized = true;
        true
    }

    /// The recorded point virtual time currently sits on.
    #[must_use]
    pub const fn at(&self) -> HlcTimestamp {
        self.at
    }

    /// Virtual now: the monotonic instant the current recorded point maps to.
    ///
    /// Frozen at [`Self::open`]'s `origin` until the first
    /// [`Self::advance`] moves the cursor, and thereafter exactly
    /// `origin + (recorded point − first recorded point)`. Unscaled by
    /// [`Self::speed`] on purpose: pacing changes how long a run takes, never
    /// what it produces.
    #[must_use]
    pub fn virtual_now(&self) -> Instant {
        self.origin + Duration::from_nanos(self.at.physical_ns().saturating_sub(self.origin_ns))
    }

    /// The stamp for the next timer tick fired at the current point (§14).
    ///
    /// Derived from the recorded point rather than read from the daemon's own
    /// [`astrs_time::HlcClock`], because that clock's logical counter is bumped
    /// by *every* caller — a log record, a status event, a heartbeat — and a
    /// stamp that depends on how many of those happened is not reproducible.
    /// Successive ticks at one point get successive logical counters, so
    /// several intervals expiring together still produce distinct, ordered,
    /// reproducible stamps.
    pub fn tick_stamp(&mut self) -> HlcTimestamp {
        let logical = self
            .at
            .logical()
            .saturating_add(1)
            .saturating_add(self.stamps);
        self.stamps = self.stamps.saturating_add(1);
        HlcTimestamp::new(self.at.physical_ns(), logical)
    }

    /// How long the daemon should wait before the next recorded point is due.
    ///
    /// [`None`] means "this source has no opinion" — the timeline is not
    /// released yet, or it is exhausted — and the caller should fall back to
    /// its ordinary wall-clock deadline logic. [`Duration::ZERO`] means the
    /// next point is due now (always, when replaying as fast as possible).
    #[must_use]
    pub fn pacing_sleep(&self, wall_now: Instant) -> Option<Duration> {
        let released_at = self.released_at?;
        if self.is_exhausted() {
            return None;
        }
        let speed = match self.speed {
            None => return Some(Duration::ZERO),
            Some(speed) => speed,
        };
        let next_ns = self.peek_ns()?;
        Some(
            pacing_deadline(released_at, next_ns.saturating_sub(self.origin_ns), speed)
                .saturating_duration_since(wall_now),
        )
    }

    /// Steps to the next recorded point, returning the entry recorded there.
    ///
    /// Returns [`None`] — leaving virtual time exactly where it was — when
    /// the timeline is not released, is exhausted, or the next point is not
    /// due yet under [`Self::speed`]. At most one point is consumed per call,
    /// which is what keeps a timer tick between two recorded entries from
    /// being skipped over.
    pub fn advance(&mut self, wall_now: Instant) -> Option<Entry> {
        let released_at = self.released_at?;
        if self.failure.is_some() {
            return None;
        }
        let position = *self.order.get(self.cursor)?;
        let row = self.reader.index().get(position)?.clone();
        if let Some(speed) = self.speed {
            let due = pacing_deadline(
                released_at,
                row.hlc.physical_ns().saturating_sub(self.origin_ns),
                speed,
            );
            if wall_now < due {
                return None;
            }
        }
        self.cursor = self.cursor.saturating_add(1);
        match self.reader.read_at(&row) {
            Ok(entry) => {
                self.at = entry.hlc();
                self.stamps = 0;
                Some(entry)
            }
            Err(error) => {
                // A recording that stops being readable half way through
                // cannot produce a reproducible run, and pretending it can by
                // skipping the bad entry would make the *next* run disagree
                // if the read succeeded that time. The timeline ends here and
                // the daemon reports why.
                self.failure = Some(error.to_string());
                None
            }
        }
    }

    /// The physical nanosecond of the next recorded point, if there is one.
    fn peek_ns(&self) -> Option<u64> {
        let position = *self.order.get(self.cursor)?;
        self.reader
            .index()
            .get(position)
            .map(|row| row.hlc.physical_ns())
    }
}

/// The wall instant `elapsed_ns` of recorded time is due at, under `speed`.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)]
fn pacing_deadline(released_at: Instant, elapsed_ns: u64, speed: f64) -> Instant {
    let scaled = elapsed_ns as f64 / speed;
    // `as` on a float that does not fit saturates in Rust, so a recording
    // spanning an absurd range paces at the far end rather than wrapping.
    let nanos = if scaled.is_finite() && scaled >= 0.0 {
        scaled as u64
    } else {
        u64::MAX
    };
    released_at + Duration::from_nanos(nanos)
}

/// The recording's canonical order: HLC, then `(node, output, on-disk
/// position)`.
///
/// The same total order [`astrs_recording::Reader::iter_all`] yields, spelled
/// here because the daemon needs the *positions* (to read one entry at a time
/// against a borrow it also holds the cursor in) rather than an iterator that
/// borrows the reader for the whole run. Two entries sharing a timestamp
/// therefore replay in the same order they iterate in, run to run.
fn canonical_order(reader: &Reader) -> Vec<usize> {
    let index = reader.index();
    let mut order: Vec<usize> = (0..index.len()).collect();
    order.sort_by(|&a, &b| {
        index[a]
            .hlc
            .cmp(&index[b].hlc)
            .then_with(|| index[a].node.as_str().cmp(index[b].node.as_str()))
            .then_with(|| index[a].output.as_str().cmp(index[b].output.as_str()))
            .then_with(|| a.cmp(&b))
    });
    order
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::PathBuf;

    use astrs_recording::{Writer, WriterOptions};
    use astrs_wire::{DataId, DataflowId, Metadata};

    use super::*;

    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "astrs-daemon-replay-{}-{}-{label}",
            std::process::id(),
            uniq()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A recording of `points`, each `(physical_ns, node, output, payload)`.
    fn record(path: &Path, points: &[(u64, &str, &str, u8)]) {
        let mut writer = Writer::create(
            path,
            WriterOptions::new(DataflowId::from_u128(1), HlcTimestamp::EPOCH),
        )
        .unwrap();
        for (physical, node, output, byte) in points {
            writer
                .append_parts(
                    NodeId::new(*node).unwrap(),
                    DataId::new(*output).unwrap(),
                    Metadata::new(HlcTimestamp::new(*physical, 0)),
                    vec![*byte],
                )
                .unwrap();
        }
        writer.finish().unwrap();
    }

    /// A three-point, one-port recording twenty milliseconds apart.
    fn three_points(dir: &Path) -> PathBuf {
        let path = dir.join("session.arec");
        record(
            &path,
            &[
                (20_000_000, "camera", "frames", 1),
                (40_000_000, "camera", "frames", 2),
                (60_000_000, "camera", "frames", 3),
            ],
        );
        path
    }

    #[test]
    fn an_unreleased_timeline_does_not_move() {
        let dir = scratch("unreleased");
        let path = three_points(&dir);
        let origin = Instant::now();
        let mut replay = ReplaySource::open(&path, None, origin).unwrap();

        assert!(!replay.is_released());
        assert_eq!(replay.virtual_now(), origin);
        assert!(replay.advance(Instant::now()).is_none());
        assert_eq!(replay.virtual_now(), origin, "still frozen");
        assert!(replay.pacing_sleep(Instant::now()).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn virtual_time_follows_the_recorded_gaps_exactly() {
        let dir = scratch("gaps");
        let path = three_points(&dir);
        let origin = Instant::now();
        let mut replay = ReplaySource::open(&path, None, origin).unwrap();
        replay.release(Instant::now());

        assert!(replay.advance(Instant::now()).is_some());
        assert_eq!(replay.virtual_now(), origin);
        assert!(replay.advance(Instant::now()).is_some());
        assert_eq!(replay.virtual_now(), origin + Duration::from_millis(20));
        assert!(replay.advance(Instant::now()).is_some());
        assert_eq!(replay.virtual_now(), origin + Duration::from_millis(40));
        assert!(replay.is_exhausted());
        assert!(replay.advance(Instant::now()).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_call_consumes_at_most_one_point() {
        // The property that keeps a tick between two recorded entries from
        // being coalesced away: the loop gets a turn between every pair.
        let dir = scratch("one-at-a-time");
        let path = three_points(&dir);
        let mut replay = ReplaySource::open(&path, None, Instant::now()).unwrap();
        replay.release(Instant::now());

        let mut seen = 0;
        while replay.advance(Instant::now()).is_some() {
            seen += 1;
            assert!(seen <= 3, "advance consumed more than it should have");
        }
        assert_eq!(seen, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_timelines_over_one_recording_agree_point_for_point() {
        // The determinism claim, at the level this type is responsible for.
        let dir = scratch("agree");
        let path = three_points(&dir);

        let stamps = |origin: Instant| {
            let mut replay = ReplaySource::open(&path, None, origin).unwrap();
            replay.release(Instant::now());
            let mut out = Vec::new();
            while let Some(entry) = replay.advance(Instant::now()) {
                out.push((entry.hlc(), entry.payload.clone(), replay.tick_stamp()));
            }
            out
        };

        let first = stamps(Instant::now());
        let second = stamps(Instant::now());
        assert_eq!(first.len(), 3);
        assert_eq!(first, second, "the same recording must replay identically");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tick_stamps_are_distinct_and_ordered_within_one_point() {
        let dir = scratch("tick-stamps");
        let path = three_points(&dir);
        let mut replay = ReplaySource::open(&path, None, Instant::now()).unwrap();
        replay.release(Instant::now());
        let entry = replay.advance(Instant::now()).unwrap();

        let first = replay.tick_stamp();
        let second = replay.tick_stamp();
        assert_eq!(first.physical_ns(), entry.hlc().physical_ns());
        assert!(first > entry.hlc(), "a tick follows the entry at its point");
        assert!(second > first, "successive ticks are ordered");

        // Stepping to the next point restarts the per-point counter.
        let next = replay.advance(Instant::now()).unwrap();
        let third = replay.tick_stamp();
        assert_eq!(third.physical_ns(), next.hlc().physical_ns());
        assert_eq!(third.logical(), next.hlc().logical() + 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_speed_factor_paces_without_changing_the_timeline() {
        let dir = scratch("speed");
        let path = three_points(&dir);
        let origin = Instant::now();
        let released = Instant::now();
        let mut replay = ReplaySource::open(&path, Some(2.0), origin).unwrap();
        replay.release(released);

        // The first point is due immediately; the second is due after half
        // the recorded 20 ms gap at double speed.
        assert!(replay.advance(released).is_some());
        assert!(
            replay.advance(released).is_none(),
            "the second point is not due yet"
        );
        assert!(
            replay
                .advance(released + Duration::from_millis(10))
                .is_some()
        );
        assert_eq!(
            replay.virtual_now(),
            origin + Duration::from_millis(20),
            "pacing does not scale the virtual timeline"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn as_fast_as_possible_never_asks_the_loop_to_sleep() {
        let dir = scratch("afap");
        let path = three_points(&dir);
        let mut replay = ReplaySource::open(&path, None, Instant::now()).unwrap();
        replay.release(Instant::now());
        assert_eq!(replay.pacing_sleep(Instant::now()), Some(Duration::ZERO));

        while replay.advance(Instant::now()).is_some() {}
        assert!(
            replay.pacing_sleep(Instant::now()).is_none(),
            "an exhausted timeline has no opinion about sleeping"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_nonsense_speed_is_read_as_no_pacing() {
        let dir = scratch("bad-speed");
        let path = three_points(&dir);
        for speed in [Some(0.0), Some(-1.0), Some(f64::NAN), Some(f64::INFINITY)] {
            let replay = ReplaySource::open(&path, speed, Instant::now()).unwrap();
            assert_eq!(replay.speed(), None, "{speed:?}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_ports_and_producers_the_recording_covers_are_reported() {
        let dir = scratch("ports");
        let path = dir.join("two-ports.arec");
        record(
            &path,
            &[
                (10, "camera", "frames", 1),
                (20, "camera", "meta", 2),
                (30, "lidar", "scan", 3),
            ],
        );
        let replay = ReplaySource::open(&path, None, Instant::now()).unwrap();

        assert_eq!(replay.len(), 3);
        assert!(!replay.is_empty());
        assert_eq!(replay.producers().len(), 2);
        assert!(replay.covers(&PortRef::from_parts("camera", "frames").unwrap()));
        assert!(replay.covers(&PortRef::from_parts("lidar", "scan").unwrap()));
        assert!(!replay.covers(&PortRef::from_parts("camera", "absent").unwrap()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ties_replay_in_the_recording_s_canonical_order() {
        let dir = scratch("ties");
        let path = dir.join("ties.arec");
        record(
            &path,
            &[
                (100, "zebra", "out", 1),
                (100, "alpha", "out", 2),
                (100, "alpha", "aaa", 3),
            ],
        );
        let mut replay = ReplaySource::open(&path, None, Instant::now()).unwrap();
        replay.release(Instant::now());

        let mut seen = Vec::new();
        while let Some(entry) = replay.advance(Instant::now()) {
            seen.push((
                entry.node.as_str().to_owned(),
                entry.output.as_str().to_owned(),
            ));
        }
        assert_eq!(
            seen,
            vec![
                ("alpha".to_owned(), "aaa".to_owned()),
                ("alpha".to_owned(), "out".to_owned()),
                ("zebra".to_owned(), "out".to_owned()),
            ],
            "equal timestamps break on (node, output)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finalization_is_claimed_exactly_once_and_only_when_it_is_due() {
        let dir = scratch("finalize");
        let path = three_points(&dir);
        let mut replay = ReplaySource::open(&path, None, Instant::now()).unwrap();

        assert!(!replay.take_finalization(), "not released yet");
        replay.release(Instant::now());
        assert!(!replay.take_finalization(), "not exhausted yet");
        while replay.advance(Instant::now()).is_some() {}
        assert!(replay.take_finalization(), "due exactly now");
        assert!(!replay.take_finalization(), "and never again");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_recording_is_exhausted_from_the_start() {
        let dir = scratch("empty");
        let path = dir.join("empty.arec");
        record(&path, &[]);
        let origin = Instant::now();
        let mut replay = ReplaySource::open(&path, None, origin).unwrap();

        assert!(replay.is_empty());
        assert!(replay.is_exhausted());
        assert_eq!(replay.virtual_now(), origin);
        replay.release(Instant::now());
        assert!(replay.advance(Instant::now()).is_none());
        assert!(replay.take_finalization());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_is_not_a_recording_is_refused() {
        let dir = scratch("not-a-recording");
        let path = dir.join("nope.arec");
        std::fs::write(&path, b"not an arec container at all").unwrap();
        assert!(ReplaySource::open(&path, None, Instant::now()).is_err());
        assert!(ReplaySource::open(&dir.join("absent.arec"), None, Instant::now()).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
