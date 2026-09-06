//! [`RestartHistory`] — the sliding restart-budget window (§12).
//!
//! > *Restart policies per node: supervised respawn with exponential backoff
//! > (`restart_delay`×2^n capped by `max_restart_delay`), budget
//! > `max_restarts` within `restart_window`.*
//!
//! Two counters, and the difference between them is the whole point:
//!
//! - **total** restarts — monotone, reported as
//!   [`astrs_wire::NodeInfo::restart_count`] and used as the backoff exponent,
//!   so a node that keeps failing waits progressively longer;
//! - **in-window** restarts — the ones inside the trailing `restart_window`,
//!   compared against `max_restarts` to decide whether the budget is spent.
//!
//! A node that restarts once an hour for a week has a large total and an
//! in-window count of one: it is a flaky node, not a failed one, and it should
//! keep being restarted. A node that restarts five times in ten seconds has
//! exhausted its budget even though its total is small. Keeping both is what
//! lets [`super::policy`] tell those apart.
//!
//! The window is a plain timestamp deque rather than a rate estimator so the
//! decision is exactly reproducible from the recorded event stream (§14) — no
//! smoothing constants, no accumulated float error.
//!
//! # Examples
//!
//! ```
//! use std::time::{Duration, Instant};
//! use astrs_daemon::supervise::RestartHistory;
//!
//! let start = Instant::now();
//! let mut history = RestartHistory::new(Duration::from_secs(10));
//!
//! history.record(start);
//! history.record(start + Duration::from_secs(1));
//! assert_eq!(history.in_window(start + Duration::from_secs(2)), 2);
//! assert_eq!(history.total(), 2);
//!
//! // Eleven seconds later both have aged out of the window, but not the total.
//! assert_eq!(history.in_window(start + Duration::from_secs(12)), 0);
//! assert_eq!(history.total(), 2);
//! ```

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The trailing window of restart timestamps for one node.
#[derive(Debug, Clone)]
pub struct RestartHistory {
    /// How far back the budget window reaches.
    window: Duration,
    /// Restart instants inside the window, oldest first.
    recent: VecDeque<Instant>,
    /// Every restart ever, including the ones that aged out.
    total: u32,
    /// When the most recent restart happened.
    last: Option<Instant>,
}

impl RestartHistory {
    /// An empty history over `window`.
    #[must_use]
    pub const fn new(window: Duration) -> Self {
        Self {
            window,
            recent: VecDeque::new(),
            total: 0,
            last: None,
        }
    }

    /// The window this history counts over.
    #[must_use]
    pub const fn window(&self) -> Duration {
        self.window
    }

    /// Changes the window, discarding nothing — the next
    /// [`RestartHistory::in_window`] applies the new span.
    pub const fn set_window(&mut self, window: Duration) {
        self.window = window;
    }

    /// Records a restart at `now`.
    pub fn record(&mut self, now: Instant) {
        self.recent.push_back(now);
        self.total = self.total.saturating_add(1);
        self.last = Some(now);
        self.prune(now);
    }

    /// How many restarts happened in the window ending at `now`.
    pub fn in_window(&mut self, now: Instant) -> u32 {
        self.prune(now);
        u32::try_from(self.recent.len()).unwrap_or(u32::MAX)
    }

    /// How many restarts happened in the window, without pruning.
    ///
    /// The read-only form, for a diagnostic that must not mutate.
    #[must_use]
    pub fn in_window_at(&self, now: Instant) -> u32 {
        let count = self
            .recent
            .iter()
            .filter(|instant| now.duration_since(**instant) < self.window)
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    /// Every restart ever recorded.
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.total
    }

    /// When the most recent restart happened.
    #[must_use]
    pub const fn last(&self) -> Option<Instant> {
        self.last
    }

    /// How long ago the most recent restart was, at `now`.
    #[must_use]
    pub fn since_last(&self, now: Instant) -> Option<Duration> {
        self.last.map(|last| now.saturating_duration_since(last))
    }

    /// Whether anything has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Forgets the window but keeps the total.
    ///
    /// What a successful, long-lived incarnation earns: a node that ran for an
    /// hour before failing should not inherit the budget it burned during a
    /// bad minute yesterday.
    pub fn clear_window(&mut self) {
        self.recent.clear();
    }

