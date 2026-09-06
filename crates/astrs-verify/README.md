# astrs-verify

SMT-backed graph proofs for AstRS: deadlock freedom, queue bounds and rate
consistency.

`astrs validate --prove` discharges the graph's verification obligations
through the [OxiZ](https://github.com/cool-japan/oxiz) SMT solver: deadlock
freedom (the graph becomes a Petri net with one transition per
`(node, input)` pair, searching for an initially-empty siphon — a set of
channels no transition can ever fill, covering service/action correlation
waits too), queue boundedness (needs a declared service time; reports so
rather than guessing when none is given), rate consistency, latency budgets
(derived from declared queue depths and rates alone, no service times
needed), and type-rule consistency. No incumbent robotics middleware ships
this. The whole crate sits behind the `verify` feature: without it, it
still compiles and exposes the model, the obligation catalog and the
report types, but every proof reports `SolverUnavailable` instead of
calling a solver — which is what lets `astrs-cli` depend on it
unconditionally and gate only the *solving* on a feature (on by default in
the shipped CLI binary).

## Example

```rust
use astrs_graph::DataflowGraph;
use astrs_manifest::Manifest;
use astrs_verify::{ProveOptions, prove};

// Two nodes waiting on each other, with nothing to start them off.
let manifest = Manifest::from_yaml_str(
    "
nodes:
  - id: planner
    path: ./planner
    inputs: { pose: localizer/pose }
    outputs: [plan]
  - id: localizer
    path: ./localizer
    inputs: { plan: planner/plan }
    outputs: [pose]
",
)?;
manifest.validate()?;
let (graph, _) = DataflowGraph::from_manifest(&manifest)?;

let report = prove(&graph, &ProveOptions::default())?;
# #[cfg(feature = "verify")]
# {
assert!(report.has_violations()); // a deadlock, found and explained
# }
# Ok::<(), Box<dyn std::error::Error>>(())
```

See the [crate documentation](https://docs.rs/astrs-verify) for every
obligation this crate encodes.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
