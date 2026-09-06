//! How a consumer attaches: where to start, and whether to take a doorbell.
//!
//! Portable, like [`crate::stats`]: a manifest-driven daemon builds
//! [`AttachOptions`] from `queue_size` / `queue_policy` fields (§8.3, §11.2)
//! long before it knows whether the route will land on the shared-memory
//! plane, so the type must exist wherever the daemon compiles.
//!
//! # Examples
//!
//! ```
//! use astrs_shm::{AttachOptions, StartPosition};
//!
//! // A latest-only sensor topic (`queue_size: 1` in the manifest) that
//! // polls rather than blocks.
//! let options = AttachOptions::new()
//!     .with_start(StartPosition::Latest)
//!     .with_doorbell(false);
//! assert_eq!(options.start(), StartPosition::Latest);
//! assert!(!options.doorbell());
//! ```

use crate::key::SegmentKey;

/// Where a freshly attached consumer starts reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum StartPosition {
    /// The oldest message still resident in the ring.
    ///
    /// The default: a consumer that attaches to a running producer gets the
    /// backlog the ring still holds rather than only future messages. This is
    /// what makes a restarted node see the frames that arrived while it was
    /// coming up, instead of silently skipping them.
    #[default]
    Oldest,
    /// Only messages committed after the attach.
    ///
    /// The right choice for latest-only topics, where a stale backlog is
    /// worse than no backlog — a pose estimate from four frames ago is not
    /// something a controller wants to act on.
    Latest,
    /// A specific sequence number.
    ///
    /// Clamped up to the oldest resident message if the requested sequence
    /// has already been reclaimed, and down to `write_seq + 1` if it is in
    /// the future. Used by replay (§14) to resume a recorded stream at a
    /// known point.
    Sequence(u64),
}

impl StartPosition {
    /// The name used in logs and manifests.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::StartPosition;
    ///
    /// assert_eq!(StartPosition::Oldest.as_str(), "oldest");
    /// assert_eq!(StartPosition::Sequence(9).as_str(), "sequence");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Oldest => "oldest",
            Self::Latest => "latest",
            Self::Sequence(_) => "sequence",
        }
    }

    /// Resolve to a concrete cursor given the ring's current watermarks.
    ///
    /// `reclaimed_seq` is the highest sequence that has left the ring and
    /// `write_seq` the highest published; the result is always inside
    /// `oldest..=write_seq + 1`, so a consumer can never start on a slot that
    /// no longer holds what it asked for.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::StartPosition;
    ///
    /// // Sequences 5..=10 are resident.
    /// assert_eq!(StartPosition::Oldest.resolve(4, 10), 5);
    /// assert_eq!(StartPosition::Latest.resolve(4, 10), 11);
    /// assert_eq!(StartPosition::Sequence(7).resolve(4, 10), 7);
    /// assert_eq!(StartPosition::Sequence(1).resolve(4, 10), 5, "clamped up");
    /// assert_eq!(StartPosition::Sequence(99).resolve(4, 10), 11, "clamped down");
    /// ```
    #[must_use]
    pub const fn resolve(self, reclaimed_seq: u64, write_seq: u64) -> u64 {
        let oldest = reclaimed_seq + 1;
        let newest = write_seq + 1;
        let requested = match self {
            Self::Oldest => oldest,
            Self::Latest => newest,
            Self::Sequence(seq) => {
                if seq < 1 {
                    1
                } else {
                    seq
                }
            }
        };
        // `oldest` can exceed `newest` only if the watermarks are transiently
        // inconsistent, in which case starting at `oldest` is the safe side —
        // the consumer simply waits.
        let lower = if requested < oldest {
            oldest
        } else {
            requested
        };
        if lower > newest && newest >= oldest {
            newest
        } else {
            lower
        }
    }
}

/// How a consumer attaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachOptions {
    start: StartPosition,
    doorbell: bool,
    expect: Option<SegmentKey>,
}

