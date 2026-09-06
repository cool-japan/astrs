//! `astrs topic pub` end to end: a dynamic-node attach over a real Unix
//! socket, landing on a real consumer node through routing — the same
//! mechanism blueprint §8.3 describes and `astrs-node-api`'s own
//! `dynamic_attach.rs` proves for the bare `Node::builder()` API. This
//! file proves the same loop through `astrs_cli::command::topic::publish`
//! itself, completely unmodified.
//!
//! # Why `MockDaemon`, and why this needs its own binary
//!
//! `astrs-node-api::testing::MockDaemon` speaks the real node protocol
//! (§7.2/§7.3) over a real socket — see that module's own docs — and its
//! `listen_unix` exists specifically so "a `path: dynamic` node can attach
//! itself with nothing pre-arranged" is exercised over a socket rather
//! than only over an in-process duplex. `command::topic::publish` never
//! takes an explicit daemon address (a real `astrs topic pub` invocation
//! relies on the local daemon being at the §24.2 default), so pointing it
//! at the mock daemon needs `ASTRS_RUNTIME_DIR` set to wherever this test
//! bound the mock's socket. Every `tests/*.rs` file is already its own
//! compiled test binary (a separate process, whether run under plain
//! `cargo test` or `cargo nextest`), which is what makes mutating that
//! one process-wide variable here safe: no other `#[test]` anywhere in
//! this crate shares this file's process.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(unix)]

use std::net::SocketAddr;
use std::time::Duration;

use astrs_cli::command::client::Endpoint;
use astrs_cli::command::topic::{self, PublishArgs};
use astrs_coordinator::{Coordinator, CoordinatorConfig, CoordinatorServer};
use astrs_data::prelude::*;
use astrs_node_api::testing::MockDaemon;
use astrs_wire::{AuthToken, DataId, InputSpec, NodeId, NodeSource, NodeSpawnSpec, PortRef};

/// Generous enough for CI jitter, short enough that a genuinely stuck
/// exchange fails the test rather than hanging the suite.
const WAIT: Duration = Duration::from_secs(5);

/// A scratch directory unique to this test and this process, standing in
/// for `$XDG_RUNTIME_DIR/astrs` (blueprint §24.2).
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("astrs-cli-pub-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch dir");
    dir
}

/// A coordinator on its own thread — `publish`'s coordinator half only
/// ever issues a soft-failing `GetNodeInfo` once the dataflow id is given
/// explicitly (see `command::topic::publish`'s own source), so an
/// otherwise empty, in-memory coordinator is everything this needs.
struct Cluster {
    addr: SocketAddr,
    token: AuthToken,
    shutdown: astrs_coordinator::ServerHandle,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Cluster {
    fn start() -> Self {
        let token = AuthToken::from_bytes([0x7a; 32]);
        let config = CoordinatorConfig::new(token.clone()).with_port(0);
        let hub = Coordinator::open_in_memory(config).expect("an in-memory store");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let server = runtime
            .block_on(CoordinatorServer::bind(hub))
            .expect("a bound coordinator");
        let addr = server.local_addr().expect("its address");
        let shutdown = server.handle();
        let thread = std::thread::spawn(move || {
            let _ = runtime.block_on(server.serve());
        });
        Self {
            addr,
            token,
            shutdown,
            thread: Some(thread),
        }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::new(self.addr, self.token.clone())
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.shutdown.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn topic_pub_attaches_as_a_dynamic_node_and_routes_into_a_consumer() {
    let cluster = Cluster::start();
    let endpoint = cluster.endpoint();

    // Blueprint §16: one cluster token authenticates every `Hello`,
    // coordinator and daemon alike — so the mock daemon must share the
    // same token `endpoint` carries, not its own default.
    let daemon = MockDaemon::start_with(
        astrs_node_api::runtime::NodeRuntime::acquire().expect("a node runtime"),
        astrs_wire::DataflowId::generate(),
        endpoint.token.clone(),
    )
    .expect("a mock daemon");
    let runtime_dir = scratch("runtime");
    let socket = runtime_dir.join("daemon.sock");
    // Kept alive for the whole test: dropping it unlinks the socket.
    let _daemon_endpoint = daemon
        .listen_unix(&socket)
        .expect("bind the default daemon socket");

    // SAFETY (in the process-isolation sense the module doc explains):
    // this is the only `#[test]` in this compiled test binary, so no
    // concurrently running test can observe this mutation.
    unsafe {
        std::env::set_var("ASTRS_RUNTIME_DIR", &runtime_dir);
    }

    // The consumer's input names the producer's port — exactly what a
    // real daemon's routing keys on, and what `MockDaemon` reproduces
    // (see `astrs-node-api`'s own `two_dynamic_nodes_route_between_themselves`).
    let consumer_spec = NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new("counter").unwrap(),
        0,
        NodeSource::Dynamic,
    )
    .with_input(InputSpec::new(
        DataId::new("ticks").unwrap(),
        PortRef::from_parts("emitter", "ticks").unwrap(),
    ));
    let (_consumer, mut consumer_events) = daemon.connect_node(consumer_spec).unwrap();

    let args = PublishArgs {
        topic: "emitter/ticks".to_owned(),
        message: "\"hello\"".to_owned(),
        dataflow: Some(daemon.dataflow().to_string()),
        rate: None,
        json: false,
    };
    let mut out = Vec::new();
    let report = topic::publish(&mut out, &endpoint, &args).expect("astrs topic pub");
    assert_eq!(report.sent, 1);
    assert_eq!(report.port.to_string(), "emitter/ticks");

    let event = consumer_events
        .recv_timeout(WAIT)
        .unwrap()
        .expect("the routed message");
    let (id, _meta, data) = event.into_input().expect("an Input event");
    assert_eq!(id.as_str(), "ticks");

    // No manifest and no reachable declared type: `publish`'s untyped
    // fallback carries the exact message text as one `Binary` column —
    // proved by decoding the real Arrow IPC bytes the consumer received,
    // the same decode `astrs topic echo` performs on its own end.
    let payload = data.to_vec();
    let batch = astrs_data::ipc::decode_payload(&payload).expect("a decodable Arrow payload");
    assert_eq!(batch.num_rows(), 1);
    let column = batch.column(0).expect("one column");
    let binary = column
        .as_any()
        .downcast_ref::<BinaryArray>()
        .expect("the untyped fallback is a Binary column");
    assert_eq!(binary.value(0), Some(args.message.as_bytes()));
}
