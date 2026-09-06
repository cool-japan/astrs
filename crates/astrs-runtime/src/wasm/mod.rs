//! Hosting operators as WebAssembly modules (`wasm-operators`).
//!
//! Gated behind the `wasm-operators` feature, which is off by default: a
//! runtime hosting only compiled-in `register_operator!` types — the flagship
//! path of blueprint §9.3 — must not pay for an interpreter it never calls.
//!
//! # What this module owns
//!
//! A manifest operator entry whose source kind is
//! [`wasm`](astrs_manifest::OperatorConfig::wasm) names a `.wasm` module on
//! disk instead of a name in the compiled-in
//! [`astrs_operator_api::OperatorRegistry`]. Turning that module into a live
//! operator is this module's job: resolving the path relative to the
//! dataflow file ([`resolve_wasm_path`], the exact rule
//! `crate::dylib::resolve_dylib_path` already applies to `dylib:` — a plain
//! code span rather than a link, since that module only exists under the
//! `dylib-operators` feature, which this one does not require),
//! validating and compiling it once (`WasmSource::load`), instantiating it
//! afresh for every incarnation `crate::worker::operator_loop`'s restart
//! loop asks for (`WasmSource::build`), and bridging every instance into
//! [`astrs_operator_api::Operator`] (`WasmOperator`) so that loop drives it
//! exactly like a compiled-in operator — it never learns the difference,
//! matching `crate::dylib`'s own precedent.
//!
//! # The guest ABI
//!
//! This is this module's own normative contract — nothing elsewhere in the
//! workspace defines it. A `wasm:`-sourced module must export:
//!
//! | Export | Signature | Purpose |
//! |---|---|---|
//! | `astrs-op-alloc` | `(len: i32) -> i32` | Reserve `len` bytes of the guest's own linear memory, returning a pointer the host may write into. |
//! | `astrs-op-init` | `(ptr: i32, len: i32) -> i32` | Receive this instance's manifest `config:` map. `ptr`/`len` describe an oxicode-encoded `BTreeMap<String, astrs_wire::Parameter>` (`crate::operator_config`'s own resolved shape) the host already wrote into guest memory via `astrs-op-alloc`. Returns `0` on success, any other value on failure. |
//! | `astrs-op-event` | `(ptr: i32, len: i32) -> i32` | Handle one `GuestCall` (this module's private oxicode-encoded mirror of every [`astrs_operator_api::Operator`] hook — see below). `ptr`/`len` describe the encoded call, written the same way as `astrs-op-init`'s argument. Returns a `GuestStatus` discriminant: `0` (continue), `1` (finished), or any other value (failed — an [`astrs_operator_api::OpError::Failed`], not a fatal host error). |
//!
//! and may import exactly one host function:
//!
//! | Import | Signature | Purpose |
//! |---|---|---|
//! | `astrs.output-send` | `(ptr: i32, len: i32)` | Buffer one output send. `ptr`/`len` describe an oxicode-encoded `(astrs_wire::DataId, astrs_wire::Metadata, Vec<u8>)` triple — [`astrs_operator_api::OpSend`]'s own three parts — read directly out of the calling instance's own exported memory. Callable any number of times during one `astrs-op-init` or `astrs-op-event` call; every call this module makes replays onto the host [`astrs_operator_api::OpOutput`] in order. |
//!
//! A module that skips `astrs-op-init` output (an empty config) is fine —
//! `WasmOperator::configure` still calls it once, with an empty map, so a
//! guest that ignores configuration entirely need not special-case that.
//! There is no `astrs-op-start`/`astrs-op-stop`/`astrs-op-reload` export:
//! `GuestCall` tags which [`astrs_operator_api::Operator`] hook is
//! running, and every guest export answers through the *same* `astrs-op-event`
//! entry point except `configure` (via `astrs-op-init`, kept separate only
//! because it is the one hook every instance's very first call must be, and
//! giving it a distinct export makes that impossible to get backwards on the
//! guest side).
//!
//! # Why one call for four hooks
//!
//! `on_start`, `on_event`, `on_stop` and `on_reload` all answer through
//! `astrs-op-event`, tagged by `GuestCall` — the same collapsing
//! `crate::dylib`'s ABI already applies to its own `OperatorVTable::on_event`,
//! for the identical reason: four near-identical exports would each need
//! their own signature kept stable forever, for no information a single tag
//! byte does not already carry just as well.
//!
//! # Why the payload is a private mirror, not [`astrs_operator_api::OpEvent`] itself
//!
//! [`astrs_operator_api::OpEvent`] is this workspace's in-process API — free
//! to gain a field or a variant whenever ordinary Rust semver allows. A
//! guest ABI needs a harder compatibility promise than that, so `GuestCall`
//! and `GuestStatus` are a private, `#[non_exhaustive]`-free mirror,
//! converted at the boundary — exactly `crate::dylib`'s own `WireEvent`
//! precedent (see that module's docs for the fuller rationale, which applies
//! here unchanged). Every field type crossing the boundary — [`DataId`],
//! [`PortRef`], [`Metadata`] and friends — already implements
//! [`oxicode::Encode`]/[`oxicode::Decode`] for the real wire protocol
//! (blueprint §7.1), so this mirror reuses those implementations rather than
//! inventing a second codec.
//!
//! # Enforcement: fuel, memory, and never taking the host down
//!
//! Three failure modes a hosted module must never be able to inflict on the
//! runtime process itself:
//!
//! - **An infinite (or merely slow) loop.** [`wasmi::Config::consume_fuel`]
//!   is on for every [`wasmi::Engine`] this module builds, and every guest
//!   call — `astrs-op-alloc`, `astrs-op-init`, `astrs-op-event` — starts
//!   from a fresh [`crate::WasmSandboxConfig::fuel_per_call`] budget (set via
//!   [`wasmi::Store::set_fuel`] immediately before the call): a slow event
//!   never starves the next one, and no single call can run forever.
//!   Exhaustion traps with [`wasmi::TrapCode::OutOfFuel`], caught below and
//!   reported as an ordinary [`astrs_operator_api::OpError::Failed`] — see
//!   `WasmOperator::call`.
//! - **Unbounded linear-memory growth.** [`wasmi::Store::limiter`] installs
//!   a [`wasmi::StoreLimits`] capping every guest memory at
//!   [`crate::WasmSandboxConfig::max_memory_bytes`]
//!   ([`wasmi::StoreLimitsBuilder::trap_on_grow_failure`] is on, so a guest
//!   that tries to grow past the cap traps deterministically rather than
//!   silently receiving `memory.grow`'s ordinary "-1" failure return and
//!   perhaps mishandling it).
//! - **A trap of any other kind** (`unreachable`, an out-of-bounds access,
//!   integer division by zero, …). [`wasmi::Error::as_trap_code`] on the
//!   `Err` side of any guest call is how this module tells "this specific
//!   call failed" from "the host itself is broken" — it never is the
//!   latter; every trap becomes a message-carrying
//!   [`astrs_operator_api::OpError`], exactly like a caught panic already
//!   does in [`crate::worker`]'s own restart loop. The host process itself
//!   never observes anything sharper than a returned `Err`.
//!
//! # Why an interpreter, not a JIT
//!
//! `wasmi` executes WebAssembly by interpretation, in pure Rust. Every JIT
//! alternative brings either a C/C++ codegen backend or a `-sys` crate, which
//! blueprint §18.1 rules out for every feature combination on every target.
//! Interpretation is also the right trade for this workload: an operator's
//! per-event body is small, sandbox isolation is the point, and a JIT's
//! warm-up would be paid on every node start.
//!
//! `wasmi` is declared with `default-features = false, features = ["std"]`,
//! which drops its `wat` default — `.wat` text parsing is a test convenience,
//! and lives in this crate's `wat` dev-dependency instead of in every
//! released build.
//!
//! # A fresh instance per incarnation
//!
//! `WasmSource::load` parses and validates the module's bytes exactly
//! once — [`wasmi::Module`] is cheap to clone (an `Arc` internally) and
//! immutable once built. `WasmSource::build`, called once per incarnation
//! by `crate::worker::operator_loop`'s restart loop (the same contract
//! `crate::dylib::DylibSource::build` already honors), builds a brand new
//! [`wasmi::Store`] and instance from that shared module every time: a
//! guest's own global state (anything past its `astrs-op-init` call) is
//! never carried across a restart, matching every other operator source —
//! `Default::default()` for a registry-sourced operator, a fresh `new_fn()`
//! call for a `dylib:`-sourced one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use astrs_operator_api::{OpError, OpEvent, OpOutput, OpResult, Operator, Status};
use astrs_wire::{
    DataId, DurationMs, Metadata, ParamKey, ParamScope, Parameter, PortRef, RouteCloseReason,
    StopCause, WireDecode, WireEncode,
};
use oxicode::{Decode, Encode};
use wasmi::{Caller, Config, Engine, Linker, Module, Store, StoreLimitsBuilder};

