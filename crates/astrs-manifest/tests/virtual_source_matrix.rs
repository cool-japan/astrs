//! An end-to-end parse matrix for every `astrs/...` virtual source form
//! (blueprint §8.4), valid and invalid, driven through the *real* pipeline
//! a manifest author hits: an input's `source:` field, resolved by
//! [`Manifest::validate`]. `src/virtual_source.rs`'s own unit tests already
//! cover [`recognize_virtual_source`] exhaustively in isolation; this file
//! is the complementary check that the low-level grammar and the
//! validation pass agree with each other for every form in one place,
//! rather than trusting that agreement implicitly.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_manifest::{Manifest, UnresolvedReferenceReason, ValidationErrorKind};

/// Build a minimal one-node manifest whose sole input's `source:` is
/// `source`, and run [`Manifest::validate`] on it.
fn validate_source(source: &str) -> Result<(), Vec<astrs_manifest::ValidationError>> {
    let yaml = format!(
        "nodes:\n  - id: consumer\n    path: ./consumer\n    inputs:\n      tick: {source}\n"
    );
    let manifest = Manifest::from_yaml_str(&yaml)
        .unwrap_or_else(|e| panic!("`{source}` as a YAML value must parse: {e}"));
    manifest
        .validate()
        .map_err(|errors| errors.into_iter().collect())
}

/// Every source string in this list must validate clean.
const VALID_FORMS: &[&str] = &[
    "astrs/timer/millis/1",
    "astrs/timer/millis/500",
    "astrs/timer/secs/1",
    "astrs/timer/secs/3600",
    "astrs/timer/hz/1",
    "astrs/timer/hz/1000",
    "astrs/logs",
    "astrs/logs/trace",
    "astrs/logs/debug",
    "astrs/logs/info",
    "astrs/logs/warn",
    "astrs/logs/error",
    "astrs/logs/error/detector",
    "astrs/status",
];

#[test]
fn every_valid_virtual_source_form_validates_clean() {
    for source in VALID_FORMS {
        let result = validate_source(source);
        assert!(
            result.is_ok(),
            "`{source}` should validate clean, got: {:#?}",
            result.err()
        );
    }
}

/// A predicate over the reason [`Manifest::validate`] reported, matching
/// only the discriminant — several [`UnresolvedReferenceReason`] variants
/// carry data [`INVALID_FORMS`] does not need to spell out per entry.
type ReasonPredicate = fn(&UnresolvedReferenceReason) -> bool;

/// Every entry pairs an invalid source string with the
/// [`UnresolvedReferenceReason`] variant [`Manifest::validate`] must
/// report for it.
const INVALID_FORMS: &[(&str, ReasonPredicate)] = &[
    ("astrs/timer/hz/0", |r| {
        matches!(r, UnresolvedReferenceReason::InvalidTimer(_))
    }),
    ("astrs/timer/hz/-5", |r| {
        matches!(r, UnresolvedReferenceReason::InvalidTimer(_))
    }),
    ("astrs/timer/hz/abc", |r| {
        matches!(r, UnresolvedReferenceReason::InvalidTimer(_))
    }),
    ("astrs/timer/fortnights/1", |r| {
        matches!(r, UnresolvedReferenceReason::InvalidTimer(_))
    }),
    ("astrs/timer/hz", |r| {
        matches!(r, UnresolvedReferenceReason::InvalidTimer(_))
    }),
    ("astrs/timer", |r| {
        matches!(r, UnresolvedReferenceReason::InvalidTimer(_))
    }),
    ("astrs/logs/critical", |r| {
        matches!(r, UnresolvedReferenceReason::InvalidLogs(_))
    }),
    ("astrs/logs/warn/detector/extra", |r| {
        matches!(r, UnresolvedReferenceReason::InvalidLogs(_))
    }),
    ("astrs/status/extra", |r| {
        matches!(r, UnresolvedReferenceReason::InvalidStatus(_))
    }),
    ("astrs/bogus", |r| {
        matches!(r, UnresolvedReferenceReason::UnknownVirtualFamily)
    }),
    ("astrs/", |r| {
        matches!(r, UnresolvedReferenceReason::UnknownVirtualFamily)
    }),
    // Not `astrs/...` at all, but also not a resolvable `node/output` —
    // included here as the boundary case that must *not* be misdiagnosed
    // as a virtual-source problem.
    ("astrs", |r| {
        matches!(r, UnresolvedReferenceReason::Malformed)
    }),
];

#[test]
fn every_invalid_virtual_source_form_reports_the_expected_reason() {
    for (source, predicate) in INVALID_FORMS {
        let errors = match validate_source(source) {
            Ok(()) => panic!("`{source}` should fail validation, but it passed"),
            Err(errors) => errors,
        };
        assert_eq!(errors.len(), 1, "`{source}`: errors were {errors:#?}");
        match &errors[0].kind {
            ValidationErrorKind::UnresolvedReference { reason, .. } => {
                assert!(
                    predicate(reason),
                    "`{source}`: unexpected reason {reason:?}"
                );
            }
            other => panic!("`{source}`: expected UnresolvedReference, got {other:?}"),
        }
    }
}
