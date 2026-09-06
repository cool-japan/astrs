# `tf-broadcast`

Broadcasting coordinate frames and an interpolated, time-travel lookup via
`astrs-tf`:

```bash
cargo build -p tf-broadcast
astrs run examples/tf-broadcast/dataflow.yml
```

```
astrs/timer/millis/20 ──► [tf-broadcaster] ──map_odom (tick 0 only)──► [tf-consumer]
                                          └──odom_base (every tick)───►
```

`tf-broadcaster` publishes a **static** `map`->`odom` offset once — the
`/tf_static` convention: a value valid at any query time needs no repeating —
and a **dynamic** `odom`->`base_link` transform that slides 0.1 m along X on
every tick — the `/tf` convention. Both ride the curated registry type
`std/geometry/v1/Transform` (`astrs_node_api::message::Transform`).

`tf-consumer` inserts every received transform into its own
`astrs_tf::TransformBuffer`, exactly as a real tf2 listener would, then looks
`map`->`base_link` up at a timestamp exactly **between** two consecutive
dynamic samples — the interpolated, time-travel query `astrs-tf` promises, not
merely "the latest value". It writes what it found to
`$TF_CONSUMER_REPORT` (default: a file under the system temporary directory)
as JSON, alongside the translation a linear motion sampled at that exact
midpoint *time* must equal: the static offset plus the arithmetic mean of the
two bracketing samples' translations. `tf_broadcast::build_report` is that
check as a pure function, so both binaries and this crate's own tests can
predict a run's outcome without a daemon — see its doctest-free unit tests in
`src/lib.rs` for the same lookup run against a deliberately unevenly spaced
sample schedule.

Two details worth copying:

- **The static frame is latched, the dynamic one is not.** `tf-broadcaster`
  sends `map_odom` on its first tick only; resending an unchanging static
  transform every tick would just be noise. `tf-consumer` still marks its
  received input closed the ordinary way once the broadcaster finishes.
- **The lookup timestamp comes from two *real* received samples, not an
  assumed tick schedule.** `midpoint_stamp` averages two actual
  `HlcTimestamp`s (via `TfStamp::from`), so the expected answer holds
  regardless of real-world scheduling jitter between ticks.

## `/tf` bridging

This example broadcasts entirely inside the AstRS graph; it does not bridge
to a real ROS 2 `/tf`/`/tf_static` topic. That bridge exists —
`bins/astrs-ros2-bridge-node` hand-writes `tf2_msgs/msg/TFMessage` support
specifically for a `ros2: { topic: /tf }` block, because `astrs-idl`
leaves `tf2_msgs` out of the pre-generated `common_interfaces` bundle (see
that crate's `Cargo.toml` for the pointer). Wiring one up is a
[`ros2-talker-bridge`](../ros2-talker-bridge/)-shaped exercise in its own
right, for a different topic — that example is the worked reference for the
declarative `ros2:` block shape.