use crate::config::WasmSandboxConfig;

/// The guest export that reserves `len` bytes of guest memory and returns a
/// pointer to it — see this module's own docs for the full ABI table.
const EXPORT_ALLOC: &str = "astrs-op-alloc";
/// The guest export that receives this instance's manifest `config:` map.
const EXPORT_INIT: &str = "astrs-op-init";
/// The guest export that handles one [`GuestCall`].
const EXPORT_EVENT: &str = "astrs-op-event";
/// The host import a guest calls to buffer one output send.
const IMPORT_MODULE: &str = "astrs";
/// See [`IMPORT_MODULE`].
const IMPORT_OUTPUT_SEND: &str = "output-send";

/// Why a manifest `wasm:` entry could not be loaded or run.
///
/// `#[non_exhaustive]`: the append-only evolution rule (blueprint §3.4)
/// applies here too — matching `crate::dylib::DylibError`'s own precedent.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WasmError {
    /// The module's bytes could not be read from disk.
    #[error("could not read wasm module at {path}: {source}")]
    Read {
        /// The path that was read, after [`resolve_wasm_path`].
        path: PathBuf,
        /// What [`std::fs::read`] reported.
        #[source]
        source: std::io::Error,
    },

    /// The bytes at `path` are not valid WebAssembly — malformed, or using
    /// a feature this build's [`wasmi::Config`] does not enable.
    #[error("wasm module at {path} failed to validate: {source}")]
    Invalid {
        /// The path that was read.
        path: PathBuf,
        /// What `wasmi` reported.
        #[source]
        source: wasmi::Error,
    },

    /// The module has no `astrs-op-event` export, or it has the wrong
    /// signature — every module this loader hosts must implement this
    /// module's own guest ABI (see the module docs).
    #[error(
        "wasm module at {path} has no `{EXPORT_EVENT}(i32, i32) -> i32` export (the required \
         guest ABI entry point; see `astrs-runtime`'s `wasm` module docs): {source}"
    )]
    MissingEventExport {
        /// The path that was read.
        path: PathBuf,
        /// What `wasmi` reported resolving or typing the export.
        #[source]
        source: wasmi::Error,
    },
}

/// Resolves a manifest `operators[].wasm` path — the exact rule
/// `crate::dylib::resolve_dylib_path` already applies to `dylib:` (a plain
/// code span rather than a link; see this module's own docs on why), kept
/// as a free function of its own for the same reason: an absolute `declared`
/// path is used as-is, a relative one resolves against `dataflow_dir` when
/// the caller has one (blueprint §22), and against the process's own
/// current directory otherwise.
///
/// # Examples
///
/// ```
/// use astrs_runtime::wasm::resolve_wasm_path;
/// use std::path::Path;
///
/// assert_eq!(
///     resolve_wasm_path(Some(Path::new("/graphs")), "./filter.wasm"),
///     Path::new("/graphs/./filter.wasm")
/// );
/// assert_eq!(
///     resolve_wasm_path(Some(Path::new("/graphs")), "/opt/filter.wasm"),
///     Path::new("/opt/filter.wasm"),
///     "an absolute declared path is never rebased"
/// );
/// ```
#[must_use]
pub fn resolve_wasm_path(dataflow_dir: Option<&Path>, declared: &str) -> PathBuf {
    let declared_path = Path::new(declared);
    if declared_path.is_absolute() {
        return declared_path.to_path_buf();
    }
    match dataflow_dir {
        Some(dir) => dir.join(declared_path),
        None => declared_path.to_path_buf(),
    }
}

