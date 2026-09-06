//! A three-coordinator cluster, end to end (blueprint §22).
//!
//! Three real [`CoordinatorServer`]s, each with its own store and its own
//! Raft replica talking to the other two over real TCP sockets, driven by a
//! real CLI connection with a real handshake. What this file proves is what a
//! unit test cannot:
//!
//! 1. **Election.** Three coordinators started together converge on exactly
//!    one leader.
//! 2. **The redirect.** A mutating request that reaches a follower is refused
//!    with the *existing* structured error, carrying a parseable
//!    `leader: <address>` hint — no wire enum gained a variant.
//! 3. **Replication.** A registry mutation accepted by the leader is readable
//!    from a follower, which is what makes the failover in (4) meaningful.
//! 4. **Failover.** Killing the leader costs one election, and the parameter
//!    written before the failure is still there afterwards — read back
//!    through the new leader, and written to again.
//!
//! # Ports
//!
//! Each replica's Raft listener needs an address its *peers* know in advance,
//! so the three are reserved by binding ephemeral ports and releasing them
//! before the replicas claim them. The coordinator's own CLI listener has no
//! such constraint and binds `:0` directly.

#![cfg(feature = "ha")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use astrs_coordinator::ha::{HaConfig, HaHandle, leader_hint};
use astrs_coordinator::{Coordinator, CoordinatorConfig, CoordinatorServer};
use astrs_raft::PeerId;
use astrs_transport::{ConnectionCounters, FramedStream, HandshakeParams, LocalIdentity, initiate};
use astrs_wire::{
    AuthToken, ControlReply, ControlRequest, ErrorCode, FeatureFlags, FrameKind, FrameLimits,
    ParamKey, ParamScope, Parameter, Role,
};
use tokio::net::TcpStream;

/// Long enough that CI jitter never fails a healthy run, short enough that a
/// genuinely stuck exchange fails instead of hanging the suite.
const TIMEOUT: Duration = Duration::from_secs(5);

/// How long a test waits for an election. At the tick below, one election
/// timeout is 50 ms, so this is many elections' worth of headroom.
const ELECTION_BUDGET: Duration = Duration::from_secs(10);

/// The cluster token every connection authenticates with.
fn token() -> AuthToken {
    AuthToken::from_bytes([0x7a; 32])
}

/// Reserves `count` ephemeral loopback ports and releases them, so the Raft
/// peers can be told each other's addresses before they bind.
async fn reserve_ports(count: usize) -> Vec<SocketAddr> {
    let mut listeners = Vec::with_capacity(count);
    let mut addresses = Vec::with_capacity(count);
    for _ in 0..count {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("an ephemeral port");
        addresses.push(listener.local_addr().expect("its address"));
        listeners.push(listener);
    }
    drop(listeners);
    addresses
}

/// One replicated coordinator: its store, its Raft handle, its CLI listener.
struct Replica {
    id: PeerId,
    cli_addr: SocketAddr,
    ha: Arc<HaHandle>,
    server: astrs_coordinator::ServerHandle,
    task: tokio::task::JoinHandle<astrs_coordinator::Result<()>>,
}

impl Replica {
    /// Whether this replica is a leader that has caught up and may serve.
    fn is_ready(&self) -> bool {
        self.ha.is_ready()
    }

    /// Stops this replica: its Raft replica, its listener, its tasks. Models
    /// a coordinator process being killed.
    async fn kill(self) {
        self.ha.shutdown();
        self.server.shutdown();
        let _ = self.task.await;
    }
}

/// Starts a three-coordinator replicated set.
async fn start_cluster() -> Vec<Replica> {
    let raft_addresses = reserve_ports(3).await;
    let peers: BTreeMap<PeerId, SocketAddr> = raft_addresses
        .iter()
        .enumerate()
        .map(|(index, address)| (PeerId::new(index as u64 + 1), *address))
        .collect();

    let mut replicas = Vec::with_capacity(3);
    for index in 0..3usize {
        let id = PeerId::new(index as u64 + 1);
        let coordinator = Coordinator::open_in_memory(
            CoordinatorConfig::new(token())
                .with_port(0)
                .with_heartbeat(Duration::from_millis(50), 3),
        )
        .expect("an in-memory store");

        // A fast tick keeps the whole failover scenario inside a second; the
        // production default (50 ms) would make this test take minutes.
        let ha_config = HaConfig::new(id, peers.clone()).with_timing(Duration::from_millis(5), 10);
        let ha = Arc::new(
            HaHandle::start(&coordinator, ha_config)
                .await
                .expect("a Raft replica"),
        );

        let hub = coordinator.with_ha(Arc::clone(&ha));
        let server = CoordinatorServer::bind(hub).await.expect("a CLI listener");
        let cli_addr = server.local_addr().expect("its address");
        let handle = server.handle();
        let task = tokio::spawn(server.serve());

        replicas.push(Replica {
            id,
            cli_addr,
            ha,
            server: handle,
            task,
        });
    }
    replicas
}

