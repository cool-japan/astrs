//! URDF robot models for AstRS: links, joints and kinematic chains feeding
//! [`astrs_tf`].
//!
//! `astrs-tf` implements the tf2 *contract* — the frame tree, its SE(3) math
//! and its `/tf` wire interop — and deliberately stops there; its own crate
//! docs name a URDF parser and kinematic-chain solver as a separate crate
//! that would *consume* `TransformBuffer` rather than live inside it. This is
//! that crate.
//!
//! # Scope
//!
//! - Parse URDF into a link/joint model ([`parse`], [`model`]): geometry,
//!   inertial properties, joint kinematics, limits, mimic relationships —
//!   on this crate's own zero-dependency XML pull parser ([`xml`]).
//! - Validate that model's cross-references and tree shape
//!   ([`model::Robot::validate`]).
//! - Build kinematic chains over that model and run forward kinematics,
//!   including `<mimic>` resolution ([`kinematics`]).
//! - Publish the resulting fixed-frame skeleton into an
//!   [`astrs_tf::buffer::TransformBuffer`]
//!   ([`kinematics::populate_static_transforms`]), so a robot description
//!   and a live transform tree stay one source of truth rather than two.
//!
//! Carrying a robot's *live* joint state (as it changes at runtime) as
//! [`astrs_data`] columnar messages — the currency every other AstRS
//! payload uses — is a natural next layer on top of what is here
//! ([`kinematics::forward_kinematics`] already accepts exactly that shape
//! of input, a joint-name-to-position map), but is not itself part of this
//! crate: nothing here publishes or subscribes to a running dataflow, and
//! this crate's own dependency on `astrs-data` stays unused for now,
//! reserved for that future integration rather than exercised today.
//!
//! # Joint kinds are the model's spine
//!
//! Nearly every downstream decision — how many numbers a joint contributes to
//! a configuration vector, whether a `<limit>` element is required, whether
//! forward kinematics needs a rotation or a translation — follows from the
//! joint's `type` attribute alone. [`JointKind`] models URDF's six, with
//! those consequences spelled as methods instead of re-derived at each call
//! site.
//!
//! ```
//! use astrs_urdf::JointKind;
//!
//! // A revolute joint rotates within bounds; a continuous one does not stop.
//! assert!(JointKind::Revolute.has_position_limits());
//! assert!(!JointKind::Continuous.has_position_limits());
//!
//! // Degrees of freedom drive the size of a configuration vector.
//! let arm = [JointKind::Revolute, JointKind::Revolute, JointKind::Prismatic];
//! let dof: usize = arm.iter().map(|joint| joint.degrees_of_freedom()).sum();
//! assert_eq!(dof, 3);
//! ```

pub mod error;
pub mod kinematics;
pub mod math;
pub mod model;
pub mod parse;
pub mod xml;

pub use error::{Result, UrdfError};
pub use model::Robot;
pub use parse::parse_str;

/// A URDF joint type (`<joint type="...">`).
///
/// URDF defines exactly these six. Which one a joint is determines how many
/// degrees of freedom it contributes, whether a `<limit>` element is required
/// of it, and what forward kinematics has to compose for it — so the variants
/// carry those consequences as methods rather than leaving every consumer to
/// re-derive them from the tag name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JointKind {
    /// Rotates about an axis, bounded by `<limit lower=.. upper=..>`.
    Revolute,
    /// Rotates about an axis without bound — no position limits apply.
    Continuous,
    /// Slides along an axis, bounded by `<limit lower=.. upper=..>`.
    Prismatic,
    /// Rigidly welds two links: no motion, no state, no limits.
    Fixed,
    /// Unconstrained six-degree-of-freedom motion between two links.
    Floating,
    /// Motion within a plane perpendicular to the joint axis (two
    /// translations and one rotation).
    Planar,
}

