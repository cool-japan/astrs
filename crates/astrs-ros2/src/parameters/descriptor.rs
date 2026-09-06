//! [`ParameterDescriptor`] and the two range constraints it can carry.

use core::fmt;

use crate::error::{Ros2Error, Ros2Result};
use crate::msg::rcl_interfaces;
use crate::parameters::value::{ParameterValue, TYPE_NOT_SET, type_name_of};

/// An inclusive integer range with a step.
///
/// `rcl_interfaces/msg/IntegerRange`. A step of zero means "any value in
/// range"; a nonzero step means the value must be `from + k·step` for some
/// non-negative integer `k`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegerRange {
    /// The lowest permitted value, inclusive.
    pub from: i64,
    /// The highest permitted value, inclusive.
    pub to: i64,
    /// The permitted spacing, or zero for a continuous range.
    pub step: u64,
}

impl IntegerRange {
    /// A range with no step constraint.
    #[must_use]
    pub const fn new(from: i64, to: i64) -> Self {
        Self { from, to, step: 0 }
    }

    /// A range with a step constraint.
    #[must_use]
    pub const fn stepped(from: i64, to: i64, step: u64) -> Self {
        Self { from, to, step }
    }

    /// True when `value` satisfies the range.
    #[must_use]
    pub const fn accepts(&self, value: i64) -> bool {
        if value < self.from || value > self.to {
            return false;
        }
        if self.step == 0 {
            return true;
        }
        // `to - from` fits in i128 for every i64 pair, so the offset cannot
        // overflow the way `value - self.from` could.
        let offset = (value as i128) - (self.from as i128);
        offset % (self.step as i128) == 0
    }

    /// Render as the wire message.
    #[must_use]
    pub const fn to_message(self) -> rcl_interfaces::IntegerRange {
        rcl_interfaces::IntegerRange {
            from_value: self.from,
            to_value: self.to,
            step: self.step,
        }
    }

    /// Read the wire message.
    #[must_use]
    pub const fn from_message(message: &rcl_interfaces::IntegerRange) -> Self {
        Self {
            from: message.from_value,
            to: message.to_value,
            step: message.step,
        }
    }
}

impl fmt::Display for IntegerRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.step == 0 {
            write!(formatter, "[{}, {}]", self.from, self.to)
        } else {
            write!(formatter, "[{}, {}] step {}", self.from, self.to, self.step)
        }
    }
}

/// An inclusive floating-point range with a step.
///
/// `rcl_interfaces/msg/FloatingPointRange`. The step is a *magnitude*, so a
/// negative one means the same as its absolute value, and zero means a
/// continuous range — that is the message's own documented rule, and it is
/// why this type keeps the step as a plain `f64` rather than a checked
/// positive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatingPointRange {
    /// The lowest permitted value, inclusive.
    pub from: f64,
    /// The highest permitted value, inclusive.
    pub to: f64,
    /// The permitted spacing, or zero for a continuous range.
    pub step: f64,
}

/// How close to a step boundary a value must be to count as on it.
///
/// Floating-point steps cannot be compared exactly: `0.1 + 0.1 + 0.1` is not
/// `0.3`, and a range with `step: 0.1` that rejected `0.3` would be useless.
/// The tolerance is relative to the step so that a step of `1e6` and a step
/// of `1e-6` are equally usable.
pub const STEP_TOLERANCE: f64 = 1e-9;

impl FloatingPointRange {
    /// A range with no step constraint.
    #[must_use]
    pub const fn new(from: f64, to: f64) -> Self {
        Self {
            from,
            to,
            step: 0.0,
        }
    }

    /// A range with a step constraint.
    #[must_use]
    pub const fn stepped(from: f64, to: f64, step: f64) -> Self {
        Self { from, to, step }
    }

    /// True when `value` satisfies the range.
    #[must_use]
    pub fn accepts(&self, value: f64) -> bool {
        if !value.is_finite() || value < self.from || value > self.to {
            return false;
        }
        let step = self.step.abs();
        if step == 0.0 || !step.is_finite() {
            return true;
        }
        let steps = (value - self.from) / step;
        let distance = (steps - steps.round()).abs();
        distance <= STEP_TOLERANCE.max(f64::EPSILON * steps.abs().max(1.0))
    }

