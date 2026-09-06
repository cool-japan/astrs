//! [`RaftReplica`] and [`RaftHandle`]: running a [`crate::RaftNode`] under
//! `tokio`.
//!
//! # One task owns the node
//!
//! [`crate::RaftNode`] is deliberately not `Sync`-shared: every rule it
//! implements is a read-modify-write of the same state, and a lock around it
//! would be a lock around the entire consensus algorithm. Instead one task
//! owns it outright, and everyone else talks to that task over channels.
//! [`RaftHandle`] is that conversation, and it is cheap to clone.
//!
//! # What a proposal actually waits for
//!
//! [`RaftHandle::propose`] resolves when the command has been **applied**, not
//! merely committed, and it resolves with whatever the state machine returned.
//! That is the only guarantee a caller can act on: a reply saying "your write
//! is durable" is worth nothing if a read on the same replica cannot see it
//! yet.
//!
//! A proposal can also *fail after being accepted*: if this replica loses
//! leadership before the entry commits, the entry may or may not survive into
//! the next term. That is reported as [`crate::RaftError::ProposalLost`],
//! which is an honest "unknown", not a "no" — the caller must re-read rather
//! than blindly retry.
//!
//! # Examples
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
//! use astrs_raft::{
//!     ChannelTransport, MemoryLog, MemoryStateMachine, PeerId, RaftConfig, RaftReplica,
//! };
//!
//! let config = RaftConfig::new(PeerId::new(1))
//!     .with_peer(PeerId::new(1), "127.0.0.1:7501".parse()?)
//!     .with_tick_interval(std::time::Duration::from_millis(2));
//! let (transport, mut inboxes) = ChannelTransport::mesh([PeerId::new(1)]);
//! let inbound = inboxes.remove(&PeerId::new(1)).expect("its own inbox");
//!
//! let replica = RaftReplica::new(
//!     config,
//!     MemoryLog::new(),
//!     MemoryStateMachine::new(),
//!     transport,
//!     inbound,
//! )?;
//! let handle = replica.handle();
//! let running = tokio::spawn(replica.run());
//!
//! // A cluster of one elects itself within a few ticks.
//! handle.wait_for(|status| status.role.is_leader()).await?;
//! let applied = handle.propose(b"hello".to_vec()).await?;
//! assert_eq!(applied, b"hello".to_vec());
//! handle.shutdown();
//! running.await??;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! # })
//! # }
//! ```

use std::collections::BTreeMap;

use tokio::sync::{mpsc, oneshot, watch};

use crate::config::RaftConfig;
use crate::core::{RaftNode, RaftStatus};
use crate::error::{RaftError, Result};
use crate::log::store::LogStore;
use crate::membership::MembershipChange;
use crate::message::Envelope;
use crate::state_machine::StateMachine;
use crate::transport::Transport;
use crate::types::{LogIndex, PeerId, Role, Term};

/// One request from a [`RaftHandle`] to the task that owns the node.
enum Command {
    /// Append a state-machine command and report what applying it returned.
    Propose {
        /// The opaque command bytes.
        command: Vec<u8>,
        /// Where to send the outcome.
        responder: oneshot::Sender<Result<Vec<u8>>>,
    },
    /// Add or remove one voter.
    Membership {
        /// The change to make.
        change: MembershipChange,
        /// Where to send the outcome.
        responder: oneshot::Sender<Result<()>>,
    },
    /// Compact the log now rather than at the configured threshold.
    Snapshot {
        /// Where to send the outcome.
        responder: oneshot::Sender<Result<()>>,
    },
    /// Stop the replica's task.
    Shutdown,
}

/// A cheap, cloneable handle to a running replica.
#[derive(Debug, Clone)]
pub struct RaftHandle {
    /// The command channel into the replica's task.
    commands: mpsc::UnboundedSender<Command>,
    /// The replica's latest status, updated after every step.
    status: watch::Receiver<RaftStatus>,
}

