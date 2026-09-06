//! [`TimerWheel`] — the hierarchical timing wheel driving every
//! `astrs/timer/*` subscription (blueprint §11.1).
//!
//! # Structure
//!
//! Five levels of 64 slots each, level *L*'s slots each spanning
//! `64^L` milliseconds — level 0 is millisecond resolution, and each level
//! above it is a coarser "overflow" wheel for scheduling that is not yet
//! close enough to need fine resolution. A registered timer starts in
//! whichever level can represent its remaining delay; as the wheel's clock
//! advances and a level's slot cycle completes, that slot's contents
//! *cascade* down into the appropriate lower level, becoming progressively
//! more precisely placed as their due time approaches. This gives `O(1)`
//! amortized work per elapsed millisecond regardless of how many timers are
//! registered, the classic timing-wheel trade (Varghese & Lauck, 1987)
//! against a heap's `O(log n)`.
//!
//! Total addressable range: `64^5 - 1` ms, about 12.4 days. A timer whose
//! delay exceeds that (there is no realistic `astrs/timer/*` use case that
//! would) is placed in the top level's slot as if it were within range —
//! its actual due instant, stored on the entry independent of wheel
//! placement, is still checked precisely before it is ever allowed to fire
//! (see "Never fires early" below), so this aliasing costs at most some
//! wasted reschedule cycles for an unrealistic configuration, never
//! incorrect delivery.
//!
//! # Never fires early
//!
//! Two independent mechanisms combine to guarantee a timer is never
//! delivered before its true scheduled instant:
//!
//! 1. Slot placement rounds a due [`Instant`] *up* to the millisecond
//!    (`ceil`, not truncation) before choosing a slot, while the wheel's
//!    own clock advances by *whole, floored* milliseconds. A slot can only
//!    be reached once the wheel's floored "now" has caught up to the
//!    ceiling of the true due instant — which is always at or after it.
//! 2. Independent of that arithmetic, [`resolve_due`](TimerWheel::resolve_due)
//!    re-checks the entry's exact [`Instant`] against the precise `now`
//!    passed to [`TimerWheel::advance`] before ever emitting a
//!    [`TimerFired`]. If a slot is reached "early" for any reason (the
//!    documented far-future aliasing above, or simply as a safety net
//!    against a bug in (1)), the entry is silently rescheduled one
//!    millisecond forward instead of firing — the wheel resolution is
//!    "may be up to ~1ms late", never "may be early".
//!
//! # Catch-up and missed ticks
//!
//! [`TimerWheel::advance`] can be called with `now` arbitrarily far ahead
//! of the wheel's own clock — a real driver task waking every millisecond
//! passes a `now` only a millisecond ahead (the common case, `O(1)`
//! amortized); a test driving a [`astrs_time::ManualClock`] may jump
//! minutes or days in one call. Below [`MAX_CATCHUP_TICKS_MS`] the wheel
//! walks forward one simulated millisecond at a time (still cheap — each
//! step touches only slots that actually cascade); above it, `advance`
//! takes an `O(registered timers)` resync path instead of an
//! `O(elapsed milliseconds)` one, so an arbitrarily large jump (resuming
//! after a suspend, a fast-forwarded replay) never turns into an
//! unboundedly long loop.
//!
//! Per-timer, falling behind is handled by [`MissedTickPolicy`], applied by
//! [`resolve_due`](TimerWheel::resolve_due) using [`astrs_time::TimerInterval`]'s
//! own drift-free arithmetic (`next_tick`/`ticks_between`) — the wheel
//! never re-derives that math itself.

use std::collections::HashMap;
use std::time::Instant;

use astrs_time::TimerInterval;

use super::p2::JitterStats;

/// Slots per wheel level.
const SLOTS_PER_LEVEL: u64 = 64;

/// Number of wheel levels (level 0 = 1ms resolution; each level above is
/// `SLOTS_PER_LEVEL`× coarser).
const LEVELS: usize = 5;

/// `LEVEL_WIDTHS[l]` = the width, in milliseconds, of one slot at level
/// `l` (`SLOTS_PER_LEVEL.pow(l)`), computed once rather than repeatedly at
/// runtime.
const LEVEL_WIDTHS: [u64; LEVELS] = [1, 64, 4_096, 262_144, 16_777_216];

/// Above this many elapsed milliseconds in one [`TimerWheel::advance`]
/// call, the wheel resyncs directly from its entry table instead of
/// simulating every intermediate millisecond (see the module docs).
/// Roughly 65.5 seconds — comfortably above any jitter a live 1ms driver
/// should ever accumulate, comfortably below "worth paying for the simpler
/// per-ms path".
const MAX_CATCHUP_TICKS_MS: u64 = 65_536;

/// The default cap on how many individual [`MissedTickPolicy::Burst`]
/// events one [`TimerWheel::advance`] call will emit for a single timer
/// before coalescing the remainder into one final event (blueprint's
/// "count reported" — see [`TimerFired::skipped`]).
pub const DEFAULT_BURST_CAP: u32 = 1_000;

/// Finds the wheel level and slot index for a timer due at `due_ms`,
/// relative to the wheel's current position `current_ms`.
///
/// A pure function of its two arguments — no wheel state — so it can be
/// (and is, in this module's tests) unit-tested in isolation from the rest
/// of the cascade machinery: `level_and_slot(delay, 0)` for a range of
/// `delay` values directly exercises the level-selection boundary at every
/// `SLOTS_PER_LEVEL^k`.
fn level_and_slot(due_ms: u64, current_ms: u64) -> (usize, usize) {
    // Guaranteed `>= 1` by every caller (`schedule_at` clamps `due_ms` to
    // at least `current_ms + 1`); the `.max(1)` here is a second,
    // belt-and-suspenders guard against ever computing a level from a
    // zero or negative delay.
    let delay = due_ms.saturating_sub(current_ms).max(1);
    let mut level = 0usize;
    while level + 1 < LEVELS && delay >= LEVEL_WIDTHS[level + 1] {
        level += 1;
    }
    let slot = ((due_ms / LEVEL_WIDTHS[level]) % SLOTS_PER_LEVEL) as usize;
    (level, slot)
}

