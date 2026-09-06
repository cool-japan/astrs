//! End-to-end test for `wasm-operators` (blueprint §9.3, §22): a real WAT
//! module — no subprocess `cargo build` needed, since `wat::parse_str`
//! compiles WebAssembly text in-process — loaded through `astrs-runtime`'s
//! own `wasm-operators` loader and driven through
//! [`astrs_runtime::RuntimeHost`] end to end: a manifest `wasm:` entry,
//! resolved to a path, loaded, instantiated, and its guest-ABI sends
//! forwarded to a live output.
//!
//! `astrs_runtime`'s own `wasm` module unit tests already exhaustively cover
//! the guest ABI itself (fuel exhaustion, the memory limiter, traps, the
//! `astrs-op-init`/`astrs-op-event` exports) by building a `wasmi::Store`/
//! `wasmi::Instance` directly. What none of those cover — and what this file
//! exists to prove — is the *wiring* around that ABI: that a manifest
//! `operators[].wasm:` path actually reaches
//! [`astrs_runtime::wasm::resolve_wasm_path`] and the private
//! `WasmSource::load` via [`RuntimeHost::new`]'s own (also private)
//! `build_one_wasm_constructor`, that the operator this produces runs on its
//! own worker thread exactly like a compiled-in one, and that a real
//! [`astrs_node_api::MockDaemon`] round-trip (input in, output out, then a
//! clean stop) crosses the guest ABI inside that real multi-threaded host
//! rather than a directly-constructed `wasmi::Store`.
//!
//! Mirrors `tests/dylib_e2e.rs`'s own role for `dylib-operators`, but needs
//! none of that file's scaffold-and-`cargo build` machinery: a `.wasm`
//! module compiles from text in-process, so this fixture is built directly
//! in [`build_fixture_wasm`].
//!
//! The guest fixture reuses `astrs_runtime`'s own `wasm` module's unit-test
//! technique (see that module's
//! `echo_operator_round_trips_a_send_through_output_send` test): rather than
//! hand-write an oxicode decoder in WAT, the guest ignores every incoming
//! call and always replays one fixed, host-encoded
//! `(DataId, Metadata, Vec<u8>)` tuple baked into its own data section. What
//! this proves is exactly the part this file owns — that
//! `astrs.output-send`'s `(ptr, len)` are read out of the *correct*
//! instance's memory and forwarded through the real `RuntimeHost` pipeline
//! to the daemon — not the ABI's own call encoding, which the unit tests
//! already exercise directly.
//!
//! # Expected call count
//!
//! `astrs_runtime`'s own `worker::operator_loop`/`run_main_loop` set the
//! exact number of `astrs-op-event` calls a single input followed by a stop
//! produces: `on_start` (1), the one `Input` event (1), the `Stop` event
//! delivered via `on_event` (1 — `run_main_loop` ends the incarnation on
//! `is_stop` regardless of the status `on_event` returns), and `on_stop`
//! itself (1, via `finalize_on_stop`) — four in total, each replaying the
//! same fixed send. `configure` (`astrs-op-init`) never calls
//! `astrs-op-event`, so it contributes none.

#![cfg(feature = "wasm-operators")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::time::Duration;

use astrs_manifest::OperatorConfig;
use astrs_node_api::MockDaemon;
use astrs_operator_api::OperatorRegistry;
use astrs_runtime::{RuntimeConfig, RuntimeHost};
use astrs_wire::{
    DataId, InputSpec, Metadata, NodeId, NodeSource, NodeSpawnSpec, OutputSpec, PortRef,
    RestartConfig, StopCause, WireEncode,
};

/// A fresh path for the fixture module — keyed by process id, matching
/// `astrs_runtime`'s own `wasm` module's unit-test convention (nextest
/// gives each test its own process).
fn fixture_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "astrs-runtime-wasm-e2e-{}.wasm",
        std::process::id()
    ))
}

/// The one payload byte every `astrs-op-event` call answers with — chosen
/// to be a value no status code or length field could plausibly be confused
/// with, purely so a wrong-bytes failure reads unambiguously in an
/// assertion message.
const ECHO_BYTE: u8 = 0x2a;

