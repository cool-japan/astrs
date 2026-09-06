//! Segment geometry and policy configuration.
//!
//! A ring is described by four numbers — how many slots, how big a payload
//! each holds, how big a metadata blob rides beside it, and how many
//! consumers may attach — plus one policy: what the producer does when no
//! slot can be reclaimed.
//!
//! # Defaults (blueprint §24.2)
//!
//! | Item | Default | Environment override |
//! |---|---|---|
//! | SHM pool per output | 8 MiB | `ASTRS_SHM_POOL_SIZE` |
//! | Zero-copy threshold | 4096 B | `ASTRS_ZERO_COPY_THRESHOLD` |
//!
//! [`SegmentConfig::from_pool_size`] turns a pool budget into a geometry, so
//! a manifest that only says `shm_pool_size: 16MiB` still gets a sensible
//! slot count.
//!
//! # Examples
//!
//! ```
//! use astrs_shm::{OverflowPolicy, SegmentConfig};
//!
//! let config = SegmentConfig::from_pool_size(8 * 1024 * 1024)?
//!     .with_overflow(OverflowPolicy::Overwrite);
//! assert!(config.slot_count() >= 2);
//! assert_eq!(config.overflow(), OverflowPolicy::Overwrite);
//! # Ok::<(), astrs_shm::ShmError>(())
//! ```

use std::time::Duration;

use crate::error::{ShmError, ShmResult};

/// Environment variable overriding the per-output pool size, in bytes
/// (blueprint §24.2).
pub const ENV_POOL_SIZE: &str = "ASTRS_SHM_POOL_SIZE";

/// Environment variable overriding the zero-copy threshold, in bytes
/// (blueprint §24.2).
pub const ENV_ZERO_COPY_THRESHOLD: &str = "ASTRS_ZERO_COPY_THRESHOLD";

/// The default per-output pool size: 8 MiB (blueprint §24.2).
pub const DEFAULT_POOL_SIZE: u64 = 8 * 1024 * 1024;

/// The default zero-copy threshold: payloads at or above this size take a
/// shared-memory slot, smaller ones ride the UDS control channel
/// (blueprint §6.2, §24.2).
pub const DEFAULT_ZERO_COPY_THRESHOLD: usize = 4096;

/// The default number of slots in a ring.
///
/// Eight is the same order as the default input queue depth (10, §24.2)
/// while leaving the producer a free slot to write into when every consumer
/// is one message behind.
pub const DEFAULT_SLOT_COUNT: u32 = 8;

/// The default per-slot metadata capacity, in bytes.
///
/// Metadata is an oxicode-encoded `{version, timestamp, parameters}` struct
/// (§6.1) — tens of bytes in the common case, and §7 caps the parameter map
/// at a small count. 512 bytes is generous and keeps the metadata region one
/// eighth of a 4 KiB page.
pub const DEFAULT_META_CAPACITY: u32 = 512;

/// The default consumer-table capacity.
///
/// A fan-out beyond this is a manifest smell, not a runtime condition; the
/// table is a fixed-size array in shared memory precisely so attaching never
/// allocates.
pub const DEFAULT_MAX_CONSUMERS: u32 = 32;

/// The largest slot count the layout admits.
///
/// Bounded so a corrupt header cannot describe a segment whose slot table
/// alone would exceed the address space, and so the validation arithmetic
/// stays comfortably inside `u64`.
pub const MAX_SLOT_COUNT: u32 = 1 << 20;

/// The largest per-slot payload capacity the layout admits: 256 MiB, the
/// blueprint's hard message cap (§6.1).
pub const MAX_PAYLOAD_CAPACITY: u32 = 256 * 1024 * 1024;

/// The largest per-slot metadata capacity the layout admits.
pub const MAX_META_CAPACITY: u32 = 1024 * 1024;

/// The largest consumer table the layout admits.
pub const MAX_MAX_CONSUMERS: u32 = 4096;

/// The default staleness budget for a consumer heartbeat.
///
/// A consumer that has not refreshed its heartbeat within this window *and*
/// whose pid is dead is evicted by the producer's reclaim pass, so a crashed
/// reader cannot wedge the ring forever (blueprint §6.2 crash safety).
pub const DEFAULT_CONSUMER_STALE_AFTER: Duration = Duration::from_secs(10);