/// Converts an [`Instant`] to whole milliseconds since `epoch`, rounding
/// *up*. See the module docs' "never fires early" section for why this
/// must be a ceiling, not a truncation.
fn ceil_ms_since_epoch(epoch: Instant, at: Instant) -> u64 {
    let nanos = at.saturating_duration_since(epoch).as_nanos();
    u64::try_from(nanos.div_ceil(1_000_000)).unwrap_or(u64::MAX)
}

/// Converts an [`Instant`] to whole milliseconds since `epoch`, rounding
/// down — the wheel's own advancing clock only ever claims milliseconds
/// that have *fully* elapsed.
fn floor_ms_since_epoch(epoch: Instant, at: Instant) -> u64 {
    let nanos = at.saturating_duration_since(epoch).as_nanos();
    u64::try_from(nanos / 1_000_000).unwrap_or(u64::MAX)
}

/// An opaque handle to one timer registered with a [`TimerWheel`].
///
/// Ids are assigned from a monotonic counter and never reused within one
/// wheel's lifetime, so there is no ABA hazard in holding one past a
/// [`TimerWheel::cancel`] call — it simply will not be found again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(u64);

/// What a [`TimerWheel`] should do when its driver falls behind a
/// registered timer by more than one period (blueprint §11.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissedTickPolicy {
    /// Deliver one [`TimerFired`] per missed period, back-to-back, each
    /// reporting `skipped: 0`. Bounded per [`TimerWheel::advance`] call by
    /// the wheel's burst cap ([`DEFAULT_BURST_CAP`] unless overridden via
    /// [`TimerWheel::with_burst_cap`]); if the cap is reached while
    /// periods are still due, the remainder is coalesced into one final
    /// event whose `skipped` count reports them — nothing is silently
    /// lost, only differently reported once the cap bites.
    Burst,
    /// Deliver exactly one [`TimerFired`] for the catch-up, with `skipped`
    /// reporting how many intermediate periods were coalesced into it.
    Skip,
}

/// A timer subscription to register with a [`TimerWheel`].
#[derive(Debug, Clone)]
pub struct TimerSpec {
    /// The drift-free periodic schedule (blueprint §8.4, §11.1).
    pub interval: TimerInterval,
    /// What to do if the wheel's driver falls behind on this timer.
    pub missed_tick_policy: MissedTickPolicy,
    /// An opaque, caller-defined label returned unchanged on every
    /// [`TimerFired`] this timer produces. `0` unless set via
    /// [`TimerSpec::with_tag`].
    ///
    /// One wheel serves every `astrs/timer/*` subscription across every
    /// node a daemon runs (blueprint §11.1); [`TimerWheel::insert`] hands
    /// back an opaque [`TimerId`], but a daemon delivering a fired tick
    /// still needs to know *which node's which input* it belongs to. A
    /// `u64` tag — typically an index into the daemon's own node/input
    /// table, or a small bitpacked `(node, input)` pair — lets a caller
    /// carry that association through the wheel without this crate
    /// needing to know the shape of a caller's routing table, and without
    /// making [`TimerWheel`] itself generic over a label type (which would
    /// force every user of this module, including ones with no routing
    /// need at all, to name that type parameter).
    pub tag: u64,
}

impl TimerSpec {
    /// A timer with no caller-defined tag (`tag: 0`).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_scheduler::{MissedTickPolicy, TimerSpec};
    /// use astrs_time::TimerInterval;
    ///
    /// let spec = TimerSpec::new(TimerInterval::from_millis(100)?, MissedTickPolicy::Burst);
    /// assert_eq!(spec.tag, 0);
    /// # Ok::<(), astrs_time::TimerIntervalError>(())
    /// ```
    #[must_use]
    pub const fn new(interval: TimerInterval, missed_tick_policy: MissedTickPolicy) -> Self {
        Self {
            interval,
            missed_tick_policy,
            tag: 0,
        }
    }

    /// Attaches an opaque caller-defined tag (see [`TimerSpec::tag`]'s
    /// docs), returning the timer with it set.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_scheduler::{MissedTickPolicy, TimerSpec};
    /// use astrs_time::TimerInterval;
    ///
    /// let spec = TimerSpec::new(TimerInterval::from_millis(100)?, MissedTickPolicy::Burst)
    ///     .with_tag(42);
    /// assert_eq!(spec.tag, 42);
    /// # Ok::<(), astrs_time::TimerIntervalError>(())
    /// ```
    #[must_use]
    pub const fn with_tag(mut self, tag: u64) -> Self {
        self.tag = tag;
        self
    }
}

/// One delivered timer tick, returned by [`TimerWheel::advance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerFired {
    /// Which timer fired.
    pub id: TimerId,
    /// The exact drift-free grid point
    /// ([`TimerInterval::next_tick`]) this delivery represents.
    pub scheduled_at: Instant,
    /// The instant [`TimerWheel::advance`] observed it as due — always
    /// `>= scheduled_at`. `fired_at - scheduled_at` is this tick's jitter,
    /// the value folded into [`JitterStats`].
    pub fired_at: Instant,
    /// How many earlier periods were coalesced into this delivery without
    /// a [`TimerFired`] of their own. See [`MissedTickPolicy`] for when
    /// this is nonzero.
    pub skipped: u64,
    /// The tag this timer was registered with ([`TimerSpec::tag`]),
    /// returned unchanged.
    pub tag: u64,
}

/// One registered timer's mutable state.
struct TimerEntry {
    interval: TimerInterval,
    policy: MissedTickPolicy,
    next_due: Instant,
    jitter: JitterStats,
    tag: u64,
}

