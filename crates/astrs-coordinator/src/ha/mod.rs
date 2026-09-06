//! Coordinator high availability: the registry, replicated (blueprint §22).
//!
//! # What is replicated, and what is not
//!
//! The coordinator's **authoritative mutable state** is the durable registry
//! in `astrs-store`: dataflow metadata, per-node status, the daemon registry,
//! parameters and the build cache. That is what a replacement coordinator must
//! have to take over, and it is what this module replicates — as a Raft log of
//! [`RegistryCommand`]s, each carrying complete records that every replica
//! applies as an overwrite (see [`command`] and [`machine`]).
//!
//! Not replicated: the *live* registry — open sockets, in-flight
//! `WaitForBuild` waiters, subscription fan-out. None of it outlives the
//! process that owns it, and a daemon that reconnects to a new leader
//! re-establishes all of it by connecting.
//!
//! # One model, two producers of the operation list
//!
//! Every mutating control request meets the same gate, and every replica
//! applies the same [`RegistryCommand`]. What differs is *where the operation
//! list comes from*:
//!
//! - **Parameters** are **proposed first**. The leader reads the current
//!   record, computes the complete new one (revision, timestamps and all), and
//!   proposes it *before* anything is written locally. Nothing exists on the
//!   leader that the cluster has not committed.
//! - **Orchestration verbs** (`Build`, `Start`, `Stop`, the topology verbs)
//!   run on the leader first, and the registry writes they produced are
//!   captured from the store's own mutation log and replicated. They cannot be
//!   proposed first, because their durable footprint is not knowable until
//!   they have run: `Start` invents a `DataflowId`, dispatches to daemons and
//!   records what each one accepted.
//!
//! The capture is exact because it runs under [`HaHandle::mutation_lock`],
//! held across execute-capture-propose, so two concurrent requests can never
//! attribute each other's writes.
//!
//! ## The bound this leaves, stated plainly
//!
//! For the orchestration verbs there is a window: a leader that crashes
//! *after* writing locally but *before* the entry commits leaves a record the
//! cluster never agreed on. The caller is never told the request succeeded —
//! the reply waits for the commit — so this is a phantom record, not a lost
//! acknowledgement. It matters little in practice because exactly that class
//! of state is re-derived from daemons as they reconnect, and it is why the
//! *parameter* path, whose records nothing else re-derives, is propose-first
//! instead.
//!
//! # Reads: leader lease, not `ReadIndex`
//!
//! Reads are served from the local store with no consensus round trip, gated
//! on [`HaHandle::is_ready`]. Two conditions make that safe, and both are
//! checked:
//!
//! 1. **A quorum answered recently.** [`astrs_raft::RaftNode::has_lease`] is
//!    true only while a majority of voters replied within the election-timeout
//!    *floor*, so no other leader can have been elected since.
//! 2. **This leader's own term has committed.** A peer becomes leader before
//!    it has applied anything; its state machine may still be missing entries
//!    the previous leader committed. `is_ready` waits for the no-op every
//!    leader appends on taking office — see [`astrs_raft::RaftNode::is_ready`].
//!
//! `ReadIndex` was the alternative: a heartbeat round trip per read, in
//! exchange for tolerating unbounded clock skew between replicas. The
//! coordinator's read path is polled continuously by `astrs top` and by every
//! `astrs list`, so paying a round trip per read is the more expensive
//! mistake; the assumption the lease makes instead — that three coordinators'
//! monotonic timers do not run at wildly different rates — is one this
//! deployment can state and hold.
//!
//! # Non-leaders redirect, they do not proxy
//!
//! A mutating request that reaches a follower is answered with the existing
//! [`astrs_wire::ControlReply::Error`], code
//! [`astrs_wire::ErrorCode::Unavailable`], carrying `leader: <address>` in its
//! `context` — no wire enum gained a variant for this. Proxying was rejected:
//! it would double every mutation's latency and make the follower a silent
//! participant in a request it cannot report on.

