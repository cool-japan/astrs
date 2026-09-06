//! `astrs run` cold-start latency gate (blueprint §20.4).
//!
//! | Bench | §20.4 target (0.1.0) |
//! |---|---|
//! | `astrs_cold_start/ten_node_timer_graph` | < 800 ms |
//!
//! # What is measured
//!
//! A real OS process, spawned exactly the way a user's shell would spawn
//! it: `astrs run <manifest>` against a generated ten-node graph, each node
//! the `hello-timer` example (blueprint §5.4) bound to a virtual
//! `astrs/timer/millis/*` input. "Ready" is `hello-timer`'s own first log
//! line — `node.log_info("hello from …")`, emitted the instant
//! `Node::init_from_env` completes registration, *before* that node has
//! processed a single tick — read back off `astrs run`'s streamed terminal
//! output (`[<id> …]`-prefixed, the same convention
//! `bins/astrs-cli/tests/run_e2e.rs`'s own assertions read). The timed span
//! is process-spawn to the last of the ten `[nN` lines appearing.
//!
//! Because `exit_when_nodes_finish: true` and every node's own registration
//! (`hello`) strictly precedes its own completion, the graph cannot finish
//! — and therefore cannot exit — until every node has already logged
//! `hello`. Readiness detection is therefore guaranteed to succeed before
//! natural process exit, whatever the true cold-start latency turns out to
//! be, which is what makes a short, fixed tick budget (`TICKS_PER_NODE` ×
//! `TICK_MILLIS`, comfortably above the target but not by much) safe to use
//! for a fast, self-terminating teardown instead of signaling the process.
//!
//! # Why this file's own setup does not just run `cargo build`
//!
//! This bench binary is itself the product of, and typically runs nested
//! inside, a `cargo bench` invocation — one that may still be holding (or
//! waiting on) the workspace's own `target/` build-directory lock. A plain
//! `cargo build --release -p astrs-cli` from inside this file's setup would
//! contend for that same lock and can **deadlock** rather than merely queue
//! (the outer invocation is itself waiting on whatever the inner one is
//! doing). [`build_release_binaries`] sidesteps this two ways:
//!
//! 1. If `ASTRS_BENCH_CLI`/`ASTRS_BENCH_NODE` name pre-built binaries (what
//!    `scripts/bench-gate.sh` sets, having built them itself, once, before
//!    running any bench), no nested `cargo` runs at all.
//! 2. Otherwise, it builds into a private `CARGO_TARGET_DIR` — a directory
//!    with its own lock, so the nested build cannot contend with the
//!    workspace's own regardless of what the outer invocation is doing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::RefCell;
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use astrs_benches::{ancestor, line_names_node, manifest_text, percentile, verdict};
use criterion::{Criterion, SamplingMode, criterion_group, criterion_main};

/// `astrs run` cold start, 10-node graph (blueprint §20.4).
const COLD_START_TARGET: Duration = Duration::from_millis(800);

/// How many nodes the generated graph has.
const NODE_COUNT: usize = 10;

/// The virtual timer's period each node subscribes to.
const TICK_MILLIS: u64 = 20;

/// How many ticks each node greets before finishing — `TICK_MILLIS *
/// TICKS_PER_NODE` of guaranteed post-registration life, comfortably above
/// [`COLD_START_TARGET`] so a slow-but-passing run still has every node
/// alive to observe, without making a *failing* run's teardown wait any
/// longer than it has to.
const TICKS_PER_NODE: u64 = 8;

/// How long readiness detection may take before this file declares the
/// graph stalled rather than waiting forever.
const READY_TIMEOUT: Duration = Duration::from_secs(15);

/// How long this file waits for the graph to finish on its own — see this
/// file's header for why it is expected to — before falling back to
/// killing it.
const NATURAL_EXIT_TIMEOUT: Duration = Duration::from_secs(20);

/// This crate's own workspace root, derived from where it was compiled —
/// not a literal path, so it holds for whoever checked the repository out
/// and wherever. See [`astrs_benches::ancestor`] for the (unit-tested) walk
/// itself.
fn workspace_root() -> PathBuf {
    ancestor(env!("CARGO_MANIFEST_DIR"), 2)
        .expect("benches/astrs-benches is two levels under the workspace root")
}