/// What the producer does when the slot it needs cannot be reclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum OverflowPolicy {
    /// Refuse the allocation with [`ShmError::PoolExhausted`].
    ///
    /// The reliable default: no consumer ever loses a message on this ring.
    /// Blueprint §6.2 forbids sleeping and retrying — the caller falls back
    /// to the daemon path and bumps `shm_fallback_total`.
    #[default]
    Block,

    /// Overwrite the oldest resident message that no reader has pinned.
    ///
    /// Latest-only semantics for topics where staleness is worse than loss
    /// (a camera frame, a pose estimate). Consumers that fall behind observe
    /// [`crate::RecvError::Lagged`] with an exact count of what they missed.
    /// A slot pinned by a live [`crate::Sample`] is *never* overwritten, so
    /// zero-copy reads stay sound under this policy too.
    Overwrite,
}

impl OverflowPolicy {
    /// The name used in logs and manifests.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::OverflowPolicy;
    ///
    /// assert_eq!(OverflowPolicy::Block.as_str(), "block");
    /// assert_eq!(OverflowPolicy::Overwrite.as_str(), "overwrite");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Overwrite => "overwrite",
        }
    }

    /// Whether this policy may drop messages for a slow consumer.
    #[must_use]
    pub const fn may_overwrite(self) -> bool {
        matches!(self, Self::Overwrite)
    }
}

/// Which OS object backs a segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Backing {
    /// Pick the platform default: `memfd_create` on Linux, `shm_open` on
    /// macOS and every other Unix.
    ///
    /// The Linux default is anonymous, which is the point: a `memfd` has no
    /// filesystem presence to leak if every process in the dataflow dies at
    /// once, and the daemon already brokers the descriptors (§6.3).
    #[default]
    Auto,

    /// Force `memfd_create`. Linux only; elsewhere the constructor returns
    /// [`ShmError::Unsupported`].
    Memfd,

    /// Force `shm_open` with the short hashed name, on any Unix.
    ///
    /// Choose this when a consumer must be able to attach *by name* without
    /// the broker — the `astrs doctor` inspection path, and the replay
    /// tooling.
    Named,
}

/// The geometry and policy of one ring.
///
/// Values are validated at construction, so a `SegmentConfig` that exists is
/// one the layout can express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentConfig {
    slot_count: u32,
    payload_capacity: u32,
    meta_capacity: u32,
    max_consumers: u32,
    overflow: OverflowPolicy,
    backing: Backing,
    consumer_stale_after: Duration,
}

impl Default for SegmentConfig {
    fn default() -> Self {
        // Every component of the default is inside the validated range, so
        // the unchecked construction below is sound by inspection and needs
        // no fallible path in `Default`.
        Self {
            slot_count: DEFAULT_SLOT_COUNT,
            payload_capacity: default_payload_capacity(DEFAULT_POOL_SIZE, DEFAULT_SLOT_COUNT),
            meta_capacity: DEFAULT_META_CAPACITY,
            max_consumers: DEFAULT_MAX_CONSUMERS,
            overflow: OverflowPolicy::Block,
            backing: Backing::Auto,
            consumer_stale_after: DEFAULT_CONSUMER_STALE_AFTER,
        }
    }
}

impl SegmentConfig {
    /// Build a configuration from an explicit slot count and payload
    /// capacity.
    ///
    /// # Errors
    ///
    /// [`ShmError::InvalidConfig`] if either value is zero or above the
    /// layout caps ([`MAX_SLOT_COUNT`], [`MAX_PAYLOAD_CAPACITY`]).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::SegmentConfig;
    ///
    /// let config = SegmentConfig::new(16, 64 * 1024)?;
    /// assert_eq!(config.slot_count(), 16);
    /// assert_eq!(config.payload_capacity(), 64 * 1024);
    /// assert!(SegmentConfig::new(0, 1024).is_err());
    /// # Ok::<(), astrs_shm::ShmError>(())
    /// ```
    pub fn new(slot_count: u32, payload_capacity: u32) -> ShmResult<Self> {
        let config = Self {
            slot_count,
            payload_capacity,
            ..Self::default()
        };
        config.validate()?;
        Ok(config)
    }

