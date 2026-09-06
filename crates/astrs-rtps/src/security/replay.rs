//! Anti-replay: a sliding window over one session's counters.
//!
//! # Why not a high-water mark
//!
//! The obvious anti-replay rule — accept a counter only when it is greater
//! than the highest seen — is wrong for RTPS, and wrong in a way that would
//! not show up as a security test failure. RTPS runs on UDP, which reorders,
//! and the reliability protocol *deliberately* sends old sequence numbers
//! again when a reader nacks them. Under a strict high-water mark, every
//! repaired sample and every reordered datagram would be discarded as a
//! replay, and the symptom would be a reliability test that hangs rather than
//! a security test that fails.
//!
//! So this is the IPsec/DTLS window (RFC 6347 §4.1.2.6): a highest-accepted
//! counter plus a bitmap of the [`REPLAY_WINDOW`] counters below it. Three
//! outcomes, and the middle one is the one people leave out:
//!
//! | Counter | Outcome |
//! |---|---|
//! | above `highest` | accepted; the window slides |
//! | inside the window and not yet seen | **accepted**; its bit is set |
//! | inside the window and already seen | rejected as a replay |
//! | below the window, or zero | rejected as too old |
//!
//! # What a window does not do
//!
//! It bounds how far back a replay can reach; it does not make one
//! impossible. A datagram delayed beyond sixty-four counters is
//! indistinguishable from a replay and is treated as one — that is the
//! trade, and it is the same one every AEAD transport makes.

/// How many counters below the highest accepted one are remembered.
///
/// Sixty-four, so the bitmap is one `u64` and the whole window is sixteen
/// octets of state per session. Deeper windows buy tolerance of reordering
/// that a loopback or a switched Ethernet fabric does not produce.
pub const REPLAY_WINDOW: u64 = 64;

/// One session's replay state.
///
/// `Default` is the fresh window: nothing accepted, so the first counter of a
/// session — which is always one, see
/// [`SessionSender::next`](crate::security::SessionSender::next) — is above
/// `highest` and accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReplayWindow {
    highest: u64,
    /// Bit `n` is set when counter `highest - n` has been accepted, so bit
    /// zero is `highest` itself and is set for every window that has accepted
    /// anything. Keeping `highest` *in* the bitmap rather than implicit is
    /// what makes the slide a plain shift: when the window moves forward by
    /// `advance`, every bit moves up by `advance` and the old high-water mark
    /// lands where it belongs with no special case.
    seen: u64,
}

