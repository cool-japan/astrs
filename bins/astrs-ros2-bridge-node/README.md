# astrs-ros2-bridge-node

The declarative ROS 2 bridge node for AstRS dataflows.

An ordinary manifest node that a `ros2:` block configures declaratively: it
joins the DDS domain through `astrs-rtps`, subscribes or publishes the
configured topics, services and actions with the requested QoS, and
converts CDR ⇄ columnar through `astrs-idl`-generated types — preserving
ROS header timestamps into HLC metadata. The binary takes no arguments at
all; everything it needs comes from the daemon's spawn handshake
(`ASTRS_NODE_CONFIG`) and the node's `env:` block. A bad manifest and a
lost daemon exit with distinguishable codes (`78`/`sysexits.h`'s
`EX_CONFIG` for anything a manifest author can fix, `1` for a crashed
cluster), so a supervisor can tell them apart without parsing text.

## Example

```yaml
  - id: lidar-in
    ros2:
      compat: humble
      topic: /scan
      message_type: sensor_msgs/msg/LaserScan
      direction: to_astrs
      qos: { reliable: true, keep_last: 10 }
    outputs: [scan]
```

See the
[`ros2-talker-bridge`](https://github.com/cool-japan/astrs/tree/main/examples/ros2-talker-bridge)
example for a complete, runnable manifest, and the [`astrs-ros2`
documentation](https://docs.rs/astrs-ros2) for the full declarative bridge
specification.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
