# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-09-06

Initial build-out: the full P0 crate catalog, plus a
handful of post-0.1.0 features pulled forward once their prerequisites landed
early (Raft-backed coordinator HA, dylib/WASM operator loading, the hub
package index, simulation, and the signal/vision built-in node libraries
among them) — organized below by pillar rather than by the day-by-day wave
history (`git log` is a sequence of checkpoint commits, not feature commits).

### Added

#### Substrate

- `astrs-wire`: the single control-plane protocol surface — the framed codec
  (`magic | ver | flags | kind | len | payload | crc32c`), one message
  family per leg (`ControlRequest`/`ControlReply`,
  `CoordinatorEvent`/`DaemonEvent`, `NodeRequest`/`NodeEvent`, `PeerEvent`),
  `Hello`/`Welcome`/`Refused` handshake and version negotiation, and the
  append-only compatibility contract frozen by protocol snapshot tests —
  including the deadline pair appended at that contract's tail
  (`NodeRequest::ReportDeadlineViolation`, `NodeEvent::DeadlineViolated`):
  a node's own `DeadlineMonitor` measurement, which only the node owning the
  input can make, relayed by its daemon onto `astrs/status` exactly like
  `NodeFailed`/`Restarted`, with the frozen prefix untouched.
- `astrs-time`: a first-party hybrid logical clock (the `uhlc` replacement),
  `Stamped<T>`, monotonic/wall clock abstractions with a fully controllable
  test clock, checked-arithmetic deadlines, human duration parsing, and the
  drift-free `TimerInterval` behind every `astrs/timer/*` virtual source.
- `astrs-data`: the columnar payload format — 64-byte aligned buffers, the
  closed P0 array set, `RecordBatch`, the `std` type URN registry, schema
  hashing, tensor/image views, compute kernels (slice/concat/cast/take/
  filter) — encoded as an Arrow IPC stream byte-compatible with arrow-rs/
  pyarrow/arrow-cpp, implemented from scratch with no `arrow`/`flatbuffers`
  dependency and gated on golden vectors under `tests/golden/arrow/`.
- `arrow-interop` (off by default; an `astrs-data`/`astrs-node-api` feature
  the `astrs` facade surfaces and `full` turns on): zero-copy
  `From`/`TryFrom` between AstRS's own columnar arrays and arrow-rs, plus
  `RawOutput::send_arrow` for handing one straight to an arrow-native
  pipeline. The one feature in the workspace that pulls the four `arrow-*`
  crates into a build, and `deny.toml` confines that edge to `astrs-data`
  alone so no other crate becomes a parent of arrow-rs.
- `astrs-shm`: the zero-copy same-host data plane — one SPMC ring per
  `(producer, output, generation)`, seqlock slots, generation stamps,
  drop-token reclamation that survives any participant being `kill -9`'d,
  a pool allocator, and pidfd/kqueue liveness watches.
- `astrs-shm`'s Windows half: named file mappings (`CreateFileMappingW`/
  `OpenFileMappingW`/`MapViewOfFile`) in the `Local\` per-session kernel
  namespace, mirroring the POSIX hashed segment name, over `windows-sys`
  ABI declarations only — no C compiled, and never in a Unix build's
  dependency tree. Deliberately partial, and documented as such: segment
  create/open/map is all of it, while `Producer`, `Consumer`, `Doorbell`
  and `SegmentBroker` stay `#[cfg(unix)]`-only, because a Windows doorbell
  and liveness watch need the `Win32_System_Threading` surface this
  workspace does not enable. A segment built there is usable from one
  process today and is the foundation the rest lands on.
- `astrs-transport`: one connection abstraction over UDS, TCP and QUIC
  (`ASTRS-MUX/1` route multiplexing, the lz4/zstd compression container,
  reconnect with backoff and in-flight buffering); QUIC ships behind a
  non-default feature pending an upstream `oxiquic` gap; TCP is the default
  cross-host plane with identical framing.