impl RaftHandle {
    /// Proposes one state-machine command and waits for it to be applied.
    ///
    /// # Errors
    ///
    /// - [`RaftError::NotLeader`] if this replica is not the leader, carrying
    ///   the leader hint when it knows one.
    /// - [`RaftError::ProposalLost`] if leadership changed before the entry
    ///   committed. The command may or may not have taken effect; re-read.
    /// - [`RaftError::ShuttingDown`] if the replica's task has stopped.
    pub async fn propose(&self, command: Vec<u8>) -> Result<Vec<u8>> {
        let (responder, answer) = oneshot::channel();
        self.commands
            .send(Command::Propose { command, responder })
            .map_err(|_| RaftError::ShuttingDown)?;
        answer.await.map_err(|_| RaftError::ShuttingDown)?
    }

    /// Adds or removes one voter and waits for the change to commit.
    ///
    /// # Errors
    ///
    /// As [`RaftHandle::propose`], plus
    /// [`RaftError::ConfigChangeInFlight`] and
    /// [`RaftError::InvalidMembership`].
    pub async fn change_membership(&self, change: MembershipChange) -> Result<()> {
        let (responder, answer) = oneshot::channel();
        self.commands
            .send(Command::Membership { change, responder })
            .map_err(|_| RaftError::ShuttingDown)?;
        answer.await.map_err(|_| RaftError::ShuttingDown)?
    }

    /// Compacts the log into a snapshot now.
    ///
    /// # Errors
    ///
    /// As [`crate::RaftNode::take_snapshot`], or
    /// [`RaftError::ShuttingDown`].
    pub async fn snapshot(&self) -> Result<()> {
        let (responder, answer) = oneshot::channel();
        self.commands
            .send(Command::Snapshot { responder })
            .map_err(|_| RaftError::ShuttingDown)?;
        answer.await.map_err(|_| RaftError::ShuttingDown)?
    }

    /// This replica's latest consensus status.
    ///
    /// Read from a `watch` channel, so it costs no round trip and never
    /// blocks — which is what makes it usable on the hot path of a server
    /// deciding whether to serve a read locally.
    #[must_use]
    pub fn status(&self) -> RaftStatus {
        self.status.borrow().clone()
    }

    /// Whether this replica is currently the leader.
    #[must_use]
    pub fn is_leader(&self) -> bool {
        self.status.borrow().role.is_leader()
    }

    /// Whether this replica holds a leader lease and may serve a read from
    /// its own state machine.
    ///
    /// A lease alone is **not** sufficient to serve a read: a freshly elected
    /// leader holds one immediately (a majority just voted for it) while its
    /// state machine may still be missing committed entries. Gate reads on
    /// [`RaftHandle::is_ready`], which adds the missing condition.
    #[must_use]
    pub fn has_lease(&self) -> bool {
        self.status.borrow().has_lease
    }

    /// Whether this replica is a leader that has committed an entry of its
    /// own term, and may therefore serve reads and accept writes.
    ///
    /// This is the predicate a server should gate on, not
    /// [`RaftHandle::is_leader`]. See [`crate::RaftNode::is_ready`].
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status.borrow().leader_ready
    }

    /// The leader this replica last heard from.
    #[must_use]
    pub fn leader(&self) -> Option<PeerId> {
        self.status.borrow().leader
    }

    /// Waits until this replica's status satisfies `predicate`.
    ///
    /// # Errors
    ///
    /// [`RaftError::ShuttingDown`] if the replica stops first.
    pub async fn wait_for(
        &self,
        mut predicate: impl FnMut(&RaftStatus) -> bool,
    ) -> Result<RaftStatus> {
        let mut status = self.status.clone();
        loop {
            {
                let current = status.borrow_and_update();
                if predicate(&current) {
                    return Ok(current.clone());
                }
            }
            status
                .changed()
                .await
                .map_err(|_| RaftError::ShuttingDown)?;
        }
    }

    /// Asks the replica's task to stop.
    pub fn shutdown(&self) {
        let _ = self.commands.send(Command::Shutdown);
    }
}

