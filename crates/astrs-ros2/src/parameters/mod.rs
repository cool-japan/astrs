//! Parameters: declare, get, set, list, describe — and the six services and
//! one topic that expose them.
//!
//! ROS 2's parameter model is three things layered:
//!
//! 1. **A value type.** [`ParameterValue`] is the real Rust enum behind
//!    `rcl_interfaces/msg/ParameterValue`'s ten-fields-and-a-discriminant
//!    wire form.
//! 2. **A store with rules.** [`ParameterStore`] holds what a node has
//!    declared and enforces every rule — type, range, read-only, atomic
//!    set, list recursion — synchronously, with no I/O.
//! 3. **A remote interface.** [`ParameterServices`] is the six
//!    `rcl_interfaces` services `ros2 param` calls, plus the
//!    `/parameter_events` topic every change is announced on.
//!
//! The split is what makes the rules testable: every assertion about what
//! ROS 2 does with a read-only parameter or a stepped range is a
//! synchronous test in [`store`] or [`descriptor`], and [`service`] is a
//! translation layer thin enough to read in one sitting.

pub mod descriptor;
pub mod service;
pub mod store;
pub mod value;

pub use descriptor::{FloatingPointRange, IntegerRange, ParameterDescriptor};
pub use service::{PARAMETER_EVENTS_TOPIC, ParameterServices};
pub use store::{
    DEPTH_RECURSIVE, ListResult, Parameter, ParameterChange, ParameterStore, SEPARATOR,
};
pub use value::ParameterValue;