- `astrs-discovery`: static peer lists plus a UDP multicast beacon for
  daemon/coordinator rendezvous, degrading to unicast fallback rather than
  failing when multicast is unavailable (sandboxes, containers, CI).
- `astrs-store`: coordinator persistence over `oxistore-core`/
  `oxistore-kv-redb` — parameters, dataflow lifecycle state, the daemon
  registry, the build cache index, and a sequence-numbered mutation log
  backing `StateCatchUp` for a reconnecting daemon.
- `astrs-yaml`: the workspace's own YAML 1.2 core-schema reader and writer,
  and the end of the last unmaintained third-party dependency in the graph
  — `serde_yaml` survives only as this crate's own dev-dependency
  differential oracle. One-based `Position`/`Span` on every syntax error, so
  a manifest diagnostic points at the line and column the editor's status
  bar shows rather than surfacing inside a `serde` visitor with no location
  at all; per-parse `Limits` on input size, nesting depth and alias
  expansion, with `Error::is_limit` separating "over a ceiling" from
  "malformed"; duplicate keys and tabs-as-indentation rejected with the
  offending position; anchors, aliases, local and verbatim tags, `%TAG`
  handles and multi-document streams; and a deterministic block-style
  emitter that round-trips every `Value` and reproduces `serde_yaml`'s exact
  layout for manifest-shaped documents, so the fixtures committed to this
  repository stay byte-identical across the switchover. Not implemented, and
  said so in the crate's own docs rather than discovered: merge keys
  (`<<:`), the YAML 1.1 types (`!!binary`, `!!timestamp`, sexagesimals,
  `yes`/`no` booleans), and recursive aliases.

#### Runtime & orchestration

- `astrs-manifest`: strict (`deny_unknown_fields`) YAML manifest parsing and
  cross-referential validation reporting every violation at once, `env:`
  expansion, recursive `module:` expansion, and JSON-schema emission
  (`astrs schema`) for editor completion.
- `astrs-graph`: the validated graph model — nodes, ports, edges, virtual
  sources, modules — edge type checking against declared URNs, SCC/cycle
  and service-action pattern detection, a placement planner, mermaid/DOT
  visualization (`astrs graph`), and diffing for dynamic topology changes.
- `astrs-scheduler`: `InputQueue` (bounded, policy-driven, eviction-immune
  for Stop-class and correlated messages), `EventMux` (prioritized,
  starvation-free multiplexing), a hierarchical `TimerWheel`, a
  `DeadlineMonitor` and an `IdleWatchdog` — the shared mechanism both
  `astrs-daemon` and `astrs-node-api` multiplex their event loops through.
- `astrs-recording`: the `.arec` container (magic `ASTRSREC`, v1) —
  zstd-framed entries, a seekable index footer, a streaming `Writer`, a
  `Reader` that recovers a truncated file by scanning, and a k-way `merge`
  across recordings by HLC.
- `astrs-verify` (feature `verify`): graph obligations discharged through
  the OxiZ SMT solver — deadlock freedom (siphon search over a Petri net,
  covering service/action correlation waits), queue boundedness, rate
  consistency, latency budgets and type-rule consistency — with
  counterexample rendering for `astrs validate --prove`. Builds without the
  `verify` feature too, reporting `SolverUnavailable` instead of a verdict
  rather than forcing every dependent crate to gate on it.
- `astrs-migrate`: `from-dora` (mechanical dora descriptor mapping — nodes,
  timer virtual inputs, restart/queue policies, service/action patterns —
  modeled against a real `dora-schema.json`) and `from-ros2` (ROS 2
  launch-file skimming that scaffolds a bridge manifest), each emitting a
  migration note for anything that needs a human rather than a silent drop.
