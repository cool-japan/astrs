//! [`UrdfError`] — the crate-wide error taxonomy.
//!
//! Every fallible entry point in this crate returns [`UrdfError`], mirroring
//! `astrs-tf::TfError`'s own shape (`Clone + PartialEq`, so tests assert an
//! exact expected error with `assert_eq!` rather than a `matches!` shape).
//! It splits into four groups, in the order a document is processed:
//!
//! 1. **XML syntax** — [`UrdfError::Xml`], a transparent wrapper around
//!    [`crate::xml::XmlError`] for a document that is not even well-formed
//!    XML.
//! 2. **URDF shape** — [`UrdfError::UnexpectedElement`],
//!    [`UrdfError::MissingChildElement`], [`UrdfError::MissingAttribute`],
//!    [`UrdfError::InvalidAttributeValue`], [`UrdfError::UnknownJointType`]:
//!    well-formed XML that is not a well-formed URDF document — a required
//!    element or attribute is missing, or a value does not parse as the
//!    number (or joint type) it needs to be. Every one of these carries the
//!    [`crate::xml::Span`] of the offending construct.
//! 3. **Tree validation** — [`UrdfError::DuplicateLinkName`] through
//!    [`UrdfError::MimicCycle`]: the document parsed cleanly into a
//!    [`crate::model::Robot`], but that robot's cross-references do not form
//!    a legal kinematic tree (blueprint §5.3's "joint graph is a rooted
//!    tree"). These have no single [`crate::xml::Span`] to point at — a
//!    "link X has two parent joints" error is a property of the *whole*
//!    document, not one element.
//! 4. **Kinematics** — [`UrdfError::UnknownLink`] through
//!    [`UrdfError::Tf`]: failures raised while walking an already-validated
//!    tree (an unknown name passed to [`crate::kinematics`], or a
//!    [`crate::kinematics::populate_static_transforms`] call that hits an
//!    `astrs-tf` buffer error).

use crate::xml::{Span, XmlError};

/// The result type every fallible operation in this crate returns.
pub type Result<T> = std::result::Result<T, UrdfError>;

