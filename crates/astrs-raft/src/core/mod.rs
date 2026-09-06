//! [`RaftNode`]: Figure 2, pre-vote, snapshots and single-server membership,
//! as one pure state machine.
//!
//! # No I/O, no clock, no threads
//!
//! A `RaftNode` never opens a socket and never reads a clock. It consumes
//! three kinds of input — [`RaftNode::tick`], [`RaftNode::step`] and
//! [`RaftNode::propose`] — and produces two kinds of output that a caller
//! drains: outbound messages ([`RaftNode::take_messages`]) and applied
//! entries ([`RaftNode::take_applied`]). Durability is the one thing it
//! delegates, through [`LogStore`].
//!
//! That shape is what makes the consensus code testable. [`crate::sim`] runs
//! this exact type against a virtual clock and a network that drops,
//! duplicates, delays and partitions on a seeded schedule; [`crate::driver`]
//! runs it against `tokio` and a real socket. Neither is a re-implementation
//! of the other.
//!
//! # Where each rule lives
//!
//! | File | Rules |
//! |---|---|
//! | this one | term handling, the tick loop, proposals, membership, status |
//! | [`election`] | pre-vote, `RequestVote`, becoming candidate/leader |
//! | [`replication`] | `AppendEntries` both ways, commit advancement |
//! | [`apply`] | the apply loop, snapshots, `InstallSnapshot` |
//!
//! # Examples
//!
//! ```
//! use astrs_raft::{MemoryLog, MemoryStateMachine, PeerId, RaftConfig, RaftNode, Role};
//!
//! // A single-peer cluster elects itself the moment it times out, because
//! // one vote is already a quorum of one.
//! let config = RaftConfig::new(PeerId::new(1))
//!     .with_peer(PeerId::new(1), "127.0.0.1:7501".parse()?);
//! let mut node = RaftNode::new(config, MemoryLog::new(), MemoryStateMachine::new())?;
//! assert_eq!(node.role(), Role::Follower);
//!
//! for _ in 0..64 {
//!     node.tick()?;
//! }
//! assert_eq!(node.role(), Role::Leader);
//!
//! let index = node.propose(b"set gain 3".to_vec())?;
//! assert!(node.commit_index() >= index);
//! assert_eq!(node.state_machine().commands(), &[b"set gain 3".to_vec()]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod apply;
pub mod election;
pub mod replication;

use std::collections::BTreeMap;

use crate::config::RaftConfig;
use crate::error::{RaftError, Result};
use crate::log::entry::{EntryPayload, HardState, LogEntry};
use crate::log::store::LogStore;
use crate::membership::{Membership, MembershipChange};
use crate::message::{Envelope, RaftMessage};
use crate::state_machine::StateMachine;
use crate::types::{LogIndex, PeerId, Rng, Role, Term};

/// What a leader knows about one follower's log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// The next index to send this follower.
    pub next_index: LogIndex,
    /// The highest index known to be replicated on this follower.
    pub match_index: LogIndex,
    /// The tick at which this follower last answered.
    pub last_contact: u64,
    /// Whether a snapshot is currently in flight to this follower, which
    /// suppresses ordinary replication until it is acknowledged.
    pub snapshot_in_flight: bool,
}

impl Progress {
    /// Fresh progress for a follower whose log is assumed to match the
    /// leader's, which is Figure 2's optimistic initialization.
    #[must_use]
    pub const fn new(next_index: LogIndex, now: u64) -> Self {
        Self {
            next_index,
            match_index: LogIndex::ZERO,
            last_contact: now,
            snapshot_in_flight: false,
        }
    }
}

/// One committed entry, handed back after it was applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// Where the entry sat in the log.
    pub index: LogIndex,
    /// The term it was created in.
    pub term: Term,
    /// What the state machine returned, for a command entry; `None` for the
    /// leader's no-op and for configuration entries, which the state machine
    /// never sees.
    pub result: Option<Vec<u8>>,
}

/// A snapshot of a replica's consensus state, for logging and for callers
/// that need to answer "who is the leader?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaftStatus {
    /// This replica's id.
    pub id: PeerId,
    /// What it currently is.
    pub role: Role,
    /// Its current term.
    pub term: Term,
    /// The leader it last heard from, if any.
    pub leader: Option<PeerId>,
    /// The highest index known committed.
    pub commit_index: LogIndex,
    /// The highest index applied to the state machine.
    pub last_applied: LogIndex,
    /// The last index in the local log.
    pub last_index: LogIndex,
    /// The configuration currently in force.
    pub membership: Membership,
    /// Whether this replica holds a valid leader lease right now.
    pub has_lease: bool,
    /// Whether this replica is a leader that has **committed an entry of its
    /// own term** and may therefore be trusted for reads and writes.
    ///
    /// See [`RaftNode::is_ready`] for why `role == Leader` is not enough.
    pub leader_ready: bool,
}

