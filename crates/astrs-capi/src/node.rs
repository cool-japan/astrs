//! [`AstrsNode`] — the opaque node handle, and the entry points that create,
//! destroy and publish through it.
//!
//! # Thread-safety contract
//!
//! Mirrors the dora C API's own audited contract (dora-rs/dora#540), because
//! the underlying constraint is the same one every non-`Sync` session handle
//! has: a single [`AstrsNode`] pointer must be accessed by at most one thread
//! at a time. Calling [`astrs_node_next_event`](crate::astrs_node_next_event)
//! or [`astrs_send_output`] concurrently with the same pointer is undefined
//! behaviour. [`astrs_node_destroy`] takes ownership and must be the last
//! call made with a given pointer, from a thread that can guarantee no other
//! thread is still using it.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::{c_char, c_int};

use astrs_node_api::prelude::DataId;
use astrs_node_api::{Node, NodeBuilder, RawOutput};

use crate::ffi::{bytes_or_empty, required_str, str_or_empty};
use crate::status::{AstrsStatus, guard, set_last_error};

/// The crate's own version, as declared in `Cargo.toml`, null-terminated for
/// direct use as a C string.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

/// A live participant in an AstRS dataflow, reached over the C ABI.
///
/// Opaque to C: the header forward-declares `AstrsNode` with no body, and
/// every function that touches one takes `AstrsNode *`. Built by
/// [`astrs_init_node_from_env`] or [`astrs_init_node_from_config`]; freed by
/// [`astrs_node_destroy`].
///
/// # Field order is load-bearing
///
/// Rust drops a struct's fields in declaration order, and that order is
/// deliberate here, reproducing the drop order an idiomatic Rust node gets
/// for free from ordinary lexical scoping (`let (node, events) = ...; let
/// out = node.raw_output(...)?;` drops `out` first, `events` second, `node`
/// last, since locals drop in *reverse* declaration order while struct
/// fields drop in *forward* declaration order — the two rules land on the
/// same sequence here only because `outputs` is declared first).
///
/// `outputs` must drop before `node`: each [`RawOutput`]'s own `Drop` sends
/// `CloseOutputs` for that one port (see its doc comment on why that framing,
/// not `OutputDone`, matters for §12's truthful producer-failure rule) — a
/// request that only reaches the daemon if the session is still open when it
/// is queued. `Node::drop` calls `shutdown()` (marks the session closed) and
/// then `drain_writer()` (flushes and stops accepting more), so a `Node`
/// dropped *before* its cached `RawOutput`s would leave every per-output
/// close silently swallowed by the `let _ =` in `RawOutput::drop` — every
/// downstream consumer would then wait for an `InputClosed` that never
/// arrives, instead of finishing. Reordering these fields "for tidiness"
/// reintroduces exactly that hang.
pub struct AstrsNode {
    /// Publishing handles, created lazily on first send and cached
    /// thereafter — [`Node::raw_output`] hands out at most one handle per
    /// output id, so a fresh lookup on every [`astrs_send_output`] call would
    /// fail on the second call to the same output.
    outputs: HashMap<DataId, RawOutput>,
    /// The node's event inbox.
    pub(crate) events: astrs_node_api::EventStream,
    /// The registered session.
    node: Node,
}

impl AstrsNode {
    /// Wraps an already-registered `(Node, EventStream)` pair.
    ///
    /// Used by [`astrs_init_node_from_env`] and [`astrs_init_node_from_config`]
    /// after a real handshake, and — as a plain Rust constructor, never
    /// `extern "C"` and never mentioned in `include/astrs.h` — by this
    /// crate's own round-trip tests (`src/tests.rs`) to drive the same
    /// `extern "C"` functions against
    /// `astrs_node_api::testing::MockDaemon`'s in-process loopback daemon
    /// instead of a live one.
    pub(crate) fn new(node: Node, events: astrs_node_api::EventStream) -> Self {
        Self {
            outputs: HashMap::new(),
            events,
            node,
        }
    }
}

