# `astrs-conformance`

The milestone end-to-end proofs. Every test drives a
**committed example manifest** — never a copy written by a test — and asserts
on durable artefacts (a JSON tally, a probe report, an exit code) rather than
on scraped terminal text.

```bash
cargo build -p astrs-cli -p astrs-replay-node \
            -p hello-timer -p rust-pipeline \
            -p service-roundtrip -p shm-zero-copy-probe \
            -p record-replay -p multi-daemon-cluster \
            -p restart-policies -p error-propagation
cargo test  -p astrs-conformance
```

Build them in **one** `cargo build` invocation, and rebuild after anything
under `crates/` changes: the example binaries and the `astrs` binary talk to
each other over the wire, and a mix of stale and fresh ones fails as a decode
error a long way from the cause.

The suite compiles nothing and mocks nothing. Cargo builds the example
*libraries* because they are dev-dependencies, but not their binaries and not
the `astrs` binary, so the build line above is a real precondition — a missing
binary fails with exactly the `cargo build -p …` line that produces it. The
suite never skips: one that silently skipped would prove nothing.

Only the two lines above need typing by hand. The repository gate runs the
same build for you: `cargo xtask preflight` (and therefore
`scripts/ci-local.sh`) has a step of its own for it, immediately before
`cargo nextest run --workspace --all-features` and under the same
`--all-features` resolution — see `CONFORMANCE_BINARY_PACKAGES` in
`xtask/src/preflight.rs`, which is this same list of ten. A bare `cargo
nextest run` typed by hand is *not* that gate and still needs the build line.

## The files, and why there are this many

**M1 — one machine.**

| File | Lane | What only it proves |
|---|---|---|
| `tests/m1_single_machine.rs` | the run verb, **in process** | the dataflow machinery, with the whole captured terminal available to explain a failure |
| `tests/m1_cli_process.rs` | the `astrs` binary, **as a child process** | clap parsing, `main`'s exit code, the real process's stdout — the command an adopter types |
| `tests/m1_example_estate.rs` | the committed files themselves | that the manifests, Cargo packages, workspace members and READMEs still agree with each other |

**M2 — the distributed system**.

| File | What only it proves |
|---|---|
| `tests/m2_multi_daemon.rs` | one coordinator, two daemons, real node processes on two Unix sockets, and a graph split by `deploy.machine` whose payloads cross a peer route in both directions |
| `tests/m2_record_replay.rs` | The byte-for-byte recording claim: what the sensor generated, what the `.arec` holds and what replay delivered are the identical list of payloads |
| `tests/m2_dora_migration.rs` | The dora contract's *migrates **and runs***: a dora descriptor through the real `migrate from-dora`, then through `astrs run` |
| `tests/m2_fault_tolerance.rs` | The restart budget as a ceiling, and a failing node's **typed** cause reaching its peer as an ordinary event |
| `tests/m2_example_estate.rs` | the estate rules M1's file cannot see: no two packages building the same binary name, and every *companion* manifest still valid |

`fixtures/` holds inputs that are not examples — a dora descriptor is input to
a verb, and what gets run is the verb's output.

The first two overlap on purpose. An in-process call gives a failure the
richest possible context; a subprocess proves the part of the program that a
library call skips. Neither substitutes for the other.

The third needs no process at all. It is the drift guard: a manifest naming a
binary the package stopped building, a `build:` line naming the wrong package,
an example missing from `members` or from the index README, a dependency
pinned locally instead of at the workspace. Each of those is silent until
someone runs the example by hand.

## `src/` — the machinery

| Module | Problem it solves |
|---|---|
| `paths` | finding another package's binaries without `CARGO_BIN_EXE_`, which does not cross packages |
| `stage` | rewriting a committed manifest so it runs from a temporary directory against binaries cargo already built |
| `cli` | spawning the real `astrs` binary, draining both pipes concurrently, killing and still reporting on a timeout |
| `report` | reading the JSON the `--json` verbs print out of a stream that also carries node output |
| `error` | one error type, each variant written so its message is a next action |

### Finding the binaries

`CARGO_BIN_EXE_<name>` exists only for binaries of the *same* package, and the
examples are separate packages. `std::env::current_exe()` is the test binary
under `<target>/<profile>/deps/` — but that is exactly one directory away from
where cargo puts every workspace member's binaries, so
`current_exe()/../..` resolves it whatever `CARGO_TARGET_DIR` was set to and
whichever profile is running. `ASTRS_CONFORMANCE_BIN_DIR` overrides it for a
harness that stages binaries itself.

### Staging, and the one test that does not stage

Staging rewrites each node's `path:` to an absolute location, which is what
makes a run independent of the profile and of `CARGO_TARGET_DIR`. It would
also happily keep passing if the committed `../../target/debug/…` path had
rotted, so `the_committed_hello_timer_manifest_runs_verbatim` runs one
manifest exactly as written, from the workspace root, with the command the
README prints.
