//! [`AstrsEvent`] and [`AstrsEventType`] — the timeout-bounded event read and
//! the accessors that read one back apart, plus [`astrs_free_event`].
//!
//! # Thread-safety contract
//!
//! An event pointer is read-only from the moment [`astrs_node_next_event`]
//! hands it back, so multiple threads may read fields from the *same* event
//! concurrently — each supplying its own `out_ptr`/`out_len` destinations —
//! exactly as dora's C API documents for the same shape of type.
//! [`astrs_free_event`] takes ownership: the caller must guarantee no other
//! thread is still reading the event when it is called, and every pointer an
//! accessor returned into that event (an input id, a payload, a metadata
//! key) becomes dangling the instant it is freed.

use std::ffi::{c_char, c_int};
use std::time::Duration;

use astrs_node_api::Event;

use crate::node::AstrsNode;
use crate::status::{AstrsStatus, guard, set_last_error};

/// Passed as `timeout_ms` to [`astrs_node_next_event`] to block until an
/// event arrives or the stream ends, with no deadline — the C-side spelling
/// of `EventStream::recv_checked`'s unbounded wait, since `u32` has no
/// natural "infinite" value the way a signed `-1` would give `poll(2)`.
pub const ASTRS_TIMEOUT_INFINITE: u32 = u32::MAX;

/// One event read from a node's inbox.
///
/// Opaque to C, exactly like [`AstrsNode`]: the header forward-declares
/// `AstrsEvent` with no body. Produced by [`astrs_node_next_event`]; every
/// pointer an accessor below returns *into* this event stays valid only
/// until it is passed to [`astrs_free_event`].
pub struct AstrsEvent {
    /// The event itself.
    event: Event,
}

/// Which kind of event an [`AstrsEvent`] carries — mirrors
/// `astrs_node_api::Event`'s variants one for one, in the same order the
/// crate declares them.
///
/// `#[non_exhaustive]`, matching [`crate::AstrsStatus`]: `Event` itself is
/// `#[non_exhaustive]` in `astrs-node-api`, so [`astrs_event_type`]'s mapping
/// already carries an [`AstrsEventType::Unknown`] catch-all for a future
/// variant this build predates, and a C caller's `switch` should carry the
/// same `default:` discipline.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AstrsEventType {
    /// A message arrived on one of the node's inputs
    /// (`astrs_node_api::Event::Input`).
    Input = 0,
    /// An input will receive nothing further
    /// (`astrs_node_api::Event::InputClosed`).
    InputClosed = 1,
    /// A closed input is live again, because its producer restarted
    /// (`astrs_node_api::Event::InputRecovered`).
    InputRecovered = 2,
    /// Finish up and exit; the stream fuses after this
    /// (`astrs_node_api::Event::Stop`).
    Stop = 3,
    /// Reload the node's code, or one operator inside it
    /// (`astrs_node_api::Event::Reload`).
    Reload = 4,
    /// Every input of this node has closed
    /// (`astrs_node_api::Event::AllInputsClosed`).
    AllInputsClosed = 5,
    /// A parameter this node reads was written
    /// (`astrs_node_api::Event::ParamUpdate`).
    ParamUpdate = 6,
    /// A parameter this node reads was deleted
    /// (`astrs_node_api::Event::ParamDeleted`).
    ParamDeleted = 7,
    /// A peer node failed (`astrs_node_api::Event::NodeFailed`).
    NodeFailed = 8,
    /// A peer node was restarted under its restart policy
    /// (`astrs_node_api::Event::Restarted`).
    Restarted = 9,
    /// An extension entry this node owned was dropped
    /// (`astrs_node_api::Event::ExtDropped`).
    ExtDropped = 10,
    /// A non-fatal condition the node should know about
    /// (`astrs_node_api::Event::Error`).
    Error = 11,
    /// A future `astrs_node_api::Event` variant this build of `astrs-capi`
    /// predates. Never produced by any variant documented above; exists so
    /// [`astrs_event_type`] has a total mapping despite `Event` being
    /// `#[non_exhaustive]`.
    Unknown = 12,
}

