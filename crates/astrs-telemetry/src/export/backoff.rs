//! [`RetryConfig`] — exponential backoff parameters for retrying a failed
//! OTLP export.

use std::time::Duration;

/// How an [`crate::export::OtlpClient`] retries a failed export: up to
/// `max_retries` additional attempts (so `max_retries + 1` total
/// attempts), with delays growing geometrically from `initial_backoff` to
/// a ceiling of `max_backoff`.
///
/// No jitter is added: the blueprint's requirement is "retry with backoff
/// on 5xx/connect errors," and a deterministic delay sequence is
/// considerably easier to write a fast, non-flaky test against (see
/// `tests/exporter_retry_backoff.rs`) than a jittered one, at the cost of
/// every AstRS process in a fleet potentially retrying in lockstep after
/// a shared collector outage — an acceptable trade for a single-collector
/// deployment, and revisitable if a fleet-scale thundering-herd problem
/// ever actually shows up.
///
/// # Examples
///
/// ```
/// use astrs_telemetry::export::RetryConfig;
/// use std::time::Duration;
///
/// let retry = RetryConfig::new(3, Duration::from_millis(100), Duration::from_secs(1));
/// assert_eq!(retry.delay_for_attempt(1), Duration::from_millis(100));
/// assert_eq!(retry.delay_for_attempt(2), Duration::from_millis(200));
/// assert_eq!(retry.delay_for_attempt(3), Duration::from_millis(400));
/// // Capped once the geometric growth would exceed `max_backoff`.
/// assert_eq!(retry.delay_for_attempt(10), Duration::from_secs(1));
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryConfig {
    /// How many additional attempts to make after the first one fails.
    pub max_retries: u32,
    /// The delay before the first retry.
    pub initial_backoff: Duration,
    /// The delay never grows past this.
    pub max_backoff: Duration,
    /// The geometric growth factor applied per additional retry.
    pub backoff_multiplier: f64,
}

impl RetryConfig {
    /// Builds a config with a `2.0` backoff multiplier.
    #[must_use]
    pub const fn new(max_retries: u32, initial_backoff: Duration, max_backoff: Duration) -> Self {
        Self {
            max_retries,
            initial_backoff,
            max_backoff,
            backoff_multiplier: 2.0,
        }
    }

    /// A config that never retries: `max_retries: 0`. Useful for tests
    /// that want a single deterministic HTTP attempt.
    #[must_use]
    pub const fn none() -> Self {
        Self::new(0, Duration::ZERO, Duration::ZERO)
    }

    /// The delay before retry attempt number `attempt` (`1` for the
    /// first retry, `2` for the second, ...): `initial_backoff *
    /// backoff_multiplier^(attempt - 1)`, capped at `max_backoff`.
    ///
    /// `attempt: 0` returns [`Duration::ZERO`] (there is no delay before
    /// the *first* attempt, only before retries).
    #[must_use]
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }
        let scale = self
            .backoff_multiplier
            .powi(i32::try_from(attempt - 1).unwrap_or(i32::MAX));
        let scaled_secs = self.initial_backoff.as_secs_f64() * scale;
        let capped_secs = scaled_secs.min(self.max_backoff.as_secs_f64());
        Duration::try_from_secs_f64(capped_secs).unwrap_or(self.max_backoff)
    }
}

impl Default for RetryConfig {
    /// Five retries, starting at 200ms and capping at 30s — a reasonable
    /// default for an OTLP collector that might be mid-restart.
    fn default() -> Self {
        Self::new(5, Duration::from_millis(200), Duration::from_secs(30))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn first_attempt_has_no_delay() {
        assert_eq!(RetryConfig::default().delay_for_attempt(0), Duration::ZERO);
    }

    #[test]
    fn delay_grows_geometrically_then_caps() {
        let retry = RetryConfig::new(10, Duration::from_millis(100), Duration::from_secs(1));
        assert_eq!(retry.delay_for_attempt(1), Duration::from_millis(100));
        assert_eq!(retry.delay_for_attempt(2), Duration::from_millis(200));
        assert_eq!(retry.delay_for_attempt(3), Duration::from_millis(400));
        assert_eq!(retry.delay_for_attempt(4), Duration::from_millis(800));
        assert_eq!(retry.delay_for_attempt(5), Duration::from_secs(1), "capped");
        assert_eq!(retry.delay_for_attempt(100), Duration::from_secs(1));
    }

    #[test]
    fn none_never_delays() {
        let retry = RetryConfig::none();
        assert_eq!(retry.max_retries, 0);
        assert_eq!(retry.delay_for_attempt(1), Duration::ZERO);
    }

    #[test]
    fn default_is_five_retries_from_200ms_to_30s() {
        let retry = RetryConfig::default();
        assert_eq!(retry.max_retries, 5);
        assert_eq!(retry.initial_backoff, Duration::from_millis(200));
        assert_eq!(retry.max_backoff, Duration::from_secs(30));
    }
}
