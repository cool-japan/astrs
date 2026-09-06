//! Elections: pre-vote, `RequestVote`, and taking office.
//!
//! # The two rules that make elections safe
//!
//! 1. **At most one vote per term.** `voted_for` is persisted with the term
//!    it belongs to, so a peer that crashes after voting cannot vote again
//!    for a different candidate in the same term after it restarts. Without
//!    that, two candidates can both collect a majority in one term.
//! 2. **Only an up-to-date log may win.** A candidate whose log is behind
//!    any voter's is refused, which is what guarantees that every committed
//!    entry survives into the next term (the *Leader Completeness* property).
//!    "Up to date" compares the last entry's **term first**, index only as a
//!    tie-break — a longer log from an older term is *not* more up to date.
//!
//! # Pre-vote, and the trap in it
//!
//! A pre-vote request asks about a term the sender has **not adopted**:
//! `term + 1`. Two things must therefore never happen, and both are enforced
//! in [`crate::RaftNode::step`]:
//!
//! - a receiver must not adopt that hypothetical term (any partitioned peer
//!   could otherwise inflate the whole cluster's term at will);
//! - a *granted* pre-vote response must not step its recipient down (it
//!   carries the responder's own, real term, which by construction is not
//!   ahead of the hypothetical one).
//!
//! Both are covered by this module's own tests.

use crate::core::RaftNode;
use crate::error::Result;
use crate::log::store::LogStore;
use crate::message::{Envelope, RaftMessage};
use crate::state_machine::StateMachine;
use crate::types::{LogIndex, PeerId, Role, Term};

impl<M: StateMachine, L: LogStore> RaftNode<M, L> {
    /// Starts a pre-vote round: asks whether a real campaign would win,
    /// without touching this replica's own term.
    pub(crate) fn campaign_pre_vote(&mut self) -> Result<()> {
        self.role = Role::PreCandidate;
        self.leader = None;
        self.votes.clear();
        self.votes.insert(self.config.id, true);
        self.reset_election_timer();

        // A cluster of one has nobody to ask; go straight to the real thing.
        if self.membership.is_quorum(1) {
            return self.campaign();
        }

        let hypothetical = self.hard.term.next();
        let last_log_index = self.log.last_index();
        let last_log_term = self.log.last_term();
        tracing::debug!(
            peer = self.config.id.get(),
            term = self.hard.term.get(),
            "starting a pre-vote round"
        );
        for peer in self.other_voters() {
            self.send(
                peer,
                RaftMessage::PreVoteRequest {
                    term: hypothetical,
                    last_log_index,
                    last_log_term,
                },
            );
        }
        Ok(())
    }

    /// Stands for election: bumps the term, votes for itself, asks everyone
    /// else.
    pub(crate) fn campaign(&mut self) -> Result<()> {
        let term = self.hard.term.next();
        self.save_hard_state(term, Some(self.config.id))?;
        self.role = Role::Candidate;
        self.leader = None;
        self.votes.clear();
        self.votes.insert(self.config.id, true);
        self.reset_election_timer();

        if self.membership.is_quorum(1) {
            return self.become_leader();
        }

        let last_log_index = self.log.last_index();
        let last_log_term = self.log.last_term();
        tracing::info!(
            peer = self.config.id.get(),
            term = term.get(),
            "standing for election"
        );
        for peer in self.other_voters() {
            self.send(
                peer,
                RaftMessage::VoteRequest {
                    term,
                    last_log_index,
                    last_log_term,
                },
            );
        }
        Ok(())
    }

    /// Answers a [`RaftMessage::PreVoteRequest`] or
    /// [`RaftMessage::VoteRequest`].
    pub(crate) fn handle_vote_request(&mut self, envelope: &Envelope) -> Result<()> {
        let (pre_vote, term, last_log_index, last_log_term) = match &envelope.message {
            RaftMessage::PreVoteRequest {
                term,
                last_log_index,
                last_log_term,
            } => (true, *term, *last_log_index, *last_log_term),
            RaftMessage::VoteRequest {
                term,
                last_log_index,
                last_log_term,
            } => (false, *term, *last_log_index, *last_log_term),
            _ => return Ok(()),
        };

        let up_to_date = self.is_log_up_to_date(last_log_index, last_log_term);
        let granted = if pre_vote {
            // Refuse while a leader is still reaching us: that is the whole
            // point of pre-vote. A peer whose own partition healed asks, is
            // told no, and never disturbs the healthy term.
            let leader_is_gone = self.leader.is_none()
                || self.election_elapsed >= self.config.election_timeout_ticks;
            term > self.hard.term && up_to_date && leader_is_gone
        } else {
            // Figure 2: vote if this term's vote is unspent or was already
            // spent on this very candidate (which makes a retransmitted
            // request idempotent), and the candidate's log is up to date.
            let vote_available = self.hard.can_vote() || self.hard.has_voted_for(envelope.from);
            vote_available && up_to_date
        };

        if granted && !pre_vote {
            self.save_hard_state(self.hard.term, Some(envelope.from))?;
            // Granting a vote means an election is under way; give the
            // candidate the full timeout to finish it.
            self.reset_election_timer();
        }

        let reply = if pre_vote {
            // The responder's own term, never the hypothetical one it was
            // asked about — see this module's header.
            RaftMessage::PreVoteResponse {
                term: self.hard.term,
                granted,
            }
        } else {
            RaftMessage::VoteResponse {
                term: self.hard.term,
                granted,
            }
        };
        self.send(envelope.from, reply);
        Ok(())
    }