    /// Forgets everything, including the total — a fresh start after an
    /// operator-initiated restart.
    pub fn reset(&mut self) {
        self.recent.clear();
        self.total = 0;
        self.last = None;
    }

    /// Drops timestamps that have aged out of the window.
    fn prune(&mut self, now: Instant) {
        while let Some(oldest) = self.recent.front() {
            if now.duration_since(*oldest) >= self.window {
                self.recent.pop_front();
            } else {
                break;
            }
        }
    }
}

impl Default for RestartHistory {
    fn default() -> Self {
        Self::new(Duration::from_secs(60))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn history() -> (Instant, RestartHistory) {
        (Instant::now(), RestartHistory::new(Duration::from_secs(10)))
    }

    #[test]
    fn an_empty_history_counts_nothing() {
        let (start, mut history) = history();
        assert!(history.is_empty());
        assert_eq!(history.total(), 0);
        assert_eq!(history.in_window(start), 0);
        assert!(history.last().is_none());
        assert!(history.since_last(start).is_none());
    }

    #[test]
    fn restarts_inside_the_window_are_counted() {
        let (start, mut history) = history();
        for offset in 0..5 {
            history.record(start + Duration::from_secs(offset));
        }
        assert_eq!(history.in_window(start + Duration::from_secs(5)), 5);
        assert_eq!(history.total(), 5);
    }

    #[test]
    fn restarts_age_out_of_the_window_but_not_the_total() {
        let (start, mut history) = history();
        history.record(start);
        history.record(start + Duration::from_secs(1));

        assert_eq!(history.in_window(start + Duration::from_secs(9)), 2);
        // The first ages out exactly at start + 10 s.
        assert_eq!(history.in_window(start + Duration::from_secs(10)), 1);
        assert_eq!(history.in_window(start + Duration::from_secs(11)), 0);
        assert_eq!(history.total(), 2);
    }

    #[test]
    fn the_read_only_count_agrees_with_the_pruning_one() {
        let (start, mut history) = history();
        history.record(start);
        history.record(start + Duration::from_secs(5));
        let now = start + Duration::from_secs(11);
        assert_eq!(history.in_window_at(now), 1);
        assert_eq!(history.in_window(now), 1);
    }

    #[test]
    fn the_last_restart_is_remembered() {
        let (start, mut history) = history();
        history.record(start);
        history.record(start + Duration::from_secs(3));
        assert_eq!(history.last(), Some(start + Duration::from_secs(3)));
        assert_eq!(
            history.since_last(start + Duration::from_secs(4)),
            Some(Duration::from_secs(1))
        );
    }

    #[test]
    fn a_reading_before_the_last_restart_saturates_to_zero() {
        let (start, mut history) = history();
        history.record(start + Duration::from_secs(5));
        assert_eq!(history.since_last(start), Some(Duration::ZERO));
    }

    #[test]
    fn clearing_the_window_keeps_the_total() {
        let (start, mut history) = history();
        history.record(start);
        history.record(start);
        history.clear_window();
        assert_eq!(history.in_window(start), 0);
        assert_eq!(history.total(), 2, "the backoff exponent is not forgiven");
    }

    #[test]
    fn a_reset_forgets_everything() {
        let (start, mut history) = history();
        history.record(start);
        history.reset();
        assert!(history.is_empty());
        assert_eq!(history.total(), 0);
        assert_eq!(history.in_window(start), 0);
        assert!(history.last().is_none());
    }

    #[test]
    fn the_window_can_be_changed_after_the_fact() {
        let (start, mut history) = history();
        history.record(start);
        assert_eq!(history.in_window(start + Duration::from_secs(11)), 0);

        let mut wide = RestartHistory::new(Duration::from_secs(10));
        wide.record(start);
        wide.set_window(Duration::from_secs(60));
        assert_eq!(wide.window(), Duration::from_secs(60));
        assert_eq!(wide.in_window(start + Duration::from_secs(11)), 1);
    }

    #[test]
    fn a_flaky_node_never_exhausts_a_short_window() {
        let (start, mut history) = history();
        for hour in 0..24u64 {
            let now = start + Duration::from_secs(hour * 3_600);
            history.record(now);
            assert_eq!(history.in_window(now), 1, "hour {hour}");
        }
        assert_eq!(history.total(), 24);
    }

    #[test]
    fn the_default_window_is_a_minute() {
        assert_eq!(RestartHistory::default().window(), Duration::from_secs(60));
    }
}