/// A hierarchical timing wheel driving `N` registered [`TimerSpec`]
/// subscriptions with drift-free absolute scheduling (blueprint §11.1).
///
/// See the module docs for the wheel's structure, its "never fires early"
/// guarantee, and how it handles a driver falling behind. `TimerWheel` is
/// synchronous and holds no clock of its own — every method that needs
/// "now" takes it explicitly as an [`Instant`], making the type equally at
/// home behind a real 1ms [`crate::TimerWheelDriver`] and
/// under direct, sleep-free control from an
/// [`astrs_time::ManualClock`]-driven test or a §14 deterministic replay.
pub struct TimerWheel {
    epoch: Instant,
    current_ms: u64,
    levels: [Vec<Vec<TimerId>>; LEVELS],
    entries: HashMap<TimerId, TimerEntry>,
    next_id: u64,
    burst_cap: u32,
}

// Hand-written rather than derived: the `levels` array is 4 × 256 slot
// vectors, and printing it would bury the three numbers anybody actually
// wants (how many timers, how far the wheel has advanced, when the next one
// is due) under several kilobytes of empty slots. A daemon holding a wheel
// inside its own `#[derive(Debug)]` state is the intended caller.
impl std::fmt::Debug for TimerWheel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimerWheel")
            .field("timers", &self.entries.len())
            .field("current_ms", &self.current_ms)
            .field("burst_cap", &self.burst_cap)
            .field("next_deadline", &self.next_deadline())
            .finish_non_exhaustive()
    }
}

impl TimerWheel {
    /// Creates an empty wheel whose clock starts at `epoch`, with the
    /// default burst cap ([`DEFAULT_BURST_CAP`]).
    #[must_use]
    pub fn new(epoch: Instant) -> Self {
        Self::with_burst_cap(epoch, DEFAULT_BURST_CAP)
    }

    /// Creates an empty wheel with an explicit burst cap (see
    /// [`MissedTickPolicy::Burst`]).
    #[must_use]
    pub fn with_burst_cap(epoch: Instant, burst_cap: u32) -> Self {
        Self {
            epoch,
            current_ms: 0,
            levels: std::array::from_fn(|_| (0..SLOTS_PER_LEVEL).map(|_| Vec::new()).collect()),
            entries: HashMap::new(),
            next_id: 0,
            burst_cap,
        }
    }

    /// Registers a new timer, scheduling its first delivery at
    /// `spec.interval.next_tick(now)`.
    ///
    /// If `now` is at or behind the wheel's own current position (a stale
    /// caller, or a wheel that has been advanced further by something
    /// else since `now` was captured), the first delivery is clamped to
    /// the wheel's very next millisecond rather than being placed "in the
    /// past" — which would otherwise wedge it until the wheel's slot
    /// cursor comes all the way back around, up to the wheel's full ~12.4
    /// day range later.
    pub fn insert(&mut self, spec: TimerSpec, now: Instant) -> TimerId {
        let first_due = spec.interval.next_tick(now);
        let id = TimerId(self.next_id);
        self.next_id += 1;
        self.entries.insert(
            id,
            TimerEntry {
                interval: spec.interval,
                policy: spec.missed_tick_policy,
                next_due: first_due,
                jitter: JitterStats::new(),
                tag: spec.tag,
            },
        );
        self.schedule_at(id, first_due);
        id
    }

    /// Removes a timer. Returns whether it was registered.
    ///
    /// The id may still be present in a wheel slot after this call; that
    /// stale reference is dropped silently, at no extra cost, the next
    /// time that slot is processed (lazy deletion) — cancellation itself
    /// stays `O(1)` rather than needing to scrub every level.
    pub fn cancel(&mut self, id: TimerId) -> bool {
        self.entries.remove(&id).is_some()
    }

    /// Whether `id` is currently registered.
    #[must_use]
    pub fn contains(&self, id: TimerId) -> bool {
        self.entries.contains_key(&id)
    }

    /// This timer's running jitter statistics, if it is registered.
    #[must_use]
    pub fn jitter_stats(&self, id: TimerId) -> Option<&JitterStats> {
        self.entries.get(&id).map(|entry| &entry.jitter)
    }

