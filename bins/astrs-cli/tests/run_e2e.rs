//! End-to-end proof for `astrs run`: a tiny two-node graph
//! (publisher/subscriber) run twice over -- once through
//! `astrs_daemon::run_dataflow_with` directly, and once through
//! `astrs_cli::command::run::run`, the verb that embeds it -- with the nodes
//! played by this very test binary in helper mode.
//!
//! | Test | Proves |
//! |---|---|
//! | `two_nodes_exchange_messages_and_finish_cleanly` | the engine: registration, publish/subscribe, a clean `DataflowResult` |
//! | `a_crashing_node_exhausts_its_restart_budget_then_fails` | the restart budget (§12) and the failure it ends in |
//! | `astrs_run_streams_node_output_and_exits_zero` | the verb: captured node output on the terminal, exit code `0` |
//! | `astrs_run_surfaces_a_restarted_node_s_final_failure` | the verb: a restart announced live, exit code `1` |
//! | `astrs_run_stops_a_long_lived_graph_on_an_interrupt` | the verb: Ctrl-C → graceful stop → a real verdict (§17) |
//!
//! The env-var role-switch pattern is borrowed from
//! `astrs-daemon/tests/node_roles.rs`: `harness = false` (see this crate's
//! `Cargo.toml`) means `main` runs unconditionally, so the binary can look at
//! one environment variable and behave as a graph node instead of running
//! tests. `std::env::current_exe()` is then a `path:` a manifest can point
//! at.
//!
//! Unlike `node_roles.rs` (whose roles `exit`/`sleep`/`crash`/... never
//! register as real nodes), this file's roles connect for real via
//! `astrs_node_api::Node::init_from_env`, publish, receive, and honor a
//! `Stop` -- proving the embedded daemon and the node client actually
//! interoperate for message exchange and for graceful shutdown, not just for
//! process supervision. Nothing else in this workspace exercised that
//! combination before this test existed.
//!
//! ```text
//!   cargo test --test run_e2e ──► run_e2e (no CLI_E2E_ROLE) ──► runs the tests
//!                                      │
//!                                      │ manifest path: current_exe(),
//!                                      │ env: CLI_E2E_ROLE=publisher|subscriber
//!                                      ▼
//!                                 run_e2e (CLI_E2E_ROLE=…) ──► node behavior
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use astrs_daemon::{RunOptions, run_dataflow_with};
use astrs_manifest::Manifest;
use astrs_node_api::{Event, Node};
use astrs_wire::{DataflowStatus, Metadata, NodeExitCause, NodeId};

/// The variable that turns this binary into a graph node instead of a test
/// runner. Deliberately outside the `ASTRS_` namespace -- see
/// `node_roles.rs`'s identical note: that prefix is reserved for the
/// daemon's own handshake plumbing (blueprint §16) and is denylisted from a
/// manifest's `env:` block, so a role a manifest sets has to be named like
/// any other user variable.
const ROLE_VAR: &str = "CLI_E2E_ROLE";

/// Where the subscriber role writes what it received, newline-separated.
const OUT_VAR: &str = "CLI_E2E_OUT";

/// The rendezvous file the subscriber creates once it is registered and the
/// publisher waits for before sending anything.
///
/// Not ceremony: `astrs-daemon`'s local router *drops* a message whose
/// consumer has no mailbox yet (`local::router`'s own comment: "a consumer
/// with no mailbox has not registered yet ... the same situation as a full
/// queue"), which is standard publish/subscribe semantics and matches dora.
/// A publisher that starts a few milliseconds ahead of its subscriber
/// therefore legitimately loses those messages, and a test asserting that
/// *all three* arrive has to rendezvous first — otherwise it is asserting a
/// scheduling coincidence rather than the data path.
const READY_VAR: &str = "CLI_E2E_READY";

/// Where a role writes a one-line description of a *fatal* setup failure.
///
/// A node process's own stderr is piped to (and captured by) the daemon, so
/// a role that cannot even register has nowhere else to say why: this file
/// is the only channel that survives into a failing assertion's message.
const DEBUG_VAR: &str = "CLI_E2E_DEBUG";

/// Where both roles append a checkpoint line as they progress.
///
/// Purely diagnostic, and read **only** into a failing assertion's message
/// (see [`diagnostics`]): this target is `harness = false`, so anything
/// printed here is uncaptured and would appear on every green run.
const TRACE_VAR: &str = "CLI_E2E_TRACE";

