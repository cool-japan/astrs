//! End-to-end proof for the cluster verbs: a real `astrs up` / `astrs
//! status` / `astrs down` cycle driven through the **real binary**, and a
//! real CLI ↔ coordinator conversation over a real loopback socket
//! (blueprint §4.2, §16, §17, §24.2).
//!
//! ```text
//!   CARGO_BIN_EXE_astrs up --port 0 ──► spawns  astrs coordinator (child)
//!                                               astrs daemon      (child)
//!                    │                                │
//!                    ├─ <runtime>/coordinator.pid ◄───┘  pid + address
//!                    └─ astrs status ─► greets it ─► `Check` ─► reachable
//!                       astrs down   ─► SIGTERM ─► pidfiles gone
//!
//!   CoordinatorServer (in this process, own thread + runtime)
//!            ▲  TCP
//!            └── command::monitor::list · command::param::{set,get,list,delete}
//!            └── astrs_cli::dispatch(argv)   ← the whole path, argv in
//! ```
//!
//! # Why two levels
//!
//! The spawned-binary half is the only way to prove what `astrs up`
//! actually promises: that this one executable can start *other processes*
//! of itself, that they outlive the CLI that spawned them, that they
//! announce themselves through a pidfile, and that `astrs down` really ends
//! them. The in-process half is the only way to prove the client verbs
//! against a coordinator whose state a test can also inspect — and it runs
//! in milliseconds, so the round-trip assertions (a parameter written by
//! one verb and read back by another) stay cheap enough to be thorough.
//!
//! Every test here runs under the ordinary libtest harness: nothing in this
//! file re-executes the test binary, so there is no reason to hand-roll one.
//! Each test builds its own scratch directory and binds `--port 0`, so they
//! are safe to run concurrently, one process per test.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use astrs_cli::command::client::{Client, Endpoint, endpoint, new_subscription_id, runtime};
use astrs_cli::command::{monitor, param, trace};
use astrs_cli::runtime_dir::{COORDINATOR_PIDFILE, DAEMON_PIDFILE, read_pidfile};
use astrs_coordinator::{Coordinator, CoordinatorConfig, CoordinatorServer};
use astrs_wire::{AuthToken, ControlRequest, LogQuery};

/// How long a spawned `astrs up`/`astrs down` may take before the test
/// calls it hung. Generous: `up` waits for two children to bind.
const CLI_TIMEOUT: Duration = Duration::from_secs(60);

/// A short scratch directory.
///
/// Short on purpose: the daemon's Unix socket lives inside it, and a Unix
/// socket path has a hard length limit (104 bytes on macOS) that a long
/// temporary directory name plus `daemon.sock` can exceed.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("as-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// The `astrs` binary under test — the very executable this crate builds.
fn astrs() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_astrs"))
}

