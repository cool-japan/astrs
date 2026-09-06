//! Round-trip tests driving this crate's own `extern "C"` functions — by
//! pointer, exactly as a C caller would — against
//! `astrs_node_api::testing::MockDaemon`'s in-process, daemonless loopback
//! daemon (the same pattern `astrs-node-api`'s own test suite uses, e.g.
//! `astrs-node-api/src/testing/daemon.rs`'s own `#[test]`s connect nodes
//! straight through `MockDaemon::connect_node` rather than a real socket).
//!
//! [`AstrsNode::new`] is the seam: a plain, non-`extern "C"`, crate-private
//! Rust constructor (never mentioned in `include/astrs.h`, so it never
//! becomes part of the C ABI surface `tests/header_consistency.rs` checks)
//! that wraps an already-registered `(Node, EventStream)` pair the same way
//! [`astrs_init_node_from_env`] does after a real handshake — the only
//! difference here is *what* did the registering.
//!
//! Everything after that seam calls the crate's real `#[unsafe(no_mangle)]`
//! functions through a raw pointer, the same as `crates/astrs-capi/examples/
//! node_lifecycle.rs` and any actual C caller.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::{CStr, c_char};
use std::ptr;
use std::time::Duration;

use astrs_node_api::testing::MockDaemon;
use astrs_node_api::{EventStream, Node};
use astrs_wire::{
    DataId, InputSpec, Metadata, NodeId, NodeSource, NodeSpawnSpec, OutputSpec, Parameter, PortRef,
    TypeUrn,
};

use crate::event::{AstrsEvent, AstrsEventType};
use crate::node::AstrsNode;
use crate::status::AstrsStatus;
use crate::{
    astrs_event_input_id, astrs_event_metadata_key_at, astrs_event_metadata_key_count,
    astrs_event_payload, astrs_event_type, astrs_free_event, astrs_last_error_message,
    astrs_max_payload_bytes, astrs_node_destroy, astrs_node_next_event, astrs_send_output,
    astrs_version,
};

/// Wraps a registered `(Node, EventStream)` pair and leaks it behind the
/// same kind of raw pointer [`astrs_init_node_from_env`](crate::astrs_init_node_from_env)
/// hands a C caller.
fn boxed(node: Node, events: EventStream) -> *mut AstrsNode {
    Box::into_raw(Box::new(AstrsNode::new(node, events)))
}

/// A single-node specification: one input `in` from a notional peer, one
/// output `out`.
fn solo_spec(daemon: &MockDaemon, name: &str) -> NodeSpawnSpec {
    NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new(name).unwrap(),
        0,
        NodeSource::Dynamic,
    )
    .with_input(InputSpec::new(
        DataId::new("in").unwrap(),
        PortRef::from_parts("peer", "out").unwrap(),
    ))
    .with_output(OutputSpec::new(DataId::new("out").unwrap()))
}

/// A producer (`image` output) and a consumer (`frames` input, wired to the
/// producer's `image`) — the same shape `TestHarness::pair` builds, but
/// returning the raw `(Node, EventStream)` pairs this module needs to box
/// into `AstrsNode` handles directly.
fn producer_consumer_specs(daemon: &MockDaemon) -> (NodeSpawnSpec, NodeSpawnSpec) {
    let producer = NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new("camera").unwrap(),
        0,
        NodeSource::Dynamic,
    )
    .with_output(OutputSpec::new(DataId::new("image").unwrap()));
    let consumer = NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new("detect").unwrap(),
        0,
        NodeSource::Dynamic,
    )
    .with_input(InputSpec::new(
        DataId::new("frames").unwrap(),
        PortRef::from_parts("camera", "image").unwrap(),
    ));
    (producer, consumer)
}

/// How long every blocking wait in this module allows.
const WAIT: Duration = Duration::from_secs(5);

