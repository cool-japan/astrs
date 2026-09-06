# astrs

AstRS: the Pure Rust dataflow middleware for the robotic age.

This is the facade: the one dependency an application adds. Add it, pick
your features, import `astrs::prelude`, write your node. The default is
**node authoring and nothing else** — `node` (implying `data`, `time`,
`log`, `wire` and `derive`) is on by default, so `cargo add astrs` costs a
node exactly what it uses. Everything else is opt-in: `operator`/`runtime`
for hosting in-process operators, `graph`/`manifest` for tooling that
reads dataflow graphs, `recording`, `telemetry`, `tui`, `verify` (pulls in
the SMT solver, so it stays well away from `default`), and `arrow-interop`
for zero-copy `From`/`TryFrom` conversions between AstRS's own columnar
arrays and `arrow-rs` plus the `send_arrow` output method (also well away
from `default`: it is the one feature here that pulls a four-crate
`arrow-*` dependency tree into a build). `full` turns on every one of
those at once. The ROS 2 pillar (`astrs-cdr`/`-rtps`/`-idl`/`-ros2`/
`-rosbag`/`-tf`) is implemented and usable — see the workspace
[README](https://github.com/cool-japan/astrs#feature-status) — but is not
yet aggregated into this facade's own feature flags; depend on those crates
directly for now.

## Example

```toml
[dependencies]
astrs = "0.1.0"
```

```rust,no_run
use astrs::prelude::*;

#[derive(AstrsMessage)]
#[astrs(urn = "std/vision/v1/Detections")]
struct Detections {
    scores: Vec<f32>,
    labels: Vec<u32>,
}

fn main() -> Result<(), NodeError> {
    let (mut node, mut events) = Node::init_from_env()?;
    let mut detections = node.output::<Detections>("detections")?;

    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, .. } if id == "frames" => {
                let found = Detections { scores: vec![0.98], labels: vec![7] };
                detections.send(found, meta.follow())?;
            }
            Event::Stop(_) => break,
            _ => {}
        }
    }
    Ok(())
}
```

See the workspace [README](https://github.com/cool-japan/astrs#writing-a-node)
for the node API this crate's default feature exposes.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
