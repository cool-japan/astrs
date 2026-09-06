# astrs-capi

The AstRS C API: a stable `extern "C"` node surface for non-Rust callers.

C, C++, Python-via-`ctypes` and every other language with a C FFI get the
same node lifecycle the Rust API offers — connect to the daemon, receive
input events, send outputs, report status — through an opaque-handle ABI
with no Rust types in any signature. Built as `staticlib` + `cdylib`
alongside the ordinary `lib` target, so the crate is linkable from a C build
and still usable (and doctestable) from Rust.

Nothing here compiles C: this crate *exports* a C ABI, it does not consume
one. The workspace's pure-Rust policy is unaffected.

## The header

`include/astrs.h` is hand-written — no `cbindgen` — and kept honest against
`src/*.rs` by `tests/header_consistency.rs`, which scans the Rust source for
every `#[no_mangle] extern "C"` function and checks each one is declared in
the header (and vice versa), plus that every `ASTRS_*` enum value in the
header equals the Rust discriminant it names.

## The surface

| Area | Entry points |
|---|---|
| Version / limits | `astrs_version`, `astrs_max_payload_bytes` |
| Init / teardown | `astrs_init_node_from_env`, `astrs_init_node_from_config`, `astrs_node_destroy` |
| Events | `astrs_node_next_event`, `astrs_free_event` |
| Event accessors | `astrs_event_type`, `astrs_event_input_id`, `astrs_event_payload`, `astrs_event_metadata_key_count`, `astrs_event_metadata_key_at` |
| Sending | `astrs_send_output` |
| Errors | `astrs_last_error_message` |

`astrs_node_next_event` bridges the async node API over a plain `timeout_ms`
argument, built entirely from `astrs-node-api`'s own synchronous facade
(`EventStream::recv_timeout`/`recv_checked`) — this crate owns no runtime of
its own. `examples/node_lifecycle.rs` drives the whole lifecycle through
these exact functions, by pointer, from Rust — the shape any other
language's binding ends up with.

## Error handling

Every entry point returns an `AstrsStatus` code rather than unwinding — a
Rust panic crossing an FFI boundary is undefined behaviour, so the boundary
catches and converts — with three deliberate exceptions that cannot fail
even in principle (`astrs_version`, `astrs_max_payload_bytes`,
`astrs_last_error_message`), which return their value directly instead.

```rust
use astrs_capi::AstrsStatus;

assert!(AstrsStatus::Ok.is_ok());
assert_eq!(AstrsStatus::Ok as i32, 0);

// Every failure is negative, so `status < 0` is a valid C-side test.
assert!(!AstrsStatus::InvalidArgument.is_ok());
assert!((AstrsStatus::InvalidArgument as i32) < 0);
```

A failing call also records a diagnostic on a thread-local slot, read back
with `astrs_last_error_message()` — valid until the same thread's next
`astrs-capi` call.

## Testing

Round-trip tests (`src/tests.rs`) drive this crate's real `extern "C"`
functions — by raw pointer, exactly as a C caller would — against
`astrs_node_api::testing::MockDaemon`, the same in-process, daemonless
loopback daemon `astrs-node-api`'s own test suite uses.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
