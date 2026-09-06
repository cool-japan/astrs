//! Deterministic simulation: safety under faults, liveness after healing.
//!
//! Every test here runs the same [`astrs_raft::RaftNode`] a production replica
//! runs, against a virtual clock and a seeded, adversarial network. A failure
//! is reproducible by its seed, and the invariant checker names the property
//! that broke rather than leaving a downstream symptom to diagnose.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_raft::sim::{FaultSchedule, SimCluster};
use astrs_raft::{LogStore, MembershipChange, PeerId};

/// A spread of seeds, so a single lucky schedule cannot make a test pass.
const SEEDS: [u64; 8] = [1, 7, 42, 1_337, 9_001, 65_537, 123_456, 999_983];

#[test]
fn election_safety_holds_on_a_chaotic_network_for_every_seed() {
    // Drops, duplicates and delays for two thousand ticks. `run` checks all
    // four safety properties after every single tick, so a violation stops the
    // run at the tick it happened rather than at the end.
    for seed in SEEDS {
        let mut cluster = SimCluster::new(5, FaultSchedule::chaotic(), seed);
        cluster
            .run(2_000)
            .unwrap_or_else(|failure| panic!("seed {seed}: {failure}"));
        assert!(
            cluster.network().dropped() > 0,
            "seed {seed} never actually dropped anything, so it proved nothing"
        );
    }
}

#[test]
fn a_chaotic_network_still_elects_a_leader() {
    // Liveness, not safety: Raft only promises progress when the network
    // eventually delivers, and a 15%-loss network does.
    for seed in SEEDS {
        let mut cluster = SimCluster::new(3, FaultSchedule::chaotic(), seed);
        let leader = cluster
            .run_until_leader(4_000)
            .unwrap_or_else(|failure| panic!("seed {seed}: {failure}"));
        assert!(leader.is_some(), "seed {seed} never elected anyone");
    }
}

#[test]
fn committed_commands_reach_every_replica_despite_faults() {
    for seed in SEEDS {
        let mut cluster = SimCluster::new(3, FaultSchedule::chaotic(), seed);
        assert!(cluster.run_until_leader(4_000).unwrap().is_some());

        for index in 0..8u64 {
            // The leader can change mid-run on a lossy network; a failed
            // proposal simply means "no leader this instant", so wait and
            // retry rather than treating it as a defect.
            for _ in 0..64 {
                if cluster.propose(index.to_le_bytes().to_vec()).is_ok() {
                    break;
                }
                cluster.run(20).unwrap();
            }
            cluster.run(40).unwrap();
        }
        cluster.run(1_500).unwrap();

        assert!(
            cluster.committed_everywhere(&0u64.to_le_bytes()),
            "seed {seed}: the first command never reached every replica"
        );
    }
}

#[test]
fn a_partitioned_minority_never_elects_and_the_majority_keeps_working() {
    let mut cluster = SimCluster::new(5, FaultSchedule::perfect(), 31);
    assert!(cluster.run_until_leader(1_000).unwrap().is_some());

    let minority = vec![PeerId::new(1), PeerId::new(2)];
    let majority = vec![PeerId::new(3), PeerId::new(4), PeerId::new(5)];
    cluster
        .network_mut()
        .partition([minority.clone(), majority.clone()]);
    cluster.run(1_000).unwrap();

    for leader in cluster.leaders() {
        assert!(
            majority.contains(&leader),
            "{leader} led from the minority side of a partition"
        );
    }
    // And the majority side is still able to commit.
    for _ in 0..64 {
        if cluster.propose(b"majority write".to_vec()).is_ok() {
            break;
        }
        cluster.run(20).unwrap();
    }
    cluster.run(300).unwrap();
    for peer in majority {
        let node = cluster.node(peer).expect("a running peer");
        assert!(
            node.state_machine()
                .commands()
                .iter()
                .any(|command| command == b"majority write"),
            "{peer} did not apply the majority's write"
        );
    }
}