/// Runs `astrs <argv>` to completion, returning its exit code and stdout.
fn run_cli(argv: &[&str]) -> (i32, String, String) {
    let started = Instant::now();
    let mut child = Command::new(astrs())
        .args(argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("spawning `astrs {}`: {err}", argv.join(" ")));

    loop {
        if let Some(status) = child.try_wait().expect("waiting on the CLI") {
            let output = child.wait_with_output().expect("its output");
            return (
                status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            );
        }
        assert!(
            started.elapsed() < CLI_TIMEOUT,
            "`astrs {}` did not finish within {:?}",
            argv.join(" "),
            CLI_TIMEOUT
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether a pid is still a process.
fn alive(pid: u32) -> bool {
    rustix::process::Pid::from_raw(pid.cast_signed())
        .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
}

/// Polls until `condition` holds or the deadline passes.
fn eventually(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The tail of a child process's log, for a failure message.
fn log(dir: &Path, role: &str) -> String {
    std::fs::read_to_string(dir.join(format!("{role}.log")))
        .unwrap_or_else(|err| format!("(no {role}.log: {err})"))
}

/// The flagship cluster test: `up` really starts two processes, `status`
/// really greets one of them over TCP, and `down` really stops both and
/// clears their pidfiles.
#[test]
fn up_status_down_lifecycle() {
    let dir = scratch("life");
    let runtime = dir.join("rt");
    let path = dir.to_string_lossy().into_owned();
    let runtime_path = runtime.to_string_lossy().into_owned();

    // `--port 0`: the kernel picks a free port, so the test never collides
    // with a developer's own cluster on 7407.
    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "up",
        "--port",
        "0",
        "--runtime-dir",
        &runtime_path,
        "--working-dir",
        &path,
    ]);
    assert_eq!(
        code,
        0,
        "up failed\nstdout: {stdout}\nstderr: {stderr}\ncoordinator log: {}\ndaemon log: {}",
        log(&runtime, "coordinator"),
        log(&runtime, "daemon")
    );

    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("`up --json` printed {stdout:?}: {err}"));
    assert_eq!(report["adopted"], false, "{report}");
    assert_eq!(report["coordinator"]["running"], true, "{report}");
    assert_eq!(report["daemon"]["running"], true, "{report}");

    // §16: the token is generated beside the manifest, owner-only.
    let token_path = dir.join(".astrs-token");
    assert!(token_path.exists(), "up must generate a cluster token");
    // Compared as `rustix::fs::Mode` flags rather than as octal because
    // `mode_t` is `u16` on some targets and `u32` on others; the flags carry
    // their own width, which is why `astrs-cli` spells the mode it *sets*
    // the same way.
    let mode = rustix::fs::Mode::from_raw_mode(
        rustix::fs::stat(&token_path)
            .expect("stat the token")
            .st_mode,
    );
    assert_eq!(
        mode.intersection(
            rustix::fs::Mode::RWXU
                .union(rustix::fs::Mode::RWXG)
                .union(rustix::fs::Mode::RWXO)
        ),
        rustix::fs::Mode::RUSR.union(rustix::fs::Mode::WUSR),
        "the token must be 0600"
    );

    let coordinator_pid = report["coordinator"]["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .expect("a coordinator pid");
    let daemon_pid = report["daemon"]["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .expect("a daemon pid");
    assert!(alive(coordinator_pid), "the coordinator must be running");
    assert!(alive(daemon_pid), "the daemon must be running");

    // The children outlive the `astrs up` process that spawned them: that
    // CLI has already exited by the time `run_cli` returned.
    let recorded = read_pidfile(&runtime.join(COORDINATOR_PIDFILE)).expect("a coordinator pidfile");
    assert_eq!(recorded.pid, coordinator_pid);
    let address = recorded.addr.clone().expect("an announced address");
    assert!(address.starts_with("127.0.0.1:"), "{address}");
    assert!(
        !address.ends_with(":0"),
        "the pidfile must name the port actually bound, not `0`: {address}"
    );

    // `status` greets the coordinator over the wire with the token `up`
    // generated — the proof that a *usable* cluster is up, not just two
    // processes.
    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "status",
        "--runtime-dir",
        &runtime_path,
        "--working-dir",
        &path,
    ]);
    assert_eq!(code, 0, "status failed\n{stdout}\n{stderr}");
    let status: serde_json::Value = serde_json::from_str(&stdout).expect("status json");
    assert_eq!(status["reachable"], true, "{status}");
    assert_eq!(status["coordinator"]["running"], true, "{status}");
    assert_eq!(status["daemon"]["running"], true, "{status}");

    // `list` over the same link: an empty cluster, but a real answer.
    let (code, stdout, _) = run_cli(&[
        "--json",
        "list",
        "--coordinator",
        &address,
        "--working-dir",
        &path,
    ]);
    assert_eq!(code, 0, "list failed: {stdout}");
    let list: serde_json::Value = serde_json::from_str(&stdout).expect("list json");
    assert_eq!(list["count"], 0, "{list}");

    let (code, stdout, stderr) = run_cli(&[
        "--json",
        "down",
        "--runtime-dir",
        &runtime_path,
        "--working-dir",
        &path,
    ]);
    assert_eq!(code, 0, "down failed\n{stdout}\n{stderr}");
    let down: serde_json::Value = serde_json::from_str(&stdout).expect("down json");
    assert_eq!(down["stopped_anything"], true, "{down}");
    assert_eq!(down["coordinator"]["outcome"], "terminated", "{down}");
    assert_eq!(down["daemon"]["outcome"], "terminated", "{down}");

    eventually("the coordinator to exit", Duration::from_secs(10), || {
        !alive(coordinator_pid)
    });
    eventually("the daemon to exit", Duration::from_secs(10), || {
        !alive(daemon_pid)
    });
    assert!(
        !runtime.join(COORDINATOR_PIDFILE).exists(),
        "down must clear the coordinator pidfile"
    );
    assert!(
        !runtime.join(DAEMON_PIDFILE).exists(),
        "down must clear the daemon pidfile"
    );

    // And a second `down` is a no-op rather than a failure.
    let (code, stdout, _) = run_cli(&[
        "down",
        "--runtime-dir",
        &runtime_path,
        "--working-dir",
        &path,
    ]);
    assert_eq!(code, 0);
    assert!(stdout.contains("nothing was running here"), "{stdout}");
}

/// A second `astrs up` in the same runtime directory adopts what is already
/// there instead of starting a second coordinator on the same port.
#[test]
fn up_twice_adopts_rather_than_duplicates() {
    let dir = scratch("adopt");
    let runtime = dir.join("rt");
    let path = dir.to_string_lossy().into_owned();
    let runtime_path = runtime.to_string_lossy().into_owned();
    let argv = [
        "--json",
        "up",
        "--port",
        "0",
        "--no-daemon",
        "--runtime-dir",
        &runtime_path,
        "--working-dir",
        &path,
    ];

    let (code, stdout, stderr) = run_cli(&argv);
    assert_eq!(
        code,
        0,
        "first up failed\n{stdout}\n{stderr}\n{}",
        log(&runtime, "coordinator")
    );
    let first: serde_json::Value = serde_json::from_str(&stdout).expect("up json");
    assert_eq!(first["adopted"], false, "{first}");
    assert!(
        first["daemon"].is_null(),
        "--no-daemon starts none: {first}"
    );
    let pid = first["coordinator"]["pid"].as_u64().expect("a pid");

    let (code, stdout, stderr) = run_cli(&argv);
    assert_eq!(code, 0, "second up failed\n{stdout}\n{stderr}");
    let second: serde_json::Value = serde_json::from_str(&stdout).expect("up json");
    assert_eq!(second["adopted"], true, "{second}");
    assert_eq!(
        second["coordinator"]["pid"].as_u64(),
        Some(pid),
        "the same coordinator must be adopted, not replaced: {second}"
    );

    // `--recreate-store` under a live coordinator is refused rather than
    // wiping the state it is serving.
    let (code, _, stderr) = run_cli(&[
        "up",
        "--recreate-store",
        "--runtime-dir",
        &runtime_path,
        "--working-dir",
        &path,
    ]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("already running"), "{stderr}");

    let (code, _, _) = run_cli(&[
        "down",
        "--runtime-dir",
        &runtime_path,
        "--working-dir",
        &path,
    ]);
    assert_eq!(code, 0);
}

/// A coordinator running in this process, on its own thread and runtime.
///
/// Its own thread because every client verb in this crate is *synchronous*
/// (it builds a current-thread runtime and blocks on it), so a test thread
/// cannot also be the thread driving the server.
struct InProcessCluster {
    addr: SocketAddr,
    token: AuthToken,
    dir: PathBuf,
    shutdown: astrs_coordinator::ServerHandle,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InProcessCluster {
    fn start(name: &str) -> Self {
        let dir = scratch(name);
        let token = AuthToken::from_bytes([0x5a; 32]);
        std::fs::write(
            dir.join(".astrs-token"),
            format!("{}\n", token.reveal_hex()),
        )
        .expect("a token file");

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
            dir,
            shutdown,
            thread: Some(thread),
        }
    }

    /// An endpoint that authenticates with this cluster's token, found the
    /// way a real invocation finds it: `<working dir>/.astrs-token` (§16).
    fn endpoint(&self) -> Endpoint {
        let endpoint = endpoint(
            Some(&self.addr.to_string()),
            None,
            None,
            Some(&self.dir),
            true,
        )
        .expect("an endpoint");
        assert_eq!(endpoint.token, self.token, "the token file must be found");
        endpoint
    }
}

impl Drop for InProcessCluster {
    fn drop(&mut self) {
        self.shutdown.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The parameter verbs against a real coordinator and a real store: set,
/// read back, list, delete, read again.
#[test]
fn param_round_trip_against_a_real_coordinator() {
    let cluster = InProcessCluster::start("param");
    let endpoint = cluster.endpoint();
    let args = |json: bool| param::ScopeArgs {
        // The global scope needs no dataflow to exist first, which is what
        // makes it the right scope for a store round-trip test.
        dataflow: param::GLOBAL_SCOPE_WORD.to_owned(),
        node: None,
        json,
    };

    let mut out = Vec::new();
    let written = param::set(&mut out, &endpoint, &args(false), "camera.fps", "30", false)
        .expect("param set");
    assert_eq!(written, astrs_wire::Parameter::Integer(30));
    assert!(
        String::from_utf8(out).unwrap().contains("camera.fps = 30"),
        "set must say what it wrote"
    );

    let mut out = Vec::new();
    let read = param::get(&mut out, &endpoint, &args(false), "camera.fps", true)
        .expect("param get")
        .expect("the value that was just written");
    assert_eq!(read, astrs_wire::Parameter::Integer(30));
    assert_eq!(String::from_utf8(out).unwrap().trim_end(), "30");

    // Every plain-JSON shape survives the wire and the store.
    for (key, text, expected) in [
        ("gain", "1.5", astrs_wire::Parameter::Float(1.5)),
        ("enabled", "true", astrs_wire::Parameter::Bool(true)),
        (
            "mode",
            "\"fast\"",
            astrs_wire::Parameter::String("fast".to_owned()),
        ),
        (
            "roi",
            "[1,2,3]",
            astrs_wire::Parameter::ListInt(vec![1, 2, 3]),
        ),
    ] {
        param::set(&mut Vec::new(), &endpoint, &args(false), key, text, false)
            .unwrap_or_else(|err| panic!("setting {key}: {err}"));
        let read = param::get(&mut Vec::new(), &endpoint, &args(false), key, true)
            .unwrap_or_else(|err| panic!("reading {key}: {err}"))
            .unwrap_or_else(|| panic!("{key} came back unset"));
        assert_eq!(read, expected, "{key}");
    }

    let mut out = Vec::new();
    let listed = param::list(&mut out, &endpoint, &args(true), None, false).expect("param list");
    assert_eq!(listed.len(), 5, "{listed:?}");
    let json: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out).unwrap()).expect("list json");
    assert_eq!(json["count"], 5, "{json}");
    assert_eq!(json["scope"], param::GLOBAL_SCOPE_WORD);

    // A prefix filter narrows it at the source, not in the CLI.
    let narrowed = param::list(
        &mut Vec::new(),
        &endpoint,
        &args(false),
        Some("camera."),
        false,
    )
    .expect("a filtered list");
    assert_eq!(narrowed.len(), 1, "{narrowed:?}");
    assert_eq!(narrowed[0].0.as_str(), "camera.fps");

    param::delete(&mut Vec::new(), &endpoint, &args(false), "camera.fps").expect("param delete");
    let after = param::get(&mut Vec::new(), &endpoint, &args(false), "camera.fps", true)
        .expect("param get after delete");
    assert!(after.is_none(), "a deleted key must read as unset");

    // A key that never existed is `None`, not an error.
    let missing = param::get(&mut Vec::new(), &endpoint, &args(false), "nope", true)
        .expect("param get for a missing key");
    assert!(missing.is_none());
}

/// `--watch` against a real coordinator: it prints the value already set
/// once at the start, then a *later* write from an independent session —
/// the "streaming subsequent updates" half of the loop, proven end to
/// end rather than only against `param::watch`'s own doc example.
#[test]
fn param_watch_streams_the_initial_value_then_a_later_transition() {
    let cluster = InProcessCluster::start("watch");
    let endpoint = cluster.endpoint();
    let args = param::ScopeArgs {
        dataflow: param::GLOBAL_SCOPE_WORD.to_owned(),
        node: None,
        json: false,
    };

    param::set(&mut Vec::new(), &endpoint, &args, "gain", "1", false).expect("seed a value");

    // A second write, from an independent connection, shortly after
    // `watch` starts polling — proving a transition *during* the watch is
    // seen, not just the value already there when it began.
    let writer_endpoint = endpoint.clone();
    let writer_args = args.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        param::set(
            &mut Vec::new(),
            &writer_endpoint,
            &writer_args,
            "gain",
            "2",
            false,
        )
        .expect("the second write")
    });

    let mut out = Vec::new();
    let seen =
        param::watch(&mut out, &endpoint, &args, "gain", true, Some(2)).expect("param watch");
    writer.join().expect("the writer thread");

    assert_eq!(
        seen,
        vec![
            Some(astrs_wire::Parameter::Integer(1)),
            Some(astrs_wire::Parameter::Integer(2)),
        ]
    );
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("gain = 1"), "{text}");
    assert!(text.contains("gain = 2"), "{text}");
}