- `astrs-raft`: Raft on the same `astrs-wire` framing and `oxicode`
  encoding every other AstRS control message uses, rather than a second
  transport with its own bugs, metrics and version skew. `RaftNode` is a
  pure state machine — ticks and inbound messages in, outbound messages a
  caller drains out, no I/O of its own, every scheduling decision a function
  of its inputs and a seeded `Rng` — with Figure 2 plus pre-vote, snapshot
  install and single-server membership changes; `MemoryLog` and a `WalLog`
  with a configurable `FsyncPolicy` and crash recovery; an in-process
  `ChannelTransport` and a `TcpTransport`; a `tokio` driver; and `sim`, a
  virtual clock and adversarial network running the same consensus code a
  production replica runs, so a failing seed replays exactly.
- `astrs-telemetry`: shared `tracing` subscriber setup, an allocation-free
  metrics registry with bounded-cardinality label sets, W3C `traceparent`
  propagation over `astrs-wire::Metadata`, and (feature `telemetry-export`,
  on by default) a hand-rolled OTLP/HTTP+JSON exporter over `oxihttp` —
  no `opentelemetry`/`tonic` dependency tree.
- `astrs-log`: structured `LogRecord`s, a rotating/retention-limited file
  writer, an HLC-ordered k-way log merger backing cross-machine `astrs logs
  -f`, and the `astrs/logs[/level[/node]]` virtual-input filter grammar.
- `astrs-daemon`: the per-machine control-plane actor — spawn (env scrub,
  generation stamps), supervision and restart policies with exponential
  backoff, the SHM broker, cross-host route bridging, the extensions table,
  the coordinator uplink (register/heartbeat/execute/reconnect), and the
  `--deterministic` recorded-clock timer wheel (`ReplaySource`), pre-split
  by design into `spawn/`, `supervise/`, `local/`, `session/`, `server/`,
  `dataflow/`, `peer/`, `shm/` and `coordinator/`.
- Hard real-time reservations: the manifest `rt: { policy:
  normal|fifo|rr, priority: 1..=99 }` block, validated in the same
  report-everything-at-once pass as the rest (a `priority` is required for
  `fifo`/`rr` and rejected alongside `normal`, which has no priority axis to
  honor — an ignored key would be config silently lying about what it does),
  applied by `astrs-daemon` through `sched_setscheduler` in a `pre_exec`
  hook between `fork` and `exec` on Linux x86_64/aarch64. An `EPERM` is a
  spawn failure whose message names `CAP_SYS_NICE` and `RLIMIT_RTPRIO`,
  never a silent downgrade to `SCHED_OTHER`; every other platform reports
  `RtOutcome::UnsupportedPlatform` with a warn log and a counter. `astrs
  doctor` probes the same ceiling the daemon would hit, so an `rt:`
  manifest's failure mode is visible before `astrs run` ever attempts it.
  Bounded on purpose: `NodeSpawnSpec` cannot carry `rt` without moving bytes
  the frozen protocol prefix pins forever, so reservations are registered
  from the manifest by `astrs run`'s single-process daemon — a
  coordinator-admitted remote daemon has no carrier for them yet.
- `astrs-coordinator`: the daemon registry, the dataflow finite state
  machine (build → start → stop → destroy) aggregating per-node exit
  causes, build orchestration and artifact serving, log/topic fan-out to
  CLI subscribers, and the parameter store API.
- `astrs-coordinator`'s `ha` feature (off by default): the durable registry
  replicated across an odd-sized coordinator set through `astrs-raft`, so
  losing a coordinator costs one election timeout instead of the fleet.
  Mutating verbs are accepted only by the leader and only once a majority
  holds the entry (parameters proposed before anything is written locally);
  reads are served locally under a leader lease, gated on the leader having
  committed an entry of its own term; a follower refuses a mutation with the
  ordinary structured `Unavailable` error carrying `leader: <address>` in
  its context, so no wire enum gained a variant for HA at all. A
  reconnecting daemon gets a full snapshot rather than a mutation-log delta
  under replication, because the Raft log index — not the store's local
  sequence counter — is then the ordering authority, and a delta from that
  counter would leave the daemon believing it had caught up. Reached through
  `astrs coordinator --ha-node-id <n> --ha-peer <id=host:port>` (the same
  peer list on every machine, only the id differing); a build without the
  feature parses those flags and refuses them rather than ignoring them.