    /// Render as the wire message.
    #[must_use]
    pub const fn to_message(self) -> rcl_interfaces::FloatingPointRange {
        rcl_interfaces::FloatingPointRange {
            from_value: self.from,
            to_value: self.to,
            step: self.step,
        }
    }

    /// Read the wire message.
    #[must_use]
    pub const fn from_message(message: &rcl_interfaces::FloatingPointRange) -> Self {
        Self {
            from: message.from_value,
            to: message.to_value,
            step: message.step,
        }
    }
}

impl fmt::Display for FloatingPointRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.step == 0.0 {
            write!(formatter, "[{}, {}]", self.from, self.to)
        } else {
            write!(formatter, "[{}, {}] step {}", self.from, self.to, self.step)
        }
    }
}

/// Everything declared about a parameter besides its value.
///
/// `rcl_interfaces/msg/ParameterDescriptor`, with the two `[<=1]` range
/// members modelled as `Option`s — which is what a bounded-to-one sequence
/// means, spelled so that a caller cannot construct two.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParameterDescriptor {
    /// The parameter's name.
    pub name: String,
    /// The `ParameterType` code the parameter is declared at.
    pub type_code: u8,
    /// A human-readable description.
    pub description: String,
    /// Constraints the two range types cannot express, in prose.
    pub additional_constraints: String,
    /// True when the value cannot change after declaration.
    pub read_only: bool,
    /// True when the value may change type.
    pub dynamic_typing: bool,
    /// The permitted range, for an integer parameter.
    pub integer_range: Option<IntegerRange>,
    /// The permitted range, for a floating-point parameter.
    pub floating_point_range: Option<FloatingPointRange>,
}

impl ParameterDescriptor {
    /// A descriptor for a parameter of `type_code`, with no constraints.
    #[must_use]
    pub fn new(name: impl Into<String>, type_code: u8) -> Self {
        Self {
            name: name.into(),
            type_code,
            ..Self::default()
        }
    }

    /// A descriptor matching a value's type.
    #[must_use]
    pub fn for_value(name: impl Into<String>, value: &ParameterValue) -> Self {
        Self::new(name, value.type_code())
    }

    /// Replace the description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Replace the prose constraints.
    #[must_use]
    pub fn with_additional_constraints(mut self, constraints: impl Into<String>) -> Self {
        self.additional_constraints = constraints.into();
        self
    }

    /// Mark the parameter read-only.
    #[must_use]
    pub const fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Allow the value to change type.
    #[must_use]
    pub const fn dynamically_typed(mut self) -> Self {
        self.dynamic_typing = true;
        self
    }

    /// Constrain an integer parameter.
    #[must_use]
    pub const fn with_integer_range(mut self, range: IntegerRange) -> Self {
        self.integer_range = Some(range);
        self
    }

    /// Constrain a floating-point parameter.
    #[must_use]
    pub const fn with_floating_point_range(mut self, range: FloatingPointRange) -> Self {
        self.floating_point_range = Some(range);
        self
    }