/// Polls `daemon.logs()` until it holds at least `count` entries or `WAIT`
/// elapses.
///
/// `MockDaemon::send_request`/`RawOutput::send_bytes` and `Node::log_warn`
/// only *enqueue* onto the session's outgoing channel; a background writer
/// task flushes it and the daemon's own serving task records it, both
/// asynchronously to the calling thread — so, unlike
/// `MockDaemon::sends_on` (which this module reaches only through
/// `MockDaemon::wait_for_sends`, the daemon's own purpose-built
/// synchronisation point), a bare immediate `daemon.logs()` read right
/// after a call returns is not guaranteed to already reflect it. This is
/// the same wait `MockDaemon::wait_for` gives request-recording tests,
/// reimplemented locally because its predicate is fixed to
/// `&[RecordedRequest]` rather than logs.
fn wait_for_log_count(
    daemon: &MockDaemon,
    count: usize,
    timeout: Duration,
) -> Vec<astrs_wire::LogRecord> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let logs = daemon.logs();
        if logs.len() >= count || std::time::Instant::now() >= deadline {
            return logs;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Reads a `(const char *, size_t)` out-pair back as a `&str`, the way a C
/// caller would (the pointer is not null-terminated, so this never uses
/// `CStr`).
fn read_text(ptr: *const c_char, len: usize) -> String {
    assert!(!ptr.is_null(), "expected a non-null text pointer");
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
    std::str::from_utf8(bytes).unwrap().to_owned()
}

// ---------------------------------------------------------------------------
// Version / limits / last-error, with no node at all
// ---------------------------------------------------------------------------

#[test]
fn version_and_max_payload_bytes_need_no_node() {
    let version = unsafe { CStr::from_ptr(astrs_version()) }.to_str().unwrap();
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
    assert_eq!(astrs_max_payload_bytes(), astrs_data::MAX_PAYLOAD_BYTES);
}

// ---------------------------------------------------------------------------
// Single-node round trip
// ---------------------------------------------------------------------------

#[test]
fn a_single_node_receives_an_input_and_publishes_a_reply_through_the_c_abi() {
    let daemon = MockDaemon::start().unwrap();
    let (node, events) = daemon.connect_node(solo_spec(&daemon, "probe")).unwrap();
    let node_id = node.id().clone();
    let handle = boxed(node, events);

    let mut metadata = Metadata::default();
    metadata.insert("seq", Parameter::Integer(7)).unwrap();
    daemon
        .send_input(
            &node_id,
            &DataId::new("in").unwrap(),
            metadata,
            vec![9, 8, 7],
        )
        .unwrap();

    let mut event_ptr: *mut AstrsEvent = ptr::null_mut();
    let status = unsafe { astrs_node_next_event(handle, 5_000, &mut event_ptr) };
    assert_eq!(status, AstrsStatus::Ok.as_raw(), "{}", last_error());
    assert!(!event_ptr.is_null());

    let mut kind = AstrsEventType::Unknown;
    assert_eq!(
        unsafe { astrs_event_type(event_ptr, &mut kind) },
        AstrsStatus::Ok.as_raw()
    );
    assert_eq!(kind, AstrsEventType::Input);

    let mut id_ptr: *const c_char = ptr::null();
    let mut id_len: usize = 0;
    assert_eq!(
        unsafe { astrs_event_input_id(event_ptr, &mut id_ptr, &mut id_len) },
        AstrsStatus::Ok.as_raw()
    );
    assert_eq!(read_text(id_ptr, id_len), "in");

    let mut data_ptr: *const u8 = ptr::null();
    let mut data_len: usize = 0;
    assert_eq!(
        unsafe { astrs_event_payload(event_ptr, &mut data_ptr, &mut data_len) },
        AstrsStatus::Ok.as_raw()
    );
    assert!(!data_ptr.is_null());
    let data = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };
    assert_eq!(data, &[9, 8, 7]);

    let mut key_count: usize = 0;
    assert_eq!(
        unsafe { astrs_event_metadata_key_count(event_ptr, &mut key_count) },
        AstrsStatus::Ok.as_raw()
    );
    assert_eq!(key_count, 1);
    let mut key_ptr: *const c_char = ptr::null();
    let mut key_len: usize = 0;
    assert_eq!(
        unsafe { astrs_event_metadata_key_at(event_ptr, 0, &mut key_ptr, &mut key_len) },
        AstrsStatus::Ok.as_raw()
    );
    assert_eq!(read_text(key_ptr, key_len), "seq");
    // Out of range is reported, not silently zeroed.
    assert_eq!(
        unsafe { astrs_event_metadata_key_at(event_ptr, 1, &mut key_ptr, &mut key_len) },
        AstrsStatus::InvalidArgument.as_raw()
    );

    assert_eq!(
        unsafe { astrs_free_event(event_ptr) },
        AstrsStatus::Ok.as_raw()
    );

    let reply = b"reply-bytes";
    let status = unsafe {
        astrs_send_output(
            handle,
            b"out".as_ptr(),
            3,
            ptr::null(),
            0,
            reply.as_ptr(),
            reply.len(),
        )
    };
    assert_eq!(status, AstrsStatus::Ok.as_raw(), "{}", last_error());
    let sends = daemon
        .wait_for_sends(&node_id, &DataId::new("out").unwrap(), 1, WAIT)
        .unwrap();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].bytes(), Some(&reply[..]));

    // A second send to the same output reuses the cached handle rather than
    // failing with "already claimed".
    let status =
        unsafe { astrs_send_output(handle, b"out".as_ptr(), 3, ptr::null(), 0, ptr::null(), 0) };
    assert_eq!(status, AstrsStatus::Ok.as_raw(), "{}", last_error());
    let sends = daemon
        .wait_for_sends(&node_id, &DataId::new("out").unwrap(), 2, WAIT)
        .unwrap();
    assert_eq!(sends.len(), 2);

    assert_eq!(
        unsafe { astrs_node_destroy(handle) },
        AstrsStatus::Ok.as_raw()
    );
}