    /// Counts a vote or pre-vote response.
    pub(crate) fn handle_vote_response(&mut self, envelope: &Envelope) -> Result<()> {
        let (pre_vote, granted) = match &envelope.message {
            RaftMessage::PreVoteResponse { granted, .. } => (true, *granted),
            RaftMessage::VoteResponse { granted, .. } => (false, *granted),
            _ => return Ok(()),
        };

        // A response to a campaign this replica is no longer running is
        // stale by definition.
        let expected = if pre_vote {
            Role::PreCandidate
        } else {
            Role::Candidate
        };
        if self.role != expected {
            return Ok(());
        }

        self.votes.insert(envelope.from, granted);
        let for_votes = self.votes.values().filter(|granted| **granted).count();
        let against = self.votes.len() - for_votes;

        if self.membership.is_quorum(for_votes) {
            return if pre_vote {
                // The cluster says a real campaign would win; now spend a
                // term on it.
                self.campaign()
            } else {
                self.become_leader()
            };
        }
        if self.membership.is_quorum(against) {
            // Lost outright: stop campaigning rather than waiting out the
            // timeout with a foregone conclusion.
            tracing::debug!(
                peer = self.config.id.get(),
                term = self.hard.term.get(),
                pre_vote,
                "campaign lost"
            );
            self.role = Role::Follower;
            self.votes.clear();
            self.reset_election_timer();
        }
        Ok(())
    }

    /// Takes office: initializes replication state and appends the no-op
    /// that settles every entry inherited from earlier terms.
    pub(crate) fn become_leader(&mut self) -> Result<()> {
        tracing::info!(
            peer = self.config.id.get(),
            term = self.hard.term.get(),
            "elected leader"
        );
        self.role = Role::Leader;
        self.leader = Some(self.config.id);
        self.votes.clear();
        self.election_elapsed = 0;
        self.heartbeat_elapsed = 0;
        self.progress.clear();
        self.reset_progress();
        // Figure 8: a leader may not count replicas to commit an entry from
        // an *earlier* term. Committing one entry of its own term settles
        // every earlier one with it, and this no-op is that entry — which is
        // also the index a caller waits on before trusting a local read.
        self.append_local(crate::log::entry::EntryPayload::Noop)?;
        Ok(())
    }

    /// Whether a candidate whose last entry is `(index, term)` is at least
    /// as up to date as this replica's log.
    ///
    /// Term first, index only as a tie-break: a longer log from an older
    /// term lost, and must not be able to win an election.
    #[must_use]
    pub(crate) fn is_log_up_to_date(&self, index: LogIndex, term: Term) -> bool {
        let own_term = self.log.last_term();
        if term != own_term {
            return term > own_term;
        }
        index >= self.log.last_index()
    }

