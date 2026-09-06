//! Replication: `AppendEntries` in both directions, and the commit rule.
//!
//! # The Log Matching property
//!
//! Every `AppendEntries` names the `(index, term)` immediately before the
//! entries it carries. A follower that does not hold exactly that pair
//! refuses, and the leader walks back. Because entries are only ever
//! appended after a matching predecessor, two logs agreeing at some index
//! agree at every index before it — which is what makes "the leader's log is
//! the truth" a safe rule rather than a hope.
//!
//! # Backing up in one round trip, not one entry at a time
//!
//! Figure 2 decrements `nextIndex` by one per rejection, which costs a round
//! trip per divergent entry. The rejection here carries a *conflict hint*
//! ([`RaftMessage::AppendEntriesResponse::conflict_term`] and
//! `conflict_index`), the optimization the Raft paper describes in §5.3: the
//! follower reports the term it actually holds at the conflict point and the
//! first index of that term, and the leader jumps straight past the whole
//! divergent run.
//!
//! # The commit rule, and the Figure 8 trap
//!
//! A leader advances `commitIndex` to the highest index replicated on a
//! majority — **but only if that entry belongs to the leader's own term**.
//! Counting replicas on an entry inherited from an earlier term can commit
//! an entry that a later leader legitimately overwrites, which is the
//! scenario Figure 8 of the paper draws. That single extra condition, and
//! the no-op a new leader appends to satisfy it quickly, is the whole fix.

use crate::core::RaftNode;
use crate::error::Result;
use crate::log::store::LogStore;
use crate::message::{Envelope, RaftMessage};
use crate::state_machine::StateMachine;
use crate::types::{LogIndex, PeerId, Role, Term};

impl<M: StateMachine, L: LogStore> RaftNode<M, L> {
    /// Sends every follower whatever it is missing.
    pub(crate) fn broadcast_append(&mut self) -> Result<()> {
        for peer in self.other_voters() {
            self.send_append(peer)?;
        }
        Ok(())
    }

    /// Sends one follower the entries after its `next_index`, or a snapshot
    /// when those entries have been compacted away.
    pub(crate) fn send_append(&mut self, peer: PeerId) -> Result<()> {
        let Some(progress) = self.progress.get(&peer).copied() else {
            return Ok(());
        };
        let first = self.log.first_index();
        if progress.next_index < first {
            // The follower needs entries this leader no longer holds.
            return self.send_snapshot(peer);
        }
        let prev_log_index = progress.next_index.previous();
        let Some(prev_log_term) = self.log.term_at(prev_log_index)? else {
            return self.send_snapshot(peer);
        };
        let entries = self
            .log
            .entries(progress.next_index, self.config.max_entries_per_append)?;
        let term = self.hard.term;
        let leader_commit = self.commit_index;
        self.send(
            peer,
            RaftMessage::AppendEntries {
                term,
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit,
            },
        );
        Ok(())
    }

    /// Handles one `AppendEntries` as a follower.
    pub(crate) fn handle_append_entries(&mut self, envelope: &Envelope) -> Result<()> {
        let RaftMessage::AppendEntries {
            term,
            prev_log_index,
            prev_log_term,
            entries,
            leader_commit,
        } = &envelope.message
        else {
            return Ok(());
        };

        // A candidate that hears from a leader of its own term has lost:
        // there is already a leader for this term, so stop campaigning.
        if self.role.is_campaigning() {
            self.become_follower(*term, Some(envelope.from))?;
        }
        self.leader = Some(envelope.from);
        self.reset_election_timer();

        let boundary = self.log.first_index().previous();
        if *prev_log_index < boundary {
            // These entries are inside a prefix this replica has already
            // snapshotted — and a snapshot is committed by construction, so
            // reporting a match at its boundary is honest and moves the
            // leader forward instead of looping.
            self.send(
                envelope.from,
                RaftMessage::AppendEntriesResponse {
                    term: self.hard.term,
                    success: true,
                    match_index: boundary,
                    conflict_index: LogIndex::ZERO,
                    conflict_term: Term::INITIAL,
                },
            );
            return Ok(());
        }

        let last_index = self.log.last_index();
        if *prev_log_index > last_index {
            // Too short: tell the leader exactly where this log ends so it
            // resumes there rather than probing backwards one at a time.
            self.send(
                envelope.from,
                RaftMessage::AppendEntriesResponse {
                    term: self.hard.term,
                    success: false,
                    match_index: LogIndex::ZERO,
                    conflict_index: last_index.next(),
                    conflict_term: Term::INITIAL,
                },
            );
            return Ok(());
        }

        let held_term = self.log.term_at(*prev_log_index)?.unwrap_or(Term::INITIAL);
        if held_term != *prev_log_term {
            let conflict_index = self.first_index_of_term(*prev_log_index, held_term)?;
            self.send(
                envelope.from,
                RaftMessage::AppendEntriesResponse {
                    term: self.hard.term,
                    success: false,
                    match_index: LogIndex::ZERO,
                    conflict_index,
                    conflict_term: held_term,
                },
            );
            return Ok(());
        }

        self.merge_entries(entries)?;

        let last_new = prev_log_index.saturating_add(entries.len() as u64);
        if *leader_commit > self.commit_index {
            // Never past what this replica actually holds: the leader's
            // commit index may already cover entries still in flight.
            self.commit_index = (*leader_commit).min(last_new);
            self.apply_committed()?;
        }

        self.send(
            envelope.from,
            RaftMessage::AppendEntriesResponse {
                term: self.hard.term,
                success: true,
                match_index: last_new,
                conflict_index: LogIndex::ZERO,
                conflict_term: Term::INITIAL,
            },
        );
        Ok(())
    }