/// One Raft replica.
pub struct RaftNode<M: StateMachine, L: LogStore> {
    /// Identity, peers and timing.
    config: RaftConfig,
    /// What this replica currently is.
    role: Role,
    /// The persisted term and vote, mirrored from [`RaftNode::log`].
    hard: HardState,
    /// The leader this replica last accepted an append from.
    leader: Option<PeerId>,
    /// Durable log and hard state.
    log: L,
    /// The application being replicated.
    machine: M,
    /// The highest index known to be committed.
    commit_index: LogIndex,
    /// The highest index handed to the state machine.
    last_applied: LogIndex,
    /// The configuration in force (applied on append, not on commit).
    membership: Membership,
    /// Per-follower replication state; leader-only, empty otherwise.
    progress: BTreeMap<PeerId, Progress>,
    /// Votes received in the current campaign, `true` for granted.
    votes: BTreeMap<PeerId, bool>,
    /// Ticks since the last message from a leader (or the last campaign).
    election_elapsed: u64,
    /// Ticks since the last heartbeat this leader sent.
    heartbeat_elapsed: u64,
    /// The randomized election timeout currently in force.
    randomized_timeout: u64,
    /// This replica's seeded randomness.
    rng: Rng,
    /// Messages waiting to be drained by the caller.
    outbox: Vec<Envelope>,
    /// Applied entries waiting to be drained by the caller.
    applied: Vec<Applied>,
    /// The index of the configuration entry that is appended but not yet
    /// committed, if there is one.
    pending_config: Option<LogIndex>,
    /// A monotonic tick counter, the only "time" this type knows.
    ticks: u64,
}

impl<M: StateMachine, L: LogStore> RaftNode<M, L> {
    /// Builds a replica over `log` and `machine`, recovering whatever the
    /// log already holds.
    ///
    /// The state machine is restored from the log's snapshot when there is
    /// one, and the configuration is recomputed by replaying every
    /// configuration entry still held — which is what makes a restart, and a
    /// log truncation, arrive at the same membership.
    ///
    /// # Errors
    ///
    /// - [`RaftError::InvalidConfig`] if `config` cannot work; see
    ///   [`RaftConfig::validate`].
    /// - [`RaftError::Snapshot`] if the state machine refuses the stored
    ///   snapshot.
    /// - Whatever the log store reports while being read.
    pub fn new(config: RaftConfig, log: L, machine: M) -> Result<Self> {
        config.validate()?;
        let hard = log.hard_state();
        let mut rng = Rng::new(config.seed);
        let randomized_timeout = config.election_timeout_ticks
            + rng.in_range(0, config.election_jitter_ticks.saturating_add(1));

        let mut node = Self {
            role: Role::Follower,
            hard,
            leader: None,
            commit_index: LogIndex::ZERO,
            last_applied: LogIndex::ZERO,
            membership: config.initial_membership(),
            progress: BTreeMap::new(),
            votes: BTreeMap::new(),
            election_elapsed: 0,
            heartbeat_elapsed: 0,
            randomized_timeout,
            rng,
            outbox: Vec::new(),
            applied: Vec::new(),
            pending_config: None,
            ticks: 0,
            config,
            log,
            machine,
        };

        // A snapshot is, by construction, entirely committed and entirely
        // applied: it *is* the state machine as of its last index.
        if let Some(snapshot) = node.log.snapshot()? {
            node.machine
                .restore(&snapshot.data)
                .map_err(|reason| RaftError::Snapshot {
                    what: "restore",
                    reason,
                })?;
            node.commit_index = snapshot.meta.last_index();
            node.last_applied = snapshot.meta.last_index();
            node.membership = snapshot.meta.membership().clone();
        }
        // A durable state machine may already be ahead of the snapshot. Raft
        // does not persist `commitIndex`, so without this a restart would
        // re-apply every entry after the snapshot a second time — see
        // [`StateMachine::applied_index`] for the two ways an implementation
        // may answer.
        let durably_applied = node.machine.applied_index();
        if durably_applied > node.last_applied {
            node.last_applied = durably_applied;
            node.commit_index = node.commit_index.max(durably_applied);
        }
        node.recompute_membership()?;
        Ok(node)
    }

    /// This replica's id.
    #[must_use]
    pub const fn id(&self) -> PeerId {
        self.config.id
    }

    /// The configuration this replica was built with.
    #[must_use]
    pub const fn config(&self) -> &RaftConfig {
        &self.config
    }

    /// What this replica currently is.
    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    /// Whether this replica is the leader.
    #[must_use]
    pub const fn is_leader(&self) -> bool {
        self.role.is_leader()
    }

    /// This replica's current term.
    #[must_use]
    pub const fn term(&self) -> Term {
        self.hard.term
    }

    /// The leader this replica last heard from.
    #[must_use]
    pub const fn leader(&self) -> Option<PeerId> {
        self.leader
    }

    /// The highest index known committed.
    #[must_use]
    pub const fn commit_index(&self) -> LogIndex {
        self.commit_index
    }