/// Waits for the next event on this node, for at most `timeout_ms`
/// milliseconds — the blocking bridge over `astrs-node-api`'s async core,
/// built entirely from its own synchronous facade
/// (`EventStream::recv_timeout`/`recv_checked`), which already does the
/// right thing whether or not the calling thread has any tokio runtime of
/// its own (it never does, for a genuine C caller) — this crate owns no
/// runtime of its own.
///
/// Pass [`ASTRS_TIMEOUT_INFINITE`] to block until an event arrives or the
/// stream ends, with no deadline. Any other value, including `0`, waits at
/// most that many milliseconds; `0` is therefore a non-blocking poll.
///
/// On [`AstrsStatus::Ok`], `*out_event` is a freshly allocated event owed
/// exactly one [`astrs_free_event`] call. On [`AstrsStatus::Timeout`],
/// `*out_event` is `NULL` and the stream is still open — call again. On
/// [`AstrsStatus::Closed`], `*out_event` is `NULL` and the stream has fused
/// or the session has ended — no further event will ever arrive, and a node
/// should wind down.
///
/// # Safety
///
/// `node` must be a valid `AstrsNode *`. `out_event` must be a valid,
/// writable `AstrsEvent *` destination.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn astrs_node_next_event(
    node: *mut AstrsNode,
    timeout_ms: u32,
    out_event: *mut *mut AstrsEvent,
) -> c_int {
    guard(|| {
        if node.is_null() || out_event.is_null() {
            return AstrsStatus::InvalidArgument;
        }
        unsafe { *out_event = std::ptr::null_mut() };
        let node = unsafe { &mut *node };

        let outcome = if timeout_ms == ASTRS_TIMEOUT_INFINITE {
            node.events.recv_checked()
        } else {
            node.events
                .recv_timeout(Duration::from_millis(u64::from(timeout_ms)))
        };

        match outcome {
            Ok(Some(event)) => {
                let boxed = Box::new(AstrsEvent { event });
                unsafe { *out_event = Box::into_raw(boxed) };
                AstrsStatus::Ok
            }
            Ok(None) if node.events.is_fused() || node.events.session_ended() => {
                AstrsStatus::Closed
            }
            Ok(None) => AstrsStatus::Timeout,
            Err(error) => {
                let status = AstrsStatus::of_node_error(&error);
                set_last_error(error.to_string());
                status
            }
        }
    })
}

/// Frees an event returned by [`astrs_node_next_event`].
///
/// Freeing `NULL` is a safe no-op, matching `free(NULL)`. Every pointer this
/// event's accessors returned (an input id, a payload, a metadata key) is
/// dangling from the instant this call returns.
///
/// # Safety
///
/// `event` must be `NULL` or a still-valid pointer [`astrs_node_next_event`]
/// produced, not already freed, with no other thread concurrently reading
/// from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn astrs_free_event(event: *mut AstrsEvent) -> c_int {
    guard(|| {
        if !event.is_null() {
            drop(unsafe { Box::from_raw(event) });
        }
        AstrsStatus::Ok
    })
}

/// Reads out which kind of event this is.
///
/// # Safety
///
/// `event` must be a valid `const AstrsEvent *`. `out_type` must be a valid,
/// writable `AstrsEventType *` destination.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn astrs_event_type(
    event: *const AstrsEvent,
    out_type: *mut AstrsEventType,
) -> c_int {
    guard(|| {
        if event.is_null() || out_type.is_null() {
            return AstrsStatus::InvalidArgument;
        }
        let event = unsafe { &*event };
        let kind = match &event.event {
            Event::Input { .. } => AstrsEventType::Input,
            Event::InputClosed { .. } => AstrsEventType::InputClosed,
            Event::InputRecovered { .. } => AstrsEventType::InputRecovered,
            Event::Stop(_) => AstrsEventType::Stop,
            Event::Reload { .. } => AstrsEventType::Reload,
            Event::AllInputsClosed => AstrsEventType::AllInputsClosed,
            Event::ParamUpdate { .. } => AstrsEventType::ParamUpdate,
            Event::ParamDeleted { .. } => AstrsEventType::ParamDeleted,
            Event::NodeFailed { .. } => AstrsEventType::NodeFailed,
            Event::Restarted { .. } => AstrsEventType::Restarted,
            Event::ExtDropped { .. } => AstrsEventType::ExtDropped,
            Event::Error(_) => AstrsEventType::Error,
            _ => AstrsEventType::Unknown,
        };
        unsafe { *out_type = kind };
        AstrsStatus::Ok
    })
}

/// Reads out the id of the input this event concerns.
///
/// Covers `Input`, `InputClosed` and `InputRecovered` — every event kind
/// `astrs_node_api::Event::input()` reports one for. Writes `(NULL, 0)` for
/// every other event kind, which is `Ok`, not a failure: most event kinds
/// simply have no input id.
///
/// `*out_ptr` is not null-terminated; its length is `*out_len`, and it is
/// guaranteed valid UTF-8. It points directly into the event's own memory
/// and must not be read after [`astrs_free_event`].
///
/// # Safety
///
/// `event` must be a valid `const AstrsEvent *`. `out_ptr`/`out_len` must be
/// valid, writable destinations.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn astrs_event_input_id(
    event: *const AstrsEvent,
    out_ptr: *mut *const c_char,
    out_len: *mut usize,
) -> c_int {
    guard(|| {
        if event.is_null() || out_ptr.is_null() || out_len.is_null() {
            return AstrsStatus::InvalidArgument;
        }
        let event = unsafe { &*event };
        match event.event.input() {
            Some(id) => unsafe { write_text(id.as_str(), out_ptr, out_len) },
            None => unsafe { write_empty(out_ptr, out_len) },
        }
        AstrsStatus::Ok
    })
}

