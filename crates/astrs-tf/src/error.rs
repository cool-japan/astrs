//! [`TfError`] — the `astrs-tf` error taxonomy.
//!
//! Every fallible path in this crate returns [`TfError`]. Mirroring
//! `astrs-idl`'s `IdlError` and `astrs-data`'s `DataError`, the type is
//! `Clone + PartialEq + Eq` so tests assert an exact expected error with
//! `assert_eq!` rather than a `matches!` shape. The taxonomy splits into
//! four groups:
//!
//! 1. **Frame graph shape** — [`TfError::UnknownFrame`],
//!    [`TfError::Disconnected`], [`TfError::Cycle`],
//!    [`TfError::FrameKindConflict`]: the two validations blueprint §10.6
//!    names explicitly (cycles, disconnected lookups) plus one more the
//!    frame tree's own invariant (each child frame has one *kind* of
//!    parent relationship — static or dynamic, never both at once) makes
//!    checkable the same way.
//! 2. **Time-travel lookup** — [`TfError::EmptyHistory`],
//!    [`TfError::Extrapolation`]: a lookup's walk reached a frame whose
//!    buffered history cannot answer the query time. `frame` on both
//!    variants names the specific hop in the walk that failed, not the
//!    lookup's original `target`/`source` endpoints — a multi-hop chain can
//!    fail on any intermediate frame, and the caller needs to know which
//!    one.
//! 3. **Value validity** — [`TfError::NonFiniteTranslation`],
//!    [`TfError::DegenerateQuaternion`]: a transform handed to
//!    [`crate::buffer::TransformBuffer::set_transform`] is not a legal
//!    rigid-body transform (NaN/infinite translation, or a rotation
//!    quaternion with no well-defined direction to normalize).
//! 4. **ROS time conversion** — [`TfError::RosTimeRangeExceeded`]: a
//!    [`crate::time::TfStamp`] does not fit `builtin_interfaces/Time`'s
//!    `i32`-seconds wire range.
//!
//! # What is deliberately *not* a validation failure
//!
//! Setting a dynamic transform for a child frame whose most recent parent
//! differs from a new insertion's parent is **not** rejected — tf2 does not
//! require a child frame's parent to be constant across time (a
//! `TransformStamped`'s `header.frame_id` legitimately changes between
//! publishes, e.g. a robot re-parented into a different localization
//! frame), so the buffer stores the parent alongside every history entry
//! rather than fixing it once. See [`crate::buffer`]'s module docs for how
//! interpolation across a parent change is resolved without inventing a
//! blended, and therefore meaningless, parent.

use std::fmt;

use crate::time::TfStamp;

/// Whether a frame's stored relationship to its parent is static (returned
/// for any query time, no history) or dynamic (a timestamped history,
/// interpolated on lookup). See [`TfError::FrameKindConflict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// A single transform, valid for any query time.
    Static,
    /// A timestamped history, interpolated on lookup.
    Dynamic,
}

impl fmt::Display for FrameKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Static => "static",
            Self::Dynamic => "dynamic",
        })
    }
}

/// Which direction a failed lookup would have needed to extrapolate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtrapolationDirection {
    /// The requested time is older than every buffered sample.
    Past,
    /// The requested time is newer than every buffered sample.
    Future,
}

impl fmt::Display for ExtrapolationDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Past => "past",
            Self::Future => "future",
        })
    }
}

