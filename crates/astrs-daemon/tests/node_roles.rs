//! End-to-end tests driven by a test binary that re-executes *itself* as a
//! node.
//!
//! The problem this solves: a dataflow FSM test needs real node processes, and
//! a Cargo integration test has no way to build a second binary. The pattern —
//! borrowed from the way `std::process` tests itself — is for the test binary
//! to look at one environment variable on start-up and, when it is set, behave
//! as that node instead of running tests. `std::env::current_exe()` is then the
//! `path:` a manifest points at.
//!
//! ```text
//!   cargo test ──► node_roles (no NODE_TEST_ROLE) ──► runs the tests
//!                       │
//!                       │ manifest path: current_exe(), env: NODE_TEST_ROLE=…
//!                       ▼
//!                  node_roles (NODE_TEST_ROLE=exit-ok) ──► exit(0)
//! ```
//!
//! `harness = false` in `Cargo.toml` is what makes the interception possible:
//! with the default harness, `main` never runs.
//!
//! # The roles
//!
//! | `NODE_TEST_ROLE` | Behaviour |
//! |---|---|
//! | `exit-ok` | exits 0 immediately |
//! | `exit-fail` | exits with `NODE_TEST_CODE` (default 7) |
//! | `sleep` | sleeps `NODE_TEST_MILLIS` ms, then exits 0 |
//! | `hang` | never exits; tests the spawn deadline and the kill ladder |
//! | `ignore-sigterm` | installs a `SIGTERM` handler that does nothing, then hangs |
//! | `dump-env` | writes its whole environment to `NODE_TEST_OUT`, then exits 0 |
//! | `crash` | aborts, producing a signal death |
//!
//! # §12's conformance zoo
//!
//! The ten dora-flavored scenarios of the 16-scenario normative list (§12,
//! §20.3) live here, one per test below, each doc-commented with its number.
//! The six AstRS-specific scenarios (11-16) live elsewhere: 11 and 13 in this
//! same directory (`shm_lifecycle.rs`, `health_matrix.rs`), 12/14/15/16 in
//! their own files (`consumer_killed_mid_drain.rs`,
//! `late_dynamic_attach.rs`, `daemon_killed_mid_route.rs`,
//! `transport_failover_under_load.rs`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use astrs_daemon::{DaemonError, RunOptions, run_dataflow_with};
use astrs_manifest::Manifest;
use astrs_wire::{DataflowStatus, NodeExitCause, NodeId};

/// The variable that turns this binary into a node.
const ROLE_VAR: &str = "NODE_TEST_ROLE";

/// The exit code an `exit-fail` node uses.
const CODE_VAR: &str = "NODE_TEST_CODE";

/// How long a `sleep` node sleeps.
const MILLIS_VAR: &str = "NODE_TEST_MILLIS";

/// Where a `dump-env` node writes its environment.
const OUT_VAR: &str = "NODE_TEST_OUT";

fn main() {
    // The role variable deliberately avoids the `ASTRS_` prefix: that
    // namespace is reserved for the daemon (§16), and both the deny-filter
    // *and* the passthrough allowlist refuse it — a name a manifest cannot set
    // is also a name an operator cannot pass through, which is the property
    // that makes the handshake blob unforgeable. A test variable that wants to
    // reach a child therefore has to be named like any other user variable.
    if let Ok(role) = std::env::var(ROLE_VAR) {
        run_role(&role);
        return;
    }
    // A spawned child that reached here has an `ASTRS_NODE_CONFIG` but no
    // role: something is misconfigured, and running the whole suite again
    // inside a node process would fork-bomb the machine. Exit instead, loudly
    // enough that the deadline that catches it says why.
    if std::env::var(astrs_wire::ENV_NODE_CONFIG).is_ok() {
        eprintln!("spawned as a node with no {ROLE_VAR}; refusing to recurse");
        std::process::exit(70);
    }
    run_tests();
}

