# AstRS examples

Each example is a workspace member with its own `dataflow.yml` beside it.
Build the binaries, then run the manifest:

```bash
cargo build -p hello-timer
astrs run examples/hello-timer/dataflow.yml
```

`path:` entries are relative to the manifest's own directory, which is what
`astrs run` resolves them against, so nothing here carries an absolute path.

| Example | Shows |
|---|---|
| [`hello-timer`](hello-timer/) | The smallest graph: a `astrs/timer/*` virtual source and the node logging API |
| [`rust-pipeline`](rust-pipeline/) | The canonical typed graph: `std` URN ports, `#[derive(AstrsMessage)]`, queue policy, restart policy |
| [`service-roundtrip`](service-roundtrip/) | Request/response over ordinary edges with metadata correlation |
| [`shm-zero-copy-probe`](shm-zero-copy-probe/) | The M1 zero-copy verification: 4 MiB frames read out of the producer's ring, uncopied |
| [`record-replay`](record-replay/) | The byte-for-byte recording promise: record a live graph to `.arec`, replay it into a graph whose detector became an assertion sink |
| [`multi-daemon-cluster`](multi-daemon-cluster/) | Milestone M2: one dataflow across two daemons, the machine crossing asserted from inside the graph |
| [`restart-policies`](restart-policies/) | Supervision: a flaky node recovering inside its budget, and the same node exhausting it |
| [`error-propagation`](error-propagation/) | Error propagation: a failing node's typed cause reaching its peer as an ordinary event |
| [`benchmark-latency`](benchmark-latency/) | The local-plane RTT performance gate: a closed-loop ping/pong timed on one clock, with p50/p90/p99 and a deliberately loose sanity ceiling |
| [`action-progress`](action-progress/) | The action pattern: goal/feedback/terminal status over the `goal_id`/`goal_status` FSM, `GoalTracker`-verified |
| [`streaming-segments`](streaming-segments/) | The stream pattern: 256 KiB payloads chunked and reassembled over `session_id`/`segment_id`/`seq`/`fin` |
| [`log-aggregation`](log-aggregation/) | Two nodes' structured logs collected through the daemon's `astrs/logs` virtual source, no `node/output` edge involved |
| [`dynamic-add-remove`](dynamic-add-remove/) | `astrs node add`/`node remove` against a running dataflow, via a single-node manifest fragment |
| [`typed-vs-any`](typed-vs-any/) | A typed columnar port and a raw-bytes port carrying the same values side by side |
| [`module-composition`](module-composition/) | Two `Operator` implementations composed in one `astrs-runtime` process, proved with a real `RuntimeHost` under `Node::init_testing` |
| [`tui-showcase`](tui-showcase/) | `astrs-tui` rendering driven against a scripted `ClusterView`, verified with `TestBackend` goldens across every tab |
| [`ros2-talker-bridge`](ros2-talker-bridge/) | The declarative `ros2:` bridge: a self-hosted DDS talker (no external ROS 2 install) bridged into AstRS through a real `ros2:` block and `bins/astrs-ros2-bridge-node` |
| [`ros2-native-listener`](ros2-native-listener/) | The `Ros2Node` client library used directly, no bridge involved: two self-hosted RTPS participants trading `std_msgs/msg/String` over real DDS |
| [`rosbag-reader`](rosbag-reader/) | rosbag2 `.db3`: a fixture bag written and read back in code via `astrs-rosbag`, fed into an ordinary AstRS pipeline |
| [`tf-broadcast`](tf-broadcast/) | The tf2 contract: a latched static frame and a sliding dynamic one, resolved with an interpolated, time-travel `TransformBuffer` lookup |

`tests/conformance` runs the first eight of these twice over — once through
the run verb in process, once through the `astrs` binary as a child process
— and asserts on what they produce. It also guards that part of the estate
against drift: the manifests, the Cargo packages, the workspace `members`
list and the table above are checked against each other on every
`cargo test -p astrs-conformance`. See its README for how it finds the
binaries.

```bash
cargo build -p astrs-cli -p astrs-replay-node \
            -p hello-timer -p rust-pipeline \
            -p service-roundtrip -p shm-zero-copy-probe \
            -p record-replay -p multi-daemon-cluster \
            -p restart-policies -p error-propagation
cargo test  -p astrs-conformance
```

The twelve newer examples above (`benchmark-latency` through `tf-broadcast`)
are covered by `astrs_conformance::EXAMPLES` and its drift guards too, exactly
like the first eight — but not by the process-spawning run itself: that stays
scoped to the first eight, above. Each of the twelve proves its own central
claim independently: `cargo build -p <example>` then `cargo test -p
<example>`, exactly as the table's own links describe. Every one of them also
parses and validates its own committed `dataflow.yml` against
`astrs-manifest` as part of that test, the same check `tests/conformance`
would otherwise be the only place making.

`ros2-talker-bridge` carries a second committed manifest, `bridge.yml`,
beside `dataflow.yml` — see its own README for why a `ros2:`-sourced node
cannot live in the headline file every other example's `astrs run
examples/<name>/dataflow.yml` line points at, and for how the estate guard
still covers it, the same way it already covers `record-replay/replay.yml`
and `restart-policies/budget-exhausted.yml`.
