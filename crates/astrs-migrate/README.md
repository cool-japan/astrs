# astrs-migrate

Manifest migration into AstRS from dora-rs descriptors and ROS 2 launch
files.

Adoption cost is an afternoon, not a quarter. `from-dora`: a mechanical
dora dataflow descriptor mapping — nodes, inputs and outputs, timer virtual
inputs, restart policies, queue policies, service and action patterns —
with a `MigrationNote` for anything that needs a human rather than a
silent drop, modeled against a real dora `dora-schema.json` rather than
reconstructed from memory. `from-ros2`: ROS 2 launch-file skimming (XML
only; Python launch files are never executed or parsed, only best-effort
skimmed) that scaffolds a bridge manifest with discovered topics,
namespaces and params pre-filled, plus its own `MigrationNote`s for
anything that needs a human. Both power the `astrs migrate from-dora` and
`astrs migrate from-ros2` CLI verbs.

## Example

```rust
use astrs_migrate::migrate_str;

let dora_yaml = "\
nodes:
  - id: camera
    path: ./camera-node
    outputs: [frames]
  - id: detector
    path: ./detector-node
    inputs:
      frames: camera/frames
      tick: dora/timer/millis/100
    restart_policy: on-failure
";

let result = migrate_str(dora_yaml)?;
assert!(result.yaml.contains("astrs/timer/millis/100"));
assert!(result.yaml.contains("on_failure"));
# Ok::<(), Box<dyn std::error::Error>>(())
```

See the [crate documentation](https://docs.rs/astrs-migrate) for the dora
compatibility contract this crate implements.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