/// The rendezvous file the subscriber creates once it has received
/// everything, and the publisher waits for before exiting.
///
/// The second half of the same problem [`READY_VAR`] solves. A publisher
/// that exits the instant after its last `send` ends the graph
/// (`exit_when_nodes_finish:`) while its consumer may still have queued
/// messages *and* while `Stop` — which pre-empts queued data on the control
/// lane, by `astrs-node-api`'s design — is on its way to that consumer.
/// Observed directly: a subscriber that had registered before the first
/// send still saw `input, Stop` instead of `input, input, input`. A real
/// graph's nodes are long-lived, so a fixture whose producer outlives its
/// consumer's delivery is both more realistic and the only way to assert
/// "every message arrives" without asserting a scheduling coincidence.
const DONE_VAR: &str = "CLI_E2E_DONE";

/// How long the publisher waits for its subscriber to register.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// How many messages the publisher sends and the subscriber expects.
const MESSAGE_COUNT: usize = 3;

/// How long the `blocker` role stays alive when nothing ever stops it.
///
/// A backstop, never the mechanism: the test that uses this role expects a
/// `Stop` within a second or two. Bounding it anyway means a bug in the
/// stop path costs one slow test rather than a node left running on a
/// developer's machine after the suite has exited.
const BLOCKER_LIFETIME: Duration = Duration::from_secs(60);

/// The whole-run ceiling the interrupt test hands `astrs run`.
///
/// Deliberately far above how long the interrupt path should take, and
/// asserted against: a run that ended *near* this value ended because the
/// ceiling elapsed, which is not the thing under test.
const BACKSTOP_TIMEOUT: Duration = Duration::from_secs(45);

fn main() {
    if let Ok(role) = std::env::var(ROLE_VAR) {
        run_role(&role);
        return;
    }
    // A spawned child with a handshake blob but no assigned role is
    // misconfigured, not a reason to run the whole suite again inside a node
    // process (which would fork-bomb the machine) -- see `node_roles.rs`.
    if std::env::var(astrs_wire::ENV_NODE_CONFIG).is_ok() {
        eprintln!("spawned as a node with no {ROLE_VAR}; refusing to recurse");
        std::process::exit(70);
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("error"))
        .try_init();
    run_tests();
}

/// Behaves as the named node role, then exits.
fn run_role(role: &str) {
    match role {
        "publisher" => publisher(),
        "subscriber" => subscriber(),
        "publisher-crash" => publisher_crash(),
        "blocker" => blocker(),
        other => {
            eprintln!("unknown role {other}");
            std::process::exit(64);
        }
    }
}

/// Reports a fatal setup failure and exits: to [`DEBUG_VAR`]'s file when the
/// caller set one (this process's own stderr is piped and captured by the
/// daemon, not shown by a test runner), and to stderr regardless.
fn debug_fail(role: &str, err: impl std::fmt::Display) -> ! {
    if let Ok(path) = std::env::var(DEBUG_VAR) {
        let _ = std::fs::write(&path, format!("{role}: {err}"));
    }
    eprintln!("{role}: {err}");
    std::process::exit(65);
}

/// Appends one progress line to [`TRACE_VAR`]'s file, when the caller set
/// one.
///
/// Append rather than truncate, and one shared file for both roles: the
/// interleaving of the publisher's and the subscriber's checkpoints is
/// exactly what makes a rendezvous bug readable afterwards. Every failure
/// here is swallowed — a node whose diagnostics cannot be written must still
/// behave as the graph node it was spawned to be.
fn checkpoint(role: &str, message: &str) {
    let Ok(path) = std::env::var(TRACE_VAR) else {
        return;
    };
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{role}: {message}");
    }
}

/// Everything the node roles recorded, gathered into one block for a
/// *failing* assertion's message.
///
/// Read lazily and never printed on its own. `assert!`/`assert_eq!` only
/// evaluate their format arguments when the assertion fails, so calling this
/// in an assertion message costs nothing on a green run — which matters here
/// more than usual, because `harness = false` means a `println!` in this file
/// is uncaptured and lands in the runner's output every single time.
fn diagnostics(paths: &[&Path]) -> String {
    let mut sections = Vec::new();
    for path in paths {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.trim().is_empty()
        {
            sections.push(format!("--- {} ---\n{}", path.display(), text.trim_end()));
        }
    }
    if sections.is_empty() {
        return "(the nodes recorded no diagnostics)".to_owned();
    }
    sections.join("\n")
}

