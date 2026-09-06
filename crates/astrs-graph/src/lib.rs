//! The AstRS dataflow graph model.
//!
//! What a validated manifest becomes before anything is spawned
//! (blueprint §5.2, §7.7):
//!
//! - The graph model: nodes, ports, edges, virtual sources and modules —
//!   see [`DataflowGraph::from_manifest`].
//! - Edge type checking against declared type URNs, with `type: any` as an
//!   explicit opt-out rather than a default — see [`EdgeTypeStatus`] and
//!   [`DataflowGraph::diagnostics`].
//! - Stable topological metadata: strongly-connected-component (cycle)
//!   detection, and service/action pattern pairing — see [`Scc`] and
//!   [`PatternPair`].
//! - The placement planner mapping nodes onto daemons and deciding which
//!   edges are same-host versus cross-host — see [`plan_placement`].
//! - Visualization to mermaid and DOT for `astrs graph` — see [`to_mermaid`]
//!   and [`to_dot`].
//! - Graph diffing for dynamic topology changes (`astrs node add/remove`)
//!   — see [`diff()`] and [`apply()`].
//!
//! # Quick start
//!
//! ```
//! use astrs_graph::DataflowGraph;
//! use astrs_manifest::Manifest;
//!
//! let yaml = "\
//! nodes:
//!   - id: camera
//!     path: ./camera
//!     outputs: [frames]
//!   - id: detector
//!     path: ./detector
//!     inputs:
//!       frames: camera/frames
//! ";
//! let manifest = Manifest::from_yaml_str(yaml)?;
//! manifest.validate()?;
//!
//! let (graph, construction_diagnostics) = DataflowGraph::from_manifest(&manifest)?;
//! assert!(construction_diagnostics.is_empty());
//! assert_eq!(graph.node_count(), 2);
//! assert_eq!(graph.edge_count(), 1);
//! assert!(graph.diagnostics().is_empty()); // no type mismatches, cycles, ...
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Scope
//!
//! This crate builds its graph from a [`Manifest`](astrs_manifest::Manifest)
//! as that crate currently defines it. Module expansion (blueprint §8.5 —
//! inlining a `module:`-sourced node's referenced sub-graph before this
//! crate ever sees it) is `astrs-manifest`'s responsibility, tracked there;
//! this crate's construction path is written to accept whatever expanded
//! (or not-yet-expandable) manifest shape that crate produces.

mod diagnostic;
mod diff;
mod edge;
mod graph;
mod ids;
mod node;
mod pattern;
mod placement;
mod scc;
mod typecheck;
mod visualize;

pub use diagnostic::{Diagnostic, DiagnosticKind, Severity};
pub use diff::{ApplyError, TopologyOp, apply, diff};
pub use edge::{Edge, EdgeKey, EdgeSource, QueueConfig};
pub use graph::{DataflowGraph, GraphBuildError};
pub use ids::{MachineId, NodeId, PortName};
pub use node::{GraphNode, InputPort, OutputPort};
pub use pattern::{PatternKind, PatternPair};
pub use placement::{CrossMachineRoute, MachinePlan, PlacementPlan, plan_placement};
pub use scc::Scc;
pub use typecheck::EdgeTypeStatus;
pub use visualize::{to_dot, to_mermaid};
