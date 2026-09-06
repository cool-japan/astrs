//! End-to-end test for `dylib-operators` (blueprint §9.3, §22): a real
//! `cdylib` operator crate, scaffolded into [`std::env::temp_dir`], built
//! with a real `cargo build`, loaded through this crate's own
//! `dylib-operators` loader, and driven through
//! [`astrs_runtime::RuntimeHost`] end to end — a manifest `dylib:` entry,
//! resolved to a path, opened, and its events forwarded to a live output.
//!
//! Mirrors the scaffold-compile pattern in
//! `crates/astrs/tests/downstream_compile.rs`: a throwaway crate with a
//! path dependency on this workspace's `astrs-operator-api`, built with a
//! shared `CARGO_TARGET_DIR` (so only the first run in a fresh environment
//! pays for compiling the dependency tree) via the same `CARGO` binary that
//! built this test — see that file's own docs for why each of those
//! choices matters. What differs here: the scaffold is a `cdylib`
//! (`cargo build`, not `cargo check` — an artifact has to exist to load),
//! and after it exists, this test loads and drives it through the real
//! loader in `crate::dylib`, not merely compiles it.

#![cfg(feature = "dylib-operators")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use astrs_manifest::OperatorConfig;
use astrs_node_api::MockDaemon;
use astrs_operator_api::OperatorRegistry;
use astrs_runtime::{RuntimeConfig, RuntimeHost};
use astrs_wire::{
    DataId, InputSpec, Metadata, NodeId, NodeSource, NodeSpawnSpec, OutputSpec, PortRef,
    RestartConfig, StopCause,
};

/// This package's directory, from which the workspace root (and so
/// every sibling crate's own path) is derived.
const RUNTIME_CRATE_DIR: &str = env!("CARGO_MANIFEST_DIR");

/// The `crates/` directory every workspace member lives under.
fn crates_dir() -> PathBuf {
    Path::new(RUNTIME_CRATE_DIR)
        .parent()
        .expect("crates/astrs-runtime has a parent directory")
        .to_path_buf()
}

/// `astrs-operator-api`'s own path — the fixture crate's dependency for
/// the `Operator` trait and `export_dylib_operator!`.
fn operator_api_dir() -> PathBuf {
    crates_dir().join("astrs-operator-api")
}

/// `astrs-wire`'s own path. Not reachable through `astrs-operator-api`
/// alone: it is only a *transitive* dependency of the fixture crate, and
/// (exactly the same "cannot find astrs_wire in this scope" a downstream
/// crate would hit — see `crates/astrs/tests/downstream_compile.rs`'s own
/// module docs on this precise failure mode for `astrs_data`) a transitive
/// dependency is not nameable from a crate's own source. The fixture's
/// `Operator::configure` needs `astrs_wire::Parameter` and its sends need
/// `astrs_wire::Metadata` by name, so this is a real, direct dependency of
/// the fixture crate, not merely of this test.
fn wire_dir() -> PathBuf {
    crates_dir().join("astrs-wire")
}

/// The shared `CARGO_TARGET_DIR` every scaffold in this file builds into —
/// see the module docs on why sharing one matters.
fn shared_target_dir() -> PathBuf {
    std::env::temp_dir().join("astrs-runtime-dylib-e2e-target")
}

/// A fresh, empty directory for the fixture crate.
///
/// Keyed by process id *and* a per-call counter, matching
/// `downstream_compile.rs`'s own convention: nextest runs each test in its
/// own process, but a future test in this file that scaffolds twice must
/// not collide with itself.
fn scratch_dir(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "astrs-runtime-dylib-e2e-{}-{name}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("the scratch directory is writable");
    dir
}

/// The cargo to drive the child build with — `CARGO`, set by cargo itself
/// for anything it runs, so this is the *same* toolchain that built this
/// test rather than whatever `PATH` resolves to (see
/// `downstream_compile.rs`'s own `cargo_binary` for the same reasoning).
fn cargo_binary() -> PathBuf {
    std::env::var_os("CARGO").map_or_else(|| PathBuf::from("cargo"), PathBuf::from)
}

/// The fixture operator crate's `Cargo.toml`: a `cdylib`, one path
/// dependency on this workspace's own `astrs-operator-api` with the
/// `dylib` feature — a real downstream operator author's manifest, not a
/// workspace member (the `[workspace]` table keeps cargo from adopting it
/// into one it happens to sit near).
fn fixture_manifest() -> String {
    format!(
        "[package]\n\
         name = \"fixture-op\"\n\
         version = \"0.0.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         \n\
         [workspace]\n\
         \n\
         [lib]\n\
         crate-type = [\"cdylib\"]\n\
         \n\
         [dependencies]\n\
         astrs-operator-api = {{ path = '{operator_api}', features = [\"dylib\"] }}\n\
         astrs-wire = {{ path = '{wire}' }}\n",
        operator_api = operator_api_dir().display(),
        wire = wire_dir().display(),
    )
}