/// Waits until exactly one of `replicas` is ready to lead.
async fn await_leader(replicas: &[Replica]) -> usize {
    let deadline = tokio::time::Instant::now() + ELECTION_BUDGET;
    loop {
        let ready: Vec<usize> = replicas
            .iter()
            .enumerate()
            .filter(|(_, replica)| replica.is_ready())
            .map(|(index, _)| index)
            .collect();
        if ready.len() == 1 {
            return ready[0];
        }
        assert!(
            ready.len() < 2,
            "two coordinators claimed leadership at once: {ready:?}"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "no coordinator became ready within {ELECTION_BUDGET:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A CLI connection to one coordinator, with a real handshake.
struct Cli {
    stream: FramedStream<TcpStream>,
}

impl Cli {
    async fn connect(addr: SocketAddr) -> Self {
        let raw = TcpStream::connect(addr).await.expect("a TCP connection");
        let mut stream =
            FramedStream::new(raw, FrameLimits::network(), ConnectionCounters::shared());
        let params = HandshakeParams::new(LocalIdentity::new(Role::Cli), token())
            .with_features(FeatureFlags::EMPTY);
        initiate(&mut stream, &params, TIMEOUT)
            .await
            .expect("a CLI handshake");
        Self { stream }
    }

    async fn request(&mut self, request: ControlRequest) -> ControlReply {
        self.stream
            .send_message(&request)
            .await
            .expect("sending a request");
        tokio::time::timeout(TIMEOUT, self.stream.expect_message(FrameKind::ControlReply))
            .await
            .expect("a reply within the timeout")
            .expect("a decodable reply")
    }
}

fn set_param(key: &str, value: i64) -> ControlRequest {
    ControlRequest::SetParam {
        scope: ParamScope::Global,
        key: ParamKey::new(key).unwrap(),
        value: Parameter::Integer(value),
        create_only: false,
    }
}

fn get_param(key: &str) -> ControlRequest {
    ControlRequest::GetParam {
        scope: ParamScope::Global,
        key: ParamKey::new(key).unwrap(),
        inherited: false,
    }
}

/// Reads a parameter through `cli`, retrying while it has not yet arrived.
///
/// Replication is asynchronous from the *reader's* point of view: the leader
/// answers as soon as a majority has the entry, and the third replica applies
/// it moments later.
async fn await_param(cli: &mut Cli, key: &str, expected: i64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let ControlReply::ParamValue { value, .. } = cli.request(get_param(key)).await
            && value == Some(Parameter::Integer(expected))
        {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_coordinators_elect_one_leader() {
    let replicas = start_cluster().await;
    let leader = await_leader(&replicas).await;

    // Exactly one, and the other two agree about who it is.
    assert_eq!(
        replicas.iter().filter(|r| r.is_ready()).count(),
        1,
        "exactly one coordinator may lead"
    );
    let leader_address = replicas[leader].ha.config().listen_addr().unwrap();
    for (index, replica) in replicas.iter().enumerate() {
        if index == leader {
            continue;
        }
        assert_eq!(
            replica.ha.leader_address(),
            Some(leader_address),
            "{} does not agree about the leader",
            replica.id
        );
    }

    for replica in replicas {
        replica.kill().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_follower_refuses_a_mutation_and_names_the_leader() {
    let replicas = start_cluster().await;
    let leader = await_leader(&replicas).await;
    let follower = (0..3).find(|index| *index != leader).unwrap();

    let mut cli = Cli::connect(replicas[follower].cli_addr).await;
    let reply = cli.request(set_param("gain", 1)).await;

    assert_eq!(
        reply.error_code(),
        Some(ErrorCode::Unavailable),
        "a follower must refuse a mutation, not apply it"
    );
    let hint = leader_hint(&reply).expect("a parseable leader hint");
    assert_eq!(
        hint,
        replicas[leader]
            .ha
            .config()
            .listen_addr()
            .unwrap()
            .to_string(),
        "the hint must point at the real leader"
    );

    // A *read* on the same follower still works: reads are served locally.
    assert!(matches!(
        cli.request(get_param("gain")).await,
        ControlReply::ParamValue { .. }
    ));

    for replica in replicas {
        replica.kill().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_registry_mutation_reaches_every_replica() {
    let replicas = start_cluster().await;
    let leader = await_leader(&replicas).await;

    let mut writer = Cli::connect(replicas[leader].cli_addr).await;
    assert_eq!(
        writer.request(set_param("gain", 42)).await,
        ControlReply::Ok
    );

    for (index, replica) in replicas.iter().enumerate() {
        let mut reader = Cli::connect(replica.cli_addr).await;
        assert!(
            await_param(&mut reader, "gain", 42).await,
            "replica {} never saw the replicated parameter",
            index + 1
        );
    }

    for replica in replicas {
        replica.kill().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_killed_leader_costs_one_election_and_no_committed_data() {
    let mut replicas = start_cluster().await;
    let leader = await_leader(&replicas).await;

    // Write through the leader, and confirm a majority has it.
    let mut writer = Cli::connect(replicas[leader].cli_addr).await;
    assert_eq!(
        writer.request(set_param("survives", 7)).await,
        ControlReply::Ok
    );
    drop(writer);

    // Kill it.
    let dead = replicas.remove(leader);
    let dead_id = dead.id;
    dead.kill().await;

    // One election later, one of the survivors leads.
    let successor = await_leader(&replicas).await;
    assert_ne!(replicas[successor].id, dead_id);

    // The pre-failover write is still readable through the new leader...
    let mut reader = Cli::connect(replicas[successor].cli_addr).await;
    assert!(
        await_param(&mut reader, "survives", 7).await,
        "a committed parameter was lost across the failover"
    );

    // ...and the new leader accepts writes, which is the other half of
    // "the cluster kept working".
    assert_eq!(
        reader.request(set_param("after", 9)).await,
        ControlReply::Ok
    );
    assert!(await_param(&mut reader, "after", 9).await);

    // The surviving follower converges on both values too.
    let other = (0..replicas.len())
        .find(|index| *index != successor)
        .unwrap();
    let mut follower = Cli::connect(replicas[other].cli_addr).await;
    assert!(await_param(&mut follower, "survives", 7).await);
    assert!(await_param(&mut follower, "after", 9).await);

    for replica in replicas {
        replica.kill().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deleted_parameter_is_replicated_too() {
    let replicas = start_cluster().await;
    let leader = await_leader(&replicas).await;

    let mut cli = Cli::connect(replicas[leader].cli_addr).await;
    assert_eq!(cli.request(set_param("doomed", 1)).await, ControlReply::Ok);
    assert!(await_param(&mut cli, "doomed", 1).await);

    assert_eq!(
        cli.request(ControlRequest::DeleteParam {
            scope: ParamScope::Global,
            key: ParamKey::new("doomed").unwrap(),
        })
        .await,
        ControlReply::Ok
    );

    let follower = (0..3).find(|index| *index != leader).unwrap();
    let mut reader = Cli::connect(replicas[follower].cli_addr).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let ControlReply::ParamValue { value: None, .. } =
            reader.request(get_param("doomed")).await
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the delete never reached the follower"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    for replica in replicas {
        replica.kill().await;
    }
}

/// The other half of the replication story: the *capture* path, used by the
/// orchestration verbs whose durable footprint is not knowable until they have
/// run. Driven here directly, because reproducing a `Start` needs daemons this
/// file deliberately does not have.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_captured_from_the_mutation_log_are_replicated_too() {
    let replicas = start_cluster().await;
    let leader = await_leader(&replicas).await;

    // Register a dataflow directly on the leader's store, exactly as an
    // orchestration handler would, then replicate what it produced.
    let store = replicas[leader].ha.store();
    let before = store.last_seq().await.expect("the current sequence");
    let dataflow = astrs_wire::DataflowId::generate();
    store
        .upsert_dataflow(dataflow, Some("perception".to_owned()), "{}".to_owned(), 2)
        .await
        .expect("registering a dataflow");
    replicas[leader]
        .ha
        .replicate_since(store, before)
        .await
        .expect("replicating the captured writes");

    for (index, replica) in replicas.iter().enumerate() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let found = replica
                .ha
                .store()
                .get_dataflow_meta(dataflow)
                .await
                .expect("reading the registry");
            if found.is_some_and(|meta| meta.name.as_deref() == Some("perception")) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "replica {} never saw the captured dataflow registration",
                index + 1
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    for replica in replicas {
        replica.kill().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_only_still_refuses_an_existing_key_under_replication() {
    // The propose-first path re-implements `create_only` (it must read before
    // it proposes); this checks it kept the same semantics.
    let replicas = start_cluster().await;
    let leader = await_leader(&replicas).await;

    let mut cli = Cli::connect(replicas[leader].cli_addr).await;
    assert_eq!(cli.request(set_param("once", 1)).await, ControlReply::Ok);
    assert!(await_param(&mut cli, "once", 1).await);

    let reply = cli
        .request(ControlRequest::SetParam {
            scope: ParamScope::Global,
            key: ParamKey::new("once").unwrap(),
            value: Parameter::Integer(2),
            create_only: true,
        })
        .await;
    assert_eq!(reply.error_code(), Some(ErrorCode::AlreadyExists));
    // And the original value is untouched.
    assert!(await_param(&mut cli, "once", 1).await);

    for replica in replicas {
        replica.kill().await;
    }
}
