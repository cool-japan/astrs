//! Migration into AstRS from the incumbents.
//!
//! Adoption cost is an afternoon, not a quarter (blueprint §5.2, §8.6):
//!
//! - [`dora`]: `from-dora` — a mechanical dora dataflow descriptor mapping
//!   — nodes, inputs and outputs, timer virtual inputs, restart policies,
//!   queue policies, service and action patterns — with a
//!   [`dora::MigrationNote`] for anything that needs a human, rather than
//!   a silent drop (blueprint §8.6). Modeled against a real dora
//!   `dora-schema.json`, not reconstructed from memory: see [`dora`]'s
//!   own module docs for exactly what is ground-truthed and what is a
//!   deliberate, documented approximation.
//! - [`ros2`]: `from-ros2` — ROS 2 launch-file skimming (XML; Python
//!   launch files are never executed or parsed, only best-effort
//!   skimmed) that scaffolds a bridge manifest with discovered topics,
//!   namespaces and params pre-filled, plus a [`ros2::MigrationNote`] for
//!   anything that needs a human — see [`ros2`]'s own module docs for the
//!   full architecture and exactly what is (and, deliberately, is not)
//!   inferred automatically.
//!
//! # Quick start
//!
//! ```
//! use astrs_migrate::migrate_str;
//!
//! let dora_yaml = "\
//! nodes:
//!   - id: camera
//!     path: ./camera-node
//!     outputs: [frames]
//!   - id: detector
//!     path: ./detector-node
//!     inputs:
//!       frames: camera/frames
//!       tick: dora/timer/millis/100
//!     restart_policy: on-failure
//! ";
//!
//! let result = migrate_str(dora_yaml)?;
//! assert!(result.yaml.contains("astrs/timer/millis/100"));
//! assert!(result.yaml.contains("on_failure"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod error;

pub mod dora;
pub mod ros2;

pub use dora::{MigrationNote, MigrationResult, NoteSeverity, migrate_file, migrate_str};
pub use error::{DoraMigrateError, Ros2MigrateError};
pub use ros2::{
    MigrationNote as Ros2MigrationNote, MigrationResult as Ros2MigrationResult,
    NoteSeverity as Ros2NoteSeverity, migrate_ros2_launch_file, migrate_ros2_launch_str,
};