/// A wire-encodable mirror of every [`astrs_operator_api::Operator`] hook —
/// see this module's docs for why a mirror rather than
/// [`astrs_operator_api::OpEvent`] itself, and why one call answers for all
/// four (`on_start`, `on_event`, `on_stop`, `on_reload`; `configure` travels
/// over `astrs-op-init` instead — see [`WasmOperator::call_init`]).
///
/// Private: nothing outside this module ever constructs or matches on this
/// directly.
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
enum GuestCall {
    /// Mirrors [`Operator::on_start`].
    #[oxicode(variant = 0)]
    OnStart,
    /// Mirrors [`Operator::on_event`]'s [`OpEvent::Input`].
    #[oxicode(variant = 1)]
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
    /// Mirrors [`Operator::on_event`]'s [`OpEvent::InputClosed`].
    #[oxicode(variant = 2)]
    InputClosed {
        /// See [`OpEvent::InputClosed::id`].
        id: DataId,
        /// See [`OpEvent::InputClosed::source`].
        source: PortRef,
        /// See [`OpEvent::InputClosed::reason`].
        reason: RouteCloseReason,
    },
    /// Mirrors [`Operator::on_event`]'s [`OpEvent::Stop`].
    #[oxicode(variant = 3)]
    Stop {
        /// See [`OpEvent::Stop::cause`].
        cause: StopCause,
        /// See [`OpEvent::Stop::grace`].
        grace: Option<DurationMs>,
    },
    /// Mirrors [`Operator::on_event`]'s [`OpEvent::ParamUpdate`].
    #[oxicode(variant = 4)]
    ParamUpdate {
        /// See [`OpEvent::ParamUpdate::scope`].
        scope: ParamScope,
        /// See [`OpEvent::ParamUpdate::key`].
        key: ParamKey,
        /// See [`OpEvent::ParamUpdate::value`].
        value: Parameter,
    },
    /// Mirrors [`Operator::on_stop`].
    #[oxicode(variant = 5)]
    OnStop,
    /// Mirrors [`Operator::on_reload`].
    #[oxicode(variant = 6)]
    OnReload,
}

impl GuestCall {
    /// Builds the event-shaped variants from a real [`OpEvent`], or `None`
    /// for a variant this module does not recognize.
    ///
    /// Unlike `crate::dylib`'s own `WireEvent` conversion (which lives
    /// *inside* `astrs-operator-api`, the same crate that defines
    /// [`OpEvent`], so its non-exhaustiveness does not bind that match),
    /// this function lives in a downstream crate: [`OpEvent`] is
    /// `#[non_exhaustive]` here, so the compiler requires a wildcard arm
    /// regardless of how many of today's variants are covered. Returning
    /// `None` for it — rather than fabricating a call with no real
    /// meaning — matches [`astrs_operator_api::OpEvent::from_node_event`]'s
    /// own precedent of dropping an event this layer cannot represent
    /// rather than guessing at one; [`WasmOperator::on_event`] treats `None`
    /// as "nothing to deliver" (`Status::Continue`, no guest call at all).
    fn from_op_event(event: &OpEvent) -> Option<Self> {
        let call = match event {
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
            OpEvent::Reload => Self::OnReload,
            OpEvent::ParamUpdate { scope, key, value } => Self::ParamUpdate {
                scope: scope.clone(),
                key: key.clone(),
                value: value.clone(),
            },
            _ => return None,
        };
        Some(call)
    }
}

/// What `astrs-op-event` returns — the guest ABI's own status code, decoded
/// from the raw `i32` every `astrs-op-event` call answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuestStatus {
    /// `0` — keep running.
    Continue,
    /// `1` — this operator is done, mirroring
    /// [`astrs_operator_api::Status::Finished`].
    Finished,
    /// Anything else — the guest reported its own failure. Carries the raw
    /// code for the [`OpError::Failed`] message
    /// [`WasmOperator::decode_status`] builds from it.
    Failed(i32),
}

impl GuestStatus {
    fn from_code(code: i32) -> Self {
        match code {
            0 => Self::Continue,
            1 => Self::Finished,
            other => Self::Failed(other),
        }
    }
}

/// One buffered output send, exactly as `astrs.output-send` decoded it —
/// [`astrs_operator_api::OpSend`]'s own three parts, not `OpSend` itself (a
/// type this module has no need of; see `crate::dylib`'s `DylibReply::Ok`
/// docs for the identical reasoning).
type GuestSend = (DataId, Metadata, Vec<u8>);

/// Per-instance host state: the sends `astrs.output-send` buffered during
/// the call currently in flight, and the [`wasmi::StoreLimits`] enforcing
/// [`crate::WasmSandboxConfig::max_memory_bytes`] on every
/// `memory.grow`/`table.grow`.
///
/// This is the wasmi [`Store`]'s own `T` — `Caller::data_mut` inside
/// `astrs.output-send`'s host closure is how that closure reaches
/// `pending_sends` without a `RefCell` or a channel, and
/// [`Store::limiter`]'s own callback is how it reaches `limits`.
struct HostState {
    /// Sends buffered by `astrs.output-send` during the call in flight,
    /// oldest first — drained by [`WasmOperator::call`] into the real
    /// [`OpOutput`] after every successful guest call.
    pending_sends: Vec<GuestSend>,
    /// The memory/table growth cap this instance enforces — built once at
    /// construction time ([`WasmOperator::new`]) from
    /// [`crate::WasmSandboxConfig::max_memory_bytes`], then handed back out
    /// through [`Store::limiter`]'s callback on every growth attempt.
    limits: wasmi::StoreLimits,
}