    /// The vote this replica has cast in its current term, if any.
    #[must_use]
    pub fn voted_for(&self) -> Option<PeerId> {
        self.hard.voted_for
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::core::harness::*;
    use crate::log::entry::LogEntry;

    fn vote_request(from: u64, term: u64, index: u64, log_term: u64) -> Envelope {
        Envelope::new(
            PeerId::new(from),
            PeerId::new(1),
            RaftMessage::VoteRequest {
                term: Term::new(term),
                last_log_index: LogIndex::new(index),
                last_log_term: Term::new(log_term),
            },
        )
    }

    #[test]
    fn a_timed_out_follower_runs_a_pre_vote_round_first() {
        let mut node = node(1);
        for _ in 0..32 {
            node.tick().unwrap();
            if node.role() == Role::PreCandidate {
                break;
            }
        }
        assert_eq!(node.role(), Role::PreCandidate);
        // Crucially, the term is untouched: a pre-vote costs nothing.
        assert_eq!(node.term(), Term::INITIAL);
        let messages = node.take_messages();
        assert_eq!(messages.len(), 2);
        for envelope in &messages {
            assert_eq!(envelope.message.variant_name(), "PreVoteRequest");
            assert_eq!(envelope.term(), Term::new(1), "asked about term + 1");
        }
    }

    #[test]
    fn winning_the_pre_vote_starts_a_real_campaign() {
        let mut node = node(1);
        for _ in 0..32 {
            node.tick().unwrap();
            if node.role() == Role::PreCandidate {
                break;
            }
        }
        let request = node.take_messages();
        node.step(Envelope::new(
            request[0].to,
            PeerId::new(1),
            RaftMessage::PreVoteResponse {
                term: Term::INITIAL,
                granted: true,
            },
        ))
        .unwrap();
        assert_eq!(node.role(), Role::Candidate);
        assert_eq!(node.term(), Term::new(1), "now the term is really spent");
        assert_eq!(node.voted_for(), Some(PeerId::new(1)));
    }

    #[test]
    fn a_pre_vote_request_never_makes_its_receiver_adopt_the_hypothetical_term() {
        // The classic pre-vote bug: adopting `term + 1` here would let any
        // partitioned peer inflate the cluster's term at will.
        let mut node = node(1);
        node.step(Envelope::new(
            PeerId::new(2),
            PeerId::new(1),
            RaftMessage::PreVoteRequest {
                term: Term::new(50),
                last_log_index: LogIndex::ZERO,
                last_log_term: Term::INITIAL,
            },
        ))
        .unwrap();
        assert_eq!(node.term(), Term::INITIAL);
        assert_eq!(node.voted_for(), None, "a pre-vote spends no vote");
        let reply = node.take_messages();
        assert_eq!(reply.len(), 1);
        assert!(matches!(
            reply[0].message,
            RaftMessage::PreVoteResponse {
                term: Term::INITIAL,
                granted: true
            }
        ));
    }

    #[test]
    fn a_granted_pre_vote_response_never_steps_its_recipient_down() {
        let mut node = leader(1);
        let before = node.term();
        node.step(Envelope::new(
            PeerId::new(2),
            PeerId::new(1),
            RaftMessage::PreVoteResponse {
                term: before.next().next(),
                granted: true,
            },
        ))
        .unwrap();
        assert_eq!(node.role(), Role::Leader);
        assert_eq!(node.term(), before);
    }

    #[test]
    fn a_rejected_pre_vote_response_from_a_higher_term_does_step_down() {
        // A rejection carries the responder's own, real term — genuine news
        // that this peer is behind.
        let mut node = leader(1);
        let ahead = node.term().next().next();
        node.step(Envelope::new(
            PeerId::new(2),
            PeerId::new(1),
            RaftMessage::PreVoteResponse {
                term: ahead,
                granted: false,
            },
        ))
        .unwrap();
        assert_eq!(node.role(), Role::Follower);
        assert_eq!(node.term(), ahead);
    }

    #[test]
    fn a_healthy_follower_refuses_a_pre_vote_and_protects_its_leader() {
        let mut node = node(1);
        // A heartbeat has just arrived, so this follower's leader is alive.
        node.step(Envelope::new(
            PeerId::new(3),
            PeerId::new(1),
            RaftMessage::AppendEntries {
                term: Term::new(2),
                prev_log_index: LogIndex::ZERO,
                prev_log_term: Term::INITIAL,
                entries: Vec::new(),
                leader_commit: LogIndex::ZERO,
            },
        ))
        .unwrap();
        node.take_messages();

        node.step(Envelope::new(
            PeerId::new(2),
            PeerId::new(1),
            RaftMessage::PreVoteRequest {
                term: Term::new(3),
                last_log_index: LogIndex::ZERO,
                last_log_term: Term::INITIAL,
            },
        ))
        .unwrap();
        let reply = node.take_messages();
        assert!(matches!(
            reply[0].message,
            RaftMessage::PreVoteResponse { granted: false, .. }
        ));
        assert_eq!(node.term(), Term::new(2), "the healthy term is untouched");
    }

    #[test]
    fn a_vote_is_cast_at_most_once_per_term() {
        let mut node = node(1);
        node.step(vote_request(2, 1, 0, 0)).unwrap();
        assert!(matches!(
            node.take_messages()[0].message,
            RaftMessage::VoteResponse { granted: true, .. }
        ));
        // A different candidate in the same term is refused.
        node.step(vote_request(3, 1, 0, 0)).unwrap();
        assert!(matches!(
            node.take_messages()[0].message,
            RaftMessage::VoteResponse { granted: false, .. }
        ));
        // The same candidate asking again is granted — retransmission must
        // be idempotent, not self-defeating.
        node.step(vote_request(2, 1, 0, 0)).unwrap();
        assert!(matches!(
            node.take_messages()[0].message,
            RaftMessage::VoteResponse { granted: true, .. }
        ));
    }

    #[test]
    fn a_vote_survives_a_restart_in_the_same_term() {
        // The whole reason `voted_for` is durable: without it a crashed peer
        // could vote twice in one term, and two leaders could coexist.
        let mut node = RaftNode::new(
            config(1),
            crate::log::store::MemoryLog::new(),
            crate::state_machine::MemoryStateMachine::new(),
        )
        .unwrap();
        node.step(vote_request(2, 5, 0, 0)).unwrap();
        assert_eq!(node.voted_for(), Some(PeerId::new(2)));

        let (log, machine) = node.into_parts();
        let mut restarted = RaftNode::new(config(1), log, machine).unwrap();
        assert_eq!(restarted.term(), Term::new(5));
        assert_eq!(restarted.voted_for(), Some(PeerId::new(2)));
        restarted.step(vote_request(3, 5, 0, 0)).unwrap();
        assert!(matches!(
            restarted.take_messages()[0].message,
            RaftMessage::VoteResponse { granted: false, .. }
        ));
    }

    #[test]
    fn a_behind_candidate_is_refused_even_with_a_higher_term() {
        let mut node = node(1);
        node.log_mut()
            .append(&[
                LogEntry::command(Term::new(4), LogIndex::new(1), vec![]),
                LogEntry::command(Term::new(4), LogIndex::new(2), vec![]),
            ])
            .unwrap();

        // Same term, shorter log: refused.
        node.step(vote_request(2, 9, 1, 4)).unwrap();
        assert!(matches!(
            node.take_messages()[0].message,
            RaftMessage::VoteResponse { granted: false, .. }
        ));
    }

    #[test]
    fn a_longer_log_from_an_older_term_is_not_more_up_to_date() {
        // The comparison is term first, index only as a tie-break. Getting
        // this backwards is how a committed entry gets lost.
        let mut node = node(1);
        node.log_mut()
            .append(&[LogEntry::command(Term::new(5), LogIndex::new(1), vec![])])
            .unwrap();
        assert!(!node.is_log_up_to_date(LogIndex::new(100), Term::new(4)));
        assert!(node.is_log_up_to_date(LogIndex::new(1), Term::new(5)));
        assert!(node.is_log_up_to_date(LogIndex::new(1), Term::new(6)));
        assert!(!node.is_log_up_to_date(LogIndex::ZERO, Term::new(5)));
    }

    #[test]
    fn a_split_vote_leaves_nobody_leader_and_the_loser_stands_down() {
        let mut node = node(1);
        for _ in 0..32 {
            node.tick().unwrap();
            if node.role() == Role::PreCandidate {
                break;
            }
        }
        grant_all(&mut node);
        assert_eq!(node.role(), Role::Candidate);
        node.take_messages();

        for peer in [2u64, 3] {
            node.step(Envelope::new(
                PeerId::new(peer),
                PeerId::new(1),
                RaftMessage::VoteResponse {
                    term: node.term(),
                    granted: false,
                },
            ))
            .unwrap();
        }
        assert_eq!(node.role(), Role::Follower);
    }

    #[test]
    fn a_new_leader_appends_a_no_op_of_its_own_term() {
        let node = leader(1);
        let last = node.log().last_index();
        let entry = node.log().entry(last).unwrap().unwrap();
        assert_eq!(entry.payload(), &crate::log::entry::EntryPayload::Noop);
        assert_eq!(entry.term(), node.term());
    }

    #[test]
    fn a_vote_response_for_a_finished_campaign_is_ignored() {
        let mut node = leader(1);
        let term = node.term();
        node.take_messages();
        node.step(Envelope::new(
            PeerId::new(2),
            PeerId::new(1),
            RaftMessage::VoteResponse {
                term,
                granted: false,
            },
        ))
        .unwrap();
        assert_eq!(node.role(), Role::Leader, "the election is over");
    }

    #[test]
    fn pre_vote_can_be_turned_off() {
        let mut node = RaftNode::new(
            config(1).with_pre_vote(false),
            crate::log::store::MemoryLog::new(),
            crate::state_machine::MemoryStateMachine::new(),
        )
        .unwrap();
        for _ in 0..32 {
            node.tick().unwrap();
            if node.role() == Role::Candidate {
                break;
            }
        }
        assert_eq!(node.role(), Role::Candidate);
        assert_eq!(node.term(), Term::new(1));
    }
}
