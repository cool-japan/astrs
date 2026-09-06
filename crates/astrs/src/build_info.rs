//! What this build of AstRS actually contains.
//!
//! A facade whose surface depends on Cargo features has a support problem
//! the underlying crates do not: when someone reports "`astrs::verify`
//! doesn't exist", the answer is almost always a feature that was not
//! enabled, and the fastest way to establish that is to ask the binary.
//! So the feature set is queryable at runtime, not merely at compile time.
//!
//! Three uses, all real:
//!
//! - a node logs [`summary`] at startup, so a recording carries the exact
//!   build that produced it (blueprint §14's replay is only reproducible
//!   against a build you can identify);
//! - `astrs doctor` (§17) reports the toolchain a graph is about to run on;
//! - a bug report pastes one line instead of a `Cargo.toml`.
//!
//! Everything here is `const`-evaluable and allocation-free apart from the
//! two rendering helpers, so calling it on a start-up path costs nothing.

use core::fmt;

/// One optional surface of the facade, and whether this build has it.
///
/// The list is exhaustive over the crate's Cargo features by construction:
/// [`FEATURES`] is written once, and every accessor derives from it, so a
/// feature added to `Cargo.toml` without a line here shows up as a missing
/// entry in this module's own tests rather than as a silently incomplete
/// report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Feature {
    /// The Cargo feature name, exactly as it is written in `Cargo.toml`.
    pub name: &'static str,
    /// Whether this build enabled it.
    pub enabled: bool,
    /// The crate it re-exports, or `None` for a feature that only turns on
    /// a derive or aggregates others.
    pub crate_name: Option<&'static str>,
    /// One line on what it is for.
    pub summary: &'static str,
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:<10} {}  {:<22} {}",
            self.name,
            if self.enabled { "on " } else { "off" },
            self.crate_name.unwrap_or("-"),
            self.summary
        )
    }
}

/// Every optional surface, in the order the crate-level feature table
/// lists them.
pub const FEATURES: &[Feature] = &[
    Feature {
        name: "node",
        enabled: cfg!(feature = "node"),
        crate_name: Some("astrs-node-api"),
        summary: "authoring a node: init, events, outputs, patterns",
    },
    Feature {
        name: "derive",
        enabled: cfg!(feature = "derive"),
        crate_name: Some("astrs-operator-macros"),
        summary: "#[derive(AstrsMessage)] and #[operator]",
    },
    Feature {
        name: "data",
        enabled: cfg!(feature = "data"),
        crate_name: Some("astrs-data"),
        summary: "the columnar data plane and Arrow IPC compatibility",
    },
    Feature {
        name: "time",
        enabled: cfg!(feature = "time"),
        crate_name: Some("astrs-time"),
        summary: "hybrid logical clocks, deadlines, Stamped<T>",
    },
    Feature {
        name: "log",
        enabled: cfg!(feature = "log"),
        crate_name: Some("astrs-log"),
        summary: "structured log records and the astrs/logs fan-out",
    },
    Feature {
        name: "wire",
        enabled: cfg!(feature = "wire"),
        crate_name: Some("astrs-wire"),
        summary: "the control-plane protocol types",
    },
    Feature {
        // No `crate_name`: `arrow-interop` adds no crate to the facade. It
        // switches on the feature of the same name inside `astrs-data` and
        // `astrs-node-api`, both of which are already here under `data` and
        // `node` — which is exactly the case `crate_name: None` exists for.
        name: "arrow-interop",
        enabled: cfg!(feature = "arrow-interop"),
        crate_name: None,
        summary: "zero-copy conversions to/from arrow-rs, and send_arrow",
    },
    Feature {
        name: "operator",
        enabled: cfg!(feature = "operator"),
        crate_name: Some("astrs-operator-api"),
        summary: "the Operator trait and its registry",
    },
    Feature {
        name: "runtime",
        enabled: cfg!(feature = "runtime"),
        crate_name: Some("astrs-runtime"),
        summary: "the operator host",
    },
    Feature {
        name: "manifest",
        enabled: cfg!(feature = "manifest"),
        crate_name: Some("astrs-manifest"),
        summary: "manifest parse, validate and module expansion",
    },
    Feature {
        name: "graph",
        enabled: cfg!(feature = "graph"),
        crate_name: Some("astrs-graph"),
        summary: "the dataflow graph, type checking and placement",
    },
    Feature {
        name: "verify",
        enabled: cfg!(feature = "verify"),
        crate_name: Some("astrs-verify"),
        summary: "SMT graph proofs behind `astrs validate --prove`",
    },
    Feature {
        name: "recording",
        enabled: cfg!(feature = "recording"),
        crate_name: Some("astrs-recording"),
        summary: "the .arec recording container",
    },
    Feature {
        name: "telemetry",
        enabled: cfg!(feature = "telemetry"),
        crate_name: Some("astrs-telemetry"),
        summary: "metrics, tracing setup and OTLP export",
    },
    Feature {
        name: "tui",
        enabled: cfg!(feature = "tui"),
        crate_name: Some("astrs-tui"),
        summary: "the live terminal monitor behind `astrs top`",
    },
];

/// Whether a named feature is enabled in this build.
///
/// Returns `None` for a name this crate does not have a feature by, which
/// is a different answer from "off" and is reported as such: a caller
/// checking `is_enabled("ros2")` today should learn that the feature does
/// not exist yet, not that it happens to be disabled.
#[must_use]
pub fn is_enabled(name: &str) -> Option<bool> {
    FEATURES
        .iter()
        .find(|feature| feature.name == name)
        .map(|feature| feature.enabled)
}