#[test]
fn a_healing_partition_restores_one_leader_and_one_history() {
    // The liveness claim that matters operationally: after a split heals, the
    // cluster reconverges on a single leader and a single log — with no safety
    // property violated at any tick in between.
    for seed in [3u64, 11, 29, 71] {
        let mut cluster = SimCluster::new(5, FaultSchedule::perfect(), seed);
        assert!(cluster.run_until_leader(1_000).unwrap().is_some());
        for _ in 0..32 {
            if cluster.propose(b"before the split".to_vec()).is_ok() {
                break;
            }
            cluster.run(20).unwrap();
        }
        cluster.run(200).unwrap();

        cluster.network_mut().partition([
            vec![PeerId::new(1), PeerId::new(2)],
            vec![PeerId::new(3), PeerId::new(4), PeerId::new(5)],
        ]);
        cluster.run(1_500).unwrap();
        for _ in 0..32 {
            if cluster.propose(b"during the split".to_vec()).is_ok() {
                break;
            }
            cluster.run(20).unwrap();
        }
        cluster.run(500).unwrap();

        cluster.network_mut().heal();
        cluster.run(3_000).unwrap();

        let leaders = cluster.leaders();
        assert_eq!(
            leaders.len(),
            1,
            "seed {seed}: a healed cluster must settle on exactly one leader, saw {leaders:?}"
        );
        assert!(
            cluster.committed_everywhere(b"before the split"),
            "seed {seed}: a pre-split commit was lost"
        );
        assert!(
            cluster.committed_everywhere(b"during the split"),
            "seed {seed}: the majority's write did not reach the reunited minority"
        );
    }
}

#[test]
fn killing_the_leader_costs_one_election_and_no_committed_data() {
    for seed in [5u64, 17, 53, 97] {
        let mut cluster = SimCluster::new(5, FaultSchedule::perfect(), seed);
        let first = cluster.run_until_leader(1_000).unwrap().expect("a leader");
        for _ in 0..32 {
            if cluster.propose(b"survives the failover".to_vec()).is_ok() {
                break;
            }
            cluster.run(20).unwrap();
        }
        cluster.run(300).unwrap();

        cluster.crash(first);
        let second = cluster
            .run_until_leader(2_000)
            .unwrap()
            .expect("a replacement leader");
        assert_ne!(second, first);
        cluster.run(500).unwrap();

        let node = cluster.node(second).expect("the new leader");
        assert!(
            node.state_machine()
                .commands()
                .iter()
                .any(|command| command == b"survives the failover"),
            "seed {seed}: the new leader is missing a committed entry"
        );
    }
}

#[test]
fn a_restarted_replica_catches_up_without_double_applying() {
    // The state-machine-safety check would fire if the restarted peer replayed
    // entries it had already applied — see `StateMachine::applied_index`.
    for seed in [2u64, 13, 61] {
        let mut cluster = SimCluster::new(3, FaultSchedule::perfect(), seed);
        let leader = cluster.run_until_leader(1_000).unwrap().expect("a leader");
        let follower = (1..=3u64)
            .map(PeerId::new)
            .find(|peer| *peer != leader)
            .expect("a follower");

        for index in 0..4u64 {
            cluster.propose(index.to_le_bytes().to_vec()).unwrap();
            cluster.run(40).unwrap();
        }
        cluster.crash(follower);
        for index in 4..8u64 {
            cluster.propose(index.to_le_bytes().to_vec()).unwrap();
            cluster.run(40).unwrap();
        }
        cluster.restart(follower).unwrap();
        cluster.run(1_000).unwrap();

        let node = cluster.node(follower).expect("the restarted follower");
        let commands = node.state_machine().commands();
        assert_eq!(
            commands.len(),
            8,
            "seed {seed}: expected exactly eight commands, saw {}",
            commands.len()
        );
        for index in 0..8u64 {
            assert_eq!(commands[index as usize], index.to_le_bytes().to_vec());
        }
    }
}

#[test]
fn membership_can_shrink_and_the_smaller_cluster_still_commits() {
    let mut cluster = SimCluster::new(5, FaultSchedule::perfect(), 77);
    let leader = cluster.run_until_leader(1_000).unwrap().expect("a leader");
    let victim = (1..=5u64)
        .map(PeerId::new)
        .find(|peer| *peer != leader)
        .expect("a follower to remove");

    cluster
        .propose_membership(MembershipChange::Remove(victim))
        .unwrap();
    cluster.run(500).unwrap();
    assert_eq!(cluster.node(leader).unwrap().membership().len(), 4);

    // Crash the removed peer: a four-voter cluster still has its quorum.
    cluster.crash(victim);
    for _ in 0..64 {
        if cluster.propose(b"after the shrink".to_vec()).is_ok() {
            break;
        }
        cluster.run(20).unwrap();
    }
    cluster.run(500).unwrap();
    assert!(cluster.committed_everywhere(b"after the shrink"));
}