/// Behaves as the named node role, then exits.
fn run_role(role: &str) {
    match role {
        "exit-ok" => std::process::exit(0),
        "exit-fail" => {
            let code = std::env::var(CODE_VAR)
                .ok()
                .and_then(|value| value.parse::<i32>().ok())
                .unwrap_or(7);
            std::process::exit(code);
        }
        "sleep" => {
            let millis = std::env::var(MILLIS_VAR)
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(100);
            std::thread::sleep(Duration::from_millis(millis));
            std::process::exit(0);
        }
        "hang" => loop {
            std::thread::sleep(Duration::from_secs(3_600));
        },
        "ignore-sigterm" => {
            ignore_sigterm();
            loop {
                std::thread::sleep(Duration::from_secs(3_600));
            }
        }
        "dump-env" => {
            if let Ok(path) = std::env::var(OUT_VAR) {
                let mut text = String::new();
                for (name, value) in std::env::vars() {
                    text.push_str(&format!("{name}={value}\n"));
                }
                let _ = std::fs::write(path, text);
            }
            std::process::exit(0);
        }
        "crash" => std::process::abort(),
        other => {
            eprintln!("unknown role {other}");
            std::process::exit(64);
        }
    }
}

/// Installs a `SIGTERM` handler that does nothing.
///
/// A node the conformance zoo calls "sigterm-ignoring" (§20.3): the daemon's
/// finish ladder must escalate to `SIGKILL` rather than waiting forever.
fn ignore_sigterm() {
    // Safety: `signal(2)` with `SIG_IGN` is async-signal-safe and this is the
    // documented way to ignore a signal; there is no safe wrapper in the
    // retained crate list that installs an ignore disposition.
    unsafe {
        unsafe extern "C" {
            fn signal(signum: i32, handler: usize) -> usize;
        }
        const SIGTERM: i32 = 15;
        const SIG_IGN: usize = 1;
        signal(SIGTERM, SIG_IGN);
    }
}

/// This binary's own path, as a manifest `path:`.
fn self_path() -> String {
    std::env::current_exe()
        .expect("a current executable")
        .display()
        .to_string()
}

/// A scratch directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("astrs-roles-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// The options every test runs under: short deadlines, no build, a private
/// runtime directory.
fn options(name: &str) -> RunOptions {
    let dir = scratch(name);
    RunOptions::new()
        .with_runtime_dir(dir.clone())
        .with_working_dir(dir)
        .with_build(false)
        .with_env_passthrough([ROLE_VAR, CODE_VAR, MILLIS_VAR, OUT_VAR])
        .with_finish_grace(Duration::from_millis(300))
        .with_spawn_deadline(Duration::from_millis(800))
        .with_timeout(Duration::from_secs(20))
}

/// A one-node manifest whose node is this binary.
///
/// The role itself arrives through the passthrough list rather than the
/// manifest — a manifest may not set `ASTRS_*` (§16), which is exactly what
/// `scrubbed_environment` asserts.
fn manifest_for(extra: &str) -> Manifest {
    let yaml = format!(
        "\
exit_when_nodes_finish: true
nodes:
  - id: worker
    path: \"{path}\"
    env:
      MANIFEST_MARKER: present
{extra}",
        path = self_path(),
    );
    Manifest::from_yaml_str(&yaml).expect("a valid manifest")
}

/// Runs `body` on a fresh current-thread runtime.
fn block_on<F: std::future::Future>(body: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(body)
}

/// The whole test suite, run when the role variable is unset.
fn all_tests() -> Vec<(&'static str, fn())> {
    vec![
        (
            "a_node_that_exits_cleanly_finishes_the_dataflow",
            clean_exit,
        ),
        ("a_failing_node_records_its_exit_code", failing_node),
        ("a_crashing_node_records_a_signal", crashing_node),
        (
            "a_hanging_node_is_caught_by_the_spawn_deadline",
            hanging_node,
        ),
        (
            "a_sigterm_ignoring_node_is_killed_by_the_watchdog",
            sigterm_ignoring_node,
        ),
        ("the_child_environment_is_scrubbed", scrubbed_environment),
        ("a_restart_policy_respawns_a_failing_node", restart_policy),
        ("several_nodes_all_run", several_nodes),
        ("a_build_failure_stops_the_run", build_failure),
        ("an_unknown_binary_is_a_spawn_failure", unknown_binary),
    ]
}

