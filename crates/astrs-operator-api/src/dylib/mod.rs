//! The stable `#[repr(C)]` ABI a shared library exports an [`Operator`]
//! through (`dylib` feature; blueprint §9.3, §22 — `astrs-runtime`'s
//! `dylib-operators` feature is the other half, opening the library and
//! driving calls through this same layout).
//!
//! # Why the vtable is only three functions
//!
//! [`Operator`] has five methods (`configure`, `on_start`, `on_event`,
//! `on_stop`, `on_reload`), but [`OperatorVTable`] exports only
//! [`OperatorVTable::new`], [`OperatorVTable::on_event`] and
//! [`OperatorVTable::drop`]. [`OperatorVTable::on_event`] does double duty
//! for all five: the host tags every call with a [`DylibCall`] (this
//! module's own private wire enum, *not* [`crate::OpEvent`] — see below),
//! and the dylib's own generated glue (`export_dylib_operator!`) dispatches
//! on that tag before ever reaching the exported operator's real
//! `on_event`. A wider vtable would just be five thin, near-identical
//! `extern "C"` shims around the same dispatch; one call plus a tag carries
//! the same information with one fifth the ABI surface to keep stable.
//!
//! # Why the payload is a private mirror, not `OpEvent` itself
//!
//! [`crate::OpEvent`] and [`crate::output::OpSend`] are this crate's
//! in-process API — free to gain a field, a variant, or a method whenever
//! ordinary Rust semantic versioning allows. An ABI that crosses a
//! `dlopen` boundary needs something with a *harder* compatibility
//! promise, pinned to [`ASTRS_OPERATOR_ABI_VERSION`] rather than to this
//! crate's own version. [`WireEvent`], [`WireStatus`], [`DylibCall`] and
//! [`DylibReply`] are that mirror: oxicode-encoded (blueprint §7.1's own
//! codec, already what every field type here — [`astrs_wire::DataId`],
//! [`astrs_wire::PortRef`], [`astrs_wire::Metadata`], and friends —
//! implements for the real wire protocol), converted to and from the real
//! types at the boundary, and never `#[non_exhaustive]`: every `match`
//! here is exhaustive *in this crate*, so a future variant added to
//! [`crate::OpEvent`] or [`Status`] is a compile error at the conversion
//! site, not a silently dropped event.
//!
//! # Why a write callback instead of a returned buffer
//!
//! [`OperatorVTable::on_event`] answers through
//! [`WriteCallback`] rather than by returning an allocated `(ptr, len)`
//! pair for the host to free. A pointer one allocator produced and another
//! frees is only sound when both sides share an allocator — true in this
//! workspace's own tests (same `std` global allocator, same process) but
//! not a promise a stable ABI can make about every future caller. The
//! callback sidesteps the question entirely: the dylib copies its answer
//! into host-owned memory (the [`WriteCallback`]'s context pointer, always
//! called synchronously, before [`OperatorVTable::on_event`] returns), and
//! nothing ever crosses the boundary needing to be freed on the other
//! side.
//!
//! # Panics never cross the boundary
//!
//! An `extern "C" fn` that unwinds past its own frame is undefined
//! behaviour (Rust's default `extern "C"` has no unwind ABI; that is what
//! `extern "C-unwind"` is *for*, and this ABI deliberately does not use
//! it). Every function [`crate::export_dylib_operator!`] generates —
//! [`new_operator`], [`on_event_operator`], [`drop_operator`] — therefore
//! wraps its entire body in [`std::panic::catch_unwind`] and reports
//! failure through this ABI's own ordinary channels ([`OperatorVTable::new`]
//! returning a null pointer; [`OperatorVTable::on_event`] answering with a
//! [`DylibReply::Err`]) rather than ever letting the unwind reach the
//! `extern "C"` edge. The loader (`astrs-runtime`'s `dylib-operators`
//! feature) wraps its own calls *into* the vtable the same way, in case a
//! future library implements this ABI by hand rather than through this
//! macro.

use std::collections::BTreeMap;
use std::ffi::c_void;

use astrs_wire::{
    DataId, DurationMs, Metadata, ParamKey, ParamScope, Parameter, PortRef, RouteCloseReason,
    StopCause, WireDecode, WireEncode,
};
use oxicode::{Decode, Encode};

use crate::error::OpResult;
use crate::event::OpEvent;
use crate::operator::{Operator, Status};
use crate::output::OpOutput;