/// Locates (or builds) the release `astrs` and `hello-timer` binaries. See
/// this file's header for why a nested `cargo build` uses an isolated
/// `CARGO_TARGET_DIR` rather than the workspace's own.
fn build_release_binaries() -> (PathBuf, PathBuf) {
    if let (Ok(cli), Ok(node)) = (
        std::env::var("ASTRS_BENCH_CLI"),
        std::env::var("ASTRS_BENCH_NODE"),
    ) {
        return (PathBuf::from(cli), PathBuf::from(node));
    }

    let root = workspace_root();
    let scratch_target = std::env::temp_dir().join("astrs-bench-cold-start-target");

    for package in ["astrs-cli", "hello-timer"] {
        let status = Command::new("cargo")
            .current_dir(&root)
            .env("CARGO_TARGET_DIR", &scratch_target)
            .args(["build", "--release", "-p", package])
            .status()
            .expect("spawn cargo build");
        assert!(
            status.success(),
            "cargo build --release -p {package} failed"
        );
    }

    (
        scratch_target.join("release").join("astrs"),
        scratch_target.join("release").join("hello-timer"),
    )
}

/// Writes a manifest with [`NODE_COUNT`] `hello-timer` nodes, each on its
/// own virtual `astrs/timer/millis/*` input and no data-plane wiring
/// between them — a graph whose cold start is pure spawn-and-register
/// latency, nothing else. See [`astrs_benches::manifest_text`] for the
/// (unit-tested) text itself.
fn write_manifest(dir: &Path, node_bin: &Path, node_ids: &[String]) -> PathBuf {
    let text = manifest_text(node_bin, node_ids, TICK_MILLIS, TICKS_PER_NODE);
    let path = dir.join("dataflow.yml");
    std::fs::write(&path, text).expect("write generated manifest");
    path
}

/// Kills and reaps the wrapped child on drop — a panic-safety backstop
/// (see [`measure_cold_start`]'s own, orderly teardown for the normal
/// path). Harmless to run twice: killing/waiting an already-reaped child
/// just returns an ignored error.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawns `astrs run` against `manifest`, measures spawn-to-all-ready, and
/// tears the process down cleanly (see this file's header). Returns the
/// measured [`Duration`].
fn measure_cold_start(
    astrs_bin: &Path,
    manifest: &Path,
    runtime_dir: &Path,
    node_ids: &[String],
) -> Duration {
    std::fs::create_dir_all(runtime_dir).expect("runtime dir");

    let start = Instant::now();
    let mut guard = KillOnDrop(
        Command::new(astrs_bin)
            .arg("run")
            .arg(manifest)
            .arg("--skip-build")
            .arg("--runtime-dir")
            .arg(runtime_dir)
            .arg("--timeout")
            .arg(READY_TIMEOUT.as_secs().to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn astrs run"),
    );
    let stdout = guard.0.stdout.take().expect("piped stdout");

    // A dedicated blocking reader thread, not a poll loop: `BufRead::lines`
    // wakes the instant a line is flushed (the CLI flushes every rendered
    // line — see `bins/astrs-cli/src/command/run.rs`), so this adds no
    // polling-interval bias to the measured latency.
    let (line_tx, line_rx) = mpsc::channel::<String>();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut seen: HashSet<&str> = HashSet::new();
    let deadline = start + READY_TIMEOUT;
    let ready_at = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!(
                "cold start: only {}/{} nodes were ready within {READY_TIMEOUT:?}",
                seen.len(),
                node_ids.len()
            );
        }
        match line_rx.recv_timeout(remaining) {
            Ok(line) => {
                for id in node_ids {
                    if line_names_node(&line, id) {
                        seen.insert(id.as_str());
                    }
                }
                if seen.len() == node_ids.len() {
                    break Instant::now();
                }
            }
            Err(_) => panic!(
                "cold start: astrs run's stdout ended with only {}/{} nodes ready",
                seen.len(),
                node_ids.len()
            ),
        }
    };
    let elapsed = ready_at.duration_since(start);

    // Orderly teardown: the graph is bounded (this file's header) and
    // finishes on its own within a couple of tick periods: wait for that
    // instead of killing a graph that is still legitimately running. Only
    // past `NATURAL_EXIT_TIMEOUT` — which would itself indicate a bug —
    // does this fall back to `KillOnDrop`'s unconditional kill.
    let wait_deadline = Instant::now() + NATURAL_EXIT_TIMEOUT;
    loop {
        match guard.0.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) if Instant::now() < wait_deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) | Err(_) => break,
        }
    }
    let _ = reader.join();

    elapsed
}