- `astrs-runtime`: the in-process operator host — one shared demux loop,
  one thread per hosted operator with panic isolation, and a reload hook,
  for operator chains that would rather pay a function call than an IPC hop.
- `astrs-node-api`: `Node` init/handshake (env or explicit builder, plus an
  in-process `MockDaemon` testing harness speaking the real wire protocol),
  typed and raw pub/sub, the `EventStream` (`Iterator` and `Stream`),
  service/action/stream pattern helpers, and the slow-start zero-copy
  upgrade wired transparently at both the producer and consumer ends.
- `astrs-operator-api` + `astrs-operator-macros`: the `Operator` trait
  (default no-op lifecycle hooks), `OperatorRegistry`/`register_operator!`,
  and the `#[derive(AstrsMessage)]`/`#[operator]` macros — Rust trait
  objects for the compiled-in registry path the operator API leads with, plus
  (feature `dylib`, off by default) a deliberately narrow `#[repr(C)]` ABI,
  three functions wide, that a shared library exports an `Operator` through;
  the C API is `astrs-capi`, below.
- `astrs-runtime`'s two ways to host an operator that was not compiled in,
  both off by default so a runtime serving only `register_operator!` types
  pays for neither: `dylib-operators` opens an `operators[].dylib:` path
  through
  `libloading` — the platform's own `dlopen`/`LoadLibrary`, FFI declarations
  to a platform service with no C compiled and no build script — and checks
  the `ASTRS_OPERATOR_ABI_VERSION` and operator name the library advertises;
  `wasm-operators` compiles and instantiates an `operators[].wasm:` module
  on `wasmi`, a pure-Rust interpreter with no JIT and no cranelift behind
  it. The dylib ABI is one tagged `on_event` doing duty for all five
  `Operator` hooks, carrying an oxicode-encoded private mirror rather than
  `OpEvent` itself (whose compatibility promise is only this crate's own)
  and answering through a host-supplied write callback so no pointer crosses
  an allocator boundary; the wasm guest ABI is three exports
  (`astrs-op-alloc`, `astrs-op-init`, `astrs-op-event`) and one import
  (`astrs.output-send`). `catch_unwind` on both sides of both boundaries, an
  unwind past an `extern "C"` frame being undefined behavior — a panicking
  operator becomes a failed operator. `astrs new operator-dylib` scaffolds a
  crate against that ABI, and either kind of loaded operator reaches the
  runtime's ordinary worker loop, which never learns it was not compiled in.
- `astrs-capi`: the C API — one crate built as `lib`, `staticlib` and
  `cdylib`, an opaque-handle `extern "C"` surface in which no Rust type ever
  appears in a signature (`astrs_init_node_from_env`/`_from_config`,
  `astrs_node_next_event` and the `astrs_event_*` accessors,
  `astrs_send_output`, `astrs_node_destroy`), an `AstrsStatus` whose `Ok` is
  `0` and whose every failure is negative so the idiomatic C-side test is
  `< 0`, a thread-local last-error slot read back through
  `astrs_last_error_message`, `catch_unwind` at every entry point so a panic
  becomes `AstrsStatus::Panic` rather than an abort, a documented
  one-thread-at-a-time handle contract, and a hand-written
  `include/astrs.h`. Rust exporting a C ABI, never Rust compiling C: no
  `cc`, no build script, no `-sys` dependency.
- `astrs` facade: the single dependency an application adds — a
  fine-grained feature matrix (`node` default; `data`/`time`/`log`/`wire`
  individually; `operator`/`runtime`/`graph`/`manifest`/`recording`/
  `telemetry`/`tui`/`verify` opt-in) so `cargo add astrs` costs a node
  exactly what it uses, plus the crate-level documentation landing page
  with a complete first-node walkthrough.

#### ROS 2 interop stack

- `astrs-cdr`: XCDR1 encode/decode (big- and little-endian) and XCDR2 read,
  with ROS 2's ordinary and `PL_CDR` alignment rules, encapsulation header
  handling, and the `ParameterList` representation RTPS discovery builds on.
- `astrs-rtps`: RTPS 2.3 on tokio — the message model (headers, every
  submessage: `DATA`/`DATA_FRAG`/`HEARTBEAT`/`ACKNACK`/`GAP`/`NACK_FRAG`/
  the `INFO_*` family) as pure functions of octets, and the behavior half
  built on it without editing it — SPDP/SEDP discovery, stateless
  best-effort and stateful reliable writers/readers, fragmentation and
  reassembly, WLP liveliness, QoS (reliability/durability/history/
  liveliness/deadline), and the Humble 24-byte / Jazzy 16-byte GID switch.
  UDPv4 transport, unicast and multicast, with a loopback self-interop test
  path standing in for cross-stack DDS validation (kept out-of-repo per the
  Pure Rust policy).
- `DURABILITY`'s reader half, which DDS puts on both endpoints rather than
  one: `ReaderProxy::wanting_history` records whether a matched reader
  actually asked for what the writer wrote before it appeared, so a
  `TRANSIENT_LOCAL` writer keeps its history for the next reader but still
  starts a `VOLATILE` one at the present. Pinned by `tests/durability.rs`
  across all four writer/reader combinations, the bound `HistoryQos` puts on
  a replay, and the case easiest to get wrong — a `KEEP_ALL` +
  `TRANSIENT_LOCAL` writer must not release history merely because every
  reader alive today has acknowledged it — synchronously against the writer
  and again end to end over real UDP through SEDP, since the writer can only
  honor a remote reader's `DURABILITY` if discovery carried it.
- `astrs-rtps`'s `security` module: DDS-Security 1.1 submessage protection
  — `SEC_PREFIX`/`SEC_BODY`/`SEC_POSTFIX`, AES128/AES256 in GCM and GMAC,
  per-session keys derived with HKDF-SHA-256, a sliding anti-replay window
  — with every primitive taken from `oxicrypto` and none implemented here
  (one file calls a cipher, and it is two functions long so that staying
  true stays checkable). Scoped honestly in its own docs: one half of one of
  the specification's five plugins, keyed by a pre-shared key an operator
  configures on both endpoints, with no authentication handshake, no access
  control, and no whole-message or serialized-payload protection — and
  therefore no interoperability with a PKI-based DDS-Security stack, not
  "not yet". What it does buy on a robot today is a `DATA` on `/cmd_vel`
  that an attacker on the same network cannot forge, alter, replay, or
  (under `encrypt`) read. An endpoint that says nothing about security stays
  byte-identical to a build without the module — asserted in
  `tests/security.rs`, not assumed.
- `astrs-idl`: a hand-rolled recursive-descent `.msg`/`.srv`/`.action`
  parser (no parser-combinator dependency), cross-file type resolution,
  `package.xml`/ament-tree discovery, Rust codegen producing types that
  implement both `astrs_cdr::CdrSerde` and `astrs_data::AstrsMessage`, and
  the `common_interfaces` set (`std_msgs`, `geometry_msgs`, `sensor_msgs`,
  `nav_msgs`, …) pre-generated in-tree with an always-on drift test
  (`generated_matches_source`) checking it against the parser/codegen.
- `astrs-ros2`: an rcl-level client library — node/context lifecycle over
  `astrs-rtps`, publishers/subscriptions with QoS mapping, services and
  actions, parameters, ROS graph introspection, name mangling
  (`rt/`/`rq/`/`rr/`), and the `astrs ros2 doctor`/`astrs ros2 topics`
  backends.
- `astrs-rosbag`: rosbag2 `.db3` reading and writing via
  `oxisql-sqlite-compat` (no C SQLite), `.mcap` reading (chunked and
  indexed), and conversion either direction against `.arec`.
- `astrs-tf`: the tf2 contract — SE(3) types, a static/dynamic frame tree
  with parent-conflict detection, interpolated time-travel lookup, and
  `/tf`/`/tf_static` bridging both directions.
- `astrs-urdf`: the robot description `astrs-tf`'s own docs point at as a
  separate crate that would consume a `TransformBuffer` rather than live
  inside one — URDF parsed on a zero-dependency XML pull parser written
  here, the link/joint model (geometry, inertials, materials, limits,
  `<mimic>` relationships) with the six joint kinds carrying their own
  consequences as methods, cross-reference and tree-shape validation, SE(3)
  math (`Vec3`/`Quat`/`Transform`) of its own, kinematic chains and forward
  kinematics that resolve mimic joints, and `populate_static_transforms`
  publishing the fixed-frame skeleton into an `astrs_tf::TransformBuffer` so
  a robot description and a live transform tree stay one source of truth
  rather than two. Carrying a robot's live joint state as `astrs-data`
  columnar messages is the layer above and is not here: the crate's
  `astrs-data` edge is reserved for it and unused today, which its own docs
  state outright rather than leaving to be discovered.
- `bins/astrs-ros2-bridge-node`: the declarative `ros2:` manifest block,
  realized — joins the DDS domain, converts CDR ⇄ columnar through
  generated `astrs-idl` types, preserves ROS header timestamps into HLC
  metadata, and turns a bad manifest and a lost daemon into distinguishable
  exit codes for a supervisor.

#### Built-in nodes

- `astrs-nodes-signal`: the common DSP stages, each shipped twice over — as
  a plain callable Rust function or type tested against closed-form cases,
  and as an `astrs_operator_api::Operator` configured from a manifest
  `operators: config:` map and reading and writing columnar payloads.
  Complex and real FFT/IFFT on `oxifft` (the pure-Rust replacement for both
  FFTW and the outright-banned `rustfft`, so a spectrum stage adds no C and
  no `-sys` crate), periodic windows for analysis and symmetric ones for FIR
  design, RBJ biquads, windowed-sinc FIR design with streaming convolution,
  polyphase decimation and interpolation, windowed STFT spectrograms, and
  moving/exponential averages. Every stateful stage computes in `f64` and
  narrows only where a value crosses the wire boundary — a stated accuracy
  choice, not an unfinished `f32` path; the transforms are the exception,
  staying generic over `oxifft::Float` since they carry no state across
  calls for rounding to compound in.
- `astrs-nodes-vision`: the same two-surface shape for camera frames — an
  owned `ImageBuffer` over `astrs-data`'s columnar image payloads,
  `PixelFormat` conversion across mono/RGB/BGR/RGBA and the packed 4:2:2
  pair, nearest-neighbour and bilinear resize, separable Gaussian blur,
  Sobel gradients, fixed and Otsu thresholding, morphology,
  connected-component labeling with per-component stats, line/rect/circle
  drawing, and a pinhole camera model with plumb-bob distortion and
  undistort maps. Which formats an op accepts follows from which of two
  families it is in, and a refusal names both the operation and the format.
  The PNG codec is written from scratch against the specification over
  `oxiarc-deflate`, so none of `libpng`, `libjpeg-turbo` or OpenCV enters a
  robotics build; JPEG is out of scope outright —
  `std/media/v1/CompressedImage` carries a JPEG frame through the graph
  unopened and this crate never decodes one.

#### Simulation

- `astrs-sim`: a deterministic fixed-step harness driving a real dataflow
  against a simulated robot — the same graph, ordinary `astrs-node-api`
  nodes, the same columnar payloads on sensor and command topics.
  `Timestep` is an exact non-zero nanosecond count and simulated time is
  always `step × count` rather than an accumulated `f64`, because a
  drifting clock makes a run useless as a regression test. On top of it: an
  occupancy `Grid`, a DDA/Amanatides–Woo lidar raycast against it,
  closed-form unicycle differential-drive integration, an `OdometryModel`
  whose optionally-noisy dead reckoning is kept deliberately apart from
  `World`'s ground truth, `astrs-urdf` forward kinematics for articulated
  arms, a frozen-forever in-crate `SplitMix64` (a third-party generator is
  free to change algorithms on a semver-compatible release, which a
  bit-identical-trajectory promise cannot tolerate), and a `TrajectoryHasher`
  fingerprint behind `World::trajectory_hash`'s determinism check. Three
  binaries
  (`astrs-sim-node`, `astrs-sim-teleop`, `astrs-sim-logger`) and a
  `dataflow.yml` at the crate root wire it into a runnable graph.

#### Tooling

- `bins/astrs-cli`: the full CLI verb set on one `astrs` binary —
  lifecycle (`run [--deterministic --from-recording]`/`up`/`down`/`build`/
  `start`/`stop`/`restart`/`destroy`/`clean`), monitoring (`list`/`logs`/
  `top`/`topic echo,hz,info,pub`/`status`/`trace`), graph ops
  (`validate [--prove]`/`expand`/`graph`/
  `node add,remove,replace`/`param get,set,list,delete`), data
  (`record`/`replay`/`bag convert,info`), ROS 2 (`ros2 doctor`/`ros2
  topics`), and dev tooling (`new node,operator,graph`/`migrate
  from-dora,from-ros2`/`doctor`/`completion`/`schema`) — every verb parses
  with its real argument schema and reaches a real implementation.
- `astrs hub` and the `hub:` manifest source: a node or operator fetched
  from a package index by name, spelled either `hub: yolo-detector@v0.3.1`
  or as a structured `name`/`rev` map (the short form is what a human types
  and what a round-trip re-emits), resolved into a git clone and checkout at
  dataflow-start time by the coordinator rather than by the CLI — a
  standalone `astrs coordinator` resolves manifests submitted to it over the
  wire and never runs a CLI verb at all. The index itself is a minimal,
  self-hostable git repository of `packages/<name>.json` files with
  append-only version lists, each version naming the exact commit or tag it
  resolves to, because a published version is a reproducibility promise a
  floating ref cannot keep. `astrs hub init`/`update`/`search`/`info`
  scaffold, clone-or-fast-forward, and query the local cache, driving the
  system `git` binary rather than `git2`/libgit2; the index URL resolves
  from `--index`, then `ASTRS_HUB_INDEX`, then a `[hub] index` config key,
  then a built-in default.
- `astrs token mint --scope read|mutate`: a read-only cluster credential
  derived from the root token with HKDF instead of stored as a second
  secret, so handing out an observer credential needs no coordinator
  restart, no redistribution step and no wire change whatsoever —
  `ControlRequest`'s read/mutate scope split was already frozen, and the
  scope a connection was admitted under is decided once at the handshake and
  carried for that connection's life rather than re-derived per request.
  Purely local: minting never dials a coordinator, and a coordinator
  configured from the same root token reaches the identical 32 bytes.
  Recovering the root token from a read-scope one means inverting an HKDF
  output, so a read holder gains nothing towards forging a mutate one.
- `astrs-tui`: the ratatui live monitor — dataflow list and per-node
  status/restarts/CPU/RSS/queue-depth, a layered graph view with plane
  badges, a level/node-filterable log tail, and an HLC-ordered timeline —
  every tab a pure function of a `ClusterSnapshot`, sourced from either a
  polled live coordinator or an `.arec` replay, and tested with
  `ratatui::backend::TestBackend` goldens with no terminal involved.
- `bins/astrs-record-node` / `bins/astrs-replay-node`: the recorder and
  replayer realized as ordinary manifest nodes, reached through `record:`
  manifest sugar, `astrs record start`, or a target manifest rewritten
  in-place by `astrs replay --into`.
- 20 example dataflows under `examples/` (each a workspace member with its
  own committed manifest), from the one-node `hello-timer` through
  `multi-daemon-cluster`, `record-replay`, dynamic topology, and live ROS 2
  interop (`ros2-talker-bridge`, `ros2-native-listener`) with no ROS
  installed — see `examples/README.md`.
- `xtask`: schema emission, wire-protocol snapshot freezing, the
  layer-dependency lint and the release preflight — every quality gate
  in one ordered pass, cheapest and most structural first — each
  implemented and unit-tested as its own module, reached through
  `cargo xtask <verb>` (the standard cargo-xtask pattern, aliased in
  `.cargo/config.toml`).
- Two further gates wired into that preflight's ordered pass:
  `no-inline-version-pins`, which walks every declared member's dependency
  tables and refuses any `version =`/bare-string pin that should be
  `workspace = true`, naming the manifest path so a violation needs no
  second lookup; and the `*-sys` sweep, which resolves the whole workspace
  graph with `cargo tree --target all` once per feature mode
  (`--no-default-features` and `--all-features`, as its own step each) and
  flags every FFI-shaped crate outside the hand-verified allowlist. Both are
  graph-only, so they sit with the cheap structural checks ahead of clippy
  and nextest, and a `cargo tree` that spawns but cannot resolve fails that
  one step rather than aborting the whole report. The layer-dependency
  lint's `PRODUCTION_LAYERS` table gained every crate above, so each one's
  place in the layering is asserted rather than assumed.
- `scripts/ci-local.sh`: the per-change gate repo policy calls for (repo
  policy allows no CI workflow beyond the publish ones) — a thin wrapper
  around `cargo xtask preflight`, optionally with `--publish-dry-run`.

#### Quality & policy

- `tests/conformance`: an in-process cluster harness (coordinator + 2
  daemons + N nodes, no Docker) exercising the example estate twice over —
  once through the run verb in-process, once through the `astrs` binary as
  a child process — plus drift guards checking the manifests, Cargo
  packages, workspace `members` list and `examples/README.md` table against
  each other.
- `tests/fuzz`: a structured, pure-Rust fuzz estate (`astrs-fuzz`) over the
  four attack-surface decoders — `astrs-wire`'s frame codec, `astrs-data`'s
  Arrow IPC reader, `astrs-cdr`'s reader, `astrs-rtps`'s submessage parser —
  an in-crate PRNG with structure-aware mutation and a committed regression
  corpus, replayed unconditionally by the default test suite and run at
  higher iteration counts by `scripts/fuzz-nightly.sh`. No `cargo-fuzz`/
  libfuzzer: that links a C++ runtime, excluded outright by the Pure Rust
  policy.
- `benches/astrs-benches` plus per-crate criterion benches (`astrs-shm`,
  `astrs-transport`, `astrs-scheduler`, `astrs-rtps`, `astrs-wire`,
  `astrs-data`): the latency ladder, each bench printing a
  `BENCH_GATE` line rather than panicking on a miss, turned into one
  pass/fail verdict across all six targets by `scripts/bench-gate.sh`.
- `deny.toml`: the COOLJAPAN dependency-replacement policy plus the AstRS
  project ban list (Zenoh, `rustdds`/`ros2-client`, `mio` 0.x, `git2`/
  OpenSSL, `ring`/`aws-lc`, `flatbuffers`/`arrow` outside `astrs-data`,
  `opentelemetry`/`tonic`, `uhlc`) — including the absolute Pure Rust rule
  `cc` is banned with a closed `wrappers`
  containment list, and the workspace forces `blake3`'s `pure` feature so
  Cargo's feature unification can't silently pull a C SIMD build back in.

[Unreleased]: https://github.com/cool-japan/astrs/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/cool-japan/astrs/releases/tag/v0.1.0