/// The ABI version this build of `astrs-operator-api` speaks.
///
/// Bumped whenever [`OperatorVTable`], [`OperatorDescriptor`],
/// [`DylibCall`] or [`DylibReply`]'s wire shape changes in a way that is
/// not backward compatible. A loader (`astrs-runtime`'s `dylib-operators`
/// feature) rejects a library whose exported
/// [`OperatorDescriptor::abi_version`] does not match this constant before
/// calling anything else in its vtable — the one check that makes every
/// other unsafe call in this module sound.
pub const ASTRS_OPERATOR_ABI_VERSION: u32 = 1;

/// A callback [`OperatorVTable::on_event`] calls (synchronously, at most
/// once, before returning) to hand its encoded [`DylibReply`] back to the
/// host — see this module's docs for why a callback rather than a returned
/// allocation.
///
/// `ctx` is opaque to the callee: whatever the caller of
/// [`OperatorVTable::on_event`] passed as its own `write_ctx` argument,
/// round-tripped unchanged. `ptr`/`len` describe a byte slice valid only
/// for the duration of the call — the callback must copy it if it wants to
/// keep it.
pub type WriteCallback = unsafe extern "C" fn(ctx: *mut c_void, ptr: *const u8, len: usize);

/// The three functions a `dylib:`-sourced operator exports (blueprint
/// §9.3, §22).
///
/// `#[repr(C)]` and built entirely from plain `extern "C"` function
/// pointers, which are themselves `Send + Sync` (an address, nothing
/// more) — this struct needs no `unsafe impl` of its own to cross a thread
/// boundary.
///
/// # The functions, and their safety contracts
///
/// - [`OperatorVTable::new`]: constructs one operator instance, returning
///   an opaque, non-null handle — or null on construction failure (an
///   erroring or panicking `Default::default()`; [`new_operator`] is what
///   [`crate::export_dylib_operator!`] instantiates here).
/// - [`OperatorVTable::on_event`]: the one dispatch entry for all five
///   [`Operator`] methods — see this module's docs for why.
/// - [`OperatorVTable::drop`]: destroys a handle [`OperatorVTable::new`]
///   produced. Called at most once per handle.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct OperatorVTable {
    /// Constructs a fresh operator instance.
    ///
    /// # Safety
    ///
    /// Sound to call any number of times; each call that returns non-null
    /// produces a new, independent handle that must eventually reach
    /// [`OperatorVTable::drop`] exactly once.
    pub new: unsafe extern "C" fn() -> *mut c_void,
    /// Dispatches one [`DylibCall`] to a live handle, answering through
    /// `write`.
    ///
    /// # Safety
    ///
    /// `handle` must be a live, non-null pointer [`OperatorVTable::new`]
    /// produced for this same vtable, not yet passed to
    /// [`OperatorVTable::drop`]. `call_ptr`/`call_len` must describe a
    /// byte slice, valid for the duration of the call, encoding a
    /// [`DylibCall`]. `write` must be safe to call with `write_ctx` and a
    /// byte slice for the duration of this call.
    pub on_event: unsafe extern "C" fn(
        handle: *mut c_void,
        call_ptr: *const u8,
        call_len: usize,
        write: WriteCallback,
        write_ctx: *mut c_void,
    ) -> i32,
    /// Destroys a handle [`OperatorVTable::new`] produced.
    ///
    /// # Safety
    ///
    /// `handle` must be a live, non-null pointer [`OperatorVTable::new`]
    /// produced for this same vtable, not already passed to this
    /// function.
    pub drop: unsafe extern "C" fn(handle: *mut c_void),
}

/// What `astrs_operator_descriptor` ([`crate::export_dylib_operator!`]'s one
/// exported symbol) returns.
///
/// `#[repr(C)]`, returned by value — a plain-old-data struct, so no
/// static-initialization-order question ever arises the way it would for
/// a symbol exporting a `static` instead.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct OperatorDescriptor {
    /// The ABI version this descriptor was built against — compared
    /// against [`ASTRS_OPERATOR_ABI_VERSION`] before anything else in this
    /// descriptor is trusted.
    pub abi_version: u32,
    /// The exported operator's own name (`stringify!` of the type
    /// `export_dylib_operator!` was invoked with, or the macro's explicit
    /// name form) — the loader cross-checks this against the manifest's
    /// `operator:` field, so a `dylib:` entry pointing at the wrong
    /// library fails with a clear name mismatch rather than silently
    /// running whatever the library happens to export.
    ///
    /// Points into the exporting library's own `'static` data; valid for
    /// as long as that library stays loaded.
    pub name_ptr: *const u8,
    /// The byte length of the string `name_ptr` points to (UTF-8, not
    /// necessarily NUL-terminated).
    pub name_len: usize,
    /// This operator's three-function vtable.
    pub vtable: OperatorVTable,
}

