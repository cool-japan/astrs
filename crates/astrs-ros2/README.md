# astrs-ros2

An rcl-level ROS 2 client library in pure Rust: nodes, topics, services,
actions, parameters.

The AstRS equivalent of ROS 2's `rcl`, built directly on `astrs-rtps`
rather than any DDS vendor library: `Ros2Context` (the RTPS participant,
the clock, the graph announcer — one per process, usually) and `Ros2Node`
(a name, a remapping table, parameters, a set of endpoints — one per
logical node); ROS 2 splits these because a component container can host a
dozen nodes in one participant, which is why `ros_discovery_info` exists at
all (DDS discovery announces endpoints, not nodes). `Ros2Node::standalone`
is the one-node-per-process convenience; `Ros2Node::new` is the general
form. On top of that: publishers and subscriptions with QoS mapping,
services and actions, parameters (get/set/list/describe, parameter
events), ROS graph introspection, and the name-mangling conventions
(`rt/`/`rq/`/`rr/`, namespaces) that make an AstRS participant show up in
`ros2 topic list` the way any other ROS 2 participant does. This is what
`astrs ros2 doctor` and `astrs ros2 topics` are built on, and what
`bins/astrs-ros2-bridge-node` uses to speak ROS 2 on the wire.

See the [crate documentation](https://docs.rs/astrs-ros2) for the client
library design.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