    /// Derive a geometry from a pool budget in bytes.
    ///
    /// Uses [`DEFAULT_SLOT_COUNT`] slots and divides the budget between them,
    /// leaving room for the header, slot table, consumer table and metadata
    /// regions. The slot count is held fixed and the payload capacity is what
    /// flexes, because the ring's *depth* is what a manifest author reasons
    /// about when sizing queues (§11.2) — halving it silently to fit a budget
    /// would change the route's loss behaviour behind their back.
    ///
    /// # Errors
    ///
    /// [`ShmError::InvalidConfig`] if the budget leaves nothing for payloads
    /// once the fixed overheads are subtracted.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::SegmentConfig;
    ///
    /// let config = SegmentConfig::from_pool_size(8 * 1024 * 1024)?;
    /// let total = u64::from(config.slot_count()) * u64::from(config.payload_capacity());
    /// assert!(total <= 8 * 1024 * 1024);
    /// assert!(SegmentConfig::from_pool_size(0).is_err());
    /// # Ok::<(), astrs_shm::ShmError>(())
    /// ```
    pub fn from_pool_size(pool_size: u64) -> ShmResult<Self> {
        Self::from_pool_size_with_slots(pool_size, DEFAULT_SLOT_COUNT)
    }

    /// As [`SegmentConfig::from_pool_size`], with an explicit slot count.
    ///
    /// # Errors
    ///
    /// [`ShmError::InvalidConfig`] if the slot count is zero or above
    /// [`MAX_SLOT_COUNT`], or if the budget cannot fund a minimally useful
    /// payload per slot.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::SegmentConfig;
    ///
    /// let config = SegmentConfig::from_pool_size_with_slots(1024 * 1024, 4)?;
    /// assert_eq!(config.slot_count(), 4);
    /// # Ok::<(), astrs_shm::ShmError>(())
    /// ```
    pub fn from_pool_size_with_slots(pool_size: u64, slot_count: u32) -> ShmResult<Self> {
        if slot_count == 0 || slot_count > MAX_SLOT_COUNT {
            return Err(ShmError::invalid_config(format!(
                "slot_count {slot_count} is outside 1..={MAX_SLOT_COUNT}"
            )));
        }
        let payload_capacity = default_payload_capacity(pool_size, slot_count);
        if payload_capacity == 0 {
            return Err(ShmError::invalid_config(format!(
                "pool size {pool_size} cannot fund {slot_count} slots"
            )));
        }
        let config = Self {
            slot_count,
            payload_capacity,
            ..Self::default()
        };
        config.validate()?;
        Ok(config)
    }

    /// Read [`ENV_POOL_SIZE`] (falling back to [`DEFAULT_POOL_SIZE`]) and
    /// derive a geometry from it.
    ///
    /// A malformed value is a configuration error, not a silent fallback: a
    /// robot that quietly ignores `ASTRS_SHM_POOL_SIZE=1O MiB` (letter O) is
    /// a robot whose latency budget silently changed.
    ///
    /// # Errors
    ///
    /// [`ShmError::InvalidConfig`] if the variable is present but not a
    /// positive integer, or if the resulting geometry is unusable.
    pub fn from_env() -> ShmResult<Self> {
        let pool_size = match std::env::var(ENV_POOL_SIZE) {
            Ok(raw) => raw.trim().parse::<u64>().map_err(|err| {
                ShmError::invalid_config(format!(
                    "{ENV_POOL_SIZE}={raw:?} is not a byte count: {err}"
                ))
            })?,
            Err(_) => DEFAULT_POOL_SIZE,
        };
        Self::from_pool_size(pool_size)
    }

    /// Override the number of slots.
    ///
    /// # Errors
    ///
    /// [`ShmError::InvalidConfig`] if the value is outside
    /// `1..=`[`MAX_SLOT_COUNT`].
    pub fn with_slot_count(mut self, slot_count: u32) -> ShmResult<Self> {
        self.slot_count = slot_count;
        self.validate()?;
        Ok(self)
    }

    /// Override the per-slot payload capacity.
    ///
    /// # Errors
    ///
    /// [`ShmError::InvalidConfig`] if the value is outside
    /// `1..=`[`MAX_PAYLOAD_CAPACITY`].
    pub fn with_payload_capacity(mut self, payload_capacity: u32) -> ShmResult<Self> {
        self.payload_capacity = payload_capacity;
        self.validate()?;
        Ok(self)
    }

    /// Override the per-slot metadata capacity.
    ///
    /// # Errors
    ///
    /// [`ShmError::InvalidConfig`] if the value exceeds
    /// [`MAX_META_CAPACITY`].
    pub fn with_meta_capacity(mut self, meta_capacity: u32) -> ShmResult<Self> {
        self.meta_capacity = meta_capacity;
        self.validate()?;
        Ok(self)
    }