/// A proposal waiting for its entry to be applied.
struct Pending {
    /// The term the entry was appended in; a change means it was lost.
    term: Term,
    /// Where to send the outcome.
    responder: oneshot::Sender<Result<Vec<u8>>>,
}

/// A [`RaftNode`] plus everything needed to run it: a transport, an inbound
/// stream and a tick timer.
pub struct RaftReplica<M: StateMachine, L: LogStore, T: Transport> {
    /// The consensus state machine.
    node: RaftNode<M, L>,
    /// Where outbound messages go.
    transport: T,
    /// Where inbound messages come from.
    inbound: mpsc::UnboundedReceiver<Envelope>,
    /// Requests from handles.
    commands: mpsc::UnboundedReceiver<Command>,
    /// The sender half, cloned into every handle.
    command_sender: mpsc::UnboundedSender<Command>,
    /// Published status.
    status: watch::Sender<RaftStatus>,
    /// Proposals waiting on their entry, by log index.
    pending: BTreeMap<LogIndex, Pending>,
    /// Membership changes waiting on their entry, by log index.
    pending_membership: BTreeMap<LogIndex, oneshot::Sender<Result<()>>>,
}

impl<M: StateMachine, L: LogStore, T: Transport> RaftReplica<M, L, T> {
    /// Builds a replica ready to be `run`.
    ///
    /// # Errors
    ///
    /// Whatever [`RaftNode::new`] reports.
    pub fn new(
        config: RaftConfig,
        log: L,
        machine: M,
        transport: T,
        inbound: mpsc::UnboundedReceiver<Envelope>,
    ) -> Result<Self> {
        let node = RaftNode::new(config, log, machine)?;
        let (command_sender, commands) = mpsc::unbounded_channel();
        let (status, _) = watch::channel(node.status());
        Ok(Self {
            node,
            transport,
            inbound,
            commands,
            command_sender,
            status,
            pending: BTreeMap::new(),
            pending_membership: BTreeMap::new(),
        })
    }

    /// A handle to this replica. Clone it freely; it is the only way in.
    #[must_use]
    pub fn handle(&self) -> RaftHandle {
        RaftHandle {
            commands: self.command_sender.clone(),
            status: self.status.subscribe(),
        }
    }

