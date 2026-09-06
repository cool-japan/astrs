# astrs-tf

A tf2-compatible transform tree with time-travel lookup and SE(3) types.

The transform contract robotics code is written against, implemented in
pure Rust: SE(3) types (translations, unit quaternions, compose, invert),
a frame tree with static and dynamic frames, parent/child validation and
multi-publisher conflict detection, time-travel lookup with interpolation
and configurable buffer windows, and `/tf`/`/tf_static` topic bridging in
both directions. This crate implements the tf2 *contract* — the frame tree,
its math, and wire interop — deliberately without a URDF parser or
kinematic-chain solver; those live in the sibling crate `astrs-urdf` (a P1
stretch crate), which consumes `TransformBuffer` rather
than being part of it.

## Example

```rust
use astrs_tf::{TfStamp, TimePoint, TransformBuffer};
use astrs_tf::math::{Isometry3, Vector3};

let mut buffer = TransformBuffer::new();

// A fixed 10m offset from `map` to `odom`, valid at any query time.
buffer.set_transform(
    "map", "odom",
    Isometry3::from_translation(Vector3::new(10.0, 0.0, 0.0)),
    TfStamp::from_nanos(0),
    true, // static
)?;

// `base_link` drives from odom's origin to +2m on X over one second.
buffer.set_transform(
    "odom", "base_link",
    Isometry3::from_translation(Vector3::new(0.0, 0.0, 0.0)),
    TfStamp::from_nanos(0),
    false,
)?;
buffer.set_transform(
    "odom", "base_link",
    Isometry3::from_translation(Vector3::new(2.0, 0.0, 0.0)),
    TfStamp::from_nanos(1_000_000_000),
    false,
)?;

// Halfway through, map->base_link composes to 11m on X.
let halfway = TimePoint::At(TfStamp::from_nanos(500_000_000));
let t = buffer.lookup_transform("map", "base_link", halfway)?;
assert!((t.translation.x - 11.0).abs() < 1e-9);
# Ok::<(), astrs_tf::TfError>(())
```

See the [crate documentation](https://docs.rs/astrs-tf) for the tf2 bridging
design.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