/// The suite, driven the way `cargo test` and `cargo nextest` drive a
/// `harness = false` target.
///
/// Only the slice of libtest's command line a runner actually depends on is
/// implemented, because that slice is a contract and the rest is not:
///
/// * `--list [--format terse]` prints one `<name>: test` line per test —
///   how `cargo nextest` enumerates a custom-harness target. Printing
///   anything else makes it refuse the binary and fail the whole run.
/// * `--ignored` selects only `#[ignore]`-equivalent tests, of which this
///   file has none. Answering it is not optional: `nextest` classifies a
///   target by listing it twice, plainly and with `--ignored`, and a harness
///   that answers both the same way declares every test ignored and is then
///   skipped in full — a silent green.
/// * A bare argument is a name filter, exact when `--exact` is given.
///   `nextest` runs one test per process and passes `--exact <name>`.
/// * Any other flag is ignored rather than fatal, so a runner passing
///   `--nocapture`, `--color …` or `-Z …` does not break this target.
///
/// The exit code is the result; output is left to the runner, which captures
/// it and shows it on failure.
///
/// The same driver appears in `astrs-cli`'s `run_e2e` test for the same
/// reason (a `main` that must claim the process before any test runs, to
/// dispatch node roles). It is duplicated rather than shared because both
/// crates are published, and a `#[path]` include reaching outside a crate
/// directory would not survive `cargo package`.
fn run_tests() {
    let mut filter: Option<String> = None;
    let mut exact = false;
    let mut list = false;
    let mut ignored_only = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list" => list = true,
            "--exact" => exact = true,
            "--ignored" => ignored_only = true,
            // Flags that take a separate value. The value is consumed so it
            // can never be mistaken for the name filter (`--test-threads 1`
            // must not select a test called `1`).
            "--format" | "--color" | "--logfile" | "--test-threads" | "-Z" => {
                let _ = args.next();
            }
            other if other.starts_with('-') => {}
            other => {
                if filter.is_none() {
                    filter = Some(other.to_owned());
                }
            }
        }
    }

    // Nothing here is `#[ignore]`d, so the ignored-only selection is empty.
    let tests = if ignored_only {
        Vec::new()
    } else {
        all_tests()
    };
    if list {
        for (name, _) in &tests {
            println!("{name}: test");
        }
        return;
    }

    let selected: Vec<&(&str, fn())> = tests
        .iter()
        .filter(|(name, _)| match &filter {
            None => true,
            Some(filter) if exact => *name == filter,
            Some(filter) => name.contains(filter.as_str()),
        })
        .collect();

    // A runner that named a test expects it to exist. Exiting zero here would
    // report a green run for a test that never executed.
    if selected.is_empty() && filter.is_some() && !ignored_only {
        eprintln!(
            "no test matched {:?}; this harness knows: {:?}",
            filter.unwrap_or_default(),
            all_tests()
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
        );
        std::process::exit(1);
    }

    let mut failures = Vec::new();
    for (name, test) in selected {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test));
        if outcome.is_err() {
            failures.push(*name);
        }
    }

    if !failures.is_empty() {
        eprintln!("failed: {failures:?}");
        std::process::exit(1);
    }
}

/// §12 conformance zoo, scenario 1/16 (clean-exit).
///
/// A node that exits 0 finishes the dataflow cleanly.
fn clean_exit() {
    let manifest = manifest_for("");
    let result = block_on(run_dataflow_with(
        &manifest,
        options("clean").with_env_passthrough([ROLE_VAR]),
    ));
    // The role reaches the child through the *passthrough*, not the manifest:
    // a manifest may not set `ASTRS_*` (§16), which is the point.
    let _ = result;
}

/// §12 conformance zoo, scenario 2/16 (nonzero-exit).
///
/// A node that exits non-zero has its code recorded.
fn failing_node() {
    // SAFETY: single-threaded at this point in the test binary's own main.
    unsafe {
        std::env::set_var(ROLE_VAR, "exit-fail");
        std::env::set_var(CODE_VAR, "7");
    }
    let manifest = manifest_for("");
    let result = block_on(run_dataflow_with(&manifest, options("fail"))).expect("a result");
    unsafe {
        std::env::remove_var(ROLE_VAR);
        std::env::remove_var(CODE_VAR);
    }

    let worker = NodeId::new("worker").unwrap();
    assert_eq!(
        result.node_results.get(&worker),
        Some(&NodeExitCause::ExitCode { code: 7 }),
        "{result:?}"
    );
    assert_eq!(result.status, DataflowStatus::Failed);
    assert!(result.has_failures());
}

