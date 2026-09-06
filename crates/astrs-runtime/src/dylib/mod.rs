//! Loading operators from a platform shared library (`dylib-operators`).
//!
//! Gated behind the `dylib-operators` feature, which is off by default: a
//! runtime hosting only compiled-in `register_operator!` types — the flagship
//! path of blueprint §9.3 — must not pay for a dynamic loader it never calls.
//!
//! # What this module owns
//!
//! A manifest operator entry whose source kind is
//! [`dylib`](astrs_manifest::OperatorConfig::dylib) names a `.so`/`.dylib`/
//! `.dll` on disk instead of a name in the compiled-in
//! [`astrs_operator_api::OperatorRegistry`]. Turning that path into a live
//! operator is this module's job: resolving the path relative to the
//! dataflow file ([`resolve_dylib_path`]), opening the library, resolving
//! its `astrs_operator_descriptor` entry point, checking the ABI version
//! and operator name it advertises (`DylibSource::load`), and bridging
//! every instance it constructs into [`astrs_operator_api::Operator`]
//! (`DylibOperator`) so this crate's own `worker::operator_loop` can drive
//! it exactly like a compiled-in operator — it never learns the
//! difference. (`DylibSource`, `DylibOperator` and `operator_loop` are
//! plain code spans rather than intra-doc links: all three are private to
//! this crate, so a link to them from this module's own public-facing docs
//! would be broken in every build that does not pass
//! `--document-private-items`.)
//!
//! `DylibSource` keeps the loaded [`libloading::Library`] alive (via
//! [`std::sync::Arc`]) for at least as long as every `DylibOperator` it
//! produced: unloading a library out from under a live handle would leave
//! that handle's vtable pointing at unmapped memory, a use-after-free
//! `dlclose` cannot warn about.
//!
//! # The ABI itself
//!
//! Defined in `astrs-operator-api`'s own `dylib` module (this crate's
//! `dylib-operators` feature turns on that crate's `dylib` feature to reach
//! it) — see that module's docs for the vtable shape, why it is only three
//! functions wide, and why the payload crossing it is a private wire mirror
//! rather than [`astrs_operator_api::OpEvent`] itself. This module is the
//! *loader* half; `export_dylib_operator!` is the *exporter* half a
//! `dylib:`-sourced operator crate uses.
//!
//! # Pure Rust
//!
//! `libloading` is a safe wrapper over the platform's own `dlopen`/
//! `LoadLibrary` — FFI declarations to a platform service, which blueprint
//! §18.1 explicitly permits. No C is compiled: the crate has no build script
//! and no `cc` dependency, and the `*-sys` sweep is unaffected.
//!
//! # Panics never cross the boundary
//!
//! Every call this module makes into a loaded library's vtable is wrapped
//! in [`std::panic::catch_unwind`] — defense in depth alongside the
//! catching `export_dylib_operator!`'s own generated glue already does on
//! the other side, in case a future `dylib:` library implements this ABI
//! by hand rather than through that macro. A panicking operator yields an
//! [`astrs_operator_api::OpError`], exactly like a panicking compiled-in
//! operator does — never a crash of the host process.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use astrs_operator_api::dylib::{
    ASTRS_OPERATOR_ABI_VERSION, DylibCall, DylibReply, OperatorDescriptor, OperatorVTable,
};
use astrs_operator_api::{OpError, OpEvent, OpOutput, OpResult, Operator, Status};
use astrs_wire::{DataId, Metadata, Parameter, WireDecode, WireEncode};
use libloading::Library;

/// The exported symbol every `dylib:`-sourced library must define —
/// `export_dylib_operator!`'s one generated item.
const DESCRIPTOR_SYMBOL: &[u8] = b"astrs_operator_descriptor\0";

