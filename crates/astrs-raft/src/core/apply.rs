//! Applying committed entries, taking snapshots, and installing one.
//!
//! # Apply is separate from commit for a reason
//!
//! An entry is *committed* when a majority holds it — a fact about the
//! cluster. It is *applied* when this replica has handed it to its state
//! machine — a fact about this process. Keeping the two indices apart is what
//! lets a replica acknowledge replication promptly and catch its own state
//! machine up afterwards, and it is what makes "read your own write" a
//! precise statement: a caller waits for `last_applied`, not `commit_index`.
//!
//! # Compaction is bounded by the state machine, not the log
//!
//! A snapshot is taken when the applied prefix has grown past
//! [`crate::RaftConfig::snapshot_threshold`] entries. Its contents come from
//! the state machine, so the log's whole purpose — reconstructing that state
//! machine — is preserved while the entries themselves are discarded.
//! Everything a discarded entry still needs to answer for (its term, the
//! configuration it implied) is carried in
//! [`crate::SnapshotMeta`].
//!
//! # A follower that fell behind the leader's compaction
//!
//! Once a leader has discarded the entries a follower needs, no amount of
//! `AppendEntries` back-off will help. `InstallSnapshot` replaces that
//! follower's entire log and state machine in one shot. It is deliberately
//! **not** chunked here: a coordinator's registry snapshot is small enough to
//! fit one frame, and a chunked protocol would add resumption state to every
//! replica for a case this deployment does not have.

use crate::core::{Applied, RaftNode};
use crate::error::{RaftError, Result};
use crate::log::entry::{EntryPayload, Snapshot, SnapshotMeta};
use crate::log::store::LogStore;
use crate::message::{Envelope, RaftMessage};
use crate::state_machine::StateMachine;
use crate::types::{LogIndex, PeerId, Role};

impl<M: StateMachine, L: LogStore> RaftNode<M, L> {
    /// Hands every newly committed entry to the state machine, in order.
    pub(crate) fn apply_committed(&mut self) -> Result<()> {
        while self.last_applied < self.commit_index {
            let index = self.last_applied.next();
            let Some(entry) = self.log.entry(index)? else {
                // The entry is inside a snapshot this replica installed, so
                // its effect is already in the state machine.
                self.last_applied = index;
                continue;
            };
            let result = match entry.payload() {
                EntryPayload::Command(command) => Some(
                    self.machine
                        .apply(index, command)
                        .map_err(|reason| RaftError::Apply { index, reason })?,
                ),
                // The leader's no-op and configuration entries are consensus
                // bookkeeping; the application never sees them.
                EntryPayload::Noop | EntryPayload::Config(_) => None,
            };
            self.last_applied = index;
            self.applied.push(Applied {
                index,
                term: entry.term(),
                result,
            });
        }
        self.maybe_compact()
    }

    /// Takes a snapshot and compacts the log once the applied prefix has
    /// grown past the configured threshold.
    fn maybe_compact(&mut self) -> Result<()> {
        let threshold = self.config.snapshot_threshold;
        if threshold == 0 || self.last_applied.is_empty_sentinel() {
            return Ok(());
        }
        let held = self.log.first_index().distance_to(self.last_applied);
        if held < threshold {
            return Ok(());
        }
        self.take_snapshot()
    }

    /// Captures the state machine and compacts the log up to
    /// [`RaftNode::last_applied`].
    ///
    /// Exposed so a caller can compact on its own schedule — before a
    /// planned restart, say — rather than only when the threshold trips.
    ///
    /// # Errors
    ///
    /// - [`RaftError::Snapshot`] if the state machine cannot serialize
    ///   itself.
    /// - Whatever the log store reports while writing.
    pub fn take_snapshot(&mut self) -> Result<()> {
        let last_index = self.last_applied;
        if last_index.is_empty_sentinel() {
            return Ok(());
        }
        let Some(last_term) = self.log.term_at(last_index)? else {
            return Ok(());
        };
        let data = self
            .machine
            .snapshot()
            .map_err(|reason| RaftError::Snapshot {
                what: "capture",
                reason,
            })?;
        let meta = SnapshotMeta::new(last_index, last_term, self.membership.clone());
        tracing::debug!(
            peer = self.config.id.get(),
            last_index = last_index.get(),
            bytes = data.len(),
            "compacting the log into a snapshot"
        );
        self.log.save_snapshot(&Snapshot::new(meta, data))
    }

