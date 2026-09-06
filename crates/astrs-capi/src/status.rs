//! [`AstrsStatus`] — the return code every `astrs-capi` entry point uses —
//! plus the thread-local last-error slot and the panic-catching boundary
//! every `extern "C" fn` in this crate runs through.
//!
//! # Three pieces, one contract
//!
//! * **The status code** narrows the failure to a category a C `switch` can
//!   branch on ([`AstrsStatus`]).
//! * **The last-error message** ([`astrs_last_error_message`]) carries the
//!   full diagnostic — `astrs_node_api::NodeError`'s own rich [`Display`],
//!   verbatim — for a caller that wants to log or show it. It is
//!   thread-local: valid until the *same thread* makes another `astrs-capi`
//!   call, and never visible from a different thread. A call that succeeds
//!   does not clear it — check the status first, exactly as a C caller checks
//!   `errno` only after a call reports failure.
//! * **The panic boundary** ([`guard`]) makes both of the above true even
//!   when something beneath this crate panics: unwinding across an `extern
//!   "C"` boundary is undefined behaviour, so every entry point catches it
//!   and reports [`AstrsStatus::Panic`] instead.

use std::cell::RefCell;
use std::ffi::{CString, c_char};
use std::panic::AssertUnwindSafe;
use std::ptr;

use astrs_node_api::NodeError;

/// The status code every `astrs-capi` entry point returns.
///
/// `#[repr(i32)]` so the discriminants are exactly the values a C header
/// declares. [`AstrsStatus::Ok`] is `0`; every failure is negative, which
/// makes `status < 0` a complete and correct C-side error test no matter how
/// many failure codes are added later.
///
/// Three functions in this crate are the deliberate exceptions to "every
/// entry point returns this": [`astrs_version`](crate::astrs_version),
/// [`astrs_last_error_message`] and
/// [`astrs_max_payload_bytes`](crate::astrs_max_payload_bytes) cannot fail
/// even in principle — there is no argument to validate and no I/O to
/// attempt — so each returns its value directly, the way `errno`/`dlerror`
/// and `zlibVersion` do in their own C libraries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(i32)]
#[non_exhaustive]
pub enum AstrsStatus {
    /// The call succeeded.
    Ok = 0,
    /// A pointer was null, a length was out of range, a string was not valid
    /// UTF-8, or an identifier failed its grammar.
    InvalidArgument = -1,
    /// The handle is not attached to a daemon session (never connected, or
    /// the session already ended).
    NotConnected = -2,
    /// The port, stream or node has been closed and cannot be used again.
    Closed = -3,
    /// The operation timed out before it could complete.
    Timeout = -4,
    /// The output or input id does not name a port this node declares.
    UnknownPort = -5,
    /// A supplied type URN does not match the port's declared type, under a
    /// type-check mode that treats the mismatch as fatal (§9.2
    /// `ASTRS_TYPE_CHECK=error`).
    TypeMismatch = -6,
    /// A failure this crate's status taxonomy does not name more precisely.
    /// [`astrs_last_error_message`] carries the underlying diagnostic.
    Internal = -7,
    /// A Rust panic was caught at the FFI boundary and converted rather than
    /// being allowed to unwind into the caller (which would be undefined
    /// behaviour).
    Panic = -127,
}

impl AstrsStatus {
    /// Whether this status reports success.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_capi::AstrsStatus;
    ///
    /// assert!(AstrsStatus::Ok.is_ok());
    /// assert!(!AstrsStatus::Timeout.is_ok());
    /// ```
    #[must_use]
    pub const fn is_ok(self) -> bool {
        matches!(self, Self::Ok)
    }

    /// This status as the plain `int` a C caller sees.
    #[must_use]
    pub const fn as_raw(self) -> i32 {
        self as i32
    }

