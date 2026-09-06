//! [`ExportError`] — everything that can go wrong sending an OTLP payload.

use thiserror::Error;

/// Errors from [`crate::export::OtlpClient`] and
/// [`crate::export::OtlpExporter`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ExportError {
    /// The underlying `oxihttp` client could not be built.
    #[error("failed to build the OTLP HTTP client: {0}")]
    ClientBuild(#[source] oxihttp::OxiHttpError),

    /// Every retry attempt failed at the transport level (connect
    /// refused, DNS failure, timeout, ...).
    #[error("OTLP export request to {endpoint} failed after {attempts} attempt(s): {source}")]
    Request {
        /// The target URL.
        endpoint: String,
        /// Total HTTP attempts made, including the first.
        attempts: u32,
        /// The last attempt's transport error.
        #[source]
        source: oxihttp::OxiHttpError,
    },

    /// The collector returned a non-success HTTP status that this crate
    /// either does not retry (any 4xx) or gave up retrying (a 5xx after
    /// exhausting [`crate::export::RetryConfig::max_retries`]).
    #[error("OTLP collector at {endpoint} returned HTTP {status} after {attempts} attempt(s)")]
    Status {
        /// The target URL.
        endpoint: String,
        /// The last HTTP status code received.
        status: u16,
        /// Total HTTP attempts made, including the first.
        attempts: u32,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn status_error_message_includes_the_endpoint_status_and_attempts() {
        let err = ExportError::Status {
            endpoint: "http://localhost:4318/v1/traces".to_owned(),
            status: 503,
            attempts: 3,
        };
        let message = err.to_string();
        assert!(message.contains("localhost:4318/v1/traces"));
        assert!(message.contains("503"));
        assert!(message.contains('3'));
    }
}