#[test]
fn a_second_membership_change_is_refused_while_one_is_in_flight() {
    // Two overlapping single-server changes can produce configurations whose
    // majorities do not overlap. The refusal is the whole safety argument.
    let mut cluster = SimCluster::new(3, FaultSchedule::lossy(100), 23);
    // Elect first on a healthy network, then cut every link so nothing
    // commits.
    cluster.network_mut().set_schedule(FaultSchedule::perfect());
    let leader = cluster.run_until_leader(1_000).unwrap().expect("a leader");
    cluster
        .network_mut()
        .set_schedule(FaultSchedule::lossy(100));

    cluster
        .propose_membership(MembershipChange::Add(PeerId::new(9)))
        .expect("the first change is accepted");
    let second = cluster.propose_membership(MembershipChange::Add(PeerId::new(10)));
    assert!(
        second.is_err(),
        "a second change must be refused while the first is uncommitted"
    );
    assert_eq!(cluster.node(leader).unwrap().membership().len(), 4);
}

#[test]
fn a_follower_behind_the_leaders_compaction_is_caught_up_by_a_snapshot() {
    // The path that separates a real snapshot implementation from one that
    // merely serializes: a follower is stranded long enough that the leader
    // *discards* the entries it still needs, so no amount of `AppendEntries`
    // back-off can reach it and `InstallSnapshot` has to carry it instead.
    // Unit tests cover each half of that exchange; only a whole cluster
    // running against a network proves the two halves meet.
    for seed in [4u64, 19, 83] {
        // The default threshold is 4096 applied entries — a simulation never
        // reaches it by accident, which is exactly why compaction needs its
        // own configured run rather than a longer one.
        let mut cluster = SimCluster::with_config(3, FaultSchedule::perfect(), seed, |config| {
            config.with_snapshot_threshold(4)
        });
        let leader = cluster.run_until_leader(1_000).unwrap().expect("a leader");
        let stranded = (1..=3u64)
            .map(PeerId::new)
            .find(|peer| *peer != leader)
            .expect("a follower to strand");
        let survivor = (1..=3u64)
            .map(PeerId::new)
            .find(|peer| *peer != leader && *peer != stranded)
            .expect("the other follower");

        // Cut one follower off. Leader plus the other follower is still a
        // majority of three, so the cluster keeps committing without it.
        cluster
            .network_mut()
            .partition([vec![stranded], vec![leader, survivor]]);
        cluster.run(200).unwrap();
        let stranded_through = cluster
            .node(stranded)
            .expect("the stranded peer is crashed, not removed")
            .log()
            .last_index();

        for index in 0..24u64 {
            for _ in 0..64 {
                if cluster.propose(index.to_le_bytes().to_vec()).is_ok() {
                    break;
                }
                cluster.run(20).unwrap();
            }
            cluster.run(30).unwrap();
        }

        // Without this the heal below would be an ordinary `AppendEntries`
        // catch-up wearing a snapshot test's name.
        let first_held = cluster
            .node(leader)
            .expect("the leader")
            .log()
            .first_index();
        assert!(
            first_held > stranded_through.next(),
            "seed {seed}: the leader still holds from {first_held}, so nothing was compacted \
             past the stranded follower at {stranded_through}"
        );

        let before = cluster.network().delivered_of("InstallSnapshot");
        cluster.network_mut().heal();
        cluster.run(2_000).unwrap();

        assert!(
            cluster.network().delivered_of("InstallSnapshot") > before,
            "seed {seed}: the follower rejoined without a single InstallSnapshot, so the \
             compaction path was never exercised ({} AppendEntries delivered)",
            cluster.network().delivered_of("AppendEntries"),
        );

        // And it is genuinely caught up, not merely reachable: every command,
        // in order, including the ones committed while it was cut off.
        let commands = cluster
            .node(stranded)
            .expect("the rejoined follower")
            .state_machine()
            .commands()
            .to_vec();
        assert_eq!(
            commands.len(),
            24,
            "seed {seed}: expected 24 commands after the snapshot, saw {}",
            commands.len()
        );
        for index in 0..24u64 {
            assert_eq!(
                commands[index as usize],
                index.to_le_bytes().to_vec(),
                "seed {seed}: command {index} is wrong after the snapshot restore"
            );
        }
        assert_eq!(cluster.leaders(), vec![leader]);
    }
}

#[test]
fn the_same_seed_replays_exactly() {
    // The property the whole harness rests on: a failure found once can be
    // reproduced. Two runs of the same seed must agree on every observable.
    let observe = || {
        let mut cluster = SimCluster::new(5, FaultSchedule::chaotic(), 424_242);
        cluster.run(1_500).unwrap();
        (
            cluster.leaders(),
            cluster.network().dropped(),
            cluster.network().duplicated(),
            cluster.network().delivered(),
            cluster
                .node(PeerId::new(1))
                .map(|node| (node.term(), node.commit_index())),
        )
    };
    assert_eq!(observe(), observe());
}
