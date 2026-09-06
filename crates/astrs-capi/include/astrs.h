#ifndef ASTRS_CAPI_H
#define ASTRS_CAPI_H

#include <stddef.h>
#include <stdint.h>

// AstRS C node API
//
// Hand-written (no cbindgen — see the crate's own README for why), and kept
// in sync with crates/astrs-capi/src/*.rs by a unit test that scans the Rust
// source for every `#[no_mangle]` `extern "C"` function and asserts each
// name appears here, and that every `astrs_*` prototype declared here names
// a real one — see crates/astrs-capi/tests/header_consistency.rs. The
// ASTRS_* enum values below are checked the same way, against the Rust
// enums' own `as i32` discriminants: if you change a value on either side
// without changing the other, that test fails the build.
//
// ---------------------------------------------------------------------------
// Threading model
//
// A single AstrsNode* (and any AstrsEvent* it produced) must be accessed by
// at most one thread at a time, with one exception:
//
//   - An AstrsNode* is NOT internally synchronized. Calling
//     astrs_node_next_event / astrs_send_output concurrently with the same
//     node is undefined behavior.
//   - An AstrsEvent* is read-only from the moment astrs_node_next_event
//     returns it, so multiple threads MAY read fields from the same event
//     concurrently, as long as each thread supplies its own out_ptr/out_len
//     destinations.
//   - astrs_node_destroy and astrs_free_event take ownership; the caller
//     must guarantee no other thread is touching the node/event when free is
//     called, and must not use the pointer (or any pointer an accessor
//     returned into it) afterward.
//
// ---------------------------------------------------------------------------
// Error handling
//
// Every function below returns an AstrsStatus (as a plain `int`) except
// three that cannot fail even in principle: astrs_version,
// astrs_max_payload_bytes and astrs_last_error_message, which return their
// value directly. `status < 0` is a complete, forward-compatible C-side
// error test: ASTRS_OK is always 0, and every current or future failure
// code is negative.
//
// A failing call also records a thread-local diagnostic, read back with
// astrs_last_error_message() from the SAME thread immediately afterward —
// it is meaningless (and may be stale, from an earlier call, or NULL) once
// another astrs-capi call has been made on that thread, and it is never
// visible from a different thread.