    /// The reverse of [`AstrsStatus::as_raw`], for a Rust caller (this
    /// crate's own tests and `examples/node_lifecycle.rs`) that received a
    /// raw `int` back from an `extern "C"` call and wants the typed status.
    ///
    /// `None` for a value this build of the enum does not define — which,
    /// since every status this crate itself ever returns is one of the
    /// named variants, means either a future `astrs-capi` extended the set
    /// and this binary predates it, or `raw` did not actually come from an
    /// `astrs-capi` call.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_capi::AstrsStatus;
    ///
    /// assert_eq!(AstrsStatus::from_raw(0), Some(AstrsStatus::Ok));
    /// assert_eq!(AstrsStatus::from_raw(-4), Some(AstrsStatus::Timeout));
    /// assert_eq!(AstrsStatus::from_raw(-99), None);
    /// ```
    #[must_use]
    pub const fn from_raw(raw: i32) -> Option<Self> {
        match raw {
            0 => Some(Self::Ok),
            -1 => Some(Self::InvalidArgument),
            -2 => Some(Self::NotConnected),
            -3 => Some(Self::Closed),
            -4 => Some(Self::Timeout),
            -5 => Some(Self::UnknownPort),
            -6 => Some(Self::TypeMismatch),
            -7 => Some(Self::Internal),
            -127 => Some(Self::Panic),
            _ => None,
        }
    }

    /// Classifies a [`NodeError`] into the coarse taxonomy a C `switch` can
    /// branch on. The full diagnostic is `error.to_string()`, which every
    /// caller of this function also records via [`set_last_error`].
    #[must_use]
    pub(crate) const fn of_node_error(error: &NodeError) -> Self {
        match error {
            NodeError::UnknownOutput { .. } | NodeError::UnknownInput { .. } => Self::UnknownPort,
            NodeError::TypeMismatch { .. } => Self::TypeMismatch,
            NodeError::DaemonGone => Self::NotConnected,
            NodeError::Stopped | NodeError::Orphaned { .. } => Self::Closed,
            NodeError::Timeout { .. } => Self::Timeout,
            NodeError::PayloadTooLarge { .. } | NodeError::Id(_) => Self::InvalidArgument,
            // Everything that can only happen while establishing a session:
            // "never connected" is exactly `NotConnected`'s own documented
            // meaning, so these do not need a dedicated variant.
            NodeError::Config(_)
            | NodeError::MissingEnv { .. }
            | NodeError::BadEnv { .. }
            | NodeError::Connect { .. }
            | NodeError::Handshake(_)
            | NodeError::Registration(_) => Self::NotConnected,
            // Everything else (wire/transport/data/shm/scheduler internals,
            // backpressure, a runtime that could not start, a pattern helper
            // used out of order, a threading misuse this crate's own
            // synchronous calling convention should never trigger) is real
            // but does not fit this taxonomy any more precisely than
            // "internal" — the message carries the detail.
            _ => Self::Internal,
        }
    }
}

impl std::fmt::Display for AstrsStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Ok => "ok",
            Self::InvalidArgument => "invalid argument",
            Self::NotConnected => "not connected to a daemon session",
            Self::Closed => "already closed",
            Self::Timeout => "timed out",
            Self::UnknownPort => "no such input or output on this node",
            Self::TypeMismatch => "type URN does not match the port's declared type",
            Self::Internal => "an internal error occurred; see astrs_last_error_message",
            Self::Panic => "a panic was caught at the FFI boundary",
        };
        f.write_str(text)
    }
}

// ---------------------------------------------------------------------------
// Thread-local last error
// ---------------------------------------------------------------------------