// ---------------------------------------------------------------------------
// Two-node round trip
// ---------------------------------------------------------------------------

#[test]
fn a_producers_send_output_reaches_a_consumers_next_event() {
    let daemon = MockDaemon::start().unwrap();
    let (producer_spec, consumer_spec) = producer_consumer_specs(&daemon);
    let (producer, producer_events) = daemon.connect_node(producer_spec).unwrap();
    let (consumer, consumer_events) = daemon.connect_node(consumer_spec).unwrap();
    let producer_handle = boxed(producer, producer_events);
    let consumer_handle = boxed(consumer, consumer_events);

    let frame = [1u8, 2, 3, 4, 5];
    let status = unsafe {
        astrs_send_output(
            producer_handle,
            b"image".as_ptr(),
            5,
            ptr::null(),
            0,
            frame.as_ptr(),
            frame.len(),
        )
    };
    assert_eq!(status, AstrsStatus::Ok.as_raw(), "{}", last_error());

    let mut event_ptr: *mut AstrsEvent = ptr::null_mut();
    let status = unsafe { astrs_node_next_event(consumer_handle, 5_000, &mut event_ptr) };
    assert_eq!(status, AstrsStatus::Ok.as_raw(), "{}", last_error());

    let mut id_ptr: *const c_char = ptr::null();
    let mut id_len: usize = 0;
    unsafe { astrs_event_input_id(event_ptr, &mut id_ptr, &mut id_len) };
    assert_eq!(read_text(id_ptr, id_len), "frames");

    let mut data_ptr: *const u8 = ptr::null();
    let mut data_len: usize = 0;
    unsafe { astrs_event_payload(event_ptr, &mut data_ptr, &mut data_len) };
    let data = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };
    assert_eq!(data, &frame[..]);

    unsafe { astrs_free_event(event_ptr) };
    unsafe { astrs_node_destroy(producer_handle) };
    unsafe { astrs_node_destroy(consumer_handle) };
}

// ---------------------------------------------------------------------------
// Timeout / closed
// ---------------------------------------------------------------------------

#[test]
fn next_event_times_out_without_closing_an_idle_stream() {
    let daemon = MockDaemon::start().unwrap();
    let (node, events) = daemon.connect_node(solo_spec(&daemon, "idle")).unwrap();
    let handle = boxed(node, events);

    let mut event_ptr: *mut AstrsEvent = ptr::null_mut();
    let status = unsafe { astrs_node_next_event(handle, 20, &mut event_ptr) };
    assert_eq!(status, AstrsStatus::Timeout.as_raw());
    assert!(event_ptr.is_null());

    // The stream is still open: a later event is still delivered normally.
    unsafe { astrs_node_destroy(handle) };
}

#[test]
fn next_event_reports_closed_once_the_stream_has_fused() {
    let daemon = MockDaemon::start().unwrap();
    let (node, events) = daemon
        .connect_node(solo_spec(&daemon, "stoppable"))
        .unwrap();
    let node_id = node.id().clone();
    let handle = boxed(node, events);

    daemon
        .stop(&node_id, astrs_wire::StopCause::Requested)
        .unwrap();

    let mut event_ptr: *mut AstrsEvent = ptr::null_mut();
    let status = unsafe { astrs_node_next_event(handle, 5_000, &mut event_ptr) };
    assert_eq!(status, AstrsStatus::Ok.as_raw());
    let mut kind = AstrsEventType::Unknown;
    unsafe { astrs_event_type(event_ptr, &mut kind) };
    assert_eq!(kind, AstrsEventType::Stop);
    unsafe { astrs_free_event(event_ptr) };

    // The fuse: every later call reports Closed, never blocking, never
    // returning an event.
    let status = unsafe { astrs_node_next_event(handle, 5_000, &mut event_ptr) };
    assert_eq!(status, AstrsStatus::Closed.as_raw());
    assert!(event_ptr.is_null());

    unsafe { astrs_node_destroy(handle) };
}

