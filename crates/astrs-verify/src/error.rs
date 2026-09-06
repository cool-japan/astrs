//! The error taxonomy for graph proving.
//!
//! Two kinds of thing can go wrong when proving a dataflow, and they are
//! deliberately kept apart:
//!
//! - [`VerifyError`] — the *proof run itself* could not be set up. The
//!   model could not be extracted, a profile did not parse, an integer
//!   scale overflowed. Nothing was proved and nothing was refuted; the
//!   caller gets an `Err`.
//! - A *discharged obligation that failed* — the graph is genuinely
//!   broken. That is not an error: it is the answer, and it travels in a
//!   [`VerificationReport`](crate::VerificationReport) as
//!   [`Discharge::Violated`](crate::Discharge::Violated) together with its
//!   counterexample.
//!
//! Anything a solver reports that is neither "holds" nor "violated"
//! (`unknown`, a resource limit) is likewise *not* an error — it is
//! [`Discharge::Inconclusive`](crate::Discharge::Inconclusive). A verifier
//! that reported "sound" because the solver gave up would be worse than
//! no verifier at all.

use std::fmt;

/// The proof run could not be set up or completed.
///
/// See this module's docs for what deliberately is *not* in here: a
/// violated obligation and an inconclusive solver run are both ordinary
/// report outcomes, not errors.
///
/// `PartialEq` but not `Eq`, because [`ProfileError`] quotes floating-point
/// declarations back at the author — see its own docs.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// A verification profile could not be parsed.
    #[error("verification profile is not valid YAML: {message}")]
    ProfileParse {
        /// The underlying parser message.
        message: String,
    },

    /// A verification profile parsed but is internally inconsistent, or
    /// refers to something the manifest does not contain.
    #[error("verification profile is inconsistent: {0}")]
    ProfileInvalid(#[from] ProfileError),

    /// A profile file could not be read.
    #[error("failed to read verification profile `{path}`: {message}")]
    ProfileIo {
        /// The path that could not be read.
        path: String,
        /// The underlying I/O error, rendered.
        message: String,
    },

    /// The manifest's declared rates cannot be put on a common integer
    /// scale without overflowing.
    ///
    /// Every obligation encoding is integer-only (linear integer
    /// arithmetic — see [`crate::scale`]), which requires a single
    /// analysis window whose length is an exact multiple of every
    /// declared trigger period. That window is the least common multiple
    /// of the declared rate denominators; a manifest that mixes many
    /// mutually prime periods can push it past what fits.
    #[error(
        "declared trigger rates have no common integer analysis window (would need {needed} seconds, limit {limit}); \
         simplify the timer periods or give them a common divisor"
    )]
    ScaleOverflow {
        /// The window length, in seconds, that would have been needed.
        needed: u128,
        /// The largest window length this crate accepts.
        limit: u128,
    },

    /// A duration in the manifest or profile is not representable in the
    /// integer nanosecond scale the encodings use.
    #[error("`{what}` is not a usable duration: {reason}")]
    BadDuration {
        /// Which field held it, e.g. `nodes.detector.inputs.frames.timeout`.
        what: String,
        /// Why it was rejected.
        reason: DurationRejection,
    },
}

/// Why a duration could not be converted to the integer nanosecond scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DurationRejection {
    /// The value was negative.
    Negative,
    /// The value was not finite (`NaN` or an infinity).
    NotFinite,
    /// The value in nanoseconds exceeds [`i64::MAX`], i.e. roughly 292
    /// years — far outside anything a dataflow deadline expresses.
    TooLarge,
}

impl fmt::Display for DurationRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Negative => "durations must not be negative",
            Self::NotFinite => "durations must be finite",
            Self::TooLarge => "duration exceeds the representable nanosecond range",
        })
    }
}