/// This crate's own version, as a null-terminated UTF-8 C string.
///
/// The one function in this crate with no arguments to validate and no I/O
/// to attempt — [`AstrsStatus`] exists to report failure, and this call
/// cannot fail, so it returns its value directly rather than through an
/// out-parameter, matching `zlibVersion()`/`sqlite3_libversion()` in their
/// own C libraries.
///
/// # Safety
///
/// None: this function reads no caller-supplied pointer. The returned
/// pointer is `'static` (a string literal baked into the binary) and never
/// needs freeing.
#[unsafe(no_mangle)]
pub extern "C" fn astrs_version() -> *const c_char {
    VERSION.as_ptr().cast::<c_char>()
}

/// The largest payload [`astrs_send_output`] can ever accept, in bytes.
///
/// A hard ceiling this build was compiled with (`astrs_data::MAX_PAYLOAD_BYTES`),
/// useful for a caller sizing a buffer before any node exists. The
/// *effective* limit for one connected node's outputs may be lower — it is
/// negotiated with the daemon at connect time — and [`astrs_send_output`]'s
/// own [`AstrsStatus::InvalidArgument`] is the authoritative check; this is
/// an upper bound, not a promise.
///
/// # Safety
///
/// None: this function reads no caller-supplied pointer and performs no I/O.
#[unsafe(no_mangle)]
pub extern "C" fn astrs_max_payload_bytes() -> usize {
    astrs_data::MAX_PAYLOAD_BYTES
}

/// Joins the dataflow using the `ASTRS_NODE_CONFIG` environment blob the
/// daemon set for this process (blueprint §24.2) — the entry point an
/// ordinary spawned node uses.
///
/// On success, `*out_node` is a freshly allocated handle; the caller must
/// eventually pass it to exactly one [`astrs_node_destroy`] call. On
/// failure, `*out_node` is set to `NULL` and nothing needs freeing.
///
/// # Safety
///
/// `out_node` must be a valid, writable `AstrsNode *` destination.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn astrs_init_node_from_env(out_node: *mut *mut AstrsNode) -> c_int {
    guard(|| {
        if out_node.is_null() {
            return AstrsStatus::InvalidArgument;
        }
        unsafe { *out_node = std::ptr::null_mut() };
        let builder = match NodeBuilder::from_env() {
            Ok(builder) => builder,
            Err(error) => {
                let status = AstrsStatus::of_node_error(&error);
                set_last_error(error.to_string());
                return status;
            }
        };
        unsafe { init_node(out_node, builder) }
    })
}

/// Joins the dataflow using an explicit configuration blob rather than
/// reading `ASTRS_NODE_CONFIG` from the process environment — for a host
/// embedding several nodes with different configurations in one process,
/// where a single process-wide environment variable cannot name more than
/// one.
///
/// `config_ptr`/`config_len` must be the same blob format
/// `ASTRS_NODE_CONFIG` carries (an `astrs_wire::NodeConfig`, produced by
/// whatever spawned this node). On success, `*out_node` is a freshly
/// allocated handle owed exactly one [`astrs_node_destroy`] call; on
/// failure, `*out_node` is set to `NULL`.
///
/// # Safety
///
/// `out_node` must be a valid, writable `AstrsNode *` destination.
/// `config_ptr`/`config_len` must describe a byte buffer valid for reads of
/// `config_len` bytes for the duration of this call (or satisfy the
/// `(NULL, 0)` idiom for an empty, and therefore invalid, blob).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn astrs_init_node_from_config(
    config_ptr: *const u8,
    config_len: usize,
    out_node: *mut *mut AstrsNode,
) -> c_int {
    guard(|| {
        if out_node.is_null() {
            return AstrsStatus::InvalidArgument;
        }
        unsafe { *out_node = std::ptr::null_mut() };
        let config = match unsafe { str_or_empty(config_ptr, config_len) } {
            Ok(config) => config,
            Err(status) => {
                set_last_error("astrs_init_node_from_config: config is not valid UTF-8");
                return status;
            }
        };
        let builder = match NodeBuilder::from_env_value(config) {
            Ok(builder) => builder,
            Err(error) => {
                let status = AstrsStatus::of_node_error(&error);
                set_last_error(error.to_string());
                return status;
            }
        };
        unsafe { init_node(out_node, builder) }
    })
}