/// A wire-encodable mirror of [`OpEvent`] — see this module's docs for why
/// a mirror rather than [`OpEvent`] itself.
///
/// `pub` only because it appears in [`DylibCall::OnEvent`]'s field (and so
/// must be at least as visible as that public enum); callers on both sides
/// of the ABI reach it only through [`DylibCall::on_event`] and
/// [`DylibReply`], never by naming this type directly.
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub enum WireEvent {
    /// Mirrors [`OpEvent::Input`].
    #[oxicode(variant = 0)]
    Input {
        /// See [`OpEvent::Input::id`].
        id: DataId,
        /// See [`OpEvent::Input::source`].
        source: PortRef,
        /// See [`OpEvent::Input::metadata`].
        metadata: Metadata,
        /// See [`OpEvent::Input::payload`].
        payload: Vec<u8>,
    },
    /// Mirrors [`OpEvent::InputClosed`].
    #[oxicode(variant = 1)]
    InputClosed {
        /// See [`OpEvent::InputClosed::id`].
        id: DataId,
        /// See [`OpEvent::InputClosed::source`].
        source: PortRef,
        /// See [`OpEvent::InputClosed::reason`].
        reason: RouteCloseReason,
    },
    /// Mirrors [`OpEvent::Stop`].
    #[oxicode(variant = 2)]
    Stop {
        /// See [`OpEvent::Stop::cause`].
        cause: StopCause,
        /// See [`OpEvent::Stop::grace`].
        grace: Option<DurationMs>,
    },
    /// Mirrors [`OpEvent::Reload`].
    #[oxicode(variant = 3)]
    Reload,
    /// Mirrors [`OpEvent::ParamUpdate`].
    #[oxicode(variant = 4)]
    ParamUpdate {
        /// See [`OpEvent::ParamUpdate::scope`].
        scope: ParamScope,
        /// See [`OpEvent::ParamUpdate::key`].
        key: ParamKey,
        /// See [`OpEvent::ParamUpdate::value`].
        value: Parameter,
    },
}

impl From<&OpEvent> for WireEvent {
    /// Exhaustive with no wildcard arm on purpose: `OpEvent` is
    /// `#[non_exhaustive]` only for crates *outside* this one, so a future
    /// variant added here is a compile error at this exact line, not a
    /// silently dropped event ABI callers never see.
    fn from(event: &OpEvent) -> Self {
        match event {
            OpEvent::Input {
                id,
                source,
                metadata,
                payload,
            } => Self::Input {
                id: id.clone(),
                source: source.clone(),
                metadata: metadata.clone(),
                payload: payload.clone(),
            },
            OpEvent::InputClosed { id, source, reason } => Self::InputClosed {
                id: id.clone(),
                source: source.clone(),
                reason: reason.clone(),
            },
            OpEvent::Stop { cause, grace } => Self::Stop {
                cause: cause.clone(),
                grace: *grace,
            },
            OpEvent::Reload => Self::Reload,
            OpEvent::ParamUpdate { scope, key, value } => Self::ParamUpdate {
                scope: scope.clone(),
                key: key.clone(),
                value: value.clone(),
            },
        }
    }
}

impl From<WireEvent> for OpEvent {
    /// See the reverse `From<&OpEvent> for WireEvent` impl's docs, just
    /// above, on why this is exhaustive with no wildcard.
    fn from(event: WireEvent) -> Self {
        match event {
            WireEvent::Input {
                id,
                source,
                metadata,
                payload,
            } => Self::Input {
                id,
                source,
                metadata,
                payload,
            },
            WireEvent::InputClosed { id, source, reason } => {
                Self::InputClosed { id, source, reason }
            }
            WireEvent::Stop { cause, grace } => Self::Stop { cause, grace },
            WireEvent::Reload => Self::Reload,
            WireEvent::ParamUpdate { scope, key, value } => Self::ParamUpdate { scope, key, value },
        }
    }
}

/// A wire-encodable mirror of [`Status`] — see this module's docs on why
/// a mirror rather than [`Status`] itself.
///
/// `pub` only because it appears in [`DylibReply::Ok`]'s field (and so must
/// be at least as visible as that public enum) and callers outside this
/// crate (`astrs-runtime`'s loader) convert it to a real [`Status`] via
/// this type's own `From` impl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub enum WireStatus {
    /// Mirrors [`Status::Continue`].
    #[oxicode(variant = 0)]
    Continue,
    /// Mirrors [`Status::Finished`].
    #[oxicode(variant = 1)]
    Finished,
}

