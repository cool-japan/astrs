//! The non-Unix stub.
//!
//! Windows (and anything else without POSIX shared memory and `SCM_RIGHTS`)
//! gets the *same type names* as a Unix build, with every constructor
//! returning [`ShmError::Unsupported`]. Downstream crates therefore compile
//! unchanged and simply take the reliable daemon path (§6.3) — no `cfg` in
//! the daemon, the node API, or the scheduler.
//!
//! The types are uninhabited: they carry a [`std::convert::Infallible`]
//! field, so the compiler knows no value of them can exist and every method
//! body is a `match` on nothing. That is stronger than returning dummies —
//! it makes "a `Segment` exists on Windows" a type error rather than a
//! runtime surprise.
//!
//! # What a caller sees
//!
//! ```ignore
//! // On Windows:
//! let err = Segment::create(key, config).unwrap_err();
//! assert!(matches!(err, ShmError::Unsupported { .. }));
//! assert!(err.should_fall_back());
//! ```
//!
//! A Windows port is not blocked on anything conceptual: the ring, the
//! layout, the slot protocol and the consumer table in this crate are all
//! portable and are compiled and tested on every platform. What is missing is
//! the three platform primitives — a shared mapping
//! (`CreateFileMapping`/`MapViewOfFile`), a doorbell (an auto-reset event),
//! and descriptor passing (`DuplicateHandle` over a named pipe). That is a
//! post-0.1.0 item, deliberately: §1.3 scopes 0.1.0 to Linux and macOS.

use std::convert::Infallible;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::attach::AttachOptions;
use crate::config::SegmentConfig;
use crate::consumer_table::ConsumerEntry;
use crate::error::{RecvError, ShmError, ShmResult};
use crate::header::{SegmentHeader, SegmentHeaderView};
use crate::key::{SegmentKey, SegmentName};
use crate::layout::SegmentLayout;
use crate::slot::SlotHeader;
use crate::stats::BrokerStats;

macro_rules! unsupported {
    ($operation:literal) => {
        Err(ShmError::unsupported($operation))
    };
}

/// A mapped segment — never constructible on this platform.
#[derive(Debug)]
pub struct Segment {
    never: Infallible,
}

impl Segment {
    /// Always fails with [`ShmError::Unsupported`].
    ///
    /// # Errors
    ///
    /// Always.
    pub fn create(_key: SegmentKey, _config: SegmentConfig) -> ShmResult<Self> {
        unsupported!("Segment::create")
    }

    /// Always fails with [`ShmError::Unsupported`].
    ///
    /// # Errors
    ///
    /// Always.
    pub fn create_shared(_key: SegmentKey, _config: SegmentConfig) -> ShmResult<Arc<Self>> {
        unsupported!("Segment::create_shared")
    }

    /// Always fails with [`ShmError::Unsupported`].
    ///
    /// # Errors
    ///
    /// Always.
    pub fn attach(_key: &SegmentKey) -> ShmResult<Self> {
        unsupported!("Segment::attach")
    }

    /// Always fails with [`ShmError::Unsupported`].
    ///
    /// # Errors
    ///
    /// Always.
    pub fn open_named(_name: &SegmentName, _expect: Option<&SegmentKey>) -> ShmResult<Self> {
        unsupported!("Segment::open_named")
    }

    /// Unreachable: no `Segment` exists on this platform.
    #[must_use]
    pub fn header(&self) -> &SegmentHeader {
        match self.never {}
    }

    /// Unreachable: no `Segment` exists on this platform.
    #[must_use]
    pub fn view(&self) -> SegmentHeaderView {
        match self.never {}
    }

    /// Unreachable: no `Segment` exists on this platform.
    #[must_use]
    pub fn layout(&self) -> &SegmentLayout {
        match self.never {}
    }

    /// Unreachable: no `Segment` exists on this platform.
    #[must_use]
    pub fn key(&self) -> Option<&SegmentKey> {
        match self.never {}
    }

    /// Unreachable: no `Segment` exists on this platform.
    #[must_use]
    pub fn name(&self) -> Option<&SegmentName> {
        match self.never {}
    }

    /// Unreachable: no `Segment` exists on this platform.
    #[must_use]
    pub fn slot(&self, _index: u32) -> &SlotHeader {
        match self.never {}
    }

    /// Unreachable: no `Segment` exists on this platform.
    #[must_use]
    pub fn consumer_entry(&self, _index: u32) -> &ConsumerEntry {
        match self.never {}
    }