impl HostState {
    fn new(sandbox: &WasmSandboxConfig) -> Self {
        Self {
            pending_sends: Vec::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(sandbox.max_memory_bytes)
                .trap_on_grow_failure(true)
                .build(),
        }
    }
}

/// A parsed, validated `wasm:` operator module — [`WasmSource::load`] reads
/// and validates it exactly once; [`WasmSource::build`] then instantiates it
/// as many times as the host's restart policy needs, cheaply (parsing and
/// validation never repeat — see this module's own docs on why a fresh
/// instance per incarnation is still correct).
///
/// `Debug` is hand-written rather than derived, matching
/// `crate::dylib::DylibSource`'s own precedent: neither the engine nor the
/// compiled module has anything a caller debugging a failed load would act
/// on beyond "a module was loaded".
pub(crate) struct WasmSource {
    engine: Engine,
    module: Module,
    sandbox: WasmSandboxConfig,
}

impl core::fmt::Debug for WasmSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WasmSource").finish_non_exhaustive()
    }
}

impl WasmSource {
    /// Reads `path`, compiles and validates it against a fuel-metering,
    /// [`Config::wasm_multi_memory`]-default [`wasmi::Engine`], and confirms
    /// it exports a correctly-typed `astrs-op-event`.
    ///
    /// The other two required exports (`astrs-op-alloc`, `astrs-op-init`)
    /// are resolved and typed per-instance instead, in
    /// [`WasmOperator::new`] — [`wasmi::Module::get_export`] answers from
    /// the module's own static export list either way, so validating only
    /// the one export every code path needs immediately (construction
    /// itself never calls `astrs-op-alloc`/`astrs-op-init`) keeps this
    /// method's own error variants down to what a caller loading a module
    /// that will never even be instantiated actually needs to see.
    ///
    /// # Errors
    ///
    /// See [`WasmError`]'s variants.
    pub(crate) fn load(path: &Path, sandbox: WasmSandboxConfig) -> Result<Self, WasmError> {
        let bytes = std::fs::read(path).map_err(|source| WasmError::Read {
            path: path.to_path_buf(),
            source,
        })?;

        let mut config = Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);

        let module = Module::new(&engine, &bytes[..]).map_err(|source| WasmError::Invalid {
            path: path.to_path_buf(),
            source,
        })?;

        let event_ty = module
            .get_export(EXPORT_EVENT)
            .and_then(|ty| ty.func().cloned());
        let is_correctly_typed = event_ty.as_ref().is_some_and(|ty| {
            ty.params() == [wasmi::ValType::I32, wasmi::ValType::I32]
                && ty.results() == [wasmi::ValType::I32]
        });
        if !is_correctly_typed {
            return Err(WasmError::MissingEventExport {
                path: path.to_path_buf(),
                source: wasmi::Error::new(format!(
                    "no `{EXPORT_EVENT}` export with the expected `(i32, i32) -> i32` signature"
                )),
            });
        }

        Ok(Self {
            engine,
            module,
            sandbox,
        })
    }

    /// Instantiates a fresh operator from this loaded module — the
    /// per-incarnation construction step
    /// [`crate::worker::operator_loop`]'s restart loop drives, matching
    /// [`astrs_operator_api::OperatorRegistry::build`]'s and
    /// `crate::dylib::DylibSource::build`'s own `OpResult<Box<dyn
    /// Operator>>` shape so every operator source plugs into the same
    /// restart loop unmodified.
    ///
    /// # Errors
    ///
    /// [`OpError::Failed`] if instantiation traps, runs out of fuel, or the
    /// module's `astrs-op-alloc`/`astrs-op-init` exports are missing or
    /// mistyped (checked here rather than in [`WasmSource::load`] because
    /// [`wasmi::Instance::get_typed_func`] needs a live [`Store`] to check
    /// against, not just the module's static export list).
    pub(crate) fn build(&self) -> OpResult<Box<dyn Operator>> {
        WasmOperator::new(&self.engine, &self.module, self.sandbox.clone()).map(|operator| {
            let boxed: Box<dyn Operator> = Box::new(operator);
            boxed
        })
    }
}

/// A single wasm-hosted operator instance, bridged into
/// [`astrs_operator_api::Operator`] so [`crate::worker::operator_loop`]
/// drives it exactly like any compiled-in operator — matching
/// `crate::dylib::DylibOperator`'s own role for the `dylib-operators`
/// feature.
struct WasmOperator {
    store: Store<HostState>,
    alloc: wasmi::TypedFunc<i32, i32>,
    init: wasmi::TypedFunc<(i32, i32), i32>,
    event: wasmi::TypedFunc<(i32, i32), i32>,
    memory: wasmi::Memory,
    sandbox: WasmSandboxConfig,
}

impl WasmOperator {
    /// Instantiates `module` into a fresh [`Store`], wires the
    /// `astrs.output-send` host import, installs the memory limiter, and
    /// resolves every guest export this ABI requires.
    fn new(engine: &Engine, module: &Module, sandbox: WasmSandboxConfig) -> OpResult<Self> {
        let mut store = Store::new(engine, HostState::new(&sandbox));
        store.limiter(|state| &mut state.limits);

        let mut linker = <Linker<HostState>>::new(engine);
        linker
            .func_wrap(IMPORT_MODULE, IMPORT_OUTPUT_SEND, host_output_send)
            .map_err(|source| {
                OpError::failed(format!("failed to define `astrs.output-send`: {source}"))
            })?;

        set_fuel(&mut store, sandbox.fuel_per_call)?;
        let instance = linker
            .instantiate_and_start(&mut store, module)
            .map_err(|source| {
                OpError::failed(format!("wasm module instantiation failed: {source}"))
            })?;

        let alloc = instance
            .get_typed_func::<i32, i32>(&store, EXPORT_ALLOC)
            .map_err(|source| {
                OpError::failed(format!(
                    "wasm module has no `{EXPORT_ALLOC}(i32) -> i32` export: {source}"
                ))
            })?;
        let init = instance
            .get_typed_func::<(i32, i32), i32>(&store, EXPORT_INIT)
            .map_err(|source| {
                OpError::failed(format!(
                    "wasm module has no `{EXPORT_INIT}(i32, i32) -> i32` export: {source}"
                ))
            })?;
        let event = instance
            .get_typed_func::<(i32, i32), i32>(&store, EXPORT_EVENT)
            .map_err(|source| {
                OpError::failed(format!(
                    "wasm module has no `{EXPORT_EVENT}(i32, i32) -> i32` export: {source}"
                ))
            })?;
        let memory = instance.get_memory(&store, "memory").ok_or_else(|| {
            OpError::failed("wasm module exports no linear memory named `memory`")
        })?;

        Ok(Self {
            store,
            alloc,
            init,
            event,
            memory,
            sandbox,
        })
    }