impl From<Status> for WireStatus {
    /// Exhaustive with no wildcard: see the `From<&OpEvent> for WireEvent`
    /// impl's docs, above — the same reasoning applies to `Status`, also
    /// `#[non_exhaustive]` only for other crates.
    fn from(status: Status) -> Self {
        match status {
            Status::Continue => Self::Continue,
            Status::Finished => Self::Finished,
        }
    }
}

impl From<WireStatus> for Status {
    fn from(status: WireStatus) -> Self {
        match status {
            WireStatus::Continue => Self::Continue,
            WireStatus::Finished => Self::Finished,
        }
    }
}

/// One [`Operator`] method call, tagged and encoded to cross
/// [`OperatorVTable::on_event`] (see this module's docs on why one call
/// answers for all five methods).
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub enum DylibCall {
    /// Mirrors [`Operator::configure`].
    #[oxicode(variant = 0)]
    Configure(BTreeMap<String, Parameter>),
    /// Mirrors [`Operator::on_start`].
    #[oxicode(variant = 1)]
    OnStart,
    /// Mirrors [`Operator::on_event`].
    #[oxicode(variant = 2)]
    OnEvent(WireEvent),
    /// Mirrors [`Operator::on_stop`].
    #[oxicode(variant = 3)]
    OnStop,
    /// Mirrors [`Operator::on_reload`].
    #[oxicode(variant = 4)]
    OnReload,
}

impl DylibCall {
    /// Builds the [`DylibCall::OnEvent`] variant from a real [`OpEvent`] —
    /// the one constructor a caller outside this module ever needs; the
    /// other four variants carry no event-shaped payload to convert.
    #[must_use]
    pub fn on_event(event: &OpEvent) -> Self {
        Self::OnEvent(WireEvent::from(event))
    }
}

/// [`OperatorVTable::on_event`]'s answer to one [`DylibCall`].
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub enum DylibReply {
    /// The call succeeded.
    #[oxicode(variant = 0)]
    Ok {
        /// The [`Status`] an `on_event` call decided, mirrored through
        /// [`WireStatus`]. Meaningless (and always [`WireStatus::Continue`])
        /// for every other [`DylibCall`] variant, which has no `Status` of
        /// its own to report.
        status: WireStatus,
        /// Every send the operator buffered while handling this call, as
        /// `(id, metadata, payload)` triples — [`crate::output::OpSend`]'s
        /// own three parts, not `OpSend` itself (a private type, and this
        /// module does not need its accessors, only its data).
        sends: Vec<(DataId, Metadata, Vec<u8>)>,
    },
    /// The call failed — an [`crate::OpError`], rendered to text at the
    /// boundary (an allocated `String` needs no further marshaling, and
    /// every [`crate::OpError`] already renders through `Display`).
    #[oxicode(variant = 1)]
    Err {
        /// The failure, as [`crate::OpError`]'s `Display` impl rendered it.
        message: String,
    },
}

/// Renders a caught panic payload as a message — every panic raised
/// through `panic!`/`assert!`/`.unwrap()`/`.expect()` carries a `&str` or
/// `String` payload; anything else gets a deliberately non-specific
/// fallback rather than a guess.
fn panic_message(payload: &(dyn std::any::Any + Send + 'static)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "operator panicked with a non-string payload".to_owned()
    }
}

/// Runs one [`DylibCall`] against `operator`, building the [`DylibReply`]
/// [`on_event_operator`] hands back to the host.
///
/// Never panics itself (every `Operator` call it makes is already inside
/// [`on_event_operator`]'s own [`std::panic::catch_unwind`]); a failed
/// call becomes [`DylibReply::Err`], never a propagated `Err` from this
/// function.
fn dispatch<T: Operator>(operator: &mut T, call: DylibCall) -> DylibReply {
    let mut out = OpOutput::new();
    let outcome: OpResult<WireStatus> = match call {
        DylibCall::Configure(config) => operator.configure(&config).map(|()| WireStatus::Continue),
        DylibCall::OnStart => operator.on_start(&mut out).map(|()| WireStatus::Continue),
        DylibCall::OnEvent(wire_event) => operator
            .on_event(&OpEvent::from(wire_event), &mut out)
            .map(WireStatus::from),
        DylibCall::OnStop => operator.on_stop(&mut out).map(|()| WireStatus::Continue),
        DylibCall::OnReload => operator.on_reload(&mut out).map(|()| WireStatus::Continue),
    };
    match outcome {
        Ok(status) => DylibReply::Ok {
            status,
            sends: out
                .drain()
                .into_iter()
                .map(crate::output::OpSend::into_parts)
                .collect(),
        },
        Err(error) => DylibReply::Err {
            message: error.to_string(),
        },
    }
}