    /// Unreachable: no `Segment` exists on this platform.
    pub fn consumer_entries(&self) -> impl Iterator<Item = (u32, &ConsumerEntry)> + Clone {
        match self.never {}
        #[allow(unreachable_code)]
        std::iter::empty()
    }

    /// Unreachable: no `Segment` exists on this platform.
    #[must_use]
    pub fn mapped_len(&self) -> usize {
        match self.never {}
    }

    /// Unreachable: no `Segment` exists on this platform.
    #[must_use]
    pub fn producer_alive(&self) -> bool {
        match self.never {}
    }

    /// Unreachable: no `Segment` exists on this platform.
    pub fn mark_closed(&self) {
        match self.never {}
    }

    /// Unreachable: no `Segment` exists on this platform.
    ///
    /// # Errors
    ///
    /// Never returns.
    pub fn unlink(&self) -> ShmResult<()> {
        match self.never {}
    }

    /// Unreachable: no `Segment` exists on this platform.
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        match self.never {}
    }
}

/// The single writer of a ring — never constructible on this platform.
#[derive(Debug)]
pub struct Producer {
    never: Infallible,
}

impl Producer {
    /// Always fails with [`ShmError::Unsupported`].
    ///
    /// # Errors
    ///
    /// Always.
    pub fn new(_segment: Arc<Segment>) -> ShmResult<Self> {
        unsupported!("Producer::new")
    }

    /// Unreachable: no `Producer` exists on this platform.
    ///
    /// # Errors
    ///
    /// Never returns.
    pub fn allocate(&mut self, _len: usize) -> ShmResult<SampleMut<'_>> {
        match self.never {}
    }

    /// Unreachable: no `Producer` exists on this platform.
    ///
    /// # Errors
    ///
    /// Never returns.
    pub fn try_allocate(&mut self, _len: usize) -> ShmResult<SampleMut<'_>> {
        match self.never {}
    }

    /// Unreachable: no `Producer` exists on this platform.
    ///
    /// # Errors
    ///
    /// Never returns.
    pub fn send(&mut self, _payload: &[u8], _meta: &[u8]) -> ShmResult<u64> {
        match self.never {}
    }

    /// Unreachable: no `Producer` exists on this platform.
    pub fn close(&mut self) {
        match self.never {}
    }
}

/// A zero-copy write window — never constructible on this platform.
#[derive(Debug)]
pub struct SampleMut<'a> {
    never: Infallible,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl SampleMut<'_> {
    /// Unreachable: no `SampleMut` exists on this platform.
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        match self.never {}
    }

    /// Unreachable: no `SampleMut` exists on this platform.
    ///
    /// # Errors
    ///
    /// Never returns.
    pub fn commit(self, _meta: &[u8]) -> ShmResult<u64> {
        match self.never {}
    }
}

/// An attached reader — never constructible on this platform.
#[derive(Debug)]
pub struct Consumer {
    never: Infallible,
}

impl Consumer {
    /// Always fails with [`ShmError::Unsupported`].
    ///
    /// # Errors
    ///
    /// Always.
    pub fn attach(_segment: Arc<Segment>, _options: AttachOptions) -> ShmResult<Self> {
        unsupported!("Consumer::attach")
    }

    /// Unreachable: no `Consumer` exists on this platform.
    ///
    /// # Errors
    ///
    /// Never returns.
    pub fn try_next(&mut self) -> Result<Sample, RecvError> {
        match self.never {}
    }

    /// Unreachable: no `Consumer` exists on this platform.
    ///
    /// # Errors
    ///
    /// Never returns.
    pub fn recv(&mut self) -> Result<Sample, RecvError> {
        match self.never {}
    }

    /// Unreachable: no `Consumer` exists on this platform.
    ///
    /// # Errors
    ///
    /// Never returns.
    pub fn next_blocking(&mut self, _timeout: Duration) -> Result<Sample, RecvError> {
        match self.never {}
    }

    /// Unreachable: no `Consumer` exists on this platform.
    pub fn detach(&mut self) {
        match self.never {}
    }
}

/// A zero-copy read view — never constructible on this platform.
#[derive(Debug)]
pub struct Sample {
    never: Infallible,
}

impl Sample {
    /// Unreachable: no `Sample` exists on this platform.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        match self.never {}
    }

    /// Unreachable: no `Sample` exists on this platform.
    #[must_use]
    pub fn metadata(&self) -> &[u8] {
        match self.never {}
    }

    /// Unreachable: no `Sample` exists on this platform.
    #[must_use]
    pub fn seq(&self) -> u64 {
        match self.never {}
    }

    /// Unreachable: no `Sample` exists on this platform.
    #[must_use]
    pub fn to_vec(&self) -> Vec<u8> {
        match self.never {}
    }
}