/// Prints the §20.4 gate line `scripts/bench-gate.sh` greps for, plus a loud
/// (but non-fatal) warning on a miss. Never fails the build itself. Uses
/// [`astrs_benches::percentile`] and [`astrs_benches::verdict`] — the
/// unit-tested percentile/pass-fail logic this file's own `test = false`
/// bench target cannot carry its own tests for (see this crate's `src/lib.rs`).
///
/// At the sample counts a real-process cold-start bench can afford (tens,
/// not thousands), "p99" is nearest-rank over a small set and reads close
/// to the observed maximum — worth naming plainly rather than implying a
/// precision this file cannot deliver.
fn report(name: &str, samples: &mut [Duration], target: Duration) {
    let n = samples.len();
    let p50 = percentile(samples, 50.0);
    let p99 = percentile(samples, 99.0);
    let max = samples.iter().max().copied().unwrap_or_default();
    let result = if n > 0 { verdict(p99, target) } else { "FAIL" };
    println!(
        "BENCH_GATE name={name} n={n} p50_ms={:.2} p99_ms={:.2} max_ms={:.2} target_ms={:.2} result={result}",
        p50.as_secs_f64() * 1e3,
        p99.as_secs_f64() * 1e3,
        max.as_secs_f64() * 1e3,
        target.as_secs_f64() * 1e3,
    );
    if result == "FAIL" {
        eprintln!(
            "WARN: {name} missed its blueprint §20.4 target: p99={:.2}ms > target={:.2}ms (n={n} samples, small-n — see this file's `report` doc)",
            p99.as_secs_f64() * 1e3,
            target.as_secs_f64() * 1e3,
        );
    }
}

/// The §20.4 gate: real-process cold start, ten nodes, spawn to all ready.
fn bench_cold_start(c: &mut Criterion) {
    let (astrs_bin, node_bin) = build_release_binaries();
    let node_ids: Vec<String> = (0..NODE_COUNT).map(|index| format!("n{index}")).collect();

    let pid = std::process::id();
    let scratch = std::env::temp_dir().join(format!("acsb{pid}"));
    std::fs::create_dir_all(&scratch).expect("scratch dir");
    let manifest = write_manifest(&scratch, &node_bin, &node_ids);

    let iteration = AtomicU64::new(0);
    let samples = RefCell::new(Vec::<Duration>::new());

    {
        let mut group = c.benchmark_group("astrs_cold_start");
        group.sampling_mode(SamplingMode::Flat);
        // The minimum criterion allows. Each iteration is a real process
        // cold start plus its bounded natural lifetime (`TICK_MILLIS *
        // TICKS_PER_NODE`) — on the order of a second, not a microsecond —
        // so a larger sample count would make this one bench dominate the
        // suite's wall time for a percentile precision §5.2's ~20k-line
        // test-and-bench budget does not ask for here.
        group.sample_size(10);
        // Deliberately tiny: with `SamplingMode::Flat`, criterion sizes
        // each sample as `ceil((measurement_time / sample_size) /
        // warmup_mean)`, which floors at one real iteration per sample
        // however small the numerator gets — so this, combined with an
        // equally tiny `warm_up_time`, is what pins this bench at exactly
        // `sample_size` real cold starts (plus the one warm-up call
        // criterion always makes first) instead of criterion's own
        // calibration multiplying that by however many it estimates fit in
        // a default multi-second budget.
        group.warm_up_time(Duration::from_millis(1));
        group.measurement_time(Duration::from_millis(1));
        group.bench_function("ten_node_timer_graph", |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let index = iteration.fetch_add(1, Ordering::Relaxed);
                    let runtime_dir = std::env::temp_dir().join(format!("acsr{pid}-{index}"));
                    let elapsed =
                        measure_cold_start(&astrs_bin, &manifest, &runtime_dir, &node_ids);
                    let _ = std::fs::remove_dir_all(&runtime_dir);
                    total += elapsed;
                    samples.borrow_mut().push(elapsed);
                }
                total
            });
        });
        group.finish();
    }

    let _ = std::fs::remove_dir_all(&scratch);

    let mut collected = samples.into_inner();
    report(
        "astrs_cli.cold_start_10_node",
        &mut collected,
        COLD_START_TARGET,
    );
}

criterion_group!(benches, bench_cold_start);
criterion_main!(benches);