thread_local! {
    /// This thread's most recently recorded diagnostic, if any. Overwritten
    /// on every [`set_last_error`]/[`clear_last_error`] call on this thread;
    /// never touched by any other thread.
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Records `message` as this thread's last error, sanitising any interior NUL
/// (which [`CString::new`] cannot represent) into a space rather than
/// truncating or failing.
pub(crate) fn set_last_error(message: impl Into<String>) {
    let sanitized = message.into().replace('\0', " ");
    let recorded = CString::new(sanitized).unwrap_or_else(|_| {
        c"astrs-capi: internal error constructing the error message".to_owned()
    });
    LAST_ERROR.with(|cell| *cell.borrow_mut() = Some(recorded));
}

/// Clears this thread's last error, so a subsequent
/// [`astrs_last_error_message`] reports none until the next failure.
///
/// No `extern "C" fn` in this crate calls this — a successful call
/// deliberately leaves the previous message in place (see this module's own
/// doc comment) — so it exists only to give this module's own `#[cfg(test)]`
/// tests a known starting state between runs on a thread nextest may reuse.
#[cfg(test)]
pub(crate) fn clear_last_error() {
    LAST_ERROR.with(|cell| *cell.borrow_mut() = None);
}

/// The message behind this thread's most recent `astrs-capi` failure.
///
/// Returns `NULL` when this thread has not yet recorded one. The returned
/// pointer is a thread-local, null-terminated, UTF-8 C string, valid until
/// this same thread's next `astrs-capi` call (which may overwrite or clear
/// it) or until this thread exits. It is never valid to read from a
/// different thread than the one that made the failing call — the record is
/// thread-local by design, matching `errno`.
///
/// A successful call does **not** clear the previous message: check the
/// status code first, and read this only after a call reports failure, the
/// same discipline a C caller already applies to `errno`.
///
/// # Safety
///
/// None beyond the pointer-lifetime contract above: this function itself
/// dereferences nothing the caller passed in.
#[unsafe(no_mangle)]
pub extern "C" fn astrs_last_error_message() -> *const c_char {
    LAST_ERROR.with(|cell| cell.borrow().as_ref().map_or(ptr::null(), |c| c.as_ptr()))
}

// ---------------------------------------------------------------------------
// The panic boundary
// ---------------------------------------------------------------------------

/// Runs `body` and converts any unwind into [`AstrsStatus::Panic`].
///
/// Every `extern "C" fn` in this crate that is not one of the three
/// infallible exceptions is implemented as `guard(|| { ... })`, so a panic
/// anywhere beneath it — in this crate, in `astrs-node-api`, or in a
/// dependency — is caught here rather than unwinding into the C caller,
/// which is undefined behaviour.
///
/// `AssertUnwindSafe` is sound for every closure this crate passes here: each
/// one either captures nothing but `Copy` primitives and raw pointers (which
/// are `UnwindSafe` on their own) or re-derives its Rust references fresh
/// from those raw pointers *inside* the closure body, so nothing torn by a
/// caught panic is ever read again through a stale reference — only through
/// a fresh dereference on the next call, which observes whatever the
/// operation left behind and is therefore no less safe than any other
/// concurrent-mutation scenario this crate's own single-thread-per-handle
/// contract already documents.
pub(crate) fn guard<F: FnOnce() -> AstrsStatus>(body: F) -> std::ffi::c_int {
    match std::panic::catch_unwind(AssertUnwindSafe(body)) {
        Ok(status) => status.as_raw(),
        Err(payload) => {
            set_last_error(panic_message(payload));
            AstrsStatus::Panic.as_raw()
        }
    }
}

/// Extracts a human-readable message from a caught panic's payload.
///
/// Takes `payload` by value and downcasts with the *consuming*
/// `Box<dyn Any>::downcast` rather than borrowing and going through
/// `Any::downcast_ref` on an explicit `&(dyn Any + Send)` reference:
/// `guard`'s `Err` arm already owns `payload` and has nothing left to do
/// with it, so moving costs nothing, and every step here reads the box's own
/// representation directly rather than re-deriving a bare trait-object
/// reference from it.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<&str>() {
        Ok(message) => (*message).to_owned(),
        Err(payload) => match payload.downcast::<String>() {
            Ok(message) => *message,
            Err(_) => "astrs-capi: a panic occurred with a non-string payload".to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    const EVERY_STATUS: &[AstrsStatus] = &[
        AstrsStatus::Ok,
        AstrsStatus::InvalidArgument,
        AstrsStatus::NotConnected,
        AstrsStatus::Closed,
        AstrsStatus::Timeout,
        AstrsStatus::UnknownPort,
        AstrsStatus::TypeMismatch,
        AstrsStatus::Internal,
        AstrsStatus::Panic,
    ];

    #[test]
    fn from_raw_round_trips_every_status_and_rejects_unknown_values() {
        for status in EVERY_STATUS {
            assert_eq!(AstrsStatus::from_raw(status.as_raw()), Some(*status));
        }
        for unknown in [1, -8, -126, -128, i32::MIN, i32::MAX] {
            assert_eq!(AstrsStatus::from_raw(unknown), None, "raw={unknown}");
        }
    }

    #[test]
    fn ok_is_zero_and_every_failure_is_negative() {
        assert_eq!(AstrsStatus::Ok.as_raw(), 0);
        for status in EVERY_STATUS {
            if status.is_ok() {
                assert_eq!(status.as_raw(), 0);
            } else {
                assert!(status.as_raw() < 0, "{status:?} must be negative");
            }
        }
    }

    #[test]
    fn discriminants_are_distinct() {
        // A duplicated discriminant would silently collapse two failure
        // modes into one for every C caller.
        let mut seen: Vec<i32> = EVERY_STATUS.iter().map(|s| s.as_raw()).collect();
        seen.sort_unstable();
        let count = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), count);
    }

    #[test]
    fn every_status_renders_a_non_empty_message() {
        for status in EVERY_STATUS {
            assert!(!status.to_string().is_empty(), "{status:?}");
        }
    }

    #[test]
    fn is_ok_is_true_only_for_ok() {
        assert!(AstrsStatus::Ok.is_ok());
        for status in EVERY_STATUS.iter().filter(|s| **s != AstrsStatus::Ok) {
            assert!(!status.is_ok(), "{status:?}");
        }
    }

    #[test]
    fn node_error_classification_covers_the_documented_cases() {
        assert_eq!(
            AstrsStatus::of_node_error(&NodeError::UnknownOutput {
                output: astrs_node_api::prelude::DataId::new("x").unwrap(),
            }),
            AstrsStatus::UnknownPort
        );
        assert_eq!(
            AstrsStatus::of_node_error(&NodeError::DaemonGone),
            AstrsStatus::NotConnected
        );
        assert_eq!(
            AstrsStatus::of_node_error(&NodeError::Stopped),
            AstrsStatus::Closed
        );
        assert_eq!(
            AstrsStatus::of_node_error(&NodeError::Timeout {
                operation: "test",
                millis: 1
            }),
            AstrsStatus::Timeout
        );
        assert_eq!(
            AstrsStatus::of_node_error(&NodeError::PayloadTooLarge { len: 2, max: 1 }),
            AstrsStatus::InvalidArgument
        );
        assert_eq!(
            AstrsStatus::of_node_error(&NodeError::Backpressure { depth: 1 }),
            AstrsStatus::Internal
        );
    }

    #[test]
    fn last_error_is_thread_local_and_overwritable() {
        // Each `#[test]` may run on a fresh thread under nextest, so start by
        // establishing a known state rather than assuming one.
        clear_last_error();
        assert!(astrs_last_error_message().is_null());

        set_last_error("first failure");
        let first = astrs_last_error_message();
        assert!(!first.is_null());
        let text = unsafe { std::ffi::CStr::from_ptr(first) }.to_str().unwrap();
        assert_eq!(text, "first failure");

        set_last_error("second failure");
        let second = astrs_last_error_message();
        let text = unsafe { std::ffi::CStr::from_ptr(second) }
            .to_str()
            .unwrap();
        assert_eq!(
            text, "second failure",
            "the newer message replaces the older"
        );

        clear_last_error();
        assert!(astrs_last_error_message().is_null());
    }

    #[test]
    fn an_interior_nul_is_sanitised_rather_than_rejected() {
        set_last_error("bad\0message");
        let text = unsafe { std::ffi::CStr::from_ptr(astrs_last_error_message()) }
            .to_str()
            .unwrap();
        assert_eq!(text, "bad message");
        clear_last_error();
    }

    #[test]
    fn guard_reports_ok_and_leaves_no_error_recorded() {
        clear_last_error();
        let raw = guard(|| AstrsStatus::Ok);
        assert_eq!(raw, 0);
        assert!(astrs_last_error_message().is_null());
    }

    #[test]
    fn guard_converts_a_panic_into_the_panic_status_and_records_the_message() {
        clear_last_error();
        // `catch_unwind` still prints the default panic hook's output to
        // stderr; that is expected noise from this one test, not a bug.
        let raw = guard(|| panic!("boom"));
        assert_eq!(raw, AstrsStatus::Panic.as_raw());
        let text = unsafe { std::ffi::CStr::from_ptr(astrs_last_error_message()) }
            .to_str()
            .unwrap();
        assert_eq!(text, "boom");
        clear_last_error();
    }

    #[test]
    fn guard_reports_a_non_string_panic_payload_without_crashing() {
        clear_last_error();
        let raw = guard(|| std::panic::panic_any(42_i32));
        assert_eq!(raw, AstrsStatus::Panic.as_raw());
        assert!(!astrs_last_error_message().is_null());
        clear_last_error();
    }
}