impl AttachOptions {
    /// Defaults: start at the oldest resident message, with a doorbell.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            start: StartPosition::Oldest,
            doorbell: true,
            expect: None,
        }
    }

    /// Choose the start position.
    #[must_use]
    pub const fn with_start(mut self, start: StartPosition) -> Self {
        self.start = start;
        self
    }

    /// Whether to create a doorbell for blocking receives.
    ///
    /// A consumer that only ever polls — a scheduler draining several rings
    /// in one pass (§11.2) — can skip the descriptor entirely and save a file
    /// descriptor per route.
    #[must_use]
    pub const fn with_doorbell(mut self, doorbell: bool) -> Self {
        self.doorbell = doorbell;
        self
    }

    /// Verify the segment's identity on attach.
    ///
    /// Redundant when the segment was itself opened with an expected key, but
    /// the daemon hands out already-mapped segments and a node still wants
    /// its own check — §6.2 says *every* attach path verifies the generation,
    /// and "someone upstream probably did" is not that.
    #[must_use]
    pub fn expecting(mut self, key: SegmentKey) -> Self {
        self.expect = Some(key);
        self
    }

    /// The configured start position.
    #[must_use]
    pub const fn start(&self) -> StartPosition {
        self.start
    }

    /// Whether a doorbell will be created.
    #[must_use]
    pub const fn doorbell(&self) -> bool {
        self.doorbell
    }

    /// The identity the attach will verify, if any.
    #[must_use]
    pub const fn expected(&self) -> Option<&SegmentKey> {
        self.expect.as_ref()
    }
}

/// `#[derive(Default)]` would give `doorbell: false`, which is the wrong
/// default for the common case — a consumer that cannot be woken — so
/// [`Default`] is implemented by hand in terms of [`AttachOptions::new`].
impl Default for AttachOptions {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::DataflowId;

    #[test]
    fn defaults_take_the_backlog_and_a_doorbell() {
        let options = AttachOptions::default();
        assert_eq!(options.start(), StartPosition::Oldest);
        assert!(options.doorbell());
        assert!(options.expected().is_none());
    }

    #[test]
    fn builders_compose_and_are_inspectable() {
        let key = SegmentKey::from_parts(DataflowId::from_u128(3), "n", "o", 2).unwrap();
        let options = AttachOptions::new()
            .with_start(StartPosition::Sequence(12))
            .with_doorbell(false)
            .expecting(key.clone());
        assert_eq!(options.start(), StartPosition::Sequence(12));
        assert!(!options.doorbell());
        assert_eq!(options.expected(), Some(&key));
    }

    #[test]
    fn resolution_clamps_into_the_resident_window() {
        // Nothing published yet: everything resolves to sequence 1.
        assert_eq!(StartPosition::Oldest.resolve(0, 0), 1);
        assert_eq!(StartPosition::Latest.resolve(0, 0), 1);
        assert_eq!(StartPosition::Sequence(0).resolve(0, 0), 1);
        assert_eq!(StartPosition::Sequence(50).resolve(0, 0), 1);

        // Sequences 5..=10 resident.
        assert_eq!(StartPosition::Oldest.resolve(4, 10), 5);
        assert_eq!(StartPosition::Latest.resolve(4, 10), 11);
        assert_eq!(StartPosition::Sequence(4).resolve(4, 10), 5);
        assert_eq!(StartPosition::Sequence(5).resolve(4, 10), 5);
        assert_eq!(StartPosition::Sequence(10).resolve(4, 10), 10);
        assert_eq!(StartPosition::Sequence(11).resolve(4, 10), 11);
        assert_eq!(StartPosition::Sequence(12).resolve(4, 10), 11);
    }

    #[test]
    fn resolution_never_leaves_the_window_for_any_input() {
        for reclaimed in 0..8u64 {
            for write in reclaimed..reclaimed + 8 {
                for requested in 0..24u64 {
                    let resolved = StartPosition::Sequence(requested).resolve(reclaimed, write);
                    assert!(resolved > reclaimed, "{requested} → {resolved}");
                    assert!(resolved <= write + 1, "{requested} → {resolved}");
                }
                assert_eq!(
                    StartPosition::Oldest.resolve(reclaimed, write),
                    reclaimed + 1
                );
                assert_eq!(StartPosition::Latest.resolve(reclaimed, write), write + 1);
            }
        }
    }

    #[test]
    fn start_positions_name_themselves() {
        assert_eq!(StartPosition::Oldest.as_str(), "oldest");
        assert_eq!(StartPosition::Latest.as_str(), "latest");
        assert_eq!(StartPosition::Sequence(1).as_str(), "sequence");
        assert_eq!(StartPosition::default(), StartPosition::Oldest);
    }
}
