# astrs-graph

Dataflow graph model, edge type checking, placement planning and
visualization for AstRS.

What a validated manifest becomes before anything is spawned: the graph
model (nodes, ports, edges, virtual sources and modules — see
`DataflowGraph::from_manifest`), edge type checking against declared type
URNs with `type: any` as an explicit opt-out rather than a silent default,
stable topological metadata (strongly-connected-component/cycle detection
and service/action pattern pairing), a placement planner mapping nodes onto
daemons and deciding which edges are same-host versus cross-host,
visualization to mermaid and DOT for `astrs graph`, and graph diffing for
dynamic topology changes (`astrs node add/remove`). This crate builds its
graph from whatever expanded manifest shape `astrs-manifest` hands it;
module expansion itself is that crate's responsibility.

## Example

```rust
use astrs_graph::DataflowGraph;
use astrs_manifest::Manifest;

let yaml = "\
nodes:
  - id: camera
    path: ./camera
    outputs: [frames]
  - id: detector
    path: ./detector
    inputs:
      frames: camera/frames
";
let manifest = Manifest::from_yaml_str(yaml)?;
manifest.validate()?;

let (graph, diagnostics) = DataflowGraph::from_manifest(&manifest)?;
assert!(diagnostics.is_empty());
assert_eq!(graph.node_count(), 2);
assert_eq!(graph.edge_count(), 1);
assert!(graph.diagnostics().is_empty()); // no type mismatches, cycles, ...
# Ok::<(), Box<dyn std::error::Error>>(())
```

See the workspace
[README](https://github.com/cool-japan/astrs#dataflow-manifest) for the
manifest this crate's graph is built from.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