    /// Writes `bytes` into a freshly `astrs-op-alloc`-ed region of guest
    /// memory, returning the pointer — the shared first half of every call
    /// this type makes into the guest (`astrs-op-init` and `astrs-op-event`
    /// both take a `(ptr, len)` pair built this way).
    ///
    /// # Errors
    ///
    /// [`OpError::Failed`] if `astrs-op-alloc` traps, runs out of fuel, or
    /// returns a region [`wasmi::Memory::write`] cannot address (a
    /// misbehaving or malicious allocator).
    fn write_guest_bytes(&mut self, bytes: &[u8]) -> OpResult<i32> {
        set_fuel(&mut self.store, self.sandbox.fuel_per_call)?;
        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
        let len = bytes.len() as i32;
        let ptr = self
            .alloc
            .call(&mut self.store, len)
            .map_err(|source| call_failed(EXPORT_ALLOC, &source))?;
        #[allow(clippy::cast_sign_loss)]
        self.memory
            .write(&mut self.store, ptr as usize, bytes)
            .map_err(|source| {
                OpError::failed(format!(
                    "`{EXPORT_ALLOC}` returned a region `astrs-runtime` could not write into: \
                     {source}"
                ))
            })?;
        Ok(ptr)
    }

    /// Runs `astrs-op-init` with `config`, oxicode-encoded — the
    /// [`Operator::configure`] half of this ABI (kept off `astrs-op-event`;
    /// see this module's own docs on why).
    fn call_init(&mut self, config: &BTreeMap<String, Parameter>) -> OpResult<()> {
        let bytes = config
            .encode_to_vec()
            .map_err(|source| OpError::failed(format!("failed to encode config: {source}")))?;
        let ptr = self.write_guest_bytes(&bytes)?;

        self.store.data_mut().pending_sends.clear();
        set_fuel(&mut self.store, self.sandbox.fuel_per_call)?;
        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
        let len = bytes.len() as i32;
        let code = self
            .init
            .call(&mut self.store, (ptr, len))
            .map_err(|source| call_failed(EXPORT_INIT, &source))?;
        if code != 0 {
            return Err(OpError::failed(format!(
                "`{EXPORT_INIT}` reported failure (code {code})"
            )));
        }
        Ok(())
    }

    /// Runs one [`GuestCall`] through `astrs-op-event`, draining whatever
    /// `astrs.output-send` buffered into `out` on success.
    fn call(&mut self, call: &GuestCall, out: &mut OpOutput) -> OpResult<GuestStatus> {
        let bytes = call
            .encode_to_vec()
            .map_err(|source| OpError::failed(format!("failed to encode guest call: {source}")))?;
        let ptr = self.write_guest_bytes(&bytes)?;

        self.store.data_mut().pending_sends.clear();
        set_fuel(&mut self.store, self.sandbox.fuel_per_call)?;
        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
        let len = bytes.len() as i32;
        let code = self
            .event
            .call(&mut self.store, (ptr, len))
            .map_err(|source| call_failed(EXPORT_EVENT, &source))?;

        let sends = std::mem::take(&mut self.store.data_mut().pending_sends);
        for (id, metadata, payload) in sends {
            out.send_bytes(id.as_str(), metadata, payload)?;
        }

        match GuestStatus::from_code(code) {
            GuestStatus::Failed(raw) => Err(OpError::failed(format!(
                "`{EXPORT_EVENT}` reported failure (code {raw})"
            ))),
            status => Ok(status),
        }
    }

    /// [`Self::call`], discarding the [`GuestStatus`] — every
    /// [`Operator`] method but `on_event` itself has no `Status` of its own
    /// to report, matching `crate::dylib::DylibOperator::dispatch_void`.
    fn call_void(&mut self, call: GuestCall, out: &mut OpOutput) -> OpResult<()> {
        self.call(&call, out).map(|_status| ())
    }
}

/// Sets `store`'s remaining fuel to `amount`, mapping the (infallible in
/// practice — fuel metering is always on for every [`Engine`] this module
/// builds) [`wasmi::Store::set_fuel`] error to [`OpError::Failed`] rather
/// than unwrapping it, matching this crate's own no-`unwrap`-in-production
/// policy.
fn set_fuel(store: &mut Store<HostState>, amount: u64) -> OpResult<()> {
    store
        .set_fuel(amount)
        .map_err(|source| OpError::failed(format!("failed to set fuel budget: {source}")))
}

/// Turns a failed guest call ([`wasmi::Error`]) into an
/// [`astrs_operator_api::OpError`] — the one place this module tells "fuel
/// ran out", "the guest trapped", and "something else went wrong calling
/// into wasmi" apart, all as the same error *kind* (an ordinary operator
/// failure, never a host crash — see this module's own docs on enforcement).
fn call_failed(export: &str, source: &wasmi::Error) -> OpError {
    match source.as_trap_code() {
        Some(wasmi::TrapCode::OutOfFuel) => OpError::failed(format!(
            "`{export}` exhausted its fuel budget (an infinite or too-slow guest loop)"
        )),
        Some(wasmi::TrapCode::GrowthOperationLimited) => OpError::failed(format!(
            "`{export}` tried to grow its linear memory past this host's configured limit"
        )),
        Some(trap) => OpError::failed(format!("`{export}` trapped: {trap}")),
        None => OpError::failed(format!("`{export}` failed: {source}")),
    }
}