/// The fixture operator's `src/lib.rs`: a real
/// [`astrs_operator_api::Operator`] impl that exercises every hook this
/// test needs to observe — `configure` (records a threshold),
/// `on_start`/`on_stop` (each emit a marker send), `on_event` (echoes the
/// input back doubled with the configured threshold added, so a correct
/// answer proves both the payload *and* the configured state crossed the
/// ABI intact), and `export_dylib_operator!` itself, the one line that
/// makes this crate loadable at all.
const FIXTURE_LIB_RS: &str = r#"
use astrs_operator_api::{
    OpEvent, OpOutput, OpResult, Operator, Status, export_dylib_operator,
};

#[derive(Default)]
struct FixtureOp {
    threshold: i64,
}

impl Operator for FixtureOp {
    fn configure(
        &mut self,
        config: &std::collections::BTreeMap<String, astrs_wire::Parameter>,
    ) -> OpResult<()> {
        if let Some(astrs_wire::Parameter::Integer(value)) = config.get("threshold") {
            self.threshold = *value;
        }
        Ok(())
    }

    fn on_start(&mut self, out: &mut OpOutput) -> OpResult<()> {
        out.send_bytes("marker", astrs_wire::Metadata::default(), b"started".to_vec())?;
        Ok(())
    }

    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match event {
            OpEvent::Input { metadata, payload, .. } => {
                let answer: Vec<u8> = payload
                    .iter()
                    .map(|byte| byte.wrapping_mul(2).wrapping_add(self.threshold as u8))
                    .collect();
                out.send_bytes("doubled", metadata.clone(), answer)?;
                Ok(Status::Continue)
            }
            OpEvent::Stop { .. } => Ok(Status::Finished),
            _ => Ok(Status::Continue),
        }
    }

    fn on_stop(&mut self, out: &mut OpOutput) -> OpResult<()> {
        out.send_bytes("marker", astrs_wire::Metadata::default(), b"stopped".to_vec())?;
        Ok(())
    }
}

export_dylib_operator!(FixtureOp);
"#;

/// Writes the fixture crate to `dir`.
fn write_fixture(dir: &Path) {
    std::fs::write(dir.join("Cargo.toml"), fixture_manifest()).expect("the manifest is writable");
    std::fs::write(dir.join("src/lib.rs"), FIXTURE_LIB_RS).expect("the source file is writable");
}

/// Runs `cargo build --lib` on the fixture crate at `dir`, offline first —
/// a release gate should not depend on a network round-trip; the retry
/// without it exists for a genuinely cold `~/.cargo` (see
/// `downstream_compile.rs`'s identical reasoning).
fn cargo_build(dir: &Path, offline: bool) -> std::io::Result<Output> {
    let mut command = Command::new(cargo_binary());
    command.arg("build").arg("--lib").arg("--quiet");
    if offline {
        command.arg("--offline");
    }
    command
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", shared_target_dir())
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CARGO_MANIFEST_PATH")
        .env_remove("CARGO_PRIMARY_PACKAGE")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_RUSTFLAGS")
        .env_remove("CARGO_MAKEFLAGS")
        .output()
}

/// This platform's dynamic library file name for a crate named
/// `fixture-op` (`cdylib` crate names have their hyphens turned into
/// underscores, matching every other Rust artifact naming rule).
fn dylib_file_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "libfixture_op.dylib"
    } else if cfg!(target_os = "windows") {
        "fixture_op.dll"
    } else {
        "libfixture_op.so"
    }
}