/// [`OperatorVTable::new`]'s implementation, generic over the exported
/// operator type — [`crate::export_dylib_operator!`] instantiates this once per
/// macro invocation and stores the monomorphized function pointer in the
/// vtable it returns.
///
/// Catches a panicking or erroring `T::default()` and reports it as a
/// null return rather than letting the unwind reach the `extern "C"` edge
/// (see this module's docs).
pub extern "C" fn new_operator<T: Operator + Default>() -> *mut c_void {
    match std::panic::catch_unwind(T::default) {
        Ok(operator) => Box::into_raw(Box::new(operator)).cast::<c_void>(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// [`OperatorVTable::on_event`]'s implementation, generic over the
/// exported operator type — see [`new_operator`]'s docs on how
/// [`crate::export_dylib_operator!`] uses this.
///
/// Decodes `call_ptr`/`call_len` as a [`DylibCall`], dispatches it to the
/// live `T` (this crate's own private `dispatch` helper), and hands the
/// encoded [`DylibReply`] to `write` — always exactly once per call, whether
/// the operator succeeded, failed, or panicked (a panic is caught and
/// reported as [`DylibReply::Err`], never let past this function's own
/// frame).
///
/// # Safety
///
/// `handle` must be a live, non-null pointer [`new_operator::<T>`]
/// produced, not yet passed to [`drop_operator::<T>`]. `call_ptr`/`call_len`
/// must describe a byte slice valid for the duration of this call. `write`
/// must be safe to call with `write_ctx` and a byte slice.
pub unsafe extern "C" fn on_event_operator<T: Operator>(
    handle: *mut c_void,
    call_ptr: *const u8,
    call_len: usize,
    write: WriteCallback,
    write_ctx: *mut c_void,
) -> i32 {
    if handle.is_null() {
        return -1;
    }
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: see this function's own `# Safety` section — `handle`
        // is a live `T` for the duration of this call, checked non-null
        // just above.
        let operator = unsafe { &mut *handle.cast::<T>() };
        // SAFETY: see this function's own `# Safety` section.
        let bytes = unsafe { std::slice::from_raw_parts(call_ptr, call_len) };
        match DylibCall::decode_exact(bytes) {
            Ok(call) => dispatch(operator, call),
            Err(error) => DylibReply::Err {
                message: format!("malformed call from host: {error}"),
            },
        }
    }));

    let reply = outcome.unwrap_or_else(|payload| DylibReply::Err {
        // `&*payload`, not `&payload`: `payload` is a `Box<dyn Any + Send>`,
        // and `Box<dyn Any + Send>` itself satisfies `Any` (the blanket
        // `impl<T: 'static> Any for T`) — `&payload` would silently
        // unsize-coerce to `&dyn Any` *over the box*, downcasting against
        // the box's own type rather than the panic payload it holds, and
        // every downcast below would then (silently) miss. Dereferencing
        // first reaches the payload itself.
        message: format!(
            "operator panicked inside dylib: {}",
            panic_message(&*payload)
        ),
    });
    let encoded = reply.encode_to_vec().unwrap_or_default();

    let write_outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: see this function's own `# Safety` section — `write`
        // and `write_ctx` are the host's own callback pair, valid for the
        // duration of this call; `encoded` is a byte slice this function
        // owns.
        unsafe { write(write_ctx, encoded.as_ptr(), encoded.len()) };
    }));
    if write_outcome.is_err() { -1 } else { 0 }
}

/// [`OperatorVTable::drop`]'s implementation, generic over the exported
/// operator type — see [`new_operator`]'s docs on how
/// [`crate::export_dylib_operator!`] uses this.
///
/// Catches a panicking `Drop for T` rather than letting the unwind reach
/// the `extern "C"` edge (see this module's docs); there is no channel to
/// report such a failure through (`Drop` has none), so it is silently
/// absorbed rather than propagated.
///
/// # Safety
///
/// `handle` must be a live, non-null pointer [`new_operator::<T>`]
/// produced for this same `T`, not already passed to this function.
pub unsafe extern "C" fn drop_operator<T>(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: see this function's own `# Safety` section.
        drop(unsafe { Box::from_raw(handle.cast::<T>()) });
    }));
}