/// A wakeup channel — never constructible on this platform.
#[derive(Debug)]
pub struct Doorbell {
    never: Infallible,
}

impl Doorbell {
    /// Always fails with [`ShmError::Unsupported`].
    ///
    /// # Errors
    ///
    /// Always.
    pub fn new() -> ShmResult<Self> {
        unsupported!("Doorbell::new")
    }

    /// Unreachable: no `Doorbell` exists on this platform.
    ///
    /// # Errors
    ///
    /// Never returns.
    pub fn wait(&self, _timeout: Option<Duration>) -> ShmResult<bool> {
        match self.never {}
    }

    /// Unreachable: no `Doorbell` exists on this platform.
    ///
    /// # Errors
    ///
    /// Never returns.
    pub fn drain(&self) -> ShmResult<u64> {
        match self.never {}
    }
}

/// A process watch — never constructible on this platform.
#[derive(Debug)]
pub struct ProcessWatch {
    never: Infallible,
}

impl ProcessWatch {
    /// Unreachable: no `ProcessWatch` exists on this platform.
    #[must_use]
    pub fn pid(&self) -> i64 {
        match self.never {}
    }

    /// Unreachable: no `ProcessWatch` exists on this platform.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        match self.never {}
    }
}

/// The daemon-side broker — never constructible on this platform.
#[derive(Debug)]
pub struct SegmentBroker {
    never: Infallible,
}

impl SegmentBroker {
    /// Always fails with [`ShmError::Unsupported`].
    ///
    /// # Errors
    ///
    /// Always.
    pub fn bind(_path: impl AsRef<Path>) -> ShmResult<Arc<Self>> {
        unsupported!("SegmentBroker::bind")
    }

    /// Unreachable: no `SegmentBroker` exists on this platform.
    ///
    /// # Errors
    ///
    /// Never returns.
    pub fn create_segment(
        &self,
        _key: SegmentKey,
        _config: SegmentConfig,
    ) -> ShmResult<Arc<Segment>> {
        match self.never {}
    }

    /// Unreachable: no `SegmentBroker` exists on this platform.
    pub fn adopt(&self, _key: SegmentKey, _segment: Arc<Segment>) {
        match self.never {}
    }

    /// Unreachable: no `SegmentBroker` exists on this platform.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        match self.never {}
    }

    /// Unreachable: no `SegmentBroker` exists on this platform.
    #[must_use]
    pub fn segment(&self, _key_digest: u128, _generation: u64) -> Option<Arc<Segment>> {
        match self.never {}
    }

    /// Unreachable: no `SegmentBroker` exists on this platform.
    #[must_use]
    pub fn len(&self) -> usize {
        match self.never {}
    }

    /// Unreachable: no `SegmentBroker` exists on this platform.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self.never {}
    }

    /// Unreachable: no `SegmentBroker` exists on this platform.
    #[must_use]
    pub fn stats(&self) -> BrokerStats {
        match self.never {}
    }

    /// Unreachable: no `SegmentBroker` exists on this platform.
    pub fn poll_producers(&self) -> Vec<SegmentKey> {
        match self.never {}
    }

    /// Unreachable: no `SegmentBroker` exists on this platform.
    pub fn rebind_producer(&self, _key_digest: u128, _generation: u64, _pid: i64) -> bool {
        match self.never {}
    }

    /// Unreachable: no `SegmentBroker` exists on this platform.
    pub fn evict_stale_consumers(&self, _stale_after: Duration) -> u32 {
        match self.never {}
    }

    /// Unreachable: no `SegmentBroker` exists on this platform.
    pub fn sweep(&self) -> usize {
        match self.never {}
    }

    /// Unreachable: no `SegmentBroker` exists on this platform.
    pub fn close_segment(&self, _key_digest: u128, _generation: u64) -> bool {
        match self.never {}
    }

    /// Unreachable: no `SegmentBroker` exists on this platform.
    pub fn stop(&self) {
        match self.never {}
    }

    /// Unreachable: no `SegmentBroker` exists on this platform.
    #[must_use]
    pub fn is_stopping(&self) -> bool {
        match self.never {}
    }

    /// Unreachable: no `SegmentBroker` exists on this platform.
    pub fn spawn(broker: &Arc<Self>) -> BrokerHandle {
        match broker.never {}
    }
}