    /// The highest index applied to the state machine.
    #[must_use]
    pub const fn last_applied(&self) -> LogIndex {
        self.last_applied
    }

    /// The configuration in force.
    #[must_use]
    pub const fn membership(&self) -> &Membership {
        &self.membership
    }

    /// The state machine, for reads served locally.
    #[must_use]
    pub const fn state_machine(&self) -> &M {
        &self.machine
    }

    /// The state machine, mutably — for a caller that owns state outside
    /// Raft's own commands (a cache, a metrics handle).
    pub const fn state_machine_mut(&mut self) -> &mut M {
        &mut self.machine
    }

    /// The durable log, for inspection.
    #[must_use]
    pub const fn log(&self) -> &L {
        &self.log
    }

    /// The monotonic tick counter.
    #[must_use]
    pub const fn ticks(&self) -> u64 {
        self.ticks
    }

    /// A snapshot of this replica's consensus state.
    #[must_use]
    pub fn status(&self) -> RaftStatus {
        RaftStatus {
            id: self.config.id,
            role: self.role,
            term: self.hard.term,
            leader: self.leader,
            commit_index: self.commit_index,
            last_applied: self.last_applied,
            last_index: self.log.last_index(),
            membership: self.membership.clone(),
            has_lease: self.has_lease(),
            leader_ready: self.is_ready(),
        }
    }

