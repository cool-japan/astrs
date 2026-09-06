# astrs-operator-macros

Procedural macros backing the AstRS operator API.

Companion crate to `astrs-operator-api`. Hosts the derive and attribute
macros that generate operator registration glue, typed input/output
accessors and schema wiring an operator would otherwise write by hand:
`#[derive(AstrsMessage)]` maps a struct's fields onto the closed columnar
type set and implements `astrs_data::AstrsMessage` (the trait itself lives
in `astrs-data`, a Layer 1 crate every implementor already depends on —
this derive is one consumer among several, not the trait's owner); and
`#[astrs::operator]` adds a ready-made registry entry to an
`astrs_operator_api::Operator` impl. A proc-macro crate emits references to
paths that resolve in the *invoking* crate's own dependency graph, so this
crate itself has no runtime dependency on `astrs-data` — only a
dev-dependency, for its own expansion tests.

Used through `astrs-operator-api`'s re-export (`use
astrs_operator_api::*;`) rather than depended on directly by node or
operator authors.

See the [crate documentation](https://docs.rs/astrs-operator-macros) for
`#[derive(AstrsMessage)]` and `#[operator]`.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