impl JointKind {
    /// This joint's `type` attribute exactly as URDF spells it.
    #[must_use]
    pub const fn as_urdf_str(self) -> &'static str {
        match self {
            Self::Revolute => "revolute",
            Self::Continuous => "continuous",
            Self::Prismatic => "prismatic",
            Self::Fixed => "fixed",
            Self::Floating => "floating",
            Self::Planar => "planar",
        }
    }

    /// Parse a URDF `type` attribute value.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_urdf::JointKind;
    ///
    /// assert_eq!(JointKind::from_urdf_str("prismatic"), Some(JointKind::Prismatic));
    /// assert_eq!(JointKind::from_urdf_str("Revolute"), None); // URDF is case-sensitive
    /// ```
    #[must_use]
    pub fn from_urdf_str(value: &str) -> Option<Self> {
        match value {
            "revolute" => Some(Self::Revolute),
            "continuous" => Some(Self::Continuous),
            "prismatic" => Some(Self::Prismatic),
            "fixed" => Some(Self::Fixed),
            "floating" => Some(Self::Floating),
            "planar" => Some(Self::Planar),
            _ => None,
        }
    }

    /// How many independent numbers a joint of this kind contributes to a
    /// configuration vector.
    #[must_use]
    pub const fn degrees_of_freedom(self) -> usize {
        match self {
            Self::Fixed => 0,
            Self::Revolute | Self::Continuous | Self::Prismatic => 1,
            Self::Planar => 3,
            Self::Floating => 6,
        }
    }

    /// Whether a `<limit lower=.. upper=..>` element is meaningful for this
    /// joint kind.
    ///
    /// Only `revolute` and `prismatic` are bounded in position: `continuous`
    /// is explicitly unbounded, and `fixed`/`floating`/`planar` have no
    /// single scalar position for a bound to constrain.
    #[must_use]
    pub const fn has_position_limits(self) -> bool {
        matches!(self, Self::Revolute | Self::Prismatic)
    }

    /// Whether this joint ever moves at all.
    #[must_use]
    pub const fn is_movable(self) -> bool {
        self.degrees_of_freedom() > 0
    }
}

impl std::fmt::Display for JointKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_urdf_str())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    const EVERY_KIND: &[JointKind] = &[
        JointKind::Revolute,
        JointKind::Continuous,
        JointKind::Prismatic,
        JointKind::Fixed,
        JointKind::Floating,
        JointKind::Planar,
    ];

    #[test]
    fn every_kind_round_trips_through_its_urdf_spelling() {
        for kind in EVERY_KIND {
            assert_eq!(JointKind::from_urdf_str(kind.as_urdf_str()), Some(*kind));
        }
    }

    #[test]
    fn urdf_type_parsing_is_case_sensitive_and_rejects_unknown_values() {
        assert_eq!(JointKind::from_urdf_str("Fixed"), None);
        assert_eq!(JointKind::from_urdf_str("ball"), None);
        assert_eq!(JointKind::from_urdf_str(""), None);
    }

    #[test]
    fn degrees_of_freedom_match_the_urdf_specification() {
        assert_eq!(JointKind::Fixed.degrees_of_freedom(), 0);
        assert_eq!(JointKind::Revolute.degrees_of_freedom(), 1);
        assert_eq!(JointKind::Continuous.degrees_of_freedom(), 1);
        assert_eq!(JointKind::Prismatic.degrees_of_freedom(), 1);
        assert_eq!(JointKind::Planar.degrees_of_freedom(), 3);
        assert_eq!(JointKind::Floating.degrees_of_freedom(), 6);
    }

    #[test]
    fn only_revolute_and_prismatic_are_bounded_in_position() {
        for kind in EVERY_KIND {
            let bounded = matches!(kind, JointKind::Revolute | JointKind::Prismatic);
            assert_eq!(kind.has_position_limits(), bounded, "{kind}");
        }
    }

    #[test]
    fn only_a_fixed_joint_never_moves() {
        for kind in EVERY_KIND {
            assert_eq!(kind.is_movable(), *kind != JointKind::Fixed, "{kind}");
        }
    }

    #[test]
    fn display_is_the_urdf_spelling() {
        assert_eq!(JointKind::Continuous.to_string(), "continuous");
    }
}
