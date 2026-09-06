//! Drives `astrs-capi`'s raw `extern "C"` functions directly, by pointer —
//! the same call sequence a C, C++ or `ctypes` caller makes, written in Rust
//! so it can live beside the crate and stay honest with the actual ABI
//! rather than a paraphrase of it.
//!
//! This is deliberately **not** written against `Node`/`EventStream` (the
//! ordinary, ergonomic Rust API in `astrs-node-api` — see `hello-timer` in
//! the workspace's top-level `examples/` for that shape instead). Every call
//! below goes through the same opaque `AstrsNode *`/`AstrsEvent *` pointers
//! and `int` status codes `include/astrs.h` declares, unsafely, on purpose.
//!
//! Run it under a real daemon:
//!
//! ```text
//! cargo run -p astrs-capi --example node_lifecycle
//! ```
//!
//! Without a daemon (or without `ASTRS_NODE_CONFIG` set), `astrs_init_node_from_env`
//! fails immediately and this prints the diagnostic rather than hanging —
//! exactly what a real C node should do too.

use std::ffi::{CStr, c_char};
use std::process::ExitCode;
use std::ptr;

use astrs_capi::{
    AstrsEvent, AstrsEventType, AstrsStatus, astrs_event_input_id, astrs_event_metadata_key_at,
    astrs_event_metadata_key_count, astrs_event_payload, astrs_event_type, astrs_free_event,
    astrs_init_node_from_env, astrs_last_error_message, astrs_max_payload_bytes,
    astrs_node_destroy, astrs_node_next_event, astrs_send_output, astrs_version,
};

/// How many milliseconds `astrs_node_next_event` waits per poll.
const TIMEOUT_MS: u32 = 1_000;

/// The output this example republishes onto, mirroring whatever input it
/// last read.
const OUTPUT_ID: &str = "echo";

fn main() -> ExitCode {
    // The two infallible getters need no node at all.
    let version = unsafe { CStr::from_ptr(astrs_version()) }.to_string_lossy();
    println!(
        "astrs-capi {version}, max payload {} bytes",
        astrs_max_payload_bytes()
    );

    let mut node: *mut astrs_capi::AstrsNode = ptr::null_mut();
    let status = unsafe { astrs_init_node_from_env(&mut node) };
    if status != AstrsStatus::Ok.as_raw() {
        eprintln!(
            "astrs_init_node_from_env failed (status {status}): {}",
            last_error()
        );
        return ExitCode::FAILURE;
    }

    let outcome = run(node);

    // `astrs_node_destroy` is unconditional: it must run whether `run`
    // returned Ok or Err, exactly like a Rust `Drop` would.
    let destroy_status = unsafe { astrs_node_destroy(node) };
    if destroy_status != AstrsStatus::Ok.as_raw() {
        eprintln!("astrs_node_destroy reported status {destroy_status}");
    }

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// The event loop proper: read, branch on kind, echo inputs back out, stop
/// on `ASTRS_EVENT_STOP`.
fn run(node: *mut astrs_capi::AstrsNode) -> Result<(), String> {
    loop {
        let mut event: *mut AstrsEvent = ptr::null_mut();
        let status = unsafe { astrs_node_next_event(node, TIMEOUT_MS, &mut event) };
        match AstrsStatus::from_raw(status) {
            Some(AstrsStatus::Ok) => {}
            Some(AstrsStatus::Timeout) => continue,
            Some(AstrsStatus::Closed) => return Ok(()),
            _ => return Err(format!("astrs_node_next_event failed: {}", last_error())),
        }

        let mut kind = AstrsEventType::Unknown;
        if unsafe { astrs_event_type(event, &mut kind) } != AstrsStatus::Ok.as_raw() {
            unsafe { astrs_free_event(event) };
            return Err(format!("astrs_event_type failed: {}", last_error()));
        }

        match kind {
            AstrsEventType::Stop => {
                unsafe { astrs_free_event(event) };
                return Ok(());
            }
            AstrsEventType::Input => handle_input(node, event)?,
            other => println!("event: {other:?}"),
        }

        if unsafe { astrs_free_event(event) } != AstrsStatus::Ok.as_raw() {
            return Err(format!("astrs_free_event failed: {}", last_error()));
        }
    }
}

/// Reads one input's id, payload and metadata keys, then republishes the
/// payload unchanged on [`OUTPUT_ID`].
fn handle_input(node: *mut astrs_capi::AstrsNode, event: *const AstrsEvent) -> Result<(), String> {
    let mut id_ptr: *const c_char = ptr::null();
    let mut id_len: usize = 0;
    unsafe { astrs_event_input_id(event, &mut id_ptr, &mut id_len) };
    let input_id = text(id_ptr, id_len);

    let mut data_ptr: *const u8 = ptr::null();
    let mut data_len: usize = 0;
    unsafe { astrs_event_payload(event, &mut data_ptr, &mut data_len) };

    let mut key_count: usize = 0;
    unsafe { astrs_event_metadata_key_count(event, &mut key_count) };
    let mut keys = Vec::with_capacity(key_count);
    for index in 0..key_count {
        let mut key_ptr: *const c_char = ptr::null();
        let mut key_len: usize = 0;
        unsafe { astrs_event_metadata_key_at(event, index, &mut key_ptr, &mut key_len) };
        keys.push(text(key_ptr, key_len));
    }
    println!("input `{input_id}`: {data_len} byte(s), metadata keys {keys:?}");

    let status = unsafe {
        astrs_send_output(
            node,
            OUTPUT_ID.as_ptr(),
            OUTPUT_ID.len(),
            ptr::null(),
            0,
            data_ptr,
            data_len,
        )
    };
    if status != AstrsStatus::Ok.as_raw() {
        return Err(format!("astrs_send_output failed: {}", last_error()));
    }
    Ok(())
}

/// Reads a `(const char *, size_t)` pair back as an owned `String`; `NULL`
/// (the "not applicable" idiom several accessors use) becomes `""`.
fn text(ptr: *const c_char, len: usize) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// This thread's last-recorded `astrs-capi` diagnostic, or a placeholder
/// when none was recorded.
fn last_error() -> String {
    // `astrs_last_error_message` is a safe `extern "C" fn` — it reads no
    // caller-supplied pointer — so no `unsafe` block is needed here.
    let ptr = astrs_last_error_message();
    if ptr.is_null() {
        return "(no diagnostic recorded)".to_owned();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}