/// Every enabled feature's name, in declaration order.
#[must_use]
pub fn enabled() -> Vec<&'static str> {
    FEATURES
        .iter()
        .filter(|feature| feature.enabled)
        .map(|feature| feature.name)
        .collect()
}

/// Every disabled feature's name, in declaration order.
#[must_use]
pub fn disabled() -> Vec<&'static str> {
    FEATURES
        .iter()
        .filter(|feature| !feature.enabled)
        .map(|feature| feature.name)
        .collect()
}

/// A one-line build identifier, suitable for a log line or a bug report.
///
/// ```
/// let line = astrs::build_info::summary();
/// assert!(line.starts_with("astrs "));
/// # #[cfg(feature = "node")]
/// assert!(line.contains("node"));
/// ```
#[must_use]
pub fn summary() -> String {
    let enabled = enabled();
    if enabled.is_empty() {
        format!("{} {} (no optional features)", crate::NAME, crate::VERSION)
    } else {
        format!(
            "{} {} [{}]",
            crate::NAME,
            crate::VERSION,
            enabled.join(", ")
        )
    }
}

/// The full feature table, one line per feature, for `astrs doctor` and
/// for a human reading a support ticket.
#[must_use]
pub fn report() -> String {
    use fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "{} {}", crate::NAME, crate::VERSION);
    for feature in FEATURES {
        let _ = writeln!(out, "  {feature}");
    }
    out
}

/// Whether this build can prove a dataflow's graph obligations
/// (blueprint §15).
///
/// Distinct from `is_enabled("verify")` in one case that matters: the
/// facade's `verify` feature turns the solver on, but an `astrs-verify`
/// linked by some other path might not have been built with its own
/// `verify` feature. This asks the crate that knows.
#[must_use]
pub fn can_prove() -> bool {
    #[cfg(feature = "verify")]
    {
        crate::verify::solver_available()
    }
    #[cfg(not(feature = "verify"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn every_feature_is_described_exactly_once() {
        let mut names: Vec<&str> = FEATURES.iter().map(|f| f.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "a feature is listed twice");
        for feature in FEATURES {
            assert!(!feature.summary.is_empty(), "{}", feature.name);
        }
    }

    /// The table must stay in step with `Cargo.toml`. Reading the manifest
    /// at test time is what makes that a check rather than a convention:
    /// adding a feature without a [`FEATURES`] entry fails here.
    #[test]
    fn the_table_matches_the_cargo_manifest() {
        let manifest = include_str!("../Cargo.toml");
        let mut declared: Vec<String> = Vec::new();
        let mut in_features = false;
        for line in manifest.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_features = trimmed == "[features]";
                continue;
            }
            if !in_features || trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some((name, _)) = trimmed.split_once('=') {
                declared.push(name.trim().to_string());
            }
        }
        assert!(!declared.is_empty(), "no [features] section was parsed");

        // `default` and `full` are aggregates, not surfaces.
        let aggregates = ["default", "full"];
        for name in &declared {
            if aggregates.contains(&name.as_str()) {
                continue;
            }
            assert!(
                is_enabled(name).is_some(),
                "feature `{name}` is declared in Cargo.toml but missing from build_info::FEATURES"
            );
        }
        for feature in FEATURES {
            assert!(
                declared.iter().any(|name| name == feature.name),
                "build_info lists `{}`, which Cargo.toml does not declare",
                feature.name
            );
        }
    }

    #[test]
    fn unknown_features_are_distinguishable_from_disabled_ones() {
        assert_eq!(is_enabled("ros2"), None);
        assert!(is_enabled("node").is_some());
    }

    #[test]
    fn enabled_and_disabled_partition_the_table() {
        let on = enabled();
        let off = disabled();
        assert_eq!(on.len() + off.len(), FEATURES.len());
        for name in &on {
            assert!(!off.contains(name), "`{name}` is in both halves");
        }
    }

    #[test]
    fn the_summary_names_the_crate_and_version() {
        let line = summary();
        assert!(line.starts_with(crate::NAME), "{line}");
        assert!(line.contains(crate::VERSION), "{line}");
    }

    #[test]
    #[cfg(feature = "node")]
    fn the_default_build_reports_the_node_surface() {
        assert_eq!(is_enabled("node"), Some(true));
        assert_eq!(is_enabled("data"), Some(true));
        assert_eq!(is_enabled("derive"), Some(true));
        assert!(summary().contains("node"));
    }

    #[test]
    #[cfg(not(feature = "verify"))]
    fn a_build_without_the_prover_says_so() {
        assert!(!can_prove());
        assert_eq!(is_enabled("verify"), Some(false));
    }

    #[test]
    #[cfg(feature = "verify")]
    fn a_build_with_the_prover_says_so() {
        assert!(can_prove());
        assert_eq!(is_enabled("verify"), Some(true));
    }

    #[test]
    fn the_report_lists_every_feature() {
        let text = report();
        for feature in FEATURES {
            assert!(
                text.contains(feature.name),
                "{} missing:\n{text}",
                feature.name
            );
        }
        assert!(text.contains(crate::VERSION));
    }

    #[test]
    fn a_feature_renders_with_its_state() {
        let on = Feature {
            name: "x",
            enabled: true,
            crate_name: Some("astrs-x"),
            summary: "does x",
        };
        let off = Feature {
            enabled: false,
            ..on
        };
        assert!(on.to_string().contains("on "));
        assert!(off.to_string().contains("off"));
        assert!(on.to_string().contains("does x"));
        assert_eq!(on.crate_name, Some("astrs-x"));
    }

    #[test]
    fn rendering_is_stable() {
        assert_eq!(summary(), summary());
        assert_eq!(report(), report());
    }
}
