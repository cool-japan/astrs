//! **AstRS** — the Pure Rust dataflow middleware for the robotic age.
//!
//! This is the facade: the one dependency an application adds
//! (blueprint §5.2). Add it, pick your features, import
//! `astrs::prelude`, write your node.
//!
//! ```toml
//! [dependencies]
//! astrs = "0.1"
//! ```
//!
//! # A node, end to end
//!
// The quickstart is the `node` feature's surface, so it only compiles when
// that feature is on. The fence — not the body — carries the condition, so
// the example a reader sees is the same either way and only its execution
// as a doctest is gated.
#![cfg_attr(feature = "node", doc = "```no_run")]
#![cfg_attr(not(feature = "node"), doc = "```ignore")]
//! use astrs::prelude::*;
//!
//! /// A message type: the derive maps the struct onto the closed columnar
//! /// type set (blueprint §6.1) and pins its type URN (§24.3), so the
//! /// manifest's `output_types:` and this declaration are checked against
//! /// each other rather than merely agreeing by convention.
//! #[derive(AstrsMessage)]
//! #[astrs(urn = "std/vision/v1/Detections")]
//! struct Detections {
//!     scores: Vec<f32>,
//!     labels: Vec<u32>,
//! }
//!
//! fn main() -> Result<(), NodeError> {
//!     // Reads the daemon's handshake out of the environment the spawner
//!     // set up; `Node::builder()` is there when something needs overriding,
//!     // and `Node::init_testing()` runs the same code against an
//!     // in-process daemon in a unit test.
//!     let (mut node, mut events) = Node::init_from_env()?;
//!     let mut detections = node.output::<Detections>("detections")?;
//!
//!     while let Some(event) = events.recv() {
//!         match event {
//!             Event::Input { id, data, meta } if id == "frames" => {
//!                 let found = Detections {
//!                     scores: vec![0.98],
//!                     labels: vec![7],
//!                 };
//!                 // `meta.follow()` carries the causal chain forward, so a
//!                 // recording can reconstruct publish → deliver → process.
//!                 detections.send(found, meta.follow())?;
//!                 let _ = data;
//!             }
//!             Event::InputClosed { .. } => {}
//!             Event::Stop(_) => break,
//!             _ => {}
//!         }
//!     }
//!     Ok(())
//! }
//! ```
//!
//! The event stream is an `Iterator` and a `Stream`, so the same loop works
//! synchronously or under `async`; after `Stop` it fuses.
//!
//! # Features
//!
//! The default is **node authoring and nothing else** — what an
//! application's `main` uses. Everything else is opt-in, so adding this
//! crate costs a node exactly what a node uses.
//!
//! | Feature | Default | What it re-exports |
//! |---|---|---|
//! | `node` | **yes** | `astrs::node_api` — `Node`, `Event`, outputs, patterns, the testing harness. Implies `data`, `time`, `log`, `wire`, `derive`. |
//! | `derive` | via `node` | `#[derive(AstrsMessage)]` and `#[operator]` |
//! | `data` | via `node` | `astrs::data` — columnar arrays, schemas, Arrow-IPC wire compatibility |
//! | `time` | via `node` | `astrs::time` — HLC clocks, deadlines, `Stamped<T>` |
//! | `log` | via `node` | `astrs::log` — structured records, rotation, the `astrs/logs` fan-out |
//! | `wire` | via `node` | `astrs::wire` — the protocol types a node's own signatures name |
//! | `arrow-interop` | no | zero-copy conversions to/from arrow-rs (§6.1) and `send_arrow` (§9.1). Implies `node`, and pulls arrow-rs in. |
//! | `operator` | no | `astrs::operator_api` — the `Operator` trait, `OperatorRegistry` and `register_operator!` |
//! | `runtime` | no | `astrs::runtime` — the operator host. Implies `operator`. |
//! | `manifest` | no | `astrs::manifest` — YAML descriptor parse/validate/expand |
//! | `graph` | no | `astrs::graph` — the dataflow graph, type checking, placement. Implies `manifest`. |
//! | `verify` | no | `astrs::verify` — SMT graph proofs (§15). Implies `graph`, and pulls in the solver. |
//! | `recording` | no | `astrs::recording` — the `.arec` container |
//! | `telemetry` | no | `astrs::telemetry` — metrics registry and OTLP export |
//! | `tui` | no | `astrs::tui` — the live monitor behind `astrs top` |
//! | `full` | no | everything above |
//!
//! **Deferred, and named here so its absence is a decision rather than an
//! oversight:** there is no `ros2` feature yet. The ROS 2 pillar
//! (`astrs-cdr`, `astrs-rtps`, `astrs-idl`, `astrs-ros2`, `astrs-rosbag`,
//! `astrs-tf`, `astrs-urdf`, blueprint §10) is fully implemented and usable
//! today, directly or through `astrs-cli`/`astrs-ros2-bridge-node` — it
//! simply predates this facade's own feature-flag aggregation and hasn't
//! been wired in yet, so `cargo add astrs --features ros2` isn't a thing
//! yet. It joins the table when that aggregation lands, as do the
//! `astrs-python` bindings (§5.3).
//!
//! # Why a facade at all
//!
//! Two reasons, both about the dependency graph rather than convenience.
//!
//! An application should name **one** version, not fourteen that must agree.
//! Every crate here is dual-pinned at the workspace level, so `astrs = "0.1"`
//! resolves a set that was built and tested together.
//!
//! And the layering should be visible. A node depends on the node API; it
//! does not depend on the coordinator, the daemon, or the transport, and no
//! feature here lets it. `astrs-daemon`, `astrs-coordinator` and
//! `astrs-transport` are deliberately **absent** from this facade: they are
//! what the `astrs` binary embeds, not what an application links.
//!
//! # Where to look next
//!
//! | Question | Crate |
//! |---|---|
//! | How do I write a node? | `astrs::node_api`, and `astrs::prelude` |
//! | What can a manifest say? | `astrs::manifest` (blueprint §8) |
//! | Is my graph sound? | `astrs::verify` (§15), or `astrs validate --prove` |
//! | How do I run one? | the `astrs` binary — `astrs run graph.yml` |
//! | What does *this* build contain? | [`build_info`] |

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod build_info;