/// Everything that can go wrong building or querying an `astrs-tf` frame
/// tree. See the [module documentation](self) for the taxonomy's four
/// groups.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TfError {
    /// A lookup named a frame that no [`crate::buffer::TransformBuffer::set_transform`]
    /// call has ever registered — as either a parent or a child.
    #[error("unknown frame {frame:?}: no set_transform call has ever registered it")]
    UnknownFrame {
        /// The unregistered frame name.
        frame: String,
    },

    /// `target_frame` and `source_frame` are both known frames, but no
    /// chain of parent edges connects them — the frame graph has more than
    /// one root and they sit in different trees.
    ///
    /// Field names deliberately spell out `_frame` (rather than the bare
    /// `target`/`source` tf2's own C++ parameter names use) so neither
    /// collides with `thiserror`'s implicit "a field literally named
    /// `source` is this error's `std::error::Error::source()`" convention —
    /// these are frame *names* (`String`), not a wrapped source error.
    #[error(
        "no transform chain connects {source_frame:?} to {target_frame:?}: the frame graph has \
         more than one connected component"
    )]
    Disconnected {
        /// The lookup's destination frame.
        target_frame: String,
        /// The lookup's origin frame.
        source_frame: String,
    },

    /// Walking a frame's parent chain revisited a frame already on the
    /// path — the frame graph is not a tree (or forest).
    ///
    /// `path` lists the frame names in walk order, with the first entry
    /// repeated at the end to show exactly where the cycle closes (e.g.
    /// `["a", "b", "c", "a"]`).
    #[error("frame graph cycle: {}", path.join(" -> "))]
    Cycle {
        /// The cyclic path, first frame repeated at the end.
        path: Vec<String>,
    },

    /// A frame already established as [`FrameKind::Static`] received a
    /// dynamic `set_transform` call, or vice versa.
    ///
    /// Each child frame's relationship to its parent is one kind or the
    /// other for as long as it is registered — see the
    /// [module documentation](self) for why this is the one thing about a
    /// frame's identity `set_transform` does treat as fixed, in contrast to
    /// the parent itself, which is allowed to change across a dynamic
    /// frame's history.
    #[error(
        "frame {frame:?} is already registered as a {existing} frame; cannot also register it \
         as {requested} without removing it first"
    )]
    FrameKindConflict {
        /// The conflicted frame's name.
        frame: String,
        /// The kind already on record.
        existing: FrameKind,
        /// The kind the rejected call asked for.
        requested: FrameKind,
    },

    /// A dynamic frame is registered but its history is empty.
    ///
    /// Unreachable through the public API — [`crate::buffer::TransformBuffer::set_transform`]
    /// never leaves a dynamic frame's history empty after a successful
    /// insert, since pruning only removes entries strictly older than the
    /// newest — but kept as a checked, typed outcome rather than a panic on
    /// the (currently unreachable) chance a future change to the pruning
    /// rule violates that invariant.
    #[error("frame {frame:?} is registered but its transform history is empty")]
    EmptyHistory {
        /// The frame with the empty history.
        frame: String,
    },

    /// A lookup's walk reached a dynamic frame whose buffered history does
    /// not bracket the requested time, and tf2 does not extrapolate (its
    /// upstream implementation removed polynomial extrapolation entirely;
    /// this crate matches that by never offering it).
    #[error(
        "lookup at {requested} for frame {frame:?} would extrapolate into the {direction} \
         beyond the buffered bound {bound}"
    )]
    Extrapolation {
        /// The specific hop in the walk whose history could not answer the
        /// query — not necessarily the lookup's original `target` or
        /// `source`.
        frame: String,
        /// The time that was requested.
        requested: TfStamp,
        /// The nearest buffered bound on the side extrapolation would have
        /// crossed.
        bound: TfStamp,
        /// Which side of the buffered history the request fell outside of.
        direction: ExtrapolationDirection,
    },

    /// A transform's translation has a non-finite (`NaN` or `±∞`) component.
    #[error("non-finite translation component in the transform to frame {child:?}")]
    NonFiniteTranslation {
        /// The child frame the offending transform was being set for.
        child: String,
    },

    /// A transform's rotation quaternion has zero (or non-finite) norm, so
    /// it cannot be normalized into a well-defined rotation.
    #[error(
        "degenerate (zero-norm or non-finite) rotation quaternion in the transform to frame \
         {child:?}"
    )]
    DegenerateQuaternion {
        /// The child frame the offending transform was being set for.
        child: String,
    },

    /// [`crate::time::TfStamp::to_ros_time`]'s whole-seconds component does
    /// not fit `builtin_interfaces/Time`'s `i32` range.
    #[error("{nanos}ns does not fit builtin_interfaces/Time's i32-second wire range")]
    RosTimeRangeExceeded {
        /// The nanosecond count that failed to narrow.
        nanos: i64,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn frame_kind_displays_lowercase() {
        assert_eq!(FrameKind::Static.to_string(), "static");
        assert_eq!(FrameKind::Dynamic.to_string(), "dynamic");
    }

    #[test]
    fn extrapolation_direction_displays_lowercase() {
        assert_eq!(ExtrapolationDirection::Past.to_string(), "past");
        assert_eq!(ExtrapolationDirection::Future.to_string(), "future");
    }

    #[test]
    fn unknown_frame_names_the_frame() {
        let err = TfError::UnknownFrame {
            frame: "odom".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "unknown frame \"odom\": no set_transform call has ever registered it"
        );
    }

    #[test]
    fn cycle_joins_the_path_with_arrows() {
        let err = TfError::Cycle {
            path: vec!["a".to_owned(), "b".to_owned(), "a".to_owned()],
        };
        assert_eq!(err.to_string(), "frame graph cycle: a -> b -> a");
    }

    #[test]
    fn frame_kind_conflict_names_both_kinds() {
        let err = TfError::FrameKindConflict {
            frame: "base_link".to_owned(),
            existing: FrameKind::Static,
            requested: FrameKind::Dynamic,
        };
        assert_eq!(
            err.to_string(),
            "frame \"base_link\" is already registered as a static frame; cannot also \
             register it as dynamic without removing it first"
        );
    }

    #[test]
    fn extrapolation_names_the_failing_hop_and_direction() {
        let err = TfError::Extrapolation {
            frame: "base_link".to_owned(),
            requested: TfStamp::from_nanos(500),
            bound: TfStamp::from_nanos(1_000),
            direction: ExtrapolationDirection::Past,
        };
        assert_eq!(
            err.to_string(),
            "lookup at 500 for frame \"base_link\" would extrapolate into the past beyond the \
             buffered bound 1000"
        );
    }

    #[test]
    fn errors_are_structurally_comparable() {
        let a = TfError::UnknownFrame {
            frame: "x".to_owned(),
        };
        let b = TfError::UnknownFrame {
            frame: "x".to_owned(),
        };
        let c = TfError::UnknownFrame {
            frame: "y".to_owned(),
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
