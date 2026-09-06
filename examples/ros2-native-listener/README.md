# `ros2-native-listener`

An AstRS node using `astrs-ros2`'s `Ros2Node` directly — no `ros2:`
declarative bridge sugar — the rcl-equivalent surface `astrs-ros2`
provides, used the way a node's own code would use it:

```bash
cargo build -p ros2-native-listener
astrs run examples/ros2-native-listener/dataflow.yml
```

```
[native-talker] ──/chatter (std_msgs/msg/String, real DDS/RTPS)──► [native-listener]
```

Neither node declares any AstRS `inputs:`/`outputs:` in `dataflow.yml` — both
are ordinary processes the daemon registers, health-checks, logs and stops
exactly like any other node in this estate, but the two only ever talk to
*each other*, over a real RTPS participant each creates for itself with
`Ros2Context::new`/`Ros2Node::new`. `native-talker` calls
`Ros2Node::create_publisher::<T>` and publishes on a plain interval;
`native-listener` calls `Ros2Node::create_subscription::<T>` and tallies
what arrives into `$NATIVE_LISTENER_REPORT` (default: a file under the
system temporary directory) as JSON. Contrast
[`ros2-talker-bridge`](../ros2-talker-bridge/), which crosses the same kind
of boundary *declaratively*, through a `ros2:` block and
`bins/astrs-ros2-bridge-node`.

Two details worth copying:

- **Both sides `select!` an AstRS-side source against a ROS 2-side one.**
  `native-talker` races `events.recv_async()` (for `Event::Stop`) against a
  publish interval; `native-listener` races it against
  `subscription.recv()` and a deadline. Exactly the shape
  `bins/astrs-ros2-bridge-node`'s own event loop uses (see that crate's
  `run.rs` module docs), minus the bridge's second, per-topic pump
  channel — a plain two-way `select!` is enough when there is only one ROS
  2 endpoint to wait on.
- **The domain id is not the ROS 2 default.** Both participants join
  `ros2_native_listener::DOMAIN_ID` (`92`), so this pair never collides
  with a real ROS 2 stack that happens to be running on the same host.

## Discovery relies on multicast — the same as a real deployment

`native-talker` and `native-listener` are two independent OS processes with
no shared state, so they discover each other the way two real ROS 2 nodes on
one host do: the standard SPDP multicast join on their shared domain. That
is a genuine, documented characteristic of the technology (`astrs-rtps`
treats multicast as "a probed capability"), not a gap this
example papers over — a network sandbox that refuses a loopback multicast
join (some CI sandboxes do) will keep this pair from finding each other,
exactly as it would keep two real `ros2 run demo_nodes_cpp talker/listener`
processes from finding each other.

This crate's own test,
`ros2_native_listener::tests::native_pub_sub_round_trips_over_real_rtps`,
proves the same `Ros2Node` calls work regardless: it wires two in-process
participants together with an explicit unicast peer address instead of
multicast — the pattern `bins/astrs-ros2-bridge-node`'s own loopback tests
use for the same reason — so `cargo test -p ros2-native-listener` passes on
a sandboxed host even when `astrs run` would need multicast.
