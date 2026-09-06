# astrs-yaml

AstRS's own pure-Rust YAML 1.2 reader and writer for dataflow manifests.

The manifest format is the user-facing contract, and its
diagnostics are only as good as the parser underneath them: a stray tab, a
mis-indented `inputs:` block or a duplicate key has to be reported at the
line and column where it actually happens, not as an opaque "invalid type"
somewhere in a `serde` visitor. This crate is the parser AstRS controls end
to end — a cursor, a recursive-descent parser, an owned `Value` tree, and
`serde` `Serializer`/`Deserializer` adapters over it — so every manifest
error can carry a real `Span` and a caret into the offending source line.

It also removes the last unmaintained dependency from the workspace's graph.

## What it supports

A **YAML 1.2 core-schema** subset, exhaustively documented in the crate
docs. Block and flow collections, all four scalar styles, literal and folded
block scalars with chomping and explicit indentation indicators, anchors and
aliases, core-schema and local tags, `%YAML`/`%TAG` directives, comments and
multi-document streams.

Deliberately **not** supported, and said so out loud: merge keys (`<<`),
YAML 1.1 types (`!!binary`, `!!timestamp`, sexagesimals) and YAML 1.1
booleans — `on:` is the *string* `"on"`, the way YAML 1.2 says it is. A tag
this crate does not implement still suppresses core-schema resolution, so
`!!binary 42` is the string `"42"` rather than the integer.

Characters YAML's `c-printable` production forbids — the C0 controls other
than tab and newline, `DEL`, the C1 controls, `U+FFFE`/`U+FFFF` — are
rejected with a position rather than carried into a `Value`.

## Safety rails

Every parse is bounded. `Limits` caps the input size, the nesting depth
(128, the same as `serde_yaml`), and the total number of nodes alias
expansion may materialize — so the classic "billion laughs" anchor bomb is
refused at the point it starts multiplying instead of after it has taken the
machine's memory. `Error::is_limit()` tells a caller which happened.

## Compatibility

`tests/` proves the switchover is safe rather than asserting it:

- `oracle.rs` — 298 inputs whose meaning is asserted against `serde_yaml`
  0.9, plus the deliberate divergences, each with the behaviour this crate
  guarantees instead. Every entry was derived by *running* `serde_yaml`, not
  by reading the specification.
- `repo_corpus.rs` — every `.yml`/`.yaml` file in the workspace, checked
  four ways: same `Value`, same `astrs_manifest::Manifest`, byte-identical
  emitted output, and a clean self round-trip.
- `roundtrip.rs` — property tests for `parse(to_string(v)) == v` over
  randomly generated values, with a string strategy drawn from YAML's
  indicator characters.
- `fuzz.rs` — a mutation loop over a seed corpus, sized by
  `ASTRS_FUZZ_ITERS` and seeded by `ASTRS_FUZZ_SEED`, plus a regression
  corpus that runs unconditionally.

## Example

```rust
use astrs_yaml::Value;

let manifest: Value = astrs_yaml::from_str(
    "name: perception\nnodes:\n  - id: camera\n    outputs: [frames]\n",
)?;
assert_eq!(manifest.get("name").and_then(Value::as_str), Some("perception"));

// Emission is deterministic block style, and reads back as what it was.
let yaml = astrs_yaml::to_string(&manifest)?;
assert_eq!(astrs_yaml::parse_str(&yaml)?, manifest);
# Ok::<(), astrs_yaml::Error>(())
```

Errors point at the source:

```rust
use astrs_yaml::{ErrorKind, Value};

let source = "nodes:\n\t- id: camera\n";
let error = astrs_yaml::from_str::<Value>(source).unwrap_err();
assert!(matches!(error.kind(), ErrorKind::TabInIndentation));
assert_eq!((error.line(), error.column()), (Some(2), Some(1)));
assert!(error.render(source).ends_with("\t- id: camera\n^"));
```

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