/// Exports `$ty` as this dylib's one operator, under the ABI this module
/// defines (blueprint §9.3, §22).
///
/// `$ty` must implement [`Operator`] and [`Default`] — exactly
/// [`register_operator!`](crate::register_operator)'s own bound, and for
/// the same reason (`Operator::default()` has no receiver to dispatch on,
/// so default-constructibility is enforced at the export site instead of
/// as a supertrait; see [`Operator`]'s own docs).
///
/// Two forms, matching [`register_operator!`](crate::register_operator):
///
/// - `export_dylib_operator!(MyOp)` — the exported name is
///   `stringify!(MyOp)` (for a qualified path this includes the path,
///   spaces and all — use the explicit-name form for those).
/// - `export_dylib_operator!("my-op" => MyOp)` — an explicit name, for a
///   qualified path or a name the manifest's `operator:` field expects
///   that differs from the Rust type name.
///
/// Call this **at most once** per crate: it exports one fixed, unmangled
/// symbol, `astrs_operator_descriptor`, and a second invocation in the
/// same crate is a duplicate-symbol link error — one dylib exports one
/// operator (blueprint §9.3's `operator:` field is what lets a manifest
/// author give that one exported operator a name of its own choosing).
///
/// ```
/// use astrs_operator_api::{Operator, OpEvent, OpOutput, OpResult, Status, export_dylib_operator};
///
/// #[derive(Default)]
/// struct Echo;
///
/// impl Operator for Echo {
///     fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
///         match event {
///             OpEvent::Input { metadata, payload, .. } => {
///                 out.send_bytes("out", metadata.clone(), payload.clone())?;
///                 Ok(Status::Continue)
///             }
///             OpEvent::Stop { .. } => Ok(Status::Finished),
///             _ => Ok(Status::Continue),
///         }
///     }
/// }
///
/// export_dylib_operator!(Echo);
/// ```
#[macro_export]
macro_rules! export_dylib_operator {
    ($ty:ty) => {
        /// The stable ABI entry point `astrs-runtime`'s `dylib-operators`
        /// feature looks up by name after `dlopen`/`LoadLibrary` — see
        /// [`astrs_operator_api::export_dylib_operator`].
        #[unsafe(no_mangle)]
        pub extern "C" fn astrs_operator_descriptor() -> $crate::dylib::OperatorDescriptor {
            const NAME: &str = ::core::stringify!($ty);
            $crate::dylib::OperatorDescriptor {
                abi_version: $crate::dylib::ASTRS_OPERATOR_ABI_VERSION,
                name_ptr: NAME.as_ptr(),
                name_len: NAME.len(),
                vtable: $crate::dylib::OperatorVTable {
                    new: $crate::dylib::new_operator::<$ty>,
                    on_event: $crate::dylib::on_event_operator::<$ty>,
                    drop: $crate::dylib::drop_operator::<$ty>,
                },
            }
        }
    };
    ($name:expr => $ty:ty) => {
        /// The stable ABI entry point `astrs-runtime`'s `dylib-operators`
        /// feature looks up by name after `dlopen`/`LoadLibrary` — see
        /// [`astrs_operator_api::export_dylib_operator`].
        #[unsafe(no_mangle)]
        pub extern "C" fn astrs_operator_descriptor() -> $crate::dylib::OperatorDescriptor {
            const NAME: &str = $name;
            $crate::dylib::OperatorDescriptor {
                abi_version: $crate::dylib::ASTRS_OPERATOR_ABI_VERSION,
                name_ptr: NAME.as_ptr(),
                name_len: NAME.len(),
                vtable: $crate::dylib::OperatorVTable {
                    new: $crate::dylib::new_operator::<$ty>,
                    on_event: $crate::dylib::on_event_operator::<$ty>,
                    drop: $crate::dylib::drop_operator::<$ty>,
                },
            }
        }
    };
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::operator::Status;
    use astrs_time::HlcTimestamp;
    use astrs_wire::Metadata;

    #[derive(Default)]
    struct Recording {
        configured_with: Option<i64>,
        started: bool,
        stopped: bool,
        reloaded: bool,
    }

    impl Operator for Recording {
        fn configure(&mut self, config: &BTreeMap<String, Parameter>) -> OpResult<()> {
            if let Some(Parameter::Integer(value)) = config.get("threshold") {
                self.configured_with = Some(*value);
            }
            Ok(())
        }

        fn on_start(&mut self, out: &mut OpOutput) -> OpResult<()> {
            self.started = true;
            out.send_bytes("status", meta(), b"started".to_vec())?;
            Ok(())
        }

        fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
            match event {
                OpEvent::Input {
                    metadata, payload, ..
                } => {
                    out.send_bytes("out", metadata.clone(), payload.clone())?;
                    Ok(Status::Continue)
                }
                OpEvent::Stop { .. } => Ok(Status::Finished),
                _ => Ok(Status::Continue),
            }
        }

        fn on_stop(&mut self, out: &mut OpOutput) -> OpResult<()> {
            self.stopped = true;
            out.send_bytes("status", meta(), b"stopped".to_vec())?;
            Ok(())
        }

        fn on_reload(&mut self, _out: &mut OpOutput) -> OpResult<()> {
            self.reloaded = true;
            Ok(())
        }
    }

    #[derive(Default)]
    struct AlwaysPanics;
    impl Operator for AlwaysPanics {
        fn on_event(&mut self, _event: &OpEvent, _out: &mut OpOutput) -> OpResult<Status> {
            panic!("boom");
        }
    }

    fn meta() -> Metadata {
        Metadata::new(HlcTimestamp::EPOCH)
    }

    fn input_event(payload: Vec<u8>) -> OpEvent {
        OpEvent::Input {
            id: DataId::new("frames").unwrap(),
            source: "camera/image".parse().unwrap(),
            metadata: meta(),
            payload,
        }
    }

    unsafe extern "C" fn collecting_write(ctx: *mut c_void, ptr: *const u8, len: usize) {
        let collected = unsafe { &mut *ctx.cast::<Vec<u8>>() };
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        collected.extend_from_slice(bytes);
    }

    /// Drives one [`DylibCall`] through a vtable's `on_event` exactly the
    /// way `astrs-runtime`'s loader would, decoding the answer.
    fn drive(vtable: &OperatorVTable, handle: *mut c_void, call: &DylibCall) -> DylibReply {
        let bytes = call.encode_to_vec().unwrap();
        let mut collected: Vec<u8> = Vec::new();
        let status = unsafe {
            (vtable.on_event)(
                handle,
                bytes.as_ptr(),
                bytes.len(),
                collecting_write,
                std::ptr::from_mut(&mut collected).cast::<c_void>(),
            )
        };
        assert_eq!(status, 0, "on_event must always answer through `write`");
        DylibReply::decode_exact(&collected).unwrap()
    }

    #[test]
    fn wire_event_round_trips_every_op_event_variant() {
        let events = [
            input_event(vec![1, 2, 3]),
            OpEvent::InputClosed {
                id: DataId::new("frames").unwrap(),
                source: "camera/image".parse().unwrap(),
                reason: RouteCloseReason::ProducerFinished,
            },
            OpEvent::Stop {
                cause: StopCause::Requested,
                grace: None,
            },
            OpEvent::Reload,
            OpEvent::ParamUpdate {
                scope: ParamScope::Global,
                key: ParamKey::new("gain").unwrap(),
                value: Parameter::Float(1.5),
            },
        ];
        for event in events {
            let wire = WireEvent::from(&event);
            let back: OpEvent = wire.clone().into();
            assert_eq!(back, event, "{wire:?} did not round-trip");
            // And through the wire codec itself, not just the `From` impls.
            let bytes = wire.encode_to_vec().unwrap();
            let decoded = WireEvent::decode_exact(&bytes).unwrap();
            assert_eq!(decoded, wire);
        }
    }

    #[test]
    fn dylib_call_and_reply_round_trip_through_the_wire_codec() {
        let call = DylibCall::on_event(&input_event(vec![9]));
        let bytes = call.encode_to_vec().unwrap();
        assert_eq!(DylibCall::decode_exact(&bytes).unwrap(), call);

        let reply = DylibReply::Ok {
            status: WireStatus::Finished,
            sends: vec![(DataId::new("out").unwrap(), meta(), vec![1, 2, 3])],
        };
        let bytes = reply.encode_to_vec().unwrap();
        assert_eq!(DylibReply::decode_exact(&bytes).unwrap(), reply);

        let err = DylibReply::Err {
            message: "boom".to_owned(),
        };
        let bytes = err.encode_to_vec().unwrap();
        assert_eq!(DylibReply::decode_exact(&bytes).unwrap(), err);
    }

    #[test]
    fn status_mirrors_both_directions() {
        assert_eq!(WireStatus::from(Status::Continue), WireStatus::Continue);
        assert_eq!(WireStatus::from(Status::Finished), WireStatus::Finished);
        assert_eq!(Status::from(WireStatus::Continue), Status::Continue);
        assert_eq!(Status::from(WireStatus::Finished), Status::Finished);
    }

    export_dylib_operator!(Recording);

    #[test]
    fn the_generated_descriptor_names_the_type_and_declares_the_current_abi_version() {
        let descriptor = astrs_operator_descriptor();
        assert_eq!(descriptor.abi_version, ASTRS_OPERATOR_ABI_VERSION);
        let name = unsafe { std::slice::from_raw_parts(descriptor.name_ptr, descriptor.name_len) };
        assert_eq!(std::str::from_utf8(name).unwrap(), "Recording");
    }

    #[test]
    fn the_generated_vtable_drives_every_hook_through_on_event() {
        let descriptor = astrs_operator_descriptor();
        let vtable = descriptor.vtable;
        let handle = unsafe { (vtable.new)() };
        assert!(!handle.is_null());

        let mut config = BTreeMap::new();
        config.insert("threshold".to_owned(), Parameter::Integer(7));
        match drive(&vtable, handle, &DylibCall::Configure(config)) {
            DylibReply::Ok { .. } => {}
            other => panic!("configure failed: {other:?}"),
        }

        match drive(&vtable, handle, &DylibCall::OnStart) {
            DylibReply::Ok { sends, .. } => {
                assert_eq!(sends.len(), 1);
                assert_eq!(sends[0].0.as_str(), "status");
                assert_eq!(sends[0].2, b"started".to_vec());
            }
            other => panic!("on_start failed: {other:?}"),
        }

        match drive(&vtable, handle, &DylibCall::on_event(&input_event(vec![9]))) {
            DylibReply::Ok { status, sends } => {
                assert_eq!(status, WireStatus::Continue);
                assert_eq!(sends.len(), 1);
                assert_eq!(sends[0].2, vec![9]);
            }
            other => panic!("on_event failed: {other:?}"),
        }

        match drive(&vtable, handle, &DylibCall::OnReload) {
            DylibReply::Ok { .. } => {}
            other => panic!("on_reload failed: {other:?}"),
        }

        let stop_event = OpEvent::Stop {
            cause: StopCause::Requested,
            grace: None,
        };
        match drive(&vtable, handle, &DylibCall::on_event(&stop_event)) {
            DylibReply::Ok { status, .. } => assert_eq!(status, WireStatus::Finished),
            other => panic!("stop event failed: {other:?}"),
        }

        match drive(&vtable, handle, &DylibCall::OnStop) {
            DylibReply::Ok { sends, .. } => {
                assert_eq!(sends[0].2, b"stopped".to_vec());
            }
            other => panic!("on_stop failed: {other:?}"),
        }

        unsafe { (vtable.drop)(handle) };
    }

    #[test]
    fn a_panicking_on_event_becomes_an_err_reply_not_a_process_abort() {
        let handle = new_operator::<AlwaysPanics>();
        assert!(!handle.is_null());
        let vtable = OperatorVTable {
            new: new_operator::<AlwaysPanics>,
            on_event: on_event_operator::<AlwaysPanics>,
            drop: drop_operator::<AlwaysPanics>,
        };
        match drive(&vtable, handle, &DylibCall::on_event(&input_event(vec![1]))) {
            DylibReply::Err { message } => {
                assert!(message.contains("boom"), "message was: {message}")
            }
            other => panic!("expected Err, got {other:?}"),
        }
        // The handle is still valid after a caught panic in `on_event` —
        // only the one call failed, not the operator instance.
        unsafe { (vtable.drop)(handle) };
    }

    #[test]
    fn a_malformed_call_is_reported_rather_than_crashing() {
        let handle = new_operator::<Recording>();
        assert!(!handle.is_null());
        let vtable = OperatorVTable {
            new: new_operator::<Recording>,
            on_event: on_event_operator::<Recording>,
            drop: drop_operator::<Recording>,
        };
        let garbage = b"not a valid DylibCall";
        let mut collected: Vec<u8> = Vec::new();
        let status = unsafe {
            (vtable.on_event)(
                handle,
                garbage.as_ptr(),
                garbage.len(),
                collecting_write,
                std::ptr::from_mut(&mut collected).cast::<c_void>(),
            )
        };
        assert_eq!(status, 0);
        match DylibReply::decode_exact(&collected).unwrap() {
            DylibReply::Err { message } => assert!(message.contains("malformed call")),
            other => panic!("expected Err, got {other:?}"),
        }
        unsafe { (vtable.drop)(handle) };
    }

    #[test]
    fn new_operator_returns_null_on_a_panicking_default() {
        struct PanicsOnDefault;
        impl Default for PanicsOnDefault {
            fn default() -> Self {
                panic!("no default for you");
            }
        }
        impl Operator for PanicsOnDefault {
            fn on_event(&mut self, _event: &OpEvent, _out: &mut OpOutput) -> OpResult<Status> {
                Ok(Status::Continue)
            }
        }
        assert!(new_operator::<PanicsOnDefault>().is_null());
    }

    #[test]
    fn drop_operator_on_null_is_a_no_op() {
        // SAFETY: `drop_operator`'s own null check makes this call sound
        // even though no handle was ever constructed.
        unsafe { drop_operator::<Recording>(std::ptr::null_mut()) };
    }
}