    /// The declared type's printable name.
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        type_name_of(self.type_code)
    }

    /// Check a value against this descriptor.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::ParameterTypeMismatch`] when the value's type differs
    /// from the declared one and `dynamic_typing` is off, and
    /// [`Ros2Error::ParameterOutOfRange`] when a range rejects it.
    pub fn check(&self, value: &ParameterValue) -> Ros2Result<()> {
        if !self.dynamic_typing
            && self.type_code != TYPE_NOT_SET
            && value.type_code() != self.type_code
            && !value.is_not_set()
        {
            return Err(Ros2Error::ParameterTypeMismatch {
                name: self.name.clone(),
                expected: self.type_name(),
                actual: value.type_name(),
            });
        }

        match (value, self.integer_range, self.floating_point_range) {
            (ParameterValue::Integer(number), Some(range), _) if !range.accepts(*number) => {
                Err(Ros2Error::ParameterOutOfRange {
                    name: self.name.clone(),
                    value: number.to_string(),
                    range: range.to_string(),
                })
            }
            (ParameterValue::Double(number), _, Some(range)) if !range.accepts(*number) => {
                Err(Ros2Error::ParameterOutOfRange {
                    name: self.name.clone(),
                    value: number.to_string(),
                    range: range.to_string(),
                })
            }
            _ => Ok(()),
        }
    }

    /// Render as the wire message.
    #[must_use]
    pub fn to_message(&self) -> rcl_interfaces::ParameterDescriptor {
        let mut message = rcl_interfaces::ParameterDescriptor {
            name: self.name.clone(),
            r#type: self.type_code,
            description: self.description.clone(),
            additional_constraints: self.additional_constraints.clone(),
            read_only: self.read_only,
            dynamic_typing: self.dynamic_typing,
            ..rcl_interfaces::ParameterDescriptor::default()
        };
        if let Some(range) = self.floating_point_range {
            // The sequence is bounded to one and holds nothing yet, so the
            // push cannot exceed the bound.
            let _ = message.floating_point_range.push(range.to_message());
        }
        if let Some(range) = self.integer_range {
            let _ = message.integer_range.push(range.to_message());
        }
        message
    }

    /// Read the wire message.
    #[must_use]
    pub fn from_message(message: &rcl_interfaces::ParameterDescriptor) -> Self {
        Self {
            name: message.name.clone(),
            type_code: message.r#type,
            description: message.description.clone(),
            additional_constraints: message.additional_constraints.clone(),
            read_only: message.read_only,
            dynamic_typing: message.dynamic_typing,
            integer_range: message
                .integer_range
                .as_slice()
                .first()
                .map(IntegerRange::from_message),
            floating_point_range: message
                .floating_point_range
                .as_slice()
                .first()
                .map(FloatingPointRange::from_message),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::parameters::value::{TYPE_DOUBLE, TYPE_INTEGER, TYPE_STRING};

    #[test]
    fn an_integer_range_bounds_at_both_ends() {
        let range = IntegerRange::new(-5, 5);
        assert!(range.accepts(-5));
        assert!(range.accepts(0));
        assert!(range.accepts(5));
        assert!(!range.accepts(-6));
        assert!(!range.accepts(6));
    }

    #[test]
    fn an_integer_step_is_measured_from_the_lower_bound() {
        let range = IntegerRange::stepped(1, 10, 3);
        assert!(range.accepts(1));
        assert!(range.accepts(4));
        assert!(range.accepts(7));
        assert!(range.accepts(10));
        assert!(!range.accepts(2));
        assert!(!range.accepts(9));
    }

    #[test]
    fn an_integer_step_over_the_widest_range_does_not_overflow() {
        let range = IntegerRange::stepped(i64::MIN, i64::MAX, 2);
        assert!(range.accepts(i64::MIN));
        assert!(
            range.accepts(i64::MIN + 2),
            "the offset is computed in i128, so the widest range is usable"
        );
        assert!(!range.accepts(i64::MIN + 1));
    }

    #[test]
    fn a_floating_point_range_tolerates_accumulated_error() {
        let range = FloatingPointRange::stepped(0.0, 1.0, 0.1);
        assert!(range.accepts(0.0));
        assert!(range.accepts(0.1));
        assert!(
            range.accepts(0.1 + 0.1 + 0.1),
            "0.30000000000000004 is on the 0.1 grid for every practical purpose"
        );
        assert!(range.accepts(1.0));
        assert!(!range.accepts(0.05));
        assert!(!range.accepts(1.5));
    }

    #[test]
    fn a_floating_point_range_rejects_non_finite_values() {
        let range = FloatingPointRange::new(0.0, 1.0);
        assert!(!range.accepts(f64::NAN));
        assert!(!range.accepts(f64::INFINITY));
        assert!(!range.accepts(f64::NEG_INFINITY));
    }

    #[test]
    fn a_negative_step_means_its_magnitude() {
        let range = FloatingPointRange::stepped(0.0, 1.0, -0.25);
        assert!(range.accepts(0.5));
        assert!(!range.accepts(0.6));
    }

    #[test]
    fn a_zero_step_is_a_continuous_range() {
        assert!(FloatingPointRange::new(0.0, 1.0).accepts(0.123_456));
        assert!(IntegerRange::new(0, 10).accepts(7));
    }

    #[test]
    fn a_descriptor_rejects_the_wrong_type() {
        let descriptor = ParameterDescriptor::new("gain", TYPE_DOUBLE);
        assert!(descriptor.check(&ParameterValue::Double(1.0)).is_ok());
        let error = descriptor
            .check(&ParameterValue::Integer(1))
            .expect_err("an integer is not a double");
        assert!(matches!(error, Ros2Error::ParameterTypeMismatch { .. }));
        assert!(error.is_configuration());
    }

    #[test]
    fn a_dynamically_typed_descriptor_accepts_anything() {
        let descriptor = ParameterDescriptor::new("anything", TYPE_STRING).dynamically_typed();
        assert!(descriptor.check(&ParameterValue::Integer(1)).is_ok());
        assert!(
            descriptor
                .check(&ParameterValue::String("x".to_owned()))
                .is_ok()
        );
    }

    #[test]
    fn an_out_of_range_value_names_the_range_it_broke() {
        let descriptor = ParameterDescriptor::new("count", TYPE_INTEGER)
            .with_integer_range(IntegerRange::new(0, 10));
        let error = descriptor
            .check(&ParameterValue::Integer(11))
            .expect_err("out of range");
        let rendered = error.to_string();
        assert!(rendered.contains("11"), "{rendered}");
        assert!(rendered.contains("[0, 10]"), "{rendered}");
    }

    #[test]
    fn a_range_of_the_wrong_kind_is_simply_not_applied() {
        // A string parameter with an integer range attached: nonsense, but
        // not a reason to refuse the value.
        let descriptor = ParameterDescriptor::new("name", TYPE_STRING)
            .with_integer_range(IntegerRange::new(0, 1));
        assert!(
            descriptor
                .check(&ParameterValue::String("anything".to_owned()))
                .is_ok()
        );
    }

    #[test]
    fn an_unset_value_passes_any_type_check() {
        let descriptor = ParameterDescriptor::new("gain", TYPE_DOUBLE);
        assert!(
            descriptor.check(&ParameterValue::NotSet).is_ok(),
            "undeclaring or clearing a parameter is not a type error"
        );
    }

    #[test]
    fn a_descriptor_round_trips_through_the_wire_message() {
        let descriptor = ParameterDescriptor::new("gain", TYPE_DOUBLE)
            .with_description("loop gain")
            .with_additional_constraints("tuned per robot")
            .read_only()
            .with_floating_point_range(FloatingPointRange::stepped(0.0, 1.0, 0.05));
        let message = descriptor.to_message();
        assert_eq!(message.floating_point_range.len(), 1);
        assert_eq!(message.integer_range.len(), 0);
        assert_eq!(ParameterDescriptor::from_message(&message), descriptor);
    }

    #[test]
    fn a_descriptor_with_both_ranges_round_trips_both() {
        let descriptor = ParameterDescriptor::new("odd", TYPE_INTEGER)
            .with_integer_range(IntegerRange::stepped(1, 9, 2))
            .with_floating_point_range(FloatingPointRange::new(0.0, 1.0));
        assert_eq!(
            ParameterDescriptor::from_message(&descriptor.to_message()),
            descriptor
        );
    }

    #[test]
    fn a_descriptor_for_a_value_takes_its_type() {
        let descriptor = ParameterDescriptor::for_value("x", &ParameterValue::Bool(true));
        assert_eq!(descriptor.type_name(), "bool");
        assert!(!descriptor.read_only);
        assert!(!descriptor.dynamic_typing);
    }

    #[test]
    fn the_range_display_forms_are_readable() {
        assert_eq!(IntegerRange::new(0, 5).to_string(), "[0, 5]");
        assert_eq!(IntegerRange::stepped(0, 5, 1).to_string(), "[0, 5] step 1");
        assert_eq!(FloatingPointRange::new(0.0, 1.0).to_string(), "[0, 1]");
        assert_eq!(
            FloatingPointRange::stepped(0.0, 1.0, 0.5).to_string(),
            "[0, 1] step 0.5"
        );
    }
}