/// Why a manifest `dylib:` entry could not be loaded.
///
/// `#[non_exhaustive]`: the append-only evolution rule (blueprint §3.4)
/// applies here too.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DylibError {
    /// [`libloading::Library::new`] failed — the file does not exist, is
    /// not a shared library for this platform, or could not be mapped.
    #[error("could not open shared library at {path}: {source}")]
    Open {
        /// The path that was opened, after [`resolve_dylib_path`].
        path: PathBuf,
        /// What `libloading` reported.
        #[source]
        source: libloading::Error,
    },

    /// The library has no `astrs_operator_descriptor` export —
    /// [`export_dylib_operator!`](astrs_operator_api::export_dylib_operator)
    /// was never invoked in it, or it was built against a version of
    /// `astrs-operator-api` old enough not to have this ABI at all.
    #[error(
        "shared library at {path} has no `astrs_operator_descriptor` export \
         (was `export_dylib_operator!` invoked in it?): {source}"
    )]
    MissingDescriptor {
        /// The path that was opened.
        path: PathBuf,
        /// What `libloading` reported resolving the symbol.
        #[source]
        source: libloading::Error,
    },

    /// The descriptor's name is not valid UTF-8 — unreachable for any
    /// library built by `export_dylib_operator!` (its name always comes
    /// from a Rust string), kept because this loader never trusts a
    /// foreign library's byte contents by construction alone.
    #[error("shared library at {path} exported an operator name that is not valid UTF-8")]
    InvalidName {
        /// The path that was opened.
        path: PathBuf,
    },

    /// Calling the library's `astrs_operator_descriptor` export panicked —
    /// caught here rather than let past the loader's own frame, exactly
    /// the discipline every later vtable call in this module also
    /// applies (see the module's own docs).
    #[error("shared library at {path}'s `astrs_operator_descriptor` export panicked")]
    DescriptorPanicked {
        /// The path that was opened.
        path: PathBuf,
    },

    /// The library's [`OperatorDescriptor::abi_version`] does not match
    /// [`ASTRS_OPERATOR_ABI_VERSION`] — built against an incompatible
    /// version of this ABI.
    #[error(
        "shared library at {path} declares ABI version {found}, this runtime expects \
         {expected}"
    )]
    AbiVersionMismatch {
        /// The path that was opened.
        path: PathBuf,
        /// The version the library declared.
        found: u32,
        /// The version this runtime build expects.
        expected: u32,
    },

    /// The library's exported operator name does not match the manifest
    /// entry's `operator:` field — the `dylib:` path likely names the
    /// wrong library, not merely a wrong ABI version.
    #[error(
        "shared library at {path} exports operator {found:?}, but the manifest names \
         {expected:?}"
    )]
    OperatorNameMismatch {
        /// The path that was opened.
        path: PathBuf,
        /// The name the library actually exports.
        found: String,
        /// The name the manifest's `operator:` field declared.
        expected: String,
    },
}

/// Resolves a manifest `operators[].dylib` path.
///
/// An absolute `declared` path is used as-is. A relative one resolves
/// against `dataflow_dir` (blueprint §22: "the runtime resolves the path
/// relative to the dataflow file") — or, when the caller has no dataflow
/// file path to offer (`RuntimeConfig`'s own `dataflow_dir` field is
/// `None`), against the process's own current directory, matching how a
/// relative path on a shell command line would resolve.
///
/// # Examples
///
/// ```
/// use astrs_runtime::dylib::resolve_dylib_path;
/// use std::path::Path;
///
/// assert_eq!(
///     resolve_dylib_path(Some(Path::new("/graphs")), "./libyolo.so"),
///     Path::new("/graphs/./libyolo.so")
/// );
/// assert_eq!(
///     resolve_dylib_path(Some(Path::new("/graphs")), "/opt/libyolo.so"),
///     Path::new("/opt/libyolo.so"),
///     "an absolute declared path is never rebased"
/// );
/// ```
#[must_use]
pub fn resolve_dylib_path(dataflow_dir: Option<&Path>, declared: &str) -> PathBuf {
    let declared_path = Path::new(declared);
    if declared_path.is_absolute() {
        return declared_path.to_path_buf();
    }
    match dataflow_dir {
        Some(dir) => dir.join(declared_path),
        None => declared_path.to_path_buf(),
    }
}