/// Shared tail of both `init_node_from_*` entry points: connects, and on
/// success boxes the result and writes it through `out_node`.
///
/// Both callers have already null-checked `out_node` and pre-written `NULL`
/// through it before reaching here, so this does neither again.
///
/// # Safety
///
/// `out_node` must be a valid, writable `AstrsNode *` destination.
unsafe fn init_node(out_node: *mut *mut AstrsNode, builder: NodeBuilder) -> AstrsStatus {
    match builder.connect() {
        Ok((node, events)) => {
            let boxed = Box::new(AstrsNode::new(node, events));
            unsafe { *out_node = Box::into_raw(boxed) };
            AstrsStatus::Ok
        }
        Err(error) => {
            let status = AstrsStatus::of_node_error(&error);
            set_last_error(error.to_string());
            status
        }
    }
}

/// Destroys a node handle: closes every output and ends the session.
///
/// Freeing `NULL` is a safe no-op, matching `free(NULL)`. Every other pointer
/// must have come from [`astrs_init_node_from_env`] or
/// [`astrs_init_node_from_config`], must not have been freed already, and
/// must not be used — by this thread or any other — after this call returns.
///
/// # Safety
///
/// `node` must be `NULL` or a still-valid pointer this crate produced, with
/// no other thread concurrently using it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn astrs_node_destroy(node: *mut AstrsNode) -> c_int {
    guard(|| {
        if !node.is_null() {
            drop(unsafe { Box::from_raw(node) });
        }
        AstrsStatus::Ok
    })
}

/// Publishes raw payload bytes on one of this node's outputs.
///
/// `output_id_ptr`/`output_id_len` name the output, exactly as declared in
/// the dataflow manifest. `data_ptr`/`data_len` are published exactly as
/// given — this crate does not interpret, wrap or validate them as any
/// particular encoding, so what a consumer's `astrs_event_payload` reads back
/// is byte-for-byte what was sent here. The `(NULL, 0)` idiom publishes an
/// empty payload.
///
/// `type_urn_ptr`/`type_urn_len` are an optional type URN (`(NULL, 0)` to
/// omit). When given and the port declares a type in the manifest, it is
/// checked the way a typed `Output<T>` handle checks `T::URN` (§9.2): under
/// `ASTRS_TYPE_CHECK=error` a mismatch is refused with
/// [`AstrsStatus::TypeMismatch`] and nothing is sent; under the default
/// `warn` a mismatch is logged (`Node::log_warn`) and the send proceeds;
/// under `off` the URN is accepted without comparison. An empty URN, or a
/// port with no declared type, skips the check entirely.
///
/// The first call naming a given output id claims that output's one and only
/// publishing handle and caches it on this node for the life of the handle
/// (mirroring `Node::raw_output`'s "one handle per output" rule) — later
/// calls to the same output id reuse it.
///
/// # Safety
///
/// `node` must be a valid `AstrsNode *`. `output_id_ptr`/`output_id_len` must
/// describe `output_id_len` readable bytes (never the `(NULL, 0)` idiom — an
/// empty output id is never valid). `type_urn_ptr`/`type_urn_len` and
/// `data_ptr`/`data_len` must each describe that many readable bytes, or
/// satisfy the `(NULL, 0)` idiom.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn astrs_send_output(
    node: *mut AstrsNode,
    output_id_ptr: *const u8,
    output_id_len: usize,
    type_urn_ptr: *const u8,
    type_urn_len: usize,
    data_ptr: *const u8,
    data_len: usize,
) -> c_int {
    guard(|| {
        if node.is_null() {
            return AstrsStatus::InvalidArgument;
        }
        // Destructured into two independent `&mut`/`&` bindings up front so
        // the borrow checker sees `outputs` and `session` as provably
        // disjoint for the rest of this call, rather than reasoning through
        // whatever lifetime `HashMap::entry`'s `Entry` return value ties
        // back to on a `node.outputs.entry(...)` projection.
        let AstrsNode {
            outputs,
            node: session,
            ..
        } = unsafe { &mut *node };

        let output_id = match unsafe { required_str(output_id_ptr, output_id_len) } {
            Ok(id) => id,
            Err(status) => {
                set_last_error("astrs_send_output: output id is empty or not valid UTF-8");
                return status;
            }
        };
        let id = match DataId::new(output_id) {
            Ok(id) => id,
            Err(error) => {
                set_last_error(format!("astrs_send_output: invalid output id: {error}"));
                return AstrsStatus::InvalidArgument;
            }
        };
        let type_urn = match unsafe { str_or_empty(type_urn_ptr, type_urn_len) } {
            Ok(urn) => urn,
            Err(status) => {
                set_last_error("astrs_send_output: type URN is not valid UTF-8");
                return status;
            }
        };
        let data = match unsafe { bytes_or_empty(data_ptr, data_len) } {
            Ok(data) => data,
            Err(status) => {
                set_last_error("astrs_send_output: data is null with a non-zero length");
                return status;
            }
        };

        let output = match outputs.entry(id.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => match session.raw_output(id.as_str()) {
                Ok(handle) => entry.insert(handle),
                Err(error) => {
                    let status = AstrsStatus::of_node_error(&error);
                    set_last_error(error.to_string());
                    return status;
                }
            },
        };

        if let Err(status) = check_declared_type(session, &id, type_urn) {
            return status;
        }

        let metadata = session.metadata();
        match output.send_bytes(data, metadata) {
            Ok(()) => AstrsStatus::Ok,
            Err(error) => {
                let status = AstrsStatus::of_node_error(&error);
                set_last_error(error.to_string());
                status
            }
        }
    })
}