pub mod command;
pub mod config;
pub mod log;
pub mod machine;
pub mod params;

use std::net::SocketAddr;
use std::sync::Arc;

use astrs_raft::{
    MembershipChange, PeerId, RaftError, RaftHandle, RaftReplica, RaftStatus, TcpTransport,
    TcpTransportServer,
};
use astrs_store::AsyncStore;
use astrs_store::record::{MutationOp, MutationSeq};
use astrs_wire::{ControlReply, ErrorCode};

use crate::coordinator::Coordinator;
use crate::error::{CoordinatorError, Result};

pub use command::RegistryCommand;
pub use config::HaConfig;
pub use log::HaLog;
pub use machine::{RegistryStateMachine, snapshot_ops};

/// The prefix a leader hint carries in a [`ControlReply::Error`]'s `context`.
///
/// Stable and parseable on purpose: a client that wants to follow the
/// redirect automatically looks for exactly this.
pub const LEADER_HINT_PREFIX: &str = "leader: ";

/// The `context` entry used when this coordinator does not know who leads.
pub const NO_LEADER_HINT: &str = "leader: unknown";

/// A running replicated coordinator: the Raft replica plus everything the
/// control plane needs to route around it.
pub struct HaHandle {
    /// The replica this coordinator drives.
    raft: RaftHandle,
    /// Who the peers are and where they live.
    config: HaConfig,
    /// The store this replica replicates — the same handle the coordinator
    /// serves reads from.
    store: AsyncStore,
    /// Held across execute-capture-propose for the orchestration verbs, so a
    /// mutation-log window can never be attributed to the wrong request.
    mutation_lock: tokio::sync::Mutex<()>,
    /// Background tasks (the listener and the replica), aborted on drop.
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for HaHandle {
    fn drop(&mut self) {
        self.raft.shutdown();
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl HaHandle {
    /// Starts a replicated coordinator over `coordinator`'s store.
    ///
    /// Binds this node's Raft listener, dials its peers and starts the
    /// replica. Returns as soon as the replica is running — **not** when an
    /// election has completed, which may take an election timeout or two.
    ///
    /// # Errors
    ///
    /// - [`CoordinatorError::InvalidArgument`] if the configuration cannot be
    ///   used.
    /// - [`CoordinatorError::Io`] if the Raft listener cannot be bound or the
    ///   write-ahead log cannot be opened.
    pub async fn start(coordinator: &Coordinator, config: HaConfig) -> Result<Self> {
        let listen = config.listen_addr()?;
        let raft_config = config.to_raft_config();
        raft_config
            .validate()
            .map_err(|error| CoordinatorError::invalid(error.to_string()))?;

        let log = match &config.log_path {
            Some(path) => HaLog::open(path, config.fsync).map_err(raft_to_coordinator)?,
            None => HaLog::in_memory(),
        };
        let machine =
            RegistryStateMachine::new(coordinator.store.clone(), Arc::clone(&coordinator.clock));

        let (server, inbound) = TcpTransportServer::bind(listen)
            .await
            .map_err(raft_to_coordinator)?;
        let bound = server.local_addr().map_err(raft_to_coordinator)?;
        let transport = TcpTransport::connect(config.other_peers());

        let replica = RaftReplica::new(raft_config, log, machine, transport, inbound)
            .map_err(raft_to_coordinator)?;
        let raft = replica.handle();

        let listening = tokio::spawn(async move {
            if let Err(error) = server.serve().await {
                tracing::warn!(%error, "the coordinator's Raft listener stopped");
            }
        });
        let running = tokio::spawn(async move {
            if let Err(error) = replica.run().await {
                tracing::error!(%error, "the coordinator's Raft replica stopped");
            }
        });

        tracing::info!(
            node = config.node_id.get(),
            peers = config.peers.len(),
            %bound,
            "coordinator high availability is enabled"
        );
        Ok(Self {
            raft,
            config,
            store: coordinator.store.clone(),
            mutation_lock: tokio::sync::Mutex::new(()),
            tasks: vec![listening, running],
        })
    }

    /// This coordinator's Raft identity.
    #[must_use]
    pub const fn node_id(&self) -> PeerId {
        self.config.node_id
    }

    /// The HA configuration in force.
    #[must_use]
    pub const fn config(&self) -> &HaConfig {
        &self.config
    }

    /// The store this replica replicates.
    ///
    /// The same handle the coordinator reads from — a replicated write is
    /// visible here the moment it is applied, which is what makes a local read
    /// under the leader lease correct.
    #[must_use]
    pub const fn store(&self) -> &AsyncStore {
        &self.store
    }

    /// The replica's consensus status.
    #[must_use]
    pub fn status(&self) -> RaftStatus {
        self.raft.status()
    }

    /// Whether this coordinator may serve reads and accept mutations.
    ///
    /// Stricter than "is the leader" — see this module's header, *Reads*.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.raft.is_ready()
    }

    /// Whether this coordinator currently believes it leads, regardless of
    /// whether its state machine has caught up.
    #[must_use]
    pub fn is_leader(&self) -> bool {
        self.raft.is_leader()
    }

    /// Where the current leader can be reached, when this coordinator knows.
    #[must_use]
    pub fn leader_address(&self) -> Option<SocketAddr> {
        self.raft.leader().and_then(|id| self.config.address_of(id))
    }

    /// The structured reply a non-leader sends for a mutating request.
    ///
    /// Reuses [`ControlReply::Error`] exactly as it stands: code
    /// [`ErrorCode::Unavailable`] ("a required participant is not reachable
    /// right now"), a human-readable message, and a `context` entry
    /// `"leader: <address>"` a client can parse and follow.
    #[must_use]
    pub fn not_leader_reply(&self, verb: &str) -> ControlReply {
        let hint = match self.leader_address() {
            Some(address) => format!("{LEADER_HINT_PREFIX}{address}"),
            None => NO_LEADER_HINT.to_owned(),
        };
        let status = self.status();
        ControlReply::error_with_context(
            ErrorCode::Unavailable,
            format!(
                "'{verb}' must be sent to the leading coordinator; this one is {} in term {}",
                status.role.as_str(),
                status.term.get()
            ),
            [
                hint,
                format!("this coordinator is node {}", self.config.node_id.get()),
            ],
        )
    }

    /// Waits until this coordinator is ready to serve, or `deadline` elapses.
    ///
    /// Returns whether it became ready. Used by startup paths and tests that
    /// need a leader before they can do anything useful.
    pub async fn wait_until_ready(&self, deadline: std::time::Duration) -> bool {
        tokio::time::timeout(deadline, self.raft.wait_for(|status| status.leader_ready))
            .await
            .is_ok_and(|outcome| outcome.is_ok())
    }

    /// Proposes one batch of registry operations and waits for it to be
    /// applied on this replica.
    ///
    /// An empty batch is a no-op: proposing it would spend a log entry to
    /// change nothing.
    ///
    /// # Errors
    ///
    /// [`CoordinatorError::InvalidArgument`] describing the Raft failure —
    /// not the leader, the proposal was superseded, the replica stopped.
    pub async fn propose(&self, ops: Vec<MutationOp>) -> Result<()> {
        if ops.is_empty() {
            return Ok(());
        }
        let command = RegistryCommand::new(ops).encode()?;
        self.raft
            .propose(command)
            .await
            .map(|_| ())
            .map_err(raft_to_coordinator)
    }

    /// Takes the mutation lock, which must be held across
    /// execute-capture-propose for a captured batch to be exact.
    pub async fn mutation_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.mutation_lock.lock().await
    }

    /// Replicates every registry write logged after `since`.
    ///
    /// This is the capture half of the orchestration path: the caller records
    /// [`AsyncStore::last_seq`] before running a handler and passes it here
    /// afterwards, still holding [`HaHandle::mutation_lock`].
    ///
    /// # Errors
    ///
    /// [`CoordinatorError::Store`] if the mutation log cannot be read, or
    /// whatever [`HaHandle::propose`] reports.
    pub async fn replicate_since(&self, store: &AsyncStore, since: MutationSeq) -> Result<()> {
        let mut cursor = since;
        let mut ops = Vec::new();
        loop {
            let page = store
                .mutations_since(cursor, astrs_store::MAX_CATCH_UP_PAGE)
                .await?;
            let caught_up = page.caught_up;
            cursor = page.next_seq;
            ops.extend(page.entries.into_iter().map(|record| record.op));
            if caught_up {
                break;
            }
        }
        self.propose(ops).await
    }

    /// Adds or removes one coordinator from the replicated set.
    ///
    /// # Errors
    ///
    /// As [`astrs_raft::RaftHandle::change_membership`].
    pub async fn change_membership(&self, change: MembershipChange) -> Result<()> {
        self.raft
            .change_membership(change)
            .await
            .map_err(raft_to_coordinator)
    }

    /// Stops the replica and its listener.
    pub fn shutdown(&self) {
        self.raft.shutdown();
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Maps a Raft failure onto this crate's error type, preserving the leader
/// hint when there is one.
fn raft_to_coordinator(error: RaftError) -> CoordinatorError {
    match &error {
        RaftError::Io { source, .. } => {
            CoordinatorError::invalid(format!("Raft I/O failure: {source}"))
        }
        _ => CoordinatorError::invalid(error.to_string()),
    }
}

/// Whether `reply` is the redirect a non-leader sends, and if so the address
/// it points at.
///
/// The parsing half of [`HaHandle::not_leader_reply`], kept beside it so the
/// two cannot drift. A client library follows a redirect with this.
///
/// # Examples
///
/// ```
/// use astrs_coordinator::ha::leader_hint;
/// use astrs_wire::{ControlReply, ErrorCode};
///
/// let reply = ControlReply::error_with_context(
///     ErrorCode::Unavailable,
///     "not the leader",
///     ["leader: 10.0.0.2:7601".to_owned()],
/// );
/// assert_eq!(leader_hint(&reply), Some("10.0.0.2:7601".to_owned()));
///
/// assert_eq!(leader_hint(&ControlReply::Ok), None);
/// ```
#[must_use]
pub fn leader_hint(reply: &ControlReply) -> Option<String> {
    let ControlReply::Error { code, context, .. } = reply else {
        return None;
    };
    if *code != ErrorCode::Unavailable {
        return None;
    }
    context
        .iter()
        .find_map(|entry| entry.strip_prefix(LEADER_HINT_PREFIX))
        .filter(|address| *address != "unknown")
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn a_leader_hint_round_trips_through_the_existing_error_reply() {
        let reply = ControlReply::error_with_context(
            ErrorCode::Unavailable,
            "not the leader",
            [format!("{LEADER_HINT_PREFIX}127.0.0.1:7602")],
        );
        assert_eq!(leader_hint(&reply), Some("127.0.0.1:7602".to_owned()));
    }

    #[test]
    fn an_unknown_leader_is_not_reported_as_an_address() {
        let reply = ControlReply::error_with_context(
            ErrorCode::Unavailable,
            "not the leader",
            [NO_LEADER_HINT.to_owned()],
        );
        assert_eq!(leader_hint(&reply), None);
    }

    #[test]
    fn other_errors_are_not_mistaken_for_redirects() {
        let reply = ControlReply::error_with_context(
            ErrorCode::NotFound,
            "no such dataflow",
            [format!("{LEADER_HINT_PREFIX}127.0.0.1:7602")],
        );
        assert_eq!(leader_hint(&reply), None);
        assert_eq!(leader_hint(&ControlReply::Ok), None);
    }

    #[test]
    fn a_raft_error_keeps_its_message_when_it_becomes_a_coordinator_error() {
        let error = raft_to_coordinator(RaftError::NotLeader {
            leader: Some(PeerId::new(3)),
        });
        assert!(error.to_string().contains("peer 3"), "{error}");
    }
}