    /// The earliest instant any currently-registered timer is next due, or
    /// `None` if nothing is registered.
    ///
    /// [`TimerWheelDriver`](crate::TimerWheelDriver) ticks at a fixed
    /// `tick_period` regardless of whether anything is actually due —
    /// simple and already correct, at the cost of waking a battery-powered
    /// robot's daemon at that rate even while idle. A caller that wants to
    /// sleep only until something is truly due instead (a variable-rate
    /// driver loop, or the daemon's own scheduling of when to next call
    /// [`TimerWheel::advance`]) can compute
    /// `next_deadline().map(|due| due.saturating_duration_since(now))` and
    /// sleep for that instead of polling at a fixed rate. Doing so safely
    /// still requires waking early on [`TimerWheel::insert`] of a timer due
    /// sooner than the current sleep — an ordinary condition-variable-style
    /// wakeup, not something this synchronous, clock-agnostic type owns.
    ///
    /// `O(registered timers)`: a plain linear minimum, not tracked
    /// incrementally. Cheap next to the wheel's own per-elapsed-millisecond
    /// cascade cost, and intended to be called once per driver loop
    /// iteration — before deciding how long to sleep — not once per
    /// simulated millisecond the way [`TimerWheel::advance`]'s internal
    /// work is.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_scheduler::{MissedTickPolicy, TimerSpec, TimerWheel};
    /// use astrs_time::TimerInterval;
    /// use std::time::{Duration, Instant};
    ///
    /// # fn main() -> Result<(), astrs_time::TimerIntervalError> {
    /// let epoch = Instant::now();
    /// let mut wheel = TimerWheel::new(epoch);
    /// assert_eq!(wheel.next_deadline(), None, "nothing registered yet");
    ///
    /// let slow = TimerInterval::with_anchor(Duration::from_millis(100), epoch)?;
    /// let fast = TimerInterval::with_anchor(Duration::from_millis(10), epoch)?;
    /// wheel.insert(TimerSpec::new(slow, MissedTickPolicy::Burst), epoch);
    /// wheel.insert(TimerSpec::new(fast, MissedTickPolicy::Burst), epoch);
    ///
    /// // The 10ms timer is due first, regardless of registration order.
    /// assert_eq!(wheel.next_deadline(), Some(epoch + Duration::from_millis(10)));
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.entries.values().map(|entry| entry.next_due).min()
    }

    /// Advances the wheel's clock to `now`, returning every [`TimerFired`]
    /// this produced, sorted by `scheduled_at` (ties broken by [`TimerId`]
    /// order) — deterministic delivery order across timers even when
    /// several catch up within the same call, which is what §14 replay's
    /// HLC-ordered delivery and this wheel's own tests both rely on.
    ///
    /// `now` behind the wheel's current position is a no-op (returns an
    /// empty `Vec`) rather than an error — time only ever moves the wheel
    /// forward.
    pub fn advance(&mut self, now: Instant) -> Vec<TimerFired> {
        let now_ms = floor_ms_since_epoch(self.epoch, now);
        let mut fired = Vec::new();
        if now_ms <= self.current_ms {
            return fired;
        }

        if now_ms - self.current_ms > MAX_CATCHUP_TICKS_MS {
            self.resync(now, now_ms, &mut fired);
        } else {
            while self.current_ms < now_ms {
                self.tick_one_ms(now, &mut fired);
            }
        }

        fired.sort_by(|a, b| {
            a.scheduled_at
                .cmp(&b.scheduled_at)
                .then(a.id.0.cmp(&b.id.0))
        });
        fired
    }

    /// Advances the wheel's internal clock by exactly one millisecond,
    /// cascading any level whose slot cycle just completed, then
    /// resolving whatever landed in level 0's slot for the new position.
    fn tick_one_ms(&mut self, target_now: Instant, fired: &mut Vec<TimerFired>) {
        self.current_ms += 1;

        // Cascade level 1 upward. Level `l`'s boundary is a multiple of
        // level `l - 1`'s (both are powers of `SLOTS_PER_LEVEL`), so the
        // first level that has *not* just wrapped means no higher level
        // has either — safe to stop scanning right there.
        for (level, &width) in LEVEL_WIDTHS.iter().enumerate().skip(1) {
            if !self.current_ms.is_multiple_of(width) {
                break;
            }
            let slot = ((self.current_ms / width) % SLOTS_PER_LEVEL) as usize;
            let ids = std::mem::take(&mut self.levels[level][slot]);
            for id in ids {
                if let Some(entry) = self.entries.get(&id) {
                    let due_ms = ceil_ms_since_epoch(self.epoch, entry.next_due);
                    let (new_level, new_slot) = level_and_slot(due_ms, self.current_ms);
                    self.levels[new_level][new_slot].push(id);
                }
                // Else: cancelled since it was scheduled; drop it.
            }
        }

        let slot0 = (self.current_ms % SLOTS_PER_LEVEL) as usize;
        let due_now = std::mem::take(&mut self.levels[0][slot0]);
        for id in due_now {
            self.resolve_due(id, target_now, fired);
        }
    }

    /// The catch-up path for a gap wider than [`MAX_CATCHUP_TICKS_MS`]:
    /// jumps the wheel's clock straight to `now_ms` and resolves every
    /// registered entry directly (`O(registered timers)`) rather than
    /// simulating every intermediate millisecond (`O(elapsed
    /// milliseconds)`, unbounded for an arbitrarily long gap).
    fn resync(&mut self, now: Instant, now_ms: u64, fired: &mut Vec<TimerFired>) {
        self.current_ms = now_ms;
        for level in &mut self.levels {
            for slot in level.iter_mut() {
                slot.clear();
            }
        }
        let ids: Vec<TimerId> = self.entries.keys().copied().collect();
        for id in ids {
            self.resolve_due(id, now, fired);
        }
    }

    /// Applies `id`'s [`MissedTickPolicy`] against `now`, appending
    /// whatever [`TimerFired`] events result, then reschedules it. A no-op
    /// (beyond a reschedule) if `id` was cancelled or is not actually due
    /// yet — see the module docs' "never fires early" section for why the
    /// latter check exists even though slot placement should already
    /// guarantee it.
    fn resolve_due(&mut self, id: TimerId, now: Instant, fired: &mut Vec<TimerFired>) {
        let Some(entry) = self.entries.get_mut(&id) else {
            return;
        };
        let tag = entry.tag;

        if entry.next_due > now {
            let due = entry.next_due;
            self.schedule_at(id, due);
            return;
        }

        match entry.policy {
            MissedTickPolicy::Burst => {
                let mut emitted = 0u32;
                while entry.next_due <= now && emitted < self.burst_cap {
                    let scheduled_at = entry.next_due;
                    entry
                        .jitter
                        .observe(now.saturating_duration_since(scheduled_at));
                    fired.push(TimerFired {
                        id,
                        scheduled_at,
                        fired_at: now,
                        skipped: 0,
                        tag,
                    });
                    entry.next_due = entry.interval.next_tick(scheduled_at);
                    emitted += 1;
                }
                if entry.next_due <= now {
                    // The cap was hit with more periods still due: report
                    // them as one final coalesced event instead of
                    // dropping them on the floor.
                    let scheduled_at = entry.next_due;
                    let remaining = 1 + entry.interval.ticks_between(scheduled_at, now);
                    entry
                        .jitter
                        .observe(now.saturating_duration_since(scheduled_at));
                    fired.push(TimerFired {
                        id,
                        scheduled_at,
                        fired_at: now,
                        skipped: remaining - 1,
                        tag,
                    });
                    entry.next_due = entry.interval.next_tick(now);
                }
            }
            MissedTickPolicy::Skip => {
                let scheduled_at = entry.next_due;
                let skipped = entry.interval.ticks_between(scheduled_at, now);
                entry
                    .jitter
                    .observe(now.saturating_duration_since(scheduled_at));
                fired.push(TimerFired {
                    id,
                    scheduled_at,
                    fired_at: now,
                    skipped,
                    tag,
                });
                entry.next_due = entry.interval.next_tick(now);
            }
        }

        let next_due = entry.next_due;
        self.schedule_at(id, next_due);
    }

    /// Places `id` into the level/slot matching `due_instant`, clamped so
    /// it never lands at or behind the wheel's current position (see
    /// [`TimerWheel::insert`]'s docs for why that clamp matters).
    fn schedule_at(&mut self, id: TimerId, due_instant: Instant) {
        let due_ms = ceil_ms_since_epoch(self.epoch, due_instant).max(self.current_ms + 1);
        let (level, slot) = level_and_slot(due_ms, self.current_ms);
        self.levels[level][slot].push(id);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::time::Duration;

    fn burst(interval: TimerInterval) -> TimerSpec {
        TimerSpec::new(interval, MissedTickPolicy::Burst)
    }

    fn skip(interval: TimerInterval) -> TimerSpec {
        TimerSpec::new(interval, MissedTickPolicy::Skip)
    }

    // -- `level_and_slot`: pure-function boundary table (advisor-style
    // isolation of the cascade math from end-to-end tick behavior). --

    #[test]
    fn the_wheel_renders_a_summary_rather_than_its_slot_array() {
        let start = Instant::now();
        let mut wheel = TimerWheel::new(start);
        wheel.insert(
            TimerSpec::new(
                TimerInterval::from_millis(10).unwrap(),
                MissedTickPolicy::Skip,
            ),
            start,
        );

        let rendered = format!("{wheel:?}");
        assert!(rendered.starts_with("TimerWheel"), "{rendered}");
        assert!(rendered.contains("timers: 1"), "{rendered}");
        assert!(rendered.contains("current_ms"), "{rendered}");
        assert!(
            rendered.len() < 512,
            "the 4x256 slot array must not be printed: {} bytes",
            rendered.len()
        );
    }

    #[test]
    fn level_and_slot_selects_level_zero_within_the_first_64ms() {
        assert_eq!(level_and_slot(1, 0), (0, 1));
        assert_eq!(level_and_slot(63, 0), (0, 63));
    }

    #[test]
    fn level_and_slot_crosses_into_level_one_at_64ms() {
        assert_eq!(level_and_slot(64, 0), (1, 1));
        assert_eq!(level_and_slot(4_095, 0), (1, 63));
    }

    #[test]
    fn level_and_slot_crosses_into_level_two_at_4096ms() {
        assert_eq!(level_and_slot(4_096, 0), (2, 1));
        assert_eq!(level_and_slot(262_143, 0), (2, 63));
    }

    #[test]
    fn level_and_slot_crosses_into_level_three_at_262144ms() {
        assert_eq!(level_and_slot(262_144, 0), (3, 1));
    }

    #[test]
    fn level_and_slot_is_relative_to_current_ms_not_absolute() {
        // The same absolute `due_ms` (5000) lands at a different level
        // depending on how close `current_ms` already is: a delay of 50ms
        // (level 0) versus the full 5000ms from the origin (level 2, since
        // 5000 clears both the 64ms and the 4096ms boundary).
        assert_eq!(level_and_slot(5_000, 4_950), (0, 5_000 % 64));
        assert_eq!(level_and_slot(5_000, 0), (2, 1));
    }

    #[test]
    fn level_and_slot_clamps_a_non_positive_delay_to_level_zero() {
        // Defensive: callers are expected to clamp `due_ms > current_ms`
        // before calling this, but the function itself must not panic or
        // pick a nonsensical level if that invariant is somehow violated.
        assert_eq!(level_and_slot(100, 200).0, 0);
    }

    // -- ms conversion helpers --

    #[test]
    fn ceil_rounds_up_a_sub_millisecond_remainder() {
        let epoch = Instant::now();
        let at = epoch + Duration::from_micros(16_667); // 16.667ms
        assert_eq!(ceil_ms_since_epoch(epoch, at), 17);
        assert_eq!(floor_ms_since_epoch(epoch, at), 16);
    }

    #[test]
    fn ceil_and_floor_agree_on_an_exact_millisecond() {
        let epoch = Instant::now();
        let at = epoch + Duration::from_millis(50);
        assert_eq!(ceil_ms_since_epoch(epoch, at), 50);
        assert_eq!(floor_ms_since_epoch(epoch, at), 50);
    }

    // -- End-to-end: expected sequence always derived from
    // `TimerInterval::next_tick` directly, never from the wheel's own
    // prior output. --

    #[test]
    fn a_60hz_timer_never_fires_before_its_true_grid_point() {
        // The advisor-flagged truncation trap: 60Hz has a 16.6667ms
        // period, which truncation would round down to slot 16 (firing
        // ~0.67ms early). Ground truth comes from `TimerInterval` itself.
        //
        // The wheel has millisecond resolution, so `expected_first` itself
        // (a fractional-millisecond instant) is not a wheel-observable
        // boundary at all: the earliest the wheel can even *look* at this
        // timer's slot is once its clock reaches the *ceiling*
        // millisecond, per the module docs' "never fires early" section.
        // This test exercises exactly that boundary: nothing at or before
        // the floor millisecond, delivered (with the exact, unrounded
        // `scheduled_at`) once the ceiling millisecond is reached.
        let epoch = Instant::now();
        let interval =
            TimerInterval::with_anchor(Duration::from_secs_f64(1.0 / 60.0), epoch).unwrap();
        let expected_first = interval.next_tick(epoch);
        let ceiling_ms = ceil_ms_since_epoch(epoch, expected_first);

        let mut wheel = TimerWheel::new(epoch);
        let id = wheel.insert(burst(interval), epoch);

        let just_before_the_ceiling_ms = epoch + Duration::from_millis(ceiling_ms - 1);
        assert!(
            wheel.advance(just_before_the_ceiling_ms).is_empty(),
            "must not fire before the wheel's clock reaches the ceiling millisecond"
        );

        let fired = wheel.advance(epoch + Duration::from_millis(ceiling_ms));
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].id, id);
        assert_eq!(
            fired[0].scheduled_at, expected_first,
            "delivered late by wheel resolution, but the recorded grid point is exact"
        );
    }

    #[test]
    fn ten_consecutive_ticks_match_next_tick_computed_independently() {
        let epoch = Instant::now();
        let interval = TimerInterval::from_millis(30).unwrap().rebase(epoch);
        let mut wheel = TimerWheel::new(epoch);
        let id = wheel.insert(burst(interval), epoch);

        let mut expected = interval.next_tick(epoch);
        for _ in 0..10 {
            let fired = wheel.advance(expected + Duration::from_millis(1));
            assert_eq!(fired.len(), 1, "one tick per period, no drift");
            assert_eq!(fired[0].id, id);
            assert_eq!(fired[0].scheduled_at, expected);
            assert_eq!(fired[0].skipped, 0);
            expected = interval.next_tick(expected);
        }
    }

    #[test]
    fn burst_policy_delivers_one_event_per_missed_period() {
        let epoch = Instant::now();
        let interval = TimerInterval::from_millis(10).unwrap().rebase(epoch);
        let mut wheel = TimerWheel::new(epoch);
        let id = wheel.insert(burst(interval), epoch);

        // Jump straight past 5 periods without intermediate `advance`s.
        let jump_to = epoch + Duration::from_millis(55);
        let fired = wheel.advance(jump_to);

        assert_eq!(fired.len(), 5, "one delivery per missed 10ms period");
        assert!(fired.iter().all(|f| f.id == id && f.skipped == 0));
        let expected: Vec<Instant> = (1..=5)
            .map(|k| epoch + Duration::from_millis(10) * k)
            .collect();
        let actual: Vec<Instant> = fired.iter().map(|f| f.scheduled_at).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn skip_policy_coalesces_missed_periods_into_one_event() {
        let epoch = Instant::now();
        let interval = TimerInterval::from_millis(10).unwrap().rebase(epoch);
        let mut wheel = TimerWheel::new(epoch);
        let id = wheel.insert(skip(interval), epoch);

        let jump_to = epoch + Duration::from_millis(55);
        let fired = wheel.advance(jump_to);

        assert_eq!(fired.len(), 1, "skip coalesces into a single delivery");
        assert_eq!(fired[0].id, id);
        assert_eq!(fired[0].scheduled_at, epoch + Duration::from_millis(10));
        // Grid points at 20,30,40,50ms all fall in (10ms, 55ms]: 4 skipped.
        assert_eq!(fired[0].skipped, 4);

        // The next delivery resumes from the caught-up point, not from the
        // original grid — no burst of stale ticks left behind.
        let next = wheel.advance(epoch + Duration::from_millis(65));
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].skipped, 0);
    }

    #[test]
    fn burst_cap_coalesces_the_remainder_instead_of_losing_it() {
        let epoch = Instant::now();
        let interval = TimerInterval::from_millis(1).unwrap().rebase(epoch);
        let mut wheel = TimerWheel::with_burst_cap(epoch, 3);
        let id = wheel.insert(burst(interval), epoch);

        // 7 periods due (1..=7ms); cap is 3.
        let fired = wheel.advance(epoch + Duration::from_millis(7));

        assert_eq!(fired.len(), 4, "3 individual + 1 coalesced final event");
        for individual in &fired[..3] {
            assert_eq!(individual.id, id);
            assert_eq!(individual.skipped, 0);
        }
        let last = fired[3];
        assert_eq!(last.scheduled_at, epoch + Duration::from_millis(4));
        // Periods at 4,5,6,7ms remain: delivered as one event covering all
        // 4, so 3 are reported as skipped (the 4th is this delivery
        // itself) -- accounting for every one of the original 7 periods
        // across the 3 individual events plus this final one.
        assert_eq!(last.skipped, 3);
    }

    #[test]
    fn two_timers_catching_up_together_are_returned_in_chronological_order() {
        let epoch = Instant::now();
        let mut wheel = TimerWheel::new(epoch);
        let slow = wheel.insert(
            burst(TimerInterval::from_millis(30).unwrap().rebase(epoch)),
            epoch,
        );
        let fast = wheel.insert(
            burst(TimerInterval::from_millis(15).unwrap().rebase(epoch)),
            epoch,
        );

        // `fast` fires at 15,30,45ms; `slow` fires at 30ms: chronologically
        // [15(fast), 30(slow), 30(fast), 45(fast)] once tie-broken by id --
        // `slow` was registered first (`TimerId(0)`), so it sorts first at
        // the 30ms tie under ascending-`TimerId` tie-breaking.
        let fired = wheel.advance(epoch + Duration::from_millis(46));
        let order: Vec<Instant> = fired.iter().map(|f| f.scheduled_at).collect();
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(order, sorted, "already in chronological order");

        let at_30: Vec<TimerId> = fired
            .iter()
            .filter(|f| f.scheduled_at == epoch + Duration::from_millis(30))
            .map(|f| f.id)
            .collect();
        assert_eq!(
            at_30,
            vec![slow, fast],
            "tie broken by ascending TimerId order"
        );
    }

    #[test]
    fn inserting_with_a_stale_now_still_schedules_ahead_of_the_wheel() {
        let epoch = Instant::now();
        let mut wheel = TimerWheel::new(epoch);
        // Move the wheel's own clock forward first.
        let _ = wheel.advance(epoch + Duration::from_millis(500));

        // Insert as if `now` were still back at the epoch -- stale by 500ms.
        let interval = TimerInterval::from_millis(10).unwrap().rebase(epoch);
        let id = wheel.insert(burst(interval), epoch);

        // It must fire soon (within a couple of milliseconds), not after
        // the wheel comes all the way back around (~12.4 days later).
        let fired = wheel.advance(epoch + Duration::from_millis(503));
        assert!(
            !fired.is_empty(),
            "must not be wedged for a full revolution"
        );
        assert!(fired.iter().any(|f| f.id == id));
    }

    #[test]
    fn cancel_prevents_further_delivery_even_if_already_scheduled() {
        let epoch = Instant::now();
        let mut wheel = TimerWheel::new(epoch);
        let interval = TimerInterval::from_millis(10).unwrap().rebase(epoch);
        let id = wheel.insert(burst(interval), epoch);

        assert!(wheel.cancel(id));
        assert!(!wheel.cancel(id), "already cancelled");
        assert!(!wheel.contains(id));

        let fired = wheel.advance(epoch + Duration::from_millis(100));
        assert!(fired.is_empty(), "a cancelled timer must never fire");
    }

    #[test]
    fn resync_path_handles_a_gap_far_beyond_the_catch_up_threshold() {
        let epoch = Instant::now();
        let interval = TimerInterval::from_secs(1).unwrap().rebase(epoch);
        let mut wheel = TimerWheel::new(epoch);
        let id = wheel.insert(skip(interval), epoch);

        // Comfortably beyond `MAX_CATCHUP_TICKS_MS` (~65.5s): a 2-hour gap.
        let jump_to = epoch + Duration::from_secs(2 * 60 * 60);
        let fired = wheel.advance(jump_to);

        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].id, id);
        assert_eq!(fired[0].scheduled_at, epoch + Duration::from_secs(1));
        // Grid points are at 1s, 2s, ..., 7200s; `skipped` counts those
        // strictly after `scheduled_at` (1s) up to and including `now`
        // (7200s) -- 7199 of them (2s..=7200s).
        assert_eq!(fired[0].skipped, 2 * 60 * 60 - 1);

        // The wheel keeps working correctly afterward.
        let next = wheel.advance(jump_to + Duration::from_secs(1));
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].skipped, 0);
    }

    #[test]
    fn jitter_stats_accumulate_across_deliveries() {
        let epoch = Instant::now();
        let interval = TimerInterval::from_millis(10).unwrap().rebase(epoch);
        let mut wheel = TimerWheel::new(epoch);
        let id = wheel.insert(burst(interval), epoch);

        assert_eq!(wheel.jitter_stats(id).unwrap().samples(), 0);
        // Deliver exactly on the grid: zero jitter.
        let _ = wheel.advance(epoch + Duration::from_millis(10));
        assert_eq!(wheel.jitter_stats(id).unwrap().samples(), 1);
        assert_eq!(wheel.jitter_stats(id).unwrap().max(), Duration::ZERO);

        // Deliver 3ms late.
        let _ = wheel.advance(epoch + Duration::from_millis(23));
        assert_eq!(wheel.jitter_stats(id).unwrap().samples(), 2);
        assert_eq!(
            wheel.jitter_stats(id).unwrap().max(),
            Duration::from_millis(3)
        );
    }

    #[test]
    fn jitter_stats_is_none_for_an_unknown_or_cancelled_timer() {
        let epoch = Instant::now();
        let mut wheel = TimerWheel::new(epoch);
        let interval = TimerInterval::from_millis(10).unwrap().rebase(epoch);
        let id = wheel.insert(burst(interval), epoch);
        assert!(wheel.cancel(id));
        assert!(wheel.jitter_stats(id).is_none());
    }

    #[test]
    fn advance_with_now_behind_the_wheel_is_a_no_op() {
        let epoch = Instant::now();
        let mut wheel = TimerWheel::new(epoch);
        let _ = wheel.advance(epoch + Duration::from_millis(100));
        assert!(wheel.advance(epoch + Duration::from_millis(50)).is_empty());
    }

    #[test]
    fn multiple_independent_timers_at_different_periods_stay_accurate_over_many_ticks() {
        let epoch = Instant::now();
        let mut wheel = TimerWheel::new(epoch);
        let periods_ms = [1u64, 3, 7, 16, 64, 100];
        let ids: Vec<TimerId> = periods_ms
            .iter()
            .map(|&ms| {
                wheel.insert(
                    burst(TimerInterval::from_millis(ms).unwrap().rebase(epoch)),
                    epoch,
                )
            })
            .collect();

        let mut delivered: HashMap<TimerId, u64> = HashMap::new();
        let end = epoch + Duration::from_millis(1000);
        let mut now = epoch;
        while now < end {
            now += Duration::from_millis(1);
            for fired in wheel.advance(now) {
                *delivered.entry(fired.id).or_insert(0) += 1;
                assert_eq!(
                    fired.skipped, 0,
                    "1ms-driven advance should never miss a tick"
                );
            }
        }

        for (period, id) in periods_ms.iter().zip(ids.iter()) {
            let expected = 1000 / period;
            let actual = delivered.get(id).copied().unwrap_or(0);
            assert_eq!(actual, expected, "period {period}ms over 1000ms");
        }
    }

    // -- `TimerWheel::next_deadline` --

    #[test]
    fn next_deadline_is_none_for_an_empty_wheel() {
        let wheel = TimerWheel::new(Instant::now());
        assert_eq!(wheel.next_deadline(), None);
    }

    #[test]
    fn next_deadline_is_the_earliest_registered_timer_regardless_of_insertion_order() {
        let epoch = Instant::now();
        let mut wheel = TimerWheel::new(epoch);
        // Registered slowest-first, so a naive "last inserted" bug would
        // report the wrong one.
        wheel.insert(
            burst(TimerInterval::with_anchor(Duration::from_millis(500), epoch).unwrap()),
            epoch,
        );
        wheel.insert(
            burst(TimerInterval::with_anchor(Duration::from_millis(100), epoch).unwrap()),
            epoch,
        );
        wheel.insert(
            burst(TimerInterval::with_anchor(Duration::from_millis(10), epoch).unwrap()),
            epoch,
        );

        assert_eq!(
            wheel.next_deadline(),
            Some(epoch + Duration::from_millis(10))
        );
    }

    #[test]
    fn next_deadline_advances_once_the_earliest_timer_fires_and_reschedules() {
        let epoch = Instant::now();
        let mut wheel = TimerWheel::new(epoch);
        wheel.insert(
            burst(TimerInterval::with_anchor(Duration::from_millis(10), epoch).unwrap()),
            epoch,
        );
        wheel.insert(
            burst(TimerInterval::with_anchor(Duration::from_millis(100), epoch).unwrap()),
            epoch,
        );
        assert_eq!(
            wheel.next_deadline(),
            Some(epoch + Duration::from_millis(10))
        );

        let _ = wheel.advance(epoch + Duration::from_millis(10));
        // The 10ms timer just fired and rescheduled to 20ms, which is still
        // earlier than the 100ms timer's first delivery.
        assert_eq!(
            wheel.next_deadline(),
            Some(epoch + Duration::from_millis(20))
        );
    }

    #[test]
    fn next_deadline_ignores_a_cancelled_timer() {
        let epoch = Instant::now();
        let mut wheel = TimerWheel::new(epoch);
        let soon = wheel.insert(
            burst(TimerInterval::with_anchor(Duration::from_millis(10), epoch).unwrap()),
            epoch,
        );
        wheel.insert(
            burst(TimerInterval::with_anchor(Duration::from_millis(100), epoch).unwrap()),
            epoch,
        );

        assert!(wheel.cancel(soon));
        assert_eq!(
            wheel.next_deadline(),
            Some(epoch + Duration::from_millis(100))
        );
    }

    // -- `TimerSpec::tag`: a caller-defined label that must survive every
    // delivery path (plain on-grid ticks, `Burst` catch-up, `Skip`
    // catch-up, and the far-future resync path) unchanged. --

    #[test]
    fn default_tag_is_zero_and_with_tag_overrides_it() {
        let interval = TimerInterval::from_millis(10).unwrap();
        assert_eq!(TimerSpec::new(interval, MissedTickPolicy::Burst).tag, 0);
        assert_eq!(
            TimerSpec::new(interval, MissedTickPolicy::Burst)
                .with_tag(99)
                .tag,
            99
        );
    }

    #[test]
    fn two_timers_report_their_own_distinct_tags_on_ordinary_ticks() {
        let epoch = Instant::now();
        let mut wheel = TimerWheel::new(epoch);
        let a = wheel.insert(
            TimerSpec::new(
                TimerInterval::from_millis(10).unwrap().rebase(epoch),
                MissedTickPolicy::Burst,
            )
            .with_tag(11),
            epoch,
        );
        let b = wheel.insert(
            TimerSpec::new(
                TimerInterval::from_millis(20).unwrap().rebase(epoch),
                MissedTickPolicy::Burst,
            )
            .with_tag(22),
            epoch,
        );

        // `a`'s 10ms period fires twice by 20ms (at 10ms and 20ms, `Burst`
        // delivering both individually); `b`'s 20ms period fires once.
        let fired = wheel.advance(epoch + Duration::from_millis(20));
        assert_eq!(
            fired.len(),
            3,
            "a's 2 ticks (10ms, 20ms) plus b's 1st (20ms)"
        );
        for event in &fired {
            let expected_tag = if event.id == a {
                11
            } else if event.id == b {
                22
            } else {
                panic!("unexpected timer id {:?}", event.id)
            };
            assert_eq!(event.tag, expected_tag);
        }
    }

    #[test]
    fn tag_survives_burst_catch_up_on_every_coalesced_and_individual_event() {
        let epoch = Instant::now();
        let interval = TimerInterval::from_millis(10).unwrap().rebase(epoch);
        let mut wheel = TimerWheel::with_burst_cap(epoch, 2);
        let id = wheel.insert(
            TimerSpec::new(interval, MissedTickPolicy::Burst).with_tag(7),
            epoch,
        );

        // 5 periods due at once; cap is 2: 2 individual + 1 coalesced final.
        let fired = wheel.advance(epoch + Duration::from_millis(50));
        assert_eq!(fired.len(), 3);
        assert!(
            fired.iter().all(|f| f.id == id && f.tag == 7),
            "the tag must be identical on the individual and the coalesced events alike"
        );
    }

    #[test]
    fn tag_survives_skip_catch_up() {
        let epoch = Instant::now();
        let interval = TimerInterval::from_millis(10).unwrap().rebase(epoch);
        let mut wheel = TimerWheel::new(epoch);
        let id = wheel.insert(
            TimerSpec::new(interval, MissedTickPolicy::Skip).with_tag(123),
            epoch,
        );

        let fired = wheel.advance(epoch + Duration::from_millis(55));
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].id, id);
        assert_eq!(fired[0].tag, 123);
    }

    #[test]
    fn tag_survives_the_far_future_resync_path() {
        let epoch = Instant::now();
        let interval = TimerInterval::from_secs(1).unwrap().rebase(epoch);
        let mut wheel = TimerWheel::new(epoch);
        let id = wheel.insert(
            TimerSpec::new(interval, MissedTickPolicy::Skip).with_tag(u64::MAX),
            epoch,
        );

        // Comfortably beyond `MAX_CATCHUP_TICKS_MS`, forcing the resync path.
        let fired = wheel.advance(epoch + Duration::from_secs(2 * 60 * 60));
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].id, id);
        assert_eq!(
            fired[0].tag,
            u64::MAX,
            "a caller's tag is opaque, including the all-bits-set case"
        );
    }
}
