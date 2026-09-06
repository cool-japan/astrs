//! The dataflow lifecycle — plan, build, spawn, run, stop (§4.2, §12).
//!
//! | Module | Concern |
//! |---|---|
//! | [`plan`] | Manifest + graph → the [`astrs_wire::NodeSpawnSpec`]s the daemon runs |
//! | [`build`] | `build:` lines, executed with the same §16 hygiene a node gets |
//! | [`fsm`] | The lifecycle itself, and [`run_dataflow_with`] — the in-process entry `astrs run` embeds |
//! | [`topology`] | Live add/remove/replace/edge ops on an already-running dataflow (§8, §17) |

pub mod build;
pub mod fsm;
pub mod plan;
pub mod topology;

pub use build::{BuildReport, StepOutcome, run_build, run_step};
pub use fsm::{RunOptions, run_dataflow_with};
pub use plan::{BuildStep, DataflowPlan, PlannedVirtualInput, plan_dataflow};
pub use topology::Superseded;