/// Whether `urn` names the same type as a port whose declared type renders
/// as `declared_base` (parameters stripped, e.g. `std/media/v1/Image` for a
/// port declared `std/media/v1/Image[pixel=rgb8]`) or `declared_full` (the
/// exact form) — matching either. The same two-way comparison `Node`'s own
/// (private) `check_declared_type` makes for a typed `Output<T>` handle
/// (`declared.base() == requested || declared.as_str() == requested`).
///
/// Takes the two rendered forms rather than the `astrs_wire::TypeUrn` object
/// itself they come from, so this crate's production code never needs to
/// name that type: `astrs-capi` has no regular dependency on `astrs-wire`
/// (see `Cargo.toml`) — only a dev-dependency, for `src/tests.rs`, which is
/// where this function is unit tested too.
pub(crate) fn urn_matches(declared_base: &str, declared_full: &str, urn: &str) -> bool {
    declared_base == urn || declared_full == urn
}

/// The type-URN half of [`astrs_send_output`], reimplemented from
/// `Node`'s own public surface: `Node::check_declared_type` (which
/// `Output::<T>::send` uses) is private to `astrs-node-api`, so a caller
/// with no Rust type to supply `T::URN` needs the same comparison built from
/// [`Node::descriptor`] and [`Node::type_check`] instead. An empty `urn`
/// means "not supplied" and always passes, matching `RawOutput::send_bytes`'s
/// own untyped contract.
fn check_declared_type(node: &Node, id: &DataId, urn: &str) -> Result<(), AstrsStatus> {
    if urn.is_empty() || !node.type_check().is_enabled() {
        return Ok(());
    }
    let Some(declared) = node
        .descriptor()
        .output(id)
        .and_then(|spec| spec.type_urn.as_ref())
    else {
        return Ok(());
    };
    if urn_matches(declared.base(), declared.as_str(), urn) {
        return Ok(());
    }
    if node.type_check().is_fatal() {
        set_last_error(format!(
            "output `{id}` is declared as `{declared}`, but the caller supplied `{urn}`"
        ));
        return Err(AstrsStatus::TypeMismatch);
    }
    node.log_warn(format!(
        "astrs_send_output: output `{id}` is declared as `{declared}`, but the caller supplied \
         `{urn}` (ASTRS_TYPE_CHECK=warn, sending anyway)"
    ));
    Ok(())
}
