//! [`OtlpClient`] — an OTLP/HTTP+JSON sender with its own retry/backoff
//! loop over [`oxihttp`].
//!
//! Blueprint §13/§18.1: this is the "own ~1.5k-line implementation over
//! oxihttp" — no `opentelemetry-otlp`, no `tonic` (both banned). Retrying
//! is implemented here rather than via `oxihttp`'s own `RetryPolicy` so
//! this crate keeps full control of exactly which conditions are
//! retryable (5xx and connect-class failures, per the blueprint) and of
//! the attempt/backoff bookkeeping the bounded-queue exporter needs to
//! report.

use std::time::Duration;

use serde::Serialize;

use crate::export::backoff::RetryConfig;
use crate::export::error::ExportError;

/// Sends OTLP/HTTP+JSON payloads with retry-with-backoff on 5xx
/// responses and connect-class errors.
///
/// # Examples
///
/// ```no_run
/// use astrs_telemetry::export::{OtlpClient, RetryConfig};
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let client = OtlpClient::new(RetryConfig::default())?;
/// let payload = serde_json::json!({"resourceSpans": []});
/// client.post_json("http://localhost:4318/v1/traces", &payload).await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct OtlpClient {
    http: oxihttp::Client,
    retry: RetryConfig,
}

impl OtlpClient {
    /// Builds a client with the given retry policy and 10-second
    /// connect/read timeouts.
    ///
    /// # Errors
    ///
    /// [`ExportError::ClientBuild`] if the underlying HTTP client could
    /// not be constructed (an `oxihttp` implementation detail — this is
    /// not expected to fail under normal configuration).
    pub fn new(retry: RetryConfig) -> Result<Self, ExportError> {
        Self::with_timeouts(retry, Duration::from_secs(10), Duration::from_secs(10))
    }

    /// Builds a client with explicit connect/read timeouts, for tests
    /// that need a short timeout against a deliberately slow or
    /// non-responding sink.
    ///
    /// # Errors
    ///
    /// See [`OtlpClient::new`].
    pub fn with_timeouts(
        retry: RetryConfig,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<Self, ExportError> {
        let http = oxihttp::Client::builder()
            .connect_timeout(connect_timeout)
            .read_timeout(read_timeout)
            .build()
            .map_err(ExportError::ClientBuild)?;
        Ok(Self { http, retry })
    }

    /// The configured retry policy.
    #[must_use]
    pub const fn retry_config(&self) -> &RetryConfig {
        &self.retry
    }

    /// POSTs `payload` as JSON to `endpoint`, retrying on a 5xx response
    /// or a connect-class error per [`OtlpClient::retry_config`].
    ///
    /// Returns the total number of HTTP attempts made (`1` if the first
    /// attempt succeeded).
    ///
    /// # Errors
    ///
    /// [`ExportError::Status`] if every attempt returned a non-success
    /// status (a 4xx is never retried: retrying a client error would
    /// just repeat it); [`ExportError::Request`] if every attempt failed
    /// at the transport level.
    pub async fn post_json<T: Serialize>(
        &self,
        endpoint: &str,
        payload: &T,
    ) -> Result<u32, ExportError> {
        let mut attempt = 0u32;
        loop {
            if attempt > 0 {
                tokio::time::sleep(self.retry.delay_for_attempt(attempt)).await;
            }
            let outcome = self.try_once(endpoint, payload).await;
            attempt += 1;
            match outcome {
                Ok(()) => return Ok(attempt),
                Err(RetryDecision::Retry(status)) if attempt <= self.retry.max_retries => {
                    tracing::debug!(endpoint, status, attempt, "OTLP export got a 5xx, retrying");
                    continue;
                }
                Err(RetryDecision::Retry(status)) => {
                    return Err(ExportError::Status {
                        endpoint: endpoint.to_owned(),
                        status,
                        attempts: attempt,
                    });
                }
                Err(RetryDecision::ClientError(status)) => {
                    return Err(ExportError::Status {
                        endpoint: endpoint.to_owned(),
                        status,
                        attempts: attempt,
                    });
                }
                Err(RetryDecision::Transport(source)) if attempt <= self.retry.max_retries => {
                    tracing::debug!(endpoint, attempt, error = %source, "OTLP export transport error, retrying");
                    continue;
                }
                Err(RetryDecision::Transport(source)) => {
                    return Err(ExportError::Request {
                        endpoint: endpoint.to_owned(),
                        attempts: attempt,
                        source,
                    });
                }
            }
        }
    }

    /// Makes exactly one HTTP attempt, classifying the outcome for the
    /// retry loop in [`OtlpClient::post_json`].
    async fn try_once<T: Serialize>(
        &self,
        endpoint: &str,
        payload: &T,
    ) -> Result<(), RetryDecision> {
        let request = self
            .http
            .post(endpoint)
            .map_err(RetryDecision::Transport)?
            .json(payload)
            .map_err(RetryDecision::Transport)?;
        match request.send().await {
            Ok(response) if response.status().is_success() => Ok(()),
            Ok(response) if response.status().is_server_error() => {
                Err(RetryDecision::Retry(response.status().as_u16()))
            }
            Ok(response) => Err(RetryDecision::ClientError(response.status().as_u16())),
            Err(source) => Err(RetryDecision::Transport(source)),
        }
    }
}

/// The outcome of one HTTP attempt, classified for the retry loop.
enum RetryDecision {
    /// A 5xx response: worth retrying.
    Retry(u16),
    /// A non-5xx, non-success response (typically 4xx): retrying would
    /// just repeat the same client error.
    ClientError(u16),
    /// A connect-class or protocol failure: worth retrying.
    Transport(oxihttp::OxiHttpError),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn builds_with_the_default_retry_policy() {
        let client = OtlpClient::new(RetryConfig::default()).unwrap();
        assert_eq!(client.retry_config().max_retries, 5);
    }

    #[tokio::test]
    async fn a_connect_failure_against_an_unroutable_address_is_a_request_error() {
        // 192.0.2.0/24 is TEST-NET-1 (RFC 5737): reserved for documentation,
        // guaranteed to never route, so this fails fast without a real
        // network dependency for the test.
        let client = OtlpClient::with_timeouts(
            RetryConfig::none(),
            Duration::from_millis(200),
            Duration::from_millis(200),
        )
        .unwrap();
        let result = client
            .post_json("http://192.0.2.1:4318/v1/traces", &serde_json::json!({}))
            .await;
        assert!(matches!(
            result,
            Err(ExportError::Request { attempts: 1, .. })
        ));
    }
}