/// The node-side broker client — never constructible on this platform.
#[derive(Debug)]
pub struct SegmentClient {
    never: Infallible,
}

impl SegmentClient {
    /// Always fails with [`ShmError::Unsupported`].
    ///
    /// # Errors
    ///
    /// Always.
    pub fn connect(_path: impl AsRef<Path>) -> ShmResult<Self> {
        unsupported!("SegmentClient::connect")
    }

    /// Unreachable: no `SegmentClient` exists on this platform.
    ///
    /// # Errors
    ///
    /// Never returns.
    pub fn attach(&mut self, _key: &SegmentKey) -> ShmResult<Segment> {
        match self.never {}
    }
}

/// The producer's handle to one consumer's doorbell — never constructible on
/// this platform.
#[derive(Debug)]
pub struct DoorbellRinger {
    never: Infallible,
}

impl DoorbellRinger {
    /// Unreachable: no `DoorbellRinger` exists on this platform.
    ///
    /// # Errors
    ///
    /// Never returns.
    pub fn ring(&self) -> ShmResult<bool> {
        match self.never {}
    }
}

/// The set of doorbells a producer must ring.
///
/// Constructible here — it is an empty list on a platform with no data plane
/// — so a daemon can hold one unconditionally.
#[derive(Debug, Default)]
pub struct DoorbellRegistry;

impl DoorbellRegistry {
    /// An empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Always zero: no doorbell can be registered on this platform.
    #[must_use]
    pub const fn len(&self) -> usize {
        0
    }

    /// Always `true`.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        true
    }
}

/// A producer's private view of the doorbell set.
#[derive(Debug, Default)]
pub struct DoorbellFanout;

impl DoorbellFanout {
    /// An empty fanout.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Always zero.
    pub fn ring_all(&mut self, _registry: &DoorbellRegistry) -> usize {
        0
    }
}

/// A producer's push-only channel to the broker — never constructible on this
/// platform.
#[derive(Debug)]
pub struct ProducerChannel {
    never: Infallible,
}

impl ProducerChannel {
    /// Unreachable: no `ProducerChannel` exists on this platform.
    ///
    /// # Errors
    ///
    /// Never returns.
    pub fn poll(&self) -> ShmResult<usize> {
        match self.never {}
    }
}

/// A running broker's background thread — never constructible on this
/// platform.
#[derive(Debug)]
pub struct BrokerHandle {
    never: Infallible,
}

impl BrokerHandle {
    /// Unreachable: no `BrokerHandle` exists on this platform.
    #[must_use]
    pub fn broker(&self) -> &Arc<SegmentBroker> {
        match self.never {}
    }

    /// Unreachable: no `BrokerHandle` exists on this platform.
    pub fn stop(self) {
        match self.never {}
    }
}

/// How often the polling liveness fallback would re-check a pid.
///
/// Present for export parity; nothing on this platform reads it.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::DataflowId;

    fn key() -> SegmentKey {
        SegmentKey::from_parts(DataflowId::from_u128(1), "n", "o", 1).expect("valid ids")
    }

    #[test]
    fn every_constructor_reports_unsupported_and_asks_for_a_fallback() {
        let config = SegmentConfig::default();
        for err in [
            Segment::create(key(), config).unwrap_err(),
            Segment::attach(&key()).unwrap_err(),
            Doorbell::new().unwrap_err(),
            SegmentBroker::bind("ignored").unwrap_err(),
            SegmentClient::connect("ignored").unwrap_err(),
        ] {
            assert!(matches!(err, ShmError::Unsupported { .. }), "{err}");
            assert!(err.should_fall_back(), "{err}");
        }
    }

    #[test]
    fn the_inert_collections_are_constructible_so_a_daemon_needs_no_cfg() {
        let registry = DoorbellRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        let mut fanout = DoorbellFanout::new();
        assert_eq!(fanout.ring_all(&registry), 0);
        assert_eq!(DEFAULT_POLL_INTERVAL, Duration::from_millis(25));
    }

    #[test]
    fn the_portable_core_still_works_on_this_platform() {
        // The layout, key and configuration types are platform-independent
        // and are exercised here too, so a Windows build proves them.
        let layout = SegmentLayout::new(&SegmentConfig::new(4, 1024).unwrap()).unwrap();
        assert_eq!(layout.data_offset() % 128, 0);
        assert_eq!(key().os_name().as_str().len(), crate::SEGMENT_NAME_LEN);
    }
}