/// Reads out the raw payload bytes of an `Input` event.
///
/// Writes exactly the bytes [`crate::astrs_send_output`]'s `data_ptr`/
/// `data_len` published, unmodified. Writes `(NULL, 0)` — `Ok`, not a
/// failure — for every event kind that is not `Input`, and for an `Input`
/// whose payload is genuinely empty; the two are indistinguishable through
/// this accessor by design, the same `(NULL, 0)` idiom
/// [`crate::astrs_send_output`] itself accepts for an empty send.
///
/// The pointer is valid only until [`astrs_free_event`].
///
/// # Safety
///
/// `event` must be a valid `const AstrsEvent *`. `out_ptr`/`out_len` must be
/// valid, writable destinations.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn astrs_event_payload(
    event: *const AstrsEvent,
    out_ptr: *mut *const u8,
    out_len: *mut usize,
) -> c_int {
    guard(|| {
        if event.is_null() || out_ptr.is_null() || out_len.is_null() {
            return AstrsStatus::InvalidArgument;
        }
        let event = unsafe { &*event };
        match event.event.payload() {
            Some(payload) if !payload.is_empty() => {
                let bytes = payload.bytes();
                unsafe {
                    *out_ptr = bytes.as_ptr();
                    *out_len = bytes.len();
                }
            }
            _ => unsafe {
                *out_ptr = std::ptr::null();
                *out_len = 0;
            },
        }
        AstrsStatus::Ok
    })
}

/// How many metadata keys ride beside this event's payload.
///
/// `0` for any event kind that carries no metadata at all (everything but
/// `Input`). AstRS-internal keys (the `_`-prefixed plumbing, e.g. the schema
/// hash) are never counted here — `Event::Input`'s metadata already has them
/// stripped before a node ever sees it (blueprint §6.1).
///
/// # Safety
///
/// `event` must be a valid `const AstrsEvent *`. `out_count` must be a valid,
/// writable destination.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn astrs_event_metadata_key_count(
    event: *const AstrsEvent,
    out_count: *mut usize,
) -> c_int {
    guard(|| {
        if event.is_null() || out_count.is_null() {
            return AstrsStatus::InvalidArgument;
        }
        let event = unsafe { &*event };
        let count = event.event.metadata().map_or(0, |meta| meta.len());
        unsafe { *out_count = count };
        AstrsStatus::Ok
    })
}

/// Reads out the metadata key at `index`, in a fixed, deterministic order
/// (lexicographic — the underlying map is key-ordered) stable for as long as
/// this event exists.
///
/// `index` must be `< ` whatever [`astrs_event_metadata_key_count`] reported
/// for this same event; out of range is reported as
/// [`AstrsStatus::InvalidArgument`] rather than silently as `(NULL, 0)`,
/// since — unlike an input id or a payload — a caller can always know the
/// valid range in advance.
///
/// `*out_ptr` is not null-terminated; its length is `*out_len`, guaranteed
/// valid UTF-8, and valid only until [`astrs_free_event`].
///
/// # Safety
///
/// `event` must be a valid `const AstrsEvent *`. `out_ptr`/`out_len` must be
/// valid, writable destinations.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn astrs_event_metadata_key_at(
    event: *const AstrsEvent,
    index: usize,
    out_ptr: *mut *const c_char,
    out_len: *mut usize,
) -> c_int {
    guard(|| {
        if event.is_null() || out_ptr.is_null() || out_len.is_null() {
            return AstrsStatus::InvalidArgument;
        }
        let event = unsafe { &*event };
        let key = event
            .event
            .metadata()
            .and_then(|meta| meta.keys().nth(index));
        match key {
            Some(key) => {
                unsafe { write_text(key.as_str(), out_ptr, out_len) };
                AstrsStatus::Ok
            }
            None => {
                unsafe { write_empty(out_ptr, out_len) };
                set_last_error("astrs_event_metadata_key_at: index out of range");
                AstrsStatus::InvalidArgument
            }
        }
    })
}

/// Writes `text`'s address and length through the two out-parameters every
/// text accessor above shares.
///
/// # Safety
///
/// `out_ptr`/`out_len` must be valid, writable destinations — every caller
/// above has already checked this before calling.
unsafe fn write_text(text: &str, out_ptr: *mut *const c_char, out_len: *mut usize) {
    unsafe {
        *out_ptr = text.as_ptr().cast::<c_char>();
        *out_len = text.len();
    }
}

/// Writes the `(NULL, 0)` idiom through the two out-parameters every text
/// accessor above shares.
///
/// # Safety
///
/// As [`write_text`].
unsafe fn write_empty(out_ptr: *mut *const c_char, out_len: *mut usize) {
    unsafe {
        *out_ptr = std::ptr::null();
        *out_len = 0;
    }
}
