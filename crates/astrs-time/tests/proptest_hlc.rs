//! Property tests: [`astrs_time::HlcClock`] monotonicity under arbitrary
//! interleavings of `now`/`update_with`, including wall-clock rewinds and
//! drift-rejected merges.
//!
//! This file is a separate crate (as every file under `tests/` is), so the
//! workspace's denied clippy lints (`unwrap_used`, `expect_used`, `panic`,
//! ...) apply to it independently of the `#[cfg(test)]` module allows in
//! `src/`; the `proptest!` macro's own expansion and this file's setup code
//! both need the allow below.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use astrs_time::{HlcClock, HlcTimestamp, ManualClock};
use proptest::prelude::*;

#[derive(Debug, Clone)]
enum Op {
    Now,
    Advance(u64),
    RewindWall(u64),
    UpdateWith(u64, u32),
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => Just(Op::Now),
        2 => (0u64..1_000_000_000).prop_map(Op::Advance),
        1 => (0u64..1_000_000_000).prop_map(Op::RewindWall),
        3 => (0u64..10_000_000_000_000, any::<u32>()).prop_map(|(p, l)| Op::UpdateWith(p, l)),
    ]
}

proptest! {
    /// Drives an `HlcClock<ManualClock>` through an arbitrary script of
    /// `now()` calls, wall-clock advances/rewinds, and `update_with()`
    /// calls (with remote timestamps that may or may not fall within the
    /// configured drift bound), and checks that every timestamp actually
    /// issued (by `now` or an *accepted* `update_with`) is strictly greater
    /// than the previous one issued — and that a *rejected* `update_with`
    /// leaves the clock's state completely unchanged, so the sequence stays
    /// monotone (and unperturbed) across rejections too.
    #[test]
    fn hlc_now_and_update_with_are_monotone_under_arbitrary_interleavings(
        ops in proptest::collection::vec(op_strategy(), 1..200),
        start_wall_ns in 1_000_000_000u64..10_000_000_000_000u64,
        max_drift_ms in 1u64..2_000u64,
    ) {
        let clock = HlcClock::with_max_drift(
            ManualClock::new(start_wall_ns),
            Duration::from_millis(max_drift_ms),
        );
        let mut previous = HlcTimestamp::EPOCH;

        for op in ops {
            match op {
                Op::Now => {
                    let ts = clock.now();
                    prop_assert!(ts > previous, "now() must strictly increase: {ts:?} <= {previous:?}");
                    previous = ts;
                }
                Op::Advance(ns) => {
                    clock.clock().advance(Duration::from_nanos(ns));
                }
                Op::RewindWall(ns) => {
                    clock.clock().rewind_wall(Duration::from_nanos(ns));
                }
                Op::UpdateWith(remote_physical_ns, remote_logical) => {
                    let remote = HlcTimestamp::new(remote_physical_ns, remote_logical);
                    match clock.update_with(remote) {
                        Ok(merged) => {
                            prop_assert!(
                                merged > previous,
                                "accepted update_with must strictly increase: {merged:?} <= {previous:?}"
                            );
                            previous = merged;
                        }
                        Err(_) => {
                            prop_assert_eq!(
                                clock.last(),
                                previous,
                                "a rejected update_with must not change clock state"
                            );
                        }
                    }
                }
            }
        }
    }

    /// A narrower, non-sequential characterization of the same receive
    /// rule: seed the local clock with one real tick, then check that
    /// every *accepted* remote timestamp produces a merge result that
    /// dominates both the local state just before the call and the remote
    /// timestamp itself — the defining algebraic property of
    /// `max(local, remote, wall) + logical bump`.
    #[test]
    fn accepted_merge_dominates_both_local_and_remote(
        start_wall_ns in 1_000_000_000u64..10_000_000_000_000u64,
        remote_physical_ns in 0u64..20_000_000_000_000u64,
        remote_logical in any::<u32>(),
        max_drift_ms in 1u64..5_000u64,
    ) {
        let clock = HlcClock::with_max_drift(
            ManualClock::new(start_wall_ns),
            Duration::from_millis(max_drift_ms),
        );
        let local_before = clock.now();
        let remote = HlcTimestamp::new(remote_physical_ns, remote_logical);

        if let Ok(merged) = clock.update_with(remote) {
            prop_assert!(merged > local_before);
            prop_assert!(merged > remote);
        }
    }

    /// The property the blueprint (§4.3) actually claims for `Stamped<T>`
    /// and the HLC underneath it — "cluster-wide causal ordering" — is
    /// inherently a *multi*-clock property: this drives several
    /// independent `HlcClock<ManualClock>`s (simulating separate
    /// processes) through a random script of "peer X sends its current
    /// timestamp to peer Y", and checks that every send/receive pair
    /// preserves causal order end to end: the receiver's post-merge state
    /// is always strictly after both the message it just received and its
    /// own prior state, and a drift-rejected message leaves the receiver's
    /// state untouched.
    ///
    /// Peer start times are generated within a window strictly smaller
    /// than `max_drift` (rather than independently across the whole `u64`
    /// range): with unrelated start times the drift bound would reject
    /// nearly every cross-clock merge, and the property below would go
    /// unexercised on its accept path.
    #[test]
    fn cross_clock_causal_order_is_preserved_across_a_peer_group(
        base_wall_ns in 1_000_000_000u64..1_000_000_000_000u64,
        max_drift_ms in 50u64..2_000u64,
        offsets in proptest::collection::vec(0u64..100_000_000u64, 4),
        messages in proptest::collection::vec((0usize..4, 0usize..4), 1..150),
    ) {
        let max_drift = Duration::from_millis(max_drift_ms);
        // Half the drift budget, so every peer's start time is well within
        // bounds of every other peer's — the accept path stays live.
        let half_drift_ns = (max_drift.as_nanos() as u64 / 2).max(1);

        let peers: Vec<HlcClock<ManualClock>> = offsets
            .iter()
            .map(|&offset| {
                HlcClock::with_max_drift(ManualClock::new(base_wall_ns + offset % half_drift_ns), max_drift)
            })
            .collect();
        let num_peers = peers.len();
        let mut last_seen = vec![HlcTimestamp::EPOCH; num_peers];

        for (send_idx, recv_idx) in messages {
            let sender = send_idx % num_peers;
            let receiver = recv_idx % num_peers;

            let sent = peers[sender].now();
            prop_assert!(sent > last_seen[sender]);
            last_seen[sender] = sent;

            if sender == receiver {
                continue;
            }

            match peers[receiver].update_with(sent) {
                Ok(merged) => {
                    // The defining cross-clock guarantee: the receiver's
                    // new state is strictly causally after the message it
                    // just received, regardless of the receiver's own
                    // independent clock history.
                    prop_assert!(merged > sent);
                    prop_assert!(merged > last_seen[receiver]);
                    last_seen[receiver] = merged;
                }
                Err(_) => {
                    prop_assert_eq!(peers[receiver].last(), last_seen[receiver]);
                }
            }
        }
    }
}