    /// Appends `entries`, truncating first at the first index whose term
    /// disagrees.
    ///
    /// Entries this replica already holds with the same term are skipped
    /// rather than re-appended: a duplicated or delayed `AppendEntries` must
    /// not truncate a log that has since moved on.
    fn merge_entries(&mut self, entries: &[crate::log::entry::LogEntry]) -> Result<()> {
        let mut append_from = entries.len();
        let mut truncated = false;
        for (offset, entry) in entries.iter().enumerate() {
            match self.log.term_at(entry.index())? {
                Some(existing) if existing == entry.term() => {}
                Some(_) => {
                    self.log.truncate_from(entry.index())?;
                    truncated = true;
                    append_from = offset;
                    break;
                }
                None => {
                    append_from = offset;
                    break;
                }
            }
        }
        let appended = entries.get(append_from..).unwrap_or_default();
        if !appended.is_empty() {
            self.log.append(appended)?;
        }
        // A configuration entry is in force from the moment it is appended,
        // and a truncation can un-append one, so both directions have to go
        // through the same recomputation.
        if truncated || appended.iter().any(crate::log::entry::LogEntry::is_config) {
            self.recompute_membership()?;
        }
        Ok(())
    }

    /// The first index of the run of entries with `term` that ends at
    /// `from`.
    fn first_index_of_term(&self, from: LogIndex, term: Term) -> Result<LogIndex> {
        let floor = self.log.first_index();
        let mut index = from;
        while index > floor {
            let candidate = index.previous();
            match self.log.term_at(candidate)? {
                Some(found) if found == term => index = candidate,
                _ => break,
            }
        }
        Ok(index)
    }

    /// Handles one `AppendEntriesResponse` as a leader.
    pub(crate) fn handle_append_response(&mut self, envelope: &Envelope) -> Result<()> {
        let RaftMessage::AppendEntriesResponse {
            term,
            success,
            match_index,
            conflict_index,
            conflict_term,
        } = &envelope.message
        else {
            return Ok(());
        };
        if self.role != Role::Leader || *term != self.hard.term {
            return Ok(());
        }
        self.note_contact(envelope.from);

        if *success {
            let last_index = self.log.last_index();
            let Some(progress) = self.progress.get_mut(&envelope.from) else {
                return Ok(());
            };
            // Never move `match_index` backwards: a delayed duplicate of an
            // older, smaller acknowledgement must not un-acknowledge work.
            if *match_index > progress.match_index {
                progress.match_index = *match_index;
            }
            progress.next_index = progress.match_index.next();
            progress.snapshot_in_flight = false;
            let behind = progress.next_index <= last_index;

            self.maybe_advance_commit()?;
            self.apply_committed()?;
            if behind {
                self.send_append(envelope.from)?;
            }
            return Ok(());
        }

        // Rejected: jump past the whole divergent run in one step.
        let mut next = *conflict_index;
        if *conflict_term != Term::INITIAL
            && let Some(last_of_term) = self.last_index_of_term(*conflict_term)?
        {
            next = last_of_term.next();
        }
        let Some(progress) = self.progress.get_mut(&envelope.from) else {
            return Ok(());
        };
        // A rejection can never take `next_index` back past what this
        // follower has already acknowledged.
        progress.next_index = next.max(progress.match_index.next()).max(LogIndex::FIRST);
        self.send_append(envelope.from)
    }

