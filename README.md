# AstRS

**The Pure Rust dataflow middleware for the robotic age.**

AstRS (*Astro Boy + Rust*, pronounced "asters") is a from-scratch, 100% Pure
Rust robotics dataflow middleware: a drop-in successor to **dora-rs** and a
practical replacement path for **ROS 2**. It is a COOLJAPAN Ecosystem
flagship project.

Dataflow graphs of isolated OS processes exchange self-describing columnar
messages over one wire spec and one payload format — not dora's
JSON+postcard+Zenoh mix, not whichever serialization a DDS vendor picked.
Same-host legs get a purpose-built zero-copy shared-memory ring; cross-host
legs get TCP or QUIC; ROS 2 nodes are reached directly over a from-scratch
RTPS 2.3 stack. No Zenoh, no DDS vendor runtime, no C/C++/Fortran anywhere in
the dependency tree, for any feature combination, on any target. Manifests
are typed and machine-checkable, with an optional SMT-backed prover that
discharges deadlock-freedom, queue-boundedness and rate-consistency
obligations before a graph ever boots.

## Why AstRS

Robotics middleware today forces a choice between two compromises: ROS 2 is
the industry standard but a C/C++ colossus with no memory-safety story and
per-vendor DDS QoS quirks; dora-rs has the right architecture thesis
(dataflow graphs of isolated processes, Arrow payloads, YAML graphs,
zero-copy) in the right language, but delegates its data plane to Zenoh,
speaks three wire formats across its own control plane, and pulls a C
libgit2+OpenSSL build into every install via `git2`. AstRS keeps dora's
thesis and rebuilds every layer dora outsources — transport, shared memory,
DDS, telemetry export — as a first-party, Pure Rust crate.

Six bets, none of which an incumbent can match without a rewrite:

1. **One binary, zero ceremony** — `cargo install astrs` gives you
   coordinator, daemon, runtime, CLI, TUI, recorder and ROS 2 bridge. No
   colcon, no `setup.bash`, no Python environment.