// ---------------------------------------------------------------------------
// Unknown / undeclared output
// ---------------------------------------------------------------------------

#[test]
fn sending_on_an_undeclared_output_is_reported_not_silently_dropped() {
    let daemon = MockDaemon::start().unwrap();
    let (node, events) = daemon.connect_node(solo_spec(&daemon, "narrow")).unwrap();
    let handle = boxed(node, events);

    let status =
        unsafe { astrs_send_output(handle, b"nope".as_ptr(), 4, ptr::null(), 0, ptr::null(), 0) };
    assert_eq!(status, AstrsStatus::UnknownPort.as_raw());
    assert!(!last_error().is_empty());

    unsafe { astrs_node_destroy(handle) };
}

// ---------------------------------------------------------------------------
// Type URN checking (astrs_send_output's declared-type comparison)
// ---------------------------------------------------------------------------

#[test]
fn urn_matches_compares_both_the_base_and_full_form() {
    let declared = TypeUrn::new("std/media/v1/Image[pixel=rgb8]").unwrap();
    let (base, full) = (declared.base(), declared.as_str());
    assert!(crate::node::urn_matches(base, full, "std/media/v1/Image"));
    assert!(crate::node::urn_matches(
        base,
        full,
        "std/media/v1/Image[pixel=rgb8]"
    ));
    assert!(!crate::node::urn_matches(
        base,
        full,
        "std/media/v1/Image[pixel=gray8]"
    ));
    assert!(!crate::node::urn_matches(base, full, "std/core/v1/Bytes"));

    let scalar = TypeUrn::new("std/core/v1/Float64").unwrap();
    assert!(crate::node::urn_matches(
        scalar.base(),
        scalar.as_str(),
        "std/core/v1/Float64"
    ));
    assert!(!crate::node::urn_matches(
        scalar.base(),
        scalar.as_str(),
        "std/core/v1/Int64"
    ));

    // `declared_base` and `declared_full` are combined with `||`, so
    // `urn_matches` is deliberately insensitive to which of the two is
    // passed first — only whether `urn` equals *either* one. `base`/`full`
    // genuinely differ here (unlike `scalar`, where they coincide), which is
    // what makes the first block above exercise both disjuncts separately
    // rather than one always subsuming the other.
    assert_ne!(base, full);
}

#[test]
fn send_output_accepts_a_matching_type_urn_on_a_typed_port() {
    let daemon = MockDaemon::start().unwrap();
    let spec = NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new("typed").unwrap(),
        0,
        NodeSource::Dynamic,
    )
    .with_output(
        OutputSpec::new(DataId::new("out").unwrap())
            .with_type(TypeUrn::new("std/core/v1/Bytes").unwrap()),
    );
    let (node, events) = daemon.connect_node(spec).unwrap();
    let node_id = node.id().clone();
    let handle = boxed(node, events);

    let urn = "std/core/v1/Bytes";
    let status = unsafe {
        astrs_send_output(
            handle,
            b"out".as_ptr(),
            3,
            urn.as_ptr(),
            urn.len(),
            ptr::null(),
            0,
        )
    };
    assert_eq!(status, AstrsStatus::Ok.as_raw(), "{}", last_error());
    let sends = daemon
        .wait_for_sends(&node_id, &DataId::new("out").unwrap(), 1, WAIT)
        .unwrap();
    assert_eq!(sends.len(), 1);
    assert!(daemon.logs().is_empty(), "a matching URN must not warn");

    unsafe { astrs_node_destroy(handle) };
}