/// Registers, sends [`MESSAGE_COUNT`] messages on output `out`, closes it,
/// and exits cleanly.
fn publisher() {
    checkpoint("publisher", "starting init_from_env");
    let (mut node, _events) =
        Node::init_from_env().unwrap_or_else(|err| debug_fail("publisher", err));
    checkpoint("publisher", "init_from_env done");
    let mut output = node
        .raw_output("out")
        .unwrap_or_else(|err| debug_fail("publisher", err));
    checkpoint("publisher", "raw_output done");
    wait_for_subscriber();
    checkpoint("publisher", "subscriber is registered");
    for i in 0..MESSAGE_COUNT {
        let payload = format!("msg-{i}").into_bytes();
        let meta = Metadata::new(node.hlc_now());
        checkpoint("publisher", &format!("about to send {i}"));
        if let Err(err) = output.send_bytes(payload, meta) {
            debug_fail("publisher", err);
        }
        // Captured by the daemon and forwarded to whatever `ReportSink` is
        // installed -- which is how `astrs run` puts a node's own output on
        // the terminal. The CLI-level test below asserts on exactly this.
        println!("published msg-{i}");
        checkpoint("publisher", &format!("sent {i}"));
    }
    checkpoint("publisher", "closing");
    let _ = output.close();
    wait_for_delivery();
    checkpoint("publisher", "exiting");
    std::process::exit(0);
}

