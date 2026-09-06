//! The crate's top-level error type.
//!
//! Each subsystem also exposes a narrower, subsystem-specific error
//! ([`crate::sampler::SamplerError`], [`crate::export::ExportError`]) for
//! callers that only touch that subsystem; [`TelemetryError`] is the type
//! returned by crate-level orchestration ([`crate::init_telemetry`], the
//! exporter's fallible shutdown) that can fail for more than one reason.

use thiserror::Error;

#[cfg(feature = "telemetry-export")]
use crate::export::ExportError;
use crate::sampler::SamplerError;

/// Everything that can go wrong setting up or running AstRS telemetry.
///
/// `#[non_exhaustive]`: new failure modes are appended, never inserted,
/// matching the append-only evolution the blueprint (§3.4) requires of
/// every AstRS error surface that crosses a crate boundary.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TelemetryError {
    /// [`crate::init_telemetry`] was called after a global `tracing`
    /// subscriber was already installed (by this crate or by anything
    /// else in the process).
    ///
    /// `tracing_subscriber`'s own `try_init` returns an opaque error for
    /// this case; a process may legitimately call [`crate::init_telemetry`]
    /// more than once in tests that share one binary, so this is a
    /// reportable error rather than a panic.
    #[error(
        "a global tracing subscriber is already installed \
         (astrs_telemetry::init_telemetry called more than once in this process)"
    )]
    AlreadyInitialized,

    /// The configured `RUST_LOG`-style filter directive string was not
    /// valid `tracing_subscriber::EnvFilter` syntax.
    ///
    /// Carries the rendered message rather than
    /// `tracing_subscriber::filter::ParseError` itself, so this crate's
    /// public error type does not tie its shape to that crate's internal
    /// error representation.
    #[error("invalid telemetry filter directive {directive:?}: {message}")]
    InvalidFilter {
        /// The directive string that failed to parse.
        directive: String,
        /// The parser's rendered error message.
        message: String,
    },

    /// Building or reading span-context metadata failed — an internal
    /// metadata key ([`crate::propagation`]) turned out not to be a valid
    /// [`astrs_wire::ParamKey`].
    ///
    /// Unreachable in practice (the keys this crate mints are fixed,
    /// valid-by-construction constants — see the module-level test in
    /// [`crate::propagation`] that asserts this), but `Metadata::insert`
    /// still returns a `Result` and this crate does not `unwrap()` it
    /// away.
    #[error(transparent)]
    Metadata(#[from] astrs_wire::IdError),

    /// The system-resource sampler ([`crate::sampler`]) failed.
    #[error(transparent)]
    Sampler(#[from] SamplerError),

    /// The OTLP exporter ([`crate::export`]) failed. Only exists when the
    /// `telemetry-export` feature (default on) is enabled.
    #[cfg(feature = "telemetry-export")]
    #[error(transparent)]
    Export(#[from] ExportError),
}

/// Result alias used throughout this crate's public, crate-level API.
pub type Result<T> = std::result::Result<T, TelemetryError>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn already_initialized_renders_a_stable_message() {
        let err = TelemetryError::AlreadyInitialized;
        assert!(err.to_string().contains("init_telemetry"));
    }

    #[test]
    fn invalid_filter_message_includes_the_directive() {
        let err = TelemetryError::InvalidFilter {
            directive: "astrs_daemon=noisy".to_owned(),
            message: "unknown level".to_owned(),
        };
        assert!(err.to_string().contains("astrs_daemon=noisy"));
        assert!(err.to_string().contains("unknown level"));
    }

    #[test]
    fn metadata_error_converts_via_from() {
        let id_err = astrs_wire::ParamKey::new("bad key").unwrap_err();
        let err: TelemetryError = id_err.into();
        assert!(matches!(err, TelemetryError::Metadata(_)));
    }
}