// ---------------------------------------------------------------------------
// Re-exports. Each is the underlying crate itself rather than a hand-copied
// subset: a facade that re-lists items drifts from what it fronts, and the
// drift is invisible until someone needs the item that was missed.
// ---------------------------------------------------------------------------

/// The node API: [`Node`], the event stream, typed and raw outputs,
/// service/action/stream patterns, and the in-process testing harness
/// (blueprint §9.1).
#[cfg(feature = "node")]
#[cfg_attr(docsrs, doc(cfg(feature = "node")))]
pub use astrs_node_api as node_api;

/// The columnar data plane: arrays, schemas, type URNs and Arrow-IPC
/// stream compatibility (blueprint §6.1).
#[cfg(feature = "data")]
#[cfg_attr(docsrs, doc(cfg(feature = "data")))]
pub use astrs_data as data;

/// Clocks and deadlines: the hybrid logical clock, monotonic and wall
/// time, `Stamped<T>` (blueprint §5.2).
#[cfg(feature = "time")]
#[cfg_attr(docsrs, doc(cfg(feature = "time")))]
pub use astrs_time as time;

/// Structured logging: records, rotation, and the `astrs/logs` virtual
/// input's fan-out (blueprint §13).
#[cfg(feature = "log")]
#[cfg_attr(docsrs, doc(cfg(feature = "log")))]
pub use astrs_log as log;

/// The wire protocol types (blueprint §7): metadata, ids, stop causes —
/// the vocabulary a node's own signatures name.
#[cfg(feature = "wire")]
#[cfg_attr(docsrs, doc(cfg(feature = "wire")))]
pub use astrs_wire as wire;

/// The operator API: the [`Operator`] trait,
/// `register_operator!`, and the event/output shims (blueprint §9.3).
#[cfg(feature = "operator")]
#[cfg_attr(docsrs, doc(cfg(feature = "operator")))]
pub use astrs_operator_api as operator_api;