/// `--watch --count 1` returns immediately on the value already set,
/// without ever sleeping through a poll interval — the bound this crate's
/// final report explains was added specifically so a test never has to
/// touch `SIGTERM` to end a watch.
#[test]
fn param_watch_with_count_one_returns_without_a_second_poll() {
    let cluster = InProcessCluster::start("watchone");
    let endpoint = cluster.endpoint();
    let args = param::ScopeArgs {
        dataflow: param::GLOBAL_SCOPE_WORD.to_owned(),
        node: None,
        json: false,
    };
    param::set(&mut Vec::new(), &endpoint, &args, "gain", "9", false).expect("seed a value");

    let started = Instant::now();
    let seen = param::watch(&mut Vec::new(), &endpoint, &args, "gain", true, Some(1))
        .expect("param watch");
    assert_eq!(seen, vec![Some(astrs_wire::Parameter::Integer(9))]);
    assert!(
        started.elapsed() < param::WATCH_POLL_INTERVAL,
        "a count of one must not wait out a poll interval: {:?}",
        started.elapsed()
    );
}

/// `astrs trace` against a real coordinator: after a couple of ordinary
/// requests, `GetTraces` answers with real spans for them — the
/// coordinator's own request-span buffer (`astrs-coordinator`'s `trace`
/// module has the full story) actually feeding `astrs trace`, not a
/// handler that always answers empty.
#[test]
fn trace_lists_the_coordinators_own_recent_requests() {
    let cluster = InProcessCluster::start("trace");
    let endpoint = cluster.endpoint();
    let args = param::ScopeArgs {
        dataflow: param::GLOBAL_SCOPE_WORD.to_owned(),
        node: None,
        json: false,
    };
    param::set(&mut Vec::new(), &endpoint, &args, "gain", "1", false)
        .expect("a SetParam for trace to see");
    param::get(&mut Vec::new(), &endpoint, &args, "gain", true)
        .expect("a GetParam for trace to see");

    let mut out = Vec::new();
    let report =
        trace::run(&mut out, &endpoint, &trace::TraceArgs::default()).expect("astrs trace");
    let names: Vec<&str> = report.spans.iter().map(|span| span.name.as_str()).collect();
    assert!(names.contains(&"SetParam"), "{names:?}");
    assert!(names.contains(&"GetParam"), "{names:?}");
    assert!(
        report
            .spans
            .iter()
            .all(|span| span.status == astrs_wire::SpanStatus::Ok),
        "{:?}",
        report.spans
    );
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("SetParam"), "{text}");
}