/// Builds the fixture crate and returns the path to its built dylib, or
/// `None` when `cargo` itself could not be started at all (an environment
/// without a toolchain on `PATH` — not a fact about this loader, and must
/// not be reported as one; see `downstream_compile.rs`'s identical
/// `spawn_failure` precedent).
fn build_fixture() -> Option<PathBuf> {
    let dir = scratch_dir("fixture-op");
    write_fixture(&dir);

    let output = match cargo_build(&dir, true) {
        Ok(output) if output.status.success() => output,
        Ok(_) => match cargo_build(&dir, false) {
            Ok(output) => output,
            Err(error) => {
                eprintln!(
                    "skipping the dylib-operators end-to-end test: cargo could not be \
                     started ({error}). This test proves nothing in this environment."
                );
                return None;
            }
        },
        Err(error) => {
            eprintln!(
                "skipping the dylib-operators end-to-end test: cargo could not be started \
                 ({error}). This test proves nothing in this environment."
            );
            return None;
        }
    };

    assert!(
        output.status.success(),
        "the fixture operator crate must build cleanly:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let dylib_path = shared_target_dir().join("debug").join(dylib_file_name());
    assert!(
        dylib_path.exists(),
        "expected a built dylib at {}",
        dylib_path.display()
    );
    Some(dylib_path)
}

/// A daemon plus one already-connected node named `runtime`: `frames` fed
/// from `producer/out`, and node-level outputs named `doubled` and
/// `marker` — `astrs-runtime`'s own `Routing` (see that module's docs)
/// fans an operator's own output onto the node's *same-named* output, so
/// the node must declare both by the exact names the fixture operator's
/// `outputs:` list uses for either send to reach `MockDaemon`'s recorder.
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
        DataId::new("doubled").expect("a valid data id"),
    ))
    .with_output(OutputSpec::new(
        DataId::new("marker").expect("a valid data id"),
    ))
    .with_restart(RestartConfig::never());
    let (node, events) = daemon.connect_node(spec).expect("the mock node connects");
    (daemon, node, events)
}

#[test]
fn a_real_cdylib_operator_is_built_loaded_and_driven_end_to_end() {
    let Some(dylib_path) = build_fixture() else {
        return;
    };

    let (daemon, node, events) = host_harness();
    let node_id = node.id().clone();

    let mut inputs = std::collections::BTreeMap::new();
    inputs.insert(
        "frames".to_owned(),
        astrs_manifest::Input::from_source("producer/out"),
    );
    let mut config_map = std::collections::BTreeMap::new();
    config_map.insert("threshold".to_owned(), serde_json::json!(5));

    let operator_config = OperatorConfig {
        id: "fixture".to_owned(),
        operator: "FixtureOp".to_owned(),
        dylib: Some(dylib_path.display().to_string()),
        wasm: None,
        hub: None,
        inputs,
        outputs: vec!["doubled".to_owned(), "marker".to_owned()],
        config: config_map,
    };

    // No `dataflow_dir`: `dylib_path` is already absolute (it came from
    // `shared_target_dir()`, itself rooted at `std::env::temp_dir()`), and
    // an absolute `dylib:` path is never rebased — see
    // `crate::dylib::resolve_dylib_path`'s own docs.
    let runtime_config = RuntimeConfig::new(vec![operator_config], OperatorRegistry::new());

    let host = RuntimeHost::new(node, events, runtime_config)
        .expect("the host resolves the dylib entry and builds cleanly");
    let runner = std::thread::spawn(move || host.run());

    // `on_start`'s marker send must have crossed the ABI before any input
    // does — proves the lifecycle ordering, not only that data moves.
    let marker_id = DataId::new("marker").expect("a valid data id");
    let started = daemon
        .wait_for_sends(&node_id, &marker_id, 1, Duration::from_secs(30))
        .expect("on_start's marker send arrives");
    assert_eq!(started[0].bytes(), Some(&b"started"[..]));

    daemon
        .send_input(
            &node_id,
            &DataId::new("frames").expect("a valid data id"),
            Metadata::default(),
            vec![1, 2, 3],
        )
        .expect("the daemon accepts the input");

    let doubled_id = DataId::new("doubled").expect("a valid data id");
    let sends = daemon
        .wait_for_sends(&node_id, &doubled_id, 1, Duration::from_secs(30))
        .expect("the dylib operator's on_event answer arrives");
    // `threshold` (5) was delivered through `configure`, crossed the ABI,
    // and was applied inside the loaded library: `byte * 2 + 5`, not just
    // `byte * 2` — proving configuration crossed the boundary, not only
    // raw event payloads.
    assert_eq!(sends[0].bytes(), Some(&[7u8, 9, 11][..]));

    daemon
        .stop(&node_id, StopCause::Requested)
        .expect("the daemon accepts the stop");
    let report = runner
        .join()
        .expect("the host thread does not panic")
        .expect("the host run reports cleanly");
    assert!(report.all_healthy(), "{report:?}");

    let stopped = daemon.sends_on(&node_id, &marker_id);
    assert_eq!(
        stopped.len(),
        2,
        "on_start and on_stop must each have sent exactly one marker"
    );
    assert_eq!(stopped[1].bytes(), Some(&b"stopped"[..]));
}