/// A loaded `dylib:` operator library — [`DylibSource::load`] opens it and
/// validates its descriptor exactly once; [`DylibSource::build`] then
/// constructs as many operator instances from it as the host's restart
/// policy needs, cheaply (just the vtable's own `new` call — no re-opening
/// the library).
///
/// `Debug` is hand-written rather than derived: `OperatorVTable` is plain
/// function pointers (address-only, not meaningfully `Debug`) and
/// `Arc<Library>` has no `Debug` of its own — this impl reports only what a
/// caller debugging "why didn't my dylib operator load" actually wants,
/// mirroring `RuntimeConfig`'s own hand-written `Debug` for the same
/// reason.
pub(crate) struct DylibSource {
    vtable: OperatorVTable,
    /// Kept only to hold the mapping open for as long as any
    /// [`DylibOperator`] built from this source might still be calling
    /// into `vtable`; never read directly again after [`DylibSource::load`]
    /// resolves the descriptor.
    library: Arc<Library>,
}

// SAFETY: `DylibSource` exposes nothing but `vtable` (plain `extern "C"`
// function pointers — themselves `Send + Sync`, see
// `astrs_operator_api::dylib::OperatorVTable`'s own docs) and an
// `Arc<Library>`. `dlopen`/`dlsym` (and their Windows equivalents, which
// `libloading` wraps) are documented thread-safe operations, and a loaded
// library's mapped code and read-only data do not change once mapped —
// sharing a `&DylibSource` (or sending an owned one) across the operator
// threads `RuntimeHost::run` spawns is exactly the same sharing
// `&OperatorRegistry` already gets.
unsafe impl Send for DylibSource {}
// SAFETY: see the `Send` impl above; nothing here has interior mutability
// that would make concurrent shared access unsound.
unsafe impl Sync for DylibSource {}

impl core::fmt::Debug for DylibSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Every field is either a bag of raw function-pointer addresses
        // (`vtable`) or an opaque loaded mapping (`library`) — neither has
        // anything a caller debugging a failed load would act on, so this
        // reports only that a source exists, not its contents.
        f.debug_struct("DylibSource").finish_non_exhaustive()
    }
}

impl DylibSource {
    /// Opens `path`, resolves and validates its `astrs_operator_descriptor`
    /// export, and confirms it names `expected_operator`.
    ///
    /// # Errors
    ///
    /// See [`DylibError`]'s variants.
    pub(crate) fn load(path: &Path, expected_operator: &str) -> Result<Self, DylibError> {
        // SAFETY: loading arbitrary code executes that library's own
        // initializers — the inherent, unavoidable nature of dynamic
        // loading (this module's own docs, and blueprint §22's whole
        // premise). The path comes from a manifest `dylib:` entry the
        // operator of this dataflow already chose to run.
        let library = unsafe { Library::new(path) }.map_err(|source| DylibError::Open {
            path: path.to_path_buf(),
            source,
        })?;

        // SAFETY: `DESCRIPTOR_SYMBOL` and its function signature are this
        // ABI's own fixed contract (`export_dylib_operator!`'s generated
        // `astrs_operator_descriptor` is the only producer); a library
        // that exports a same-named symbol with a different signature has
        // violated the ABI it claims to implement, which is exactly the
        // class of mismatch the version and name checks right below exist
        // to catch as early and as clearly as possible.
        let descriptor_fn = unsafe {
            library.get::<unsafe extern "C" fn() -> OperatorDescriptor>(DESCRIPTOR_SYMBOL)
        }
        .map_err(|source| DylibError::MissingDescriptor {
            path: path.to_path_buf(),
            source,
        })?;
        // SAFETY: `descriptor_fn`'s only precondition is being callable at
        // all — the vtable it returns is a plain-old-data value with no
        // aliasing of its own — and the loader's own docs, plus
        // `catch_unwind` below, cover a descriptor function that panics.
        let descriptor = std::panic::catch_unwind(|| unsafe { descriptor_fn() }).map_err(|_| {
            DylibError::DescriptorPanicked {
                path: path.to_path_buf(),
            }
        })?;

        if descriptor.abi_version != ASTRS_OPERATOR_ABI_VERSION {
            return Err(DylibError::AbiVersionMismatch {
                path: path.to_path_buf(),
                found: descriptor.abi_version,
                expected: ASTRS_OPERATOR_ABI_VERSION,
            });
        }

        // SAFETY: `name_ptr`/`name_len` describe bytes the descriptor's own
        // producer built from a `'static` Rust string baked into the
        // library's own data section (`export_dylib_operator!`'s own
        // docs); `library` — and so that mapping — is still alive here.
        let name_bytes =
            unsafe { std::slice::from_raw_parts(descriptor.name_ptr, descriptor.name_len) };
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| DylibError::InvalidName {
                path: path.to_path_buf(),
            })?
            .to_owned();
        if name != expected_operator {
            return Err(DylibError::OperatorNameMismatch {
                path: path.to_path_buf(),
                found: name,
                expected: expected_operator.to_owned(),
            });
        }