    /// Whether this replica is a leader whose view of the state machine is
    /// **up to date**, and which may therefore serve reads and accept writes
    /// derived from what it can currently see.
    ///
    /// `role == Leader` is *not* enough, and the difference is a real bug
    /// class. A peer becomes leader the instant it wins the vote, before it
    /// has replicated or applied anything. Its state machine at that moment
    /// may be missing entries the previous leader had already committed —
    /// Raft guarantees those entries are *in its log*, not that they have
    /// been applied. A caller that read at that instant, or that computed a
    /// new value from what it read (`revision + 1`, say), would be working
    /// from pre-failover state.
    ///
    /// The fix is the no-op every leader appends on taking office: once an
    /// entry of the leader's **own term** is committed, every entry before it
    /// is committed too, and the apply loop has run through all of them. That
    /// is exactly the condition tested here.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_raft::{MemoryLog, MemoryStateMachine, PeerId, RaftConfig, RaftNode};
    ///
    /// let config = RaftConfig::new(PeerId::new(1))
    ///     .with_peer(PeerId::new(1), "127.0.0.1:7501".parse()?);
    /// let mut node = RaftNode::new(config, MemoryLog::new(), MemoryStateMachine::new())?;
    /// assert!(!node.is_ready(), "a follower is never ready to serve");
    /// for _ in 0..64 {
    ///     node.tick()?;
    /// }
    /// assert!(node.is_ready());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn is_ready(&self) -> bool {
        if !self.role.is_leader() {
            return false;
        }
        // `term_at` reads the log, which cannot fail for an index this
        // replica just committed; a read error is reported as "not ready",
        // which is the safe direction.
        matches!(self.log.term_at(self.commit_index), Ok(Some(term)) if term == self.hard.term)
    }

    /// Takes every message produced since the last drain.
    ///
    /// The caller is responsible for delivering them — best effort is
    /// enough. Raft assumes an unreliable network: a dropped message is
    /// retried by the next heartbeat, and a duplicated one is idempotent.
    pub fn take_messages(&mut self) -> Vec<Envelope> {
        std::mem::take(&mut self.outbox)
    }

    /// Takes every entry applied since the last drain.
    pub fn take_applied(&mut self) -> Vec<Applied> {
        std::mem::take(&mut self.applied)
    }

    /// Advances this replica by one tick.
    ///
    /// # Errors
    ///
    /// Whatever the log store or the state machine reports.
    pub fn tick(&mut self) -> Result<()> {
        self.ticks = self.ticks.saturating_add(1);
        if self.role.is_leader() {
            self.tick_leader()
        } else {
            self.tick_follower()
        }
    }

    /// A leader's tick: heartbeats, and a step-down if quorum contact is
    /// lost.
    fn tick_leader(&mut self) -> Result<()> {
        self.heartbeat_elapsed = self.heartbeat_elapsed.saturating_add(1);
        if self.heartbeat_elapsed >= self.config.heartbeat_ticks {
            self.heartbeat_elapsed = 0;
            self.broadcast_append()?;
        }
        // A leader that cannot reach a quorum must stop being one, or it
        // would keep serving lease reads from a log the rest of the cluster
        // has already moved past. This is the same bound the lease uses, so
        // a leader's lease can never outlive its ability to renew it.
        self.election_elapsed = self.election_elapsed.saturating_add(1);
        if self.election_elapsed >= self.config.election_timeout_ticks
            && !self.has_quorum_contact(self.config.election_timeout_ticks)
        {
            tracing::warn!(
                peer = self.config.id.get(),
                term = self.hard.term.get(),
                "stepping down: no quorum contact within the election timeout"
            );
            self.become_follower(self.hard.term, None)?;
        }
        Ok(())
    }

    /// A follower's or candidate's tick: campaign when the timeout elapses.
    fn tick_follower(&mut self) -> Result<()> {
        self.election_elapsed = self.election_elapsed.saturating_add(1);
        if self.election_elapsed < self.randomized_timeout {
            return Ok(());
        }
        // A peer that is not a voter (one being removed, or one that has not
        // yet been added) must never campaign: it cannot win, and its
        // `RequestVote` would only disturb a working leader.
        if !self.membership.contains(self.config.id) {
            self.election_elapsed = 0;
            return Ok(());
        }
        if self.config.pre_vote {
            self.campaign_pre_vote()
        } else {
            self.campaign()
        }
    }

    /// Feeds one inbound message to this replica.
    ///
    /// # Errors
    ///
    /// Whatever the log store or the state machine reports.
    pub fn step(&mut self, envelope: Envelope) -> Result<()> {
        if envelope.to != self.config.id {
            // Misrouted: dropping is the only safe answer, and saying so
            // beats silently acting on another peer's mail.
            tracing::debug!(
                peer = self.config.id.get(),
                addressed_to = envelope.to.get(),
                "dropping a misrouted Raft message"
            );
            return Ok(());
        }

        let incoming = envelope.message.term();
        if self.hard.term.is_stale_against(incoming) {
            self.handle_higher_term(&envelope, incoming)?;
        } else if self.hard.term.rejects(incoming) {
            return self.reject_stale(&envelope);
        }

        match &envelope.message {
            RaftMessage::PreVoteRequest { .. } | RaftMessage::VoteRequest { .. } => {
                self.handle_vote_request(&envelope)
            }
            RaftMessage::PreVoteResponse { .. } | RaftMessage::VoteResponse { .. } => {
                self.handle_vote_response(&envelope)
            }
            RaftMessage::AppendEntries { .. } => self.handle_append_entries(&envelope),
            RaftMessage::AppendEntriesResponse { .. } => self.handle_append_response(&envelope),
            RaftMessage::InstallSnapshot { .. } => self.handle_install_snapshot(&envelope),
            RaftMessage::InstallSnapshotResponse { .. } => {
                self.handle_install_snapshot_response(&envelope)
            }
            RaftMessage::TimeoutNow { .. } => {
                // A deliberate hand-over: campaign immediately rather than
                // waiting out a timeout. Pre-vote is skipped because the
                // leader has already told us it is standing down.
                if self.membership.contains(self.config.id) {
                    self.campaign()?;
                }
                Ok(())
            }
        }
    }

    /// Adopts a strictly higher term, with the pre-vote exceptions.
    fn handle_higher_term(&mut self, envelope: &Envelope, incoming: Term) -> Result<()> {
        match &envelope.message {
            // A pre-vote *request* carries the sender's term **plus one**, a
            // term nobody is in. Adopting it would let any partitioned peer
            // inflate the whole cluster's term at will — which is the exact
            // disruption pre-vote exists to prevent.
            RaftMessage::PreVoteRequest { .. } => Ok(()),
            // A granted pre-vote response carries the responder's own term,
            // which by construction is not higher than the hypothetical one
            // asked about, so it can never step this peer down. A *rejected*
            // one may carry a genuinely higher term, and that is real news.
            RaftMessage::PreVoteResponse { granted: true, .. } => Ok(()),
            RaftMessage::AppendEntries { .. }
            | RaftMessage::InstallSnapshot { .. }
            | RaftMessage::TimeoutNow { .. } => self.become_follower(incoming, Some(envelope.from)),
            _ => self.become_follower(incoming, None),
        }
    }

    /// Answers (or drops) a message from an older term.
    fn reject_stale(&mut self, envelope: &Envelope) -> Result<()> {
        match &envelope.message {
            RaftMessage::PreVoteRequest { .. } => {
                self.send(
                    envelope.from,
                    RaftMessage::PreVoteResponse {
                        term: self.hard.term,
                        granted: false,
                    },
                );
            }
            RaftMessage::VoteRequest { .. } => {
                self.send(
                    envelope.from,
                    RaftMessage::VoteResponse {
                        term: self.hard.term,
                        granted: false,
                    },
                );
            }
            RaftMessage::AppendEntries { .. } | RaftMessage::InstallSnapshot { .. } => {
                // Telling a stale leader its term is old is what makes it
                // step down promptly instead of retrying until it times out.
                self.send(
                    envelope.from,
                    RaftMessage::AppendEntriesResponse {
                        term: self.hard.term,
                        success: false,
                        match_index: LogIndex::ZERO,
                        conflict_index: LogIndex::ZERO,
                        conflict_term: Term::INITIAL,
                    },
                );
            }
            // A response from an old term is stale information about a
            // question nobody is asking any more.
            _ => {}
        }
        Ok(())
    }

    /// Proposes one state-machine command.
    ///
    /// Returns the log index the command was appended at. The command is
    /// *not* committed yet; the caller learns that from
    /// [`RaftNode::take_applied`] or by watching [`RaftNode::commit_index`].
    ///
    /// # Errors
    ///
    /// [`RaftError::NotLeader`] on a non-leader, carrying the leader hint
    /// when this replica knows one.
    pub fn propose(&mut self, command: Vec<u8>) -> Result<LogIndex> {
        self.append_local(EntryPayload::Command(command))
    }

    /// Proposes a single-server membership change.
    ///
    /// The new configuration takes effect on this replica the moment the
    /// entry is appended (see [`crate::membership`]).
    ///
    /// # Errors
    ///
    /// - [`RaftError::NotLeader`] on a non-leader.
    /// - [`RaftError::ConfigChangeInFlight`] if an earlier change is
    ///   appended but not yet committed.
    /// - [`RaftError::InvalidMembership`] if the change is a no-op or would
    ///   empty the cluster.
    pub fn propose_membership(&mut self, change: MembershipChange) -> Result<LogIndex> {
        if !self.role.is_leader() {
            return Err(RaftError::NotLeader {
                leader: self.leader,
            });
        }
        // At most one change may be in flight: two overlapping single-server
        // changes can produce configurations whose majorities do not overlap,
        // which is precisely the hazard single-server changes exist to avoid.
        if let Some(pending) = self.pending_config
            && pending > self.commit_index
        {
            return Err(RaftError::ConfigChangeInFlight { pending });
        }
        // A leader must have committed at least one entry of its own term
        // before it can safely change the configuration, or it might be
        // acting on a membership a later leader will overwrite.
        let next = self.membership.apply(change)?;
        let index = self.append_local(EntryPayload::Config(next))?;
        self.pending_config = Some(index);
        Ok(index)
    }

    /// Appends one entry of this leader's own term and starts replicating
    /// it.
    fn append_local(&mut self, payload: EntryPayload) -> Result<LogIndex> {
        if !self.role.is_leader() {
            return Err(RaftError::NotLeader {
                leader: self.leader,
            });
        }
        let index = self.log.last_index().next();
        let entry = LogEntry::new(self.hard.term, index, payload);
        if let Some(membership) = entry.config() {
            self.membership = membership.clone();
            self.reset_progress();
        }
        self.log.append(std::slice::from_ref(&entry))?;
        if let Some(progress) = self.progress.get_mut(&self.config.id) {
            progress.match_index = index;
            progress.next_index = index.next();
        }
        self.broadcast_append()?;
        self.maybe_advance_commit()?;
        self.apply_committed()?;
        Ok(index)
    }

    /// Queues one message for `to`.
    pub(crate) fn send(&mut self, to: PeerId, message: RaftMessage) {
        self.outbox.push(Envelope::new(self.config.id, to, message));
    }

    /// Every voter except this replica.
    pub(crate) fn other_voters(&self) -> Vec<PeerId> {
        self.membership
            .voters()
            .iter()
            .copied()
            .filter(|peer| *peer != self.config.id)
            .collect()
    }

    /// Persists a new term and vote.
    pub(crate) fn save_hard_state(&mut self, term: Term, voted_for: Option<PeerId>) -> Result<()> {
        let state = HardState::new(term, voted_for);
        if state == self.hard {
            return Ok(());
        }
        self.log.save_hard_state(state)?;
        self.hard = state;
        Ok(())
    }

    /// Steps down (or stays) as a follower in `term`.
    pub(crate) fn become_follower(&mut self, term: Term, leader: Option<PeerId>) -> Result<()> {
        let changing_term = term != self.hard.term;
        if changing_term {
            // A new term means a fresh vote: the old one belongs to a term
            // that is over.
            self.save_hard_state(term, None)?;
        }
        self.role = Role::Follower;
        self.leader = leader;
        self.progress.clear();
        self.votes.clear();
        self.reset_election_timer();
        Ok(())
    }

    /// Redraws the randomized election timeout and restarts it.
    pub(crate) fn reset_election_timer(&mut self) {
        self.election_elapsed = 0;
        self.randomized_timeout = self.config.election_timeout_ticks
            + self
                .rng
                .in_range(0, self.config.election_jitter_ticks.saturating_add(1));
    }

    /// Rebuilds [`RaftNode::membership`] by replaying the configuration
    /// entries still held, on top of the snapshot's (or the bootstrap)
    /// configuration.
    ///
    /// Called after a truncation, because a configuration entry that was
    /// appended — and therefore already in force — may have belonged to a
    /// term that lost.
    pub(crate) fn recompute_membership(&mut self) -> Result<()> {
        let base = match self.log.snapshot_meta() {
            Some(meta) => meta.membership().clone(),
            None => self.config.initial_membership(),
        };
        let first = self.log.first_index();
        let count = first.distance_to(self.log.last_index()).saturating_add(1);
        let held = self
            .log
            .entries(first, usize::try_from(count).unwrap_or(usize::MAX))?;
        self.membership = Membership::from_entries(&base, held.iter());
        // A configuration entry past the commit index is still in flight.
        self.pending_config = held
            .iter()
            .rev()
            .find(|entry| entry.is_config())
            .map(LogEntry::index)
            .filter(|index| *index > self.commit_index);
        Ok(())
    }

    /// Whether a quorum of voters has answered within `within` ticks.
    #[must_use]
    pub(crate) fn has_quorum_contact(&self, within: u64) -> bool {
        let cutoff = self.ticks.saturating_sub(within);
        let mut fresh = 0usize;
        for voter in self.membership.voters() {
            if *voter == self.config.id {
                fresh += 1;
                continue;
            }
            if self
                .progress
                .get(voter)
                .is_some_and(|progress| progress.last_contact >= cutoff)
            {
                fresh += 1;
            }
        }
        self.membership.is_quorum(fresh)
    }

    /// Whether this replica may serve a linearizable read from its own state
    /// machine right now.
    ///
    /// **Leader lease, not `ReadIndex`.** A read is safe without a round trip
    /// as long as no other leader can exist, and no other leader can exist
    /// until some follower's election timeout elapses. So: a leader that
    /// heard from a quorum of voters within the last
    /// [`RaftConfig::lease_ticks`] ticks — which is the election-timeout
    /// **floor**, the value an adversarial follower would use — knows no
    /// election can have completed since, and reads locally. The alternative,
    /// `ReadIndex`, pays a heartbeat round trip per read and in exchange
    /// tolerates unbounded clock drift between replicas; this crate's ticks
    /// come from one process's own monotonic timer per replica, and the
    /// assumption it makes explicit is that those timers do not run at wildly
    /// different rates.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_raft::{MemoryLog, MemoryStateMachine, PeerId, RaftConfig, RaftNode};
    ///
    /// let config = RaftConfig::new(PeerId::new(1))
    ///     .with_peer(PeerId::new(1), "127.0.0.1:7501".parse()?);
    /// let mut node = RaftNode::new(config, MemoryLog::new(), MemoryStateMachine::new())?;
    /// for _ in 0..64 {
    ///     node.tick()?;
    /// }
    /// // A cluster of one is its own quorum, so its lease never lapses.
    /// assert!(node.has_lease());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn has_lease(&self) -> bool {
        self.role.is_leader() && self.has_quorum_contact(self.config.lease_ticks())
    }

    /// Records that `peer` answered at the current tick.
    pub(crate) fn note_contact(&mut self, peer: PeerId) {
        let now = self.ticks;
        if let Some(progress) = self.progress.get_mut(&peer) {
            progress.last_contact = now;
        }
    }

    /// Rebuilds [`RaftNode::progress`] for the configuration in force,
    /// keeping what is already known about peers that remain.
    pub(crate) fn reset_progress(&mut self) {
        let next_index = self.log.last_index().next();
        let now = self.ticks;
        let mut rebuilt = BTreeMap::new();
        for voter in self.membership.voters() {
            let existing = self.progress.get(voter).copied();
            rebuilt.insert(*voter, existing.unwrap_or(Progress::new(next_index, now)));
        }
        if let Some(own) = rebuilt.get_mut(&self.config.id) {
            own.match_index = self.log.last_index();
            own.next_index = next_index;
            own.last_contact = now;
        }
        self.progress = rebuilt;
    }

    /// This replica's view of one follower's replication progress.
    #[must_use]
    pub fn progress_of(&self, peer: PeerId) -> Option<Progress> {
        self.progress.get(&peer).copied()
    }

    /// The log store, mutably — for a caller bootstrapping a cluster, and
    /// for this crate's own tests.
    pub const fn log_mut(&mut self) -> &mut L {
        &mut self.log
    }

    /// Dismantles this replica into the log and state machine it was built
    /// over.
    ///
    /// What a restart looks like from the outside: the durable half is
    /// handed back so it can be handed to a fresh [`RaftNode::new`].
    #[must_use]
    pub fn into_parts(self) -> (L, M) {
        (self.log, self.machine)
    }
}