#ifdef __cplusplus
extern "C" {
#endif

// -----------------------------------------------------------------------
// Status codes
// -----------------------------------------------------------------------

typedef enum AstrsStatus {
    ASTRS_OK = 0,               // The call succeeded.
    ASTRS_INVALID_ARGUMENT = -1,// A null pointer, an out-of-range length, a
                                 // non-UTF-8 string, or a malformed id.
    ASTRS_NOT_CONNECTED = -2,   // Never connected, or the session ended.
    ASTRS_CLOSED = -3,          // The port, stream or node is closed.
    ASTRS_TIMEOUT = -4,         // Timed out before completing.
    ASTRS_UNKNOWN_PORT = -5,    // The id names no declared input/output.
    ASTRS_TYPE_MISMATCH = -6,   // A supplied type URN does not match the
                                 // port's declared type (ASTRS_TYPE_CHECK=error).
    ASTRS_INTERNAL = -7,        // Not one of the above; see the last-error
                                 // message for the underlying diagnostic.
    ASTRS_PANIC = -127,         // A Rust panic was caught at the boundary.
} AstrsStatus;

// Passed as `timeout_ms` to astrs_node_next_event to block until an event
// arrives or the stream ends, with no deadline.
#define ASTRS_TIMEOUT_INFINITE 0xFFFFFFFFu

// -----------------------------------------------------------------------
// Opaque handles
// -----------------------------------------------------------------------

// A live participant in an AstRS dataflow. Never defined in this header —
// only ever reached through a pointer. See astrs_init_node_from_env /
// astrs_init_node_from_config / astrs_node_destroy.
typedef struct AstrsNode AstrsNode;

// One event read from a node's inbox. Never defined in this header. See
// astrs_node_next_event / astrs_free_event and the astrs_event_* accessors.
typedef struct AstrsEvent AstrsEvent;

// Which kind of event an AstrsEvent carries, read with astrs_event_type.
// ASTRS_EVENT_UNKNOWN is never produced by this build; it exists so a
// future AstRS event kind this header predates still maps to *something*,
// and a caller's `switch` should carry a matching `default:`.
typedef enum AstrsEventType {
    ASTRS_EVENT_INPUT = 0,             // A message arrived on an input.
    ASTRS_EVENT_INPUT_CLOSED = 1,      // An input will receive nothing further.
    ASTRS_EVENT_INPUT_RECOVERED = 2,   // A closed input's producer restarted.
    ASTRS_EVENT_STOP = 3,              // Finish up and exit.
    ASTRS_EVENT_RELOAD = 4,            // Reload the node's code / an operator.
    ASTRS_EVENT_ALL_INPUTS_CLOSED = 5, // Every input has closed.
    ASTRS_EVENT_PARAM_UPDATE = 6,      // A watched parameter was written.
    ASTRS_EVENT_PARAM_DELETED = 7,     // A watched parameter was deleted.
    ASTRS_EVENT_NODE_FAILED = 8,       // A peer node failed.
    ASTRS_EVENT_RESTARTED = 9,         // A peer node was restarted.
    ASTRS_EVENT_EXT_DROPPED = 10,      // An owned extension entry was dropped.
    ASTRS_EVENT_ERROR = 11,            // A non-fatal condition worth knowing.
    ASTRS_EVENT_UNKNOWN = 12,          // A future event kind (see above).
} AstrsEventType;

// -----------------------------------------------------------------------
// Version, limits, errors
// -----------------------------------------------------------------------

// This crate's own version (`Cargo.toml`'s `version`), as a null-terminated
// UTF-8 C string. Cannot fail; returns the value directly. The pointer is
// 'static and never needs freeing.
const char *astrs_version(void);

// The largest payload astrs_send_output can ever accept, in bytes. A hard
// ceiling this build was compiled with — the effective limit for one
// connected node may be lower (negotiated with the daemon at connect time);
// astrs_send_output's own ASTRS_INVALID_ARGUMENT is the authoritative
// check. Cannot fail; returns the value directly.
size_t astrs_max_payload_bytes(void);

// This thread's most recent astrs-capi failure message, or NULL if this
// thread has not recorded one. See "Error handling" above for the
// thread-local, until-the-next-call validity contract. Cannot fail (in the
// AstrsStatus sense); returns the value directly.
const char *astrs_last_error_message(void);

// -----------------------------------------------------------------------
// Node lifecycle
// -----------------------------------------------------------------------

// Joins the dataflow using the ASTRS_NODE_CONFIG environment blob the
// daemon set for this process — the entry point an ordinary spawned node
// uses. On ASTRS_OK, *out_node is a freshly allocated handle owed exactly
// one astrs_node_destroy call; on failure, *out_node is NULL.
int astrs_init_node_from_env(AstrsNode **out_node);

// As astrs_init_node_from_env, but from an explicit configuration blob
// (the same format ASTRS_NODE_CONFIG carries) rather than the process
// environment — for a host embedding several nodes with different
// configurations in one process. The (NULL, 0) idiom is never valid here
// (an empty blob cannot configure a node).
int astrs_init_node_from_config(const uint8_t *config_ptr, size_t config_len, AstrsNode **out_node);

// Destroys a node handle: closes every output and ends the session.
// Destroying NULL is a safe no-op, matching free(NULL). `node` must not be
// used again by any thread after this call, including by a thread that had
// it before this call started.
int astrs_node_destroy(AstrsNode *node);

// -----------------------------------------------------------------------
// Events
// -----------------------------------------------------------------------

// Waits for the next event on `node`, for at most `timeout_ms`
// milliseconds (or forever, if timeout_ms == ASTRS_TIMEOUT_INFINITE); 0 is
// therefore a non-blocking poll. On ASTRS_OK, *out_event is a freshly
// allocated event owed exactly one astrs_free_event call. On
// ASTRS_TIMEOUT, *out_event is NULL and the stream is still open — call
// again. On ASTRS_CLOSED, *out_event is NULL and no further event will
// ever arrive.
int astrs_node_next_event(AstrsNode *node, uint32_t timeout_ms, AstrsEvent **out_event);

// Frees an event returned by astrs_node_next_event. Freeing NULL is a safe
// no-op. Every pointer this event's accessors returned becomes dangling
// the instant this call returns.
int astrs_free_event(AstrsEvent *event);

// -----------------------------------------------------------------------
// Event accessors
// -----------------------------------------------------------------------

// Reads out which kind of event this is.
int astrs_event_type(const AstrsEvent *event, AstrsEventType *out_type);

// Reads out the id of the input this event concerns (covers
// ASTRS_EVENT_INPUT, ASTRS_EVENT_INPUT_CLOSED and
// ASTRS_EVENT_INPUT_RECOVERED). Writes (NULL, 0) — ASTRS_OK, not a failure
// — for every other event kind. *out_ptr is UTF-8, NOT null-terminated,
// and valid only until astrs_free_event.
int astrs_event_input_id(const AstrsEvent *event, const char **out_ptr, size_t *out_len);

// Reads out the raw payload bytes of an ASTRS_EVENT_INPUT event: exactly
// what astrs_send_output's data_ptr/data_len published, byte for byte.
// Writes (NULL, 0) — ASTRS_OK, not a failure — for any other event kind,
// and for a payload that was itself empty; the two are indistinguishable
// through this accessor by design. Valid only until astrs_free_event.
int astrs_event_payload(const AstrsEvent *event, const uint8_t **out_ptr, size_t *out_len);

// How many metadata keys ride beside this event's payload. 0 for any event
// kind other than ASTRS_EVENT_INPUT.
int astrs_event_metadata_key_count(const AstrsEvent *event, size_t *out_count);

// Reads out the metadata key at `index`, in a fixed lexicographic order.
// `index` must be < astrs_event_metadata_key_count's result for this same
// event, or this returns ASTRS_INVALID_ARGUMENT. *out_ptr is UTF-8, NOT
// null-terminated, and valid only until astrs_free_event.
int astrs_event_metadata_key_at(const AstrsEvent *event, size_t index, const char **out_ptr, size_t *out_len);

// -----------------------------------------------------------------------
// Sending
// -----------------------------------------------------------------------

// Publishes data_ptr/data_len exactly as given on the output named by
// output_id_ptr/output_id_len (never the (NULL, 0) idiom — an empty output
// id is never valid); the (NULL, 0) idiom on data_ptr/data_len publishes
// an empty payload.
//
// type_urn_ptr/type_urn_len are an optional type URN — (NULL, 0) to omit.
// When given and the port declares a type in the manifest, it is checked
// against the declared type the way a typed Rust `Output<T>` handle checks
// T::URN: under ASTRS_TYPE_CHECK=error a mismatch is refused with
// ASTRS_TYPE_MISMATCH and nothing is sent; under the default `warn` a
// mismatch is logged and the send proceeds; under `off`, or when the port
// declares no type, or when type_urn_len is 0, no comparison is made.
//
// The first call naming a given output id claims that output's one and
// only publishing handle for the life of this node; later calls to the
// same output id reuse it.
int astrs_send_output(
    AstrsNode *node,
    const uint8_t *output_id_ptr, size_t output_id_len,
    const uint8_t *type_urn_ptr, size_t type_urn_len,
    const uint8_t *data_ptr, size_t data_len
);

#ifdef __cplusplus
}
#endif

#endif // ASTRS_CAPI_H