/// A verification profile that parsed but does not describe the manifest
/// it was handed with.
///
/// Kept separate from [`VerifyError`] so a caller can surface every
/// profile problem at once (the same reasoning
/// [`astrs_manifest::ValidationErrors`] documents) rather than aborting on
/// the first.
///
/// `PartialEq` but not `Eq`: two variants quote the offending `f64` back
/// verbatim so the message names what the author actually wrote, and a
/// float carries no total equality. Nothing in this crate keys a
/// collection on an error.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ProfileError {
    /// The profile assigns a property to a node id the manifest does not
    /// declare.
    #[error("`{section}` names node `{node}`, which the manifest does not declare")]
    UnknownNode {
        /// The profile section that named it, e.g. `nodes`.
        section: &'static str,
        /// The node id that was named.
        node: String,
    },

    /// The profile assigns a property to a port the named node does not
    /// declare.
    #[error("`{section}` names `{node}/{port}`, which node `{node}` does not declare")]
    UnknownPort {
        /// The profile section that named it.
        section: &'static str,
        /// The node that was named.
        node: String,
        /// The port that was named.
        port: String,
    },

    /// A worst-case execution time interval has `min > max`.
    #[error("node `{node}` declares wcet min {min}s above max {max}s")]
    InvertedWcet {
        /// The node the interval belongs to.
        node: String,
        /// The declared minimum, in seconds.
        min: f64,
        /// The declared maximum, in seconds.
        max: f64,
    },

    /// A declared rate is zero or negative.
    #[error("node `{node}` declares a rate of {rate} Hz; rates must be positive")]
    NonPositiveRate {
        /// The node the rate belongs to.
        node: String,
        /// The rate that was rejected.
        rate: f64,
    },

    /// A declared rate has no exact rational representation this crate can
    /// put on the integer analysis window.
    #[error(
        "node `{node}` declares a rate of {rate} Hz, which has no exact rational form (use a simple fraction such as 30, 7.5 or 0.5)"
    )]
    IrrationalRate {
        /// The node the rate belongs to.
        node: String,
        /// The rate that was rejected.
        rate: f64,
    },

    /// A latency path names endpoints that are not connected in the graph.
    #[error("latency path `{name}` has no route from `{from}` to `{to}` in the graph")]
    DisconnectedPath {
        /// The path's name in the profile.
        name: String,
        /// The declared start node.
        from: String,
        /// The declared end node.
        to: String,
    },

    /// A latency path declares a non-positive budget.
    #[error("latency path `{name}` declares a budget of {budget}s; budgets must be positive")]
    NonPositiveBudget {
        /// The path's name in the profile.
        name: String,
        /// The budget that was rejected.
        budget: f64,
    },

    /// Two latency paths share a name.
    #[error("latency path `{name}` is declared more than once")]
    DuplicatePath {
        /// The duplicated name.
        name: String,
    },

    /// A duration or rate in the profile is not representable.
    #[error("`{what}`: {reason}")]
    BadDuration {
        /// Which profile field held it.
        what: String,
        /// Why it was rejected.
        reason: DurationRejection,
    },
}

/// A convenience alias for results carrying a [`VerifyError`].
pub type Result<T> = std::result::Result<T, VerifyError>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn duration_rejections_render_distinctly() {
        let rendered: Vec<String> = [
            DurationRejection::Negative,
            DurationRejection::NotFinite,
            DurationRejection::TooLarge,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(rendered.len(), 3);
        assert_ne!(rendered[0], rendered[1]);
        assert_ne!(rendered[1], rendered[2]);
    }

    #[test]
    fn scale_overflow_names_both_bounds() {
        let err = VerifyError::ScaleOverflow {
            needed: 1_000_000,
            limit: 1_000,
        };
        let text = err.to_string();
        assert!(text.contains("1000000"), "{text}");
        assert!(text.contains("1000"), "{text}");
    }

    #[test]
    fn profile_errors_flow_into_verify_error() {
        let err: VerifyError = ProfileError::UnknownNode {
            section: "nodes",
            node: "ghost".to_string(),
        }
        .into();
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn bad_duration_names_the_field() {
        let err = VerifyError::BadDuration {
            what: "nodes.detector.inputs.frames.timeout".to_string(),
            reason: DurationRejection::Negative,
        };
        assert!(err.to_string().contains("inputs.frames.timeout"));
        assert!(err.to_string().contains("negative"));
    }

    #[test]
    fn unknown_port_renders_both_halves() {
        let err = ProfileError::UnknownPort {
            section: "outputs",
            node: "camera".to_string(),
            port: "frames".to_string(),
        };
        assert!(err.to_string().contains("camera/frames"));
    }
}