    /// Sends `peer` this replica's snapshot, because the entries it needs
    /// are gone.
    pub(crate) fn send_snapshot(&mut self, peer: PeerId) -> Result<()> {
        let Some(snapshot) = self.log.snapshot()? else {
            // No snapshot to send: the follower's `next_index` must simply be
            // walked back into the range this log still holds.
            let first = self.log.first_index();
            if let Some(progress) = self.progress.get_mut(&peer) {
                progress.next_index = first;
            }
            return Ok(());
        };
        let last_index = snapshot.meta.last_index();
        if let Some(progress) = self.progress.get_mut(&peer) {
            progress.snapshot_in_flight = true;
            // Optimistic: if the follower accepts, this is where replication
            // resumes. A rejection walks it back like any other.
            progress.next_index = last_index.next();
        }
        let term = self.hard.term;
        tracing::info!(
            peer = self.config.id.get(),
            follower = peer.get(),
            through = last_index.get(),
            "sending a snapshot to a follower that fell behind compaction"
        );
        self.send(
            peer,
            RaftMessage::InstallSnapshot {
                term,
                snapshot: Box::new(snapshot),
            },
        );
        Ok(())
    }

    /// Accepts a leader's snapshot, replacing this replica's log and state
    /// machine.
    pub(crate) fn handle_install_snapshot(&mut self, envelope: &Envelope) -> Result<()> {
        let RaftMessage::InstallSnapshot { snapshot, .. } = &envelope.message else {
            return Ok(());
        };
        if self.role.is_campaigning() {
            self.become_follower(self.hard.term, Some(envelope.from))?;
        }
        self.leader = Some(envelope.from);
        self.reset_election_timer();

        let last_index = snapshot.meta.last_index();
        if last_index <= self.commit_index {
            // Already have everything this snapshot covers; say so rather
            // than throwing away a longer, equally valid log.
            self.send(
                envelope.from,
                RaftMessage::InstallSnapshotResponse {
                    term: self.hard.term,
                    last_index: self.log.last_index(),
                },
            );
            return Ok(());
        }

        self.machine
            .restore(&snapshot.data)
            .map_err(|reason| RaftError::Snapshot {
                what: "restore",
                reason,
            })?;
        self.log.install_snapshot(snapshot)?;
        self.commit_index = last_index;
        self.last_applied = last_index;
        self.membership = snapshot.meta.membership().clone();
        self.pending_config = None;

        tracing::info!(
            peer = self.config.id.get(),
            through = last_index.get(),
            "installed a leader's snapshot"
        );
        self.send(
            envelope.from,
            RaftMessage::InstallSnapshotResponse {
                term: self.hard.term,
                last_index,
            },
        );
        Ok(())
    }

