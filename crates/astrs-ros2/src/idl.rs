//! The path shim generated code resolves `astrs_idl::…` through.
//!
//! `astrs-idl`'s code generator emits **absolute** paths — deliberately, so
//! that one emitted text is correct both inside `astrs-idl` (where
//! `extern crate self as astrs_idl` makes them resolve) and inside a
//! downstream crate. Two path shapes appear:
//!
//! | Emitted path | Means |
//! |---|---|
//! | `astrs_idl::runtime::…` | the columnar leaf contract every generated type implements |
//! | `astrs_idl::generated::<pkg>::<Type>` | a field whose type lives in another ROS package |
//!
//! The first resolves against the real dependency here. The second does
//! not, and cannot: `action_msgs/msg/GoalInfo.msg` references
//! `unique_identifier_msgs/UUID`, which [`crate::interfaces`] owns rather
//! than `astrs_idl::generated`. Half of this crate's cross-package
//! references point into `astrs-idl`'s tree (`builtin_interfaces/Time`) and
//! half point into its own (`unique_identifier_msgs/UUID`,
//! `rcl_interfaces/ParameterValue`, …).
//!
//! So every generated file in this crate opens with
//!
//! ```text
//! use crate::idl as astrs_idl;
//! ```
//!
//! and this module merges the two trees behind that one name. Nothing in a
//! generated body is rewritten — the emitted text is byte-for-byte what
//! `astrs-idl` produces, which is what makes
//! `tests/interfaces_match_source.rs` a real drift guard rather than a
//! comparison of one local transformation against itself.
//!
//! # Why an import rather than a crate-root module
//!
//! A `pub mod astrs_idl` at the crate root would not do it: paths inside
//! `crate::interfaces::action_msgs::goal_info` resolve `astrs_idl` in *that*
//! module's scope, falling through to the extern prelude — the real
//! dependency. An explicit `use` in the module that needs it shadows the
//! prelude, which is exactly the scope the substitution should have.

/// `astrs-idl`'s columnar runtime, re-exported verbatim.
///
/// Generated code calls [`ColumnValue`](astrs_idl::runtime::ColumnValue) and
/// the `encode_*_rows`/`struct_column` composition helpers through this path.
pub use astrs_idl::runtime;

/// The union of the two generated interface trees.
///
/// `astrs-idl`'s pre-generated `common_interfaces` set plus this crate's own
/// `msg-src/` packages, under one name, so a generated
/// `astrs_idl::generated::<pkg>::<Type>` path resolves whichever tree owns
/// the package.
pub mod generated {
    pub use astrs_idl::generated::{
        builtin_interfaces, geometry_msgs, nav_msgs, sensor_msgs, std_msgs, std_srvs,
    };

    pub use crate::interfaces::{
        action_msgs, example_interfaces, rcl_interfaces, rosgraph_msgs, unique_identifier_msgs,
    };
}