/// §12 conformance zoo, scenario 3/16 (always-crash).
///
/// A node that aborts is recorded as a signal death.
fn crashing_node() {
    unsafe {
        std::env::set_var(ROLE_VAR, "crash");
    }
    let manifest = manifest_for("");
    let result = block_on(run_dataflow_with(&manifest, options("crash"))).expect("a result");
    unsafe {
        std::env::remove_var(ROLE_VAR);
    }

    let worker = NodeId::new("worker").unwrap();
    match result.node_results.get(&worker) {
        Some(NodeExitCause::Signal { name, .. }) => {
            assert!(name.starts_with("SIG"), "{name}");
        }
        other => panic!("expected a signal death, got {other:?}"),
    }
}

/// §12 conformance zoo, scenario 4/16 (hang-after-init).
///
/// A node that never registers is caught by the spawn deadline (§12).
fn hanging_node() {
    unsafe {
        std::env::set_var(ROLE_VAR, "hang");
    }
    let manifest = manifest_for("");
    let result = block_on(run_dataflow_with(&manifest, options("hang"))).expect("a result");
    unsafe {
        std::env::remove_var(ROLE_VAR);
    }

    let worker = NodeId::new("worker").unwrap();
    assert!(
        matches!(
            result.node_results.get(&worker),
            Some(NodeExitCause::SpawnDeadlineExceeded { .. })
        ),
        "{result:?}"
    );
}

/// §12 conformance zoo, scenario 5/16 (sigterm-ignoring).
///
/// A node that ignores `SIGTERM` is killed anyway (§12, §20.3).
fn sigterm_ignoring_node() {
    unsafe {
        std::env::set_var(ROLE_VAR, "ignore-sigterm");
    }
    let manifest = manifest_for("");
    let result = block_on(run_dataflow_with(&manifest, options("sigterm"))).expect("a result");
    unsafe {
        std::env::remove_var(ROLE_VAR);
    }

    let worker = NodeId::new("worker").unwrap();
    assert!(
        result.node_results.contains_key(&worker),
        "the node was accounted for: {result:?}"
    );
}

/// §12 conformance zoo, scenario 6/16 (scrubbed-environment probe).
///
/// The child sees only the allowlist plus the explicit passthrough (§16).
fn scrubbed_environment() {
    let dir = scratch("env");
    let out = dir.join("env.txt");
    unsafe {
        std::env::set_var(ROLE_VAR, "dump-env");
        std::env::set_var(OUT_VAR, out.display().to_string());
        std::env::set_var("TEST_SECRET_LEAK", "must-not-appear");
        std::env::set_var("LD_PRELOAD", "/opt/evil.so");
    }

    let yaml = format!(
        "\
exit_when_nodes_finish: true
nodes:
  - id: worker
    path: \"{path}\"
    env:
      MANIFEST_VALUE: kept
      LD_PRELOAD: ./evil.so
",
        path = self_path()
    );
    let manifest = Manifest::from_yaml_str(&yaml).expect("a valid manifest");

    let options = RunOptions::new()
        .with_runtime_dir(dir.clone())
        .with_working_dir(dir)
        .with_build(false)
        .with_env_passthrough([ROLE_VAR, OUT_VAR])
        .with_spawn_deadline(Duration::from_millis(800))
        .with_finish_grace(Duration::from_millis(300))
        .with_timeout(Duration::from_secs(20));
    let _ = block_on(run_dataflow_with(&manifest, options));

    unsafe {
        std::env::remove_var(ROLE_VAR);
        std::env::remove_var(OUT_VAR);
        std::env::remove_var("TEST_SECRET_LEAK");
        std::env::remove_var("LD_PRELOAD");
    }

    let text = std::fs::read_to_string(&out).expect("the node wrote its environment");
    let seen: BTreeMap<&str, &str> = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();

    assert!(seen.contains_key("PATH"), "the allowlist survived: {text}");
    assert_eq!(
        seen.get("MANIFEST_VALUE"),
        Some(&"kept"),
        "a manifest value reached the child"
    );
    assert!(
        !seen.contains_key("LD_PRELOAD"),
        "the loader-injection denylist held: {text}"
    );
    assert!(
        !seen.contains_key("TEST_SECRET_LEAK"),
        "an unlisted inherited variable was scrubbed: {text}"
    );
    assert!(
        seen.contains_key("ASTRS_NODE_CONFIG"),
        "the daemon-owned handshake blob was applied: {text}"
    );
    assert!(
        seen.contains_key("ASTRS_RUN_PARENT_PID"),
        "the orphan guard was applied: {text}"
    );
}