/// Blocks until the subscriber says it has registered (see [`READY_VAR`]),
/// or gives up loudly rather than publishing into a void.
fn wait_for_subscriber() {
    let Ok(path) = std::env::var(READY_VAR) else {
        return;
    };
    let deadline = std::time::Instant::now() + READY_TIMEOUT;
    while !std::path::Path::new(&path).exists() {
        if std::time::Instant::now() >= deadline {
            debug_fail(
                "publisher",
                format!("the subscriber never registered (no {path} after {READY_TIMEOUT:?})"),
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Blocks until the subscriber says it has received everything (see
/// [`DONE_VAR`]), so this process's exit never ends the graph early.
///
/// Bounded: if the subscriber never finishes, this returns anyway and the
/// test fails on what the subscriber actually wrote — a far more useful
/// failure than a hung suite.
fn wait_for_delivery() {
    let Ok(path) = std::env::var(DONE_VAR) else {
        return;
    };
    let deadline = std::time::Instant::now() + READY_TIMEOUT;
    while !std::path::Path::new(&path).exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A restart-policy fixture: sends nothing and exits non-zero every time,
/// so the daemon's restart budget (§12) is what eventually ends the run.
fn publisher_crash() {
    std::process::exit(9);
}

/// A long-lived node: registers, announces itself through [`READY_VAR`], and
/// then does nothing but wait to be asked to stop.
///
/// The fixture for `astrs run`'s Ctrl-C path. A graph whose nodes exit on
/// their own proves nothing about an interrupt — it would have ended anyway
/// — so the node this test runs ends *only* because the daemon delivered
/// [`Event::Stop`], which is exactly what one `SIGINT` on `astrs run` turns
/// into (blueprint §17, and `command::run`'s `cancel` watch).
///
/// Non-`Stop` events are ignored rather than acted on: a node with no inputs
/// is told `AllInputsClosed` almost immediately, and treating that as a
/// reason to exit would make this fixture race the interrupt it exists to
/// wait for.
fn blocker() {
    checkpoint("blocker", "starting init_from_env");
    let (_node, mut events) =
        Node::init_from_env().unwrap_or_else(|err| debug_fail("blocker", err));
    checkpoint("blocker", "init_from_env done");
    // Registered: from here on the daemon knows about this node and can
    // deliver a `Stop` to it, so the test may safely raise its signal.
    if let Ok(path) = std::env::var(READY_VAR) {
        std::fs::write(&path, "ready").unwrap_or_else(|err| debug_fail("blocker", err));
    }
    checkpoint("blocker", "announced readiness");

    let deadline = std::time::Instant::now() + BLOCKER_LIFETIME;
    while std::time::Instant::now() < deadline {
        match events.recv() {
            Some(event) if event.is_stop() => {
                checkpoint("blocker", "asked to stop; exiting cleanly");
                break;
            }
            Some(other) => checkpoint("blocker", &format!("ignoring {other}")),
            // The stream has fused or the session ended. Sleeping rather
            // than spinning keeps this a *waiting* process (which the
            // daemon's stop ladder can still reach) instead of a busy one.
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    std::process::exit(0);
}

/// Registers, receives [`MESSAGE_COUNT`] input events on `in`, writes what
/// it saw to [`OUT_VAR`] (when set), and exits cleanly.
fn subscriber() {
    checkpoint("subscriber", "starting init_from_env");
    let (_node, mut events) =
        Node::init_from_env().unwrap_or_else(|err| debug_fail("subscriber", err));
    checkpoint("subscriber", "init_from_env done");
    // Registered: the daemon now has a mailbox for this node, so a message
    // published from here on has somewhere to land (see [`READY_VAR`]).
    if let Ok(path) = std::env::var(READY_VAR) {
        std::fs::write(&path, "ready").unwrap_or_else(|err| debug_fail("subscriber", err));
    }
    checkpoint("subscriber", "announced readiness");
    let mut received: Vec<String> = Vec::new();
    let mut log: Vec<String> = Vec::new();
    while received.len() < MESSAGE_COUNT {
        checkpoint("subscriber", "calling recv");
        match events.recv() {
            Some(Event::Input { data, .. }) => {
                checkpoint("subscriber", &format!("got input len={}", data.len()));
                log.push(format!("input {}", data.len()));
                let text = String::from_utf8_lossy(&data.to_vec()).into_owned();
                println!("received {text}");
                received.push(text);
            }
            Some(other) => {
                checkpoint("subscriber", &format!("got other: {other:?}"));
                log.push(format!("other: {other}"));
                if other.is_stop() {
                    break;
                }
            }
            None => {
                checkpoint("subscriber", "got none (stream ended)");
                log.push("stream ended".to_owned());
                break;
            }
        }
    }
    if let Ok(path) = std::env::var(DEBUG_VAR) {
        let _ = std::fs::write(path, log.join("\n"));
    }
    if let Ok(path) = std::env::var(OUT_VAR) {
        let _ = std::fs::write(path, received.join("\n"));
    }
    // Written last, and only once the output file is on disk: the publisher
    // waits for this before exiting (see [`DONE_VAR`]).
    if let Ok(path) = std::env::var(DONE_VAR) {
        let _ = std::fs::write(path, format!("{}", received.len()));
    }
    std::process::exit(0);
}

/// This binary's own path, as a manifest `path:`.
fn self_path() -> String {
    std::env::current_exe()
        .expect("a current executable")
        .display()
        .to_string()
}

/// A fresh scratch directory with a deliberately *short* name, for a test
/// whose directory also holds the embedded daemon's Unix socket.
///
/// `run_dataflow_with` puts a `run-<pid>-<hex>.sock` inside the runtime
/// directory, and a Unix socket path may be at most 103 bytes -- which a
/// long macOS `TMPDIR` plus a descriptive directory name exceeds on its
/// own. (The daemon reports that clearly rather than failing obscurely;
/// this helper simply stays under the limit.)
fn short_scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("as-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// A fresh scratch directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("astrs-cli-run-e2e-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// Runs `body` on a fresh current-thread runtime, matching
/// `node_roles.rs`'s own helper.
fn block_on<F: std::future::Future>(body: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(body)
}

fn options(name: &str) -> RunOptions {
    let dir = scratch(name);
    RunOptions::new()
        .with_runtime_dir(dir.clone())
        .with_working_dir(dir)
        .with_build(false)
        .with_finish_grace(Duration::from_millis(500))
        .with_spawn_deadline(Duration::from_secs(5))
        .with_timeout(Duration::from_secs(20))
}

/// Every test in this file, by the name a runner reports it under.
fn all_tests() -> Vec<(&'static str, fn())> {
    vec![
        (
            "two_nodes_exchange_messages_and_finish_cleanly",
            pubsub_exchange,
        ),
        (
            "a_crashing_node_exhausts_its_restart_budget_then_fails",
            crash_then_restart_exhausted,
        ),
        (
            "astrs_run_streams_node_output_and_exits_zero",
            cli_run_streams_and_exits_zero,
        ),
        (
            "astrs_run_surfaces_a_restarted_node_s_final_failure",
            cli_run_reports_a_restart_then_a_failure,
        ),
        (
            "astrs_run_stops_a_long_lived_graph_on_an_interrupt",
            cli_run_stops_on_an_interrupt,
        ),
    ]
}

/// The suite, driven the way `cargo test` and `cargo nextest` drive a
/// `harness = false` target (run when [`ROLE_VAR`] is unset).
///
/// Only the slice of libtest's command line that a runner actually depends on
/// is implemented, because that slice is a contract and the rest is not:
///
/// * `--list [--format terse]` prints one `<name>: test` line per test. This
///   is how `cargo nextest` enumerates a custom-harness target; printing
///   anything else here is what makes it refuse the whole binary.
/// * `--ignored` selects only `#[ignore]`-equivalent tests — of which this
///   file has none, so it selects nothing. Answering it is not optional:
///   `nextest` classifies a target by listing it *twice*, once plainly and
///   once with `--ignored`, and a harness that returns the same list both
///   times declares every one of its tests ignored and is then silently
///   skipped in full.
/// * A bare argument is a name filter, exact when `--exact` is given.
///   `nextest` runs one test per process and passes exactly that.
/// * Any other flag is ignored rather than fatal: runners pass `--nocapture`,
///   `--color`, `-Z unstable-options` and more, and a custom harness that
///   rejects an unknown flag breaks on the next runner release.
///
/// The exit code is the result: zero when every selected test passed. Output
/// is left to the runner, which captures it and shows it on failure.
///
/// The same driver appears in `astrs-daemon`'s `node_roles` test for the same
/// reason. It is duplicated rather than shared because both crates are
/// published, and a `#[path]` include reaching outside a crate directory would
/// not survive `cargo package`.
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
            // Flags that take a separate value. The value is consumed here so
            // it can never be mistaken for the name filter (`--test-threads 1`
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

    // Nothing here is `#[ignore]`d, so the ignored-only selection is empty —
    // both to list and to run.
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

    // A runner that named a test expects that test to exist. Exiting zero
    // here would report a green run for a test that never executed, which is
    // the one failure mode a custom harness must never have.
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

/// The load-bearing proof: two real nodes, real registration, real
/// publish/subscribe, a clean finish, and a `DataflowResult` that says so.
fn pubsub_exchange() {
    let dir = scratch("pubsub");
    let out = dir.join("received.txt");
    let ready = dir.join("sub.ready");
    let done = dir.join("sub.done");
    let pub_debug = dir.join("pub.debug");
    let sub_debug = dir.join("sub.debug");
    let trace = dir.join("trace.log");
    let path = self_path();
    let yaml = format!(
        "\
exit_when_nodes_finish: true
nodes:
  - id: pub
    path: \"{path}\"
    outputs: [out]
    env:
      {role_var}: publisher
      {ready_var}: \"{ready}\"
      {done_var}: \"{done}\"
      {debug_var}: \"{pub_debug}\"
      {trace_var}: \"{trace}\"
  - id: sub
    path: \"{path}\"
    inputs:
      in: pub/out
    env:
      {role_var}: subscriber
      {out_var}: \"{out}\"
      {ready_var}: \"{ready}\"
      {done_var}: \"{done}\"
      {debug_var}: \"{sub_debug}\"
      {trace_var}: \"{trace}\"
",
        role_var = ROLE_VAR,
        out_var = OUT_VAR,
        ready_var = READY_VAR,
        ready = ready.display(),
        done_var = DONE_VAR,
        done = done.display(),
        out = out.display(),
        debug_var = DEBUG_VAR,
        pub_debug = pub_debug.display(),
        sub_debug = sub_debug.display(),
        trace_var = TRACE_VAR,
        trace = trace.display(),
    );
    let manifest = Manifest::from_yaml_str(&yaml).expect("a valid manifest");
    let result = block_on(run_dataflow_with(&manifest, options("pubsub"))).expect("a result");

    // Evaluated only when an assertion below actually fails (see
    // [`diagnostics`]) — a green run prints nothing at all.
    let notes = || diagnostics(&[&pub_debug, &sub_debug, &trace]);
    assert_eq!(
        result.status,
        DataflowStatus::Finished,
        "expected a clean finish: {result:?}\n{}",
        notes()
    );
    assert!(!result.has_failures(), "{result:?}\n{}", notes());
    assert_eq!(result.node_results.len(), 2, "{result:?}\n{}", notes());
    for id in ["pub", "sub"] {
        let cause = result
            .node_results
            .get(&NodeId::new(id).unwrap())
            .unwrap_or_else(|| panic!("no result recorded for {id}: {result:?}\n{}", notes()));
        assert!(
            cause.is_success(),
            "{id} exited abnormally: {cause}\n{}",
            notes()
        );
    }

    let text = std::fs::read_to_string(&out).unwrap_or_else(|err| {
        panic!(
            "the subscriber never wrote {}: {err}\n{}",
            out.display(),
            notes()
        )
    });
    let received: Vec<&str> = text.lines().collect();
    assert_eq!(
        received,
        vec!["msg-0", "msg-1", "msg-2"],
        "the subscriber must see every message, in order\n{}",
        notes()
    );
}

/// A node that always exits non-zero is respawned under `on_failure` until
/// its budget is spent, and the dataflow is reported failed with the
/// exhausted-budget cause -- the "crashing node + restart budget" half of
/// this crate's required coverage.
fn crash_then_restart_exhausted() {
    let path = self_path();
    let yaml = format!(
        "\
exit_when_nodes_finish: true
nodes:
  - id: flaky
    path: \"{path}\"
    restart_policy: on_failure
    max_restarts: 2
    restart_delay: 0.01
    max_restart_delay: 0.05
    restart_window: 60
    env:
      {role_var}: publisher-crash
",
        role_var = ROLE_VAR,
    );
    let manifest = Manifest::from_yaml_str(&yaml).expect("a valid manifest");
    let result =
        block_on(run_dataflow_with(&manifest, options("crash"))).expect("a result even on failure");

    assert_eq!(result.status, DataflowStatus::Failed, "{result:?}");
    assert!(result.has_failures());
    let flaky = NodeId::new("flaky").unwrap();
    match result.node_results.get(&flaky) {
        Some(NodeExitCause::RestartBudgetExhausted { restarts, .. }) => {
            assert_eq!(*restarts, 2, "the budget was spent exactly: {result:?}");
        }
        other => panic!("expected an exhausted restart budget, got {other:?}"),
    }
}

/// The same two-node graph, driven through **`astrs run` itself** rather
/// than through the daemon function it wraps: `command::run::run` builds
/// the runtime, embeds the daemon, installs the terminal log sink and
/// computes the exit code, and this asserts all four (blueprint §4.2, §17).
///
/// Called directly from the test function, never inside [`block_on`]:
/// `command::run::run` builds a multi-thread runtime of its own, and
/// building one inside another panics.
fn cli_run_streams_and_exits_zero() {
    let dir = short_scratch("clirun");
    let out_file = dir.join("received.txt");
    let ready = dir.join("sub.ready");
    let done = dir.join("sub.done");
    let trace = dir.join("trace.log");
    let path = self_path();
    let manifest = dir.join("dataflow.yml");
    std::fs::write(
        &manifest,
        format!(
            "\
exit_when_nodes_finish: true
nodes:
  - id: pub
    path: \"{path}\"
    outputs: [out]
    env:
      {role_var}: publisher
      {ready_var}: \"{ready}\"
      {done_var}: \"{done}\"
      {trace_var}: \"{trace}\"
  - id: sub
    path: \"{path}\"
    inputs:
      in: pub/out
    env:
      {role_var}: subscriber
      {out_var}: \"{out}\"
      {ready_var}: \"{ready}\"
      {done_var}: \"{done}\"
      {trace_var}: \"{trace}\"
",
            role_var = ROLE_VAR,
            out_var = OUT_VAR,
            ready_var = READY_VAR,
            ready = ready.display(),
            done_var = DONE_VAR,
            done = done.display(),
            trace_var = TRACE_VAR,
            trace = trace.display(),
            out = out_file.display(),
        ),
    )
    .expect("a manifest");

    let mut args = astrs_cli::command::run::RunArgs::new(&manifest);
    args.skip_build = true;
    args.working_dir = Some(dir.clone());
    args.runtime_dir = Some(dir.clone());
    args.timeout = Some(Duration::from_secs(30));
    args.grace = Some(Duration::from_millis(200));

    let mut terminal: Vec<u8> = Vec::new();
    let report = astrs_cli::command::run::run(&mut terminal, &args).expect("a run report");
    let text = String::from_utf8(terminal).expect("utf-8 terminal output");
    // Read only by a failing assertion (see [`diagnostics`]).
    let notes = || diagnostics(&[&trace]);

    assert_eq!(
        report.exit_code(),
        astrs_cli::command::run::EXIT_OK,
        "a clean two-node run must exit zero:\n{text}\n{}",
        notes()
    );
    assert_eq!(
        report.result.status,
        DataflowStatus::Finished,
        "{text}\n{}",
        notes()
    );
    assert!(!report.result.has_failures(), "{text}\n{}", notes());
    assert_eq!(report.result.node_results.len(), 2, "{text}\n{}", notes());

    // Log lines really reached the terminal, node-prefixed, from both
    // nodes' captured stdout.
    assert!(text.contains("[pub"), "no publisher lines:\n{text}");
    assert!(text.contains("[sub"), "no subscriber lines:\n{text}");
    assert!(text.contains("published msg-0"), "{text}");
    assert!(text.contains("received msg-2"), "{text}");
    assert!(
        text.contains("finished"),
        "the summary must be printed:\n{text}"
    );
    assert!(report.printed >= 2, "{report:?}");
    assert_eq!(report.dropped, 0, "nothing should have been dropped");

    // …and the messages really arrived, not just the log lines about them.
    let received = std::fs::read_to_string(&out_file)
        .unwrap_or_else(|err| panic!("the subscriber's file: {err}\n{text}\n{}", notes()));
    assert_eq!(
        received.lines().collect::<Vec<_>>(),
        vec!["msg-0", "msg-1", "msg-2"],
        "{}",
        notes()
    );
}

/// A node that always fails is restarted under its `on_failure` budget, the
/// restarts are announced on the terminal, and the run's final verdict is a
/// failure with a non-zero exit code (blueprint §12, §17).
fn cli_run_reports_a_restart_then_a_failure() {
    let dir = short_scratch("clicrash");
    let path = self_path();
    let manifest = dir.join("dataflow.yml");
    std::fs::write(
        &manifest,
        format!(
            "\
exit_when_nodes_finish: true
nodes:
  - id: flaky
    path: \"{path}\"
    restart_policy: on_failure
    max_restarts: 2
    restart_delay: 0.01
    max_restart_delay: 0.05
    restart_window: 60
    env:
      {role_var}: publisher-crash
",
            role_var = ROLE_VAR,
        ),
    )
    .expect("a manifest");

    let mut args = astrs_cli::command::run::RunArgs::new(&manifest);
    args.skip_build = true;
    args.working_dir = Some(dir.clone());
    args.runtime_dir = Some(dir.clone());
    args.timeout = Some(Duration::from_secs(30));
    args.grace = Some(Duration::from_millis(200));

    let mut terminal: Vec<u8> = Vec::new();
    let report = astrs_cli::command::run::run(&mut terminal, &args)
        .expect("a report even when the graph fails");
    let text = String::from_utf8(terminal).expect("utf-8 terminal output");

    assert_eq!(
        report.exit_code(),
        astrs_cli::command::run::EXIT_FAILED,
        "a failed graph must exit one:\n{text}"
    );
    assert_eq!(report.result.status, DataflowStatus::Failed, "{text}");
    assert!(report.result.has_failures(), "{text}");

    // The restarts were announced while they happened…
    assert!(
        text.contains("restarting"),
        "the terminal must show the restart:\n{text}"
    );
    // …and the budget's exhaustion is the recorded cause.
    let flaky = NodeId::new("flaky").unwrap();
    match report.result.node_results.get(&flaky) {
        Some(NodeExitCause::RestartBudgetExhausted { restarts, .. }) => {
            assert_eq!(*restarts, 2, "{text}");
        }
        other => panic!("expected an exhausted restart budget, got {other:?}\n{text}"),
    }
    assert!(text.contains("failed"), "the summary must say so:\n{text}");
}

/// Ctrl-C on `astrs run`: one interrupt asks every node to stop, the daemon's
/// graceful ladder runs, and the verb returns a real verdict rather than
/// leaving the graph — or the shell — behind (blueprint §17, §4.2).
///
/// The graph here is one node that *never* ends on its own (see [`blocker`]),
/// so nothing but the interrupt can finish this run. The timeout is a
/// backstop set far above the expected duration, and the test asserts the run
/// ended well inside it: an interrupt that quietly did nothing would still
/// terminate eventually, and that must not read as a pass.
fn cli_run_stops_on_an_interrupt() {
    let dir = short_scratch("clistop");
    let ready = dir.join("hold.ready");
    let trace = dir.join("trace.log");
    let path = self_path();
    let manifest = dir.join("dataflow.yml");
    std::fs::write(
        &manifest,
        format!(
            "\
nodes:
  - id: hold
    path: \"{path}\"
    outputs: [tick]
    env:
      {role_var}: blocker
      {ready_var}: \"{ready}\"
      {trace_var}: \"{trace}\"
",
            role_var = ROLE_VAR,
            ready_var = READY_VAR,
            ready = ready.display(),
            trace_var = TRACE_VAR,
            trace = trace.display(),
        ),
    )
    .expect("a manifest");

    // The signal must arrive *after* `command::run::run` has armed its
    // handler, and the node's own readiness file is the evidence of that this
    // test can observe: a node cannot register before the embedded daemon is
    // up, and the daemon is not up before the handler is installed. Raising
    // it earlier would meet `SIGINT`'s default disposition and kill the test
    // binary outright.
    let ready_path = ready.clone();
    let signaller = std::thread::spawn(move || {
        let pid =
            rustix::process::Pid::from_raw(std::process::id().cast_signed()).expect("a own pid");
        let deadline = std::time::Instant::now() + READY_TIMEOUT;
        while !ready_path.exists() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        rustix::process::kill_process(pid, rustix::process::Signal::INT).is_ok()
    });

    let mut args = astrs_cli::command::run::RunArgs::new(&manifest);
    args.skip_build = true;
    args.working_dir = Some(dir.clone());
    args.runtime_dir = Some(dir.clone());
    args.timeout = Some(BACKSTOP_TIMEOUT);
    args.grace = Some(Duration::from_secs(2));

    let started = std::time::Instant::now();
    let mut terminal: Vec<u8> = Vec::new();
    let report = astrs_cli::command::run::run(&mut terminal, &args)
        .expect("a report even for an interrupted run");
    let elapsed = started.elapsed();
    let signalled = signaller.join().expect("the signalling thread");
    let text = String::from_utf8(terminal).expect("utf-8 terminal output");
    let notes = || diagnostics(&[&trace]);

    assert!(
        signalled,
        "the node never registered, so no interrupt was raised\n{text}\n{}",
        notes()
    );
    // The interrupt was announced on the terminal as it happened — matched
    // on the exact wording `command::run::interrupt_notice` produces, so
    // nothing but that code path can satisfy this.
    assert!(
        text.contains("asking every node to stop"),
        "one interrupt must be announced while the graph is still running:\n{text}\n{}",
        notes()
    );
    // …the *first* one asks rather than gives up…
    assert!(
        !report.abandoned,
        "a single interrupt asks the graph to stop; it does not abandon the wait:\n{text}"
    );
    // …the graph really ended because of it, not because the backstop
    // elapsed…
    assert!(
        elapsed + Duration::from_secs(5) < BACKSTOP_TIMEOUT,
        "the interrupt, not the timeout, must have ended this run (it took {elapsed:?} of \
         {BACKSTOP_TIMEOUT:?}):\n{text}\n{}",
        notes()
    );
    // …and a verdict was reached for the node that was asked to stop.
    assert!(
        matches!(
            report.result.status,
            DataflowStatus::Finished | DataflowStatus::Failed
        ),
        "an interrupted run must still reach a terminal status, got {:?}:\n{text}\n{}",
        report.result.status,
        notes()
    );
    let hold = NodeId::new("hold").unwrap();
    let cause = report
        .result
        .node_results
        .get(&hold)
        .unwrap_or_else(|| panic!("no result recorded for `hold`:\n{text}\n{}", notes()));
    assert!(
        cause.is_success(),
        "a node that honored the stop must be recorded as a clean exit, got {cause}:\n{text}\n{}",
        notes()
    );
    assert_eq!(
        report.exit_code(),
        astrs_cli::command::run::EXIT_OK,
        "a graph that stopped cleanly on Ctrl-C exits zero:\n{text}"
    );
}
