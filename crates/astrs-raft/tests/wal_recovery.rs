//! A replica over the real file-backed log: restart, torn tail, compaction.
//!
//! The unit tests in `log::wal` prove the container's own recovery contract.
//! These prove the thing that actually matters: a [`RaftNode`] built over a
//! recovered [`WalLog`] comes back with the same term, the same vote and the
//! same log it had — which is what Raft's safety argument assumes about
//! "stable storage".

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use astrs_raft::{
    FsyncPolicy, LogIndex, LogStore, MemoryStateMachine, PeerId, RaftConfig, RaftNode,
    RecoveryOutcome, Role, WalLog,
};

/// A fresh scratch directory under the platform temp dir.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("astrs-raft-it-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A single-peer configuration, so elections need no network.
fn solo_config() -> RaftConfig {
    RaftConfig::new(PeerId::new(1))
        .with_peer(
            PeerId::new(1),
            std::net::SocketAddr::from(([127, 0, 0, 1], 7501)),
        )
        .with_election_timeout(3, 2)
        .with_heartbeat_ticks(1)
}

/// Runs a solo replica until it leads, then applies `commands`.
fn run_solo(log: WalLog, commands: &[&[u8]]) -> WalLog {
    let mut node = RaftNode::new(solo_config(), log, MemoryStateMachine::new()).unwrap();
    for _ in 0..64 {
        node.tick().unwrap();
        if node.role() == Role::Leader {
            break;
        }
    }
    assert_eq!(node.role(), Role::Leader);
    for command in commands {
        node.propose((*command).to_vec()).unwrap();
    }
    node.into_parts().0
}

#[test]
fn a_replica_recovers_its_term_vote_and_log_from_disk() {
    let dir = scratch("recover");
    let path = dir.join("raft.wal");

    let log = run_solo(
        WalLog::open(&path, FsyncPolicy::Always).unwrap(),
        &[b"alpha", b"beta"],
    );
    let term_before = log.hard_state().term;
    let last_before = log.last_index();
    drop(log);

    let reopened = WalLog::open(&path, FsyncPolicy::Always).unwrap();
    assert_eq!(reopened.outcome(), RecoveryOutcome::Intact);
    assert_eq!(reopened.hard_state().term, term_before);
    assert_eq!(
        reopened.hard_state().voted_for,
        Some(PeerId::new(1)),
        "a vote must survive a restart or a peer can vote twice in one term"
    );
    assert_eq!(reopened.last_index(), last_before);

    let node = RaftNode::new(solo_config(), reopened, MemoryStateMachine::new()).unwrap();
    assert_eq!(node.term(), term_before);
    assert_eq!(node.log().last_index(), last_before);
}

#[test]
fn a_torn_tail_costs_only_the_entry_that_was_being_written() {
    let dir = scratch("torn");
    let path = dir.join("raft.wal");

    let log = run_solo(
        WalLog::open(&path, FsyncPolicy::Always).unwrap(),
        &[b"one", b"two", b"three"],
    );
    let complete = log.last_index();
    drop(log);

    // Chop the last nine bytes: the process died part-way through a record.
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..bytes.len() - 9]).unwrap();

    let repaired = WalLog::open(&path, FsyncPolicy::Always).unwrap();
    assert!(repaired.repaired_tail());
    assert!(repaired.last_index() < complete);
    let recovered = repaired.last_index();

    // A replica built on the repaired file works normally, and the entries it
    // appends afterwards survive the *next* restart — which is the property a
    // scan-only recovery would silently break.
    let log = run_solo(repaired, &[b"after the repair"]);
    drop(log);

    let again = WalLog::open(&path, FsyncPolicy::Always).unwrap();
    assert_eq!(again.outcome(), RecoveryOutcome::Intact);
    assert!(again.last_index() > recovered);
}

#[test]
fn compaction_survives_a_restart_and_keeps_the_state_machine() {
    let dir = scratch("compact");
    let path = dir.join("raft.wal");
    let config = solo_config().with_snapshot_threshold(8);

    let log = {
        let mut node = RaftNode::new(
            config.clone(),
            WalLog::open(&path, FsyncPolicy::Always).unwrap(),
            MemoryStateMachine::new(),
        )
        .unwrap();
        for _ in 0..64 {
            node.tick().unwrap();
            if node.role() == Role::Leader {
                break;
            }
        }
        for index in 0..32u64 {
            node.propose(index.to_le_bytes().to_vec()).unwrap();
        }
        assert!(
            node.log().first_index() > LogIndex::FIRST,
            "the threshold should have compacted the prefix"
        );
        node.into_parts().0
    };
    let boundary = log.snapshot_meta().expect("a snapshot").last_index();
    drop(log);

    let reopened = WalLog::open(&path, FsyncPolicy::Always).unwrap();
    assert_eq!(
        reopened.snapshot_meta().map(|meta| meta.last_index()),
        Some(boundary)
    );
    let node = RaftNode::new(config, reopened, MemoryStateMachine::new()).unwrap();
    assert_eq!(node.last_applied(), boundary);
    assert!(
        !node.state_machine().commands().is_empty(),
        "the state machine must be restored from the snapshot sidecar"
    );
}

#[test]
fn a_never_syncing_log_still_recovers_within_the_process() {
    // `FsyncPolicy::Never` is a deliberate choice, not a broken one: data
    // reaches the page cache, so a *process* restart loses nothing.
    let dir = scratch("nosync");
    let path = dir.join("raft.wal");
    let log = run_solo(
        WalLog::open(&path, FsyncPolicy::Never).unwrap(),
        &[b"cached"],
    );
    let last = log.last_index();
    drop(log);

    let reopened = WalLog::open(&path, FsyncPolicy::Never).unwrap();
    assert_eq!(reopened.last_index(), last);
    assert!(!reopened.policy().is_crash_safe());
}
