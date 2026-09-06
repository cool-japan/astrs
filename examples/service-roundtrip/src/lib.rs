//! Shared vocabulary for `service-roundtrip` (blueprint §9.4).
//!
//! ```text
//!   [client] ──request(std/core/v1/Int64)──► [server]
//!       ▲                                        │
//!       └────────response(std/core/v1/Int64)─────┘
//! ```
//!
//! # A service is two edges, not a subsystem
//!
//! §9.4 is explicit: *"All three ride ordinary edges plus metadata
//! correlation — no separate RPC subsystem."* A request is a message with a
//! `request_id` in its metadata; a response is a message that copies that id
//! back. Everything else — the queue, the plane, the type check — is the same
//! machinery every other edge uses, which is why a service call can be
//! recorded, replayed and traced like any other message.
//!
//! The manifest's `pattern: service-client` / `service-server` declares the
//! roles so `astrs validate` can check the pair is wired both ways (a
//! half-wired service is a `PartialPatternPair` diagnostic), and so the
//! scheduler grants correlated messages queue-eviction immunity (§11.2): a
//! response must not be dropped because a camera filled the queue behind it.
//!
//! # The cycle is legal, and that is the point
//!
//! `client → server → client` is a directed cycle. §5.2 makes cycles covered
//! by a service/action correlation *legal* precisely because the correlation
//! bounds them: every request has exactly one response, so the loop
//! terminates. `astrs validate` on this manifest reports nothing.

use serde::{Deserialize, Serialize};

/// The client's output and the server's input.
pub const REQUEST_PORT: &str = "request";

/// The server's output and the client's input.
pub const RESPONSE_PORT: &str = "response";

/// The client's timer input, which paces the calls.
pub const TICK_PORT: &str = "tick";

/// Environment variable naming the file the client writes its result to.
pub const ENV_RESULT_PATH: &str = "SERVICE_RESULT";

/// Environment variable overriding how many calls the client makes.
pub const ENV_CALLS: &str = "SERVICE_CALLS";

/// How many round trips the client performs by default.
pub const DEFAULT_CALLS: u64 = 8;

/// Where the client writes its result when the manifest names no path.
#[must_use]
pub fn default_result_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-service-roundtrip.json")
}

/// The file the client writes its result to.
#[must_use]
pub fn result_path() -> std::path::PathBuf {
    std::env::var(ENV_RESULT_PATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default_result_path, std::path::PathBuf::from)
}

/// How many round trips this run should perform.
#[must_use]
pub fn call_budget() -> u64 {
    std::env::var(ENV_CALLS)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_CALLS)
}

/// The answer the server owes for `value`.
///
/// A pure function so both ends agree without sharing state, and so the
/// client can check the answer rather than merely count it.
#[must_use]
pub const fn expected_answer(value: i64) -> i64 {
    value.saturating_mul(value)
}

/// What the client observed, written as JSON when it finishes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundTripResult {
    /// How many requests the client issued.
    pub requests: u64,
    /// How many responses came back correlated to one of them.
    pub responses: u64,
    /// How many of those carried the right answer.
    pub correct: u64,
    /// How many responses could not be matched to an outstanding request.
    pub uncorrelated: u64,
    /// The slowest request-to-response time, in microseconds.
    pub max_latency_us: u64,
}

impl RoundTripResult {
    /// Whether every request was answered exactly once and correctly.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.requests > 0
            && self.responses == self.requests
            && self.correct == self.requests
            && self.uncorrelated == 0
    }

    /// Renders the result as pretty JSON.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if the result cannot be serialised, which its
    /// field types make impossible in practice.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_answer_is_the_square() {
        assert_eq!(expected_answer(7), 49);
        assert_eq!(expected_answer(-3), 9);
        // Saturating rather than wrapping: a probe must not panic in debug
        // and quietly disagree in release.
        assert_eq!(expected_answer(i64::MAX), i64::MAX);
    }

    #[test]
    fn a_clean_result_needs_every_answer() {
        let clean = RoundTripResult {
            requests: 8,
            responses: 8,
            correct: 8,
            uncorrelated: 0,
            max_latency_us: 900,
        };
        assert!(clean.is_clean());

        let short = RoundTripResult {
            responses: 7,
            correct: 7,
            ..clean.clone()
        };
        assert!(!short.is_clean());

        let wrong = RoundTripResult {
            correct: 7,
            ..clean.clone()
        };
        assert!(!wrong.is_clean());
    }

    #[test]
    fn the_result_round_trips_as_json() {
        let result = RoundTripResult {
            requests: 3,
            responses: 3,
            correct: 3,
            uncorrelated: 0,
            max_latency_us: 42,
        };
        let parsed: RoundTripResult = serde_json::from_str(&result.to_json().unwrap()).unwrap();
        assert_eq!(parsed, result);
    }

    #[test]
    fn the_result_path_defaults_under_the_temp_dir() {
        assert!(default_result_path().starts_with(std::env::temp_dir()));
    }
}