/// The path AstRS's own procedural macros reach their runtime crates by.
///
/// **Not public API.** It exists because Rust puts only a crate's *direct*
/// dependencies in its extern prelude: an application whose whole
/// `[dependencies]` section is `astrs = "0.1"` cannot name `astrs_data`, so
/// the `#[derive(AstrsMessage)]` expansion cannot emit `::astrs_data::…` and
/// have it resolve. `astrs-operator-macros` detects that case from the
/// invoking crate's manifest and emits a path through this module instead —
/// see that crate's `crate_path` module for the full resolution order.
///
/// Nothing here is covered by semantic versioning; name
/// [`data`](crate::data) / [`operator_api`](crate::operator_api) instead.
#[doc(hidden)]
pub mod __private {
    #[cfg(feature = "data")]
    pub use astrs_data as data;

    #[cfg(feature = "operator")]
    pub use astrs_operator_api as operator_api;
}

/// The operator host: the shared event loop, per-operator threads and
/// panic isolation (blueprint §5.2).
#[cfg(feature = "runtime")]
#[cfg_attr(docsrs, doc(cfg(feature = "runtime")))]
pub use astrs_runtime as runtime;

/// The dataflow manifest: parse, validate, expand modules, emit the JSON
/// schema (blueprint §8).
#[cfg(feature = "manifest")]
#[cfg_attr(docsrs, doc(cfg(feature = "manifest")))]
pub use astrs_manifest as manifest;

/// The dataflow graph: edges, type checking, cycles, pattern pairing,
/// placement planning and visualization (blueprint §5.2).
#[cfg(feature = "graph")]
#[cfg_attr(docsrs, doc(cfg(feature = "graph")))]
pub use astrs_graph as graph;

/// Graph proofs: deadlock freedom, queue boundedness, rate consistency,
/// latency budgets and type-rule consistency, discharged through an SMT
/// solver (blueprint §15). The engine behind `astrs validate --prove`.
#[cfg(feature = "verify")]
#[cfg_attr(docsrs, doc(cfg(feature = "verify")))]
pub use astrs_verify as verify;

/// The `.arec` recording container: writer, reader, merger (blueprint §14).
#[cfg(feature = "recording")]
#[cfg_attr(docsrs, doc(cfg(feature = "recording")))]
pub use astrs_recording as recording;

/// Metrics, tracing setup and the OTLP/HTTP exporter (blueprint §13).
#[cfg(feature = "telemetry")]
#[cfg_attr(docsrs, doc(cfg(feature = "telemetry")))]
pub use astrs_telemetry as telemetry;

/// The live terminal monitor behind `astrs top` (blueprint §5.2).
#[cfg(feature = "tui")]
#[cfg_attr(docsrs, doc(cfg(feature = "tui")))]
pub use astrs_tui as tui;

// ---------------------------------------------------------------------------
// Flattened conveniences: the handful of names a node's `main` says out
// loud, promoted to the crate root so `astrs::Node` works without an import
// path that names an implementation crate.
// ---------------------------------------------------------------------------

#[cfg(feature = "node")]
#[cfg_attr(docsrs, doc(cfg(feature = "node")))]
pub use astrs_node_api::{Event, EventStream, Node, NodeBuilder, NodeError, Output, RawOutput};

/// `#[derive(AstrsMessage)]` — maps a struct onto the closed columnar type
/// set and pins its type URN (blueprint §9.2).
///
/// Named in both the macro and the type namespace, exactly as `serde`
/// offers `Serialize`: the line below re-exports the *trait*, this one the
/// *derive*, and the two never collide. Importing [`prelude`] brings in
/// both.
#[cfg(feature = "derive")]
#[cfg_attr(docsrs, doc(cfg(feature = "derive")))]
pub use astrs_operator_macros::AstrsMessage;

#[cfg(feature = "data")]
#[cfg_attr(docsrs, doc(cfg(feature = "data")))]
pub use astrs_data::AstrsMessage;

/// `#[astrs::operator]` — the attribute form of a registry entry
/// (blueprint §9.3), giving a type an inherent `operator_entry()` that
/// produces exactly what [`register_operator!`] does.
#[cfg(all(feature = "derive", feature = "operator"))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "derive", feature = "operator"))))]
pub use astrs_operator_macros::operator;