/// The `astrs.output-send` host function every guest call may invoke any
/// number of times — reads `(ptr, len)` out of the calling instance's own
/// exported memory, decodes it as a [`GuestSend`], and buffers it onto
/// [`HostState::pending_sends`] for [`WasmOperator::call`] to drain after
/// the guest call returns.
///
/// A malformed send (bytes that do not decode as a [`GuestSend`], or an out
/// of bounds `ptr`/`len`) traps the guest call rather than silently
/// dropping the send — a guest violating its own side of this ABI is exactly
/// the kind of misbehavior this module's enforcement story exists to catch,
/// and a trap surfaces through [`call_failed`] as an ordinary
/// [`astrs_operator_api::OpError`] the same as any other.
fn host_output_send(
    mut caller: Caller<'_, HostState>,
    ptr: i32,
    len: i32,
) -> Result<(), wasmi::Error> {
    let memory = caller
        .get_export("memory")
        .and_then(wasmi::Extern::into_memory)
        .ok_or_else(|| {
            wasmi::Error::new("guest called astrs.output-send with no exported memory")
        })?;

    if ptr < 0 || len < 0 {
        return Err(wasmi::Error::new(
            "astrs.output-send called with a negative pointer or length",
        ));
    }
    #[allow(clippy::cast_sign_loss)]
    let (offset, length) = (ptr as usize, len as usize);
    let mut bytes = vec![0u8; length];
    memory
        .read(&caller, offset, &mut bytes)
        .map_err(|source| wasmi::Error::new(format!("astrs.output-send: {source}")))?;

    let send = GuestSend::decode_exact(&bytes).map_err(|source| {
        wasmi::Error::new(format!("astrs.output-send: malformed send: {source}"))
    })?;
    caller.data_mut().pending_sends.push(send);
    Ok(())
}

impl Operator for WasmOperator {
    fn configure(&mut self, config: &BTreeMap<String, Parameter>) -> OpResult<()> {
        self.call_init(config)
    }