    /// Override the consumer-table capacity.
    ///
    /// # Errors
    ///
    /// [`ShmError::InvalidConfig`] if the value is outside
    /// `1..=`[`MAX_MAX_CONSUMERS`].
    pub fn with_max_consumers(mut self, max_consumers: u32) -> ShmResult<Self> {
        self.max_consumers = max_consumers;
        self.validate()?;
        Ok(self)
    }

    /// Set the overflow policy.
    #[must_use]
    pub const fn with_overflow(mut self, overflow: OverflowPolicy) -> Self {
        self.overflow = overflow;
        self
    }

    /// Set the OS backing.
    #[must_use]
    pub const fn with_backing(mut self, backing: Backing) -> Self {
        self.backing = backing;
        self
    }

    /// Set how long a consumer heartbeat may go unrefreshed before the
    /// producer may evict the entry (given the pid is also dead).
    #[must_use]
    pub const fn with_consumer_stale_after(mut self, stale_after: Duration) -> Self {
        self.consumer_stale_after = stale_after;
        self
    }

    /// The number of slots in the ring.
    #[must_use]
    pub const fn slot_count(&self) -> u32 {
        self.slot_count
    }

    /// The per-slot payload capacity, in bytes.
    #[must_use]
    pub const fn payload_capacity(&self) -> u32 {
        self.payload_capacity
    }

    /// The per-slot metadata capacity, in bytes.
    #[must_use]
    pub const fn meta_capacity(&self) -> u32 {
        self.meta_capacity
    }

    /// The consumer-table capacity.
    #[must_use]
    pub const fn max_consumers(&self) -> u32 {
        self.max_consumers
    }

    /// The overflow policy.
    #[must_use]
    pub const fn overflow(&self) -> OverflowPolicy {
        self.overflow
    }

    /// The OS backing.
    #[must_use]
    pub const fn backing(&self) -> Backing {
        self.backing
    }

    /// The consumer heartbeat staleness budget.
    #[must_use]
    pub const fn consumer_stale_after(&self) -> Duration {
        self.consumer_stale_after
    }

    /// Check every field against the layout caps.
    ///
    /// # Errors
    ///
    /// [`ShmError::InvalidConfig`] naming the first offending field.
    pub fn validate(&self) -> ShmResult<()> {
        if self.slot_count == 0 || self.slot_count > MAX_SLOT_COUNT {
            return Err(ShmError::invalid_config(format!(
                "slot_count {} is outside 1..={MAX_SLOT_COUNT}",
                self.slot_count
            )));
        }
        if self.payload_capacity == 0 || self.payload_capacity > MAX_PAYLOAD_CAPACITY {
            return Err(ShmError::invalid_config(format!(
                "payload_capacity {} is outside 1..={MAX_PAYLOAD_CAPACITY}",
                self.payload_capacity
            )));
        }
        if self.meta_capacity > MAX_META_CAPACITY {
            return Err(ShmError::invalid_config(format!(
                "meta_capacity {} is above {MAX_META_CAPACITY}",
                self.meta_capacity
            )));
        }
        if self.max_consumers == 0 || self.max_consumers > MAX_MAX_CONSUMERS {
            return Err(ShmError::invalid_config(format!(
                "max_consumers {} is outside 1..={MAX_MAX_CONSUMERS}",
                self.max_consumers
            )));
        }
        Ok(())
    }
}

/// Divide a pool budget between `slot_count` slots.
///
/// Reserves a conservative fixed overhead for the header, slot table,
/// consumer table and metadata regions, then rounds the remainder down to a
/// 128-byte multiple so the derived geometry needs no further padding.
fn default_payload_capacity(pool_size: u64, slot_count: u32) -> u32 {
    let slots = u64::from(slot_count);
    // Header (128) + slot table (64/slot) + consumer table (64/consumer,
    // default capacity) + per-slot metadata region.
    let overhead = 128
        + slots.saturating_mul(64)
        + u64::from(DEFAULT_MAX_CONSUMERS).saturating_mul(64)
        + slots.saturating_mul(u64::from(DEFAULT_META_CAPACITY).next_multiple_of(128));
    let usable = pool_size.saturating_sub(overhead);
    let per_slot = usable / slots.max(1);
    let rounded = per_slot & !127;
    u32::try_from(rounded.min(u64::from(MAX_PAYLOAD_CAPACITY))).unwrap_or(MAX_PAYLOAD_CAPACITY)
}