/// `register_operator!(MyOp)` — one `(name, constructor)` entry for
/// [`OperatorRegistry::from_entries`] (blueprint §9.3).
///
/// Blueprint §9.3 writes this call as `astrs::register_operator!(MyOp)`, and
/// `astrs new operator`'s scaffold does too, so the macro has to be reachable
/// at *this* crate's root and not only at `astrs::operator_api`'s. A
/// `#[macro_export]` macro carries its definition site with it, so the
/// `$crate` inside the expansion still resolves to `astrs-operator-api` even
/// when the calling crate has never named that crate.
#[cfg(feature = "operator")]
#[cfg_attr(docsrs, doc(cfg(feature = "operator")))]
pub use astrs_operator_api::register_operator;

/// The table `register_operator!` entries are collected into, and the one an
/// operator host builds instances from (blueprint §9.3).
#[cfg(feature = "operator")]
#[cfg_attr(docsrs, doc(cfg(feature = "operator")))]
pub use astrs_operator_api::{Operator, OperatorRegistry};

/// Everything a node's `main` needs, in one import.
///
/// ```no_run
/// use astrs::prelude::*;
///
/// fn main() -> Result<(), NodeError> {
///     let (mut node, mut events) = Node::init_from_env()?;
///     let mut echo = node.raw_output("echo")?;
///     while let Some(event) = events.recv() {
///         match event {
///             Event::Input { data, meta, .. } => {
///                 echo.send_bytes(data.to_vec(), meta.follow())?;
///             }
///             Event::Stop(_) => break,
///             _ => {}
///         }
///     }
///     Ok(())
/// }
/// ```
///
/// What is *not* here is as deliberate as what is: no manifest types, no
/// graph model, no coordinator client. A node does not parse its own
/// manifest — the daemon hands it its descriptor — and a prelude that
/// implied otherwise would teach the wrong architecture.
#[cfg(feature = "node")]
#[cfg_attr(docsrs, doc(cfg(feature = "node")))]
pub mod prelude {
    pub use astrs_node_api::prelude::*;

    /// The stream-pattern types an application names when it *reads* a
    /// segment (blueprint §9.4), rather than only writing one.
    ///
    /// A deliberate superset of `astrs-node-api`'s own prelude: reading a
    /// chunk means matching on its `(session, segment, seq, fin)`
    /// reference, and a prelude that made writing ergonomic while leaving
    /// the reading half to a longer path would be a prelude that only
    /// covers half the pattern. Found by `tests/one_dependency.rs`, which
    /// exists to find exactly this.
    pub use astrs_node_api::patterns::{ChunkRef, StreamSegment};

    #[cfg(feature = "derive")]
    #[cfg_attr(docsrs, doc(cfg(feature = "derive")))]
    pub use astrs_operator_macros::AstrsMessage;

    /// The operator surface, for a crate that hosts operators rather than
    /// (or as well as) running as a node.
    ///
    /// [`OperatorRegistry`] and
    /// [`register_operator!`](astrs_operator_api::register_operator) are part
    /// of it: blueprint §9.3's sample builds its table in the same file that
    /// declares the operator, so a prelude that carried the trait but not the
    /// registration would leave the shorter half of that sample unimportable.
    #[cfg(feature = "operator")]
    #[cfg_attr(docsrs, doc(cfg(feature = "operator")))]
    pub use astrs_operator_api::{
        OpError, OpEvent, OpOutput, OpResult, Operator, OperatorConstructor, OperatorRegistry,
        Status, register_operator,
    };

    /// `#[astrs::operator]`, the attribute form of a registry entry.
    #[cfg(all(feature = "derive", feature = "operator"))]
    #[cfg_attr(docsrs, doc(cfg(all(feature = "derive", feature = "operator"))))]
    pub use astrs_operator_macros::operator;
}