#[cfg(test)]
pub(crate) mod harness {
    //! Shared fixtures for the core's own unit tests.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::log::store::MemoryLog;
    use crate::state_machine::MemoryStateMachine;

    /// A three-peer configuration with deterministic, well-separated
    /// election timeouts.
    pub(crate) fn config(id: u64) -> RaftConfig {
        let mut config = RaftConfig::new(PeerId::new(id));
        for peer in 1..=3u64 {
            config = config.with_peer(
                PeerId::new(peer),
                std::net::SocketAddr::from(([127, 0, 0, 1], 7500 + peer as u16)),
            );
        }
        config.with_election_timeout(10, 5).with_heartbeat_ticks(2)
    }

    /// A follower in a three-peer cluster.
    pub(crate) fn node(id: u64) -> RaftNode<MemoryStateMachine, MemoryLog> {
        RaftNode::new(config(id), MemoryLog::new(), MemoryStateMachine::new()).unwrap()
    }

    /// A node that has been elected leader of its three-peer cluster,
    /// with both followers acknowledging.
    pub(crate) fn leader(id: u64) -> RaftNode<MemoryStateMachine, MemoryLog> {
        let mut node = node(id);
        for _ in 0..32 {
            node.tick().unwrap();
            if node.role() == Role::Leader {
                break;
            }
            grant_all(&mut node);
        }
        assert_eq!(node.role(), Role::Leader, "the fixture must elect a leader");
        // Acknowledge the no-op so the fixture hands back a leader whose own
        // term is already committed — the state every rule below assumes.
        acknowledge_all(&mut node);
        node
    }

