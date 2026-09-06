# astrs-manifest

Dataflow manifest parsing, validation, module expansion and JSON-schema
emission for AstRS.

The YAML descriptor is the user-facing contract, and it is a strict one:
`deny_unknown_fields` parsing of the root, node, input long-form and
`ros2:` bridge schemas, an enum match with no fallback for
`restart_policy`/`queue_policy`, and a fixed grammar for `astrs/timer/...`
sources (config never silently lies about what it does), environment
expansion with documented precedence rules, module expansion (flattening
a `module:`-sourced node's referenced sub-graph into `parent.child` ids,
recursively), and JSON-schema emission (via `schemars`) driving editor
completion and `astrs schema`. Errors come in two stages:
`Manifest::from_yaml_str` rejects a document that fails to *parse* on the
first problem found (there is nothing useful to say about node 4 if node
2 didn't parse), while `Manifest::validate` then reports **every**
structural/cross-referential violation at once — duplicate ids, dangling
input references, inconsistent restart fields — so fixing a 20-node graph
doesn't take 20 runs.

## Example

```rust
use astrs_manifest::Manifest;

let yaml = r#"
name: perception-demo
nodes:
  - id: camera
    path: ./target/release/camera-node
    outputs: [frames]
  - id: planner
    path: ./planner
    inputs:
      frames: camera/frames
      tick: astrs/timer/hz/50
"#;

let manifest = Manifest::from_yaml_str(yaml)?;
manifest.validate()?;
assert_eq!(manifest.nodes.len(), 2);
# Ok::<(), Box<dyn std::error::Error>>(())
```

See the workspace
[README](https://github.com/cool-japan/astrs#dataflow-manifest) for the full
manifest schema.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