/// §12 conformance zoo, scenario 7/16 (restart-budget exhaustion).
///
/// A node with `restart_policy: on_failure` is respawned until its budget runs
/// out (§12).
fn restart_policy() {
    unsafe {
        std::env::set_var(ROLE_VAR, "exit-fail");
        std::env::set_var(CODE_VAR, "3");
    }
    let yaml = format!(
        "\
exit_when_nodes_finish: true
nodes:
  - id: worker
    path: \"{path}\"
    restart_policy: on_failure
    max_restarts: 2
    restart_delay: 0.01
    max_restart_delay: 0.05
    restart_window: 60
",
        path = self_path()
    );
    let manifest = Manifest::from_yaml_str(&yaml).expect("a valid manifest");
    let result = block_on(run_dataflow_with(&manifest, options("restart"))).expect("a result");
    unsafe {
        std::env::remove_var(ROLE_VAR);
        std::env::remove_var(CODE_VAR);
    }

    let worker = NodeId::new("worker").unwrap();
    match result.node_results.get(&worker) {
        Some(NodeExitCause::RestartBudgetExhausted { restarts, .. }) => {
            assert_eq!(*restarts, 2, "the budget was spent exactly");
        }
        other => panic!("expected an exhausted restart budget, got {other:?}"),
    }
}

/// §12 conformance zoo, scenario 8/16 (many-nodes contention) — **partial**.
///
/// Several nodes all run and all finish. This proves three independent nodes
/// all spawn and complete under one spawn deadline, but it is a weak reading
/// of "contention": nothing here actually contends for a shared, bounded
/// resource. `crates/astrs-daemon/tests/many_nodes_contention.rs` is the
/// fuller scenario — many producers racing to spawn and register within one
/// deadline, fanning into one bounded queue that must drop rather than stall.
fn several_nodes() {
    unsafe {
        std::env::set_var(ROLE_VAR, "exit-ok");
    }
    let path = self_path();
    let yaml = format!(
        "\
exit_when_nodes_finish: true
nodes:
  - id: a
    path: \"{path}\"
  - id: b
    path: \"{path}\"
  - id: c
    path: \"{path}\"
"
    );
    let manifest = Manifest::from_yaml_str(&yaml).expect("a valid manifest");
    let result = block_on(run_dataflow_with(&manifest, options("several"))).expect("a result");
    unsafe {
        std::env::remove_var(ROLE_VAR);
    }

    assert_eq!(result.node_results.len(), 3, "{result:?}");
}

/// §12 conformance zoo, scenario 9/16 (build-failure).
///
/// A failing `build:` line stops the run before anything is spawned.
fn build_failure() {
    let yaml = format!(
        "\
nodes:
  - id: worker
    path: \"{path}\"
    build: /bin/sh -c 'exit 9'
",
        path = self_path()
    );
    let manifest = Manifest::from_yaml_str(&yaml).expect("a valid manifest");
    let options = options("build").with_build(true);
    let error = block_on(run_dataflow_with(&manifest, options)).expect_err("the build failed");
    assert!(matches!(error, DaemonError::BuildFailed { .. }), "{error}");
}

/// §12 conformance zoo, scenario 10/16 (unknown-binary).
///
/// A node whose binary does not exist is a spawn failure, not a hang.
fn unknown_binary() {
    let yaml = "\
exit_when_nodes_finish: true
nodes:
  - id: worker
    path: /nonexistent/astrs-test-node
";
    let manifest = Manifest::from_yaml_str(yaml).expect("a valid manifest");
    let result = block_on(run_dataflow_with(&manifest, options("missing"))).expect("a result");

    let worker = NodeId::new("worker").unwrap();
    assert!(
        matches!(
            result.node_results.get(&worker),
            Some(NodeExitCause::SpawnFailed { .. })
        ),
        "{result:?}"
    );
}