    /// Grants every outstanding vote (pre-vote or real) from both peers.
    pub(crate) fn grant_all(node: &mut RaftNode<MemoryStateMachine, MemoryLog>) {
        for envelope in node.take_messages() {
            let reply = match envelope.message {
                RaftMessage::PreVoteRequest { .. } => Some(RaftMessage::PreVoteResponse {
                    term: node.term(),
                    granted: true,
                }),
                RaftMessage::VoteRequest { term, .. } => Some(RaftMessage::VoteResponse {
                    term,
                    granted: true,
                }),
                _ => None,
            };
            if let Some(message) = reply {
                node.step(Envelope::new(envelope.to, envelope.from, message))
                    .unwrap();
            }
        }
    }

    /// Answers every outstanding `AppendEntries` with a success carrying the
    /// index the leader actually sent.
    pub(crate) fn acknowledge_all(node: &mut RaftNode<MemoryStateMachine, MemoryLog>) {
        for envelope in node.take_messages() {
            if let RaftMessage::AppendEntries {
                term,
                prev_log_index,
                entries,
                ..
            } = envelope.message
            {
                let match_index = prev_log_index.saturating_add(entries.len() as u64);
                node.step(Envelope::new(
                    envelope.to,
                    envelope.from,
                    RaftMessage::AppendEntriesResponse {
                        term,
                        success: true,
                        match_index,
                        conflict_index: LogIndex::ZERO,
                        conflict_term: Term::INITIAL,
                    },
                ))
                .unwrap();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::harness::*;
    use super::*;
    use crate::log::store::MemoryLog;
    use crate::state_machine::MemoryStateMachine;

    #[test]
    fn a_fresh_replica_is_a_follower_at_term_zero() {
        let node = node(1);
        assert_eq!(node.role(), Role::Follower);
        assert_eq!(node.term(), Term::INITIAL);
        assert_eq!(node.commit_index(), LogIndex::ZERO);
        assert_eq!(node.membership().len(), 3);
        assert!(!node.has_lease());
        assert_eq!(node.id(), PeerId::new(1));
    }

    #[test]
    fn a_misrouted_message_is_dropped_without_effect() {
        let mut node = node(1);
        node.step(Envelope::new(
            PeerId::new(2),
            PeerId::new(3),
            RaftMessage::VoteRequest {
                term: Term::new(9),
                last_log_index: LogIndex::ZERO,
                last_log_term: Term::INITIAL,
            },
        ))
        .unwrap();
        assert_eq!(
            node.term(),
            Term::INITIAL,
            "another peer's mail is not ours"
        );
        assert!(node.take_messages().is_empty());
    }

    #[test]
    fn a_higher_term_on_an_append_makes_this_peer_a_follower_of_its_sender() {
        let mut node = leader(1);
        node.step(Envelope::new(
            PeerId::new(2),
            PeerId::new(1),
            RaftMessage::AppendEntries {
                term: Term::new(99),
                prev_log_index: LogIndex::ZERO,
                prev_log_term: Term::INITIAL,
                entries: Vec::new(),
                leader_commit: LogIndex::ZERO,
            },
        ))
        .unwrap();
        assert_eq!(node.role(), Role::Follower);
        assert_eq!(node.term(), Term::new(99));
        assert_eq!(node.leader(), Some(PeerId::new(2)));
    }

    #[test]
    fn a_higher_term_on_a_vote_request_does_not_name_a_leader() {
        // A candidate is not a leader: recording it as one would let a CLI
        // be redirected to a peer that has not won anything.
        let mut node = node(1);
        node.step(Envelope::new(
            PeerId::new(2),
            PeerId::new(1),
            RaftMessage::VoteRequest {
                term: Term::new(4),
                last_log_index: LogIndex::ZERO,
                last_log_term: Term::INITIAL,
            },
        ))
        .unwrap();
        assert_eq!(node.term(), Term::new(4));
        assert_eq!(node.leader(), None);
    }

    #[test]
    fn a_stale_append_is_answered_with_this_peers_term() {
        let mut node = node(1);
        node.save_hard_state(Term::new(7), None).unwrap();
        node.step(Envelope::new(
            PeerId::new(2),
            PeerId::new(1),
            RaftMessage::AppendEntries {
                term: Term::new(3),
                prev_log_index: LogIndex::ZERO,
                prev_log_term: Term::INITIAL,
                entries: Vec::new(),
                leader_commit: LogIndex::ZERO,
            },
        ))
        .unwrap();
        let replies = node.take_messages();
        assert_eq!(replies.len(), 1);
        match replies[0].message {
            RaftMessage::AppendEntriesResponse {
                term,
                success: false,
                ..
            } => assert_eq!(term, Term::new(7)),
            ref other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn a_stale_response_is_dropped_silently() {
        let mut node = leader(1);
        node.save_hard_state(Term::new(20), Some(PeerId::new(1)))
            .unwrap();
        node.take_messages();
        node.step(Envelope::new(
            PeerId::new(2),
            PeerId::new(1),
            RaftMessage::VoteResponse {
                term: Term::new(3),
                granted: true,
            },
        ))
        .unwrap();
        assert!(node.take_messages().is_empty());
    }

    #[test]
    fn a_non_leader_refuses_proposals_with_a_leader_hint() {
        let mut node = node(1);
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
        let error = node.propose(b"x".to_vec()).unwrap_err();
        assert_eq!(error.leader_hint(), Some(PeerId::new(3)));
        assert!(error.is_redirectable());
    }

    #[test]
    fn a_single_peer_cluster_elects_itself_and_commits_immediately() {
        let config = RaftConfig::new(PeerId::new(1)).with_peer(
            PeerId::new(1),
            std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
        );
        let mut node = RaftNode::new(config, MemoryLog::new(), MemoryStateMachine::new()).unwrap();
        for _ in 0..64 {
            node.tick().unwrap();
        }
        assert_eq!(node.role(), Role::Leader);
        let index = node.propose(b"only".to_vec()).unwrap();
        assert!(node.commit_index() >= index);
        assert_eq!(node.state_machine().commands(), &[b"only".to_vec()]);
        assert!(node.has_lease());
    }

    #[test]
    fn a_leader_steps_down_when_it_cannot_reach_a_quorum() {
        let mut node = leader(1);
        assert!(node.has_lease());
        // Nobody answers from here on.
        for _ in 0..64 {
            node.tick().unwrap();
            node.take_messages();
        }
        assert!(
            !node.is_leader(),
            "a leader with no quorum contact must stop being one"
        );
        assert!(!node.has_lease());
    }

    #[test]
    fn a_lease_holds_while_a_quorum_keeps_answering() {
        let mut node = leader(1);
        for _ in 0..64 {
            node.tick().unwrap();
            acknowledge_all(&mut node);
        }
        assert_eq!(node.role(), Role::Leader);
        assert!(node.has_lease());
    }

    #[test]
    fn a_leader_is_not_ready_until_its_own_term_has_committed() {
        // The bug class this guards: a peer becomes leader the instant it
        // wins the vote, before it has applied anything, so a read at that
        // moment can see pre-failover state.
        let mut node = node(1);
        for _ in 0..32 {
            node.tick().unwrap();
            if node.role() == Role::Leader {
                break;
            }
            grant_all(&mut node);
        }
        assert_eq!(node.role(), Role::Leader);
        assert!(
            !node.is_ready(),
            "leadership alone must not make a replica readable"
        );
        assert!(
            node.has_lease(),
            "the lease is held immediately — which is exactly why it is not the readiness test"
        );

        acknowledge_all(&mut node);
        assert!(node.is_ready(), "the no-op committing makes it ready");
        assert!(node.status().leader_ready);
    }

    #[test]
    fn a_follower_is_never_ready() {
        let node = node(1);
        assert!(!node.is_ready());
        assert!(!node.status().leader_ready);
    }

    #[test]
    fn status_reports_the_whole_consensus_state() {
        let node = leader(1);
        let status = node.status();
        assert!(status.leader_ready);
        assert_eq!(status.id, PeerId::new(1));
        assert_eq!(status.role, Role::Leader);
        assert_eq!(status.leader, Some(PeerId::new(1)));
        assert_eq!(status.membership.len(), 3);
        assert!(status.has_lease);
        assert!(status.last_index >= status.commit_index);
    }

    #[test]
    fn timeout_now_starts_a_campaign_without_waiting() {
        let mut node = node(2);
        node.step(Envelope::new(
            PeerId::new(1),
            PeerId::new(2),
            RaftMessage::TimeoutNow { term: Term::new(1) },
        ))
        .unwrap();
        assert_eq!(node.role(), Role::Candidate, "a hand-over skips pre-vote");
        assert_eq!(node.term(), Term::new(2));
    }

    #[test]
    fn a_replica_outside_the_configuration_never_campaigns() {
        // A peer being removed must not disturb the cluster it is leaving.
        let mut node = node(1);
        node.membership = Membership::new([PeerId::new(2), PeerId::new(3)]);
        for _ in 0..64 {
            node.tick().unwrap();
        }
        assert_eq!(node.role(), Role::Follower);
        assert_eq!(node.term(), Term::INITIAL);
        assert!(node.take_messages().is_empty());
    }
}