/// A URDF parsing, validation, or kinematics failure.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum UrdfError {
    // -- XML syntax ------------------------------------------------------
    /// The input was not well-formed XML at all.
    #[error(transparent)]
    Xml(#[from] XmlError),

    // -- URDF shape --------------------------------------------------------
    /// An element appeared where a different one (or one of a fixed set)
    /// was expected.
    #[error("expected `<{expected}>`, found `<{found}>` at {span}")]
    UnexpectedElement {
        /// What was expected (e.g. `"link or joint"`).
        expected: &'static str,
        /// What tag was actually found.
        found: String,
        /// Where.
        span: Span,
    },

    /// A required child element was never seen before its parent closed.
    #[error("`<{parent}>` is missing its required `<{expected}>` child, closed at {span}")]
    MissingChildElement {
        /// The parent element's tag name.
        parent: &'static str,
        /// The missing child's tag name.
        expected: &'static str,
        /// The span of the parent's own closing tag.
        span: Span,
    },

    /// A required attribute was absent from an element.
    #[error("`<{element}>` is missing its required `{attribute}` attribute, at {span}")]
    MissingAttribute {
        /// The element's tag name.
        element: &'static str,
        /// The missing attribute's name.
        attribute: &'static str,
        /// The span of the element's opening tag.
        span: Span,
    },

    /// An attribute was present but its value did not parse as the type it
    /// needs to be (a float, an integer count, a `bool`, an enumerated
    /// joint type, ...).
    #[error("`{element}`'s `{attribute}` attribute has an invalid value `{value}`, at {span}")]
    InvalidAttributeValue {
        /// The element's tag name.
        element: &'static str,
        /// The attribute's name.
        attribute: &'static str,
        /// The raw (unparsed) attribute text.
        value: String,
        /// The span of the attribute's value.
        span: Span,
    },

    /// A `<joint type="...">` value was not one of URDF's six.
    #[error("joint `{joint}` has an unknown type `{found}`, at {span}")]
    UnknownJointType {
        /// The joint's `name` attribute.
        joint: String,
        /// The unrecognized `type` value.
        found: String,
        /// The span of the `type` attribute's value.
        span: Span,
    },

    // -- Tree validation ---------------------------------------------------
    /// Two links share a name.
    #[error("duplicate link name `{name}`")]
    DuplicateLinkName {
        /// The repeated name.
        name: String,
    },

    /// Two joints share a name.
    #[error("duplicate joint name `{name}`")]
    DuplicateJointName {
        /// The repeated name.
        name: String,
    },

    /// A joint's `parent` names a link that was never declared.
    #[error("joint `{joint}`'s parent link `{link}` is not declared")]
    UnknownParentLink {
        /// The joint's name.
        joint: String,
        /// The undeclared link name.
        link: String,
    },

    /// A joint's `child` names a link that was never declared.
    #[error("joint `{joint}`'s child link `{link}` is not declared")]
    UnknownChildLink {
        /// The joint's name.
        joint: String,
        /// The undeclared link name.
        link: String,
    },

    /// Two different joints claim the same link as their `child` — a link
    /// can have at most one parent joint, or the graph is not a tree.
    #[error(
        "link `{link}` has two parent joints, `{first_joint}` and `{second_joint}` — a link may have at most one"
    )]
    MultipleParentJoints {
        /// The doubly-claimed link.
        link: String,
        /// The first joint (in document order) claiming it.
        first_joint: String,
        /// The second joint claiming it.
        second_joint: String,
    },

    /// No link is a root (every link is somebody's child) — the joint
    /// graph has no valid rooted tree at all, either because it is empty
    /// (impossible for a non-empty robot with at least one link) or because
    /// every link participates in a cycle.
    #[error("no root link found: every link has a parent joint")]
    NoRootLink,

    /// More than one link has no parent joint — a rooted tree has exactly
    /// one.
    #[error("multiple root links found: {roots:?}")]
    MultipleRootLinks {
        /// Every link with no parent joint, in declaration order.
        roots: Vec<String>,
    },

    /// One root was found, but not every link is reachable from it — some
    /// links form a disconnected component (which, given
    /// [`UrdfError::MultipleParentJoints`] already rules out more than one
    /// parent per link, can only mean a cycle among those links: none of
    /// them is ever *nobody's* child, so none was counted as an extra root
    /// either).
    #[error("link(s) unreachable from the root: {links:?}")]
    DisconnectedLinks {
        /// Every link never reached while walking from the root, in
        /// declaration order.
        links: Vec<String>,
    },

    /// [`crate::kinematics`] discovered a repeated link while walking the
    /// tree from its root — the *reactive* counterpart to
    /// [`UrdfError::MultipleParentJoints`]/[`UrdfError::DisconnectedLinks`],
    /// mirroring `astrs_tf::TfError::Cycle`'s own two-layer design
    /// (`TransformBuffer::walk_to_root`'s reactive guard alongside
    /// `set_transform`'s proactive check). [`crate::model::Robot::validate`]
    /// is expected to catch this *before* [`crate::kinematics`] ever runs;
    /// this variant only fires when a caller skipped validation (or hand-
    /// built a [`crate::model::Robot`] bypassing it) and the tree genuinely
    /// cycles.
    #[error("joint graph cycle discovered while walking from the root: {path:?}")]
    JointGraphCycle {
        /// The cyclic path of link names, starting and ending at the same
        /// link.
        path: Vec<String>,
    },

    /// A `<mimic joint="...">` names a joint that was never declared.
    #[error("joint `{joint}`'s mimic target `{target}` is not declared")]
    UnknownMimicTarget {
        /// The mimicking joint's name.
        joint: String,
        /// The undeclared target joint name.
        target: String,
    },

    /// A `<mimic>` was declared on, or targets, a joint with other than
    /// exactly one degree of freedom — `multiplier * position + offset`
    /// only has meaning for a scalar joint position.
    #[error(
        "joint `{joint}` has a mimic relationship with `{other}`, but one of them is not a single-DOF joint"
    )]
    MimicRequiresSingleDofJoint {
        /// The mimicking joint's name.
        joint: String,
        /// The other joint in the relationship (its mimic target).
        other: String,
    },

    /// Following `<mimic>` targets from some joint returns to that same
    /// joint.
    #[error("mimic cycle: {path:?}")]
    MimicCycle {
        /// The cyclic path of joint names, starting and ending at the same
        /// joint.
        path: Vec<String>,
    },

    /// A visual's `<material name="..."/>` (with no inline color/texture)
    /// names a robot-level material that was never declared.
    #[error("link `{link}`'s visual references undeclared material `{material}`")]
    UnknownMaterial {
        /// The link that referenced it.
        link: String,
        /// The undeclared material name.
        material: String,
    },

    // -- Kinematics ----------------------------------------------------
    /// A link name passed to a [`crate::kinematics`] query is not in the
    /// robot.
    #[error("unknown link `{link}`")]
    UnknownLink {
        /// The unrecognized name.
        link: String,
    },

    /// A joint name passed to a [`crate::kinematics`] query is not in the
    /// robot.
    #[error("unknown joint `{joint}`")]
    UnknownJoint {
        /// The unrecognized name.
        joint: String,
    },

    /// [`crate::kinematics::Chain::extract`] was asked for a chain between
    /// two links with no ancestor relationship — `base` is not an ancestor
    /// of `tip` (or vice versa) in the rooted tree.
    #[error("no chain exists from `{base}` to `{tip}`: neither is an ancestor of the other")]
    NoChainBetween {
        /// The requested base link.
        base: String,
        /// The requested tip link.
        tip: String,
    },

    /// A [`crate::kinematics::JointPosition`] variant did not match the
    /// degrees of freedom [`crate::JointKind::degrees_of_freedom`] the
    /// joint it was supplied for actually has (e.g. a
    /// [`crate::kinematics::JointPosition::Planar`] value for a revolute
    /// joint).
    #[error("joint `{joint}` (a {kind} joint) was given a position value of the wrong shape")]
    JointPositionKindMismatch {
        /// The joint's name.
        joint: String,
        /// The joint's actual kind, for the message.
        kind: crate::JointKind,
    },

    /// A [`crate::model::Joint::axis`] with zero (or non-finite) length
    /// reached [`crate::kinematics::forward_kinematics`]'s planar-joint
    /// solver, which needs a well-defined plane normal to give
    /// [`crate::kinematics::JointPosition::Planar`]'s `x`/`y` any meaning.
    ///
    /// [`crate::parse`] already normalizes and rejects a degenerate `<axis
    /// xyz="..">` at parse time (see [`UrdfError::InvalidAttributeValue`]),
    /// so this only fires for a [`crate::model::Robot`] a caller built or
    /// mutated by hand, bypassing that check — [`crate::model::Joint::axis`]
    /// is a public field.
    #[error("joint `{joint}`'s axis is degenerate (zero length or non-finite)")]
    DegenerateJointAxis {
        /// The joint whose axis was degenerate.
        joint: String,
    },

    /// [`crate::kinematics::populate_static_transforms`] hit a buffer
    /// error registering a fixed joint's transform (a degenerate rotation
    /// or non-finite translation in the URDF's own `<origin>`, or — should
    /// [`crate::model::Robot::validate`] somehow have been bypassed — a
    /// frame-graph cycle).
    #[error(transparent)]
    Tf(#[from] astrs_tf::TfError),
}