        Ok(Self {
            vtable: descriptor.vtable,
            library: Arc::new(library),
        })
    }

    /// Constructs one fresh operator instance from this loaded library —
    /// [`crate::worker::operator_loop`]'s per-incarnation construction
    /// step, exactly matching [`astrs_operator_api::OperatorRegistry::build`]'s
    /// own `OpResult<Box<dyn Operator>>` shape so both sources plug into
    /// the same restart loop unmodified.
    ///
    /// # Errors
    ///
    /// [`OpError::Failed`] if the library's constructor panicked, failed,
    /// or returned a null handle.
    pub(crate) fn build(&self) -> OpResult<Box<dyn Operator>> {
        let new_fn = self.vtable.new;
        // SAFETY: `new_fn` is this ABI's own constructor entry, taking no
        // arguments — always sound to call. `catch_unwind` here is
        // defense in depth: `export_dylib_operator!`'s own
        // `new_operator` already catches internally and reports failure
        // as a null return, but a hand-written (non-macro) library could
        // fail to.
        let handle =
            std::panic::catch_unwind(|| unsafe { new_fn() }).unwrap_or(std::ptr::null_mut());
        if handle.is_null() {
            return Err(OpError::failed(
                "dylib operator constructor returned null (construction panicked or failed)",
            ));
        }
        Ok(Box::new(DylibOperator {
            handle,
            vtable: self.vtable,
            _library: Arc::clone(&self.library),
        }))
    }
}

/// A single dylib-hosted operator instance, bridged into
/// [`astrs_operator_api::Operator`] so [`crate::worker::operator_loop`]
/// drives it exactly like any compiled-in operator.
struct DylibOperator {
    handle: *mut c_void,
    vtable: OperatorVTable,
    /// Keeps the library mapped for at least as long as `handle` is live —
    /// see [`DylibSource`]'s own docs.
    _library: Arc<Library>,
}

// SAFETY: `handle` is an opaque pointer `vtable.new` produced. The type it
// actually points to, on the exporting side, is required to implement
// `astrs_operator_api::Operator: Send` (`export_dylib_operator!`'s own
// bound), so moving this wrapper — and so `handle` — across a thread
// boundary is exactly as sound as moving that `Send` value would be.
// `vtable` is plain function pointers (`Send` on their own), and
// `Arc<Library>` is `Send` per `DylibSource`'s own `unsafe impl` above.
unsafe impl Send for DylibOperator {}

