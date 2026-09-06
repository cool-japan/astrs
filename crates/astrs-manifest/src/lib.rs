//! The AstRS dataflow manifest: parse, validate, expand.
//!
//! The YAML descriptor is the user-facing contract, and it is a strict one
//! (blueprint §8):
//!
//! - `deny_unknown_fields` parsing of the root, node, input long-form and
//!   `ros2:` bridge schemas, an enum match with no fallback for
//!   `restart_policy`/`queue_policy`, and a fixed grammar for
//!   `astrs/timer/...` sources — config never silently lies about what it
//!   does.
//! - Environment expansion with the documented precedence rules.
//! - Module expansion ([`expand`]): flattening a `module:`-sourced node's
//!   referenced sub-graph into `parent.child` ids, recursively.
//! - The dora manifest importer used by `astrs migrate from-dora` (planned).
//! - JSON-schema emission (schemars) driving editor completion and
//!   `astrs schema`.
//!
//! # Quick start
//!
//! ```
//! use astrs_manifest::Manifest;
//!
//! let yaml = r#"
//! name: perception-demo
//! nodes:
//!   - id: camera
//!     path: ./target/release/camera-node
//!     outputs: [frames]
//!   - id: planner
//!     path: ./planner
//!     inputs:
//!       frames: camera/frames
//!       tick: astrs/timer/hz/50
//! "#;
//!
//! let manifest = Manifest::from_yaml_str(yaml)?;
//! manifest.validate()?;
//! assert_eq!(manifest.nodes.len(), 2);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Two-stage errors: parse, then validate
//!
//! [`Manifest::from_yaml_str`] / [`Manifest::from_yaml_file`] reject
//! anything that is not a syntactically well-formed manifest document —
//! wrong types, unknown fields, missing required fields — via
//! [`ManifestError`], which fails on the *first* problem (this is what
//! `astrs_yaml` gives us, and it is also the right behavior for outright
//! malformed YAML: there is nothing useful to say about node 4 if node 2
//! didn't parse).
//!
//! [`Manifest::validate`] then checks structural, cross-referential
//! properties of an already-parsed manifest — duplicate node ids, dangling
//! input references, inconsistent restart fields, and so on — and reports
//! **every** violation at once via [`ValidationErrors`], because those
//! checks are independent of each other and a user fixing a 20-node graph
//! should not have to re-run the validator 20 times.
//!
//! # Scope
//!
//! This crate implements the manifest's complete schema, validation, and
//! module expansion (blueprint §8.1–§8.5, §10.5): the root and node schema,
//! `env:` expansion, the `ros2:` bridge block, virtual sources
//! ([`VirtualSource`]), JSON Schema emission, and [`expand`] — tested
//! against the blueprint's canonical §8.1 example manifest (checked in at
//! `tests/fixtures/valid/perception_demo.yaml`).
//!
//! One piece named in §8 is **not** implemented here and is left for a
//! follow-up revision: **the dora manifest importer** (§8.6) used by
//! `astrs migrate from-dora`.

mod deploy;
mod duration;
mod env;
mod error;
pub mod expand;
mod module_header;
mod node;
mod root;
mod schema;
mod type_rule;
mod urn;
mod validate;
mod virtual_source;

pub use deploy::Deploy;
pub use duration::{DurationParseError, DurationSecs};
pub use env::{EnvExpandError, EnvMapExpandError, EnvValue, expand_map, expand_str};
pub use error::ManifestError;
pub use module_header::{MODULE_BOUNDARY_NODE_ID, ModuleHeader};
pub use node::{
    BridgeDirection, DYNAMIC_PATH_SENTINEL, Durability, HUB_REV_SEPARATOR, HubSource, Input,
    LogLevel, Node, OperatorConfig, Pattern, PriorityLane, Qos, QueuePolicy, RT_PRIORITY_MAX,
    RT_PRIORITY_MIN, RestartPolicy, Ros2Config, Ros2Role, Ros2Topic, RosCompat, RtConfig, RtPolicy,
    default_queue_size,
};
pub use root::{DEFAULT_MANIFEST_FORMAT, Manifest, default_health_check_interval};
pub use schema::emit_schema;
pub use type_rule::TypeRule;
pub use urn::{Urn, UrnError, UrnParts};
pub use validate::{
    UnresolvedReferenceReason, ValidationError, ValidationErrorKind, ValidationErrors,
};
pub use virtual_source::{
    VirtualSource, VirtualSourceError, recognize as recognize_virtual_source,
};