impl ReplayWindow {
    /// A window that has accepted nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            highest: 0,
            seen: 0,
        }
    }

    /// The highest counter accepted so far, or zero.
    #[must_use]
    pub const fn highest(&self) -> u64 {
        self.highest
    }

    /// How far back the window reaches.
    #[must_use]
    pub const fn window(&self) -> u64 {
        REPLAY_WINDOW
    }

    /// True when `counter` would be accepted, without recording it.
    ///
    /// The predicate [`accept`](Self::accept) is built from, exposed so a
    /// caller can check freshness before doing the expensive work — this
    /// crate checks *after* verifying the tag, deliberately, so that an
    /// unauthenticated counter can never advance the window.
    #[must_use]
    pub const fn is_fresh(&self, counter: u64) -> bool {
        if counter == 0 {
            // Counters start at one. Zero is what an empty window holds, and
            // accepting it would let a forged header claim the origin.
            return false;
        }
        if counter > self.highest {
            return true;
        }
        let behind = self.highest - counter;
        if behind >= REPLAY_WINDOW {
            return false;
        }
        self.seen & (1_u64 << behind) == 0
    }

    /// Accept `counter` if it is fresh, recording it.
    ///
    /// Returns whether it was accepted. A rejected counter changes nothing at
    /// all, so a flood of replays cannot walk the window forward.
    pub const fn accept(&mut self, counter: u64) -> bool {
        if !self.is_fresh(counter) {
            return false;
        }
        if counter > self.highest {
            let advance = counter - self.highest;
            let slid = if advance >= REPLAY_WINDOW {
                // The whole window moved past everything remembered.
                0
            } else {
                // Shifting left drops the counters that fell out of the
                // window and carries the old `highest` from bit 0 to bit
                // `advance`, which is where it now sits relative to the new
                // one.
                self.seen << advance
            };
            // Bit 0 is always the current `highest`.
            self.seen = slid | 1;
            self.highest = counter;
        } else {
            let behind = self.highest - counter;
            self.seen |= 1_u64 << behind;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_fresh_window_accepts_the_first_counter_and_refuses_zero() {
        let mut window = ReplayWindow::new();
        assert_eq!(window, ReplayWindow::default());
        assert_eq!(window.highest(), 0);
        assert_eq!(window.window(), REPLAY_WINDOW);

        assert!(!window.is_fresh(0), "counters start at one");
        assert!(!window.accept(0));
        assert!(window.accept(1));
        assert_eq!(window.highest(), 1);
    }

    #[test]
    fn an_exact_replay_is_refused() {
        let mut window = ReplayWindow::new();
        for counter in 1..=10 {
            assert!(window.accept(counter), "{counter} is new");
        }
        for counter in 1..=10 {
            assert!(!window.accept(counter), "{counter} is a replay");
        }
        assert_eq!(window.highest(), 10, "and none of them moved the window");
    }

    #[test]
    fn a_reordered_but_unseen_counter_is_accepted() {
        // The test that is easy to leave out, and whose absence ships an
        // anti-replay filter that silently drops repaired traffic.
        let mut window = ReplayWindow::new();
        assert!(window.accept(10));
        assert!(
            window.accept(9),
            "arriving late is not the same as replayed"
        );
        assert!(window.accept(5));
        assert!(window.accept(1));
        assert_eq!(window.highest(), 10, "none of them advanced the window");

        assert!(!window.accept(9), "…but only once each");
        assert!(!window.accept(5));
        assert!(window.accept(11), "and the window still moves forward");
    }

    #[test]
    fn the_window_has_a_floor() {
        let mut window = ReplayWindow::new();
        assert!(window.accept(1_000));
        assert!(
            window.accept(1_000 - REPLAY_WINDOW + 1),
            "the oldest counter still inside the window"
        );
        assert!(
            !window.accept(1_000 - REPLAY_WINDOW),
            "and the first one outside it is refused"
        );
        assert!(!window.accept(1));
    }

    #[test]
    fn a_large_jump_forgets_everything_behind_it() {
        let mut window = ReplayWindow::new();
        for counter in 1..=64 {
            assert!(window.accept(counter));
        }
        assert!(window.accept(1_000), "a long silence, then traffic again");
        assert_eq!(window.highest(), 1_000);
        assert!(
            !window.accept(64),
            "what fell out of the window stays refused"
        );
        assert!(window.accept(999), "and the new window works normally");
        assert!(!window.accept(999));
    }

    #[test]
    fn sliding_by_one_keeps_the_bit_that_was_the_high_water_mark() {
        let mut window = ReplayWindow::new();
        assert!(window.accept(1));
        assert!(window.accept(2));
        assert!(!window.accept(1), "one is still remembered after the slide");
        assert!(!window.accept(2));
        assert!(window.accept(3));
        assert!(!window.accept(1));
        assert!(!window.accept(2));
    }

    #[test]
    fn every_counter_in_a_shuffled_run_is_accepted_exactly_once() {
        // A whole window's worth, delivered in a pathological order.
        let mut window = ReplayWindow::new();
        let mut order: Vec<u64> = (1..=REPLAY_WINDOW).collect();
        order.reverse();
        let mut accepted = 0_usize;
        for counter in &order {
            if window.accept(*counter) {
                accepted += 1;
            }
        }
        assert_eq!(
            accepted,
            usize::try_from(REPLAY_WINDOW).expect("64 fits"),
            "reverse order must not cost a single counter"
        );
        for counter in &order {
            assert!(!window.accept(*counter), "{counter} the second time");
        }
    }

    #[test]
    fn a_rejected_counter_changes_nothing() {
        let mut window = ReplayWindow::new();
        assert!(window.accept(100));
        let before = window;
        for replay in [0, 1, 100, 100 - REPLAY_WINDOW] {
            assert!(!window.accept(replay));
            assert_eq!(window, before, "replay {replay} must not mutate the window");
        }
    }
}