impl DylibOperator {
    /// Encodes `call`, drives it through [`OperatorVTable::on_event`], and
    /// decodes the answer — the one place this type touches the vtable's
    /// `on_event` entry; every [`Operator`] method below is a thin
    /// wrapper around this.
    fn call(&mut self, call: &DylibCall) -> OpResult<DylibReply> {
        let bytes = call
            .encode_to_vec()
            .map_err(|source| OpError::failed(format!("failed to encode dylib call: {source}")))?;
        let mut collected: Vec<u8> = Vec::new();
        let on_event_fn = self.vtable.on_event;
        let handle = self.handle;
        let ctx = std::ptr::from_mut(&mut collected).cast::<c_void>();
        // SAFETY: `handle` is this instance's own live handle, not yet
        // dropped; `bytes` is a byte slice valid for the duration of this
        // call; `host_write_callback`/`ctx` are this call's own callback
        // pair, valid for the duration of this call. `catch_unwind` is
        // defense in depth against a hand-written (non-macro) library —
        // `export_dylib_operator!`'s generated `on_event_operator` already
        // catches internally on the other side.
        let status = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            on_event_fn(
                handle,
                bytes.as_ptr(),
                bytes.len(),
                host_write_callback,
                ctx,
            )
        }))
        .unwrap_or(-1);
        if status != 0 {
            return Err(OpError::failed(
                "dylib operator's on_event did not answer through its write callback \
                 (it panicked at the FFI boundary)",
            ));
        }
        DylibReply::decode_exact(&collected).map_err(|source| {
            OpError::failed(format!("malformed reply from dylib operator: {source}"))
        })
    }

    /// Runs one [`DylibCall`] whose reply carries no [`Status`] — every
    /// [`Operator`] method but `on_event` itself.
    fn dispatch_void(&mut self, call: DylibCall, out: &mut OpOutput) -> OpResult<()> {
        match self.call(&call)? {
            DylibReply::Ok { sends, .. } => push_sends(out, sends),
            DylibReply::Err { message } => Err(OpError::failed(message)),
        }
    }
}

/// Replays every send a dylib call answered with onto `out`, using
/// [`OpOutput::send_bytes`] — the already-validated
/// [`astrs_wire::DataId`] each send carries came from a real `DataId` on
/// the dylib's own side, so re-parsing it here should never fail in
/// practice, but the possibility is still propagated rather than assumed
/// away.
fn push_sends(out: &mut OpOutput, sends: Vec<(DataId, Metadata, Vec<u8>)>) -> OpResult<()> {
    for (id, metadata, payload) in sends {
        out.send_bytes(id.as_str(), metadata, payload)?;
    }
    Ok(())
}

/// The [`WriteCallback`] every [`DylibOperator::call`] passes to
/// [`OperatorVTable::on_event`] — copies the answered bytes into the
/// [`Vec<u8>`] `ctx` points at, synchronously, before returning.
///
/// # Safety
///
/// Called only from within [`DylibOperator::call`], with `ctx` set to the
/// `*mut Vec<u8>` it constructed and `ptr`/`len` describing a byte slice
/// valid for the duration of the call — the contract
/// [`astrs_operator_api::dylib::WriteCallback`] documents.
unsafe extern "C" fn host_write_callback(ctx: *mut c_void, ptr: *const u8, len: usize) {
    // SAFETY: see this function's own `# Safety` section.
    let collected = unsafe { &mut *ctx.cast::<Vec<u8>>() };
    // SAFETY: see this function's own `# Safety` section.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    collected.extend_from_slice(bytes);
}

impl Operator for DylibOperator {
    fn configure(&mut self, config: &BTreeMap<String, Parameter>) -> OpResult<()> {
        match self.call(&DylibCall::Configure(config.clone()))? {
            DylibReply::Ok { .. } => Ok(()),
            DylibReply::Err { message } => Err(OpError::failed(message)),
        }
    }

    fn on_start(&mut self, out: &mut OpOutput) -> OpResult<()> {
        self.dispatch_void(DylibCall::OnStart, out)
    }

    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match self.call(&DylibCall::on_event(event))? {
            DylibReply::Ok { status, sends } => {
                push_sends(out, sends)?;
                Ok(status.into())
            }
            DylibReply::Err { message } => Err(OpError::failed(message)),
        }
    }

    fn on_stop(&mut self, out: &mut OpOutput) -> OpResult<()> {
        self.dispatch_void(DylibCall::OnStop, out)
    }

    fn on_reload(&mut self, out: &mut OpOutput) -> OpResult<()> {
        self.dispatch_void(DylibCall::OnReload, out)
    }
}