    /// Records a follower's acknowledgement of a snapshot.
    pub(crate) fn handle_install_snapshot_response(&mut self, envelope: &Envelope) -> Result<()> {
        let RaftMessage::InstallSnapshotResponse { term, last_index } = &envelope.message else {
            return Ok(());
        };
        if self.role != Role::Leader || *term != self.hard.term {
            return Ok(());
        }
        self.note_contact(envelope.from);
        if let Some(progress) = self.progress.get_mut(&envelope.from) {
            progress.snapshot_in_flight = false;
            if *last_index > progress.match_index {
                progress.match_index = *last_index;
            }
            progress.next_index = progress.match_index.next().max(LogIndex::FIRST);
        }
        self.maybe_advance_commit()?;
        self.apply_committed()?;
        self.send_append(envelope.from)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::core::harness::*;
    use crate::log::store::MemoryLog;
    use crate::membership::Membership;
    use crate::state_machine::MemoryStateMachine;
    use crate::types::Term;

    #[test]
    fn only_command_entries_reach_the_state_machine() {
        let mut node = leader(1);
        // The leader's no-op is already committed by the fixture.
        let applied = node.take_applied();
        assert!(
            applied.iter().all(|entry| entry.result.is_none()),
            "a no-op is consensus bookkeeping, not an application command"
        );
        assert!(node.state_machine().commands().is_empty());

        node.propose(b"real".to_vec()).unwrap();
        acknowledge_all(&mut node);
        let applied = node.take_applied();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].result, Some(b"real".to_vec()));
    }

    #[test]
    fn applied_entries_are_reported_once_and_in_order() {
        let mut node = leader(1);
        node.take_applied();
        for command in [b"a".as_slice(), b"b", b"c"] {
            node.propose(command.to_vec()).unwrap();
        }
        acknowledge_all(&mut node);
        let applied = node.take_applied();
        let indices: Vec<u64> = applied.iter().map(|entry| entry.index.get()).collect();
        assert!(indices.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(node.state_machine().commands().len(), 3);
        // Draining twice must not repeat anything.
        assert!(node.take_applied().is_empty());
    }

    #[test]
    fn a_configuration_entry_never_reaches_the_state_machine() {
        let mut node = leader(1);
        node.take_applied();
        node.propose_membership(crate::membership::MembershipChange::Add(PeerId::new(4)))
            .unwrap();
        acknowledge_all(&mut node);
        assert!(node.state_machine().commands().is_empty());
        assert_eq!(node.membership().len(), 4);
    }

    #[test]
    fn the_threshold_compacts_the_log_and_keeps_the_state_machine() {
        let mut node = RaftNode::new(
            config(1).with_snapshot_threshold(8),
            MemoryLog::new(),
            MemoryStateMachine::new(),
        )
        .unwrap();
        for _ in 0..32 {
            node.tick().unwrap();
            if node.role() == Role::Leader {
                break;
            }
            grant_all(&mut node);
        }
        assert_eq!(node.role(), Role::Leader);
        node.take_messages();

        for index in 0..24u64 {
            node.propose(index.to_le_bytes().to_vec()).unwrap();
            acknowledge_all(&mut node);
        }
        assert!(
            node.log().first_index() > LogIndex::FIRST,
            "the prefix should have been compacted"
        );
        assert_eq!(node.state_machine().commands().len(), 24);
        assert!(node.log().snapshot().unwrap().is_some());
    }

    #[test]
    fn a_replica_restarted_from_a_snapshot_recovers_its_state_machine() {
        let mut node = RaftNode::new(
            config(1).with_snapshot_threshold(4),
            MemoryLog::new(),
            MemoryStateMachine::new(),
        )
        .unwrap();
        for _ in 0..32 {
            node.tick().unwrap();
            if node.role() == Role::Leader {
                break;
            }
            grant_all(&mut node);
        }
        node.take_messages();
        for index in 0..12u64 {
            node.propose(index.to_le_bytes().to_vec()).unwrap();
            acknowledge_all(&mut node);
        }
        let applied = node.last_applied();
        let commands = node.state_machine().commands().len();

        let last_index = node.log().last_index();

        let (log, _) = node.into_parts();
        let restarted = RaftNode::new(
            config(1).with_snapshot_threshold(4),
            log,
            MemoryStateMachine::new(),
        )
        .unwrap();
        // Raft does not persist `commitIndex`: a restarted replica knows only
        // that its snapshot is committed, and relearns the rest from whoever
        // wins the next election. So the state machine comes back exactly at
        // the snapshot boundary, with the entries after it still in the log,
        // waiting to be applied a second time by that leader's commit index.
        let boundary = restarted
            .log()
            .snapshot()
            .unwrap()
            .unwrap()
            .meta
            .last_index();
        assert_eq!(restarted.last_applied(), boundary);
        assert_eq!(restarted.commit_index(), boundary);
        assert!(boundary <= applied);
        assert!(restarted.state_machine().commands().len() <= commands);
        assert_eq!(
            restarted.log().last_index(),
            last_index,
            "no entry is lost by the restart, only the knowledge that it committed"
        );
    }

    #[test]
    fn a_follower_behind_compaction_is_sent_a_snapshot() {
        let mut node = RaftNode::new(
            config(1).with_snapshot_threshold(4),
            MemoryLog::new(),
            MemoryStateMachine::new(),
        )
        .unwrap();
        for _ in 0..32 {
            node.tick().unwrap();
            if node.role() == Role::Leader {
                break;
            }
            grant_all(&mut node);
        }
        node.take_messages();
        for index in 0..12u64 {
            node.propose(index.to_le_bytes().to_vec()).unwrap();
            acknowledge_all(&mut node);
        }
        node.take_messages();

        // Peer 3 turns out to be far behind: it rejects all the way back.
        if let Some(progress) = node.progress.get_mut(&PeerId::new(3)) {
            progress.next_index = LogIndex::FIRST;
            progress.match_index = LogIndex::ZERO;
        }
        node.send_append(PeerId::new(3)).unwrap();
        let messages = node.take_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message.variant_name(), "InstallSnapshot");
    }

    #[test]
    fn an_installed_snapshot_replaces_the_log_and_the_state_machine() {
        let mut source = MemoryStateMachine::new();
        source.apply(LogIndex::new(1), b"one").unwrap();
        source.apply(LogIndex::new(2), b"two").unwrap();
        let data = source.snapshot().unwrap();

        let mut node = node(1);
        node.log_mut()
            .append(&[crate::log::entry::LogEntry::command(
                Term::new(1),
                LogIndex::new(1),
                b"stale".to_vec(),
            )])
            .unwrap();

        node.step(Envelope::new(
            PeerId::new(2),
            PeerId::new(1),
            RaftMessage::InstallSnapshot {
                term: Term::new(4),
                snapshot: Box::new(Snapshot::new(
                    SnapshotMeta::new(
                        LogIndex::new(2),
                        Term::new(3),
                        Membership::new([PeerId::new(1), PeerId::new(2)]),
                    ),
                    data,
                )),
            },
        ))
        .unwrap();

        assert_eq!(node.commit_index(), LogIndex::new(2));
        assert_eq!(node.last_applied(), LogIndex::new(2));
        assert_eq!(node.state_machine().commands().len(), 2);
        assert_eq!(node.log().first_index(), LogIndex::new(3));
        assert_eq!(node.membership().len(), 2);
        assert_eq!(
            node.take_messages()[0].message.variant_name(),
            "InstallSnapshotResponse"
        );
    }

    #[test]
    fn a_snapshot_this_replica_already_covers_is_acknowledged_not_applied() {
        let mut node = node(1);
        node.step(Envelope::new(
            PeerId::new(2),
            PeerId::new(1),
            RaftMessage::AppendEntries {
                term: Term::new(2),
                prev_log_index: LogIndex::ZERO,
                prev_log_term: Term::INITIAL,
                entries: vec![
                    crate::log::entry::LogEntry::command(Term::new(2), LogIndex::new(1), vec![1]),
                    crate::log::entry::LogEntry::command(Term::new(2), LogIndex::new(2), vec![2]),
                    crate::log::entry::LogEntry::command(Term::new(2), LogIndex::new(3), vec![3]),
                ],
                leader_commit: LogIndex::new(3),
            },
        ))
        .unwrap();
        node.take_messages();
        assert_eq!(node.commit_index(), LogIndex::new(3));

        node.step(Envelope::new(
            PeerId::new(2),
            PeerId::new(1),
            RaftMessage::InstallSnapshot {
                term: Term::new(2),
                snapshot: Box::new(Snapshot::new(
                    SnapshotMeta::new(LogIndex::new(2), Term::new(2), Membership::default()),
                    Vec::new(),
                )),
            },
        ))
        .unwrap();
        assert_eq!(
            node.log().last_index(),
            LogIndex::new(3),
            "a longer, equally valid log must not be thrown away"
        );
    }

    #[test]
    fn an_explicit_snapshot_can_be_taken_on_demand() {
        let mut node = leader(1);
        node.propose(b"a".to_vec()).unwrap();
        acknowledge_all(&mut node);
        node.take_snapshot().unwrap();
        let snapshot = node.log().snapshot().unwrap().unwrap();
        assert_eq!(snapshot.meta.last_index(), node.last_applied());
        assert_eq!(snapshot.meta.membership(), node.membership());
    }

    #[test]
    fn taking_a_snapshot_of_nothing_is_a_no_op() {
        let mut node = node(1);
        node.take_snapshot().unwrap();
        assert!(node.log().snapshot().unwrap().is_none());
    }
}
