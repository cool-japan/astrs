//! Plain-data counters for the producer, the consumer and the broker.
//!
//! These live in their own portable module for one concrete reason: they are
//! part of the crate's public API on **every** platform, including the ones
//! where the shared-memory plane itself does not exist. A daemon that reports
//! `ProducerStats` in its telemetry (§13) must compile on Windows without a
//! `cfg`, even though it will never observe a nonzero value there.
//!
//! Every counter is monotone and saturating. None of them participate in any
//! algorithm — they are read by telemetry and by tests, never branched on by
//! the data plane — which is why the shared-memory versions of the same
//! quantities are loaded [`Relaxed`](std::sync::atomic::Ordering::Relaxed).
//!
//! # Examples
//!
//! ```
//! use astrs_shm::{ConsumerStats, ProducerStats};
//!
//! let mut produced = ProducerStats::default();
//! produced.published = 97;
//! produced.exhausted = 3;
//!
//! let mut consumed = ConsumerStats::default();
//! consumed.received = 87;
//! consumed.lagged = 10;
//!
//! // The accounting identity every ring upholds: a message is either
//! // delivered or accounted as lost, never neither and never both.
//! assert_eq!(consumed.accounted(), produced.published);
//! // Three of a hundred allocation *attempts* fell back to the daemon path.
//! assert_eq!(produced.fallback_rate_per_thousand(), 30);
//! ```

/// Counters describing what a producer has done.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProducerStats {
    /// Messages committed.
    pub published: u64,
    /// Payload bytes committed.
    pub payload_bytes: u64,
    /// Metadata bytes committed.
    pub meta_bytes: u64,
    /// Allocations refused because no slot could be reclaimed.
    ///
    /// Each one is a fall-back to the reliable daemon path and a tick of
    /// `shm_fallback_total` (blueprint §6.2).
    pub exhausted: u64,
    /// Slots taken back from consumers that had not read them.
    ///
    /// Only nonzero under [`crate::OverflowPolicy::Overwrite`], and it counts
    /// *reader-visible* loss: recycling a slot no consumer still wanted does
    /// not tick this.
    pub overwritten: u64,
    /// Write windows opened and then abandoned.
    pub aborted: u64,
    /// Consumer-table entries evicted after their process died.
    pub evicted_consumers: u64,
    /// Doorbell rings delivered.
    pub doorbell_rings: u64,
}

impl ProducerStats {
    /// How often allocation fell back, per thousand attempts.
    ///
    /// The shape §20.4's performance gate wants: a rate, not a raw count, so
    /// a long-running dataflow's number stays comparable.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::ProducerStats;
    ///
    /// let mut stats = ProducerStats::default();
    /// stats.published = 999;
    /// stats.exhausted = 1;
    /// assert_eq!(stats.fallback_rate_per_thousand(), 1);
    /// assert_eq!(ProducerStats::default().fallback_rate_per_thousand(), 0);
    /// ```
    #[must_use]
    pub const fn fallback_rate_per_thousand(&self) -> u64 {
        match (self.exhausted * 1000).checked_div(self.published + self.exhausted) {
            Some(rate) => rate,
            None => 0,
        }
    }

    /// The mean committed payload size, in bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::ProducerStats;
    ///
    /// let mut stats = ProducerStats::default();
    /// stats.published = 4;
    /// stats.payload_bytes = 400;
    /// assert_eq!(stats.mean_payload_bytes(), 100);
    /// ```
    #[must_use]
    pub const fn mean_payload_bytes(&self) -> u64 {
        match self.payload_bytes.checked_div(self.published) {
            Some(mean) => mean,
            None => 0,
        }
    }
}

/// Counters describing what a consumer has seen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConsumerStats {
    /// Messages delivered.
    pub received: u64,
    /// Messages missed because the producer overwrote them.
    pub lagged: u64,
    /// Payload bytes delivered.
    pub payload_bytes: u64,
    /// Receive attempts that found nothing.
    pub empty_polls: u64,
    /// Doorbell waits entered.
    pub doorbell_waits: u64,
    /// Times a receive lost a race with the producer and re-ran its checks.
    pub race_retries: u64,
}

impl ConsumerStats {
    /// Every message this consumer accounted for, delivered or lost.
    ///
    /// The left-hand side of the ring's accounting identity: for a consumer
    /// that attached at sequence 1 and ran to the end of a stream, this must
    /// equal the producer's `published`.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::ConsumerStats;
    ///
    /// let mut stats = ConsumerStats::default();
    /// stats.received = 7;
    /// stats.lagged = 3;
    /// assert_eq!(stats.accounted(), 10);
    /// ```
    #[must_use]
    pub const fn accounted(&self) -> u64 {
        self.received + self.lagged
    }

    /// The fraction of accounted messages that were lost, per thousand.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::ConsumerStats;
    ///
    /// let mut stats = ConsumerStats::default();
    /// stats.received = 990;
    /// stats.lagged = 10;
    /// assert_eq!(stats.loss_per_thousand(), 10);
    /// ```
    #[must_use]
    pub const fn loss_per_thousand(&self) -> u64 {
        match (self.lagged * 1000).checked_div(self.accounted()) {
            Some(rate) => rate,
            None => 0,
        }
    }
}

/// A snapshot of a broker's counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BrokerStats {
    /// Descriptors handed out.
    pub served: u64,
    /// Requests refused.
    pub refused: u64,
    /// Consumer doorbells wired to producers.
    pub doorbells: u64,
    /// Doorbell registrations relayed on to a producer's own process.
    pub doorbells_relayed: u64,
    /// Segments marked closed because their producer died.
    pub closed_on_death: u64,
    /// Segments unlinked after draining.
    pub swept: u64,
    /// Consumer-table entries evicted after their process died.
    pub evicted_consumers: u64,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn producer_rates_handle_the_empty_case() {
        assert_eq!(ProducerStats::default().fallback_rate_per_thousand(), 0);
        assert_eq!(ProducerStats::default().mean_payload_bytes(), 0);
    }

    #[test]
    fn producer_rates_are_computed_over_attempts_not_successes() {
        let stats = ProducerStats {
            published: 750,
            exhausted: 250,
            payload_bytes: 750 * 64,
            ..Default::default()
        };
        assert_eq!(stats.fallback_rate_per_thousand(), 250);
        assert_eq!(stats.mean_payload_bytes(), 64);
    }

    #[test]
    fn consumer_accounting_is_additive() {
        let stats = ConsumerStats {
            received: 12,
            lagged: 5,
            ..Default::default()
        };
        assert_eq!(stats.accounted(), 17);
        assert_eq!(ConsumerStats::default().accounted(), 0);
        assert_eq!(ConsumerStats::default().loss_per_thousand(), 0);
        assert_eq!(
            ConsumerStats {
                received: 0,
                lagged: 4,
                ..Default::default()
            }
            .loss_per_thousand(),
            1000
        );
    }

    #[test]
    fn broker_stats_default_to_zero() {
        let stats = BrokerStats::default();
        assert_eq!(stats.served, 0);
        assert_eq!(stats.refused, 0);
        assert_eq!(stats.doorbells, 0);
        assert_eq!(stats.doorbells_relayed, 0);
        assert_eq!(stats.closed_on_death, 0);
        assert_eq!(stats.swept, 0);
        assert_eq!(stats.evicted_consumers, 0);
    }
}