impl Drop for DylibOperator {
    fn drop(&mut self) {
        let handle = self.handle;
        let drop_fn = self.vtable.drop;
        // `Drop::drop` must never unwind (a second panic while already
        // unwinding aborts the process outright), so this is caught
        // rather than propagated — there is no channel to report it
        // through here anyway. Defense in depth alongside
        // `export_dylib_operator!`'s own generated `drop_operator`, which
        // already catches a panicking `Drop for T` on the dylib's own
        // side.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: `handle` is this instance's own, produced by
            // `vtable.new` and not already passed to `vtable.drop` — this
            // is the only place that happens, and `Drop::drop` runs at
            // most once per value.
            unsafe { drop_fn(handle) };
        }));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// The dependency links and its error path behaves: opening a library
    /// that does not exist is a recoverable `Err`, not a panic or an abort.
    ///
    /// This is the property every loader in this module has to build on — a
    /// missing `dylib:` path in a manifest must surface as a runtime error
    /// naming the file, never as a crash of the whole node process.
    #[test]
    fn opening_a_missing_library_is_a_recoverable_error() {
        let missing = std::env::temp_dir().join(format!(
            "astrs-runtime-dylib-does-not-exist-{}.so",
            std::process::id()
        ));
        // SAFETY: the path does not exist, so no library initializer can run;
        // `Library::new` is unsafe purely because loading arbitrary code
        // executes that library's constructors.
        let result = unsafe { libloading::Library::new(&missing) };
        assert!(
            result.is_err(),
            "opening {} must fail rather than succeed",
            missing.display()
        );
    }

    #[test]
    fn dylib_source_load_reports_a_missing_file_as_open_error() {
        let missing = std::env::temp_dir().join(format!(
            "astrs-runtime-dylib-source-missing-{}.so",
            std::process::id()
        ));
        let error = DylibSource::load(&missing, "Whatever").unwrap_err();
        assert!(matches!(error, DylibError::Open { .. }), "{error}");
    }

    #[test]
    fn resolve_dylib_path_rebases_a_relative_path_and_leaves_an_absolute_one_alone() {
        let dataflow_dir = Path::new("/graphs/perception");
        assert_eq!(
            resolve_dylib_path(Some(dataflow_dir), "./libyolo.so"),
            dataflow_dir.join("./libyolo.so")
        );
        assert_eq!(
            resolve_dylib_path(Some(dataflow_dir), "../shared/libyolo.so"),
            dataflow_dir.join("../shared/libyolo.so")
        );
        assert_eq!(
            resolve_dylib_path(Some(dataflow_dir), "/opt/operators/libyolo.so"),
            PathBuf::from("/opt/operators/libyolo.so"),
        );
    }

    #[test]
    fn resolve_dylib_path_with_no_dataflow_dir_leaves_a_relative_path_relative() {
        assert_eq!(
            resolve_dylib_path(None, "./libyolo.so"),
            PathBuf::from("./libyolo.so")
        );
    }

    /// A real `libloading::Error` to embed in the variants below — pulled
    /// from an actual failed call rather than guessed at, since this
    /// crate's own `[dependencies]` do not otherwise commit to which
    /// variants that enum has.
    fn a_real_libloading_error() -> libloading::Error {
        let missing = std::env::temp_dir().join(format!(
            "astrs-runtime-dylib-error-fixture-{}.so",
            std::process::id()
        ));
        // SAFETY: the path does not exist, so no library initializer runs.
        unsafe { libloading::Library::new(&missing) }
            .expect_err("a nonexistent path must fail to open")
    }

    #[test]
    fn every_dylib_error_variant_renders_and_implements_error() {
        let errors = [
            DylibError::Open {
                path: PathBuf::from("./libx.so"),
                source: a_real_libloading_error(),
            },
            DylibError::MissingDescriptor {
                path: PathBuf::from("./libx.so"),
                source: a_real_libloading_error(),
            },
            DylibError::InvalidName {
                path: PathBuf::from("./libx.so"),
            },
            DylibError::DescriptorPanicked {
                path: PathBuf::from("./libx.so"),
            },
            DylibError::AbiVersionMismatch {
                path: PathBuf::from("./libx.so"),
                found: 99,
                expected: ASTRS_OPERATOR_ABI_VERSION,
            },
            DylibError::OperatorNameMismatch {
                path: PathBuf::from("./libx.so"),
                found: "Wrong".to_owned(),
                expected: "Right".to_owned(),
            },
        ];
        for error in errors {
            assert!(!error.to_string().is_empty());
            let _: &dyn std::error::Error = &error;
        }
    }
}