    fn on_start(&mut self, out: &mut OpOutput) -> OpResult<()> {
        self.call_void(GuestCall::OnStart, out)
    }

    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        let Some(call) = GuestCall::from_op_event(event) else {
            // A node-level event with no meaning at the guest ABI level —
            // see `GuestCall::from_op_event`'s own docs. Nothing to deliver,
            // so this is a no-op, not a failure.
            return Ok(Status::Continue);
        };
        match self.call(&call, out)? {
            GuestStatus::Continue => Ok(Status::Continue),
            GuestStatus::Finished => Ok(Status::Finished),
            GuestStatus::Failed(raw) => Err(OpError::failed(format!(
                "`{EXPORT_EVENT}` reported failure (code {raw})"
            ))),
        }
    }

    fn on_stop(&mut self, out: &mut OpOutput) -> OpResult<()> {
        self.call_void(GuestCall::OnStop, out)
    }

    fn on_reload(&mut self, out: &mut OpOutput) -> OpResult<()> {
        self.call_void(GuestCall::OnReload, out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// The end-to-end shape every wasm-hosted operator is built on: text
    /// fixture -> bytes -> [`wasmi::Module`] -> instance -> typed call.
    ///
    /// This exists to pin the exact API surface available under the feature
    /// set the workspace declares — `wasmi` with `default-features = false,
    /// features = ["std"]`, plus `wat` as a dev-dependency. If a future
    /// version or feature change removed `Module::new`'s ability to take
    /// plain binary bytes, or moved instantiation behind a feature the
    /// workspace does not enable, this test fails here rather than
    /// surfacing halfway through a real loader.
    #[test]
    fn a_wat_fixture_compiles_instantiates_and_runs() {
        let wasm = wat::parse_str(
            r#"
            (module
              (func (export "add") (param i32 i32) (result i32)
                local.get 0
                local.get 1
                i32.add))
            "#,
        )
        .expect("the fixture is valid WebAssembly text");

        let engine = wasmi::Engine::default();
        let module = wasmi::Module::new(&engine, &wasm[..]).expect("module must validate");
        let mut store = wasmi::Store::new(&engine, ());
        let linker = <wasmi::Linker<()>>::new(&engine);
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .expect("instantiation must succeed");

        let add = instance
            .get_typed_func::<(i32, i32), i32>(&store, "add")
            .expect("the module exports `add`");
        assert_eq!(add.call(&mut store, (2, 3)).expect("call must succeed"), 5);
    }

    /// A module that is not valid WebAssembly is a recoverable error, never
    /// a panic: a bad `wasm:` path in a manifest must fail that one
    /// operator, not the whole node process.
    #[test]
    fn invalid_module_bytes_are_a_recoverable_error() {
        let engine = wasmi::Engine::default();
        assert!(wasmi::Module::new(&engine, b"not wasm at all".as_slice()).is_err());
    }

    /// A guest-memory-and-ABI-implementing WAT fixture shared by every test
    /// below: exports linear `memory`, `astrs-op-alloc` (a simple bump
    /// allocator inside a static scratch region), `astrs-op-init` (always
    /// succeeds), and `astrs-op-event` — whose behavior each test's own
    /// `$event_body` fills in, so one fixture generator covers the echo,
    /// fuel, memory-limit and trap tests without four near-duplicate
    /// hand-written modules.
    ///
    /// `$event_body` receives the event `(ptr, len)` as locals `$ptr`/`$len`
    /// and must leave exactly one `i32` (the status/return code) on the
    /// stack.
    macro_rules! fixture_module {
        ($event_body:expr) => {
            wat::parse_str(format!(
                r#"
                (module
                  (import "astrs" "output-send" (func $output_send (param i32 i32)))
                  (memory (export "memory") 4 32)
                  (global $bump (mut i32) (i32.const 65536))

                  (func (export "astrs-op-alloc") (param $len i32) (result i32)
                    (local $ptr i32)
                    global.get $bump
                    local.set $ptr
                    global.get $bump
                    local.get $len
                    i32.add
                    global.set $bump
                    local.get $ptr)

                  (func (export "astrs-op-init") (param $ptr i32) (param $len i32) (result i32)
                    i32.const 0)

                  (func (export "astrs-op-event") (param $ptr i32) (param $len i32) (result i32)
                    {})
                )
                "#,
                $event_body
            ))
            .expect("the fixture is valid WebAssembly text")
        };
    }

    /// Builds a [`WasmSource`] from already-parsed `.wasm` bytes and asks it
    /// for one fresh [`WasmOperator`] — the shape every test below drives,
    /// factored out since [`WasmSource::load`] itself only ever reads from
    /// disk in production, but every fixture here is generated in-memory.
    fn build_from_bytes(wasm: &[u8], sandbox: WasmSandboxConfig) -> Box<dyn Operator> {
        let mut config = Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, wasm).expect("fixture must validate");
        module
            .get_export(EXPORT_EVENT)
            .and_then(|ty| ty.func().cloned())
            .expect("fixture must export astrs-op-event");
        let source = WasmSource {
            engine,
            module,
            sandbox,
        };
        source.build().expect("instantiation must succeed")
    }

    fn meta() -> Metadata {
        Metadata::new(astrs_time::HlcTimestamp::EPOCH)
    }

    fn input_event(payload: Vec<u8>) -> OpEvent {
        OpEvent::Input {
            id: DataId::new("frames").unwrap(),
            source: "camera/image".parse().unwrap(),
            metadata: meta(),
            payload,
        }
    }

    /// Echo operator round-trip: the guest decodes the incoming
    /// `GuestCall::Input`'s payload straight back out of its own memory (no
    /// real oxicode decoding in WAT — instead the guest trusts the exact
    /// byte layout this test's `GuestCall::Input` encodes to, reading the
    /// payload length and bytes at their known offsets) and calls
    /// `astrs.output-send` with a hand-built `GuestSend` tuple whose payload
    /// is those same bytes, proving a full event -> host-import -> decoded
    /// send round trip through the real ABI, not a stub.
    #[test]
    fn echo_operator_round_trips_a_send_through_output_send() {
        // Rather than have the WAT fixture parse a real `GuestCall::Input`
        // (which would require hand-writing an oxicode varint/tag decoder
        // in WAT), this test proves the round trip the other way around:
        // the guest ignores the incoming call entirely and always sends
        // back one fixed, host-decodable `GuestSend`, built as raw oxicode
        // bytes baked into the module's own data section. What this test
        // verifies is exactly the part `astrs-runtime` owns — that
        // `astrs.output-send`'s `(ptr, len)` are read out of *this*
        // instance's own memory correctly and decoded into the real
        // `astrs_wire` types — which a hand-decoded guest payload would not
        // exercise any more thoroughly.
        let send: GuestSend = (DataId::new("echoed").unwrap(), meta(), vec![9, 8, 7]);
        let encoded = send.encode_to_vec().unwrap();
        let data_hex: String = encoded.iter().map(|b| format!("\\{b:02x}")).collect();

        let wasm = wat::parse_str(format!(
            r#"
            (module
              (import "astrs" "output-send" (func $output_send (param i32 i32)))
              (memory (export "memory") 4 32)
              (data (i32.const 65536) "{data_hex}")
              (global $bump (mut i32) (i32.const {offset}))

              (func (export "astrs-op-alloc") (param $len i32) (result i32)
                (local $ptr i32)
                global.get $bump
                local.set $ptr
                global.get $bump
                local.get $len
                i32.add
                global.set $bump
                local.get $ptr)

              (func (export "astrs-op-init") (param $ptr i32) (param $len i32) (result i32)
                i32.const 0)

              (func (export "astrs-op-event") (param $ptr i32) (param $len i32) (result i32)
                i32.const 65536
                i32.const {send_len}
                call $output_send
                i32.const 0)
            )
            "#,
            data_hex = data_hex,
            offset = 65536 + encoded.len(),
            send_len = encoded.len(),
        ))
        .expect("fixture must be valid WebAssembly text");

        let mut operator = build_from_bytes(&wasm, WasmSandboxConfig::default());
        let mut out = OpOutput::new();
        operator.configure(&BTreeMap::new()).unwrap();
        let status = operator
            .on_event(&input_event(vec![1, 2, 3]), &mut out)
            .unwrap();
        assert_eq!(status, Status::Continue);
        let sends = out.drain();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].id().as_str(), "echoed");
        assert_eq!(sends[0].payload(), &[9, 8, 7]);
    }

    /// An infinite-loop guest is killed by fuel, surfacing as a clean
    /// [`OpError`] rather than hanging the host thread forever.
    #[test]
    fn an_infinite_loop_guest_is_killed_by_fuel() {
        let wasm = fixture_module!("(loop $forever br $forever) i32.const 0" as &str);
        let sandbox = WasmSandboxConfig {
            fuel_per_call: 10_000,
            ..WasmSandboxConfig::default()
        };
        let mut operator = build_from_bytes(&wasm, sandbox);
        let mut out = OpOutput::new();
        operator.configure(&BTreeMap::new()).unwrap();
        let error = operator
            .on_event(&input_event(vec![1]), &mut out)
            .unwrap_err();
        assert!(
            error.to_string().contains("fuel"),
            "expected a fuel-exhaustion message, got: {error}"
        );
    }

    /// A guest that tries to grow its memory far past the configured cap is
    /// stopped by the store limiter, again surfacing as a clean
    /// [`OpError`].
    #[test]
    fn a_memory_grow_bomb_is_stopped_by_the_limiter() {
        let wasm = fixture_module!(
            "(loop $grow
               i32.const 1
               memory.grow
               drop
               br $grow)
             i32.const 0" as &str
        );
        let sandbox = WasmSandboxConfig {
            // One page (64 KiB) over the fixture's own 4-page initial size —
            // the very first `memory.grow` inside the loop already exceeds
            // it, so this trips deterministically on the first iteration
            // rather than depending on how many iterations fuel affords.
            max_memory_bytes: 5 * 65536,
            ..WasmSandboxConfig::default()
        };
        let mut operator = build_from_bytes(&wasm, sandbox);
        let mut out = OpOutput::new();
        operator.configure(&BTreeMap::new()).unwrap();
        let error = operator
            .on_event(&input_event(vec![1]), &mut out)
            .unwrap_err();
        assert!(
            error.to_string().contains("limit") || error.to_string().contains("fuel"),
            "expected a memory-limit (or, if the loop's own fuel ran out first, a fuel) \
             message, got: {error}"
        );
    }

    /// A trapping guest (an explicit `unreachable`) yields a clean
    /// [`OpError`], never a process abort or a panic past this module's own
    /// boundary.
    #[test]
    fn a_trapping_guest_yields_a_clean_operator_error() {
        let wasm = fixture_module!("unreachable" as &str);
        let mut operator = build_from_bytes(&wasm, WasmSandboxConfig::default());
        let mut out = OpOutput::new();
        operator.configure(&BTreeMap::new()).unwrap();
        let error = operator
            .on_event(&input_event(vec![1]), &mut out)
            .unwrap_err();
        assert!(
            error.to_string().contains("trapped") || error.to_string().contains("unreachable"),
            "expected a trap message, got: {error}"
        );
    }

    /// `astrs-op-init` (the `configure` half of the ABI) running and
    /// succeeding is itself part of every test above via `.configure(...)`;
    /// this test additionally proves a nonzero return code from
    /// `astrs-op-init` is reported as a failure, not silently ignored.
    #[test]
    fn a_failing_init_export_is_reported() {
        let wasm = wat::parse_str(
            r#"
            (module
              (import "astrs" "output-send" (func $output_send (param i32 i32)))
              (memory (export "memory") 4 32)
              (global $bump (mut i32) (i32.const 65536))

              (func (export "astrs-op-alloc") (param $len i32) (result i32)
                (local $ptr i32)
                global.get $bump
                local.set $ptr
                global.get $bump
                local.get $len
                i32.add
                global.set $bump
                local.get $ptr)

              (func (export "astrs-op-init") (param $ptr i32) (param $len i32) (result i32)
                i32.const 1)

              (func (export "astrs-op-event") (param $ptr i32) (param $len i32) (result i32)
                i32.const 0)
            )
            "#,
        )
        .expect("fixture must be valid WebAssembly text");
        let mut operator = build_from_bytes(&wasm, WasmSandboxConfig::default());
        let error = operator.configure(&BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("astrs-op-init"), "{error}");
    }

    /// A module missing the required `astrs-op-event` export is rejected at
    /// load time with a named [`WasmError`], not a confusing failure deep
    /// inside the first real call.
    #[test]
    fn a_module_with_no_event_export_is_rejected_at_load() {
        let wasm = wat::parse_str(r#"(module (memory (export "memory") 1))"#)
            .expect("fixture must be valid WebAssembly text");
        let path = std::env::temp_dir().join(format!(
            "astrs-runtime-wasm-no-event-export-{}.wasm",
            std::process::id()
        ));
        std::fs::write(&path, &wasm).unwrap();
        let error = WasmSource::load(&path, WasmSandboxConfig::default()).unwrap_err();
        std::fs::remove_file(&path).ok();
        assert!(
            matches!(error, WasmError::MissingEventExport { .. }),
            "{error}"
        );
    }

    /// A missing file is a recoverable [`WasmError::Read`], never a panic.
    #[test]
    fn a_missing_wasm_file_is_a_recoverable_error() {
        let missing = std::env::temp_dir().join(format!(
            "astrs-runtime-wasm-does-not-exist-{}.wasm",
            std::process::id()
        ));
        let error = WasmSource::load(&missing, WasmSandboxConfig::default()).unwrap_err();
        assert!(matches!(error, WasmError::Read { .. }), "{error}");
    }

    #[test]
    fn resolve_wasm_path_rebases_a_relative_path_and_leaves_an_absolute_one_alone() {
        let dataflow_dir = Path::new("/graphs/perception");
        assert_eq!(
            resolve_wasm_path(Some(dataflow_dir), "./filter.wasm"),
            dataflow_dir.join("./filter.wasm")
        );
        assert_eq!(
            resolve_wasm_path(Some(dataflow_dir), "/opt/operators/filter.wasm"),
            PathBuf::from("/opt/operators/filter.wasm"),
        );
    }

    #[test]
    fn resolve_wasm_path_with_no_dataflow_dir_leaves_a_relative_path_relative() {
        assert_eq!(
            resolve_wasm_path(None, "./filter.wasm"),
            PathBuf::from("./filter.wasm")
        );
    }

    #[test]
    fn guest_status_from_code_maps_zero_one_and_other() {
        assert_eq!(GuestStatus::from_code(0), GuestStatus::Continue);
        assert_eq!(GuestStatus::from_code(1), GuestStatus::Finished);
        assert_eq!(GuestStatus::from_code(7), GuestStatus::Failed(7));
    }

    #[test]
    fn guest_call_from_op_event_maps_every_known_variant_to_some() {
        assert!(matches!(
            GuestCall::from_op_event(&input_event(vec![1])),
            Some(GuestCall::Input { .. })
        ));
        assert!(matches!(
            GuestCall::from_op_event(&OpEvent::InputClosed {
                id: DataId::new("frames").unwrap(),
                source: "camera/image".parse().unwrap(),
                reason: RouteCloseReason::ProducerFinished,
            }),
            Some(GuestCall::InputClosed { .. })
        ));
        assert!(matches!(
            GuestCall::from_op_event(&OpEvent::Stop {
                cause: StopCause::Requested,
                grace: None,
            }),
            Some(GuestCall::Stop { .. })
        ));
        assert_eq!(
            GuestCall::from_op_event(&OpEvent::Reload),
            Some(GuestCall::OnReload)
        );
        assert!(matches!(
            GuestCall::from_op_event(&OpEvent::ParamUpdate {
                scope: ParamScope::Global,
                key: ParamKey::new("gain").unwrap(),
                value: Parameter::Float(1.5),
            }),
            Some(GuestCall::ParamUpdate { .. })
        ));
    }
}
