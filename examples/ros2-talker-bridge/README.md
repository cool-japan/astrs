# `ros2-talker-bridge`

Bridging a self-hosted DDS talker into an AstRS sink through the
declarative `ros2:` block, using the real
`bins/astrs-ros2-bridge-node`:

```bash
cargo build -p ros2-talker-bridge
astrs run examples/ros2-talker-bridge/dataflow.yml
```

```
[ros2-talker] ──/scan (real DDS/RTPS)──► [lidar-in: ros2:] ──scan──► [ros2-scan-sink]
\_____________ dataflow.yml ____________/ \_______________ bridge.yml _______________/
```

## Two manifests, on purpose

`dataflow.yml` is **just the talker**: a self-hosted loopback RTPS
participant — no external ROS 2 install needed — publishing
`sensor_msgs/msg/LaserScan` on `/scan`. That is what lets this file set
`exit_when_nodes_finish: true` and genuinely finish on its own, the same as
every other example's headline command.

The declarative bridge itself lives in this directory's `bridge.yml`:

```yaml
nodes:
  - id: lidar-in
    ros2:
      compat: humble
      topic: /scan
      message_type: sensor_msgs/msg/LaserScan
      direction: to_astrs
      qos: { reliable: true, keep_last: 10 }
    outputs: [scan]
```

Two structural facts push it out of `dataflow.yml`:

- **A `ros2:`-sourced node has no `path:`/`build:` to give** (the
  manifest lists `ros2:` as one of the mutually exclusive node *sources*, on
  par with `path:`/`git:`/`operators:`). `astrs-daemon` resolves
  `lidar-in` straight to `bins/astrs-ros2-bridge-node`'s own binary,
  regardless of anything a manifest could write there.
- **A bridge has no natural finish condition.** It runs until stopped —
  the same as it would in front of a real robot's ROS 2 stack — so a
  manifest built around `exit_when_nodes_finish` would simply hang
  forever waiting for it.

Run the full pair by hand, in two terminals:

```bash
cargo build -p ros2-talker-bridge -p astrs-ros2-bridge-node
astrs run examples/ros2-talker-bridge/dataflow.yml &   # the talker
astrs run examples/ros2-talker-bridge/bridge.yml        # runs until Ctrl-C
cat "${TMPDIR:-/tmp}/astrs-ros2-talker-bridge-report.json"   # ros2-scan-sink's running tally
```

Both sides join the same, deliberately non-default DDS domain
(`ros2_talker_bridge::DOMAIN_ID`, `93`) — `bridge.yml` sets it via
`lidar-in`'s own `env: { ROS_DOMAIN_ID: "93" }` — so this pair never
collides with a real ROS 2 stack running on the same host, and relies on
the standard SPDP multicast join to find each other, exactly as a real
deployment would. If your network sandbox refuses that join, see the test
below.

## Why `tests/conformance`'s estate guard is fine with `bridge.yml`

The guard walks every top-level `*.yml` in each covered example's
directory when checking that a committed manifest parses, validates and
carries no absolute path — `record-replay/replay.yml` gets the same
coverage, for the same reason. The one rule that would choke on a
`ros2:`-sourced node — "every node declares a `build:` line matching this
package" — only ever reads `dataflow.yml` itself, which is exactly why
`lidar-in` lives in `bridge.yml` instead.

## Proving the central claim without a live bridge process

This crate does not re-run `bins/astrs-ros2-bridge-node`'s own loopback
tests — that crate's `tests/loopback_bridge.rs` already proves the bridge
itself works, and duplicating it here would need a dependency this example
does not carry. What `cargo test -p ros2-talker-bridge` proves instead,
independently:

- `ros2-talker`'s own sample construction and CDR encoding produce a
  `sensor_msgs/msg/LaserScan` a plain ROS 2 subscriber decodes byte-for-byte
  unchanged (`the_talker_publishes_a_scan_a_real_subscriber_decodes`) —
  unicast-wired on loopback rather than relying on multicast, exactly as
  `bins/astrs-ros2-bridge-node`'s own loopback tests do, so it passes
  regardless of whether this sandbox permits a multicast join.
- `ros2-scan-sink`'s own decode path round-trips a scan through a columnar
  `RecordBatch` exactly as `Payload::batch()` plus
  `AstrsMessage::from_record_batch` would (`a_scan_round_trips_through_a_record_batch`).
- Both committed manifests parse and validate, and `bridge.yml`'s `ros2:`
  block is asserted against the exact fields this README shows
  (`the_committed_bridge_manifest_declares_the_ros2_block`).