/// `list` and `status` against a real coordinator, including the "name one
/// of several" failure a user actually hits.
#[test]
fn list_and_status_against_a_real_coordinator() {
    let cluster = InProcessCluster::start("list");
    let endpoint = cluster.endpoint();

    let mut out = Vec::new();
    let report = monitor::list(
        &mut out,
        &endpoint,
        &monitor::ListArgs {
            all: true,
            json: true,
        },
    )
    .expect("list");
    assert!(report.dataflows.is_empty(), "{report:?}");
    let json: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out).unwrap()).expect("list json");
    assert_eq!(json["count"], 0);

    // With no dataflows at all, a verb that has to resolve one says so
    // rather than guessing or hanging.
    let error = monitor::logs(&mut Vec::new(), &endpoint, &monitor::LogsArgs::default())
        .expect_err("nothing to tail");
    assert!(
        matches!(error, astrs_cli::CliError::UnknownDataflow { .. }),
        "{error}"
    );

    let error = monitor::dataflow_status(
        &mut Vec::new(),
        &endpoint,
        &monitor::DataflowStatusArgs::default(),
    )
    .expect_err("nothing to describe");
    assert!(
        matches!(error, astrs_cli::CliError::UnknownDataflow { .. }),
        "{error}"
    );
}

/// The whole path, argv in: `astrs_cli::dispatch` against a real
/// coordinator, exactly as `main.rs` would call it.
#[test]
fn dispatch_reaches_a_real_coordinator() {
    use clap::Parser as _;

    let cluster = InProcessCluster::start("dispatch");
    let address = cluster.addr.to_string();
    let working = cluster.dir.to_string_lossy().into_owned();

    let argv = |extra: &[&str]| -> Vec<String> {
        let mut argv = vec!["astrs".to_owned(), "--json".to_owned()];
        argv.extend(extra.iter().map(|part| (*part).to_owned()));
        argv.extend([
            "--coordinator".to_owned(),
            address.clone(),
            "--working-dir".to_owned(),
            working.clone(),
        ]);
        argv
    };

    let dispatch = |extra: &[&str]| -> (i32, String, String) {
        let cli = astrs_cli::Cli::try_parse_from(argv(extra))
            .unwrap_or_else(|err| panic!("parsing {extra:?}: {err}"));
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = astrs_cli::dispatch(cli, &mut out, &mut err, false);
        (
            code,
            String::from_utf8(out).expect("utf-8 stdout"),
            String::from_utf8(err).expect("utf-8 stderr"),
        )
    };

    let (code, stdout, stderr) = dispatch(&["list"]);
    assert_eq!(code, 0, "{stderr}");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("list json");
    assert_eq!(json["count"], 0, "{json}");

    // A mutating verb, over the same link, with the token read from the
    // working directory — §16's path end to end.
    let (code, stdout, stderr) = dispatch(&["param", "set", "global", "fleet.size", "3"]);
    assert_eq!(code, 0, "{stderr}");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("set json");
    assert_eq!(json["value"], 3, "{json}");
    assert_eq!(json["type"], "integer", "{json}");

    let (code, stdout, stderr) = dispatch(&["param", "get", "global", "fleet.size"]);
    assert_eq!(code, 0, "{stderr}");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("get json");
    assert_eq!(json["found"], true, "{json}");
    assert_eq!(json["value"], 3, "{json}");

    // A key that is not set exits non-zero, so scripts can branch on it.
    let (code, stdout, _) = dispatch(&["param", "get", "global", "fleet.absent"]);
    assert_eq!(code, 1, "{stdout}");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("get json");
    assert_eq!(json["found"], false, "{json}");

    let (code, stdout, stderr) = dispatch(&["param", "delete", "global", "fleet.size"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        serde_json::from_str::<serde_json::Value>(&stdout).expect("delete json")["deleted"] == true
    );

    // `stop` of a dataflow that does not exist is refused by the
    // coordinator, and the refusal reaches the exit code rather than a
    // panic.
    let (code, _, stderr) = dispatch(&["stop", "no-such-flow"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(!stderr.is_empty(), "a refusal must be explained");
}

/// The wire pair `astrs logs -f` rides: `LogSubscribe` opens the stream and
/// `TopicUnsubscribe` closes it, both accepted by a real coordinator.
///
/// The pushed-frame half is covered by `command::monitor`'s own unit tests
/// (a `Log` frame in, a rendered line out); what only a real coordinator
/// can prove is that the *requests* this CLI sends are the ones it accepts,
/// and that two subscriptions minted by one process do not collide — the
/// registry is keyed by id across every session, so a fixed id would let
/// one CLI steal another's stream.
#[test]
fn log_subscribe_and_unsubscribe_against_a_real_coordinator() {
    let cluster = InProcessCluster::start("logsub");
    let endpoint = cluster.endpoint();
    let runtime = runtime().expect("a runtime");

    runtime.block_on(async {
        let mut client = Client::connect(&endpoint).await.expect("connect");
        let first = new_subscription_id();
        let second = new_subscription_id();
        assert_ne!(first, second, "minted ids must differ within one process");
        assert!(!first.is_none(), "the reserved zero id is never minted");

        client
            .request_ok(
                "logs -f",
                &ControlRequest::LogSubscribe {
                    // `None`: the whole cluster, which is what `astrs logs
                    // -f` with no dataflow named subscribes to.
                    dataflow: None,
                    node: None,
                    query: LogQuery::new(),
                    subscription: first,
                },
            )
            .await
            .expect("the coordinator must accept a cluster-wide log subscription");

        client
            .request_ok(
                "logs -f",
                &ControlRequest::TopicUnsubscribe {
                    subscription: first,
                },
            )
            .await
            .expect("the coordinator must accept the unsubscribe");

        client.close().await.expect("a clean shutdown");
    });
}