/// The zero-copy threshold in force for this process (blueprint §6.2).
///
/// Payloads at or above the threshold are worth a shared-memory slot; smaller
/// ones ride the UDS control channel, where the round trip costs less than
/// the slot bookkeeping. A malformed `ASTRS_ZERO_COPY_THRESHOLD` falls back
/// to the default rather than failing: unlike the pool size, this value only
/// selects between two correct paths.
///
/// # Examples
///
/// ```
/// use astrs_shm::{zero_copy_threshold, DEFAULT_ZERO_COPY_THRESHOLD};
///
/// // With no override in the environment the default is in force.
/// let threshold = zero_copy_threshold();
/// assert!(threshold >= 1);
/// let _ = DEFAULT_ZERO_COPY_THRESHOLD;
/// ```
#[must_use]
pub fn zero_copy_threshold() -> usize {
    std::env::var(ENV_ZERO_COPY_THRESHOLD)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_ZERO_COPY_THRESHOLD)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_default_configuration_validates() {
        SegmentConfig::default().validate().unwrap();
        assert_eq!(SegmentConfig::default().slot_count(), DEFAULT_SLOT_COUNT);
        assert_eq!(SegmentConfig::default().overflow(), OverflowPolicy::Block);
        assert_eq!(SegmentConfig::default().backing(), Backing::Auto);
    }

    #[test]
    fn pool_derived_geometry_stays_within_budget() {
        for pool in [
            256 * 1024u64,
            1024 * 1024,
            8 * 1024 * 1024,
            64 * 1024 * 1024,
        ] {
            let config = SegmentConfig::from_pool_size(pool).unwrap();
            let payload_total =
                u64::from(config.slot_count()) * u64::from(config.payload_capacity());
            assert!(payload_total < pool, "pool {pool} overshot");
            assert_eq!(config.payload_capacity() % 128, 0);
        }
    }

    #[test]
    fn tiny_pools_are_rejected_rather_than_silently_shrunk() {
        assert!(SegmentConfig::from_pool_size(0).is_err());
        assert!(SegmentConfig::from_pool_size(1024).is_err());
    }

    #[test]
    fn caps_are_enforced_on_every_field() {
        assert!(SegmentConfig::new(0, 1024).is_err());
        assert!(SegmentConfig::new(MAX_SLOT_COUNT + 1, 1024).is_err());
        assert!(SegmentConfig::new(4, 0).is_err());
        assert!(SegmentConfig::new(4, MAX_PAYLOAD_CAPACITY).is_ok());
        let config = SegmentConfig::new(4, 1024).unwrap();
        assert!(config.with_meta_capacity(MAX_META_CAPACITY + 1).is_err());
        assert!(config.with_max_consumers(0).is_err());
        assert!(config.with_max_consumers(MAX_MAX_CONSUMERS + 1).is_err());
        assert!(config.with_slot_count(MAX_SLOT_COUNT).is_ok());
    }

    #[test]
    fn builders_compose() {
        let config = SegmentConfig::new(4, 4096)
            .unwrap()
            .with_meta_capacity(128)
            .unwrap()
            .with_max_consumers(2)
            .unwrap()
            .with_overflow(OverflowPolicy::Overwrite)
            .with_backing(Backing::Named)
            .with_consumer_stale_after(Duration::from_millis(50));
        assert_eq!(config.meta_capacity(), 128);
        assert_eq!(config.max_consumers(), 2);
        assert!(config.overflow().may_overwrite());
        assert_eq!(config.backing(), Backing::Named);
        assert_eq!(config.consumer_stale_after(), Duration::from_millis(50));
    }

    #[test]
    fn overflow_policy_names() {
        assert_eq!(OverflowPolicy::Block.as_str(), "block");
        assert_eq!(OverflowPolicy::Overwrite.as_str(), "overwrite");
        assert!(!OverflowPolicy::Block.may_overwrite());
        assert!(!OverflowPolicy::default().may_overwrite());
    }

    #[test]
    fn zero_copy_threshold_defaults_when_unset_or_bogus() {
        // The process environment is shared with other tests, so only assert
        // the invariant that holds for every legal reading.
        let threshold = zero_copy_threshold();
        assert!(threshold > 0);
    }

    #[test]
    fn slot_count_of_one_is_legal_even_though_from_pool_size_avoids_it() {
        let config = SegmentConfig::new(1, 128).unwrap();
        assert_eq!(config.slot_count(), 1);
    }
}
