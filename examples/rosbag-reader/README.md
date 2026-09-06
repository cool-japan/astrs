# `rosbag-reader`

Reading a rosbag2 `.db3` via `astrs-rosbag` and feeding it into a pipeline:

```bash
cargo build -p rosbag-reader
astrs run examples/rosbag-reader/dataflow.yml
```

```
[write .db3] ──► [read .db3] ──► [bag-feeder] ──feed──► [bag-sink] ──► ROSBAG_READER_REPORT (JSON)
```

`bag-feeder` writes a tiny fixture bag **in code** with `astrs_rosbag::db3::Writer`
— one topic (`/chatter`, `std_msgs/msg/String`, CDR-encoded, exactly as a
real rosbag2 recording would carry it), eight messages by default — then
reads it straight back with `astrs_rosbag::db3::Reader` before the AstRS
event loop even starts, and republishes one decoded message per timer tick
on the typed port `feed`. `bag-sink` tallies what arrives and writes
`$ROSBAG_READER_REPORT` (default: a file under the system temporary
directory) as JSON. No checked-in binary fixture, no external ROS install:
`astrs run` on a clean checkout produces and consumes its own bag.

## Inspecting the generated bag with `astrs bag info`

`bag-feeder` writes the bag to `$ROSBAG_READER_BAG_PATH`, defaulting to a
process-id-tagged path under the system temporary directory that it logs on
startup — read the path from that line and hand it to `astrs bag info`:

```bash
astrs run examples/rosbag-reader/dataflow.yml
#    0.782s [feeder] bag-feeder up: 8 messages from /tmp/astrs-rosbag-reader-<pid>.db3
astrs bag info /tmp/astrs-rosbag-reader-<pid>.db3   # the path that line printed
```

Exporting `ROSBAG_READER_BAG_PATH` in the shell that invokes `astrs run`
does *not* reach `bag-feeder`: the daemon spawns every node with its
environment scrubbed to a small allowlist plus whatever the manifest's own
`env:` block declares — a value the invoking shell merely
exported is gone before the node ever sees it. Pinning the bag to a fixed
path needs a literal value in the manifest's own `env:` block instead, the
same way `ROSBAG_READER_MESSAGES` already is one.

`astrs bag info` reads the same `.db3`/`metadata.yaml` pair this example's
writer produces — the topic, its message type and count, and the bag's time
range — the same command a reader would run against a bag captured from a
real ROS 2 robot.

Two details worth copying:

- **The fixture is written and read back before the node registers with the
  daemon.** A failure in either shows up as this node failing to start,
  not as a graph that silently ran with no messages.
- **`write_fixture_bag` and `message_text`/`message_timestamp_ns` are pure
  functions in `src/lib.rs`.** Both binaries and this crate's own test
  (`a_written_fixture_reads_back_exactly`) predict a fixture's exact
  contents — CDR round trip included — without a daemon.