    /// Runs until [`RaftHandle::shutdown`] or until every handle is dropped.
    ///
    /// # Errors
    ///
    /// Whatever the node, its log store or its state machine reports. A
    /// replica that cannot write its log must stop, not carry on.
    pub async fn run(mut self) -> Result<()> {
        let mut ticker = tokio::time::interval(self.node.config().tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => self.node.tick()?,
                received = self.inbound.recv() => match received {
                    Some(envelope) => self.node.step(envelope)?,
                    // Every sender is gone; nothing can reach this replica.
                    None => return self.finish(),
                },
                received = self.commands.recv() => match received {
                    Some(Command::Shutdown) | None => return self.finish(),
                    Some(command) => self.handle_command(command)?,
                },
            }
            self.flush()?;
        }
    }

    /// Applies one handle request.
    fn handle_command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Propose { command, responder } => {
                let term = self.node.term();
                match self.node.propose(command) {
                    Ok(index) => {
                        self.pending.insert(index, Pending { term, responder });
                    }
                    Err(error) => {
                        let _ = responder.send(Err(error));
                    }
                }
            }
            Command::Membership { change, responder } => {
                match self.node.propose_membership(change) {
                    Ok(index) => {
                        self.pending_membership.insert(index, responder);
                    }
                    Err(error) => {
                        let _ = responder.send(Err(error));
                    }
                }
            }
            Command::Snapshot { responder } => {
                let _ = responder.send(self.node.take_snapshot());
            }
            Command::Shutdown => {}
        }
        Ok(())
    }

    /// Ships outbound messages, resolves finished proposals, republishes
    /// status.
    fn flush(&mut self) -> Result<()> {
        for envelope in self.node.take_messages() {
            if let Err(error) = self.transport.send(envelope) {
                // A dropped message is a normal event in Raft's network
                // model; the next heartbeat retries.
                tracing::debug!(%error, "a Raft message could not be sent");
            }
        }

        for applied in self.node.take_applied() {
            if let Some(pending) = self.pending.remove(&applied.index) {
                let outcome = if pending.term == applied.term {
                    Ok(applied.result.unwrap_or_default())
                } else {
                    // The index committed, but with a *different* entry: this
                    // replica's proposal was overwritten by a later leader.
                    Err(RaftError::ProposalLost {
                        index: applied.index,
                        term: pending.term,
                    })
                };
                let _ = pending.responder.send(outcome);
            }
            if let Some(responder) = self.pending_membership.remove(&applied.index) {
                let _ = responder.send(Ok(()));
            }
        }

        // A replica that stopped leading cannot finish the proposals it
        // accepted. Saying so at once beats leaving a caller blocked until it
        // gives up.
        if !self.node.role().is_leader() {
            self.fail_pending();
        }

        self.status.send_replace(self.node.status());
        Ok(())
    }

    /// Fails every outstanding proposal with [`RaftError::ProposalLost`].
    fn fail_pending(&mut self) {
        let term = self.node.term();
        for (index, pending) in std::mem::take(&mut self.pending) {
            let _ = pending
                .responder
                .send(Err(RaftError::ProposalLost { index, term }));
        }
        for (index, responder) in std::mem::take(&mut self.pending_membership) {
            let _ = responder.send(Err(RaftError::ProposalLost { index, term }));
        }
    }

    /// Final tidy-up: nobody is left waiting on a promise this replica can no
    /// longer keep.
    fn finish(mut self) -> Result<()> {
        self.fail_pending();
        Ok(())
    }

    /// The node this replica drives, for tests and introspection.
    #[must_use]
    pub const fn node(&self) -> &RaftNode<M, L> {
        &self.node
    }
}