/// Builds the WAT fixture: a guest that answers every `astrs-op-event` call
/// by replaying one fixed, pre-encoded `(DataId, Metadata, Vec<u8>)` send —
/// see this file's own module docs for why a fixed reply is enough to prove
/// what this file owns, and `astrs_runtime`'s own `wasm` module docs for the
/// full guest ABI this fixture implements (`astrs-op-alloc`, `astrs-op-init`,
/// `astrs-op-event`, `astrs.output-send`).
fn build_fixture_wasm() -> Vec<u8> {
    let send = (
        DataId::new("echo").expect("a valid data id"),
        Metadata::default(),
        vec![ECHO_BYTE],
    );
    let encoded = send.encode_to_vec().expect("the fixture send encodes");
    let data_hex: String = encoded.iter().map(|b| format!("\\{b:02x}")).collect();

    wat::parse_str(format!(
        r#"
        (module
          (import "astrs" "output-send" (func $output_send (param i32 i32)))
          (memory (export "memory") 4 32)
          (data (i32.const 65536) "{data_hex}")
          (global $bump (mut i32) (i32.const {offset}))

          (func (export "astrs-op-alloc") (param $len i32) (result i32)
            (local $ptr i32)
            global.get $bump
            local.set $ptr
            global.get $bump
            local.get $len
            i32.add
            global.set $bump
            local.get $ptr)

          (func (export "astrs-op-init") (param $ptr i32) (param $len i32) (result i32)
            i32.const 0)

          (func (export "astrs-op-event") (param $ptr i32) (param $len i32) (result i32)
            i32.const 65536
            i32.const {send_len}
            call $output_send
            i32.const 0)
        )
        "#,
        data_hex = data_hex,
        offset = 65536 + encoded.len(),
        send_len = encoded.len(),
    ))
    .expect("the fixture is valid WebAssembly text")
}

/// A daemon plus one already-connected node named `runtime`: `frames` fed
/// from `producer/out`, and a node-level `echo` output — matching
/// `tests/dylib_e2e.rs`'s own `host_harness` shape, with one output instead
/// of two (this fixture has no reason to distinguish `on_start`/`on_stop`
/// markers from the input echo — see the module docs on why a single fixed
/// reply is enough here).
fn host_harness() -> (
    MockDaemon,
    astrs_node_api::Node,
    astrs_node_api::EventStream,
) {
    let daemon = MockDaemon::start().expect("the mock daemon starts");
    let spec = NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new("runtime").expect("a valid node id"),
        0,
        NodeSource::Dynamic,
    )
    .with_input(InputSpec::new(
        DataId::new("frames").expect("a valid data id"),
        PortRef::from_parts("producer", "out").expect("a valid port ref"),
    ))
    .with_output(OutputSpec::new(
        DataId::new("echo").expect("a valid data id"),
    ))
    .with_restart(RestartConfig::never());
    let (node, events) = daemon.connect_node(spec).expect("the mock node connects");
    (daemon, node, events)
}

#[test]
fn a_wasm_module_is_resolved_loaded_and_driven_end_to_end_through_runtime_host() {
    let path = fixture_path();
    std::fs::write(&path, build_fixture_wasm()).expect("the fixture module is writable");

    let (daemon, node, events) = host_harness();
    let node_id = node.id().clone();

    let mut inputs = std::collections::BTreeMap::new();
    inputs.insert(
        "frames".to_owned(),
        astrs_manifest::Input::from_source("producer/out"),
    );

    let operator_config = OperatorConfig {
        id: "echo".to_owned(),
        operator: "Echo".to_owned(),
        dylib: None,
        wasm: Some(path.display().to_string()),
        hub: None,
        inputs,
        outputs: vec!["echo".to_owned()],
        config: std::collections::BTreeMap::new(),
    };

    // No `dataflow_dir`: `path` is already absolute (rooted at
    // `std::env::temp_dir()`), and an absolute `wasm:` path is never
    // rebased — see `astrs_runtime::wasm::resolve_wasm_path`'s own docs.
    let runtime_config = RuntimeConfig::new(vec![operator_config], OperatorRegistry::new());

    let host = RuntimeHost::new(node, events, runtime_config)
        .expect("the host resolves the wasm entry and builds cleanly");
    let runner = std::thread::spawn(move || host.run());

    let echo_id = DataId::new("echo").expect("a valid data id");
    // `on_start`'s send must cross the ABI before any input does — proves
    // the lifecycle ordering, not only that data moves at all.
    let started = daemon
        .wait_for_sends(&node_id, &echo_id, 1, Duration::from_secs(30))
        .expect("on_start's send arrives");
    assert_eq!(started[0].bytes(), Some(&[ECHO_BYTE][..]));

    daemon
        .send_input(
            &node_id,
            &DataId::new("frames").expect("a valid data id"),
            Metadata::default(),
            vec![1, 2, 3],
        )
        .expect("the daemon accepts the input");
    daemon
        .wait_for_sends(&node_id, &echo_id, 2, Duration::from_secs(30))
        .expect("the input event's send arrives");

    daemon
        .stop(&node_id, StopCause::Requested)
        .expect("the daemon accepts the stop");
    let report = runner
        .join()
        .expect("the host thread does not panic")
        .expect("the host run reports cleanly");
    assert!(report.all_healthy(), "{report:?}");

    // `on_event(Stop)` and `on_stop` each answer through `astrs-op-event`
    // too (see this file's own module docs on the exact count) — two more
    // sends, four in total, all the same fixed byte.
    let all_sends = daemon.sends_on(&node_id, &echo_id);
    assert_eq!(
        all_sends.len(),
        4,
        "expected on_start + input + stop-event + on_stop, one send apiece"
    );
    for send in &all_sends {
        assert_eq!(send.bytes(), Some(&[ECHO_BYTE][..]));
    }

    std::fs::remove_file(&path).ok();
}