/// The version of AstRS this facade fronts.
///
/// Every crate it re-exports carries the same version — they are released
/// together from one workspace — so this is *the* AstRS version, not merely
/// the facade's.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// This crate's name, as a log line or a bug report writes it.
pub const NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    #[test]
    fn version_is_the_workspace_version() {
        assert_eq!(super::VERSION, env!("CARGO_PKG_VERSION"));
        assert!(super::VERSION.starts_with("0."));
    }

    #[test]
    #[cfg(feature = "node")]
    fn the_node_surface_is_reachable_from_the_root() {
        // Naming the types is the test: if a re-export were dropped or
        // renamed, this stops compiling.
        fn uses_types(
            _: Option<super::Event>,
            _: Option<super::NodeBuilder>,
            _: Option<super::EventStream>,
        ) -> std::result::Result<(), super::NodeError> {
            Ok(())
        }
        assert!(uses_types(None, None, None).is_ok());
    }

    #[test]
    #[cfg(feature = "node")]
    fn the_prelude_carries_the_node_vocabulary() {
        use super::prelude::*;
        let _: Option<Event> = None;
        let _: Option<NodeBuilder> = None;
        fn _returns(_: Result<()>) {}
    }

    #[test]
    #[cfg(feature = "derive")]
    fn the_derive_and_the_trait_share_one_name() {
        use super::AstrsMessage;

        #[derive(AstrsMessage)]
        #[astrs(urn = "std/test/v1/Reading")]
        struct Reading {
            value: Vec<f64>,
        }

        // The trait half of the same name resolves too.
        assert_eq!(<Reading as AstrsMessage>::URN, "std/test/v1/Reading");
        let _ = Reading { value: vec![1.0] };
    }

    #[test]
    #[cfg(feature = "manifest")]
    fn the_manifest_surface_is_reachable() {
        let manifest =
            super::manifest::Manifest::from_yaml_str("nodes:\n  - id: solo\n    path: ./solo\n")
                .expect("valid manifest");
        assert_eq!(manifest.nodes.len(), 1);
    }

    #[test]
    #[cfg(feature = "graph")]
    fn the_graph_surface_is_reachable() {
        let manifest =
            super::manifest::Manifest::from_yaml_str("nodes:\n  - id: solo\n    path: ./solo\n")
                .expect("valid manifest");
        manifest.validate().expect("valid");
        let (graph, _) =
            super::graph::DataflowGraph::from_manifest(&manifest).expect("graph builds");
        assert_eq!(graph.node_count(), 1);
    }

    #[test]
    #[cfg(feature = "verify")]
    fn the_verify_surface_proves_through_the_facade() {
        let manifest = super::manifest::Manifest::from_yaml_str(
            "
nodes:
  - id: a
    path: ./a
    inputs: { i: b/out }
    outputs: [out]
  - id: b
    path: ./b
    inputs: { i: a/out }
    outputs: [out]
",
        )
        .expect("valid manifest");
        manifest.validate().expect("valid");
        let (graph, _) =
            super::graph::DataflowGraph::from_manifest(&manifest).expect("graph builds");
        let report = super::verify::prove(&graph, &super::verify::ProveOptions::default())
            .expect("model builds");
        assert!(report.has_violations(), "this graph deadlocks");
    }

    #[test]
    #[cfg(feature = "operator")]
    fn the_operator_surface_is_reachable() {
        use super::operator_api::{Operator, Status};

        #[derive(Default)]
        struct Noop;
        impl Operator for Noop {
            fn on_event(
                &mut self,
                _event: &super::operator_api::OpEvent,
                _out: &mut super::operator_api::OpOutput,
            ) -> super::operator_api::OpResult<Status> {
                Ok(Status::Continue)
            }
        }

        // Registering it exercises the registry re-export as well as the
        // trait's object safety.
        let mut registry = super::operator_api::OperatorRegistry::new();
        registry
            .register(
                "Noop",
                Box::new(|| Box::<Noop>::default() as Box<dyn Operator>),
            )
            .expect("one operator, one name");
        let mut built = registry.build("Noop").expect("registered");
        let mut out = super::operator_api::OpOutput::new();
        let stop = super::operator_api::OpEvent::Stop {
            cause: super::wire::StopCause::Requested,
            grace: None,
        };
        assert_eq!(
            built.on_event(&stop, &mut out).expect("no-op never fails"),
            Status::Continue
        );
    }
}
