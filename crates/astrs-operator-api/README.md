# astrs-operator-api

The AstRS operator trait and event/output shims for in-process dataflow
stages.

Operators are lightweight stages that share one event loop inside an
`astrs-runtime` host: `Operator` — the trait a stage implements, with
optional `on_start`/`on_stop`/`on_reload` lifecycle hooks that default to
no-ops; `OpEvent` — a lean mirror of `astrs_wire::NodeEvent`, the five
variants an operator actually needs; `OpOutput` — the buffered send side an
operator writes through instead of holding a connection; `Status` — what
an `on_event` call decided (`Continue` or `Finished`); and
`OperatorRegistry`/`register_operator!` — the static registration table
`astrs-runtime` iterates to build the operators a manifest names. The
default path is that static registry, compiled-in Rust trait objects with
no C vtable involved. For an operator built and shipped separately, this
crate's own `dylib` feature (off by default) exports a stable `#[repr(C)]`
ABI through `export_dylib_operator!`, with `astrs-runtime`'s
`dylib-operators` feature (also off by default) as the loader; a whole node
authored from C/C++ rather than an in-process operator is the separate
`astrs-capi` crate. The `#[derive(AstrsMessage)]` and
`#[operator]` macros (hosted in the companion `astrs-operator-macros`
crate) are re-exported here, so `use astrs_operator_api::*;` reaches
everything an operator author needs in one import.

## Example

```rust
use astrs_operator_api::{Operator, OpEvent, OpOutput, OpResult, Status, register_operator};

#[derive(Default)]
struct Echo;

impl Operator for Echo {
    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match event {
            OpEvent::Input { metadata, payload, .. } => {
                out.send_bytes("echo", metadata.clone(), payload.clone())?;
                Ok(Status::Continue)
            }
            OpEvent::Stop { .. } => Ok(Status::Finished),
            _ => Ok(Status::Continue),
        }
    }
}

let registry = astrs_operator_api::OperatorRegistry::from_entries([
    register_operator!(Echo),
])?;
let _echo = registry.build("Echo")?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

See the [crate documentation](https://docs.rs/astrs-operator-api) for the
operator API design.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