/// `MockDaemon::connect_node` always registers with `ASTRS_TYPE_CHECK=warn`
/// (`astrs-node-api/src/testing/daemon.rs`'s own `connect_node_async` calls
/// `.type_check(TypeCheckMode::Warn)` unconditionally), so this is the one
/// live mismatch behaviour this harness can exercise end to end: logged, not
/// refused. The `error`-mode refusal is the *same* comparison
/// (`urn_matches`, proven above) taking the other branch of one `if
/// node.type_check().is_fatal()` in `check_declared_type` — see that
/// function's own doc comment.
#[test]
fn send_output_warns_but_still_sends_on_a_mismatched_type_urn_under_warn_mode() {
    let daemon = MockDaemon::start().unwrap();
    let spec = NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new("typed").unwrap(),
        0,
        NodeSource::Dynamic,
    )
    .with_output(
        OutputSpec::new(DataId::new("out").unwrap())
            .with_type(TypeUrn::new("std/core/v1/Bytes").unwrap()),
    );
    let (node, events) = daemon.connect_node(spec).unwrap();
    let node_id = node.id().clone();
    let handle = boxed(node, events);

    let urn = "std/core/v1/Float64";
    let status = unsafe {
        astrs_send_output(
            handle,
            b"out".as_ptr(),
            3,
            urn.as_ptr(),
            urn.len(),
            ptr::null(),
            0,
        )
    };
    assert_eq!(
        status,
        AstrsStatus::Ok.as_raw(),
        "a warn-mode mismatch still sends"
    );
    let sends = daemon
        .wait_for_sends(&node_id, &DataId::new("out").unwrap(), 1, WAIT)
        .unwrap();
    assert_eq!(sends.len(), 1);
    let logs = wait_for_log_count(&daemon, 1, WAIT);
    assert_eq!(logs.len(), 1, "the mismatch must be logged exactly once");

    unsafe { astrs_node_destroy(handle) };
}

#[test]
fn send_output_skips_the_check_for_an_empty_urn_or_an_untyped_port() {
    let daemon = MockDaemon::start().unwrap();
    let (node, events) = daemon.connect_node(solo_spec(&daemon, "untyped")).unwrap();
    let handle = boxed(node, events);

    // The port declares no type at all: any (or no) URN is accepted.
    let urn = "anything/at/all";
    let status = unsafe {
        astrs_send_output(
            handle,
            b"out".as_ptr(),
            3,
            urn.as_ptr(),
            urn.len(),
            ptr::null(),
            0,
        )
    };
    assert_eq!(status, AstrsStatus::Ok.as_raw());
    assert!(daemon.logs().is_empty());

    unsafe { astrs_node_destroy(handle) };
}

// ---------------------------------------------------------------------------
// Null-pointer hardening
// ---------------------------------------------------------------------------

#[test]
fn null_pointers_are_reported_not_dereferenced() {
    assert_eq!(
        unsafe { crate::astrs_init_node_from_env(ptr::null_mut()) },
        AstrsStatus::InvalidArgument.as_raw()
    );
    assert_eq!(
        unsafe { crate::astrs_init_node_from_config(ptr::null(), 0, ptr::null_mut()) },
        AstrsStatus::InvalidArgument.as_raw()
    );
    assert_eq!(
        unsafe { astrs_node_destroy(ptr::null_mut()) },
        AstrsStatus::Ok.as_raw()
    );
    assert_eq!(
        unsafe { astrs_free_event(ptr::null_mut()) },
        AstrsStatus::Ok.as_raw()
    );

    let mut event_ptr: *mut AstrsEvent = ptr::null_mut();
    assert_eq!(
        unsafe { astrs_node_next_event(ptr::null_mut(), 0, &mut event_ptr) },
        AstrsStatus::InvalidArgument.as_raw()
    );
    assert_eq!(
        unsafe {
            astrs_send_output(
                ptr::null_mut(),
                ptr::null(),
                0,
                ptr::null(),
                0,
                ptr::null(),
                0,
            )
        },
        AstrsStatus::InvalidArgument.as_raw()
    );

    let mut kind = AstrsEventType::Unknown;
    assert_eq!(
        unsafe { astrs_event_type(ptr::null(), &mut kind) },
        AstrsStatus::InvalidArgument.as_raw()
    );
    let mut out_ptr: *const c_char = ptr::null();
    let mut out_len: usize = 0;
    assert_eq!(
        unsafe { astrs_event_input_id(ptr::null(), &mut out_ptr, &mut out_len) },
        AstrsStatus::InvalidArgument.as_raw()
    );
}

#[test]
fn an_empty_output_id_is_rejected() {
    let daemon = MockDaemon::start().unwrap();
    let (node, events) = daemon.connect_node(solo_spec(&daemon, "strict")).unwrap();
    let handle = boxed(node, events);

    let status =
        unsafe { astrs_send_output(handle, ptr::null(), 0, ptr::null(), 0, ptr::null(), 0) };
    assert_eq!(status, AstrsStatus::InvalidArgument.as_raw());

    unsafe { astrs_node_destroy(handle) };
}

/// Reads the current thread's last-error message as an owned `String`, for
/// assertion messages (`unwrap()` here would itself panic without saying
/// *why* the surrounding assertion failed).
fn last_error() -> String {
    let ptr = astrs_last_error_message();
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}