/// Whether `role` may accept proposals.
///
/// A tiny predicate with a name, so a server's "can I write?" check reads the
/// same way everywhere.
///
/// # Examples
///
/// ```
/// use astrs_raft::{driver::accepts_proposals, Role};
///
/// assert!(accepts_proposals(Role::Leader));
/// assert!(!accepts_proposals(Role::Candidate));
/// ```
#[must_use]
pub const fn accepts_proposals(role: Role) -> bool {
    role.is_leader()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::log::store::MemoryLog;
    use crate::state_machine::MemoryStateMachine;
    use crate::transport::ChannelTransport;
    use std::time::Duration;

    fn config(id: u64, peers: u64) -> RaftConfig {
        let mut config = RaftConfig::new(PeerId::new(id));
        for peer in 1..=peers {
            config = config.with_peer(
                PeerId::new(peer),
                std::net::SocketAddr::from(([127, 0, 0, 1], 7500 + peer as u16)),
            );
        }
        config
            .with_tick_interval(Duration::from_millis(2))
            .with_election_timeout(5, 5)
            .with_heartbeat_ticks(1)
    }

    /// A one-replica cluster, its handle, and the task running it.
    fn solo() -> (RaftHandle, tokio::task::JoinHandle<Result<()>>) {
        let (transport, mut inboxes) = ChannelTransport::mesh([PeerId::new(1)]);
        let inbound = inboxes.remove(&PeerId::new(1)).unwrap();
        let replica = RaftReplica::new(
            config(1, 1),
            MemoryLog::new(),
            MemoryStateMachine::new(),
            transport,
            inbound,
        )
        .unwrap();
        let handle = replica.handle();
        (handle, tokio::spawn(replica.run()))
    }

    #[tokio::test]
    async fn a_proposal_resolves_with_what_the_state_machine_returned() {
        let (handle, running) = solo();
        handle
            .wait_for(|status| status.role.is_leader())
            .await
            .unwrap();
        assert_eq!(handle.propose(b"echo".to_vec()).await.unwrap(), b"echo");
        handle.shutdown();
        running.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn status_is_readable_without_a_round_trip() {
        let (handle, running) = solo();
        handle
            .wait_for(|status| status.role.is_leader())
            .await
            .unwrap();
        assert!(handle.is_leader());
        assert!(handle.has_lease());
        assert_eq!(handle.leader(), Some(PeerId::new(1)));
        assert_eq!(handle.status().id, PeerId::new(1));
        handle.shutdown();
        running.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_follower_refuses_a_proposal_rather_than_hanging() {
        // Three configured peers, only one running: it can never win, so it
        // must answer NotLeader promptly instead of blocking forever.
        let (transport, mut inboxes) = ChannelTransport::mesh([PeerId::new(1)]);
        let inbound = inboxes.remove(&PeerId::new(1)).unwrap();
        let replica = RaftReplica::new(
            config(1, 3),
            MemoryLog::new(),
            MemoryStateMachine::new(),
            transport,
            inbound,
        )
        .unwrap();
        let handle = replica.handle();
        let running = tokio::spawn(replica.run());

        let error = handle.propose(b"x".to_vec()).await.unwrap_err();
        assert!(matches!(error, RaftError::NotLeader { .. }));
        handle.shutdown();
        running.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn many_proposals_apply_in_order() {
        let (handle, running) = solo();
        handle
            .wait_for(|status| status.role.is_leader())
            .await
            .unwrap();
        for index in 0..16u64 {
            let echoed = handle.propose(index.to_le_bytes().to_vec()).await.unwrap();
            assert_eq!(echoed, index.to_le_bytes().to_vec());
        }
        assert!(handle.status().last_applied.get() >= 16);
        handle.shutdown();
        running.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_snapshot_can_be_requested_through_the_handle() {
        let (handle, running) = solo();
        handle
            .wait_for(|status| status.role.is_leader())
            .await
            .unwrap();
        handle.propose(b"a".to_vec()).await.unwrap();
        handle.snapshot().await.unwrap();
        handle.shutdown();
        running.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_membership_change_takes_effect_on_append_even_if_it_costs_the_quorum() {
        // Growing a one-peer cluster to two makes the quorum two, and the new
        // peer is not running — so this leader immediately loses quorum
        // contact and steps down. Both halves of that are correct Raft, and
        // both are asserted: the configuration is in force from the moment it
        // was *appended* (see `astrs_raft::membership`), and the proposal is
        // reported lost rather than left hanging.
        let (handle, running) = solo();
        handle
            .wait_for(|status| status.role.is_leader())
            .await
            .unwrap();
        let outcome = handle
            .change_membership(MembershipChange::Add(PeerId::new(2)))
            .await;
        assert!(
            matches!(outcome, Err(RaftError::ProposalLost { .. })),
            "expected the change to be reported lost, got {outcome:?}"
        );
        assert_eq!(handle.status().membership.len(), 2);
        handle.shutdown();
        running.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_handle_on_a_stopped_replica_reports_shutting_down() {
        let (handle, running) = solo();
        handle.shutdown();
        running.await.unwrap().unwrap();
        let error = handle.propose(b"late".to_vec()).await.unwrap_err();
        assert!(matches!(error, RaftError::ShuttingDown));
    }

    #[test]
    fn only_a_leader_accepts_proposals() {
        assert!(accepts_proposals(Role::Leader));
        assert!(!accepts_proposals(Role::Follower));
        assert!(!accepts_proposals(Role::PreCandidate));
        assert!(!accepts_proposals(Role::Candidate));
    }
}