    /// The highest index in this leader's log whose entry has `term`.
    fn last_index_of_term(&self, term: Term) -> Result<Option<LogIndex>> {
        let floor = self.log.first_index();
        let mut index = self.log.last_index();
        while index >= floor && !index.is_empty_sentinel() {
            match self.log.term_at(index)? {
                Some(found) if found == term => return Ok(Some(index)),
                Some(found) if found < term => return Ok(None),
                _ => {}
            }
            index = index.previous();
        }
        Ok(None)
    }

    /// Advances the commit index to the highest entry of **this leader's
    /// term** that a majority holds.
    pub(crate) fn maybe_advance_commit(&mut self) -> Result<()> {
        if self.role != Role::Leader {
            return Ok(());
        }
        let mut matched: Vec<LogIndex> = Vec::with_capacity(self.membership.len());
        for voter in self.membership.voters() {
            if *voter == self.config.id {
                matched.push(self.log.last_index());
            } else if let Some(progress) = self.progress.get(voter) {
                matched.push(progress.match_index);
            } else {
                matched.push(LogIndex::ZERO);
            }
        }
        matched.sort_unstable_by(|a, b| b.cmp(a));
        let quorum = self.membership.quorum();
        let Some(candidate) = matched.get(quorum.saturating_sub(1)).copied() else {
            return Ok(());
        };
        if candidate <= self.commit_index {
            return Ok(());
        }
        // Figure 8: replication count alone may not commit an entry from an
        // earlier term. The leader's own no-op is what makes this condition
        // true within a heartbeat of taking office.
        if self.log.term_at(candidate)? != Some(self.hard.term) {
            return Ok(());
        }
        self.commit_index = candidate;
        if let Some(pending) = self.pending_config
            && pending <= self.commit_index
        {
            self.pending_config = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::core::harness::*;
    use crate::log::entry::{EntryPayload, LogEntry};
    use crate::membership::Membership;

    fn append(
        term: u64,
        prev_index: u64,
        prev_term: u64,
        entries: Vec<LogEntry>,
        commit: u64,
    ) -> Envelope {
        Envelope::new(
            PeerId::new(2),
            PeerId::new(1),
            RaftMessage::AppendEntries {
                term: Term::new(term),
                prev_log_index: LogIndex::new(prev_index),
                prev_log_term: Term::new(prev_term),
                entries,
                leader_commit: LogIndex::new(commit),
            },
        )
    }

    fn entry(term: u64, index: u64) -> LogEntry {
        LogEntry::command(Term::new(term), LogIndex::new(index), vec![index as u8])
    }

    fn response(
        node: &mut crate::core::RaftNode<
            crate::state_machine::MemoryStateMachine,
            crate::log::store::MemoryLog,
        >,
    ) -> RaftMessage {
        let messages = node.take_messages();
        assert_eq!(messages.len(), 1, "expected exactly one reply");
        messages[0].message.clone()
    }

    #[test]
    fn a_follower_accepts_entries_that_follow_a_matching_predecessor() {
        let mut node = node(1);
        node.step(append(1, 0, 0, vec![entry(1, 1), entry(1, 2)], 0))
            .unwrap();
        match response(&mut node) {
            RaftMessage::AppendEntriesResponse {
                success: true,
                match_index,
                ..
            } => assert_eq!(match_index, LogIndex::new(2)),
            other => panic!("expected an acceptance, got {other:?}"),
        }
        assert_eq!(node.log().last_index(), LogIndex::new(2));
        assert_eq!(node.leader(), Some(PeerId::new(2)));
    }

    #[test]
    fn a_follower_whose_log_is_too_short_says_exactly_where_it_ends() {
        let mut node = node(1);
        node.step(append(1, 0, 0, vec![entry(1, 1)], 0)).unwrap();
        node.take_messages();

        node.step(append(1, 9, 1, vec![entry(1, 10)], 0)).unwrap();
        match response(&mut node) {
            RaftMessage::AppendEntriesResponse {
                success: false,
                conflict_index,
                conflict_term,
                ..
            } => {
                assert_eq!(conflict_index, LogIndex::new(2));
                assert_eq!(conflict_term, Term::INITIAL);
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn a_term_conflict_reports_the_whole_divergent_run_in_one_hint() {
        let mut node = node(1);
        // Terms 1,1,1 then 4,4 — a leader probing index 5 should be told to
        // resume at index 4, not to walk back one entry at a time.
        node.step(append(
            4,
            0,
            0,
            vec![
                entry(1, 1),
                entry(1, 2),
                entry(1, 3),
                entry(4, 4),
                entry(4, 5),
            ],
            0,
        ))
        .unwrap();
        node.take_messages();

        node.step(append(9, 5, 7, vec![entry(9, 6)], 0)).unwrap();
        match response(&mut node) {
            RaftMessage::AppendEntriesResponse {
                success: false,
                conflict_index,
                conflict_term,
                ..
            } => {
                assert_eq!(conflict_term, Term::new(4));
                assert_eq!(conflict_index, LogIndex::new(4));
            }
            other => panic!("expected a conflict hint, got {other:?}"),
        }
    }

    #[test]
    fn conflicting_entries_are_truncated_and_replaced() {
        let mut node = node(1);
        node.step(append(
            1,
            0,
            0,
            vec![entry(1, 1), entry(1, 2), entry(1, 3)],
            0,
        ))
        .unwrap();
        node.take_messages();

        // A new leader at term 5 overwrites from index 2.
        node.step(append(5, 1, 1, vec![entry(5, 2)], 0)).unwrap();
        assert_eq!(node.log().last_index(), LogIndex::new(2));
        assert_eq!(
            node.log().entry(LogIndex::new(2)).unwrap().unwrap().term(),
            Term::new(5)
        );
        assert!(node.log().entry(LogIndex::new(3)).unwrap().is_none());
    }

    #[test]
    fn a_delayed_duplicate_append_does_not_truncate_a_log_that_moved_on() {
        // Re-sending entries the follower already holds must be a no-op: a
        // naive "truncate then append" would drop everything after them.
        let mut node = node(1);
        node.step(append(
            1,
            0,
            0,
            vec![entry(1, 1), entry(1, 2), entry(1, 3)],
            0,
        ))
        .unwrap();
        node.take_messages();

        node.step(append(1, 0, 0, vec![entry(1, 1)], 0)).unwrap();
        assert_eq!(node.log().last_index(), LogIndex::new(3));
    }

    #[test]
    fn a_followers_commit_index_never_outruns_what_it_holds() {
        let mut node = node(1);
        // The leader claims commit 99 while sending only two entries.
        node.step(append(1, 0, 0, vec![entry(1, 1), entry(1, 2)], 99))
            .unwrap();
        assert_eq!(node.commit_index(), LogIndex::new(2));
        assert_eq!(node.last_applied(), LogIndex::new(2));
    }

    #[test]
    fn a_leader_commits_an_entry_a_majority_holds() {
        let mut node = leader(1);
        let index = node.propose(b"replicated".to_vec()).unwrap();
        assert!(node.commit_index() < index || node.membership().len() == 1);
        acknowledge_all(&mut node);
        assert!(node.commit_index() >= index);
        assert_eq!(node.state_machine().commands(), &[b"replicated".to_vec()]);
    }

    #[test]
    fn a_leader_will_not_commit_an_entry_from_an_earlier_term_by_count_alone() {
        // Figure 8: this is the scenario where counting replicas on an
        // inherited entry loses committed data.
        let mut node = node(1);
        node.log_mut().append(&[entry(1, 1), entry(1, 2)]).unwrap();
        node.save_hard_state(Term::new(5), None).unwrap();
        node.role = Role::Leader;
        node.leader = Some(PeerId::new(1));
        node.reset_progress();

        // Both followers report holding index 2 — a majority — but index 2
        // belongs to term 1, not this leader's term 5.
        for peer in [2u64, 3] {
            if let Some(progress) = node.progress.get_mut(&PeerId::new(peer)) {
                progress.match_index = LogIndex::new(2);
            }
        }
        node.maybe_advance_commit().unwrap();
        assert_eq!(
            node.commit_index(),
            LogIndex::ZERO,
            "an inherited entry must not commit on replication count alone"
        );

        // One entry of the leader's own term settles the inherited ones too.
        node.log_mut().append(&[entry(5, 3)]).unwrap();
        for peer in [2u64, 3] {
            if let Some(progress) = node.progress.get_mut(&PeerId::new(peer)) {
                progress.match_index = LogIndex::new(3);
            }
        }
        node.maybe_advance_commit().unwrap();
        assert_eq!(node.commit_index(), LogIndex::new(3));
    }

    #[test]
    fn a_stale_duplicate_acknowledgement_never_moves_match_index_backwards() {
        let mut node = leader(1);
        node.propose(b"a".to_vec()).unwrap();
        node.propose(b"b".to_vec()).unwrap();
        acknowledge_all(&mut node);
        let high = node.progress_of(PeerId::new(2)).unwrap().match_index;

        node.step(Envelope::new(
            PeerId::new(2),
            PeerId::new(1),
            RaftMessage::AppendEntriesResponse {
                term: node.term(),
                success: true,
                match_index: LogIndex::new(1),
                conflict_index: LogIndex::ZERO,
                conflict_term: Term::INITIAL,
            },
        ))
        .unwrap();
        assert_eq!(node.progress_of(PeerId::new(2)).unwrap().match_index, high);
    }

    #[test]
    fn a_rejection_backs_next_index_up_but_never_past_match_index() {
        let mut node = leader(1);
        node.propose(b"a".to_vec()).unwrap();
        acknowledge_all(&mut node);
        let matched = node.progress_of(PeerId::new(2)).unwrap().match_index;

        node.step(Envelope::new(
            PeerId::new(2),
            PeerId::new(1),
            RaftMessage::AppendEntriesResponse {
                term: node.term(),
                success: false,
                match_index: LogIndex::ZERO,
                conflict_index: LogIndex::FIRST,
                conflict_term: Term::INITIAL,
            },
        ))
        .unwrap();
        assert!(node.progress_of(PeerId::new(2)).unwrap().next_index > matched);
    }

    #[test]
    fn a_leaders_heartbeat_carries_no_entries_but_does_carry_the_commit_index() {
        let mut node = leader(1);
        node.propose(b"a".to_vec()).unwrap();
        acknowledge_all(&mut node);
        node.take_messages();
        for _ in 0..node.config().heartbeat_ticks {
            node.tick().unwrap();
        }
        let messages = node.take_messages();
        assert_eq!(messages.len(), 2);
        for envelope in messages {
            match envelope.message {
                RaftMessage::AppendEntries {
                    entries,
                    leader_commit,
                    ..
                } => {
                    assert!(entries.is_empty());
                    assert_eq!(leader_commit, node.commit_index());
                }
                other => panic!("expected a heartbeat, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_candidate_hearing_from_a_leader_of_its_own_term_stands_down() {
        let mut node = node(1);
        for _ in 0..32 {
            node.tick().unwrap();
            if node.role() == Role::PreCandidate {
                break;
            }
        }
        grant_all(&mut node);
        assert_eq!(node.role(), Role::Candidate);
        let term = node.term().get();
        node.take_messages();

        node.step(append(term, 0, 0, Vec::new(), 0)).unwrap();
        assert_eq!(node.role(), Role::Follower);
        assert_eq!(node.leader(), Some(PeerId::new(2)));
    }

    #[test]
    fn a_config_entry_arriving_by_replication_takes_effect_on_append() {
        let mut node = node(1);
        let smaller = Membership::new([PeerId::new(1), PeerId::new(2)]);
        node.step(append(
            2,
            0,
            0,
            vec![LogEntry::new(
                Term::new(2),
                LogIndex::new(1),
                EntryPayload::Config(smaller.clone()),
            )],
            0,
        ))
        .unwrap();
        assert_eq!(node.membership(), &smaller, "in force before it commits");
    }

    #[test]
    fn truncating_away_a_config_entry_rolls_the_membership_back() {
        let mut node = node(1);
        let smaller = Membership::new([PeerId::new(1), PeerId::new(2)]);
        node.step(append(
            2,
            0,
            0,
            vec![LogEntry::new(
                Term::new(2),
                LogIndex::new(1),
                EntryPayload::Config(smaller),
            )],
            0,
        ))
        .unwrap();
        node.take_messages();
        assert_eq!(node.membership().len(), 2);

        // A new leader at term 5 overwrites index 1 with a plain command.
        node.step(append(5, 0, 0, vec![entry(5, 1)], 0)).unwrap();
        assert_eq!(
            node.membership().len(),
            3,
            "the bootstrap configuration is restored when its entry is gone"
        );
    }
}