2. **Pure Rust, all the way down** — no C/C++/Fortran in the default build,
   for any feature or target; see [Pure Rust policy](#pure-rust-policy).
3. **A coherent wire story** — one codec on every control leg, one payload
   format on every data leg, where dora ships JSON+postcard+Zenoh
   simultaneously and ROS 2 ships whatever the DDS vendor decided.
4. **First-party zero-copy** — a purpose-built shared-memory plane with
   generation-stamped segments and crash-safe reclamation, not a general
   pub/sub library's SHM mode.
5. **Native ROS 2 citizenship without ROS 2** — a from-scratch RTPS 2.3
   stack interoperates with existing ROS 2 nodes on the wire (Humble and
   Jazzy GID layouts alike), with zero ROS installation required.
6. **Provable graphs** — `astrs validate --prove` discharges
   deadlock-freedom, queue-boundedness and rate-consistency obligations
   through an SMT solver before a graph boots. No incumbent middleware
   claims this.

What specifically gets fixed, verified against dora-rs `1.0.0-rc.4` source
rather than its docs:

| dora defect (verified against source) | AstRS fix |
|---|---|
| Heterogeneous serialization: JSON over WebSocket (CLI↔coordinator, coordinator↔daemon), postcard (daemon↔node, daemon↔daemon), binary WS frames with a UUID prefix (topic proxy) | One frame format everywhere: `oxicode`-encoded, length-prefixed, CRC-protected envelopes on every leg — see `astrs-wire`'s module docs for the exact byte layout. JSON only at the human boundary (`--json` CLI output) |
| Data plane delegated to Zenoh — a large dependency tree, its own SHM provider, key-expression routing, a second config surface | First-party planes: `astrs-shm` (same host), `astrs-transport` (cross host, TCP/QUIC), `astrs-discovery` (rendezvous). No routing middleware in the middle |
| `git2` with vendored OpenSSL — a C library build in the default install | Manifest `git:` sources shell out to the system `git` binary; no libgit2, no OpenSSL |
| ROS 2 via `rustdds`/`ros2-client` → an unfixable `mio 0.6.23` (a null-deref UB under debug assertions, abandoned since 2019) | A from-scratch `astrs-rtps` on tokio UDP sockets — no `mio` 0.6 anywhere in the tree |
| `output_framing` is dead manifest config: parsed, schema'd, documented, read by nothing | The field does not exist (see [Dataflow manifest](#dataflow-manifest)) — config never lies about what a frame looks like |
| Stale architecture docs describing removed subsystems | The wire protocol's enum shapes are frozen into golden snapshot files (`crates/astrs-wire/tests/golden/protocol{,.frozen}.snap`) that `cargo xtask snapshot-protocol` verifies on every release preflight — a reorder or removed variant fails the check, not a stale paragraph |
| Python-first runtime concerns (`uv` venv management, pip rewriting) baked into core build code | Out of core entirely; Python support would be one feature-gated adapter crate, never a core dependency |

dora ships roughly 30 real crates plus ~50 example/test fixtures; AstRS
carries a substantially wider surface (see [Workspace map](#workspace-map))
because it internalizes what dora imports — the columnar layer, the
transport, the DDS stack, the HLC, and the telemetry exporter.

## Status

**0.1.0** (2026-09-06) — the first release. 40 publishable crates and
binaries: the full substrate (wire protocol, columnar data, shared memory,
transport, discovery, coordinator persistence, the workspace's own YAML 1.2
reader), the daemon/coordinator/runtime orchestration layer, the complete
ROS 2 interop stack (CDR, RTPS 2.3, IDL codegen, an rcl-level client
library, rosbag2, tf2, URDF), the built-in signal and vision node
libraries, the deterministic simulation harness, the C API, and the CLI/TUI
tooling are all implemented and exercised by the workspace test suite —
9,696 tests on default features and 9,854 with `--all-features`, zero
failures. See
[Feature status](#feature-status) for what is done, what is deliberately
partial, and what is out of scope for 0.1.0.

## Quickstart

The fastest path to a running graph is one node scaffolded against itself:

```sh
cargo install astrs-cli    # installs the `astrs` binary
astrs new node hello       # scaffolds hello/{Cargo.toml,src/main.rs,dataflow.yml}
cd hello
cargo build --release
astrs validate dataflow.yml
astrs run dataflow.yml
```

`astrs new graph <name>` instead scaffolds a multi-node starter manifest for
you to wire real nodes into by hand; `astrs validate` type-checks either
shape before anything runs.

The path-dependency equivalent of that loop is one of this repository's own
examples, and running it end to end is how the flow above was checked:

```console
$ cargo build -p hello-timer
$ astrs run examples/hello-timer/dataflow.yml
   0.176s [greeter] hello from greeter, 10 ticks to go
   0.219s [greeter] tick 1 at hlc 1787562511209225000-0
   ...
   2.019s [greeter] done after 10 ticks
   2.019s [greeter] hello-timer: greeted 10 ticks
   2.033s [greeter] exited (exited successfully)
dataflow 01a03307-4a90-7772-ac55-3076cd385431 finished (1 node(s), 0 failed)
```

Twenty runnable example graphs live under [`examples/`](examples/) — see
[`examples/README.md`](examples/README.md) for the full table, from the
one-node `hello-timer` through multi-daemon clusters, record/replay, and
live ROS 2 interop with no ROS installed.

Prove a graph before running it — deadlock freedom, queue boundedness, rate
consistency, latency budgets and type-rule consistency, discharged through
the [OxiZ](https://github.com/cool-japan/oxiz) SMT solver:

```console
$ astrs validate --prove examples/hello-timer/dataflow.yml
[warning] (prove) 1 obligation(s) lack a declaration to decide them and 0 were left undecided; this proof rules out what it lists, and no more
0 error(s), 1 warning(s)

graph proofs — 1 node(s), 1 channel(s), 1s analysis window
5 obligation(s): 1 hold, 0 violated, 4 not attempted, 0 inconclusive

obligations
  ok   deadlock freedom
       no channel can be permanently starved, and every service or action correlation can be answered
  skip queue boundedness (node `greeter`)
       node `greeter` has no declared service time; declare `nodes.greeter.wcet` in a verification profile
  ...
```

No incumbent robotics middleware ships this, and the shipped CLI binary has
it on by default (see [Feature status](#feature-status)).

## Dataflow manifest

A dataflow is one YAML file. Node identity, wiring, types and fault-tolerance
policy all live here; `astrs-schema.json` (generated by `cargo xtask schema`,
checked by `cargo xtask schema --check`) is the authoritative JSON Schema,
consumed by editors for completion and validation.

```yaml
astrs: "1"                 # manifest format major (optional, default 1)
name: perception-demo
health_check_interval: 5.0
exit_when_nodes_finish: true

nodes:
  - id: camera
    path: ./target/release/camera-node
    build: cargo build --release -p camera-node
    outputs: [frames]
    output_types: { frames: "std/media/v1/Image[pixel=rgb8]" }
    env: { CAMERA_INDEX: 0 }

  - id: detector
    git: https://github.com/cool-japan/astrs-yolo
    tag: v0.3.1
    build: cargo build --release
    path: target/release/yolo-node
    inputs:
      frames:
        source: camera/frames
        queue_size: 2
        queue_policy: drop_oldest
    outputs: [detections]
    output_types: { detections: "std/vision/v1/Detections" }
    restart_policy: on_failure
    max_restarts: 5

  - id: recorder
    record: [camera/frames, detector/detections]   # sugar → astrs-record-node

  - id: planner
    path: ./planner
    deploy: { machine: robot-1 }
    inputs:
      detections: detector/detections
      tick: astrs/timer/hz/50
```

`nodes` is the only required root key; `name`, `health_check_interval`,
`exit_when_nodes_finish`, `strict_types`, `type_rules: [{from, to}]`, a
graph-wide `env` and a default `deploy` round out the root, all
`deny_unknown_fields`-checked.

Every node needs exactly one **source**: `path` (a built binary), `git`
(+`branch`/`tag`/`rev`), `hub:` (a package-index reference resolved by the
coordinator — see [Feature status](#feature-status)), `module` (a reusable
sub-graph), `operators:` (runtime-hosted, in-process), `ros2:` (a bridge
node), `record:` (sugar for the recorder node), or `path: dynamic` (an
externally attached node); `shell:` sources exist behind `--allow-shell`.
Inputs accept a short `node/output` form or the long
`{source, queue_size, queue_policy, timeout}` form; `input_types`/
`output_types` carry type URNs (`std/<category>/v<n>/<Type>[k=v,…]` — see
[Type URNs](#type-urns)). Fault tolerance is `restart_policy`
(`never`/`on_failure`/`always`), `max_restarts`, `restart_delay` (backing
off up to `max_restart_delay`), `restart_window` and
`health_check_timeout`/`finish_grace_secs`. Placement and performance are
`deploy: {machine, labels, working_dir}`, `cpu_affinity`, `shm_pool_size`,
and the real-time `rt: {policy, priority}` block (see
[Feature status](#feature-status) for where that's honored today).

**Virtual sources** need no `nodes` entry to wire against:
`astrs/timer/millis/N` · `astrs/timer/secs/N` · `astrs/timer/hz/N` ·
`astrs/logs[/level[/node]]` · `astrs/status` (peer lifecycle events as an
ordinary input, so a supervisor-shaped node reacts to restarts in-graph).

**Modules** are reusable sub-graphs: a manifest with its own `module:`
header (`name`/`inputs`/`outputs`), wired internally via `_mod/<port>` and
expanded at validate-time into `parent.child` ids — `astrs expand` prints
the flattened result. The runtime never sees a module as such.

**dora migration**: `astrs migrate from-dora dataflow.yml` performs a
mechanical mapping — timer paths, `dora/…` → `astrs/…`, restart fields,
patterns, modules — and emits a labeled TODO comment for anything with no
AstRS equivalent (`hub:`'s dora shape, `_unstable_debug`, `conda_env`)
rather than silently dropping it.

## Writing a node

```rust
use astrs::prelude::*;

#[derive(AstrsMessage)]              // derives columnar encode/decode + type URN
#[astrs(urn = "std/vision/v1/Detections")]
struct Detections { boxes: Vec<[f32; 4]>, scores: Vec<f32>, labels: Vec<u32> }

fn main() -> Result<(), NodeError> {
    let (mut node, mut events) = Node::init_from_env()?;   // or Node::builder()…
    let detections = node.output::<Detections>("detections")?;  // typed handle

    while let Some(event) = events.recv() {                // sync; .recv_async() too
        match event {
            Event::Input { id, data, meta } if id == "frames" => {
                let img: ImageView = data.view()?;          // zero-copy view into SHM
                detections.send(run_model(&img)?, meta.follow())?;
            }
            Event::InputClosed { .. } => {}
            Event::Stop(_) => break,
            _ => {}
        }
    }
    Ok(())
}
```

`Node::init_from_env()` reads the daemon-issued `ASTRS_NODE_CONFIG` handshake
blob; `Node::init_from_node_id` attaches dynamically (`path: dynamic`), and
`Node::init_testing` spins an in-process daemon for unit tests with no
manifest at all. `EventStream` implements both `Iterator` and `Stream`, and
fuses once a `Stop` event has been observed.

`#[derive(AstrsMessage)]` maps struct fields onto the closed columnar type
set at compile time; a mismatch against the manifest's declared URN fails
`astrs validate` statically and `Node::init` dynamically. `ASTRS_TYPE_CHECK`
(`off`/`warn`/`error`, default `warn`) controls how loudly the dynamic check
complains. Correlated service/action ports are exempt from the runtime
payload check, matching dora's own behavior here.

In-process stages implement `Operator` instead of owning a process:

```rust
pub trait Operator: Default + Send {
    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status>;
}
astrs::register_operator!(MyOp);
```

Operators compiled in via the registry macro run on their own thread inside
`astrs-runtime`'s shared event loop; a panic is caught, reported as
`NodeFailed`, and honors the operator's own restart policy without tearing
down its siblings. Loading an operator from a `dylib` or a `.wasm` module at
runtime — instead of compiling it in — is also implemented, behind the
non-default `dylib-operators`/`wasm-operators` features (see
[Feature status](#feature-status)).

Services, actions and streams all ride ordinary edges plus metadata
correlation rather than a separate RPC subsystem: services correlate on
`request_id`; actions walk a `goal_id`/`goal_status` state machine
(`Accepted → Executing → {Succeeded, Aborted, Canceled}`); streams correlate
on `session_id`/`segment_id`/`seq`/`fin`/`flush`. The scheduler grants every
correlated message queue-eviction immunity — dropping a reply to make room
would wedge a client forever. Setting `pattern:` in the manifest
(`service-server`/`service-client`/`action-server`/`action-client`)
generates the matching port pairs and validates the wiring.

## Feature status

Honest status, not aspiration — every row below was checked against the
tree, not copied from a specification.

| Area | Status |
|---|---|
| Dataflow runtime (`astrs run`, single process) | **Implemented** — daemon, scheduler, node API, zero-copy SHM plane |
| Cluster mode (`astrs up`/`down`, coordinator + daemons) | **Implemented** — daemon registry, dataflow FSM, state catch-up on reconnect after a partition |
| Cross-host transport | **Implemented** over TCP (default) and UDS. QUIC is implemented but ships behind the non-default `quic` feature: `oxiquic` 0.2.1's server-endpoint constructors name `rustls` types this workspace's retained-crate list doesn't re-export, so TCP — identical framing, same mux — is the default cross-host plane until that's resolved upstream |
| Same-host zero-copy (SHM) | **Implemented** — generation-stamped rings, crash-safe (`kill -9`-survives) reclamation, slow-start upgrade from the reliable daemon path |
| Windows shared memory | **Partial, and documented as such.** Named file mappings (`CreateFileMappingW`/`OpenFileMappingW`/`MapViewOfFile`) over `windows-sys` ABI declarations only — no C compiled, and never in a Unix build's dependency tree — so segment create/open/map is real. `Producer`, `Consumer`, `Doorbell` and `SegmentBroker` stay `#[cfg(unix)]`-only: a Windows doorbell and liveness watch need the `Win32_System_Threading` surface this workspace does not enable. Linux and macOS are the 0.1.0 targets |
| `astrs validate --prove` (SMT graph proofs) | **Implemented** — deadlock freedom, queue boundedness, rate consistency, latency budgets, type-rule consistency. The `verify` feature (real solving, via OxiZ) is **on by default in the `astrs` CLI binary**; the `astrs-verify` library itself keeps it opt-in, reporting `SolverUnavailable` instead of a verdict when it's off |
| Deterministic replay (`astrs run --deterministic`) | **Implemented.** `astrs run --deterministic --from-recording <session.arec>` stands down every node the recording covers (`path: dynamic`, never spawned) and drives it from the recording's HLC stream through `astrs-daemon`'s recorded-clock timer wheel (`ReplaySource`) instead of the wall clock — `--speed` paces the walk, or omit it to replay as fast as the loop can; verified end to end against a real `.arec` session. `--deterministic` without `--from-recording` is refused before anything is built or spawned (`CliError::DeterministicNeedsRecording`) rather than silently running non-deterministically |
| Record / replay / bag convert | **Implemented** — `astrs record start/stop` against the `.arec` container (writer, reader, merger, seekable index); `astrs replay <file> --into <manifest>` rewrites a manifest to replay recorded outputs offline; `astrs replay <file> <dataflow>` performs the same rewrite live against a running dataflow's expanded manifest and cuts it over in place via `ReplaceNode` (`bins/astrs-cli/src/command/replay.rs`'s `run_live`); `astrs bag info`/`convert` across `.arec`/`.db3`/`.mcap` |
| ROS 2 interop | **Implemented** — CDR/XCDR1 (+XCDR2 read), a from-scratch RTPS 2.3 stack (SPDP/SEDP discovery, best-effort and reliable delivery, fragmentation, Humble 24-byte / Jazzy 16-byte GID layouts), `.msg`/`.srv`/`.action` codegen with `common_interfaces` pre-generated in-tree, an rcl-level client library (nodes, topics, services, actions, parameters, graph introspection), and the declarative `ros2:` bridge node. Reachable today via `astrs-ros2`/`astrs-rtps`/etc. directly, or through the CLI and `astrs-ros2-bridge-node` — **not yet aggregated into the `astrs` facade crate's own feature flags** (its `full` feature predates this pillar landing, so `cargo add astrs --features ros2` isn't a thing yet) |
| DDS-Security | **One half of one of the specification's five plugins, by design — not "not yet".** `astrs-rtps`'s `security` module does submessage protection (`SEC_PREFIX`/`SEC_BODY`/`SEC_POSTFIX`, AES128/AES256 GCM and GMAC, HKDF-SHA-256 session keys, a sliding anti-replay window), keyed by a pre-shared key an operator configures on both endpoints. No authentication handshake, no access control, no whole-message or serialized-payload protection — and therefore **no interoperability with a PKI-based DDS-Security stack**. What it buys today: a `DATA` on `/cmd_vel` an attacker on the same network cannot forge, alter, replay or (under `encrypt`) read. Every primitive comes from `oxicrypto`; none is implemented here |
| URDF robot descriptions (`astrs-urdf`) | **Implemented** — a zero-dependency XML pull parser, the link/joint model (geometry, inertials, materials, limits, `<mimic>`), tree-shape validation, its own SE(3) math, kinematic chains and forward kinematics, and `populate_static_transforms` publishing the fixed-frame skeleton straight into an `astrs_tf::TransformBuffer`. Carrying a robot's *live* joint state as columnar messages is the layer above and is not here — the crate's `astrs-data` edge is reserved for it and unused today, which its own docs state outright |
| `astrs migrate from-dora` / `from-ros2` | **Implemented** — mechanical dora descriptor mapping (nodes, timers, restart/queue policies, service/action patterns) and best-effort ROS 2 launch-file skimming, both emitting a migration note for anything that needs a human rather than dropping it silently |
| rosbag2 (`.db3` / `.mcap`) | **Implemented** — `.db3` read and write via `oxisql-sqlite-compat` (no C SQLite in the build), `.mcap` read, conversion either direction against `.arec` |
| tf2 transform tree | **Implemented** — SE(3) math, static/dynamic frame tree with conflict detection, time-travel interpolated lookup, `/tf` and `/tf_static` bridging both directions |
| TUI (`astrs top`) | **Implemented** — live dataflow/graph/logs/timeline tabs against a running cluster, and an `--replay` mode that drives the same views from a `.arec` file with no cluster at all |
| Telemetry export (OTLP/HTTP) | **Implemented** — a hand-rolled OTLP/HTTP+JSON exporter over `oxihttp`; no `opentelemetry`/`tonic` dependency tree. On by default (`telemetry-export` feature) |
| `xtask` release automation | **Implemented.** `cargo xtask schema/snapshot-protocol/layer-lint/preflight` all dispatch through a real CLI — see [Repository automation](#repository-automation) |
| Coordinator high availability | **Implemented**, behind `astrs-coordinator`'s non-default `ha` feature — the durable registry replicated across an odd-sized coordinator set through `astrs-raft`, so losing a coordinator costs one election timeout instead of the fleet. Mutating verbs are accepted only by the leader and only once a majority holds the entry; reads are served locally under a leader lease. A follower refuses a mutation with the ordinary structured `Unavailable` error carrying `leader: <address>` — no wire enum gained a variant for HA at all. Reached through `astrs coordinator --ha-node-id <n> --ha-peer <id=host:port>`; a build without the feature parses those flags and refuses them rather than ignoring them |
| Operators loaded at runtime (dylib / WASM) | **Implemented**, both behind non-default features so a runtime serving only compiled-in `register_operator!` types pays for neither. `dylib-operators` opens an `operators[].dylib:` path through `libloading` — the platform's own `dlopen`/`LoadLibrary`, no C compiled and no build script — checking the ABI version and operator name the library advertises; `wasm-operators` runs an `operators[].wasm:` module on `wasmi`, a pure-Rust interpreter with no JIT. `catch_unwind` on both sides of both boundaries. `astrs new operator-dylib` scaffolds a crate against the ABI |
| C API (`astrs-capi`) | **Implemented** — one crate built as `lib`, `staticlib` and `cdylib`, an opaque-handle `extern "C"` surface in which no Rust type ever appears in a signature, an `AstrsStatus` whose every failure is negative (so the idiomatic C-side test is `< 0`), a thread-local last-error slot, `catch_unwind` at every entry point, and a hand-written `include/astrs.h`. Rust *exporting* a C ABI, never Rust *compiling* C — see [Pure Rust policy](#pure-rust-policy) |
| Hard real-time reservations (`rt:`) | **Implemented, and bounded on purpose.** The manifest `rt: { policy: normal\|fifo\|rr, priority: 1..=99 }` block is validated in the same report-everything-at-once pass as the rest and applied through `sched_setscheduler` in a `pre_exec` hook on **Linux x86_64/aarch64**; every other platform reports `RtOutcome::UnsupportedPlatform` with a warn log and a counter, and an `EPERM` is a spawn failure naming `CAP_SYS_NICE`/`RLIMIT_RTPRIO`, never a silent downgrade. `NodeSpawnSpec` cannot carry `rt` without moving bytes the frozen protocol prefix pins forever, so reservations are registered from the manifest by **`astrs run`'s single-process daemon only** — a coordinator-admitted remote daemon has no carrier for them yet. `astrs doctor` probes the same ceiling ahead of time |
| `astrs hub` package index | **Implemented** — `hub: yolo-detector@v0.3.1` (or the structured `name`/`rev` map) as a manifest node source, resolved into a git clone and checkout at dataflow-start time by the *coordinator*, so a standalone `astrs coordinator` resolves manifests submitted over the wire without a CLI verb ever running. The index is a minimal, self-hostable git repository of `packages/<name>.json` files with append-only version lists, each version naming the exact commit or tag it resolves to. `astrs hub init/update/search/info` drive the system `git` binary, never `git2`/libgit2 |
| `astrs token mint --scope read\|mutate` | **Implemented** — a read-only cluster credential derived from the root token with HKDF instead of stored as a second secret, so handing out an observer credential needs no coordinator restart, no redistribution and no wire change (`ControlRequest`'s read/mutate split was already frozen). Purely local: minting never dials a coordinator, and a coordinator configured from the same root token reaches the identical 32 bytes |
| Simulation (`astrs-sim`) | **Implemented** — a deterministic fixed-step harness driving a real dataflow against a simulated robot: the same graph, ordinary `astrs-node-api` nodes, the same columnar payloads. Simulated time is always `step × count`, never an accumulated `f64`. Occupancy grid, DDA lidar raycast, closed-form unicycle drive, an odometry model kept apart from ground truth, `astrs-urdf` forward kinematics for arms, a frozen-forever in-crate `SplitMix64`, and a `trajectory_hash` determinism check. Three binaries and a `dataflow.yml` wire it into a runnable graph |
| Built-in node libraries | **Implemented** — `astrs-nodes-signal` (FFT/IFFT on `oxifft`, windows, RBJ biquads, windowed-sinc FIR, polyphase resampling, STFT spectrograms, moving/exponential averages) and `astrs-nodes-vision` (an owned `ImageBuffer`, pixel-format conversion, resize, Gaussian blur, Sobel, fixed and Otsu thresholding, morphology, connected components, drawing, a pinhole camera model with plumb-bob undistort). Each stage ships twice over: a plain callable Rust function *and* an `Operator` configured from a manifest. The PNG codec is written from scratch over `oxiarc-deflate`; **JPEG is out of scope outright** — `std/media/v1/CompressedImage` carries a JPEG frame through the graph unopened |
| arrow-rs interop (`arrow-interop`) | **Implemented**, off by default — zero-copy `From`/`TryFrom` between AstRS's own columnar arrays and arrow-rs, plus `RawOutput::send_arrow` for handing one straight to an arrow-native pipeline. Surfaced by `astrs-data`, `astrs-node-api` and the `astrs` facade (and turned on by `full`). This is the one feature in the workspace that pulls the four `arrow-*` crates into a build, and `deny.toml` confines that edge to `astrs-data` alone so no other crate becomes a parent of arrow-rs — see [Pure Rust policy](#pure-rust-policy). Everything else about `astrs-data` is Arrow-IPC-compatible *without* arrow-rs: the IPC stream is hand-encoded and gated on golden vectors |
| Manifest YAML (`astrs-yaml`) | **Implemented** — the workspace's own YAML 1.2 core-schema reader and writer, and the end of the last unmaintained third-party dependency in the graph. One-based `Position`/`Span` on every syntax error, per-parse `Limits` on size/depth/alias expansion, anchors and aliases, local and verbatim tags, `%TAG` handles, multi-document streams, and a deterministic block emitter that round-trips every `Value`. Not implemented, and said so in the crate's own docs rather than left to be discovered: merge keys (`<<:`), the YAML 1.1 types (`!!binary`, `!!timestamp`, sexagesimals, `yes`/`no` booleans), and recursive aliases |
| Windows daemon / transport | **Not implemented.** `astrs-transport`'s UDS backend and `astrs-telemetry`'s process sampler have no Windows branch, so `astrs-daemon` does not cross-compile to `x86_64-pc-windows-msvc`. A real Windows leg means a named-pipe (or loopback-TCP) transport plus a redesigned peer-identity story — Unix domain sockets are chosen here specifically for kernel-vouched uid/gid/pid credentials — which is a new-platform IPC design, not a compile fix. Linux and macOS are the 0.1.0 targets |

## Design principles

Ten rules that function as this repository's standing review contract —
every change is checked against them, not just the test suite:

1. **Pure Rust, default-clean.** The default feature set compiles zero C,
   C++ or Fortran. OS syscalls go through `rustix` (Unix) or `windows-sys`
   (Windows). Anything that must link foreign code lives in a non-default,
   clearly named adapter crate — never the default build.
2. **Minimal dependencies, maximal ownership.** If a capability is core to
   the product (codec, transport, columnar data, DDS, telemetry export),
   AstRS owns the implementation rather than depending on it.
3. **One spec per concern.** One frame format. One payload format. One
   manifest schema. One CLI. Where dora or ROS 2 offer N ways, AstRS offers
   one good way plus an explicit compatibility adapter.
4. **Append-only protocol evolution.** Every wire enum is
   `#[non_exhaustive]`, encoded by stable variant index, extended only at
   the tail. A single protocol version is negotiated at handshake on every
   leg.
5. **Crash-first design.** Every resource (SHM segment, extension entry,
   route, child process) has a defined owner, a generation stamp, and a
   reclamation path that works when the owner dies without goodbye.
6. **Determinism as a feature.** HLC-timestamped event logs, seedable
   schedulers, and a replay mode that reproduces an execution from a
   recording. "It only happens on the robot" becomes a replayable file.
7. **Typed by default, dynamic by consent.** Ports carry type URNs; graph
   edges type-check at `validate`. Raw/untyped ports remain available but
   are explicit (`type: any`).
8. **No unwrap, no warnings, no files over 2000 lines.** This is not a
   cleanup phase; it is the review gate for every change.
9. **Meet users where they are.** dora manifests migrate mechanically; ROS 2
   nodes interop over the wire; rosbag2 files open natively. Adoption cost
   is meant to be an afternoon, not a quarter.
10. **Honesty over aspiration.** [Feature status](#feature-status) below is
    checked against the tree on every edit, not copied from a plan. What
    isn't done says so, plainly.

## Architecture

Four layers, dependencies strictly downward — enforced by `cargo deny` and
`cargo xtask layer-lint` (zero violations as of this checkout):

```
Interfaces         astrs-cli / astrs-tui (one `astrs` binary) · astrs-node-api · astrs-operator-api(+macros)
                   astrs-capi (C ABI) · astrs-nodes-signal / -vision · astrs-sim
Orchestration        astrs-coordinator · astrs-daemon · astrs-runtime
Domain libraries       astrs-manifest/-graph · astrs-scheduler · astrs-recording · astrs-verify · astrs-migrate · astrs-raft
ROS 2 pillar             astrs-cdr · astrs-rtps · astrs-idl · astrs-ros2 · astrs-rosbag · astrs-tf · astrs-urdf
Observability             astrs-telemetry · astrs-log
Substrate                   astrs-wire · astrs-data · astrs-shm · astrs-transport · astrs-discovery · astrs-time · astrs-store · astrs-yaml
```

One machine's process topology:

```
                     coordinator :7407   cluster-wide truth: daemon registry,
                          │               dataflow FSM, param store, log/topic fan-out
                          │  wire over TCP or QUIC (control)
                     daemon :7408 + UDS socket
                      │  spawns/supervises node processes, brokers the local
                      │  SHM plane, bridges cross-host routes, heartbeats up
                      ├── node             one OS process per manifest node
                      ├── node
                      └── runtime           hosts several in-process operators
                                             on one shared event loop
```

`astrs run` collapses this to a single process: an embedded daemon, no
coordinator socket, the whole graph under one supervisor with an orphan
guard. Data never transits the daemon on the happy path — nodes publish
directly into SHM rings (same host) or open a direct transport route (cross
host); the daemon is the control-plane actor and the reliability fallback
(slow-start handshake). Every process runs one merged event loop
(`tokio::select!`) over peer connections, node messages, timers and internal
channels; every event is `Stamped<T>` — HLC-timestamped by `astrs-time` —
which is what gives the recorder and the TUI timeline cluster-wide causal
ordering.

## Performance gates

Six criterion-tracked latency/throughput targets, each
backed by a real bench in-tree (not aspirational numbers):

| Benchmark | Target (0.1.0) | Bench |
|---|---|---|
| Same-host SHM handoff, 4 MB frame, p99 | < 120 µs | `cargo bench -p astrs-shm --bench handoff` |
| Same-host small message (256 B) RTT, p99 | < 25 µs | `cargo bench -p astrs-shm --bench handoff` |
| Cross-host transport, 4 MB frame, LAN, p99 | < 4 ms | `cargo bench -p astrs-transport --bench mux_loopback` |
| Timer jitter @ 1 kHz, p99 | < 150 µs | `cargo bench -p astrs-scheduler --bench timer_jitter` |
| `astrs run` cold start, 10-node graph | < 800 ms | `cargo bench -p astrs-benches --bench cold_start` |
| RTPS pub → ROS 2 sub, 1 KB, p99 | < 1.5 ms | `cargo bench -p astrs-rtps --bench loopback_pubsub` |

That third row was specified as "Cross-host **QUIC** 4 MB frame"; the in-tree
bench measures the mux/framing layer over **TCP** loopback instead, because
that is the default cross-host plane today (see [Feature
status](#feature-status) on QUIC) — same framing, so the same numbers
apply once QUIC is the transport underneath. The RTPS row's in-tree bench
similarly measures loopback self-interop between two real AstRS
participants (the in-repo evidence standard), not a live
Fast-DDS/CycloneDDS stack; that cross-stack validation lives in a separate
out-of-repo project, listed in [`TODO.md`](TODO.md).

Run all six and get one pass/fail verdict:

```sh
scripts/bench-gate.sh
```

This is an **advisory, local gate**, not a CI job — the repository runs no
CI beyond the publish workflows — so run it by hand on an
otherwise-quiet machine before trusting a borderline result; see the
script's own header for why wall-clock latency benches are sensitive to
machine load.

## Build & test

Requires a stable Rust toolchain at or above the workspace MSRV (1.95) with
the 2024 edition.

```sh
cargo check --workspace
cargo nextest run --workspace --all-features   # or: cargo test --workspace --all-features
cargo test --workspace --doc --all-features    # every public API's doc example
cargo clippy --workspace --all-features -- -D warnings
cargo deny check bans                          # dependency ban list
```

For 0.1.0 that suite is **9,696 tests on default features and 9,854 with
`--all-features`, zero failures** (7 skipped either way), plus 1,174
doctests. The real-time end-to-end tests measure wall-clock budgets over
loopback sockets, so run them on an otherwise-quiet machine — under heavy
concurrent CPU load a latency-budgeted test can flake.

Structured, pure-Rust fuzzing (no `cargo-fuzz`/libfuzzer — that links a C++
runtime, which the Pure Rust policy excludes outright) runs at low iteration
counts inside the default test suite; a deeper nightly pass is:

```sh
scripts/fuzz-nightly.sh
```

`scripts/ci-local.sh` is the per-change gate (repo policy allows no CI
workflow beyond the publish ones, so this script is what stands in for
one) — a thin wrapper around `cargo xtask preflight`, which
runs every quality gate as one ordered pass rather than stopping at the first
failure: the graph-only structural checks first (the layer lint, the
manifest JSON-schema `--check`, the ≤ 2000-line file-size audit, a
no-inline-version-pins sweep over every member's dependency tables, and a
`*-sys` sweep resolving the whole workspace graph once per feature mode),
then `cargo fmt --check` and `cargo deny check bans` in
both its default- and `--all-features` forms, then the wire-protocol
snapshot freeze, then the same nextest/doctest/`cargo doc` commands listed
above plus clippy with `--all-targets` added (reaching tests/benches/
examples too, beyond what `--all-features` alone covers):

```sh
scripts/ci-local.sh                     # every gate above, one ordered pass
scripts/ci-local.sh --publish-dry-run   # ... plus a dry-run publish of all 40, in dependency order
```

## Workspace map

40 publishable crates and binaries, one line each from their own
`Cargo.toml` `description`. Per-crate READMEs (linked below) go into more
depth on each one.

### Substrate

| Crate | What it is |
|---|---|
| [`astrs-wire`](crates/astrs-wire/) | AstRS control-plane protocol types, framed codec and version negotiation |
| [`astrs-time`](crates/astrs-time/) | Hybrid logical clock, monotonic and wall clocks, deadlines and `Stamped<T>` |
| [`astrs-data`](crates/astrs-data/) | Arrow-IPC-compatible columnar arrays, aligned buffers, schema hashing and kernels |
| [`astrs-shm`](crates/astrs-shm/) | Crash-safe POSIX shared-memory rings powering the zero-copy same-host data plane |
| [`astrs-transport`](crates/astrs-transport/) | UDS, TCP and QUIC connection abstraction with backpressure and reconnect |
| [`astrs-discovery`](crates/astrs-discovery/) | Static peer lists and UDP multicast beacons for daemon/coordinator rendezvous |
| [`astrs-store`](crates/astrs-store/) | Coordinator persistence for parameters, dataflow state and the build cache index |
| [`astrs-yaml`](crates/astrs-yaml/) | The workspace's own pure-Rust YAML 1.2 reader and writer for dataflow manifests |

### Domain libraries & orchestration

| Crate | What it is |
|---|---|
| [`astrs-manifest`](crates/astrs-manifest/) | Dataflow manifest parsing, validation, module expansion and JSON-schema emission |
| [`astrs-graph`](crates/astrs-graph/) | Dataflow graph model, edge type checking, placement planning and visualization |
| [`astrs-scheduler`](crates/astrs-scheduler/) | Input queues, hierarchical timer wheel, deadline monitor and priority lanes |
| [`astrs-recording`](crates/astrs-recording/) | The `.arec` recording container: writer, reader, merger and seekable index |
| [`astrs-verify`](crates/astrs-verify/) | SMT-backed graph proofs: deadlock freedom, queue bounds and rate consistency |
| [`astrs-migrate`](crates/astrs-migrate/) | Manifest migration from dora-rs descriptors and ROS 2 launch files |
| [`astrs-raft`](crates/astrs-raft/) | Raft consensus over `astrs-wire`: coordinator high availability for AstRS clusters |
| [`astrs-telemetry`](crates/astrs-telemetry/) | Tracing setup, an allocation-free metrics registry and a pure-Rust OTLP/HTTP exporter |
| [`astrs-log`](crates/astrs-log/) | Structured log records, rotation and level-filtered virtual-input fan-out |
| [`astrs-daemon`](crates/astrs-daemon/) | The per-machine daemon: node supervision, SHM broker, route bridging and health |
| [`astrs-coordinator`](crates/astrs-coordinator/) | The cluster coordinator: daemon registry, dataflow FSM, build orchestration and fan-out |
| [`astrs-runtime`](crates/astrs-runtime/) | The operator host: shared event loop, per-operator threads and panic isolation |
| [`astrs-node-api`](crates/astrs-node-api/) | The node API: handshake, typed pub/sub, zero-copy allocation and event streams |
| [`astrs-operator-api`](crates/astrs-operator-api/) | The operator trait and event/output shims for in-process dataflow stages |
| [`astrs-operator-macros`](crates/astrs-operator-macros/) | Procedural macros backing the operator API (`#[derive(AstrsMessage)]`, `#[operator]`) |

### ROS 2 pillar

| Crate | What it is |
|---|---|
| [`astrs-cdr`](crates/astrs-cdr/) | CDR and XCDR1 serialization (plus XCDR2 read) with ROS 2 alignment rules |
| [`astrs-rtps`](crates/astrs-rtps/) | A from-scratch RTPS 2.3 stack on tokio UDP: discovery, reliability, QoS and fragmentation |
| [`astrs-idl`](crates/astrs-idl/) | ROS 2 `.msg`/`.srv`/`.action` parsing with Rust code generation |
| [`astrs-ros2`](crates/astrs-ros2/) | An rcl-level ROS 2 client library in pure Rust: nodes, topics, services, actions, parameters |
| [`astrs-rosbag`](crates/astrs-rosbag/) | rosbag2 `.db3` and `.mcap` reading, `.db3` writing and `.arec` conversion |
| [`astrs-tf`](crates/astrs-tf/) | A tf2-compatible transform tree with time-travel lookup and SE(3) types |
| [`astrs-urdf`](crates/astrs-urdf/) | URDF robot models: links, joints and kinematic chains feeding `astrs-tf` |
| [`astrs-ros2-bridge-node`](bins/astrs-ros2-bridge-node/) | The declarative `ros2:` bridge node |

### Interfaces & tooling

| Crate | What it is |
|---|---|
| [`astrs`](crates/astrs/) | The facade: prelude, feature aggregation, doc landing page |
| [`astrs-cli`](bins/astrs-cli/) | The `astrs` command line: run, build, monitor, migrate and bridge robotic dataflows |
| [`astrs-tui`](crates/astrs-tui/) | The ratatui live monitor: graph view, node stats, HLC timeline and log tail |
| [`astrs-capi`](crates/astrs-capi/) | The C API: a stable `extern "C"` node surface for non-Rust callers |
| [`astrs-nodes-signal`](crates/astrs-nodes-signal/) | Ready-made signal-processing nodes and operators: FFT, windows, filters |
| [`astrs-nodes-vision`](crates/astrs-nodes-vision/) | Ready-made vision nodes and operators: image codecs, resize, colour conversion |
| [`astrs-sim`](crates/astrs-sim/) | A deterministic fixed-step simulation harness driving AstRS dataflows |
| [`astrs-record-node`](bins/astrs-record-node/) | The recorder node: writes dataflow traffic into `.arec` sessions |
| [`astrs-replay-node`](bins/astrs-replay-node/) | The replay node: re-injects recorded `.arec` outputs into a dataflow |

36 crates live under `crates/`, four binaries under `bins/`; `examples/`
(20 members), `tests/conformance`, `tests/fuzz`, `benches/astrs-benches` and
`xtask` round out the workspace and are `publish = false`.

## Pure Rust policy

AstRS contains **zero C/C++/Fortran anywhere in the dependency tree, for
every feature combination, on every target** — not merely in the default
build. This is enforced two ways, not just claimed:

- `deny.toml`'s `[bans].deny` list bans `cc`, `git2`/`libgit2-sys`,
  `openssl`/`openssl-sys`, `ring`, `aws-lc-rs`/`aws-lc-sys`,
  `opentelemetry*`/`tonic`, `arrow`/`arrow-ipc` and `flatbuffers` outright
  (`oxicrypto` replaces the crypto set, `oxihttp`/`astrs-telemetry` replace
  the OTLP client, the system `git` binary replaces `git2`, and
  `astrs-data` hand-encodes the Arrow IPC wire format rather than depending
  on `arrow`/`arrow-ipc`/`flatbuffers` to do it). `cc` carries a closed
  `wrappers` list — any *new* parent of a C compiler that isn't already
  named there fails `cargo deny check bans` outright, by design. The four
  `arrow-*` crates the optional `arrow-interop` feature pulls in
  (`arrow-array`, `arrow-buffer`, `arrow-data`, `arrow-schema`) are banned
  with the same `wrappers` containment shape, naming `astrs-data` and
  arrow-rs's own internal edges and nothing else — so the feature is
  reachable, and no *other* crate can become a parent of arrow-rs without
  failing the ban.
- Every intra-workspace crate build is audited with `cargo tree
  --all-features --target all -i cc` / `-i ring`, and `cargo tree -e normal
  --no-default-features` is checked for stray `*-sys` crates beyond
  `windows-sys`/`libc`.

Two things in the tree look like exceptions and are not. `astrs-capi`
builds as a `staticlib`/`cdylib` and ships a hand-written
`include/astrs.h`, but that is Rust *exporting* a C ABI for out-of-tree C
callers — no `cc`, no build script, no `-sys` dependency, and nothing in
this workspace's own build compiles or links that header. Likewise, the
`dylib-operators` and `wasm-operators` features carry FFI *declarations* to
a platform service the OS already provides (`dlopen`/`LoadLibrary` via
`libloading`) or a pure-Rust interpreter with no JIT (`wasmi`) — neither
compiles a line of C either.

The one genuine, deliberately scoped exception is `arrow-interop`: turning
it on really does add four arrow-rs crates to the graph. It costs nothing
against the rule above — arrow-rs is pure Rust — but it is the reason
`cargo tree --all-features | grep arrow` is not empty, and the reason those
four crates are ban-with-`wrappers` rather than simply absent. Every other
build, including the default one, resolves none of them.

Run `cargo deny check bans` yourself to see the current state of both
checks — this README states the policy and its enforcement mechanism, not a
point-in-time pass/fail snapshot that would go stale the moment a dependency
changes.

### Retained external crates

The default build isn't zero-dependency — it's zero-*C*. What's actually in
the graph and why, the closed list in `deny.toml` encodes structurally
without the rationale:

| Crate | Why retained |
|---|---|
| `tokio` | Async runtime |
| `serde`, `serde_json` | Trait ecosystem lingua franca; JSON at human boundaries |
| `clap` (+`clap_complete`) | CLI parsing and shell completions |
| `tracing`, `tracing-subscriber` | Logging façade |
| `ratatui`, `crossterm` | The `astrs top` TUI (pure Rust) |
| `thiserror` | Error derive — no `anyhow`/`eyre` in libraries |
| `rustix`, `windows-sys` (+`windows-core` family), `core-foundation-sys`, `libc` | OS syscall bindings, not C compilation |
| `uuid` (v7), `semver`, `regex`, `memchr`, `itertools`, `shlex`, `glob` | Micro-utilities below the reimplementation threshold |
| `proc-macro2`/`quote`/`syn`, `heck`, `prettyplease` | Codegen (`astrs-idl`, the derive macros) |
| `redb` | Embedded KV, arrives via the `oxistore` backend |
| `schemars` | JSON Schema emission for the manifest |
| `quick-xml` (`oxixml-quickxml-compat`) | `package.xml`/ament discovery (`astrs-idl`), ROS 2 launch-file parsing (`astrs-migrate`) — not `astrs-urdf`, which has its own zero-dependency XML parser (see [Feature status](#feature-status)) |
| `proptest`, `criterion` (dev-only) | Property and micro-benchmark testing |

`astrs-yaml` — the workspace's own YAML 1.2 reader and writer — retired
`serde_yaml` from every shipped build; it survives only as a
`[dev-dependencies]` cross-check in `astrs-yaml`'s own test suite (see
[Feature status](#feature-status)).

### COOLJAPAN ecosystem

AstRS also depends on sibling COOLJAPAN crates for capabilities core enough
to own rather than import from crates.io mainstream — see root `Cargo.toml`
for exact pinned versions, recorded once there rather than here, where it
would go stale:

| Crate | Used for |
|---|---|
| `oxicode` | Control-plane codec: framed envelopes, zero-copy `BorrowDecode`, `encode_presized` |
| `oxiarc-lz4`, `oxiarc-zstd` | Route compression; `.arec` framing |
| `oxistore-core` (+ redb backend) | Coordinator persistence, build cache |
| `oxisql-sqlite-compat` | rosbag2 `.db3` without `libsqlite3` |
| `oxiquic` | Cross-host transport (behind the non-default `quic` feature — see [Feature status](#feature-status)) |
| `oxihttp` | OTLP/HTTP exporter client; hub artifact fetch |
| `oxicrypto` | Token generation, constant-time compare, hashing |
| `oxiz` | The `verify` feature's graph proofs |
| `oxifft` | `astrs-nodes-signal` |

Reference-only sources — consulted for design, never depended on — and their
attribution are recorded in [`NOTICE.md`](NOTICE.md).

## Repository automation

`xtask` is where release automation lives — schema emission, wire-protocol
snapshot freezing, the layer-dependency lint, and the release preflight —
reached through the standard cargo-xtask pattern, `cargo xtask <verb>`
(`.cargo/config.toml` aliases `xtask` to `run --quiet --package xtask --`,
so the two invocations are equivalent; `cargo xtask --help` lists all
four). Each task is implemented and unit-tested as its own module:

| Verb | Module | Task |
|---|---|---|
| `cargo xtask schema [--check]` | `xtask::schema` | Emit the dataflow manifest's JSON schema, or (`--check`) verify the committed `astrs-schema.json` still matches it |
| `cargo xtask snapshot-protocol [--frozen-only]` | `xtask::snapshot_protocol` | Verify the wire-protocol freeze |
| `cargo xtask layer-lint` | `xtask::layer_lint` | Check that crate dependencies only ever point down the layer stack |
| `cargo xtask preflight [--publish-dry-run]` | `xtask::preflight` | Every quality gate — structural checks, `fmt`/`deny`, the protocol freeze, then clippy/nextest/doctests/`doc` — in one ordered pass, reporting pass/fail per step |

`scripts/ci-local.sh` is the per-change gate built on the last of these —
see [Build & test](#build--test) above. `preflight` has no `--execute`
mode, by design (its own module doc says so explicitly): publishing for
real is deliberately not something a repository script can trigger.

## CLI reference

One binary, `astrs` (`astrs-tui` compiled in for `astrs top`):

| Group | Verbs |
|---|---|
| Lifecycle | `run` (single-process) · `up`/`down` (cluster) · `build` · `start`/`stop`/`restart`/`destroy` |
| Monitoring | `list` · `logs [-f]` · `top` (TUI) · `topic echo/hz/info/pub` · `status` · `trace` |
| Graph ops | `validate [--prove] [--profile FILE]` · `expand` · `graph` (mermaid/DOT/HTML) · `node add/remove/replace` · `param get/set/list/delete` |
| Data | `record start/stop` · `replay <file> --into <manifest>` (offline) / `replay <file> <dataflow>` (live) · `bag convert/info` |
| ROS 2 | `ros2 doctor` (discovery probe) · `ros2 topics` (live DDS graph) |
| Cluster credentials | `token mint --scope read\|mutate` |
| Package index | `hub init/update/search/info` |
| Dev | `new` (templates: node/operator/operator-dylib/graph) · `migrate from-dora`/`from-ros2` · `doctor` · `completion` · `schema` |
| Internal (hidden) | `daemon` · `coordinator` · `runtime` |

`astrs new node --lang rust` scaffolds a buildable node crate wired into a
starter manifest. `astrs doctor` checks ports, SHM limits (`/dev/shm` size,
macOS SC limits), multicast availability, real-time scheduling ceilings and
toolchain. Every verb accepts `--json` for scripting.

## Security

- **Cluster token**: a 64-hex token at `<working_dir>/.astrs-token` (mode
  0600), generated by `astrs up`, required in every `Hello` handshake and
  compared constant-time (`oxicrypto`). QUIC legs derive a PSK from it; UDS
  legs additionally rely on filesystem permissions plus a kernel
  peer-credential check (`rustix`'s `SO_PEERCRED`).
- **Read vs. mutate scopes**: `astrs token mint --scope read|mutate` derives
  a read-only credential from the root token via HKDF — handing out an
  observer credential needs no coordinator restart or wire change, since
  `ControlRequest`'s read/mutate split was frozen from the start (see
  [Feature status](#feature-status)).
- **Env hygiene at spawn**: inherited environment is scrubbed to an
  allowlist; manifest `env:` entries are filtered against a denylist
  (`ASTRS_*` internals, `LD_PRELOAD`, `DYLD_*`); daemon-owned variables are
  applied last, so a manifest can never override the handshake.
- **No shell by default**: `build:`/`path:` execute directly (argv split by
  shlex rules, no shell interpolation); `shell:` node sources require the
  explicit `--allow-shell` flag.
- **DDS-Security**: partial by design, not by omission — see
  [Feature status](#feature-status) for exactly what `astrs-rtps`'s
  pre-shared-key submessage protection does and does not cover.

## Defaults & environment

| Item | Default | Override |
|---|---|---|
| Coordinator port | 7407 | `ASTRS_COORDINATOR_PORT` / manifest |
| Daemon node port (loopback) | 7408 | `ASTRS_DAEMON_PORT` |
| Node handshake blob | — | `ASTRS_NODE_CONFIG` (oxicode + base64, daemon-set) |
| Node auth token fallback | — | `ASTRS_AUTH_TOKEN` |
| Zero-copy threshold | 4096 B | `ASTRS_ZERO_COPY_THRESHOLD` |
| SHM pool per output | 8 MiB | manifest `shm_pool_size` / `ASTRS_SHM_POOL_SIZE` |
| Default queue size | 10 | manifest per-input |
| Heartbeat / metrics / health interval | 5 s / 2 s / 5 s | manifest `health_check_interval` |
| Max frame | 64 MiB | config |
| Type check mode | `warn` | `ASTRS_TYPE_CHECK=off\|warn\|error` |
| Runtime dir | `$XDG_RUNTIME_DIR/astrs` | `ASTRS_RUNTIME_DIR` |
| Hub index URL | built-in default | `ASTRS_HUB_INDEX` / config file |
| Hub cache dir | XDG cache dir | `ASTRS_HUB_CACHE_DIR` |

Every row above is a literal constant or `env::var` call somewhere in the
tree, not a plan — the hub row, for instance, is
`crates/astrs-coordinator/src/hub_index.rs`'s `ENV_HUB_INDEX`/
`ENV_HUB_CACHE_DIR` constants.

## Type URNs

Every typed port carries a URN, `std/<category>/v<n>/<Type>[params]`. The
curated `std` set:

`std/core/v1/{Bool,Int8..64,UInt8..64,Float16/32/64,String,Bytes,Empty}` ·
`std/media/v1/{Image[pixel=…],AudioFrame[sample=…],CompressedImage[format=…]}` ·
`std/vision/v1/{Detections,Keypoints,Mask}` ·
`std/geometry/v1/{Pose,Transform,Twist,Accel,Quaternion,Vector3}` ·
`std/sensor/v1/{LaserScan,PointCloud[fields=…],Imu,NavSatFix,Range}` ·
`std/nav/v1/{Odometry,Path,OccupancyGrid}` ·
`std/time/v1/{Timestamp,Duration}`.

Each type's normative columnar layout lives in `astrs-data`'s `types`
rustdoc, and the registry itself is `crates/astrs-data/src/urn/registry.rs`.
ROS 2 `common_interfaces` map onto this set bidirectionally in `astrs-ros2`;
`astrs-idl` additionally mints one mechanical `std/ros2/v1/<Package><Type>`
URN per generated ROS 2 wire type (around 90 of them) — a distinct, larger
layer that the curated table above is not a replacement for.

## Glossary

**Manifest** — the YAML dataflow description (dora calls it a descriptor).
**Route** — one producer-output → consumer-input delivery path with a chosen
plane. **Plane** — a transport substrate (SHM, UDS, TCP, QUIC). **Generation**
— the incarnation counter of a spawned node; a restart bumps it, and stale
messages are rejected by comparing stamps. **HLC** — hybrid logical clock,
`astrs-time`'s replacement for `uhlc`. **Zoo** — the fault-tolerance
conformance node set (`tests/conformance/`), ported from dora's own 16-node
misbehaving zoo and extended with AstRS-specific SHM and transport-partition
cases. **URN** — the port type identifier, `std/<category>/v<n>/<Type>[params]`
(see [Type URNs](#type-urns)).

## License

Apache-2.0. See [`NOTICE.md`](NOTICE.md) for third-party attribution and the
COOLJAPAN reference-material notices for the ROS 2 pillar.

---

*This repository was built from `astrs.md`, a 24-section engineering
blueprint retired before 0.1.0 shipped: its normative content had already
migrated into this README and into each crate's own rustdoc — see, for one
example among many, `crates/astrs-wire/src/codec.rs`'s frame-format
doctest. Every document that ships with this workspace has since been swept
of its "blueprint §N" citations; code comments still carry them, and those
section numbers no longer resolve anywhere, though the sentence each one
sits in stands on its own. `TODO.md`'s "Doc debt" entry tracks that
remaining sweep as unfinished cleanup, not a mystery.*
