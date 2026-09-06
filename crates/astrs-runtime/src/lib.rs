//! The AstRS operator host process.
//!
//! A node-shaped process that hosts several in-process operators for
//! low-latency chains (blueprint §4.2, §9.3):
//!
//! - One shared demux loop feeding every hosted operator, so a crop → NMS
//!   chain costs a function call rather than an IPC hop.
//! - A thread per operator with panic isolation: a misbehaving stage fails
//!   its operator, not the host.
//! - The reload hook for live operator replacement.
//!
//! ```text
//!               ┌────────────────────── RuntimeHost ──────────────────────┐
//!  daemon/wire ─┤ Node + EventStream        demux         operator threads│─ daemon/wire
//! (node-level   │      │                      │            (crop) (nms)   │ (node-level
//!  inputs)      │      └── Event::Input ──▶ Routing ──▶ OperatorInbox ──▶ Operator::on_event
//!  outputs) ◀───┤                                        └─ sibling wire ─┘│
//!               └───────────────────────────────────────────────────────────┘
//! ```
//!
//! # The pieces
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`config`] | [`RuntimeConfig`] — the manifest's `operators:` list plus the compiled-in registry |
//! | [`routing`] | Resolving that list onto one connected node's ports (blueprint §9.3's "astrs-runtime's concern") |
//! | [`inbox`] | `OperatorInbox` — one operator's bounded, policy-aware, priority-preempting queue |
//! | [`output_sink`] | The shared, thread-safe front door onto the node's own outputs |
//! | [`worker`] | `operator_loop` — one operator's thread: hooks, panic isolation, in-place restart |
//! | [`host`] | [`RuntimeHost`] — ties every piece together: the demux loop and the shutdown sequence |
//! | [`report`] | [`RuntimeReport`] and friends — what [`RuntimeHost::run`] hands back |
//! | [`error`] | [`RuntimeError`] |
//!
//! # Where operators come from
//!
//! By default an operator is a compiled-in type registered with
//! `astrs::register_operator!` and looked up by name in the
//! [`astrs_operator_api::OperatorRegistry`] this binary was built with — the
//! flagship path of blueprint §9.3, and the only one with no loading step at
//! all. Two optional features add the manifest's other operator source kinds,
//! both off by default so a runtime that never uses them carries neither the
//! loader nor the interpreter:
//!
//! | Feature | Module | Manifest source kind |
//! |---|---|---|
//! | `dylib-operators` | `dylib` | `operators[].dylib: <path>` — a platform shared library opened through `libloading` |
//! | `wasm-operators` | `wasm` | `operators[].wasm: <path>` — a WebAssembly module run on the pure-Rust `wasmi` interpreter |
//!
//! (The two module names are plain code spans rather than intra-doc links:
//! each module only exists when its own feature is on, and a link to a
//! `cfg`-ed-out module is a broken link in every build that does not enable
//! it.)
//!
//! Neither compiles C: `libloading` is a safe wrapper over the platform's own
//! `dlopen`/`LoadLibrary` (FFI declarations to a platform service, which
//! blueprint §18.1 permits), and `wasmi` is a pure-Rust interpreter rather
//! than a JIT with a native codegen backend.
//!
//! # Why not a channel in front of `OperatorInbox`
//!
//! Blueprint §9.3 asks for "a bounded channel" per operator, and §11.2 asks
//! for manifest `queue_size`/`queue_policy` honored *per input*. A single
//! [`std::sync::mpsc::sync_channel`] or `tokio::sync::mpsc::channel` gives
//! exactly one FIFO capacity for every message an operator receives
//! together — it has no notion of "this input drops the oldest message,
//! that one buffers to ten times its size before dropping", no eviction
//! immunity for correlated messages, and no way to let a `Stop` or a
//! `Reload` pre-empt a backlog of ordinary frames. Reaching for one of
//! those channel types and then trying to bolt per-input policy on top of
//! it would mean re-deriving [`astrs_scheduler::InputQueue`] and
//! [`astrs_scheduler::EventMux`] badly, in this crate, from scratch.
//! `OperatorInbox` instead builds directly on those two
//! types — the exact primitives `astrs-node-api`'s own per-node inbox
//! ([`astrs_node_api::events::EventSource`]) is built from — plus
//! [`astrs_node_api::Signal`] for the blocking wakeup an operator's plain
//! [`std::thread`] needs (no tokio runtime lives on that thread). That
//! *is* "a bounded channel" in every sense blueprint §9.3 cares about; it
//! is simply not [`std::sync::mpsc`], because [`std::sync::mpsc`] cannot
//! express the policy blueprint §11.2 requires.
//!
//! # Threading model
//!
//! Every hosted operator runs on its own [`std::thread`] (blueprint §9.3),
//! spawned inside one [`std::thread::scope`] call so [`RuntimeHost::run`]
//! can borrow its own `Routing`,
//! `OperatorInbox`es and `OutputSink`
//! into every worker closure without an `Arc` — none of those types need
//! to outlive the run, and each already provides its own internal
//! synchronization. Restart happens *in place*, on the failing operator's
//! own thread (see [`crate::worker`]'s module docs for why): a sibling's
//! delivery is never stalled by another operator's backoff sleep.
//!
//! # Shutdown ordering
//!
//! Blueprint §9.3: *"`Stop` -> drain channels -> `on_stop` each operator ->
//! close outputs"*. Concretely: the demux loop (this process's own
//! [`astrs_node_api::EventStream::recv`]) returns an [`astrs_node_api::Event::Stop`]
//! or ends outright (the session simply closing counts as an implied
//! [`astrs_wire::StopCause::DaemonShutdown`]); every operator inbox not
//! already permanently closed gets that `Stop` pushed onto its control
//! lane, which — like every control-lane item — pre-empts whatever data
//! backlog is still queued for that operator (the same rule
//! [`astrs_node_api::EventStream`] itself applies to its own `Stop`
//! delivery); [`RuntimeHost::run`] then joins every worker thread, each of
//! which called `on_stop` exactly once on its way out; only once every
//! thread has joined does it close the node's own outputs — a flush from
//! `on_stop` has to find its output still open.

pub mod config;
#[cfg(feature = "dylib-operators")]
pub mod dylib;
pub mod error;
pub mod host;
pub mod inbox;
pub mod operator_config;
pub mod output_sink;
pub mod report;
pub mod routing;
#[cfg(feature = "wasm-operators")]
pub mod wasm;
pub mod worker;

pub use config::{RuntimeConfig, WasmSandboxConfig};
pub use error::{Result, RuntimeError};
pub use host::{RuntimeHost, run_runtime};
pub use operator_config::ConfigValueError;
pub use report::{FailedHook, OperatorFailure, OperatorOutcome, OperatorReport, RuntimeReport};
